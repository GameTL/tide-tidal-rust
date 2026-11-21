//! Every main page at phone, in-between and wide window widths, drawn for real
//! by the patched Skia renderer into memory (no window on screen), the phone
//! dock glass and opaque. Checks that nothing reaches past the window's right
//! edge; set `TIDE_PHONE_SHOTS=<dir>` to keep the frames as PNGs.
#![cfg(target_os = "macos")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use i_slint_renderer_skia::{SkiaRenderer, SkiaSharedContext};
use slint::platform::{Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalSize, ModelRc, PhysicalSize, Rgba8Pixel, SharedPixelBuffer, VecModel};

slint::include_modules!();

const SCALE: f32 = 2.;

struct Adapter {
    window: slint::Window,
    renderer: SkiaRenderer,
    size: Cell<PhysicalSize>,
}

impl WindowAdapter for Adapter {
    fn window(&self) -> &slint::Window {
        &self.window
    }
    fn size(&self) -> PhysicalSize {
        self.size.get()
    }
    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }
}

thread_local! {
    static ADAPTER: RefCell<Option<Rc<Adapter>>> = const { RefCell::new(None) };
}

struct Offscreen(std::time::Instant);

impl Platform for Offscreen {
    fn duration_since_start(&self) -> std::time::Duration {
        self.0.elapsed()
    }

    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let adapter: Rc<Adapter> = Rc::new_cyclic(|weak: &std::rc::Weak<Adapter>| Adapter {
            window: slint::Window::new(weak.clone()),
            renderer: SkiaRenderer::default(&SkiaSharedContext::default()),
            size: Cell::new(PhysicalSize::new(2880, 1800)),
        });
        ADAPTER.with(|a| *a.borrow_mut() = Some(adapter.clone()));
        Ok(adapter)
    }
}

fn resize(ui: &MainWindow, w: f32, h: f32) {
    ADAPTER.with(|a| {
        a.borrow().as_ref().unwrap().size.set(PhysicalSize::new((w * SCALE) as u32, (h * SCALE) as u32))
    });
    ui.window().dispatch_event(WindowEvent::Resized { size: LogicalSize::new(w, h) });
}

type Shot = SharedPixelBuffer<Rgba8Pixel>;

fn settle() {
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(25));
        slint::platform::update_timers_and_animations();
    }
}

fn shoot(ui: &MainWindow, name: &str) -> Shot {
    settle();
    let shot = ui.window().take_snapshot().expect("snapshot");
    if let Some(dir) = std::env::var_os("TIDE_PHONE_SHOTS") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        image::save_buffer(
            dir.join(format!("{name}.png")),
            shot.as_bytes(),
            shot.width(),
            shot.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
    shot
}

fn cover(seed: usize) -> slint::Image {
    let palette = [[226, 70, 60], [40, 150, 230], [240, 200, 40], [120, 70, 200], [40, 190, 120], [230, 120, 40]];
    let c = palette[seed % palette.len()];
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(64, 64);
    for (i, p) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % 64) as f32 / 64., (i / 64) as f32 / 64.);
        let k = 0.55 + 0.45 * (1. - (x * 0.6 + y * 0.4));
        *p = Rgba8Pixel { r: (c[0] as f32 * k) as u8, g: (c[1] as f32 * k) as u8, b: (c[2] as f32 * k) as u8, a: 255 };
    }
    slint::Image::from_rgba8(buffer)
}

