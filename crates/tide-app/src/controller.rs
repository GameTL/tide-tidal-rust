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

use slint::{
    Color, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
};
use tide_api::{
    ApiHandle, ArtistRef, Card, CatalogDetail, FavoriteIds, LaterItems, Lyrics, Section,
    TidalService, TrackItem,
};
use tide_audio::{AudioPlayer, TrackQueueItem};
use tide_cache::{AudioCache, ImageCache, MetadataCache, PlaybackCursor, SavedPlayback};
use tide_macos::{MacMediaControls, MediaControlEvent};

use crate::signal::{khz, signal_path};
use crate::{
    App, ArtistLink, CardData, CreditLine, LyricLine, MainWindow, MemPart, MemSample, SectionData,
    SignalStage, TrackData,
};

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
    /// Collection grid artwork size: 0 smallest … 4 largest (3 is the original size).
    pub grid_size: i32,
    /// Trade the sliding page transitions and some rendering caches for memory.
    pub low_ram: bool,
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
            grid_size: 3,
            low_ram: true,
        }
    }
}

impl Settings {
    pub fn load(meta: &MetadataCache) -> Self {
        let mut settings: Self = meta
            .get_json("settings")
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        settings.grid_size = settings.grid_size.clamp(0, 4);
        settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_next_preserves_the_current_track_and_shuffled_order() {
        let track = |id: &str| TrackItem {
            id: id.into(),
            ..Default::default()
        };
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
        let play_order: Vec<_> = state
            .order
            .iter()
            .map(|&i| state.queue[i].id.as_str())
            .collect();
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
            groups
                .iter()
                .map(|(g, c)| (g.clone(), c.iter().map(|c| c.title.clone()).collect()))
                .collect()
        };
        let flat = |sort: &str| {
            titles(&arrange_grid(&cards, "", sort, "None", false))[0]
                .1
                .clone()
        };
        assert_eq!(flat("Date added"), ["apple", "Mango", "Zebra"]);
        assert_eq!(flat("A-Z"), ["apple", "Mango", "Zebra"]);
        assert_eq!(flat("Artist"), ["Mango", "Zebra", "apple"]);
        assert_eq!(flat("Release date"), ["Mango", "apple", "Zebra"]);

        // Filtering matches the title or who it is by.
        assert_eq!(
            titles(&arrange_grid(&cards, " lauv ", "A-Z", "None", false))[0].1,
            ["Mango", "Zebra"]
        );

        // Groups keep the sort order inside; years run newest first.
        assert_eq!(
            titles(&arrange_grid(&cards, "", "A-Z", "Artist", false)),
            [
                (
                    "Lauv".to_string(),
                    vec!["Mango".to_string(), "Zebra".into()]
                ),
                ("Weezer".into(), vec!["apple".into()])
            ]
        );
        let years: Vec<String> = arrange_grid(&cards, "", "A-Z", "Year", false)
            .into_iter()
            .map(|(g, _)| g)
            .collect();
        assert_eq!(years, ["2024", "2019"]);

        // Folders lead, in their own group when grouping.
        let mut with_folder = cards.clone();
        with_folder.push(Card {
            kind: "folder".into(),
            id: "f".into(),
            title: "Zzz".into(),
            ..Default::default()
        });
        assert_eq!(
            arrange_grid(&with_folder, "", "A-Z", "None", false)[0].1[0].title,
            "Zzz"
        );
        assert_eq!(
            arrange_grid(&with_folder, "", "A-Z", "Title", false)[0].0,
            "Folders"
        );
    }

