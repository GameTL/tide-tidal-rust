//! UI controller: owns navigation, the play queue and all Slint models.
//!
//! Everything in [`Ui`] lives on the Slint event-loop thread. Background work
//! runs on tokio with only the `Send` [`Ctx`] and reports back via [`on_ui`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use slint::{Color, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tide_api::{ApiHandle, Card, CatalogDetail, Lyrics, Section, TidalService, TrackItem};
use tide_audio::{AudioPlayer, TrackQueueItem};
use tide_cache::{AudioCache, ImageCache, MetadataCache};
use tide_macos::{MacMediaControls, MediaControlEvent};

use crate::signal::{khz, signal_path};
use crate::{App, CardData, LyricLine, MainWindow, SectionData, SignalStage, TrackData};

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
    /// Render on the CPU (about half the memory of the GPU renderer).
    pub low_memory: bool,
    pub show_collection: bool,
    pub show_audio_metadata: bool,
    /// Size limit of the offline audio cache.
    pub cache_max_gb: f64,
    /// Re-download the most played tracks when they are missing from the cache.
    pub keep_most_played: bool,
    /// Switch the output device to each track's sample rate (bit-perfect).
    pub rate_matching: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            quality: "max".into(),
            low_memory: false,
            show_collection: true,
            show_audio_metadata: true,
            cache_max_gb: 20.0,
            keep_most_played: true,
            rate_matching: true,
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
    fn loads_display_defaults_from_existing_settings() {
        let meta = MetadataCache::in_memory().unwrap();
        assert!(!Settings::load(&meta).low_memory);
        meta.put_json("settings", r#"{"quality":"high","low_memory":true}"#).unwrap();

        let settings = Settings::load(&meta);
        assert_eq!(settings.quality, "high");
        assert!(settings.low_memory);
        assert!(settings.show_collection);
        assert!(settings.show_audio_metadata);
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
        if let Some(path) = cached {
            return Ok(path);
        }
        let api = self.api().ok_or("offline and not cached")?;
        let stream = api.stream(track_id, &quality).await.map_err(|e| e.to_string())?;
        tracing::info!(track_id, quality = %stream.quality, bits = stream.bit_depth, rate = stream.sample_rate, "resolved stream");
        let path = self.audio.fetch_or_cache(track_id, &cache_key, &stream.urls).await.map_err(|e| e.to_string())?;
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
    Folder { id: String, title: String },
    Detail(Card),
    ArtistTracks(Card),
    Search(String),
    Queue,
    Settings,
    Stub { key: String, title: String },
}

#[derive(Default)]
struct State {
    history: Vec<Page>,
    hist_pos: usize,
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
    prefetched_for: Option<u64>,
    last_serial: u64,
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

pub struct Ui {
    win: MainWindow,
    ctx: Arc<Ctx>,
    state: RefCell<State>,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

/// Run `f` on the UI thread with the controller.
pub fn on_ui(f: impl FnOnce(&Rc<Ui>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = UI.with(|u| u.borrow().clone()) {
            f(&ui);
        }
    });
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
        image: Image::default(),
        image_url: c.image_url.as_deref().unwrap_or_default().into(),
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
        });
        UI.with(|u| *u.borrow_mut() = Some(ui.clone()));
        ui.wire();
        ui
    }

    fn app(&self) -> App<'_> {
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
        bind!(on_set_low_memory, |ui, on| ui.update_settings(|s| s.low_memory = on));
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
        app.set_low_memory(settings.low_memory);
        app.set_show_collection(settings.show_collection);
        app.set_show_audio_metadata(settings.show_audio_metadata);
        app.set_keep_most_played(settings.keep_most_played);
        app.set_rate_matching(settings.rate_matching);
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
        self.app().set_grid_cards(ModelRc::from(self.cards_model(cards, 320)));
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
            "profiles" => Page::Stub { key: key.into(), title: "Profiles".into() },
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
            kind => Page::Detail(Card {
                kind: kind.to_string(),
                id: card.id.to_string(),
                title: card.title.to_string(),
                subtitle: card.subtitle.to_string(),
                image_url: (!card.image_url.is_empty()).then(|| card.image_url.to_string()),
            }),
        };
        self.show(page, true);
    }

    fn step_history(self: &Rc<Self>, delta: isize) {
        let page = {
            let mut st = self.state.borrow_mut();
            let target = st.hist_pos as isize + delta;
            if target < 0 || target as usize >= st.history.len() {
                return;
            }
            st.hist_pos = target as usize;
            st.history[st.hist_pos].clone()
        };
        self.show(page, false);
    }

    fn show(self: &Rc<Self>, page: Page, push: bool) {
        let page_gen = {
            let mut st = self.state.borrow_mut();
            if push {
                let keep = if st.history.is_empty() { 0 } else { st.hist_pos + 1 };
                st.history.truncate(keep);
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
            Page::Mixes => self.show_grid("mixes", "Mixes & Radio", page_gen, |api| async move { api.mixes().await }),
            Page::Playlists => {
                self.show_grid("playlists", "Playlists", page_gen, |api| async move { api.playlists("root").await })
            }
            Page::Albums => self.show_grid("albums", "Albums", page_gen, |api| async move { api.albums().await }),
            Page::Folder { id, title } => {
                let key = format!("folder:{id}");
                self.show_grid(&key, &title, page_gen, move |api| async move { api.playlists(&id).await });
                app.set_nav_key("playlists".into());
            }
            Page::Detail(card) => {
                let Card { kind, id, title, .. } = &card;
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
                if matches!(view, "artist" | "album") {
                    self.load_catalog_detail(kind, id, page_gen);
                }
                if view == "artist" {
                    self.load_artist_releases(id, page_gen);
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
                if let Some(artist) = detail.artist {
                    app.set_page_artist(card_row(&artist));
                }
                app.set_page_year(detail.year.into());
                app.set_page_quality(match detail.quality.as_str() {
                    "HI_RES_LOSSLESS" | "HIRES_LOSSLESS" => "MAX",
                    "LOSSLESS" => "LOSSLESS",
                    "HIGH" => "HIGH",
                    _ => "",
                }.into());
                if detail.track_count > 0 {
                    let label = if detail.track_count == 1 { "TRACK" } else { "TRACKS" };
                    app.set_page_summary(format!("{} {label}  ({})", detail.track_count, fmt_time(detail.duration)).into());
                }
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

    fn artist_release_art(self: &Rc<Self>, singles: bool, from: usize, upto: usize) {
        let app = self.app();
        let model = if singles { app.get_artist_singles() } else { app.get_artist_albums() };
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
    fn show_grid<F, Fut>(self: &Rc<Self>, key: &str, title: &str, page_gen: u64, fetch: F)
    where
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Vec<Card>, tide_api::ApiError>> + Send,
    {
        let app = self.app();
        app.set_view("grid".into());
        app.set_nav_key(key.into());
        app.set_page_title(title.into());
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
        }
        self.app().set_shuffle(self.state.borrow().shuffle);
        self.start_current();
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
        app.set_total(fmt_time(track.duration_secs as u64).into());
        app.set_elapsed("0:00".into());
        app.set_progress(0.0);
        if let Some(url) = &track.cover_url {
            let id = track.id.clone();
            let weak = Rc::downgrade(self);
            self.load_image(&sized_cover(url, 640), 640, move |img, tint| {
                let Some(ui) = weak.upgrade() else { return };
                if ui.app().get_current_id() == id.as_str() {
                    ui.app().set_np_cover(img.clone());
                    ui.app().set_np_cover_large(img.clone());
                    ui.app().set_np_tint(tint);
                }
            });
        } else {
            app.set_np_cover(Image::default());
            app.set_np_cover_large(Image::default());
        }
        if app.get_show_now_playing() {
            self.load_lyrics();
        }
        let cover = track.cover_url.as_deref().map(|u| sized_cover(u, 640));
        self.ctx.with_media(|m| {
            let _ = m.set_metadata(
                &track.title,
                &track.artist,
                &track.album,
                Some(Duration::from_secs(track.duration_secs as u64)),
                cover.as_deref(),
            );
        });
    }

    fn present_format(&self) {
        let app = self.app();
        let Some(format) = self.ctx.player.format_info() else { return };
        let khz = format.sample_rate as f32 / 1000.0;
        if format.bits_per_sample == 0 {
            app.set_quality_label("High".into());
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
            app.set_signal_quality("".into());
            return;
        };
        let volume = if app.get_muted() { 0.0 } else { app.get_volume() };
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
        let Some(track) = self.current_track() else { return };
        let generation = self.ctx.play_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.ctx.player.pause();
        self.ctx.player.clear_queue();
        {
            let mut st = self.state.borrow_mut();
            st.loading_track = true;
            st.prefetched_for = None;
        }
        self.present_current();
        let app = self.app();
        app.set_playing(false);
        app.set_buffering(true);

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
                    tokio::task::spawn_blocking(move || player_ctx.player.play_track(item).map_err(|e| e.to_string()))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
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
                let app = ui.app();
                app.set_buffering(false);
                match result {
                    Ok(()) => {
                        app.set_playing(true);
                        ui.present_format();
                        ui.ctx.with_media(|m| {
                            let _ = m.set_playback_state(true, Some(Duration::ZERO));
                        });
                    }
                    Err(e) => {
                        tracing::error!("Could not play {}: {e}", track.title);
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
                self.start_current();
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
    }

    fn skip(self: &Rc<Self>, _delta: i32, manual: bool) {
        match self.following(!manual) {
            Some(next) => {
                self.state.borrow_mut().pos = next;
                self.start_current();
            }
            None => {
                self.ctx.player.pause();
                self.app().set_playing(false);
            }
        }
    }

    fn previous(self: &Rc<Self>) {
        if self.ctx.player.elapsed() > Duration::from_secs(3) || self.state.borrow().pos == 0 {
            self.seek(0.0);
            return;
        }
        self.state.borrow_mut().pos -= 1;
        self.start_current();
    }

    fn seek(self: &Rc<Self>, fraction: f32) {
        let Some(track) = self.current_track() else { return };
        let to = Duration::from_secs_f32(track.duration_secs as f32 * fraction.clamp(0.0, 1.0));
        if self.ctx.player.seek(to) {
            let app = self.app();
            app.set_progress(fraction);
            app.set_elapsed(fmt_time(to.as_secs()).into());
            let playing = self.ctx.player.is_playing();
            self.ctx.with_media(|m| {
                let _ = m.set_playback_state(playing, Some(to));
            });
        }
    }

    fn set_volume(&self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.state.borrow_mut().muted = false;
        self.ctx.player.set_volume(volume);
        let app = self.app();
        app.set_volume(volume);
        app.set_muted(false);
        self.present_signal();
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
    }

    fn toggle_shuffle(self: &Rc<Self>) {
        let shuffle = {
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
            st.shuffle
        };
        // Whatever was pre-loaded for a gapless hand-over is no longer "next".
        self.ctx.player.clear_queue();
        self.app().set_shuffle(shuffle);
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
    }

    /// Called ~4x a second: progress, gapless prefetch and end-of-track handling.
    pub fn tick(self: &Rc<Self>) {
        let app = self.app();
        if !app.get_has_track() || self.state.borrow().loading_track {
            return;
        }
        let player = &self.ctx.player;

        // Follow the system output (headphones plugged in, Bluetooth connected, ...).
        let (check_output, idle_ticks) = {
            let mut st = self.state.borrow_mut();
            st.tick_count = st.tick_count.wrapping_add(1);
            st.idle_ticks = if player.is_playing() { 0 } else { st.idle_ticks.saturating_add(1) };
            (st.tick_count % 8 == 0, st.idle_ticks)
        };
        if check_output && player.sync_output() {
            self.present_signal();
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

        // The engine moved on to the pre-loaded track by itself (gapless).
        let serial = player.track_serial();
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
            if let Some(track) = self.current_track() {
                let _ = self.ctx.meta.record_play(&track.id);
            }
        }

        if player.is_finished() {
            self.skip(1, false);
            return;
        }

        let Some(track) = self.current_track() else { return };
        let elapsed = player.elapsed();
        let total = Duration::from_secs(track.duration_secs.max(1) as u64);
        app.set_playing(player.is_playing());
        app.set_elapsed(fmt_time(elapsed.as_secs().min(total.as_secs())).into());
        app.set_progress((elapsed.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0));

        if player.is_playing() && total.saturating_sub(elapsed) < PREFETCH_BEFORE_END {
            self.prefetch_next();
        }
        if app.get_show_now_playing() {
            self.update_lyric_line(elapsed);
        }
    }

    /// Download and decode-queue the next track so the hand-over is gapless.
    fn prefetch_next(self: &Rc<Self>) {
        let generation = self.ctx.play_gen.load(Ordering::SeqCst);
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
        self.spawn(|ctx| async move {
            match ctx.resolve_audio(&track.id).await {
                Ok(path) if ctx.play_gen.load(Ordering::SeqCst) == generation => {
                    let _queued_gapless = ctx.player.enqueue_next(TrackQueueItem {
                        id: track.id.clone(),
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        file_path: path,
                        duration: Duration::from_secs(track.duration_secs as u64),
                    });
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
                st.fav_ids.insert(id.to_string());
                let known = st.page_tracks.iter().chain(st.queue.iter()).find(|t| t.id == id).cloned();
                if let Some(mut track) = known {
                    track.date_added = Some("Today".into());
                    st.favorites.insert(0, track);
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
                ui.update_lyric_line(ui.ctx.player.elapsed());
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
        self.ctx.play_gen.fetch_add(1, Ordering::SeqCst);
        self.ctx.player.pause();
        self.ctx.player.clear_queue();
        *self.ctx.api.write().unwrap() = None;
        let _ = self.ctx.meta.clear_json();
        *self.state.borrow_mut() = State { volume_before_mute: 1.0, ..Default::default() };
        let app = self.app();
        app.set_logged_in(false);
        app.set_has_track(false);
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
            (200, "", Box::new(|ui| ui.navigate_key("albums"))),
            (4000, "05-albums", Box::new(|_| {})),
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
                                "snapshot {name}: {saved:?} view={} tracks={} cards={} sections={} playing={} elapsed={} quality={} signal={:?} output={:?}",
                                app.get_view(),
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
