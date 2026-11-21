use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};

slint::include_modules!();

fn move_to(win: &slint::Window, x: f32, y: f32) {
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(x, y),
    });
    // Let the change handlers that publish the drop target run.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn press(win: &slint::Window, x: f32, y: f32) {
    move_to(win, x, y);
    win.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(x, y),
        button: PointerEventButton::Left,
    });
}

fn release(win: &slint::Window, x: f32, y: f32) {
    win.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(x, y),
        button: PointerEventButton::Left,
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

type Adds = Rc<RefCell<Vec<(String, String, String)>>>;

/// 1440x900, logged in, "Fixture track" playing, eight playlists of which the
/// first five are the recent ones.
fn fixture() -> (MainWindow, Adds, Rc<Cell<u32>>) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_has_track(true);
    app.set_current_id("123".into());
    app.set_np_title("Fixture track".into());
    app.set_np_artist("Fixture artist".into());
    let playlists: Vec<CardData> = (0..8)
        .map(|i| CardData {
            id: format!("p{i}").into(),
            kind: "playlist".into(),
            title: format!("Playlist {i}").into(),
            ..Default::default()
        })
        .collect();
    app.set_own_playlists(ModelRc::new(VecModel::from(playlists)));
    app.set_recent_playlists(5);

    let adds: Adds = Rc::default();
    app.on_add_track_to_playlist({
        let adds = adds.clone();
        move |track, title, playlist| {
            adds.borrow_mut()
                .push((track.into(), title.into(), playlist.into()))
        }
    });
    let opened = Rc::new(Cell::new(0));
    app.on_open_now_playing({
        let opened = opened.clone();
        move || opened.set(opened.get() + 1)
    });
    app.on_open_now_playing_album(|| panic!("a drag by the name must not open the album"));
    (ui, adds, opened)
}

// Playlist rows are 57px, and 17px separate the recent rows from the rest.
const ROW: f32 = 57.;

/// Where the playlist list starts, found with a drag down the panel: below the
/// traffic lights and the heading, and in tests below the in-window menu bar.
fn list_top(win: &slint::Window, app: &App) -> f32 {
    press(win, 46., 850.);
    move_to(win, 120., 850.);
    assert!(app.get_drag_active());
    let top = (60..400)
        .find(|y| {
            move_to(win, 120., *y as f32);
            app.get_drop_target() == "p0"
        })
        .expect("the first playlist is somewhere down the panel") as f32;
    app.invoke_drag_cancel();
    release(win, 120., top);
    top
}

#[test]
fn artwork_dragged_onto_a_playlist_adds_the_track() {
    let (ui, adds, opened) = fixture();
    let app = ui.global::<App>();
    let win = ui.window();
    let top = list_top(win, &app);
    assert!(adds.borrow().is_empty());

    // A plain click on the artwork still opens Now Playing.
    press(win, 46., 850.);
    release(win, 46., 850.);
    assert_eq!(opened.get(), 1);
    assert!(!app.get_drag_active());

    // A few pixels are not a drag yet.
    press(win, 46., 850.);
    move_to(win, 49., 851.);
    assert!(!app.get_drag_active());
    move_to(win, 60., 850.);
    assert!(
        app.get_drag_active(),
        "moving away turns the press into a drag"
    );
    assert_eq!(app.get_drag_track_id(), "123");
    assert_eq!(app.get_drag_title(), "Fixture track");
    assert_eq!(app.get_drop_target(), "", "still over the player bar");

    // Over the third recent playlist.
    move_to(win, 120., top + 2. * ROW + 20.);
    assert!(
        app.get_drag_active(),
        "the drag survives leaving the artwork"
    );
    assert_eq!(app.get_drop_target(), "p2");
    assert_eq!(app.get_drop_title(), "Playlist 2");

    // On the hairline between the recent playlists and the rest: nothing.
    move_to(win, 120., top + 5. * ROW + 8.);
    assert_eq!(app.get_drop_target(), "");
    // Below it the rows are shifted by the gap.
    move_to(win, 120., top + 5. * ROW + 17. + 20.);
    assert_eq!(app.get_drop_target(), "p5");
    // Beside the list: nothing.
    move_to(win, 400., top + 20.);
    assert_eq!(app.get_drop_target(), "");

    move_to(win, 120., top + 20.);
    assert_eq!(app.get_drop_target(), "p0");
    release(win, 120., top + 20.);
    assert_eq!(
        adds.borrow().as_slice(),
        [(
            "123".to_string(),
            "Fixture track".to_string(),
            "p0".to_string()
        )]
    );
    assert!(!app.get_drag_active());
    assert_eq!(opened.get(), 1, "the drop is not a click on the artwork");

    // The next plain click works again.
    press(win, 46., 850.);
    release(win, 46., 850.);
    assert_eq!(opened.get(), 2);
}

#[test]
fn a_drag_let_go_elsewhere_or_cancelled_adds_nothing() {
    let (ui, adds, opened) = fixture();
    let app = ui.global::<App>();
    let win = ui.window();
    let top = list_top(win, &app);

    // Let go over the page.
    press(win, 46., 850.);
    move_to(win, 700., 400.);
    assert!(app.get_drag_active());
    release(win, 700., 400.);
    assert!(!app.get_drag_active());

    // Dragged out and back onto the artwork: still not a click.
    press(win, 46., 850.);
    move_to(win, 300., 600.);
    move_to(win, 46., 850.);
    release(win, 46., 850.);
    assert!(!app.get_drag_active());
    assert_eq!(opened.get(), 0);

    // Esc over a playlist, then the button comes up.
    press(win, 46., 850.);
    move_to(win, 120., top + 20.);
    assert_eq!(app.get_drop_target(), "p0");
    app.invoke_drag_cancel();
    assert!(!app.get_drag_active());
    move_to(win, 120., top + ROW + 20.);
    assert!(
        !app.get_drag_active(),
        "moving on does not restart the drag"
    );
    release(win, 120., top + ROW + 20.);

    assert!(adds.borrow().is_empty());
    assert_eq!(opened.get(), 0);

    // Afterwards everything is back to normal.
    press(win, 46., 850.);
    release(win, 46., 850.);
    assert_eq!(opened.get(), 1);
}

