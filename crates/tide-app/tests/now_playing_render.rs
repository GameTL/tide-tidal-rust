//! Now Playing, drawn for real by the patched Skia renderer into memory (no
//! window on screen): the arrangements with and without lyrics at wide,
//! medium and narrow widths, glass on and off, and the artwork tilted by the
//! pointer. Set `TIDE_GLASS_SHOTS=<dir>` to keep the frames as PNGs.
#![cfg(target_os = "macos")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use i_slint_renderer_skia::{SkiaRenderer, SkiaSharedContext};
use slint::platform::{Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{
    ComponentHandle, LogicalPosition, LogicalSize, ModelRc, PhysicalSize, Rgba8Pixel, SharedPixelBuffer,
    VecModel,
};

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
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        slint::platform::update_timers_and_animations();
    }
}

fn shoot(ui: &MainWindow, name: &str) -> Shot {
    settle();
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

fn at(shot: &Shot, x: f32, y: f32) -> (u8, u8, u8) {
    let (px, py) = ((x * SCALE) as u32, (y * SCALE) as u32);
    let p = shot.as_slice()[(py * shot.width() + px) as usize];
    (p.r, p.g, p.b)
}

/// A cover with hard straight edges, so a tilt shows as bent lines.
fn cover(px: u32) -> slint::Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(px, px);
    let n = px as usize;
    for (i, p) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i % n, i / n);
        let (fx, fy) = (x as f32 / n as f32, y as f32 / n as f32);
        let grid = x % (n / 8) < n / 80 || y % (n / 8) < n / 80;
        let (r, g, b) = if grid {
            (250, 240, 220)
        } else {
            ((235. - 120. * fy) as u8, (120. + 80. * fx) as u8, (40. + 60. * fy) as u8)
        };
        *p = Rgba8Pixel { r, g, b, a: 255 };
    }
    slint::Image::from_rgba8(buffer)
}

fn blur_wash() -> slint::Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(32, 32);
    for (i, p) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % 32) as f32 / 32., (i / 32) as f32 / 32.);
        *p = Rgba8Pixel { r: (150. + 60. * x) as u8, g: (80. + 40. * y) as u8, b: 30, a: 255 };
    }
    slint::Image::from_rgba8(buffer)
}

const LINES: [&str; 24] = [
    "Streetlights hum a song in a key of their own",
    "I walk it home",
    "Counting the windows still awake on the hill",
    "Some of them always will",
    "Paper boats in the gutter racing the rain",
    "Down to the drain",
    "Every corner remembers a name I forgot",
    "Maybe it's not",
    "Hold the night like a coin in a pocket of light",
    "Spend it right",
    "We were younger than the sound of the trains",
    "Running the lanes",
    "If the morning finds us out on the pier",
    "Say we were here",
    "Static on the radio, a voice through the hiss",
    "Something like this",
    "Turn it up until the ceiling starts to sing",
    "Everything",
    "Streetlights hum a song in a key of their own",
    "I walk it home",
    "Counting the windows still awake on the hill",
    "Some of them always will",
    "Hold the night like a coin in a pocket of light",
    "Spend it right",
];

