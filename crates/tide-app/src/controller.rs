//! UI controller: owns navigation, the play queue and all Slint models.
//!
//! Everything in [`Ui`] lives on the Slint event-loop thread. Background work
//! runs on tokio with only the `Send` [`Ctx`] and reports back via [`on_ui`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use slint::{Color, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tide_api::{ApiHandle, Card, CatalogDetail, Lyrics, Section, TidalService, TrackItem};
use tide_audio::{AudioPlayer, TrackQueueItem};
use tide_cache::{AudioCache, ImageCache, MetadataCache, PlaybackCursor, SavedPlayback};
use tide_macos::{MacMediaControls, MediaControlEvent};

use crate::signal::{khz, signal_path};
use crate::{App, CardData, CreditLine, LyricLine, MainWindow, SectionData, SignalStage, TrackData};

/// Cache key for "best quality the account can stream".
pub const QUALITY_KEY: &str = "MAX";
const PREFETCH_BEFORE_END: Duration = Duration::from_secs(25);
/// "Keep most played offline" looks at this many top tracks, played at least this often.
const KEEP_OFFLINE_TOP: usize = 50;
const KEEP_OFFLINE_MIN_PLAYS: u32 = 2;
/// Quarter-second ticks of silence before the output device is handed back (3 s).
const IDLE_TICKS_BEFORE_RELEASE: u32 = 12;
/// How long the app waits for the browser to come back from a login.
const LOGIN_WAIT: Duration = Duration::from_secs(600);

fn gb_to_bytes(gb: f64) -> u64 {
    (gb.clamp(0.1, 4096.0) * 1024.0 * 1024.0 * 1024.0) as u64
}

/// User preferences, persisted in the local database.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "max" | "high" | "low"
    pub quality: String,
    pub show_collection: bool,
    pub show_audio_metadata: bool,
    /// Size limit of the offline audio cache.
    pub cache_max_gb: f64,
    /// Re-download the most played tracks when they are missing from the cache.
    pub keep_most_played: bool,
    /// Switch the output device to each track's sample rate (bit-perfect).
    pub rate_matching: bool,
    /// Hand the device back its original sample rate on pause and quit.
    #[serde(default = "default_true")]
    pub restore_sample_rate: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            quality: "max".into(),
            show_collection: true,
            show_audio_metadata: true,
            cache_max_gb: 20.0,
            keep_most_played: true,
            rate_matching: true,
            restore_sample_rate: true,
        }
    }
}

