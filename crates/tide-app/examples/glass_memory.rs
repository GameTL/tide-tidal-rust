//! What the Liquid Glass player bar costs in memory, measured in a real
//! window on the real renderer: the bar is switched between opaque and glass
//! while an album grid scrolls beneath it, and the process footprint (what
//! Activity Monitor shows) is printed for each phase.
//!
//! The window is invisible (alpha 0, click-through) and the app does not
//! activate or show in the Dock, so the run does not interrupt anything.
//!
//!     cargo run --release -p tide-app --example glass_memory [available|unavailable]
//!
//! `unavailable` is `TIDE_GLASS=0`: the window's buffers are not readable.
//!
//! 2026-11-20, 1440x900 window at 2x, lean profile: glass costs 5.3 MB of
//! footprint while scrolling and 4.5 MB idle (GPU cache +5.75 MB).
#![cfg(target_os = "macos")]

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

#[path = "../src/mem.rs"]
#[allow(dead_code)]
mod mem;

slint::include_modules!();

const MB: f64 = 1024. * 1024.;
/// Each phase scrolls for 5 s, then sits idle for 6 s (past the 3 s idle purge).
const SCROLL_SECS: f64 = 5.;
const IDLE_SECS: f64 = 6.;
const PHASES: u32 = 8;

/// The main screen's refresh rate (60 Hz when it can't be read), so the
/// scroll moves one step per frame of the panel, as the app's own motion does.
fn screen_fps() -> f64 {
    let mtm = objc2::MainThreadMarker::new().expect("main thread");
    objc2_app_kit::NSScreen::mainScreen(mtm)
        .map(|screen| screen.maximumFramesPerSecond() as f64)
        .filter(|fps| *fps >= 60.)
        .unwrap_or(60.)
}

fn cover(index: usize) -> Image {
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(320, 320);
    for (i, p) in pixels.make_mut_slice().iter_mut().enumerate() {
        let on = (i % 320 / 40 + i / 320 / 40) % 2 == 0;
        let c = (index * 37 % 200) as u8;
        *p = if on {
            Rgba8Pixel::new(c, 255 - c, 120, 255)
        } else {
            Rgba8Pixel::new(245, 245, 245, 255)
        };
    }
    Image::from_rgba8(pixels)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use slint::winit_030::winit::platform::macos::{
        ActivationPolicy, EventLoopBuilderExtMacOS, WindowAttributesExtMacOS,
    };
    let available = std::env::args().nth(1).as_deref() != Some("unavailable");
    i_slint_renderer_skia::glass::set_available(available);
    // The lean profile, as `platform::apply_memory_profile(true)`.
    i_slint_renderer_skia::memory::set_gpu_cache_limit(48 * 1024 * 1024);
    i_slint_renderer_skia::memory::set_idle_purge(Duration::from_secs(3));
    i_slint_renderer_skia::memory::set_two_drawables(true);

    let mut event_loop = slint::winit_030::winit::event_loop::EventLoop::with_user_event();
    event_loop
        .with_activation_policy(ActivationPolicy::Accessory)
        .with_activate_ignoring_other_apps(false);
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("skia".into())
        .require_metal()
        .with_winit_event_loop_builder(event_loop)
        .with_winit_window_attributes_hook(|attributes| {
            attributes
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true)
                .with_title_hidden(true)
                .with_active(false)
                .with_position(slint::winit_030::winit::dpi::LogicalPosition::new(-20000., -20000.))
        })
        .select()?;

    let ui = MainWindow::new()?;
    ui.window().set_size(slint::LogicalSize::new(1440., 900.));
    let app = ui.global::<App>();
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("1".into());
    app.set_np_title("Fixture track".into());
    app.set_np_artist("Fixture artist".into());
    app.set_np_cover(cover(3));
    app.set_view("grid".into());
    app.set_page_title("Albums".into());
    app.set_grid_kind("albums".into());
    let cards: ModelRc<CardData> = ModelRc::new(VecModel::from(
        (0..120)
            .map(|i| CardData {
                id: i.to_string().into(),
                kind: "album".into(),
                title: format!("Album {i}").into(),
                subtitle: "Artist".into(),
                image: cover(i),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    ));
    app.set_grid_cards(cards.clone());
    app.set_grid_groups(ModelRc::new(VecModel::from(vec![SectionData {
        cards,
        ..Default::default()
    }])));
    app.set_glass(false);

    // The window is created far off screen (see the attributes hook), made
    // invisible and click-through there, and only then moved onto the screen:
    // nothing of it is ever seen.
    ui.show()?;
    let hidden = Rc::new(Cell::new(false));
    let hide = {
        let hidden = hidden.clone();
        move || {
            use objc2_app_kit::NSApplication;
            if hidden.get() {
                return true;
            }
            let mtm = objc2::MainThreadMarker::new().expect("main thread");
            let windows = NSApplication::sharedApplication(mtm).windows();
            for window in windows.iter() {
                window.setAlphaValue(0.);
                window.setIgnoresMouseEvents(true);
                window.setFrameOrigin(objc2_foundation::NSPoint::new(40., 40.));
                hidden.set(true);
            }
            hidden.get()
        }
    };

    let fps = screen_fps();
    let scroll_ticks = (SCROLL_SECS * fps) as u32;
    let phase_ticks = ((SCROLL_SECS + IDLE_SECS) * fps) as u32;
    let tick = Rc::new(Cell::new(0u32));
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs_f64(1. / fps), {
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let app = ui.global::<App>();
            if !hide() {
                return;
            }
            let t = tick.get();
            tick.set(t + 1);
            let phase = t / phase_ticks;
            let within = t % phase_ticks;
            // Phase 0 is a warm-up (opaque); then glass and opaque alternate.
            let glass = phase % 2 == 1;
            if within == 0 {
                if phase == PHASES {
                    slint::quit_event_loop().unwrap();
                    return;
                }
                app.set_glass(glass);
            }
            if within < scroll_ticks {
                // Back and forth over 1500 px of covers.
                let s = (within as f32 / scroll_ticks as f32 * std::f32::consts::TAU).sin();
                app.set_scroll_y(-(750. + 750. * s));
            }
            if within == scroll_ticks - 1 || within == phase_ticks - 1 {
                let size = ui.window().size();
                println!(
                    "phase={phase} glass={} state={} size={}x{} footprint_mb={:.2} gpu_cache_mb={:.2}",
                    glass as u8,
                    if within == scroll_ticks - 1 { "scrolling" } else { "idle" },
                    size.width,
                    size.height,
                    mem::footprint() as f64 / MB,
                    i_slint_renderer_skia::memory::gpu_cache_usage() as f64 / MB,
                );
            }
        }
    });
    slint::run_event_loop_until_quit()?;
    Ok(())
}
