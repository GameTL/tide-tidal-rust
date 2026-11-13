use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tide_api::TrackItem;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Dir not found")]
    NoDir,
}

/// Half-life of a play when ranking tracks for the offline cache.
pub const SCORE_HALF_LIFE_DAYS: f64 = 30.0;

/// DASH init + first media segments — enough to probe and start decoding while the rest downloads.
const PLAYBACK_READY_SEGMENTS: usize = 3;

async fn write_url_range(
    client: &reqwest::Client,
    file: &mut std::fs::File,
    urls: &[String],
    from: usize,
    to: usize,
    on_chunk: &(dyn Fn(usize, u8, u64) + Send + Sync),
    total_bytes: &mut u64,
) -> Result<(), CacheError> {
    use std::io::Write;
    const WINDOW: usize = 8;
    let slice = &urls[from..to];
    for (w, window) in slice.chunks(WINDOW).enumerate() {
        let base = from + w * WINDOW;
        for i in 0..window.len() {
            on_chunk(base + i, 1, *total_bytes);
        }
        let mut set = tokio::task::JoinSet::new();
        for (i, url) in window.iter().enumerate() {
            let (client, url) = (client.clone(), url.clone());
            set.spawn(async move {
                let bytes = client
                    .get(&url)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;
                Ok::<_, reqwest::Error>((i, bytes))
            });
        }
        let mut parts: Vec<Option<Vec<u8>>> = vec![None; window.len()];
        while let Some(joined) = set.join_next().await {
            let (i, bytes) = joined.map_err(|e| std::io::Error::other(e.to_string()))??;
            *total_bytes += bytes.len() as u64;
            on_chunk(base + i, 2, *total_bytes);
            parts[i] = Some(bytes.to_vec());
        }
        for part in parts.into_iter().flatten() {
            file.write_all(&part)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct SavedPlayback {
    pub queue: Vec<TrackItem>,
    pub order: Vec<usize>,
    pub pos: usize,
    pub elapsed_ms: u64,
    pub context: String,
    pub shuffle: bool,
    pub repeat_mode: i32,
    pub volume: f32,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PlaybackCursor {
    pub pos: usize,
    pub elapsed_ms: u64,
    pub shuffle: bool,
    pub repeat_mode: i32,
    pub volume: f32,
    pub muted: bool,
}

pub struct MetadataCache {
    conn: std::sync::Mutex<Connection>,
}

impl MetadataCache {
    pub fn new() -> Result<Self, CacheError> {
        let db_path = default_db_path()?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        // Library and listening history are personal: owner-only access.
        tide_api::restrict_to_owner(&db_path);
        let cache = Self {
            conn: std::sync::Mutex::new(conn),
        };
        cache.init_schema()?;
        Ok(cache)
    }

    pub fn in_memory() -> Result<Self, CacheError> {
        let conn = Connection::open_in_memory()?;
        let cache = Self {
            conn: std::sync::Mutex::new(conn),
        };
        cache.init_schema()?;
        Ok(cache)
    }

    fn init_schema(&self) -> Result<(), CacheError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS tracks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                artist TEXT NOT NULL,
                artist_id TEXT,
                album TEXT NOT NULL,
                album_id TEXT,
                duration_secs INTEGER NOT NULL,
                cover_url TEXT,
                audio_quality TEXT NOT NULL,
                explicit INTEGER NOT NULL DEFAULT 0,
                cached_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS favorites (
                track_id TEXT PRIMARY KEY,
                added_at INTEGER NOT NULL,
                FOREIGN KEY (track_id) REFERENCES tracks(id)
            );

            CREATE TABLE IF NOT EXISTS history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id TEXT NOT NULL,
                played_at INTEGER NOT NULL,
                FOREIGN KEY (track_id) REFERENCES tracks(id)
            );

            CREATE INDEX IF NOT EXISTS idx_history_played ON history(played_at DESC);

            CREATE TABLE IF NOT EXISTS cache_stats (
                track_id TEXT PRIMARY KEY,
                hits INTEGER NOT NULL DEFAULT 0,
                misses INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS playback_session (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                pos INTEGER NOT NULL DEFAULT 0,
                elapsed_ms INTEGER NOT NULL DEFAULT 0,
                context TEXT NOT NULL DEFAULT '',
                shuffle INTEGER NOT NULL DEFAULT 0,
                repeat_mode INTEGER NOT NULL DEFAULT 0,
                volume REAL NOT NULL DEFAULT 1.0,
                muted INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS playback_queue (
                queue_idx INTEGER PRIMARY KEY,
                track_id TEXT NOT NULL REFERENCES tracks(id)
            );

            CREATE TABLE IF NOT EXISTS playback_order (
                play_pos INTEGER PRIMARY KEY,
                queue_idx INTEGER NOT NULL REFERENCES playback_queue(queue_idx)
            );
            ",
        )?;
        // Upgrade older databases that predate `tracks.explicit`.
        let _ = conn.execute(
            "ALTER TABLE tracks ADD COLUMN explicit INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Every credited artist as JSON `[{"id","name"}]` (older rows keep only the first).
        let _ = conn.execute("ALTER TABLE tracks ADD COLUMN artists TEXT", []);
        Ok(())
    }

    /// Store an arbitrary JSON document (home feed, playlists, ...) for instant offline boot.
    pub fn put_json(&self, key: &str, json: &str) -> Result<(), CacheError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO kv (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![key, json, now],
        )?;
        Ok(())
    }

    pub fn get_json(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .ok()
    }

    pub fn clear_json(&self) -> Result<(), CacheError> {
        // Account data goes; app preferences stay.
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM kv WHERE key != 'settings'", [])?;
        Ok(())
    }

    pub fn upsert_track(&self, track: &TrackItem) -> Result<(), CacheError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = self.conn.lock().unwrap();
        Self::upsert_track_conn(&conn, track, now)
    }

    fn upsert_track_conn(conn: &Connection, track: &TrackItem, now: i64) -> Result<(), CacheError> {
        conn.execute(
            "INSERT INTO tracks (id, title, artist, artist_id, album, album_id, duration_secs, cover_url, audio_quality, explicit, cached_at, artists)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                title=excluded.title,
                artist=excluded.artist,
                artist_id=excluded.artist_id,
                album=excluded.album,
                album_id=excluded.album_id,
                duration_secs=excluded.duration_secs,
                cover_url=excluded.cover_url,
                audio_quality=excluded.audio_quality,
                explicit=excluded.explicit,
                cached_at=excluded.cached_at,
                artists=excluded.artists",
            params![
                track.id,
                track.title,
                track.artist,
                track.artist_id,
                track.album,
                track.album_id,
                track.duration_secs,
                track.cover_url,
                track.audio_quality,
                i32::from(track.explicit),
                now,
                (!track.artists.is_empty())
                    .then(|| serde_json::to_string(&track.artists).ok())
                    .flatten(),
            ],
        )?;
        Ok(())
    }

    fn row_to_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackItem> {
        Ok(TrackItem {
            id: row.get(0)?,
            title: row.get(1)?,
            artist: row.get(2)?,
            artist_id: row.get(3)?,
            album: row.get(4)?,
            album_id: row.get(5)?,
            duration_secs: row.get(6)?,
            cover_url: row.get(7)?,
            audio_quality: row.get(8)?,
            explicit: row.get::<_, i32>(9).unwrap_or(0) != 0,
            artists: row
                .get::<_, Option<String>>(10)
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default(),
            ..Default::default()
        })
    }

    pub fn get_track(&self, track_id: &str) -> Result<Option<TrackItem>, CacheError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, artist, artist_id, album, album_id, duration_secs, cover_url, audio_quality, explicit, artists
             FROM tracks WHERE id = ?1"
        )?;
        let mut rows = stmt.query(params![track_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::row_to_track(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn save_favorites(&self, tracks: &[TrackItem]) -> Result<(), CacheError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for track in tracks {
            self.upsert_track(track)?;
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO favorites (track_id, added_at) VALUES (?1, ?2)
                 ON CONFLICT(track_id) DO NOTHING",
                params![track.id, now],
            )?;
        }
        Ok(())
    }

    pub fn get_favorites(&self) -> Result<Vec<TrackItem>, CacheError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.title, t.artist, t.artist_id, t.album, t.album_id, t.duration_secs, t.cover_url, t.audio_quality, t.explicit, t.artists
             FROM favorites f
             JOIN tracks t ON f.track_id = t.id
             ORDER BY f.added_at DESC"
        )?;
        let rows = stmt.query_map([], Self::row_to_track)?;
        let mut result = Vec::new();
        for item in rows {
            result.push(item?);
        }
        Ok(result)
    }

    pub fn record_play(&self, track_id: &str) -> Result<(), CacheError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO history (track_id, played_at) VALUES (?1, ?2)",
            params![track_id, now],
        )?;
        Ok(())
    }

    /// Note whether a play was served from the local audio cache.
    pub fn record_cache_lookup(&self, track_id: &str, hit: bool) -> Result<(), CacheError> {
        let conn = self.conn.lock().unwrap();
        let (h, m) = if hit { (1, 0) } else { (0, 1) };
        conn.execute(
            "INSERT INTO cache_stats (track_id, hits, misses) VALUES (?1, ?2, ?3)
             ON CONFLICT(track_id) DO UPDATE SET hits = hits + excluded.hits, misses = misses + excluded.misses",
            params![track_id, h, m],
        )?;
        Ok(())
    }

    /// Total (hits, misses) of the audio cache since the stats began.
    pub fn cache_hit_stats(&self) -> (u64, u64) {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(hits), 0), COALESCE(SUM(misses), 0) FROM cache_stats",
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )
        .unwrap_or((0, 0))
    }

    /// How much each track is worth keeping offline: every play counts, and a
    /// play loses half its weight every [`SCORE_HALF_LIFE_DAYS`] days, so the
    /// score follows what you listen to now rather than years ago.
    pub fn play_scores(&self) -> std::collections::HashMap<String, f64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        self.play_scores_at(now)
    }

    pub fn play_scores_at(&self, now: i64) -> std::collections::HashMap<String, f64> {
        let mut scores = std::collections::HashMap::new();
        let conn = self.conn.lock().unwrap();
        let Ok(mut stmt) = conn.prepare("SELECT track_id, played_at FROM history") else {
            return scores;
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        else {
            return scores;
        };
        for (track_id, played_at) in rows.flatten() {
            let age_days = (now - played_at).max(0) as f64 / 86_400.0;
            *scores.entry(track_id).or_insert(0.0) += 0.5f64.powf(age_days / SCORE_HALF_LIFE_DAYS);
        }
        scores
    }

    /// The most played tracks (by decayed score), best first, with at least `min_plays` plays.
    pub fn top_played(&self, limit: usize, min_plays: u32) -> Vec<TrackItem> {
        let counts: std::collections::HashMap<String, u32> = {
            let conn = self.conn.lock().unwrap();
            let Ok(mut stmt) =
                conn.prepare("SELECT track_id, COUNT(*) FROM history GROUP BY track_id")
            else {
                return Vec::new();
            };
            let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))
            else {
                return Vec::new();
            };
            rows.flatten().collect()
        };
        let mut ranked: Vec<(String, f64)> = self
            .play_scores()
            .into_iter()
            .filter(|(id, _)| counts.get(id).copied().unwrap_or(0) >= min_plays)
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked
            .into_iter()
            .take(limit)
            .filter_map(|(id, _)| self.get_track(&id).ok().flatten())
            .collect()
    }

    pub fn get_recently_played(&self, limit: u32) -> Result<Vec<TrackItem>, CacheError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.title, t.artist, t.artist_id, t.album, t.album_id, t.duration_secs, t.cover_url, t.audio_quality, t.explicit, t.artists
             FROM history h
             JOIN tracks t ON h.track_id = t.id
             ORDER BY h.played_at DESC
             LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit], Self::row_to_track)?;
        let mut result = Vec::new();
        for item in rows {
            result.push(item?);
        }
        Ok(result)
    }

    /// Persist the full playback queue, play order, and transport/volume state in one transaction.
    pub fn save_playback_queue(&self, state: &SavedPlayback) -> Result<(), CacheError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for track in &state.queue {
            Self::upsert_track_conn(&tx, track, now)?;
        }
        tx.execute("DELETE FROM playback_order", [])?;
        tx.execute("DELETE FROM playback_queue", [])?;
        {
            let mut ins_q =
                tx.prepare("INSERT INTO playback_queue (queue_idx, track_id) VALUES (?1, ?2)")?;
            for (idx, track) in state.queue.iter().enumerate() {
                ins_q.execute(params![idx as i64, track.id])?;
            }
        }
        {
            let mut ins_o =
                tx.prepare("INSERT INTO playback_order (play_pos, queue_idx) VALUES (?1, ?2)")?;
            for (play_pos, &queue_idx) in state.order.iter().enumerate() {
                ins_o.execute(params![play_pos as i64, queue_idx as i64])?;
            }
        }
        tx.execute(
            "INSERT INTO playback_session (id, pos, elapsed_ms, context, shuffle, repeat_mode, volume, muted, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                pos = excluded.pos,
                elapsed_ms = excluded.elapsed_ms,
                context = excluded.context,
                shuffle = excluded.shuffle,
                repeat_mode = excluded.repeat_mode,
                volume = excluded.volume,
                muted = excluded.muted,
                updated_at = excluded.updated_at",
            params![
                state.pos as i64,
                state.elapsed_ms as i64,
                state.context,
                i32::from(state.shuffle),
                state.repeat_mode,
                f64::from(state.volume),
                i32::from(state.muted),
                now,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Persist play order + session state when shuffle toggles without rewriting `playback_queue`.
    pub fn update_playback_order(
        &self,
        order: &[usize],
        cursor: PlaybackCursor,
    ) -> Result<(), CacheError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM playback_order", [])?;
        {
            let mut ins_o =
                tx.prepare("INSERT INTO playback_order (play_pos, queue_idx) VALUES (?1, ?2)")?;
            for (play_pos, &queue_idx) in order.iter().enumerate() {
                ins_o.execute(params![play_pos as i64, queue_idx as i64])?;
            }
        }
        Self::upsert_cursor_conn(&tx, cursor, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Lightweight single-row update for transport cursor, timestamp, repeat, volume, and mute.
    pub fn update_playback_cursor(&self, cursor: PlaybackCursor) -> Result<(), CacheError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = self.conn.lock().unwrap();
        Self::upsert_cursor_conn(&conn, cursor, now)
    }

    fn upsert_cursor_conn(
        conn: &Connection,
        cursor: PlaybackCursor,
        now: i64,
    ) -> Result<(), CacheError> {
        conn.execute(
            "INSERT INTO playback_session (id, pos, elapsed_ms, context, shuffle, repeat_mode, volume, muted, updated_at)
             VALUES (1, ?1, ?2, '', ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                pos = excluded.pos,
                elapsed_ms = excluded.elapsed_ms,
                shuffle = excluded.shuffle,
                repeat_mode = excluded.repeat_mode,
                volume = excluded.volume,
                muted = excluded.muted,
                updated_at = excluded.updated_at",
            params![
                cursor.pos as i64,
                cursor.elapsed_ms as i64,
                i32::from(cursor.shuffle),
                cursor.repeat_mode,
                f64::from(cursor.volume),
                i32::from(cursor.muted),
                now,
            ],
        )?;
        Ok(())
    }

    /// Reconstruct the saved playback session, queue, and play order on startup.
    pub fn load_playback_state(&self) -> Result<Option<SavedPlayback>, CacheError> {
        let conn = self.conn.lock().unwrap();
        let Ok((pos, elapsed_ms, context, shuffle, repeat_mode, volume, muted)) = conn.query_row(
            "SELECT pos, elapsed_ms, context, shuffle, repeat_mode, volume, muted FROM playback_session WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?.max(0) as usize,
                    r.get::<_, i64>(1)?.max(0) as u64,
                    r.get::<_, String>(2)?,
                    r.get::<_, i32>(3)? != 0,
                    r.get::<_, i32>(4)?.clamp(0, 2),
                    (r.get::<_, f64>(5)? as f32).clamp(0.0, 1.0),
                    r.get::<_, i32>(6)? != 0,
                ))
            },
        ) else {
            return Ok(None);
        };

        let mut stmt_q = conn.prepare(
            "SELECT t.id, t.title, t.artist, t.artist_id, t.album, t.album_id, t.duration_secs, t.cover_url, t.audio_quality, t.explicit, t.artists
             FROM playback_queue q
             JOIN tracks t ON q.track_id = t.id
             ORDER BY q.queue_idx ASC",
        )?;
        let queue: Vec<TrackItem> = stmt_q
            .query_map([], Self::row_to_track)?
            .collect::<rusqlite::Result<_>>()?;

        let mut stmt_o =
            conn.prepare("SELECT queue_idx FROM playback_order ORDER BY play_pos ASC")?;
        let mut order: Vec<usize> = stmt_o
            .query_map([], |r| Ok(r.get::<_, i64>(0)?.max(0) as usize))?
            .filter_map(Result::ok)
            .filter(|&idx| idx < queue.len())
            .collect();
        if !queue.is_empty() && order.len() != queue.len() {
            order = (0..queue.len()).collect();
        }
        let clamped_pos = if order.is_empty() {
            0
        } else {
            pos.min(order.len() - 1)
        };

        Ok(Some(SavedPlayback {
            queue,
            order,
            pos: clamped_pos,
            elapsed_ms,
            context,
            shuffle,
            repeat_mode,
            volume,
            muted,
        }))
    }

    /// Flush only the live playback elapsed time on process exit (safe from `atexit`).
    pub fn update_playback_elapsed(&self, elapsed_ms: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Ok(conn) = self.conn.try_lock() {
            let _ = conn.execute(
                "UPDATE playback_session SET elapsed_ms = ?1, updated_at = ?2 WHERE id = 1",
                params![elapsed_ms as i64, now],
            );
        }
    }

    pub fn clear_playback_state(&self) -> Result<(), CacheError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM playback_order", [])?;
        tx.execute("DELETE FROM playback_queue", [])?;
        tx.execute("DELETE FROM playback_session", [])?;
        tx.commit()?;
        Ok(())
    }
}

