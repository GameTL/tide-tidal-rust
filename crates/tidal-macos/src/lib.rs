use std::time::Duration;
pub use souvlaki::{MediaControlEvent, MediaMetadata, MediaPlayback};
use souvlaki::{MediaControls, PlatformConfig};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MacosMediaError {
    #[error("Media controls error: {0}")]
    Souvlaki(#[from] souvlaki::Error),
}

pub struct MacMediaControls {
    controls: MediaControls,
}

impl MacMediaControls {
    pub fn new<F>(app_name: &'static str, event_handler: F) -> Result<Self, MacosMediaError>
    where
        F: Fn(MediaControlEvent) + Send + 'static,
    {
        #[cfg(target_os = "macos")]
        let config = PlatformConfig {
            dbus_name: app_name,
            display_name: app_name,
            hwnd: None,
        };

        #[cfg(not(target_os = "macos"))]
        let config = PlatformConfig {
            dbus_name: app_name,
            display_name: app_name,
            hwnd: None,
        };

        let mut controls = MediaControls::new(config)?;
        controls.attach(event_handler)?;

        Ok(Self { controls })
    }

    pub fn set_metadata(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        duration: Option<Duration>,
        cover_url: Option<&str>,
    ) -> Result<(), MacosMediaError> {
        let metadata = MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: Some(album),
            duration,
            cover_url,
        };
        self.controls.set_metadata(metadata)?;
        Ok(())
    }

    pub fn set_playback_state(&mut self, is_playing: bool, progress: Option<Duration>) -> Result<(), MacosMediaError> {
        let state = if is_playing {
            MediaPlayback::Playing { progress: progress.map(souvlaki::MediaPosition) }
        } else {
            MediaPlayback::Paused { progress: progress.map(souvlaki::MediaPosition) }
        };
        self.controls.set_playback(state)?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), MacosMediaError> {
        self.controls.set_playback(MediaPlayback::Stopped)?;
        Ok(())
    }
}
