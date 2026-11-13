use std::cell::Cell;
use std::rc::Rc;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize};

slint::include_modules!();

fn click(win: &slint::Window, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button });
    win.dispatch_event(WindowEvent::PointerReleased { position, button });
}

#[test]
fn debug_button_toggles_and_leaves_the_window_clickable() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_can_go_back(true);
    let backs = Rc::new(Cell::new(0));
    app.on_back({
        let backs = backs.clone();
        move || backs.set(backs.get() + 1)
    });

    // The page pane starts below the in-window menu bar the test backend
    // draws; its geometry is published once the first event lays the window out.
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(1., 1.),
    });
    let (pane_x, top) = (app.get_pane_x(), app.get_pane_y());
    assert!(pane_x > 0.);

    // Centre of the debug circle in the top bar.
    let (debug_x, debug_y) = (1440. - 56. - 17.5, top + 32.);
    click(win, debug_x, debug_y);
    assert!(app.get_debug_open());

    // The rest of the window still takes clicks while the panel is open.
    click(win, pane_x + 18. + 4. + 17., top + 32.);
    assert_eq!(backs.get(), 1);
    assert!(app.get_debug_open());

    // Clicks on the panel do not fall through or close it.
    click(win, 1440. - 13. - 180., top + 58. + 40.);
    assert!(app.get_debug_open());

    click(win, debug_x, debug_y);
    assert!(!app.get_debug_open());

    app.set_show_now_playing(true);
    // The same debug circle is immediately left of Lyrics in Now Playing.
    let now_playing_debug_x = 1440. - 16. - 34. - 10. - 68. - 10. - 17.5;
    click(win, now_playing_debug_x, top + 32.);
    assert!(app.get_debug_open());
    assert!(app.get_show_now_playing());
    click(win, 1200., top + 100.);
    assert!(app.get_debug_open(), "the panel stays above Now Playing");
    click(win, now_playing_debug_x, top + 32.);
    assert!(!app.get_debug_open());
    click(win, 1440. - 16. - 17., top + 32.);
    assert!(
        !app.get_show_now_playing(),
        "the collapse control still works"
    );
}

#[test]
fn error_alert_shows_one_message_and_dismisses() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);

    app.set_alert_text("Could not play Dream Alone: Decoder error".into());
    app.set_alert_text("An error occurred on audio stream".into());
    assert_eq!(app.get_alert_text(), "An error occurred on audio stream");

    // Close button: right edge of the 520px alert sitting 16px above the player bar.
    let alert_right = (1440. + 520.) / 2.;
    click(win, alert_right - 4. - 14., 900. - 96. - 16. - 20.);
    assert_eq!(app.get_alert_text(), "");
}

#[test]
fn grid_page_filters_and_new_playlist_dialog_submits() {
    use slint::{ModelRc, VecModel};
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_view("grid".into());
    app.set_page_title("Playlists".into());
    app.set_grid_kind("playlists".into());
    let card = |title: &str| CardData {
        id: title.into(),
        kind: "playlist".into(),
        title: title.into(),
        ..Default::default()
    };
    let cards = ModelRc::new(VecModel::from(vec![card("One"), card("Two")]));
    app.set_grid_cards(cards.clone());
    app.set_grid_groups(ModelRc::new(VecModel::from(vec![SectionData {
        title: "Me".into(),
        layout: "cards".into(),
        cards,
        tracks: ModelRc::default(),
    }])));

    let opened = Rc::new(Cell::new(0));
    app.on_open_card({
        let opened = opened.clone();
        move |_| opened.set(opened.get() + 1)
    });
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(1., 1.),
    });
    let (pane_x, top) = (app.get_pane_x(), app.get_pane_y());

    // Artwork-size slider sits on the right of the title row; the leftmost tick is 0.
    let size = Rc::new(Cell::new(-1i32));
    app.on_set_grid_size({
        let size = size.clone();
        move |step| size.set(step)
    });
    let pane_w = app.get_pane_width();
    let pad = ((pane_w - 900.0) * 0.12).clamp(24.0, 90.0);
    let slider_left = pane_x + pane_w - pad - 128.0;
    let slider_y = top + 64.0 + 8.0 + 19.0;
    click(win, slider_left + 22.0, slider_y);
    assert_eq!(size.get(), 0);

    // Typing in the filter box reports the text.
    let filtered = Rc::new(std::cell::RefCell::new(String::new()));
    app.on_filter({
        let filtered = filtered.clone();
        move |text| *filtered.borrow_mut() = text.to_string()
    });
    // Filter box: 64 (top bar) + 8 + 38 + 26 below the pane top, 44 high.
    click(win, pane_x + 300., top + 64. + 8. + 38. + 26. + 22.);
    win.dispatch_event(WindowEvent::KeyPressed { text: "t".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: "t".into() });
    assert_eq!(filtered.borrow().as_str(), "t");
    assert_eq!(app.get_filter_text(), "t");

    // A card under the group header opens.
    click(
        win,
        pane_x + 200.,
        top + 64. + 8. + 38. + 26. + 44. + 36. + 58. + 100.,
    );
    assert_eq!(opened.get(), 1);

    // The name dialog creates on Return and closes.
    let created = Rc::new(std::cell::RefCell::new(None));
    app.on_create_in_collection({
        let created = created.clone();
        move |kind, name| *created.borrow_mut() = Some((kind.to_string(), name.to_string()))
    });
    app.set_create_kind("folder".into());
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(2., 2.),
    });
    for key in ["R", "o", "c", "k"] {
        win.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        win.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    }
    win.dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Return.into(),
    });
    win.dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Return.into(),
    });
    assert_eq!(
        created.borrow().clone(),
        Some(("folder".to_string(), "Rock".to_string()))
    );
    assert_eq!(app.get_create_kind(), "");
}