pub struct AudioCache {
    base_dir: PathBuf,
    max_bytes: std::sync::atomic::AtomicU64,
    /// Track/quality keys with a progressive download still appending their `.tmp`.
    live_partials: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

fn partial_key(track_id: &str, quality: &str) -> String {
    format!("{track_id}\0{quality}")
}

impl AudioCache {
    pub fn new() -> Result<Self, CacheError> {
        let base_dir = default_audio_cache_dir()?;
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self {
            base_dir,
            max_bytes: (20u64 * 1024 * 1024 * 1024).into(), // 20 GB default
            live_partials: Default::default(),
        })
    }

    pub fn with_dir(dir: PathBuf, max_bytes: u64) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            base_dir: dir,
            max_bytes: max_bytes.into(),
            live_partials: Default::default(),
        })
    }

    pub fn mark_partial_live(&self, track_id: &str, quality: &str) {
        self.live_partials
            .lock()
            .unwrap()
            .insert(partial_key(track_id, quality));
    }

    pub fn clear_partial_live(&self, track_id: &str, quality: &str) {
        self.live_partials
            .lock()
            .unwrap()
            .remove(&partial_key(track_id, quality));
    }

    fn is_partial_live(&self, track_id: &str, quality: &str) -> bool {
        self.live_partials
            .lock()
            .unwrap()
            .contains(&partial_key(track_id, quality))
    }

    pub fn file_path(&self, track_id: &str, quality: &str) -> PathBuf {
        let safe_quality = quality.replace([' ', '/'], "_");
        self.base_dir
            .join(format!("{}_{}.audio", track_id, safe_quality))
    }

    pub fn get_cached(&self, track_id: &str, quality: &str) -> Option<PathBuf> {
        let path = self.file_path(track_id, quality);
        if path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            // Touch access time
            let _ = std::fs::File::open(&path);
            Some(path)
        } else {
            None
        }
    }

    /// Growing cache file while a playback download is still appending segments.
    pub fn partial_path(&self, track_id: &str, quality: &str) -> PathBuf {
        self.file_path(track_id, quality).with_extension("tmp")
    }

    /// Cached audio, or a live progressive `.tmp` that a download is still filling.
    /// Orphan `.tmp` files (left after a crashed/killed download) are deleted and ignored.
    pub fn get_playable(&self, track_id: &str, quality: &str) -> Option<PathBuf> {
        if let Some(path) = self.get_cached(track_id, quality) {
            return Some(path);
        }
        let tmp = self.partial_path(track_id, quality);
        if !tmp.exists() {
            return None;
        }
        if self.is_partial_live(track_id, quality)
            && tmp.metadata().map(|m| m.len() > 0).unwrap_or(false)
        {
            return Some(tmp);
        }
        let _ = std::fs::remove_file(&tmp);
        None
    }

    pub async fn fetch_or_cache(
        &self,
        track_id: &str,
        quality: &str,
        urls: &[String],
    ) -> Result<PathBuf, CacheError> {
        self.fetch_or_cache_with(
            track_id,
            quality,
            urls,
            false,
            std::sync::Arc::new(|_, _, _| {}),
        )
        .await
    }

    /// `on_chunk(index, state, bytes_so_far)` — state `1` in flight, `2` received.
    /// When `stream_for_playback` is true, returns after [`PLAYBACK_READY_SEGMENTS`]
    /// and keeps downloading the rest in the background.
    pub async fn fetch_or_cache_with(
        &self,
        track_id: &str,
        quality: &str,
        urls: &[String],
        stream_for_playback: bool,
        on_chunk: std::sync::Arc<dyn Fn(usize, u8, u64) + Send + Sync>,
    ) -> Result<PathBuf, CacheError> {
        if let Some(cached) = self.get_cached(track_id, quality) {
            tracing::info!(track_id = %track_id, %quality, "Audio cache HIT: streaming offline from disk");
            return Ok(cached);
        }

        let start = std::time::Instant::now();
        let num_segments = urls.len();
        let ready_end = if stream_for_playback {
            num_segments.min(PLAYBACK_READY_SEGMENTS)
        } else {
            num_segments
        };
        tracing::info!(
            track_id = %track_id,
            %quality,
            segments = num_segments,
            stream_for_playback,
            ready_end,
            "Audio cache MISS: downloading DASH segments"
        );
        let dest = self.file_path(track_id, quality);
        let tmp = dest.with_extension("tmp");
        // Drop any orphan from a previous crashed download before truncating.
        if tmp.exists() && !self.is_partial_live(track_id, quality) {
            let _ = std::fs::remove_file(&tmp);
        }

        let client = reqwest::Client::new();
        let mut file = std::fs::File::create(&tmp)?;
        let mut total_bytes = 0u64;
        let hook = on_chunk.as_ref();

        write_url_range(
            &client,
            &mut file,
            urls,
            0,
            ready_end,
            hook,
            &mut total_bytes,
        )
        .await?;

        let streaming = stream_for_playback && ready_end < num_segments;
        if streaming {
            use std::io::Write;
            file.flush()?;
            drop(file);
            self.mark_partial_live(track_id, quality);
            let head_ms = start.elapsed().as_millis();
            tracing::info!(
                track_id = %track_id,
                ready_end,
                head_ms,
                "Playback can start; caching remaining segments in background"
            );
            let urls_rest = urls.to_vec();
            let track = track_id.to_string();
            let quality_owned = quality.to_string();
            let chunk_cb = on_chunk.clone();
            let tmp_bg = tmp.clone();
            let dest_bg = dest.clone();
            let live = self.live_partials.clone();
            let live_key = partial_key(track_id, quality);
            tokio::spawn(async move {
                let clear_live = || {
                    live.lock().unwrap().remove(&live_key);
                };
                let client = reqwest::Client::new();
                let open = std::fs::OpenOptions::new().append(true).open(&tmp_bg);
                let Ok(mut file) = open else {
                    tracing::error!(track_id = %track, "could not reopen partial cache for append");
                    clear_live();
                    return;
                };
                let mut tail_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
                let hook = chunk_cb.as_ref();
                if let Err(e) = write_url_range(
                    &client,
                    &mut file,
                    &urls_rest,
                    ready_end,
                    urls_rest.len(),
                    hook,
                    &mut tail_bytes,
                )
                .await
                {
                    tracing::error!(track_id = %track, "background cache download failed: {e}");
                    let _ = std::fs::remove_file(&tmp_bg);
                    clear_live();
                    return;
                }
                drop(file);
                if let Err(e) = std::fs::rename(&tmp_bg, &dest_bg) {
                    tracing::error!(track_id = %track, "could not finalize audio cache: {e}");
                    clear_live();
                    return;
                }
                clear_live();
                tracing::info!(
                    track_id = %track,
                    quality = %quality_owned,
                    bytes = tail_bytes,
                    segments = urls_rest.len(),
                    "Finished downloading track to cache"
                );
            });
            return Ok(tmp);
        }

        drop(file);
        std::fs::rename(&tmp, &dest)?;
        let elapsed = start.elapsed();
        let mb = total_bytes as f64 / (1024.0 * 1024.0);
        let speed_mb_s = if elapsed.as_secs_f64() > 0.0 {
            mb / elapsed.as_secs_f64()
        } else {
            0.0
        };
        tracing::info!(
            track_id = %track_id,
            bytes = total_bytes,
            segments = num_segments,
            duration_ms = elapsed.as_millis(),
            speed = format!("{:.2} MB/s", speed_mb_s),
            "Finished downloading track to cache"
        );
        Ok(dest)
    }

    /// Number of cached tracks and their total size in bytes.
    pub fn usage(&self) -> (usize, u64) {
        std::fs::read_dir(&self.base_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.metadata().ok())
                    .filter(|m| m.is_file())
                    .fold((0, 0), |(n, bytes), m| (n + 1, bytes + m.len()))
            })
            .unwrap_or((0, 0))
    }

    /// Delete every cached audio file.
    pub fn clear(&self) -> Result<(), CacheError> {
        for entry in std::fs::read_dir(&self.base_dir)?.flatten() {
            if entry.metadata().is_ok_and(|m| m.is_file()) {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_max_bytes(&self, max_bytes: u64) {
        self.max_bytes
            .store(max_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Track id of a cache file named `<track id>_<QUALITY>.audio`.
    fn track_id_of(path: &std::path::Path) -> Option<String> {
        if path.extension().and_then(|s| s.to_str()) != Some("audio") {
            return None;
        }
        path.file_stem()?
            .to_str()?
            .rsplit_once('_')
            .map(|(id, _)| id.to_string())
    }

    /// Bring the cache back under its size limit, least recently used first.
    pub fn enforce_quota(&self) -> Result<(), CacheError> {
        self.evict(&std::collections::HashMap::new(), &[])
            .map(|_| ())
    }

    /// Bring the cache back under its size limit, deleting the tracks that are
    /// least worth keeping: lowest play score first, then least recently used.
    /// Tracks in `protect` (playing or about to play) are never deleted.
    /// Returns how many files were removed.
    pub fn evict(
        &self,
        scores: &std::collections::HashMap<String, f64>,
        protect: &[String],
    ) -> Result<usize, CacheError> {
        let max_bytes = self.max_bytes();
        let mut files = Vec::new();
        let mut total_size = 0u64;
        for entry in std::fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(track_id) = Self::track_id_of(&path) else {
                continue;
            };
            let meta = entry.metadata()?;
            total_size += meta.len();
            let score = scores.get(&track_id).copied().unwrap_or(0.0);
            files.push((
                path,
                meta.len(),
                score,
                meta.accessed().unwrap_or(UNIX_EPOCH),
                track_id,
            ));
        }
        if total_size <= max_bytes {
            return Ok(0);
        }
        files.sort_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.3.cmp(&b.3)));
        let mut removed = 0;
        for (path, size, _, _, track_id) in files {
            if total_size <= max_bytes {
                break;
            }
            if protect.contains(&track_id) {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                total_size = total_size.saturating_sub(size);
                removed += 1;
            }
        }
        Ok(removed)
    }
}

pub struct ImageCache {
    base_dir: PathBuf,
}

impl ImageCache {
    pub fn new() -> Result<Self, CacheError> {
        let base_dir = default_image_cache_dir()?;
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    pub fn file_path(&self, id: &str) -> PathBuf {
        let safe_id = id.replace(['/', ':'], "_");
        self.base_dir.join(format!("{}.jpg", safe_id))
    }

    pub async fn get_or_download(&self, id: &str, url: &str) -> Result<PathBuf, CacheError> {
        let dest = self.file_path(id);
        if dest.exists() && dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            return Ok(dest);
        }

        let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
        std::fs::write(&dest, &bytes)?;
        Ok(dest)
    }
}

pub fn default_db_path() -> Result<PathBuf, CacheError> {
    let base = dirs::data_dir().ok_or(CacheError::NoDir)?;
    Ok(base.join("tidal-rust").join("library.db"))
}

pub fn default_audio_cache_dir() -> Result<PathBuf, CacheError> {
    let base = dirs::cache_dir().ok_or(CacheError::NoDir)?;
    Ok(base.join("tidal-rust").join("audio"))
}

pub fn default_image_cache_dir() -> Result<PathBuf, CacheError> {
    let base = dirs::cache_dir().ok_or(CacheError::NoDir)?;
    Ok(base.join("tidal-rust").join("images"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_cache_favorites() {
        let cache = MetadataCache::in_memory().unwrap();
        let track = TrackItem {
            id: "1001".to_string(),
            title: "Test Track".to_string(),
            artist: "Test Artist".to_string(),
            artist_id: Some("2001".to_string()),
            album: "Test Album".to_string(),
            album_id: Some("3001".to_string()),
            duration_secs: 240,
            cover_url: Some("https://example.com/cover.jpg".to_string()),
            audio_quality: "HI_RES".to_string(),
            ..Default::default()
        };

        cache.save_favorites(std::slice::from_ref(&track)).unwrap();
        let favs = cache.get_favorites().unwrap();
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].id, "1001");
        assert_eq!(favs[0].title, "Test Track");

        cache.record_play("1001").unwrap();
        let history = cache.get_recently_played(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "1001");
    }

    #[test]
    fn test_audio_cache_path() {
        let dir = std::env::temp_dir().join("test_tidal_audio_cache");
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).unwrap();
        let path = cache.file_path("12345", "HI_RES");
        assert!(path.to_str().unwrap().contains("12345_HI_RES.audio"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphaned_tmp_is_not_playable_and_is_removed() {
        let dir = std::env::temp_dir().join(format!("tide_orphan_tmp_{}", std::process::id()));
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).unwrap();
        let tmp = cache.partial_path("560201309", "MAX");
        std::fs::write(&tmp, vec![1u8; 64]).unwrap();

        assert!(cache.get_playable("560201309", "MAX").is_none());
        assert!(
            !tmp.exists(),
            "orphan .tmp must be deleted so the next fetch starts clean"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn live_partial_tmp_is_playable() {
        let dir = std::env::temp_dir().join(format!("tide_live_tmp_{}", std::process::id()));
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).unwrap();
        let tmp = cache.partial_path("560201309", "MAX");
        std::fs::write(&tmp, vec![1u8; 64]).unwrap();
        cache.mark_partial_live("560201309", "MAX");

        let playable = cache.get_playable("560201309", "MAX").unwrap();
        assert_eq!(playable, tmp);
        assert!(tmp.exists());

        cache.clear_partial_live("560201309", "MAX");
        assert!(cache.get_playable("560201309", "MAX").is_none());
        assert!(!tmp.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn write_cached(cache: &AudioCache, id: &str, bytes: usize) {
        std::fs::write(cache.file_path(id, "MAX"), vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn eviction_keeps_most_played_and_protected_tracks() {
        let dir = std::env::temp_dir().join(format!("tide_evict_{}", std::process::id()));
        let cache = AudioCache::with_dir(dir.clone(), 250).unwrap();
        for id in ["rare", "fav", "never", "playing"] {
            write_cached(&cache, id, 100);
        }
        let scores: std::collections::HashMap<String, f64> =
            [("fav".to_string(), 9.0), ("rare".to_string(), 0.4)]
                .into_iter()
                .collect();

        // 400 bytes on disk, limit 250: two files must go. "never" (score 0) and
        // "playing" (score 0) are the cheapest, but "playing" is protected, so
        // the next cheapest, "rare", goes instead.
        let removed = cache.evict(&scores, &["playing".to_string()]).unwrap();
        assert_eq!(removed, 2);
        assert!(cache.get_cached("fav", "MAX").is_some());
        assert!(cache.get_cached("playing", "MAX").is_some());
        assert!(cache.get_cached("never", "MAX").is_none());
        assert!(cache.get_cached("rare", "MAX").is_none());

        // Raising the limit stops eviction.
        cache.set_max_bytes(10_000);
        assert_eq!(cache.evict(&scores, &[]).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn play_scores_favour_recent_and_frequent_plays() {
        let cache = MetadataCache::in_memory().unwrap();
        let now = 1_800_000_000i64;
        let day = 86_400i64;
        for id in ["old-hit", "current", "once"] {
            cache
                .upsert_track(&TrackItem {
                    id: id.into(),
                    ..Default::default()
                })
                .unwrap();
        }
        {
            let conn = cache.conn.lock().unwrap();
            let add = |id: &str, at: i64| {
                conn.execute(
                    "INSERT INTO history (track_id, played_at) VALUES (?1, ?2)",
                    params![id, at],
                )
                .unwrap();
            };
            add("old-hit", now - 300 * day); // three plays, but ten half-lives ago
            add("old-hit", now - 300 * day);
            add("old-hit", now - 300 * day);
            add("current", now - day); // two recent plays
            add("current", now);
            add("once", now - 30 * day); // exactly one half-life ago
        }
        let scores = cache.play_scores_at(now);
        assert!((scores["once"] - 0.5).abs() < 1e-9);
        assert!(scores["current"] > 1.9);
        assert!(scores["old-hit"] < 0.01);
        assert!(scores["current"] > scores["once"] && scores["once"] > scores["old-hit"]);

        cache.record_cache_lookup("current", true).unwrap();
        cache.record_cache_lookup("current", true).unwrap();
        cache.record_cache_lookup("once", false).unwrap();
        assert_eq!(cache.cache_hit_stats(), (2, 1));
    }

    #[test]
    fn playback_queue_and_cursor_roundtrip_in_sqlite() {
        let cache = MetadataCache::in_memory().unwrap();
        assert!(cache.load_playback_state().unwrap().is_none());

        let mk = |id: &str, title: &str, explicit: bool| TrackItem {
            id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration_secs: 210,
            audio_quality: "HI_RES".into(),
            explicit,
            ..Default::default()
        };
        // Includes duplicate track IDs ("1" appears twice) and shuffled order.
        let queue = vec![
            mk("1", "One", true),
            mk("2", "Two", false),
            mk("1", "One", true),
        ];
        let order = vec![1, 2, 0];
        cache
            .save_playback_queue(&SavedPlayback {
                queue: queue.clone(),
                order: order.clone(),
                pos: 0,
                elapsed_ms: 42_500,
                context: "GTAVI: The Album".into(),
                shuffle: true,
                repeat_mode: 1,
                volume: 0.65,
                muted: false,
            })
            .unwrap();

        let loaded = cache.load_playback_state().unwrap().unwrap();
        assert_eq!(loaded.queue.len(), 3);
        assert_eq!(loaded.queue[0].id, "1");
        assert!(loaded.queue[0].explicit);
        assert_eq!(loaded.queue[1].id, "2");
        assert!(!loaded.queue[1].explicit);
        assert_eq!(loaded.queue[2].id, "1");
        assert_eq!(loaded.order, vec![1, 2, 0]);
        assert_eq!(loaded.pos, 0);
        assert_eq!(loaded.elapsed_ms, 42_500);
        assert_eq!(loaded.context, "GTAVI: The Album");
        assert!(loaded.shuffle);
        assert_eq!(loaded.repeat_mode, 1);
        assert!((loaded.volume - 0.65).abs() < 1e-5);
        assert!(!loaded.muted);

        // Update cursor & unshuffle order without rewriting queue_idx rows.
        cache
            .update_playback_order(
                &[0, 1, 2],
                PlaybackCursor {
                    pos: 1,
                    elapsed_ms: 99_000,
                    shuffle: false,
                    repeat_mode: 2,
                    volume: 0.65,
                    muted: true,
                },
            )
            .unwrap();
        cache.update_playback_elapsed(105_250);

        let updated = cache.load_playback_state().unwrap().unwrap();
        assert_eq!(updated.order, vec![0, 1, 2]);
        assert_eq!(updated.pos, 1);
        assert_eq!(updated.elapsed_ms, 105_250);
        assert_eq!(updated.context, "GTAVI: The Album");
        assert!(!updated.shuffle);
        assert_eq!(updated.repeat_mode, 2);
        assert!(updated.muted);
        assert!((updated.volume - 0.65).abs() < 1e-5);

        cache.clear_playback_state().unwrap();
        assert!(cache.load_playback_state().unwrap().is_none());
    }
}
