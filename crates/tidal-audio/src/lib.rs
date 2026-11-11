//! Symphonia decoder feeding a cpal (CoreAudio) output stream.
//!
//! The decoder thread keeps ~1.5 s of interleaved stereo samples buffered at the
//! device rate. Track boundaries are marked by sample index so that the output
//! callback switches "current track" exactly when the first sample of the next
//! track is played, which is what makes gapless transitions sample-accurate.

use std::collections::VecDeque;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AudioError {
    #[error("Device error: {0}")]
    Device(String),
    #[error("Stream error: {0}")]
    Stream(String),
    #[error("Decoder error: {0}")]
    Decoder(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("No tracks found in media file")]
    NoTrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone)]
pub struct TrackQueueItem {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub file_path: PathBuf,
    pub duration: Duration,
}

/// Source format of the track that is currently audible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatInfo {
    pub bits_per_sample: u32,
    pub sample_rate: u32,
}

const NONE: u64 = u64::MAX;
/// Samples (not frames) kept decoded ahead of the output callback.
const BUFFER_AHEAD_SECS: f32 = 1.5;

struct AudioSource {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    bits_per_sample: u32,
    item: TrackQueueItem,
    sample_buf: Option<SampleBuffer<f32>>,
    // Linear resampler state (only used when the device cannot run at the source rate).
    resample_pos: f64,
    last_frame: [f32; 2],
}

impl AudioSource {
    fn open(item: TrackQueueItem) -> Result<Self, AudioError> {
        let file = File::open(&item.file_path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let probed = symphonia::default::get_probe()
            .format(&Hint::new(), mss, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| AudioError::Decoder(e.to_string()))?;

        let reader = probed.format;
        let track = reader
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or(AudioError::NoTrack)?;

        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| AudioError::Decoder(e.to_string()))?;

        Ok(Self {
            track_id: track.id,
            sample_rate: track.codec_params.sample_rate.unwrap_or(44_100),
            bits_per_sample: track.codec_params.bits_per_sample.unwrap_or(0),
            reader,
            decoder,
            item,
            sample_buf: None,
            resample_pos: 0.0,
            last_frame: [0.0; 2],
        })
    }

    fn format(&self) -> FormatInfo {
        FormatInfo { bits_per_sample: self.bits_per_sample, sample_rate: self.sample_rate }
    }

    /// Decode one packet into interleaved stereo at `out_rate`. Returns false at end of stream.
    fn read_stereo(&mut self, out_rate: u32, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        let packet = match self.reader.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(SymphoniaError::ResetRequired) => return Ok(false),
            Err(e) => return Err(AudioError::Decoder(e.to_string())),
        };
        if packet.track_id() != self.track_id {
            return Ok(true);
        }
        let decoded = match self.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => return Ok(true),
            Err(e) => return Err(AudioError::Decoder(e.to_string())),
        };

        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let needed = decoded.capacity() as u64;
        if self.sample_buf.as_ref().is_none_or(|b| (b.capacity() as u64) < needed * channels as u64) {
            self.sample_buf = Some(SampleBuffer::new(needed, spec));
        }
        let buf = self.sample_buf.as_mut().expect("sample buffer allocated above");
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        let frame = |i: usize| -> [f32; 2] {
            let base = i * channels;
            if channels == 1 { [samples[base]; 2] } else { [samples[base], samples[base + 1]] }
        };
        let frames = samples.len() / channels;

        if self.sample_rate == out_rate {
            out.reserve(frames * 2);
            for i in 0..frames {
                out.extend_from_slice(&frame(i));
            }
        } else {
            // `resample_pos` is the read position in source frames relative to this
            // packet's first frame; -1 refers to the last frame of the previous packet.
            let step = self.sample_rate as f64 / out_rate as f64;
            let mut pos = self.resample_pos;
            while pos < frames as f64 - 1.0 {
                let idx = pos.floor();
                let frac = (pos - idx) as f32;
                let a = if idx < 0.0 { self.last_frame } else { frame(idx as usize) };
                let b = frame((idx + 1.0) as usize);
                out.push(a[0] + (b[0] - a[0]) * frac);
                out.push(a[1] + (b[1] - a[1]) * frac);
                pos += step;
            }
            self.resample_pos = pos - frames as f64;
            if frames > 0 {
                self.last_frame = frame(frames - 1);
            }
        }
        Ok(true)
    }

    fn seek(&mut self, to: Duration) -> Result<(), AudioError> {
        self.reader
            .seek(SeekMode::Accurate, SeekTo::Time { time: Time::from(to.as_secs_f64()), track_id: Some(self.track_id) })
            .map_err(|e| AudioError::Decoder(e.to_string()))?;
        self.decoder.reset();
        self.resample_pos = 0.0;
        self.last_frame = [0.0; 2];
        Ok(())
    }
}

