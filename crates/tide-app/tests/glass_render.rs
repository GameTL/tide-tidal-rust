//! The glass player bar, drawn for real: the window is rendered by the patched
//! Skia renderer into memory (no window on screen) and the pixels are checked.
//! Set `TIDE_GLASS_SHOTS=<dir>` to keep the frames as PNGs.
#![cfg(target_os = "macos")]

use std::cell::Cell;
use std::rc::Rc;

use i_slint_renderer_skia::{SkiaRenderer, SkiaSharedContext};
use slint::platform::{Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalSize, ModelRc, PhysicalSize, Rgba8Pixel, SharedPixelBuffer, VecModel};

slint::include_modules!();

const W: u32 = 1440;
const H: u32 = 900;
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

struct Offscreen(std::time::Instant);

impl Platform for Offscreen {
    fn duration_since_start(&self) -> std::time::Duration {
        self.0.elapsed()
    }

    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let adapter: Rc<Adapter> = Rc::new_cyclic(|weak: &std::rc::Weak<Adapter>| Adapter {
            window: slint::Window::new(weak.clone()),
            renderer: SkiaRenderer::default(&SkiaSharedContext::default()),
            size: Cell::new(PhysicalSize::new((W as f32 * SCALE) as u32, (H as f32 * SCALE) as u32)),
        });
        Ok(adapter)
    }
}

type Shot = SharedPixelBuffer<Rgba8Pixel>;

fn shoot(ui: &MainWindow, name: &str) -> Shot {
    // Let page fade-ins and other animations finish.
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        slint::platform::update_timers_and_animations();
    }
    let shot = ui.window().take_snapshot().expect("snapshot");
    if let Some(dir) = std::env::var_os("TIDE_GLASS_SHOTS") {
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

/// Logical point -> pixel.
fn at(shot: &Shot, x: f32, y: f32) -> (u8, u8, u8) {
    let (px, py) = ((x * SCALE) as u32, (y * SCALE) as u32);
    let p = shot.as_slice()[(py * shot.width() + px) as usize];
    (p.r, p.g, p.b)
}

fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32
}

/// Mean and largest difference of luma between neighbours along a logical row.
fn row_stats(shot: &Shot, y: f32, x0: f32, x1: f32) -> (f32, f32) {
    let lumas: Vec<f32> =
        ((x0 * SCALE) as u32..(x1 * SCALE) as u32).map(|px| luma(at(shot, px as f32 / SCALE, y))).collect();
    let mean = lumas.iter().sum::<f32>() / lumas.len() as f32;
    let step = lumas.windows(2).map(|w| (w[0] - w[1]).abs()).fold(0., f32::max);
    (mean, step)
}

fn cover(color: [u8; 3]) -> slint::Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(64, 64);
    for (i, p) in buffer.make_mut_slice().iter_mut().enumerate() {
        // A hard-edged pattern, so blur is measurable.
        let on = (i % 64 / 8 + i / 64 / 8) % 2 == 0;
        let c = if on { color } else { [255, 255, 255] };
        *p = Rgba8Pixel { r: c[0], g: c[1], b: c[2], a: 255 };
    }
    slint::Image::from_rgba8(buffer)
}

