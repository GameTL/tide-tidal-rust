//! Music videos: the full-window player, its quality choice and "Similar videos".
//!
//! AVFoundation (through `tide_macos::VideoPlayer`) streams and decodes the
//! video in hardware; this module pulls each new frame into a Slint image, keeps
//! the player bar in step, and routes the transport controls to the video while
//! one is the current media. Starting music stops the video.
//!
//! The soundtrack does not come out of AVPlayer: the same rendition's audio is
//! fetched and decoded by the app's own player, so it goes through the signal
//! path like music (DSP, device rate matching, the bit-perfect indicator).
//! AVPlayer stays muted and its clock leads; the audio is nudged to stay in
//! step with the picture.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slint::{Image, Model, ModelRc, SharedPixelBuffer, VecModel};
use tide_api::hls::{fetch_media_playlist, fetch_segment_audio, segment_at};
use tide_api::{Card, VideoStream};
use tide_audio::TrackQueueItem;
use tide_macos::{VideoDownloadStats, VideoMediaInfo, VideoPlayer, VideoStatus};

use super::{Ui, card_row, fmt_time, on_ui};

/// How often a new frame is looked for: often enough for 120 Hz displays.
const FRAME_POLL: Duration = Duration::from_millis(8);
/// Player bar, Now Playing and end-of-video checks run every this many polls (~250 ms).
const STATUS_EVERY: u32 = 30;
/// Keep long sessions from growing the chunk grid without limit.
const MAX_DEBUG_CHUNKS: u64 = 4096;
/// The soundtrack is nudged once it is this far off the picture (seconds).
const SYNC_TOLERANCE: f64 = 0.035;
/// Further off than this, the soundtrack is fetched again from the picture.
const SYNC_RESTART: f64 = 1.0;
/// The soundtrack is fetched at most this far ahead of the picture (seconds).
const AUDIO_AHEAD: f64 = 30.0;
/// Waiting longer than this for the soundtrack, AVPlayer plays its own.
const AUDIO_WAIT: Duration = Duration::from_secs(8);
/// Codec and bitrate details are refreshed every this many status checks.
const MEDIA_INFO_EVERY: u32 = 4;

/// The video's soundtrack, played by the app's own audio player.
#[derive(Default)]
struct RoutedAudio {
    /// The fetch in flight; a task stops once this moves past its own.
    fetch: Arc<AtomicU64>,
    /// Picture position in ms, so the fetch stays [`AUDIO_AHEAD`] in front.
    playhead: Arc<AtomicU64>,
    /// Player time at which the audio file's first sample plays.
    offset: f64,
    /// Loaded in the app's player.
    ready: bool,
    /// Could not be routed: AVPlayer plays its own audio for this video.
    failed: bool,
    /// The file being written for the current fetch.
    path: Option<PathBuf>,
    /// AVPlayer is held paused until the soundtrack is ready.
    hold_since: Option<Instant>,
    /// Play once the soundtrack is ready (the user has not paused meanwhile).
    resume: bool,
    /// Average bitrate of the soundtrack, kbps.
    kbps: u32,
    /// Picture position at the last poll, to see it actually moving.
    last_position: f64,
}

impl RoutedAudio {
    fn routed(&self) -> bool {
        !self.failed
    }
}

/// The video side of the player.
#[derive(Default)]
pub(super) struct VideoState {
    player: Option<VideoPlayer>,
    timer: Option<slint::Timer>,
    /// The videos Next / Previous step through, and where we are in them.
    queue: Vec<Card>,
    pos: usize,
    context: String,
    stream: Option<VideoStream>,
    /// What is playing: "Auto" or a rendition label such as "1080p".
    choice: String,
    /// The quality last picked in the menu (empty: the best available).
    wanted: String,
    /// Incremented per video, so a late stream lookup for an earlier one is dropped.
    load_gen: u64,
    polls: u32,
    last_status: VideoStatus,
    similar: Vec<Card>,
    last_download: VideoDownloadStats,
    audio: RoutedAudio,
    media: VideoMediaInfo,
    media_polls: u32,
}

/// Where a soundtrack fetch writes: one file per fetch, removed when done with.
fn audio_file(generation: u64) -> PathBuf {
    std::env::temp_dir()
        .join("tide-video-audio")
        .join(format!("{}-{generation}.tmp", std::process::id()))
}

fn remove_audio_file(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("audio"));
}

/// The rendition whose soundtrack is played: the one picked, or for "Auto"
/// the best one (the audio bitrate differs between renditions).
pub(super) fn audio_playlist(stream: &VideoStream, choice: &str) -> Option<String> {
    stream
        .variants
        .iter()
        .find(|v| rendition_label(v.height) == choice)
        .or(stream.variants.first())
        .map(|v| v.url.clone())
}

/// "4.8 Mbps" / "850 kbps".
fn fmt_bitrate(bits_per_sec: f64) -> String {
    if bits_per_sec >= 1_000_000.0 {
        format!("{:.1} Mbps", bits_per_sec / 1_000_000.0)
    } else {
        format!("{:.0} kbps", bits_per_sec / 1000.0)
    }
}

