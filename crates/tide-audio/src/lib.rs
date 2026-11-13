//! Symphonia decoder feeding a cpal (CoreAudio) output stream.
//!
//! The decoder thread keeps ~1.5 s of interleaved stereo samples buffered at the
//! device rate. Track boundaries are marked by sample index so that the output
//! callback switches "current track" exactly when the first sample of the next
//! track is played, which is what makes gapless transitions sample-accurate.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use thiserror::Error;

pub mod eq;
pub use eq::{Band, BandKind, EqChain, EqProfile};

/// Open cache file while DASH segments are still being appended.
/// Reports `is_seekable() == false` and `byte_len() == None` so Symphonia's ISO-MP4
/// demuxer streams incrementally across appended `moof`+`mdat` fragments without
/// requiring a fixed end-of-file length up front (`IsoMp4Reader::try_new` returns
/// `SeekErrorKind::Unseekable` when `is_seekable() && byte_len().is_none()`).
struct GrowingFile {
    file: File,
}

impl Read for GrowingFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for GrowingFile {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "growing cache file is stream-only",
        ))
    }
}

impl MediaSource for GrowingFile {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

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
    /// Average bitrate of the source file in kbps, measured from its size on
    /// disk and the track duration. 0 while unknown (file still downloading).
    pub bitrate_kbps: u32,
}

/// How the output device is connected, which decides whether TIDE may
/// re-clock it to the track's sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    BuiltIn,
    Usb,
    Bluetooth,
    Hdmi,
    /// AirPlay and other network outputs.
    Network,
    /// Other wired connections (Thunderbolt, FireWire, PCI, S/PDIF, line).
    Wired,
    /// Aggregate or virtual devices (loopback drivers, multi-output).
    Virtual,
    Unknown,
}

impl OutputKind {
    /// Wireless and virtual outputs run at a rate of their own choosing;
    /// re-clocking them is pointless (the link re-encodes the audio) and can
    /// drop the connection.
    pub fn allows_rate_matching(self) -> bool {
        !matches!(
            self,
            OutputKind::Bluetooth | OutputKind::Network | OutputKind::Virtual
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            OutputKind::BuiltIn => "Built-in",
            OutputKind::Usb => "USB",
            OutputKind::Bluetooth => "Bluetooth",
            OutputKind::Hdmi => "HDMI / DisplayPort",
            OutputKind::Network => "AirPlay / network",
            OutputKind::Wired => "Wired",
            OutputKind::Virtual => "Virtual device",
            OutputKind::Unknown => "Output",
        }
    }
}

/// The device audio is being sent to right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    pub id: String,
    pub name: String,
    pub kind: OutputKind,
    /// Rate the device is running at.
    pub sample_rate: u32,
    /// Standard rates the device can be switched to.
    pub supported_rates: Vec<u32>,
}

impl Default for OutputInfo {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "System Default".into(),
            kind: OutputKind::Unknown,
            sample_rate: 48_000,
            supported_rates: Vec::new(),
        }
    }
}

const STANDARD_RATES: [u32; 8] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];

/// Largest integer downsample we handle with the HCF (box-average) path.
/// 384 kHz → 48 kHz and 352.8 kHz → 44.1 kHz are both ÷8.
const MAX_HCF_DECIM: usize = 8;

const NONE: u64 = u64::MAX;

/// Highest common factor. Reduces a resampling ratio `from:to` to lowest terms.
fn hcf(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Integer downsample factor `M` when `from/to` reduces to `M:1` via HCF.
/// `Some(2)` for 88.2→44.1 and 96→48; `None` for cross-family rates like 96→44.1.
fn downsample_factor(from: u32, to: u32) -> Option<u32> {
    if from <= to || to == 0 {
        return None;
    }
    let g = hcf(from, to);
    let upsample = to / g;
    let downsample = from / g;
    (upsample == 1 && downsample > 1 && downsample as usize <= MAX_HCF_DECIM).then_some(downsample)
}

/// Rate the device should run at for `source`. Native if the device has it;
/// otherwise the highest supported integer-divisor (HCF) rate, so 96 kHz plays
/// at 48 kHz rather than being interpolated down to 44.1 kHz.
fn pick_output_rate(source: u32, supported: &[u32], current: u32) -> u32 {
    if supported.contains(&source) {
        return source;
    }
    supported
        .iter()
        .copied()
        .filter(|&rate| downsample_factor(source, rate).is_some())
        .max()
        .unwrap_or(current)
}

fn push_box_average(frames: &[[f32; 2]], out: &mut Vec<f32>) {
    let n = frames.len() as f32;
    let mut l = 0.0;
    let mut r = 0.0;
    for frame in frames {
        l += frame[0];
        r += frame[1];
    }
    out.push(l / n);
    out.push(r / n);
}

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
    // Linear resampler state (used when the ratio is not an integer HCF downsample).
    resample_pos: f64,
    last_frame: [f32; 2],
    /// Leftover source frames for integer HCF downsample across packets.
    hold: [[f32; 2]; MAX_HCF_DECIM],
    hold_len: u8,
    /// Cache file still growing while segments download in the background.
    growing: bool,
    eof_wait: Option<Instant>,
    /// Source frames already decoded — used to resume after reopening a finalized cache file.
    frames_decoded: u64,
}

