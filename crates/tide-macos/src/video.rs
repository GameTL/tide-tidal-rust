//! Video playback through AVFoundation.
//!
//! `AVPlayer` streams TIDAL's HLS playlists and decodes them in hardware
//! through VideoToolbox. Decoded frames are pulled from an
//! `AVPlayerItemVideoOutput` as BGRA pixel buffers, so the UI draws the video
//! itself, under its own controls, instead of a separate native view sitting on
//! top of it. AVFoundation ignores audio mixes (and so processing taps) on
//! HLS streams, so the app routes video audio through its own output by
//! decoding the stream's audio itself and muting AVPlayer.
//!
//! All calls must happen on the main thread.

/// What the player is doing, as far as the UI cares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoStatus {
    #[default]
    Idle,
    Loading,
    Playing,
    Paused,
    Ended,
    Failed,
}

/// Native HLS download counters for the current player item. AVFoundation
/// exposes completed media requests, not individual in-flight segment states.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoDownloadStats {
    pub chunks_received: u64,
    pub bytes_received: u64,
    pub buffered_ahead_secs: f64,
    pub observed_bitrate: f64,
    pub stalls: u64,
}

/// What is known about the streams being played.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoMediaInfo {
    /// Bits per second of the audio track, from its format (0 if unknown).
    pub audio_bitrate: f64,
    /// Bits per second of the video track (0 if unknown).
    pub video_bitrate: f64,
    /// The rendition's advertised total bandwidth (0 if unknown).
    pub indicated_bitrate: f64,
    /// Sample rate the audio decodes at (0 until known).
    pub audio_sample_rate: u32,
    /// "AAC", "AC-3", ... (empty if unknown).
    pub audio_codec: String,
    /// "H.264", "HEVC", ... (empty if unknown).
    pub video_codec: String,
}

/// Display name for a CoreMedia four-character media subtype.
#[cfg(any(target_os = "macos", test))]
fn codec_name(subtype: u32) -> String {
    match &subtype.to_be_bytes() {
        b"aac " => "AAC".into(),
        b"ac-3" => "AC-3".into(),
        b"ec-3" => "E-AC-3".into(),
        b".mp3" => "MP3".into(),
        b"avc1" | b"avc3" => "H.264".into(),
        b"hvc1" | b"hev1" => "HEVC".into(),
        b"av01" => "AV1".into(),
        code => String::from_utf8_lossy(code).trim().to_string(),
    }
}

