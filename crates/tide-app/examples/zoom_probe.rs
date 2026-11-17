//! Synthetic native UI check: cargo run -p tide-app --example zoom_probe
use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{cell::Cell, rc::Rc, time::Duration};
#[path = "../src/zoom.rs"]
mod zoom;
slint::include_modules!();

fn change(ui: &MainWindow, percent: i32) {
    ui.global::<App>().set_zoom_percent(percent);
    zoom::schedule(ui);
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
    app.set_np_title("Zoom fixture".into());
    app.set_np_artist("Synthetic data".into());
    app.set_show_collection(true);
    app.set_page_title("Zoom fixture".into());
    app.set_sections(ModelRc::new(VecModel::from(vec![SectionData {
        title: "Made for you".into(),
        layout: "cards".into(),
        cards: ModelRc::new(VecModel::from(
            (0..12)
                .map(|i| CardData {
                    title: format!("Album {}", i + 1).into(),
                    subtitle: "Synthetic artist".into(),
                    kind: "album".into(),
                    id: i.to_string().into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )),
        ..Default::default()
    }])));
    for (callback, direction) in [(true, 1), (false, -1)] {
        let weak = ui.as_weak();
        let action = move || {
            let ui = weak.unwrap();
            let p = zoom::step(ui.global::<App>().get_zoom_percent(), direction);
            change(&ui, p);
        };
        if callback {
            app.on_zoom_in(action);
        } else {
            app.on_zoom_out(action);
        }
    }
    app.on_reset_zoom({
        let weak = ui.as_weak();
        move || change(&weak.unwrap(), 100)
    });
    ui.window().on_winit_window_event({
        let weak = ui.as_weak();
        move |window, event| {
            zoom::window_event(
                window,
                event,
                weak.unwrap().global::<App>().get_zoom_percent(),
            )
        }
    });
    ui.window().set_size(slint::LogicalSize::new(1440., 900.));
    ui.show()?;
    ui.window()
        .with_winit_window(|w| w.set_title("TIDE zoom fixture"));
    let expected = Rc::new(Cell::new(ui.window().size()));
    let step = Rc::new(Cell::new(0usize));
    let timer = slint::Timer::default();
    let directory = std::path::Path::new("/tmp/tide-zoom-check");
    std::fs::create_dir_all(directory)?;
    let manual = std::env::args().any(|a| a == "--manual");
    if !manual {
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(400), {
            let weak = ui.as_weak();
            move || {
                let ui = weak.unwrap();
                let current = step.get();
                if current == 0 {
                    expected.set(ui.window().size());
                }
                let states = [
                    100, 125, 150, 175, 200, 175, 150, 125, 100, 75, 50, 75, 100, 200,
                ];
                if current > 0 {
                    let native = ui.window().with_winit_window(|w| w.inner_size()).unwrap();
                    let scale = ui.window().scale_factor();
                    let percent = ui.global::<App>().get_zoom_percent();
                    println!(
                        "zoom={percent} scale={scale} native={native:?} compact={}",
                        ui.global::<App>().get_compact_layout()
                    );
                    assert_eq!(
                        (native.width, native.height),
                        (expected.get().width, expected.get().height),
                        "zoom must not resize native window"
                    );
                    let shot = ui.window().take_snapshot().unwrap();
                    image::save_buffer(
                        directory.join(format!("{current:02}-{percent}.png")),
                        shot.as_bytes(),
                        shot.width(),
                        shot.height(),
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                }
                if current == states.len() {
                    step.set(current + 1);
                    // Recreate native window and require the user scale to survive.
                    ui.hide().unwrap();
                    ui.show().unwrap();
                    zoom::schedule(&ui);
                    let weak = ui.as_weak();
                    slint::Timer::single_shot(Duration::from_millis(400), move || {
                        let ui = weak.unwrap();
                        let native_scale =
                            ui.window().with_winit_window(|w| w.scale_factor()).unwrap() as f32;
                        assert_eq!(ui.window().scale_factor(), native_scale * 2.0);
                        println!("Reopen at 200% passed");
                        slint::quit_event_loop().unwrap();
                    });
                    return;
                }
                if current > states.len() {
                    return;
                }
                change(&ui, states[current]);
                step.set(current + 1);
            }
        });
    }
    slint::run_event_loop_until_quit()?;
    Ok(())
}
