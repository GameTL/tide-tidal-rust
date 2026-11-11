use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use rusqlite::{params, Connection};
use thiserror::Error;
use tidal_api::TrackItem;

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
            "
        )?;
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
    max_bytes: u64,
}

impl AudioCache {
    pub fn new() -> Result<Self, CacheError> {
        let base_dir = default_audio_cache_dir()?;
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self {
            base_dir,
            max_bytes: 20 * 1024 * 1024 * 1024, // 20 GB default
        })
    }

    pub fn with_dir(dir: PathBuf, max_bytes: u64) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            base_dir: dir,
            max_bytes,
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

        for url in urls {
            let mut resp = client.get(url).send().await?.error_for_status()?;
            while let Some(chunk) = resp.chunk().await? {
                use std::io::Write;
                file.write_all(&chunk)?;
            }
        }

        std::fs::rename(&tmp, &dest)?;
        self.enforce_quota()?;
        Ok(dest)
    }

    pub fn enforce_quota(&self) -> Result<(), CacheError> {
        let entries = std::fs::read_dir(&self.base_dir)?;
        let mut files = Vec::new();
        let mut total_size = 0u64;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("audio") {
                let meta = entry.metadata()?;
                let size = meta.len();
                let accessed = meta.accessed().unwrap_or(UNIX_EPOCH);
                total_size += size;
                files.push((path, size, accessed));
            }
        }

        if total_size > self.max_bytes {
            // Sort by oldest accessed first
            files.sort_by_key(|(_, _, accessed)| *accessed);
            for (path, size, _) in files {
                if total_size <= self.max_bytes {
                    break;
                }
                if let Ok(()) = std::fs::remove_file(&path) {
                    total_size = total_size.saturating_sub(size);
                }
            }
        }

        Ok(())
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
}
