//! User zoom is independent of the monitor's backing scale (Retina, for example).

use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use slint::{ComponentHandle, PhysicalSize};

use crate::{App, MainWindow};

pub const LEVELS: [i32; 7] = [50, 75, 100, 125, 150, 175, 200];

pub fn normalize(percent: i32) -> i32 {
    *LEVELS
        .iter()
        .min_by_key(|&&level| (i64::from(level) - i64::from(percent)).abs())
        .unwrap()
}

pub fn step(percent: i32, direction: i32) -> i32 {
    let index = LEVELS
        .iter()
        .position(|&level| level == normalize(percent))
        .unwrap();
    LEVELS[(index as i32 + direction.signum()).clamp(0, LEVELS.len() as i32 - 1) as usize]
}

fn resize_at_scale(window: &slint::Window, physical: PhysicalSize, scale: f32) {
    window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    // A scale change alone doesn't update Slint's logical viewport. Resize the
    // viewport to the same physical surface, without resizing the native window.
    window.dispatch_event(slint::platform::WindowEvent::Resized {
        size: physical.to_logical(scale),
    });
    window.request_redraw();
}

fn apply(window: &slint::Window, percent: i32, display_scale: Option<f64>) {
    let Some((native_scale, size)) =
        window.with_winit_window(|w| (w.scale_factor(), w.inner_size()))
    else {
        return;
    };
    let scale = display_scale.unwrap_or(native_scale) as f32 * normalize(percent) as f32 / 100.0;
    if (window.scale_factor() - scale).abs() > f32::EPSILON {
        resize_at_scale(window, PhysicalSize::new(size.width, size.height), scale);
    }
}

/// Menu callbacks run while the backend borrows the native menu/window. Apply
/// the scale after they return, using the latest value if keys repeat quickly.
pub fn schedule(win: &MainWindow) {
    let weak = win.as_weak();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        if let Some(win) = weak.upgrade() {
            apply(win.window(), win.global::<App>().get_zoom_percent(), None);
        }
    });
}

/// Called by the existing input hook: it also restores zoom when Slint has
/// recreated a closed window. Consume DPI events so the backend cannot replace
/// the combined scale with the monitor scale alone.
pub fn window_event(window: &slint::Window, event: &WindowEvent, percent: i32) -> EventResult {
    if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
        apply(window, percent, Some(*scale_factor));
        EventResult::PreventDefault
    } else {
        apply(window, percent, None);
        EventResult::Propagate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_through_exact_levels_and_stops_at_bounds() {
        for (index, &level) in LEVELS.iter().enumerate() {
            assert_eq!(step(level, -1), LEVELS[index.saturating_sub(1)]);
            assert_eq!(step(level, 1), LEVELS[(index + 1).min(LEVELS.len() - 1)]);
        }
        assert_eq!(normalize(i32::MIN), 50);
        assert_eq!(normalize(i32::MAX), 200);
        assert_eq!(normalize(112), 100);
        assert_eq!(normalize(113), 125);
    }

    #[test]
    fn changing_zoom_preserves_pixels_and_maps_input_at_retina_scale() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Platform, WindowAdapter};
        use std::{cell::Cell, rc::Rc};

        struct TestPlatform(Rc<MinimalSoftwareWindow>);
        impl Platform for TestPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        slint::slint! {
            export component ZoomProbe inherits Window {
                out property <length> viewport-width: self.width;
                callback hit();
                TouchArea { x: 100px; y: 50px; width: 40px; height: 40px; clicked => { root.hit(); } }
            }
        }
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        adapter.set_size(PhysicalSize::new(2880, 1800));
        slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
        let probe = ZoomProbe::new().unwrap();
        probe.show().unwrap();
        let hits = Rc::new(Cell::new(0));
        probe.on_hit({
            let hits = hits.clone();
            move || hits.set(hits.get() + 1)
        });
        for level in LEVELS {
            let scale = 2.0 * level as f32 / 100.0;
            resize_at_scale(probe.window(), PhysicalSize::new(2880, 1800), scale);
            assert_eq!(probe.window().size(), PhysicalSize::new(2880, 1800));
            assert_eq!(probe.get_viewport_width(), 2880.0 / scale);
            let physical =
                slint::PhysicalPosition::new((120.0 * scale) as i32, (70.0 * scale) as i32);
            let position = physical.to_logical(scale);
            for event in [
                slint::platform::WindowEvent::PointerMoved { position },
                slint::platform::WindowEvent::PointerPressed {
                    position,
                    button: slint::platform::PointerEventButton::Left,
                },
                slint::platform::WindowEvent::PointerReleased {
                    position,
                    button: slint::platform::PointerEventButton::Left,
                },
            ] {
                probe.window().dispatch_event(event);
            }
        }
        assert_eq!(hits.get(), LEVELS.len());
    }
}
