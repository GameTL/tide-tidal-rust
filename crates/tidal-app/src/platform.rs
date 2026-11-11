//! Native window chrome: content extends under a transparent title bar, like the official client.

use slint::ComponentHandle;

use crate::MainWindow;

/// Must run before the first window is created.
pub fn configure_backend() {
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
        let selected = slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_window_attributes_hook(|attrs| {
                attrs.with_titlebar_transparent(true).with_fullsize_content_view(true).with_title_hidden(true)
            })
            .select();
        if let Err(e) = selected {
            tracing::warn!("Falling back to the default window chrome: {e}");
        }
    }
}

/// Start a native window move; called when empty chrome is pressed.
pub fn drag_window(win: &MainWindow) {
    use slint::winit_030::WinitWindowAccessor;
    win.window().with_winit_window(|w| {
        let _ = w.drag_window();
    });
}
