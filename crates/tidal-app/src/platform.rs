//! Native window chrome: content extends under a transparent title bar, like the official client.

use slint::ComponentHandle;

use crate::MainWindow;

/// Must run before the first window is created.
pub fn configure_backend(low_memory: bool) {
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
        let mut selector = slint::BackendSelector::new().backend_name("winit".into());
        // The software renderer needs no GPU window surfaces (~120 MB less on a
        // Retina display); TIDE_RENDERER=femtovg|software overrides the setting.
        let renderer = std::env::var("TIDE_RENDERER")
            .unwrap_or_else(|_| if low_memory { "software" } else { "femtovg" }.to_string());
        selector = selector.renderer_name(renderer);
        let selected = selector
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