struct Playing {
    item: TrackQueueItem,
    format: FormatInfo,
}

/// Everything guarded by the sample-queue lock, so the output callback and the
/// control methods always see a consistent picture.
struct Buffer {
    samples: VecDeque<f32>,
    /// Running count of samples pushed / consumed since the last reset.
    pushed: u64,
    consumed: u64,
    /// Sample index at which `upcoming` becomes the audible track.
    switch_at: u64,
    /// Sample index after which nothing more will be played.
    end_at: u64,
    current: Option<Playing>,
    upcoming: Option<Playing>,
}

struct Shared {
    buffer: Mutex<Buffer>,
    source: Mutex<Option<AudioSource>>,
    queue: Mutex<VecDeque<TrackQueueItem>>,
    playing: AtomicBool,
    finished: AtomicBool,
    volume: AtomicU32,
    /// Device frames of the current track played so far (including the seek offset).
    frames_played: AtomicU64,
    /// Bumped every time the audible track changes on its own (gapless hand-over).
    serial: AtomicU64,
    device_rate: AtomicU32,
}

pub struct AudioPlayer {
    stream: Mutex<Option<Stream>>,
    shared: Arc<Shared>,
}

fn build_stream(shared: &Arc<Shared>, want_rate: Option<u32>) -> Result<(Stream, u32), AudioError> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| AudioError::Device("No output device available".to_string()))?;
    let default_config = device.default_output_config().map_err(|e| AudioError::Device(e.to_string()))?;

    // Prefer running the device at the track's native rate (bit-perfect, no resampling).
    let config = want_rate
        .filter(|r| *r != default_config.sample_rate())
        .and_then(|rate| {
            device.supported_output_configs().ok()?.find_map(|range| {
                (range.sample_format() == SampleFormat::F32 && range.channels() == default_config.channels())
                    .then(|| range.try_with_sample_rate(rate))
                    .flatten()
            })
        })
        .unwrap_or(default_config);

    if config.sample_format() != SampleFormat::F32 {
        return Err(AudioError::Device("Unsupported sample format".to_string()));
    }
    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    let cb_shared = Arc::clone(shared);

    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                data.fill(0.0);
                if !cb_shared.playing.load(Ordering::Relaxed) {
                    return;
                }
                let vol = cb_shared.volume.load(Ordering::Relaxed) as f32 / 1000.0;
                let gain = vol * vol;
                let mut buf = cb_shared.buffer.lock().unwrap();
                let mut frames = 0u64;
                for out in data.chunks_mut(channels) {
                    if buf.samples.len() < 2 {
                        break;
                    }
                    let l = buf.samples.pop_front().unwrap_or(0.0) * gain;
                    let r = buf.samples.pop_front().unwrap_or(0.0) * gain;
                    if channels == 1 {
                        out[0] = (l + r) * 0.5;
                    } else {
                        out[0] = l;
                        out[1] = r;
                    }
                    frames += 1;
                }
                buf.consumed += frames * 2;
                if buf.consumed >= buf.switch_at {
                    // The next track's first sample has just been played.
                    let into_next = (buf.consumed - buf.switch_at) / 2;
                    buf.switch_at = NONE;
                    if let Some(next) = buf.upcoming.take() {
                        buf.current = Some(next);
                        cb_shared.frames_played.store(into_next, Ordering::Relaxed);
                        cb_shared.serial.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    cb_shared.frames_played.fetch_add(frames, Ordering::Relaxed);
                }
                if buf.consumed >= buf.end_at {
                    buf.end_at = NONE;
                    cb_shared.finished.store(true, Ordering::Relaxed);
                    cb_shared.playing.store(false, Ordering::Relaxed);
                }
            },
            |err| tracing::error!("An error occurred on audio stream: {}", err),
            None,
        )
        .map_err(|e| AudioError::Stream(e.to_string()))?;
    stream.play().map_err(|e| AudioError::Stream(e.to_string()))?;
    Ok((stream, rate))
}