#[test]
fn track_name_in_the_player_bar_drags_too() {
    let (ui, adds, _) = fixture();
    let app = ui.global::<App>();
    let win = ui.window();
    let top = list_top(win, &app);

    // The name is the first line right of the artwork. One jump lands on a
    // playlist: the panel appears with the pointer already over it.
    press(win, 100., 842.);
    move_to(win, 120., top + 3. * ROW + 20.);
    assert!(app.get_drag_active());
    assert_eq!(app.get_drop_target(), "p3");
    release(win, 120., top + 3. * ROW + 20.);
    assert_eq!(adds.borrow().len(), 1);
    assert_eq!(adds.borrow()[0].2, "p3");
}

#[test]
fn track_row_drags_to_a_playlist() {
    let (ui, adds, _) = fixture();
    let app = ui.global::<App>();
    let win = ui.window();
    let top = list_top(win, &app);
    app.set_view("tracks".into());
    app.set_tracks(ModelRc::new(VecModel::from(
        (0..3)
            .map(|i| TrackData {
                id: format!("t{i}").into(),
                title: format!("Row track {i}").into(),
                artist: "Row artist".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    let played = Rc::new(Cell::new(0));
    app.on_play_track({
        let played = played.clone();
        move |_| played.set(played.get() + 1)
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));

    // Find the first row by pressing down the page until a drag starts.
    let mut row_y = None;
    for y in (150..700).step_by(10) {
        press(win, 520., y as f32);
        move_to(win, 500., y as f32);
        let dragging = app.get_drag_active();
        if dragging && row_y.is_none() {
            row_y = Some(y as f32);
            assert_eq!(app.get_drag_track_id(), "t0");
            assert_eq!(app.get_drag_title(), "Row track 0");
            move_to(win, 120., top + ROW + 20.);
            assert_eq!(app.get_drop_target(), "p1");
            release(win, 120., top + ROW + 20.);
            break;
        }
        release(win, 500., y as f32);
    }
    assert!(row_y.is_some(), "no track row started a drag");
    assert_eq!(
        adds.borrow().as_slice(),
        [(
            "t0".to_string(),
            "Row track 0".to_string(),
            "p1".to_string()
        )]
    );
    assert_eq!(played.get(), 0, "dragging a row does not play it");
}

#[test]
fn track_drops_on_own_playlist_cards_of_the_playlists_page() {
    let (ui, adds, _) = fixture();
    let app = ui.global::<App>();
    let win = ui.window();
    let top = list_top(win, &app);
    let card = |id: &str, kind: &str, by: &str| CardData {
        id: id.into(),
        kind: kind.into(),
        title: format!("Card {id}").into(),
        by: by.into(),
        ..Default::default()
    };
    // A folder and someone else's playlist sit between the user's own.
    let cards = ModelRc::new(VecModel::from(vec![
        card("f", "folder", ""),
        card("g0", "playlist", "Me"),
        card("x", "playlist", "TIDAL"),
        card("g1", "playlist", "Me"),
        card("g2", "playlist", "Me"),
        card("g3", "playlist", "Me"),
    ]));
    app.set_view("grid".into());
    app.set_grid_cards(cards.clone());
    app.set_grid_groups(ModelRc::new(VecModel::from(vec![SectionData {
        cards,
        ..Default::default()
    }])));
    app.on_open_card(|_| panic!("a drop is not a click on the card"));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));

    // Sweep the page with the track in hand and note what each spot targets.
    press(win, 46., 850.);
    move_to(win, 120., 850.);
    let mut seen = std::collections::BTreeMap::new();
    for y in (150..800).step_by(30) {
        for x in (280..1420).step_by(30) {
            move_to(win, x as f32, y as f32);
            let target = app.get_drop_target().to_string();
            if !target.is_empty() {
                assert_eq!(app.get_drop_source(), "grid");
                assert_eq!(app.get_drop_title(), format!("Card {target}"));
                seen.entry(target).or_insert((x as f32, y as f32));
            }
        }
    }
    assert_eq!(
        seen.keys().map(String::as_str).collect::<Vec<_>>(),
        ["g0", "g1", "g2", "g3"],
        "only the user's own playlists take a track"
    );

    // From a card to a row of the panel and back: each move has one target.
    let (x, y) = seen["g1"];
    move_to(win, 120., top + 20.);
    assert_eq!(app.get_drop_target(), "p0");
    assert_eq!(app.get_drop_source(), "panel");
    move_to(win, x, y);
    assert_eq!(app.get_drop_target(), "g1");
    // Off the card, onto the top bar: nothing.
    move_to(win, x, 30.);
    assert_eq!(app.get_drop_target(), "");

    move_to(win, x, y);
    release(win, x, y);
    assert_eq!(
        adds.borrow().as_slice(),
        [(
            "123".to_string(),
            "Fixture track".to_string(),
            "g1".to_string()
        )]
    );
    assert!(!app.get_drag_active());
}