/// The signal-path line for the picture: "H.264 1920×1080 · 4.8 Mbps".
pub(super) fn video_stage_detail(
    media: &VideoMediaInfo,
    size: (u32, u32),
    bandwidth: u64,
) -> String {
    let mut parts = Vec::new();
    let codec = if media.video_codec.is_empty() {
        "Video".to_string()
    } else {
        media.video_codec.clone()
    };
    parts.push(if size.1 > 0 {
        format!("{codec} {}×{}", size.0, size.1)
    } else {
        codec
    });
    let rate = if media.video_bitrate > 0.0 {
        media.video_bitrate
    } else {
        (bandwidth as f64 - media.audio_bitrate).max(0.0)
    };
    if rate > 0.0 {
        parts.push(fmt_bitrate(rate));
    }
    parts.join(" · ")
}

/// "1080p" for a rendition height.
fn rendition_label(height: u32) -> String {
    format!("{height}p")
}

/// Menu entries for the quality pill: Auto first, then each rendition, highest first.
pub(super) fn quality_choices(stream: &VideoStream) -> Vec<String> {
    std::iter::once("Auto".to_string())
        .chain(stream.variants.iter().map(|v| rendition_label(v.height)))
        .collect()
}

/// The menu entry to play at, given the one last picked (`wanted`, empty if
/// none). Unless "Auto" was picked, the best rendition at or below the wanted
/// height is used: adaptive HLS starts on its lowest rendition and takes
/// several seconds to climb, so it is not the default.
pub(super) fn pick_choice(stream: &VideoStream, wanted: &str) -> String {
    if wanted == "Auto" || stream.variants.is_empty() {
        return "Auto".into();
    }
    let cap = wanted
        .strip_suffix('p')
        .and_then(|h| h.parse::<u32>().ok())
        .unwrap_or(u32::MAX);
    let pick = stream
        .variants
        .iter()
        .find(|v| v.height <= cap)
        .or(stream.variants.last());
    pick.map_or_else(|| "Auto".into(), |v| rendition_label(v.height))
}

/// The playlist to open for a quality choice: a fixed rendition, or the
/// adaptive master playlist for "Auto" (and for a choice this video lacks).
pub(super) fn url_for_choice(stream: &VideoStream, choice: &str) -> String {
    stream
        .variants
        .iter()
        .find(|v| rendition_label(v.height) == choice)
        .map_or_else(|| stream.master.clone(), |v| v.url.clone())
}

impl Ui {
    pub(super) fn video_active(&self) -> bool {
        self.app().get_video_active()
    }

    /// Play `card`, with `queue` (which holds it) behind Next / Previous.
    pub(super) fn play_video(self: &Rc<Self>, card: Card, queue: Vec<Card>, context: &str) {
        let mut queue = queue;
        let pos = match queue.iter().position(|c| c.id == card.id) {
            Some(pos) => pos,
            None => {
                queue = vec![card.clone()];
                0
            }
        };
        {
            let mut v = self.video.borrow_mut();
            v.queue = queue;
            v.pos = pos;
            v.context = context.to_string();
            if v.wanted.is_empty() {
                v.wanted = self
                    .ctx
                    .meta
                    .get_json("video-quality")
                    .and_then(|j| serde_json::from_str(&j).ok())
                    .unwrap_or_default();
            }
        }
        self.start_video();
    }

