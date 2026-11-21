//! The phone layout and the widths around it: which arrangement each width
//! gets, the tab bar and mini player at the bottom, the search on Explore,
//! menus that fit a phone-width window, Now Playing's top row clear of the
//! traffic lights, and the one-row player bar at its narrowest.
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

/// The testing backend draws the menu bar inside the window: content starts here.
const TOP: f32 = 40.;

fn tick() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
}

fn resize(win: &slint::Window, w: f32, h: f32) {
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(w, h) });
    tick();
}

fn hover(win: &slint::Window, x: f32, y: f32) {
    win.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(x, y) });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn click(win: &slint::Window, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

type Log = Rc<RefCell<Vec<String>>>;

fn setup() -> (MainWindow, Log) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    ui.show().unwrap();
    resize(ui.window(), 1440., 900.);
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("t1".into());
    app.set_np_title("Derezzed".into());
    app.set_np_artist("Daft Punk".into());
    app.set_view("home".into());
    app.set_nav_key("home".into());
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let push = |log: &Log, s: String| log.borrow_mut().push(s);
    app.on_navigate({
        let log = log.clone();
        move |key| push(&log, format!("navigate {key}"))
    });
    app.on_open_now_playing({
        let log = log.clone();
        move || push(&log, "now-playing".into())
    });
    app.on_toggle_play({
        let log = log.clone();
        move || push(&log, "play".into())
    });
    app.on_next({
        let log = log.clone();
        move || push(&log, "next".into())
    });
    app.on_cycle_repeat({
        let log = log.clone();
        move || push(&log, "repeat".into())
    });
    app.on_menu_action({
        let log = log.clone();
        move |action, arg| push(&log, format!("{action} {arg}"))
    });
    (ui, log)
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.borrow_mut())
}

#[test]
fn widths_pick_the_arrangement_and_the_dock_replaces_sidebar_and_bar() {
    let (ui, log) = setup();
    let app = ui.global::<App>();
    let win = ui.window();
    for (w, phone, compact) in [(320., true, true), (719., true, true), (720., false, true), (959., false, true), (960., false, false)] {
        resize(win, w, 800.);
        assert_eq!((app.get_phone_layout(), app.get_compact_layout()), (phone, compact), "at {w}");
    }

    // Phone: no sidebar, the pane is the whole width; the dock is the mini
    // player over the tab bar, and pages end above it unless it is glass.
    let (w, h) = (330., 760.);
    resize(win, w, h);
    assert_eq!(app.get_pane_x(), 0.);
    assert_eq!(app.get_pane_width(), w);
    let dock = app.get_dock_height();
    assert_eq!(dock, app.get_mini_height() + app.get_tabs_height());
    assert_eq!(app.get_pane_y() + app.get_pane_height(), h - dock);
    app.set_glass(true);
    tick();
    assert_eq!(app.get_bar_inset(), dock);
    assert_eq!(app.get_pane_y() + app.get_pane_height(), h);
    app.set_glass(false);
    tick();

    // The four tabs, evenly across the bottom.
    let tabs_y = h - app.get_tabs_height() / 2. - 3.;
    for (i, key) in ["home", "feed", "explore", "collection"].iter().enumerate() {
        let slot = (w - 16.) / 4.;
        click(win, 8. + slot * (i as f32 + 0.5), tabs_y);
        assert_eq!(take(&log), [format!("navigate {key}")]);
    }

    // The mini player: play / pause and next on the right, the rest opens
    // Now Playing.
    let mini_y = h - dock + app.get_mini_height() / 2.;
    click(win, w - 16. - 44. - 20., mini_y);
    click(win, w - 12. - 20., mini_y);
    click(win, 140., mini_y);
    assert_eq!(take(&log), ["play", "next", "now-playing"]);

    // Without a track the dock is the tab bar alone.
    app.set_has_track(false);
    tick();
    assert_eq!(app.get_dock_height(), app.get_tabs_height());
    app.set_has_track(true);
    tick();

    // Wider again, the sidebar and the player bar are back.
    resize(win, 1200., 800.);
    assert!(app.get_pane_x() > 0.);
    assert_eq!(app.get_dock_height(), app.get_player_height());
}

