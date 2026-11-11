use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tidlers::auth::TidalAuth;
use tidlers::client::models::playback::AudioQuality;
use tidlers::client::models::search::config::{SearchConfig, SearchType};
use tidlers::TidalClient;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackItem {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub album: String,
    pub album_id: Option<String>,
    pub duration_secs: u32,
    pub cover_url: Option<String>,
    pub audio_quality: String,
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
        Ok(Self { client, session_file })
    }

    pub fn with_device_auth() -> Result<Self, ApiError> {
        let auth = TidalAuth::with_oauth();
        let client = TidalClient::new(&auth);
        let session_file = default_session_path()?;
        Ok(Self { client, session_file })
    }

    pub async fn start_device_auth(&mut self) -> Result<DeviceAuthInfo, ApiError> {
        let link = self.client.get_oauth_link().await.map_err(|e| ApiError::Client(e.to_string()))?;
        Ok(DeviceAuthInfo {
            verification_uri: link.verification_uri,
            verification_uri_complete: link.verification_uri_complete,
            user_code: link.user_code,
            device_code: link.device_code,
            expires_in: link.expires_in,
            interval: link.interval,
        })
    }

    pub async fn poll_device_auth(&mut self, device_code: &str, expires_in: u64, interval: u64) -> Result<String, ApiError> {
        self.client.wait_for_oauth(device_code, expires_in, interval, None)
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        let user = self.client.get_user_info().await.map_err(|e| ApiError::Client(e.to_string()))?;
        let username = user.attributes.username;
        let _ = self.client.refresh_user_info().await;
        self.save_session()?;
        Ok(username)
    }

    pub fn start_pkce_login(&mut self) -> Result<String, ApiError> {
        self.client.initiate_pkce_login().map_err(|e| ApiError::Client(e.to_string()))
    }

    pub async fn finish_pkce_login(&mut self, redirect_url: &str) -> Result<String, ApiError> {
        self.client.finish_pkce_login(redirect_url).await.map_err(|e| ApiError::Client(e.to_string()))?;
        let user = self.client.get_user_info().await.map_err(|e| ApiError::Client(e.to_string()))?;
        let username = user.attributes.username;
        let _ = self.client.refresh_user_info().await;
        self.save_session()?;
        Ok(username)
    }

    pub fn save_session(&self) -> Result<(), ApiError> {
        let json = self.client.get_json();
        if let Some(parent) = self.session_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.session_file, json)?;
        Ok(())
    }

    pub async fn load_session(&mut self) -> Result<bool, ApiError> {
        if !self.session_file.exists() {
            return Ok(false);
        }
        let data = std::fs::read_to_string(&self.session_file)?;
        self.client = TidalClient::from_json(&data).map_err(|e| ApiError::Client(e.to_string()))?;
        if let Err(e) = self.client.refresh_access_token(false).await {
            tracing::warn!("Failed to refresh token: {e}");
            return Ok(false);
        }
        let _ = self.client.refresh_user_info().await;
        Ok(true)
    }

    pub fn is_authenticated(&self) -> bool {
        self.session_file.exists()
    }

    pub fn logout(&mut self) -> Result<(), ApiError> {
        if self.session_file.exists() {
            let _ = std::fs::remove_file(&self.session_file);
        }
        Ok(())
    }

    pub async fn get_favorite_tracks(&mut self, limit: Option<u32>) -> Result<Vec<TrackItem>, ApiError> {
        let resp = self.client.get_collection_track_favorites(limit, None).await.map_err(|e| ApiError::Client(e.to_string()))?;
        let mut tracks = Vec::new();
        for entry in resp.items {
            let track = entry.item;
            let cover_url = track.album.as_ref().and_then(|a| a.cover.as_ref()).map(|c| {
                format!("https://resources.tidal.com/images/{}/640x640.jpg", c.replace('-', "/"))
            });
            let album_name = track.album.as_ref().map(|a| a.title.clone()).unwrap_or_default();
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
            });
        }
        Ok(tracks)
    }

    pub async fn get_stream_urls(&mut self, track_id: &str) -> Result<PlaybackStream, ApiError> {
        self.client.set_audio_quality(AudioQuality::HiRes);
        let playback = self.client
            .get_track_postpaywall_playback_info(track_id, None)
            .await
            .map_err(|e| ApiError::Client(e.to_string()))?;
        
        let urls = playback.get_stream_urls().ok_or_else(|| ApiError::NoStream(track_id.to_string()))?;
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
        let track = self.client.get_track(track_id).await.map_err(|e| ApiError::Client(e.to_string()))?;
        let cover_url = track.album.as_ref().and_then(|a| a.cover.as_ref()).map(|c| {
            format!("https://resources.tidal.com/images/{}/640x640.jpg", c.replace('-', "/"))
        });
        let album_name = track.album.as_ref().map(|a| a.title.clone()).unwrap_or_default();
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
        })
    }

    pub async fn search_tracks(&mut self, query: &str) -> Result<Vec<TrackItem>, ApiError> {
        let config = SearchConfig {
            query: query.to_string(),
            types: vec![SearchType::Tracks],
            limit: 25,
            ..Default::default()
        };
        let results = self.client.search(config).await.map_err(|e| ApiError::Client(e.to_string()))?;
        let mut tracks = Vec::new();
        if let Some(track_list) = results.tracks {
            for track in track_list.items {
                let cover_url = track
                    .album
                    .as_ref()
                    .map(|a| format!("https://resources.tidal.com/images/{}/640x640.jpg", a.cover.replace('-', "/")));
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
                let album_name = track
                    .album
                    .as_ref()
                    .map(|a| a.title.clone())
                    .unwrap_or_default();
                let album_id = track
                    .album
                    .as_ref()
                    .map(|a| a.id.to_string());
                tracks.push(TrackItem {
                    id: track.id.to_string(),
                    title: track.title,
                    artist: artist_name,
                    artist_id,
                    album: album_name,
                    album_id,
                    duration_secs: track.duration as u32,
                    cover_url,
                    audio_quality: track.audio_quality.unwrap_or_else(|| "LOSSLESS".to_string()),
                });
            }
        }
        Ok(tracks)
    }
}

pub fn default_session_path() -> Result<PathBuf, ApiError> {
    let base = dirs::data_dir().ok_or_else(|| ApiError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "No data dir found")))?;
    Ok(base.join("tidal-rust").join("session.json"))
}