impl AudioSource {
    fn open(item: TrackQueueItem) -> Result<Self, AudioError> {
        let growing = item.file_path.extension().and_then(|s| s.to_str()) == Some("tmp");
        let file = File::open(&item.file_path)?;
        let mss = if growing {
            MediaSourceStream::new(Box::new(GrowingFile { file }), Default::default())
        } else {
            MediaSourceStream::new(Box::new(file), Default::default())
        };
        let probed = symphonia::default::get_probe()
            .format(
                &Hint::new(),
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
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
            hold: [[0.0; 2]; MAX_HCF_DECIM],
            hold_len: 0,
            growing,
            eof_wait: None,
            frames_decoded: 0,
        })
    }

    fn format(&self) -> FormatInfo {
        FormatInfo {
            bits_per_sample: self.bits_per_sample,
            sample_rate: self.sample_rate,
            bitrate_kbps: 0,
        }
    }

    /// Progressive head hit EOF after the `.tmp` was renamed to `.audio`. Reopen the
    /// finished file and seek to where we left off so the track does not end early.
    fn try_reopen_finalized(&mut self) -> Result<bool, AudioError> {
        let complete = self.item.file_path.with_extension("audio");
        if !complete.exists() {
            return Ok(false);
        }
        let pos =
            Duration::from_secs_f64(self.frames_decoded as f64 / self.sample_rate.max(1) as f64);
        let mut item = self.item.clone();
        item.file_path = complete;
        let decoded = self.frames_decoded;
        let mut fresh = Self::open(item)?;
        if pos > Duration::from_millis(20) {
            fresh.seek(pos)?;
        }
        fresh.frames_decoded = decoded;
        tracing::info!(?pos, title = %fresh.item.title, "Reopened finalized audio after progressive head");
        *self = fresh;
        Ok(true)
    }

    /// Decode one packet into interleaved stereo at `out_rate`. Returns false at end of stream.
    fn read_stereo(&mut self, out_rate: u32, out: &mut Vec<f32>) -> Result<bool, AudioError> {
        const GROWING_EOF_GRACE: Duration = Duration::from_secs(120);
        let packet = loop {
            match self.reader.next_packet() {
                Ok(packet) => {
                    self.eof_wait = None;
                    break packet;
                }
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    if !self.growing {
                        return Ok(false);
                    }
                    if self.try_reopen_finalized()? {
                        continue;
                    }
                    let start = *self.eof_wait.get_or_insert_with(Instant::now);
                    if start.elapsed() > GROWING_EOF_GRACE {
                        self.eof_wait = None;
                        self.growing = false;
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(SymphoniaError::ResetRequired) => {
                    if self.growing && self.try_reopen_finalized()? {
                        continue;
                    }
                    return Ok(false);
                }
                Err(e) => return Err(AudioError::Decoder(e.to_string())),
            }
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
        if self
            .sample_buf
            .as_ref()
            .is_none_or(|b| (b.capacity() as u64) < needed * channels as u64)
        {
            self.sample_buf = Some(SampleBuffer::new(needed, spec));
        }
        let buf = self
            .sample_buf
            .as_mut()
            .expect("sample buffer allocated above");
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        let frame = |i: usize| -> [f32; 2] {
            let base = i * channels;
            if channels == 1 {
                [samples[base]; 2]
            } else {
                [samples[base], samples[base + 1]]
            }
        };
        let frames = samples.len() / channels;

        if self.sample_rate == out_rate {
            out.reserve(frames * 2);
            for i in 0..frames {
                out.extend_from_slice(&frame(i));
            }
        } else if let Some(factor) = downsample_factor(self.sample_rate, out_rate) {
            // Integer HCF downsample (88.2→44.1, 96→48, 192→48, …): box-average
            // each group of M source frames. Cheap anti-alias vs drop-every-Mth.
            // ponytail: box average, not a half-band FIR. Upgrade if we grow a
            // real resampler for cross-family rates (96→44.1) as well.
            let m = factor as usize;
            let mut i = 0;
            if self.hold_len > 0 {
                while i < frames && (self.hold_len as usize) < m {
                    self.hold[self.hold_len as usize] = frame(i);
                    self.hold_len += 1;
                    i += 1;
                }
                if self.hold_len as usize == m {
                    push_box_average(&self.hold[..m], out);
                    self.hold_len = 0;
                }
            }
            let inv = 1.0 / factor as f32;
            while i + m <= frames {
                let mut l = 0.0;
                let mut r = 0.0;
                for k in 0..m {
                    let f = frame(i + k);
                    l += f[0];
                    r += f[1];
                }
                out.push(l * inv);
                out.push(r * inv);
                i += m;
            }
            while i < frames {
                self.hold[self.hold_len as usize] = frame(i);
                self.hold_len += 1;
                i += 1;
            }
        } else {
            // `resample_pos` is the read position in source frames relative to this
            // packet's first frame; -1 refers to the last frame of the previous packet.
            let step = self.sample_rate as f64 / out_rate as f64;
            let mut pos = self.resample_pos;
            while pos < frames as f64 - 1.0 {
                let idx = pos.floor();
                let frac = (pos - idx) as f32;
                let a = if idx < 0.0 {
                    self.last_frame
                } else {
                    frame(idx as usize)
                };
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
        self.frames_decoded = self.frames_decoded.saturating_add(frames as u64);
        Ok(true)
    }

    fn seek(&mut self, to: Duration) -> Result<(), AudioError> {
        if self.growing {
            let complete = self.item.file_path.with_extension("audio");
            let reopen_path = if complete.exists() {
                complete
            } else {
                self.item.file_path.clone()
            };
            let mut item = self.item.clone();
            item.file_path = reopen_path;
            let mut fresh = Self::open(item)?;
            fresh
                .reader
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time: Time::from(to.as_secs_f64()),
                        track_id: Some(fresh.track_id),
                    },
                )
                .map_err(|e| AudioError::Decoder(e.to_string()))?;
            fresh.decoder.reset();
            fresh.frames_decoded =
                (to.as_secs_f64() * fresh.sample_rate.max(1) as f64).round() as u64;
            *self = fresh;
            return Ok(());
        }
        self.reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from(to.as_secs_f64()),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| AudioError::Decoder(e.to_string()))?;
        self.decoder.reset();
        self.resample_pos = 0.0;
        self.last_frame = [0.0; 2];
        self.hold = [[0.0; 2]; MAX_HCF_DECIM];
        self.hold_len = 0;
        self.frames_decoded = (to.as_secs_f64() * self.sample_rate.max(1) as f64).round() as u64;
        self.eof_wait = None;
        Ok(())
    }
}

/// Average bitrate of a fully downloaded track: file size over duration.
/// A `.tmp` file still growing is measured once its finished `.audio` sibling exists.
fn average_bitrate_kbps(item: &TrackQueueItem) -> Option<u32> {
    let secs = item.duration.as_secs_f64();
    if secs < 1.0 {
        return None;
    }
    let path = if item.file_path.extension().and_then(|s| s.to_str()) == Some("tmp") {
        item.file_path.with_extension("audio")
    } else {
        item.file_path.clone()
    };
    let bytes = std::fs::metadata(path).ok()?.len();
    Some((bytes as f64 * 8.0 / secs / 1000.0).round() as u32)
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
    /// Switch the device to each track's native rate when the output allows it.
    rate_matching: AtomicBool,
    /// Put the device back at the rate it had before TIDE changed it, on pause and quit.
    restore_sample_rate: AtomicBool,
    output: Mutex<OutputInfo>,
    /// (device id, rate) the output had before TIDE first touched it, so it
    /// can be handed back the way it was found.
    original: Mutex<Option<(String, u32)>>,
    /// The device has been handed back (paused / quitting) and must be
    /// re-clocked before playback continues.
    released: AtomicBool,
    /// Equalisers the output callback runs, in order. The callback picks up a
    /// change when `eq_version` moves.
    eq: Mutex<Vec<EqProfile>>,
    eq_version: AtomicU64,
}

pub struct AudioPlayer {
    stream: Mutex<Option<Stream>>,
    shared: Arc<Shared>,
}

