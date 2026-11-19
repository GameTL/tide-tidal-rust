use std::time::{Duration, Instant};
use thiserror::Error;

pub mod video;
pub use video::{VideoDownloadStats, VideoMediaInfo, VideoPlayer, VideoStatus, bgra_to_rgba};

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

/// Decode AppKit's NX_SYSDEFINED media-key payload (IOKit/hidsystem/ev_keymap.h).
/// The bool is true only for the initial key-down; releases and repeats must
/// still be consumed without toggling playback again.
#[cfg(any(target_os = "macos", test))]
fn media_key(data: isize) -> Option<(MediaControlEvent, bool)> {
    let command = match (data >> 16) & 0xffff {
        16 => MediaControlEvent::Toggle,   // NX_KEYTYPE_PLAY
        17 => MediaControlEvent::Next,     // NX_KEYTYPE_NEXT
        18 => MediaControlEvent::Previous, // NX_KEYTYPE_PREVIOUS
        19 => MediaControlEvent::Next,     // NX_KEYTYPE_FAST
        20 => MediaControlEvent::Previous, // NX_KEYTYPE_REWIND
        _ => return None,
    };
    let state = (data >> 8) & 0xff;
    if state != 0x0a && state != 0x0b {
        return None;
    }
    Some((command, state == 0x0a && data & 1 == 0))
}

/// Play/pause, next, and previous each arrive twice on macOS: AppKit's
/// NX_SYSDEFINED event and MPRemoteCommandCenter. Treat those as one press.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaCommandClass {
    Transport,
    Next,
    Previous,
}

fn media_command_class(event: &MediaControlEvent) -> Option<MediaCommandClass> {
    match event {
        MediaControlEvent::Play
        | MediaControlEvent::Pause
        | MediaControlEvent::Toggle
        | MediaControlEvent::Stop => Some(MediaCommandClass::Transport),
        MediaControlEvent::Next => Some(MediaCommandClass::Next),
        MediaControlEvent::Previous => Some(MediaCommandClass::Previous),
        MediaControlEvent::SetPosition(_) | MediaControlEvent::SetShuffle(_) => None,
    }
}

const MEDIA_COMMAND_COALESCE: Duration = Duration::from_millis(300);

