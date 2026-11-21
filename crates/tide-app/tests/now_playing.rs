use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

fn advance(win: &slint::Window, millis: u64) {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(millis));
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(700., 500.),
    });
}

fn click(win: &slint::Window, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    win.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn key(win: &slint::Window, key: Key) {
    win.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[test]
fn track_artwork_shortcuts_links_and_shared_header_preserve_playback() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_playing(true);
    app.set_np_title("Fixture track".into());
    app.set_np_artist("Fixture artist".into());
    app.set_np_album_id("album-1".into());
    app.set_np_context("Tracks".into());
    let requests = Rc::new(Cell::new(0));
    app.on_open_now_playing({
        let weak = ui.as_weak();
        let requests = requests.clone();
        move || {
            requests.set(requests.get() + 1);
            let ui = weak.unwrap();
            let app = ui.global::<App>();
            app.set_show_now_playing(!app.get_show_now_playing());
        }
    });
    app.on_toggle_play(|| panic!("Now Playing navigation must preserve playback"));
    advance(win, 300);
    click(win, 46., 850.);
    assert!(app.get_show_now_playing());
    advance(win, 300);
    click(win, 1360., 770.);
    assert!(!app.get_show_now_playing());
    advance(win, 300);
    key(win, Key::Return);
    assert!(app.get_show_now_playing());
    advance(win, 300);
    key(win, Key::Escape);
    assert!(!app.get_show_now_playing());
    advance(win, 300);
    win.dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    win.dispatch_event(WindowEvent::KeyPressed { text: "l".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: "l".into() });
    win.dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    assert!(
        app.get_show_now_playing(),
        "the View menu's command-L shortcut opens Now Playing"
    );
    advance(win, 300);
    let top = app.get_pane_y();
    click(win, 1366.5, top + 32.);
    assert!(app.get_debug_open());
    click(win, 1366.5, top + 32.);
    assert!(!app.get_debug_open());

    let albums = Rc::new(Cell::new(0));
    app.on_open_now_playing_album({
        let weak = ui.as_weak();
        let albums = albums.clone();
        move || {
            albums.set(albums.get() + 1);
            weak.unwrap().global::<App>().set_show_now_playing(false);
        }
    });
    click(win, 70., 833.);
    assert_eq!(albums.get(), 1);
    assert!(!app.get_show_now_playing());
    advance(win, 300);
    key(win, Key::Return);
    advance(win, 300);
    let sources = Rc::new(Cell::new(0));
    app.on_open_now_playing_context({
        let weak = ui.as_weak();
        let sources = sources.clone();
        move || {
            sources.set(sources.get() + 1);
            weak.unwrap().global::<App>().set_show_now_playing(false);
        }
    });
    click(win, 70., 873.);
    assert_eq!(sources.get(), 1);
    assert!(!app.get_show_now_playing());
    advance(win, 300);

    let searches = Rc::new(Cell::new(0));
    app.on_search({
        let searches = searches.clone();
        move |query| {
            assert_eq!(query, "Queen");
            searches.set(searches.get() + 1);
        }
    });
    app.set_focus_search(app.get_focus_search() + 1);
    win.dispatch_event(WindowEvent::KeyPressed {
        text: "Queen".into(),
    });
    assert!(app.get_edit_focus() > 0);
    let before = requests.get();
    key(win, Key::Return);
    assert_eq!(searches.get(), 1);
    assert_eq!(
        requests.get(),
        before,
        "Enter in a text field must not open Now Playing"
    );
    assert!(app.get_playing());
}

#[test]
fn artwork_collapse_survives_page_snapshot_capture() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_has_track(true);
    advance(win, 300);
    app.set_show_now_playing(true);
    advance(win, 300);
    let opens = Rc::new(Cell::new(0));
    app.on_open_now_playing({
        let opens = opens.clone();
        move || opens.set(opens.get() + 1)
    });
    // Navigation snapshots hide overlays, then reveal the destination as the
    // artwork returns to its player slot. Hiding must not reset that surface.
    app.set_capturing(true);
    advance(win, 1);
    app.set_capturing(false);
    app.set_show_now_playing(false);
    advance(win, 10);
    click(win, 46., 850.);
    assert_eq!(
        opens.get(),
        0,
        "the collapsing surface still covers the player slot"
    );
    advance(win, 300);
    click(win, 46., 850.);
    assert_eq!(
        opens.get(),
        1,
        "the slot becomes clickable once the collapse ends"
    );
}

