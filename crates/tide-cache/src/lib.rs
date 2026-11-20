use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
const DOWNLOAD_WINDOW: usize = 8;

/// `urls[0]` is the DASH init segment; the rest are media. Maps a scrubber
/// time onto the media URL that should be fetched first.
///
/// Segments are fixed-length with a shorter last one, so spreading the track
/// evenly over them overshoots by up to a block late in the track. Start one
/// block early so the playable head begins at or before `at_secs`.
pub fn media_index_for_time(url_count: usize, duration_secs: f64, at_secs: f64) -> usize {
    if url_count <= 1 {
        return 0;
    }
    let media = url_count - 1;
    let frac = if duration_secs <= 0.0 {
        0.0
    } else {
        (at_secs / duration_secs).clamp(0.0, 0.999_999)
    };
    1 + ((frac * media as f64).floor() as usize).saturating_sub(1)
}

/// Next `limit` undownloaded URL indices: init first, then the seek block
/// running toward the end, then earlier gaps.
pub fn next_download_indices(have: &[bool], priority: usize, limit: usize) -> Vec<usize> {
    if have.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(limit);
    let push = |out: &mut Vec<usize>, i: usize| {
        if i < have.len() && !have[i] && !out.contains(&i) && out.len() < limit {
            out.push(i);
        }
    };
    push(&mut out, 0);
    let start = priority.max(1).min(have.len() - 1);
    for i in start..have.len() {
        push(&mut out, i);
        if out.len() == limit {
            return out;
        }
    }
    for i in 1..start {
        push(&mut out, i);
        if out.len() == limit {
            return out;
        }
    }
    out
}

fn missing_playback_head(have: &[bool], from: usize) -> Vec<usize> {
    if have.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if !have[0] {
        out.push(0);
    }
    let start = from.max(1);
    let need = PLAYBACK_READY_SEGMENTS.saturating_sub(1);
    for i in (start..have.len()).take(need) {
        if !have[i] {
            out.push(i);
        }
    }
    out
}

fn playback_head_ready(have: &[bool], from: usize) -> bool {
    if have.is_empty() || !have[0] {
        return false;
    }
    let start = from.max(1);
    if start >= have.len() {
        return true;
    }
    let need = PLAYBACK_READY_SEGMENTS.saturating_sub(1);
    let available: Vec<usize> = (start..have.len()).take(need).collect();
    !available.is_empty() && available.iter().all(|&i| have[i])
}

fn part_file(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("{index:04}"))
}

fn write_playable_concat(
    parts_dir: &Path,
    dest: &Path,
    from: usize,
    url_count: usize,
) -> Result<usize, std::io::Error> {
    use std::io::Write;
    let mut file = std::fs::File::create(dest)?;
    let mut n = 0usize;
    let mut write_idx = |file: &mut std::fs::File, i: usize| -> std::io::Result<bool> {
        let path = part_file(parts_dir, i);
        if !path.exists() {
            return Ok(false);
        }
        file.write_all(&std::fs::read(path)?)?;
        n += 1;
        Ok(true)
    };
    if url_count == 0 {
        return Ok(0);
    }
    if !write_idx(&mut file, 0)? {
        return Ok(0);
    }
    let start = from.max(1);
    for i in start..url_count {
        if !write_idx(&mut file, i)? {
            break;
        }
    }
    file.flush()?;
    Ok(n)
}

fn contiguous_through(have: &[bool], from: usize) -> usize {
    let start = from.max(1);
    if start >= have.len() || !have.get(start).copied().unwrap_or(false) {
        return start.saturating_sub(1);
    }
    let mut end = start;
    while end + 1 < have.len() && have[end + 1] {
        end += 1;
    }
    end
}

/// Write the finished track next to `dest` and rename it into place, so a
/// reader never sees a half-written `.audio`.
fn finalize_concat(parts_dir: &Path, dest: &Path, url_count: usize) -> std::io::Result<()> {
    let staged = dest.with_extension("audio.new");
    write_playable_concat(parts_dir, &staged, 1, url_count)?;
    std::fs::rename(&staged, dest)
}

fn append_part(tmp: &Path, part: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(tmp)?;
    file.write_all(&std::fs::read(part)?)?;
    Ok(())
}

#[derive(Default)]
struct LiveDownload {
    requested_ms: AtomicU64,
    duration_ms: AtomicU64,
    url_count: AtomicUsize,
    playable_from: AtomicUsize,
    playable_gen: AtomicU64,
    appended_through: AtomicUsize,
}

impl LiveDownload {
    /// True when the playable `.tmp` already holds media block `index`.
    fn covers(&self, index: usize) -> bool {
        let from = self.playable_from.load(Ordering::Relaxed);
        from != 0 && from <= index && index <= self.appended_through.load(Ordering::Relaxed)
    }

