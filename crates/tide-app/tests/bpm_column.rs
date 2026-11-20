use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};

slint::include_modules!();

const WIDTH: f32 = 1440.;
const HEIGHT: f32 = 900.;

/// A logged-in window on `view` with BPM off, and every request to switch it.
fn fixture(view: &str) -> (MainWindow, Rc<RefCell<Vec<bool>>>) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(WIDTH, HEIGHT),
    });
    app.set_logged_in(true);
    app.set_view(view.into());
    app.set_show_audio_metadata(false);
    let asked = Rc::new(RefCell::new(Vec::new()));
    app.on_set_show_audio_metadata({
        let asked = asked.clone();
        let weak = ui.as_weak();
        move |on| {
            asked.borrow_mut().push(on);
            weak.unwrap().global::<App>().set_show_audio_metadata(on);
        }
    });
    settle();
    (ui, asked)
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn click(win: &slint::Window, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button });
    win.dispatch_event(WindowEvent::PointerReleased { position, button });
    settle();
}

/// The BPM switch is a row of Settings: clicking down the controls of the
/// page reaches a toggle that turns the column on, and off again.
#[test]
fn settings_has_the_bpm_toggle() {
    let (ui, asked) = fixture("settings");
    let win = ui.window();
    let middle = LogicalPosition::new(WIDTH / 2., HEIGHT / 2.);
    let mut found = None;
    'pages: for _ in 0..40 {
        for y in (90..(HEIGHT as i32 - 130)).step_by(12) {
            for x in ((WIDTH as i32 / 2)..(WIDTH as i32 - 8)).step_by(16) {
                click(win, x as f32, y as f32);
                if !asked.borrow().is_empty() {
                    found = Some((x as f32, y as f32));
                    break 'pages;
                }
            }
        }
        win.dispatch_event(WindowEvent::PointerMoved { position: middle });
        win.dispatch_event(WindowEvent::PointerScrolled {
            position: middle,
            delta_x: 0.,
            delta_y: -300.,
        });
        settle();
    }
    let (x, y) = found.expect("a BPM toggle in Settings");
    assert_eq!(asked.borrow().as_slice(), [true]);
    assert!(ui.global::<App>().get_show_audio_metadata());
    click(win, x, y);
    assert_eq!(asked.borrow().as_slice(), [true, false]);
}

/// Nothing in a track list switches BPM: its heading is only a heading.
fn track_list_has_no_switch(view: &str) {
    let (ui, asked) = fixture(view);
    let app = ui.global::<App>();
    app.set_tracks(ModelRc::new(VecModel::from(
        (0..3)
            .map(|i| TrackData {
                id: i.to_string().into(),
                title: format!("Track {i}").into(),
                bpm: "120".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    for on in [false, true] {
        app.set_show_audio_metadata(on);
        settle();
        for y in (70..560).step_by(6) {
            for x in ((WIDTH as i32 - 260)..(WIDTH as i32 - 20)).step_by(8) {
                click(ui.window(), x as f32, y as f32);
            }
        }
        assert!(asked.borrow().is_empty(), "{view}, BPM {on}");
    }
}

#[test]
fn album_tracks_have_no_bpm_switch() {
    track_list_has_no_switch("album");
}

#[test]
fn playlist_tracks_have_no_bpm_switch() {
    track_list_has_no_switch("detail");
}