/// How the system's default output device is connected, straight from
/// CoreAudio (cpal does not report the transport type on macOS).
#[cfg(target_os = "macos")]
fn default_output_transport() -> Option<OutputKind> {
    use std::ffi::c_void;

    #[repr(C)]
    struct PropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }
    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            data_size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }
    const fn fourcc(code: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*code)
    }
    const SYSTEM_OBJECT: u32 = 1;
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");

    let read_u32 = |object: u32, selector: u32| -> Option<u32> {
        let address = PropertyAddress {
            selector,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let mut value = 0u32;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: both properties are documented to yield a single 32-bit value,
        // and `size` tells CoreAudio how much room `value` has.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut value as *mut u32).cast(),
            )
        };
        (status == 0).then_some(value)
    };
    let device = read_u32(SYSTEM_OBJECT, fourcc(b"dOut"))?;
    let transport = read_u32(device, fourcc(b"tran"))?;
    Some(kind_from_transport(&transport.to_be_bytes()))
}

/// Map a CoreAudio transport type (a four-character code) to an [`OutputKind`].
fn kind_from_transport(code: &[u8; 4]) -> OutputKind {
    match code {
        b"bltn" => OutputKind::BuiltIn,
        b"usb " => OutputKind::Usb,
        b"blue" | b"blea" => OutputKind::Bluetooth,
        b"hdmi" | b"dprt" => OutputKind::Hdmi,
        b"airp" | b"eavb" => OutputKind::Network,
        b"thun" | b"1394" | b"pci " => OutputKind::Wired,
        b"virt" | b"grup" => OutputKind::Virtual,
        _ => OutputKind::Unknown,
    }
}

fn describe_output(device: &cpal::Device, sample_rate: u32, channels: u16) -> OutputInfo {
    use cpal::InterfaceType as I;
    let description = device.description().ok();
    let kind = match description.as_ref().map(|d| d.interface_type()) {
        Some(I::BuiltIn) => OutputKind::BuiltIn,
        Some(I::Usb) => OutputKind::Usb,
        Some(I::Bluetooth) => OutputKind::Bluetooth,
        Some(I::Hdmi | I::DisplayPort) => OutputKind::Hdmi,
        Some(I::Network) => OutputKind::Network,
        Some(I::Thunderbolt | I::FireWire | I::Pci | I::Spdif | I::Line) => OutputKind::Wired,
        Some(I::Virtual | I::Aggregate) => OutputKind::Virtual,
        _ => OutputKind::Unknown,
    };
    #[cfg(target_os = "macos")]
    let kind = default_output_transport().unwrap_or(kind);
    let ranges: Vec<_> = device
        .supported_output_configs()
        .map(|configs| {
            configs
                .filter(|r| r.sample_format() == SampleFormat::F32 && r.channels() == channels)
                .map(|r| (r.min_sample_rate(), r.max_sample_rate()))
                .collect()
        })
        .unwrap_or_default();
    OutputInfo {
        id: device.id().map(|id| id.to_string()).unwrap_or_default(),
        name: description.map_or_else(|| "System Default".to_string(), |d| d.name().to_string()),
        kind,
        sample_rate,
        supported_rates: STANDARD_RATES
            .iter()
            .copied()
            .filter(|rate| ranges.iter().any(|(min, max)| (*min..=*max).contains(rate)))
            .collect(),
    }
}

fn default_output_id() -> Option<String> {
    cpal::default_host()
        .default_output_device()?
        .id()
        .ok()
        .map(|id| id.to_string())
}