#[test]
fn explore_holds_the_search_and_command_f_focuses_it() {
    let (ui, _log) = setup();
    let app = ui.global::<App>();
    let win = ui.window();
    resize(win, 330., 760.);
    // As the controller does for ⌘F in phone layout: Explore, then the bump.
    app.set_view("modules".into());
    app.set_nav_key("explore".into());
    app.set_page_title("Explore".into());
    app.set_search_text("".into());
    app.set_focus_search(app.get_focus_search() + 1);
    tick();
    win.dispatch_event(WindowEvent::KeyPressed { text: "daft".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: "daft".into() });
    assert_eq!(app.get_search_text(), "daft");
    assert_eq!(app.get_focus_search_seen(), app.get_focus_search(), "the field took the request");
}

#[test]
fn a_menu_and_its_flyout_fit_a_phone_width_window() {
    let (ui, log) = setup();
    let app = ui.global::<App>();
    let win = ui.window();
    let (w, h) = (330., 760.);
    resize(win, w, h);
    app.set_own_playlists(ModelRc::new(VecModel::from(
        (0..4)
            .map(|i| CardData { id: format!("p{i}").into(), kind: "playlist".into(), title: format!("Mine {i}").into(), by: "Me".into(), ..Default::default() })
            .collect::<Vec<_>>(),
    )));
    app.set_menu_kind("track".into());
    app.set_menu_track(TrackData { id: "t3".into(), title: "A track".into(), ..Default::default() });
    app.set_menu_x(250.);
    app.set_menu_y(200.);
    app.set_menu_open(true);
    tick();
    // Down the menu until "Add to playlist" (the second entry) opens its flyout.
    let mut y = TOP + 200.;
    while app.get_menu_sub() != "playlists" && y < h {
        hover(win, 60., y);
        y += 6.;
    }
    assert_eq!(app.get_menu_sub(), "playlists", "no flyout");
    let anchor = y - 6.;
    // There is no room beside the menu: the flyout covers it, starting
    // under the entry. Its first playlist takes the click.
    hover(win, 60., anchor + 30.);
    click(win, 60., anchor + 50.);
    assert_eq!(take(&log), ["add-to-playlist p0"]);
}

#[test]
fn now_playing_minimize_clears_the_traffic_lights() {
    let (ui, _log) = setup();
    let app = ui.global::<App>();
    let win = ui.window();
    app.on_open_now_playing(|| {});
    for w in [320., 340., 360., 480., 719.] {
        resize(win, w, 700.);
        app.set_show_now_playing(true);
        tick();
        // The traffic lights take the window's top left; nothing of Now
        // Playing may be there, Minimize is just right of them.
        for x in [10., 40., 70., 78.] {
            click(win, x, TOP + 30.);
            assert!(app.get_show_now_playing(), "something under the traffic lights at {x}, width {w}");
        }
        let mut closed_at = None;
        for x in (80..(w as i32 - 101)).step_by(4) {
            click(win, x as f32, TOP + 32.);
            if !app.get_show_now_playing() {
                closed_at = Some(x);
                break;
            }
        }
        assert!(closed_at.is_some(), "no Minimize in the top row at width {w}");
    }
}

#[test]
fn the_one_row_player_bar_keeps_repeat_and_queue_apart() {
    let (ui, log) = setup();
    let app = ui.global::<App>();
    let win = ui.window();
    for w in [960., 1000., 1059., 1100.] {
        resize(win, w, 800.);
        let row = 800. - app.get_player_height() + 36.;
        // Repeat ends at w/2 + 116; between it and the queue nothing answers.
        click(win, w / 2. + 96., row);
        assert_eq!(take(&log), ["repeat"], "repeat at {w}");
        for x in [w / 2. + 120., w / 2. + 126.] {
            click(win, x, row);
            assert!(take(&log).is_empty(), "the queue reaches the transport at {w} (x {x})");
        }
    }
}