#[test]
fn source_jump_centers_the_requested_track_including_the_last_row() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_view("tracks".into());
    app.set_tracks(ModelRc::new(VecModel::from(
        (0..100)
            .map(|i| TrackData {
                id: i.to_string().into(),
                title: format!("Track {i}").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    advance(win, 1);
    for index in [50, 99] {
        app.set_reveal_track_index(index);
        app.set_reveal_track_revision(app.get_reveal_track_revision() + 1);
        advance(win, 1);
        let viewport = app.get_pane_height() - 64.;
        let expected = viewport / 2. - 279. - (index as f32 + 0.5) * 53.;
        assert!(
            (app.get_scroll_y() - expected).abs() < 1.,
            "row {index}: expected {expected}, got {}",
            app.get_scroll_y()
        );
    }
}

fn wheel(win: &slint::Window, x: f32, y: f32, dy: f32) {
    let position = LogicalPosition::new(x, y);
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerScrolled { position, delta_x: 0., delta_y: dy });
}

/// The Lyrics toggle, the lyric column's click-to-seek and "Sync lyrics".
/// (The arrangements themselves are drawn in now_playing_render.rs; the
/// ElementHandle API that could measure them needs a debug-info build.)
#[test]
fn lyrics_toggle_seek_and_sync() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(1440., 900.) });
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("1".into());
    app.set_np_title("Fixture track".into());
    app.set_lyrics(ModelRc::new(VecModel::from(
        (0..30)
            .map(|i| LyricLine { text: format!("Line number {i} of the fixture").into(), time: 3_000 * i })
            .collect::<Vec<_>>(),
    )));
    app.set_lyrics_synced(true);
    app.set_lyric_index(8);
    let toggles = Rc::new(std::cell::RefCell::new(Vec::new()));
    app.on_set_show_lyrics({
        let toggles = toggles.clone();
        move |on| toggles.borrow_mut().push(on)
    });
    let seeks = Rc::new(std::cell::RefCell::new(Vec::new()));
    app.on_seek_lyric({
        let seeks = seeks.clone();
        move |row| seeks.borrow_mut().push(row)
    });
    app.set_show_now_playing(true);
    advance(win, 700);

    // The window draws its menu bar here: the content starts at y 40 and is
    // 860 high. The lyric column spans x 755..1315, y 104..740; the sung
    // line rests 38% of the way down; the Lyrics pill is left of the
    // search field.
    let top = 40.;
    let (pill_x, pill_y) = (1440. - 407. - 101. + 34., top + 32.);
    let (line_x, anchor) = (790., top + 64. + 636. * 0.38);
    click(win, line_x, anchor);
    assert_eq!(*seeks.borrow(), [8], "the sung line sits at the anchor and seeks");

    // The pill is a toggle, remembered through set-show-lyrics. Without
    // lyrics there is no column to click.
    click(win, pill_x, pill_y);
    assert!(!app.get_show_lyrics());
    advance(win, 700);
    click(win, line_x, anchor);
    assert_eq!(seeks.borrow().len(), 1, "the column left with the lyrics");
    click(win, pill_x, pill_y);
    assert!(app.get_show_lyrics());
    assert_eq!(*toggles.borrow(), [false, true]);
    advance(win, 700);

    // Scrolled by hand, the column stays put: another line is at the anchor.
    wheel(win, line_x, 400., -150.);
    advance(win, 700);
    click(win, line_x, anchor);
    let picked = *seeks.borrow().last().unwrap();
    assert!(picked > 8, "scrolling down shows later lines (got {picked})");
    // Clicking a line goes back to following; so does "Sync lyrics", at the
    // column's bottom right.
    advance(win, 700);
    wheel(win, line_x, 400., -150.);
    advance(win, 700);
    click(win, 1315. - 50., 740. - 24.);
    advance(win, 700);
    click(win, line_x, anchor);
    assert_eq!(*seeks.borrow().last().unwrap(), 8, "Sync lyrics returns to the sung line");
}