#[cfg(any(target_os = "macos", test))]
impl VideoDownloadStats {
    fn add_access_event(&mut self, requests: isize, bytes: i64, stalls: isize, bitrate: f64) {
        // AVFoundation reports negative counters when a value is unknown.
        self.chunks_received = self.chunks_received.saturating_add(requests.max(0) as u64);
        self.bytes_received = self.bytes_received.saturating_add(bytes.max(0) as u64);
        self.stalls = self.stalls.saturating_add(stalls.max(0) as u64);
        if bitrate.is_finite() && bitrate > 0.0 {
            self.observed_bitrate = bitrate;
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn buffered_ahead(position: f64, mut ranges: Vec<(f64, f64)>) -> f64 {
    if !position.is_finite() || position < 0.0 {
        return 0.0;
    }
    ranges.retain(|(start, duration)| start.is_finite() && duration.is_finite() && *duration > 0.0);
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut end = position;
    for (start, duration) in ranges {
        if start > end {
            break;
        }
        end = end.max(start + duration);
    }
    (end - position).max(0.0)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    use objc2::encode::{Encode, Encoding, RefEncode};
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_core_foundation::CGSize;
    use objc2_foundation::{NSNumber, NSString};

    use super::{VideoDownloadStats, VideoMediaInfo, VideoStatus, buffered_ahead, codec_name};

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    struct CMTime {
        value: i64,
        timescale: i32,
        flags: u32,
        epoch: i64,
    }

    unsafe impl Encode for CMTime {
        const ENCODING: Encoding = Encoding::Struct(
            "?",
            &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
        );
    }

    unsafe impl RefEncode for CMTime {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTimeRange {
        start: CMTime,
        duration: CMTime,
    }

    unsafe impl Encode for CMTimeRange {
        const ENCODING: Encoding = Encoding::Struct("?", &[CMTime::ENCODING, CMTime::ENCODING]);
    }

    #[repr(C)]
    struct CVBuffer {
        _private: [u8; 0],
    }

    unsafe impl RefEncode for CVBuffer {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__CVBuffer", &[]));
    }

    #[repr(C)]
    struct OpaqueFormatDescription {
        _private: [u8; 0],
    }

    unsafe impl RefEncode for OpaqueFormatDescription {
        const ENCODING_REF: Encoding =
            Encoding::Pointer(&Encoding::Struct("opaqueCMFormatDescription", &[]));
    }

    /// `AudioStreamBasicDescription`; only the sample rate is read.
    #[repr(C)]
    struct StreamDescription {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }

    const CMTIME_VALID: u32 = 1;
    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
    const LOCK_READ_ONLY: u64 = 1;

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMTimeMakeWithSeconds(seconds: f64, preferred_timescale: i32) -> CMTime;
        fn CMTimeGetSeconds(time: CMTime) -> f64;
        fn CMFormatDescriptionGetMediaSubType(desc: *const OpaqueFormatDescription) -> u32;
        fn CMAudioFormatDescriptionGetStreamBasicDescription(
            desc: *const OpaqueFormatDescription,
        ) -> *const StreamDescription;
    }

    #[link(name = "QuartzCore", kind = "framework")]
    unsafe extern "C" {
        fn CACurrentMediaTime() -> f64;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        static kCVPixelBufferPixelFormatTypeKey: &'static NSString;
        fn CVPixelBufferLockBaseAddress(pb: *mut CVBuffer, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(pb: *mut CVBuffer, flags: u64) -> i32;
        fn CVPixelBufferGetBaseAddress(pb: *mut CVBuffer) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRow(pb: *mut CVBuffer) -> usize;
        fn CVPixelBufferGetWidth(pb: *mut CVBuffer) -> usize;
        fn CVPixelBufferGetHeight(pb: *mut CVBuffer) -> usize;
        fn CVBufferRelease(pb: *mut CVBuffer);
    }

    fn seconds(t: CMTime) -> f64 {
        if t.flags & CMTIME_VALID == 0 {
            return 0.0;
        }
        let s = unsafe { CMTimeGetSeconds(t) };
        if s.is_finite() { s.max(0.0) } else { 0.0 }
    }

    pub struct VideoPlayer {
        player: Retained<AnyObject>,
        item: Option<Retained<AnyObject>>,
        output: Option<Retained<AnyObject>>,
        /// Asked to play; AVPlayer may still be buffering.
        wants_play: bool,
        /// Position to seek to once the new item is ready.
        start_at: f64,
        started: bool,
    }

    impl Default for VideoPlayer {
        fn default() -> Self {
            Self::new()
        }
    }

    impl VideoPlayer {
        pub fn new() -> Self {
            let player: Retained<AnyObject> = unsafe { msg_send![class!(AVPlayer), new] };
            Self {
                player,
                item: None,
                output: None,
                wants_play: false,
                start_at: 0.0,
                started: false,
            }
        }

        /// The asset track of the item's first track of `media` ("soun", "vide").
        fn asset_track(item: &AnyObject, media: &str) -> Option<Retained<AnyObject>> {
            unsafe {
                let tracks: Retained<AnyObject> = msg_send![item, tracks];
                let count: usize = msg_send![&tracks, count];
                (0..count).find_map(|i| {
                    let track: Retained<AnyObject> = msg_send![&tracks, objectAtIndex: i];
                    let asset: Option<Retained<AnyObject>> = msg_send![&track, assetTrack];
                    let asset = asset?;
                    let kind: Retained<NSString> = msg_send![&asset, mediaType];
                    (kind.to_string() == media).then_some(asset)
                })
            }
        }

        /// Open `url` (an HLS playlist) and start playing from `start_secs`.
        pub fn load(&mut self, url: &str, start_secs: f64) {
            unsafe {
                let ns = NSString::from_str(url);
                let nsurl: Option<Retained<AnyObject>> =
                    msg_send![class!(NSURL), URLWithString: &*ns];
                let Some(nsurl) = nsurl else {
                    tracing::error!(url, "video URL is not valid");
                    return;
                };
                let item: Retained<AnyObject> =
                    msg_send![class!(AVPlayerItem), playerItemWithURL: &*nsurl];
                let _: () = msg_send![&item, setPreferredForwardBufferDuration: 8.0f64];
                let format = NSNumber::new_u32(BGRA);
                let attrs: Retained<AnyObject> = msg_send![
                    class!(NSDictionary),
                    dictionaryWithObject: &*format,
                    forKey: kCVPixelBufferPixelFormatTypeKey
                ];
                let alloc: Allocated<AnyObject> = msg_send![class!(AVPlayerItemVideoOutput), alloc];
                let output: Retained<AnyObject> =
                    msg_send![alloc, initWithPixelBufferAttributes: &*attrs];
                let _: () = msg_send![&item, addOutput: &*output];
                let _: () = msg_send![&self.player, replaceCurrentItemWithPlayerItem: Some(&*item)];
                self.item = Some(item);
                self.output = Some(output);
            }
            self.start_at = start_secs.max(0.0);
            self.started = false;
            self.wants_play = true;
            self.poll();
        }

        /// Drive start-up; call regularly (each frame pull does).
        fn poll(&mut self) {
            if self.started {
                return;
            }
            let Some(item) = &self.item else { return };
            let status: isize = unsafe { msg_send![item, status] };
            // AVPlayerItemStatusReadyToPlay
            if status != 1 {
                return;
            }
            self.started = true;
            if self.start_at > 0.5 {
                self.seek(self.start_at);
            }
            if self.wants_play {
                unsafe {
                    let _: () = msg_send![&self.player, play];
                }
            }
        }

        pub fn play(&mut self) {
            self.wants_play = true;
            if self.started {
                unsafe {
                    let _: () = msg_send![&self.player, play];
                }
            }
        }

        pub fn pause(&mut self) {
            self.wants_play = false;
            unsafe {
                let _: () = msg_send![&self.player, pause];
            }
        }

        /// Stop and release the current item and its decoder.
        pub fn stop(&mut self) {
            self.pause();
            unsafe {
                let none: Option<&AnyObject> = None;
                let _: () = msg_send![&self.player, replaceCurrentItemWithPlayerItem: none];
            }
            self.item = None;
            self.output = None;
            self.started = false;
        }

        /// Codecs and bitrates of the tracks playing now.
        pub fn media_info(&self) -> VideoMediaInfo {
            let mut info = VideoMediaInfo::default();
            let Some(item) = &self.item else { return info };
            unsafe {
                for (media, is_audio) in [("soun", true), ("vide", false)] {
                    let Some(track) = Self::asset_track(item, media) else {
                        continue;
                    };
                    let rate: f32 = msg_send![&track, estimatedDataRate];
                    let rate = if rate.is_finite() && rate > 0.0 {
                        rate as f64
                    } else {
                        0.0
                    };
                    let descs: Retained<AnyObject> = msg_send![&track, formatDescriptions];
                    let count: usize = msg_send![&descs, count];
                    let desc: Option<Retained<AnyObject>> =
                        (count > 0).then(|| msg_send![&descs, objectAtIndex: 0usize]);
                    let desc = desc
                        .as_ref()
                        .map(|d| Retained::as_ptr(d) as *const OpaqueFormatDescription);
                    let codec = desc
                        .map(|d| codec_name(CMFormatDescriptionGetMediaSubType(d)))
                        .unwrap_or_default();
                    if is_audio {
                        info.audio_bitrate = rate;
                        info.audio_codec = codec;
                        if let Some(asbd) = desc.and_then(|d| {
                            CMAudioFormatDescriptionGetStreamBasicDescription(d).as_ref()
                        }) {
                            info.audio_sample_rate = asbd.sample_rate.round() as u32;
                        }
                    } else {
                        info.video_bitrate = rate;
                        info.video_codec = codec;
                    }
                }
                let log: Option<Retained<AnyObject>> = msg_send![item, accessLog];
                if let Some(log) = log {
                    let events: Retained<AnyObject> = msg_send![&log, events];
                    let count: usize = msg_send![&events, count];
                    if count > 0 {
                        let event: Retained<AnyObject> =
                            msg_send![&events, objectAtIndex: count - 1];
                        let indicated: f64 = msg_send![&event, indicatedBitrate];
                        if indicated.is_finite() && indicated > 0.0 {
                            info.indicated_bitrate = indicated;
                        }
                        let audio: f64 = msg_send![&event, averageAudioBitrate];
                        let video: f64 = msg_send![&event, averageVideoBitrate];
                        if info.audio_bitrate == 0.0 && audio.is_finite() && audio > 0.0 {
                            info.audio_bitrate = audio;
                        }
                        if info.video_bitrate == 0.0 && video.is_finite() && video > 0.0 {
                            info.video_bitrate = video;
                        }
                    }
                }
            }
            info
        }

        pub fn seek(&mut self, secs: f64) {
            if !self.started {
                self.start_at = secs;
                return;
            }
            unsafe {
                let to = CMTimeMakeWithSeconds(secs.max(0.0), 600);
                let tolerance = CMTimeMakeWithSeconds(0.0, 600);
                let _: () = msg_send![
                    &self.player,
                    seekToTime: to,
                    toleranceBefore: tolerance,
                    toleranceAfter: tolerance
                ];
            }
        }

        pub fn set_volume(&self, volume: f32) {
            unsafe {
                let _: () = msg_send![&self.player, setVolume: volume.clamp(0.0, 1.0)];
            }
        }

        pub fn position(&self) -> f64 {
            if !self.started {
                return self.start_at;
            }
            seconds(unsafe { msg_send![&self.player, currentTime] })
        }

        pub fn duration(&self) -> f64 {
            match &self.item {
                Some(item) => seconds(unsafe { msg_send![item, duration] }),
                None => 0.0,
            }
        }

        /// Decoded size of the stream on screen, if known.
        pub fn presentation_size(&self) -> (u32, u32) {
            match &self.item {
                Some(item) => {
                    let size: CGSize = unsafe { msg_send![item, presentationSize] };
                    (size.width as u32, size.height as u32)
                }
                None => (0, 0),
            }
        }

        /// Snapshot the current item's actual HLS requests and buffered ranges.
        /// Access-log events cover different playback periods/renditions; their
        /// counters are per event, so sum them once on each snapshot.
        pub fn download_stats(&self) -> VideoDownloadStats {
            let mut stats = VideoDownloadStats::default();
            let Some(item) = &self.item else { return stats };
            unsafe {
                let log: Option<Retained<AnyObject>> = msg_send![item, accessLog];
                if let Some(log) = log {
                    let events: Retained<AnyObject> = msg_send![&log, events];
                    let count: usize = msg_send![&events, count];
                    for i in 0..count {
                        let event: Retained<AnyObject> = msg_send![&events, objectAtIndex: i];
                        // For HLS, numberOfMediaRequests counts downloaded segments.
                        stats.add_access_event(
                            msg_send![&event, numberOfMediaRequests],
                            msg_send![&event, numberOfBytesTransferred],
                            msg_send![&event, numberOfStalls],
                            msg_send![&event, observedBitrate],
                        );
                    }
                }
                let loaded: Retained<AnyObject> = msg_send![item, loadedTimeRanges];
                let count: usize = msg_send![&loaded, count];
                let mut ranges = Vec::with_capacity(count);
                for i in 0..count {
                    let value: Retained<AnyObject> = msg_send![&loaded, objectAtIndex: i];
                    let range: CMTimeRange = msg_send![&value, CMTimeRangeValue];
                    ranges.push((seconds(range.start), seconds(range.duration)));
                }
                stats.buffered_ahead_secs = buffered_ahead(self.position(), ranges);
            }
            stats
        }

        pub fn status(&self) -> VideoStatus {
            let Some(item) = &self.item else {
                return VideoStatus::Idle;
            };
            let item_status: isize = unsafe { msg_send![item, status] };
            if item_status == 2 {
                return VideoStatus::Failed;
            }
            let duration = self.duration();
            if self.started && duration > 0.0 && self.position() >= duration - 0.15 {
                return VideoStatus::Ended;
            }
            if !self.wants_play {
                return VideoStatus::Paused;
            }
            // AVPlayerTimeControlStatusPlaying
            let control: isize = unsafe { msg_send![&self.player, timeControlStatus] };
            if self.started && control == 2 {
                VideoStatus::Playing
            } else {
                VideoStatus::Loading
            }
        }

        /// The reason playback failed, if it did.
        pub fn error(&self) -> Option<String> {
            let item = self.item.as_ref()?;
            let err: Option<Retained<AnyObject>> = unsafe { msg_send![item, error] };
            let err = err?;
            let text: Retained<NSString> = unsafe { msg_send![&err, localizedDescription] };
            Some(text.to_string())
        }

        /// Hand the frame due now, if it is new, to `draw` as
        /// `(width, height, bytes_per_row, bgra)`.
        pub fn pull_frame(&mut self, draw: impl FnOnce(u32, u32, usize, &[u8])) -> bool {
            self.poll();
            let Some(output) = &self.output else {
                return false;
            };
            unsafe {
                let now: CMTime = msg_send![output, itemTimeForHostTime: CACurrentMediaTime()];
                let fresh: bool = msg_send![output, hasNewPixelBufferForItemTime: now];
                if !fresh {
                    return false;
                }
                let shown: *mut CMTime = std::ptr::null_mut();
                let pb: *mut CVBuffer =
                    msg_send![output, copyPixelBufferForItemTime: now, itemTimeForDisplay: shown];
                if pb.is_null() {
                    return false;
                }
                let mut drawn = false;
                if CVPixelBufferLockBaseAddress(pb, LOCK_READ_ONLY) == 0 {
                    let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
                    let stride = CVPixelBufferGetBytesPerRow(pb);
                    let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
                    if !base.is_null() && w > 0 && h > 0 && stride >= w * 4 {
                        let bytes = std::slice::from_raw_parts(base, stride * h);
                        draw(w as u32, h as u32, stride, bytes);
                        drawn = true;
                    }
                    CVPixelBufferUnlockBaseAddress(pb, LOCK_READ_ONLY);
                }
                CVBufferRelease(pb);
                drawn
            }
        }
    }

    impl Drop for VideoPlayer {
        fn drop(&mut self) {
            self.stop();
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{VideoDownloadStats, VideoMediaInfo, VideoStatus};

    #[derive(Default)]
    pub struct VideoPlayer;

    impl VideoPlayer {
        pub fn new() -> Self {
            Self
        }
        pub fn media_info(&self) -> VideoMediaInfo {
            VideoMediaInfo::default()
        }
        pub fn load(&mut self, _url: &str, _start_secs: f64) {}
        pub fn play(&mut self) {}
        pub fn pause(&mut self) {}
        pub fn stop(&mut self) {}
        pub fn seek(&mut self, _secs: f64) {}
        pub fn set_volume(&self, _volume: f32) {}
        pub fn position(&self) -> f64 {
            0.0
        }
        pub fn duration(&self) -> f64 {
            0.0
        }
        pub fn presentation_size(&self) -> (u32, u32) {
            (0, 0)
        }
        pub fn download_stats(&self) -> VideoDownloadStats {
            VideoDownloadStats::default()
        }
        pub fn status(&self) -> VideoStatus {
            VideoStatus::Failed
        }
        pub fn error(&self) -> Option<String> {
            Some("video playback needs macOS".into())
        }
        pub fn pull_frame(&mut self, _draw: impl FnOnce(u32, u32, usize, &[u8])) -> bool {
            false
        }
    }
}

pub use imp::VideoPlayer;

/// Convert one BGRA frame (rows `stride` bytes apart) into tightly packed RGBA.
pub fn bgra_to_rgba(src: &[u8], width: usize, height: usize, stride: usize, dst: &mut [u8]) {
    let row = width * 4;
    for y in 0..height {
        let from = &src[y * stride..y * stride + row];
        let to = &mut dst[y * row..(y + 1) * row];
        for (d, s) in to
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(from.as_chunks::<4>().0)
        {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn access_counters_sum_events_and_ignore_unknown_values() {
        let mut stats = super::VideoDownloadStats::default();
        stats.add_access_event(4, 1024, 1, 8_000_000.0);
        stats.add_access_event(2, 2048, 0, 12_000_000.0);
        stats.add_access_event(-1, -1, -1, f64::NAN);
        assert_eq!(stats.chunks_received, 6);
        assert_eq!(stats.bytes_received, 3072);
        assert_eq!(stats.stalls, 1);
        assert_eq!(stats.observed_bitrate, 12_000_000.0);
    }

    #[test]
    fn buffer_ahead_merges_ranges_but_does_not_count_past_a_gap() {
        let ranges = vec![(20.0, 10.0), (6.0, 4.0), (0.0, 8.0), (f64::NAN, 5.0)];
        assert_eq!(super::buffered_ahead(5.0, ranges.clone()), 5.0);
        assert_eq!(super::buffered_ahead(12.0, ranges.clone()), 0.0);
        assert_eq!(super::buffered_ahead(25.0, ranges), 5.0);
    }

    #[test]
    fn bgra_rows_are_swizzled_and_stride_skipped() {
        // 2x2 image, 12-byte rows (4 bytes padding).
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, //
            7, 8, 9, 0, 10, 11, 12, 0, 9, 9, 9, 9,
        ];
        let mut dst = [0u8; 16];
        super::bgra_to_rgba(&src, 2, 2, 12, &mut dst);
        assert_eq!(
            dst,
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }
}
