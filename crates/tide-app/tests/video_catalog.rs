use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, Model, ModelRc, VecModel};

slint::include_modules!();

fn click(window: &slint::Window, x: f32, y: f32, button: PointerEventButton) {
    let position = LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
}

fn advance(window: &slint::Window, millis: u64) {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(millis));
    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(700., 500.),
    });
}

fn fixture(view: &str, width: f32) -> MainWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = MainWindow::new().unwrap();
    window.show().unwrap();
    window.window().dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(width, 900.),
    });
    let app = window.global::<App>();
    app.set_logged_in(true);
    app.set_view(view.into());
    app.set_page_title("Fixture artist".into());
    app.set_page_biography("Fixture biography".into());
    app.set_tracks(ModelRc::new(VecModel::from(vec![TrackData {
        id: "track-1".into(),
        title: "Audio track".into(),
        ..Default::default()
    }])));
    app.on_play_track(|_| panic!("Video tabs must not play hidden audio rows"));
    advance(window.window(), 100);
    window
}

fn videos(count: usize) -> ModelRc<CardData> {
    ModelRc::new(VecModel::from(
        (0..count)
            .map(|index| CardData {
                id: format!("video-{index}").into(),
                kind: "video".into(),
                title: format!("Video {index}").into(),
                subtitle: "Fixture artist".into(),
                detail: "3 MIN".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    ))
}

fn left(app: &App) -> f32 {
    app.get_pane_x() + ((app.get_pane_width() - 900.) * 0.12).clamp(24., 89.)
}

fn artist_tabs_y(app: &App) -> f32 {
    let pad = ((app.get_pane_width() - 900.) * 0.12).clamp(24., 89.);
    app.get_pane_y() + 64. + ((app.get_pane_width() - 2. * pad) * 0.28).clamp(218., 352.) + 22.
}

#[test]
fn artist_video_tab_opens_cards_and_menus_and_preserves_about() {
    let window = fixture("artist", 1440.);
    let app = window.global::<App>();
    let native = window.window();
    app.set_artist_videos(videos(4));
    let opened = Rc::new(Cell::new(0));
    app.on_open_card({
        let opened = opened.clone();
        move |card| {
            assert_eq!(card.kind, "video");
            assert_eq!(card.id, "video-0");
            opened.set(opened.get() + 1);
        }
    });
    let menus = Rc::new(Cell::new(0));
    app.on_request_card_menu({
        let menus = menus.clone();
        move |card, _, _| {
            assert_eq!(card.kind, "video");
            assert_eq!(card.id, "video-0");
            menus.set(menus.get() + 1);
        }
    });
    click(
        native,
        left(&app) + 125.,
        artist_tabs_y(&app) + 23.,
        PointerEventButton::Left,
    );
    advance(native, 100);
    assert_eq!(app.get_artist_tab(), 2);
    let card_y = artist_tabs_y(&app) + 62. + 50. + 24.;
    click(native, left(&app) + 80., card_y, PointerEventButton::Right);
    assert_eq!(menus.get(), 1);
    assert_eq!(opened.get(), 0);
    click(native, left(&app) + 80., card_y, PointerEventButton::Left);
    assert_eq!(opened.get(), 1);
    click(
        native,
        left(&app) + 195.,
        artist_tabs_y(&app) + 23.,
        PointerEventButton::Left,
    );
    assert_eq!(app.get_artist_tab(), 3);
    assert_eq!(app.get_page_biography(), "Fixture biography");
    click(
        native,
        left(&app) + 58.,
        artist_tabs_y(&app) + 23.,
        PointerEventButton::Left,
    );
    assert_eq!(app.get_artist_tab(), 1);
    click(
        native,
        left(&app) + 8.,
        artist_tabs_y(&app) + 23.,
        PointerEventButton::Left,
    );
    assert_eq!(app.get_artist_tab(), 0);
    assert_eq!(app.get_tracks().row_count(), 1);
}

#[test]
fn search_video_filter_works_without_tracks_at_minimum_width() {
    let window = fixture("search", 960.);
    let app = window.global::<App>();
    let native = window.window();
    app.set_tracks(ModelRc::default());
    app.set_search_videos(videos(3));
    let opened = Rc::new(Cell::new(false));
    app.on_open_card({
        let opened = opened.clone();
        move |card| {
            assert_eq!(card.id, "video-0");
            assert_eq!(card.kind, "video");
            opened.set(true);
        }
    });
    let tabs_y = app.get_pane_y() + 64. + 58.;
    click(
        native,
        left(&app) + 64.,
        tabs_y + 23.,
        PointerEventButton::Left,
    );
    advance(native, 100);
    assert_eq!(app.get_search_tab(), 1);
    click(
        native,
        left(&app) + 80.,
        tabs_y + 62. + 50. + 24.,
        PointerEventButton::Left,
    );
    assert!(opened.get());
    click(
        native,
        left(&app) + 8.,
        tabs_y + 23.,
        PointerEventButton::Left,
    );
    assert_eq!(app.get_search_tab(), 0);
    assert_eq!(app.get_search_videos().row_count(), 3);
}

#[test]
fn video_grid_requests_visible_art_and_handles_loading_empty_and_error() {
    let window = fixture("search", 1440.);
    let app = window.global::<App>();
    let native = window.window();
    app.set_search_tab(1);
    app.set_search_videos_status("loading".into());
    advance(native, 100);
    app.set_search_videos_status("error".into());
    advance(native, 100);
    app.set_search_videos_status("".into());
    advance(native, 100);
    let ranges = Rc::new(RefCell::new(Vec::new()));
    app.on_catalog_video_art({
        let ranges = ranges.clone();
        move |artist, from, upto| {
            assert!(!artist);
            ranges.borrow_mut().push((from, upto));
        }
    });
    app.set_search_videos(videos(100));
    advance(native, 100);
    advance(native, 200);
    assert!(
        ranges
            .borrow()
            .iter()
            .any(|&(from, upto)| from == 0 && upto > 0 && upto < 100)
    );
    app.set_scroll_y(-2500.);
    advance(native, 200);
    advance(native, 200);
    assert!(
        ranges
            .borrow()
            .iter()
            .any(|&(from, upto)| from > 0 && upto > from && upto < 100)
    );
}
