use slint::platform::WindowEvent;
use slint::{ComponentHandle, LogicalSize};

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