fn decoder_loop(shared: Arc<Shared>) {
    let mut chunk = Vec::with_capacity(16_384);
    loop {
        let device_rate = shared.device_rate.load(Ordering::Relaxed);
        let buffered = shared.buffer.lock().unwrap().samples.len();
        if buffered as f32 > device_rate as f32 * 2.0 * BUFFER_AHEAD_SECS {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }

        let mut source = shared.source.lock().unwrap();
        if source.is_none() {
            let Some(next) = shared.queue.lock().unwrap().pop_front() else {
                drop(source);
                std::thread::sleep(Duration::from_millis(30));
                continue;
            };
            match AudioSource::open(next) {
                Ok(opened) => {
                    tracing::info!(title = %opened.item.title, "Gapless transition queued");
                    let mut buf = shared.buffer.lock().unwrap();
                    buf.switch_at = buf.pushed;
                    buf.end_at = NONE;
                    buf.upcoming = Some(Playing { item: opened.item.clone(), format: opened.format() });
                    *source = Some(opened);
                }
                Err(e) => tracing::error!("Failed to open next audio source: {e}"),
            }
            continue;
        }

        chunk.clear();
        let more = match source.as_mut().expect("checked above").read_stereo(device_rate, &mut chunk) {
            Ok(more) => more,
            Err(e) => {
                tracing::error!("Decode error: {e}");
                false
            }
        };
        let mut buf = shared.buffer.lock().unwrap();
        buf.pushed += chunk.len() as u64;
        buf.samples.extend(chunk.drain(..));
        if !more {
            *source = None;
            if shared.queue.lock().unwrap().is_empty() {
                buf.end_at = buf.pushed;
            }
        }
    }
}

impl AudioPlayer {
    pub fn new() -> Result<Self, AudioError> {
        let shared = Arc::new(Shared {
            buffer: Mutex::new(Buffer {
                samples: VecDeque::with_capacity(48_000 * 4),
                pushed: 0,
                consumed: 0,
                switch_at: NONE,
                end_at: NONE,
                current: None,
                upcoming: None,
            }),
            source: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            playing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            volume: AtomicU32::new(1000),
            frames_played: AtomicU64::new(0),
            serial: AtomicU64::new(0),
            device_rate: AtomicU32::new(48_000),
        });

        let (stream, rate) = build_stream(&shared, None)?;
        shared.device_rate.store(rate, Ordering::Relaxed);

        let decoder_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("tidal-decoder".into())
            .spawn(move || decoder_loop(decoder_shared))
            .map_err(AudioError::Io)?;

        Ok(Self { stream: Mutex::new(Some(stream)), shared })
    }

    /// Run the output device at `rate` if it supports it; otherwise keep the current rate.
    fn match_device_rate(&self, rate: u32) {
        if self.shared.device_rate.load(Ordering::Relaxed) == rate {
            return;
        }
        let mut stream = self.stream.lock().unwrap();
        // The old stream must be gone before CoreAudio lets us reconfigure the device.
        *stream = None;
        let rebuilt = build_stream(&self.shared, Some(rate)).or_else(|e| {
            tracing::warn!("Could not open device at {rate} Hz ({e}); using default rate");
            build_stream(&self.shared, None)
        });
        match rebuilt {
            Ok((new_stream, actual)) => {
                tracing::info!(requested = rate, actual, "Output stream reconfigured");
                self.shared.device_rate.store(actual, Ordering::Relaxed);
                *stream = Some(new_stream);
            }
            Err(e) => tracing::error!("Failed to rebuild output stream: {e}"),
        }
    }

