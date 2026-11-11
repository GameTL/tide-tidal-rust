use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MacosMediaError {
    #[error("Media controls error: {0}")]
    Generic(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaPosition(pub Duration);

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum MediaControlEvent {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    SetPosition(MediaPosition),
    SetShuffle(bool),
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::Path;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, AnyThread};
    use objc2_app_kit::NSImage;
    use objc2_core_foundation::CGSize;
    use objc2_foundation::{NSData, NSDictionary, NSMutableDictionary, NSNumber, NSString};
    use objc2_media_player::{
        MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle,
        MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
        MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyDefaultPlaybackRate,
        MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyMediaType,
        MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommandCenter,
        MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
    };

    use super::{MacosMediaError, MediaControlEvent, MediaPosition};

    pub struct MacMediaControls {
        targets: Vec<Retained<AnyObject>>,
        dict: Retained<NSMutableDictionary<NSString, AnyObject>>,
    }

    unsafe impl Send for MacMediaControls {}
    unsafe impl Sync for MacMediaControls {}

    impl MacMediaControls {
        pub fn new<F>(_app_name: &'static str, event_handler: F) -> Result<Self, MacosMediaError>
        where
            F: Fn(MediaControlEvent) + Send + 'static,
        {
            let command_center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
            let handler = Arc::new(event_handler);
            let mut targets = Vec::new();

            // Toggle play/pause
            let toggle_cmd = unsafe { command_center.togglePlayPauseCommand() };
            unsafe { toggle_cmd.setEnabled(true) };
            let h = handler.clone();
            let toggle_block = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                h(MediaControlEvent::Toggle);
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { toggle_cmd.addTargetWithHandler(&toggle_block) };
            targets.push(target);

            // Play
            let play_cmd = unsafe { command_center.playCommand() };
            unsafe { play_cmd.setEnabled(true) };
            let h = handler.clone();
            let play_block = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                h(MediaControlEvent::Play);
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { play_cmd.addTargetWithHandler(&play_block) };
            targets.push(target);

            // Pause
            let pause_cmd = unsafe { command_center.pauseCommand() };
            unsafe { pause_cmd.setEnabled(true) };
            let h = handler.clone();
            let pause_block = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                h(MediaControlEvent::Pause);
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { pause_cmd.addTargetWithHandler(&pause_block) };
            targets.push(target);

            // Stop
            let stop_cmd = unsafe { command_center.stopCommand() };
            unsafe { stop_cmd.setEnabled(true) };
            let h = handler.clone();
            let stop_block = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                h(MediaControlEvent::Stop);
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { stop_cmd.addTargetWithHandler(&stop_block) };
            targets.push(target);

            // Next track
            let next_cmd = unsafe { command_center.nextTrackCommand() };
            unsafe { next_cmd.setEnabled(true) };
            let h = handler.clone();
            let next_block = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                h(MediaControlEvent::Next);
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { next_cmd.addTargetWithHandler(&next_block) };
            targets.push(target);

            // Previous track
            let prev_cmd = unsafe { command_center.previousTrackCommand() };
            unsafe { prev_cmd.setEnabled(true) };
            let h = handler.clone();
            let prev_block = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                h(MediaControlEvent::Previous);
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { prev_cmd.addTargetWithHandler(&prev_block) };
            targets.push(target);

            // Change playback position (scrubbing in Control Center)
            let pos_cmd = unsafe { command_center.changePlaybackPositionCommand() };
            unsafe { pos_cmd.setEnabled(true) };
            let h = handler.clone();
            let pos_block = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
                let pos_event = unsafe { &*(event.as_ptr() as *const MPChangePlaybackPositionCommandEvent) };
                let secs = unsafe { pos_event.positionTime() };
                if secs.is_finite() && secs >= 0.0 {
                    h(MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs_f64(secs))));
                }
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { pos_cmd.addTargetWithHandler(&pos_block) };
            targets.push(target);

            let dict = NSMutableDictionary::new();

            Ok(Self { targets, dict })
        }

        pub fn set_metadata(
            &mut self,
            title: &str,
            artist: &str,
            album: &str,
            duration: Option<Duration>,
            cover_url: Option<&str>,
        ) -> Result<(), MacosMediaError> {
            let title_ns = NSString::from_str(title);
            let artist_ns = NSString::from_str(artist);
            let album_ns = NSString::from_str(album);
            let media_type = NSNumber::new_u64(1); // Audio

            unsafe {
                let _: () = msg_send![&self.dict, setObject: &*title_ns, forKey: MPMediaItemPropertyTitle];
                let _: () = msg_send![&self.dict, setObject: &*artist_ns, forKey: MPMediaItemPropertyArtist];
                let _: () = msg_send![&self.dict, setObject: &*album_ns, forKey: MPMediaItemPropertyAlbumTitle];
                let _: () = msg_send![&self.dict, setObject: &*media_type, forKey: MPNowPlayingInfoPropertyMediaType];

                if let Some(dur) = duration {
                    let dur_ns = NSNumber::new_f64(dur.as_secs_f64());
                    let _: () = msg_send![&self.dict, setObject: &*dur_ns, forKey: MPMediaItemPropertyPlaybackDuration];
                }
            }

            if let Some(url) = cover_url {
                if let Some(path) = url.strip_prefix("file://").map(Path::new) {
                    if path.exists() {
                        let _ = self.set_artwork_file(path);
                    }
                } else if Path::new(url).exists() {
                    let _ = self.set_artwork_file(Path::new(url));
                }
            }

            self.publish();
            Ok(())
        }

        pub fn set_artwork(&mut self, image_data: &[u8]) -> Result<(), MacosMediaError> {
            let data = NSData::with_bytes(image_data);
            let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
                return Err(MacosMediaError::Generic("Failed to decode artwork image".into()));
            };
            self.apply_artwork(image);
            Ok(())
        }

        pub fn set_artwork_file(&mut self, path: &Path) -> Result<(), MacosMediaError> {
            let path_str = path.to_string_lossy();
            let path_ns = NSString::from_str(&path_str);
            let Some(image) = NSImage::initWithContentsOfFile(NSImage::alloc(), &path_ns) else {
                return Err(MacosMediaError::Generic(format!("Failed to load artwork file: {}", path.display())));
            };
            self.apply_artwork(image);
            Ok(())
        }

        fn apply_artwork(&mut self, image: Retained<NSImage>) {
            let size = image.size();
            let cg_size = CGSize { width: size.width, height: size.height };
            let image_ptr = Retained::into_raw(image);
            let handler = RcBlock::new(move |_req_size: CGSize| -> NonNull<NSImage> {
                NonNull::new(image_ptr).expect("non-null NSImage for artwork")
            });
            let artwork = unsafe {
                MPMediaItemArtwork::initWithBoundsSize_requestHandler(
                    MPMediaItemArtwork::alloc(),
                    cg_size,
                    &handler,
                )
            };
            unsafe {
                let _: () = msg_send![&self.dict, setObject: &*artwork, forKey: MPMediaItemPropertyArtwork];
            }
            self.publish();
        }

        pub fn set_playback_state(&mut self, is_playing: bool, progress: Option<Duration>) -> Result<(), MacosMediaError> {
            let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
            let state = if is_playing {
                MPNowPlayingPlaybackState::Playing
            } else {
                MPNowPlayingPlaybackState::Paused
            };
            unsafe { center.setPlaybackState(state) };

            let rate = if is_playing { 1.0f64 } else { 0.0f64 };
            let rate_ns = NSNumber::new_f64(rate);
            let default_rate_ns = NSNumber::new_f64(1.0);

            unsafe {
                let _: () = msg_send![&self.dict, setObject: &*rate_ns, forKey: MPNowPlayingInfoPropertyPlaybackRate];
                let _: () = msg_send![&self.dict, setObject: &*default_rate_ns, forKey: MPNowPlayingInfoPropertyDefaultPlaybackRate];

                if let Some(pos) = progress {
                    let elapsed_ns = NSNumber::new_f64(pos.as_secs_f64());
                    let _: () = msg_send![&self.dict, setObject: &*elapsed_ns, forKey: MPNowPlayingInfoPropertyElapsedPlaybackTime];
                }
            }

            self.publish();
            Ok(())
        }

        pub fn stop(&mut self) -> Result<(), MacosMediaError> {
            let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
            unsafe {
                center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                let rate_ns = NSNumber::new_f64(0.0);
                let _: () = msg_send![&self.dict, setObject: &*rate_ns, forKey: MPNowPlayingInfoPropertyPlaybackRate];
            }
            self.publish();
            Ok(())
        }

        fn publish(&self) {
            let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
            let dict_ref: &NSDictionary<NSString, AnyObject> = &self.dict;
            unsafe { center.setNowPlayingInfo(Some(dict_ref)) };
        }
    }

    impl Drop for MacMediaControls {
        fn drop(&mut self) {
            let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
            unsafe {
                center.playCommand().setEnabled(false);
                center.pauseCommand().setEnabled(false);
                center.togglePlayPauseCommand().setEnabled(false);
                center.stopCommand().setEnabled(false);
                center.nextTrackCommand().setEnabled(false);
                center.previousTrackCommand().setEnabled(false);
                center.changePlaybackPositionCommand().setEnabled(false);

                for target in &self.targets {
                    center.playCommand().removeTarget(Some(target));
                    center.pauseCommand().removeTarget(Some(target));
                    center.togglePlayPauseCommand().removeTarget(Some(target));
                    center.stopCommand().removeTarget(Some(target));
                    center.nextTrackCommand().removeTarget(Some(target));
                    center.previousTrackCommand().removeTarget(Some(target));
                    center.changePlaybackPositionCommand().removeTarget(Some(target));
                }

                let info_center = MPNowPlayingInfoCenter::defaultCenter();
                info_center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                info_center.setNowPlayingInfo(None);
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use std::path::Path;
    use std::time::Duration;
    use super::{MacosMediaError, MediaControlEvent};

    pub struct MacMediaControls;

    impl MacMediaControls {
        pub fn new<F>(_app_name: &'static str, _event_handler: F) -> Result<Self, MacosMediaError>
        where
            F: Fn(MediaControlEvent) + Send + 'static,
        {
            Ok(Self)
        }

        pub fn set_metadata(
            &mut self,
            _title: &str,
            _artist: &str,
            _album: &str,
            _duration: Option<Duration>,
            _cover_url: Option<&str>,
        ) -> Result<(), MacosMediaError> {
            Ok(())
        }

        pub fn set_artwork(&mut self, _image_data: &[u8]) -> Result<(), MacosMediaError> {
            Ok(())
        }

        pub fn set_artwork_file(&mut self, _path: &Path) -> Result<(), MacosMediaError> {
            Ok(())
        }

        pub fn set_playback_state(&mut self, _is_playing: bool, _progress: Option<Duration>) -> Result<(), MacosMediaError> {
            Ok(())
        }

        pub fn stop(&mut self) -> Result<(), MacosMediaError> {
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::MacMediaControls;

#[cfg(not(target_os = "macos"))]
pub use stub::MacMediaControls;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn now_playing_metadata_and_playback_state_lifecycle() {
        use objc2_foundation::NSString;
        use objc2_media_player::{
            MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork,
            MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
            MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState,
        };

        let mut controls = MacMediaControls::new("TIDE-TEST", |_| {}).expect("controls init");

        controls
            .set_metadata("In the End", "Linkin Park", "Hybrid Theory", Some(Duration::from_secs(216)), None)
            .expect("metadata set");

        controls.set_playback_state(true, Some(Duration::from_secs(30))).expect("playback set");

        let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        assert_eq!(unsafe { center.playbackState() }, MPNowPlayingPlaybackState::Playing);

        let info = unsafe { center.nowPlayingInfo() }.expect("now playing info present");
        let title: Option<objc2::rc::Retained<NSString>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyTitle] };
        assert_eq!(title.as_deref().map(|s| s.to_string()), Some("In the End".into()));

        let artist: Option<objc2::rc::Retained<NSString>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyArtist] };
        assert_eq!(artist.as_deref().map(|s| s.to_string()), Some("Linkin Park".into()));

        let album: Option<objc2::rc::Retained<NSString>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyAlbumTitle] };
        assert_eq!(album.as_deref().map(|s| s.to_string()), Some("Hybrid Theory".into()));

        let rate: Option<objc2::rc::Retained<objc2_foundation::NSNumber>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPNowPlayingInfoPropertyPlaybackRate] };
        assert_eq!(rate.map(|n| n.as_f64()), Some(1.0));

        let elapsed: Option<objc2::rc::Retained<objc2_foundation::NSNumber>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPNowPlayingInfoPropertyElapsedPlaybackTime] };
        assert_eq!(elapsed.map(|n| n.as_f64()), Some(30.0));

        // Pause
        controls.set_playback_state(false, Some(Duration::from_secs(45))).expect("pause set");
        assert_eq!(unsafe { center.playbackState() }, MPNowPlayingPlaybackState::Paused);
        let info = unsafe { center.nowPlayingInfo() }.expect("now playing info present");
        let rate: Option<objc2::rc::Retained<objc2_foundation::NSNumber>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPNowPlayingInfoPropertyPlaybackRate] };
        assert_eq!(rate.map(|n| n.as_f64()), Some(0.0));

        // Artwork
        #[rustfmt::skip]
        let png = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
            0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00,
            0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
            0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
            0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        controls.set_artwork(&png).expect("artwork set");
        let info = unsafe { center.nowPlayingInfo() }.expect("now playing info present");
        let artwork: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyArtwork] };
        assert!(artwork.is_some(), "artwork should be present in dictionary");

        // Stop
        controls.stop().expect("stop set");
        assert_eq!(unsafe { center.playbackState() }, MPNowPlayingPlaybackState::Stopped);
    }
}

