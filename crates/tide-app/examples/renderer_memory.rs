use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

#[path = "../src/mem.rs"]
mod mem;

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let renderer = arguments.next().unwrap_or_else(|| "skia".into());
    let steps = arguments
        .next()
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(12);
    let selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(renderer.clone());
    #[cfg(target_os = "macos")]
    let selector = {
        use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
        let selector = if renderer == "skia" {
            selector.require_metal()
        } else {
            selector
        };
        selector.with_winit_window_attributes_hook(|attributes| {
            attributes
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true)
                .with_title_hidden(true)
        })
    };
    selector.select()?;
    println!("pid={}", std::process::id());
    let ui = MainWindow::new()?;
    ui.window().set_size(slint::LogicalSize::new(2056., 1289.));
    let app = ui.global::<App>();
    app.set_logged_in(true);
    app.set_page_title("Renderer memory probe".into());
    app.set_grid_kind("albums".into());
    let step = Rc::new(Cell::new(0u32));
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), {
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let app = ui.global::<App>();
            let current = step.get();
            let size = ui.window().size();
            let footprint = mem::footprint();
            let malloc = mem::malloc_in_use();
            let reading = mem::Breakdown::split(
                footprint,
                malloc,
                0,
                0,
                u64::from(size.width) * u64::from(size.height) * 4 * 3,
            );
            println!(
                "renderer={renderer} step={current} size={}x{} footprint={} malloc={} remainder={} footprint_bytes={footprint}",
                size.width,
                size.height,
                mem::human(reading.total()),
                mem::human(malloc),
                mem::human(reading.other),
            );
            if current == steps {
                let snapshot = ui.window().take_snapshot().expect("renderer snapshot");
                assert_eq!(
                    (snapshot.width(), snapshot.height()),
                    (size.width, size.height)
                );
                slint::quit_event_loop().unwrap();
                return;
            }
            if current % 3 == 2 {
                app.set_view("settings".into());
                app.set_grid_cards(ModelRc::default());
                app.set_grid_groups(ModelRc::default());
            } else {
                let cards = ModelRc::new(VecModel::from(
                    (0..48)
                        .map(|index| {
                            let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(256, 256);
                            pixels.make_mut_slice().fill(Rgba8Pixel::new(
                                (index * 5) as u8,
                                (current * 20) as u8,
                                100,
                                255,
                            ));
                            CardData {
                                id: format!("{current}-{index}").into(),
                                title: format!("Album {index}").into(),
                                kind: "album".into(),
                                image: Image::from_rgba8(pixels),
                                ..Default::default()
                            }
                        })
                        .collect::<Vec<_>>(),
                ));
                app.set_grid_cards(cards.clone());
                app.set_grid_groups(ModelRc::new(VecModel::from(vec![SectionData {
                    cards,
                    ..Default::default()
                }])));
                app.set_view("grid".into());
            }
            step.set(current + 1);
        }
    });
    ui.run()?;
    Ok(())
}
