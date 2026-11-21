use slint::platform::WindowEvent;
use slint::{ComponentHandle, LogicalSize};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

fn key(win: &slint::Window, text: &str) {
    win.dispatch_event(WindowEvent::KeyPressed { text: text.into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: text.into() });
}

#[test]
fn space_controls_restored_playback_before_any_click_but_preserves_text_input() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    // Match boot: login and the paused track are restored after window creation.
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_playing(false);
    let toggles = Rc::new(Cell::new(0));
    app.on_toggle_play({
        let weak = ui.as_weak();
        let toggles = toggles.clone();
        move || {
            toggles.set(toggles.get() + 1);
            let ui = weak.unwrap();
            let app = ui.global::<App>();
            app.set_playing(!app.get_playing());
        }
    });

    // No mouse events and no manual focus call: the first key must work.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    key(win, " ");
    assert_eq!(toggles.get(), 1, "Space must work immediately after launch");
    assert!(app.get_playing());
    key(win, " ");
    assert_eq!(toggles.get(), 2);
    assert!(!app.get_playing());

    app.set_menu_open(true);
    key(win, " ");
    assert_eq!(toggles.get(), 2, "an open menu owns Space");
    app.set_menu_open(false);

    app.set_focus_search(app.get_focus_search() + 1);
    key(win, "hello");
    assert!(app.get_edit_focus() > 0);
    key(win, " ");
    key(win, "world");
    assert_eq!(toggles.get(), 2, "typing spaces must not toggle playback");
    let searches = Rc::new(Cell::new(0));
    app.on_search({
        let searches = searches.clone();
        move |query| {
            assert_eq!(query, "hello world");
            searches.set(searches.get() + 1);
        }
    });
    let enter: slint::SharedString = slint::platform::Key::Return.into();
    key(win, enter.as_str());
    assert_eq!(searches.get(), 1);
    assert!(ui.get_typing(), "the search field still has focus");

    // A click on the field itself keeps focus; a click elsewhere (a track row)
    // lets go, so Space plays and pauses again.
    let click = |x: f32, y: f32| {
        app.set_click_x(x);
        app.set_click_y(y);
        app.set_click_seq(app.get_click_seq() + 1);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    };
    // The header search field spans x 1081..1299, y 62..82 at this size.
    click(1150., 72.);
    assert!(ui.get_typing(), "a click inside the search field keeps it focused");
    click(700., 500.);
    assert!(!ui.get_typing(), "a click outside the search field takes its focus");
    assert_eq!(app.get_edit_focus(), 0);
}
