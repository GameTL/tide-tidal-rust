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
            app.set_menu_track(TrackData { id, title: "A track".into(), ..Default::default() });
            app.set_menu_x(x);
            app.set_menu_y(y);
            app.set_menu_open(true);
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
    assert!(app.get_menu_open());
    assert_eq!(app.get_menu_track().id, "123");

    click(win, 30., 850., PointerEventButton::Left);
    assert!(!app.get_menu_open());
    press(win, Key::Return);
    assert!(app.get_menu_open(), "closing the menu should return focus to More");
    assert_eq!(open_requests.get(), 2);

    // The next track can start while the menu is open. Sharing still targets
    // the original track, and keyboard navigation skips unavailable actions.
    app.set_current_id("456".into());
    press(win, Key::DownArrow); // Add to playlist
    press(win, Key::DownArrow); // My Collection
    press(win, Key::DownArrow); // Share (album and artist are unavailable)
    press(win, Key::RightArrow);
    press(win, Key::Escape);
    assert!(app.get_menu_open(), "Escape closes Share before the track menu");
    press(win, Key::RightArrow);
    press(win, Key::Return);
    assert_eq!(copied.borrow().as_deref(), Some("123"));
    assert!(!app.get_menu_open());

    click(win, 46., 850., PointerEventButton::Right);
    assert!(app.get_menu_open());
    assert_eq!(app.get_menu_track().id, "456");
    press(win, Key::Escape);
    assert!(!app.get_menu_open());
}

#[test]
fn scrubber_seeks_on_release_and_toggles_remaining_time() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(1440., 900.) });

    assert_eq!(app.invoke_format_time(0), "0:00");
    assert_eq!(app.invoke_format_time(5), "0:05");
    assert_eq!(app.invoke_format_time(188), "3:08");
    assert_eq!(app.invoke_format_time(3723), "62:03");

    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_duration_secs(200);
    app.set_total("3:20".into());

    let sought = Rc::new(Cell::new(None::<f32>));
    app.on_seek({
        let sought = sought.clone();
        move |v| sought.set(Some(v))
    });

    // Window 1440x900 -> PlayerBar y=[804..900], Scrubber y=[868..888], center y=878.
    // bar-w = 1440 * 0.354 = 509.76, x=[465.12..974.88].
    let start = LogicalPosition::new(500., 878.);
    let drag_to = LogicalPosition::new(720., 878.); // ~50% across bar
    win.dispatch_event(WindowEvent::PointerMoved { position: start });
    win.dispatch_event(WindowEvent::PointerPressed { position: start, button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerMoved { position: drag_to });
    assert!(sought.get().is_none(), "Scrubber should preview while dragging and seek only on release");

    win.dispatch_event(WindowEvent::PointerReleased { position: drag_to, button: PointerEventButton::Left });
    let frac = sought.get().expect("seek emitted on release");
    assert!((frac - 0.5).abs() < 0.02, "expected ~0.5, got {frac}");

    // Right timestamp sits at (1440 + 509.76)/2 + 14 ≈ 988.88, y=878.
    assert!(!app.get_show_remaining());
    click(win, 998., 878., PointerEventButton::Left);
    assert!(app.get_show_remaining(), "clicking total duration toggles remaining-time mode");
}

#[test]
fn right_click_on_a_card_opens_the_shared_menu_with_its_own_entries() {
    use slint::{ModelRc, VecModel};
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(1440., 900.) });
    app.set_logged_in(true);
    app.set_view("grid".into());
    app.set_grid_kind("albums".into());
    let album = CardData { id: "9".into(), kind: "album".into(), title: "An album".into(), ..Default::default() };
    let cards = ModelRc::new(VecModel::from(vec![album]));
    app.set_grid_cards(cards.clone());
    app.set_grid_groups(ModelRc::new(VecModel::from(vec![SectionData {
        layout: "cards".into(),
        cards,
        ..Default::default()
    }])));
    let playlist = CardData { id: "p1".into(), kind: "playlist".into(), title: "Mine".into(), ..Default::default() };
    app.set_own_playlists(ModelRc::new(VecModel::from(vec![playlist])));

    app.on_request_card_menu({
        let weak = ui.as_weak();
        move |card, x, y| {
            let ui = weak.unwrap();
            let app = ui.global::<App>();
            app.set_menu_kind(card.kind.clone());
            app.set_menu_card(card);
            app.set_menu_x(x);
            app.set_menu_y(y);
            app.set_menu_open(true);
        }
    });
    app.on_open_card(|_| panic!("Secondary click must not open the card"));
    let actions = Rc::new(RefCell::new(Vec::new()));
    app.on_menu_action({
        let actions = actions.clone();
        move |action, arg| actions.borrow_mut().push((action.to_string(), arg.to_string()))
    });

    win.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(1., 1.) });
    let (pane_x, top) = (app.get_pane_x(), app.get_pane_y());
    // First card of the grid: below the title, filter box and spacing.
    click(win, pane_x + 200., top + 64. + 8. + 38. + 26. + 44. + 36. + 100., PointerEventButton::Right);
    assert!(app.get_menu_open());
    assert_eq!(app.get_menu_kind(), "album");
    assert_eq!(app.get_menu_card().id, "9");

    // Play now is the first entry.
    press(win, Key::Return);
    assert!(!app.get_menu_open());
    assert_eq!(actions.borrow().last().cloned(), Some(("play".to_string(), String::new())));

    // Add to playlist opens a flyout listing the user's playlists.
    click(win, pane_x + 200., top + 64. + 8. + 38. + 26. + 44. + 36. + 100., PointerEventButton::Right);
    press(win, Key::DownArrow);
    press(win, Key::DownArrow);
    press(win, Key::RightArrow);
    assert_eq!(app.get_menu_sub(), "playlists");
    press(win, Key::Return);
    assert_eq!(actions.borrow().last().cloned(), Some(("add-to-playlist".to_string(), "p1".to_string())));
    assert!(!app.get_menu_open());
}