    fn reset_buffer(&self, buf: &mut Buffer, start_frames: u64) {
        buf.samples.clear();
        buf.pushed = 0;
        buf.consumed = 0;
        buf.switch_at = NONE;
        buf.end_at = NONE;
        buf.upcoming = None;
        self.shared.frames_played.store(start_frames, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
    }

    /// Stop whatever is playing and start `item` from the beginning.
    pub fn play_track(&self, item: TrackQueueItem) -> Result<(), AudioError> {
        let opened = AudioSource::open(item)?;
        self.shared.playing.store(false, Ordering::Relaxed);
        self.match_device_rate(opened.sample_rate);

        let mut source = self.shared.source.lock().unwrap();
        self.shared.queue.lock().unwrap().clear();
        let mut buf = self.shared.buffer.lock().unwrap();
        self.reset_buffer(&mut buf, 0);
        buf.current = Some(Playing { item: opened.item.clone(), format: opened.format() });
        *source = Some(opened);
        drop(buf);
        drop(source);
        self.shared.playing.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Queue a track to start seamlessly when the current one ends.
    pub fn enqueue_next(&self, item: TrackQueueItem) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.clear();
        queue.push_back(item);
        // If the decoder already hit the end of the current track, cancel the pending stop.
        let mut buf = self.shared.buffer.lock().unwrap();
        if buf.end_at != NONE && buf.consumed < buf.end_at {
            buf.end_at = NONE;
        }
    }

    pub fn clear_queue(&self) {
        self.shared.queue.lock().unwrap().clear();
    }

    /// Jump within the current track. Returns false if nothing is loaded.
    pub fn seek(&self, to: Duration) -> bool {
        let mut source = self.shared.source.lock().unwrap();
        let mut buf = self.shared.buffer.lock().unwrap();
        let Some(current_item) = buf.current.as_ref().map(|c| c.item.clone()) else { return false };

        // The decoder may already be past this track (finished or decoding the next one):
        // reopen the current file and put the pre-loaded track back in the queue.
        if source.as_ref().is_none_or(|s| s.item.id != current_item.id) {
            if let Some(ahead) = source.take() {
                self.shared.queue.lock().unwrap().push_front(ahead.item);
            }
            match AudioSource::open(current_item) {
                Ok(reopened) => *source = Some(reopened),
                Err(e) => {
                    tracing::error!("Seek failed to reopen track: {e}");
                    return false;
                }
            }
        }
        let src = source.as_mut().expect("source ensured above");
        if let Err(e) = src.seek(to) {
            tracing::warn!("Seek failed: {e}");
            return false;
        }
        let rate = self.shared.device_rate.load(Ordering::Relaxed) as f64;
        self.reset_buffer(&mut buf, (to.as_secs_f64() * rate) as u64);
        true
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        if self.shared.buffer.lock().unwrap().current.is_some() && !self.shared.finished.load(Ordering::Relaxed) {
            self.shared.playing.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// True once the last queued sample has been played.
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> PlaybackState {
        if self.is_playing() {
            PlaybackState::Playing
        } else if self.is_finished() || self.shared.buffer.lock().unwrap().current.is_none() {
            PlaybackState::Stopped
        } else {
            PlaybackState::Paused
        }
    }

    /// Id of the track that is audible right now.
    pub fn current_id(&self) -> Option<String> {
        self.shared.buffer.lock().unwrap().current.as_ref().map(|c| c.item.id.clone())
    }

    /// Changes whenever playback moved on to the next queued track by itself.
    pub fn track_serial(&self) -> u64 {
        self.shared.serial.load(Ordering::Relaxed)
    }

    pub fn format_info(&self) -> Option<FormatInfo> {
        self.shared.buffer.lock().unwrap().current.as_ref().map(|c| c.format)
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store((volume.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }

    pub fn get_volume(&self) -> f32 {
        self.shared.volume.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn elapsed(&self) -> Duration {
        let rate = self.shared.device_rate.load(Ordering::Relaxed).max(1) as f64;
        Duration::from_secs_f64(self.shared.frames_played.load(Ordering::Relaxed) as f64 / rate)
    }

    /// Current output device sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.shared.device_rate.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a mono 16-bit PCM WAV containing a sine wave.
    fn write_wav(path: &std::path::Path, rate: u32, secs: f32) {
        let n = (rate as f32 * secs) as u32;
        let mut bytes = Vec::with_capacity(44 + n as usize * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + n * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let s = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin();
            bytes.extend_from_slice(&((s * 8000.0) as i16).to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn item(path: &std::path::Path, secs: f32) -> TrackQueueItem {
        TrackQueueItem {
            id: path.file_name().unwrap().to_string_lossy().into_owned(),
            title: "sine".into(),
            artist: "test".into(),
            file_path: path.to_path_buf(),
            duration: Duration::from_secs_f32(secs),
        }
    }

    #[test]
    fn decodes_mono_to_stereo_and_resamples_to_expected_length() {
        let path = std::env::temp_dir().join("tidal_audio_resample.wav");
        write_wav(&path, 44_100, 1.0);

        for (out_rate, expected_frames) in [(44_100u32, 44_100f64), (48_000, 48_000.0), (22_050, 22_050.0)] {
            let mut src = AudioSource::open(item(&path, 1.0)).unwrap();
            assert_eq!(src.format(), FormatInfo { bits_per_sample: 16, sample_rate: 44_100 });
            let mut out = Vec::new();
            while src.read_stereo(out_rate, &mut out).unwrap() {}
            let frames = (out.len() / 2) as f64;
            assert!((frames - expected_frames).abs() <= 2.0, "{out_rate} Hz gave {frames} frames");
            // Mono source: both channels identical, and the signal is not silent.
            assert!(out.chunks(2).all(|f| f[0] == f[1]));
            assert!(out.iter().any(|s| s.abs() > 0.1));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seek_lands_near_requested_position() {
        let path = std::env::temp_dir().join("tidal_audio_seek.wav");
        write_wav(&path, 44_100, 2.0);
        let mut src = AudioSource::open(item(&path, 2.0)).unwrap();
        src.seek(Duration::from_millis(1500)).unwrap();
        let mut out = Vec::new();
        while src.read_stereo(44_100, &mut out).unwrap() {}
        let remaining = out.len() as f64 / 2.0 / 44_100.0;
        assert!((remaining - 0.5).abs() < 0.05, "expected ~0.5s after seek, got {remaining}");
        let _ = std::fs::remove_file(&path);
    }
}