#[test]
fn now_playing_arrangements_render() {
    slint::platform::set_platform(Box::new(Offscreen(std::time::Instant::now()))).unwrap();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: SCALE });
    resize(&ui, 1440., 900.);

    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("1".into());
    app.set_np_title("Paper Boats (Night Version)".into());
    app.set_np_artist("The Fixture Ensemble".into());
    app.set_np_album_id("a1".into());
    app.set_np_cover(cover(64));
    app.set_np_cover_large(cover(640));
    app.set_np_tint(slint::Color::from_rgb_u8(110, 60, 20));
    app.set_np_backdrop(blur_wash());
    app.set_lyrics(ModelRc::new(VecModel::from(
        LINES
            .iter()
            .enumerate()
            .map(|(i, l)| LyricLine { text: (*l).into(), time: 4_000 * i as i32 })
            .collect::<Vec<_>>(),
    )));
    app.set_lyrics_synced(true);
    app.set_lyric_index(6);
    app.set_lyrics_status("".into());
    app.set_np_context("0_1 - My OG / Nostalgia Style".into());
    app.set_np_queue(ModelRc::new(VecModel::from(
        [
            ("Not Today", "twenty one pilots", "Blurryface", "3:58"),
            ("Love Runs Out", "OneRepublic", "Native (Gold Edition)", "3:44"),
            ("Rolling In the Deep", "Adele", "21", "3:48"),
            ("Hello", "Adele", "25", "4:55"),
            ("Lonely Boy", "The Black Keys", "El Camino", "3:13"),
            ("Lo/Hi", "The Black Keys", "Let's Rock", "2:57"),
            ("Walk Across The Water", "The Black Keys", "\"Let's Rock\"", "3:55"),
            ("Sooner or Later", "Mat Kearney", "Young Love", "3:36"),
            ("Paris", "Magic Man", "Before the Waves", "3:34"),
            ("Way Up", "Jaden", "Spider-Man: Into the Spider-Verse", "2:34"),
            ("Shatter Me", "Lindsey Stirling", "Shatter Me", "4:40"),
            ("Payphone", "Maroon 5", "Overexposed", "3:51"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (title, artist, album, dur))| TrackData {
            id: format!("q-{i}").into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration: dur.into(),
            explicit: i % 4 == 0,
            cover: cover(96),
            ..Default::default()
        })
        .collect::<Vec<_>>(),
    )));

    for glass in [true, false] {
        let g = if glass { "glass" } else { "plain" };
        app.set_glass(glass);

        // Wide, lyrics: art left, the column right, the sung line lit.
        resize(&ui, 1440., 900.);
        app.set_show_queue(false);
        app.set_show_lyrics(true);
        app.set_show_now_playing(true);
        let lyrics = shoot(&ui, &format!("np-wide-lyrics-{g}"));
        // Wide, queue: art left, borderless queue right.
        app.set_show_queue(true);
        app.set_show_lyrics(false);
        let queue = shoot(&ui, &format!("np-wide-queue-{g}"));
        // Scroll the queue mid-row (-85px) so bright album covers straddle both
        // the top (y≈215) and bottom (y≈725) list boundaries, verifying smooth
        // edge fade with zero hard cut-off.
        ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position: LogicalPosition::new(980., 440.),
            delta_x: 0.,
            delta_y: -85.,
        });
        let queue_scrolled = shoot(&ui, &format!("np-wide-queue-scrolled-{g}"));
        // Wide, no panel: the art centred.
        app.set_show_queue(false);
        app.set_show_lyrics(false);
        let centred = shoot(&ui, &format!("np-wide-centred-{g}"));
        // Centre of the window is art without a panel; with lyrics or queue it
        // is the gap between art and column, i.e. background.
        assert_ne!(at(&lyrics, 720., 420.), at(&centred, 720., 420.));
        assert_ne!(at(&queue, 720., 420.), at(&centred, 720., 420.));
        // Queue sits borderless aligned with the artwork (not high under the
        // header; x=1200 clears the centred artwork's 60px drop shadow).
        assert_eq!(at(&queue, 1200., 120.), at(&centred, 1200., 120.));
        assert_ne!(at(&queue, 785., 245.), at(&centred, 785., 245.));
        // Compare cover column (x=785) against the adjacent backdrop gap (x=745)
        // in the same scrolled frame: both edges dissolve into the backdrop while
        // the middle cover stays crisp.
        let diff = |a: (u8, u8, u8), b: (u8, u8, u8)| -> i32 {
            (a.0 as i32 - b.0 as i32).abs()
                + (a.1 as i32 - b.1 as i32).abs()
                + (a.2 as i32 - b.2 as i32).abs()
        };
        let top_edge_diff = diff(at(&queue_scrolled, 785., 216.), at(&queue_scrolled, 745., 216.));
        let bot_edge_diff = diff(at(&queue_scrolled, 785., 724.), at(&queue_scrolled, 745., 724.));
        let mid_cover_diff = diff(at(&queue_scrolled, 785., 450.), at(&queue_scrolled, 745., 450.));
        assert!(
            top_edge_diff < 22,
            "scrolled queue top edge hard-clipped ({g}: diff={top_edge_diff} vs mid={mid_cover_diff})"
        );
        assert!(
            bot_edge_diff < 22,
            "scrolled queue bottom edge hard-clipped ({g}: diff={bot_edge_diff} vs mid={mid_cover_diff})"
        );
        assert!(
            mid_cover_diff > 60,
            "scrolled queue middle cover not visible ({g}: diff={mid_cover_diff})"
        );

        // Medium: the bar is compact, the title moves under the art.
        resize(&ui, 840., 720.);
        app.set_show_queue(false);
        app.set_show_lyrics(true);
        shoot(&ui, &format!("np-medium-lyrics-{g}"));
        app.set_show_queue(true);
        app.set_show_lyrics(false);
        shoot(&ui, &format!("np-medium-queue-{g}"));
        app.set_show_queue(false);
        app.set_show_lyrics(false);
        shoot(&ui, &format!("np-medium-centred-{g}"));

        // Narrow: one column.
        resize(&ui, 390., 800.);
        shoot(&ui, &format!("np-narrow-{g}"));
        app.set_show_lyrics(true);
        shoot(&ui, &format!("np-narrow-lyrics-{g}"));
        app.set_show_queue(true);
        app.set_show_lyrics(false);
        shoot(&ui, &format!("np-narrow-queue-{g}"));
        app.set_show_queue(false);
        app.set_show_now_playing(false);
        settle();
    }

    // Tight narrow (320px) + empty queue with glass on.
    app.set_glass(true);
    resize(&ui, 320., 560.);
    app.set_show_lyrics(false);
    app.set_show_queue(true);
    app.set_show_now_playing(true);
    shoot(&ui, "np-tight-queue-glass");
    resize(&ui, 1440., 900.);
    let saved_queue = app.get_np_queue();
    app.set_np_queue(ModelRc::default());
    shoot(&ui, "np-wide-queue-empty-glass");
    app.set_np_queue(saved_queue);
    app.set_show_queue(false);

    // The widths in between, glass on.
    app.set_glass(true);
    app.set_show_now_playing(true);
    for (w, h) in [(1100., 800.), (960., 700.), (720., 620.), (700., 800.), (480., 620.), (320., 560.)] {
        resize(&ui, w, h);
        app.set_show_lyrics(true);
        shoot(&ui, &format!("np-sweep-{w}x{h}-lyrics"));
        app.set_show_lyrics(false);
        shoot(&ui, &format!("np-sweep-{w}x{h}-centred"));
    }
    app.set_show_now_playing(false);
    settle();

    // Mid-transition frames: Centred -> Queue (must not overlap art) and Lyrics -> Queue.
    app.set_glass(true);
    resize(&ui, 1440., 900.);
    app.set_show_queue(false);
    app.set_show_lyrics(false);
    app.set_show_now_playing(true);
    settle();
    app.set_show_queue(true);
    for (step, name) in [(3, "np-centred-to-queue-mid1"), (4, "np-centred-to-queue-mid2")] {
        for _ in 0..step {
            std::thread::sleep(std::time::Duration::from_millis(30));
            slint::platform::update_timers_and_animations();
        }
        let frame = ui.window().take_snapshot().unwrap();
        if let Some(dir) = std::env::var_os("TIDE_GLASS_SHOTS") {
            let dir = std::path::PathBuf::from(dir);
            image::save_buffer(
                dir.join(format!("{name}.png")),
                frame.as_bytes(),
                frame.width(),
                frame.height(),
                image::ColorType::Rgba8,
            )
            .unwrap();
        }
    }
    settle();
    app.set_show_queue(false);
    app.set_show_lyrics(true);
    settle();
    app.set_show_queue(true);
    app.set_show_lyrics(false);
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        slint::platform::update_timers_and_animations();
    }
    let cross = ui.window().take_snapshot().unwrap();
    if let Some(dir) = std::env::var_os("TIDE_GLASS_SHOTS") {
        let dir = std::path::PathBuf::from(dir);
        image::save_buffer(
            dir.join("np-lyrics-to-queue-mid.png"),
            cross.as_bytes(),
            cross.width(),
            cross.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }

    // Live 1:1 trackpad swipe mid-gesture (progress = 0.45): Lyrics glides left,
    // Queue glides in side-by-side from the right without overlapping Lyrics,
    // pill thumb tracks 45%, and both sides of the right-column box dissolve
    // softly into the backdrop without hard clip.
    app.set_show_queue(false);
    app.set_show_lyrics(true);
    let lyrics_base = shoot(&ui, "np-swipe-base-glass");
    app.set_np_swipe_from_centred(false);
    app.set_np_swipe_active(true);
    app.set_np_swipe_progress(0.45);
    let swipe = shoot(&ui, "np-swipe-mid-glass");
    let diff = |a: (u8, u8, u8), b: (u8, u8, u8)| -> i32 {
        (a.0 as i32 - b.0 as i32).abs()
            + (a.1 as i32 - b.1 as i32).abs()
            + (a.2 as i32 - b.2 as i32).abs()
    };
    // At 1440x900: col-x ≈ 757, col-w = 560, side-fade ≈ 47 -> box spans
    // roughly x=710..1364. Compare against the settled `lyrics_base` at the
    // exact same coordinates (outside col-x..col-x+col-w, so identical backdrop
    // and artwork shadow) to isolate edge leakage from background gradients.
    let mut left_max = 0;
    let mut right_max = 0;
    for y in (260..680).step_by(12) {
        left_max = left_max.max(diff(at(&swipe, 712., y as f32), at(&lyrics_base, 712., y as f32)));
        right_max = right_max.max(diff(at(&swipe, 1362., y as f32), at(&lyrics_base, 1362., y as f32)));
    }
    assert!(
        left_max < 24,
        "live swipe left box edge hard-clipped (max diff={left_max})"
    );
    assert!(
        right_max < 24,
        "live swipe right box edge hard-clipped (max diff={right_max})"
    );
    app.set_np_swipe_active(false);
    app.set_np_swipe_progress(0.);
    app.set_show_queue(false);
    app.set_show_now_playing(false);
    settle();

    // The tilt: the pointer near the art's top right corner, then gone.
    app.set_glass(true);
    resize(&ui, 1440., 900.);
    app.set_show_lyrics(false);
    app.set_show_now_playing(true);
    let flat = shoot(&ui, "np-tilt-rest");
    // The centred art at 1440x900 spans about 430..1010 x 151..732 (the
    // menu bar is drawn in the window here, 40px).
    let position = LogicalPosition::new(990., 175.);
    ui.window().dispatch_event(WindowEvent::PointerMoved { position });
    let tilted = shoot(&ui, "np-tilt-top-right");
    // Pressed at the top right, that corner goes away: its edge moves in.
    let edge_row = 190.;
    let right_edge = |shot: &Shot| {
        (900..1100).rev().find(|x| {
            let (r, g, b) = at(shot, *x as f32, edge_row);
            r as u32 + g as u32 + b as u32 > 300
        })
    };
    let (flat_edge, tilted_edge) = (right_edge(&flat), right_edge(&tilted));
    println!("right edge at y={edge_row}: rest {flat_edge:?}, tilted {tilted_edge:?}");
    assert!(tilted_edge.unwrap() < flat_edge.unwrap(), "the pressed corner does not recede");
    let position = LogicalPosition::new(450., 710.);
    ui.window().dispatch_event(WindowEvent::PointerMoved { position });
    shoot(&ui, "np-tilt-bottom-left");
    ui.window().dispatch_event(WindowEvent::PointerExited);
    let back = shoot(&ui, "np-tilt-left");
    assert_eq!(right_edge(&back), flat_edge, "the art does not come back flat");

    // Half-way through the expansion, from the bar's slot.
    app.set_show_now_playing(false);
    settle();
    app.set_show_now_playing(true);
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        slint::platform::update_timers_and_animations();
    }
    let mid = ui.window().take_snapshot().unwrap();
    if let Some(dir) = std::env::var_os("TIDE_GLASS_SHOTS") {
        let dir = std::path::PathBuf::from(dir);
        image::save_buffer(dir.join("np-expanding.png"), mid.as_bytes(), mid.width(), mid.height(), image::ColorType::Rgba8)
            .unwrap();
    }
}
