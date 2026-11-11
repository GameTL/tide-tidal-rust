use std::collections::VecDeque;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
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

pub struct TrackQueueItem {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub file_path: PathBuf,
    pub duration: Duration,
}

#[allow(dead_code)]
struct AudioSource {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
    current_item: TrackQueueItem,
}

impl AudioSource {
    fn open(item: TrackQueueItem) -> Result<Self, AudioError> {
        let file = File::open(&item.file_path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let hint = Hint::new();
        let format_opts = FormatOptions::default();
        let metadata_opts = MetadataOptions::default();
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &metadata_opts)
            .map_err(|e| AudioError::Decoder(e.to_string()))?;

        let reader = probed.format;
        let track = reader
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or(AudioError::NoTrack)?;

        let track_id = track.id;
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

        let decoder_opts = DecoderOptions::default();
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &decoder_opts)
            .map_err(|e| AudioError::Decoder(e.to_string()))?;

        Ok(Self {
            reader,
            decoder,
            track_id,
            sample_rate,
            channels,
            current_item: item,
        })
    }

    fn read_samples(&mut self, buffer: &mut Vec<f32>) -> Result<bool, AudioError> {
        let packet = match self.reader.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(false);
            }
            Err(e) => return Err(AudioError::Decoder(e.to_string())),
        };

        if packet.track_id() != self.track_id {
            return Ok(true);
        }

        match self.decoder.decode(&packet) {
            Ok(decoded) => {
                match decoded {
                    AudioBufferRef::F32(buf) => {
                        let planes = buf.planes();
                        let channels = buf.spec().channels.count();
                        let num_frames = buf.frames();
                        for frame in 0..num_frames {
                            for ch in 0..channels {
                                buffer.push(planes.planes()[ch][frame]);
                            }
                            if channels == 1 {
                                buffer.push(planes.planes()[0][frame]);
                            }
                        }
                    }
                    AudioBufferRef::S16(buf) => {
                        let planes = buf.planes();
                        let channels = buf.spec().channels.count();
                        let num_frames = buf.frames();
                        for frame in 0..num_frames {
                            for ch in 0..channels {
                                let s = planes.planes()[ch][frame] as f32 / 32768.0;
                                buffer.push(s);
                            }
                            if channels == 1 {
                                let s = planes.planes()[0][frame] as f32 / 32768.0;
                                buffer.push(s);
                            }
                        }
                    }
                    AudioBufferRef::S24(buf) => {
                        let planes = buf.planes();
                        let channels = buf.spec().channels.count();
                        let num_frames = buf.frames();
                        for frame in 0..num_frames {
                            for ch in 0..channels {
                                let s = (planes.planes()[ch][frame].0 as f32) / 8388608.0;
                                buffer.push(s);
                            }
                            if channels == 1 {
                                let s = (planes.planes()[0][frame].0 as f32) / 8388608.0;
                                buffer.push(s);
                            }
                        }
                    }
                    AudioBufferRef::S32(buf) => {
                        let planes = buf.planes();
                        let channels = buf.spec().channels.count();
                        let num_frames = buf.frames();
                        for frame in 0..num_frames {
                            for ch in 0..channels {
                                let s = planes.planes()[ch][frame] as f32 / 2147483648.0;
                                buffer.push(s);
                            }
                            if channels == 1 {
                                let s = planes.planes()[0][frame] as f32 / 2147483648.0;
                                buffer.push(s);
                            }
                        }
                    }
                    AudioBufferRef::U8(buf) => {
                        let planes = buf.planes();
                        let channels = buf.spec().channels.count();
                        let num_frames = buf.frames();
                        for frame in 0..num_frames {
                            for ch in 0..channels {
                                let s = (planes.planes()[ch][frame] as f32 - 128.0) / 128.0;
                                buffer.push(s);
                            }
                            if channels == 1 {
                                let s = (planes.planes()[0][frame] as f32 - 128.0) / 128.0;
                                buffer.push(s);
                            }
                        }
                    }
                    _ => {}
                }
                Ok(true)
            }
            Err(SymphoniaError::DecodeError(_)) => Ok(true),
            Err(e) => Err(AudioError::Decoder(e.to_string())),
        }
    }
}

pub struct AudioPlayer {
    _stream: Stream,
    sample_queue: Arc<Mutex<VecDeque<f32>>>,
    queue: Arc<Mutex<VecDeque<TrackQueueItem>>>,
    current_source: Arc<Mutex<Option<AudioSource>>>,
    is_playing: Arc<AtomicBool>,
    volume: Arc<AtomicU32>, // 0..1000 representing 0.0..1.0
    elapsed_ms: Arc<AtomicU64>,
    sample_rate: u32,
}

