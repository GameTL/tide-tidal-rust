//! Layout and input around the glass player bar: pages run beneath it, the
//! bar keeps what is aimed at it, and the last row can be scrolled clear.
//! (What the glass looks like is checked in glass_render.rs.)
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};

slint::include_modules!();

const W: f32 = 1440.;
const H: f32 = 900.;

fn right_click(win: &slint::Window, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    let button = PointerEventButton::Right;
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button });
    win.dispatch_event(WindowEvent::PointerReleased { position, button });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn wheel(win: &slint::Window, x: f32, y: f32, dy: f32) {
    let position = LogicalPosition::new(x, y);
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerScrolled { position, delta_x: 0., delta_y: dy });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

#[test]
fn pages_run_under_a_glass_bar_and_the_bar_keeps_its_input() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(W, H) });
    app.set_logged_in(true);
    app.set_view("tracks".into());
    app.set_tracks(ModelRc::new(VecModel::from(
        (0..80)
            .map(|i| TrackData {
                id: i.to_string().into(),
                title: format!("Track {i}").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    let menus = Rc::new(RefCell::new(Vec::<String>::new()));
    app.on_request_track_menu({
        let menus = menus.clone();
        move |id, _, _| menus.borrow_mut().push(id.to_string())
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));

    let bar = app.get_player_height();
    let bar_top = H - bar;
    let top = app.get_pane_y();

    // Opaque bar: the pane ends where the bar begins, as it always has.
    assert!(!app.get_glass());
    assert_eq!(app.get_bar_inset(), 0.);
    assert_eq!(app.get_pane_height(), bar_top - top);

    // Glass: the pane runs to the bottom of the window.
    app.set_glass(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(app.get_bar_inset(), bar);
    assert_eq!(app.get_pane_height(), H - top);

    // Rows are under the bar now. A secondary click just above the bar opens
    // a row's menu; the same click on an empty part of the bar does not.
    app.set_scroll_y(-333.);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    right_click(win, 500., bar_top - 12.);
    assert_eq!(menus.borrow().len(), 1, "a row above the bar takes the click");
    menus.borrow_mut().clear();
    for y in [bar_top + 6., bar_top + 30., H - 4.] {
        right_click(win, 500., y);
        right_click(win, 1040., y);
    }
    assert!(menus.borrow().is_empty(), "rows under the bar were clicked: {:?}", menus.borrow());

    // The wheel over the bar does not scroll the page beneath it.
    wheel(win, 500., bar_top + 20., -120.);
    assert_eq!(app.get_scroll_y(), -333.);
    wheel(win, 500., bar_top - 100., -120.);
    assert!(app.get_scroll_y() < -333., "the page itself still scrolls");

    // The last track, revealed, sits in the middle of what is visible above
    // the bar, not under it.
    app.set_reveal_track_index(79);
    app.set_reveal_track_revision(app.get_reveal_track_revision() + 1);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let page_top = top + 64.;
    right_click(win, 500., page_top + (bar_top - page_top) / 2.);
    assert_eq!(menus.borrow().as_slice(), ["79"]);
}