/// `force` re-clocks regardless of the rate-matching rules; it is used to put
/// a device back to the rate it had before TIDE changed it.
fn build_stream(
    shared: &Arc<Shared>,
    want_rate: Option<u32>,
    force: bool,
) -> Result<(Stream, OutputInfo), AudioError> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| AudioError::Device("No output device available".to_string()))?;
    let default_config = device
        .default_output_config()
        .map_err(|e| AudioError::Device(e.to_string()))?;
    let described = describe_output(
        &device,
        default_config.sample_rate(),
        default_config.channels(),
    );
    let may_match = force
        || (shared.rate_matching.load(Ordering::Relaxed) && described.kind.allows_rate_matching());
    {
        // First contact with this device: whatever it runs at now is "original".
        let mut original = shared.original.lock().unwrap();
        if original.as_ref().is_none_or(|(id, _)| *id != described.id) {
            *original = Some((described.id.clone(), default_config.sample_rate()));
        }
    }

    // Prefer running the device at the track's native rate (bit-perfect, no resampling).
    let config = want_rate
        .filter(|r| may_match && *r != default_config.sample_rate())
        .and_then(|rate| {
            device.supported_output_configs().ok()?.find_map(|range| {
                (range.sample_format() == SampleFormat::F32
                    && range.channels() == default_config.channels())
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
    let mut eq_seen = shared.eq_version.load(Ordering::Relaxed);
    let mut eq = EqChain::default();
    eq.configure(&shared.eq.lock().unwrap(), rate);

    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                data.fill(0.0);
                let eq_version = cb_shared.eq_version.load(Ordering::Relaxed);
                if eq_version != eq_seen {
                    // Never wait in the audio callback: a busy lock is retried next time.
                    if let Ok(profiles) = cb_shared.eq.try_lock() {
                        eq.configure(&profiles, rate);
                        eq_seen = eq_version;
                    }
                }
                if !cb_shared.playing.load(Ordering::Relaxed) {
                    return;
                }
                let equalise = eq.is_active();
                let vol = cb_shared.volume.load(Ordering::Relaxed) as f32 / 1000.0;
                let gain = vol * vol;
                let mut buf = cb_shared.buffer.lock().unwrap();
                let mut frames = 0u64;
                for out in data.chunks_mut(channels) {
                    if buf.samples.len() < 2 {
                        break;
                    }
                    let mut frame = [
                        buf.samples.pop_front().unwrap_or(0.0),
                        buf.samples.pop_front().unwrap_or(0.0),
                    ];
                    if equalise {
                        frame = eq.process(frame);
                    }
                    let [l, r] = [frame[0] * gain, frame[1] * gain];
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
    stream
        .play()
        .map_err(|e| AudioError::Stream(e.to_string()))?;
    Ok((
        stream,
        OutputInfo {
            sample_rate: rate,
            ..described
        },
    ))
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
                    buf.upcoming = Some(Playing {
                        item: opened.item.clone(),
                        format: opened.format(),
                    });
                    *source = Some(opened);
                }
                Err(e) => tracing::error!("Failed to open next audio source: {e}"),
            }
            continue;
        }

        chunk.clear();
        let more = match source
            .as_mut()
            .expect("checked above")
            .read_stereo(device_rate, &mut chunk)
        {
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
            rate_matching: AtomicBool::new(true),
            restore_sample_rate: AtomicBool::new(true),
            output: Mutex::new(OutputInfo::default()),
            original: Mutex::new(None),
            released: AtomicBool::new(false),
            eq: Mutex::new(Vec::new()),
            eq_version: AtomicU64::new(0),
        });

        let (stream, output) = build_stream(&shared, None, false)?;
        shared
            .device_rate
            .store(output.sample_rate, Ordering::Relaxed);
        *shared.output.lock().unwrap() = output;

        let decoder_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("tide-decoder".into())
            .spawn(move || decoder_loop(decoder_shared))
            .map_err(AudioError::Io)?;

        Ok(Self {
            stream: Mutex::new(Some(stream)),
            shared,
        })
    }

    /// Open the current default output again, at `want_rate` when the device
    /// allows it. Returns true if the device rate changed.
    fn rebuild_stream(&self, want_rate: Option<u32>, force: bool) -> bool {
        let before = self.shared.device_rate.load(Ordering::Relaxed);
        let mut stream = self.stream.lock().unwrap();
        // The old stream must be gone before CoreAudio lets us reconfigure the device.
        *stream = None;
        let rebuilt = build_stream(&self.shared, want_rate, force).or_else(|e| {
            tracing::warn!(
                "Could not open the output at {want_rate:?} Hz ({e}); using its default rate"
            );
            build_stream(&self.shared, None, false)
        });
        match rebuilt {
            Ok((new_stream, output)) => {
                tracing::info!(device = %output.name, kind = ?output.kind, requested = ?want_rate, actual = output.sample_rate, "Output stream opened");
                self.shared
                    .device_rate
                    .store(output.sample_rate, Ordering::Relaxed);
                let changed = output.sample_rate != before;
                *self.shared.output.lock().unwrap() = output;
                *stream = Some(new_stream);
                changed
            }
            Err(e) => {
                tracing::error!("Failed to rebuild output stream: {e}");
                false
            }
        }
    }

    /// True when the output can and may be switched to `rate`.
    fn can_match(&self, rate: u32) -> bool {
        let output = self.shared.output.lock().unwrap();
        self.shared.rate_matching.load(Ordering::Relaxed)
            && output.kind.allows_rate_matching()
            && output.supported_rates.contains(&rate)
    }

    /// Rate to run the device at for this source: native, else the highest
    /// integer HCF downsample the device supports, else whatever it is at now.
    fn preferred_output_rate(&self, source: u32) -> u32 {
        let output = self.shared.output.lock().unwrap();
        if !self.shared.rate_matching.load(Ordering::Relaxed) || !output.kind.allows_rate_matching()
        {
            return output.sample_rate;
        }
        pick_output_rate(source, &output.supported_rates, output.sample_rate)
    }

    /// Run the output at the preferred rate for this source.
    fn match_device_rate(&self, source_rate: u32) {
        self.shared.released.store(false, Ordering::Relaxed);
        let rate = self.preferred_output_rate(source_rate);
        if self.shared.device_rate.load(Ordering::Relaxed) != rate && self.can_match(rate) {
            self.rebuild_stream(Some(rate), false);
        }
    }

    /// The device audio is going to, and the rate it runs at.
    pub fn output_info(&self) -> OutputInfo {
        self.shared.output.lock().unwrap().clone()
    }

    /// Enable or disable switching the device to each track's sample rate.
    /// Takes effect for the current track straight away.
    pub fn set_rate_matching(&self, enabled: bool) {
        if self.shared.rate_matching.swap(enabled, Ordering::Relaxed) != enabled && enabled {
            self.rematch_current();
        }
    }

    pub fn rate_matching(&self) -> bool {
        self.shared.rate_matching.load(Ordering::Relaxed)
    }

    /// When off, pause and quit leave the device at the track's sample rate.
    /// While paused, turning it on hands the device back, and turning it off
    /// takes the track's rate again.
    pub fn set_restore_sample_rate(&self, enabled: bool) {
        self.shared
            .restore_sample_rate
            .store(enabled, Ordering::Relaxed);
        if self.is_playing() {
            return;
        }
        if enabled {
            let _ = self.release_output();
        } else if self.is_released() {
            self.rematch_current();
        }
    }

    pub fn restore_sample_rate(&self) -> bool {
        self.shared.restore_sample_rate.load(Ordering::Relaxed)
    }

    /// Re-open the output for the current track's rate and resume at the same position.
    fn rematch_current(&self) {
        let position = self.elapsed();
        let want = self
            .format_info()
            .map(|f| self.preferred_output_rate(f.sample_rate));
        self.shared.released.store(false, Ordering::Relaxed);
        if self.rebuild_stream(want, false) {
            // Buffered samples were produced for the old rate.
            self.seek(position);
        }
    }

    /// Rate the current device had before TIDE changed it, if it differs from now.
    fn rate_to_restore(&self) -> Option<u32> {
        let output = self.shared.output.lock().unwrap();
        let original = self.shared.original.lock().unwrap();
        original
            .as_ref()
            .filter(|(id, rate)| *id == output.id && *rate != output.sample_rate)
            .map(|(_, rate)| *rate)
    }

    /// Hand the device back at the sample rate it had before TIDE changed it,
    /// so other apps are not left with a re-clocked output. Called when
    /// playback has been paused for a moment and when quitting. The position
    /// is kept; [`resume`](Self::resume) re-clocks the device again.
    /// Returns true if the device rate was restored.
    pub fn release_output(&self) -> bool {
        if self.is_playing() || !self.restore_sample_rate() {
            return false;
        }
        let Some(original) = self.rate_to_restore() else {
            return false;
        };
        let position = self.elapsed();
        let restored = self.rebuild_stream(Some(original), true);
        if restored {
            self.shared.released.store(true, Ordering::Relaxed);
            self.seek(position);
        }
        restored
    }

    /// True while the device is running at its original rate because playback is paused.
    pub fn is_released(&self) -> bool {
        self.shared.released.load(Ordering::Relaxed)
    }

    /// Follow the system's default output (headphones plugged in, Bluetooth
    /// connected, ...). Returns true when playback moved to another device.
    pub fn sync_output(&self) -> bool {
        let Some(default_id) = default_output_id() else {
            return false;
        };
        if default_id == self.shared.output.lock().unwrap().id {
            return false;
        }
        self.rematch_current();
        true
    }

    fn reset_buffer(&self, buf: &mut Buffer, start_frames: u64) {
        buf.samples.clear();
        buf.pushed = 0;
        buf.consumed = 0;
        buf.switch_at = NONE;
        buf.end_at = NONE;
        buf.upcoming = None;
        self.shared
            .frames_played
            .store(start_frames, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
    }

    /// Stop whatever is playing and start `item` from the beginning.
    pub fn play_track(&self, item: TrackQueueItem) -> Result<(), AudioError> {
        self.play_track_at(item, Duration::ZERO)
    }

    /// Stop whatever is playing and start `item` at `start_at`.
    pub fn play_track_at(
        &self,
        item: TrackQueueItem,
        start_at: Duration,
    ) -> Result<(), AudioError> {
        let mut opened = AudioSource::open(item)?;
        let actual_start = if start_at.is_zero() {
            Duration::ZERO
        } else {
            match opened.seek(start_at) {
                Ok(()) => start_at,
                Err(e) => {
                    tracing::warn!(
                        ?start_at,
                        "Initial seek failed; starting from beginning: {e}"
                    );
                    opened = AudioSource::open(opened.item)?;
                    Duration::ZERO
                }
            }
        };
        self.shared.playing.store(false, Ordering::Relaxed);
        self.match_device_rate(opened.sample_rate);
        let rate = self.shared.device_rate.load(Ordering::Relaxed) as f64;
        let frames_played = (actual_start.as_secs_f64() * rate) as u64;

        let mut source = self.shared.source.lock().unwrap();
        self.shared.queue.lock().unwrap().clear();
        let mut buf = self.shared.buffer.lock().unwrap();
        self.reset_buffer(&mut buf, frames_played);
        buf.current = Some(Playing {
            item: opened.item.clone(),
            format: opened.format(),
        });
        *source = Some(opened);
        drop(buf);
        drop(source);
        self.shared.playing.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Queue a track to start seamlessly when the current one ends.
    ///
    /// Returns false (and queues nothing) when the track has a different
    /// sample rate that the output can be switched to: the caller should
    /// start it normally once the current track ends, so the device is
    /// re-clocked and playback stays bit-perfect at the cost of a short gap.
    pub fn enqueue_next(&self, item: TrackQueueItem) -> bool {
        if let Ok(probe) = AudioSource::open(item.clone()) {
            let rate = self.preferred_output_rate(probe.sample_rate);
            if rate != self.shared.device_rate.load(Ordering::Relaxed) {
                tracing::info!(title = %item.title, rate, "Next track needs a different device rate; not queued gaplessly");
                return false;
            }
        }
        let mut queue = self.shared.queue.lock().unwrap();
        queue.clear();
        queue.push_back(item);
        // If the decoder already hit the end of the current track, cancel the pending stop.
        let mut buf = self.shared.buffer.lock().unwrap();
        if buf.end_at != NONE && buf.consumed < buf.end_at {
            buf.end_at = NONE;
        }
        true
    }

    pub fn clear_queue(&self) {
        self.shared.queue.lock().unwrap().clear();
    }

    /// Discard an upcoming track without dropping any remaining samples of
    /// the current track. The decoder holds the source lock while staging audio.
    pub fn cancel_upcoming(&self) {
        let mut source = self.shared.source.lock().unwrap();
        self.clear_queue();
        let mut buf = self.shared.buffer.lock().unwrap();
        if buf.upcoming.take().is_some() {
            let boundary = buf.switch_at;
            let remaining = boundary.saturating_sub(buf.consumed) as usize;
            buf.samples.truncate(remaining);
            buf.pushed = boundary;
            buf.switch_at = NONE;
            *source = None;
        }
        if source.is_none() && buf.current.is_some() {
            buf.end_at = buf.pushed;
        }
    }

    /// Jump within the current track. Returns false if nothing is loaded.
    pub fn seek(&self, to: Duration) -> bool {
        let mut source = self.shared.source.lock().unwrap();
        let mut buf = self.shared.buffer.lock().unwrap();
        let Some(current_item) = buf.current.as_ref().map(|c| c.item.clone()) else {
            return false;
        };

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
        if self.shared.buffer.lock().unwrap().current.is_some()
            && !self.shared.finished.load(Ordering::Relaxed)
        {
            if self.shared.released.load(Ordering::Relaxed) {
                // The device was handed back while paused: take it again at the track's rate.
                self.rematch_current();
            }
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
        self.shared
            .buffer
            .lock()
            .unwrap()
            .current
            .as_ref()
            .map(|c| c.item.id.clone())
    }

    /// Changes whenever playback moved on to the next queued track by itself.
    pub fn track_serial(&self) -> u64 {
        self.shared.serial.load(Ordering::Relaxed)
    }

    pub fn format_info(&self) -> Option<FormatInfo> {
        let (mut format, item) = {
            let buf = self.shared.buffer.lock().unwrap();
            let current = buf.current.as_ref()?;
            (current.format, current.item.clone())
        };
        if format.bitrate_kbps == 0 {
            format.bitrate_kbps = average_bitrate_kbps(&item).unwrap_or(0);
        }
        Some(format)
    }

    /// Bitrate to report for the current source when its file cannot tell
    /// (a stream being written as it plays).
    pub fn set_source_bitrate(&self, kbps: u32) {
        if let Some(current) = self.shared.buffer.lock().unwrap().current.as_mut() {
            current.format.bitrate_kbps = kbps;
        }
    }

    /// Stop and forget whatever is loaded: nothing is current afterwards.
    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        let mut source = self.shared.source.lock().unwrap();
        self.shared.queue.lock().unwrap().clear();
        let mut buf = self.shared.buffer.lock().unwrap();
        self.reset_buffer(&mut buf, 0);
        buf.current = None;
        *source = None;
    }

    /// Move what is heard by `frames` device frames without a seek, to keep
    /// in step with a picture: positive skips ahead (drops buffered audio),
    /// negative holds back (plays silence first). Never crosses into the next
    /// track. Returns how many frames it actually moved.
    pub fn nudge(&self, frames: i64) -> i64 {
        let mut buf = self.shared.buffer.lock().unwrap();
        if frames >= 0 {
            let boundary = [buf.switch_at, buf.end_at]
                .into_iter()
                .filter(|&at| at != NONE)
                .map(|at| at.saturating_sub(buf.consumed) / 2)
                .min()
                .unwrap_or(u64::MAX);
            let n = (frames as u64)
                .min(buf.samples.len() as u64 / 2)
                .min(boundary);
            buf.samples.drain(..(n * 2) as usize);
            buf.consumed += n * 2;
            self.shared.frames_played.fetch_add(n, Ordering::Relaxed);
            n as i64
        } else {
            let n = frames.unsigned_abs();
            for _ in 0..n * 2 {
                buf.samples.push_front(0.0);
            }
            let buf = &mut *buf;
            buf.pushed += n * 2;
            for at in [&mut buf.switch_at, &mut buf.end_at] {
                if *at != NONE {
                    *at += n * 2;
                }
            }
            // The silence is counted as it plays: the position stands still.
            let _ = self.shared.frames_played.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |played| Some(played.saturating_sub(n)),
            );
            -(n as i64)
        }
    }

    /// Replace the equalisers the output runs. Heard within one audio buffer.
    pub fn set_eq(&self, profiles: Vec<EqProfile>) {
        *self.shared.eq.lock().unwrap() = profiles;
        self.shared.eq_version.fetch_add(1, Ordering::Relaxed);
    }

    /// True when an equaliser is changing the samples on their way out.
    pub fn eq_active(&self) -> bool {
        self.shared
            .eq
            .lock()
            .unwrap()
            .iter()
            .any(EqProfile::alters_audio)
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared
            .volume
            .store((volume.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
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

        for (out_rate, expected_frames) in [
            (44_100u32, 44_100f64),
            (48_000, 48_000.0),
            (22_050, 22_050.0),
        ] {
            let mut src = AudioSource::open(item(&path, 1.0)).unwrap();
            assert_eq!(
                src.format(),
                FormatInfo {
                    bits_per_sample: 16,
                    sample_rate: 44_100,
                    bitrate_kbps: 0
                }
            );
            let mut out = Vec::new();
            while src.read_stereo(out_rate, &mut out).unwrap() {}
            let frames = (out.len() / 2) as f64;
            assert!(
                (frames - expected_frames).abs() <= 2.0,
                "{out_rate} Hz gave {frames} frames"
            );
            // Mono source: both channels identical, and the signal is not silent.
            assert!(out.chunks(2).all(|f| f[0] == f[1]));
            assert!(out.iter().any(|s| s.abs() > 0.1));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hcf_reduces_same_family_rates_to_integer_downsample() {
        assert_eq!(hcf(96_000, 48_000), 48_000);
        assert_eq!(hcf(88_200, 44_100), 44_100);
        assert_eq!(hcf(192_000, 48_000), 48_000);
        assert_eq!(hcf(176_400, 44_100), 44_100);
        assert_eq!(hcf(96_000, 44_100), 300);
        assert_eq!(downsample_factor(96_000, 48_000), Some(2));
        assert_eq!(downsample_factor(88_200, 44_100), Some(2));
        assert_eq!(downsample_factor(192_000, 48_000), Some(4));
        assert_eq!(downsample_factor(176_400, 44_100), Some(4));
        assert_eq!(downsample_factor(96_000, 44_100), None);
        assert_eq!(downsample_factor(88_200, 48_000), None);
        assert_eq!(downsample_factor(44_100, 44_100), None);
    }

    #[test]
    fn pick_output_rate_prefers_native_then_hcf_family() {
        let dac = [44_100, 48_000];
        assert_eq!(pick_output_rate(96_000, &dac, 44_100), 48_000);
        assert_eq!(pick_output_rate(88_200, &dac, 48_000), 44_100);
        assert_eq!(pick_output_rate(48_000, &dac, 44_100), 48_000);
        assert_eq!(pick_output_rate(96_000, &[44_100], 44_100), 44_100);
        let hi = [44_100, 48_000, 88_200, 96_000];
        assert_eq!(pick_output_rate(96_000, &hi, 44_100), 96_000);
        assert_eq!(pick_output_rate(192_000, &hi, 44_100), 96_000);
        assert_eq!(pick_output_rate(176_400, &hi, 48_000), 88_200);
    }

    fn write_alternating_wav(path: &std::path::Path, rate: u32, frames: u32) {
        let mut bytes = Vec::with_capacity(44 + frames as usize * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(frames * 2).to_le_bytes());
        for i in 0..frames {
            let s: i16 = if i % 2 == 0 { 8000 } else { -8000 };
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn hcf_downsample_96k_and_88k2_averages_pairs() {
        for (src_rate, out_rate, name) in [
            (96_000, 48_000, "tidal_audio_hcf_96"),
            (88_200, 44_100, "tidal_audio_hcf_882"),
        ] {
            let path = std::env::temp_dir().join(format!("{name}.wav"));
            write_wav(&path, src_rate, 0.25);
            let mut src = AudioSource::open(item(&path, 0.25)).unwrap();
            let mut out = Vec::new();
            while src.read_stereo(out_rate, &mut out).unwrap() {}
            let expected = src_rate as f64 / (src_rate / out_rate) as f64 * 0.25;
            let frames = (out.len() / 2) as f64;
            assert!(
                (frames - expected).abs() <= 2.0,
                "{src_rate}→{out_rate} gave {frames} frames, expected {expected}"
            );
            assert!(out.iter().any(|s| s.abs() > 0.1));
            let _ = std::fs::remove_file(&path);
        }

        // Alternating +/− at 96 kHz is Nyquist. Averaging pairs → silence;
        // drop-every-other (linear step=2) would keep a full-scale tone.
        let path = std::env::temp_dir().join("tidal_audio_hcf_nyquist.wav");
        write_alternating_wav(&path, 96_000, 96_000);
        let mut src = AudioSource::open(item(&path, 1.0)).unwrap();
        let mut out = Vec::new();
        while src.read_stereo(48_000, &mut out).unwrap() {}
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            peak < 0.02,
            "96→48 HCF downsample should cancel Nyquist, peak={peak}"
        );
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
        assert!(
            (remaining - 0.5).abs() < 0.05,
            "expected ~0.5s after seek, got {remaining}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Hi-Res streams arrive as DASH: an init segment plus fragmented-MP4 FLAC
    /// segments, concatenated on disk. Point TIDE_DASH_SAMPLE at such a file.
    #[test]
    fn decodes_fragmented_mp4_flac_when_sample_provided() {
        let Some(path) = std::env::var_os("TIDE_DASH_SAMPLE").map(PathBuf::from) else {
            return;
        };
        let mut src = AudioSource::open(item(&path, 12.0)).unwrap();
        let format = src.format();
        let mut out = Vec::new();
        while src.read_stereo(format.sample_rate, &mut out).unwrap() {}
        let secs = out.len() as f64 / 2.0 / format.sample_rate as f64;
        println!("fmp4: {format:?}, decoded {secs:.2}s");
        assert!(format.sample_rate >= 44_100 && secs > 1.0);
        assert!(out.iter().any(|s| s.abs() > 0.01));

        src.seek(Duration::from_secs(6)).unwrap();
        let mut rest = Vec::new();
        while src.read_stereo(format.sample_rate, &mut rest).unwrap() {}
        let remaining = rest.len() as f64 / 2.0 / format.sample_rate as f64;
        println!("fmp4: {remaining:.2}s left after seeking to 6s");
        assert!((remaining - (secs - 6.0)).abs() < 0.5);
    }

    #[test]
    fn tmp_extension_marks_source_as_growing() {
        let path = std::env::temp_dir().join("tidal_audio_growing.tmp");
        write_wav(&path, 44_100, 0.25);
        let src = AudioSource::open(item(&path, 0.25)).unwrap();
        assert!(src.growing);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn growing_file_is_stream_only_and_audio_source_seeks_by_reopening() {
        let path = std::env::temp_dir().join("tidal_audio_grow_src.tmp");
        write_wav(&path, 44_100, 1.0);
        let mut src = GrowingFile {
            file: File::open(&path).unwrap(),
        };
        assert!(!MediaSource::is_seekable(&src));
        assert!(MediaSource::byte_len(&src).is_none());
        assert!(src.seek(SeekFrom::Start(0)).is_err());

        // AudioSource opened on .tmp can seek forward and backward within written bytes by reopening.
        let mut audio = AudioSource::open(item(&path, 1.0)).unwrap();
        audio.seek(Duration::from_millis(500)).unwrap();
        let mut out = Vec::new();
        for _ in 0..4 {
            assert!(audio.read_stereo(44_100, &mut out).unwrap());
        }
        assert!(!out.is_empty());
        audio.seek(Duration::from_millis(100)).unwrap();
        out.clear();
        for _ in 0..4 {
            assert!(audio.read_stereo(44_100, &mut out).unwrap());
        }
        assert!(!out.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seek_on_tmp_upgrades_to_finalized_audio() {
        let dir = std::env::temp_dir().join(format!("tide_seek_upgrade_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let tmp = dir.join("track.tmp");
        write_wav(&tmp, 44_100, 2.0);
        let mut src = AudioSource::open(item(&tmp, 2.0)).unwrap();
        assert!(src.growing);

        let finalized = tmp.with_extension("audio");
        std::fs::rename(&tmp, &finalized).unwrap();
        src.seek(Duration::from_millis(1500)).unwrap();
        assert!(!src.growing);
        let mut out = Vec::new();
        while src.read_stereo(44_100, &mut out).unwrap() {}
        let remaining = out.len() as f64 / 2.0 / 44_100.0;
        assert!(
            (remaining - 0.5).abs() < 0.05,
            "expected ~0.5s after seek, got {remaining}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reopen_finalized_resumes_without_doubling_length() {
        let dir = std::env::temp_dir().join(format!("tide_grow_eof_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let tmp = dir.join("track.tmp");
        write_wav(&tmp, 44_100, 1.0);
        std::fs::copy(&tmp, tmp.with_extension("audio")).unwrap();

        let mut src = AudioSource::open(item(&tmp, 1.0)).unwrap();
        assert!(src.growing);
        let mut out = Vec::new();
        for _ in 0..8 {
            assert!(src.read_stereo(44_100, &mut out).unwrap());
        }
        let mid = out.len();
        assert!(mid > 0);
        assert!(src.try_reopen_finalized().unwrap());
        assert!(!src.growing);
        while src.read_stereo(44_100, &mut out).unwrap() {}
        let frames = out.len() / 2;
        assert!(
            (frames as i32 - 44_100).abs() <= 800,
            "expected ~1s after reopen-at-position, got {frames} frames (mid samples {mid})"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn progressive_fmp4_tmp_opens_streams_appended_fragments_and_seeks() {
        let dir = std::env::temp_dir().join(format!("tide_mp4_tmp_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let tmp = dir.join("track.tmp");
        let (head, tail_frag) = minimal_fmp4_head_and_tail();
        std::fs::write(&tmp, &head).unwrap();

        // 1. Opening progressive ISO-MP4 .tmp must succeed (not fail with SeekErrorKind::Unseekable).
        let mut src = AudioSource::open(item(&tmp, 0.3)).expect("progressive fMP4 .tmp must open");
        assert!(src.growing);

        // 2. Decode first 8 frames (two 4-sample fragments initially on disk).
        let mut out = Vec::new();
        for _ in 0..8 {
            assert!(src.read_stereo(48_000, &mut out).unwrap());
        }
        assert_eq!(out.len() / 2, 8);

        // 3. Append a 3rd fragment while open -> reader continues seamlessly past initial file length.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            f.write_all(&tail_frag).unwrap();
        }
        for _ in 0..4 {
            assert!(src.read_stereo(48_000, &mut out).unwrap());
        }
        assert_eq!(out.len() / 2, 12);

        // 4. Seeking backward within already-written bytes on active .tmp must succeed.
        src.seek(Duration::ZERO)
            .expect("seek backward within downloaded bytes");
        let mut after_seek = Vec::new();
        for _ in 0..12 {
            assert!(src.read_stereo(48_000, &mut after_seek).unwrap());
        }
        assert_eq!(after_seek, out);

        // 5. Seeking past downloaded bytes fails cleanly without corrupting current playback.
        assert!(src.seek(Duration::from_secs(10)).is_err());
        let mut after_failed_seek = Vec::new();
        src.seek(Duration::ZERO).unwrap();
        for _ in 0..4 {
            assert!(src.read_stereo(48_000, &mut after_failed_seek).unwrap());
        }
        assert_eq!(after_failed_seek.len() / 2, 4);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Minimal valid fragmented ISO-MP4 (`ftyp` + `moov`/`mvex` + `moof`/`mdat` fragments)
    /// carrying 16-bit mono 48 kHz PCM (`sowt`) so Symphonia's `IsoMp4Reader` runs its exact
    /// `try_new`, `next_packet`, and `seek` code paths in deterministic unit tests.
    fn minimal_fmp4_head_and_tail() -> (Vec<u8>, Vec<u8>) {
        fn atom(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let len = (8 + payload.len()) as u32;
            let mut v = Vec::with_capacity(len as usize);
            v.extend_from_slice(&len.to_be_bytes());
            v.extend_from_slice(fourcc);
            v.extend_from_slice(payload);
            v
        }
        fn full_atom(fourcc: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
            let mut p = Vec::with_capacity(4 + body.len());
            p.push(version);
            p.extend_from_slice(&flags.to_be_bytes()[1..4]);
            p.extend_from_slice(body);
            atom(fourcc, &p)
        }

        let ftyp = atom(b"ftyp", b"isom\x00\x00\x02\x00isomiso6mp41");

        let mut mvhd_b = vec![0u8; 96];
        mvhd_b[8..12].copy_from_slice(&48_000u32.to_be_bytes());
        mvhd_b[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        mvhd_b[20..22].copy_from_slice(&0x0100u16.to_be_bytes());
        mvhd_b[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        mvhd_b[48..52].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        mvhd_b[64..68].copy_from_slice(&0x4000_0000u32.to_be_bytes());
        mvhd_b[92..96].copy_from_slice(&2u32.to_be_bytes());
        let mvhd = full_atom(b"mvhd", 0, 0, &mvhd_b);

        let mut tkhd_b = vec![0u8; 80];
        tkhd_b[8..12].copy_from_slice(&1u32.to_be_bytes());
        tkhd_b[28..30].copy_from_slice(&0x0100u16.to_be_bytes());
        tkhd_b[36..40].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        tkhd_b[52..56].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        tkhd_b[68..72].copy_from_slice(&0x4000_0000u32.to_be_bytes());
        let tkhd = full_atom(b"tkhd", 0, 3, &tkhd_b);

        let mut mdhd_b = vec![0u8; 20];
        mdhd_b[8..12].copy_from_slice(&48_000u32.to_be_bytes());
        let mdhd = full_atom(b"mdhd", 0, 0, &mdhd_b);

        let mut hdlr_b = vec![0u8; 21];
        hdlr_b[4..8].copy_from_slice(b"soun");
        let hdlr = full_atom(b"hdlr", 0, 0, &hdlr_b);
        let smhd = full_atom(b"smhd", 0, 0, &[0u8; 4]);

        let url = full_atom(b"url ", 0, 1, &[]);
        let mut dref_b = Vec::new();
        dref_b.extend_from_slice(&1u32.to_be_bytes());
        dref_b.extend_from_slice(&url);
        let dref = full_atom(b"dref", 0, 0, &dref_b);
        let dinf = atom(b"dinf", &dref);

        let mut sowt_b = vec![0u8; 28];
        sowt_b[6..8].copy_from_slice(&1u16.to_be_bytes());
        sowt_b[16..18].copy_from_slice(&1u16.to_be_bytes());
        sowt_b[18..20].copy_from_slice(&16u16.to_be_bytes());
        sowt_b[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());
        let sowt = atom(b"sowt", &sowt_b);
        let mut stsd_b = Vec::new();
        stsd_b.extend_from_slice(&1u32.to_be_bytes());
        stsd_b.extend_from_slice(&sowt);
        let stsd = full_atom(b"stsd", 0, 0, &stsd_b);

        let stts = full_atom(b"stts", 0, 0, &0u32.to_be_bytes());
        let stsc = full_atom(b"stsc", 0, 0, &0u32.to_be_bytes());
        let stsz = full_atom(b"stsz", 0, 0, &[0u8; 8]);
        let stco = full_atom(b"stco", 0, 0, &0u32.to_be_bytes());
        let stbl = atom(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
        let minf = atom(b"minf", &[smhd, dinf, stbl].concat());
        let mdia = atom(b"mdia", &[mdhd, hdlr, minf].concat());
        let trak = atom(b"trak", &[tkhd, mdia].concat());

        // mvex -> trex: each PCM sample is 1 frame (duration = 1, size = 2 bytes)
        let mut trex_b = Vec::with_capacity(20);
        trex_b.extend_from_slice(&1u32.to_be_bytes());
        trex_b.extend_from_slice(&1u32.to_be_bytes());
        trex_b.extend_from_slice(&1u32.to_be_bytes());
        trex_b.extend_from_slice(&2u32.to_be_bytes());
        trex_b.extend_from_slice(&0u32.to_be_bytes());
        let trex = full_atom(b"trex", 0, 0, &trex_b);
        let mvex = atom(b"mvex", &trex);
        let moov = atom(b"moov", &[mvhd, trak, mvex].concat());

        let make_fragment = |seq: u32, decode_time: u64| -> Vec<u8> {
            let samples = 4usize; // 4 PCM samples per fragment
            let mut pcm = Vec::with_capacity(samples * 2);
            for i in 0..samples {
                let s = (i as i16 + 1) * 4000;
                pcm.extend_from_slice(&s.to_le_bytes());
            }
            let mfhd = full_atom(b"mfhd", 0, 0, &seq.to_be_bytes());
            let tfhd = full_atom(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());
            let tfdt = full_atom(b"tfdt", 1, 0, &decode_time.to_be_bytes());
            let mut trun_b = Vec::with_capacity(8);
            trun_b.extend_from_slice(&(samples as u32).to_be_bytes());
            trun_b.extend_from_slice(&96i32.to_be_bytes());
            let trun = full_atom(b"trun", 0, 0x000001, &trun_b);
            let traf = atom(b"traf", &[tfhd, tfdt, trun].concat());
            let moof = atom(b"moof", &[mfhd, traf].concat());
            assert_eq!(moof.len(), 88);
            let mdat = atom(b"mdat", &pcm);
            [moof, mdat].concat()
        };

        let head = [ftyp, moov, make_fragment(1, 0), make_fragment(2, 4)].concat();
        let tail = make_fragment(3, 8);
        (head, tail)
    }

    #[test]
    fn opens_real_progressive_dash_tmp() {
        let real = PathBuf::from(std::env::var("HOME").unwrap())
            .join("Library/Caches/tidal-rust/audio/180829094_MAX.tmp");
        if !real.exists() {
            return;
        }
        let mut src =
            AudioSource::open(item(&real, 12.0)).expect("real progressive DASH .tmp must open");
        assert!(src.growing);
        let mut out = Vec::new();
        assert!(src.read_stereo(src.format().sample_rate, &mut out).unwrap());
        assert!(src.read_stereo(src.format().sample_rate, &mut out).unwrap());
        assert!(!out.is_empty());
        // Seek backward to 0s within downloaded bytes of the real FLAC-in-fMP4 .tmp!
        src.seek(Duration::ZERO)
            .expect("seeking within downloaded bytes of real DASH .tmp must succeed");
        out.clear();
        assert!(src.read_stereo(src.format().sample_rate, &mut out).unwrap());
        assert!(!out.is_empty());
    }

    #[test]
    fn coreaudio_transport_codes_map_to_output_kinds() {
        assert_eq!(kind_from_transport(b"blue"), OutputKind::Bluetooth);
        assert_eq!(kind_from_transport(b"blea"), OutputKind::Bluetooth);
        assert_eq!(kind_from_transport(b"airp"), OutputKind::Network);
        assert_eq!(kind_from_transport(b"bltn"), OutputKind::BuiltIn);
        assert_eq!(kind_from_transport(b"usb "), OutputKind::Usb);
        assert_eq!(kind_from_transport(b"virt"), OutputKind::Virtual);
        assert_eq!(kind_from_transport(b"????"), OutputKind::Unknown);
    }

    #[test]
    fn wireless_and_virtual_outputs_are_never_reclocked() {
        for kind in [
            OutputKind::Bluetooth,
            OutputKind::Network,
            OutputKind::Virtual,
        ] {
            assert!(!kind.allows_rate_matching(), "{kind:?}");
        }
        for kind in [
            OutputKind::BuiltIn,
            OutputKind::Usb,
            OutputKind::Hdmi,
            OutputKind::Wired,
            OutputKind::Unknown,
        ] {
            assert!(kind.allows_rate_matching(), "{kind:?}");
        }
    }
}