impl AudioPlayer {
    pub fn new() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::Device("No output device available".to_string()))?;

        let default_config = device
            .default_output_config()
            .map_err(|e| AudioError::Device(e.to_string()))?;

        let sample_rate = default_config.sample_rate();
        let channels = default_config.channels() as usize;

        let sample_queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(sample_rate as usize * 4)));
        let queue = Arc::new(Mutex::new(VecDeque::<TrackQueueItem>::new()));
        let current_source = Arc::new(Mutex::new(Option::<AudioSource>::None));
        let is_playing = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1000)); // Default 100% volume
        let elapsed_ms = Arc::new(AtomicU64::new(0));

        let q_clone = Arc::clone(&sample_queue);
        let play_clone = Arc::clone(&is_playing);
        let vol_clone = Arc::clone(&volume);
        let elapsed_clone = Arc::clone(&elapsed_ms);

        let err_fn = |err| tracing::error!("An error occurred on audio stream: {}", err);

        let stream = match default_config.sample_format() {
            SampleFormat::F32 => {
                let config: StreamConfig = default_config.into();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            let playing = play_clone.load(Ordering::Relaxed);
                            let vol = vol_clone.load(Ordering::Relaxed) as f32 / 1000.0;
                            let mut q = q_clone.lock().unwrap();

                            for sample in data.iter_mut() {
                                if playing {
                                    if let Some(s) = q.pop_front() {
                                        *sample = s * vol;
                                    } else {
                                        *sample = 0.0;
                                    }
                                } else {
                                    *sample = 0.0;
                                }
                            }

                            if playing {
                                let frames = data.len() / channels;
                                let ms = (frames as f64 / sample_rate as f64 * 1000.0) as u64;
                                elapsed_clone.fetch_add(ms, Ordering::Relaxed);
                            }
                        },
                        err_fn,
                        None,
                    )
                    .map_err(|e| AudioError::Stream(e.to_string()))?
            }
            _ => return Err(AudioError::Device("Unsupported sample format".to_string())),
        };

        stream.play().map_err(|e| AudioError::Stream(e.to_string()))?;

        let player = Self {
            _stream: stream,
            sample_queue,
            queue,
            current_source,
            is_playing,
            volume,
            elapsed_ms,
            sample_rate,
        };

        player.spawn_decoder_thread();
        Ok(player)
    }

    fn spawn_decoder_thread(&self) {
        let sample_queue = Arc::clone(&sample_queue_from_player(self));
        let queue = Arc::clone(&self.queue);
        let current_source = Arc::clone(&self.current_source);
        let is_playing = Arc::clone(&self.is_playing);
        let elapsed_ms = Arc::clone(&self.elapsed_ms);

        std::thread::spawn(move || {
            let mut temp_buf = Vec::with_capacity(4096);
            loop {
                // Keep audio buffer filled ~1-2 seconds ahead
                let buffer_len = {
                    let q = sample_queue.lock().unwrap();
                    q.len()
                };

                if buffer_len > 96000 {
                    std::thread::sleep(Duration::from_millis(15));
                    continue;
                }

                let mut src_guard = current_source.lock().unwrap();
                if src_guard.is_none() {
                    // Try next from queue for gapless playback
                    let mut q_guard = queue.lock().unwrap();
                    if let Some(next_item) = q_guard.pop_front() {
                        tracing::info!(title = %next_item.title, "Gapless transition to next track");
                        match AudioSource::open(next_item) {
                            Ok(source) => {
                                *src_guard = Some(source);
                                elapsed_ms.store(0, Ordering::Relaxed);
                                is_playing.store(true, Ordering::Relaxed);
                            }
                            Err(e) => {
                                tracing::error!("Failed to open next audio source: {e}");
                            }
                        }
                    } else {
                        drop(src_guard);
                        drop(q_guard);
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                }

                if let Some(ref mut source) = *src_guard {
                    temp_buf.clear();
                    match source.read_samples(&mut temp_buf) {
                        Ok(true) => {
                            let mut q = sample_queue.lock().unwrap();
                            q.extend(temp_buf.drain(..));
                        }
                        Ok(false) => {
                            // Current track finished, prepare for gapless next track
                            tracing::info!("Track finished, transitioning to next in queue");
                            *src_guard = None;
                        }
                        Err(e) => {
                            tracing::error!("Decode error: {e}");
                            *src_guard = None;
                        }
                    }
                }
            }
        });
    }

    pub fn play_track(&self, item: TrackQueueItem) -> Result<(), AudioError> {
        let source = AudioSource::open(item)?;
        {
            let mut current = self.current_source.lock().unwrap();
            *current = Some(source);
        }
        {
            let mut q = self.sample_queue.lock().unwrap();
            q.clear();
        }
        self.elapsed_ms.store(0, Ordering::Relaxed);
        self.is_playing.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn enqueue_next(&self, item: TrackQueueItem) {
        let mut q = self.queue.lock().unwrap();
        q.push_back(item);
    }

    pub fn clear_queue(&self) {
        let mut q = self.queue.lock().unwrap();
        q.clear();
    }

    pub fn pause(&self) {
        self.is_playing.store(false, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.is_playing.store(true, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        let val = (clamped * 1000.0) as u32;
        self.volume.store(val, Ordering::Relaxed);
    }

    pub fn get_volume(&self) -> f32 {
        self.volume.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(self.elapsed_ms.load(Ordering::Relaxed))
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

fn sample_queue_from_player(player: &AudioPlayer) -> &Arc<Mutex<VecDeque<f32>>> {
    &player.sample_queue
}
