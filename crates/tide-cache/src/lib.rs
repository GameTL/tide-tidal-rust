use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use rusqlite::{params, Connection};
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
        let cache = Self { conn: std::sync::Mutex::new(conn) };
        cache.init_schema()?;
        Ok(cache)
    }

    pub fn in_memory() -> Result<Self, CacheError> {
        let conn = Connection::open_in_memory()?;
        let cache = Self { conn: std::sync::Mutex::new(conn) };
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
            "
        )?;
        Ok(())
    }

    /// Store an arbitrary JSON document (home feed, playlists, ...) for instant offline boot.
    pub fn put_json(&self, key: &str, json: &str) -> Result<(), CacheError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
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
        conn.query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| r.get(0)).ok()
    }

    pub fn clear_json(&self) -> Result<(), CacheError> {
        // Account data goes; app preferences stay.
        self.conn.lock().unwrap().execute("DELETE FROM kv WHERE key != 'settings'", [])?;
        Ok(())
    }

    pub fn upsert_track(&self, track: &TrackItem) -> Result<(), CacheError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tracks (id, title, artist, artist_id, album, album_id, duration_secs, cover_url, audio_quality, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                title=excluded.title,
                artist=excluded.artist,
                artist_id=excluded.artist_id,
                album=excluded.album,
                album_id=excluded.album_id,
                duration_secs=excluded.duration_secs,
                cover_url=excluded.cover_url,
                audio_quality=excluded.audio_quality,
                cached_at=excluded.cached_at",
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
                now,
            ],
        )?;
        Ok(())
    }

    pub fn get_track(&self, track_id: &str) -> Result<Option<TrackItem>, CacheError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, artist, artist_id, album, album_id, duration_secs, cover_url, audio_quality
             FROM tracks WHERE id = ?1"
        )?;
        let mut rows = stmt.query(params![track_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(TrackItem {
                id: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
                artist_id: row.get(3)?,
                album: row.get(4)?,
                album_id: row.get(5)?,
                duration_secs: row.get(6)?,
                cover_url: row.get(7)?,
                audio_quality: row.get(8)?,
                ..Default::default()
            }))
        } else {
            Ok(None)
        }
    }

    pub fn save_favorites(&self, tracks: &[TrackItem]) -> Result<(), CacheError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
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
            "SELECT t.id, t.title, t.artist, t.artist_id, t.album, t.album_id, t.duration_secs, t.cover_url, t.audio_quality
             FROM favorites f
             JOIN tracks t ON f.track_id = t.id
             ORDER BY f.added_at DESC"
        )?;
        let rows = stmt.query_map([], |row| {
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
                ..Default::default()
            })
        })?;
        let mut result = Vec::new();
        for item in rows {
            result.push(item?);
        }
        Ok(result)
    }

    pub fn record_play(&self, track_id: &str) -> Result<(), CacheError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
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
        conn.query_row("SELECT COALESCE(SUM(hits), 0), COALESCE(SUM(misses), 0) FROM cache_stats", [], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })
        .unwrap_or((0, 0))
    }

    /// How much each track is worth keeping offline: every play counts, and a
    /// play loses half its weight every [`SCORE_HALF_LIFE_DAYS`] days, so the
    /// score follows what you listen to now rather than years ago.
    pub fn play_scores(&self) -> std::collections::HashMap<String, f64> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        self.play_scores_at(now)
    }

    pub fn play_scores_at(&self, now: i64) -> std::collections::HashMap<String, f64> {
        let mut scores = std::collections::HashMap::new();
        let conn = self.conn.lock().unwrap();
        let Ok(mut stmt) = conn.prepare("SELECT track_id, played_at FROM history") else { return scores };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) else {
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
            let Ok(mut stmt) = conn.prepare("SELECT track_id, COUNT(*) FROM history GROUP BY track_id") else {
                return Vec::new();
            };
            let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))) else {
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
        ranked.into_iter().take(limit).filter_map(|(id, _)| self.get_track(&id).ok().flatten()).collect()
    }

    pub fn get_recently_played(&self, limit: u32) -> Result<Vec<TrackItem>, CacheError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.title, t.artist, t.artist_id, t.album, t.album_id, t.duration_secs, t.cover_url, t.audio_quality
             FROM history h
             JOIN tracks t ON h.track_id = t.id
             ORDER BY h.played_at DESC
             LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit], |row| {
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
                ..Default::default()
            })
        })?;
        let mut result = Vec::new();
        for item in rows {
            result.push(item?);
        }
        Ok(result)
    }
}

