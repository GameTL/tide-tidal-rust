use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize};
use std::{cell::RefCell, rc::Rc};

slint::include_modules!();

#[test]
fn dsp_copy_and_warning_can_toggle_without_panic() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_view("dsp".into());
    app.set_dsp_device("ADAM Audio D3V".into());
    app.set_dsp_outputs(1);
    app.set_dsp_overshoot(0.0);
    app.set_dsp_tab(0);
    app.set_dsp_usr_on(false);
    app.set_dsp_spk_on(true);

    app.set_dsp_overshoot(6.5);
    app.set_dsp_outputs(3);
    app.set_dsp_tab(1);
    app.set_dsp_usr_on(true);
    app.set_dsp_overshoot(0.0);
    app.set_dsp_tab(0);
}

#[test]
fn backup_and_restore_buttons_work_at_wide_and_minimum_windows() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    app.set_logged_in(true);
    app.set_view("dsp".into());
    app.set_dsp_device("ADAM Audio D3V".into());
    let actions = Rc::new(RefCell::new(Vec::new()));
    app.on_dsp_action({
        let actions = actions.clone();
        move |command| actions.borrow_mut().push(command.to_string())
    });
    for (width, height) in [(1440.0_f32, 900.0_f32), (960.0, 620.0)] {
        win.dispatch_event(WindowEvent::Resized {
            size: LogicalSize::new(width, height),
        });
        actions.borrow_mut().clear();
        // Sweep the title row: both pills must be there to click, inside the window.
        for y in [124.0, 138.0, 152.0] {
            let mut x = 0.0;
            while x < width {
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
                x += 20.0;
            }
        }
        for action in ["backup", "restore"] {
            assert!(
                actions.borrow().iter().any(|a| a == action),
                "{action} is reachable at {width} wide: {:?}",
                actions.borrow()
            );
        }
    }
}