    fn index(&self) -> usize {
        media_index_for_time(
            self.url_count.load(Ordering::Relaxed),
            self.duration_ms.load(Ordering::Relaxed) as f64 / 1000.0,
            self.requested_ms.load(Ordering::Relaxed) as f64 / 1000.0,
        )
    }
}

async fn download_indices(
    client: &reqwest::Client,
    urls: &[String],
    indices: &[usize],
    parts_dir: &Path,
    on_chunk: &(dyn Fn(usize, u8, u64) + Send + Sync),
    total_bytes: &mut u64,
    have: &mut [bool],
) -> Result<(), CacheError> {
    if indices.is_empty() {
        return Ok(());
    }
    for &i in indices {
        on_chunk(i, 1, *total_bytes);
    }
    let mut set = tokio::task::JoinSet::new();
    for &i in indices {
        let (client, url) = (client.clone(), urls[i].clone());
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
    while let Some(joined) = set.join_next().await {
        let (i, bytes) = joined.map_err(|e| std::io::Error::other(e.to_string()))??;
        std::fs::write(part_file(parts_dir, i), &bytes)?;
        *total_bytes += bytes.len() as u64;
        if let Some(slot) = have.get_mut(i) {
            *slot = true;
        }
        on_chunk(i, 2, *total_bytes);
    }
    Ok(())
}

fn publish_playable(
    parts_dir: &Path,
    tmp: &Path,
    have: &[bool],
    live: &LiveDownload,
    url_count: usize,
    replace: bool,
) -> Result<(), CacheError> {
    let from = live.index().max(1);
    let prev_from = live.playable_from.load(Ordering::Relaxed);
    let prev_through = live.appended_through.load(Ordering::Relaxed);
    // Rebuild only when the seek block is outside the current file (and not
    // the next block it appends): the player keeps reading the file it has.
    let outside = prev_from == 0 || from < prev_from || from > prev_through + 1;
    if replace || outside || !tmp.exists() {
        if !playback_head_ready(have, from) {
            return Ok(());
        }
        let through = contiguous_through(have, from);
        let staged = tmp.with_extension("tmp.new");
        write_playable_concat(parts_dir, &staged, from, url_count)?;
        std::fs::rename(&staged, tmp)?;
        live.playable_from.store(from, Ordering::Relaxed);
        live.appended_through.store(through, Ordering::Relaxed);
        live.playable_gen.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let mut through_have = live.appended_through.load(Ordering::Relaxed);
    while through_have + 1 < url_count && have.get(through_have + 1).copied() == Some(true) {
        append_part(tmp, &part_file(parts_dir, through_have + 1))?;
        through_have += 1;
        live.appended_through.store(through_have, Ordering::Relaxed);
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
        // Account data goes; app preferences, the local pins and the EQs (which
        // belong to this computer's sound outputs, not to the account) stay.
        self.conn.lock().unwrap().execute(
            "DELETE FROM kv WHERE key NOT IN ('settings', 'dsp', 'autoeq_index', 'pins')",
            [],
        )?;
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
    live: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<LiveDownload>>>>,
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
            live: Default::default(),
        })
    }

    pub fn with_dir(dir: PathBuf, max_bytes: u64) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            base_dir: dir,
            max_bytes: max_bytes.into(),
            live_partials: Default::default(),
            live: Default::default(),
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

    fn live_for(&self, track_id: &str, quality: &str) -> Arc<LiveDownload> {
        self.live
            .lock()
            .unwrap()
            .entry(partial_key(track_id, quality))
            .or_default()
            .clone()
    }

    fn drop_live(&self, track_id: &str, quality: &str) {
        self.live
            .lock()
            .unwrap()
            .remove(&partial_key(track_id, quality));
        self.clear_partial_live(track_id, quality);
    }

    /// Point the in-flight DASH fetch at `at` so that block is downloaded first.
    pub fn request_time(&self, track_id: &str, quality: &str, at: Duration, duration: Duration) {
        let live = self.live_for(track_id, quality);
        live.requested_ms
            .store(at.as_millis() as u64, Ordering::Relaxed);
        live.duration_ms
            .store(duration.as_millis() as u64, Ordering::Relaxed);
    }

    /// URL index the current playable `.tmp` was built from (0 if none).
    pub fn playable_index(&self, track_id: &str, quality: &str) -> usize {
        self.live
            .lock()
            .unwrap()
            .get(&partial_key(track_id, quality))
            .map(|live| live.playable_from.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// True when the bytes for the requested seek block can be played now:
    /// the track is fully cached or the playable `.tmp` holds that block.
    pub fn requested_ready(&self, track_id: &str, quality: &str) -> bool {
        if self.get_cached(track_id, quality).is_some() {
            return true;
        }
        self.live
            .lock()
            .unwrap()
            .get(&partial_key(track_id, quality))
            .is_some_and(|live| live.covers(live.index()))
    }

    pub fn playable_generation(&self, track_id: &str, quality: &str) -> u64 {
        self.live
            .lock()
            .unwrap()
            .get(&partial_key(track_id, quality))
            .map(|live| live.playable_gen.load(Ordering::Relaxed))
            .unwrap_or(0)
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
    /// When `stream_for_playback` is true, returns once the current seek block
    /// (init + a couple of media segments at [`AudioCache::request_time`]) is on
    /// disk and keeps filling the rest in the background.
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

        if !stream_for_playback && self.is_partial_live(track_id, quality) {
            // A playback download is still filling this track in the background
            // (and its `.tmp` may start mid-track). Let it finish instead of
            // downloading the same segments twice.
            while self.is_partial_live(track_id, quality) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            if let Some(cached) = self.get_cached(track_id, quality) {
                return Ok(cached);
            }
        }

        let start = std::time::Instant::now();
        let num_segments = urls.len();
        let dest = self.file_path(track_id, quality);
        let tmp = dest.with_extension("tmp");
        let parts_dir = dest.with_extension("parts");
        std::fs::create_dir_all(&parts_dir)?;
        if tmp.exists() && !self.is_partial_live(track_id, quality) {
            let _ = std::fs::remove_file(&tmp);
        }

        let live = self.live_for(track_id, quality);
        live.url_count.store(num_segments, Ordering::Relaxed);
        tracing::info!(
            track_id = %track_id,
            %quality,
            segments = num_segments,
            stream_for_playback,
            priority = live.index(),
            "Audio cache MISS: downloading DASH segments"
        );

        let client = reqwest::Client::new();
        let mut have = vec![false; num_segments];
        let mut total_bytes = 0u64;
        let hook = on_chunk.as_ref();
        for (i, slot) in have.iter_mut().enumerate() {
            if let Ok(meta) = std::fs::metadata(part_file(&parts_dir, i)) {
                *slot = true;
                total_bytes += meta.len();
                hook(i, 2, total_bytes);
            }
        }

        if have.iter().all(|h| *h) && num_segments > 0 {
            finalize_concat(&parts_dir, &dest, num_segments)?;
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_dir_all(&parts_dir);
            self.drop_live(track_id, quality);
            return Ok(dest);
        }

        if stream_for_playback && playback_head_ready(&have, live.index()) {
            publish_playable(&parts_dir, &tmp, &have, &live, num_segments, true)?;
            self.mark_partial_live(track_id, quality);
            self.spawn_fill(
                track_id,
                quality,
                urls,
                parts_dir,
                tmp.clone(),
                dest,
                on_chunk,
                live,
                have,
                total_bytes,
            );
            return Ok(tmp);
        }

        loop {
            let priority = live.index();
            let batch = if stream_for_playback && !playback_head_ready(&have, priority) {
                missing_playback_head(&have, priority)
            } else {
                next_download_indices(&have, priority, DOWNLOAD_WINDOW)
            };
            if batch.is_empty() {
                break;
            }
            download_indices(
                &client,
                urls,
                &batch,
                &parts_dir,
                hook,
                &mut total_bytes,
                &mut have,
            )
            .await?;
            if stream_for_playback && playback_head_ready(&have, live.index()) {
                publish_playable(&parts_dir, &tmp, &have, &live, num_segments, true)?;
                let head_ms = start.elapsed().as_millis();
                tracing::info!(
                    track_id = %track_id,
                    priority = live.index(),
                    head_ms,
                    "Playback can start; caching remaining segments in background"
                );
                self.mark_partial_live(track_id, quality);
                self.spawn_fill(
                    track_id,
                    quality,
                    urls,
                    parts_dir,
                    tmp.clone(),
                    dest,
                    on_chunk,
                    live,
                    have,
                    total_bytes,
                );
                return Ok(tmp);
            }
        }

        finalize_concat(&parts_dir, &dest, num_segments)?;
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_dir_all(&parts_dir);
        self.drop_live(track_id, quality);
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

    #[allow(clippy::too_many_arguments)]
    fn spawn_fill(
        &self,
        track_id: &str,
        quality: &str,
        urls: &[String],
        parts_dir: PathBuf,
        tmp: PathBuf,
        dest: PathBuf,
        on_chunk: std::sync::Arc<dyn Fn(usize, u8, u64) + Send + Sync>,
        live: Arc<LiveDownload>,
        mut have: Vec<bool>,
        mut total_bytes: u64,
    ) {
        let urls = urls.to_vec();
        let track = track_id.to_string();
        let quality_owned = quality.to_string();
        let live_map = self.live.clone();
        let live_partials = self.live_partials.clone();
        let live_key = partial_key(track_id, quality);
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let hook = on_chunk.as_ref();
            loop {
                let priority = live.index();
                let batch = if !playback_head_ready(&have, priority) {
                    let head = missing_playback_head(&have, priority);
                    if head.is_empty() {
                        next_download_indices(&have, priority, DOWNLOAD_WINDOW)
                    } else {
                        head
                    }
                } else {
                    next_download_indices(&have, priority, DOWNLOAD_WINDOW)
                };
                if batch.is_empty() {
                    break;
                }
                if let Err(e) = download_indices(
                    &client,
                    &urls,
                    &batch,
                    &parts_dir,
                    hook,
                    &mut total_bytes,
                    &mut have,
                )
                .await
                {
                    tracing::error!(track_id = %track, "background cache download failed: {e}");
                    let _ = std::fs::remove_file(&tmp);
                    let _ = std::fs::remove_dir_all(&parts_dir);
                    live_partials.lock().unwrap().remove(&live_key);
                    live_map.lock().unwrap().remove(&live_key);
                    return;
                }
                if let Err(e) = publish_playable(&parts_dir, &tmp, &have, &live, urls.len(), false)
                {
                    tracing::warn!(track_id = %track, "could not refresh playable cache: {e}");
                }
            }
            if let Err(e) = finalize_concat(&parts_dir, &dest, urls.len()) {
                tracing::error!(track_id = %track, "could not finalize audio cache: {e}");
                live_partials.lock().unwrap().remove(&live_key);
                live_map.lock().unwrap().remove(&live_key);
                return;
            }
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_dir_all(&parts_dir);
            live_partials.lock().unwrap().remove(&live_key);
            live_map.lock().unwrap().remove(&live_key);
            tracing::info!(
                track_id = %track,
                quality = %quality_owned,
                bytes = total_bytes,
                segments = urls.len(),
                "Finished downloading track to cache"
            );
        });
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

    /// Folder that holds the cached audio files.
    pub fn dir(&self) -> &Path {
        &self.base_dir
    }

    /// Delete every cached audio file, keeping the folder itself and the
    /// `.tmp` / `.parts` of downloads still in flight (deleting those would
    /// break the track that is playing and fail its background fill).
    pub fn clear(&self) -> Result<(), CacheError> {
        std::fs::create_dir_all(&self.base_dir)?;
        let in_flight: std::collections::HashSet<std::ffi::OsString> = {
            let live = self.live.lock().unwrap();
            let partials = self.live_partials.lock().unwrap();
            live.keys()
                .chain(partials.iter())
                .filter_map(|key| key.split_once('\0'))
                .filter_map(|(track, quality)| {
                    self.file_path(track, quality)
                        .file_stem()
                        .map(|s| s.to_os_string())
                })
                .collect()
        };
        for entry in std::fs::read_dir(&self.base_dir)?.flatten() {
            let path = entry.path();
            if path
                .file_stem()
                .is_some_and(|stem| in_flight.contains(stem))
            {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else if entry.metadata().is_ok_and(|m| m.is_file()) {
                std::fs::remove_file(path)?;
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

/// Artwork on disk, keyed by what the picture is rather than the URL that served it.
///
/// TIDAL artwork URLs look like `https://resources.tidal.com/images/{uuid as a/b/c/d/e}/{W}x{H}.jpg`.
/// Every track of an album shares the album's uuid, so a 2000-track list needs one
/// request per album, not per track. Files are stored as `{uuid}_{W}x{H}.jpg`; a request
/// for a size we lack is served from a larger cached size of the same picture (the
/// decoder downsizes anyway) before going to the network.
pub struct ImageCache {
    base_dir: PathBuf,
    client: reqwest::Client,
    /// One download per file at a time; a second caller waits and then reads the file.
    inflight: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Set when the CDN answers 429/503: every download waits until then.
    cooldown_until: std::sync::Mutex<Option<tokio::time::Instant>>,
}

/// Square sizes the TIDAL image CDN serves, smallest first.
const ART_SIZES: [u32; 8] = [80, 160, 320, 480, 640, 750, 1080, 1280];
const ART_ATTEMPTS: u32 = 4;

/// `(uuid, size)` for a TIDAL image URL: `("cf78bf6b-3be0-…", "160x160")` or `(…, "origin")`.
pub fn art_identity(url: &str) -> Option<(String, String)> {
    let rest = url.split_once("://")?.1;
    let (host, path) = rest.split_once('/')?;
    if !host.ends_with("tidal.com") {
        return None;
    }
    let path = path.split(['?', '#']).next()?;
    let path = path.strip_prefix("images/")?;
    let (dir, file) = path.rsplit_once('/')?;
    let size = file.strip_suffix(".jpg")?;
    let parts: Vec<&str> = dir.split('/').collect();
    let ok = parts.len() == 5
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_hexdigit()));
    let size_ok = size == "origin"
        || size
            .split_once('x')
            .is_some_and(|(w, h)| w.parse::<u32>().is_ok() && h.parse::<u32>().is_ok());
    (ok && size_ok).then(|| (parts.join("-").to_ascii_lowercase(), size.to_string()))
}

fn art_px(size: &str) -> Option<u32> {
    let (w, h) = size.split_once('x')?;
    Some(w.parse::<u32>().ok()?.max(h.parse::<u32>().ok()?))
}

impl ImageCache {
    pub fn new() -> Result<Self, CacheError> {
        Self::with_dir(default_image_cache_dir()?)
    }

    pub fn with_dir(base_dir: PathBuf) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&base_dir)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(16)
            .build()?;
        Ok(Self {
            base_dir,
            client,
            inflight: Default::default(),
            cooldown_until: Default::default(),
        })
    }

    pub fn file_path(&self, id: &str) -> PathBuf {
        let safe_id = id.replace(['/', ':'], "_");
        self.base_dir.join(format!("{}.jpg", safe_id))
    }

    fn usable(path: &Path) -> bool {
        path.metadata().is_ok_and(|m| m.len() > 0)
    }

    /// A cached copy of the same picture at another size: the smallest one at least
    /// `px` (capped at 1280, so a thumbnail never decodes a multi-MB original), else
    /// with `allow_smaller` the largest one below it.
    fn other_size(&self, uuid: &str, px: u32, allow_smaller: bool) -> Option<PathBuf> {
        let mut smaller = None;
        for s in ART_SIZES {
            let path = self.file_path(&format!("{uuid}_{s}x{s}"));
            if !Self::usable(&path) {
                continue;
            }
            if s >= px {
                return Some(path);
            }
            smaller = Some(path);
        }
        if allow_smaller { smaller } else { None }
    }

    /// The file for `url` when it is already on disk (at its own size, a larger
    /// one, or under its `legacy_id`), without touching the network. Callers that
    /// throttle downloads check this first, so a cache hit never queues behind them.
    pub fn cached(&self, legacy_id: &str, url: &str) -> Option<PathBuf> {
        let identity = art_identity(url);
        let dest = self.file_path(&identity.as_ref().map_or_else(
            || legacy_id.to_string(),
            |(uuid, size)| format!("{uuid}_{size}"),
        ));
        if Self::usable(&dest) {
            return Some(dest);
        }
        if let Some((uuid, size)) = &identity
            && let Some(px) = art_px(size)
            && let Some(path) = self.other_size(uuid, px, false)
        {
            return Some(path);
        }
        let legacy = self.file_path(legacy_id);
        (legacy != dest && Self::usable(&legacy) && std::fs::rename(&legacy, &dest).is_ok())
            .then_some(dest)
    }

    /// The cached file for `url`, downloading it if needed. `legacy_id` is the key the
    /// file was stored under before artwork was keyed by uuid; a hit there is renamed.
    pub async fn get_or_download(&self, legacy_id: &str, url: &str) -> Result<PathBuf, CacheError> {
        if let Some(path) = self.cached(legacy_id, url) {
            return Ok(path);
        }
        let identity = art_identity(url);
        let id = identity.as_ref().map_or_else(
            || legacy_id.to_string(),
            |(uuid, size)| format!("{uuid}_{size}"),
        );
        let dest = self.file_path(&id);
        let px = identity.as_ref().and_then(|(_, size)| art_px(size));

        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(id.clone()).or_default().clone()
        };
        let guard = lock.lock().await;
        let result = self.fill(&dest, legacy_id, url).await;
        drop(guard);
        {
            let mut map = self.inflight.lock().unwrap();
            if map.get(&id).is_some_and(|l| Arc::strong_count(l) <= 2) {
                map.remove(&id);
            }
        }
        match result {
            Ok(()) => Ok(dest),
            Err(e) => {
                // Offline or throttled: a smaller copy beats a black tile.
                if let (Some((uuid, _)), Some(px)) = (&identity, px)
                    && let Some(path) = self.other_size(uuid, px, true)
                {
                    tracing::debug!("artwork {url}: {e}; using a smaller cached copy");
                    return Ok(path);
                }
                Err(e)
            }
        }
    }

    async fn fill(&self, dest: &Path, legacy_id: &str, url: &str) -> Result<(), CacheError> {
        // Another caller may have finished it while we waited for the lock.
        if Self::usable(dest) {
            return Ok(());
        }
        let legacy = self.file_path(legacy_id);
        if legacy != dest && Self::usable(&legacy) && std::fs::rename(&legacy, dest).is_ok() {
            return Ok(());
        }
        let bytes = self.download(url).await?;
        let tmp = dest.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }

    /// GET with retries: connection errors, timeouts, 429 and 5xx back off and try
    /// again; a 429/503 pauses every artwork download, honouring `Retry-After`.
    async fn download(&self, url: &str) -> Result<Vec<u8>, CacheError> {
        let mut attempt = 0;
        loop {
            let wait = self
                .cooldown_until
                .lock()
                .unwrap()
                .filter(|t| *t > tokio::time::Instant::now());
            if let Some(t) = wait {
                tokio::time::sleep_until(t).await;
            }
            attempt += 1;
            let backoff = Duration::from_millis(400 * (1 << (attempt - 1)));
            let res = self.client.get(url).send().await;
            let err = match res {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(b) if !b.is_empty() => return Ok(b.to_vec()),
                    Ok(_) => None,
                    Err(e) => Some(e),
                },
                Ok(r) => {
                    let status = r.status();
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if status.as_u16() == 429 || status.as_u16() == 503 {
                        let after = r
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .map_or(backoff, |s| Duration::from_secs(s.min(60)));
                        let until = tokio::time::Instant::now() + after;
                        let mut c = self.cooldown_until.lock().unwrap();
                        if c.is_none_or(|t| t < until) {
                            *c = Some(until);
                        }
                        tracing::warn!("artwork CDN said {status}; pausing downloads {after:?}");
                    }
                    let e = r.error_for_status().err();
                    if !retryable || attempt >= ART_ATTEMPTS {
                        return Err(e.map_or_else(
                            || std::io::Error::other(format!("HTTP {status}")).into(),
                            CacheError::from,
                        ));
                    }
                    e
                }
                Err(e) => Some(e),
            };
            if attempt >= ART_ATTEMPTS {
                return Err(err.map_or_else(
                    || std::io::Error::other("empty artwork response").into(),
                    CacheError::from,
                ));
            }
            tokio::time::sleep(backoff).await;
        }
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
    fn logging_out_keeps_preferences_pins_and_eqs_but_not_account_data() {
        let cache = MetadataCache::in_memory().unwrap();
        for key in ["settings", "dsp", "autoeq_index", "pins", "home", "favorites"] {
            cache.put_json(key, "{}").unwrap();
        }
        cache.clear_json().unwrap();
        for key in ["settings", "dsp", "autoeq_index", "pins"] {
            assert!(cache.get_json(key).is_some(), "{key} should survive");
        }
        for key in ["home", "favorites"] {
            assert!(cache.get_json(key).is_none(), "{key} should be cleared");
        }
    }

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
    fn art_identity_keys_by_picture_and_size() {
        let (uuid, size) = art_identity(
            "https://resources.tidal.com/images/cf78bf6b/3be0/4527/b52f/ad19596a086f/160x160.jpg",
        )
        .unwrap();
        assert_eq!(uuid, "cf78bf6b-3be0-4527-b52f-ad19596a086f");
        assert_eq!(size, "160x160");
        let (u2, s2) = art_identity(
            "https://resources.tidal.com/images/cf78bf6b/3be0/4527/b52f/ad19596a086f/origin.jpg",
        )
        .unwrap();
        assert_eq!((u2.as_str(), s2.as_str()), (uuid.as_str(), "origin"));
        assert!(art_identity("https://example.com/cover.jpg").is_none());
        assert!(art_identity("https://resources.tidal.com/images/x/y/160x160.jpg").is_none());
    }

    #[tokio::test]
    async fn image_cache_serves_a_larger_cached_size_without_network() {
        let dir = std::env::temp_dir().join(format!("tide_art_{}", std::process::id()));
        let cache = ImageCache::with_dir(dir.clone()).unwrap();
        let uuid = "cf78bf6b-3be0-4527-b52f-ad19596a086f";
        std::fs::write(cache.file_path(&format!("{uuid}_640x640")), b"jpg").unwrap();
        // Unroutable host: a network attempt would fail, so a hit proves no request was made.
        let url =
            "https://resources.tidal.com/images/cf78bf6b/3be0/4527/b52f/ad19596a086f/160x160.jpg";
        let path = cache.cached("legacy", url).unwrap();
        assert!(path.ends_with(format!("{uuid}_640x640.jpg")));
        assert_eq!(cache.get_or_download("legacy", url).await.unwrap(), path);
        // Not on disk at all: no file, and no request either.
        let missing =
            "https://resources.tidal.com/images/00000000/1111/2222/3333/444444444444/320x320.jpg";
        assert!(cache.cached("nothing", missing).is_none());
        // A legacy URL-hash file is adopted under the new key.
        let url2 =
            "https://resources.tidal.com/images/aaaaaaaa/bbbb/cccc/dddd/eeeeeeeeeeee/320x320.jpg";
        std::fs::write(cache.file_path("oldhash_0"), b"jpg").unwrap();
        let path = cache.get_or_download("oldhash_0", url2).await.unwrap();
        assert!(path.ends_with("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee_320x320.jpg"));
        assert!(!cache.file_path("oldhash_0").exists());
        let _ = std::fs::remove_dir_all(&dir);
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
    fn clear_empties_the_folder_but_spares_downloads_in_flight() {
        let dir = std::env::temp_dir().join(format!("tide_clear_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).unwrap();
        let done = cache.file_path("1", "MAX");
        std::fs::write(&done, b"audio").unwrap();
        std::fs::create_dir_all(cache.file_path("2", "MAX").with_extension("parts")).unwrap();
        let live_parts = cache.file_path("3", "MAX").with_extension("parts");
        let live_tmp = cache.file_path("3", "MAX").with_extension("tmp");
        std::fs::create_dir_all(&live_parts).unwrap();
        std::fs::write(&live_tmp, b"head").unwrap();
        cache.live_for("3", "MAX");
        cache.mark_partial_live("3", "MAX");

        cache.clear().unwrap();

        assert!(dir.is_dir());
        assert!(!done.exists());
        assert!(!cache.file_path("2", "MAX").with_extension("parts").exists());
        assert!(live_parts.is_dir() && live_tmp.exists());
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

    #[test]
    fn media_index_maps_scrubber_time_onto_dash_urls() {
        // urls[0] is init; the rest are media.
        assert_eq!(media_index_for_time(1, 100.0, 50.0), 0);
        assert_eq!(media_index_for_time(11, 100.0, 0.0), 1);
        // One block early: floor(0.75 * 10) = 7 → URL 7 (media block 6).
        assert_eq!(media_index_for_time(11, 100.0, 75.0), 7);
        assert_eq!(media_index_for_time(11, 100.0, 99.9), 9);
        assert_eq!(media_index_for_time(11, 100.0, 100.0), 9);
    }

    #[test]
    fn next_download_window_fetches_init_then_the_seek_block_then_gaps() {
        let have = vec![false; 12];
        assert_eq!(next_download_indices(&have, 8, 4), vec![0, 8, 9, 10]);
        let mut have = vec![false; 12];
        for i in [0, 8, 9] {
            have[i] = true;
        }
        assert_eq!(next_download_indices(&have, 8, 4), vec![10, 11, 1, 2]);
    }

    #[test]
    fn missing_playback_head_is_init_plus_two_media_at_priority() {
        let have = vec![false; 12];
        assert_eq!(missing_playback_head(&have, 1), vec![0, 1, 2]);
        assert_eq!(missing_playback_head(&have, 9), vec![0, 9, 10]);
        let mut have = vec![false; 12];
        have[0] = true;
        have[9] = true;
        assert_eq!(missing_playback_head(&have, 9), vec![10]);
    }

    #[test]
    fn playback_head_is_ready_once_init_and_two_media_at_priority_exist() {
        let mut have = vec![false; 10];
        assert!(!playback_head_ready(&have, 5));
        have[0] = true;
        have[5] = true;
        assert!(!playback_head_ready(&have, 5));
        have[6] = true;
        assert!(playback_head_ready(&have, 5));
        // From the start: init + first two media.
        let mut from_start = vec![false; 10];
        from_start[0] = true;
        from_start[1] = true;
        from_start[2] = true;
        assert!(playback_head_ready(&from_start, 1));
        // Short tail: last media segment is enough.
        let mut tail = vec![false; 6];
        tail[0] = true;
        tail[5] = true;
        assert!(playback_head_ready(&tail, 5));
    }

    #[test]
    fn playable_concat_is_init_plus_contiguous_media_from_the_seek_block() {
        let dir = std::env::temp_dir().join(format!("tide_parts_concat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (i, body) in ["I", "A", "B", "C", "D", "E"].into_iter().enumerate() {
            if i == 3 || i == 4 {
                continue; // hole at 3-4
            }
            std::fs::write(part_file(&dir, i), body.as_bytes()).unwrap();
        }
        let dest = dir.join("play.tmp");
        let n = write_playable_concat(&dir, &dest, 5, 6).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"IE");
        assert_eq!(n, 2);
        let n = write_playable_concat(&dir, &dest, 1, 6).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"IAB");
        assert_eq!(n, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn serve_segments(
        bodies: Vec<Vec<u8>>,
    ) -> (
        Vec<String>,
        tokio::task::JoinHandle<()>,
        std::net::SocketAddr,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let n = bodies.len();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let bodies = bodies.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.split_whitespace().nth(1).unwrap_or("/");
                    let idx: usize = path.trim_start_matches('/').parse().unwrap_or(0);
                    let body = bodies.get(idx).cloned().unwrap_or_default();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        let urls = (0..n).map(|i| format!("http://{addr}/{i}")).collect();
        (urls, handle, addr)
    }

    #[tokio::test]
    async fn fetch_for_playback_builds_tmp_from_the_seek_block() {
        let bodies: Vec<Vec<u8>> = (0..12).map(|i| format!("S{i}").into_bytes()).collect();
        let (urls, server, _) = serve_segments(bodies).await;
        let dir = std::env::temp_dir().join(format!(
            "tide_prio_fetch_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = AudioCache::with_dir(dir.clone(), 10_000_000).unwrap();
        cache.request_time(
            "t",
            "MAX",
            Duration::from_secs(75),
            Duration::from_secs(100),
        );
        // 12 URLs (init + 11 media): 75% → floor(0.75 * 11) = 8, one block early → 8.
        assert_eq!(media_index_for_time(12, 100.0, 75.0), 8);

        let path = cache
            .fetch_or_cache_with("t", "MAX", &urls, true, std::sync::Arc::new(|_, _, _| {}))
            .await
            .unwrap();
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("tmp"));
        assert_eq!(std::fs::read(&path).unwrap(), b"S0S8S9");
        assert_eq!(cache.playable_index("t", "MAX"), 8);
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fetch_from_the_start_returns_init_and_first_media() {
        let bodies: Vec<Vec<u8>> = (0..12).map(|i| format!("S{i}").into_bytes()).collect();
        let (urls, server, _) = serve_segments(bodies).await;
        let dir = std::env::temp_dir().join(format!(
            "tide_start_fetch_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = AudioCache::with_dir(dir.clone(), 10_000_000).unwrap();
        let path = cache
            .fetch_or_cache_with("t", "MAX", &urls, true, std::sync::Arc::new(|_, _, _| {}))
            .await
            .unwrap();
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("tmp"));
        assert_eq!(std::fs::read(&path).unwrap(), b"S0S1S2");
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Seeking inside the playable `.tmp` must keep appending to the same file
    /// (the player is reading it); only a seek outside it rebuilds the head.
    #[test]
    fn playable_tmp_is_rebuilt_only_for_seeks_outside_it() {
        let dir = std::env::temp_dir().join(format!("tide_rebase_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let parts = dir.join("parts");
        std::fs::create_dir_all(&parts).unwrap();
        let tmp = dir.join("t.tmp");
        let write_part = |i: usize| std::fs::write(part_file(&parts, i), format!("S{i}")).unwrap();
        let mut have = vec![false; 12];
        for (i, slot) in have.iter_mut().enumerate().take(7) {
            write_part(i);
            *slot = true;
        }
        // 12 URLs over 110 s: `seek_block(k)` lands where block k is fetched first.
        let live = LiveDownload::default();
        live.url_count.store(12, Ordering::Relaxed);
        live.duration_ms.store(110_000, Ordering::Relaxed);
        let seek_block = |k: u64| {
            live.requested_ms
                .store(10_000 * k + 5_000, Ordering::Relaxed)
        };
        let read = || String::from_utf8(std::fs::read(&tmp).unwrap()).unwrap();

        seek_block(1);
        publish_playable(&parts, &tmp, &have, &live, 12, true).unwrap();
        assert_eq!(read(), "S0S1S2S3S4S5S6");
        let gen0 = live.playable_gen.load(Ordering::Relaxed);
        assert!(live.covers(4) && !live.covers(7));

        // Inside the file: same file, no new generation.
        seek_block(4);
        publish_playable(&parts, &tmp, &have, &live, 12, false).unwrap();
        assert_eq!(live.playable_gen.load(Ordering::Relaxed), gen0);
        assert_eq!(live.playable_from.load(Ordering::Relaxed), 1);

        // The next block to append: keep appending.
        seek_block(7);
        write_part(7);
        have[7] = true;
        publish_playable(&parts, &tmp, &have, &live, 12, false).unwrap();
        assert_eq!(read(), "S0S1S2S3S4S5S6S7");
        assert_eq!(live.playable_gen.load(Ordering::Relaxed), gen0);

        // Outside: rebuild from the seek block.
        seek_block(10);
        for i in [10, 11] {
            write_part(i);
            have[i] = true;
        }
        publish_playable(&parts, &tmp, &have, &live, 12, false).unwrap();
        assert_eq!(read(), "S0S10S11");
        assert_eq!(live.playable_gen.load(Ordering::Relaxed), gen0 + 1);
        assert!(live.covers(live.index()));

        // Back before the rebuilt head: rebuild again.
        seek_block(2);
        publish_playable(&parts, &tmp, &have, &live, 12, false).unwrap();
        assert_eq!(read(), "S0S2S3S4S5S6S7");
        assert_eq!(live.playable_gen.load(Ordering::Relaxed), gen0 + 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