    #[test]
    fn creator_groups_can_combine_everyone_else() {
        let playlist = |title: &str, by: &str| Card {
            kind: "playlist".into(),
            id: title.into(),
            title: title.into(),
            by: by.into(),
            ..Default::default()
        };
        let cards = vec![
            playlist("Mine", "Me"),
            playlist("A", "Alice"),
            playlist("B", "Bob"),
            Card {
                kind: "folder".into(),
                id: "f".into(),
                title: "Zzz".into(),
                ..Default::default()
            },
        ];
        let names = |combine: bool| {
            arrange_grid(&cards, "", "A-Z", "Creator", combine)
                .into_iter()
                .map(|(g, members)| {
                    (
                        g,
                        members.iter().map(|c| c.title.clone()).collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(false),
            [
                ("Folders".into(), vec!["Zzz".into()]),
                ("Me".into(), vec!["Mine".into()]),
                ("Alice".into(), vec!["A".into()]),
                ("Bob".into(), vec!["B".into()]),
            ]
        );
        assert_eq!(
            names(true),
            [
                ("Folders".into(), vec!["Zzz".into()]),
                ("Me".into(), vec!["Mine".into()]),
                ("Others".into(), vec!["A".into(), "B".into()]),
            ]
        );
        // Combining is a Creator-only switch; Artist grouping stays split.
        let albums = vec![
            Card {
                kind: "album".into(),
                title: "One".into(),
                by: "Lauv".into(),
                ..Default::default()
            },
            Card {
                kind: "album".into(),
                title: "Two".into(),
                by: "Weezer".into(),
                ..Default::default()
            },
        ];
        let artists: Vec<String> = arrange_grid(&albums, "", "A-Z", "Artist", true)
            .into_iter()
            .map(|(g, _)| g)
            .collect();
        assert_eq!(artists, ["Lauv", "Weezer"]);
    }

    #[test]
    fn release_dates_are_spelled_out() {
        assert_eq!(long_date("2026-11-12"), "NOVEMBER 12, 2026");
        assert_eq!(long_date("2024-07-19T00:00:00.000+0000"), "JULY 19, 2024");
        assert_eq!(long_date(""), "");
    }

    #[test]
    fn short_bio_hides_wimp_markup() {
        let raw = concat!(
            r#"The skilled U.K. genre-mashers [wimpLink artistId="4611746"]Glass Animals[/wimpLink] "#,
            r#"bring synth pop, indie, R&B, and hip-hop together in innovative and inviting ways. "#,
            r#"On 2014's [wimpLink albumId="30209241"]ZABA[/wimpLink], Dave Bayley's androgynous "#,
            r#"falsetto and surreal lyrics fit the album's rippling guitars."#,
        );
        let teaser = short_bio(raw);
        assert!(!teaser.contains("wimpLink"));
        assert!(!teaser.contains('['));
        assert!(teaser.starts_with("The skilled U.K. genre-mashers Glass Animals"));
        assert!(teaser.contains("ZABA"));
        assert_eq!(teaser.chars().count(), 168);
        assert!(teaser.ends_with('…'));
    }

    #[test]
    fn same_page_is_recognised_for_history() {
        let card = |kind: &str, id: &str| Card {
            kind: kind.into(),
            id: id.into(),
            ..Default::default()
        };
        assert!(Page::Home.same_as(&Page::Home));
        assert!(!Page::Home.same_as(&Page::Tracks));
        assert!(Page::Detail(card("album", "1")).same_as(&Page::Detail(card("album", "1"))));
        assert!(!Page::Detail(card("album", "1")).same_as(&Page::Detail(card("album", "2"))));
        assert!(
            !Page::Detail(card("artist", "1")).same_as(&Page::ArtistTracks(card("artist", "1")))
        );
        assert!(
            Page::ArtistReleases {
                artist: card("artist", "1"),
                singles: false
            }
            .same_as(&Page::ArtistReleases {
                artist: card("artist", "1"),
                singles: false
            })
        );
        assert!(
            !Page::ArtistReleases {
                artist: card("artist", "1"),
                singles: false
            }
            .same_as(&Page::ArtistReleases {
                artist: card("artist", "1"),
                singles: true
            })
        );
        assert!(
            !Page::ArtistReleases {
                artist: card("artist", "1"),
                singles: false
            }
            .same_as(&Page::ArtistReleases {
                artist: card("artist", "2"),
                singles: false
            })
        );
        assert!(!Page::Search("a".into()).same_as(&Page::Search("b".into())));
        assert!(Page::Explore.same_as(&Page::Explore));
        assert!(!Page::Explore.same_as(&Page::Feed));
        assert!(
            Page::Catalog {
                path: "pages/x".into(),
                title: "X".into()
            }
            .same_as(&Page::Catalog {
                path: "pages/x".into(),
                title: "Y".into()
            })
        );
        assert!(
            !Page::Catalog {
                path: "pages/x".into(),
                title: "X".into()
            }
            .same_as(&Page::Catalog {
                path: "pages/y".into(),
                title: "X".into()
            })
        );
    }

    #[test]
    fn artist_release_grid_has_no_grouping() {
        let (sorts, groups) = grid_options("artist-releases");
        assert_eq!(sorts, &["Release date", "A-Z"]);
        assert!(groups.is_empty());
    }

    #[test]
    fn card_row_stamps_collection_membership() {
        let album = Card {
            kind: "album".into(),
            id: "1".into(),
            title: "A".into(),
            ..Default::default()
        };
        let mut collection = Collection::default();
        collection.albums.insert("1".into());
        assert!(card_row_fav(&album, &collection).favorite);
        assert!(!card_row_fav(&album, &Collection::default()).favorite);
        let mix = Card {
            kind: "mix".into(),
            id: "1".into(),
            ..Default::default()
        };
        assert!(!card_row_fav(&mix, &collection).favorite);
        let video = Card {
            kind: "video".into(),
            id: "7".into(),
            ..Default::default()
        };
        collection.videos.insert("7".into());
        assert!(card_row_fav(&video, &collection).favorite);
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
        meta.put_json("settings", r#"{"quality":"high","rate_matching":false}"#)
            .unwrap();

        let settings = Settings::load(&meta);
        assert_eq!(settings.quality, "high");
        assert!(!settings.rate_matching);
        // Missing field stays on, so older settings files keep handing the device back.
        assert!(settings.restore_sample_rate);
        assert!(settings.show_collection);
        assert!(settings.show_audio_metadata);
        assert_eq!(settings.grid_size, 3);

        meta.put_json("settings", r#"{"restore_sample_rate":false}"#)
            .unwrap();
        assert!(!Settings::load(&meta).restore_sample_rate);
        assert_eq!(Settings::load(&meta).grid_size, 3);
    }

    #[test]
    fn low_ram_mode_is_on_unless_the_user_turned_it_off() {
        let meta = MetadataCache::in_memory().unwrap();
        assert!(Settings::load(&meta).low_ram);

        meta.put_json("settings", r#"{"quality":"high"}"#).unwrap();
        assert!(Settings::load(&meta).low_ram);

        meta.put_json("settings", r#"{"low_ram":false}"#).unwrap();
        assert!(!Settings::load(&meta).low_ram);
    }

    #[test]
    fn card_images_outside_the_keep_window_are_dropped() {
        i_slint_backend_testing::init_no_event_loop();
        let img = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(2, 2));
        let rows: Vec<CardData> = (0..5)
            .map(|i| CardData {
                id: i.to_string().into(),
                image: img.clone(),
                ..Default::default()
            })
            .collect();
        let model = ModelRc::new(VecModel::from(rows));
        drop_card_images_outside(&model, 1, 3);
        assert_eq!(model.row_data(0).unwrap().image.size().width, 0);
        assert_eq!(model.row_data(1).unwrap().image.size().width, 2);
        assert_eq!(model.row_data(2).unwrap().image.size().width, 2);
        assert_eq!(model.row_data(3).unwrap().image.size().width, 0);
        assert_eq!(model.row_data(4).unwrap().image.size().width, 0);
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
    async fn resolve_audio_for(
        &self,
        track_id: &str,
        for_playback: bool,
    ) -> Result<PathBuf, String> {
        let lock = self
            .downloads
            .lock()
            .unwrap()
            .entry(track_id.to_string())
            .or_default()
            .clone();
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
                show_chunks(
                    debug_gen,
                    "Cached on SSD".into(),
                    "local file · 0 ms".into(),
                    Vec::new(),
                    false,
                );
            }
            return Ok(path);
        }
        if for_playback && let Some(path) = self.audio.get_playable(track_id, &cache_key) {
            tracing::info!(track_id, %cache_key, "Using in-progress partial cache for playback");
            return Ok(path);
        }
        let t_api = std::time::Instant::now();
        let api = self.api().ok_or("offline and not cached")?;
        let stream = api
            .stream(track_id, &quality)
            .await
            .map_err(|e| e.to_string())?;
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
                show_chunks(
                    debug_gen,
                    status,
                    format!("{done}/{total} · {mb:.1} MB"),
                    snapshot,
                    active,
                );
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
                    show_chunks(
                        debug_gen,
                        format!("Failed · {e}"),
                        format!("{dl_ms} ms"),
                        blocks.lock().unwrap().clone(),
                        false,
                    );
                }
                return Err(e);
            }
        };
        tracing::info!(
            track_id,
            dl_ms,
            streaming = path.extension().and_then(|s| s.to_str()) == Some("tmp"),
            "Audio ready for player"
        );
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
            Ok(removed) => {
                tracing::info!(removed, "evicted least-played tracks from the audio cache")
            }
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
        if let Ok(mut guard) = self.media.lock()
            && let Some(media) = guard.as_mut()
        {
            f(media);
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
    Videos,
    Profiles,
    Folder { id: String, title: String },
    Detail(Card),
    ArtistTracks(Card),
    ArtistReleases { artist: Card, singles: bool },
    Search(String),
    Queue,
    Settings,
    Dsp,
    Explore,
    Feed,
    Later,
    Catalog { path: String, title: String },
}

impl Page {
    /// Whether both are the same destination, so history need not hold it twice in a row.
    fn same_as(&self, other: &Page) -> bool {
        match (self, other) {
            (Page::Detail(a), Page::Detail(b)) | (Page::ArtistTracks(a), Page::ArtistTracks(b)) => {
                a.kind == b.kind && a.id == b.id
            }
            (
                Page::ArtistReleases {
                    artist: a,
                    singles: sa,
                },
                Page::ArtistReleases {
                    artist: b,
                    singles: sb,
                },
            ) => a.id == b.id && sa == sb,
            (Page::Folder { id: a, .. }, Page::Folder { id: b, .. }) => a == b,
            (Page::Search(a), Page::Search(b)) => a == b,
            (Page::Catalog { path: a, .. }, Page::Catalog { path: b, .. }) => a == b,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

type SectionModel = (Rc<VecModel<CardData>>, Rc<VecModel<TrackData>>, usize);
type ImageWaiters = HashMap<String, Vec<Box<dyn FnOnce(&Image, Color)>>>;

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
    /// Album / playlist / video ids in My Collection, for cover-heart fill.
    collection: Collection,
    /// Card currently backing the play queue (`kind`, `id`).
    playing_card: Option<(String, String)>,
    later: LaterItems,
    sections: Vec<Section>,
    /// Per home section: card model, track model, and an unused loaded-count slot.
    section_models: Vec<SectionModel>,

    /// Tracks behind the table on screen (after filtering) and its model.
    page_tracks: Vec<TrackItem>,
    page_context: String,
    tracks_model: Option<Rc<VecModel<TrackData>>>,
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
    image_waiters: ImageWaiters,
    /// Visible artwork window [from, upto) per model key, so a late decode is dropped.
    art_keep: HashMap<String, (usize, usize)>,
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
    video: RefCell<video::VideoState>,
    dsp: RefCell<dsp::DspState>,
    mem: RefCell<MemTracker>,
}

/// RAM inspector state: one reading per second, kept for the tracker.
#[derive(Default)]
struct MemTracker {
    timer: slint::Timer,
    history: VecDeque<crate::mem::Breakdown>,
    peak: u64,
    samples: u32,
}

/// Seconds of history in the RAM tracker (matches `slots` in chrome.slint).
const MEM_SLOTS: usize = 90;
/// Inspector colours, bottom of the stack first: artwork, snapshots, heap,
/// window buffers, GPU and the rest.
const MEM_COLORS: [(u8, u8, u8); 5] = [
    (0x33, 0xff, 0xee),
    (0xff, 0xd4, 0x32),
    (0x3d, 0xd6, 0x8c),
    (0xf5, 0xb9, 0x42),
    (0x9b, 0x8c, 0xff),
];

#[path = "video.rs"]
mod video;

#[path = "dsp.rs"]
mod dsp;

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
    Settling {
        from: f32,
        to: f32,
        start: Instant,
        secs: f32,
        smooth: bool,
    },
    /// Arrived; the snapshot stays up until the live page has its content.
    Waiting {
        since: Instant,
    },
    Fading {
        start: Instant,
    },
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
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Whether a released swipe completes the page change. `p` is how far the pane
/// was pulled (0..=1) and `velocity` its speed in px/s towards completion.
fn swipe_commits(p: f32, velocity: f32) -> bool {
    // Past halfway and not being pulled back, or a clear flick.
    (p > 0.5 && velocity > -300.0) || (p > 0.02 && velocity > 600.0)
}

/// Cut `(x, y, w, h)` out of a window snapshot.
fn crop_pane(
    buf: &SharedPixelBuffer<Rgba8Pixel>,
    rect: (u32, u32, u32, u32),
) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let (x, y) = (rect.0.min(buf.width()), rect.1.min(buf.height()));
    let (w, h) = (rect.2.min(buf.width() - x), rect.3.min(buf.height() - y));
    if w == 0 || h == 0 {
        return None;
    }
    let src = buf.as_slice();
    let stride = buf.width() as usize;
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    for (row, line) in out
        .make_mut_slice()
        .chunks_exact_mut(w as usize)
        .enumerate()
    {
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
        if ui.video_active() || ui.ctx.play_gen.load(Ordering::SeqCst) != generation {
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
    /// Fold every playlist creator except Me into one "Others" group.
    combine_others: bool,
    /// Per-group card models on screen, so artwork can be filled and dropped by range.
    models: Vec<Rc<VecModel<CardData>>>,
}

/// Sort and group choices for a grid page; the first of each is the default.
fn grid_options(kind: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match kind {
        "albums" | "videos" => (
            &["Date added", "A-Z", "Artist", "Release date"],
            &["None", "Artist", "Year", "Title"],
        ),
        "playlists" => (
            &["A-Z", "Date added", "Creator"],
            &["None", "Creator", "Title"],
        ),
        "artist-releases" => (&["Release date", "A-Z"], &[]),
        _ => (&["Date added", "A-Z"], &["None", "Title"]),
    }
}

/// Filter, sort and split cards into titled groups (one untitled group for "None").
/// Folders stay ahead of playlists, as in the official client.
fn arrange_grid(
    cards: &[Card],
    filter: &str,
    sort: &str,
    group: &str,
    combine_others: bool,
) -> Vec<(String, Vec<Card>)> {
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
            "Year" => c
                .released
                .get(..4)
                .filter(|y| !y.is_empty())
                .unwrap_or("Unknown year")
                .to_string(),
            "Title" => match c.title.trim().chars().next() {
                Some(first) if first.is_alphabetic() => first.to_uppercase().collect(),
                _ => "#".into(),
            },
            "Creator" if combine_others && c.by != "Me" => "Others".into(),
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
        let by_name = if group == "Year" {
            b.cmp(a)
        } else {
            a.to_lowercase().cmp(&b.to_lowercase())
        };
        rank(a).cmp(&rank(b)).then(by_name)
    });
    groups
}

/// "2026-11-12" -> "NOVEMBER 12, 2026".
fn long_date(iso: &str) -> String {
    const MONTHS: [&str; 12] = [
        "JANUARY",
        "FEBRUARY",
        "MARCH",
        "APRIL",
        "MAY",
        "JUNE",
        "JULY",
        "AUGUST",
        "SEPTEMBER",
        "OCTOBER",
        "NOVEMBER",
        "DECEMBER",
    ];
    let mut parts = iso
        .get(..10)
        .unwrap_or("")
        .split('-')
        .filter_map(|p| p.parse::<usize>().ok());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m @ 1..=12), Some(d)) => format!("{} {d}, {y}", MONTHS[m - 1]),
        _ => String::new(),
    }
}

fn fmt_time(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Every credited artist as UI links; falls back to the single known artist for rows that
/// predate the full list. Empty when no artist id is known (nothing to link to).
fn artist_links(t: &TrackItem) -> Vec<ArtistLink> {
    if !t.artists.is_empty() {
        return refs_to_links(&t.artists);
    }
    t.artist_id
        .iter()
        .filter(|id| !id.is_empty())
        .map(|id| ArtistLink {
            id: id.as_str().into(),
            name: t.artist.as_str().into(),
        })
        .collect()
}

fn refs_to_links(refs: &[ArtistRef]) -> Vec<ArtistLink> {
    refs.iter()
        .map(|a| ArtistLink {
            id: a.id.as_str().into(),
            name: a.name.as_str().into(),
        })
        .collect()
}

fn link_model(links: Vec<ArtistLink>) -> ModelRc<ArtistLink> {
    ModelRc::new(VecModel::from(links))
}

fn track_row(t: &TrackItem, favorite: bool) -> TrackData {
    TrackData {
        id: t.id.as_str().into(),
        title: t.title.as_str().into(),
        artist: t.artist.as_str().into(),
        artist_id: t.artist_id.as_deref().unwrap_or_default().into(),
        artists: link_model(artist_links(t)),
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

fn tile_tint(id: &str, title: &str) -> Color {
    const PALETTE: [[u8; 3]; 10] = [
        [196, 62, 62],
        [61, 90, 128],
        [42, 157, 143],
        [231, 111, 81],
        [123, 45, 142],
        [29, 53, 87],
        [188, 108, 37],
        [82, 122, 109],
        [155, 34, 38],
        [106, 76, 147],
    ];
    let mut h: u32 = 2166136261;
    for b in id.bytes().chain(title.bytes()) {
        h ^= u32::from(b);
        h = h.wrapping_mul(16777619);
    }
    let [r, g, b] = PALETTE[h as usize % PALETTE.len()];
    Color::from_rgb_u8(r, g, b)
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
        favorite: false,
        tint: tile_tint(&c.id, &c.title),
        detail: video_length(c.duration).into(),
    }
}

/// Ids of what is in My Collection, by kind.
#[derive(Clone, Default)]
struct Collection {
    albums: HashSet<String>,
    playlists: HashSet<String>,
    videos: HashSet<String>,
}

impl Collection {
    fn contains(&self, kind: &str, id: &str) -> bool {
        match kind {
            "album" => self.albums.contains(id),
            "playlist" => self.playlists.contains(id),
            "video" => self.videos.contains(id),
            _ => false,
        }
    }

    fn set_mut(&mut self, kind: &str) -> Option<&mut HashSet<String>> {
        match kind {
            "album" => Some(&mut self.albums),
            "playlist" => Some(&mut self.playlists),
            "video" => Some(&mut self.videos),
            _ => None,
        }
    }
}

/// "2 MIN" under a video card; empty for anything without a length.
fn video_length(secs: u32) -> String {
    if secs == 0 {
        return String::new();
    }
    format!("{} MIN", ((secs + 30) / 60).max(1))
}

fn card_row_fav(c: &Card, collection: &Collection) -> CardData {
    let mut row = card_row(c);
    row.favorite = collection.contains(&c.kind, &c.id);
    row
}

fn restamp_model(model: &ModelRc<CardData>, collection: &Collection) {
    for i in 0..model.row_count() {
        let Some(mut row) = model.row_data(i) else {
            continue;
        };
        let fav = collection.contains(row.kind.as_str(), row.id.as_str());
        if row.favorite != fav {
            row.favorite = fav;
            model.set_row_data(i, row);
        }
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
    matches!(card.kind.as_str(), "album" | "video")
        || (card.kind == "playlist" && !card.by.is_empty() && card.by != "Me")
}

/// The `favorites/ids` key for a card kind.
fn favorite_kind(kind: &str) -> &'static str {
    match kind {
        "album" => "ALBUM",
        "playlist" => "PLAYLIST",
        "video" => "VIDEO",
        _ => "ARTIST",
    }
}

async fn set_card_favorite(
    api: &ApiHandle,
    card: &Card,
    favorite: bool,
) -> Result<(), tide_api::ApiError> {
    match card.kind.as_str() {
        "album" => api.set_album_favorite(&card.id, favorite).await,
        "playlist" => api.set_playlist_favorite(&card.id, favorite).await,
        "video" => api.set_video_favorite(&card.id, favorite).await,
        _ => api.set_artist_followed(&card.id, favorite).await,
    }
}

/// Plain-text teaser of an artist biography: one paragraph, cut to roughly three lines.
fn short_bio(bio: &str) -> String {
    let out = tide_api::plain_bio(bio)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
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

fn drop_card_images_outside(model: &ModelRc<CardData>, from: usize, upto: usize) {
    let n = model.row_count();
    let upto = upto.min(n);
    let from = from.min(upto);
    for i in 0..n {
        if (from..upto).contains(&i) {
            continue;
        }
        let Some(mut row) = model.row_data(i) else {
            continue;
        };
        if row.image.size().width == 0 {
            continue;
        }
        row.image = Image::default();
        model.set_row_data(i, row);
    }
}

/// Download (or read from the artwork cache) and decode off the UI thread.
async fn fetch_image(
    ctx: &Ctx,
    url: &str,
    max_px: u32,
) -> Option<(SharedPixelBuffer<Rgba8Pixel>, [u8; 3])> {
    let _permit = ctx.img_sem.acquire().await.ok()?;
    let path = ctx
        .images
        .get_or_download(&url_key(url, 0), url)
        .await
        .inspect_err(|e| tracing::warn!("artwork download failed: {e}"))
        .ok()?;
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
        Some((
            SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()),
            avg,
        ))
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
            state: RefCell::new(State {
                volume_before_mute: 1.0,
                ..Default::default()
            }),
            nav: RefCell::default(),
            clipboard: RefCell::default(),
            video: RefCell::default(),
            dsp: RefCell::default(),
            mem: RefCell::default(),
        });
        UI.with(|u| *u.borrow_mut() = Some(ui.clone()));
        ui.wire();
        ui.start_mem_tracker();
        ui
    }

    // ------------------------------------------------------------ window lifecycle

    /// ⌘W / File ▸ Close Window: the window goes away, TIDE keeps playing.
    /// Hiding destroys the native window and its GPU surfaces (see
    /// `platform::destroy_window_on_hide`); the Dock icon brings it back.
    pub fn close_window(self: &Rc<Self>) {
        self.enter_background();
        // Menu actions (⌘W included) run while Slint holds the native window,
        // so it can only be torn down once the action has returned.
        let weak = self.win.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(win) = weak.upgrade()
                && let Err(e) = win.hide()
            {
                tracing::warn!("Could not close the window: {e}");
            }
        });
    }

    /// Drop what only the window needs before it goes away.
    fn enter_background(self: &Rc<Self>) {
        // A video has nothing to draw into; keep music going, but not video.
        if self.video_active() && self.app().get_playing() {
            self.video_toggle_play();
        }
        self.state.borrow_mut().snaps.clear();
        {
            let mut nav = self.nav.borrow_mut();
            if matches!(nav.phase, NavPhase::Idle) {
                nav.outgoing = None;
                nav.incoming = None;
            }
        }
        // Once the window and its surfaces are gone, hand the freed pages back.
        slint::Timer::single_shot(Duration::from_secs(1), || {
            let freed = crate::mem::release_free_memory();
            tracing::info!(
                footprint = %crate::mem::human(crate::mem::footprint()),
                returned = %crate::mem::human(freed),
                "Window closed; playback continues in the background"
            );
        });
    }

    /// The Dock icon was clicked: bring the closed window back.
    pub fn reopen_window(self: &Rc<Self>) {
        let window = self.win.window();
        if window.is_visible() {
            if window.is_minimized() {
                window.set_minimized(false);
            }
            return;
        }
        if let Err(e) = self.win.show() {
            tracing::warn!("Could not reopen the window: {e}");
            return;
        }
        #[cfg(target_os = "macos")]
        crate::platform::window_reopened(&self.win);
    }

    // ------------------------------------------------------------ memory

    fn low_ram(&self) -> bool {
        self.ctx.settings.read().unwrap().low_ram
    }

    fn set_low_ram(self: &Rc<Self>, on: bool) {
        self.update_settings(|s| s.low_ram = on);
        self.app().set_low_ram(on);
        crate::platform::apply_memory_profile(on);
        if on {
            self.nav_finish();
            self.state.borrow_mut().snaps.clear();
            crate::platform::purge_gpu_cache();
        }
    }

    // ------------------------------------------------------------ RAM inspector

    fn start_mem_tracker(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.mem.borrow().timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(ui) = weak.upgrade() {
                    ui.sample_memory();
                }
            },
        );
        self.sample_memory();
    }

    /// Take one reading, keep it for the tracker and push it to the
    /// inspector, so the panel is filled the moment it opens.
    fn sample_memory(&self) {
        let pixels = |img: &Image| {
            let s = img.size();
            s.width as u64 * s.height as u64 * 4
        };
        let (artwork, covers, mut snapshots) = {
            let st = self.state.borrow();
            let art: u64 = st.images.values().map(|(img, _)| pixels(img)).sum();
            let snaps: u64 = st.snaps.values().map(pixels).sum();
            (art, st.images.len(), snaps)
        };
        {
            let nav = self.nav.borrow();
            snapshots += nav
                .outgoing
                .iter()
                .chain(nav.incoming.iter())
                .map(pixels)
                .sum::<u64>();
        }
        // One BGRA surface per drawable at physical size: three, or two in
        // Low RAM mode. None while the window is closed.
        let size = self.win.window().size();
        let buffers: u64 = if !self.win.window().is_visible() {
            0
        } else if self.low_ram() {
            2
        } else {
            3
        };
        let window = size.width as u64 * size.height as u64 * 4 * buffers;
        let footprint = crate::mem::footprint();
        let reading = crate::mem::Breakdown::split(
            footprint,
            crate::mem::malloc_in_use(),
            artwork,
            snapshots,
            window,
        );

        let mut tracker = self.mem.borrow_mut();
        // Large buffers that were freed (page snapshots, decoded artwork)
        // stay in the allocator as dirty pages; hand them back now and then.
        tracker.samples = tracker.samples.wrapping_add(1);
        if tracker.samples.is_multiple_of(5) {
            crate::mem::release_free_memory();
        }
        tracker.peak = tracker.peak.max(footprint);
        tracker.history.push_back(reading);
        while tracker.history.len() > MEM_SLOTS {
            tracker.history.pop_front();
        }
        let app = self.app();
        if footprint == 0 {
            return;
        }

        const MB: f32 = 1024.0 * 1024.0;
        let total = reading.total().max(1) as f32;
        let parts = [
            (format!("Artwork cache · {covers} covers"), reading.artwork),
            ("Page snapshots".to_string(), reading.snapshots),
            ("Other heap · models, decode, API".to_string(), reading.heap),
            (
                format!("Window buffers · {}×{} ×{buffers}", size.width, size.height),
                reading.window,
            ),
            (
                format!(
                    "Graphics, driver & system · GPU cache {}",
                    crate::mem::human(crate::platform::gpu_cache_bytes())
                ),
                reading.other,
            ),
        ];
        let parts: Vec<MemPart> = parts
            .into_iter()
            .zip(MEM_COLORS)
            .map(|((label, bytes), (r, g, b))| MemPart {
                label: label.into(),
                size: crate::mem::human(bytes).into(),
                color: Color::from_rgb_u8(r, g, b),
                frac: bytes as f32 / total,
            })
            .collect();
        let history: Vec<MemSample> = tracker
            .history
            .iter()
            .map(|b| MemSample {
                artwork: b.artwork as f32 / MB,
                snapshots: b.snapshots as f32 / MB,
                heap: b.heap as f32 / MB,
                window: b.window as f32 / MB,
                other: b.other as f32 / MB,
            })
            .collect();
        let top = tracker.history.iter().map(|b| b.total()).max().unwrap_or(0) as f32 / MB;
        // Round the tracker's scale up to the next 100 MB so it does not jitter.
        app.set_mem_scale(((top / 100.0).ceil() * 100.0).max(100.0));
        app.set_mem_total(crate::mem::human(footprint).into());
        app.set_mem_peak(crate::mem::human(tracker.peak).into());
        app.set_mem_note(
            "Artwork and snapshots are counted pixels; window buffers are estimated. \
             Covers held only by on-screen rows count under Other heap. \
             Graphics, driver & system is the remainder, not measured GPU texture usage."
                .into(),
        );
        app.set_mem_parts(ModelRc::new(VecModel::from(parts)));
        app.set_mem_history(ModelRc::new(VecModel::from(history)));
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
        bind!(on_play_card, |ui, card| ui.play_card(card));
        bind!(on_toggle_card_favorite, |ui, card| ui
            .toggle_card_favorite(card));
        bind!(on_set_home_tab, |ui, tab| ui.set_home_tab(tab));
        bind!(on_open_track_artist, |ui, track| ui.open_card(CardData {
            kind: "artist".into(),
            id: track.artist_id,
            title: track.artist,
            ..Default::default()
        }));
        bind!(on_open_track_album, |ui, track| ui.open_card(CardData {
            kind: "album".into(),
            id: track.album_id,
            title: track.album,
            subtitle: track.artist,
            image_url: track.cover_url,
            ..Default::default()
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
        bind!(on_show_artist_releases, |ui, singles| {
            let artist = match ui.current_page() {
                Some(Page::Detail(card)) if card.kind == "artist" => Some(card),
                Some(Page::Detail(_)) => {
                    let row = ui.app().get_page_artist();
                    (!row.id.is_empty()).then(|| card_from_row(&row))
                }
                _ => None,
            };
            if let Some(artist) = artist {
                ui.show(Page::ArtistReleases { artist, singles }, true);
            }
        });
        bind!(on_artist_release_art, |ui, singles, from, upto| {
            ui.artist_release_art(singles, from.max(0) as usize, upto.max(0) as usize);
        });
        bind!(on_catalog_video_art, |ui, artist, from, upto| {
            let app = ui.app();
            let (key, model) = if artist {
                ("artist-videos", app.get_artist_videos())
            } else {
                ("search-videos", app.get_search_videos())
            };
            ui.card_art(key, model, from.max(0) as usize, upto.max(0) as usize);
        });
        bind!(on_sort_playlists, |ui| {
            let app = ui.app();
            app.set_sidebar_ascending(!app.get_sidebar_ascending());
            ui.present_sidebar();
        });
        bind!(on_set_grid_sort, |ui, sort| ui.set_grid_arrangement(
            Some(sort.to_string()),
            None,
            None
        ));
        bind!(on_set_grid_group, |ui, group| ui.set_grid_arrangement(
            None,
            Some(group.to_string()),
            None
        ));
        bind!(on_set_grid_combine_others, |ui, on| ui
            .set_grid_arrangement(None, None, Some(on)));
        bind!(on_set_grid_size, |ui, size| ui
            .update_settings(|s| s.grid_size = size.clamp(0, 4)));
        bind!(on_create_in_collection, |ui, kind, name| ui
            .create_in_collection(&kind, name.trim()));
        bind!(on_toggle_page_favorite, |ui| ui.toggle_page_favorite());
        bind!(on_page_action, |ui, action, arg| ui
            .page_action(&action, &arg));
        bind!(on_related_art, |ui, from, upto| {
            ui.card_art(
                "related",
                ui.app().get_related_artists(),
                from.max(0) as usize,
                upto.max(0) as usize,
            );
        });
        bind!(on_back, |ui| ui.step_history(-1));
        bind!(on_forward, |ui| ui.step_history(1));
        bind!(on_np_artist_hovered, |ui, artist| ui
            .load_artist_hover_card(
                artist.id.to_string(),
                artist.name.to_string()
            ));
        bind!(on_open_artist, |ui, artist| ui.open_card(CardData {
            kind: "artist".into(),
            id: artist.id,
            title: artist.name,
            ..Default::default()
        }));
        bind!(on_toggle_follow_artist, |ui| ui.toggle_follow_artist());
        bind!(on_open_now_playing_artist, |ui| {
            // The hover card's "Read more" opens whichever credited artist it is showing.
            let hovered = ui.state.borrow().hover_artist.clone();
            if !hovered.is_empty() {
                let name = ui.app().get_np_artist_name();
                ui.open_card(CardData {
                    kind: "artist".into(),
                    id: hovered.into(),
                    title: name,
                    ..Default::default()
                });
            } else if let Some(track) = ui.current_track()
                && let Some(id) = track.artist_id
            {
                ui.open_card(CardData {
                    kind: "artist".into(),
                    id: id.into(),
                    title: track.artist.into(),
                    ..Default::default()
                });
            }
        });
        bind!(on_search, |ui, q| {
            let q = q.trim().to_string();
            if !q.is_empty() {
                ui.show(Page::Search(q), true);
            }
        });
        bind!(on_filter, |ui, text| ui.apply_filter(&text));
        bind!(on_rows_visible, |ui, first, count| ui
            .rows_visible(first.max(0) as usize, count.max(0) as usize));
        bind!(on_request_track_menu, |ui, id, x, y| ui
            .open_track_menu(&id, x, y));
        bind!(on_request_card_menu, |ui, card, x, y| ui
            .open_card_menu(card, x, y));
        bind!(on_menu_action, |ui, action, arg| ui
            .menu_action(&action, &arg));
        bind!(on_play_track_next, |ui, id| ui.play_track_next(&id));
        bind!(on_copy_track_link, |ui, id| {
            if let Err(error) = ui.copy_track_link(&id) {
                tracing::warn!("Could not copy track link: {error}");
            }
        });
        bind!(on_play_track, |ui, id| ui.play_from_page(&id));
        bind!(on_play_section_track, |ui, section, id| ui
            .play_from_section(section.max(0) as usize, &id));
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
        bind!(on_open_now_playing, |ui| {
            if ui.video_active() {
                let open = !ui.app().get_video_open();
                ui.show_video_view(open);
            } else {
                ui.toggle_now_playing();
            }
        });
        bind!(on_open_video, |ui| ui.show_video_view(true));
        bind!(on_close_video, |ui| ui.show_video_view(false));
        bind!(on_stop_video, |ui| ui.stop_video());
        bind!(on_set_video_quality, |ui, choice| ui
            .set_video_quality(&choice));
        bind!(on_play_similar_video, |ui, index| ui
            .play_similar_video(index.max(0) as usize));
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
        bind!(on_close_window, |ui| ui.close_window());
        {
            // The red close button: same as ⌘W. Slint hides the window itself.
            let weak = Rc::downgrade(self);
            self.win.window().on_close_requested(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.enter_background();
                }
                slint::CloseRequestResponse::HideWindow
            });
        }
        app.on_open_help(|| {
            let _ = open::that("https://support.tidal.com/");
        });
        bind!(on_section_visible, |ui, index, from, upto| ui
            .section_visible(
                index.max(0) as usize,
                from.max(0) as usize,
                upto.max(0) as usize
            ));
        bind!(on_grid_art, |ui, group, from, upto| ui.grid_art(
            group.max(0) as usize,
            from.max(0) as usize,
            upto.max(0) as usize
        ));
        bind!(on_set_quality, |ui, quality| ui
            .update_settings(|s| s.quality = quality.to_string()));
        bind!(on_set_show_collection, |ui, on| ui
            .update_settings(|s| s.show_collection = on));
        bind!(on_set_show_audio_metadata, |ui, on| ui
            .update_settings(|s| s.show_audio_metadata = on));
        bind!(on_clear_cache, |ui| {
            if let Err(e) = ui.ctx.audio.clear() {
                tracing::warn!("could not clear the audio cache: {e}");
            }
            ui.present_settings();
        });
        bind!(on_pkce_start, |ui| ui.pkce_start(None));
        bind!(on_pkce_start_safari, |ui| ui
            .pkce_start(Some(crate::platform::SAFARI)));
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
                let _ = tokio::task::spawn_blocking(move || ctx.player.set_restore_sample_rate(on))
                    .await;
                on_ui(|ui| ui.present_signal());
            });
        });
        bind!(on_set_keep_most_played, |ui, on| ui
            .update_settings(|s| s.keep_most_played = on));
        bind!(on_set_low_ram, |ui, on| ui.set_low_ram(on));
        bind!(on_pkce_finish, |ui, url| ui
            .pkce_finish(url.trim().to_string()));
        self.dsp_wire();
    }

    // ------------------------------------------------------------ boot

    /// Paint everything we have on disk, then refresh from the network.
    pub fn boot(self: &Rc<Self>, has_session: bool) {
        let app = self.app();
        app.set_logged_in(has_session);
        app.set_quality_label("Max".into());
        app.set_app_version(env!("CARGO_PKG_VERSION").into());
        self.present_settings();
        self.dsp_boot();
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
                ui.app()
                    .set_username(username.unwrap_or_else(|| "TIDAL".into()).into());
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
        if let Some(ids) = load::<FavoriteIds>(&self.ctx.meta, "favorite-ids") {
            self.apply_favorite_ids(ids);
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
        let Ok(Some(saved)) = self.ctx.meta.load_playback_state() else {
            return;
        };
        let app = self.app();
        self.ctx
            .player
            .set_volume(if saved.muted { 0.0 } else { saved.volume });
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
                    let _ = ctx.meta.put_json(
                        "home",
                        &serde_json::to_string(&sections).unwrap_or_default(),
                    );
                    on_ui(move |ui| {
                        if matches!(ui.current_page(), Some(Page::Home))
                            && ui.app().get_home_tab() == 0
                        {
                            ui.set_sections(sections);
                        }
                    });
                }
                Err(e) => tracing::warn!("home feed failed: {e}"),
            }
        });
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.favorite_ids().await {
                Ok(ids) => {
                    let _ = ctx.meta.put_json(
                        "favorite-ids",
                        &serde_json::to_string(&ids).unwrap_or_default(),
                    );
                    on_ui(move |ui| ui.apply_favorite_ids(ids));
                }
                Err(e) => tracing::warn!("favorite ids failed: {e}"),
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
            let _ = ctx.meta.put_json(
                "favorites",
                &serde_json::to_string(&all).unwrap_or_default(),
            );
        });
    }

    /// Refresh the sidebar's playlists and folders.
    fn sync_playlists(self: &Rc<Self>) {
        self.spawn(|ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.playlists("root").await {
                Ok(cards) => {
                    let _ = ctx.meta.put_json(
                        "cards:playlists",
                        &serde_json::to_string(&cards).unwrap_or_default(),
                    );
                    on_ui(move |ui| ui.set_sidebar(&cards));
                }
                Err(e) => tracing::warn!("playlists failed: {e}"),
            }
        });
    }

    // ------------------------------------------------------------ images

    fn load_image(
        self: &Rc<Self>,
        url: &str,
        max_px: u32,
        apply: impl FnOnce(&Image, Color) + 'static,
    ) {
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
                let waiters = ui
                    .state
                    .borrow_mut()
                    .image_waiters
                    .remove(&key)
                    .unwrap_or_default();
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

    fn cards_model(
        self: &Rc<Self>,
        key: &str,
        cards: &[Card],
        max_px: u32,
    ) -> Rc<VecModel<CardData>> {
        let model = Rc::new(VecModel::from(self.rows_for(cards)));
        self.keep_card_images(key, ModelRc::from(model.clone()), 0, cards.len(), max_px);
        model
    }

    /// Decode artwork for cards in `[from, upto)` and drop pixels outside that window.
    fn keep_card_images(
        self: &Rc<Self>,
        key: &str,
        model: ModelRc<CardData>,
        from: usize,
        upto: usize,
        max_px: u32,
    ) {
        let n = model.row_count();
        let upto = upto.min(n);
        let from = from.min(upto);
        self.state
            .borrow_mut()
            .art_keep
            .insert(key.to_string(), (from, upto));
        for i in 0..n {
            if (from..upto).contains(&i) {
                continue;
            }
            if let Some(row) = model.row_data(i)
                && !row.image_url.is_empty()
                && row.image.size().width > 0
            {
                self.state
                    .borrow_mut()
                    .images
                    .remove(&url_key(&row.image_url, max_px));
            }
        }
        drop_card_images_outside(&model, from, upto);
        for i in from..upto {
            let Some(row) = model
                .row_data(i)
                .filter(|r| !r.image_url.is_empty() && r.image.size().width == 0)
            else {
                continue;
            };
            let url = row.image_url.to_string();
            let id = row.id.clone();
            let model = model.clone();
            let key = key.to_string();
            let ui = Rc::clone(self);
            self.load_image(&url, max_px, move |img, _| {
                let keep = ui.state.borrow().art_keep.get(&key).copied();
                if !keep.is_some_and(|(a, b)| i >= a && i < b) {
                    return;
                }
                if let Some(mut row) = model.row_data(i).filter(|r| r.id == id) {
                    row.image = img.clone();
                    model.set_row_data(i, row);
                }
            });
        }
    }

    fn keep_track_covers(
        self: &Rc<Self>,
        key: &str,
        model: &Rc<VecModel<TrackData>>,
        from: usize,
        upto: usize,
        max_px: u32,
    ) {
        let n = model.row_count();
        let upto = upto.min(n);
        let from = from.min(upto);
        self.state
            .borrow_mut()
            .art_keep
            .insert(key.to_string(), (from, upto));
        for i in 0..n {
            if (from..upto).contains(&i) {
                continue;
            }
            let Some(mut row) = model.row_data(i) else {
                continue;
            };
            if row.cover.size().width == 0 {
                continue;
            }
            if !row.cover_url.is_empty() {
                self.state
                    .borrow_mut()
                    .images
                    .remove(&url_key(&row.cover_url, max_px));
            }
            row.cover = Image::default();
            model.set_row_data(i, row);
        }
        for i in from..upto {
            let Some(row) = model
                .row_data(i)
                .filter(|r| !r.cover_url.is_empty() && r.cover.size().width == 0)
            else {
                continue;
            };
            let url = row.cover_url.to_string();
            let id = row.id.clone();
            let model = model.clone();
            let key = key.to_string();
            let ui = Rc::clone(self);
            self.load_image(&url, max_px, move |img, _| {
                let keep = ui.state.borrow().art_keep.get(&key).copied();
                if !keep.is_some_and(|(a, b)| i >= a && i < b) {
                    return;
                }
                if let Some(mut row) = model.row_data(i).filter(|r| r.id == id) {
                    row.cover = img.clone();
                    model.set_row_data(i, row);
                }
            });
        }
    }

    fn drop_section_art(self: &Rc<Self>) {
        let models = self.state.borrow().section_models.clone();
        for (i, (cards, tracks, _)) in models.iter().enumerate() {
            self.keep_card_images(
                &format!("home:{i}"),
                ModelRc::from(cards.clone()),
                0,
                0,
                256,
            );
            self.keep_track_covers(&format!("home-tracks:{i}"), tracks, 0, 0, 128);
        }
    }

    fn drop_grid_art(self: &Rc<Self>) {
        let models = self.state.borrow().grid.models.clone();
        for (i, model) in models.iter().enumerate() {
            self.keep_card_images(
                &format!("grid:{i}"),
                ModelRc::from(model.clone()),
                0,
                0,
                320,
            );
        }
    }

    fn drop_track_art(self: &Rc<Self>) {
        let Some(model) = self.state.borrow().tracks_model.clone() else {
            return;
        };
        self.keep_track_covers("tracks", &model, 0, 0, 96);
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
        let (fav_ids, collection) = {
            let st = self.state.borrow();
            (st.fav_ids.clone(), st.collection.clone())
        };
        let mut models = Vec::with_capacity(sections.len());
        let rows: Vec<SectionData> = sections
            .iter()
            .map(|s| {
                let rows_layout =
                    s.layout == "cards" && s.title.eq_ignore_ascii_case("Recently played");
                // Artwork is requested later, when the section scrolls near the viewport.
                let cards = Rc::new(VecModel::from(
                    s.cards
                        .iter()
                        .map(|c| card_row_fav(c, &collection))
                        .collect::<Vec<_>>(),
                ));
                let tracks = Rc::new(VecModel::from(
                    s.tracks
                        .iter()
                        .map(|t| track_row(t, fav_ids.contains(&t.id)))
                        .collect::<Vec<_>>(),
                ));
                models.push((cards.clone(), tracks.clone(), 0));
                SectionData {
                    title: s.title.as_str().into(),
                    layout: if rows_layout {
                        "rows".into()
                    } else {
                        s.layout.as_str().into()
                    },
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
        self.app()
            .set_sections(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    /// Home section `index` is near the viewport: keep artwork for cards `[from, upto)`.
    fn section_visible(self: &Rc<Self>, index: usize, from: usize, upto: usize) {
        let (card_model, track_model, small) = {
            let st = self.state.borrow();
            let Some(section) = st.sections.get(index) else {
                return;
            };
            let Some((card_model, track_model, _)) = st.section_models.get(index) else {
                return;
            };
            let small = section.layout == "shortcuts"
                || section.title.eq_ignore_ascii_case("Recently played");
            (card_model.clone(), track_model.clone(), small)
        };
        self.keep_card_images(
            &format!("home:{index}"),
            ModelRc::from(card_model),
            from,
            upto,
            if small { 112 } else { 256 },
        );
        self.keep_track_covers(
            &format!("home-tracks:{index}"),
            &track_model,
            from,
            upto,
            128,
        );
    }

    fn grid_art(self: &Rc<Self>, group: usize, from: usize, upto: usize) {
        let Some(model) = self.state.borrow().grid.models.get(group).cloned() else {
            return;
        };
        self.keep_card_images(
            &format!("grid:{group}"),
            ModelRc::from(model),
            from,
            upto,
            320,
        );
    }

    // ------------------------------------------------------------ settings

    fn present_settings(&self) {
        let app = self.app();
        let settings = self.ctx.settings.read().unwrap().clone();
        app.set_quality(settings.quality.into());
        app.set_show_collection(settings.show_collection);
        app.set_show_audio_metadata(settings.show_audio_metadata);
        app.set_keep_most_played(settings.keep_most_played);
        app.set_low_ram(settings.low_ram);
        app.set_rate_matching(settings.rate_matching);
        app.set_restore_sample_rate(settings.restore_sample_rate);
        app.set_grid_size(settings.grid_size.clamp(0, 4));
        app.set_cache_max(format!("{}", settings.cache_max_gb).into());
        let (tracks, bytes) = self.ctx.audio.usage();
        let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let (hits, misses) = self.ctx.meta.cache_hit_stats();
        let hit_rate = if hits + misses == 0 {
            String::new()
        } else {
            format!(
                " · {:.0}% of plays came from the cache",
                hits as f64 * 100.0 / (hits + misses) as f64
            )
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
            (
                serde_json::to_string(&*settings).unwrap_or_default(),
                settings.quality != old_quality,
            )
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
            self.app()
                .set_pkce_status("Paste the address from your browser first.".into());
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
                        app.set_pkce_status(
                            "Max quality is enabled. New plays stream in Hi-Res where available."
                                .into(),
                        );
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
                        app.set_pkce_status(
                            format!("Login failed: {e}. Press the button to try again.").into(),
                        );
                    });
                }
            }
        });
    }

    /// Change the cache size limit from the Settings box (in GB).
    fn set_cache_max(self: &Rc<Self>, text: &str) {
        let parsed = text
            .trim()
            .trim_end_matches(|c: char| c.is_alphabetic() || c == ' ')
            .replace(',', ".")
            .parse::<f64>();
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
        let own: Vec<CardData> = cards
            .iter()
            .filter(|c| c.kind == "playlist" && c.by == "Me")
            .map(card_row)
            .collect();
        self.app()
            .set_own_playlists(ModelRc::new(VecModel::from(own)));
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
        app.set_sidebar_playlists(ModelRc::from(self.cards_model("sidebar", &cards, 96)));
    }

    fn set_grid(self: &Rc<Self>, cards: &[Card]) {
        self.state.borrow_mut().grid.all = cards.to_vec();
        self.present_grid();
    }

    /// Draw the grid from what was loaded, with the current filter, sort and grouping.
    fn present_grid(self: &Rc<Self>) {
        let groups = {
            let st = self.state.borrow();
            arrange_grid(
                &st.grid.all,
                &st.grid.filter,
                &st.grid.sort,
                &st.grid.group,
                st.grid.combine_others,
            )
        };
        let collection = self.state.borrow().collection.clone();
        let models: Vec<Rc<VecModel<CardData>>> = groups
            .iter()
            .map(|(_, cards)| {
                Rc::new(VecModel::from(
                    cards
                        .iter()
                        .map(|c| card_row_fav(c, &collection))
                        .collect::<Vec<_>>(),
                ))
            })
            .collect();
        let flat: Vec<CardData> = groups
            .iter()
            .flat_map(|(_, cards)| cards.iter().map(|c| card_row_fav(c, &collection)))
            .collect();
        let sections: Vec<SectionData> = groups
            .iter()
            .zip(models.iter())
            .map(|((title, _), model)| SectionData {
                title: title.as_str().into(),
                layout: "cards".into(),
                cards: ModelRc::from(model.clone()),
                tracks: ModelRc::default(),
            })
            .collect();
        self.state.borrow_mut().grid.models = models;
        let app = self.app();
        app.set_grid_cards(ModelRc::new(VecModel::from(flat)));
        app.set_grid_groups(ModelRc::new(VecModel::from(sections)));
    }

    /// Change the sort and / or grouping of the grid on screen and remember it for that page kind.
    fn set_grid_arrangement(
        self: &Rc<Self>,
        sort: Option<String>,
        group: Option<String>,
        combine: Option<bool>,
    ) {
        let (kind, sort, group, combine) = {
            let mut st = self.state.borrow_mut();
            if let Some(sort) = sort {
                st.grid.sort = sort;
            }
            if let Some(group) = group {
                st.grid.group = group;
            }
            if let Some(combine) = combine {
                st.grid.combine_others = combine;
            }
            (
                st.grid.kind.clone(),
                st.grid.sort.clone(),
                st.grid.group.clone(),
                st.grid.combine_others,
            )
        };
        let _ = self.ctx.meta.put_json(
            &format!("grid-prefs:{kind}"),
            &serde_json::json!([sort, group, if combine { "combine" } else { "each" }]).to_string(),
        );
        let app = self.app();
        app.set_grid_sort(sort.into());
        app.set_grid_group(group.into());
        app.set_grid_combine_others(combine);
        self.present_grid();
    }

    /// Create an empty playlist or folder, or rename / delete a folder.
    fn create_in_collection(self: &Rc<Self>, kind: &str, name: &str) {
        let target = self.app().get_create_folder_id().to_string();
        match kind {
            "rename-folder" => self.rename_folder(&target, name),
            "delete-folder" => self.delete_folder(&target, name),
            _ => self.create_collection_item(kind == "folder", name, &target),
        }
    }

    fn create_collection_item(self: &Rc<Self>, folder: bool, name: &str, parent: &str) {
        if name.is_empty() {
            return;
        }
        let parent = if parent.is_empty() {
            match self.current_page() {
                Some(Page::Folder { id, .. }) => id,
                _ => "root".to_string(),
            }
        } else {
            parent.to_string()
        };
        let name = name.to_string();
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            let what = if folder { "folder" } else { "playlist" };
            match api.create_in_collection(&name, &parent, folder).await {
                Ok(()) => on_ui(move |ui| {
                    crate::alerts::notice(&ui.app(), &format!("Created {what} “{name}”"));
                    ui.reload_collection(None);
                }),
                Err(e) => tracing::error!("Could not create {what} “{name}”: {e}"),
            }
        });
    }

    fn rename_folder(self: &Rc<Self>, id: &str, name: &str) {
        if id.is_empty() || name.is_empty() {
            return;
        }
        let (id, name) = (id.to_string(), name.to_string());
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.rename_folder(&id, &name).await {
                Ok(()) => {
                    let (id, name) = (id.clone(), name.clone());
                    on_ui(move |ui| {
                        {
                            let mut st = ui.state.borrow_mut();
                            let pos = st.hist_pos;
                            if let Some(Page::Folder { id: fid, title }) = st.history.get_mut(pos) {
                                if fid == &id {
                                    *title = name.clone();
                                }
                            }
                        }
                        crate::alerts::notice(&ui.app(), &format!("Renamed folder “{name}”"));
                        ui.reload_collection(None);
                    });
                }
                Err(e) => tracing::error!("Could not rename folder “{name}”: {e}"),
            }
        });
    }

    fn delete_folder(self: &Rc<Self>, id: &str, name: &str) {
        if id.is_empty() {
            return;
        }
        let (id, name) = (id.to_string(), name.to_string());
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.delete_folder(&id).await {
                Ok(()) => {
                    let (id, name) = (id.clone(), name.clone());
                    on_ui(move |ui| {
                        let text = if name.is_empty() {
                            "Deleted folder".into()
                        } else {
                            format!("Deleted folder “{name}”")
                        };
                        crate::alerts::notice(&ui.app(), &text);
                        ui.reload_collection(Some(&id));
                    });
                }
                Err(e) => tracing::error!("Could not delete folder “{name}”: {e}"),
            }
        });
    }

    /// Refresh the sidebar and the playlists grid after a collection write.
    fn reload_collection(self: &Rc<Self>, deleted_folder: Option<&str>) {
        self.sync_playlists();
        let page = match self.current_page() {
            Some(Page::Folder { id, .. }) if deleted_folder == Some(id.as_str()) => {
                let mut st = self.state.borrow_mut();
                let pos = st.hist_pos;
                if let Some(slot) = st.history.get_mut(pos) {
                    *slot = Page::Playlists;
                }
                Page::Playlists
            }
            Some(page @ (Page::Playlists | Page::Folder { .. })) => page,
            _ => return,
        };
        self.show(page, false);
    }

    /// Replace the track table.
    fn set_tracks(self: &Rc<Self>, tracks: Vec<TrackItem>, context: &str) {
        let model = {
            let mut st = self.state.borrow_mut();
            let rows: Vec<TrackData> = tracks
                .iter()
                .map(|t| track_row(t, st.fav_ids.contains(&t.id)))
                .collect();
            let model = Rc::new(VecModel::from(rows));
            st.page_tracks = tracks;
            st.page_context = context.to_string();
            st.tracks_model = Some(model.clone());
            model
        };
        self.app().set_tracks(ModelRc::from(model));
        let count = if self.app().get_view() == "artist" {
            4
        } else {
            30
        };
        self.rows_visible(0, count);
    }

    /// Lazily fetch artwork for the rows on screen, plus one extra page each way.
    fn rows_visible(self: &Rc<Self>, first: usize, count: usize) {
        let Some(model) = self.state.borrow().tracks_model.clone() else {
            return;
        };
        let len = self.state.borrow().page_tracks.len();
        let (start, end) = if self.app().get_view() == "artist" {
            (0, len.min(4))
        } else {
            (
                first.saturating_sub(count),
                (first + count + count).min(len),
            )
        };
        self.keep_track_covers("tracks", &model, start, end, 96);
    }

    fn apply_filter(self: &Rc<Self>, text: &str) {
        if self.app().get_view() == "grid" {
            self.state.borrow_mut().grid.filter = text.to_string();
            self.present_grid();
            return;
        }
        let needle = text.trim().to_lowercase();
        let (tracks, title) = {
            let st = self.state.borrow();
            let src: &[TrackItem] = match st.history.get(st.hist_pos) {
                Some(Page::Tracks) => &st.favorites,
                Some(Page::Later) => &st.later.tracks,
                _ => return,
            };
            let tracks = src
                .iter()
                .filter(|t| {
                    needle.is_empty()
                        || t.title.to_lowercase().contains(&needle)
                        || t.artist.to_lowercase().contains(&needle)
                        || t.album.to_lowercase().contains(&needle)
                })
                .cloned()
                .collect();
            let title = match st.history.get(st.hist_pos) {
                Some(Page::Later) => "Save for Later",
                _ => "Tracks",
            };
            (tracks, title)
        };
        self.set_tracks(tracks, title);
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
            "dsp" => Page::Dsp,
            "focus-search" => {
                let app = self.app();
                app.set_focus_search(app.get_focus_search() + 1);
                return;
            }
            "explore" => Page::Explore,
            "feed" => Page::Feed,
            "videos" => Page::Videos,
            "profiles" => Page::Profiles,
            "later" => Page::Later,
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
            "folder" => Page::Folder {
                id: card.id.to_string(),
                title: card.title.to_string(),
            },
            "page" => Page::Catalog {
                path: card.id.to_string(),
                title: card.title.to_string(),
            },
            "video" => {
                self.play_video_card(&card);
                return;
            }
            _ => Page::Detail(card_from_row(&card)),
        };
        self.show(page, true);
    }

    /// Back / forward from a button, menu item or shortcut.
    fn step_history(self: &Rc<Self>, delta: isize) {
        if self.low_ram() {
            // No snapshots, no panes: the page changes like any opened page,
            // with its artwork fading in.
            self.nav_finish();
            self.step_history_now(delta, None);
        } else if !crate::platform::reduce_motion() && self.nav_begin(delta) {
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
        // Low RAM mode keeps no snapshots: each is a full-resolution copy of
        // the page (about 15 MB on a Retina display).
        if !app.get_logged_in() || self.low_ram() {
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
        let rect = (
            px(app.get_pane_x()),
            px(app.get_pane_y()),
            px(app.get_pane_width()),
            px(app.get_pane_height()),
        );
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
        if self.low_ram() {
            // Follow the swipe without drawing panes; `swipe_end` changes page.
            let st = self.state.borrow();
            let target = st.hist_pos as isize + delta;
            if target < 0 || target as usize >= st.history.len() {
                return false;
            }
            let mut nav = self.nav.borrow_mut();
            nav.phase = NavPhase::Tracking;
            nav.delta = delta;
            nav.p = 0.0;
            nav.travel = 0.0;
            nav.samples.clear();
            return true;
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
        let (under, over) = if delta < 0 {
            (target.clone(), Some(outgoing.clone()))
        } else {
            (Some(outgoing.clone()), target.clone())
        };
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
        nav.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(8),
            move || {
                if let Some(ui) = weak.upgrade() {
                    ui.nav_tick();
                }
            },
        );
    }

    fn nav_tick(self: &Rc<Self>) {
        let now = Instant::now();
        let phase = self.nav.borrow().phase;
        match phase {
            NavPhase::Settling {
                from,
                to,
                start,
                secs,
                smooth,
            } => {
                let t = ((now - start).as_secs_f32() / secs).min(1.0);
                let eased = if smooth { ease_in_out(t) } else { ease_out(t) };
                self.nav_set(from + (to - from) * eased);
                if t >= 1.0 {
                    if to >= 1.0 {
                        self.nav_commit_pending();
                        self.nav.borrow_mut().phase = NavPhase::Waiting {
                            since: Instant::now(),
                        };
                    } else {
                        self.nav_clear();
                    }
                }
            }
            NavPhase::Waiting { since } => {
                // A fast page shows no loading state: hold the snapshot until
                // the content and its artwork are there, but never for long.
                // The floor lets the page lay out and ask for its artwork.
                let waited = now - since;
                let art_ready = self.state.borrow().image_waiters.is_empty();
                if waited >= Duration::from_millis(500)
                    || (waited >= Duration::from_millis(80)
                        && !self.app().get_loading()
                        && art_ready)
                {
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
            while nav
                .samples
                .front()
                .is_some_and(|(t, _)| now - *t > Duration::from_millis(120))
            {
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
                (Some((t0, x0)), Some((t1, x1)))
                    if *t1 > *t0 && t1.elapsed() < Duration::from_millis(100) =>
                {
                    (x1 - x0) / (*t1 - *t0).as_secs_f32()
                }
                _ => 0.0,
            };
            (nav.p, velocity)
        };
        let commit = !cancelled && swipe_commits(p, velocity);
        if self.low_ram() {
            let delta = {
                let mut nav = self.nav.borrow_mut();
                nav.phase = NavPhase::Idle;
                nav.delta
            };
            if commit {
                self.step_history_now(delta, None);
            }
            return;
        }
        let remaining = if commit { 1.0 - p } else { p };
        // Carries on from the fingers' speed, so it only eases out.
        self.nav_settle(commit, 0.14 + 0.20 * remaining, false);
    }

    fn show(self: &Rc<Self>, page: Page, push: bool) {
        if self.app().get_video_open() {
            self.show_video_view(false);
        }
        self.app().set_menu_open(false);
        let was = self.app().get_view().to_string();
        // Going to the page already on screen shows it afresh, without a
        // second history entry for it.
        let push = push && {
            let mut st = self.state.borrow_mut();
            let pos = st.hist_pos;
            let repeat = st
                .history
                .get(pos)
                .is_some_and(|current| current.same_as(&page));
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
                let keep = if st.history.is_empty() {
                    0
                } else {
                    st.hist_pos + 1
                };
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
        app.set_page_artists(link_model(Vec::new()));
        app.set_page_year("".into());
        app.set_page_quality("".into());
        app.set_page_summary("".into());
        app.set_page_biography("".into());
        app.set_page_tint(Color::from_rgb_u8(34, 34, 34));
        app.set_artist_tab(0);
        app.set_artist_albums(ModelRc::default());
        app.set_artist_singles(ModelRc::default());
        app.set_artist_videos(ModelRc::default());
        app.set_artist_videos_status("".into());
        app.set_search_cards(ModelRc::default());
        app.set_search_videos(ModelRc::default());
        app.set_search_videos_status("".into());
        app.set_search_tab(0);
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
                app.set_home_tab(0);
                self.load_home_feed(0);
            }
            Page::Explore => {
                app.set_view("modules".into());
                app.set_nav_key("explore".into());
                app.set_page_title("Explore".into());
                self.load_page_sections(
                    "explore",
                    page_gen,
                    |api| async move { api.explore().await },
                );
            }
            Page::Catalog { path, title } => {
                app.set_view("modules".into());
                app.set_nav_key("explore".into());
                app.set_page_title(title.into());
                let cache_key = format!("page:{path}");
                self.load_page_sections(&cache_key, page_gen, move |api| async move {
                    api.catalog_page(&path).await
                });
            }
            Page::Feed => {
                app.set_view("feed".into());
                app.set_nav_key("feed".into());
                app.set_page_title("Feed".into());
                self.load_page_sections("feed", page_gen, |api| async move { api.feed().await });
            }
            Page::Later => {
                app.set_view("modules".into());
                app.set_nav_key("later".into());
                app.set_page_title("Save for Later".into());
                if self.ctx.meta.get_json("later").is_none() {
                    app.set_loading(true);
                    self.set_sections(Vec::new());
                }
                self.load_catalog(
                    "later".into(),
                    page_gen,
                    |api| async move { api.later().await },
                    |ui, items| ui.present_later(items),
                );
            }
            Page::Tracks => {
                app.set_view("tracks".into());
                app.set_nav_key("tracks".into());
                app.set_page_title("Tracks".into());
                app.set_tracks_show_date(true);
                app.set_filter_text("".into());
                self.apply_filter("");
            }
            Page::Mixes => self.show_grid(
                "mixes",
                "mixes",
                "Mixes & Radio",
                page_gen,
                |api| async move { api.mixes().await },
            ),
            Page::Playlists => self.show_grid(
                "playlists",
                "playlists",
                "Playlists",
                page_gen,
                |api| async move { api.playlists("root").await },
            ),
            Page::Albums => {
                self.show_grid("albums", "albums", "Albums", page_gen, |api| async move {
                    api.albums().await
                })
            }
            Page::Videos => {
                self.show_grid("videos", "videos", "Videos", page_gen, |api| async move {
                    api.videos().await
                })
            }
            Page::Profiles => self.show_grid(
                "profiles",
                "profiles",
                "Profiles",
                page_gen,
                |api| async move { api.followed_artists().await },
            ),
            Page::Folder { id, title } => {
                let key = format!("folder:{id}");
                self.show_grid(&key, "playlists", &title, page_gen, move |api| async move {
                    api.playlists(&id).await
                });
                app.set_nav_key("playlists".into());
            }
            Page::Detail(card) => {
                let Card {
                    kind,
                    id,
                    title,
                    quality,
                    ..
                } = &card;
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
                    let video_id = id.clone();
                    self.load_video_catalog(
                        format!("artist-videos:{id}"),
                        page_gen,
                        true,
                        move |api| async move { api.artist_videos(&video_id).await },
                    );
                }
                if matches!(kind.as_str(), "album" | "playlist" | "mix") {
                    app.set_page_card(self.row_for(&card));
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
                self.show_track_list(&cache_key, title, page_gen, move |api| async move {
                    api.tracks_of(&kind, &id).await
                });
            }
            Page::ArtistTracks(card) => {
                app.set_view("detail".into());
                app.set_nav_key("".into());
                app.set_page_title("Top Tracks".into());
                app.set_tracks_show_date(false);
                let id = card.id;
                self.show_track_list(
                    &format!("tracks:artist:{id}"),
                    &card.title,
                    page_gen,
                    move |api| async move { api.tracks_of("artist", &id).await },
                );
            }
            Page::ArtistReleases { artist, singles } => {
                let title = if singles { "EPs & Singles" } else { "Albums" };
                let hint = if singles {
                    "Filter EPs & singles"
                } else {
                    "Filter albums"
                };
                let id = artist.id;
                let key = format!("artist-{}:{id}", if singles { "singles" } else { "albums" });
                self.show_grid(
                    &key,
                    "artist-releases",
                    title,
                    page_gen,
                    move |api| async move { api.artist_releases(&id, singles).await },
                );
                app.set_nav_key("".into());
                app.set_grid_filter_hint(hint.into());
            }
            Page::Search(query) => {
                app.set_view("search".into());
                app.set_nav_key("".into());
                app.set_search_text(query.as_str().into());
                app.set_page_title(format!("Results for “{query}”").into());
                app.set_tracks_show_date(false);
                let cache_key = format!("search:{}", query.to_lowercase());
                let catalog_query = query.clone();
                let video_query = query.clone();
                self.load_video_catalog(
                    format!("search-videos:{}", query.to_lowercase()),
                    page_gen,
                    false,
                    move |api| async move { api.search_videos(&video_query).await },
                );
                self.load_catalog(
                    format!("search-catalog:{}", query.to_lowercase()),
                    page_gen,
                    move |api| async move { api.search_catalog(&catalog_query).await },
                    |ui, cards: Vec<Card>| {
                        ui.app()
                            .set_search_cards(ModelRc::from(ui.cards_model("search", &cards, 96)));
                    },
                );
                self.show_track_list(&cache_key, "Search", page_gen, move |api| async move {
                    api.search_tracks(&query).await
                });
            }
            Page::Queue => {
                app.set_view("queue".into());
                app.set_nav_key("queue".into());
                app.set_page_title("Play queue".into());
                app.set_tracks_show_date(false);
                let (tracks, context) = {
                    let st = self.state.borrow();
                    let upcoming = st
                        .order
                        .iter()
                        .skip(st.pos)
                        .filter_map(|i| st.queue.get(*i).cloned())
                        .collect();
                    (upcoming, st.context.clone())
                };
                self.set_tracks(tracks, &context);
            }
            Page::Settings => {
                app.set_view("settings".into());
                app.set_nav_key("".into());
                self.present_settings();
            }
            Page::Dsp => {
                app.set_view("dsp".into());
                app.set_nav_key("dsp".into());
                self.dsp_sync_device();
                self.dsp_present();
            }
        }
        let now = self.app().get_view();
        let modules = |v: &str| matches!(v, "home" | "modules" | "feed");
        if modules(&was) && !modules(now.as_str()) {
            self.drop_section_art();
        }
        if was == "grid" && now != "grid" {
            self.drop_grid_art();
        }
        let tracks_view = |v: &str| {
            matches!(
                v,
                "tracks" | "detail" | "album" | "artist" | "search" | "queue"
            )
        };
        if tracks_view(&was) && !tracks_view(now.as_str()) {
            self.drop_track_art();
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
                if let Some(ui) = weak
                    .upgrade()
                    .filter(|ui| ui.state.borrow().page_gen == page_gen)
                {
                    ui.app().set_page_cover(image.clone());
                    ui.app().set_page_tint(tint);
                }
            });
        }
    }

    /// Fetch the picture and biography for one credited artist of the current track
    /// (player-bar hover card).
    fn load_artist_hover_card(self: &Rc<Self>, id: String, name: String) {
        if id.is_empty() {
            return;
        }
        self.app().set_np_artist_name(name.into());
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
            let Ok(detail) = api.catalog_detail("artist", &id).await else {
                return;
            };
            on_ui(move |ui| {
                if ui.state.borrow().hover_artist != id {
                    return;
                }
                ui.app()
                    .set_np_artist_bio(short_bio(&detail.biography).into());
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
                if is_album {
                    app.set_page_artists(link_model(refs_to_links(&detail.artists)));
                }
                if let Some(artist) = detail.artist {
                    app.set_page_artist(card_row(&artist));
                    if let Some(url) = &artist.image_url {
                        let (weak, artist_id) = (Rc::downgrade(ui), artist.id.clone());
                        ui.load_image(url, 96, move |image, _| {
                            let Some(ui) = weak
                                .upgrade()
                                .filter(|ui| ui.state.borrow().page_gen == page_gen)
                            else {
                                return;
                            };
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
                    let label = if detail.track_count == 1 {
                        "TRACK"
                    } else {
                        "TRACKS"
                    };
                    let summary = format!(
                        "{} {label}  ({})",
                        detail.track_count,
                        fmt_time(detail.duration)
                    );
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

    fn present_video_catalog(&self, artist: bool, cards: Vec<Card>) {
        let model = ModelRc::new(VecModel::from(self.rows_for(&cards)));
        if artist {
            self.app().set_artist_videos(model);
        } else {
            self.app().set_search_videos(model);
        }
    }

    fn set_video_catalog_status(&self, artist: bool, status: &str) {
        if artist {
            self.app().set_artist_videos_status(status.into());
        } else {
            self.app().set_search_videos_status(status.into());
        }
    }

    fn load_video_catalog<F, Fut>(
        self: &Rc<Self>,
        key: String,
        page_gen: u64,
        artist: bool,
        fetch: F,
    ) where
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Vec<Card>, tide_api::ApiError>> + Send,
    {
        let cached = self.ctx.meta.get_json(&key);
        let cached_cards = cached
            .as_deref()
            .and_then(|json| serde_json::from_str::<Vec<Card>>(json).ok());
        let has_cache = cached_cards.is_some();
        self.set_video_catalog_status(artist, if has_cache { "" } else { "loading" });
        if let Some(cards) = cached_cards {
            self.present_video_catalog(artist, cards);
        }
        self.spawn(move |ctx| async move {
            let result = match ctx.api() {
                Some(api) => fetch(api).await,
                None => Err(tide_api::ApiError::Client("Not logged in".into())),
            };
            let mut changed = None;
            let failed = result.is_err();
            match result {
                Ok(cards) => {
                    if let Ok(json) = serde_json::to_string(&cards)
                        && cached.as_deref() != Some(&json)
                    {
                        let _ = ctx.meta.put_json(&key, &json);
                        changed = Some(cards);
                    }
                }
                Err(error) => tracing::warn!("video catalog load failed for {key}: {error}"),
            }
            on_ui(move |ui| {
                if ui.state.borrow().page_gen != page_gen {
                    return;
                }
                if let Some(cards) = changed {
                    ui.present_video_catalog(artist, cards);
                }
                ui.set_video_catalog_status(
                    artist,
                    if failed && !has_cache { "error" } else { "" },
                );
            });
        });
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
                    let model = ModelRc::new(VecModel::from(ui.rows_for(&cards)));
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
        let (id, album_id, album_title) = (
            artist_id.to_string(),
            album.id.clone(),
            album.title.to_lowercase(),
        );
        self.load_catalog(
            format!("artist-albums:{id}"),
            page_gen,
            {
                let id = id.clone();
                move |api| async move { api.artist_releases(&id, false).await }
            },
            move |ui, cards: Vec<Card>| {
                // Other editions of this album carry the same title.
                let keep: Vec<Card> = cards
                    .into_iter()
                    .filter(|c| c.id != album_id && c.title.to_lowercase() != album_title)
                    .collect();
                let rows = ui.rows_for(&keep);
                ui.app()
                    .set_artist_albums(ModelRc::new(VecModel::from(rows)));
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
                ui.app()
                    .set_related_artists(ModelRc::new(VecModel::from(ui.rows_for(&cards))));
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
        let Some(card) = self
            .page_card()
            .filter(|card| card.kind == "artist" || can_favorite(card))
        else {
            return;
        };
        let page_gen = self.state.borrow().page_gen;
        let app = self.app();
        let favorite = !app.get_page_favorite();
        app.set_page_favorite(favorite);
        if can_favorite(&card) {
            self.remember_collection(&card.kind, &card.id, favorite);
        }
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            if let Err(e) = set_card_favorite(&api, &card, favorite).await {
                tracing::error!("Could not update My Collection for {}: {e}", card.title);
                on_ui(move |ui| {
                    if ui.state.borrow().page_gen == page_gen {
                        ui.app().set_page_favorite(!favorite);
                    }
                    if can_favorite(&card) {
                        ui.remember_collection(&card.kind, &card.id, !favorite);
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
                                    .map(|c| CreditLine {
                                        role: c.role.into(),
                                        names: c.names.into(),
                                    })
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

    /// Open the context menu on an album, playlist, mix, artist or folder card.
    fn open_card_menu(self: &Rc<Self>, row: CardData, x: f32, y: f32) {
        let app = self.app();
        if app.get_menu_open() || !app.get_logged_in() || row.id.is_empty() {
            return;
        }
        if !matches!(
            row.kind.as_str(),
            "album" | "playlist" | "mix" | "artist" | "video" | "folder"
        ) {
            return;
        }
        let card = card_from_row(&row);
        // Opened from the page's own More button: the page knows the cover and the state.
        let on_page = self
            .page_card()
            .is_some_and(|page| page.kind == card.kind && page.id == card.id);
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
            let Some(track) = self.state.borrow().menu_track.clone() else {
                return;
            };
            if action == "add-to-playlist" && !arg.is_empty() {
                self.add_to_playlist(arg, vec![track.id], track.title);
            }
            return;
        }
        let Some(card) = self.state.borrow().menu_card.clone() else {
            return;
        };
        match action {
            "open" => self.open_card(app.get_menu_card()),
            "copy-link" => self.copy_card_link(&card),
            "rename" if card.kind == "folder" => {
                app.set_create_folder_id(card.id.into());
                app.set_create_name(card.title.into());
                app.set_create_kind("rename-folder".into());
            }
            "delete" if card.kind == "folder" => {
                app.set_create_folder_id(card.id.into());
                app.set_create_name(card.title.into());
                app.set_create_kind("delete-folder".into());
            }
            "create-playlist" if card.kind == "folder" => {
                app.set_create_folder_id(card.id.into());
                app.set_create_name("".into());
                app.set_create_kind("playlist".into());
            }
            "favorite" if can_favorite(&card) => {
                let favorite = !app.get_menu_favorite();
                let on_page = self
                    .page_card()
                    .is_some_and(|page| page.kind == card.kind && page.id == card.id);
                let page_gen = self.state.borrow().page_gen;
                if on_page {
                    app.set_page_favorite(favorite);
                }
                self.remember_collection(&card.kind, &card.id, favorite);
                self.spawn(move |ctx| async move {
                    let Some(api) = ctx.api() else { return };
                    match set_card_favorite(&api, &card, favorite).await {
                        Ok(()) => on_ui(move |ui| {
                            let text = if favorite {
                                "Added to My Collection"
                            } else {
                                "Removed from My Collection"
                            };
                            crate::alerts::notice(&ui.app(), text);
                            if card.kind == "playlist" {
                                ui.sync_playlists();
                            }
                        }),
                        Err(e) => {
                            tracing::error!(
                                "Could not update My Collection for {}: {e}",
                                card.title
                            );
                            on_ui(move |ui| {
                                if on_page && ui.state.borrow().page_gen == page_gen {
                                    ui.app().set_page_favorite(!favorite);
                                }
                                ui.remember_collection(&card.kind, &card.id, !favorite);
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
                        Err(e) => {
                            tracing::warn!("could not find the artist of {}: {e}", card.title)
                        }
                    }
                });
            }
            "play" if card.kind == "video" => self.play_video_card(&app.get_menu_card()),
            "play" | "play-next" | "add-to-playlist" => {
                let (action, arg) = (action.to_string(), arg.to_string());
                // The page already holds the tracks of the card it shows.
                let on_page = self
                    .page_card()
                    .is_some_and(|page| page.kind == card.kind && page.id == card.id);
                let loaded = on_page
                    .then(|| self.state.borrow().page_tracks.clone())
                    .filter(|t| !t.is_empty());
                if let Some(tracks) = loaded {
                    self.card_tracks_action(&action, &arg, &card, tracks);
                    return;
                }
                self.spawn(move |ctx| async move {
                    let Some(api) = ctx.api() else { return };
                    match api.tracks_of(&card.kind, &card.id).await {
                        Ok(tracks) => {
                            on_ui(move |ui| ui.card_tracks_action(&action, &arg, &card, tracks))
                        }
                        Err(e) => {
                            tracing::error!("Could not load the tracks of {}: {e}", card.title)
                        }
                    }
                });
            }
            _ => {}
        }
    }

    /// Play, queue or add to a playlist the tracks behind a card.
    fn card_tracks_action(
        self: &Rc<Self>,
        action: &str,
        arg: &str,
        card: &Card,
        tracks: Vec<TrackItem>,
    ) {
        if tracks.is_empty() {
            return;
        }
        match action {
            "play" => {
                self.state.borrow_mut().playing_card = Some((card.kind.clone(), card.id.clone()));
                self.play_queue(tracks, 0, &card.title, Some(false));
            }
            "play-next" if self.current_track().is_none() => {
                self.play_queue(tracks, 0, &card.title, Some(false))
            }
            "play-next" => {
                // Inserted last to first, so they follow in their own order.
                for track in tracks.into_iter().rev() {
                    self.queue_next(track);
                }
            }
            "add-to-playlist" if !arg.is_empty() => {
                self.add_to_playlist(
                    arg,
                    tracks.into_iter().map(|t| t.id).collect(),
                    card.title.clone(),
                );
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
        let model = if singles {
            app.get_artist_singles()
        } else {
            app.get_artist_albums()
        };
        self.card_art(
            if singles {
                "artist-singles"
            } else {
                "artist-albums"
            },
            model,
            from,
            upto,
        );
    }

    /// Lazily load artwork for cards [from, upto) of a carousel's model.
    fn card_art(self: &Rc<Self>, key: &str, model: ModelRc<CardData>, from: usize, upto: usize) {
        self.keep_card_images(key, model, from, upto, 240);
    }

    fn current_page(&self) -> Option<Page> {
        let st = self.state.borrow();
        st.history.get(st.hist_pos).cloned()
    }

    fn rows_for(&self, cards: &[Card]) -> Vec<CardData> {
        let st = self.state.borrow();
        cards
            .iter()
            .map(|c| card_row_fav(c, &st.collection))
            .collect()
    }

    fn row_for(&self, card: &Card) -> CardData {
        self.rows_for(std::slice::from_ref(card))
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    fn push_playing_card(&self) {
        let (kind, id) = self.state.borrow().playing_card.clone().unwrap_or_default();
        self.app().set_playing_card_kind(kind.into());
        self.app().set_playing_card_id(id.into());
    }

    fn apply_favorite_ids(&self, ids: FavoriteIds) {
        {
            let mut st = self.state.borrow_mut();
            st.collection = Collection {
                albums: ids.albums,
                playlists: ids.playlists,
                videos: ids.videos,
            };
        }
        self.restamp_card_favorites();
    }

    fn remember_collection(&self, kind: &str, id: &str, favorite: bool) {
        {
            let mut st = self.state.borrow_mut();
            let Some(set) = st.collection.set_mut(kind) else {
                return;
            };
            if favorite {
                set.insert(id.to_string());
            } else {
                set.remove(id);
            }
        }
        self.patch_card_favorite(kind, id, favorite);
        if kind == "video" {
            self.video_favorite_changed(id, favorite);
        }
        let ids = {
            let st = self.state.borrow();
            FavoriteIds {
                albums: st.collection.albums.clone(),
                playlists: st.collection.playlists.clone(),
                videos: st.collection.videos.clone(),
            }
        };
        let _ = self.ctx.meta.put_json(
            "favorite-ids",
            &serde_json::to_string(&ids).unwrap_or_default(),
        );
    }

    fn restamp_card_favorites(&self) {
        let (collection, models) = {
            let st = self.state.borrow();
            let mut models: Vec<Rc<VecModel<CardData>>> = st
                .section_models
                .iter()
                .map(|(cards, _, _)| cards.clone())
                .collect();
            models.extend(st.grid.models.iter().cloned());
            (st.collection.clone(), models)
        };
        for model in models {
            restamp_model(&ModelRc::from(model), &collection);
        }
        let app = self.app();
        restamp_model(&app.get_search_cards(), &collection);
        restamp_model(&app.get_artist_albums(), &collection);
        restamp_model(&app.get_artist_singles(), &collection);
        restamp_model(&app.get_artist_videos(), &collection);
        restamp_model(&app.get_search_videos(), &collection);
        restamp_model(&app.get_related_artists(), &collection);
        let mut page = app.get_page_card();
        let fav = match page.kind.as_str() {
            "album" | "playlist" | "video" => collection.contains(&page.kind, &page.id),
            _ => page.favorite,
        };
        if page.favorite != fav {
            page.favorite = fav;
            app.set_page_card(page);
        }
    }

    fn patch_card_favorite(&self, kind: &str, id: &str, favorite: bool) {
        let models = {
            let st = self.state.borrow();
            let mut models: Vec<Rc<VecModel<CardData>>> = st
                .section_models
                .iter()
                .map(|(cards, _, _)| cards.clone())
                .collect();
            models.extend(st.grid.models.iter().cloned());
            models
        };
        let patch = |model: &ModelRc<CardData>| {
            for i in 0..model.row_count() {
                if let Some(mut row) = model.row_data(i)
                    && row.kind == kind
                    && row.id == id
                    && row.favorite != favorite
                {
                    row.favorite = favorite;
                    model.set_row_data(i, row);
                }
            }
        };
        for model in models {
            patch(&ModelRc::from(model));
        }
        let app = self.app();
        patch(&app.get_search_cards());
        patch(&app.get_artist_albums());
        patch(&app.get_artist_singles());
        patch(&app.get_artist_videos());
        patch(&app.get_search_videos());
        patch(&app.get_related_artists());
        let mut page = app.get_page_card();
        if page.kind == kind && page.id == id {
            page.favorite = favorite;
            app.set_page_card(page);
        }
        if app.get_menu_open() && app.get_menu_card().id == id && app.get_menu_card().kind == kind {
            app.set_menu_favorite(favorite);
        }
    }

    fn set_home_tab(self: &Rc<Self>, tab: i32) {
        if !matches!(self.current_page(), Some(Page::Home)) {
            return;
        }
        let tab = tab.clamp(0, 2);
        self.app().set_home_tab(tab);
        self.load_home_feed(tab);
    }

    fn load_home_feed(self: &Rc<Self>, tab: i32) {
        let (key, feed) = match tab {
            1 => ("home:staff", "editorial"),
            2 => ("home:uploads", "uploads"),
            _ => ("home", "static"),
        };
        let page_gen = self.state.borrow().page_gen;
        if self.ctx.meta.get_json(key).is_none() {
            self.app().set_loading(true);
            self.set_sections(Vec::new());
        }
        self.load_catalog(
            key.into(),
            page_gen,
            move |api| async move { api.home_feed(feed).await },
            move |ui, sections| {
                if ui.app().get_home_tab() == tab && matches!(ui.current_page(), Some(Page::Home)) {
                    ui.app().set_loading(false);
                    ui.set_sections(sections);
                }
            },
        );
    }

    fn load_page_sections<F, Fut>(self: &Rc<Self>, key: &str, page_gen: u64, fetch: F)
    where
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Vec<Section>, tide_api::ApiError>> + Send,
    {
        if self.ctx.meta.get_json(key).is_none() {
            self.app().set_loading(true);
            self.set_sections(Vec::new());
        }
        self.load_catalog(key.into(), page_gen, fetch, |ui, sections| {
            ui.app().set_loading(false);
            ui.set_sections(sections);
        });
    }

    fn present_later(self: &Rc<Self>, items: LaterItems) {
        self.app().set_loading(false);
        self.app().set_page_title("Save for Later".into());
        self.app().set_nav_key("later".into());
        self.state.borrow_mut().later = items.clone();
        if items.cards.is_empty() && items.tracks.is_empty() {
            self.app().set_view("modules".into());
            self.set_sections(Vec::new());
            return;
        }
        if items.cards.is_empty() {
            self.app().set_view("tracks".into());
            self.app().set_tracks_show_date(false);
            self.app().set_filter_text("".into());
            self.set_tracks(items.tracks, "Save for Later");
            return;
        }
        self.app().set_view("modules".into());
        let mut sections = Vec::new();
        if !items.tracks.is_empty() {
            sections.push(Section {
                title: "Tracks".into(),
                layout: "tracks".into(),
                tracks: items.tracks,
                ..Default::default()
            });
        }
        sections.push(Section {
            title: "Saved".into(),
            layout: "cards".into(),
            cards: items.cards,
            ..Default::default()
        });
        self.set_sections(sections);
    }

    fn play_card(self: &Rc<Self>, row: CardData) {
        if row.id.is_empty() {
            return;
        }
        match row.kind.as_str() {
            "folder" | "page" => {}
            "video" => {
                let playing = self.video_active() && self.app().get_current_id() == row.id;
                if playing {
                    self.video_toggle_play();
                } else {
                    self.play_video_card(&row);
                }
            }
            "tracks" => {
                let favs = self.state.borrow().favorites.clone();
                self.state.borrow_mut().playing_card = Some(("tracks".into(), row.id.to_string()));
                self.play_queue(favs, 0, "My Tracks", Some(false));
            }
            _ => {
                let card = card_from_row(&row);
                let playing = self
                    .state
                    .borrow()
                    .playing_card
                    .as_ref()
                    .is_some_and(|(kind, id)| kind == &card.kind && id == &card.id);
                if playing {
                    self.toggle_play();
                    return;
                }
                let on_page = self
                    .page_card()
                    .is_some_and(|page| page.kind == card.kind && page.id == card.id);
                let loaded = on_page
                    .then(|| self.state.borrow().page_tracks.clone())
                    .filter(|t| !t.is_empty());
                if let Some(tracks) = loaded {
                    self.card_tracks_action("play", "", &card, tracks);
                    return;
                }
                self.spawn(move |ctx| async move {
                    let Some(api) = ctx.api() else { return };
                    match api.tracks_of(&card.kind, &card.id).await {
                        Ok(tracks) => {
                            on_ui(move |ui| ui.card_tracks_action("play", "", &card, tracks))
                        }
                        Err(e) => {
                            tracing::error!("Could not load the tracks of {}: {e}", card.title)
                        }
                    }
                });
            }
        }
    }

    /// Play a video card with the videos around it on screen behind Next / Previous.
    fn play_video_card(self: &Rc<Self>, row: &CardData) {
        let mut card = card_from_row(row);
        let (queue, context) = self.video_queue_for(&card);
        if let Some(full) = queue.iter().find(|c| c.id == card.id) {
            card.duration = full.duration;
        }
        self.play_video(card, queue, &context);
    }

    fn toggle_card_favorite(self: &Rc<Self>, row: CardData) {
        let card = card_from_row(&row);
        if !can_favorite(&card) {
            return;
        }
        let favorite = !row.favorite;
        let page_gen = self.state.borrow().page_gen;
        let on_page = self
            .page_card()
            .is_some_and(|page| page.kind == card.kind && page.id == card.id);
        self.remember_collection(&card.kind, &card.id, favorite);
        if on_page {
            self.app().set_page_favorite(favorite);
        }
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match set_card_favorite(&api, &card, favorite).await {
                Ok(()) => on_ui(move |ui| {
                    let text = if favorite {
                        "Added to My Collection"
                    } else {
                        "Removed from My Collection"
                    };
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
                        ui.remember_collection(&card.kind, &card.id, !favorite);
                    });
                }
            }
        });
    }

    fn load_catalog<T, F, Fut, P>(self: &Rc<Self>, key: String, page_gen: u64, fetch: F, present: P)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, tide_api::ApiError>> + Send,
        P: Fn(&Rc<Self>, T) + Send + 'static,
    {
        let cached = self.ctx.meta.get_json(&key);
        if let Some(data) = cached
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
        {
            present(self, data);
        }
        self.spawn(move |ctx| async move {
            let mut changed = None;
            if let Some(api) = ctx.api() {
                match fetch(api).await {
                    Ok(data) => {
                        if let Ok(json) = serde_json::to_string(&data)
                            && cached.as_deref() != Some(&json)
                        {
                            let _ = ctx.meta.put_json(&key, &json);
                            changed = Some(data);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("catalog load failed for {key}: {e}");
                        on_ui(move |ui| {
                            if ui.state.borrow().page_gen == page_gen {
                                ui.app().set_loading(false);
                            }
                        });
                    }
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
    fn show_grid<F, Fut>(
        self: &Rc<Self>,
        key: &str,
        kind: &str,
        title: &str,
        page_gen: u64,
        fetch: F,
    ) where
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
            saved
                .filter(|s| options.contains(&s.as_str()))
                .cloned()
                .unwrap_or_else(|| options[0].to_string())
        };
        let sort = pick(saved.first(), sorts);
        let group = if groups.is_empty() {
            "None".to_string()
        } else {
            pick(saved.get(1), groups)
        };
        let combine = saved.get(2).is_some_and(|s| s == "combine");
        let texts = |options: &[&str]| {
            ModelRc::new(VecModel::from(
                options
                    .iter()
                    .map(|o| slint::SharedString::from(*o))
                    .collect::<Vec<_>>(),
            ))
        };
        app.set_grid_kind(kind.into());
        app.set_grid_sort_options(texts(sorts));
        app.set_grid_group_options(texts(groups));
        app.set_grid_sort(sort.as_str().into());
        app.set_grid_group(group.as_str().into());
        app.set_grid_combine_others(combine);
        app.set_grid_can_create(kind == "playlists");
        app.set_grid_filter_hint(format!("Filter {kind}").into());
        app.set_filter_text("".into());
        self.state.borrow_mut().grid = Grid {
            kind: kind.into(),
            sort,
            group,
            combine_others: combine,
            ..Default::default()
        };
        let cache_key = format!("cards:{key}");
        let cached: Vec<Card> = self
            .ctx
            .meta
            .get_json(&cache_key)
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
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

    fn show_track_list<F, Fut>(
        self: &Rc<Self>,
        cache_key: &str,
        context: &str,
        page_gen: u64,
        fetch: F,
    ) where
        F: FnOnce(ApiHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Vec<TrackItem>, tide_api::ApiError>> + Send,
    {
        let cached: Vec<TrackItem> = self
            .ctx
            .meta
            .get_json(cache_key)
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
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
            st.page_tracks
                .iter()
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
        let track = self
            .state
            .borrow()
            .menu_track
            .as_ref()
            .filter(|track| track.id == id)
            .cloned();
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
            let upcoming = st
                .order
                .iter()
                .skip(st.pos)
                .map(|&i| st.queue[i].clone())
                .collect();
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
            let playing = self.page_card().map(|c| (c.kind, c.id));
            self.state.borrow_mut().playing_card = playing;
            self.play_queue(tracks, index, &context, None);
        }
    }

    fn play_from_section(self: &Rc<Self>, section: usize, id: &str) {
        let found = {
            let st = self.state.borrow();
            st.sections
                .get(section)
                .map(|s| (s.tracks.clone(), s.title.clone()))
        };
        if let Some((tracks, title)) = found
            && let Some(index) = tracks.iter().position(|t| t.id == id)
        {
            self.state.borrow_mut().playing_card = None;
            self.play_queue(tracks, index, &title, None);
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
        let playing = self.page_card().map(|c| (c.kind, c.id));
        self.state.borrow_mut().playing_card = playing;
        let start = if shuffle {
            shuffled(tracks.len(), usize::MAX)[0]
        } else {
            0
        };
        self.play_queue(tracks, start, &context, Some(shuffle));
    }

    fn play_queue(
        self: &Rc<Self>,
        tracks: Vec<TrackItem>,
        index: usize,
        context: &str,
        shuffle: Option<bool>,
    ) {
        {
            let mut st = self.state.borrow_mut();
            if let Some(shuffle) = shuffle {
                st.shuffle = shuffle;
            }
            st.order = if st.shuffle {
                shuffled(tracks.len(), index)
            } else {
                (0..tracks.len()).collect()
            };
            st.pos = if st.shuffle { 0 } else { index };
            st.queue = tracks;
            st.context = context.to_string();
            st.pending_seek = None;
        }
        self.app().set_shuffle(self.state.borrow().shuffle);
        self.push_playing_card();
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
        let volume = if st.muted {
            st.volume_before_mute
        } else {
            self.app().get_volume()
        };
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
        let Some(track) = self.current_track() else {
            return;
        };
        let app = self.app();
        let (context, favorite) = {
            let st = self.state.borrow();
            (st.context.clone(), st.fav_ids.contains(&track.id))
        };
        self.push_playing_card();
        app.set_has_track(true);
        app.set_current_id(track.id.as_str().into());
        app.set_np_title(track.title.as_str().into());
        app.set_np_artist(track.artist.as_str().into());
        app.set_np_artist_id(track.artist_id.clone().unwrap_or_default().as_str().into());
        app.set_np_artists(link_model(artist_links(&track)));
        app.set_np_context(context.into());
        app.set_np_explicit(track.explicit);
        app.set_np_favorite(favorite);
        app.set_duration_secs(track.duration_secs as i32);
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
                cached_cover
                    .as_deref()
                    .and_then(|p| p.to_str())
                    .or(cover.as_deref()),
            );
        });
    }

    fn present_format(&self) {
        let app = self.app();
        let Some(format) = self.ctx.player.format_info() else {
            return;
        };
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
        // Each sound output has its own EQs: follow playback to a new one.
        self.dsp_sync_device();
        let output = player.output_info();
        app.set_output_info(
            format!(
                "{} · {} · {}",
                output.name,
                output.kind.label(),
                khz(output.sample_rate)
            )
            .into(),
        );
        let Some(format) = player.format_info().filter(|_| app.get_has_track()) else {
            if let Ok(mut st) = self.state.try_borrow_mut() {
                st.shown_bitrate = 0;
            }
            app.set_signal_quality("".into());
            return;
        };
        let volume = if app.get_muted() {
            0.0
        } else {
            app.get_volume()
        };
        if let Ok(mut st) = self.state.try_borrow_mut() {
            st.shown_bitrate = format.bitrate_kbps;
        }
        let path = signal_path(
            format,
            &output,
            volume,
            player.rate_matching(),
            player.is_released(),
            self.dsp_signal_detail().as_deref(),
        );
        let mut stages = path.stages;
        let mut note = path.note;
        if let Some(picture) = self.video_signal_stage() {
            // A video: its picture first, then its soundtrack as the audio source.
            let quality = stages[0].quality;
            stages[0].title = "Audio".into();
            stages.insert(
                0,
                crate::signal::Stage {
                    kind: "video",
                    title: "Video".into(),
                    detail: picture,
                    quality,
                },
            );
            if format.bits_per_sample == 0 && note.starts_with("The stream is AAC") {
                note = "The video's soundtrack is AAC, a lossy format.".into();
            }
        }
        app.set_signal_quality(path.quality.key().into());
        app.set_signal_title(path.quality.title().into());
        app.set_signal_note(note.into());
        let stages: Vec<SignalStage> = stages
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
        let Some(track) = self.current_track() else {
            return;
        };
        self.stop_video();
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
        if self.video_active() {
            self.video_toggle_play();
            return;
        }
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
                    self.state
                        .borrow_mut()
                        .pending_seek
                        .take()
                        .unwrap_or(Duration::ZERO)
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

    fn skip(self: &Rc<Self>, delta: i32, manual: bool) {
        if self.video_active() {
            self.video_skip(delta, manual);
            return;
        }
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
        if self.video_active() {
            self.video_previous();
            return;
        }
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
        if self.video_active() {
            self.video_seek(fraction);
            return;
        }
        let Some(track) = self.current_track() else {
            return;
        };
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
        if self.video_active() {
            let duration = self.app().get_duration_secs().max(1) as f32;
            self.video_seek(to.as_secs_f32() / duration);
            return;
        }
        let Some(track) = self.current_track() else {
            return;
        };
        let total = track.duration_secs as f32;
        let fraction = if total > 0.0 {
            to.as_secs_f32() / total
        } else {
            0.0
        };
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
        self.sync_video_volume();
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
        self.sync_video_volume();
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
        let _ = self
            .ctx
            .meta
            .update_playback_order(&order, self.current_cursor());
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
        self.dsp_flush();
        if !app.get_has_track() || self.state.borrow().loading_track {
            return;
        }
        let player = &self.ctx.player;

        // Follow the system output (headphones plugged in, Bluetooth connected, ...).
        let (check_output, save_tick, idle_ticks) = {
            let mut st = self.state.borrow_mut();
            st.tick_count = st.tick_count.wrapping_add(1);
            // A video's soundtrack waits for its picture: not idle meanwhile.
            st.idle_ticks = if player.is_playing() || self.video_active() && app.get_playing() {
                0
            } else {
                st.idle_ticks.saturating_add(1)
            };
            (
                st.tick_count.is_multiple_of(8),
                st.tick_count.is_multiple_of(20),
                st.idle_ticks,
            )
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
                let released = tokio::task::spawn_blocking(move || ctx.player.release_output())
                    .await
                    .unwrap_or(false);
                if released {
                    on_ui(|ui| ui.present_signal());
                }
            });
        }
        if idle_ticks == 1
            || (idle_ticks == 0
                && player.is_playing()
                && self.app().get_signal_note().starts_with("Paused"))
        {
            self.present_signal();
        }

        // A video is the current media: its own pump drives the player bar.
        if self.video_active() {
            return;
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

        let Some(track) = self.current_track() else {
            return;
        };
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
                let _ = m.set_playback_state(
                    self.ctx.player.is_playing(),
                    Some(self.ctx.player.elapsed()),
                );
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
        let Some(track) = next
            .and_then(|p| st.order.get(p))
            .and_then(|i| st.queue.get(*i))
            .cloned()
        else {
            return;
        };
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
            MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => {
                self.toggle_play()
            }
            MediaControlEvent::Next => self.skip(1, true),
            MediaControlEvent::Previous => self.previous(),
            MediaControlEvent::Stop => {
                if self.ctx.player.is_playing() || (self.video_active() && self.app().get_playing())
                {
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
                    let known = st
                        .menu_track
                        .iter()
                        .chain(st.page_tracks.iter())
                        .chain(st.queue.iter())
                        .find(|t| t.id == id)
                        .cloned();
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
        if self.video_active() && self.app().get_current_id() == id {
            self.video_toggle_favorite();
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
        let Some(track) = self.current_track() else {
            return;
        };
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
            let cached: Option<Lyrics> = ctx
                .meta
                .get_json(&cache_key)
                .and_then(|j| serde_json::from_str(&j).ok());
            let lyrics = match (cached, ctx.api()) {
                (Some(lyrics), _) => Some(lyrics),
                (None, Some(api)) => api.lyrics(&id).await.ok().inspect(|l| {
                    let _ = ctx
                        .meta
                        .put_json(&cache_key, &serde_json::to_string(l).unwrap_or_default());
                }),
                (None, None) => None,
            };
            on_ui(move |ui| {
                if ui.state.borrow().lyrics_for != id {
                    return;
                }
                let lyrics = lyrics.unwrap_or_default();
                let empty = lyrics.plain.trim().is_empty() && lyrics.synced.is_empty();
                ui.app().set_lyrics_status(
                    if empty {
                        "No lyrics available for this track"
                    } else {
                        ""
                    }
                    .into(),
                );
                if lyrics.synced.is_empty() && !empty {
                    let lines: Vec<LyricLine> = lyrics
                        .plain
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .take(8)
                        .map(|l| LyricLine {
                            text: l.trim().into(),
                            active: false,
                        })
                        .collect();
                    ui.app()
                        .set_lyrics(ModelRc::from(Rc::new(VecModel::from(lines))));
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
                .map(|(i, (_, text))| LyricLine {
                    text: text.as_str().into(),
                    active: idx == Some(start + i),
                })
                .collect::<Vec<_>>()
        };
        self.app()
            .set_lyrics(ModelRc::from(Rc::new(VecModel::from(lines))));
    }

    // ------------------------------------------------------------ session

    fn login(self: &Rc<Self>) {
        self.app().set_auth_status("Contacting TIDAL…".into());
        self.spawn(|ctx| async move {
            let mut svc = ctx.svc.lock().await;
            let info = match svc.start_device_auth().await {
                Ok(info) => info,
                Err(e) => {
                    on_ui(move |ui| {
                        ui.app()
                            .set_auth_status(format!("Couldn't start login: {e}").into())
                    });
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
            match svc
                .poll_device_auth(&info.device_code, info.expires_in, info.interval)
                .await
            {
                Ok(_) => {
                    drop(svc);
                    on_ui(|ui| {
                        ui.app().set_user_code("".into());
                        ui.app().set_auth_status("".into());
                        ui.boot(true);
                    });
                }
                Err(e) => on_ui(move |ui| {
                    ui.app()
                        .set_auth_status(format!("Login failed: {e}").into())
                }),
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
        *self.state.borrow_mut() = State {
            volume_before_mute: 1.0,
            ..Default::default()
        };
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
            (
                200,
                "",
                Box::new(|ui| {
                    let cards = ui.app().get_grid_cards();
                    if let Some(card) = cards.iter().find(|c| c.kind == "playlist") {
                        ui.open_card(card);
                    }
                }),
            ),
            (4000, "04a-playlist-detail", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("profiles"))),
            (4000, "04b-profiles", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("albums"))),
            (4000, "05-albums", Box::new(|_| {})),
            (
                200,
                "",
                Box::new(|ui| ui.set_grid_arrangement(None, Some("Artist".into()), None)),
            ),
            (1500, "05b-albums-by-artist", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("videos"))),
            (4000, "05c-videos", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("albums"))),
            (1500, "", Box::new(|_| {})),
            (
                200,
                "",
                Box::new(|ui| ui.set_grid_arrangement(None, Some("None".into()), None)),
            ),
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
            (
                200,
                "",
                Box::new(|ui| {
                    ui.app().set_scroll_y(0.0);
                    ui.app().set_debug_open(true);
                    tracing::error!(track_id = 1, "Snapshot tour: sample error alert");
                }),
            ),
            (800, "06-debug-panel", Box::new(|_| {})),
            // The shared context menu: on the album (More), then on a track.
            (
                200,
                "",
                Box::new(|ui| {
                    ui.app().set_debug_open(false);
                    ui.app().set_alert_text("".into());
                    let card = ui.app().get_page_card();
                    ui.open_card_menu(card, 900.0, 300.0);
                }),
            ),
            (1500, "06-menu-album", Box::new(|_| {})),
            (
                100,
                "",
                Box::new(|ui| ui.app().set_menu_sub("playlists".into())),
            ),
            (500, "06-menu-album-playlists", Box::new(|_| {})),
            (
                200,
                "",
                Box::new(|ui| {
                    ui.app().set_menu_open(false);
                    let id = ui.state.borrow().page_tracks.first().map(|t| t.id.clone());
                    if let Some(id) = id {
                        ui.open_track_menu(&id, 700.0, 380.0);
                    }
                }),
            ),
            (1000, "06-menu-track", Box::new(|_| {})),
            (100, "", Box::new(|ui| ui.app().set_menu_open(false))),
            (
                200,
                "",
                Box::new(|ui| {
                    ui.app().set_debug_open(false);
                    ui.app().set_alert_text("".into());
                    ui.app().set_create_kind("playlist".into());
                }),
            ),
            (600, "06-create-dialog", Box::new(|_| {})),
            (100, "", Box::new(|ui| ui.app().set_create_kind("".into()))),
            (
                200,
                "",
                Box::new(|ui| {
                    let app = ui.app();
                    let artist = app.get_page_artist();
                    if !artist.id.is_empty() {
                        app.invoke_open_card(artist);
                    } else if let Some(track) = app.get_tracks().row_data(0) {
                        app.invoke_open_track_artist(track);
                    }
                }),
            ),
            (4000, "06a-artist-detail", Box::new(|_| {})),
            (
                200,
                "",
                Box::new(|ui| {
                    if ui.app().get_view() == "artist" {
                        ui.app().invoke_show_artist_tracks();
                    }
                }),
            ),
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
            (
                200,
                "06f-swipe-held",
                Box::new(|ui| {
                    if ui.swipe_begin(true) {
                        ui.swipe_move(ui.app().get_pane_width() * 0.4);
                    }
                }),
            ),
            (200, "", Box::new(|ui| ui.swipe_end(true))),
            (800, "06g-swipe-called-off", Box::new(|_| {})),
            (
                200,
                "",
                Box::new(|ui| ui.app().invoke_search("grammy".into())),
            ),
            (4000, "07-search", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("settings"))),
            (1500, "08-settings", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.app().set_settings_tab(1))),
            (1000, "09-settings-account", Box::new(|_| {})),
            (200, "", Box::new(|ui| ui.navigate_key("dsp"))),
            (1000, "09a-dsp", Box::new(|_| {})),
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
            // A music video: full window, then Similar videos, a fixed
            // rendition, the player bar with the view closed, and back to music.
            steps.push((200, "", Box::new(|ui| ui.navigate_key("videos"))));
            steps.push((
                4000,
                "",
                Box::new(|ui| {
                    if let Some(card) = ui.app().get_grid_cards().row_data(0) {
                        ui.open_card(card);
                    }
                }),
            ));
            steps.push((8000, "14-video", Box::new(|_| {})));
            // The soundtrack goes through the signal path; print it.
            steps.push((
                200,
                "",
                Box::new(|ui| {
                    let app = ui.app();
                    for s in app.get_signal_stages().iter() {
                        println!(
                            "signal stage {}: {} · {} ({})",
                            s.kind, s.title, s.detail, s.quality
                        );
                    }
                    println!("signal note: {}", app.get_signal_note());
                }),
            ));
            steps.push((200, "", Box::new(|ui| ui.video_seek(0.4))));
            steps.push((6000, "14b-video-seek", Box::new(|_| {})));
            steps.push((
                200,
                "",
                Box::new(|ui| ui.app().set_video_wake(ui.app().get_video_wake() + 1)),
            ));
            steps.push((600, "14a-video-controls", Box::new(|_| {})));
            steps.push((
                200,
                "",
                Box::new(|ui| ui.app().set_video_similar_open(true)),
            ));
            steps.push((2500, "15-video-similar", Box::new(|_| {})));
            steps.push((
                200,
                "",
                Box::new(|ui| ui.app().set_video_similar_open(false)),
            ));
            steps.push((200, "", Box::new(|ui| ui.set_video_quality("360p"))));
            steps.push((5000, "16-video-360p", Box::new(|_| {})));
            steps.push((200, "", Box::new(|ui| ui.set_video_quality("1080p"))));
            steps.push((200, "", Box::new(|ui| ui.show_video_view(false))));
            steps.push((1500, "17-video-bar", Box::new(|_| {})));
            // The quality pill opens the signal path with the quality column.
            steps.push((
                200,
                "",
                Box::new(|ui| {
                    use slint::platform::{PointerEventButton, WindowEvent};
                    let win = ui.win.window();
                    let size = win.size().to_logical(win.scale_factor());
                    let position =
                        slint::LogicalPosition::new(size.width - 86.0, size.height - 48.0);
                    let button = PointerEventButton::Left;
                    win.dispatch_event(WindowEvent::PointerMoved { position });
                    win.dispatch_event(WindowEvent::PointerPressed { position, button });
                    win.dispatch_event(WindowEvent::PointerReleased { position, button });
                }),
            ));
            steps.push((800, "17a-video-signal-path", Box::new(|_| {})));
            steps.push((200, "", Box::new(|ui| ui.stop_video())));
            steps.push((1000, "18-video-stopped", Box::new(|_| {})));
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
                                "snapshot {name}: {saved:?} view={} scroll={} tracks={} cards={} sections={} playing={} elapsed={} quality={} signal={:?} output={:?} video={}/{}",
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
                                app.get_video_active(),
                                app.get_video_quality_label(),
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
