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
    ApiHandle, ArtistRef, Card, CatalogDetail, FavoriteIds, LaterItems, Lyrics, PlaylistDetail,
    Section,
    TidalService, TrackItem,
};
use tide_audio::{AudioPlayer, Loudness, Normalization, TrackQueueItem};
use tide_cache::{AudioCache, ImageCache, MetadataCache, PlaybackCursor, SavedPlayback};
use tide_macos::{MacMediaControls, MediaControlEvent};

use crate::signal::signal_path;
use crate::{
    App, ArtistLink, CardData, CreditLine, LyricLine, MainWindow, MemPart, MemSample, SectionData,
    SignalStage, TrackData,
};

/// Cache key for "best quality the account can stream".
pub const QUALITY_KEY: &str = "MAX";
/// Preload the next track once the playhead (or a scrub) is in the last quarter.
const PREFETCH_ZONE: f32 = 0.75;
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
    /// Draw each sidebar page's key letter white. Holding ⌘ shows them anyway.
    pub show_key_letters: bool,
    /// Size limit of the offline audio cache.
    pub cache_max_gb: f64,
    /// Re-download the most played tracks when they are missing from the cache.
    pub keep_most_played: bool,
    /// Switch the output device to each track's sample rate (bit-perfect).
    pub rate_matching: bool,
    /// Hand the device back its original sample rate on pause and quit.
    #[serde(default = "default_true")]
    pub restore_sample_rate: bool,
    /// Blend each track into the next over `crossfade_secs` (1–12) instead of gapless.
    pub crossfade: bool,
    pub crossfade_secs: u32,
    /// Play every track at the same loudness, from TIDAL's ReplayGain values.
    pub normalize_volume: bool,
    /// Level whole albums instead of single tracks, keeping an album's own dynamics.
    pub normalize_album: bool,
    /// Collection grid artwork size: 0 smallest … 4 largest (3 is the original size).
    pub grid_size: i32,
    /// Whole-interface zoom, independent of display DPI.
    pub zoom_percent: i32,
    /// Sliding back / forward transitions (page snapshots) and a larger
    /// graphics budget. Off by default: TIDE keeps its memory low and still
    /// animates at the display's refresh rate.
    pub fancy_animation: bool,
    /// Now Playing shows the lyrics beside the artwork (the Lyrics toggle).
    #[serde(default = "default_true")]
    pub show_lyrics: bool,
    /// Draw the player bar as glass over what scrolls beneath it. Unset until
    /// the listener uses the switch: glass then follows macOS Reduce
    /// Transparency. Their choice overrides it either way.
    pub glass: Option<bool>,
    /// Last window placement, kept with preferences so logout preserves it.
    #[cfg(target_os = "macos")]
    pub window: Option<crate::window_state::WindowState>,
    /// An update the listener chose to skip; only Check for Updates… offers it again.
    pub skipped_version: String,
    /// Albums with an animated cover play it (Now Playing, the player bar,
    /// album pages, album tiles under the pointer).
    #[serde(default = "default_true")]
    pub animated_artwork: bool,
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
            show_key_letters: true,
            cache_max_gb: 20.0,
            keep_most_played: true,
            rate_matching: true,
            restore_sample_rate: true,
            crossfade: false,
            crossfade_secs: 6,
            normalize_volume: false,
            normalize_album: false,
            grid_size: 3,
            zoom_percent: 100,
            fancy_animation: false,
            show_lyrics: true,
            glass: None,
            #[cfg(target_os = "macos")]
            window: None,
            skipped_version: String::new(),
            animated_artwork: true,
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
        settings.zoom_percent = crate::zoom::normalize(settings.zoom_percent);
        settings.crossfade_secs = settings.crossfade_secs.clamp(1, 12);
        settings
    }

    /// Crossfade length the player runs; 0 is plain gapless.
    pub fn crossfade_secs(&self) -> u32 {
        if self.crossfade {
            self.crossfade_secs
        } else {
            0
        }
    }

    pub fn normalization(&self) -> Normalization {
        match (self.normalize_volume, self.normalize_album) {
            (false, _) => Normalization::Off,
            (true, false) => Normalization::Track,
            (true, true) => Normalization::Album,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_summary_reads_like_the_official_header() {
        let detail = |tracks, duration, followers| PlaylistDetail {
            track_count: tracks,
            duration,
            followers,
            ..Default::default()
        };
        assert_eq!(
            playlist_summary(&detail(500, 72_600, 11_500)),
            "500 TRACKS  (20:10:00)  •  11.5K FANS"
        );
        assert_eq!(playlist_summary(&detail(1, 187, 1)), "1 TRACK  (3:07)  •  1 FAN");
        assert_eq!(playlist_summary(&detail(0, 0, 0)), "0 TRACKS");
        assert_eq!(
            [999, 1_000, 999_949, 999_950, 2_340_000].map(fmt_count),
            ["999", "1K", "999.9K", "1M", "2.3M"].map(String::from)
        );
    }

    #[test]
    fn add_targets_put_the_recently_changed_playlists_first() {
        let playlist = |id: &str, by: &str, updated: &str| Card {
            id: id.into(),
            kind: "playlist".into(),
            title: id.into(),
            by: by.into(),
            updated: updated.into(),
            ..Default::default()
        };
        let mut cards = vec![
            Card {
                id: "folder".into(),
                kind: "folder".into(),
                ..Default::default()
            },
            playlist("a", "Me", "2020-01-01T00:00:00.000+0000"),
            playlist("b", "Me", "2026-03-01T00:00:00.000+0000"),
            playlist("theirs", "TIDAL", "2026-11-01T00:00:00.000+0000"),
            playlist("c", "Me", ""),
            playlist("d", "Me", "2026-01-01T00:00:00.000+0000"),
            playlist("e", "Me", "2021-01-01T00:00:00.000+0000"),
            playlist("f", "Me", "2025-01-01T00:00:00.000+0000"),
            playlist("g", "Me", "2019-01-01T00:00:00.000+0000"),
        ];
        let ids = |cards: &[Card], adds: &HashMap<String, i64>| {
            let (order, recent) = add_targets(cards, adds);
            let ids: Vec<String> = order.into_iter().map(|card| card.id).collect();
            (ids.join(""), recent)
        };
        // Newest five first, then the others as listed; never someone else's.
        assert_eq!(ids(&cards, &HashMap::new()), ("bdfeacg".to_string(), 5));
        // A playlist TIDE just added to leads, even before TIDAL's date catches up.
        let now = tide_api::web::iso_seconds("2026-11-20T00:00:00");
        let adds = HashMap::from([("g".to_string(), now), ("gone".to_string(), now)]);
        assert_eq!(ids(&cards, &adds), ("gbdfeac".to_string(), 5));
        // Nothing to promote when every playlist fits in the recent rows.
        cards.truncate(6);
        assert_eq!(ids(&cards, &adds), ("abcd".to_string(), 0));
    }

    #[test]
    fn lyric_rows_skip_blank_lines_and_the_sung_row_follows_the_time() {
        let synced: Vec<(u64, String)> = [(1_000, "One"), (4_000, ""), (6_000, "Two"), (9_000, " Three ")]
            .map(|(t, l)| (t, l.to_string()))
            .to_vec();
        let lyrics = Lyrics { plain: String::new(), synced: synced.clone() };
        let rows = lyric_rows(&lyrics);
        let shown: Vec<(String, i32)> = rows.iter().map(|r| (r.text.to_string(), r.time)).collect();
        assert_eq!(
            shown,
            [("One".to_string(), 1_000), ("Two".to_string(), 6_000), ("Three".to_string(), 9_000)]
        );
        assert_eq!(sung_row(&synced, 0), -1);
        assert_eq!(sung_row(&synced, 1_000), 0);
        assert_eq!(sung_row(&synced, 4_500), -1, "an instrumental break lights no line");
        assert_eq!(sung_row(&synced, 6_000), 1);
        assert_eq!(sung_row(&synced, 60_000), 2);
        // Untimed lyrics: every line, none of them seekable.
        let plain = Lyrics { plain: "A\n\n  B  \n".into(), synced: vec![] };
        let rows = lyric_rows(&plain);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.time == -1));
        assert_eq!(rows[1].text, "B");
    }

    #[test]
    fn kept_window_is_the_request_not_the_clamp() {
        let mut keep = HashMap::new();
        // A tracks-only home section has no cards: the card pass clamps to nothing...
        assert_eq!(keep_window(&mut keep, "home:1", 0, 5, 0), (0, 0));
        // ...but a refresh must replay the full request, or the track covers are dropped.
        assert_eq!(keep["home:1"], (0, 5));
        // A feed section: 2 cards then tracks; the window reaches into the tracks.
        assert_eq!(keep_window(&mut keep, "home:2", 0, 6, 2), (0, 2));
        assert_eq!(keep["home:2"], (0, 6));
        assert_eq!(keep_window(&mut keep, "grid:0", 8, 4, 10), (4, 4));
    }

    fn row(id: &str, title: &str) -> CardData {
        CardData {
            kind: "album".into(),
            id: id.into(),
            title: title.into(),
            image_url: format!("https://img/{id}").into(),
            ..Default::default()
        }
    }

    fn with_art(mut row: CardData) -> CardData {
        row.image = Image::from_rgba8(SharedPixelBuffer::new(1, 1));
        row
    }

    fn ids(model: &VecModel<CardData>) -> Vec<String> {
        model.iter().map(|r| r.id.to_string()).collect()
    }

    fn patch(model: &VecModel<CardData>, rows: Vec<CardData>) -> usize {
        patch_rows(model, rows, card_key, merge_card, |r| r.fresh = true)
    }

    #[test]
    fn patch_rows_inserts_only_what_is_new_and_keeps_artwork() {
        let model = VecModel::from(vec![with_art(row("a", "A")), with_art(row("b", "B"))]);
        let inserted = patch(&model, vec![row("n", "New"), row("a", "A"), row("b", "B")]);
        assert_eq!(inserted, 1);
        assert_eq!(ids(&model), ["n", "a", "b"]);
        let rows: Vec<CardData> = model.iter().collect();
        assert!(rows[0].fresh && rows[0].image.size().width == 0);
        assert!(
            rows[1..]
                .iter()
                .all(|r| !r.fresh && r.image.size().width == 1)
        );
    }

    #[test]
    fn patch_rows_removes_moves_and_updates_in_place() {
        let model = VecModel::from(vec![
            with_art(row("a", "A")),
            with_art(row("b", "B")),
            with_art(row("c", "C")),
        ]);
        let inserted = patch(&model, vec![row("c", "C"), row("a", "A renamed")]);
        assert_eq!(inserted, 0);
        assert_eq!(ids(&model), ["c", "a"]);
        let a = model.row_data(1).unwrap();
        assert_eq!(a.title, "A renamed");
        assert_eq!(
            a.image.size().width,
            1,
            "same art url keeps the loaded image"
        );
        assert!(model.iter().all(|r| !r.fresh));
    }

    #[test]
    fn patch_rows_drops_artwork_whose_url_changed() {
        let model = VecModel::from(vec![with_art(row("a", "A"))]);
        let mut moved = row("a", "A");
        moved.image_url = "https://img/other".into();
        patch(&model, vec![moved]);
        assert_eq!(model.row_data(0).unwrap().image.size().width, 0);
    }

    #[test]
    fn patch_groups_keeps_surviving_sections_and_adds_new_ones() {
        let cards = Rc::new(VecModel::from(vec![row("a", "A")]));
        let tracks = Rc::new(VecModel::<TrackData>::default());
        let outer = VecModel::from(vec![SectionData {
            title: "Old".into(),
            layout: "cards".into(),
            cards: ModelRc::from(cards.clone()),
            tracks: ModelRc::from(tracks.clone()),
        }]);
        let group = |title: &str, rows: Vec<CardData>| GroupRows {
            title: title.into(),
            layout: "cards".into(),
            cards: rows,
            tracks: Vec::new(),
        };
        let patched = patch_groups(
            &outer,
            vec![(group_key("cards", "Old"), (cards.clone(), tracks, 0))],
            vec![
                group("Brand new", vec![row("x", "X")]),
                group("Old", vec![row("a", "A"), row("b", "B")]),
            ],
        );
        assert_eq!(patched.kept, [(1, 0)]);
        assert_eq!(outer.row_count(), 2);
        assert_eq!(outer.row_data(0).unwrap().title, "Brand new");
        assert!(
            Rc::ptr_eq(&patched.models[1].0, &cards),
            "kept section reuses its model"
        );
        assert_eq!(ids(&cards), ["a", "b"]);
        assert!(cards.row_data(1).unwrap().fresh);
        assert!(patched.models[0].0.row_data(0).unwrap().fresh);
    }

    #[test]
    fn card_art_url_picks_a_size_the_cdn_serves() {
        let u = "https://resources.tidal.com/images/a/b/320x320.jpg";
        assert_eq!(card_art_url("album", u, 320), u);
        assert!(card_art_url("album", u, 640).ends_with("/640x640.jpg"));
        assert!(card_art_url("playlist", u, 640).ends_with("/640x640.jpg"));
        assert!(card_art_url("artist", u, 640).ends_with("/750x750.jpg"));
        assert_eq!(card_art_url("mix", u, 640), u);
        assert_eq!(grid_art_px(2), 320);
        assert_eq!(grid_art_px(3), 640);
        assert_eq!(grid_art_px(4), 640);
    }

    #[test]
    fn origin_cover_swaps_the_size_segment() {
        let base = "https://resources.tidal.com/images/4bbfd9de/8edd/400f/85a3/06d70061d82f";
        assert_eq!(
            origin_cover(&format!("{base}/640x640.jpg")).as_deref(),
            Some(format!("{base}/origin.jpg").as_str())
        );
        assert_eq!(origin_cover(&format!("{base}/origin.jpg")), None);
        assert_eq!(origin_cover("https://example.com/cover.png"), None);
    }

    #[test]
    fn player_navigation_preserves_the_real_playlist_after_browsing_and_restore() {
        let source = Page::Detail(Card {
            kind: "playlist".into(),
            id: "playlist-42".into(),
            title: "Road trip".into(),
            ..Default::default()
        });
        let saved = serde_json::to_string(&Some(source)).unwrap();
        let mut state = State {
            playback_source: serde_json::from_str(&saved).unwrap(),
            history: vec![Page::Albums],
            context: "Road trip".into(),
            ..Default::default()
        };
        assert!(matches!(state.playback_page(), Page::Detail(card) if card.id == "playlist-42"));
        state.playback_source = None;
        state.context = "My Tracks".into();
        assert!(matches!(state.playback_page(), Page::Tracks));
    }

    #[test]
    fn player_navigation_waits_for_the_track_and_rejects_a_superseded_page() {
        let mut state = State {
            page_gen: 7,
            reveal_track: Some((7, "target".into())),
            ..Default::default()
        };
        assert_eq!(state.take_reveal_track(), None);
        assert!(state.reveal_track.is_some());
        state.page_tracks = ["other", "target"]
            .map(|id| TrackItem {
                id: id.into(),
                ..Default::default()
            })
            .to_vec();
        assert_eq!(state.take_reveal_track(), Some(1));
        assert!(state.reveal_track.is_none());
        state.reveal_track = Some((7, "target".into()));
        state.page_gen = 8;
        assert_eq!(state.take_reveal_track(), None);
        assert!(state.reveal_track.is_none());
    }

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
    fn upcoming_tracks_follow_play_order_after_current_position() {
        let track = |id: &str| TrackItem {
            id: id.into(),
            ..Default::default()
        };
        let state = State {
            queue: ["A", "B", "C", "D"].map(track).to_vec(),
            order: vec![3, 1, 0, 2],
            pos: 1,
            shuffle: true,
            ..Default::default()
        };
        let upcoming: Vec<_> = state
            .upcoming_tracks()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(upcoming, ["A", "C"]);
    }

    #[test]
    fn last_quarter_of_the_track_preloads_the_next_song() {
        assert!(!in_prefetch_zone(0.0));
        assert!(!in_prefetch_zone(0.749));
        assert!(in_prefetch_zone(0.75));
        assert!(in_prefetch_zone(1.0));
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
        // Search results: "Relevance" leaves the server's order alone.
        assert_eq!(flat("Relevance"), ["Zebra", "apple", "Mango"]);
        for kind in ["search-albums", "search-playlists"] {
            let (sorts, groups) = grid_options(kind);
            assert_eq!((sorts[0], groups[0]), ("Relevance", "None"));
        }

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
    fn pinned_cards_move_to_a_group_on_top() {
        let album = |title: &str, by: &str| Card {
            kind: "album".into(),
            id: title.into(),
            title: title.into(),
            by: by.into(),
            ..Default::default()
        };
        let cards = vec![
            album("Zebra", "Lauv"),
            album("apple", "Weezer"),
            album("Mango", "Lauv"),
        ];
        let names = |pins: &[&str], filter: &str, group: &str| {
            let pins: HashSet<String> = pins.iter().map(|id| pin_key("album", id)).collect();
            arrange_pinned(&cards, &pins, filter, "A-Z", group, false)
                .into_iter()
                .map(|(g, members)| {
                    (
                        g,
                        members.iter().map(|c| c.title.clone()).collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        // No pins: the page as it always was.
        assert_eq!(
            names(&[], "", "None"),
            [(String::new(), vec!["apple".into(), "Mango".into(), "Zebra".into()])]
        );
        // Pinned cards lead, sorted like the rest, and leave their old place.
        assert_eq!(
            names(&["Zebra", "Mango"], "", "None"),
            [
                ("Pinned".into(), vec!["Mango".into(), "Zebra".into()]),
                (String::new(), vec!["apple".into()]),
            ]
        );
        // Grouping arranges only what is not pinned; an emptied group goes.
        assert_eq!(
            names(&["Zebra", "Mango"], "", "Artist"),
            [
                ("Pinned".into(), vec!["Mango".into(), "Zebra".into()]),
                ("Weezer".into(), vec!["apple".into()]),
            ]
        );
        // The filter applies to the pins as well.
        assert_eq!(
            names(&["Zebra"], "apple", "None"),
            [(String::new(), vec!["apple".into()])]
        );
        // A pin is per kind: a playlist with the same id stays put.
        let pins: HashSet<String> = [pin_key("playlist", "Zebra")].into();
        assert_eq!(arrange_pinned(&cards, &pins, "", "A-Z", "None", false).len(), 1);
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
    fn now_playing_swipe_toggles_between_lyrics_and_queue() {
        // Viewing Lyrics or Centred: swipe left tracks towards Queue; swipe right stays put.
        assert_eq!(np_swipe_target(false, false), Some(true));
        assert_eq!(np_swipe_target(false, true), None);
        // Viewing Queue: swipe right tracks towards Lyrics (or closes Queue without lyrics); swipe left stays put.
        assert_eq!(np_swipe_target(true, true), Some(false));
        assert_eq!(np_swipe_target(true, false), None);

        // Rubber-band keeps 0..=1 linear and damps overscroll softly.
        assert_eq!(np_swipe_rubber_band(0.4), 0.4);
        assert!(np_swipe_rubber_band(-0.2) < 0.0 && np_swipe_rubber_band(-0.2) > -0.14);
        assert!(np_swipe_rubber_band(1.2) > 1.0 && np_swipe_rubber_band(1.2) < 1.14);

        // Settle target: past 35% or a crisp flick commits; pull-back or cancel reverts.
        assert!(np_swipe_settle_target(false, 0.4, 0.0, false));
        assert!(np_swipe_settle_target(false, 0.1, 1.2, false));
        assert!(!np_swipe_settle_target(false, 0.2, 0.2, false));
        assert!(!np_swipe_settle_target(false, 0.5, -1.0, false));
        assert!(!np_swipe_settle_target(false, 0.8, 1.0, true));

        assert!(!np_swipe_settle_target(true, 0.6, 0.0, false));
        assert!(!np_swipe_settle_target(true, 0.9, -1.2, false));
        assert!(np_swipe_settle_target(true, 0.8, -0.2, false));
        assert!(np_swipe_settle_target(true, 0.5, 1.0, false));
        assert!(np_swipe_settle_target(true, 0.2, -1.0, true));
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
    fn zoom_preferences_default_to_actual_size_and_survive_restart() {
        let meta = MetadataCache::in_memory().unwrap();
        assert_eq!(Settings::load(&meta).zoom_percent, 100);
        meta.put_json("settings", r#"{"quality":"high"}"#).unwrap();
        assert_eq!(Settings::load(&meta).zoom_percent, 100);
        for percent in crate::zoom::LEVELS {
            let mut settings = Settings::load(&meta);
            settings.zoom_percent = percent;
            meta.put_json("settings", &serde_json::to_string(&settings).unwrap())
                .unwrap();
            assert_eq!(Settings::load(&meta).zoom_percent, percent);
        }
        for (stored, expected) in [(-100, 50), (99, 100), (250, 200)] {
            meta.put_json("settings", &format!(r#"{{"zoom_percent":{stored}}}"#))
                .unwrap();
            assert_eq!(Settings::load(&meta).zoom_percent, expected);
        }
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
    fn fancy_animation_is_off_unless_the_user_turned_it_on() {
        let meta = MetadataCache::in_memory().unwrap();
        assert!(!Settings::load(&meta).fancy_animation);

        meta.put_json("settings", r#"{"quality":"high"}"#).unwrap();
        assert!(!Settings::load(&meta).fancy_animation);

        // The old Low RAM switch is gone; the lean profile is the default.
        meta.put_json("settings", r#"{"low_ram":false}"#).unwrap();
        assert!(!Settings::load(&meta).fancy_animation);

        meta.put_json("settings", r#"{"fancy_animation":true}"#)
            .unwrap();
        assert!(Settings::load(&meta).fancy_animation);
    }

    #[test]
    fn glass_follows_the_system_until_the_user_chooses() {
        let meta = MetadataCache::in_memory().unwrap();
        assert_eq!(Settings::load(&meta).glass, None);

        // 1.1.19 saved `liquid_glass: true` with every settings change; that
        // was never a choice.
        meta.put_json("settings", r#"{"liquid_glass":true}"#).unwrap();
        assert_eq!(Settings::load(&meta).glass, None);

        meta.put_json("settings", r#"{"glass":true}"#).unwrap();
        assert_eq!(Settings::load(&meta).glass, Some(true));
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
    /// Artwork downloads in flight.
    img_sem: tokio::sync::Semaphore,
    /// Artwork decodes in flight (CPU-bound, so bounded by the cores).
    decode_sem: tokio::sync::Semaphore,
    downloads: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Scrubber target for the in-flight start, so a click during buffering is honoured.
    desired_elapsed_ms: AtomicU64,
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
        player.set_crossfade(settings.crossfade_secs());
        player.set_normalization(settings.normalization());
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
            decode_sem: tokio::sync::Semaphore::new(
                std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8)),
            ),
            downloads: Mutex::new(HashMap::new()),
            desired_elapsed_ms: AtomicU64::new(0),
        }))
    }

    pub fn api(&self) -> Option<ApiHandle> {
        self.api.read().unwrap().clone()
    }

    fn audio_quality_key(&self) -> String {
        self.settings.read().unwrap().quality.to_uppercase()
    }

    /// TIDAL's loudness measurements for a track, as remembered from its stream lookup.
    fn known_loudness(&self, track_id: &str) -> Option<Option<Loudness>> {
        let json = self.meta.get_json(&format!("loudness:{track_id}"))?;
        serde_json::from_str(&json).ok()
    }

    fn remember_loudness(&self, track_id: &str, gain: Option<[f32; 4]>) -> Option<Loudness> {
        let loudness = gain.map(|[track_gain_db, track_peak, album_gain_db, album_peak]| {
            Loudness {
                track_gain_db,
                track_peak,
                album_gain_db,
                album_peak,
            }
        });
        let json = serde_json::to_string(&loudness).unwrap_or_default();
        let _ = self.meta.put_json(&format!("loudness:{track_id}"), &json);
        loudness
    }

    /// Loudness for a track about to play. A cached track was never looked up:
    /// with normalisation on, ask TIDAL once (it is remembered afterwards).
    async fn loudness(&self, track_id: &str) -> Option<Loudness> {
        if let Some(known) = self.known_loudness(track_id) {
            return known;
        }
        if self.player.normalization() == Normalization::Off {
            return None;
        }
        let api = self.api()?;
        let quality = self.settings.read().unwrap().quality.clone();
        let lookup = tokio::time::timeout(Duration::from_secs(2), api.stream(track_id, &quality));
        let stream = lookup.await.ok()?.ok()?;
        self.remember_loudness(track_id, stream.replay_gain)
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
        self.remember_loudness(track_id, stream.replay_gain);
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    /// Phone layout's Collection tab: My Collection's pages and the playlists,
    /// what the sidebar holds on a wider window.
    Collection,
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
    /// The section list on screen, patched in place when a newer copy arrives.
    sections_model: Option<Rc<VecModel<SectionData>>>,
    /// Page and cache key the sections on screen were drawn for.
    sections_source: Option<(u64, String)>,
    /// Background refreshes running for the page on screen.
    sync: SyncState,

    /// Tracks behind the table on screen (after filtering) and its model.
    page_tracks: Vec<TrackItem>,
    page_context: String,
    tracks_model: Option<Rc<VecModel<TrackData>>>,
    /// Upcoming queue rows shown inside Now Playing.
    np_queue_model: Option<Rc<VecModel<TrackData>>>,
    sidebar_cards: Vec<Card>,
    /// When TIDE last added to each playlist (id → Unix seconds), kept so
    /// "Add to playlist" can list the ones in use first.
    playlist_adds: HashMap<String, i64>,
    /// The card grid on screen, as loaded; filter, sort and grouping are applied on top.
    grid: Grid,
    /// Search result lists the Albums and Playlists tabs sort and group.
    search_lists: HashMap<Shelf, SearchList>,
    /// Album whose credits are loaded or loading.
    credits_for: String,
    /// Page and artist whose "more albums" and related artists were requested.
    extras_for: (u64, String),

    menu_track: Option<TrackItem>,
    /// The album, playlist, mix or artist the context menu was opened on.
    menu_card: Option<Card>,
    /// The playlist open in the Edit dialog, as it was when the dialog opened.
    editing: Option<PlaylistDetail>,
    queue: Vec<TrackItem>,
    order: Vec<usize>,
    pos: usize,
    context: String,
    /// The actual source destination, independent of subsequent browsing.
    playback_source: Option<Page>,
    /// A page load may finish after its source link was clicked.
    reveal_track: Option<(u64, String)>,
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
    /// Last playable DASH head the player was opened on, so a later seek can reopen.
    playable_gen: u64,
    volume_before_mute: f32,
    muted: bool,

    lyrics: Lyrics,
    lyrics_for: String,
    /// The row of `App.lyrics` last shown as being sung (-1: none), or
    /// `None` before the first update for these lyrics.
    lyric_row: Option<i32>,

    images: HashMap<String, (Image, Color)>,
    image_waiters: ImageWaiters,
    /// Visible artwork window [from, upto) per model key, so a late decode is dropped.
    art_keep: HashMap<String, (usize, usize)>,
    /// Artist whose hover card (bio + picture) is loaded or loading.
    hover_artist: String,
}

impl State {
    fn playback_page(&self) -> Page {
        self.playback_source.clone().unwrap_or_else(|| {
            if self.context == "My Tracks" || self.context == "Tracks" {
                Page::Tracks
            } else {
                Page::Queue
            }
        })
    }

    fn take_reveal_track(&mut self) -> Option<usize> {
        let (generation, id) = self.reveal_track.as_ref()?;
        if *generation != self.page_gen {
            self.reveal_track = None;
            return None;
        }
        let index = self.page_tracks.iter().position(|track| track.id == *id)?;
        self.reveal_track = None;
        Some(index)
    }

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

    fn upcoming_tracks(&self) -> Vec<TrackItem> {
        self.order
            .iter()
            .skip(self.pos.saturating_add(1))
            .filter_map(|&i| self.queue.get(i).cloned())
            .collect()
    }
}

pub struct Ui {
    win: MainWindow,
    ctx: Arc<Ctx>,
    state: RefCell<State>,
    nav: RefCell<Nav>,
    np_swipe: RefCell<NpSwipe>,
    clipboard: RefCell<Option<arboard::Clipboard>>,
    video: RefCell<video::VideoState>,
    art: RefCell<artwork::ArtState>,
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

#[path = "artwork.rs"]
mod artwork;

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

/// Phase of a live two-finger horizontal trackpad swipe inside Now Playing.
#[derive(Clone, Copy, Default)]
enum NpSwipePhase {
    #[default]
    Idle,
    /// Fingers are down on the trackpad; columns and pill thumb track 1:1.
    Tracking,
    /// Fingers lifted; easing smoothly to Lyrics (0) or Queue (1).
    Settling {
        from: f32,
        to: f32,
        start: Instant,
        secs: f32,
        commit_queue: bool,
    },
}

/// Live horizontal two-finger swipe state inside Now Playing (Lyrics 0 ↔ Queue 1).
#[derive(Default)]
struct NpSwipe {
    phase: NpSwipePhase,
    /// Un-clamped progress in 0..=1 (0 = Lyrics or Centred, 1 = Queue).
    raw: f32,
    /// Rubber-banded progress currently shown on screen.
    visual: f32,
    /// Whether the swipe began on Queue (`true`) or Lyrics / Centred (`false`).
    start_queue: bool,
    /// True when `0.0` is centred artwork (neither panel open or track has no lyrics).
    from_centred: bool,
    /// True when the current track has lyrics available.
    has_lyrics: bool,
    samples: VecDeque<(Instant, f32)>,
    timer: slint::Timer,
}

/// Target panel for a horizontal two-finger swipe in Now Playing.
/// `right` is a swipe to the right (`dx > 0`, towards the left Lyrics segment);
/// `!right` is a swipe to the left (`dx < 0`, towards the right Queue segment).
/// Returns `Some(true)` to swipe towards Queue, `Some(false)` to swipe towards
/// Lyrics (or close Queue when the track has no lyrics), or `None` when already
/// at that boundary.
fn np_swipe_target(show_queue: bool, right: bool) -> Option<bool> {
    if right {
        show_queue.then_some(false)
    } else {
        (!show_queue).then_some(true)
    }
}

/// Apple-style logarithmic rubber-band damping past the `0..=1` swipe bounds.
fn np_swipe_rubber_band(raw: f32) -> f32 {
    if raw < 0.0 {
        let d = -raw;
        -(d / (1.0 + d / 0.14))
    } else if raw > 1.0 {
        let d = raw - 1.0;
        1.0 + d / (1.0 + d / 0.14)
    } else {
        raw
    }
}

/// Whether a released Now Playing horizontal swipe lands on Queue (`true`) or
/// Lyrics / Centred (`false`). `raw` is the un-clamped progress (`0..=1`) and
/// `velocity` is progress/sec (positive towards Queue, negative towards Lyrics).
fn np_swipe_settle_target(start_queue: bool, raw: f32, velocity: f32, cancelled: bool) -> bool {
    if cancelled {
        return start_queue;
    }
    if !start_queue {
        (raw >= 0.35 && velocity > -0.6) || (raw >= 0.06 && velocity > 0.85)
    } else {
        !((raw <= 0.65 && velocity < 0.6) || (raw <= 0.94 && velocity < -0.85))
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
    /// Drives the page up to its top when Esc is pressed on home.
    static SCROLL_TO_TOP: slint::Timer = slint::Timer::default();
}

/// Glide the page to its top, quick at first and settling at the end. Any
/// scroll of the user's own (or a page change) takes over and stops it.
fn scroll_to_top(app: &App<'_>) {
    let from = app.get_scroll_y();
    SCROLL_TO_TOP.with(|t| t.stop());
    if from >= 0.0 || crate::platform::reduce_motion() {
        app.set_scroll_y(0.0);
        return;
    }
    // Longer pages take a little longer, but never feel slow.
    let duration = Duration::from_millis((250.0 + -from as f64 * 0.05).min(550.0) as u64);
    let started = Instant::now();
    let view = app.get_view();
    let mut last = from;
    SCROLL_TO_TOP.with(|t| {
        t.start(
            slint::TimerMode::Repeated,
            crate::platform::frame_interval(),
            move || {
                let done = with_ui(|ui| {
                    let app = ui.app();
                    if app.get_view() != view || (app.get_scroll_y() - last).abs() > 0.5 {
                        return true;
                    }
                    let t = (started.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.0);
                    let eased = 1.0 - (1.0 - t).powi(3);
                    last = from * (1.0 - eased);
                    app.set_scroll_y(last);
                    t >= 1.0
                })
                .unwrap_or(true);
                if done {
                    SCROLL_TO_TOP.with(|t| t.stop());
                }
            },
        );
    });
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
        app.set_loaded(loaded_fraction(&blocks, active));
        app.set_debug_blocks(ModelRc::new(VecModel::from(blocks)));
    });
}

/// Share of the track playable from the start: the unbroken run of received
/// chunks. No chunks and not downloading means the whole file is local.
fn loaded_fraction(blocks: &[i32], active: bool) -> f32 {
    if blocks.is_empty() {
        return if active { 0.0 } else { 1.0 };
    }
    let run = blocks.iter().take_while(|b| **b == 2).count();
    run as f32 / blocks.len() as f32
}

/// Space toggles playback unless a text field, a menu or the create dialog
/// takes the key. Answers whether it was a playback shortcut, so the caller
/// can keep Slint from seeing it.
pub fn space_toggles_play(pressed: bool, repeat: bool) -> bool {
    let Some(ui) = UI.with(|u| u.borrow().clone()) else {
        return false;
    };
    let app = ui.app();
    if !app.get_logged_in()
        || app.get_menu_open()
        || !app.get_create_kind().is_empty()
        || ui.win.get_typing()
    {
        return false;
    }
    if pressed && !repeat {
        ui.toggle_play();
    }
    true
}

/// ⌘ (Ctrl off macOS) is down: the sidebar shows its page keys.
pub fn set_command_held(held: bool) {
    if let Some(ui) = UI.with(|u| u.borrow().clone()) {
        let app = ui.app();
        if app.get_command_held() != held {
            app.set_command_held(held);
        }
    }
}

/// A sidebar page key: e Explore, f Feed, m Mixes, p Playlists,
/// a Albums, t Tracks, v Videos, o Profiles. "/" focuses the filter of the
/// collection page on screen, or opens Tracks and focuses its filter. Returns
/// whether the key was taken. Not while typing, in a menu, a dialog or the
/// video player.
pub fn nav_shortcut(text: &str, pressed: bool, repeat: bool) -> bool {
    let key = match text {
        "e" => "explore",
        "f" => "feed",
        "m" => "mixes",
        "p" => "playlists",
        "a" => "albums",
        "t" => "tracks",
        "v" => "videos",
        "o" => "profiles",
        "/" => "/",
        _ => return false,
    };
    let Some(ui) = UI.with(|u| u.borrow().clone()) else {
        return false;
    };
    let app = ui.app();
    if !app.get_logged_in()
        || app.get_menu_open()
        || !app.get_create_kind().is_empty()
        || app.get_video_open()
        || ui.win.get_typing()
    {
        return false;
    }
    if !pressed || repeat {
        return true;
    }
    ui.page_key(key);
    true
}

/// Note a primary click in window coordinates, so a focused text field it
/// missed lets go of the keyboard.
pub fn note_click(x: f32, y: f32) {
    if let Some(ui) = UI.with(|u| u.borrow().clone())
        && ui.win.get_typing()
    {
        let app = ui.app();
        app.set_click_x(x);
        app.set_click_y(y);
        app.set_click_seq(app.get_click_seq().wrapping_add(1));
    }
}

/// Ask for one frame of the main window (nothing while it is closed).
pub fn request_redraw() {
    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref() {
            ui.win.window().request_redraw();
        }
    });
}

/// The cover as Now Playing's glass backdrop: its colours and nothing else.
/// 40 px and blurred, so stretched over the window it is a soft wash that
/// costs 6 KB rather than a window-sized blur.
fn backdrop_of(cover: &Image) -> Image {
    const SIDE: u32 = 40;
    let Some(pixels) = cover.to_rgba8() else {
        return Image::default();
    };
    let Some(source) =
        image::RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
    else {
        return Image::default();
    };
    let small = image::imageops::resize(&source, SIDE, SIDE, image::imageops::FilterType::Triangle);
    let soft = image::imageops::blur(&small, 3.);
    Image::from_rgba8(SharedPixelBuffer::clone_from_slice(soft.as_raw(), SIDE, SIDE))
}

/// `TIDE_FANCY_ANIMATION=1` (or 0) overrides the Fancy Animation setting for
/// this run. Kept out of `Settings`, which is saved whole (window placement
/// on exit).
pub fn fancy_animation_override() -> Option<bool> {
    static OVERRIDE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| std::env::var("TIDE_FANCY_ANIMATION").ok().map(|v| v != "0"))
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
    /// Cards pinned to the top of this page (`pin_key`); empty where pins don't apply.
    pins: HashSet<String>,
    /// Per-group card models on screen, so artwork can be filled and dropped by range.
    models: Vec<Rc<VecModel<CardData>>>,
    /// The group list on screen, patched in place when a newer copy arrives.
    groups_model: Option<Rc<VecModel<SectionData>>>,
}

/// One search tab's results in relevance order, and how they are arranged on screen.
#[derive(Default)]
struct SearchList {
    all: Vec<Card>,
    sort: String,
    group: String,
    /// Per-group card models on screen, for artwork windows and favourite patches.
    models: Vec<Rc<VecModel<CardData>>>,
}

/// Refreshes of the page on screen still waiting on the server.
#[derive(Default)]
struct SyncState {
    page_gen: u64,
    running: u32,
    since: Option<Instant>,
}

/// The toolbar wave fills for at least this long, so a quick check reads as one.
const SYNC_MIN_SHOWN: Duration = Duration::from_millis(1200);

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
        // Search results keep the server's order unless sorted. Same lists as views.slint.
        "search-albums" => (
            &["Relevance", "A-Z", "Artist", "Release date"],
            &["None", "Artist", "Year", "Title"],
        ),
        "search-playlists" => (&["Relevance", "A-Z", "Creator"], &["None", "Creator", "Title"]),
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

/// Library pages whose cards can be pinned to the top.
fn pinnable_grid(kind: &str) -> bool {
    matches!(kind, "mixes" | "playlists" | "albums" | "videos" | "profiles")
}

/// Where the pins live in the meta store. Not account data: kept through logout.
const PINS_KEY: &str = "pins";

/// How a card is named among the pins: kinds share ids, so the kind goes with it.
fn pin_key(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

/// `arrange_grid`, with the pinned cards lifted out into a "Pinned" group on top.
/// Each card shows once: pinned ones leave the groups they would sit in.
fn arrange_pinned(
    cards: &[Card],
    pins: &HashSet<String>,
    filter: &str,
    sort: &str,
    group: &str,
    combine_others: bool,
) -> Vec<(String, Vec<Card>)> {
    let (pinned, rest): (Vec<Card>, Vec<Card>) = cards
        .iter()
        .cloned()
        .partition(|c| pins.contains(&pin_key(&c.kind, &c.id)));
    let rest = arrange_grid(&rest, filter, sort, group, combine_others);
    let pinned: Vec<Card> = arrange_grid(&pinned, filter, sort, "None", false)
        .into_iter()
        .flat_map(|(_, members)| members)
        .collect();
    if pinned.is_empty() {
        return rest;
    }
    std::iter::once(("Pinned".to_string(), pinned))
        .chain(rest.into_iter().filter(|(_, members)| !members.is_empty()))
        .collect()
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

/// The rows of the lyrics column: every line with words in it, with its start
/// in milliseconds when the lyrics are timed (-1 otherwise).
fn lyric_rows(lyrics: &Lyrics) -> Vec<LyricLine> {
    if lyrics.synced.is_empty() {
        lyrics
            .plain
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| LyricLine { text: l.into(), time: -1 })
            .collect()
    } else {
        lyrics
            .synced
            .iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .map(|(t, text)| LyricLine {
                text: text.trim().into(),
                time: (*t).min(i32::MAX as u64) as i32,
            })
            .collect()
    }
}

/// The row of [`lyric_rows`] being sung `ms` into the track: -1 before the
/// first line and during a blank (instrumental) line.
fn sung_row(synced: &[(u64, String)], ms: u64) -> i32 {
    let Some(idx) = synced.iter().rposition(|(t, _)| *t <= ms) else {
        return -1;
    };
    if synced[idx].1.trim().is_empty() {
        return -1;
    }
    synced[..idx].iter().filter(|(_, text)| !text.trim().is_empty()).count() as i32
}

/// A long running time the way the official client writes it: `20:10:00`, or
/// `42:07` under an hour.
fn fmt_clock(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        fmt_time(secs)
    }
}

/// 11 500 -> "11.5K": a follower count short enough for one line.
fn fmt_count(n: u64) -> String {
    let short = |value: f64, unit: &str| {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_949 => short(n as f64 / 1e3, "K"),
        _ => short(n as f64 / 1e6, "M"),
    }
}

/// The small-caps line of a playlist page: `500 TRACKS  (20:10:00)  •  11.5K FANS`.
fn playlist_summary(detail: &PlaylistDetail) -> String {
    let mut line = tide_api::web::count_label(detail.track_count, detail.video_count);
    if detail.duration > 0 {
        line.push_str(&format!("  ({})", fmt_clock(detail.duration)));
    }
    if detail.followers > 0 {
        let fans = if detail.followers == 1 { "FAN" } else { "FANS" };
        line.push_str(&format!("  •  {} {fans}", fmt_count(detail.followers)));
    }
    line
}

fn in_prefetch_zone(progress: f32) -> bool {
    progress >= PREFETCH_ZONE
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
        fresh: false,
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
        is_public: c.is_public,
        favorite: false,
        tint: tile_tint(&c.id, &c.title),
        detail: video_length(c.duration).into(),
        fresh: false,
        explicit: false,
        video_cover: c.video_cover.as_deref().unwrap_or_default().into(),
    }
}

/// Bring `model` in line with `rows` through single-row edits, so the view keeps
/// the instances (and loaded artwork) of every row that is still there. A kept
/// row goes through `merge(old, new)`, which returns the row to store or `None`
/// when nothing changed; a row that was not there before goes through `mark`.
/// Returns how many rows were inserted.
fn patch_rows<T: Clone + 'static>(
    model: &VecModel<T>,
    rows: Vec<T>,
    key: impl Fn(&T) -> String,
    merge: impl Fn(&T, T) -> Option<T>,
    mark: impl Fn(&mut T),
) -> usize {
    let wanted: HashSet<String> = rows.iter().map(&key).collect();
    for i in (0..model.row_count()).rev() {
        if model
            .row_data(i)
            .is_some_and(|row| !wanted.contains(&key(&row)))
        {
            model.remove(i);
        }
    }
    let mut inserted = 0;
    let len = rows.len();
    for (i, row) in rows.into_iter().enumerate() {
        let k = key(&row);
        let found = (i..model.row_count()).find_map(|j| {
            model
                .row_data(j)
                .filter(|old| key(old) == k)
                .map(|old| (j, old))
        });
        match found {
            Some((j, old)) if j == i => {
                if let Some(row) = merge(&old, row) {
                    model.set_row_data(i, row);
                }
            }
            Some((j, old)) => {
                model.remove(j);
                model.insert(i, merge(&old, row).unwrap_or(old));
            }
            None => {
                let mut row = row;
                mark(&mut row);
                model.insert(i, row);
                inserted += 1;
            }
        }
    }
    while model.row_count() > len {
        model.remove(model.row_count() - 1);
    }
    inserted
}

fn card_key(row: &CardData) -> String {
    format!("{}:{}", row.kind, row.id)
}

fn track_key(row: &TrackData) -> String {
    row.id.to_string()
}

/// Keep the artwork already on screen unless the card now points at other art.
fn merge_card(old: &CardData, mut row: CardData) -> Option<CardData> {
    if row.image_url == old.image_url {
        row.image = old.image.clone();
    }
    row.fresh = old.fresh;
    (row != *old).then_some(row)
}

fn merge_track(old: &TrackData, mut row: TrackData) -> Option<TrackData> {
    if row.cover_url == old.cover_url {
        row.cover = old.cover.clone();
    }
    row.fresh = old.fresh;
    (row != *old).then_some(row)
}

/// Rows that popped in lose the flag once the animation is over, so a view
/// that rebuilds them later does not play it again.
fn settle_fresh(cards: Vec<Rc<VecModel<CardData>>>, tracks: Vec<Rc<VecModel<TrackData>>>) {
    if cards.is_empty() && tracks.is_empty() {
        return;
    }
    slint::Timer::single_shot(Duration::from_millis(1200), move || {
        for model in &cards {
            for i in 0..model.row_count() {
                if let Some(mut row) = model.row_data(i).filter(|row| row.fresh) {
                    row.fresh = false;
                    model.set_row_data(i, row);
                }
            }
        }
        for model in &tracks {
            for i in 0..model.row_count() {
                if let Some(mut row) = model.row_data(i).filter(|row| row.fresh) {
                    row.fresh = false;
                    model.set_row_data(i, row);
                }
            }
        }
    });
}

/// Remember `[from, upto)` as the window the view asked to keep for `key`, and
/// return it clamped to the `n` rows there are now. The request is stored as
/// made: a refresh replays it, the rows may have grown since, and a feed
/// section's window runs past its cards into its tracks. Clamped to the card
/// count, it would replay as an empty track window and drop every track cover.
fn keep_window(
    keep: &mut HashMap<String, (usize, usize)>,
    key: &str,
    from: usize,
    upto: usize,
    n: usize,
) -> (usize, usize) {
    keep.insert(key.to_string(), (from, upto));
    let upto = upto.min(n);
    (from.min(upto), upto)
}

/// How a section is laid out on screen.
fn section_layout(s: &Section) -> &str {
    if s.layout == "cards" && s.title.eq_ignore_ascii_case("Recently played") {
        "rows"
    } else {
        &s.layout
    }
}

/// A section or grid group as it should now look: its header and its rows.
struct GroupRows {
    title: String,
    layout: String,
    cards: Vec<CardData>,
    tracks: Vec<TrackData>,
}

/// The outcome of patching a list of sections in place.
struct PatchedGroups {
    models: Vec<SectionModel>,
    /// (new index, old index) of every section whose models were kept.
    kept: Vec<(usize, usize)>,
    fresh_cards: Vec<Rc<VecModel<CardData>>>,
    fresh_tracks: Vec<Rc<VecModel<TrackData>>>,
}

/// Sections are matched across refreshes by layout and title.
fn group_key(layout: &str, title: &str) -> String {
    format!("{layout}\u{1f}{title}")
}

/// Patch the sections on screen towards `groups`. A section whose layout and
/// title survive keeps its models, and only its changed rows are touched; a new
/// section grows in. `old` is keyed with `group_key`.
fn patch_groups(
    outer: &VecModel<SectionData>,
    old: Vec<(String, SectionModel)>,
    groups: Vec<GroupRows>,
) -> PatchedGroups {
    let mut pool: Vec<Option<(String, SectionModel)>> = old.into_iter().map(Some).collect();
    let mut out = PatchedGroups {
        models: Vec::with_capacity(groups.len()),
        kept: Vec::new(),
        fresh_cards: Vec::new(),
        fresh_tracks: Vec::new(),
    };
    let mut rows = Vec::with_capacity(groups.len());
    for (i, group) in groups.into_iter().enumerate() {
        let key = group_key(&group.layout, &group.title);
        let reused = pool
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|(old, _)| *old == key))
            .and_then(|at| pool[at].take().map(|(_, models)| (at, models)));
        let models = match reused {
            Some((at, (cards, tracks, n))) => {
                if patch_rows(&cards, group.cards, card_key, merge_card, |row| {
                    row.fresh = true
                }) > 0
                {
                    out.fresh_cards.push(cards.clone());
                }
                if patch_rows(&tracks, group.tracks, track_key, merge_track, |row| {
                    row.fresh = true
                }) > 0
                {
                    out.fresh_tracks.push(tracks.clone());
                }
                out.kept.push((i, at));
                (cards, tracks, n)
            }
            None => {
                let cards: Vec<CardData> = group
                    .cards
                    .into_iter()
                    .map(|row| CardData { fresh: true, ..row })
                    .collect();
                let tracks: Vec<TrackData> = group
                    .tracks
                    .into_iter()
                    .map(|row| TrackData { fresh: true, ..row })
                    .collect();
                let models = (
                    Rc::new(VecModel::from(cards)),
                    Rc::new(VecModel::from(tracks)),
                    0,
                );
                out.fresh_cards.push(models.0.clone());
                out.fresh_tracks.push(models.1.clone());
                models
            }
        };
        rows.push(SectionData {
            title: group.title.into(),
            layout: group.layout.into(),
            cards: ModelRc::from(models.0.clone()),
            tracks: ModelRc::from(models.1.clone()),
        });
        out.models.push(models);
    }
    patch_rows(
        outer,
        rows,
        |row| group_key(&row.layout, &row.title),
        |old, row| (row != *old).then_some(row),
        |_| {},
    );
    out
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
        is_public: card.is_public,
        video_cover: (!card.video_cover.is_empty()).then(|| card.video_cover.to_string()),
        ..Default::default()
    }
}

/// How many playlists "Add to playlist" lists first, most recently changed on top.
const RECENT_PLAYLISTS: usize = 5;

/// The user's own playlists in "Add to playlist" order: the few that last had
/// something added (here, or anywhere according to TIDAL) first, newest on
/// top, then the rest as the sidebar lists them. Also returns how many rows
/// are the recent ones; none when everything fits in that many rows anyway.
fn add_targets(cards: &[Card], adds: &HashMap<String, i64>) -> (Vec<Card>, usize) {
    let own: Vec<&Card> = cards
        .iter()
        .filter(|c| c.kind == "playlist" && c.by == "Me")
        .collect();
    if own.len() <= RECENT_PLAYLISTS {
        return (own.into_iter().cloned().collect(), 0);
    }
    let changed = |card: &Card| {
        tide_api::web::iso_seconds(&card.updated).max(adds.get(&card.id).copied().unwrap_or(0))
    };
    let mut recent: Vec<usize> = (0..own.len()).filter(|&i| changed(own[i]) > 0).collect();
    recent.sort_by_key(|&i| std::cmp::Reverse(changed(own[i])));
    recent.truncate(RECENT_PLAYLISTS);
    let rest = (0..own.len()).filter(|i| !recent.contains(i));
    let count = recent.len();
    let order: Vec<Card> = recent
        .iter()
        .copied()
        .chain(rest)
        .map(|i| own[i].clone())
        .collect();
    (order, count)
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

/// Collection grid artwork px: 640 for the two largest slider steps (tiles of
/// 236pt and up are 470px+ on a 2x display), 320 below that.
fn grid_art_px(grid_size: i32) -> u32 {
    if grid_size >= 3 { 640 } else { 320 }
}

/// Cards carry 320px URLs; above that, ask for a size the CDN has for the kind.
/// Albums and playlists have 640x640; artist pictures skip 640 (403) and have 750x750.
/// Other kinds (mixes, videos, pages) keep their URL.
/// Playlists the search page's All tab previews before the Playlists tab.
const SEARCH_PLAYLIST_PREVIEW: usize = 6;

/// A card list fetched for one page and shown as a grid (videos, playlists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Shelf {
    ArtistVideos,
    SearchVideos,
    SearchAlbums,
    SearchPlaylists,
}

impl Shelf {
    const ALL: [Shelf; 4] = [
        Shelf::ArtistVideos,
        Shelf::SearchVideos,
        Shelf::SearchAlbums,
        Shelf::SearchPlaylists,
    ];

    fn key(self) -> &'static str {
        match self {
            Shelf::ArtistVideos => "artist-videos",
            Shelf::SearchVideos => "search-videos",
            Shelf::SearchAlbums => "search-albums",
            Shelf::SearchPlaylists => "search-playlists",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Shelf::ALL.into_iter().find(|shelf| shelf.key() == key)
    }

    /// Search tabs whose results can be sorted and grouped.
    fn arranged(self) -> bool {
        matches!(self, Shelf::SearchAlbums | Shelf::SearchPlaylists)
    }

    /// Heading over the results when they are not grouped.
    fn label(self) -> &'static str {
        match self {
            Shelf::ArtistVideos | Shelf::SearchVideos => "Videos",
            Shelf::SearchAlbums => "Albums",
            Shelf::SearchPlaylists => "Playlists",
        }
    }
}

/// Where a grid's Enter pick starts: the first card that plays (folders lead
/// playlist grids, and Enter only opens them).
fn first_playable(rows: &[CardData]) -> usize {
    rows.iter()
        .position(|row| !matches!(row.kind.as_str(), "folder" | "page"))
        .unwrap_or(0)
}

/// ↑ / ↓ while a search or filter field has focus: move the row or card Enter
/// plays (`delta` -1 or +1). Returns whether the key was taken (the field never
/// sees it).
pub fn move_pick(delta: i32, pressed: bool) -> bool {
    let Some(ui) = UI.with(|u| u.borrow().clone()) else {
        return false;
    };
    let app = ui.app();
    let view = app.get_view();
    let picking = match view.as_str() {
        "search" => app.get_search_focused() && app.get_search_text() == app.get_search_shown(),
        "tracks" | "grid" => app.get_filter_focused(),
        _ => false,
    };
    if !picking || app.get_menu_open() {
        return false;
    }
    let count = if view == "grid" {
        app.get_grid_cards().row_count()
    } else {
        app.get_tracks().row_count()
    };
    if count == 0 {
        return true;
    }
    if pressed {
        let at = (app.get_pick_index() + delta).clamp(0, count as i32 - 1);
        app.set_pick_index(at);
        if view == "grid"
            && let Some(row) = app.get_grid_cards().row_data(at as usize)
        {
            app.set_grid_pick_id(row.id);
        }
    }
    true
}

fn card_art_url(kind: &str, url: &str, max_px: u32) -> String {
    if max_px <= 320 || !url.ends_with("/320x320.jpg") {
        return url.to_string();
    }
    let size = match kind {
        "album" | "playlist" => "640x640",
        "artist" => "750x750",
        _ => return url.to_string(),
    };
    url.replace("/320x320.jpg", &format!("/{size}.jpg"))
}

/// Decode cap for the Now Playing original: about the largest the view gets on a 5K display.
const LARGE_COVER_PX: u32 = 2048;

/// `…/{W}x{H}.jpg` → `…/origin.jpg`, the uploaded master (no fixed size above 1280 exists).
fn origin_cover(url: &str) -> Option<String> {
    let (base, file) = url.rsplit_once('/')?;
    let (w, h) = file.strip_suffix(".jpg")?.split_once('x')?;
    (w.parse::<u32>().is_ok() && h.parse::<u32>().is_ok()).then(|| format!("{base}/origin.jpg"))
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
    let legacy = url_key(url, 0);
    // Only downloads take a download slot. A page's network fetches carry on
    // after it is left, and a cached cover queued behind them would keep the
    // page we came back to blank for as long as they run.
    let path = match ctx.images.cached(&legacy, url) {
        Some(path) => path,
        None => {
            let _download = ctx.img_sem.acquire().await.ok()?;
            ctx.images
                .get_or_download(&legacy, url)
                .await
                .inspect_err(|e| tracing::warn!("artwork download failed: {e}"))
                .ok()?
        }
    };
    let _decode = ctx.decode_sem.acquire().await.ok()?;
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
            np_swipe: RefCell::default(),
            clipboard: RefCell::default(),
            video: RefCell::default(),
            art: RefCell::default(),
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
        #[cfg(target_os = "macos")]
        crate::window_state::remember(self.win.window(), &self.ctx);
        let weak = Rc::downgrade(self);
        let finish = move || {
            // Menu actions (⌘W included) run while Slint holds the native
            // window, so it can only be torn down once the action has returned.
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                ui.enter_background();
                if let Err(e) = ui.win.hide() {
                    tracing::warn!("Could not close the window: {e}");
                }
            });
        };
        // The window shrinks away first, then goes.
        #[cfg(target_os = "macos")]
        crate::platform::animate_window_out(self.win.window(), finish);
        #[cfg(not(target_os = "macos"))]
        finish();
    }

    /// Drop what only the window needs before it goes away.
    fn enter_background(self: &Rc<Self>) {
        #[cfg(target_os = "macos")]
        {
            crate::window_state::remember(self.win.window(), &self.ctx);
            crate::window_state::prepare_restore();
        }
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
        #[cfg(target_os = "macos")]
        crate::window_state::place_before_show(self.win.window(), &self.ctx);
        if let Err(e) = self.win.show() {
            tracing::warn!("Could not reopen the window: {e}");
            return;
        }
        #[cfg(target_os = "macos")]
        {
            crate::platform::window_reopened();
            slint::Timer::single_shot(Duration::ZERO, || on_ui(|ui| ui.restore_window()));
        }
    }

    #[cfg(target_os = "macos")]
    pub fn restore_window(&self) {
        crate::window_state::restore(self.win.window(), &self.ctx);
    }

    fn set_liquid_glass(self: &Rc<Self>, on: bool) {
        self.update_settings(|s| s.glass = Some(on));
        crate::platform::set_glass_choice(Some(on));
        let glass = crate::platform::glass_wanted();
        self.app().set_liquid_glass(glass);
        self.app().set_glass(glass);
    }

    // ------------------------------------------------------------ memory

    fn fancy_animation(&self) -> bool {
        fancy_animation_override().unwrap_or(self.ctx.settings.read().unwrap().fancy_animation)
    }

    /// The lean memory profile: everything but Fancy Animation.
    fn low_ram(&self) -> bool {
        !self.fancy_animation()
    }

    fn set_fancy_animation(self: &Rc<Self>, on: bool) {
        self.update_settings(|s| s.fancy_animation = on);
        self.app().set_fancy_animation(on);
        crate::platform::apply_memory_profile(!on);
        if !on {
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
        // One BGRA surface per drawable at physical size: two, or three with
        // Fancy Animation. None while the window is closed.
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
        bind!(on_play_grid_pick, |ui| ui.play_grid_pick());
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
        bind!(on_catalog_art, |ui, shelf, group, from, upto| {
            let Some(shelf) = Shelf::from_key(&shelf) else {
                return;
            };
            let (from, upto) = (from.max(0) as usize, upto.max(0) as usize);
            if group < 0 {
                let model = ui.shelf_model(shelf);
                ui.card_art(shelf.key(), model, from, upto);
                return;
            }
            let model = ui
                .state
                .borrow()
                .search_lists
                .get(&shelf)
                .and_then(|list| list.models.get(group as usize).cloned());
            if let Some(model) = model {
                ui.card_art(&format!("{}:{group}", shelf.key()), ModelRc::from(model), from, upto);
            }
        });
        bind!(on_arrange_search, |ui, shelf, sort, group| {
            if let Some(shelf) = Shelf::from_key(&shelf).filter(|s| s.arranged()) {
                ui.arrange_search(shelf, Some(sort.to_string()), Some(group.to_string()));
            }
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
        bind!(on_set_grid_size, |ui, size| ui.set_grid_size(size));
        bind!(on_zoom_in, |ui| ui.change_zoom(1));
        bind!(on_zoom_out, |ui| ui.change_zoom(-1));
        bind!(on_reset_zoom, |ui| ui.set_zoom(100));
        bind!(on_create_in_collection, |ui, kind, name| ui
            .create_in_collection(&kind, name.trim()));
        bind!(on_save_playlist, |ui, title, description, public| ui
            .save_playlist(&title, &description, public));
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
            if q.is_empty() {
                return;
            }
            // Enter again on the results already shown plays the picked hit
            // (the first, or where ↑ ↓ moved it).
            if matches!(ui.current_page(), Some(Page::Search(shown)) if shown == q) {
                let at = ui.app().get_pick_index().max(0) as usize;
                if let Some(track) = ui.app().get_tracks().row_data(at) {
                    ui.app().invoke_play_track(track.id);
                }
                return;
            }
            ui.show(Page::Search(q), true);
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
        bind!(on_add_track_to_playlist, |ui, id, title, playlist| {
            if !id.is_empty() && !playlist.is_empty() {
                ui.add_to_playlist(&playlist, vec![id.to_string()], title.to_string());
            }
        });
        bind!(on_play_track_next, |ui, id| ui.play_track_next(&id));
        bind!(on_copy_track_link, |ui, id| {
            if let Err(error) = ui.copy_track_link(&id) {
                tracing::warn!("Could not copy track link: {error}");
            }
        });
        bind!(on_play_track, |ui, id| ui.play_from_page(&id));
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
        bind!(on_open_now_playing_album, |ui| ui.open_playing_album());
        bind!(on_open_now_playing_context, |ui| ui.open_playing_source());
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
        bind!(on_check_for_updates, |ui| ui.check_for_updates());
        bind!(on_update_now, |ui| crate::update::install_now(&ui));
        bind!(on_update_skip, |ui| ui.skip_update());
        bind!(on_update_dismiss, |ui| crate::update::dismiss(&ui));
        app.on_open_update_page(|| {
            let _ = open::that(crate::update::RELEASES_PAGE);
        });
        {
            // The red close button: same as ⌘W, animation included.
            let weak = Rc::downgrade(self);
            self.win.window().on_close_requested(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.close_window();
                }
                slint::CloseRequestResponse::KeepWindowShown
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
        bind!(on_set_show_key_letters, |ui, on| ui
            .update_settings(|s| s.show_key_letters = on));
        bind!(on_clear_cache, |ui| {
            if let Err(e) = ui.ctx.audio.clear() {
                tracing::warn!("could not clear the audio cache: {e}");
            }
            ui.present_settings();
        });
        bind!(on_open_cache_folder, |ui| {
            let dir = ui.ctx.audio.dir();
            let _ = std::fs::create_dir_all(dir);
            if let Err(e) = std::process::Command::new("open").arg(dir).spawn() {
                tracing::warn!("could not open the audio cache folder: {e}");
            }
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
        bind!(on_set_crossfade, |ui, on| {
            ui.update_settings(|s| s.crossfade = on);
            ui.apply_playback_settings();
        });
        bind!(on_set_crossfade_secs, |ui, secs| {
            ui.update_settings(|s| s.crossfade_secs = secs.clamp(1, 12) as u32);
            ui.apply_playback_settings();
        });
        bind!(on_set_normalize_volume, |ui, on| {
            ui.update_settings(|s| s.normalize_volume = on);
            ui.apply_playback_settings();
        });
        bind!(on_set_normalize_album, |ui, on| {
            ui.update_settings(|s| s.normalize_album = on);
            ui.apply_playback_settings();
        });
        bind!(on_set_keep_most_played, |ui, on| ui
            .update_settings(|s| s.keep_most_played = on));
        bind!(on_set_fancy_animation, |ui, on| ui.set_fancy_animation(on));
        bind!(on_set_liquid_glass, |ui, on| ui.set_liquid_glass(on));
        bind!(on_set_animated_artwork, |ui, on| ui
            .set_animated_artwork(on));
        bind!(on_hover_art, |ui, key, uuid| ui.hover_art(&key, &uuid));
        bind!(on_set_show_lyrics, |ui, on| {
            ui.update_settings(|s| s.show_lyrics = on);
            ui.app().set_show_lyrics(on);
        });
        bind!(on_seek_lyric, |ui, row| ui.seek_lyric(row));
        bind!(on_play_queue_index, |ui, index| ui.play_queue_index(index));
        bind!(on_np_queue_visible, |ui, first, count| ui
            .np_queue_visible(first.max(0) as usize, count.max(0) as usize));
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
        crate::zoom::schedule(&self.win);
        self.present_signal();
        if !has_session {
            self.detect_login_browser();
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
        if let Some(adds) = load::<HashMap<String, i64>>(&self.ctx.meta, "playlist-adds") {
            self.state.borrow_mut().playlist_adds = adds;
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
        let source = self
            .ctx
            .meta
            .get_json("playback-source")
            .and_then(|json| serde_json::from_str::<Option<Page>>(&json).ok())
            .flatten();
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
                st.playback_source = source;
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
                            let page_gen = ui.state.borrow().page_gen;
                            ui.show_sections("home", page_gen, sections);
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

    /// Now Playing art from the full-size original (`origin.jpg`, quality 100).
    /// Until it arrives, or if it 403s, the view shows the 640px `np-cover`.
    /// Kept out of `state.images`: originals can be several MB each.
    fn load_large_cover(self: &Rc<Self>, track_id: &str, cover_url: &str) {
        self.app().set_np_cover_large(Image::default());
        if std::env::var_os("TIDE_NO_ART").is_some() {
            return;
        }
        let Some(url) = origin_cover(cover_url) else {
            return;
        };
        let id = track_id.to_string();
        self.spawn(|ctx| async move {
            let Some((buffer, _)) = fetch_image(&ctx, &url, LARGE_COVER_PX).await else {
                return;
            };
            on_ui(move |ui| {
                if ui.app().get_current_id() == id.as_str() {
                    ui.app().set_np_cover_large(Image::from_rgba8(buffer));
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
        let (from, upto) = keep_window(&mut self.state.borrow_mut().art_keep, key, from, upto, n);
        for i in 0..n {
            if (from..upto).contains(&i) {
                continue;
            }
            if let Some(row) = model.row_data(i)
                && !row.image_url.is_empty()
                && row.image.size().width > 0
            {
                self.state.borrow_mut().images.remove(&url_key(
                    &card_art_url(&row.kind, &row.image_url, max_px),
                    max_px,
                ));
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
            let url = card_art_url(&row.kind, &row.image_url, max_px);
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
        let (from, upto) = keep_window(&mut self.state.borrow_mut().art_keep, key, from, upto, n);
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
        self.drop_grid_art_at(self.grid_px());
    }

    fn drop_grid_art_at(self: &Rc<Self>, px: u32) {
        let models = self.state.borrow().grid.models.clone();
        for (i, model) in models.iter().enumerate() {
            self.keep_card_images(&format!("grid:{i}"), ModelRc::from(model.clone()), 0, 0, px);
        }
    }

    /// Grid artwork size for the current grid-size setting (see `grid_art_px`).
    fn grid_px(&self) -> u32 {
        grid_art_px(self.ctx.settings.read().unwrap().grid_size)
    }

    /// The grid slider moved: if the artwork size changed, reload what is on screen at it.
    fn set_grid_size(self: &Rc<Self>, size: i32) {
        let before = self.grid_px();
        self.update_settings(|s| s.grid_size = size.clamp(0, 4));
        if self.grid_px() == before {
            return;
        }
        let ranges: Vec<_> = {
            let st = self.state.borrow();
            (0..st.grid.models.len())
                .map(|i| st.art_keep.get(&format!("grid:{i}")).copied())
                .collect()
        };
        self.drop_grid_art_at(before);
        for (group, range) in ranges.into_iter().enumerate() {
            if let Some((from, upto)) = range {
                self.grid_art(group, from, upto);
            }
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
                    layout: section_layout(s).into(),
                    cards: ModelRc::from(cards),
                    tracks: ModelRc::from(tracks),
                }
            })
            .collect();
        let outer = Rc::new(VecModel::from(rows));
        {
            let mut st = self.state.borrow_mut();
            st.sections = sections;
            st.section_models = models;
            st.sections_model = Some(outer.clone());
            st.sections_source = None;
        }
        self.app().set_sections(ModelRc::from(outer));
    }

    /// Sections loaded from `source` (a cache key) for page `page_gen`. The first
    /// copy a page shows is drawn whole; a newer one from the server is patched
    /// in, so only what changed moves and new cards grow in where they belong.
    fn show_sections(self: &Rc<Self>, source: &str, page_gen: u64, sections: Vec<Section>) {
        let outer = {
            let st = self.state.borrow();
            let same = st
                .sections_source
                .as_ref()
                .is_some_and(|(g, key)| *g == page_gen && key == source);
            st.sections_model
                .clone()
                .filter(|_| same && !st.sections.is_empty())
        };
        let Some(outer) = outer else {
            self.set_sections(sections);
            self.state.borrow_mut().sections_source = Some((page_gen, source.to_string()));
            return;
        };
        let (fav_ids, collection, old, ranges) = {
            let st = self.state.borrow();
            let old: Vec<(String, SectionModel)> = st
                .sections
                .iter()
                .zip(st.section_models.iter().cloned())
                .map(|(s, models)| (group_key(section_layout(s), &s.title), models))
                .collect();
            let ranges: Vec<Option<(usize, usize)>> = (0..old.len())
                .map(|i| st.art_keep.get(&format!("home:{i}")).copied())
                .collect();
            (st.fav_ids.clone(), st.collection.clone(), old, ranges)
        };
        let groups = sections
            .iter()
            .map(|s| GroupRows {
                title: s.title.clone(),
                layout: section_layout(s).to_string(),
                cards: s
                    .cards
                    .iter()
                    .map(|c| card_row_fav(c, &collection))
                    .collect(),
                tracks: s
                    .tracks
                    .iter()
                    .map(|t| track_row(t, fav_ids.contains(&t.id)))
                    .collect(),
            })
            .collect();
        let patched = patch_groups(&outer, old, groups);
        {
            let mut st = self.state.borrow_mut();
            st.sections = sections;
            st.section_models = patched.models;
        }
        // Kept sections may have moved or gained rows: fill their artwork again.
        for (now, before) in patched.kept {
            if let Some((from, upto)) = ranges[before] {
                self.section_visible(now, from, upto);
            }
        }
        settle_fresh(patched.fresh_cards, patched.fresh_tracks);
    }

    /// Home section `index` is near the viewport: keep artwork for cards `[from, upto)`.
    fn section_visible(self: &Rc<Self>, index: usize, from: usize, upto: usize) {
        let (card_model, track_model, small, shift) = {
            let st = self.state.borrow();
            let Some(section) = st.sections.get(index) else {
                return;
            };
            let Some((card_model, track_model, _)) = st.section_models.get(index) else {
                return;
            };
            let small = section.layout == "shortcuts"
                || section.title.eq_ignore_ascii_case("Recently played");
            // Feed rows list their tracks after the cards: shift the range onto the tracks.
            let shift = if section_layout(section) == "rows" {
                section.cards.len()
            } else {
                0
            };
            (card_model.clone(), track_model.clone(), small, shift)
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
            from.saturating_sub(shift),
            upto.saturating_sub(shift),
            128,
        );
    }

    fn grid_art(self: &Rc<Self>, group: usize, from: usize, upto: usize) {
        let Some(model) = self.state.borrow().grid.models.get(group).cloned() else {
            return;
        };
        let px = self.grid_px();
        self.keep_card_images(
            &format!("grid:{group}"),
            ModelRc::from(model),
            from,
            upto,
            px,
        );
    }

    // ------------------------------------------------------------ settings

    pub(super) fn change_zoom(self: &Rc<Self>, direction: i32) {
        let percent = self.ctx.settings.read().unwrap().zoom_percent;
        self.set_zoom(crate::zoom::step(percent, direction));
    }

    fn set_zoom(self: &Rc<Self>, percent: i32) {
        let percent = crate::zoom::normalize(percent);
        if self.ctx.settings.read().unwrap().zoom_percent == percent {
            return;
        }
        // Page-transition snapshots were captured at the old scale.
        self.nav_finish();
        self.state.borrow_mut().snaps.clear();
        self.app().set_menu_open(false);
        self.update_settings(|s| s.zoom_percent = percent);
        crate::zoom::schedule(&self.win);
    }

    fn present_settings(&self) {
        let app = self.app();
        let settings = self.ctx.settings.read().unwrap().clone();
        app.set_zoom_percent(settings.zoom_percent);
        app.set_quality(settings.quality.into());
        app.set_show_collection(settings.show_collection);
        app.set_show_audio_metadata(settings.show_audio_metadata);
        app.set_show_key_letters(settings.show_key_letters);
        app.set_keep_most_played(settings.keep_most_played);
        app.set_fancy_animation(fancy_animation_override().unwrap_or(settings.fancy_animation));
        app.set_show_lyrics(settings.show_lyrics);
        app.set_animated_artwork(settings.animated_artwork);
        app.set_liquid_glass(crate::platform::glass_wanted());
        app.set_glass_available(crate::platform::glass_available());
        app.set_rate_matching(settings.rate_matching);
        app.set_restore_sample_rate(settings.restore_sample_rate);
        app.set_crossfade(settings.crossfade);
        app.set_crossfade_secs(settings.crossfade_secs as i32);
        app.set_normalize_volume(settings.normalize_volume);
        app.set_normalize_album(settings.normalize_album);
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

    /// Hand crossfade and volume normalisation to the player and show them.
    fn apply_playback_settings(&self) {
        let settings = self.ctx.settings.read().unwrap().clone();
        let app = self.app();
        app.set_crossfade(settings.crossfade);
        app.set_crossfade_secs(settings.crossfade_secs as i32);
        app.set_normalize_volume(settings.normalize_volume);
        app.set_normalize_album(settings.normalize_album);
        self.ctx.player.set_crossfade(settings.crossfade_secs());
        self.ctx.player.set_normalization(settings.normalization());
        self.present_signal();
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

    /// Show the login steps that fit the default browser before the listener
    /// clicks anything: TIDE can read the final address from Safari and
    /// Chromium browsers, and needs it pasted from any other.
    fn detect_login_browser(self: &Rc<Self>) {
        self.spawn(|_ctx| async move {
            let browser = tokio::task::spawn_blocking(crate::platform::default_browser_id).await.ok().flatten();
            let scriptable = browser.is_some_and(|id| crate::platform::browser_is_scriptable(&id));
            on_ui(move |ui| {
                let app = ui.app();
                if !app.get_pkce_waiting() {
                    app.set_pkce_manual(!scriptable);
                }
            });
        });
    }

    /// Start the Max-quality login in the default browser (or `browser`, a
    /// bundle id) and wait for the browser to land on TIDAL's redirect address.
    fn pkce_start(self: &Rc<Self>, browser: Option<&'static str>) {
        let app = self.app();
        app.set_pkce_waiting(true);
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
                // A browser that can't be asked for the address: the steps say to
                // copy it and paste it, so no status line is needed.
                app.set_pkce_manual(!scriptable);
                app.set_pkce_offer_safari(!scriptable);
                app.set_pkce_status(if scriptable { "Waiting for you to sign in…" } else { "" }.into());
            });

            // Watch the browser for the redirect until it shows up, the attempt is
            // replaced, or we give up. Any other browser finishes through the paste box.
            let deadline = tokio::time::Instant::now() + LOGIN_WAIT;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if ctx.pkce_attempt.load(Ordering::SeqCst) != attempt {
                    return;
                }
                if !scriptable {
                    continue;
                }
                let id = browser_id.clone();
                let found = tokio::task::spawn_blocking(move || crate::platform::redirect_from_browser(&id)).await;
                match found {
                    Ok(Ok(Some(redirect))) => {
                        if ctx.pkce_attempt.load(Ordering::SeqCst) == attempt {
                            on_ui(move |ui| ui.pkce_finish(redirect));
                        }
                        return;
                    }
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(e)) => {
                        // Not allowed to ask the browser (or it failed): fall back to copy and paste.
                        tracing::warn!("cannot read the login address from the browser: {e}");
                        scriptable = false;
                        on_ui(|ui| {
                            let app = ui.app();
                            app.set_pkce_manual(true);
                            app.set_pkce_status("TIDE can't read your browser. Copy and paste the address (steps 3 and 4).".into());
                        });
                    }
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

    /// Look for a newer release now and then every few hours (release builds only).
    pub fn start_update_checks(self: &Rc<Self>) {
        if crate::update::enabled() {
            self.spawn(crate::update::run_checks);
        }
    }

    /// TIDE → Check for Updates…: ignores a skipped version and always answers.
    fn check_for_updates(self: &Rc<Self>) {
        if !crate::update::enabled() {
            crate::alerts::notice(
                &self.app(),
                &format!(
                    "Development build {}: updates come from git",
                    crate::update::CURRENT
                ),
            );
            return;
        }
        self.spawn(|ctx| async move { crate::update::check(&ctx, true).await });
    }

    /// Skip this version: no toast for it again unless asked from the menu.
    fn skip_update(self: &Rc<Self>) {
        if let Some(version) = crate::update::offered_version() {
            self.update_settings(|s| s.skipped_version = version);
        }
        self.app().set_update_state("".into());
    }

    fn set_sidebar(self: &Rc<Self>, cards: &[Card]) {
        self.state.borrow_mut().sidebar_cards = cards.to_vec();
        self.present_add_targets();
        self.present_sidebar();
    }

    /// Fill the "Add to playlist" list (the menu's flyout and the panel a
    /// dragged track is dropped on).
    fn present_add_targets(self: &Rc<Self>) {
        let (own, recent) = {
            let st = self.state.borrow();
            add_targets(&st.sidebar_cards, &st.playlist_adds)
        };
        let app = self.app();
        app.set_own_playlists(ModelRc::from(self.cards_model("own-playlists", &own, 96)));
        app.set_recent_playlists(recent as i32);
    }

    fn present_sidebar(self: &Rc<Self>) {
        let app = self.app();
        let mut cards = self.state.borrow().sidebar_cards.clone();
        // "Move to folder" lists them by name, whichever way the sidebar is sorted.
        let folders: Vec<CardData> = cards
            .iter()
            .filter(|card| card.kind == "folder")
            .map(card_row)
            .collect();
        app.set_folders(ModelRc::new(VecModel::from(folders)));
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
            arrange_pinned(
                &st.grid.all,
                &st.grid.pins,
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
        let outer = Rc::new(VecModel::from(sections));
        {
            let mut st = self.state.borrow_mut();
            st.grid.models = models;
            st.grid.groups_model = Some(outer.clone());
        }
        let app = self.app();
        self.reset_grid_pick(&flat, false);
        app.set_grid_cards(ModelRc::new(VecModel::from(flat)));
        app.set_grid_groups(ModelRc::from(outer));
    }

    /// A newer copy of the grid from the server: patch the groups on screen
    /// instead of redrawing them, so only new or changed cards move.
    fn update_grid(self: &Rc<Self>, cards: &[Card]) {
        let outer = self
            .state
            .borrow()
            .grid
            .groups_model
            .clone()
            .filter(|m| m.row_count() > 0);
        let Some(outer) = outer else {
            self.set_grid(cards);
            return;
        };
        self.state.borrow_mut().grid.all = cards.to_vec();
        let (groups, collection, old, ranges) = {
            let st = self.state.borrow();
            let groups = arrange_pinned(
                &st.grid.all,
                &st.grid.pins,
                &st.grid.filter,
                &st.grid.sort,
                &st.grid.group,
                st.grid.combine_others,
            );
            let old: Vec<(String, SectionModel)> = (0..outer.row_count())
                .filter_map(|i| outer.row_data(i))
                .zip(st.grid.models.iter())
                .map(|(row, cards)| {
                    let tracks = Rc::new(VecModel::<TrackData>::default());
                    (group_key("cards", &row.title), (cards.clone(), tracks, 0))
                })
                .collect();
            let ranges: Vec<Option<(usize, usize)>> = (0..old.len())
                .map(|i| st.art_keep.get(&format!("grid:{i}")).copied())
                .collect();
            (groups, st.collection.clone(), old, ranges)
        };
        let flat: Vec<CardData> = groups
            .iter()
            .flat_map(|(_, cards)| cards.iter().map(|c| card_row_fav(c, &collection)))
            .collect();
        let rows = groups
            .iter()
            .map(|(title, cards)| GroupRows {
                title: title.clone(),
                layout: "cards".into(),
                cards: cards.iter().map(|c| card_row_fav(c, &collection)).collect(),
                tracks: Vec::new(),
            })
            .collect();
        let patched = patch_groups(&outer, old, rows);
        self.state.borrow_mut().grid.models = patched
            .models
            .into_iter()
            .map(|(cards, ..)| cards)
            .collect();
        // Its length and the picked card are read; the cards live in the groups.
        self.reset_grid_pick(&flat, true);
        self.app()
            .set_grid_cards(ModelRc::new(VecModel::from(flat)));
        for (now, before) in patched.kept {
            if let Some((from, upto)) = ranges[before] {
                self.grid_art(now, from, upto);
            }
        }
        settle_fresh(patched.fresh_cards, Vec::new());
    }

    /// Every pinned card, as `pin_key`s.
    fn pins(&self) -> HashSet<String> {
        self.ctx
            .meta
            .get_json(PINS_KEY)
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// Pin a card to the top of its library page, or let it go back to its place.
    fn toggle_pin(self: &Rc<Self>, card: &Card) {
        let key = pin_key(&card.kind, &card.id);
        let mut pins = self.pins();
        let pinned = !pins.remove(&key);
        if pinned {
            pins.insert(key);
        }
        let mut sorted: Vec<&String> = pins.iter().collect();
        sorted.sort();
        if let Err(e) = self
            .ctx
            .meta
            .put_json(PINS_KEY, &serde_json::json!(sorted).to_string())
        {
            tracing::warn!("could not save the pins: {e}");
        }
        let text = if pinned {
            format!("Pinned “{}”", card.title)
        } else {
            format!("Unpinned “{}”", card.title)
        };
        crate::alerts::notice(&self.app(), &text);
        let on_grid = self.app().get_view() == "grid" && {
            let mut st = self.state.borrow_mut();
            let pinnable = pinnable_grid(&st.grid.kind);
            if pinnable {
                st.grid.pins = pins;
            }
            pinnable
        };
        if on_grid {
            self.reflow_grid(&card.kind, &card.id);
        }
    }

    /// Rearrange the grid on screen in place, so every card that stays keeps
    /// its element and glides to its new slot, and the card that moved flies
    /// over from where it was (views.slint GridItem).
    fn reflow_grid(self: &Rc<Self>, kind: &str, id: &str) {
        let app = self.app();
        app.set_grid_reflow(true);
        // The artwork already on screen, so a card that changes group keeps
        // its cover in flight instead of waiting on the artwork loader.
        let covers: HashMap<(String, String), slint::Image> = self
            .state
            .borrow()
            .grid
            .models
            .iter()
            .flat_map(|model| model.iter())
            .filter(|row| row.image.size().width > 0)
            .map(|row| ((row.kind.to_string(), row.id.to_string()), row.image))
            .collect();
        let all = self.state.borrow().grid.all.clone();
        self.update_grid(&all);
        let mut group = -1;
        for (g, model) in self.state.borrow().grid.models.iter().enumerate() {
            for i in 0..model.row_count() {
                let Some(mut row) = model.row_data(i) else { continue };
                let moved = row.kind == kind && row.id == id;
                if moved {
                    group = g as i32;
                }
                let cover = (row.image.size().width == 0)
                    .then(|| covers.get(&(row.kind.to_string(), row.id.to_string())))
                    .flatten();
                // The moved card arrives as a new row: it flies, not pops.
                if cover.is_some() || (moved && row.fresh) {
                    if let Some(cover) = cover {
                        row.image = cover.clone();
                    }
                    if moved {
                        row.fresh = false;
                    }
                    model.set_row_data(i, row);
                }
            }
        }
        app.set_grid_fly_group(group);
        let ui = Rc::downgrade(self);
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            if let Some(ui) = ui.upgrade() {
                ui.app().set_grid_reflow(false);
                ui.app().set_grid_fly_group(-1);
            }
        });
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

    /// What the name dialog confirmed: create an empty playlist or folder, rename
    /// or delete a folder, delete a playlist or make it public.
    fn create_in_collection(self: &Rc<Self>, kind: &str, name: &str) {
        let target = self.app().get_create_folder_id().to_string();
        match kind {
            "rename-folder" => self.rename_folder(&target, name),
            "delete-folder" => self.delete_folder(&target, name),
            "delete-playlist" => self.delete_playlist(&target, name),
            "publish-playlist" => self.set_playlist_public(&target, name, true),
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
                            if let Some(Page::Folder { id: fid, title }) = st.history.get_mut(pos)
                                && fid == &id
                            {
                                *title = name.clone();
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

    /// What only a playlist's menu or page offers. Everything but moving it to
    /// a folder is for the user's own playlists.
    fn playlist_action(self: &Rc<Self>, card: &Card, action: &str, arg: &str) {
        let app = self.app();
        let own = card.by == "Me";
        let confirm = |kind: &str| {
            app.set_create_folder_id(card.id.as_str().into());
            app.set_create_name(card.title.as_str().into());
            app.set_create_kind(kind.into());
        };
        match action {
            "move-to-folder" if !arg.is_empty() => self.move_playlist(card, arg),
            "edit" if own => self.edit_playlist(card),
            // Going public is confirmed first; going private again is harmless.
            "make-public" if own => confirm("publish-playlist"),
            "make-private" if own => self.set_playlist_public(&card.id, &card.title, false),
            // The dialog asks before anything goes.
            "delete" if own => confirm("delete-playlist"),
            _ => {}
        }
    }

    /// Open the Edit dialog on one of the user's own playlists, with its
    /// current description and sharing fetched first.
    fn edit_playlist(self: &Rc<Self>, card: &Card) {
        let app = self.app();
        let on_page = self
            .page_card()
            .is_some_and(|page| page.kind == card.kind && page.id == card.id);
        let menu = app.get_menu_card();
        app.set_edit_cover(if on_page {
            app.get_page_cover()
        } else if menu.id == card.id.as_str() {
            menu.image
        } else {
            Image::default()
        });
        let (id, title) = (card.id.clone(), card.title.clone());
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.playlist_detail(&id).await {
                Ok(detail) => on_ui(move |ui| {
                    let app = ui.app();
                    app.set_create_folder_id(detail.card.id.as_str().into());
                    app.set_create_name(detail.card.title.as_str().into());
                    app.set_edit_description(detail.description.as_str().into());
                    app.set_edit_public(detail.card.is_public);
                    ui.state.borrow_mut().editing = Some(detail);
                    app.set_create_kind("edit-playlist".into());
                }),
                Err(e) => tracing::error!("Could not open “{title}” for editing: {e}"),
            }
        });
    }

    /// Save of the Edit dialog: only what changed is sent.
    fn save_playlist(self: &Rc<Self>, title: &str, description: &str, public: bool) {
        let Some(was) = self.state.borrow_mut().editing.take() else {
            return;
        };
        let (title, description) = (title.trim().to_string(), description.trim().to_string());
        let text_changed = title != was.card.title || description != was.description;
        let public_changed = public != was.card.is_public;
        if title.is_empty() || !(text_changed || public_changed) {
            return;
        }
        let id = was.card.id;
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            let saved = async {
                if text_changed {
                    api.edit_playlist(&id, &title, &description).await?;
                }
                if public_changed {
                    api.set_playlist_public(&id, public).await?;
                }
                Ok::<(), tide_api::ApiError>(())
            };
            match saved.await {
                Ok(()) => on_ui(move |ui| {
                    crate::alerts::notice(&ui.app(), &format!("Saved “{title}”"));
                    ui.playlist_edited(&id, Some(&description), |card| {
                        card.title = title.clone();
                        card.is_public = public;
                    });
                }),
                Err(e) => tracing::error!("Could not save “{title}”: {e}"),
            }
        });
    }

    fn set_playlist_public(self: &Rc<Self>, id: &str, name: &str, public: bool) {
        if id.is_empty() {
            return;
        }
        let (id, name) = (id.to_string(), name.to_string());
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            let state = if public { "public" } else { "private" };
            match api.set_playlist_public(&id, public).await {
                Ok(()) => on_ui(move |ui| {
                    crate::alerts::notice(&ui.app(), &format!("“{name}” is now {state}"));
                    ui.playlist_edited(&id, None, |card| card.is_public = public);
                }),
                Err(e) => tracing::error!("Could not make “{name}” {state}: {e}"),
            }
        });
    }

    /// Put a playlist of the collection into `folder` ("root": out of its folder).
    fn move_playlist(self: &Rc<Self>, card: &Card, folder: &str) {
        let target = self
            .state
            .borrow()
            .sidebar_cards
            .iter()
            .find(|c| c.kind == "folder" && c.id == folder)
            .map(|c| c.title.clone());
        let (id, name, folder) = (card.id.clone(), card.title.clone(), folder.to_string());
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.move_playlist(&id, &folder).await {
                Ok(()) => on_ui(move |ui| {
                    let text = match &target {
                        Some(target) => format!("Moved “{name}” to “{target}”"),
                        None => format!("Moved “{name}” out of its folder"),
                    };
                    crate::alerts::notice(&ui.app(), &text);
                    ui.reload_collection(None);
                }),
                Err(e) => tracing::error!("Could not move “{name}”: {e}"),
            }
        });
    }

    /// A playlist the user just changed: every copy on screen follows at once,
    /// without waiting for TIDAL to hand the change back.
    fn playlist_edited(
        self: &Rc<Self>,
        id: &str,
        description: Option<&str>,
        change: impl Fn(&mut Card),
    ) {
        let is_it = |card: &Card| card.kind == "playlist" && card.id == id;
        let cards = {
            let mut st = self.state.borrow_mut();
            for page in st.history.iter_mut() {
                if let Page::Detail(card) = page
                    && is_it(card)
                {
                    change(card);
                }
            }
            for card in st.sidebar_cards.iter_mut().filter(|card| is_it(card)) {
                change(card);
            }
            st.sidebar_cards.clone()
        };
        self.set_sidebar(&cards);
        // The page's saved detail, so the header agrees now and on the next visit.
        let key = format!("catalog:playlist:{id}");
        let saved = self.ctx.meta.get_json(&key);
        if let Some(mut detail) = saved
            .as_deref()
            .and_then(|json| serde_json::from_str::<PlaylistDetail>(json).ok())
        {
            change(&mut detail.card);
            if let Some(description) = description {
                detail.description = description.to_string();
            }
            if let Ok(json) = serde_json::to_string(&detail) {
                let _ = self.ctx.meta.put_json(&key, &json);
            }
            self.present_playlist_detail(&detail);
        }
        self.reload_collection(None);
    }

    /// Delete one of the user's own playlists, once the dialog has confirmed it.
    fn delete_playlist(self: &Rc<Self>, id: &str, name: &str) {
        if id.is_empty() {
            return;
        }
        let (id, name) = (id.to_string(), name.to_string());
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.delete_playlist(&id).await {
                Ok(()) => on_ui(move |ui| {
                    let text = if name.is_empty() {
                        "Deleted playlist".into()
                    } else {
                        format!("Deleted playlist “{name}”")
                    };
                    crate::alerts::notice(&ui.app(), &text);
                    // Gone at once, without waiting for the sync below.
                    let cards: Vec<Card> = {
                        let st = ui.state.borrow();
                        st.sidebar_cards
                            .iter()
                            .filter(|card| !(card.kind == "playlist" && card.id == id))
                            .cloned()
                            .collect()
                    };
                    ui.set_sidebar(&cards);
                    ui.reload_collection(Some(&id));
                }),
                Err(e) => tracing::error!("Could not delete playlist “{name}”: {e}"),
            }
        });
    }

    /// Refresh the sidebar and the playlists grid after a collection write.
    /// `deleted` is the folder or playlist that is gone: its page gives way to
    /// the playlists grid.
    fn reload_collection(self: &Rc<Self>, deleted: Option<&str>) {
        self.sync_playlists();
        let gone = |page: &Page| match page {
            Page::Folder { id, .. } => deleted == Some(id.as_str()),
            Page::Detail(card) => card.kind == "playlist" && deleted == Some(card.id.as_str()),
            _ => false,
        };
        let page = match self.current_page() {
            Some(page) if gone(&page) => {
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
        self.reveal_playing_track();
        let count = if self.app().get_view() == "artist" {
            4
        } else {
            30
        };
        self.rows_visible(0, count);
    }

    /// A newer copy of the track list on screen: patch it row by row.
    fn update_tracks(self: &Rc<Self>, tracks: Vec<TrackItem>, context: &str) {
        let model = {
            let st = self.state.borrow();
            st.tracks_model
                .clone()
                .filter(|m| m.row_count() > 0 && st.page_context == context)
        };
        let Some(model) = model.filter(|_| self.app().get_filter_text().is_empty()) else {
            self.set_tracks(tracks, context);
            return;
        };
        let rows: Vec<TrackData> = {
            let st = self.state.borrow();
            tracks
                .iter()
                .map(|t| track_row(t, st.fav_ids.contains(&t.id)))
                .collect()
        };
        let inserted = patch_rows(&model, rows, track_key, merge_track, |row| row.fresh = true);
        self.state.borrow_mut().page_tracks = tracks;
        let (from, upto) = self
            .state
            .borrow()
            .art_keep
            .get("tracks")
            .copied()
            .unwrap_or((0, 30));
        self.keep_track_covers("tracks", &model, from, upto, 96);
        if inserted > 0 {
            settle_fresh(Vec::new(), vec![model]);
        }
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
        self.app().set_pick_index(0);
    }

    // ------------------------------------------------------------ navigation

    /// A page key from the keyboard or the Navigation menu: open the page and
    /// close Now Playing over it. "/" focuses the filter of the collection
    /// page on screen, or opens Tracks and focuses its filter.
    fn page_key(self: &Rc<Self>, key: &str) {
        let app = self.app();
        app.set_show_now_playing(false);
        if key == "/" {
            let view = app.get_view();
            if view != "grid" && view != "tracks" {
                self.navigate_key("tracks");
            }
            let app = self.app();
            app.set_focus_filter(app.get_focus_filter().wrapping_add(1));
        } else {
            self.navigate_key(key);
        }
    }

    fn navigate_key(self: &Rc<Self>, key: &str) {
        if let Some(key) = key.strip_prefix("page-key:") {
            self.page_key(key);
            return;
        }
        let page = match key {
            "home" => Page::Home,
            "tracks" => Page::Tracks,
            "mixes" => Page::Mixes,
            "playlists" => Page::Playlists,
            "albums" => Page::Albums,
            "queue" => Page::Queue,
            "settings" => Page::Settings,
            "dsp" => Page::Dsp,
            "escape-home" => {
                self.escape_home();
                return;
            }
            "focus-search" => {
                let app = self.app();
                // Phone layout has no search in the top bar: ⌘F goes to
                // Explore, whose search field takes the focus when it appears.
                if app.get_phone_layout() && !app.get_video_open() {
                    app.set_show_now_playing(false);
                    let on_search = app.get_view() == "search"
                        || (app.get_view() == "modules" && app.get_nav_key() == "explore");
                    if !on_search {
                        self.show(Page::Explore, true);
                    }
                }
                let app = self.app();
                app.set_focus_search(app.get_focus_search() + 1);
                return;
            }
            "explore" => Page::Explore,
            "feed" => Page::Feed,
            "videos" => Page::Videos,
            "profiles" => Page::Profiles,
            "later" => Page::Later,
            "collection" => Page::Collection,
            _ => return,
        };
        self.show(page, true);
    }

    /// Esc: back to home where it was last left, as a new history entry.
    /// Already on home, it scrolls to the top instead.
    fn escape_home(self: &Rc<Self>) {
        let app = self.app();
        let left_at = {
            let mut st = self.state.borrow_mut();
            let pos = st.hist_pos;
            if st.history.get(pos).is_some_and(|p| matches!(p, Page::Home)) {
                if let Some(scroll) = st.hist_scroll.get_mut(pos) {
                    *scroll = 0.0;
                }
                drop(st);
                scroll_to_top(&app);
                return;
            }
            let end = pos.min(st.history.len());
            st.history[..end]
                .iter()
                .rposition(|p| matches!(p, Page::Home))
                .and_then(|i| st.hist_scroll.get(i).copied())
                .unwrap_or(0.0)
        };
        self.show(Page::Home, true);
        if left_at != 0.0 {
            let mut st = self.state.borrow_mut();
            let pos = st.hist_pos;
            if let Some(scroll) = st.hist_scroll.get_mut(pos) {
                *scroll = left_at;
            }
            drop(st);
            app.set_scroll_y(left_at);
        }
    }

    fn open_card(self: &Rc<Self>, card: CardData) {
        if card.id.is_empty() {
            return;
        }
        let page = match card.kind.as_str() {
            // A track has no page of its own: opening it plays it.
            "track" => {
                self.play_track_card(&card.id, false);
                return;
            }
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
        // Only Fancy Animation keeps snapshots: each is a full-resolution copy of
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
            crate::platform::frame_interval(),
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

    /// Update whether trackpad fingers are currently down during a vertical
    /// scroll in Now Playing, for native macOS elastic rubber-band overscroll.
    pub fn set_np_scroll_touching(&self, touching: bool) {
        let app = self.app();
        if app.get_np_scroll_touching() != touching {
            app.set_np_scroll_touching(touching);
        }
    }

    /// Keep synced lyrics tightly aligned to playback while Now Playing is open.
    pub fn tick_lyrics(&self) {
        let app = self.app();
        if !app.get_show_now_playing() || !app.get_playing() || !app.get_lyrics_synced() {
            return;
        }
        self.update_lyric_line(self.effective_elapsed());
    }

    /// A horizontal two-finger swipe started. `back` is a swipe to the right.
    /// Inside Now Playing this tracks Lyrics (0) ↔ Queue (1) 1:1 under the
    /// fingers; elsewhere it steps through page history when `allow_page` is true.
    pub fn swipe_begin(self: &Rc<Self>, back: bool, allow_page: bool) -> bool {
        let app = self.app();
        if app.get_logged_in() && app.get_show_now_playing() {
            if app.get_video_open()
                || app.get_menu_open()
                || app.get_debug_open()
                || !app.get_create_kind().is_empty()
            {
                return false;
            }
            let has_lyrics = app.get_lyrics_status().is_empty();
            let mut swipe = self.np_swipe.borrow_mut();
            // Interrupt an in-flight settle cleanly from its live position.
            if matches!(swipe.phase, NpSwipePhase::Settling { .. }) {
                swipe.timer.stop();
                swipe.phase = NpSwipePhase::Tracking;
                swipe.raw = swipe.visual.clamp(0.0, 1.0);
                swipe.start_queue = swipe.raw >= 0.5;
                swipe.samples.clear();
                return true;
            }
            let show_queue = app.get_show_queue();
            let show_lyrics = app.get_show_lyrics();
            if np_swipe_target(show_queue, back).is_none() {
                return false;
            }
            swipe.timer.stop();
            let start = if show_queue { 1.0 } else { 0.0 };
            let from_centred = (!show_queue && !show_lyrics) || !has_lyrics;
            swipe.phase = NpSwipePhase::Tracking;
            swipe.raw = start;
            swipe.visual = start;
            swipe.start_queue = show_queue;
            swipe.from_centred = from_centred;
            swipe.has_lyrics = has_lyrics;
            swipe.samples.clear();
            drop(swipe);
            app.set_np_swipe_from_centred(from_centred);
            app.set_np_swipe_progress(start);
            app.set_np_swipe_active(true);
            return true;
        }
        allow_page && self.nav_begin(if back { -1 } else { 1 })
    }

    /// The fingers moved `dx` logical pixels (positive to the right).
    pub fn swipe_move(&self, dx: f32) {
        let np_visual = {
            let mut swipe = self.np_swipe.borrow_mut();
            if matches!(swipe.phase, NpSwipePhase::Tracking) {
                let win_w = self.app().get_pane_width().max(320.0);
                // Comfortable trackpad drag span across the column.
                let span = (win_w * 0.34).clamp(220.0, 340.0);
                // Swipe left (dx < 0) moves 0 -> 1 towards Queue; swipe right (dx > 0) moves 1 -> 0 towards Lyrics.
                swipe.raw = (swipe.raw - dx / span).clamp(-0.6, 1.6);
                swipe.visual = np_swipe_rubber_band(swipe.raw);
                let now = Instant::now();
                let raw = swipe.raw;
                swipe.samples.push_back((now, raw));
                while swipe
                    .samples
                    .front()
                    .is_some_and(|(t, _)| now - *t > Duration::from_millis(120))
                {
                    swipe.samples.pop_front();
                }
                Some(swipe.visual)
            } else {
                None
            }
        };
        if let Some(visual) = np_visual {
            self.app().set_np_swipe_progress(visual);
            return;
        }
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

    /// The fingers lifted: settle the page change or Now Playing swipe smoothly.
    pub fn swipe_end(self: &Rc<Self>, cancelled: bool) {
        let settling = {
            let mut swipe = self.np_swipe.borrow_mut();
            if matches!(swipe.phase, NpSwipePhase::Tracking) {
                let velocity = match (swipe.samples.front(), swipe.samples.back()) {
                    (Some((t0, p0)), Some((t1, p1)))
                        if *t1 >= *t0 + Duration::from_millis(4)
                            && t1.elapsed() < Duration::from_millis(100) =>
                    {
                        (p1 - p0) / (*t1 - *t0).as_secs_f32()
                    }
                    _ => 0.0,
                };
                let commit_queue =
                    np_swipe_settle_target(swipe.start_queue, swipe.raw, velocity, cancelled);
                let to = if commit_queue { 1.0 } else { 0.0 };
                let from = swipe.visual;
                let dist = (to - from).abs();
                // Carry release speed into a crisp ease-out duration.
                let speed_boost = (velocity.abs() * 0.04).min(0.08);
                let secs = (0.14 + 0.18 * dist - speed_boost).clamp(0.10, 0.32);
                swipe.phase = NpSwipePhase::Settling {
                    from,
                    to,
                    start: Instant::now(),
                    secs,
                    commit_queue,
                };
                true
            } else {
                false
            }
        };
        if settling {
            let weak = Rc::downgrade(self);
            self.np_swipe.borrow().timer.start(
                slint::TimerMode::Repeated,
                crate::platform::frame_interval(),
                move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.np_swipe_step();
                    }
                },
            );
            return;
        }
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

    fn np_swipe_step(self: &Rc<Self>) {
        let (visual, finished) = {
            let mut swipe = self.np_swipe.borrow_mut();
            let NpSwipePhase::Settling {
                from,
                to,
                start,
                secs,
                commit_queue,
            } = swipe.phase
            else {
                return;
            };
            let t = (start.elapsed().as_secs_f32() / secs).min(1.0);
            let value = from + (to - from) * ease_out(t);
            swipe.visual = value;
            if t >= 1.0 {
                swipe.timer.stop();
                swipe.phase = NpSwipePhase::Idle;
                (to, Some((commit_queue, swipe.from_centred, swipe.has_lyrics)))
            } else {
                (value, None)
            }
        };
        let app = self.app();
        app.set_np_swipe_progress(visual);
        if let Some((commit_queue, from_centred, has_lyrics)) = finished {
            if app.get_show_now_playing() && !app.get_video_open() {
                if commit_queue {
                    app.set_show_queue(true);
                    app.set_show_lyrics(false);
                } else {
                    app.set_show_queue(false);
                    if !from_centred && has_lyrics {
                        app.set_show_lyrics(true);
                        self.update_settings(|s| s.show_lyrics = true);
                    }
                }
            }
            // Clear `np-swipe-active` on the next frame after Slint's 0ms
            // panel animations have snapped to the already-landed end state.
            let weak = Rc::downgrade(self);
            self.np_swipe.borrow().timer.start(
                slint::TimerMode::SingleShot,
                crate::platform::frame_interval(),
                move || {
                    if let Some(ui) = weak.upgrade()
                        && matches!(ui.np_swipe.borrow().phase, NpSwipePhase::Idle)
                    {
                        let app = ui.app();
                        app.set_np_swipe_active(false);
                        app.set_np_swipe_from_centred(false);
                    }
                },
            );
        }
    }

    fn show(self: &Rc<Self>, page: Page, push: bool) {
        self.state.borrow_mut().reveal_track = None;
        self.app().set_reveal_track_index(-1);
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
        self.state.borrow_mut().sync = SyncState {
            page_gen,
            ..Default::default()
        };
        app.set_syncing(false);
        {
            let st = self.state.borrow();
            app.set_can_go_back(st.hist_pos > 0);
            app.set_can_go_forward(st.hist_pos + 1 < st.history.len());
        }
        app.set_show_now_playing(false);
        app.set_page_has_cover(false);
        app.set_page_cover(Image::default());
        self.animate_art(artwork::Slot::Page, None);
        self.hover_art(&app.get_hover_motion_key(), "");
        app.set_page_subtitle("".into());
        app.set_page_artist(CardData::default());
        app.set_page_artists(link_model(Vec::new()));
        app.set_page_year("".into());
        app.set_page_quality("".into());
        app.set_page_summary("".into());
        app.set_page_description("".into());
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
        app.set_search_playlists(ModelRc::default());
        app.set_search_playlists_status("".into());
        app.set_search_albums_status("".into());
        app.set_search_album_groups(ModelRc::default());
        app.set_search_playlist_groups(ModelRc::default());
        app.set_search_tab(0);
        app.set_grid_pick_id("".into());
        app.set_pick_index(0);
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
            Page::Collection => {
                app.set_view("collection".into());
                app.set_nav_key("collection".into());
                app.set_page_title("Collection".into());
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
                "Mixes",
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
                if kind == "playlist" {
                    // Its own count is the line below; the creator arrives with the detail.
                    if card.by == "Me" {
                        app.set_page_subtitle("".into());
                    }
                    self.load_playlist_detail(id, page_gen);
                }
                if view == "artist" {
                    self.load_artist_releases(id, page_gen);
                    self.load_related_artists(id, page_gen);
                    let video_id = id.clone();
                    self.load_shelf(
                        format!("artist-videos:{id}"),
                        page_gen,
                        Shelf::ArtistVideos,
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
                app.set_search_shown(query.as_str().into());
                app.set_page_title(format!("Results for “{query}”").into());
                app.set_tracks_show_date(false);
                self.reset_search_lists();
                let cache_key = format!("search:{}", query.to_lowercase());
                let catalog_query = query.clone();
                let video_query = query.clone();
                let playlist_query = query.clone();
                let album_query = query.clone();
                self.load_shelf(
                    format!("search-albums:{}", query.to_lowercase()),
                    page_gen,
                    Shelf::SearchAlbums,
                    move |api| async move { api.search_albums(&album_query).await },
                );
                self.load_shelf(
                    format!("search-videos:{}", query.to_lowercase()),
                    page_gen,
                    Shelf::SearchVideos,
                    move |api| async move { api.search_videos(&video_query).await },
                );
                self.load_shelf(
                    format!("search-playlists:{}", query.to_lowercase()),
                    page_gen,
                    Shelf::SearchPlaylists,
                    move |api| async move { api.search_playlists(&playlist_query).await },
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
        if card.video_cover.is_some() {
            self.animate_art(artwork::Slot::Page, card.video_cover.as_deref());
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

    /// Creator, description, length and follower count of the playlist page.
    fn load_playlist_detail(self: &Rc<Self>, id: &str, page_gen: u64) {
        let id = id.to_string();
        self.load_catalog(
            format!("catalog:playlist:{id}"),
            page_gen,
            move |api| async move { api.playlist_detail(&id).await },
            |ui, detail: PlaylistDetail| ui.present_playlist_detail(&detail),
        );
    }

    fn present_playlist_detail(self: &Rc<Self>, detail: &PlaylistDetail) {
        let card = &detail.card;
        // The page's own card learns whose playlist it is and whether it is public.
        {
            let mut st = self.state.borrow_mut();
            let pos = st.hist_pos;
            match st.history.get_mut(pos) {
                Some(Page::Detail(page)) if page.kind == "playlist" && page.id == card.id => {
                    page.title = card.title.clone();
                    page.by = card.by.clone();
                    page.is_public = card.is_public;
                }
                _ => return,
            }
        }
        let app = self.app();
        app.set_page_title(card.title.as_str().into());
        let creator = match card.by.as_str() {
            "Me" => "Created by me",
            by => by,
        };
        app.set_page_subtitle(creator.into());
        // One paragraph: the header has two lines for it.
        let description = detail.description.split_whitespace().collect::<Vec<_>>();
        app.set_page_description(description.join(" ").into());
        app.set_page_summary(playlist_summary(detail).into());
        let mut row = app.get_page_card();
        row.title = card.title.as_str().into();
        row.by = card.by.as_str().into();
        row.is_public = card.is_public;
        app.set_page_card(row);
        app.set_page_can_favorite(can_favorite(card));
    }

    fn shelf_model(&self, shelf: Shelf) -> ModelRc<CardData> {
        let app = self.app();
        match shelf {
            Shelf::ArtistVideos => app.get_artist_videos(),
            Shelf::SearchVideos => app.get_search_videos(),
            Shelf::SearchAlbums => ModelRc::default(),
            Shelf::SearchPlaylists => app.get_search_playlists(),
        }
    }

    fn present_shelf(self: &Rc<Self>, shelf: Shelf, cards: Vec<Card>) {
        let model = ModelRc::new(VecModel::from(self.rows_for(&cards)));
        let app = self.app();
        match shelf {
            Shelf::ArtistVideos => app.set_artist_videos(model),
            Shelf::SearchVideos => app.set_search_videos(model),
            Shelf::SearchAlbums => {}
            Shelf::SearchPlaylists => {
                app.set_search_playlists(model.clone());
                // The All tab shows the first few without a grid of its own to ask for art.
                self.card_art(shelf.key(), model, 0, SEARCH_PLAYLIST_PREVIEW);
            }
        }
        if shelf.arranged() {
            self.state
                .borrow_mut()
                .search_lists
                .entry(shelf)
                .or_default()
                .all = cards;
            self.present_search_list(shelf);
        }
    }

    /// Saved sort and grouping for each sortable search tab, before results arrive.
    fn reset_search_lists(self: &Rc<Self>) {
        let mut lists = HashMap::new();
        for shelf in Shelf::ALL.into_iter().filter(|s| s.arranged()) {
            let (sorts, groups) = grid_options(shelf.key());
            let saved: Vec<String> = self
                .ctx
                .meta
                .get_json(&format!("grid-prefs:{}", shelf.key()))
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();
            let pick = |saved: Option<&String>, options: &[&str]| {
                saved
                    .filter(|s| options.contains(&s.as_str()))
                    .cloned()
                    .unwrap_or_else(|| options[0].to_string())
            };
            let list = SearchList {
                sort: pick(saved.first(), sorts),
                group: pick(saved.get(1), groups),
                ..Default::default()
            };
            self.show_search_arrangement(shelf, &list.sort, &list.group);
            lists.insert(shelf, list);
        }
        self.state.borrow_mut().search_lists = lists;
        for shelf in Shelf::ALL.into_iter().filter(|s| s.arranged()) {
            self.present_search_list(shelf);
        }
    }

    fn show_search_arrangement(&self, shelf: Shelf, sort: &str, group: &str) {
        let app = self.app();
        match shelf {
            Shelf::SearchAlbums => {
                app.set_search_album_sort(sort.into());
                app.set_search_album_group(group.into());
            }
            Shelf::SearchPlaylists => {
                app.set_search_playlist_sort(sort.into());
                app.set_search_playlist_group(group.into());
            }
            _ => {}
        }
    }

    fn arrange_search(self: &Rc<Self>, shelf: Shelf, sort: Option<String>, group: Option<String>) {
        let (sort, group) = {
            let mut st = self.state.borrow_mut();
            let list = st.search_lists.entry(shelf).or_default();
            if let Some(sort) = sort {
                list.sort = sort;
            }
            if let Some(group) = group {
                list.group = group;
            }
            (list.sort.clone(), list.group.clone())
        };
        let _ = self.ctx.meta.put_json(
            &format!("grid-prefs:{}", shelf.key()),
            &serde_json::json!([sort, group]).to_string(),
        );
        self.show_search_arrangement(shelf, &sort, &group);
        self.present_search_list(shelf);
    }

    /// Sort and group one search tab's results into titled grids. Ungrouped
    /// (or empty) results are one grid under the tab's name.
    fn present_search_list(self: &Rc<Self>, shelf: Shelf) {
        let groups = {
            let st = self.state.borrow();
            let Some(list) = st.search_lists.get(&shelf) else {
                return;
            };
            let mut groups = arrange_grid(&list.all, "", &list.sort, &list.group, false);
            if groups.is_empty() {
                groups.push((String::new(), Vec::new()));
            }
            groups
        };
        let models: Vec<Rc<VecModel<CardData>>> = groups
            .iter()
            .map(|(_, cards)| Rc::new(VecModel::from(self.rows_for(cards))))
            .collect();
        let sections: Vec<SectionData> = groups
            .iter()
            .zip(models.iter())
            .map(|((title, cards), model)| SectionData {
                title: if title.is_empty() {
                    shelf.label().into()
                } else {
                    format!("{title}  ·  {}", cards.len()).into()
                },
                layout: "cards".into(),
                cards: ModelRc::from(model.clone()),
                tracks: ModelRc::default(),
            })
            .collect();
        {
            let mut st = self.state.borrow_mut();
            let stale: Vec<String> = st
                .art_keep
                .keys()
                .filter(|k| k.starts_with(&format!("{}:", shelf.key())))
                .cloned()
                .collect();
            for key in stale {
                st.art_keep.remove(&key);
            }
            if let Some(list) = st.search_lists.get_mut(&shelf) {
                list.models = models;
            }
        }
        let outer = ModelRc::new(VecModel::from(sections));
        let app = self.app();
        match shelf {
            Shelf::SearchAlbums => app.set_search_album_groups(outer),
            Shelf::SearchPlaylists => app.set_search_playlist_groups(outer),
            _ => {}
        }
    }

    fn set_shelf_status(&self, shelf: Shelf, status: &str) {
        let app = self.app();
        match shelf {
            Shelf::ArtistVideos => app.set_artist_videos_status(status.into()),
            Shelf::SearchVideos => app.set_search_videos_status(status.into()),
            Shelf::SearchAlbums => app.set_search_albums_status(status.into()),
            Shelf::SearchPlaylists => app.set_search_playlists_status(status.into()),
        }
    }

    fn load_shelf<F, Fut>(
        self: &Rc<Self>,
        key: String,
        page_gen: u64,
        shelf: Shelf,
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
        self.set_shelf_status(shelf, if has_cache { "" } else { "loading" });
        if let Some(cards) = cached_cards {
            self.present_shelf(shelf, cards);
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
                Err(error) => tracing::warn!("catalog shelf load failed for {key}: {error}"),
            }
            on_ui(move |ui| {
                if ui.state.borrow().page_gen != page_gen {
                    return;
                }
                if let Some(cards) = changed {
                    ui.present_shelf(shelf, cards);
                }
                ui.set_shelf_status(
                    shelf,
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

    /// Share, Credits, Edit and Private / Public on an album, playlist or mix page.
    fn page_action(self: &Rc<Self>, action: &str, _arg: &str) {
        let Some(card) = self.page_card() else { return };
        match action {
            "copy-link" => self.copy_card_link(&card),
            "edit" | "make-public" | "make-private" if card.kind == "playlist" => {
                self.playlist_action(&card, action, "");
            }
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
        if row.kind == "track" {
            self.open_track_menu(&row.id, x, y);
            return;
        }
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
        app.set_menu_pinned(self.pins().contains(&pin_key(&card.kind, &card.id)));
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
            "pin" => self.toggle_pin(&card),
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
            "edit" | "make-public" | "make-private" | "move-to-folder" | "delete"
                if card.kind == "playlist" =>
            {
                self.playlist_action(&card, action, arg);
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
                            let text = match (card.kind.as_str(), favorite) {
                                ("playlist", true) => format!("Following “{}”", card.title),
                                ("playlist", false) => format!("Unfollowed “{}”", card.title),
                                (_, true) => "Added to My Collection".to_string(),
                                (_, false) => "Removed from My Collection".to_string(),
                            };
                            crate::alerts::notice(&ui.app(), &text);
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
                self.play_queue(
                    tracks,
                    0,
                    &card.title,
                    Some(false),
                    Some(Page::Detail(card.clone())),
                );
            }
            "play-next" if self.current_track().is_none() => self.play_queue(
                tracks,
                0,
                &card.title,
                Some(false),
                Some(Page::Detail(card.clone())),
            ),
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
        let name = self
            .state
            .borrow()
            .sidebar_cards
            .iter()
            .find(|card| card.kind == "playlist" && card.id == playlist)
            .map(|card| card.title.clone())
            .filter(|name| !name.is_empty());
        let target = name.map_or("the playlist".to_string(), |name| format!("“{name}”"));
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else { return };
            match api.add_to_playlist(&playlist, &ids).await {
                Ok(()) => on_ui(move |ui| {
                    crate::alerts::notice(&ui.app(), &format!("Added “{title}” to {target}"));
                    ui.remember_playlist_add(&playlist);
                    ui.sync_playlists();
                }),
                Err(e) => tracing::error!("Could not add “{title}” to {target}: {e}"),
            }
        });
    }

    /// The playlist just added to goes to the top of "Add to playlist", now
    /// and after a restart; TIDAL's own date for it arrives with the next sync.
    fn remember_playlist_add(self: &Rc<Self>, playlist: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let json = {
            let mut st = self.state.borrow_mut();
            st.playlist_adds.insert(playlist.to_string(), now);
            // Only the newest few can ever be listed.
            if st.playlist_adds.len() > 4 * RECENT_PLAYLISTS {
                let mut times: Vec<i64> = st.playlist_adds.values().copied().collect();
                times.sort_unstable_by_key(|&t| std::cmp::Reverse(t));
                let oldest_kept = times[4 * RECENT_PLAYLISTS - 1];
                st.playlist_adds.retain(|_, t| *t >= oldest_kept);
            }
            serde_json::to_string(&st.playlist_adds).unwrap_or_default()
        };
        if let Err(e) = self.ctx.meta.put_json("playlist-adds", &json) {
            tracing::warn!("could not save the recent playlists: {e}");
        }
        self.present_add_targets();
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
            models.extend(st.search_lists.values().flat_map(|l| l.models.iter().cloned()));
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
        restamp_model(&app.get_search_playlists(), &collection);
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
            models.extend(st.search_lists.values().flat_map(|l| l.models.iter().cloned()));
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
        patch(&app.get_search_playlists());
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
                    ui.show_sections(key, page_gen, sections);
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
        let source = key.to_string();
        self.load_catalog(key.into(), page_gen, fetch, move |ui, sections| {
            ui.app().set_loading(false);
            ui.show_sections(&source, page_gen, sections);
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

    /// Put a grid's Enter pick on its first playable card, or with `keep`, leave
    /// it on the card it is on while that card is still shown (a server refresh).
    fn reset_grid_pick(&self, rows: &[CardData], keep: bool) {
        let app = self.app();
        let current = app.get_grid_pick_id();
        let kept = keep
            .then(|| rows.iter().position(|row| row.id == current && !current.is_empty()))
            .flatten();
        let at = kept.unwrap_or_else(|| first_playable(rows));
        app.set_pick_index(at as i32);
        app.set_grid_pick_id(rows.get(at).map(|row| row.id.clone()).unwrap_or_default());
    }

    /// Enter in a grid's filter: play the picked playlist, album or artist, or
    /// open a picked folder. Already playing stays playing.
    fn play_grid_pick(self: &Rc<Self>) {
        let app = self.app();
        let picked = app.get_grid_pick_id();
        let cards = app.get_grid_cards();
        let Some(row) = (0..cards.row_count())
            .filter_map(|i| cards.row_data(i))
            .find(|row| row.id == picked && !picked.is_empty())
        else {
            return;
        };
        if matches!(row.kind.as_str(), "folder" | "page") {
            self.open_card(row);
            return;
        }
        let current = self.state.borrow().playing_card.clone();
        let same = current.is_some_and(|(kind, id)| kind == row.kind.as_str() && id == row.id.as_str());
        if same && app.get_playing() {
            return;
        }
        self.play_card(row);
    }

    fn play_card(self: &Rc<Self>, row: CardData) {
        if row.id.is_empty() {
            return;
        }
        match row.kind.as_str() {
            "folder" | "page" => {}
            "track" => self.play_track_card(&row.id, true),
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
                self.play_queue(favs, 0, "My Tracks", Some(false), Some(Page::Tracks));
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

    /// A track shown as a slim item: play it with the section it sits in as the
    /// queue. Already the current track: `toggle` pauses / resumes it (its play
    /// button), otherwise it only resumes (clicking the row again).
    fn play_track_card(self: &Rc<Self>, id: &str, toggle: bool) {
        let app = self.app();
        if app.get_current_id() == id {
            if toggle || !app.get_playing() {
                self.toggle_play();
            }
            return;
        }
        let section = self
            .state
            .borrow()
            .sections
            .iter()
            .position(|s| s.tracks.iter().any(|t| t.id == id));
        match section {
            Some(index) => self.play_from_section(index, id),
            None => self.play_from_page(id),
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

    /// A refresh of the page `page_gen` went out: fill the toolbar wave.
    fn sync_begin(&self, page_gen: u64) {
        let mut st = self.state.borrow_mut();
        if st.page_gen != page_gen {
            return;
        }
        if st.sync.page_gen != page_gen {
            st.sync = SyncState {
                page_gen,
                ..Default::default()
            };
        }
        st.sync.running += 1;
        st.sync.since.get_or_insert_with(Instant::now);
        drop(st);
        self.app().set_syncing(true);
    }

    /// A refresh of the page `page_gen` came back. The wave settles once the last
    /// one has, after it has shown for a moment.
    fn sync_end(self: &Rc<Self>, page_gen: u64) {
        let rest = {
            let mut st = self.state.borrow_mut();
            if st.sync.page_gen != page_gen || st.sync.running == 0 {
                return;
            }
            st.sync.running -= 1;
            if st.sync.running > 0 {
                return;
            }
            let shown = st.sync.since.take().map_or(SYNC_MIN_SHOWN, |t| t.elapsed());
            SYNC_MIN_SHOWN.saturating_sub(shown)
        };
        if rest.is_zero() {
            self.app().set_syncing(false);
            return;
        }
        let ui = Rc::clone(self);
        slint::Timer::single_shot(rest, move || {
            let st = ui.state.borrow();
            if st.sync.page_gen == page_gen && st.sync.running == 0 {
                drop(st);
                ui.app().set_syncing(false);
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
        self.sync_begin(page_gen);
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
                ui.sync_end(page_gen);
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
        let pins = if pinnable_grid(kind) {
            self.pins()
        } else {
            HashSet::new()
        };
        self.state.borrow_mut().grid = Grid {
            kind: kind.into(),
            sort,
            group,
            combine_others: combine,
            pins,
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
        self.sync_begin(page_gen);
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else {
                on_ui(move |ui| ui.sync_end(page_gen));
                return;
            };
            match fetch(api).await {
                Ok(cards) => {
                    let json = serde_json::to_string(&cards).unwrap_or_default();
                    let changed = ctx.meta.get_json(&cache_key).as_deref() != Some(json.as_str());
                    let _ = ctx.meta.put_json(&cache_key, &json);
                    on_ui(move |ui| {
                        if ui.state.borrow().page_gen == page_gen {
                            ui.app().set_loading(false);
                            if changed || cached_len == 0 {
                                ui.update_grid(&cards);
                            }
                        }
                        ui.sync_end(page_gen);
                    });
                }
                Err(e) => {
                    tracing::warn!("grid load failed: {e}");
                    on_ui(move |ui| {
                        if ui.state.borrow().page_gen == page_gen {
                            ui.app().set_loading(false);
                        }
                        ui.sync_end(page_gen);
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
        self.sync_begin(page_gen);
        self.spawn(move |ctx| async move {
            let Some(api) = ctx.api() else {
                on_ui(move |ui| {
                    if ui.state.borrow().page_gen == page_gen {
                        ui.app().set_loading(false);
                    }
                    ui.sync_end(page_gen);
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
                                ui.update_tracks(tracks, &context);
                            }
                        }
                        ui.sync_end(page_gen);
                    });
                }
                Err(e) => {
                    tracing::warn!("track list failed: {e}");
                    on_ui(move |ui| {
                        if ui.state.borrow().page_gen == page_gen {
                            ui.app().set_loading(false);
                            ui.app().set_catalog_error(!had_cached_tracks);
                        }
                        ui.sync_end(page_gen);
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
            self.play_queue(vec![track], 0, "", None, None);
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
        self.refresh_np_queue();
        self.refresh_queue_page();
    }

    fn refresh_np_queue(self: &Rc<Self>) {
        let (model, keep) = {
            let mut st = self.state.borrow_mut();
            let upcoming = st.upcoming_tracks();
            let rows: Vec<TrackData> = upcoming
                .iter()
                .map(|t| track_row(t, st.fav_ids.contains(&t.id)))
                .collect();
            let model = Rc::new(VecModel::from(rows));
            st.np_queue_model = Some(model.clone());
            let keep = st.art_keep.get("np-queue").copied();
            (model, keep)
        };
        self.app().set_np_queue(ModelRc::from(model.clone()));
        if let Some((from, upto)) = keep
            && upto > from
        {
            self.keep_track_covers("np-queue", &model, from, upto, 96);
        } else if self.app().get_show_now_playing() && self.app().get_show_queue() {
            self.np_queue_visible(0, 16);
        }
    }

    fn np_queue_visible(self: &Rc<Self>, first: usize, count: usize) {
        let Some(model) = self.state.borrow().np_queue_model.clone() else {
            return;
        };
        if count == 0 {
            self.keep_track_covers("np-queue", &model, 0, 0, 96);
            return;
        }
        let len = model.row_count();
        let start = first.saturating_sub(count);
        let end = (first + count + count).min(len);
        self.keep_track_covers("np-queue", &model, start, end, 96);
    }

    fn play_queue_index(self: &Rc<Self>, index: i32) {
        let Ok(offset) = usize::try_from(index) else {
            return;
        };
        let jumped = {
            let mut st = self.state.borrow_mut();
            let target = st.pos.saturating_add(1).saturating_add(offset);
            if target < st.order.len() {
                st.pos = target;
                st.pending_seek = None;
                st.prefetched_for = None;
                true
            } else {
                false
            }
        };
        if !jumped {
            return;
        }
        self.persist_cursor();
        self.start_current();
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
            self.play_queue(
                tracks,
                index,
                &context,
                None,
                self.playback_source_for_page(),
            );
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
            self.play_queue(tracks, index, &title, None, self.playback_source_for_page());
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
        self.play_queue(
            tracks,
            start,
            &context,
            Some(shuffle),
            self.playback_source_for_page(),
        );
    }

    fn playback_source_for_page(&self) -> Option<Page> {
        match self.current_page() {
            Some(Page::Queue) => self.state.borrow().playback_source.clone(),
            page => page,
        }
    }

    fn open_playing_album(self: &Rc<Self>) {
        let Some(track) = self.current_track() else {
            return;
        };
        let Some(id) = track.album_id.filter(|id| !id.is_empty()) else {
            return;
        };
        let card = Card {
            kind: "album".into(),
            id,
            title: track.album,
            subtitle: track.artist,
            image_url: track.cover_url,
            ..Default::default()
        };
        self.show_from_player(Page::Detail(card), Some(track.id));
    }

    fn open_playing_source(self: &Rc<Self>) {
        if self.video_active() {
            self.show_from_player(Page::Videos, None);
            return;
        }
        let Some(track) = self.current_track() else {
            return;
        };
        let page = self.state.borrow().playback_page();
        self.show_from_player(page, Some(track.id));
    }

    /// Reuse the normal navigation wait/fade, while Now Playing collapses.
    fn show_from_player(self: &Rc<Self>, page: Page, track: Option<String>) {
        self.nav_finish();
        self.show(page, true);
        if let Some(track) = track {
            let generation = self.state.borrow().page_gen;
            self.state.borrow_mut().reveal_track = Some((generation, track));
            self.reveal_playing_track();
        }
        let app = self.app();
        app.set_nav_has_under(false);
        app.set_nav_has_over(false);
        app.set_nav_under(Image::default());
        app.set_nav_over(Image::default());
        app.set_nav_reveal(1.0);
        app.set_nav_fade(1.0);
        app.set_nav_active(true);
        let weak = Rc::downgrade(self);
        let mut nav = self.nav.borrow_mut();
        nav.phase = NavPhase::Waiting {
            since: Instant::now(),
        };
        nav.pending = false;
        nav.timer.start(
            slint::TimerMode::Repeated,
            crate::platform::frame_interval(),
            move || {
                if let Some(ui) = weak.upgrade() {
                    ui.nav_tick();
                }
            },
        );
    }

    fn reveal_playing_track(&self) {
        let Some(index) = self.state.borrow_mut().take_reveal_track() else {
            return;
        };
        let app = self.app();
        app.set_reveal_track_index(index as i32);
        app.set_reveal_track_revision(app.get_reveal_track_revision().wrapping_add(1));
    }

    fn play_queue(
        self: &Rc<Self>,
        tracks: Vec<TrackItem>,
        index: usize,
        context: &str,
        shuffle: Option<bool>,
        source: Option<Page>,
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
            st.playback_source = source;
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
        if let Ok(json) = serde_json::to_string(&self.state.borrow().playback_source) {
            let _ = self.ctx.meta.put_json("playback-source", &json);
        }
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
        app.set_np_album_id(track.album_id.as_deref().unwrap_or_default().into());
        app.set_np_explicit(track.explicit);
        app.set_np_favorite(favorite);
        app.set_duration_secs(track.duration_secs as i32);
        app.set_total(fmt_time(track.duration_secs as u64).into());
        app.set_elapsed("0:00".into());
        app.set_progress(0.0);
        app.set_loaded(0.0);
        self.animate_art(artwork::Slot::Now, track.video_cover.as_deref());
        let cover = track.cover_url.as_deref().map(|u| sized_cover(u, 640));
        let cached_cover = cover
            .as_deref()
            .and_then(|u| self.ctx.images.cached(&url_key(u, 0), u));

        if let Some(cover_url) = cover.clone() {
            let id = track.id.clone();
            let weak = Rc::downgrade(self);
            let cover_url_for_cache = cover_url.clone();
            self.load_image(&cover_url, 640, move |img, tint| {
                let Some(ui) = weak.upgrade() else { return };
                if ui.app().get_current_id() == id.as_str() {
                    ui.app().set_np_cover(img.clone());
                    ui.app().set_np_tint(tint);
                    ui.app().set_np_backdrop(backdrop_of(img));
                    if let Some(path) =
                        ui.ctx.images.cached(&url_key(&cover_url_for_cache, 0), &cover_url_for_cache)
                    {
                        ui.ctx.with_media(|m| {
                            let _ = m.set_artwork_file(&path);
                        });
                    }
                }
            });
            self.load_large_cover(&track.id, &cover_url);
        } else {
            app.set_np_cover(Image::default());
            app.set_np_cover_large(Image::default());
            app.set_np_backdrop(Image::default());
        }
        if app.get_show_now_playing() {
            self.load_lyrics();
        }
        self.refresh_np_queue();
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
        self.dsp_present_upsample();
        let output = player.output_info();
        app.set_output_info(
            format!(
                "{} · {} · {}",
                output.name,
                output.kind.label(),
                crate::signal::output_format(&output)
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
            player.upsampling(),
            &crate::signal::Leveling {
                normalization: player.normalization(),
                loudness: player.loudness(),
                crossfade_secs: player.crossfade(),
            },
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
        self.ctx
            .desired_elapsed_ms
            .store(start_at.as_millis() as u64, Ordering::Relaxed);
        let duration = Duration::from_secs(track.duration_secs as u64);
        let quality = self.ctx.audio_quality_key();
        self.ctx
            .audio
            .request_time(&track.id, &quality, start_at, duration);
        {
            let mut st = self.state.borrow_mut();
            st.loading_track = true;
            st.prefetched_for = None;
            st.pending_seek = (!start_at.is_zero()).then_some(start_at);
            st.playable_gen = 0;
        }
        self.present_current();
        let app = self.app();
        let total_secs = track.duration_secs.max(1) as f32;
        let clamped = start_at.min(duration);
        let progress = (clamped.as_secs_f32() / total_secs).clamp(0.0, 1.0);
        if !start_at.is_zero() {
            app.set_elapsed(fmt_time(clamped.as_secs()).into());
            app.set_progress(progress);
        }
        if in_prefetch_zone(progress) {
            self.prefetch_next();
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
                    let quality = ctx.audio_quality_key();
                    let wait_until = std::time::Instant::now() + Duration::from_secs(120);
                    // A seek during buffering may have moved the download; start
                    // once the playable file holds the block under the playhead.
                    while ctx.play_gen.load(Ordering::SeqCst) == generation
                        && ctx.audio.playable_index(&track.id, &quality) != 0
                        && !ctx.audio.requested_ready(&track.id, &quality)
                        && std::time::Instant::now() < wait_until
                    {
                        tokio::time::sleep(Duration::from_millis(15)).await;
                    }
                    if ctx.play_gen.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    let start_at =
                        Duration::from_millis(ctx.desired_elapsed_ms.load(Ordering::Relaxed));
                    let loudness = ctx.loudness(&track.id).await;
                    if ctx.play_gen.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    let file_path = ctx
                        .audio
                        .get_playable(&track.id, &quality)
                        .unwrap_or(path);
                    let item = TrackQueueItem {
                        id: track.id.clone(),
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        file_path,
                        duration: Duration::from_secs(track.duration_secs as u64),
                        loudness,
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
                let desired =
                    Duration::from_millis(ui.ctx.desired_elapsed_ms.load(Ordering::Relaxed));
                let elapsed = ui.ctx.player.elapsed();
                let seek_moved = result.is_ok()
                    && desired.abs_diff(elapsed) > Duration::from_millis(400);
                {
                    let quality = ui.ctx.audio_quality_key();
                    let playable_gen = ui.ctx.audio.playable_generation(&track.id, &quality);
                    let mut st = ui.state.borrow_mut();
                    st.loading_track = false;
                    st.last_serial = ui.ctx.player.track_serial();
                    st.pending_seek = seek_moved.then_some(desired);
                    st.playable_gen = playable_gen;
                }
                let total_buffering_ms = t_buffering_start.elapsed().as_millis();
                let app = ui.app();
                app.set_buffering(seek_moved);
                match result {
                    Ok(()) => {
                        tracing::info!(track_id = %track.id, total_buffering_ms, "Buffering complete, playback starting");
                        app.set_playing(true);
                        ui.present_format();
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
        let duration = Duration::from_secs(track.duration_secs as u64);
        let to = Duration::from_secs_f32(track.duration_secs as f32 * clamped);
        self.ctx
            .desired_elapsed_ms
            .store(to.as_millis() as u64, Ordering::Relaxed);
        self.state.borrow_mut().pending_seek = Some(to);
        let quality = self.ctx.audio_quality_key();
        self.ctx
            .audio
            .request_time(&track.id, &quality, to, duration);
        let app = self.app();
        app.set_progress(clamped);
        app.set_elapsed(fmt_time(to.as_secs()).into());
        let playing = self.ctx.player.is_playing();
        self.ctx.with_media(|m| {
            let _ = m.set_playback_state(playing, Some(to));
        });
        self.persist_cursor();
        if in_prefetch_zone(clamped) {
            self.prefetch_next();
        }
        if self.ctx.player.seek(to) {
            self.state.borrow_mut().pending_seek = None;
            app.set_buffering(false);
            return;
        }
        app.set_buffering(true);
        if !self.state.borrow().loading_track {
            self.try_reopen_at(to);
        }
    }

    /// Reopen the current playable file at `to` once the download has rebuilt
    /// the DASH head for that time.
    fn try_reopen_at(self: &Rc<Self>, to: Duration) {
        let Some(track) = self.current_track() else {
            return;
        };
        let quality = self.ctx.audio_quality_key();
        let playable_gen = self.ctx.audio.playable_generation(&track.id, &quality);
        if !self.ctx.audio.requested_ready(&track.id, &quality) {
            return;
        }
        if playable_gen == self.state.borrow().playable_gen {
            return;
        }
        let Some(path) = self.ctx.audio.get_playable(&track.id, &quality) else {
            return;
        };
        self.state.borrow_mut().playable_gen = playable_gen;
        let item = TrackQueueItem {
            id: track.id.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            file_path: path,
            duration: Duration::from_secs(track.duration_secs as u64),
            loudness: self.ctx.known_loudness(&track.id).flatten(),
        };
        self.spawn(move |ctx| async move {
            let play = tokio::task::spawn_blocking(move || ctx.player.play_track_at(item, to))
                .await
                .unwrap_or_else(|e| Err(tide_audio::AudioError::Decoder(e.to_string())));
            on_ui(move |ui| {
                if play.is_err() {
                    ui.state.borrow_mut().playable_gen = 0;
                    return;
                }
                ui.state.borrow_mut().pending_seek = None;
                ui.app().set_buffering(false);
                ui.app().set_playing(ui.ctx.player.is_playing());
                ui.present_format();
                let elapsed = ui.ctx.player.elapsed();
                ui.ctx.with_media(|m| {
                    let _ = m.set_playback_state(ui.ctx.player.is_playing(), Some(elapsed));
                });
                ui.persist_cursor();
            });
        });
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
        self.refresh_np_queue();
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
        #[cfg(target_os = "macos")]
        {
            self.restore_window();
            crate::window_state::remember(self.win.window(), &self.ctx);
        }
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
        // Copy out before acting: `try_reopen_at` and the seek below borrow
        // `state` mutably, and a `Ref` held across them panics (and aborts,
        // since this runs inside an AppKit callback).
        let pending_seek = self.state.borrow().pending_seek;
        if player.current_id().is_none() {
            // Restored session before audio has been opened: keep the saved elapsed/scrubber untouched.
            if let Some(pending) = pending_seek {
                self.try_reopen_at(pending);
            }
            return;
        }

        if let Some(pending) = pending_seek {
            let ready = self.current_track().is_some_and(|track| {
                let quality = self.ctx.audio_quality_key();
                self.ctx.audio.requested_ready(&track.id, &quality)
            });
            if ready && self.ctx.player.seek(pending) {
                self.state.borrow_mut().pending_seek = None;
                app.set_buffering(false);
            } else {
                self.try_reopen_at(pending);
                return;
            }
        }

        let Some(track) = self.current_track() else {
            return;
        };
        let elapsed = player.elapsed();
        let total = Duration::from_secs(track.duration_secs.max(1) as u64);
        app.set_playing(player.is_playing());
        app.set_elapsed(fmt_time(elapsed.as_secs().min(total.as_secs())).into());
        let progress = (elapsed.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);
        app.set_progress(progress);
        if player.is_playing() && save_tick {
            self.persist_cursor();
        }

        if in_prefetch_zone(progress) {
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
                    let loudness = ctx.loudness(&track.id).await;
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
                        loudness,
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
        let playing = if self.video_active() {
            self.app().get_playing()
        } else {
            self.ctx.player.is_playing()
        };
        match event {
            MediaControlEvent::Toggle => self.toggle_play(),
            MediaControlEvent::Play if !playing => self.toggle_play(),
            MediaControlEvent::Pause | MediaControlEvent::Stop if playing => self.toggle_play(),
            MediaControlEvent::Next => self.skip(1, true),
            MediaControlEvent::Previous => self.previous(),
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
            (st.tracks_model.clone(), st.np_queue_model.clone())
        };
        for model in [model.0, model.1].into_iter().flatten() {
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
            st.lyric_row = None;
        }
        let app = self.app();
        app.set_lyrics(ModelRc::default());
        app.set_lyric_index(-1);
        app.set_lyrics_synced(false);
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
                let rows = lyric_rows(&lyrics);
                let app = ui.app();
                app.set_lyrics_status(
                    if rows.is_empty() {
                        "No lyrics available for this track"
                    } else {
                        ""
                    }
                    .into(),
                );
                app.set_lyrics_synced(!lyrics.synced.is_empty());
                app.set_lyrics(ModelRc::from(Rc::new(VecModel::from(rows))));
                ui.state.borrow_mut().lyrics = lyrics;
                ui.update_lyric_line(ui.effective_elapsed());
            });
        });
    }

    /// Mark the line being sung. The column holds every line; only the index
    /// changes, and only when another line starts.
    fn update_lyric_line(&self, elapsed: Duration) {
        let row = {
            let mut st = self.state.borrow_mut();
            if st.lyrics.synced.is_empty() {
                return;
            }
            let row = sung_row(&st.lyrics.synced, elapsed.as_millis() as u64);
            if st.lyric_row == Some(row) {
                return;
            }
            st.lyric_row = Some(row);
            row
        };
        self.app().set_lyric_index(row);
    }

    /// Seek to where line `row` of `App.lyrics` starts.
    fn seek_lyric(self: &Rc<Self>, row: i32) {
        let Some(line) = usize::try_from(row)
            .ok()
            .and_then(|row| self.app().get_lyrics().row_data(row))
        else {
            return;
        };
        let Some(track) = self.current_track() else {
            return;
        };
        if line.time < 0 || track.duration_secs == 0 {
            return;
        }
        let at = Duration::from_millis(line.time as u64);
        self.seek(at.as_secs_f32() / track.duration_secs as f32);
        self.update_lyric_line(at);
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
        self.animate_art(artwork::Slot::Now, None);
        app.set_duration_secs(0);
        app.set_progress(0.0);
        app.set_elapsed("0:00".into());
        app.set_total("0:00".into());
        app.set_playing(false);
        app.set_current_id("".into());
        app.set_user_code("".into());
        app.set_auth_status("".into());
        self.detect_login_browser();
        app.set_sections(ModelRc::default());
        app.set_tracks(ModelRc::default());
        app.set_np_queue(ModelRc::default());
        app.set_show_queue(false);
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
        type StepFn = Box<dyn Fn(&Rc<Ui>)>;
        type Step = (u64, &'static str, StepFn);
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
                    if ui.swipe_begin(true, true) {
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
        ];
        // View ▸ Zoom: the same callbacks the menu items and ⌘= / ⌘− / ⌘0 fire.
        // Restore the saved preference afterwards so the tour leaves no trace.
        let saved_zoom = Rc::new(std::cell::Cell::new(100));
        let report = |label: &'static str| -> StepFn {
            Box::new(move |ui| {
                println!(
                    "zoom {label}: {}% scale={} compact={}",
                    ui.app().get_zoom_percent(),
                    ui.win.window().scale_factor(),
                    ui.app().get_compact_layout()
                )
            })
        };
        steps.push((200, "", {
            let saved = saved_zoom.clone();
            Box::new(move |ui| {
                saved.set(ui.ctx.settings.read().unwrap().zoom_percent);
                ui.app().invoke_reset_zoom();
                ui.navigate_key("albums");
            })
        }));
        steps.push((3000, "09b-zoom-100", report("100")));
        for _ in 0..4 {
            steps.push((150, "", Box::new(|ui| ui.app().invoke_zoom_in())));
        }
        steps.push((1500, "09c-zoom-200", report("200")));
        // Past the top it stays at 200%.
        steps.push((150, "", Box::new(|ui| ui.app().invoke_zoom_in())));
        steps.push((800, "", report("200 clamp")));
        for _ in 0..6 {
            steps.push((150, "", Box::new(|ui| ui.app().invoke_zoom_out())));
        }
        steps.push((1500, "09d-zoom-50", report("50")));
        steps.push((150, "", Box::new(|ui| ui.app().invoke_zoom_out())));
        steps.push((800, "", report("50 clamp")));
        steps.push((200, "", Box::new(|ui| ui.app().invoke_reset_zoom())));
        steps.push((1500, "09e-zoom-reset", report("reset")));
        steps.push((200, "", {
            let saved = saved_zoom.clone();
            Box::new(move |ui| {
                ui.set_zoom(saved.get());
                ui.navigate_key("tracks");
            })
        }));
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
        // `TIDE_TOUR=now-playing` / `scroll`: only time those animations.
        match std::env::var("TIDE_TOUR").as_deref() {
            Ok("now-playing") => steps = now_playing_frame_tour(),
            Ok("scroll") => steps = scroll_frame_tour(),
            _ => {}
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

/// Toggles Now Playing 16 times through the same path as the artwork click,
/// printing a `MARK` with the wall time of each toggle. Run with
/// `TIDE_FRAME_LOG=1` so the renderer prints every frame's timing too.
#[allow(clippy::type_complexity)]
fn now_playing_frame_tour() -> Vec<(u64, &'static str, Box<dyn Fn(&Rc<Ui>)>)> {
    let mut steps: Vec<(u64, &'static str, Box<dyn Fn(&Rc<Ui>)>)> = Vec::new();
    steps.push((
        7000,
        "np-00-ready",
        // AppKit only draws a visible window, and the tour never raises it
        // over other apps: keep the TIDE window in view while it runs.
        Box::new(|ui| println!("frame-tour has_track={}", ui.app().get_has_track())),
    ));
    for i in 0..16 {
        steps.push((
            700,
            // Taken as the second toggle starts collapsing: still fully open.
            if i == 1 { "np-01-expanded" } else { "" },
            Box::new(|ui| {
                ui.toggle_now_playing();
                let wall = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1e3;
                let dir = if ui.app().get_show_now_playing() {
                    "open"
                } else {
                    "close"
                };
                eprintln!(
                    "MARK {dir} wall={wall:.2} fancy={} gpu_cache={}MB",
                    ui.fancy_animation(),
                    crate::platform::gpu_cache_bytes() / (1024 * 1024)
                );
            }),
        ));
        if i == 2 {
            // Mid-expand: the fading parts must be drawn from their cached layers.
            steps.push((110, "np-02-mid-expand", Box::new(|_| {})));
        }
    }
    steps
}

fn tour_wall_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1e3
}

/// Scrolls each heavy page down and back up with in-process scroll events
/// every 4 ms (faster than any panel, so input never limits the frame rate),
/// printing `MARK scroll-<page> start|end`. Run with `TIDE_FRAME_LOG=1`.
#[allow(clippy::type_complexity)]
fn scroll_frame_tour() -> Vec<(u64, &'static str, Box<dyn Fn(&Rc<Ui>)>)> {
    let scroller: Rc<RefCell<Option<slint::Timer>>> = Rc::default();
    let mut steps: Vec<(u64, &'static str, Box<dyn Fn(&Rc<Ui>)>)> = Vec::new();
    steps.push((
        7000,
        "scroll-00-ready",
        // Never raised over other apps; see `now_playing_frame_tour`.
        Box::new(|_| {}),
    ));
    let pages: [(&'static str, &'static str); 5] = [
        ("home", "home"),
        ("tracks", "tracks"),
        ("albums", "albums"),
        ("playlists", "playlists"),
        ("playlist-detail", ""),
    ];
    for (name, key) in pages {
        steps.push((
            300,
            "",
            Box::new(move |ui| {
                if key.is_empty() {
                    let cards = ui.app().get_grid_cards();
                    if let Some(card) = cards.iter().find(|c| c.kind == "playlist") {
                        ui.open_card(card);
                    }
                } else {
                    ui.navigate_key(key);
                }
            }),
        ));
        // Down, then back up.
        for (direction, delay) in [(-6.0f32, 4000u64), (6.0, 200)] {
            let start = scroller.clone();
            steps.push((
                delay,
                "",
                Box::new(move |ui| {
                    let weak = Rc::downgrade(ui);
                    let timer = slint::Timer::default();
                    timer.start(
                        slint::TimerMode::Repeated,
                        crate::platform::frame_interval(),
                        move || {
                            let Some(ui) = weak.upgrade() else { return };
                            let win = ui.win.window();
                            let size = win.size().to_logical(win.scale_factor());
                            let app = ui.app();
                            let position = slint::LogicalPosition::new(
                                app.get_pane_x() + app.get_pane_width() / 2.0,
                                (size.height / 2.0).max(app.get_pane_y() + 100.0),
                            );
                            win.dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                                position,
                                delta_x: 0.0,
                                delta_y: direction,
                            });
                        },
                    );
                    *start.borrow_mut() = Some(timer);
                    let dir = if direction < 0.0 { "down" } else { "up" };
                    eprintln!("MARK scroll-{name}-{dir} start wall={:.2}", tour_wall_ms());
                }),
            ));
            let stop = scroller.clone();
            steps.push((
                1500,
                "",
                Box::new(move |ui| {
                    stop.borrow_mut().take();
                    let dir = if direction < 0.0 { "down" } else { "up" };
                    eprintln!(
                        "MARK scroll-{name}-{dir} end wall={:.2} view={} scroll={} fancy={} gpu_cache={}MB",
                        tour_wall_ms(),
                        ui.app().get_view(),
                        ui.app().get_scroll_y(),
                        ui.fancy_animation(),
                        crate::platform::gpu_cache_bytes() / (1024 * 1024),
                    );
                }),
            ));
        }
    }
    steps
}