pub struct AudioCache {
    base_dir: PathBuf,
    max_bytes: std::sync::atomic::AtomicU64,
}

impl AudioCache {
    pub fn new() -> Result<Self, CacheError> {
        let base_dir = default_audio_cache_dir()?;
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self {
            base_dir,
            max_bytes: (20u64 * 1024 * 1024 * 1024).into(), // 20 GB default
        })
    }

    pub fn with_dir(dir: PathBuf, max_bytes: u64) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            base_dir: dir,
            max_bytes: max_bytes.into(),
        })
    }

    pub fn file_path(&self, track_id: &str, quality: &str) -> PathBuf {
        let safe_quality = quality.replace(' ', "_").replace('/', "_");
        self.base_dir.join(format!("{}_{}.audio", track_id, safe_quality))
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

    pub async fn fetch_or_cache(
        &self,
        track_id: &str,
        quality: &str,
        urls: &[String],
    ) -> Result<PathBuf, CacheError> {
        if let Some(cached) = self.get_cached(track_id, quality) {
            tracing::info!(track_id = %track_id, "Audio cache HIT: streaming offline from disk");
            return Ok(cached);
        }

        tracing::info!(track_id = %track_id, "Audio cache MISS: streaming & caching to local SSD");
        let dest = self.file_path(track_id, quality);
        let tmp = dest.with_extension("tmp");

        let client = reqwest::Client::new();
        let mut file = std::fs::File::create(&tmp)?;

        // DASH streams are hundreds of small segments: fetch a window of them
        // concurrently, but always write in order.
        const WINDOW: usize = 8;
        for window in urls.chunks(WINDOW) {
            let mut set = tokio::task::JoinSet::new();
            for (i, url) in window.iter().enumerate() {
                let (client, url) = (client.clone(), url.clone());
                set.spawn(async move {
                    let bytes = client.get(&url).send().await?.error_for_status()?.bytes().await?;
                    Ok::<_, reqwest::Error>((i, bytes))
                });
            }
            let mut parts = vec![None; window.len()];
            while let Some(joined) = set.join_next().await {
                let (i, bytes) = joined.map_err(|e| std::io::Error::other(e.to_string()))??;
                parts[i] = Some(bytes);
            }
            for part in parts.into_iter().flatten() {
                use std::io::Write;
                file.write_all(&part)?;
            }
        }
        drop(file);

        std::fs::rename(&tmp, &dest)?;
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
        self.max_bytes.store(max_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Track id of a cache file named `<track id>_<QUALITY>.audio`.
    fn track_id_of(path: &std::path::Path) -> Option<String> {
        if path.extension().and_then(|s| s.to_str()) != Some("audio") {
            return None;
        }
        path.file_stem()?.to_str()?.rsplit_once('_').map(|(id, _)| id.to_string())
    }

    /// Bring the cache back under its size limit, least recently used first.
    pub fn enforce_quota(&self) -> Result<(), CacheError> {
        self.evict(&std::collections::HashMap::new(), &[]).map(|_| ())
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
            let Some(track_id) = Self::track_id_of(&path) else { continue };
            let meta = entry.metadata()?;
            total_size += meta.len();
            let score = scores.get(&track_id).copied().unwrap_or(0.0);
            files.push((path, meta.len(), score, meta.accessed().unwrap_or(UNIX_EPOCH), track_id));
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
        let safe_id = id.replace('/', "_").replace(':', "_");
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

        cache.save_favorites(&[track.clone()]).unwrap();
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
            [("fav".to_string(), 9.0), ("rare".to_string(), 0.4)].into_iter().collect();

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
            cache.upsert_track(&TrackItem { id: id.into(), ..Default::default() }).unwrap();
        }
        {
            let conn = cache.conn.lock().unwrap();
            let add = |id: &str, at: i64| {
                conn.execute("INSERT INTO history (track_id, played_at) VALUES (?1, ?2)", params![id, at]).unwrap();
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
}
