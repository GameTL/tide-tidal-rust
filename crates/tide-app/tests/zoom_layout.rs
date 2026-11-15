use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize};
use std::{cell::Cell, rc::Rc, time::Duration};

slint::include_modules!();

fn click(window: &slint::Window, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

#[test]
fn zoomed_minimum_window_keeps_navigation_search_and_transport_reachable() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let window = ui.window();
    ui.show().unwrap();
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_view("stub".into());
    let plays = Rc::new(Cell::new(0));
    app.on_toggle_play({
        let plays = plays.clone();
        move || plays.set(plays.get() + 1)
    });
    let navigations = Rc::new(Cell::new(0));
    app.on_navigate({
        let count = navigations.clone();
        move |key| {
            assert_eq!(key, "home");
            count.set(count.get() + 1);
        }
    });
    for percent in [50, 75, 100, 125, 150, 175, 200, 100] {
        let zoom = percent as f32 / 100.0;
        let width = 960.0 / zoom;
        let height = 620.0 / zoom;
        app.set_zoom_percent(percent);
        window.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: 2.0 * zoom,
        });
        window.dispatch_event(WindowEvent::Resized {
            size: LogicalSize::new(width, height),
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
        window.dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(0.0, 0.0),
        });
        assert_eq!(app.get_compact_layout(), width < 960.0, "{percent}%");
        let before = plays.get();
        click(window, width / 2.0, height - app.get_player_height() + 36.0);
        assert_eq!(plays.get(), before + 1, "transport at {percent}%");
        let header = if app.get_sidebar_compact() {
            90.0_f32.max(90.0 / zoom)
        } else {
            64.0
        };
        let before = navigations.get();
        // The test backend renders its menu bar inside the window; AppKit does not.
        click(window, 36.0, app.get_pane_y() + header + 20.0);
        assert_eq!(navigations.get(), before + 1, "sidebar at {percent}%");
        app.set_search_text("".into());
        app.set_focus_search(app.get_focus_search() + 1);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        window.dispatch_event(WindowEvent::KeyPressed {
            text: "zoom".into(),
        });
        window.dispatch_event(WindowEvent::KeyReleased {
            text: "zoom".into(),
        });
        assert_eq!(app.get_search_text(), "zoom", "search at {percent}%");
    }
}