fn card(i: usize, kind: &str, title: &str, subtitle: &str) -> CardData {
    CardData {
        id: format!("{kind}{i}").into(),
        kind: kind.into(),
        title: title.into(),
        subtitle: subtitle.into(),
        image: cover(i),
        by: if kind == "playlist" { "Someone".into() } else { "".into() },
        quality: if kind == "album" && i.is_multiple_of(2) { "MAX".into() } else { "".into() },
        ..Default::default()
    }
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

const TITLES: [&str; 12] = [
    "Intro (MTV Unplugged) [Live]",
    "Stressed Out (MTV Unplugged) [Live]",
    "Tear in My Heart (MTV Unplugged) [Live]",
    "House Of Gold / Lane Boy (MTV Unplugged) [Live]",
    "Shy Away (MTV Unplugged) [Live]",
    "Ride / Nico And The Niners (MTV Unplugged) [Live]",
    "Car Radio / Heathens (MTV Unplugged) [Live]",
    "Paper Boats",
    "Streetlights",
    "Some of Them Always Will",
    "Counting Windows on the Hill",
    "Say We Were Here",
];

fn tracks(n: usize) -> ModelRc<TrackData> {
    model(
        (0..n)
            .map(|i| TrackData {
                id: format!("t{i}").into(),
                title: TITLES[i % TITLES.len()].into(),
                artist: "twenty one pilots".into(),
                artist_id: "ar1".into(),
                artists: model(vec![ArtistLink { id: "ar1".into(), name: "twenty one pilots".into() }]),
                album: "MTV Unplugged (Live)".into(),
                album_id: "al1".into(),
                duration: format!("{}:{:02}", 3 + i % 3, (i * 17) % 60).into(),
                date_added: "Apr 21, 2023".into(),
                bpm: "128".into(),
                explicit: i % 5 == 2,
                favorite: i % 4 == 0,
                cover: cover(i),
                ..Default::default()
            })
            .collect(),
    )
}

fn home_sections() -> ModelRc<SectionData> {
    let shortcuts = vec![
        CardData { id: "fav".into(), kind: "tracks".into(), title: "My Tracks".into(), ..Default::default() },
        card(1, "album", "Trench", "twenty one pilots"),
        card(2, "playlist", ".FuelForWork", "Created by me"),
        card(3, "album", "Clancy", "twenty one pilots"),
        card(4, "album", "Ride Lonesome", "Beck"),
        card(5, "playlist", "Reading…", "Created by me"),
    ];
    let history = (0..10)
        .map(|i| card(i, "mix", &format!("My Most Listened {i}"), "Harry Styles, Post Malone, Weezer and more"))
        .collect::<Vec<_>>();
    let recent = (0..6).map(|i| card(i + 3, "album", &format!("Recently played {i}"), "Album by Someone")).collect::<Vec<_>>();
    model(vec![
        SectionData { title: "".into(), layout: "shortcuts".into(), cards: model(shortcuts), tracks: model(vec![]) },
        SectionData { title: "Your listening history".into(), layout: "cards".into(), cards: model(history), tracks: model(vec![]) },
        SectionData { title: "Recently played".into(), layout: "rows".into(), cards: model(recent), tracks: tracks(4) },
    ])
}

fn feed_sections() -> ModelRc<SectionData> {
    let row = |n: usize, off: usize| {
        model(
            (0..n)
                .map(|i| card(i + off, "album", ["Take Away The Pain", "Infinite EP", "Butter", "Get Ready (From \"Snoopy Presents: To Mom (and Dad), With Love\")"][(i + off) % 4], "Single by Magic!"))
                .collect::<Vec<_>>(),
        )
    };
    model(vec![
        SectionData { title: "Last week".into(), layout: "rows".into(), cards: row(2, 0), tracks: model(vec![]) },
        SectionData { title: "This month".into(), layout: "rows".into(), cards: row(5, 2), tracks: model(vec![]) },
        SectionData { title: "Last month".into(), layout: "rows".into(), cards: row(6, 1), tracks: model(vec![]) },
    ])
}

fn explore_sections() -> ModelRc<SectionData> {
    let chips = |names: &[&str]| {
        model(names.iter().enumerate().map(|(i, n)| CardData { id: format!("p{i}").into(), kind: "page".into(), title: (*n).into(), ..Default::default() }).collect::<Vec<_>>())
    };
    let tiles = model(
        ["For DJs", "TIDAL Magazine", "Record Labels", "Party", "Workout", "Chill"]
            .iter()
            .enumerate()
            .map(|(i, n)| CardData { id: format!("m{i}").into(), kind: "page".into(), title: (*n).into(), tint: slint::Color::from_rgb_u8(40 + 30 * i as u8, 60, 90), ..Default::default() })
            .collect::<Vec<_>>(),
    );
    let featured = model((0..8).map(|i| card(i, "album", &format!("Featured release {i}"), "Phoebe Bridgers")).collect::<Vec<_>>());
    model(vec![
        SectionData { title: "Featured".into(), layout: "cards".into(), cards: featured, tracks: model(vec![]) },
        SectionData { title: "Genres".into(), layout: "links".into(), cards: chips(&["Hip-Hop", "Pop", "R&B / Soul", "Rock", "Electronic", "Jazz", "Classical", "Country"]), tracks: model(vec![]) },
        SectionData { title: "Moods & Activities".into(), layout: "tiles".into(), cards: tiles, tracks: model(vec![]) },
    ])
}

/// Everything the window holds, set as the controller would.
fn fill(app: &App) {
    app.set_logged_in(true);
    app.set_username("listener@example.com".into());
    app.set_has_track(true);
    app.set_playing(true);
    app.set_current_id("t1".into());
    app.set_np_title("Derezzed".into());
    app.set_np_artist("Daft Punk".into());
    app.set_np_artists(model(vec![ArtistLink { id: "dp".into(), name: "Daft Punk".into() }]));
    app.set_np_album_id("al9".into());
    app.set_np_context("TRON: Legacy".into());
    app.set_np_cover(cover(1));
    app.set_np_cover_large(cover(1));
    app.set_np_tint(slint::Color::from_rgb_u8(30, 80, 110));
    app.set_np_backdrop(cover(1));
    app.set_progress(0.37);
    app.set_elapsed("1:02".into());
    app.set_total("1:44".into());
    app.set_quality_label("MAX".into());
    app.set_signal_quality("max".into());
    app.set_lyrics_status("No lyrics".into());
    app.set_sidebar_playlists(model(vec![
        CardData { id: "f1".into(), kind: "folder".into(), title: "Work".into(), subtitle: "4 PLAYLISTS".into(), ..Default::default() },
        CardData { by: "Me".into(), subtitle: "98 TRACKS".into(), ..card(2, "playlist", ".FuelForWork", "") },
        CardData { by: "Me".into(), subtitle: "12 TRACKS".into(), ..card(3, "playlist", "📖 Reading…", "") },
        card(4, "playlist", "Staff Picks: Indie", "by TIDAL"),
        card(5, "playlist", "A playlist whose name goes on for rather a long time indeed", "by Someone"),
    ]));
    app.set_show_collection(true);
    app.set_sections(home_sections());
    app.set_grid_sort_options(model(vec!["Recently added".into(), "A-Z".into()]));
    app.set_grid_sort("Recently added".into());
    app.set_grid_group_options(model(vec!["None".into(), "Artist".into()]));
    app.set_grid_group("None".into());
    app.set_grid_can_create(true);
}

fn show(app: &App, page: &str) {
    app.set_scroll_y(0.);
    app.set_page_has_cover(false);
    app.set_page_card(CardData::default());
    app.set_page_can_favorite(false);
    app.set_page_subtitle("".into());
    app.set_page_description("".into());
    app.set_page_summary("".into());
    app.set_page_artists(model(vec![]));
    app.set_page_artist(CardData::default());
    match page {
        "home" => {
            app.set_view("home".into());
            app.set_nav_key("home".into());
            app.set_sections(home_sections());
        }
        "feed" => {
            app.set_view("feed".into());
            app.set_nav_key("feed".into());
            app.set_sections(feed_sections());
        }
        "explore" => {
            app.set_view("modules".into());
            app.set_nav_key("explore".into());
            app.set_page_title("Explore".into());
            app.set_sections(explore_sections());
        }
        "albums" => {
            app.set_view("grid".into());
            app.set_nav_key("albums".into());
            app.set_page_title("Albums".into());
            app.set_grid_kind("albums".into());
            let cards = model((0..16).map(|i| card(i, "album", &format!("Album with a title {i}"), "twenty one pilots")).collect::<Vec<_>>());
            app.set_grid_cards(cards.clone());
            app.set_grid_groups(model(vec![SectionData { cards, ..Default::default() }]));
        }
        "album" => {
            app.set_view("album".into());
            app.set_nav_key("".into());
            app.set_page_title("MTV Unplugged (Live)".into());
            app.set_page_has_cover(true);
            app.set_page_cover(cover(1));
            app.set_page_tint(slint::Color::from_rgb_u8(30, 80, 110));
            app.set_page_subtitle("twenty one pilots".into());
            app.set_page_artist(card(9, "artist", "twenty one pilots", ""));
            app.set_page_artists(model(vec![ArtistLink { id: "ar1".into(), name: "twenty one pilots".into() }]));
            app.set_page_year("2023".into());
            app.set_page_quality("MAX".into());
            app.set_page_summary("7 TRACKS  (34:23)".into());
            app.set_page_release_line("APRIL 21, 2023  ·  7 TRACKS  (34:23)".into());
            app.set_page_copyright("© 2023 FUELED BY RAMEN LLC".into());
            app.set_page_card(card(1, "album", "MTV Unplugged (Live)", "twenty one pilots"));
            app.set_page_can_favorite(true);
            app.set_page_favorite(true);
            app.set_tracks(tracks(7));
        }
        "playlist" => {
            app.set_view("detail".into());
            app.set_nav_key("".into());
            app.set_page_title(".FuelForWork".into());
            app.set_page_has_cover(true);
            app.set_page_cover(cover(2));
            app.set_page_description("Songs to keep the afternoon going, from the first coffee to the last commit of the day.".into());
            app.set_page_summary("98 TRACKS  (5 HR 12 MIN)".into());
            app.set_page_card(CardData { by: "Me".into(), is_public: true, ..card(2, "playlist", ".FuelForWork", "") });
            app.set_tracks_show_date(true);
            app.set_tracks(tracks(24));
        }
        "artist" => {
            app.set_view("artist".into());
            app.set_nav_key("".into());
            app.set_page_title("twenty one pilots".into());
            app.set_page_cover(cover(3));
            app.set_page_tint(slint::Color::from_rgb_u8(110, 60, 20));
            app.set_artist_tab(0);
            app.set_tracks(tracks(5));
            app.set_artist_albums(model((0..6).map(|i| card(i, "album", &format!("Album {i}"), "2021")).collect::<Vec<_>>()));
            app.set_page_biography("An American duo from Columbus, Ohio, formed in 2009.".into());
        }
        "search" => {
            app.set_view("search".into());
            app.set_nav_key("".into());
            app.set_search_text("pilots".into());
            app.set_search_shown("pilots".into());
            app.set_page_title("Results for “pilots”".into());
            app.set_search_tab(0);
            app.set_search_cards(model(vec![
                card(1, "artist", "twenty one pilots", "Artist"),
                card(2, "album", "Trench", "twenty one pilots"),
                card(3, "album", "Blurryface", "twenty one pilots"),
                card(4, "album", "Scaled And Icy", "twenty one pilots"),
            ]));
            app.set_search_playlists(model((0..4).map(|i| card(i, "playlist", &format!("Pilots playlist {i}"), "by TIDAL")).collect::<Vec<_>>()));
            app.set_tracks_show_date(false);
            app.set_tracks(tracks(8));
        }
        "tracks" => {
            app.set_view("tracks".into());
            app.set_nav_key("tracks".into());
            app.set_page_title("Tracks".into());
            app.set_tracks_show_date(true);
            app.set_tracks(tracks(30));
        }
        "queue" => {
            app.set_view("queue".into());
            app.set_nav_key("queue".into());
            app.set_page_title("Play queue".into());
            app.set_tracks_show_date(false);
            app.set_tracks(tracks(12));
        }
        "settings" => {
            app.set_view("settings".into());
            app.set_nav_key("".into());
            app.set_settings_tab(0);
            app.set_output_info("MacBook Pro Speakers · 48 kHz".into());
            app.set_cache_info("1.2 GB of 20 GB used, 312 tracks".into());
        }
        "dsp" => {
            app.set_view("dsp".into());
            app.set_nav_key("dsp".into());
            app.set_dsp_device("MacBook Pro Speakers".into());
        }
        "collection" => {
            app.set_view("collection".into());
            app.set_nav_key("collection".into());
            app.set_page_title("Collection".into());
        }
        _ => unreachable!(),
    }
}

/// Nothing is drawn in the window's right-most column but the page's own
/// background: a cell or a button that reached past the edge would be cut
/// there. (Rows of the window's chrome, top bar and dock, are left out.)
fn right_edge_clear(shot: &Shot, w: f32, h: f32, dock: f32) -> Option<f32> {
    let px = (w * SCALE) as u32 - 1;
    let top = ((40. + 64.) * SCALE) as u32;
    let bottom = ((h - dock) * SCALE) as u32;
    (top..bottom).step_by(2).find_map(|py| {
        let p = shot.as_slice()[(py * shot.width() + px) as usize];
        (p.r as u32 + p.g as u32 + p.b as u32 > 120).then_some(py as f32 / SCALE)
    })
}

const PAGES: [&str; 13] = [
    "home", "feed", "explore", "albums", "album", "playlist", "artist", "search", "tracks", "queue", "settings", "dsp",
    "collection",
];

#[test]
fn pages_render_at_every_width() {
    slint::platform::set_platform(Box::new(Offscreen(std::time::Instant::now()))).unwrap();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: SCALE });
    resize(&ui, 1440., 900.);
    fill(&app);

    let mut cut = Vec::new();
    for (w, h) in [(330., 760.), (480., 800.), (720., 800.), (960., 700.), (1440., 900.)] {
        resize(&ui, w, h);
        settle();
        assert_eq!(app.get_phone_layout(), w < 720., "phone layout at {w}");
        assert_eq!(app.get_compact_layout(), w < 960., "compact layout at {w}");
        for glass in [true, false] {
            // Glass and opaque both at the phone widths; one is enough wider up.
            if !glass && w > 480. {
                continue;
            }
            app.set_glass(glass);
            let g = if glass { "glass" } else { "plain" };
            for page in PAGES {
                show(&app, page);
                let shot = shoot(&ui, &format!("{w}-{page}-{g}"));
                // The album, artist and DSP pages are drawn edge to edge
                // (hero art, full-width wash): only the others are checked.
                if !matches!(page, "album" | "artist")
                    && let Some(y) = right_edge_clear(&shot, w, h, app.get_dock_height())
                {
                    cut.push(format!("{page} at {w}: something at the right edge, y {y}"));
                }
            }
        }
    }

    // The album page, scrolled to the track list; a context menu with a
    // flyout; the dialogs; the debug panel: at the narrowest window.
    resize(&ui, 330., 760.);
    app.set_glass(true);
    show(&app, "album");
    app.set_scroll_y(-420.);
    shoot(&ui, "330-album-scrolled");
    show(&app, "home");
    app.set_own_playlists(model((0..6).map(|i| CardData { by: "Me".into(), ..card(i, "playlist", &format!("My playlist {i}"), "") }).collect::<Vec<_>>()));
    app.set_menu_kind("track".into());
    app.set_menu_track(TrackData { title: TITLES[3].into(), artist: "twenty one pilots".into(), cover: cover(3), album_id: "al1".into(), artist_id: "ar1".into(), ..Default::default() });
    app.set_menu_x(250.);
    app.set_menu_y(300.);
    app.set_menu_open(true);
    // Point at "Add to playlist" (the menu's second entry) to open its flyout.
    settle();
    let mut y = 40. + 300.;
    while app.get_menu_sub() != "playlists" && y < 760. {
        ui.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(80., y) });
        slint::platform::update_timers_and_animations();
        y += 6.;
    }
    assert_eq!(app.get_menu_sub(), "playlists");
    shoot(&ui, "330-menu-flyout");
    app.set_menu_open(false);
    app.set_create_name("Work".into());
    app.set_create_kind("publish-playlist".into());
    shoot(&ui, "330-dialog-publish");
    app.set_create_kind("edit-playlist".into());
    app.set_edit_description("Songs for work.".into());
    app.set_edit_cover(cover(2));
    shoot(&ui, "330-dialog-edit");
    app.set_create_kind("".into());
    app.set_debug_open(true);
    shoot(&ui, "330-debug");
    app.set_debug_open(false);
    // Now Playing over the phone layout, and its minimized state (the art
    // back in the mini player's slot).
    app.set_show_now_playing(true);
    shoot(&ui, "330-now-playing");
    app.set_show_now_playing(false);
    shoot(&ui, "330-now-playing-closed");
    // A video in the mini player's slot.
    app.set_video_active(true);
    app.set_video_frame(cover(4));
    shoot(&ui, "330-video-slot");
    app.set_video_active(false);
    // No track: the dock is only the tab bar.
    app.set_has_track(false);
    shoot(&ui, "330-no-track");
    app.set_has_track(true);
    // The in-between widths of the player bar and the compact sidebar.
    for w in [760., 880., 1000., 1100.] {
        resize(&ui, w, 760.);
        show(&app, "album");
        shoot(&ui, &format!("{w}-album-glass"));
    }

    assert!(cut.is_empty(), "{}", cut.join("\n"));
}
