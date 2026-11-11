use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize};

slint::include_modules!();

fn click(win: &slint::Window, x: f32, y: f32, button: PointerEventButton) {
    let position = LogicalPosition::new(x, y);
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button });
    win.dispatch_event(WindowEvent::PointerReleased { position, button });
}

fn press(win: &slint::Window, key: Key) {
    win.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[test]
fn player_menu_targets_the_original_track_and_restores_focus() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(1440., 900.) });
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("123".into());
    app.set_np_title("Fixture track".into());
    let open_requests = Rc::new(Cell::new(0));
    app.on_request_track_menu({
        let weak = ui.as_weak();
        let requests = open_requests.clone();
        move |id, x, y| {
            requests.set(requests.get() + 1);
            let ui = weak.unwrap();
            let app = ui.global::<App>();
            app.set_track_menu_track(TrackData { id, title: "A track".into(), ..Default::default() });
            app.set_track_menu_x(x);
            app.set_track_menu_y(y);
            app.set_track_menu_open(true);
        }
    });
    app.on_open_now_playing(|| panic!("Secondary click must not open Now Playing"));
    let copied = Rc::new(RefCell::new(None));
    app.on_copy_track_link({
        let copied = copied.clone();
        move |id| *copied.borrow_mut() = Some(id)
    });

    // Secondary click on the player artwork opens the same menu as the More button.
    click(win, 46., 850., PointerEventButton::Right);
    assert!(app.get_track_menu_open());
    assert_eq!(app.get_track_menu_track().id, "123");

    click(win, 30., 850., PointerEventButton::Left);
    assert!(!app.get_track_menu_open());
    press(win, Key::Return);
    assert!(app.get_track_menu_open(), "closing the menu should return focus to More");
    assert_eq!(open_requests.get(), 2);

    // The next track can start while the menu is open. Sharing still targets
    // the original track, and keyboard navigation skips unavailable actions.
    app.set_current_id("456".into());
    press(win, Key::DownArrow);
    press(win, Key::DownArrow);
    press(win, Key::RightArrow);
    press(win, Key::Escape);
    assert!(app.get_track_menu_open(), "Escape closes Share before the track menu");
    press(win, Key::RightArrow);
    press(win, Key::Return);
    assert_eq!(copied.borrow().as_deref(), Some("123"));
    assert!(!app.get_track_menu_open());

    click(win, 46., 850., PointerEventButton::Right);
    assert!(app.get_track_menu_open());
    assert_eq!(app.get_track_menu_track().id, "456");
    press(win, Key::Escape);
    assert!(!app.get_track_menu_open());
}
