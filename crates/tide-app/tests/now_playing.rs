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
    // line rests 38% of the way down; the [ Lyrics | Queue ] pill sits left of
    // the search field (group width 309px: 3px pads around four 68px
    // segments, then 31px to the divider and past it).
    let top = 40.;
    let (pill_x, pill_y) = (1440. - 407. - 309. + 3. + 34., top + 32.);
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

#[test]
fn queue_panel_toggle_play_menu_and_player_bar_button() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(1440., 900.) });
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("now".into());
    app.set_np_title("Current Track".into());
    app.set_np_artist("Current Artist".into());
    app.set_np_context("My OG / Nostalgia Style".into());
    app.set_lyrics(ModelRc::new(VecModel::from(vec![LyricLine {
        text: "A line".into(),
        time: 0,
    }])));
    app.set_lyrics_synced(true);
    app.set_np_queue(ModelRc::new(VecModel::from(
        (0..20)
            .map(|i| TrackData {
                id: format!("q-{i}").into(),
                title: format!("Queued Track {i}").into(),
                artist: format!("Artist {i}").into(),
                album: format!("Album {i}").into(),
                duration: "3:45".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));

    let navs = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_navigate({
        let navs = navs.clone();
        move |k| navs.borrow_mut().push(k.to_string())
    });
    let played = Rc::new(std::cell::RefCell::new(Vec::<i32>::new()));
    app.on_play_queue_index({
        let played = played.clone();
        move |i| played.borrow_mut().push(i)
    });
    let menus = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_request_track_menu({
        let menus = menus.clone();
        move |id, _, _| menus.borrow_mut().push(id.to_string())
    });
    let art_windows = Rc::new(std::cell::RefCell::new(Vec::<(i32, i32)>::new()));
    app.on_np_queue_visible({
        let art_windows = art_windows.clone();
        move |first, count| art_windows.borrow_mut().push((first, count))
    });

    app.set_show_lyrics(true);
    app.set_show_now_playing(true);
    advance(win, 700);

    let top = 40.;
    let group_x = 1440. - 407. - 309.;
    let (lyrics_x, queue_x, pill_y) = (group_x + 3. + 34., group_x + 3. + 68. + 34., top + 32.);

    // Switch from Lyrics to Queue via the top segmented pill.
    click(win, queue_x, pill_y);
    assert!(app.get_show_queue());
    assert!(!app.get_show_lyrics());
    advance(win, 700);
    assert!(
        art_windows.borrow().iter().any(|&(_, count)| count > 0),
        "opening Queue requests cover artwork for visible rows"
    );

    // QueueColumn aligns with the artwork's top edge (on-y ~118.8px + 40px menu
    // bar = ~158.8px), with a 56px context header, 4px pad, and 58px rows.
    let col_x = 755.;
    let col_w = 560.;
    let col_top = top + 118.8;
    let header_h = 56.;
    let first_row_y = col_top + header_h + 4. + 29.;

    // Left-clicking row 1 jumps to index 1.
    click(win, col_x + 160., first_row_y + 58.);
    assert_eq!(*played.borrow(), [1]);

    // Clicking the More button on row 0 opens that track's context menu.
    click(win, col_x + col_w - 8. - 22., first_row_y);
    assert_eq!(*menus.borrow(), ["q-0"]);

    // Right-clicking row 2 opens its context menu too.
    let pos = LogicalPosition::new(col_x + 160., first_row_y + 58. * 2.);
    win.dispatch_event(WindowEvent::PointerMoved { position: pos });
    win.dispatch_event(WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Right });
    win.dispatch_event(WindowEvent::PointerReleased { position: pos, button: PointerEventButton::Right });
    assert_eq!(*menus.borrow(), ["q-0", "q-2"]);

    // Clicking the Lyrics segment switches back to Lyrics and releases queue art.
    click(win, lyrics_x, pill_y);
    assert!(!app.get_show_queue());
    assert!(app.get_show_lyrics());
    advance(win, 700);
    assert_eq!(
        art_windows.borrow().last().copied(),
        Some((0, 0)),
        "leaving Queue drops decoded queue covers"
    );

    // Clicking the overlay player bar's Queue button toggles Queue in-place
    // instead of navigating away to the Queue page.
    let bar_y = 900. - app.get_player_height() + 48.;
    let queue_btn_x = 1440. - 20. - 86. - 12. - 8. - 16. - 5. * 34. - 4. * 8. + 17.;
    let mut toggled = false;
    for dx in (-40..=120).step_by(8) {
        click(win, queue_btn_x + dx as f32, bar_y);
        if app.get_show_queue() {
            toggled = true;
            break;
        }
    }
    assert!(toggled, "overlay player bar Queue icon toggles Queue in-place");
    assert!(navs.borrow().is_empty(), "Queue icon in Now Playing must not navigate away");
    assert!(!app.get_show_lyrics());

    // Similar tracks and Credits are the pill's third and fourth segments:
    // each opens in place of Queue, asks for its content, and closes again.
    let opened = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_np_tab_opened({
        let opened = opened.clone();
        move |tab| opened.borrow_mut().push(tab.to_string())
    });
    let (similar_x, credits_x) = (group_x + 3. + 68. * 2. + 34., group_x + 3. + 68. * 3. + 34.);
    click(win, similar_x, pill_y);
    assert_eq!(app.get_np_tab(), "similar");
    assert!(!app.get_show_queue());
    click(win, credits_x, pill_y);
    assert_eq!(app.get_np_tab(), "credits");
    assert_eq!(*opened.borrow(), ["similar", "credits"]);
    click(win, credits_x, pill_y);
    assert_eq!(app.get_np_tab(), "", "the open tab's segment closes it");
    click(win, similar_x, pill_y);
    click(win, lyrics_x, pill_y);
    assert_eq!(app.get_np_tab(), "");
    assert!(app.get_show_lyrics(), "Lyrics takes over from Similar tracks");
}