    /// Open the queue entry at `pos`.
    fn start_video(self: &Rc<Self>) {
        let Some(card) = ({
            let v = self.video.borrow();
            v.queue.get(v.pos).cloned()
        }) else {
            return;
        };
        // Music and video never play at once: the app's player is about to
        // carry the video's soundtrack, so remember where the music was.
        if !self.video_active() && self.current_track().is_some() {
            let elapsed = self.effective_elapsed();
            self.state.borrow_mut().pending_seek = Some(elapsed);
        }
        // Invalidate pending audio loads and their chunk callbacks as well.
        self.ctx.play_gen.fetch_add(1, Ordering::SeqCst);
        self.ctx.player.pause();
        self.cancel_routed_audio();
        let app = self.app();
        app.set_show_now_playing(false);
        app.set_menu_open(false);
        let (load_gen, context) = {
            let mut v = self.video.borrow_mut();
            v.load_gen += 1;
            v.stream = None;
            v.last_status = VideoStatus::Loading;
            v.similar.clear();
            v.last_download = VideoDownloadStats::default();
            v.media = VideoMediaInfo::default();
            if let Some(player) = &mut v.player {
                player.stop();
            }
            (v.load_gen, v.context.clone())
        };
        let favorite = self.state.borrow().collection.contains("video", &card.id);
        let mut row = card_row(&card);
        row.favorite = favorite;
        app.set_video_card(row);
        app.set_video_wake(app.get_video_wake().wrapping_add(1));
        app.set_video_active(true);
        app.set_video_open(true);
        app.set_video_loading(true);
        app.set_video_frame(Image::default());
        app.set_video_similar(ModelRc::default());
        app.set_video_qualities(ModelRc::default());
        app.set_video_quality_label("".into());
        app.set_has_track(true);
        app.set_current_id(card.id.as_str().into());
        app.set_np_title(card.title.as_str().into());
        self.reset_video_debug("Resolving video");
        app.set_np_artist(card.subtitle.as_str().into());
        app.set_np_artist_id("".into());
        app.set_np_artists(Default::default());
        app.set_np_context(context.into());
        app.set_np_explicit(false);
        app.set_np_favorite(favorite);
        app.set_np_cover(Image::default());
        app.set_playing(false);
        app.set_buffering(true);
        app.set_playing_card_kind("video".into());
        app.set_playing_card_id(card.id.as_str().into());
        self.present_video_time(0.0, card.duration as f64);
        if let Some(url) = card.image_url.clone() {
            let (id, weak) = (card.id.clone(), Rc::downgrade(self));
            self.load_image(&url, 640, move |img, tint| {
                let Some(ui) = weak.upgrade() else { return };
                if ui.video_active() && ui.app().get_current_id() == id.as_str() {
                    ui.app().set_np_cover(img.clone());
                    ui.app().set_np_tint(tint);
                }
            });
        }
        self.ctx.with_media(|m| {
            let _ = m.set_metadata(
                &card.title,
                &card.subtitle,
                "",
                Some(Duration::from_secs(card.duration as u64)),
                card.image_url.as_deref(),
            );
        });
        self.ensure_video_timer();

        let id = card.id.clone();
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.video_stream(&id).await {
                Ok(stream) => on_ui(move |ui| ui.video_stream_ready(load_gen, stream)),
                Err(e) => {
                    tracing::error!("Could not play this video: {e}");
                    let reason = e.to_string();
                    on_ui(move |ui| {
                        if ui.video.borrow().load_gen == load_gen {
                            ui.app().set_video_loading(false);
                            ui.app().set_buffering(false);
                            ui.app().set_debug_status("Video stream failed".into());
                            ui.app().set_debug_stats(reason.into());
                            ui.app().set_debug_active(false);
                        }
                    });
                }
            }
            match api.video_recommendations(&id).await {
                Ok(cards) => on_ui(move |ui| ui.present_similar(load_gen, cards)),
                Err(e) => tracing::warn!("similar videos failed: {e}"),
            }
        });
    }

    fn video_stream_ready(self: &Rc<Self>, load_gen: u64, stream: VideoStream) {
        let mut v = self.video.borrow_mut();
        if v.load_gen != load_gen {
            return;
        }
        let choices = quality_choices(&stream);
        v.choice = pick_choice(&stream, &v.wanted);
        let url = url_for_choice(&stream, &v.choice);
        let player = v.player.get_or_insert_with(VideoPlayer::new);
        // Muted: the soundtrack comes through the app's own player.
        player.set_volume(0.0);
        player.load(&url, 0.0);
        v.stream = Some(stream);
        let choice = v.choice.clone();
        drop(v);
        self.start_routed_audio(0.0, true);
        let app = self.app();
        app.set_video_qualities(ModelRc::new(VecModel::from(
            choices
                .into_iter()
                .map(slint::SharedString::from)
                .collect::<Vec<_>>(),
        )));
        app.set_video_quality(choice.into());
    }

    fn present_similar(self: &Rc<Self>, load_gen: u64, cards: Vec<Card>) {
        if self.video.borrow().load_gen != load_gen {
            return;
        }
        let playing = self.app().get_current_id();
        let cards: Vec<Card> = cards
            .into_iter()
            .filter(|c| c.id != playing.as_str())
            .collect();
        let model = Rc::new(VecModel::from(self.rows_for(&cards)));
        for (i, card) in cards.iter().enumerate() {
            let Some(url) = card.image_url.as_deref() else {
                continue;
            };
            let model = model.clone();
            let url = url.replace("/640x360.jpg", "/320x180.jpg");
            self.load_image(&url, 320, move |img, _| {
                if let Some(mut row) = model.row_data(i) {
                    row.image = img.clone();
                    model.set_row_data(i, row);
                }
            });
        }
        self.video.borrow_mut().similar = cards;
        self.app().set_video_similar(ModelRc::from(model));
    }

    /// One poll of the frame pump; drives the player bar every few polls.
    fn video_poll(self: &Rc<Self>) {
        let app = self.app();
        let (status, position, duration, size, download) = {
            let mut guard = self.video.borrow_mut();
            let v = &mut *guard;
            // A reused native player is idle until this video's lookup finishes.
            // Keep Resolving (or the lookup error) visible during that interval.
            if v.stream.is_none() {
                return;
            }
            let Some(player) = v.player.as_mut() else {
                return;
            };
            // The same live frame feeds the mini player and its full-window
            // expansion. Collapsing must not discard or stop presenting frames.
            let mut fresh = None;
            player.pull_frame(|w, h, stride, bgra| {
                // A fresh buffer per frame: the renderer may still hold the last one.
                let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
                tide_macos::bgra_to_rgba(
                    bgra,
                    w as usize,
                    h as usize,
                    stride,
                    buf.make_mut_bytes(),
                );
                fresh = Some(buf);
            });
            if let Some(buf) = fresh {
                app.set_video_frame(Image::from_rgba8(buf));
            }
            v.polls = v.polls.wrapping_add(1);
            let check = v.polls.is_multiple_of(STATUS_EVERY);
            // Held for the soundtrack: it is starting, not paused.
            let held = v.audio.hold_since.is_some() && v.audio.resume;
            (
                match player.status() {
                    VideoStatus::Paused if held => VideoStatus::Loading,
                    status => status,
                },
                player.position(),
                player.duration(),
                player.presentation_size(),
                check.then(|| player.download_stats()),
            )
        };
        self.sync_routed_audio(status, position, download.is_some());
        let Some(download) = download else { return };
        self.present_video_debug(status, download);
        self.refresh_video_media(size);
        let last = std::mem::replace(&mut self.video.borrow_mut().last_status, status);
        let playing = status == VideoStatus::Playing;
        app.set_playing(playing || status == VideoStatus::Loading && last != VideoStatus::Paused);
        app.set_buffering(status == VideoStatus::Loading);
        app.set_video_loading(
            status == VideoStatus::Loading && app.get_video_frame().size().width == 0,
        );
        if size.1 > 0 {
            app.set_video_quality_label(format!("{}P", size.1).into());
            app.set_video_aspect(size.0 as f32 / size.1 as f32);
        }
        self.present_video_time(position, duration);
        if status != last {
            self.ctx.with_media(|m| {
                let _ = m.set_playback_state(playing, Some(Duration::from_secs_f64(position)));
            });
        }
        match status {
            VideoStatus::Ended if last != VideoStatus::Ended => self.video_skip(1, false),
            VideoStatus::Failed if last != VideoStatus::Failed => {
                let reason = self
                    .video
                    .borrow()
                    .player
                    .as_ref()
                    .and_then(VideoPlayer::error)
                    .unwrap_or_else(|| "unknown error".into());
                tracing::error!("Video playback failed: {reason}");
                app.set_video_loading(false);
                app.set_playing(false);
            }
            _ => {}
        }
    }

    fn reset_video_debug(&self, status: &str) {
        let app = self.app();
        app.set_debug_title(app.get_np_title());
        app.set_debug_status(status.into());
        app.set_debug_stats("HLS · waiting for media requests".into());
        app.set_debug_active(true);
        app.set_debug_blocks(ModelRc::default());
    }

    fn present_video_debug(&self, status: VideoStatus, stats: VideoDownloadStats) {
        let previous = std::mem::replace(&mut self.video.borrow_mut().last_download, stats.clone());
        let downloading = stats.bytes_received > previous.bytes_received
            || stats.chunks_received > previous.chunks_received;
        let app = self.app();
        app.set_debug_status(
            match status {
                VideoStatus::Idle => "Video · idle",
                VideoStatus::Loading => "Video · buffering",
                VideoStatus::Playing => "Video · playing",
                VideoStatus::Paused => "Video · paused",
                VideoStatus::Ended => "Video · ended",
                VideoStatus::Failed => "Video · failed",
            }
            .into(),
        );
        app.set_debug_active(downloading || status == VideoStatus::Loading);
        app.set_debug_stats(
            format!(
                "HLS · {} chunks received · {:.1} MiB\n{:.1}s buffered · {:.1} Mbps · {} stalls",
                stats.chunks_received,
                stats.bytes_received as f64 / (1024.0 * 1024.0),
                stats.buffered_ahead_secs,
                stats.observed_bitrate / 1_000_000.0,
                stats.stalls,
            )
            .into(),
        );
        let shown = stats.chunks_received.min(MAX_DEBUG_CHUNKS) as usize;
        if app.get_debug_blocks().row_count() != shown {
            // AVFoundation only reports completed HLS requests. Do not invent
            // queued/in-flight states for downloads owned by the native player.
            app.set_debug_blocks(ModelRc::new(VecModel::from(vec![2; shown])));
        }
    }

    fn present_video_time(&self, position: f64, duration: f64) {
        let app = self.app();
        let total = duration.max(0.0);
        app.set_duration_secs(total as i32);
        app.set_total(fmt_time(total as u64).into());
        app.set_elapsed(fmt_time(position.min(total).max(0.0) as u64).into());
        let progress = if total > 0.0 { position / total } else { 0.0 };
        app.set_progress(progress.clamp(0.0, 1.0) as f32);
    }

    fn ensure_video_timer(self: &Rc<Self>) {
        let mut v = self.video.borrow_mut();
        if v.timer.is_some() {
            return;
        }
        let timer = slint::Timer::default();
        let weak = Rc::downgrade(self);
        timer.start(slint::TimerMode::Repeated, FRAME_POLL, move || {
            if let Some(ui) = weak.upgrade() {
                ui.video_poll();
            }
        });
        v.timer = Some(timer);
    }

    pub(super) fn video_toggle_play(self: &Rc<Self>) {
        let mut v = self.video.borrow_mut();
        if v.audio.hold_since.is_some() {
            // Still waiting for the soundtrack: change what happens once it is in.
            v.audio.resume = !v.audio.resume;
            let resume = v.audio.resume;
            drop(v);
            let app = self.app();
            app.set_playing(resume);
            app.set_video_wake(app.get_video_wake().wrapping_add(1));
            return;
        }
        let ended = v.last_status == VideoStatus::Ended;
        let Some(player) = v.player.as_mut() else {
            return;
        };
        let playing = matches!(player.status(), VideoStatus::Playing | VideoStatus::Loading);
        if ended {
            player.seek(0.0);
            player.play();
        } else if playing {
            player.pause();
            // The soundtrack stops with the picture rather than on the next check.
            self.ctx.player.pause();
        } else {
            player.play();
        }
        let position = player.position();
        drop(v);
        let app = self.app();
        app.set_playing(!playing || ended);
        app.set_video_wake(app.get_video_wake().wrapping_add(1));
        self.ctx.with_media(|m| {
            let _ =
                m.set_playback_state(!playing || ended, Some(Duration::from_secs_f64(position)));
        });
    }

    /// Next (`delta` 1) or the following video once one ends (`manual` false).
    pub(super) fn video_skip(self: &Rc<Self>, delta: i32, manual: bool) {
        let repeat = self.state.borrow().repeat;
        let next = {
            let v = self.video.borrow();
            let len = v.queue.len();
            if !manual && repeat == 2 {
                Some(v.pos)
            } else if delta > 0 && v.pos + 1 < len {
                Some(v.pos + 1)
            } else if delta > 0 && repeat == 1 && len > 0 {
                Some(0)
            } else {
                None
            }
        };
        match next {
            Some(pos) => {
                self.video.borrow_mut().pos = pos;
                self.start_video();
            }
            None if !manual => {
                if let Some(player) = self.video.borrow_mut().player.as_mut() {
                    player.pause();
                }
                self.app().set_playing(false);
            }
            None => {}
        }
    }

    pub(super) fn video_previous(self: &Rc<Self>) {
        let (position, pos) = {
            let v = self.video.borrow();
            (v.player.as_ref().map_or(0.0, VideoPlayer::position), v.pos)
        };
        if position > 3.0 || pos == 0 {
            self.video_seek(0.0);
            return;
        }
        self.video.borrow_mut().pos = pos - 1;
        self.start_video();
    }

    pub(super) fn video_seek(self: &Rc<Self>, fraction: f32) {
        let mut v = self.video.borrow_mut();
        let Some(player) = v.player.as_mut() else {
            return;
        };
        let duration = player.duration();
        let to = duration * fraction.clamp(0.0, 1.0) as f64;
        player.seek(to);
        if v.last_status == VideoStatus::Ended {
            v.last_status = VideoStatus::Paused;
        }
        let routed = v.audio.routed();
        drop(v);
        self.present_video_time(to, duration);
        let playing = self.app().get_playing();
        if routed {
            self.start_routed_audio(to, playing);
        }
        self.ctx.with_media(|m| {
            let _ = m.set_playback_state(playing, Some(Duration::from_secs_f64(to)));
        });
    }

    /// The volume the player bar shows, or silence while muted.
    fn effective_volume(&self) -> f32 {
        if self.state.borrow().muted {
            0.0
        } else {
            self.app().get_volume()
        }
    }

    pub(super) fn sync_video_volume(&self) {
        let volume = self.effective_volume();
        let v = self.video.borrow();
        if let Some(player) = &v.player {
            // A routed soundtrack takes the app player's volume instead.
            player.set_volume(if v.audio.routed() { 0.0 } else { volume });
        }
    }

    /// Heart in the player bar while a video plays.
    pub(super) fn video_toggle_favorite(self: &Rc<Self>) {
        let mut row = self.app().get_video_card();
        if row.id.is_empty() {
            return;
        }
        row.favorite = self.state.borrow().collection.contains("video", &row.id);
        self.toggle_card_favorite(row);
    }

    /// Reflect a My Collection change on the video that is playing.
    pub(super) fn video_favorite_changed(&self, id: &str, favorite: bool) {
        let app = self.app();
        let mut row = app.get_video_card();
        if app.get_video_active() && row.id == id {
            row.favorite = favorite;
            app.set_video_card(row);
            app.set_np_favorite(favorite);
        }
    }

    pub(super) fn set_video_quality(self: &Rc<Self>, choice: &str) {
        let mut v = self.video.borrow_mut();
        if v.choice == choice {
            return;
        }
        v.choice = choice.to_string();
        v.wanted = choice.to_string();
        let _ = self
            .ctx
            .meta
            .put_json("video-quality", &serde_json::json!(choice).to_string());
        let Some(url) = v.stream.as_ref().map(|s| url_for_choice(s, choice)) else {
            return;
        };
        let (holding, resume) = (v.audio.hold_since.is_some(), v.audio.resume);
        let Some(player) = v.player.as_mut() else {
            return;
        };
        // Reopen at the same spot; the last frame stays up until the new one arrives.
        let paused = if holding {
            !resume
        } else {
            player.status() == VideoStatus::Paused
        };
        let position = player.position();
        player.load(&url, position);
        if paused {
            player.pause();
        }
        v.last_download = VideoDownloadStats::default();
        drop(v);
        self.reset_video_debug("Video · switching quality");
        self.app().set_video_quality(choice.into());
        // Each rendition has its own soundtrack (bitrates differ).
        self.start_routed_audio(position, !paused);
    }

    pub(super) fn play_similar_video(self: &Rc<Self>, index: usize) {
        let similar = self.video.borrow().similar.clone();
        if let Some(card) = similar.get(index).cloned() {
            self.play_video(card, similar, "Similar videos");
        }
    }

    /// Show or leave the full-window view. Leaving it keeps the video playing.
    pub(super) fn show_video_view(&self, open: bool) {
        let app = self.app();
        if !app.get_video_active() {
            return;
        }
        app.set_video_open(open);
        if open {
            app.set_video_wake(app.get_video_wake().wrapping_add(1));
        } else {
            app.set_video_similar_open(false);
        }
    }

    /// Stop the video and hand the player bar back to the music queue.
    pub(super) fn stop_video(self: &Rc<Self>) {
        if !self.video_active() {
            return;
        }
        {
            let mut v = self.video.borrow_mut();
            v.load_gen += 1;
            v.timer = None;
            if let Some(player) = &mut v.player {
                player.stop();
            }
            v.player = None;
        }
        self.cancel_routed_audio();
        // The frames' textures would otherwise sit in the GPU cache.
        crate::platform::purge_gpu_cache();
        let app = self.app();
        app.set_video_active(false);
        app.set_video_open(false);
        app.set_video_similar_open(false);
        app.set_video_frame(Image::default());
        app.set_video_loading(false);
        app.set_playing(false);
        app.set_buffering(false);
        app.set_debug_title("".into());
        app.set_debug_status("Idle".into());
        app.set_debug_stats("".into());
        app.set_debug_active(false);
        app.set_debug_blocks(ModelRc::default());
        app.set_playing_card_kind("".into());
        app.set_playing_card_id("".into());
        // The soundtrack's signal path is gone with it.
        self.present_signal();
        if self.current_track().is_some() {
            self.present_current();
            self.present_format();
            let elapsed = self.effective_elapsed();
            if let Some(track) = self.current_track() {
                let total = track.duration_secs.max(1) as f32;
                app.set_elapsed(fmt_time(elapsed.as_secs()).into());
                app.set_progress((elapsed.as_secs_f32() / total).clamp(0.0, 1.0));
            }
        } else {
            app.set_has_track(false);
        }
    }

    /// Fetch and play the soundtrack from `at` (seconds) through the app's
    /// player, holding the picture until it is ready. `resume`: play then.
    fn start_routed_audio(self: &Rc<Self>, at: f64, resume: bool) {
        let (generation, current, playhead, url, path) = {
            let mut guard = self.video.borrow_mut();
            let v = &mut *guard;
            let generation = v.audio.fetch.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(old) = v.audio.path.take() {
                remove_audio_file(&old);
            }
            let url = v.stream.as_ref().and_then(|s| audio_playlist(s, &v.choice));
            let path = audio_file(generation);
            v.audio.path = Some(path.clone());
            v.audio.ready = false;
            v.audio.failed = false;
            v.audio.resume = resume;
            v.audio.hold_since = Some(Instant::now());
            v.audio
                .playhead
                .store((at.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
            if let Some(player) = v.player.as_mut() {
                player.set_volume(0.0);
                player.pause();
            }
            (
                generation,
                v.audio.fetch.clone(),
                v.audio.playhead.clone(),
                url,
                path,
            )
        };
        self.ctx.player.stop();
        let Some(url) = url else {
            self.routed_audio_failed(generation, "the stream lists no renditions");
            return;
        };
        self.spawn(move |_| async move {
            if let Err(e) = fetch_soundtrack(url, at, path, generation, current, playhead).await {
                on_ui(move |ui| ui.routed_audio_failed(generation, &e));
            }
        });
    }

    /// The first stretch of the soundtrack is on disk: load it into the app's player.
    fn routed_audio_ready(
        self: &Rc<Self>,
        generation: u64,
        path: PathBuf,
        offset: f64,
        at: f64,
        kbps: u32,
    ) {
        let current = {
            let v = self.video.borrow();
            if v.audio.fetch.load(Ordering::SeqCst) != generation {
                return;
            }
            v.audio.fetch.clone()
        };
        let app = self.app();
        let card = app.get_video_card();
        let item = TrackQueueItem {
            id: format!("video:{}", card.id),
            title: card.title.to_string(),
            artist: card.subtitle.to_string(),
            file_path: path,
            duration: Duration::from_secs(app.get_duration_secs().max(0) as u64),
        };
        let start = Duration::from_secs_f64((at - offset).max(0.0));
        self.spawn(move |ctx| async move {
            let player_ctx = ctx.clone();
            let result = tokio::task::spawn_blocking(move || {
                if current.load(Ordering::SeqCst) != generation {
                    return Err("superseded".to_string());
                }
                let result = player_ctx.player.play_track_at(item, start);
                // The picture is still held: start in step with it, not before.
                player_ctx.player.pause();
                result.map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            on_ui(move |ui| ui.routed_audio_started(generation, result, offset, kbps));
        });
    }

    fn routed_audio_started(
        self: &Rc<Self>,
        generation: u64,
        result: Result<(), String>,
        offset: f64,
        kbps: u32,
    ) {
        let mut v = self.video.borrow_mut();
        if v.audio.fetch.load(Ordering::SeqCst) != generation {
            return;
        }
        if let Err(e) = result {
            drop(v);
            self.routed_audio_failed(generation, &e);
            return;
        }
        v.audio.offset = offset;
        v.audio.ready = true;
        v.audio.kbps = kbps;
        v.audio.hold_since = None;
        let resume = v.audio.resume;
        if let Some(player) = v.player.as_mut()
            && resume
        {
            player.play();
        }
        drop(v);
        self.ctx.player.set_source_bitrate(kbps);
        tracing::info!(
            offset,
            kbps,
            "Video soundtrack routed through the signal path"
        );
        self.present_signal();
    }

    /// The soundtrack cannot be routed: let AVPlayer play its own.
    fn routed_audio_failed(self: &Rc<Self>, generation: u64, reason: &str) {
        let volume = self.effective_volume();
        let mut v = self.video.borrow_mut();
        if v.audio.fetch.load(Ordering::SeqCst) != generation {
            return;
        }
        tracing::warn!("Video audio plays outside the signal path: {reason}");
        v.audio.fetch.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = v.audio.path.take() {
            remove_audio_file(&path);
        }
        v.audio.failed = true;
        v.audio.ready = false;
        v.audio.hold_since = None;
        let resume = v.audio.resume;
        if let Some(player) = v.player.as_mut() {
            player.set_volume(volume);
            if resume {
                player.play();
            }
        }
        drop(v);
        self.ctx.player.stop();
        self.present_signal();
    }

    /// Drop the soundtrack: stop its fetch, its playback and its file.
    fn cancel_routed_audio(&self) {
        {
            let mut v = self.video.borrow_mut();
            v.audio.fetch.fetch_add(1, Ordering::SeqCst);
            if let Some(path) = v.audio.path.take() {
                remove_audio_file(&path);
            }
            v.audio.ready = false;
            v.audio.failed = false;
            v.audio.hold_since = None;
        }
        self.ctx.player.stop();
    }

    /// Keep the soundtrack running with the picture and in step with it.
    /// Runs on every frame poll so the sound starts the moment the picture
    /// moves; drift is checked on status polls (`check`).
    fn sync_routed_audio(self: &Rc<Self>, status: VideoStatus, position: f64, check: bool) {
        let (offset, moving, waited) = {
            let mut v = self.video.borrow_mut();
            let moving = position > v.audio.last_position + 0.001;
            v.audio.last_position = position;
            let waited = check && v.audio.hold_since.is_some_and(|t| t.elapsed() > AUDIO_WAIT);
            if waited {
                (None, moving, Some(v.audio.fetch.load(Ordering::SeqCst)))
            } else if v.audio.ready && !v.audio.failed {
                if check {
                    v.audio
                        .playhead
                        .store((position.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
                }
                (Some(v.audio.offset), moving, None)
            } else {
                (None, moving, None)
            }
        };
        if let Some(generation) = waited {
            self.routed_audio_failed(generation, "the soundtrack took too long to load");
            return;
        }
        let Some(offset) = offset else { return };
        let player = &self.ctx.player;
        // Positive: the soundtrack is behind the picture.
        let drift = || position - (offset + player.elapsed().as_secs_f64());
        if status != VideoStatus::Playing {
            if player.is_playing() {
                player.pause();
            }
            return;
        }
        if !player.is_playing() {
            // AVPlayer reports playing a little before its frames move.
            if moving {
                player.nudge((drift() * player.sample_rate() as f64).round() as i64);
                player.resume();
            }
            return;
        }
        if !check {
            return;
        }
        let drift = drift();
        tracing::trace!(drift, position, "video soundtrack drift");
        if drift.abs() > SYNC_RESTART {
            tracing::info!(
                drift,
                "Video soundtrack fell out of step; fetching it again"
            );
            self.start_routed_audio(position, true);
        } else if drift.abs() > SYNC_TOLERANCE {
            let moved = player.nudge((drift * player.sample_rate() as f64).round() as i64);
            tracing::debug!(drift, moved, "Video soundtrack nudged into step");
        }
    }

    /// Follow the codec / bitrate / size of what is on screen into the signal path.
    fn refresh_video_media(&self, size: (u32, u32)) {
        let changed = {
            let mut guard = self.video.borrow_mut();
            let v = &mut *guard;
            v.media_polls = v.media_polls.wrapping_add(1);
            if !v.media_polls.is_multiple_of(MEDIA_INFO_EVERY) {
                return;
            }
            let Some(info) = v.player.as_ref().map(VideoPlayer::media_info) else {
                return;
            };
            let bandwidth = selected_bandwidth(v.stream.as_ref(), &v.choice);
            let before = video_stage_detail(&v.media, size, bandwidth);
            let after = video_stage_detail(&info, size, bandwidth);
            v.media = info;
            before != after
                || self.app().get_signal_stages().row_count() > 0
                    && !self
                        .app()
                        .get_signal_stages()
                        .iter()
                        .any(|s| s.kind == "video")
        };
        if changed {
            self.present_signal();
        }
    }

    /// The picture's line in the signal path, while a routed video plays.
    pub(super) fn video_signal_stage(&self) -> Option<String> {
        if !self.video_active() {
            return None;
        }
        let v = self.video.try_borrow().ok()?;
        if !v.audio.ready {
            return None;
        }
        let size = v.player.as_ref()?.presentation_size();
        let bandwidth = selected_bandwidth(v.stream.as_ref(), &v.choice);
        Some(video_stage_detail(&v.media, size, bandwidth))
    }

    /// The videos around `card` on screen, for Next / Previous, and what to call them.
    pub(super) fn video_queue_for(&self, card: &Card) -> (Vec<Card>, String) {
        let app = self.app();
        let from = |model: ModelRc<super::CardData>| -> Vec<Card> {
            model
                .iter()
                .filter(|row| row.kind == "video")
                .map(|row| super::card_from_row(&row))
                .collect()
        };
        let grid = if app.get_view() == "grid" {
            let st = self.state.borrow();
            st.grid
                .models
                .iter()
                .flat_map(|m| m.iter().collect::<Vec<_>>())
                .filter(|row| row.kind == "video")
                .map(|row| super::card_from_row(&row))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let all = self.state.borrow().grid.all.clone();
        let with_lengths = |mut cards: Vec<Card>| {
            for c in &mut cards {
                if let Some(full) = all.iter().find(|a| a.kind == c.kind && a.id == c.id) {
                    c.duration = full.duration;
                }
            }
            cards
        };
        if grid.iter().any(|c| c.id == card.id) {
            let context = if app.get_nav_key() == "videos" {
                "My Videos".to_string()
            } else {
                app.get_page_title().to_string()
            };
            return (with_lengths(grid), context);
        }
        let similar = self.video.borrow().similar.clone();
        if similar.iter().any(|c| c.id == card.id) {
            return (similar, "Similar videos".into());
        }
        let search = from(app.get_search_cards());
        if search.iter().any(|c| c.id == card.id) {
            return (search, "Search".into());
        }
        let sections: Vec<Card> = {
            let st = self.state.borrow();
            st.section_models
                .iter()
                .find(|(cards, _, _)| {
                    cards
                        .iter()
                        .any(|r| r.kind == "video" && r.id == card.id.as_str())
                })
                .map(|(cards, _, _)| from(ModelRc::from(cards.clone())))
                .unwrap_or_default()
        };
        if !sections.is_empty() {
            return (sections, app.get_page_title().to_string());
        }
        (vec![card.clone()], String::new())
    }
}

/// Advertised bandwidth of the rendition picked (0 for "Auto").
fn selected_bandwidth(stream: Option<&VideoStream>, choice: &str) -> u64 {
    stream
        .and_then(|s| {
            s.variants
                .iter()
                .find(|v| rendition_label(v.height) == choice)
        })
        .map_or(0, |v| v.bandwidth)
}

/// Write the soundtrack of the rendition playlist `url` from `at` seconds
/// into `path`, one segment at a time, staying [`AUDIO_AHEAD`] of the
/// picture. Stops quietly once `current` moves past `generation`.
async fn fetch_soundtrack(
    url: String,
    at: f64,
    path: PathBuf,
    generation: u64,
    current: Arc<AtomicU64>,
    playhead: Arc<AtomicU64>,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let live = || current.load(Ordering::SeqCst) == generation;
    let segments = fetch_media_playlist(&url)
        .await
        .map_err(|e| e.to_string())?;
    if segments.is_empty() {
        return Err("the rendition playlist is empty".into());
    }
    let first = segment_at(&segments, at);
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| e.to_string())?;
    let (mut bytes, mut secs) = (0usize, 0f64);
    for (i, seg) in segments[first..].iter().enumerate() {
        loop {
            if !live() {
                return Ok(());
            }
            let ahead = seg.start - playhead.load(Ordering::Relaxed) as f64 / 1000.0;
            if i == 0 || ahead <= AUDIO_AHEAD {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let mut tries = 0;
        let ts = loop {
            match fetch_segment_audio(&seg.url).await {
                Ok(ts) => break ts,
                Err(e) if tries >= 3 => return Err(e.to_string()),
                Err(e) => {
                    tries += 1;
                    tracing::warn!(
                        "video soundtrack segment {} failed ({e}); retrying",
                        first + i
                    );
                    tokio::time::sleep(Duration::from_millis(500 * tries)).await;
                }
            }
        };
        if !live() {
            return Ok(());
        }
        if ts.adts.is_empty() {
            if i == 0 {
                return Err("the stream carries no AAC audio".into());
            }
            continue;
        }
        file.write_all(&ts.adts).await.map_err(|e| e.to_string())?;
        file.flush().await.map_err(|e| e.to_string())?;
        bytes += ts.adts.len();
        secs += seg.duration;
        if i == 0 {
            // The player's timeline starts at the segment's first video frame.
            let offset = seg.start + ts.audio_lead();
            let kbps = (bytes as f64 * 8.0 / secs / 1000.0).round() as u32;
            let path = path.clone();
            on_ui(move |ui| ui.routed_audio_ready(generation, path, offset, at, kbps));
        }
    }
    drop(file);
    if live() {
        // Complete: the decoder may now treat the end of the file as the end.
        let _ = tokio::fs::rename(&path, path.with_extension("audio")).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tide_api::VideoVariant;

    fn stream() -> VideoStream {
        let variant = |height, url: &str| VideoVariant {
            width: height * 16 / 9,
            height,
            bandwidth: height as u64 * 1000,
            url: url.into(),
        };
        VideoStream {
            master: "master".into(),
            variants: vec![variant(1080, "v1080"), variant(720, "v720")],
        }
    }

    #[test]
    fn quality_menu_offers_auto_then_each_rendition() {
        assert_eq!(quality_choices(&stream()), ["Auto", "1080p", "720p"]);
    }

    #[test]
    fn best_rendition_is_the_default() {
        let s = stream();
        assert_eq!(pick_choice(&s, ""), "1080p");
        assert_eq!(pick_choice(&s, "Auto"), "Auto");
        assert_eq!(pick_choice(&s, "720p"), "720p");
        // Picked on a taller video: the best this one has under it.
        assert_eq!(pick_choice(&s, "900p"), "720p");
        // Lower than anything offered: the lowest there is.
        assert_eq!(pick_choice(&s, "360p"), "720p");
    }

    #[test]
    fn quality_choice_picks_the_playlist() {
        let s = stream();
        assert_eq!(url_for_choice(&s, "Auto"), "master");
        assert_eq!(url_for_choice(&s, "720p"), "v720");
        // A rendition this video lacks falls back to adaptive.
        assert_eq!(url_for_choice(&s, "2160p"), "master");
    }
}