impl Settings {
    pub fn load(meta: &MetadataCache) -> Self {
        meta.get_json("settings").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_next_preserves_the_current_track_and_shuffled_order() {
        let track = |id: &str| TrackItem { id: id.into(), ..Default::default() };
        let mut state = State {
            queue: ["A", "B", "C", "D"].map(track).to_vec(),
            order: vec![3, 1, 0, 2],
            pos: 1,
            shuffle: true,
            prefetched_for: Some(1),
            ..Default::default()
        };
        state.insert_next(track("X"));
        state.insert_next(track("Y"));
        let play_order: Vec<_> = state.order.iter().map(|&i| state.queue[i].id.as_str()).collect();
        assert_eq!(play_order, ["D", "B", "Y", "X", "A", "C"]);
        assert_eq!(play_order[state.pos], "B");
        assert_eq!(state.prefetched_for, None);
    }

    #[test]
    fn grid_is_filtered_sorted_and_grouped() {
        let album = |title: &str, by: &str, added: &str, released: &str| Card {
            kind: "album".into(),
            id: title.into(),
            title: title.into(),
            subtitle: by.into(),
            by: by.into(),
            added: added.into(),
            released: released.into(),
            ..Default::default()
        };
        let cards = vec![
            album("Zebra", "Lauv", "2026-01-01", "2019-05-01"),
            album("apple", "Weezer", "2026-03-01", "2024-02-02"),
            album("Mango", "Lauv", "2026-02-01", "2024-09-09"),
        ];
        let titles = |groups: &[(String, Vec<Card>)]| -> Vec<(String, Vec<String>)> {
            groups.iter().map(|(g, c)| (g.clone(), c.iter().map(|c| c.title.clone()).collect())).collect()
        };
        let flat = |sort: &str| titles(&arrange_grid(&cards, "", sort, "None"))[0].1.clone();
        assert_eq!(flat("Date added"), ["apple", "Mango", "Zebra"]);
        assert_eq!(flat("A-Z"), ["apple", "Mango", "Zebra"]);
        assert_eq!(flat("Artist"), ["Mango", "Zebra", "apple"]);
        assert_eq!(flat("Release date"), ["Mango", "apple", "Zebra"]);

        // Filtering matches the title or who it is by.
        assert_eq!(titles(&arrange_grid(&cards, " lauv ", "A-Z", "None"))[0].1, ["Mango", "Zebra"]);

        // Groups keep the sort order inside; years run newest first.
        assert_eq!(
            titles(&arrange_grid(&cards, "", "A-Z", "Artist")),
            [("Lauv".to_string(), vec!["Mango".to_string(), "Zebra".into()]), ("Weezer".into(), vec!["apple".into()])]
        );
        let years: Vec<String> = arrange_grid(&cards, "", "A-Z", "Year").into_iter().map(|(g, _)| g).collect();
        assert_eq!(years, ["2024", "2019"]);

        // Folders lead, in their own group when grouping.
        let mut with_folder = cards.clone();
        with_folder.push(Card { kind: "folder".into(), id: "f".into(), title: "Zzz".into(), ..Default::default() });
        assert_eq!(arrange_grid(&with_folder, "", "A-Z", "None")[0].1[0].title, "Zzz");
        assert_eq!(arrange_grid(&with_folder, "", "A-Z", "Title")[0].0, "Folders");
    }

    #[test]
    fn release_dates_are_spelled_out() {
        assert_eq!(long_date("2026-11-12"), "NOVEMBER 12, 2026");
        assert_eq!(long_date("2024-07-19T00:00:00.000+0000"), "JULY 19, 2024");
        assert_eq!(long_date(""), "");
    }

    #[test]
    fn same_page_is_recognised_for_history() {
        let card = |kind: &str, id: &str| Card { kind: kind.into(), id: id.into(), ..Default::default() };
        assert!(Page::Home.same_as(&Page::Home));
        assert!(!Page::Home.same_as(&Page::Tracks));
        assert!(Page::Detail(card("album", "1")).same_as(&Page::Detail(card("album", "1"))));
        assert!(!Page::Detail(card("album", "1")).same_as(&Page::Detail(card("album", "2"))));
        assert!(!Page::Detail(card("artist", "1")).same_as(&Page::ArtistTracks(card("artist", "1"))));
        assert!(!Page::Search("a".into()).same_as(&Page::Search("b".into())));
    }

    #[test]
    fn swipe_completes_past_halfway_or_on_a_flick() {
        assert!(swipe_commits(0.6, 0.0));
        assert!(swipe_commits(0.1, 900.0));
        // A short, slow pull and a pull that is being taken back both return.
        assert!(!swipe_commits(0.3, 100.0));
        assert!(!swipe_commits(0.7, -800.0));
        assert!(!swipe_commits(0.0, 2000.0));
    }

    #[test]
    fn pane_is_cut_out_of_the_window_snapshot() {
        let mut window = SharedPixelBuffer::<Rgba8Pixel>::new(8, 6);
        for (i, px) in window.make_mut_slice().iter_mut().enumerate() {
            px.r = i as u8;
        }
        let pane = crop_pane(&window, (2, 1, 4, 4)).unwrap();
        assert_eq!((pane.width(), pane.height()), (4, 4));
        assert_eq!(pane.as_slice()[0].r, 10);
        assert_eq!(pane.as_slice()[15].r, 37);
        // A pane that reaches past the window is clipped to it.
        assert_eq!(crop_pane(&window, (6, 4, 10, 10)).unwrap().width(), 2);
        assert!(crop_pane(&window, (8, 0, 4, 4)).is_none());
    }

    #[test]
    fn loads_display_defaults_from_existing_settings() {
        let meta = MetadataCache::in_memory().unwrap();
        assert!(Settings::load(&meta).rate_matching);
        assert!(Settings::load(&meta).restore_sample_rate);
        meta.put_json("settings", r#"{"quality":"high","rate_matching":false}"#).unwrap();

        let settings = Settings::load(&meta);
        assert_eq!(settings.quality, "high");
        assert!(!settings.rate_matching);
        // Missing field stays on, so older settings files keep handing the device back.
        assert!(settings.restore_sample_rate);
        assert!(settings.show_collection);
        assert!(settings.show_audio_metadata);

        meta.put_json("settings", r#"{"restore_sample_rate":false}"#).unwrap();
        assert!(!Settings::load(&meta).restore_sample_rate);
    }
}

pub struct Ctx {
    pub settings: RwLock<Settings>,
    /// A PKCE (Max-quality) login that has been started but not finished yet.
    pkce: tokio::sync::Mutex<Option<TidalService>>,
    /// Identifies the login attempt the redirect watcher belongs to.
    pkce_attempt: AtomicU64,
    pub svc: tokio::sync::Mutex<TidalService>,
    pub api: RwLock<Option<ApiHandle>>,
    pub meta: MetadataCache,
    pub audio: AudioCache,
    pub images: ImageCache,
    pub player: AudioPlayer,
    pub media: Mutex<Option<MacMediaControls>>,
    /// Bumped on every explicit track start so stale downloads don't start playing.
    pub play_gen: AtomicU64,
    /// Serializes replacement of the gapless queue with a prefetch completing off-thread.
    next_revision: Mutex<u64>,
    img_sem: tokio::sync::Semaphore,
    downloads: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Ctx {
    pub fn new(svc: TidalService) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let meta = MetadataCache::new()?;
        let settings = Settings::load(&meta);
        let audio = AudioCache::new()?;
        audio.set_max_bytes(gb_to_bytes(settings.cache_max_gb));
        let player = AudioPlayer::new()?;
        player.set_rate_matching(settings.rate_matching);
        player.set_restore_sample_rate(settings.restore_sample_rate);
        Ok(Arc::new(Self {
            settings: RwLock::new(settings),
            pkce: tokio::sync::Mutex::new(None),
            pkce_attempt: AtomicU64::new(0),
            svc: tokio::sync::Mutex::new(svc),
            api: RwLock::new(None),
            meta,
            audio,
            images: ImageCache::new()?,
            player,
            media: Mutex::new(None),
            play_gen: AtomicU64::new(0),
            next_revision: Mutex::new(0),
            img_sem: tokio::sync::Semaphore::new(8),
            downloads: Mutex::new(HashMap::new()),
        }))
    }

    pub fn api(&self) -> Option<ApiHandle> {
        self.api.read().unwrap().clone()
    }

    /// Local file for a track: straight from the SSD cache, else stream-and-save.
    pub async fn resolve_audio(&self, track_id: &str) -> Result<PathBuf, String> {
        self.resolve_audio_for(track_id, true).await
    }

    /// `for_playback` is false for background downloads, which must not count
    /// towards the cache hit rate.
    async fn resolve_audio_for(&self, track_id: &str, for_playback: bool) -> Result<PathBuf, String> {
        let lock = self.downloads.lock().unwrap().entry(track_id.to_string()).or_default().clone();
        let _one_download_per_track = lock.lock().await;
        let quality = self.settings.read().unwrap().quality.clone();
        let cache_key = quality.to_uppercase();
        let cached = self.audio.get_cached(track_id, &cache_key);
        if for_playback {
            let _ = self.meta.record_cache_lookup(track_id, cached.is_some());
        }
        let debug_gen = self.play_gen.load(Ordering::SeqCst);
        if let Some(path) = cached {
            tracing::info!(track_id, %cache_key, "Audio cache HIT: instant start from local SSD");
            if for_playback {
                show_chunks(debug_gen, "Cached on SSD".into(), "local file · 0 ms".into(), Vec::new(), false);
            }
            return Ok(path);
        }
        if for_playback {
            if let Some(path) = self.audio.get_playable(track_id, &cache_key) {
                tracing::info!(track_id, %cache_key, "Using in-progress partial cache for playback");
                return Ok(path);
            }
        }
        let t_api = std::time::Instant::now();
        let api = self.api().ok_or("offline and not cached")?;
        let stream = api.stream(track_id, &quality).await.map_err(|e| e.to_string())?;
        let api_ms = t_api.elapsed().as_millis();
        tracing::info!(
            track_id,
            quality = %stream.quality,
            bits = stream.bit_depth,
            rate = stream.sample_rate,
            segments = stream.urls.len(),
            api_ms,
            "Resolved stream manifest from Tidal API"
        );
        let n = stream.urls.len();
        let blocks = Arc::new(Mutex::new(vec![0i32; n]));
        let tier = stream.quality.clone();
        if for_playback {
            show_chunks(
                debug_gen,
                format!("Downloading · {tier}"),
                format!("0/{n} · api {api_ms} ms"),
                vec![0; n],
                true,
            );
        }
        let t_dl = std::time::Instant::now();
        let report_blocks = blocks.clone();
        let report_tier = tier.clone();
        let on_chunk: std::sync::Arc<dyn Fn(usize, u8, u64) + Send + Sync> = if for_playback {
            std::sync::Arc::new(move |idx: usize, state: u8, bytes: u64| {
                let snapshot = {
                    let mut g = report_blocks.lock().unwrap();
                    if let Some(slot) = g.get_mut(idx) {
                        *slot = i32::from(state);
                    }
                    g.clone()
                };
                let done = snapshot.iter().filter(|s| **s == 2).count();
                let total = snapshot.len();
                let mb = bytes as f64 / (1024.0 * 1024.0);
                let active = done < total;
                let status = if active {
                    format!("Streaming · {report_tier}")
                } else {
                    format!("Cached · {report_tier}")
                };
                show_chunks(debug_gen, status, format!("{done}/{total} · {mb:.1} MB"), snapshot, active);
            })
        } else {
            std::sync::Arc::new(|_, _, _| {})
        };
        let path = self
            .audio
            .fetch_or_cache_with(track_id, &cache_key, &stream.urls, for_playback, on_chunk)
            .await
            .map_err(|e| e.to_string());
        let dl_ms = t_dl.elapsed().as_millis();
        let path = match path {
            Ok(path) => path,
            Err(e) => {
                if for_playback {
                    show_chunks(debug_gen, format!("Failed · {e}"), format!("{dl_ms} ms"), blocks.lock().unwrap().clone(), false);
                }
                return Err(e);
            }
        };
        tracing::info!(track_id, dl_ms, streaming = path.extension().and_then(|s| s.to_str()) == Some("tmp"), "Audio ready for player");
        if for_playback && path.extension().and_then(|s| s.to_str()) != Some("tmp") {
            show_chunks(
                debug_gen,
                format!("Ready · {tier}"),
                format!("{n} segments · {dl_ms} ms"),
                blocks.lock().unwrap().clone(),
                false,
            );
        }
        self.trim_audio_cache(&[track_id.to_string()]);
        Ok(path)
    }

    /// Keep the audio cache under its limit, dropping the least played tracks first.
    pub fn trim_audio_cache(&self, also_keep: &[String]) {
        let mut protect = also_keep.to_vec();
        protect.extend(self.player.current_id());
        match self.audio.evict(&self.meta.play_scores(), &protect) {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "evicted least-played tracks from the audio cache"),
            Err(e) => tracing::warn!("audio cache eviction failed: {e}"),
        }
    }

    /// Tracks worth keeping offline that are currently missing from the cache.
    fn missing_most_played(&self) -> Vec<TrackItem> {
        let cache_key = self.settings.read().unwrap().quality.to_uppercase();
        self.meta
            .top_played(KEEP_OFFLINE_TOP, KEEP_OFFLINE_MIN_PLAYS)
            .into_iter()
            .filter(|t| self.audio.get_cached(&t.id, &cache_key).is_none())
            .collect()
    }

    fn with_media(&self, f: impl FnOnce(&mut MacMediaControls)) {
        if let Ok(mut guard) = self.media.lock() {
            if let Some(media) = guard.as_mut() {
                f(media);
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Page {
    Home,
    Tracks,
    Mixes,
    Playlists,
    Albums,
    Profiles,
    Folder { id: String, title: String },
    Detail(Card),
    ArtistTracks(Card),
    Search(String),
    Queue,
    Settings,
    Stub { key: String, title: String },
}

impl Page {
    /// Whether both are the same destination, so history need not hold it twice in a row.
    fn same_as(&self, other: &Page) -> bool {
        match (self, other) {
            (Page::Detail(a), Page::Detail(b)) | (Page::ArtistTracks(a), Page::ArtistTracks(b)) => {
                a.kind == b.kind && a.id == b.id
            }
            (Page::Folder { id: a, .. }, Page::Folder { id: b, .. }) => a == b,
            (Page::Search(a), Page::Search(b)) => a == b,
            (Page::Stub { key: a, .. }, Page::Stub { key: b, .. }) => a == b,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

#[derive(Default)]
struct State {
    history: Vec<Page>,
    hist_pos: usize,
    /// How far each history entry was scrolled when it was left (0 or negative).
    hist_scroll: Vec<f32>,
    /// Snapshots of pages near the current one, by history index. Back and
    /// forward transitions draw their panes from these.
    snaps: HashMap<usize, Image>,
    /// Incremented per page change; async page loads check it before applying.
    page_gen: u64,

    favorites: Vec<TrackItem>,
    fav_ids: HashSet<String>,
    sections: Vec<Section>,
    /// Per home section: card model, track model, and how many cards have artwork requested.
    section_models: Vec<(Rc<VecModel<CardData>>, Rc<VecModel<TrackData>>, usize)>,

    /// Tracks behind the table on screen (after filtering) and its model.
    page_tracks: Vec<TrackItem>,
    page_context: String,
    tracks_model: Option<Rc<VecModel<TrackData>>>,
    rows_requested: HashSet<usize>,
    sidebar_cards: Vec<Card>,
    /// The card grid on screen, as loaded; filter, sort and grouping are applied on top.
    grid: Grid,
    /// Album whose credits are loaded or loading.
    credits_for: String,
    /// Page and artist whose "more albums" and related artists were requested.
    extras_for: (u64, String),

    menu_track: Option<TrackItem>,
    /// The album, playlist, mix or artist the context menu was opened on.
    menu_card: Option<Card>,
    queue: Vec<TrackItem>,
    order: Vec<usize>,
    pos: usize,
    context: String,
    shuffle: bool,
    repeat: i32,
    loading_track: bool,
    /// Ticks since playback stopped; the output device is handed back after a moment.
    idle_ticks: u32,
    tick_count: u32,
    /// Source bitrate last shown in the signal path, to refresh it once the download finishes.
    shown_bitrate: u32,
    prefetched_for: Option<u64>,
    last_serial: u64,
    /// Saved position for a restored session before the audio file has been opened.
    pending_seek: Option<Duration>,
    volume_before_mute: f32,
    muted: bool,

    lyrics: Lyrics,
    lyrics_for: String,
    lyric_idx: Option<usize>,

    images: HashMap<String, (Image, Color)>,
    image_waiters: HashMap<String, Vec<Box<dyn FnOnce(&Image, Color)>>>,
    /// Artist whose hover card (bio + picture) is loaded or loading.
    hover_artist: String,
}

impl State {
    fn insert_next(&mut self, track: TrackItem) {
        // Keep the backing queue in list order, while inserting immediately
        // after the current track in the independent shuffled order.
        let insert_at = self.order[self.pos] + 1;
        self.queue.insert(insert_at, track);
        for index in &mut self.order {
            if *index >= insert_at {
                *index += 1;
            }
        }
        self.order.insert(self.pos + 1, insert_at);
        self.prefetched_for = None;
    }
}

pub struct Ui {
    win: MainWindow,
    ctx: Arc<Ctx>,
    state: RefCell<State>,
    nav: RefCell<Nav>,
    clipboard: RefCell<Option<arboard::Clipboard>>,
}

/// Snapshots are kept for pages at most this many steps from the current one.
const SNAP_REACH: usize = 2;
/// Back / forward from a button or shortcut (the 0.15 s page-motion token).
const NAV_DURATION: f32 = 0.15;
/// Cross-fade from the arrived pane's snapshot to the live page.
const NAV_FADE: f32 = 0.18;

#[derive(Clone, Copy, Default)]
enum NavPhase {
    #[default]
    Idle,
    /// Following the fingers.
    Tracking,
    /// Running to one end: 1 = the page change completes, 0 = it is called off.
    Settling { from: f32, to: f32, start: Instant, secs: f32, smooth: bool },
    /// Arrived; the snapshot stays up until the live page has its content.
    Waiting { since: Instant },
    Fading { start: Instant },
}

/// A back / forward page transition in progress.
#[derive(Default)]
struct Nav {
    phase: NavPhase,
    /// -1 back, +1 forward.
    delta: isize,
    /// Progress of the page change, 0..=1.
    p: f32,
    /// How far the swipe has pulled the pane, in logical pixels.
    travel: f32,
    samples: VecDeque<(Instant, f32)>,
    /// Snapshot of the page being left.
    outgoing: Option<Image>,
    /// Snapshot of the page being shown, when there is one.
    incoming: Option<Image>,
    /// The page change is decided but waits for the panes to land: building
    /// a page mid-flight would make the motion stutter.
    pending: bool,
    timer: slint::Timer,
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// Whether a released swipe completes the page change. `p` is how far the pane
/// was pulled (0..=1) and `velocity` its speed in px/s towards completion.
fn swipe_commits(p: f32, velocity: f32) -> bool {
    // Past halfway and not being pulled back, or a clear flick.
    (p > 0.5 && velocity > -300.0) || (p > 0.02 && velocity > 600.0)
}

/// Cut `(x, y, w, h)` out of a window snapshot.
fn crop_pane(buf: &SharedPixelBuffer<Rgba8Pixel>, rect: (u32, u32, u32, u32)) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let (x, y) = (rect.0.min(buf.width()), rect.1.min(buf.height()));
    let (w, h) = (rect.2.min(buf.width() - x), rect.3.min(buf.height() - y));
    if w == 0 || h == 0 {
        return None;
    }
    let src = buf.as_slice();
    let stride = buf.width() as usize;
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    for (row, line) in out.make_mut_slice().chunks_exact_mut(w as usize).enumerate() {
        let start = (y as usize + row) * stride + x as usize;
        line.copy_from_slice(&src[start..start + w as usize]);
    }
    Some(out)
}

/// Run `f` with the controller. Only for code already on the UI thread.
pub fn with_ui<R>(f: impl FnOnce(&Rc<Ui>) -> R) -> Option<R> {
    let ui = UI.with(|u| u.borrow().clone())?;
    Some(f(&ui))
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

/// Push the dev chunk grid. `blocks`: 0 queued, 1 in flight, 2 received.
/// `generation` must match the current `play_gen` or the update is dropped (prevents
/// prefetch / superseded downloads from stacking on the panel).
fn show_chunks(generation: u64, status: String, stats: String, blocks: Vec<i32>, active: bool) {
    on_ui(move |ui| {
        if ui.ctx.play_gen.load(Ordering::SeqCst) != generation {
            return;
        }
        let app = ui.app();
        app.set_debug_status(status.into());
        app.set_debug_stats(stats.into());
        app.set_debug_active(active);
        app.set_debug_blocks(ModelRc::new(VecModel::from(blocks)));
    });
}

/// Run `f` on the UI thread with the controller.
pub fn on_ui(f: impl FnOnce(&Rc<Ui>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = UI.with(|u| u.borrow().clone()) {
            f(&ui);
        }
    });
}

/// A card grid page: what was loaded, and how it is filtered, sorted and grouped.
#[derive(Default)]
struct Grid {
    /// mixes | playlists | albums | profiles — picks the options and the saved preferences.
    kind: String,
    all: Vec<Card>,
    filter: String,
    sort: String,
    group: String,
}

/// Sort and group choices for a grid page; the first of each is the default.
fn grid_options(kind: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match kind {
        "albums" => (&["Date added", "A-Z", "Artist", "Release date"], &["None", "Artist", "Year", "Title"]),
        "playlists" => (&["A-Z", "Date added", "Creator"], &["None", "Creator", "Title"]),
        _ => (&["Date added", "A-Z"], &["None", "Title"]),
    }
}

/// Filter, sort and split cards into titled groups (one untitled group for "None").
/// Folders stay ahead of playlists, as in the official client.
fn arrange_grid(cards: &[Card], filter: &str, sort: &str, group: &str) -> Vec<(String, Vec<Card>)> {
    let needle = filter.trim().to_lowercase();
    let mut cards: Vec<Card> = cards
        .iter()
        .filter(|c| {
            needle.is_empty()
                || c.title.to_lowercase().contains(&needle)
                || c.subtitle.to_lowercase().contains(&needle)
                || c.by.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect();
    let title = |c: &Card| c.title.to_lowercase();
    match sort {
        "A-Z" => cards.sort_by_key(title),
        "Artist" | "Creator" => cards.sort_by_key(|c| (c.by.to_lowercase(), title(c))),
        "Date added" => cards.sort_by(|a, b| b.added.cmp(&a.added)),
        "Release date" => cards.sort_by(|a, b| b.released.cmp(&a.released)),
        _ => {}
    }
    cards.sort_by_key(|c| c.kind != "folder");
    if group == "None" || group.is_empty() {
        return vec![(String::new(), cards)];
    }
    let name = |c: &Card| -> String {
        if c.kind == "folder" {
            return "Folders".into();
        }
        match group {
            "Year" => c.released.get(..4).filter(|y| !y.is_empty()).unwrap_or("Unknown year").to_string(),
            "Title" => match c.title.trim().chars().next() {
                Some(first) if first.is_alphabetic() => first.to_uppercase().collect(),
                _ => "#".into(),
            },
            _ if c.by.is_empty() => "Unknown".into(),
            _ => c.by.clone(),
        }
    };
    let mut groups: Vec<(String, Vec<Card>)> = Vec::new();
    for card in cards {
        let key = name(&card);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(card),
            None => groups.push((key, vec![card])),
        }
    }
    // Folders and the user's own playlists lead; "#" and unknowns trail.
    let rank = |k: &str| match k {
        "Folders" => 0,
        "Me" if group == "Creator" => 1,
        "#" | "Unknown" | "Unknown year" => 3,
        _ => 2,
    };
    groups.sort_by(|(a, _), (b, _)| {
        let by_name = if group == "Year" { b.cmp(a) } else { a.to_lowercase().cmp(&b.to_lowercase()) };
        rank(a).cmp(&rank(b)).then(by_name)
    });
    groups
}

/// "2026-11-12" -> "NOVEMBER 12, 2026".
fn long_date(iso: &str) -> String {
    const MONTHS: [&str; 12] = [
        "JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY", "AUGUST", "SEPTEMBER", "OCTOBER", "NOVEMBER",
        "DECEMBER",
    ];
    let mut parts = iso.get(..10).unwrap_or("").split('-').filter_map(|p| p.parse::<usize>().ok());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m @ 1..=12), Some(d)) => format!("{} {d}, {y}", MONTHS[m - 1]),
        _ => String::new(),
    }
}

fn fmt_time(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn track_row(t: &TrackItem, favorite: bool) -> TrackData {
    TrackData {
        id: t.id.as_str().into(),
        title: t.title.as_str().into(),
        artist: t.artist.as_str().into(),
        artist_id: t.artist_id.as_deref().unwrap_or_default().into(),
        album: t.album.as_str().into(),
        album_id: t.album_id.as_deref().unwrap_or_default().into(),
        cover_url: t.cover_url.as_deref().unwrap_or_default().into(),
        duration: fmt_time(t.duration_secs as u64).into(),
        date_added: t.date_added.clone().unwrap_or_default().into(),
        bpm: t.bpm.map_or("-".to_string(), |b| b.to_string()).into(),
        explicit: t.explicit,
        favorite,
        cover: Image::default(),
    }
}

fn card_row(c: &Card) -> CardData {
    CardData {
        id: c.id.as_str().into(),
        kind: c.kind.as_str().into(),
        title: c.title.as_str().into(),
        subtitle: c.subtitle.as_str().into(),
        quality: c.quality.as_str().into(),
        image: Image::default(),
        image_url: c.image_url.as_deref().unwrap_or_default().into(),
        by: c.by.as_str().into(),
    }
}

fn card_from_row(card: &CardData) -> Card {
    Card {
        kind: card.kind.to_string(),
        id: card.id.to_string(),
        title: card.title.to_string(),
        subtitle: card.subtitle.to_string(),
        quality: card.quality.to_string(),
        image_url: (!card.image_url.is_empty()).then(|| card.image_url.to_string()),
        by: card.by.to_string(),
        ..Default::default()
    }
}

/// Whether the collection toggle applies: albums, and playlists known to be someone else's.
fn can_favorite(card: &Card) -> bool {
    card.kind == "album" || (card.kind == "playlist" && !card.by.is_empty() && card.by != "Me")
}

/// The `favorites/ids` key for a card kind.
fn favorite_kind(kind: &str) -> &'static str {
    match kind {
        "album" => "ALBUM",
        "playlist" => "PLAYLIST",
        _ => "ARTIST",
    }
}

async fn set_card_favorite(api: &ApiHandle, card: &Card, favorite: bool) -> Result<(), tide_api::ApiError> {
    match card.kind.as_str() {
        "album" => api.set_album_favorite(&card.id, favorite).await,
        "playlist" => api.set_playlist_favorite(&card.id, favorite).await,
        _ => api.set_artist_followed(&card.id, favorite).await,
    }
}

/// Plain-text teaser of an artist biography: markup removed, cut to roughly three lines.
fn short_bio(bio: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in bio.replace("<br/>", " ").replace("<br>", " ").chars() {
        match c {
            '[' | '<' => in_tag = true,
            ']' | '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 170 {
        let cut: String = out.chars().take(167).collect();
        format!("{}…", cut.trim_end())
    } else {
        out
    }
}

fn sized_cover(url: &str, size: u32) -> String {
    // Track covers are stored as 160px URLs; swap the size segment for larger art.
    url.replace("/160x160.jpg", &format!("/{size}x{size}.jpg"))
}

fn url_key(url: &str, max_px: u32) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("{:016x}_{max_px}", h.finish())
}

/// Download (or read from the artwork cache) and decode off the UI thread.
async fn fetch_image(ctx: &Ctx, url: &str, max_px: u32) -> Option<(SharedPixelBuffer<Rgba8Pixel>, [u8; 3])> {
    let _permit = ctx.img_sem.acquire().await.ok()?;
    let path = ctx.images.get_or_download(&url_key(url, 0), url).await.inspect_err(|e| tracing::warn!("artwork download failed: {e}")).ok()?;
    tokio::task::spawn_blocking(move || {
        // Cached files are named .jpg regardless of content, so sniff the real format.
        let mut img = image::ImageReader::open(&path)
            .and_then(|r| r.with_guessed_format())
            .map_err(image::ImageError::IoError)
            .and_then(|r| r.decode())
            .inspect_err(|e| tracing::warn!("artwork decode failed for {}: {e}", path.display()))
            .ok()?;
        if img.width() > max_px || img.height() > max_px {
            img = img.thumbnail(max_px, max_px);
        }
        let rgba = img.to_rgba8();
        let (mut sum, mut n) = ([0u64; 3], 0u64);
        for p in rgba.pixels().step_by(37) {
            for c in 0..3 {
                sum[c] += p[c] as u64;
            }
            n += 1;
        }
        let avg = [0, 1, 2].map(|c| (sum[c] / n.max(1)) as u8);
        Some((SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()), avg))
    })
    .await
    .ok()
    .flatten()
}

fn shuffled(len: usize, first: usize) -> Vec<usize> {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9e3779b97f4a7c15, |d| d.as_nanos() as u64)
        | 1;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut order: Vec<usize> = (0..len).filter(|i| *i != first).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, (next() % (i as u64 + 1)) as usize);
    }
    if first < len {
        order.insert(0, first);
    }
    order
}

impl Ui {
    pub fn install(win: &MainWindow, ctx: Arc<Ctx>) -> Rc<Self> {
        let ui = Rc::new(Self {
            win: win.clone_strong(),
            ctx,
            state: RefCell::new(State { volume_before_mute: 1.0, ..Default::default() }),
            nav: RefCell::default(),
            clipboard: RefCell::default(),
        });
        UI.with(|u| *u.borrow_mut() = Some(ui.clone()));
        ui.wire();
        ui
    }

    pub(crate) fn app(&self) -> App<'_> {
        self.win.global::<App>()
    }

    fn spawn<F>(&self, fut: impl FnOnce(Arc<Ctx>) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(fut(self.ctx.clone()));
    }

    fn wire(self: &Rc<Self>) {
        let app = self.app();
        macro_rules! bind {
            ($setter:ident, |$ui:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(self);
                app.$setter(move |$($arg),*| {
                    if let Some($ui) = weak.upgrade() {
                        $body
                    }
                });
            }};
        }
        bind!(on_navigate, |ui, key| ui.navigate_key(&key));
        bind!(on_open_card, |ui, card| ui.open_card(card));
        bind!(on_open_track_artist, |ui, track| ui.open_card(CardData {
            kind: "artist".into(), id: track.artist_id, title: track.artist, ..Default::default()
        }));
        bind!(on_open_track_album, |ui, track| ui.open_card(CardData {
            kind: "album".into(), id: track.album_id, title: track.album,
            subtitle: track.artist, image_url: track.cover_url, ..Default::default()
        }));
        bind!(on_show_artist_tracks, |ui| {
            let current = {
                let st = ui.state.borrow();
                st.history.get(st.hist_pos).cloned()
            };
            if let Some(Page::Detail(card)) = current {
                ui.show(Page::ArtistTracks(card), true);
            }
        });
        bind!(on_artist_release_art, |ui, singles, from, upto| {
            ui.artist_release_art(singles, from.max(0) as usize, upto.max(0) as usize);
        });
        bind!(on_sort_playlists, |ui| {
            let app = ui.app();
            app.set_sidebar_ascending(!app.get_sidebar_ascending());
            ui.present_sidebar();
        });
        bind!(on_set_grid_sort, |ui, sort| ui.set_grid_arrangement(Some(sort.to_string()), None));
        bind!(on_set_grid_group, |ui, group| ui.set_grid_arrangement(None, Some(group.to_string())));
        bind!(on_create_in_collection, |ui, kind, name| ui.create_in_collection(kind == "folder", name.trim()));
        bind!(on_toggle_page_favorite, |ui| ui.toggle_page_favorite());
        bind!(on_page_action, |ui, action, arg| ui.page_action(&action, &arg));
        bind!(on_related_art, |ui, from, upto| {
            ui.card_art(ui.app().get_related_artists(), from.max(0) as usize, upto.max(0) as usize);
        });
        bind!(on_back, |ui| ui.step_history(-1));
        bind!(on_forward, |ui| ui.step_history(1));
        bind!(on_np_artist_hovered, |ui| ui.load_artist_hover_card());
        bind!(on_toggle_follow_artist, |ui| ui.toggle_follow_artist());
        bind!(on_open_now_playing_artist, |ui| {
            if let Some(track) = ui.current_track() {
                if let Some(id) = track.artist_id {
                    ui.open_card(CardData {
                        kind: "artist".into(), id: id.into(), title: track.artist.into(), ..Default::default()
                    });
                }
            }
        });
        bind!(on_search, |ui, q| {
            let q = q.trim().to_string();
            if !q.is_empty() {
                ui.show(Page::Search(q), true);
            }
        });
        bind!(on_filter, |ui, text| ui.apply_filter(&text));
        bind!(on_rows_visible, |ui, first, count| ui.rows_visible(first.max(0) as usize, count.max(0) as usize));
        bind!(on_request_track_menu, |ui, id, x, y| ui.open_track_menu(&id, x, y));
        bind!(on_request_card_menu, |ui, card, x, y| ui.open_card_menu(card, x, y));
        bind!(on_menu_action, |ui, action, arg| ui.menu_action(&action, &arg));
        bind!(on_play_track_next, |ui, id| ui.play_track_next(&id));
        bind!(on_copy_track_link, |ui, id| {
            if let Err(error) = ui.copy_track_link(&id) {
                tracing::warn!("Could not copy track link: {error}");
            }
        });
        bind!(on_play_track, |ui, id| ui.play_from_page(&id));
        bind!(on_play_section_track, |ui, section, id| ui.play_from_section(section.max(0) as usize, &id));
        bind!(on_play_all, |ui, shuffle| ui.play_all(shuffle));
        bind!(on_toggle_play, |ui| ui.toggle_play());
        bind!(on_next, |ui| ui.skip(1, true));
        bind!(on_prev, |ui| ui.previous());
        bind!(on_seek, |ui, fraction| ui.seek(fraction));
        bind!(on_set_volume, |ui, v| ui.set_volume(v));
        bind!(on_toggle_mute, |ui| ui.toggle_mute());
        bind!(on_toggle_shuffle, |ui| ui.toggle_shuffle());
        bind!(on_cycle_repeat, |ui| ui.cycle_repeat());
        bind!(on_toggle_favorite, |ui, id| ui.toggle_favorite(&id));
        bind!(on_open_now_playing, |ui| ui.toggle_now_playing());
        bind!(on_login, |ui| ui.login());
        bind!(on_open_browser, |ui| {
            let url = ui.app().get_verification_url().to_string();
            if !url.is_empty() {
                let _ = open::that(url);
            }
        });
        bind!(on_logout, |ui| ui.logout());
        bind!(on_drag_window, |ui| crate::platform::drag_window(&ui.win));
        bind!(on_minimize, |ui| ui.win.window().set_minimized(true));
        bind!(on_close_window, |ui| {
            let _ = ui.win.hide();
        });
        app.on_open_help(|| {
            let _ = open::that("https://support.tidal.com/");
        });
        bind!(on_section_visible, |ui, index, upto| ui.section_visible(index.max(0) as usize, upto.max(0) as usize));
        bind!(on_set_quality, |ui, quality| ui.update_settings(|s| s.quality = quality.to_string()));
        bind!(on_set_show_collection, |ui, on| ui.update_settings(|s| s.show_collection = on));
        bind!(on_set_show_audio_metadata, |ui, on| ui.update_settings(|s| s.show_audio_metadata = on));
        bind!(on_clear_cache, |ui| {
            if let Err(e) = ui.ctx.audio.clear() {
                tracing::warn!("could not clear the audio cache: {e}");
            }
            ui.present_settings();
        });
        bind!(on_pkce_start, |ui| ui.pkce_start(None));
        bind!(on_pkce_start_safari, |ui| ui.pkce_start(Some(crate::platform::SAFARI)));
        bind!(on_set_cache_max, |ui, text| ui.set_cache_max(&text));
        bind!(on_set_rate_matching, |ui, on| {
            ui.update_settings(|s| s.rate_matching = on);
            // Re-opening the device can take a moment: keep it off the UI thread.
            ui.spawn(|ctx| async move {
                let _ = tokio::task::spawn_blocking(move || ctx.player.set_rate_matching(on)).await;
                on_ui(|ui| ui.present_signal());
            });
        });
        bind!(on_set_restore_sample_rate, |ui, on| {
            ui.update_settings(|s| s.restore_sample_rate = on);
            ui.spawn(|ctx| async move {
                let _ = tokio::task::spawn_blocking(move || ctx.player.set_restore_sample_rate(on)).await;
                on_ui(|ui| ui.present_signal());
            });
        });
        bind!(on_set_keep_most_played, |ui, on| ui.update_settings(|s| s.keep_most_played = on));
        bind!(on_pkce_finish, |ui, url| ui.pkce_finish(url.trim().to_string()));
    }

    // ------------------------------------------------------------ boot

    /// Paint everything we have on disk, then refresh from the network.
    pub fn boot(self: &Rc<Self>, has_session: bool) {
        let app = self.app();
        app.set_logged_in(has_session);
        app.set_quality_label("Max".into());
        app.set_app_version(env!("CARGO_PKG_VERSION").into());
        self.present_settings();
        self.present_signal();
        if !has_session {
            return;
        }
        self.restore_cached();
        self.show(Page::Home, true);
        self.spawn(|ctx| async move {
            let (ok, api, username, max_login) = {
                let mut svc = ctx.svc.lock().await;
                let ok = svc.load_session().await.unwrap_or(false);
                (ok, svc.handle(), svc.username(), svc.is_pkce())
            };
            if !ok {
                tracing::warn!("Could not refresh the session; running from local cache only");
            }
            *ctx.api.write().unwrap() = if ok { api } else { None };
            on_ui(move |ui| {
                ui.app().set_username(username.unwrap_or_else(|| "TIDAL".into()).into());
                ui.app().set_max_login(max_login);
                if ok {
                    ui.sync_library();
                }
            });
        });
    }

    fn restore_cached(self: &Rc<Self>) {
        fn load<T: serde::de::DeserializeOwned>(meta: &MetadataCache, key: &str) -> Option<T> {
            serde_json::from_str(&meta.get_json(key)?).ok()
        }
        if let Some(favs) = load::<Vec<TrackItem>>(&self.ctx.meta, "favorites") {
            self.set_favorites(favs);
        }
        if let Some(sections) = load::<Vec<Section>>(&self.ctx.meta, "home") {
            self.set_sections(sections);
        }
        if let Some(cards) = load::<Vec<Card>>(&self.ctx.meta, "cards:playlists") {
            self.set_sidebar(&cards);
        }
        self.restore_playback();
    }

    fn restore_playback(self: &Rc<Self>) {
        let Ok(Some(saved)) = self.ctx.meta.load_playback_state() else { return };
        let app = self.app();
        self.ctx.player.set_volume(if saved.muted { 0.0 } else { saved.volume });
        app.set_volume(saved.volume);
        app.set_muted(saved.muted);
        app.set_shuffle(saved.shuffle);
        app.set_repeat_mode(saved.repeat_mode);

        let elapsed = Duration::from_millis(saved.elapsed_ms);
        let has_queue = !saved.queue.is_empty() && !saved.order.is_empty();
        {
            let mut st = self.state.borrow_mut();
            st.volume_before_mute = saved.volume;
            st.muted = saved.muted;
            st.shuffle = saved.shuffle;
            st.repeat = saved.repeat_mode;
            if has_queue {
                st.queue = saved.queue;
                st.order = saved.order;
                st.pos = saved.pos;
                st.context = saved.context;
                st.pending_seek = (!elapsed.is_zero()).then_some(elapsed);
            }
        }
        self.present_signal();
        if !has_queue {
            return;
        }
        self.present_current();
        if let Some(track) = self.current_track() {
            let total_secs = track.duration_secs.max(1) as f32;
            let clamped = elapsed.min(Duration::from_secs(track.duration_secs as u64));
            let fraction = (clamped.as_secs_f32() / total_secs).clamp(0.0, 1.0);
            app.set_elapsed(fmt_time(clamped.as_secs()).into());
            app.set_progress(fraction);
            self.ctx.with_media(|m| {
                let _ = m.set_playback_state(false, Some(clamped));
            });
        }
    }

    /// Pull home feed, playlists and the complete favourites list.
    fn sync_library(self: &Rc<Self>) {
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.home().await {
                Ok(sections) => {
                    let _ = ctx.meta.put_json("home", &serde_json::to_string(&sections).unwrap_or_default());
                    on_ui(move |ui| ui.set_sections(sections));
                }
                Err(e) => tracing::warn!("home feed failed: {e}"),
            }
        });
        self.sync_playlists();
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else { return };
            let mut all: Vec<TrackItem> = Vec::new();
            loop {
                match api.favorite_tracks(all.len() as u32, 100).await {
                    Ok((page, total)) => {
                        let done = page.is_empty();
                        all.extend(page);
                        let first_page = all.len() <= 100;
                        let finished = done || all.len() as u32 >= total || all.len() >= 10_000;
                        // Paint early, then again once everything is in.
                        if first_page || finished {
                            let snapshot = all.clone();
                            on_ui(move |ui| ui.set_favorites(snapshot));
                        }
                        if finished {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("favorites failed: {e}");
                        return;
                    }
                }
            }
            let _ = ctx.meta.save_favorites(&all);
            let _ = ctx.meta.put_json("favorites", &serde_json::to_string(&all).unwrap_or_default());
        });
    }

    /// Refresh the sidebar's playlists and folders.
    fn sync_playlists(self: &Rc<Self>) {
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.playlists("root").await {
                Ok(cards) => {
                    let _ = ctx.meta.put_json("cards:playlists", &serde_json::to_string(&cards).unwrap_or_default());
                    on_ui(move |ui| ui.set_sidebar(&cards));
                }
                Err(e) => tracing::warn!("playlists failed: {e}"),
            }
        });
    }

    // ------------------------------------------------------------ images

    fn load_image(self: &Rc<Self>, url: &str, max_px: u32, apply: impl FnOnce(&Image, Color) + 'static) {
        let key = url_key(url, max_px);
        if std::env::var_os("TIDE_NO_ART").is_some() {
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            if let Some((img, tint)) = st.images.get(&key).cloned() {
                drop(st);
                apply(&img, tint);
                return;
            }
            let waiters = st.image_waiters.entry(key.clone()).or_default();
            waiters.push(Box::new(apply));
            if waiters.len() > 1 {
                return;
            }
        }
        let url = url.to_string();
        self.spawn(|ctx| async move {
            let decoded = fetch_image(&ctx, &url, max_px).await;
            on_ui(move |ui| {
                let waiters = ui.state.borrow_mut().image_waiters.remove(&key).unwrap_or_default();
                let Some((buffer, avg)) = decoded else { return };
                let img = Image::from_rgba8(buffer);
                let tint = Color::from_rgb_u8(avg[0], avg[1], avg[2]);
                {
                    let mut st = ui.state.borrow_mut();
                    // The models keep what is on screen alive; this is only a reuse cache.
                    if st.images.len() > 250 {
                        st.images.clear();
                    }
                    st.images.insert(key, (img.clone(), tint));
                }
                for waiter in waiters {
                    waiter(&img, tint);
                }
            });
        });
    }

    fn cards_model(self: &Rc<Self>, cards: &[Card], max_px: u32) -> Rc<VecModel<CardData>> {
        let model = Rc::new(VecModel::from(cards.iter().map(card_row).collect::<Vec<_>>()));
        for (i, card) in cards.iter().enumerate() {
            let Some(url) = &card.image_url else { continue };
            let (model, id) = (model.clone(), card.id.clone());
            self.load_image(url, max_px, move |img, _| {
                if let Some(mut row) = model.row_data(i).filter(|r| r.id == id.as_str()) {
                    row.image = img.clone();
                    model.set_row_data(i, row);
                }
            });
        }
        model
    }

    fn load_track_cover(self: &Rc<Self>, model: &Rc<VecModel<TrackData>>, i: usize, track: &TrackItem, max_px: u32) {
        let Some(url) = &track.cover_url else { return };
        let (model, id) = (model.clone(), track.id.clone());
        self.load_image(url, max_px, move |img, _| {
            if let Some(mut row) = model.row_data(i).filter(|r| r.id == id.as_str()) {
                row.cover = img.clone();
                model.set_row_data(i, row);
            }
        });
    }

    // ------------------------------------------------------------ content

    fn set_favorites(self: &Rc<Self>, favorites: Vec<TrackItem>) {
        let on_tracks_page = {
            let mut st = self.state.borrow_mut();
            st.fav_ids = favorites.iter().map(|t| t.id.clone()).collect();
            st.favorites = favorites;
            matches!(st.history.get(st.hist_pos), Some(Page::Tracks))
        };
        if on_tracks_page {
            let filter = self.app().get_filter_text().to_string();
            self.apply_filter(&filter);
        }
        self.refresh_now_playing_favorite();
    }

    fn set_sections(self: &Rc<Self>, sections: Vec<Section>) {
        let fav_ids = self.state.borrow().fav_ids.clone();
        let mut models = Vec::with_capacity(sections.len());
        let rows: Vec<SectionData> = sections
            .iter()
            .map(|s| {
                let rows_layout = s.layout == "cards" && s.title.eq_ignore_ascii_case("Recently played");
                // Artwork is requested later, when the section scrolls near the viewport.
                let cards = Rc::new(VecModel::from(s.cards.iter().map(card_row).collect::<Vec<_>>()));
                let tracks = Rc::new(VecModel::from(
                    s.tracks.iter().map(|t| track_row(t, fav_ids.contains(&t.id))).collect::<Vec<_>>(),
                ));
                models.push((cards.clone(), tracks.clone(), 0));
                SectionData {
                    title: s.title.as_str().into(),
                    layout: if rows_layout { "rows".into() } else { s.layout.as_str().into() },
                    cards: ModelRc::from(cards),
                    tracks: ModelRc::from(tracks),
                }
            })
            .collect();
        {
            let mut st = self.state.borrow_mut();
            st.sections = sections;
            st.section_models = models;
        }
        self.app().set_sections(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    /// Home section `index` is near the viewport: fetch artwork for its first `upto` cards.
    fn section_visible(self: &Rc<Self>, index: usize, upto: usize) {
        let (cards, tracks, card_model, track_model, from, small) = {
            let mut st = self.state.borrow_mut();
            let Some(section) = st.sections.get(index).cloned() else { return };
            let Some((card_model, track_model, loaded)) = st.section_models.get_mut(index) else { return };
            let from = *loaded;
            let upto = upto.min(section.cards.len());
            *loaded = upto.max(from);
            let small = section.layout == "shortcuts" || section.title.eq_ignore_ascii_case("Recently played");
            let tracks = if from == 0 { section.tracks.clone() } else { Vec::new() };
            let cards: Vec<(usize, Card)> =
                (from..upto.max(from)).map(|i| (i, section.cards[i].clone())).collect();
            (cards, tracks, card_model.clone(), track_model.clone(), from, small)
        };
        let _ = from;
        for (i, card) in cards {
            let Some(url) = card.image_url else { continue };
            let model = card_model.clone();
            self.load_image(&url, if small { 112 } else { 256 }, move |img, _| {
                if let Some(mut row) = model.row_data(i).filter(|r| r.id == card.id.as_str()) {
                    row.image = img.clone();
                    model.set_row_data(i, row);
                }
            });
        }
        for (i, track) in tracks.iter().enumerate() {
            self.load_track_cover(&track_model, i, track, 128);
        }
    }

    // ------------------------------------------------------------ settings

    fn present_settings(&self) {
        let app = self.app();
        let settings = self.ctx.settings.read().unwrap().clone();
        app.set_quality(settings.quality.into());
        app.set_show_collection(settings.show_collection);
        app.set_show_audio_metadata(settings.show_audio_metadata);
        app.set_keep_most_played(settings.keep_most_played);
        app.set_rate_matching(settings.rate_matching);
        app.set_restore_sample_rate(settings.restore_sample_rate);
        app.set_cache_max(format!("{}", settings.cache_max_gb).into());
        let (tracks, bytes) = self.ctx.audio.usage();
        let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let (hits, misses) = self.ctx.meta.cache_hit_stats();
        let hit_rate = if hits + misses == 0 {
            String::new()
        } else {
            format!(" · {:.0}% of plays came from the cache", hits as f64 * 100.0 / (hits + misses) as f64)
        };
        app.set_cache_info(
            format!(
                "{tracks} track{} · {gb:.2} GB of {} GB{hit_rate}",
                if tracks == 1 { "" } else { "s" },
                settings.cache_max_gb
            )
            .into(),
        );
    }

    fn update_settings(&self, change: impl FnOnce(&mut Settings)) {
        let (json, quality_changed) = {
            let mut settings = self.ctx.settings.write().unwrap();
            let old_quality = settings.quality.clone();
            change(&mut settings);
            (serde_json::to_string(&*settings).unwrap_or_default(), settings.quality != old_quality)
        };
        if let Err(e) = self.ctx.meta.put_json("settings", &json) {
            tracing::warn!("could not save settings: {e}");
        }
        // A pre-loaded next track may be in the old quality.
        if quality_changed {
            self.ctx.player.clear_queue();
            self.state.borrow_mut().prefetched_for = None;
        }
        self.present_settings();
    }

    /// Start the Max-quality login in the default browser (or `browser`, a
    /// bundle id) and wait for the browser to land on TIDAL's redirect address.
    fn pkce_start(self: &Rc<Self>, browser: Option<&'static str>) {
        let app = self.app();
        app.set_pkce_waiting(true);
        app.set_pkce_manual(false);
        app.set_pkce_offer_safari(false);
        app.set_pkce_status("Opening TIDAL in your browser…".into());
        let attempt = self.ctx.pkce_attempt.fetch_add(1, Ordering::SeqCst) + 1;
        self.spawn(|ctx| async move {
            let started = TidalService::new().and_then(|mut svc| svc.start_pkce_login().map(|url| (svc, url)));
            let (svc, url) = match started {
                Ok(started) => started,
                Err(e) => {
                    on_ui(move |ui| {
                        ui.app().set_pkce_waiting(false);
                        ui.app().set_pkce_status(format!("Couldn't start the login: {e}").into());
                    });
                    return;
                }
            };
            *ctx.pkce.lock().await = Some(svc);

            let default_browser = tokio::task::spawn_blocking(crate::platform::default_browser_id).await.ok().flatten();
            let browser_id = browser.map(String::from).or(default_browser).unwrap_or_default();
            let opened_elsewhere = browser.is_some_and(|b| crate::platform::open_in_browser(b, &url));
            if !opened_elsewhere {
                let _ = open::that(&url);
            }
            let mut scriptable = crate::platform::browser_is_scriptable(&browser_id);
            on_ui(move |ui| {
                let app = ui.app();
                if scriptable {
                    app.set_pkce_status("Sign in to TIDAL in your browser. TIDE continues by itself when you're done.".into());
                } else {
                    // This browser can't be asked for the address: copying it is enough.
                    app.set_pkce_status(
                        "Sign in to TIDAL in your browser. When you land on a page that doesn't load, copy its address (⌘L, then ⌘C) and TIDE continues by itself."
                            .into(),
                    );
                    app.set_pkce_manual(true);
                    app.set_pkce_offer_safari(true);
                }
            });

            // Watch for the redirect until it shows up, the attempt is replaced, or we give up.
            let mut clipboard_change = -1i64;
            let deadline = tokio::time::Instant::now() + LOGIN_WAIT;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if ctx.pkce_attempt.load(Ordering::SeqCst) != attempt {
                    return;
                }
                let id = browser_id.clone();
                let found = tokio::task::spawn_blocking(move || {
                    let from_browser = if scriptable { crate::platform::redirect_from_browser(&id) } else { Ok(None) };
                    let from_clipboard = crate::platform::redirect_from_clipboard(&mut clipboard_change);
                    (from_browser, from_clipboard, clipboard_change)
                })
                .await;
                let Ok((from_browser, from_clipboard, change)) = found else { continue };
                clipboard_change = change;
                let redirect = match from_browser {
                    Ok(url) => url.or(from_clipboard),
                    Err(e) => {
                        // Not allowed to ask the browser (or it failed): fall back to copy/paste.
                        tracing::warn!("cannot read the login address from the browser: {e}");
                        scriptable = false;
                        on_ui(|ui| {
                            let app = ui.app();
                            app.set_pkce_manual(true);
                            app.set_pkce_status(
                                "TIDE isn't allowed to read the address from your browser. Copy the address of the page you land on (⌘L, then ⌘C) and TIDE continues by itself."
                                    .into(),
                            );
                        });
                        from_clipboard
                    }
                };
                if let Some(redirect) = redirect {
                    if ctx.pkce_attempt.load(Ordering::SeqCst) == attempt {
                        on_ui(move |ui| ui.pkce_finish(redirect));
                    }
                    return;
                }
            }
            on_ui(|ui| {
                ui.app().set_pkce_waiting(false);
                ui.app().set_pkce_status("The login timed out. Press the button to try again.".into());
            });
        });
    }

    /// Step 2: exchange the pasted redirect address for a Hi-Res capable session.
    fn pkce_finish(self: &Rc<Self>, redirect_url: String) {
        if redirect_url.is_empty() {
            self.app().set_pkce_status("Paste the address from your browser first.".into());
            return;
        }
        // Stops the redirect watcher of this attempt.
        self.ctx.pkce_attempt.fetch_add(1, Ordering::SeqCst);
        self.app().set_pkce_status("Finishing login…".into());
        self.spawn(|ctx| async move {
            let Some(mut svc) = ctx.pkce.lock().await.take() else {
                on_ui(|ui| ui.app().set_pkce_status("Start the login first.".into()));
                return;
            };
            // The existing session is only replaced once the new one is saved.
            match svc.finish_pkce_login(&redirect_url).await {
                Ok(username) => {
                    *ctx.api.write().unwrap() = svc.handle();
                    *ctx.svc.lock().await = svc;
                    // Cached files were fetched at the old quality cap.
                    let _ = ctx.audio.clear();
                    on_ui(move |ui| {
                        let app = ui.app();
                        app.set_max_login(true);
                        app.set_username(username.into());
                        app.set_pkce_waiting(false);
                        app.set_pkce_manual(false);
                        app.set_pkce_offer_safari(false);
                        app.set_pkce_status("Max quality is enabled. New plays stream in Hi-Res where available.".into());
                        ui.present_settings();
                        if app.get_logged_in() {
                            ui.sync_library();
                        } else {
                            ui.boot(true);
                        }
                    });
                }
                Err(e) => {
                    // Keep the attempt so the user can paste again.
                    *ctx.pkce.lock().await = Some(svc);
                    on_ui(move |ui| {
                        let app = ui.app();
                        app.set_pkce_waiting(false);
                        app.set_pkce_manual(true);
                        app.set_pkce_status(format!("Login failed: {e}. Press the button to try again.").into());
                    });
                }
            }
        });
    }

    /// Change the cache size limit from the Settings box (in GB).
    fn set_cache_max(self: &Rc<Self>, text: &str) {
        let parsed = text.trim().trim_end_matches(|c: char| c.is_alphabetic() || c == ' ').replace(',', ".").parse::<f64>();
        if let Ok(gb) = parsed.map(|gb| (gb.clamp(0.5, 2000.0) * 10.0).round() / 10.0) {
            self.ctx.audio.set_max_bytes(gb_to_bytes(gb));
            self.update_settings(|s| s.cache_max_gb = gb);
            // A smaller limit takes effect right away.
            self.ctx.trim_audio_cache(&[]);
        }
        self.present_settings();
    }

    /// Every so often, re-download the most played tracks that are no longer
    /// in the cache (after a clear, a quality change or an eviction), so the
    /// music played most is the music that starts instantly and works offline.
    pub fn start_offline_keeper(self: &Rc<Self>) {
        self.spawn(|ctx| async move {
            tokio::time::sleep(Duration::from_secs(45)).await;
            loop {
                let enabled = ctx.settings.read().unwrap().keep_most_played;
                if enabled && ctx.api().is_some() {
                    // A few per round keeps this gentle on the connection.
                    for track in ctx.missing_most_played().into_iter().take(5) {
                        if !ctx.settings.read().unwrap().keep_most_played {
                            break;
                        }
                        match ctx.resolve_audio_for(&track.id, false).await {
                            Ok(_) => tracing::info!(title = %track.title, "kept a most-played track offline"),
                            Err(e) => tracing::warn!("could not keep {} offline: {e}", track.title),
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(15 * 60)).await;
            }
        });
    }

    fn set_sidebar(self: &Rc<Self>, cards: &[Card]) {
        self.state.borrow_mut().sidebar_cards = cards.to_vec();
        let own: Vec<CardData> = cards.iter().filter(|c| c.kind == "playlist" && c.by == "Me").map(card_row).collect();
        self.app().set_own_playlists(ModelRc::new(VecModel::from(own)));
        self.present_sidebar();
    }

    fn present_sidebar(self: &Rc<Self>) {
        let app = self.app();
        let mut cards = self.state.borrow().sidebar_cards.clone();
        if !app.get_sidebar_ascending() {
            // Reverse within each group, with folders still above playlists.
            cards.reverse();
            cards.sort_by_key(|card| card.kind != "folder");
        }
        app.set_sidebar_playlists(ModelRc::from(self.cards_model(&cards, 96)));
    }

    fn set_grid(self: &Rc<Self>, cards: &[Card]) {
        self.state.borrow_mut().grid.all = cards.to_vec();
        self.present_grid();
    }

    /// Draw the grid from what was loaded, with the current filter, sort and grouping.
    fn present_grid(self: &Rc<Self>) {
        let groups = {
            let st = self.state.borrow();
            arrange_grid(&st.grid.all, &st.grid.filter, &st.grid.sort, &st.grid.group)
        };
        let flat: Vec<CardData> = groups.iter().flat_map(|(_, cards)| cards.iter().map(card_row)).collect();
        let sections: Vec<SectionData> = groups
            .iter()
            .map(|(title, cards)| SectionData {
                title: title.as_str().into(),
                layout: "cards".into(),
                cards: ModelRc::from(self.cards_model(cards, 320)),
                tracks: ModelRc::default(),
            })
            .collect();
        let app = self.app();
        app.set_grid_cards(ModelRc::new(VecModel::from(flat)));
        app.set_grid_groups(ModelRc::new(VecModel::from(sections)));
    }

    /// Change the sort and / or grouping of the grid on screen and remember it for that page kind.
    fn set_grid_arrangement(self: &Rc<Self>, sort: Option<String>, group: Option<String>) {
        let (kind, sort, group) = {
            let mut st = self.state.borrow_mut();
            if let Some(sort) = sort {
                st.grid.sort = sort;
            }
            if let Some(group) = group {
                st.grid.group = group;
            }
            (st.grid.kind.clone(), st.grid.sort.clone(), st.grid.group.clone())
        };
        let _ = self.ctx.meta.put_json(&format!("grid-prefs:{kind}"), &serde_json::json!([sort, group]).to_string());
        let app = self.app();
        app.set_grid_sort(sort.into());
        app.set_grid_group(group.into());
        self.present_grid();
    }

    /// Create an empty playlist or folder, inside the folder on screen if there is one.
    fn create_in_collection(self: &Rc<Self>, folder: bool, name: &str) {
        if name.is_empty() {
            return;
        }
        let current = {
            let st = self.state.borrow();
            st.history.get(st.hist_pos).cloned()
        };
        let parent = match &current {
            Some(Page::Folder { id, .. }) => id.clone(),
            _ => "root".to_string(),
        };
        let name = name.to_string();
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            let what = if folder { "folder" } else { "playlist" };
            match api.create_in_collection(&name, &parent, folder).await {
                Ok(()) => on_ui(move |ui| {
                    crate::alerts::notice(&ui.app(), &format!("Created {what} “{name}”"));
                    ui.sync_playlists();
                    // Reload the playlists page if that is where we are.
                    let current = {
                        let st = ui.state.borrow();
                        st.history.get(st.hist_pos).cloned()
                    };
                    if let Some(page @ (Page::Playlists | Page::Folder { .. })) = current {
                        ui.show(page, false);
                    }
                }),
                Err(e) => tracing::error!("Could not create {what} “{name}”: {e}"),
            }
        });
    }

    /// Replace the track table.
    fn set_tracks(self: &Rc<Self>, tracks: Vec<TrackItem>, context: &str) {
        let model = {
            let mut st = self.state.borrow_mut();
            let rows: Vec<TrackData> = tracks.iter().map(|t| track_row(t, st.fav_ids.contains(&t.id))).collect();
            let model = Rc::new(VecModel::from(rows));
            st.page_tracks = tracks;
            st.page_context = context.to_string();
            st.tracks_model = Some(model.clone());
            st.rows_requested.clear();
            model
        };
        self.app().set_tracks(ModelRc::from(model));
        let count = if self.app().get_view() == "artist" { 4 } else { 30 };
        self.rows_visible(0, count);
    }

    /// Lazily fetch artwork for the rows on screen (plus a little look-ahead).
    fn rows_visible(self: &Rc<Self>, first: usize, count: usize) {
        let wanted: Vec<(usize, TrackItem, Rc<VecModel<TrackData>>)> = {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            let Some(model) = st.tracks_model.clone() else { return };
            let end = if self.app().get_view() == "artist" {
                st.page_tracks.len().min(4)
            } else {
                (first + count + 12).min(st.page_tracks.len())
            };
            (first.saturating_sub(6)..end)
                .filter(|i| st.rows_requested.insert(*i))
                .map(|i| (i, st.page_tracks[i].clone(), model.clone()))
                .collect()
        };
        for (i, track, model) in wanted {
            self.load_track_cover(&model, i, &track, 96);
        }
    }

    fn apply_filter(self: &Rc<Self>, text: &str) {
        if self.app().get_view() == "grid" {
            self.state.borrow_mut().grid.filter = text.to_string();
            self.present_grid();
            return;
        }
        let needle = text.trim().to_lowercase();
        let tracks: Vec<TrackItem> = {
            let st = self.state.borrow();
            if !matches!(st.history.get(st.hist_pos), Some(Page::Tracks)) {
                return;
            }
            st.favorites
                .iter()
                .filter(|t| {
                    needle.is_empty()
                        || t.title.to_lowercase().contains(&needle)
                        || t.artist.to_lowercase().contains(&needle)
                        || t.album.to_lowercase().contains(&needle)
                })
                .cloned()
                .collect()
        };
        self.set_tracks(tracks, "Tracks");
    }

    // ------------------------------------------------------------ navigation

    fn navigate_key(self: &Rc<Self>, key: &str) {
        let page = match key {
            "home" => Page::Home,
            "tracks" => Page::Tracks,
            "mixes" => Page::Mixes,
            "playlists" => Page::Playlists,
            "albums" => Page::Albums,
            "queue" => Page::Queue,
            "settings" => Page::Settings,
            "focus-search" => {
                let app = self.app();
                app.set_focus_search(app.get_focus_search() + 1);
                return;
            }
            "explore" => Page::Stub { key: key.into(), title: "Explore".into() },
            "feed" => Page::Stub { key: key.into(), title: "Feed".into() },
            "videos" => Page::Stub { key: key.into(), title: "Videos".into() },
            "profiles" => Page::Profiles,
            "later" => Page::Stub { key: key.into(), title: "Save for Later".into() },
            _ => return,
        };
        self.show(page, true);
    }

    fn open_card(self: &Rc<Self>, card: CardData) {
        if card.id.is_empty() {
            return;
        }
        let page = match card.kind.as_str() {
            "tracks" => Page::Tracks,
            "folder" => Page::Folder { id: card.id.to_string(), title: card.title.to_string() },
            _ => Page::Detail(card_from_row(&card)),
        };
        self.show(page, true);
    }

    /// Back / forward from a button, menu item or shortcut.
    fn step_history(self: &Rc<Self>, delta: isize) {
        if !crate::platform::reduce_motion() && self.nav_begin(delta) {
            self.nav_settle(true, NAV_DURATION, true);
        } else {
            let snap = self.capture_pane();
            self.step_history_now(delta, snap);
        }
    }

    /// Move through history without a transition. `leaving` is a snapshot of
    /// the page on screen, kept for a later transition back to it.
    fn step_history_now(self: &Rc<Self>, delta: isize, leaving: Option<Image>) {
        let page = {
            let mut st = self.state.borrow_mut();
            let target = st.hist_pos as isize + delta;
            if target < 0 || target as usize >= st.history.len() {
                return;
            }
            let from = st.hist_pos;
            st.hist_scroll[from] = self.app().get_scroll_y();
            match leaving {
                Some(snap) => st.snaps.insert(from, snap),
                None => st.snaps.remove(&from),
            };
            st.hist_pos = target as usize;
            let pos = st.hist_pos;
            st.snaps.retain(|i, _| i.abs_diff(pos) <= SNAP_REACH);
            st.history[st.hist_pos].clone()
        };
        self.show(page, false);
    }

    // ------------------------------------------------------------ page transitions
    //
    // Back and forward move two panes drawn from page snapshots (the overlay
    // in app.slint). Going back, the current page slides off to the right and
    // the previous one grows from 75% and fades in from dark underneath;
    // going forward runs the same motion in reverse. A two-finger swipe drives
    // the progress directly; buttons and shortcuts run it on a timer.

    /// Snapshot of the page pane as it is on screen, without the top bar.
    fn capture_pane(&self) -> Option<Image> {
        let app = self.app();
        if !app.get_logged_in() {
            return None;
        }
        let window = self.win.window();
        app.set_capturing(true);
        let started = Instant::now();
        let shot = window.take_snapshot();
        app.set_capturing(false);
        tracing::debug!("Page snapshot took {:?}", started.elapsed());
        let buf = shot.ok()?;
        let scale = window.scale_factor();
        let px = |v: f32| (v * scale).round().max(0.0) as u32;
        let rect = (px(app.get_pane_x()), px(app.get_pane_y()), px(app.get_pane_width()), px(app.get_pane_height()));
        Some(Image::from_rgba8(crop_pane(&buf, rect)?))
    }

    /// Put up the two panes for a step through history. False when there is
    /// nothing to step to or the page cannot be drawn into a pane.
    fn nav_begin(self: &Rc<Self>, delta: isize) -> bool {
        let carried = self.nav_finish();
        let app = self.app();
        if !app.get_logged_in() || app.get_show_now_playing() {
            return false;
        }
        let target = {
            let st = self.state.borrow();
            let target = st.hist_pos as isize + delta;
            if target < 0 || target as usize >= st.history.len() {
                return false;
            }
            st.snaps.get(&(target as usize)).cloned()
        };
        let Some(outgoing) = carried.or_else(|| self.capture_pane()) else {
            return false;
        };
        // The page to the right in history is always the top pane.
        let (under, over) =
            if delta < 0 { (target.clone(), Some(outgoing.clone())) } else { (Some(outgoing.clone()), target.clone()) };
        app.set_nav_has_under(under.is_some());
        app.set_nav_under(under.unwrap_or_default());
        app.set_nav_has_over(over.is_some());
        app.set_nav_over(over.unwrap_or_default());
        app.set_nav_fade(1.0);
        {
            let mut nav = self.nav.borrow_mut();
            nav.phase = NavPhase::Tracking;
            nav.delta = delta;
            nav.travel = 0.0;
            nav.samples.clear();
            nav.outgoing = Some(outgoing);
            nav.incoming = target;
        }
        self.nav_set(0.0);
        app.set_nav_active(true);
        true
    }

    fn nav_set(&self, p: f32) {
        let reveal = {
            let mut nav = self.nav.borrow_mut();
            nav.p = p;
            if nav.delta < 0 { p } else { 1.0 - p }
        };
        self.app().set_nav_reveal(reveal);
    }

    /// Take the panes down; the live page is on screen again.
    fn nav_clear(&self) {
        {
            let mut nav = self.nav.borrow_mut();
            if matches!(nav.phase, NavPhase::Idle) {
                return;
            }
            nav.timer.stop();
            nav.phase = NavPhase::Idle;
            nav.outgoing = None;
            nav.incoming = None;
            nav.pending = false;
        }
        let app = self.app();
        app.set_nav_active(false);
        app.set_nav_under(Image::default());
        app.set_nav_over(Image::default());
    }

    /// End a transition at once, wherever it is. If its page change was still
    /// waiting, make it and return the snapshot of the page now showing: the
    /// live page has not been drawn yet, so it cannot be captured.
    fn nav_finish(self: &Rc<Self>) -> Option<Image> {
        let carried = self.nav_commit_pending();
        self.nav_clear();
        carried
    }

    fn nav_commit_pending(self: &Rc<Self>) -> Option<Image> {
        let (delta, outgoing, incoming) = {
            let mut nav = self.nav.borrow_mut();
            if !std::mem::take(&mut nav.pending) {
                return None;
            }
            (nav.delta, nav.outgoing.take(), nav.incoming.clone())
        };
        self.step_history_now(delta, outgoing);
        incoming
    }

    /// Run the transition to its end (`commit`) or back to the start.
    fn nav_settle(self: &Rc<Self>, commit: bool, secs: f32, smooth: bool) {
        let from = {
            let mut nav = self.nav.borrow_mut();
            nav.pending = commit;
            nav.p
        };
        if commit && self.nav.borrow().incoming.is_none() {
            // Nothing to show for the page yet: load it behind the panes.
            self.nav_commit_pending();
        }
        let mut nav = self.nav.borrow_mut();
        nav.phase = NavPhase::Settling {
            from,
            to: if commit { 1.0 } else { 0.0 },
            start: Instant::now(),
            secs: secs.max(0.001),
            smooth,
        };
        let weak = Rc::downgrade(self);
        nav.timer.start(slint::TimerMode::Repeated, Duration::from_millis(8), move || {
            if let Some(ui) = weak.upgrade() {
                ui.nav_tick();
            }
        });
    }

    fn nav_tick(self: &Rc<Self>) {
        let now = Instant::now();
        let phase = self.nav.borrow().phase;
        match phase {
            NavPhase::Settling { from, to, start, secs, smooth } => {
                let t = ((now - start).as_secs_f32() / secs).min(1.0);
                let eased = if smooth { ease_in_out(t) } else { ease_out(t) };
                self.nav_set(from + (to - from) * eased);
                if t >= 1.0 {
                    if to >= 1.0 {
                        self.nav_commit_pending();
                        self.nav.borrow_mut().phase = NavPhase::Waiting { since: Instant::now() };
                    } else {
                        self.nav_clear();
                    }
                }
            }
            NavPhase::Waiting { since } => {
                // A fast page shows no loading state: hold the snapshot until
                // the content is there, but never for long.
                let waited = now - since;
                if waited >= Duration::from_millis(500) || (waited >= Duration::from_millis(48) && !self.app().get_loading()) {
                    self.nav.borrow_mut().phase = NavPhase::Fading { start: now };
                }
            }
            NavPhase::Fading { start } => {
                let t = ((now - start).as_secs_f32() / NAV_FADE).min(1.0);
                self.app().set_nav_fade(1.0 - t);
                if t >= 1.0 {
                    self.nav_clear();
                }
            }
            NavPhase::Idle | NavPhase::Tracking => {}
        }
    }

    /// A horizontal two-finger swipe started. `back` is a swipe to the right.
    /// False when there is no page in that direction.
    pub fn swipe_begin(self: &Rc<Self>, back: bool) -> bool {
        self.nav_begin(if back { -1 } else { 1 })
    }

    /// The fingers moved `dx` logical pixels (positive to the right).
    pub fn swipe_move(&self, dx: f32) {
        let width = self.app().get_pane_width().max(1.0);
        let p = {
            let mut nav = self.nav.borrow_mut();
            if !matches!(nav.phase, NavPhase::Tracking) {
                return;
            }
            // Back pulls the pane right, forward pulls it left.
            nav.travel = (nav.travel - nav.delta as f32 * dx).clamp(0.0, width);
            let now = Instant::now();
            let travel = nav.travel;
            nav.samples.push_back((now, travel));
            while nav.samples.front().is_some_and(|(t, _)| now - *t > Duration::from_millis(120)) {
                nav.samples.pop_front();
            }
            nav.travel / width
        };
        self.nav_set(p);
    }

    /// The fingers lifted: finish the page change or put the page back.
    pub fn swipe_end(self: &Rc<Self>, cancelled: bool) {
        let (p, velocity) = {
            let nav = self.nav.borrow();
            if !matches!(nav.phase, NavPhase::Tracking) {
                return;
            }
            let velocity = match (nav.samples.front(), nav.samples.back()) {
                // Fingers that rested before lifting carry no speed.
                (Some((t0, x0)), Some((t1, x1))) if *t1 > *t0 && t1.elapsed() < Duration::from_millis(100) => {
                    (x1 - x0) / (*t1 - *t0).as_secs_f32()
                }
                _ => 0.0,
            };
            (nav.p, velocity)
        };
        let commit = !cancelled && swipe_commits(p, velocity);
        let remaining = if commit { 1.0 - p } else { p };
        // Carries on from the fingers' speed, so it only eases out.
        self.nav_settle(commit, 0.14 + 0.20 * remaining, false);
    }

    fn show(self: &Rc<Self>, page: Page, push: bool) {
        self.app().set_menu_open(false);
        // Going to the page already on screen shows it afresh, without a
        // second history entry for it.
        let push = push && {
            let mut st = self.state.borrow_mut();
            let pos = st.hist_pos;
            let repeat = st.history.get(pos).is_some_and(|current| current.same_as(&page));
            if repeat {
                st.hist_scroll[pos] = 0.0;
            }
            !repeat
        };
        let scrolled = self.app().get_scroll_y();
        // A new page ends any transition; the page being left keeps a
        // snapshot so going back to it can be drawn.
        let leaving = if push {
            self.nav_finish().or_else(|| self.capture_pane())
        } else {
            None
        };
        let page_gen = {
            let mut st = self.state.borrow_mut();
            if push {
                let keep = if st.history.is_empty() { 0 } else { st.hist_pos + 1 };
                let from = st.hist_pos;
                st.snaps.retain(|i, _| *i < from && from - *i < SNAP_REACH);
                if let (Some(snap), true) = (leaving, keep > 0) {
                    st.snaps.insert(from, snap);
                }
                st.history.truncate(keep);
                st.hist_scroll.truncate(keep);
                if let Some(left) = st.hist_scroll.get_mut(from) {
                    *left = scrolled;
                }
                st.hist_scroll.push(0.0);
                st.history.push(page.clone());
                st.hist_pos = st.history.len() - 1;
            }
            st.page_gen += 1;
            // Artwork of the page we are leaving is dropped with its model.
            st.images.clear();
            st.page_gen
        };
        let app = self.app();
        app.set_page_title("".into());
        // A page reached through history comes back where it was left.
        let scroll = {
            let st = self.state.borrow();
            st.hist_scroll.get(st.hist_pos).copied().unwrap_or(0.0)
        };
        app.set_scroll_y(scroll);
        app.set_page_revision(page_gen as i32);
        {
            let st = self.state.borrow();
            app.set_can_go_back(st.hist_pos > 0);
            app.set_can_go_forward(st.hist_pos + 1 < st.history.len());
        }
        app.set_show_now_playing(false);
        app.set_page_has_cover(false);
        app.set_page_cover(Image::default());
        app.set_page_subtitle("".into());
        app.set_page_artist(CardData::default());
        app.set_page_year("".into());
        app.set_page_quality("".into());
        app.set_page_summary("".into());
        app.set_page_biography("".into());
        app.set_page_tint(Color::from_rgb_u8(34, 34, 34));
        app.set_artist_tab(0);
        app.set_artist_albums(ModelRc::default());
        app.set_artist_singles(ModelRc::default());
        app.set_search_cards(ModelRc::default());
        app.set_catalog_error(false);
        app.set_loading(false);
        app.set_page_favorite(false);
        app.set_page_release_line("".into());
        app.set_page_copyright("".into());
        app.set_related_artists(ModelRc::default());
        app.set_page_credits(ModelRc::default());
        app.set_page_credits_status("".into());
        app.set_create_kind("".into());
        app.set_page_card(CardData::default());
        app.set_page_can_favorite(false);
        self.state.borrow_mut().credits_for.clear();

        match page {
            Page::Home => {
                app.set_view("home".into());
                app.set_nav_key("home".into());
            }
            Page::Tracks => {
                app.set_view("tracks".into());
                app.set_nav_key("tracks".into());
                app.set_page_title("Tracks".into());
                app.set_tracks_show_date(true);
                app.set_filter_text("".into());
                self.apply_filter("");
            }
            Page::Mixes => {
                self.show_grid("mixes", "mixes", "Mixes & Radio", page_gen, |api| async move { api.mixes().await })
            }
            Page::Playlists => self.show_grid("playlists", "playlists", "Playlists", page_gen, |api| async move {
                api.playlists("root").await
            }),
            Page::Albums => {
                self.show_grid("albums", "albums", "Albums", page_gen, |api| async move { api.albums().await })
            }
            Page::Profiles => self.show_grid("profiles", "profiles", "Profiles", page_gen, |api| async move {
                api.followed_artists().await
            }),
            Page::Folder { id, title } => {
                let key = format!("folder:{id}");
                self.show_grid(&key, "playlists", &title, page_gen, move |api| async move { api.playlists(&id).await });
                app.set_nav_key("playlists".into());
            }
            Page::Detail(card) => {
                let Card { kind, id, title, quality, .. } = &card;
                let view = match kind.as_str() {
                    "album" => "album",
                    "artist" => "artist",
                    _ => "detail",
                };
                app.set_view(view.into());
                app.set_nav_key("".into());
                app.set_page_has_cover(true);
                app.set_tracks_show_date(kind == "playlist");
                let cache_key = format!("tracks:{kind}:{id}");
                self.present_catalog_card(&card, page_gen);
                if kind == "album" && !quality.is_empty() {
                    app.set_page_quality(quality.clone().into());
                }
                if matches!(view, "artist" | "album") {
                    self.load_catalog_detail(kind, id, page_gen);
                }
                if view == "artist" {
                    self.load_artist_releases(id, page_gen);
                    self.load_related_artists(id, page_gen);
                }
                if matches!(kind.as_str(), "album" | "playlist" | "mix") {
                    app.set_page_card(card_row(&card));
                    app.set_page_can_favorite(can_favorite(&card));
                }
                if view == "artist" || can_favorite(&card) {
                    let (fav_kind, fav_id) = (favorite_kind(kind), id.clone());
                    self.spawn(move |ctx| async move {
                        let Some(api) = ctx.api() else { return };
                        if let Ok(favorite) = api.is_favorite(fav_kind, &fav_id).await {
                            on_ui(move |ui| {
                                if ui.state.borrow().page_gen == page_gen {
                                    ui.app().set_page_favorite(favorite);
                                }
                            });
                        }
                    });
                }
                let (kind, id) = (kind.clone(), id.clone());
                self.show_track_list(&cache_key, title, page_gen, move |api| async move { api.tracks_of(&kind, &id).await });
            }
            Page::ArtistTracks(card) => {
                app.set_view("detail".into());
                app.set_nav_key("".into());
                app.set_page_title("Top Tracks".into());
                app.set_tracks_show_date(false);
                let id = card.id;
                self.show_track_list(&format!("tracks:artist:{id}"), &card.title, page_gen, move |api| async move {
                    api.tracks_of("artist", &id).await
                });
            }
            Page::Search(query) => {
                app.set_view("search".into());
                app.set_nav_key("".into());
                app.set_search_text(query.as_str().into());
                app.set_page_title(format!("Results for “{query}”").into());
                app.set_tracks_show_date(false);
                let cache_key = format!("search:{}", query.to_lowercase());
                let catalog_query = query.clone();
                self.load_catalog(
                    format!("search-catalog:{}", query.to_lowercase()),
                    page_gen,
                    move |api| async move { api.search_catalog(&catalog_query).await },
                    |ui, cards: Vec<Card>| {
                        ui.app().set_search_cards(ModelRc::from(ui.cards_model(&cards, 96)));
                    },
                );
                self.show_track_list(&cache_key, "Search", page_gen, move |api| async move { api.search_tracks(&query).await });
            }
            Page::Queue => {
                app.set_view("queue".into());
                app.set_nav_key("queue".into());
                app.set_page_title("Play queue".into());
                app.set_tracks_show_date(false);
                let (tracks, context) = {
                    let st = self.state.borrow();
                    let upcoming = st.order.iter().skip(st.pos).filter_map(|i| st.queue.get(*i).cloned()).collect();
                    (upcoming, st.context.clone())
                };
                self.set_tracks(tracks, &context);
            }
            Page::Settings => {
                app.set_view("settings".into());
                app.set_nav_key("".into());
                self.present_settings();
            }
            Page::Stub { key, title } => {
                app.set_view("stub".into());
                app.set_nav_key(key.into());
                app.set_page_title(title.into());
            }
        }
    }

    fn present_catalog_card(self: &Rc<Self>, card: &Card, page_gen: u64) {
        let app = self.app();
        if !card.title.is_empty() {
            app.set_page_title(card.title.as_str().into());
        }
        if !card.subtitle.is_empty() {
            app.set_page_subtitle(card.subtitle.as_str().into());
        }
        if card.kind == "album" && !card.quality.is_empty() {
            app.set_page_quality(card.quality.as_str().into());
        }
        if let Some(url) = &card.image_url {
            let url = if card.kind == "artist" {
                url.replace("/320x320.jpg", "/750x750.jpg")
            } else if card.kind == "album" {
                sized_cover(url, 640).replace("/320x320.jpg", "/640x640.jpg")
            } else {
                url.clone()
            };
            let weak = Rc::downgrade(self);
            self.load_image(&url, 640, move |image, tint| {
                if let Some(ui) = weak.upgrade().filter(|ui| ui.state.borrow().page_gen == page_gen) {
                    ui.app().set_page_cover(image.clone());
                    ui.app().set_page_tint(tint);
                }
            });
        }
    }

    /// Fetch the picture and biography for the artist of the current track (player-bar hover card).
    fn load_artist_hover_card(self: &Rc<Self>) {
        let Some(id) = self.current_track().and_then(|t| t.artist_id) else { return };
        {
            let mut st = self.state.borrow_mut();
            if st.hover_artist == id {
                return;
            }
            st.hover_artist = id.clone();
        }
        let app = self.app();
        app.set_np_artist_bio("".into());
        app.set_np_artist_image(Image::default());
        app.set_np_artist_followed(false);
        {
            let id = id.clone();
            self.spawn(move |ctx| async move {
                let Some(api) = ctx.api() else { return };
                if let Ok(followed) = api.is_artist_followed(&id).await {
                    on_ui(move |ui| {
                        if ui.state.borrow().hover_artist == id {
                            ui.app().set_np_artist_followed(followed);
                        }
                    });
                }
            });
        }
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            let Ok(detail) = api.catalog_detail("artist", &id).await else { return };
            on_ui(move |ui| {
                if ui.state.borrow().hover_artist != id {
                    return;
                }
                ui.app().set_np_artist_bio(short_bio(&detail.biography).into());
                if let Some(url) = detail.card.image_url {
                    let weak = Rc::downgrade(ui);
                    ui.load_image(&url, 640, move |img, _| {
                        let Some(ui) = weak.upgrade() else { return };
                        if ui.state.borrow().hover_artist == id {
                            ui.app().set_np_artist_image(img.clone());
                        }
                    });
                }
            });
        });
    }

    fn toggle_follow_artist(self: &Rc<Self>) {
        let id = self.state.borrow().hover_artist.clone();
        if id.is_empty() {
            return;
        }
        let app = self.app();
        let follow = !app.get_np_artist_followed();
        app.set_np_artist_followed(follow);
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            if let Err(e) = api.set_artist_followed(&id, follow).await {
                tracing::warn!("follow toggle failed: {e}");
                on_ui(move |ui| {
                    if ui.state.borrow().hover_artist == id {
                        ui.app().set_np_artist_followed(!follow);
                    }
                });
            }
        });
    }

    fn load_catalog_detail(self: &Rc<Self>, kind: &str, id: &str, page_gen: u64) {
        let (kind, id) = (kind.to_string(), id.to_string());
        self.load_catalog(
            format!("catalog:{kind}:{id}"),
            page_gen,
            move |api| async move { api.catalog_detail(&kind, &id).await },
            move |ui, detail: CatalogDetail| {
                ui.present_catalog_card(&detail.card, page_gen);
                let app = ui.app();
                let is_album = detail.card.kind == "album";
                if let Some(artist) = detail.artist {
                    app.set_page_artist(card_row(&artist));
                    if let Some(url) = &artist.image_url {
                        let (weak, artist_id) = (Rc::downgrade(ui), artist.id.clone());
                        ui.load_image(url, 96, move |image, _| {
                            let Some(ui) = weak.upgrade().filter(|ui| ui.state.borrow().page_gen == page_gen) else { return };
                            let mut row = ui.app().get_page_artist();
                            if row.id == artist_id.as_str() {
                                row.image = image.clone();
                                ui.app().set_page_artist(row);
                            }
                        });
                    }
                    if is_album {
                        ui.load_album_extras(&artist.id, &detail.card, page_gen);
                    }
                }
                app.set_page_year(detail.year.into());
                app.set_page_quality(detail.quality.into());
                if detail.track_count > 0 {
                    let label = if detail.track_count == 1 { "TRACK" } else { "TRACKS" };
                    let summary = format!("{} {label}  ({})", detail.track_count, fmt_time(detail.duration));
                    let date = long_date(&detail.release_date);
                    if is_album && !date.is_empty() {
                        app.set_page_release_line(format!("{date}  ·  {summary}").into());
                    }
                    app.set_page_summary(summary.into());
                }
                app.set_page_copyright(detail.copyright.to_uppercase().into());
                app.set_page_biography(detail.biography.into());
            },
        );
    }

    fn load_artist_releases(self: &Rc<Self>, id: &str, page_gen: u64) {
        for singles in [false, true] {
            let id = id.to_string();
            let group = if singles { "singles" } else { "albums" };
            self.load_catalog(
                format!("artist-{group}:{id}"),
                page_gen,
                move |api| async move { api.artist_releases(&id, singles).await },
                move |ui, cards: Vec<Card>| {
                    let model = ModelRc::new(VecModel::from(cards.iter().map(card_row).collect::<Vec<_>>()));
                    if singles {
                        ui.app().set_artist_singles(model);
                    } else {
                        ui.app().set_artist_albums(model);
                    }
                },
            );
        }
    }

    /// "More albums by" and related artists under an album's track list. Asked once per page.
    fn load_album_extras(self: &Rc<Self>, artist_id: &str, album: &Card, page_gen: u64) {
        {
            let mut st = self.state.borrow_mut();
            let key = (page_gen, artist_id.to_string());
            if st.extras_for == key {
                return;
            }
            st.extras_for = key;
        }
        let (id, album_id, album_title) = (artist_id.to_string(), album.id.clone(), album.title.to_lowercase());
        self.load_catalog(
            format!("artist-albums:{id}"),
            page_gen,
            {
                let id = id.clone();
                move |api| async move { api.artist_releases(&id, false).await }
            },
            move |ui, cards: Vec<Card>| {
                // Other editions of this album carry the same title.
                let rows: Vec<CardData> = cards
                    .iter()
                    .filter(|c| c.id != album_id && c.title.to_lowercase() != album_title)
                    .map(card_row)
                    .collect();
                ui.app().set_artist_albums(ModelRc::new(VecModel::from(rows)));
            },
        );
        self.load_related_artists(&id, page_gen);
    }

    fn load_related_artists(self: &Rc<Self>, artist_id: &str, page_gen: u64) {
        let id = artist_id.to_string();
        self.load_catalog(
            format!("artist-similar:{id}"),
            page_gen,
            move |api| async move { api.similar_artists(&id).await },
            |ui, cards: Vec<Card>| {
                let rows: Vec<CardData> = cards.iter().map(card_row).collect();
                ui.app().set_related_artists(ModelRc::new(VecModel::from(rows)));
            },
        );
    }

    /// The album, playlist, mix or artist page on screen, if that is what is showing.
    fn page_card(&self) -> Option<Card> {
        let st = self.state.borrow();
        match st.history.get(st.hist_pos) {
            Some(Page::Detail(card)) => Some(card.clone()),
            _ => None,
        }
    }

    /// Add / remove the album from the collection, or follow / unfollow the artist.
    fn toggle_page_favorite(self: &Rc<Self>) {
        let Some(card) = self.page_card().filter(|card| card.kind == "artist" || can_favorite(card)) else { return };
        let page_gen = self.state.borrow().page_gen;
        let app = self.app();
        let favorite = !app.get_page_favorite();
        app.set_page_favorite(favorite);
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            if let Err(e) = set_card_favorite(&api, &card, favorite).await {
                tracing::error!("Could not update My Collection for {}: {e}", card.title);
                on_ui(move |ui| {
                    if ui.state.borrow().page_gen == page_gen {
                        ui.app().set_page_favorite(!favorite);
                    }
                });
            }
        });
    }

    /// Share and Credits on an album, playlist or mix page.
    fn page_action(self: &Rc<Self>, action: &str, _arg: &str) {
        let Some(card) = self.page_card() else { return };
        match action {
            "copy-link" => self.copy_card_link(&card),
            "credits" if card.kind == "album" => {
                {
                    let mut st = self.state.borrow_mut();
                    if st.credits_for == card.id {
                        return;
                    }
                    st.credits_for = card.id.clone();
                }
                let app = self.app();
                app.set_page_credits(ModelRc::default());
                app.set_page_credits_status("Loading…".into());
                let id = card.id;
                self.spawn(move |ctx| async move {
                    let Some(api) = ctx.api() else { return };
                    let credits = api.album_credits(&id).await;
                    on_ui(move |ui| {
                        if ui.state.borrow().credits_for != id {
                            return;
                        }
                        let app = ui.app();
                        match credits {
                            Ok(credits) => {
                                app.set_page_credits_status("No credits available".into());
                                let rows: Vec<CreditLine> = credits
                                    .into_iter()
                                    .map(|c| CreditLine { role: c.role.into(), names: c.names.into() })
                                    .collect();
                                app.set_page_credits(ModelRc::new(VecModel::from(rows)));
                            }
                            Err(e) => {
                                tracing::warn!("credits failed: {e}");
                                app.set_page_credits_status("Couldn’t load credits".into());
                                ui.state.borrow_mut().credits_for.clear();
                            }
                        }
                    });
                });
            }
            _ => {}
        }
    }

    fn copy_card_link(&self, card: &Card) {
        match self.copy_link(&format!("https://tidal.com/{}/{}", card.kind, card.id)) {
            Ok(()) => crate::alerts::notice(&self.app(), "Link copied"),
            Err(e) => tracing::warn!("Could not copy link: {e}"),
        }
    }

    /// Open the context menu on an album, playlist, mix or artist card.
    fn open_card_menu(self: &Rc<Self>, row: CardData, x: f32, y: f32) {
        let app = self.app();
        if app.get_menu_open() || !app.get_logged_in() || row.id.is_empty() {
            return;
        }
        if !matches!(row.kind.as_str(), "album" | "playlist" | "mix" | "artist") {
            return;
        }
        let card = card_from_row(&row);
        // Opened from the page's own More button: the page knows the cover and the state.
        let on_page = self.page_card().is_some_and(|page| page.kind == card.kind && page.id == card.id);
        let mut row = row;
        if on_page && row.image.size().width == 0 {
            row.image = app.get_page_cover();
        }
        app.set_menu_sub("".into());
        app.set_menu_kind(row.kind.clone());
        app.set_menu_card(row);
        app.set_menu_can_favorite(can_favorite(&card));
        app.set_menu_favorite(on_page && app.get_page_favorite());
        app.set_menu_x(x);
        app.set_menu_y(y);
        app.set_menu_open(true);
        self.state.borrow_mut().menu_card = Some(card.clone());
        if can_favorite(&card) && !on_page {
            self.spawn(move |ctx| async move {
                let Some(api) = ctx.api() else { return };
                if let Ok(true) = api.is_favorite(favorite_kind(&card.kind), &card.id).await {
                    on_ui(move |ui| {
                        let app = ui.app();
                        if app.get_menu_open() && app.get_menu_card().id == card.id.as_str() {
                            app.set_menu_favorite(true);
                        }
                    });
                }
            });
        }
    }

    /// An entry of the context menu that the controller carries out: every
    /// card entry, and "add-to-playlist" for tracks too.
    fn menu_action(self: &Rc<Self>, action: &str, arg: &str) {
        let app = self.app();
        if app.get_menu_kind() == "track" {
            let Some(track) = self.state.borrow().menu_track.clone() else { return };
            if action == "add-to-playlist" && !arg.is_empty() {
                self.add_to_playlist(arg, vec![track.id], track.title);
            }
            return;
        }
        let Some(card) = self.state.borrow().menu_card.clone() else { return };
        match action {
            "open" => self.open_card(app.get_menu_card()),
            "copy-link" => self.copy_card_link(&card),
            "favorite" if can_favorite(&card) => {
                let favorite = !app.get_menu_favorite();
                let on_page = self.page_card().is_some_and(|page| page.kind == card.kind && page.id == card.id);
                let page_gen = self.state.borrow().page_gen;
                if on_page {
                    app.set_page_favorite(favorite);
                }
                self.spawn(move |ctx| async move {
                    let Some(api) = ctx.api() else { return };
                    match set_card_favorite(&api, &card, favorite).await {
                        Ok(()) => on_ui(move |ui| {
                            let text = if favorite { "Added to My Collection" } else { "Removed from My Collection" };
                            crate::alerts::notice(&ui.app(), text);
                            if card.kind == "playlist" {
                                ui.sync_playlists();
                            }
                        }),
                        Err(e) => {
                            tracing::error!("Could not update My Collection for {}: {e}", card.title);
                            on_ui(move |ui| {
                                if on_page && ui.state.borrow().page_gen == page_gen {
                                    ui.app().set_page_favorite(!favorite);
                                }
                            });
                        }
                    }
                });
            }
            "artist" if card.kind == "album" => {
                self.spawn(move |ctx| async move {
                    let Some(api) = ctx.api() else { return };
                    match api.catalog_detail("album", &card.id).await {
                        Ok(detail) => on_ui(move |ui| {
                            if let Some(artist) = detail.artist {
                                ui.open_card(card_row(&artist));
                            }
                        }),
                        Err(e) => tracing::warn!("could not find the artist of {}: {e}", card.title),
                    }
                });
            }
            "play" | "play-next" | "add-to-playlist" => {
                let (action, arg) = (action.to_string(), arg.to_string());
                // The page already holds the tracks of the card it shows.
                let on_page = self.page_card().is_some_and(|page| page.kind == card.kind && page.id == card.id);
                let loaded = on_page.then(|| self.state.borrow().page_tracks.clone()).filter(|t| !t.is_empty());
                if let Some(tracks) = loaded {
                    self.card_tracks_action(&action, &arg, &card, tracks);
                    return;
                }
                self.spawn(move |ctx| async move {
                    let Some(api) = ctx.api() else { return };
                    match api.tracks_of(&card.kind, &card.id).await {
                        Ok(tracks) => on_ui(move |ui| ui.card_tracks_action(&action, &arg, &card, tracks)),
                        Err(e) => tracing::error!("Could not load the tracks of {}: {e}", card.title),
                    }
                });
            }
            _ => {}
        }
    }

    /// Play, queue or add to a playlist the tracks behind a card.
    fn card_tracks_action(self: &Rc<Self>, action: &str, arg: &str, card: &Card, tracks: Vec<TrackItem>) {
        if tracks.is_empty() {
            return;
        }
        match action {
            "play" => self.play_queue(tracks, 0, &card.title, Some(false)),
            "play-next" if self.current_track().is_none() => self.play_queue(tracks, 0, &card.title, Some(false)),
            "play-next" => {
                // Inserted last to first, so they follow in their own order.
                for track in tracks.into_iter().rev() {
                    self.queue_next(track);
                }
            }
            "add-to-playlist" if !arg.is_empty() => {
                self.add_to_playlist(arg, tracks.into_iter().map(|t| t.id).collect(), card.title.clone());
            }
            _ => {}
        }
    }

    fn add_to_playlist(self: &Rc<Self>, playlist: &str, ids: Vec<String>, title: String) {
        let playlist = playlist.to_string();
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.add_to_playlist(&playlist, &ids).await {
                Ok(()) => on_ui(move |ui| {
                    crate::alerts::notice(&ui.app(), &format!("Added “{title}” to the playlist"));
                    ui.sync_playlists();
                }),
                Err(e) => tracing::error!("Could not add “{title}” to the playlist: {e}"),
            }
        });
    }

    fn artist_release_art(self: &Rc<Self>, singles: bool, from: usize, upto: usize) {
        let app = self.app();
        let model = if singles { app.get_artist_singles() } else { app.get_artist_albums() };
        self.card_art(model, from, upto);
    }

    /// Lazily load artwork for cards [from, upto) of a carousel's model.
    fn card_art(self: &Rc<Self>, model: ModelRc<CardData>, from: usize, upto: usize) {
        for i in from..upto.min(model.row_count()) {
            let Some(row) = model.row_data(i).filter(|row| !row.image_url.is_empty() && row.image.size().width == 0) else {
                continue;
            };
            let model = model.clone();
            self.load_image(&row.image_url, 240, move |image, _| {
                if let Some(mut row) = model.row_data(i) {
                    row.image = image.clone();
                    model.set_row_data(i, row);
                }
            });
        }
    }

    fn load_catalog<T, F, Fut, P>(self: &Rc<Self>, key: String, page_gen: u64, fetch: F, present: P)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, tide_api::ApiError>> + Send,
        P: Fn(&Rc<Self>, T) + Send + 'static,
    {
        let cached = self.ctx.meta.get_json(&key);
        if let Some(data) = cached.as_deref().and_then(|json| serde_json::from_str(json).ok()) {
            present(self, data);
        }
        self.spawn(move |ctx| async move {
            let mut changed = None;
            if let Some(api) = ctx.api() {
                match fetch(api).await {
                    Ok(data) => {
                        if let Ok(json) = serde_json::to_string(&data) {
                            if cached.as_deref() != Some(&json) {
                                let _ = ctx.meta.put_json(&key, &json);
                                changed = Some(data);
                            }
                        }
                    }
                    Err(e) => tracing::warn!("catalog load failed for {key}: {e}"),
                }
            }
            on_ui(move |ui| {
                if let Some(data) = changed.filter(|_| ui.state.borrow().page_gen == page_gen) {
                    present(ui, data);
                }
            });
        });
    }

    /// Card grid: cached copy first (zero network), then refresh.
    fn show_grid<F, Fut>(self: &Rc<Self>, key: &str, kind: &str, title: &str, page_gen: u64, fetch: F)
    where
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Vec<Card>, tide_api::ApiError>> + Send,
    {
        let app = self.app();
        app.set_view("grid".into());
        app.set_nav_key(key.into());
        app.set_page_title(title.into());
        // Sort and grouping are remembered per kind of page.
        let (sorts, groups) = grid_options(kind);
        let saved: Vec<String> = self
            .ctx
            .meta
            .get_json(&format!("grid-prefs:{kind}"))
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let pick = |saved: Option<&String>, options: &[&str]| {
            saved.filter(|s| options.contains(&s.as_str())).cloned().unwrap_or_else(|| options[0].to_string())
        };
        let (sort, group) = (pick(saved.first(), sorts), pick(saved.get(1), groups));
        let texts = |options: &[&str]| {
            ModelRc::new(VecModel::from(options.iter().map(|o| slint::SharedString::from(*o)).collect::<Vec<_>>()))
        };
        app.set_grid_kind(kind.into());
        app.set_grid_sort_options(texts(sorts));
        app.set_grid_group_options(texts(groups));
        app.set_grid_sort(sort.as_str().into());
        app.set_grid_group(group.as_str().into());
        app.set_grid_can_create(kind == "playlists");
        app.set_grid_filter_hint(format!("Filter {kind}").into());
        app.set_filter_text("".into());
        self.state.borrow_mut().grid = Grid { kind: kind.into(), sort, group, ..Default::default() };
        let cache_key = format!("cards:{key}");
        let cached: Vec<Card> =
            self.ctx.meta.get_json(&cache_key).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        app.set_loading(cached.is_empty());
        self.set_grid(&cached);
        let cached_len = cached.len();
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else { return };
            match fetch(api).await {
                Ok(cards) => {
                    let json = serde_json::to_string(&cards).unwrap_or_default();
                    let changed = ctx.meta.get_json(&cache_key).as_deref() != Some(json.as_str());
                    let _ = ctx.meta.put_json(&cache_key, &json);
                    on_ui(move |ui| {
                        if ui.state.borrow().page_gen == page_gen {
                            ui.app().set_loading(false);
                            if changed || cached_len == 0 {
                                ui.set_grid(&cards);
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("grid load failed: {e}");
                    on_ui(move |ui| {
                        if ui.state.borrow().page_gen == page_gen {
                            ui.app().set_loading(false);
                        }
                    });
                }
            }
        });
    }

    fn show_track_list<F, Fut>(self: &Rc<Self>, cache_key: &str, context: &str, page_gen: u64, fetch: F)
    where
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Vec<TrackItem>, tide_api::ApiError>> + Send,
    {
        let cached: Vec<TrackItem> =
            self.ctx.meta.get_json(cache_key).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        self.app().set_loading(cached.is_empty());
        let had_cached_tracks = !cached.is_empty();
        let cached_json = serde_json::to_string(&cached).unwrap_or_default();
        self.set_tracks(cached, context);
        let (cache_key, context) = (cache_key.to_string(), context.to_string());
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else {
                on_ui(move |ui| {
                    if ui.state.borrow().page_gen == page_gen {
                        ui.app().set_loading(false);
                    }
                });
                return;
            };
            match fetch(api).await {
                Ok(tracks) => {
                    let json = serde_json::to_string(&tracks).unwrap_or_default();
                    let changed = json != cached_json;
                    let _ = ctx.meta.put_json(&cache_key, &json);
                    on_ui(move |ui| {
                        if ui.state.borrow().page_gen == page_gen {
                            ui.app().set_loading(false);
                            if changed {
                                ui.set_tracks(tracks, &context);
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("track list failed: {e}");
                    on_ui(move |ui| {
                        if ui.state.borrow().page_gen == page_gen {
                            ui.app().set_loading(false);
                            ui.app().set_catalog_error(!had_cached_tracks);
                        }
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------ track menu

    fn open_track_menu(self: &Rc<Self>, id: &str, x: f32, y: f32) {
        let app = self.app();
        if app.get_menu_open() || !app.get_logged_in() {
            return;
        }
        let track = {
            let st = self.state.borrow();
            st.page_tracks.iter()
                .chain(st.queue.iter())
                .chain(st.sections.iter().flat_map(|section| section.tracks.iter()))
                .find(|track| track.id == id)
                .cloned()
        };
        let Some(track) = track else { return };
        let mut row = track_row(&track, self.state.borrow().fav_ids.contains(id));
        if app.get_current_id() == id {
            row.cover = app.get_np_cover();
        }
        self.state.borrow_mut().menu_track = Some(track);
        app.set_menu_sub("".into());
        app.set_menu_kind("track".into());
        let cover_url = row.cover_url.clone();
        app.set_menu_track(row);
        app.set_menu_x(x);
        app.set_menu_y(y);
        app.set_menu_open(true);
        if !cover_url.is_empty() {
            let id = id.to_owned();
            let weak = Rc::downgrade(self);
            // The stored URL is the 160px cover; TIDAL does not serve a 128px one.
            self.load_image(&cover_url, 128, move |image, _| {
                let Some(ui) = weak.upgrade() else { return };
                let app = ui.app();
                let mut row = app.get_menu_track();
                if app.get_menu_open() && row.id == id {
                    row.cover = image.clone();
                    app.set_menu_track(row);
                }
            });
        }
    }

    fn copy_track_link(&self, id: &str) -> Result<(), arboard::Error> {
        self.copy_link(&format!("https://tidal.com/track/{id}"))
    }

    fn copy_link(&self, url: &str) -> Result<(), arboard::Error> {
        let mut clipboard = self.clipboard.borrow_mut();
        if clipboard.is_none() {
            *clipboard = Some(arboard::Clipboard::new()?);
        }
        if let Some(clipboard) = clipboard.as_mut() {
            clipboard.set_text(url.to_string())?;
        }
        Ok(())
    }

    fn play_track_next(self: &Rc<Self>, id: &str) {
        let track = self.state.borrow().menu_track.as_ref().filter(|track| track.id == id).cloned();
        let Some(track) = track else { return };
        if self.current_track().is_none() {
            self.play_queue(vec![track], 0, "", None);
            self.refresh_queue_page();
            return;
        }
        self.queue_next(track);
    }

    /// Put `track` right after the one playing. Something must be playing or queued.
    fn queue_next(self: &Rc<Self>, track: TrackItem) {
        // A stale prefetch can be downloading, waiting in the audio queue, or
        // already decoded. Cancel all three before changing the playback order.
        {
            let mut revision = self.ctx.next_revision.lock().unwrap();
            *revision = revision.wrapping_add(1);
            self.ctx.player.cancel_upcoming();
        }
        if !self.state.borrow().loading_track {
            self.sync_gapless_transition();
        }
        self.state.borrow_mut().insert_next(track);
        self.persist_queue();
        self.refresh_queue_page();
    }

    fn refresh_queue_page(self: &Rc<Self>) {
        if self.app().get_view() != "queue" {
            return;
        }
        let (tracks, context) = {
            let st = self.state.borrow();
            let upcoming = st.order.iter().skip(st.pos).map(|&i| st.queue[i].clone()).collect();
            (upcoming, st.context.clone())
        };
        self.set_tracks(tracks, &context);
    }

    // ------------------------------------------------------------ playback

    fn play_from_page(self: &Rc<Self>, id: &str) {
        let (tracks, context) = {
            let st = self.state.borrow();
            (st.page_tracks.clone(), st.page_context.clone())
        };
        if let Some(index) = tracks.iter().position(|t| t.id == id) {
            self.play_queue(tracks, index, &context, None);
        }
    }

    fn play_from_section(self: &Rc<Self>, section: usize, id: &str) {
        let found = {
            let st = self.state.borrow();
            st.sections.get(section).map(|s| (s.tracks.clone(), s.title.clone()))
        };
        if let Some((tracks, title)) = found {
            if let Some(index) = tracks.iter().position(|t| t.id == id) {
                self.play_queue(tracks, index, &title, None);
            }
        }
    }

    fn play_all(self: &Rc<Self>, shuffle: bool) {
        let (tracks, context) = {
            let st = self.state.borrow();
            (st.page_tracks.clone(), st.page_context.clone())
        };
        if tracks.is_empty() {
            return;
        }
        let start = if shuffle { shuffled(tracks.len(), usize::MAX)[0] } else { 0 };
        self.play_queue(tracks, start, &context, Some(shuffle));
    }

    fn play_queue(self: &Rc<Self>, tracks: Vec<TrackItem>, index: usize, context: &str, shuffle: Option<bool>) {
        {
            let mut st = self.state.borrow_mut();
            if let Some(shuffle) = shuffle {
                st.shuffle = shuffle;
            }
            st.order = if st.shuffle { shuffled(tracks.len(), index) } else { (0..tracks.len()).collect() };
            st.pos = if st.shuffle { 0 } else { index };
            st.queue = tracks;
            st.context = context.to_string();
            st.pending_seek = None;
        }
        self.app().set_shuffle(self.state.borrow().shuffle);
        self.persist_queue();
        self.start_current();
    }

    fn effective_elapsed(&self) -> Duration {
        let st = self.state.borrow();
        if let Some(pending) = st.pending_seek {
            pending
        } else {
            self.ctx.player.elapsed()
        }
    }

    fn current_cursor(&self) -> PlaybackCursor {
        let st = self.state.borrow();
        let volume = if st.muted { st.volume_before_mute } else { self.app().get_volume() };
        let elapsed = st.pending_seek.unwrap_or_else(|| self.ctx.player.elapsed());
        PlaybackCursor {
            pos: st.pos,
            elapsed_ms: elapsed.as_millis() as u64,
            shuffle: st.shuffle,
            repeat_mode: st.repeat,
            volume,
            muted: st.muted,
        }
    }

    fn persist_queue(&self) {
        let cursor = self.current_cursor();
        let saved = {
            let st = self.state.borrow();
            SavedPlayback {
                queue: st.queue.clone(),
                order: st.order.clone(),
                pos: cursor.pos,
                elapsed_ms: cursor.elapsed_ms,
                context: st.context.clone(),
                shuffle: cursor.shuffle,
                repeat_mode: cursor.repeat_mode,
                volume: cursor.volume,
                muted: cursor.muted,
            }
        };
        let _ = self.ctx.meta.save_playback_queue(&saved);
    }

    fn persist_cursor(&self) {
        let _ = self.ctx.meta.update_playback_cursor(self.current_cursor());
    }

    fn current_track(&self) -> Option<TrackItem> {
        let st = self.state.borrow();
        st.order.get(st.pos).and_then(|i| st.queue.get(*i)).cloned()
    }

    /// Queue position that follows `pos`, honouring repeat. `auto` = track ended by itself.
    fn following(&self, auto: bool) -> Option<usize> {
        let st = self.state.borrow();
        if st.order.is_empty() {
            return None;
        }
        if auto && st.repeat == 2 {
            return Some(st.pos);
        }
        if st.pos + 1 < st.order.len() {
            Some(st.pos + 1)
        } else if st.repeat == 1 {
            Some(0)
        } else {
            None
        }
    }

    /// Show the current queue entry in the player bar (no audio side effects).
    fn present_current(self: &Rc<Self>) {
        let Some(track) = self.current_track() else { return };
        let app = self.app();
        let (context, favorite) = {
            let st = self.state.borrow();
            (st.context.clone(), st.fav_ids.contains(&track.id))
        };
        app.set_has_track(true);
        app.set_current_id(track.id.as_str().into());
        app.set_np_title(track.title.as_str().into());
        app.set_np_artist(track.artist.as_str().into());
        app.set_np_artist_id(track.artist_id.clone().unwrap_or_default().as_str().into());
        app.set_np_context(context.into());
        app.set_np_explicit(track.explicit);
        app.set_np_favorite(favorite);
        app.set_duration_secs(track.duration_secs.max(0) as i32);
        app.set_total(fmt_time(track.duration_secs as u64).into());
        app.set_elapsed("0:00".into());
        app.set_progress(0.0);
        let cover = track.cover_url.as_deref().map(|u| sized_cover(u, 640));
        let cached_cover = cover
            .as_deref()
            .map(|u| self.ctx.images.file_path(&url_key(u, 0)))
            .filter(|p| p.exists());

        if let Some(cover_url) = cover.clone() {
            let id = track.id.clone();
            let weak = Rc::downgrade(self);
            let cover_url_for_cache = cover_url.clone();
            self.load_image(&cover_url, 640, move |img, tint| {
                let Some(ui) = weak.upgrade() else { return };
                if ui.app().get_current_id() == id.as_str() {
                    ui.app().set_np_cover(img.clone());
                    ui.app().set_np_cover_large(img.clone());
                    ui.app().set_np_tint(tint);
                    let path = ui.ctx.images.file_path(&url_key(&cover_url_for_cache, 0));
                    if path.exists() {
                        ui.ctx.with_media(|m| {
                            let _ = m.set_artwork_file(&path);
                        });
                    }
                }
            });
        } else {
            app.set_np_cover(Image::default());
            app.set_np_cover_large(Image::default());
        }
        if app.get_show_now_playing() {
            self.load_lyrics();
        }
        self.ctx.with_media(|m| {
            let _ = m.set_metadata(
                &track.title,
                &track.artist,
                &track.album,
                Some(Duration::from_secs(track.duration_secs as u64)),
                cached_cover.as_deref().and_then(|p| p.to_str()).or(cover.as_deref()),
            );
        });
    }

    fn present_format(&self) {
        let app = self.app();
        let Some(format) = self.ctx.player.format_info() else { return };
        let khz = format.sample_rate as f32 / 1000.0;
        if format.bits_per_sample == 0 {
            // Lossy streams only come from the "Low" tier, which is AAC 320 kbps.
            app.set_quality_label("AAC 320kbps".into());
            app.set_quality_tier("high".into());
        } else {
            app.set_quality_label(format!("{}-bit {khz}kHz", format.bits_per_sample).into());
            let max = format.bits_per_sample > 16 || format.sample_rate > 48_000;
            app.set_quality_tier(if max { "max" } else { "lossless" }.into());
        }
        self.present_signal();
    }

    /// Refresh the signal-path indicator and the output description.
    fn present_signal(&self) {
        let app = self.app();
        let player = &self.ctx.player;
        let output = player.output_info();
        app.set_output_info(format!("{} · {} · {}", output.name, output.kind.label(), khz(output.sample_rate)).into());
        let Some(format) = player.format_info().filter(|_| app.get_has_track()) else {
            if let Ok(mut st) = self.state.try_borrow_mut() {
                st.shown_bitrate = 0;
            }
            app.set_signal_quality("".into());
            return;
        };
        let volume = if app.get_muted() { 0.0 } else { app.get_volume() };
        if let Ok(mut st) = self.state.try_borrow_mut() {
            st.shown_bitrate = format.bitrate_kbps;
        }
        let path = signal_path(format, &output, volume, player.rate_matching(), player.is_released());
        app.set_signal_quality(path.quality.key().into());
        app.set_signal_title(path.quality.title().into());
        app.set_signal_note(path.note.into());
        let stages: Vec<SignalStage> = path
            .stages
            .into_iter()
            .map(|s| SignalStage {
                kind: s.kind.into(),
                title: s.title.into(),
                detail: s.detail.into(),
                quality: s.quality.key().into(),
            })
            .collect();
        app.set_signal_stages(ModelRc::from(Rc::new(VecModel::from(stages))));
    }

    /// Stop what is playing and start the current queue entry from the top.
    fn start_current(self: &Rc<Self>) {
        self.start_current_at(Duration::ZERO);
    }

    /// Stop what is playing and start the current queue entry at `start_at`.
    fn start_current_at(self: &Rc<Self>, start_at: Duration) {
        let Some(track) = self.current_track() else { return };
        let generation = self.ctx.play_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.ctx.player.pause();
        self.ctx.player.clear_queue();
        {
            let mut st = self.state.borrow_mut();
            st.loading_track = true;
            st.prefetched_for = None;
            st.pending_seek = None;
        }
        self.present_current();
        let app = self.app();
        if !start_at.is_zero() {
            let total_secs = track.duration_secs.max(1) as f32;
            let clamped = start_at.min(Duration::from_secs(track.duration_secs as u64));
            app.set_elapsed(fmt_time(clamped.as_secs()).into());
            app.set_progress((clamped.as_secs_f32() / total_secs).clamp(0.0, 1.0));
        }
        app.set_playing(false);
        app.set_buffering(true);
        app.set_debug_title(track.title.as_str().into());
        app.set_debug_status("Resolving".into());
        app.set_debug_stats("".into());
        app.set_debug_active(true);
        app.set_debug_blocks(ModelRc::new(VecModel::from(Vec::<i32>::new())));
        let t_buffering_start = std::time::Instant::now();
        tracing::info!(track_id = %track.id, title = %track.title, ?start_at, "Buffering started");

        self.spawn(|ctx| async move {
            let result = match ctx.resolve_audio(&track.id).await {
                Ok(path) if ctx.play_gen.load(Ordering::SeqCst) == generation => {
                    let item = TrackQueueItem {
                        id: track.id.clone(),
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        file_path: path,
                        duration: Duration::from_secs(track.duration_secs as u64),
                    };
                    let player_ctx = ctx.clone();
                    let t_play = std::time::Instant::now();
                    let play_res = tokio::task::spawn_blocking(move || {
                        player_ctx.player.play_track_at(item, start_at).map_err(|e| e.to_string())
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    tracing::info!(track_id = %track.id, setup_ms = t_play.elapsed().as_millis(), "Audio player initialized");
                    play_res
                }
                Ok(_) => return, // superseded by a newer request
                Err(e) => Err(e),
            };
            let _ = ctx.meta.upsert_track(&track);
            let _ = ctx.meta.record_play(&track.id);
            on_ui(move |ui| {
                if ui.ctx.play_gen.load(Ordering::SeqCst) != generation {
                    return;
                }
                {
                    let mut st = ui.state.borrow_mut();
                    st.loading_track = false;
                    st.last_serial = ui.ctx.player.track_serial();
                }
                let total_buffering_ms = t_buffering_start.elapsed().as_millis();
                let app = ui.app();
                app.set_buffering(false);
                match result {
                    Ok(()) => {
                        tracing::info!(track_id = %track.id, total_buffering_ms, "Buffering complete, playback starting");
                        app.set_playing(true);
                        ui.present_format();
                        let elapsed = ui.ctx.player.elapsed();
                        ui.ctx.with_media(|m| {
                            let _ = m.set_playback_state(true, Some(elapsed));
                        });
                        ui.persist_cursor();
                    }
                    Err(e) => {
                        tracing::error!(track_id = %track.id, total_buffering_ms, "Could not play {}: {e}", track.title);
                        app.set_np_artist(format!("Couldn't play this track ({e})").into());
                    }
                }
            });
        });
    }

    fn toggle_play(self: &Rc<Self>) {
        if self.state.borrow().loading_track {
            return;
        }
        let player = &self.ctx.player;
        if player.is_playing() {
            player.pause();
        } else if player.is_finished() || player.current_id().is_none() {
            if self.current_track().is_some() {
                let start_at = if player.is_finished() {
                    Duration::ZERO
                } else {
                    self.state.borrow_mut().pending_seek.take().unwrap_or(Duration::ZERO)
                };
                self.start_current_at(start_at);
            } else {
                // Nothing queued yet: start the list on screen.
                self.play_all(false);
            }
            return;
        } else {
            player.resume();
        }
        let playing = player.is_playing();
        self.app().set_playing(playing);
        let elapsed = player.elapsed();
        self.ctx.with_media(|m| {
            let _ = m.set_playback_state(playing, Some(elapsed));
        });
        self.persist_cursor();
    }

    fn skip(self: &Rc<Self>, _delta: i32, manual: bool) {
        match self.following(!manual) {
            Some(next) => {
                {
                    let mut st = self.state.borrow_mut();
                    st.pos = next;
                    st.pending_seek = None;
                }
                self.persist_cursor();
                self.start_current();
            }
            None => {
                self.ctx.player.pause();
                self.app().set_playing(false);
                self.ctx.with_media(|m| {
                    let _ = m.set_playback_state(false, Some(Duration::ZERO));
                });
                self.persist_cursor();
            }
        }
    }

    fn previous(self: &Rc<Self>) {
        if self.effective_elapsed() > Duration::from_secs(3) || self.state.borrow().pos == 0 {
            self.seek(0.0);
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            st.pos -= 1;
            st.pending_seek = None;
        }
        self.persist_cursor();
        self.start_current();
    }

    fn seek(self: &Rc<Self>, fraction: f32) {
        let Some(track) = self.current_track() else { return };
        let clamped = fraction.clamp(0.0, 1.0);
        let to = Duration::from_secs_f32(track.duration_secs as f32 * clamped);
        if self.ctx.player.seek(to) {
            let app = self.app();
            app.set_progress(clamped);
            app.set_elapsed(fmt_time(to.as_secs()).into());
            let playing = self.ctx.player.is_playing();
            self.ctx.with_media(|m| {
                let _ = m.set_playback_state(playing, Some(to));
            });
            self.persist_cursor();
        } else if self.ctx.player.current_id().is_none() {
            // Audio isn't open yet (e.g. restored session while paused): stage the new offset.
            self.state.borrow_mut().pending_seek = Some(to);
            let app = self.app();
            app.set_progress(clamped);
            app.set_elapsed(fmt_time(to.as_secs()).into());
            self.ctx.with_media(|m| {
                let _ = m.set_playback_state(false, Some(to));
            });
            self.persist_cursor();
        }
    }

    fn seek_to(self: &Rc<Self>, to: Duration) {
        let Some(track) = self.current_track() else { return };
        let total = track.duration_secs as f32;
        let fraction = if total > 0.0 { to.as_secs_f32() / total } else { 0.0 };
        self.seek(fraction);
    }

    fn set_volume(&self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        {
            let mut st = self.state.borrow_mut();
            st.muted = false;
            st.volume_before_mute = volume;
        }
        self.ctx.player.set_volume(volume);
        let app = self.app();
        app.set_volume(volume);
        app.set_muted(false);
        self.present_signal();
        self.persist_cursor();
    }

    fn toggle_mute(&self) {
        let app = self.app();
        let mut st = self.state.borrow_mut();
        st.muted = !st.muted;
        if st.muted {
            st.volume_before_mute = app.get_volume();
            self.ctx.player.set_volume(0.0);
        } else {
            self.ctx.player.set_volume(st.volume_before_mute);
        }
        app.set_muted(st.muted);
        drop(st);
        self.present_signal();
        self.persist_cursor();
    }

    fn toggle_shuffle(self: &Rc<Self>) {
        let (shuffle, order) = {
            let mut st = self.state.borrow_mut();
            st.shuffle = !st.shuffle;
            let current = st.order.get(st.pos).copied().unwrap_or(0);
            if st.shuffle {
                st.order = shuffled(st.queue.len(), current);
                st.pos = 0;
            } else {
                st.order = (0..st.queue.len()).collect();
                st.pos = current;
            }
            st.prefetched_for = None;
            (st.shuffle, st.order.clone())
        };
        // Whatever was pre-loaded for a gapless hand-over is no longer "next".
        self.ctx.player.clear_queue();
        self.app().set_shuffle(shuffle);
        let _ = self.ctx.meta.update_playback_order(&order, self.current_cursor());
        self.refresh_queue_page();
    }

    fn cycle_repeat(&self) {
        let mode = {
            let mut st = self.state.borrow_mut();
            st.repeat = (st.repeat + 1) % 3;
            st.prefetched_for = None;
            st.repeat
        };
        self.ctx.player.clear_queue();
        self.app().set_repeat_mode(mode);
        self.persist_cursor();
    }

    /// Called ~4x a second: progress, gapless prefetch and end-of-track handling.
    pub fn tick(self: &Rc<Self>) {
        let app = self.app();
        if !app.get_has_track() || self.state.borrow().loading_track {
            return;
        }
        let player = &self.ctx.player;

        // Follow the system output (headphones plugged in, Bluetooth connected, ...).
        let (check_output, save_tick, idle_ticks) = {
            let mut st = self.state.borrow_mut();
            st.tick_count = st.tick_count.wrapping_add(1);
            st.idle_ticks = if player.is_playing() { 0 } else { st.idle_ticks.saturating_add(1) };
            (st.tick_count % 8 == 0, st.tick_count % 20 == 0, st.idle_ticks)
        };
        if check_output && player.sync_output() {
            self.present_signal();
        }
        if check_output {
            let bitrate = player.format_info().map_or(0, |f| f.bitrate_kbps);
            if bitrate != self.state.borrow().shown_bitrate {
                self.present_signal();
            }
        }
        // Paused for a few seconds: give the device back at its original sample
        // rate so other apps are not left with a re-clocked output.
        if idle_ticks == IDLE_TICKS_BEFORE_RELEASE {
            self.spawn(|ctx| async move {
                let released = tokio::task::spawn_blocking(move || ctx.player.release_output()).await.unwrap_or(false);
                if released {
                    on_ui(|ui| ui.present_signal());
                }
            });
        }
        if idle_ticks == 1 || (idle_ticks == 0 && player.is_playing() && self.app().get_signal_note().starts_with("Paused")) {
            self.present_signal();
        }

        self.sync_gapless_transition();

        if player.is_finished() {
            self.skip(1, false);
            return;
        }
        if player.current_id().is_none() {
            // Restored session before audio has been opened: keep the saved elapsed/scrubber untouched.
            return;
        }

        let Some(track) = self.current_track() else { return };
        let elapsed = player.elapsed();
        let total = Duration::from_secs(track.duration_secs.max(1) as u64);
        app.set_playing(player.is_playing());
        app.set_elapsed(fmt_time(elapsed.as_secs().min(total.as_secs())).into());
        app.set_progress((elapsed.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0));
        if player.is_playing() && save_tick {
            self.persist_cursor();
        }

        if player.is_playing() && total.saturating_sub(elapsed) < PREFETCH_BEFORE_END {
            self.prefetch_next();
        }
        if app.get_show_now_playing() {
            self.update_lyric_line(elapsed);
        }
    }

    fn sync_gapless_transition(self: &Rc<Self>) {
        // The engine moved on to the pre-loaded track by itself (gapless).
        let serial = self.ctx.player.track_serial();
        if serial != self.state.borrow().last_serial {
            let next = self.following(true);
            {
                let mut st = self.state.borrow_mut();
                st.last_serial = serial;
                st.prefetched_for = None;
                if let Some(next) = next {
                    st.pos = next;
                }
            }
            self.present_current();
            self.present_format();
            self.ctx.with_media(|m| {
                let _ = m.set_playback_state(self.ctx.player.is_playing(), Some(self.ctx.player.elapsed()));
            });
            if let Some(track) = self.current_track() {
                let _ = self.ctx.meta.record_play(&track.id);
            }
            self.persist_cursor();
            self.refresh_queue_page();
        }
    }

    /// Download and decode-queue the next track so the hand-over is gapless.
    fn prefetch_next(self: &Rc<Self>) {
        let generation = self.ctx.play_gen.load(Ordering::SeqCst);
        let revision = *self.ctx.next_revision.lock().unwrap();
        let serial = self.ctx.player.track_serial();
        let next = {
            let mut st = self.state.borrow_mut();
            if st.prefetched_for == Some(generation) {
                return;
            }
            st.prefetched_for = Some(generation);
            drop(st);
            self.following(true)
        };
        let st = self.state.borrow();
        let Some(track) = next.and_then(|p| st.order.get(p)).and_then(|i| st.queue.get(*i)).cloned() else { return };
        drop(st);
        // Full fetch, no debug UI: progressive `.tmp` enqueue would end the next
        // track early the same way, and chunk callbacks would stack on the panel.
        self.spawn(|ctx| async move {
            match ctx.resolve_audio_for(&track.id, false).await {
                Ok(path) if ctx.play_gen.load(Ordering::SeqCst) == generation => {
                    let revision_guard = ctx.next_revision.lock().unwrap();
                    if *revision_guard != revision
                        || ctx.play_gen.load(Ordering::SeqCst) != generation
                        || ctx.player.track_serial() != serial
                    {
                        return;
                    }
                    let _queued_gapless = ctx.player.enqueue_next(TrackQueueItem {
                        id: track.id.clone(),
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        file_path: path,
                        duration: Duration::from_secs(track.duration_secs as u64),
                    });
                    drop(revision_guard);
                    let _ = ctx.meta.upsert_track(&track);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("prefetch failed: {e}"),
            }
        });
    }

    pub fn media_event(self: &Rc<Self>, event: MediaControlEvent) {
        match event {
            MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => self.toggle_play(),
            MediaControlEvent::Next => self.skip(1, true),
            MediaControlEvent::Previous => self.previous(),
            MediaControlEvent::Stop => {
                if self.ctx.player.is_playing() {
                    self.toggle_play();
                }
            }
            MediaControlEvent::SetPosition(pos) => self.seek_to(pos.0),
            _ => {}
        }
    }

    // ------------------------------------------------------------ favourites

    fn refresh_now_playing_favorite(&self) {
        let app = self.app();
        let id = app.get_current_id().to_string();
        app.set_np_favorite(self.state.borrow().fav_ids.contains(&id));
    }

    fn mark_favorite(self: &Rc<Self>, id: &str, favorite: bool) {
        let model = {
            let mut st = self.state.borrow_mut();
            if favorite {
                if st.fav_ids.insert(id.to_string()) {
                    let known = st.menu_track.iter().chain(st.page_tracks.iter()).chain(st.queue.iter())
                        .find(|t| t.id == id).cloned();
                    if let Some(mut track) = known {
                        track.date_added = Some("Today".into());
                        st.favorites.insert(0, track);
                    }
                }
            } else {
                st.fav_ids.remove(id);
                st.favorites.retain(|t| t.id != id);
            }
            st.tracks_model.clone()
        };
        if let Some(model) = model {
            for i in 0..model.row_count() {
                if let Some(mut row) = model.row_data(i).filter(|r| r.id == id) {
                    row.favorite = favorite;
                    model.set_row_data(i, row);
                }
            }
        }
        self.refresh_now_playing_favorite();
        if self.app().get_view() == "tracks" && self.app().get_nav_key() == "tracks" {
            let filter = self.app().get_filter_text();
            self.apply_filter(&filter);
        }
    }

    fn toggle_favorite(self: &Rc<Self>, id: &str) {
        if id.is_empty() {
            return;
        }
        let favorite = !self.state.borrow().fav_ids.contains(id);
        self.mark_favorite(id, favorite);
        let id = id.to_string();
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else { return };
            if let Err(e) = api.set_favorite(&id, favorite).await {
                tracing::warn!("favorite toggle failed: {e}");
                on_ui(move |ui| ui.mark_favorite(&id, !favorite));
            }
        });
    }

    // ------------------------------------------------------------ now playing / lyrics

    fn toggle_now_playing(self: &Rc<Self>) {
        let app = self.app();
        if !app.get_has_track() {
            return;
        }
        let show = !app.get_show_now_playing();
        app.set_show_now_playing(show);
        if show {
            self.load_lyrics();
        }
    }

    fn load_lyrics(self: &Rc<Self>) {
        let Some(track) = self.current_track() else { return };
        if self.state.borrow().lyrics_for == track.id {
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            st.lyrics_for = track.id.clone();
            st.lyrics = Lyrics::default();
            st.lyric_idx = None;
        }
        let app = self.app();
        app.set_lyrics(ModelRc::default());
        app.set_lyrics_status("".into());
        let id = track.id.clone();
        self.spawn(|ctx| async move {
            let cache_key = format!("lyrics:{id}");
            let cached: Option<Lyrics> = ctx.meta.get_json(&cache_key).and_then(|j| serde_json::from_str(&j).ok());
            let lyrics = match (cached, ctx.api()) {
                (Some(lyrics), _) => Some(lyrics),
                (None, Some(api)) => api.lyrics(&id).await.ok().inspect(|l| {
                    let _ = ctx.meta.put_json(&cache_key, &serde_json::to_string(l).unwrap_or_default());
                }),
                (None, None) => None,
            };
            on_ui(move |ui| {
                if ui.state.borrow().lyrics_for != id {
                    return;
                }
                let lyrics = lyrics.unwrap_or_default();
                let empty = lyrics.plain.trim().is_empty() && lyrics.synced.is_empty();
                ui.app().set_lyrics_status(if empty { "No lyrics available for this track" } else { "" }.into());
                if lyrics.synced.is_empty() && !empty {
                    let lines: Vec<LyricLine> = lyrics
                        .plain
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .take(8)
                        .map(|l| LyricLine { text: l.trim().into(), active: false })
                        .collect();
                    ui.app().set_lyrics(ModelRc::from(Rc::new(VecModel::from(lines))));
                }
                ui.state.borrow_mut().lyrics = lyrics;
                ui.update_lyric_line(ui.effective_elapsed());
            });
        });
    }

    /// Show a window of lines around the one being sung.
    fn update_lyric_line(&self, elapsed: Duration) {
        let lines = {
            let mut st = self.state.borrow_mut();
            if st.lyrics.synced.is_empty() {
                return;
            }
            let ms = elapsed.as_millis() as u64;
            let idx = st.lyrics.synced.iter().rposition(|(t, _)| *t <= ms);
            if idx == st.lyric_idx && st.lyric_idx.is_some() {
                return;
            }
            st.lyric_idx = idx.or(Some(usize::MAX));
            let current = idx.unwrap_or(0);
            let start = current.saturating_sub(2);
            st.lyrics.synced[start..]
                .iter()
                .enumerate()
                .filter(|(_, (_, text))| !text.is_empty())
                .take(7)
                .map(|(i, (_, text))| LyricLine { text: text.as_str().into(), active: idx == Some(start + i) })
                .collect::<Vec<_>>()
        };
        self.app().set_lyrics(ModelRc::from(Rc::new(VecModel::from(lines))));
    }

    // ------------------------------------------------------------ session

    fn login(self: &Rc<Self>) {
        self.app().set_auth_status("Contacting TIDAL…".into());
        self.spawn(|ctx| async move {
            let mut svc = ctx.svc.lock().await;
            let info = match svc.start_device_auth().await {
                Ok(info) => info,
                Err(e) => {
                    on_ui(move |ui| ui.app().set_auth_status(format!("Couldn't start login: {e}").into()));
                    return;
                }
            };
            let url = if info.verification_uri_complete.starts_with("http") {
                info.verification_uri_complete.clone()
            } else {
                format!("https://{}", info.verification_uri_complete)
            };
            let (code, link) = (info.user_code.clone(), url.clone());
            on_ui(move |ui| {
                let app = ui.app();
                app.set_user_code(code.into());
                app.set_verification_url(link.into());
                app.set_auth_status("Waiting for approval in your browser…".into());
            });
            let _ = open::that(&url);
            match svc.poll_device_auth(&info.device_code, info.expires_in, info.interval).await {
                Ok(_) => {
                    drop(svc);
                    on_ui(|ui| {
                        ui.app().set_user_code("".into());
                        ui.app().set_auth_status("".into());
                        ui.boot(true);
                    });
                }
                Err(e) => on_ui(move |ui| ui.app().set_auth_status(format!("Login failed: {e}").into())),
            }
        });
    }

    fn logout(self: &Rc<Self>) {
        self.app().set_menu_open(false);
        self.ctx.play_gen.fetch_add(1, Ordering::SeqCst);
        self.ctx.player.pause();
        self.ctx.player.clear_queue();
        *self.ctx.api.write().unwrap() = None;
        let _ = self.ctx.meta.clear_json();
        let _ = self.ctx.meta.clear_playback_state();
        *self.state.borrow_mut() = State { volume_before_mute: 1.0, ..Default::default() };
        let app = self.app();
        app.set_logged_in(false);
        app.set_has_track(false);
        app.set_duration_secs(0);
        app.set_progress(0.0);
        app.set_elapsed("0:00".into());
        app.set_total("0:00".into());
        app.set_playing(false);
        app.set_current_id("".into());
        app.set_user_code("".into());
        app.set_auth_status("".into());
        app.set_sections(ModelRc::default());
        app.set_tracks(ModelRc::default());
        app.set_grid_cards(ModelRc::default());
        app.set_sidebar_playlists(ModelRc::default());
        self.spawn(|ctx| async move {
            let mut svc = ctx.svc.lock().await;
            if let Err(e) = svc.revoke().await {
                tracing::warn!("could not revoke the session with TIDAL: {e}");
            }
            let _ = svc.logout();
        });
    }

    /// Keep the API handle fresh (access tokens expire after a few hours).
    pub fn start_token_refresh(self: &Rc<Self>) {
        self.spawn(|ctx| async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20 * 60)).await;
                if ctx.api().is_none() {
                    continue;
                }
                let mut svc = ctx.svc.lock().await;
                match svc.refresh().await {
                    Ok(()) => *ctx.api.write().unwrap() = svc.handle(),
                    Err(e) => tracing::warn!("token refresh failed: {e}"),
                }
            }
        });
    }

    // ------------------------------------------------------------ self-test

    /// Drive the UI through its main views and save a PNG of each (`--snapshot <dir>`).
    pub fn run_snapshot_tour(self: &Rc<Self>, dir: PathBuf, play: bool) {
        type Step = (u64, &'static str, Box<dyn Fn(&Rc<Ui>)>);
        let mut steps: Vec<Step> = vec![
            (7000, "01-home", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("tracks"))),
            (3000, "02-tracks", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("mixes"))),
            (4000, "03-mixes", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("playlists"))),
            (4000, "04-playlists", Box::new(|_| {})),
            (200, "", Box::new(|ui| {
                let cards = ui.app().get_grid_cards();
                if let Some(card) = cards.iter().find(|c| c.kind == "playlist") {
                    ui.open_card(card);
                }
            })),
            (4000, "04a-playlist-detail", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("profiles"))),
            (4000, "04b-profiles", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("albums"))),
            (4000, "05-albums", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.set_grid_arrangement(None, Some("Artist".into())))),
            (1500, "05b-albums-by-artist", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.set_grid_arrangement(None, Some("None".into())))),
            (
                200,
                "",
                Box::new(|ui| {
                    let cards = ui.app().get_grid_cards();
                    if let Some(card) = cards.row_data(0) {
                        ui.open_card(card);
                    }
                }),
            ),
            (4000, "06-album-detail", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.app().set_scroll_y(-520.0))),
            (2500, "06-album-footer", Box::new(|_| {})),
            (200, "", Box::new(|ui| {
                ui.app().set_scroll_y(0.0);
                ui.app().set_debug_open(true);
                tracing::error!(track_id = 1, "Snapshot tour: sample error alert");
            })),
            (800, "06-debug-panel", Box::new(|_| {})),
            // The shared context menu: on the album (More), then on a track.
            (200, "", Box::new(|ui| {
                ui.app().set_debug_open(false);
                ui.app().set_alert_text("".into());
                let card = ui.app().get_page_card();
                ui.open_card_menu(card, 900.0, 300.0);
            })),
            (1500, "06-menu-album", Box::new(|_| {})),
            (100, "", Box::new(|ui| ui.app().set_menu_sub("playlists".into()))),
            (500, "06-menu-album-playlists", Box::new(|_| {})),
            (200, "", Box::new(|ui| {
                ui.app().set_menu_open(false);
                let id = ui.state.borrow().page_tracks.first().map(|t| t.id.clone());
                if let Some(id) = id {
                    ui.open_track_menu(&id, 700.0, 380.0);
                }
            })),
            (1000, "06-menu-track", Box::new(|_| {})),
            (100, "", Box::new(|ui| ui.app().set_menu_open(false))),
            (200, "", Box::new(|ui| {
                ui.app().set_debug_open(false);
                ui.app().set_alert_text("".into());
                ui.app().set_create_kind("playlist".into());
            })),
            (600, "06-create-dialog", Box::new(|_| {})),
            (100, "", Box::new(|ui| ui.app().set_create_kind("".into()))),
            (200, "", Box::new(|ui| {
                let app = ui.app();
                let artist = app.get_page_artist();
                if !artist.id.is_empty() {
                    app.invoke_open_card(artist);
                } else if let Some(track) = app.get_tracks().row_data(0) {
                    app.invoke_open_track_artist(track);
                }
            })),
            (4000, "06a-artist-detail", Box::new(|_| {})),
            (200, "", Box::new(|ui| {
                if ui.app().get_view() == "artist" {
                    ui.app().invoke_show_artist_tracks();
                }
            })),
            (1000, "06b-artist-tracks", Box::new(|_| {})),
            // Back / forward panes: caught mid-flight, held by a swipe, and settled.
            (200, "", Box::new(|ui| ui.app().set_scroll_y(-600.0))),
            (300, "", Box::new(|ui| ui.app().invoke_back())),
            (75, "06c-back-midway", Box::new(|_| {})),
            (1500, "06d-back-arrived", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.app().invoke_forward())),
            (75, "06e-forward-midway", Box::new(|_| {})),
            // Should report scroll=-600: the page comes back where it was left.
            (1500, "06e2-forward-arrived", Box::new(|_| {})),
            (200, "06f-swipe-held", Box::new(|ui| {
                if ui.swipe_begin(true) {
                    ui.swipe_move(ui.app().get_pane_width() * 0.4);
                }
            })),
            (200, "", Box::new(|ui| ui.swipe_end(true))),
            (800, "06g-swipe-called-off", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.app().invoke_search("grammy".into()))),
            (4000, "07-search", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("settings"))),
            (1500, "08-settings", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.app().set_settings_tab(1))),
            (1000, "09-settings-account", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("tracks"))),
        ];
        if play {
            // Exercise the whole audio pipeline silently.
            steps.push((100, "", Box::new(|ui| ui.set_volume(0.0))));
            steps.push((
                1500,
                "",
                Box::new(|ui| {
                    let id = ui.state.borrow().page_tracks.first().map(|t| t.id.clone());
                    if let Some(id) = id {
                        ui.play_from_page(&id);
                    }
                }),
            ));
            steps.push((14_000, "10-playing", Box::new(|_| {})));
            steps.push((200, "", Box::new(|ui| ui.seek(0.5))));
            steps.push((2500, "11-after-seek", Box::new(|_| {})));
            steps.push((200, "", Box::new(|ui| ui.toggle_now_playing())));
            steps.push((5000, "12-now-playing", Box::new(|_| {})));
            steps.push((200, "", Box::new(|ui| ui.skip(1, true))));
            steps.push((12_000, "13-next-track", Box::new(|_| {})));
        }
        let _ = std::fs::create_dir_all(&dir);
        fn run(ui: Rc<Ui>, mut steps: std::vec::IntoIter<Step>, dir: PathBuf) {
            let Some((delay, name, action)) = steps.next() else {
                let _ = slint::quit_event_loop();
                return;
            };
            slint::Timer::single_shot(Duration::from_millis(delay), move || {
                action(&ui);
                if !name.is_empty() {
                    match ui.win.window().take_snapshot() {
                        Ok(buf) => {
                            let path = dir.join(format!("{name}.png"));
                            let saved = image::save_buffer(
                                &path,
                                buf.as_bytes(),
                                buf.width(),
                                buf.height(),
                                image::ExtendedColorType::Rgba8,
                            );
                            let app = ui.app();
                            println!(
                                "snapshot {name}: {saved:?} view={} scroll={} tracks={} cards={} sections={} playing={} elapsed={} quality={} signal={:?} output={:?}",
                                app.get_view(),
                                app.get_scroll_y(),
                                app.get_tracks().row_count(),
                                app.get_grid_cards().row_count(),
                                app.get_sections().row_count(),
                                app.get_playing(),
                                app.get_elapsed(),
                                app.get_quality_label(),
                                app.get_signal_title(),
                                app.get_output_info(),
                            );
                        }
                        Err(e) => println!("snapshot {name} failed: {e}"),
                    }
                }
                run(ui, steps, dir);
            });
        }
        run(self.clone(), steps.into_iter(), dir);
    }
}
