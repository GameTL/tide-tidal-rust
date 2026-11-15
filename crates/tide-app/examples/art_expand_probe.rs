//! Times Now Playing artwork expansion against the display.
//! `cargo run -p tide-app --example art_expand_probe`
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Image, RenderingState, Rgba8Pixel, SharedPixelBuffer};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

fn checker(px: u32) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(px, px);
    let size = px as usize;
    for (i, pixel) in buf.make_mut_slice().iter_mut().enumerate() {
        let x = i % size;
        let y = i / size;
        let on = (x / 64 + y / 64).is_multiple_of(2);
        let v = if on { 48 } else { 210 };
        *pixel = Rgba8Pixel {
            r: v,
            g: v / 2,
            b: 90,
            a: 255,
        };
    }
    Image::from_rgba8(buf)
}

fn report(label: &str, samples: &[Duration]) {
    let n = samples.len();
    if n == 0 {
        println!("{label} frames=0");
        return;
    }
    let mut sorted = samples.to_vec();
    sorted.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let max = sorted[n - 1];
    let p95 = sorted[n * 95 / 100];
    let median = sorted[n / 2];
    let over16 = sorted
        .iter()
        .filter(|d| d.as_secs_f64() > 1.0 / 60.0)
        .count();
    let over8 = sorted
        .iter()
        .filter(|d| d.as_secs_f64() > 1.0 / 120.0)
        .count();
    println!(
        "{label} frames={n} median_ms={:.2} p95_ms={:.2} max_ms={:.2} over_16.7ms={over16} over_8.3ms={over8}",
        ms(median),
        ms(p95),
        ms(max),
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("skia".into());
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
        selector = selector
            .require_metal()
            .with_winit_window_attributes_hook(|a| {
                a.with_titlebar_transparent(true)
                    .with_fullsize_content_view(true)
                    .with_title_hidden(true)
            });
    }
    selector.select()?;

    let ui = MainWindow::new()?;
    let app = ui.global::<App>();
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_playing(true);
    app.set_np_title("Artwork probe".into());
    app.set_np_artist("Fixture".into());
    app.set_np_cover(checker(640));
    app.set_np_cover_large(checker(2048));
    ui.window().set_size(slint::LogicalSize::new(1440., 900.));

    let frames = Rc::new(RefCell::new(Vec::<Duration>::new()));
    let redraws = Rc::new(RefCell::new(Vec::<Duration>::new()));
    let last = Rc::new(Cell::new(None::<Instant>));
    let last_redraw = Rc::new(Cell::new(None::<Instant>));
    let armed = Rc::new(Cell::new(false));
    let draws = Rc::new(Cell::new(0u32));
    let paints = Rc::new(Cell::new(0u32));
    let states = Rc::new(RefCell::new(Vec::<String>::new()));

    ui.window().on_winit_window_event({
        let redraws = redraws.clone();
        let last_redraw = last_redraw.clone();
        let armed = armed.clone();
        let paints = paints.clone();
        move |_window, event| {
            if matches!(event, WindowEvent::RedrawRequested) {
                paints.set(paints.get() + 1);
                if armed.get() {
                    let now = Instant::now();
                    if let Some(prev) = last_redraw.replace(Some(now)) {
                        redraws
                            .borrow_mut()
                            .push(now.saturating_duration_since(prev));
                    }
                }
            }
            EventResult::Propagate
        }
    });

    ui.show()?;
    if let Err(err) = ui.window().set_rendering_notifier({
        let frames = frames.clone();
        let last = last.clone();
        let armed = armed.clone();
        let draws = draws.clone();
        let states = states.clone();
        move |state, _| {
            states.borrow_mut().push(format!("{state:?}"));
            draws.set(draws.get() + 1);
            if !matches!(state, RenderingState::AfterRendering) {
                return;
            }
            if !armed.get() {
                return;
            }
            let now = Instant::now();
            if let Some(prev) = last.replace(Some(now)) {
                frames
                    .borrow_mut()
                    .push(now.saturating_duration_since(prev));
            }
        }
    }) {
        println!("art-expand notifier={err:?}");
    }

    slint::Timer::single_shot(Duration::from_millis(200), {
        let weak = ui.as_weak();
        let armed = armed.clone();
        let last = last.clone();
        let last_redraw = last_redraw.clone();
        move || {
            last.set(None);
            last_redraw.set(None);
            armed.set(true);
            weak.unwrap().global::<App>().set_show_now_playing(true);
        }
    });
    slint::Timer::single_shot(Duration::from_millis(700), {
        let frames = frames.clone();
        let redraws = redraws.clone();
        let armed = armed.clone();
        let draws = draws.clone();
        let paints = paints.clone();
        let states = states.clone();
        move || {
            armed.set(false);
            println!("art-expand draws={} paints={}", draws.get(), paints.get());
            let unique = {
                let s = states.borrow();
                let mut u = s.clone();
                u.sort();
                u.dedup();
                u
            };
            println!("art-expand states={unique:?}");
            report("art-expand after-render", &frames.borrow());
            report("art-expand redraw", &redraws.borrow());
            slint::quit_event_loop().ok();
        }
    });
    slint::run_event_loop()?;
    Ok(())
}