fn accept_media_command(
    last: &mut Option<(MediaCommandClass, Instant)>,
    event: &MediaControlEvent,
    now: Instant,
) -> bool {
    let Some(class) = media_command_class(event) else {
        return true;
    };
    if let Some((prev, at)) = *last
        && prev == class
        && now.saturating_duration_since(at) < MEDIA_COMMAND_COALESCE
    {
        return false;
    }
    *last = Some((class, now));
    true
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::Path;
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{AnyThread, MainThreadMarker, msg_send};
    use objc2_app_kit::{NSApplication, NSEvent, NSEventMask, NSImage};
    use objc2_core_foundation::CGSize;
    use objc2_foundation::{NSData, NSDictionary, NSMutableDictionary, NSNumber, NSString};
    use objc2_media_player::{
        MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle,
        MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
        MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
        MPNowPlayingInfoPropertyDefaultPlaybackRate, MPNowPlayingInfoPropertyElapsedPlaybackTime,
        MPNowPlayingInfoPropertyMediaType, MPNowPlayingInfoPropertyPlaybackRate,
        MPNowPlayingPlaybackState, MPRemoteCommandCenter, MPRemoteCommandEvent,
        MPRemoteCommandHandlerStatus,
    };

    use super::{MacosMediaError, MediaControlEvent, MediaPosition};

    pub struct MacMediaControls {
        targets: Vec<Retained<AnyObject>>,
        dict: Retained<NSMutableDictionary<NSString, AnyObject>>,
        local_monitor: Option<Retained<AnyObject>>,
        /// Kept so the coalescer Arc outlives the command-center blocks.
        _handler: Arc<dyn Fn(MediaControlEvent) + Send + Sync>,
    }

    unsafe impl Send for MacMediaControls {}
    unsafe impl Sync for MacMediaControls {}

    impl MacMediaControls {
        pub fn new<F>(_app_name: &'static str, event_handler: F) -> Result<Self, MacosMediaError>
        where
            F: Fn(MediaControlEvent) + Send + Sync + 'static,
        {
            let command_center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
            let inner: Arc<dyn Fn(MediaControlEvent) + Send + Sync> = Arc::new(event_handler);
            let last = Arc::new(Mutex::new(None));
            let handler: Arc<dyn Fn(MediaControlEvent) + Send + Sync> = Arc::new(move |event| {
                let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                if super::accept_media_command(&mut last, &event, Instant::now()) {
                    inner(event);
                }
            });
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
                let pos_event =
                    unsafe { &*(event.as_ptr() as *const MPChangePlaybackPositionCommandEvent) };
                let secs = unsafe { pos_event.positionTime() };
                if secs.is_finite() && secs >= 0.0 {
                    h(MediaControlEvent::SetPosition(MediaPosition(
                        Duration::from_secs_f64(secs),
                    )));
                }
                MPRemoteCommandHandlerStatus::Success
            });
            let target = unsafe { pos_cmd.addTargetWithHandler(&pos_block) };
            targets.push(target);

            let dict = NSMutableDictionary::new();

            // AppKit can deliver physical media keys to the foreground app
            // before it has become the system's Now Playing owner. Winit does
            // not translate these system-defined events into keyboard input.
            // Consume both edges so one press cannot also reach another player.
            // Background/Control Center commands continue through MPRemoteCommandCenter.
            let h = handler.clone();
            let local_block = RcBlock::new(move |event: NonNull<NSEvent>| {
                // SAFETY: AppKit keeps the event alive throughout this callback.
                let native_event = unsafe { event.as_ref() };
                let Some(mtm) = MainThreadMarker::new() else {
                    return event.as_ptr();
                };
                let app = NSApplication::sharedApplication(mtm);
                if !app.isActive() || app.keyWindow().is_none() || native_event.subtype().0 != 8 {
                    return event.as_ptr();
                }
                let Some((command, dispatch)) = super::media_key(native_event.data1()) else {
                    return event.as_ptr();
                };
                if dispatch {
                    h(command);
                }
                std::ptr::null_mut()
            });
            let local_monitor = MainThreadMarker::new().and_then(|_| {
                // SAFETY: on the main thread, and the block returns the
                // original live event or null.
                unsafe {
                    NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                        NSEventMask::SystemDefined,
                        &local_block,
                    )
                }
            });
            if local_monitor.is_none() {
                tracing::warn!("Could not install foreground media-key handling");
            }

            Ok(Self {
                targets,
                dict,
                local_monitor,
                _handler: handler,
            })
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
                let _: () =
                    msg_send![&self.dict, setObject: &*title_ns, forKey: MPMediaItemPropertyTitle];
                let _: () = msg_send![&self.dict, setObject: &*artist_ns, forKey: MPMediaItemPropertyArtist];
                let _: () = msg_send![&self.dict, setObject: &*album_ns, forKey: MPMediaItemPropertyAlbumTitle];
                let _: () = msg_send![&self.dict, setObject: &*media_type, forKey: MPNowPlayingInfoPropertyMediaType];

                if let Some(dur) = duration {
                    let dur_ns = NSNumber::new_f64(dur.as_secs_f64());
                    let _: () = msg_send![&self.dict, setObject: &*dur_ns, forKey: MPMediaItemPropertyPlaybackDuration];
                }
            }

            // A new item starts without the previous one's artwork: until its
            // own cover is on disk the system shows a placeholder, not a stale one.
            let local = cover_url
                .map(|url| Path::new(url.strip_prefix("file://").unwrap_or(url)))
                .filter(|path| path.exists());
            if local.is_none_or(|path| self.set_artwork_file(path).is_err()) {
                unsafe {
                    let _: () = msg_send![&self.dict, removeObjectForKey: MPMediaItemPropertyArtwork];
                }
            }

            self.publish();
            Ok(())
        }

        pub fn set_artwork(&mut self, image_data: &[u8]) -> Result<(), MacosMediaError> {
            let data = NSData::with_bytes(image_data);
            let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
                return Err(MacosMediaError::Generic(
                    "Failed to decode artwork image".into(),
                ));
            };
            self.apply_artwork(image);
            Ok(())
        }

        pub fn set_artwork_file(&mut self, path: &Path) -> Result<(), MacosMediaError> {
            let path_str = path.to_string_lossy();
            let path_ns = NSString::from_str(&path_str);
            let Some(image) = NSImage::initWithContentsOfFile(NSImage::alloc(), &path_ns) else {
                return Err(MacosMediaError::Generic(format!(
                    "Failed to load artwork file: {}",
                    path.display()
                )));
            };
            self.apply_artwork(image);
            Ok(())
        }

        fn apply_artwork(&mut self, image: Retained<NSImage>) {
            let size = image.size();
            let cg_size = CGSize {
                width: size.width,
                height: size.height,
            };
            // The block owns the image, so it is released with the artwork
            // instead of leaking one decoded cover per track.
            let handler = RcBlock::new(move |_req_size: CGSize| -> NonNull<NSImage> {
                NonNull::from(&*image)
            });
            let artwork = unsafe {
                MPMediaItemArtwork::initWithBoundsSize_requestHandler(
                    MPMediaItemArtwork::alloc(),
                    cg_size,
                    &handler,
                )
            };
            unsafe {
                let _: () =
                    msg_send![&self.dict, setObject: &*artwork, forKey: MPMediaItemPropertyArtwork];
            }
            self.publish();
        }

        pub fn set_playback_state(
            &mut self,
            is_playing: bool,
            progress: Option<Duration>,
        ) -> Result<(), MacosMediaError> {
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
            if let Some(monitor) = self.local_monitor.take() {
                // SAFETY: this token came from NSEvent's monitor registration.
                unsafe { NSEvent::removeMonitor(&monitor) };
            }
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
                    center
                        .changePlaybackPositionCommand()
                        .removeTarget(Some(target));
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
    use super::{MacosMediaError, MediaControlEvent};
    use std::path::Path;
    use std::time::Duration;

    pub struct MacMediaControls;

    impl MacMediaControls {
        pub fn new<F>(_app_name: &'static str, _event_handler: F) -> Result<Self, MacosMediaError>
        where
            F: Fn(MediaControlEvent) + Send + Sync + 'static,
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

        pub fn set_playback_state(
            &mut self,
            _is_playing: bool,
            _progress: Option<Duration>,
        ) -> Result<(), MacosMediaError> {
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
    fn one_physical_keypress_does_not_play_then_immediately_stop() {
        use std::time::Instant;

        let mut last = None;
        let t0 = Instant::now();
        // Local NX_KEYTYPE_PLAY (Toggle) plus the Now Playing Pause/Toggle
        // that macOS delivers for the same F8/play-pause press.
        assert!(accept_media_command(
            &mut last,
            &MediaControlEvent::Toggle,
            t0
        ));
        assert!(
            !accept_media_command(
                &mut last,
                &MediaControlEvent::Pause,
                t0 + Duration::from_millis(40)
            ),
            "Pause from MPRemoteCommandCenter must not undo the local Toggle"
        );
        assert!(!accept_media_command(
            &mut last,
            &MediaControlEvent::Toggle,
            t0 + Duration::from_millis(80)
        ));
        assert!(!accept_media_command(
            &mut last,
            &MediaControlEvent::Play,
            t0 + Duration::from_millis(90)
        ));
        assert!(
            accept_media_command(
                &mut last,
                &MediaControlEvent::Next,
                t0 + Duration::from_millis(90)
            ),
            "skip is a different key and must still work"
        );
        assert!(!accept_media_command(
            &mut last,
            &MediaControlEvent::Next,
            t0 + Duration::from_millis(120)
        ));
        assert!(accept_media_command(
            &mut last,
            &MediaControlEvent::Toggle,
            t0 + Duration::from_millis(400)
        ));
        assert!(accept_media_command(
            &mut last,
            &MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs(12))),
            t0 + Duration::from_millis(410)
        ));
    }

    #[test]
    fn physical_media_keys_dispatch_once_and_leave_other_system_keys_alone() {
        for (key, command) in [
            (16, MediaControlEvent::Toggle),
            (17, MediaControlEvent::Next),
            (18, MediaControlEvent::Previous),
            (19, MediaControlEvent::Next),
            (20, MediaControlEvent::Previous),
        ] {
            assert_eq!(
                media_key((key << 16) | 0x0a00),
                Some((command.clone(), true))
            );
            assert_eq!(
                media_key((key << 16) | 0x0b00),
                Some((command.clone(), false))
            );
            assert_eq!(media_key((key << 16) | 0x0a01), Some((command, false)));
        }
        for key in [0, 1, 7, 21] {
            assert_eq!(media_key((key << 16) | 0x0a00), None);
        }
        assert_eq!(media_key(16 << 16), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn now_playing_metadata_and_playback_state_lifecycle() {
        use objc2_foundation::NSString;
        use objc2_media_player::{
            MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork,
            MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
            MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate,
            MPNowPlayingPlaybackState,
        };

        let mut controls = MacMediaControls::new("TIDE-TEST", |_| {}).expect("controls init");

        controls
            .set_metadata(
                "In the End",
                "Linkin Park",
                "Hybrid Theory",
                Some(Duration::from_secs(216)),
                None,
            )
            .expect("metadata set");

        controls
            .set_playback_state(true, Some(Duration::from_secs(30)))
            .expect("playback set");

        let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        assert_eq!(
            unsafe { center.playbackState() },
            MPNowPlayingPlaybackState::Playing
        );

        let info = unsafe { center.nowPlayingInfo() }.expect("now playing info present");
        let title: Option<objc2::rc::Retained<NSString>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyTitle] };
        assert_eq!(
            title.as_deref().map(|s| s.to_string()),
            Some("In the End".into())
        );

        let artist: Option<objc2::rc::Retained<NSString>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyArtist] };
        assert_eq!(
            artist.as_deref().map(|s| s.to_string()),
            Some("Linkin Park".into())
        );

        let album: Option<objc2::rc::Retained<NSString>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyAlbumTitle] };
        assert_eq!(
            album.as_deref().map(|s| s.to_string()),
            Some("Hybrid Theory".into())
        );

        let rate: Option<objc2::rc::Retained<objc2_foundation::NSNumber>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPNowPlayingInfoPropertyPlaybackRate] };
        assert_eq!(rate.map(|n| n.as_f64()), Some(1.0));

        let elapsed: Option<objc2::rc::Retained<objc2_foundation::NSNumber>> = unsafe {
            objc2::msg_send![&*info, objectForKey: MPNowPlayingInfoPropertyElapsedPlaybackTime]
        };
        assert_eq!(elapsed.map(|n| n.as_f64()), Some(30.0));

        // Pause
        controls
            .set_playback_state(false, Some(Duration::from_secs(45)))
            .expect("pause set");
        assert_eq!(
            unsafe { center.playbackState() },
            MPNowPlayingPlaybackState::Paused
        );
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

        // The next item, cover not on disk yet: no stale artwork from the last one.
        controls
            .set_metadata("Next", "Artist", "Album", None, Some("/nonexistent/cover.jpg"))
            .expect("metadata set");
        let info = unsafe { center.nowPlayingInfo() }.expect("now playing info present");
        let artwork: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
            unsafe { objc2::msg_send![&*info, objectForKey: MPMediaItemPropertyArtwork] };
        assert!(artwork.is_none(), "previous artwork should be cleared");

        // Stop
        controls.stop().expect("stop set");
        assert_eq!(
            unsafe { center.playbackState() },
            MPNowPlayingPlaybackState::Stopped
        );
    }
}
