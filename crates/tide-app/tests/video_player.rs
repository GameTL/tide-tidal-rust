use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Image, LogicalPosition, LogicalSize, Rgba8Pixel, SharedPixelBuffer};

slint::include_modules!();

fn click(win: &slint::Window, x: f32, y: f32, button: PointerEventButton) {
    let position = LogicalPosition::new(x, y);
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button });
    win.dispatch_event(WindowEvent::PointerReleased { position, button });
}

fn advance(win: &slint::Window, millis: u64) {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(millis));
    // Resolve layout and animated bindings at the new tick.
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(700., 500.),
    });
}

#[test]
fn video_expands_from_thumbnail_title_and_enter_and_collapses_without_pausing() {
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
    app.set_video_active(true);
    app.set_playing(true);
    app.set_np_title("Fixture video".into());
    app.set_np_artist("Fixture artist".into());
    app.set_video_card(CardData {
        id: "video-123".into(),
        kind: "video".into(),
        ..Default::default()
    });
    let frame = SharedPixelBuffer::<Rgba8Pixel>::new(160, 90);
    app.set_video_frame(Image::from_rgba8(frame));

    let opens = Rc::new(Cell::new(0));
    app.on_open_video({
        let weak = ui.as_weak();
        let opens = opens.clone();
        move || {
            opens.set(opens.get() + 1);
            weak.unwrap().global::<App>().set_video_open(true);
        }
    });
    app.on_close_video({
        let weak = ui.as_weak();
        move || weak.unwrap().global::<App>().set_video_open(false)
    });
    let toggles = Rc::new(Cell::new(0));
    app.on_toggle_play({
        let toggles = toggles.clone();
        move || toggles.set(toggles.get() + 1)
    });
    let menus = Rc::new(Cell::new(0));
    app.on_request_card_menu({
        let menus = menus.clone();
        move |card, _, _| {
            assert_eq!(card.id, "video-123");
            menus.set(menus.get() + 1);
        }
    });
    advance(win, 500);

    // Secondary click stays on the video's context menu, without expanding.
    click(win, 66., 852., PointerEventButton::Right);
    assert_eq!(menus.get(), 1);
    assert!(!app.get_video_open());

    click(win, 66., 852., PointerEventButton::Left);
    assert!(app.get_video_open());
    assert_eq!(opens.get(), 1);

    // The opening starts at the thumbnail, rather than immediately covering
    // this point in the page. Halfway through, the live surface has reached it.
    advance(win, 10);
    click(win, 900., 600., PointerEventButton::Left);
    assert_eq!(toggles.get(), 0);
    advance(win, 120);
    click(win, 900., 600., PointerEventButton::Left);
    assert_eq!(toggles.get(), 1);
    advance(win, 500);

    // Click the pill's left side, outside the chevron itself.
    click(win, 40., 770., PointerEventButton::Left);
    assert!(!app.get_video_open(), "the lower-left pill minimizes video");
    advance(win, 500);
    assert_eq!(toggles.get(), 1, "minimizing must not pause playback");
    assert!(app.get_playing());
    assert_eq!(app.get_video_frame().size().width, 160);

    // The title is a second entry point to the same animated surface.
    click(win, 150., 840., PointerEventButton::Left);
    assert!(app.get_video_open(), "clicking the video title expands it");
    assert_eq!(opens.get(), 2);
    advance(win, 500);
    click(win, 80., 770., PointerEventButton::Left);
    advance(win, 500);

    win.dispatch_event(WindowEvent::KeyPressed {
        text: Key::Return.into(),
    });
    win.dispatch_event(WindowEvent::KeyReleased {
        text: Key::Return.into(),
    });
    assert!(app.get_video_open(), "Enter expands the mini player");
    assert_eq!(opens.get(), 3);
    assert_eq!(toggles.get(), 1);
}

#[test]
fn video_reuses_search_profile_and_debug_without_clicking_through_to_playback() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_video_active(true);
    app.set_video_open(true);
    app.set_playing(true);
    app.set_debug_title("Fixture video".into());
    app.set_debug_status("Video · playing".into());
    app.set_debug_blocks(slint::ModelRc::new(slint::VecModel::from(vec![2; 7])));
    advance(win, 500);
    let top = app.get_pane_y();

    app.on_toggle_play(|| panic!("Header and debug clicks must not toggle playback"));
    click(win, 1366.5, top + 32., PointerEventButton::Left);
    assert!(app.get_debug_open());
    // The shared panel is above the video and consumes its own clicks.
    click(win, 1200., top + 100., PointerEventButton::Left);
    assert!(app.get_debug_open());
    click(win, 1366.5, top + 32., PointerEventButton::Left);
    assert!(!app.get_debug_open());

    let settings = Rc::new(Cell::new(false));
    app.on_navigate({
        let settings = settings.clone();
        move |key| {
            assert_eq!(key, "settings");
            settings.set(true);
        }
    });
    click(win, 1409.5, top + 32., PointerEventButton::Left);
    click(win, 1240., top + 129., PointerEventButton::Left);
    assert!(settings.get(), "the reused profile menu opens Settings");

    let searched = Rc::new(Cell::new(false));
    app.on_search({
        let searched = searched.clone();
        move |query| {
            assert_eq!(query, "Queen");
            searched.set(true);
        }
    });
    // The search shortcut also wakes controls after they have faded away.
    advance(win, 4000);
    advance(win, 250);
    app.set_focus_search(app.get_focus_search() + 1);
    win.dispatch_event(WindowEvent::KeyPressed {
        text: "Queen".into(),
    });
    win.dispatch_event(WindowEvent::KeyReleased {
        text: "Queen".into(),
    });
    assert_eq!(app.get_search_text(), "Queen");
    // Typing holds the video controls open past the usual idle timeout.
    advance(win, 4000);
    win.dispatch_event(WindowEvent::KeyPressed {
        text: Key::Return.into(),
    });
    win.dispatch_event(WindowEvent::KeyReleased {
        text: Key::Return.into(),
    });
    assert!(searched.get());
    assert!(app.get_playing());
}