#[test]
fn pages_show_through_the_player_bar_as_glass() {
    slint::platform::set_platform(Box::new(Offscreen(std::time::Instant::now()))).unwrap();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: SCALE });
    ui.window().dispatch_event(WindowEvent::Resized { size: LogicalSize::new(W as f32, H as f32) });

    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("1".into());
    app.set_np_title("Fixture track with a fairly long title".into());
    app.set_np_artist("Fixture artist".into());
    app.set_np_cover(cover([220, 40, 60]));

    // A grid of bright covers: the hardest thing to keep the bar legible over.
    app.set_view("grid".into());
    app.set_page_title("Albums".into());
    app.set_grid_kind("albums".into());
    let colors = [[240, 200, 30], [30, 160, 240], [250, 250, 250], [230, 60, 160], [60, 220, 120]];
    let cards: ModelRc<CardData> = ModelRc::new(VecModel::from(
        (0..60)
            .map(|i| CardData {
                id: i.to_string().into(),
                kind: "album".into(),
                title: format!("Album {i}").into(),
                subtitle: "Artist".into(),
                image: cover(colors[i % colors.len()]),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    ));
    app.set_grid_cards(cards.clone());
    app.set_grid_groups(ModelRc::new(VecModel::from(vec![SectionData {
        cards,
        ..Default::default()
    }])));

    let bar_top = H as f32 - app.get_player_height();
    // The stretch of the bar between the titles and the transport controls,
    // above the scrubber: nothing of the bar's own is drawn there.
    let (x0, x1) = (430., 590.);
    let rows = [bar_top + 8., bar_top + 26., bar_top + 44.];

    // Opaque bar (the fallback): pages end at the bar, the bar is flat.
    app.set_glass(false);
    app.set_scroll_y(-190.);
    let opaque = shoot(&ui, "grid-opaque");
    assert_eq!(app.get_pane_height(), bar_top - app.get_pane_y());
    assert_eq!(at(&opaque, 500., bar_top + 6.), (0x1b, 0x1b, 0x1c));

    // Glass: the pane runs under the bar.
    app.set_glass(true);
    let glass = shoot(&ui, "grid-glass");
    assert_eq!(app.get_pane_height(), H as f32 - app.get_pane_y());

    // Above the bar nothing changed.
    for y in [200., 400., bar_top - 30.] {
        for x in [300., 700., 1300.] {
            assert_eq!(at(&glass, x, y), at(&opaque, x, y), "page changed at {x},{y}");
        }
    }

    // In the bar, the covers show through: lit well above the bar over a
    // black page (about luma 26), but with no hard edge left (the covers'
    // checker pattern has steps of 200+).
    let mut lightest = 0f32;
    let mut sharpest = 0f32;
    for y in rows {
        let lumas: Vec<f32> = (x0 as u32..x1 as u32).step_by(4).map(|x| luma(at(&glass, x as f32, y))).collect();
        lightest = lumas.iter().fold(lightest, |hi, l| hi.max(*l));
        sharpest = sharpest.max(row_stats(&glass, y, x0, x1).1);
    }
    println!("bar over covers: lightest luma {lightest}, sharpest step {sharpest}");
    assert!(lightest > 40., "nothing shows through the bar (luma {lightest})");
    assert!(sharpest < 20., "content under the bar is not blurred (step {sharpest})");
    // White text needs a dark enough ground (4.5:1 against #fff is luma < ~117;
    // #909090 secondary text needs it darker still).
    assert!(lightest < 75., "bar too light over bright covers: luma {lightest}");

    // The rim: the bar's top line is lighter than the glass just below it.
    let (rim, _) = row_stats(&glass, bar_top + 0.25, x0, x1);
    let (below, _) = row_stats(&glass, bar_top + 4., x0, x1);
    assert!(rim > below + 4., "no rim highlight ({rim} vs {below})");

    // TopBar chrome (< >, Search, Debug, Profile) uses Liquid Glass when glass is on.
    let menu = 40.;
    let top_nav_opaque = luma(at(&opaque, 312., menu + 13.5));
    let top_nav_glass_rim = luma(at(&glass, 312., menu + 13.5));
    let top_search_glass_rim = luma(at(&glass, 1180., menu + 12.75));
    let top_search_glass_body = luma(at(&glass, 1180., menu + 20.));
    assert!(
        top_nav_glass_rim > top_nav_opaque + 6.,
        "TopBar back/forward pill has no glass rim ({top_nav_glass_rim} vs {top_nav_opaque})"
    );
    assert!(
        top_search_glass_rim > top_search_glass_body + 6.,
        "TopBar search pill has no glass rim ({top_search_glass_rim} vs {top_search_glass_body})"
    );

    // A track list, scrolled, for the record (and a second page type).
    app.set_view("tracks".into());
    app.set_page_title("Tracks".into());
    app.set_tracks(ModelRc::new(VecModel::from(
        (0..80)
            .map(|i| TrackData {
                id: i.to_string().into(),
                title: format!("Track number {i}").into(),
                artist: "Fixture artist".into(),
                album: "Fixture album".into(),
                duration: "3:21".into(),
                cover: cover(colors[i % colors.len()]),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    app.set_scroll_y(-333.);
    let tracks = shoot(&ui, "tracks-glass");
    let (_, step) = row_stats(&tracks, bar_top + 8., x0, x1);
    assert!(step < 20., "track rows under the bar are not blurred (step {step})");
    app.set_glass(false);
    shoot(&ui, "tracks-opaque");

    // Now Playing: the cover's colours behind, the controls on a glass pane.
    app.set_glass(true);
    app.set_np_backdrop(cover([220, 40, 60]));
    app.set_np_tint(slint::Color::from_rgb_u8(120, 30, 40));
    app.set_show_now_playing(true);
    let playing = shoot(&ui, "now-playing-glass");
    let pane = luma(at(&playing, 400., bar_top + 30.));
    let beside = luma(at(&playing, 400., bar_top - 20.));
    println!("now playing: pane luma {pane}, page beside it {beside}");
    assert!((pane - beside).abs() > 3., "the controls' pane does not stand out ({pane} vs {beside})");

    // Live expansion (rendering mid-animation frames, as 120 Hz does): once
    // settled, Search, Debug and Profile all render Liquid Glass over the backdrop.
    app.set_show_now_playing(false);
    shoot(&ui, "np-repro-closed");
    app.set_show_now_playing(true);
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        slint::platform::update_timers_and_animations();
        let _ = ui.window().take_snapshot().unwrap();
    }
    let settled = shoot(&ui, "np-repro-settled");
    let search_px = at(&settled, 1180., menu + 20.);
    let search_rim = at(&settled, 1180., menu + 12.75);
    let debug_px = at(&settled, 1366., menu + 18.);
    let profile_px = at(&settled, 1409., menu + 18.);
    println!(
        "after live expansion: search={search_px:?} rim={search_rim:?} debug={debug_px:?} profile={profile_px:?}"
    );
    assert_ne!(search_px, (30, 30, 30), "Search stayed opaque non-glass after live expansion");
    assert_ne!(debug_px, (42, 42, 42), "Debug stayed opaque non-glass after live expansion");
    assert_ne!(profile_px, (42, 42, 42), "Profile stayed opaque non-glass after live expansion");
    assert!(
        luma(search_rim) > luma(search_px) + 5.,
        "Now Playing Search pill has no glass rim ({search_rim:?} vs {search_px:?})"
    );
}
