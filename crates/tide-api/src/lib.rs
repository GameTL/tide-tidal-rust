use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tidlers::TidalClient;
use tidlers::auth::TidalAuth;
use tidlers::client::models::playback::AudioQuality;
use tidlers::client::models::search::config::{SearchConfig, SearchType};

pub mod catalog;
pub mod hls;
pub mod web;
pub use catalog::{LaterItems, feed_group, feed_group_on};
pub use web::{
    ApiHandle, Card, CatalogDetail, Credit, FavoriteIds, Lyrics, Section, StreamInfo, VideoStream,
    VideoVariant, plain_bio,
};

#[derive(Error, Debug)]
pub enum ApiError {
    #[error("Authentication failed: {0}")]
    Auth(String),
    #[error("API request error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Tidlers error: {0}")]
    Client(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("No stream available for track {0}")]
    NoStream(String),
}

/// The request never got an HTTP answer (DNS, connect, TLS, timeout).
fn is_transport_error(e: &tidlers::TidalError) -> bool {
    use tidlers::requests::RequestClientError as R;
    matches!(
        e,
        tidlers::TidalError::Request(_)
            | tidlers::TidalError::RequestClient(R::RequestError(_) | R::Timeout)
    )
}

/// An error with its causes, since tidlers' own messages stop at "failed to do request".
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackItem {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    /// Every credited artist, in billing order. Empty when only `artist` / `artist_id` are known.
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
    pub album: String,
    pub album_id: Option<String>,
    pub duration_secs: u32,
    pub cover_url: Option<String>,
    pub audio_quality: String,
    #[serde(default)]
    pub explicit: bool,
    /// Human-readable "date added" label (e.g. "Last week") when listed from a collection.
    #[serde(default)]
    pub date_added: Option<String>,
    #[serde(default)]
    pub bpm: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStream {
    pub track_id: String,
    pub quality: String,
    pub stream_urls: Vec<String>,
    pub mime_type: Option<String>,
    pub codecs: Option<String>,
    pub album_replay_gain: f64,
    pub track_replay_gain: f64,
}

#[derive(Debug, Clone)]
pub struct DeviceAuthInfo {
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub user_code: String,
    pub device_code: String,
    pub expires_in: u64,
    pub interval: u64,
}

pub struct TidalService {
    client: TidalClient,
    session_file: PathBuf,
}

impl TidalService {
    pub fn new() -> Result<Self, ApiError> {
        let auth = TidalAuth::with_pkce();
        let client = TidalClient::new(&auth);
        let session_file = default_session_path()?;
        Ok(Self {
            client,
            session_file,
        })
    }

    pub fn with_device_auth() -> Result<Self, ApiError> {
        let auth = TidalAuth::with_oauth();
        let client = TidalClient::new(&auth);
        let session_file = default_session_path()?;
        Ok(Self {
            client,
            session_file,
        })
    }

    pub async fn start_device_auth(&mut self) -> Result<DeviceAuthInfo, ApiError> {
        let link = self
            .client
            .get_oauth_link()
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        Ok(DeviceAuthInfo {
            verification_uri: link.verification_uri,
            verification_uri_complete: link.verification_uri_complete,
            user_code: link.user_code,
            device_code: link.device_code,
            expires_in: link.expires_in,
            interval: link.interval,
        })
    }

    pub async fn poll_device_auth(
        &mut self,
        device_code: &str,
        expires_in: u64,
        interval: u64,
    ) -> Result<String, ApiError> {
        self.client
            .wait_for_oauth(device_code, expires_in, interval, None)
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        let user = self
            .client
            .get_user_info()
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        let username = user.attributes.username;
        let _ = self.client.refresh_user_info().await;
        self.save_session()?;
        Ok(username)
    }

    pub fn start_pkce_login(&mut self) -> Result<String, ApiError> {
        self.client
            .initiate_pkce_login()
            .map_err(|e| ApiError::Client(e.to_string()))
    }

    pub async fn finish_pkce_login(&mut self, redirect_url: &str) -> Result<String, ApiError> {
        // A dropped connection never reaches TIDAL, so the one-time code is still good: retry.
        let mut attempt = 0;
        loop {
            match self.client.finish_pkce_login(redirect_url).await {
                Ok(()) => break,
                Err(e) if is_transport_error(&e) && attempt < 3 => {
                    attempt += 1;
                    tracing::warn!(
                        "PKCE token exchange failed ({}), retry {attempt}/3",
                        error_chain(&e)
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(600 * attempt)).await;
                }
                Err(e) => return Err(ApiError::Client(error_chain(&e))),
            }
        }
        // The code is spent now: keep the session even if the profile lookup below fails.
        self.save_session()?;
        let username = match self.client.get_user_info().await {
            Ok(user) => user.attributes.username,
            Err(e) => {
                tracing::warn!(
                    "logged in, but the profile lookup failed: {}",
                    error_chain(&e)
                );
                self.client
                    .user_info
                    .as_ref()
                    .map(|u| u.username.clone())
                    .unwrap_or_default()
            }
        };
        let _ = self.client.refresh_user_info().await;
        self.save_session()?;
        Ok(username)
    }

    pub fn save_session(&self) -> Result<(), ApiError> {
        let json = self.client.get_json();
        if let Some(parent) = self.session_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_private(&self.session_file, json.as_bytes())?;
        Ok(())
    }

    pub async fn load_session(&mut self) -> Result<bool, ApiError> {
        if !self.session_file.exists() {
            return Ok(false);
        }
        restrict_to_owner(&self.session_file);
        let data = std::fs::read_to_string(&self.session_file)?;
        self.client = TidalClient::from_json(&data).map_err(|e| ApiError::Client(e.to_string()))?;
        if let Err(e) = self.client.refresh_access_token(false).await {
            tracing::warn!("Failed to refresh token: {e}");
            return Ok(false);
        }
        let _ = self.client.refresh_user_info().await;
        Ok(true)
    }

    /// Snapshot of the current credentials for lock-free API calls.
    pub fn handle(&self) -> Option<web::ApiHandle> {
        Some(web::ApiHandle {
            token: self.client.session.auth.access_token.clone()?,
            country: self.country_code(),
            user_id: self.client.session.auth.user_id?,
        })
    }

    /// Refresh the access token if it is close to expiry and persist the session.
    pub async fn refresh(&mut self) -> Result<(), ApiError> {
        self.client
            .refresh_access_token(false)
            .await
            .map_err(|e| ApiError::Auth(e.to_string()))?;
        self.save_session()
    }

    pub fn user_id(&self) -> Option<u64> {
        self.client.session.auth.user_id
    }

    /// True when the session came from the PKCE login, which unlocks Hi-Res (Max) streams.
    pub fn is_pkce(&self) -> bool {
        self.client.session.auth.pkce_login
    }

    /// Display name of the logged-in account.
    pub fn username(&self) -> Option<String> {
        let info = serde_json::to_value(self.client.user_info.as_ref()?).ok()?;
        ["username", "email", "firstName"].iter().find_map(|k| {
            info.get(*k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
    }

    pub fn country_code(&self) -> String {
        self.client
            .user_info
            .as_ref()
            .and_then(|u| serde_json::to_value(u).ok())
            .and_then(|v| {
                v.get("countryCode")
                    .and_then(|c| c.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "US".into())
    }

    pub fn is_authenticated(&self) -> bool {
        self.session_file.exists()
    }

    /// Ask TIDAL to invalidate the current access token (best effort).
    pub async fn revoke(&self) -> Result<(), ApiError> {
        self.client
            .logout()
            .await
            .map_err(|e| ApiError::Auth(e.to_string()))
    }

    pub fn logout(&mut self) -> Result<(), ApiError> {
        if self.session_file.exists() {
            let _ = std::fs::remove_file(&self.session_file);
        }
        Ok(())
    }

    pub async fn get_favorite_tracks(
        &mut self,
        limit: Option<u32>,
    ) -> Result<Vec<TrackItem>, ApiError> {
        let resp = self
            .client
            .get_collection_track_favorites(limit, None)
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        let mut tracks = Vec::new();
        for entry in resp.items {
            let track = entry.item;
            let cover_url = track
                .album
                .as_ref()
                .and_then(|a| a.cover.as_ref())
                .map(|c| {
                    format!(
                        "https://resources.tidal.com/images/{}/640x640.jpg",
                        c.replace('-', "/")
                    )
                });
            let album_name = track
                .album
                .as_ref()
                .map(|a| a.title.clone())
                .unwrap_or_default();
            let album_id = track.album.as_ref().map(|a| a.id.to_string());
            tracks.push(TrackItem {
                id: track.id.to_string(),
                title: track.title,
                artist: track.artist.name,
                artist_id: Some(track.artist.id.to_string()),
                album: album_name,
                album_id,
                duration_secs: track.duration as u32,
                cover_url,
                audio_quality: format!("{:?}", track.audio_quality),
                ..Default::default()
            });
        }
        Ok(tracks)
    }

    pub async fn get_stream_urls(&mut self, track_id: &str) -> Result<PlaybackStream, ApiError> {
        self.client.set_audio_quality(AudioQuality::HiRes);
        let playback = self
            .client
            .get_track_postpaywall_playback_info(track_id, None)
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;

        let urls = playback
            .get_stream_urls()
            .ok_or_else(|| ApiError::NoStream(track_id.to_string()))?;
        Ok(PlaybackStream {
            track_id: track_id.to_string(),
            quality: format!("{:?}", playback.audio_quality),
            stream_urls: urls,
            mime_type: playback.get_mime_type(),
            codecs: playback.get_codecs(),
            album_replay_gain: playback.album_replay_gain,
            track_replay_gain: playback.track_replay_gain,
        })
    }

    pub async fn get_track(&mut self, track_id: &str) -> Result<TrackItem, ApiError> {
        let track = self
            .client
            .get_track(track_id)
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        let cover_url = track
            .album
            .as_ref()
            .and_then(|a| a.cover.as_ref())
            .map(|c| {
                format!(
                    "https://resources.tidal.com/images/{}/640x640.jpg",
                    c.replace('-', "/")
                )
            });
        let album_name = track
            .album
            .as_ref()
            .map(|a| a.title.clone())
            .unwrap_or_default();
        let album_id = track.album.as_ref().map(|a| a.id.to_string());
        Ok(TrackItem {
            id: track.id.to_string(),
            title: track.title,
            artist: track.artist.name,
            artist_id: Some(track.artist.id.to_string()),
            album: album_name,
            album_id,
            duration_secs: track.duration as u32,
            cover_url,
            audio_quality: format!("{:?}", track.audio_quality),
            ..Default::default()
        })
    }

    pub async fn search_tracks(&mut self, query: &str) -> Result<Vec<TrackItem>, ApiError> {
        let config = SearchConfig {
            query: query.to_string(),
            types: vec![SearchType::Tracks],
            limit: 25,
            ..Default::default()
        };
        let results = self
            .client
            .search(config)
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        let mut tracks = Vec::new();
        if let Some(track_list) = results.tracks {
            for track in track_list.items {
                let cover_url = track.album.as_ref().map(|a| {
                    format!(
                        "https://resources.tidal.com/images/{}/640x640.jpg",
                        a.cover.replace('-', "/")
                    )
                });
                let artist_name = track
                    .artists
                    .first()
                    .and_then(|a| a.name.clone())
                    .unwrap_or_else(|| "Unknown Artist".to_string());
                let artist_id = track
                    .artists
                    .first()
                    .and_then(|a| a.id)
                    .map(|id| id.to_string());
                let artists = track
                    .artists
                    .iter()
                    .filter_map(|a| {
                        Some(ArtistRef {
                            id: a.id?.to_string(),
                            name: a.name.clone()?,
                        })
                    })
                    .collect();
                let album_name = track
                    .album
                    .as_ref()
                    .map(|a| a.title.clone())
                    .unwrap_or_default();
                let album_id = track.album.as_ref().map(|a| a.id.to_string());
                tracks.push(TrackItem {
                    id: track.id.to_string(),
                    title: track.title,
                    artist: artist_name,
                    artist_id,
                    artists,
                    album: album_name,
                    album_id,
                    duration_secs: track.duration as u32,
                    cover_url,
                    audio_quality: track
                        .audio_quality
                        .unwrap_or_else(|| "LOSSLESS".to_string()),
                    ..Default::default()
                });
            }
        }
        Ok(tracks)
    }
}

/// Make an existing file (and its directory) readable by the current user only.
pub fn restrict_to_owner(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(dir) = path.parent() {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Write a file only the current user can read (it holds OAuth tokens).
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Some(dir) = path.parent() {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        // Write to a private temp file and rename, so the tokens are never
        // world-readable and a crash cannot leave a half-written session.
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

pub fn default_session_path() -> Result<PathBuf, ApiError> {
    let base = dirs::data_dir().ok_or_else(|| {
        ApiError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No data dir found",
        ))
    })?;
    Ok(base.join("tidal-rust").join("session.json"))
}
