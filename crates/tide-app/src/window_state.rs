//! Persist AppKit frames in logical points, including the frame behind Window ▸ Zoom.

use std::cell::Cell;

use core_foundation::{base::TCFType, string::CFString, uuid::CFUUID};
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::{Deserialize, Serialize};
use slint::winit_030::WinitWindowAccessor;

use crate::{controller::Ctx, platform};

/// The window's native minimum (MainWindow's min-width / min-height at 100%
/// zoom): as narrow as a phone, where TIDE shows its phone layout. Zoomed in,
/// Slint's own minimum grows with the zoom and widens the window if needed.
const MIN_WIDTH: f64 = 320.0;
const MIN_HEIGHT: f64 = 620.0;

thread_local! {
    // Startup and Dock reopen must restore before native events can save defaults.
    static READY: Cell<bool> = const { Cell::new(false) };
    static DIRTY: Cell<bool> = const { Cell::new(false) };
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    normal_frame: Rect,
    screen: Screen,
    maximized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Screen {
    /// A display UUID survives reboot, reconnection, and resolution changes.
    id: Option<String>,
    name: String,
    frame: Rect,
    visible: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f64::is_finite)
            && self.width > 0.0
            && self.height > 0.0
    }

    fn fitted(self, visible: Self) -> Self {
        let width = self
            .width
            .clamp(MIN_WIDTH.min(visible.width), visible.width);
        let height = self
            .height
            .clamp(MIN_HEIGHT.min(visible.height), visible.height);
        Self {
            x: self.x.clamp(visible.x, visible.x + visible.width - width),
            y: self.y.clamp(visible.y, visible.y + visible.height - height),
            width,
            height,
        }
    }

    fn same(self, other: Self) -> bool {
        (self.x - other.x).abs() < 1.0
            && (self.y - other.y).abs() < 1.0
            && (self.width - other.width).abs() < 1.0
            && (self.height - other.height).abs() < 1.0
    }
}

impl From<NSRect> for Rect {
    fn from(frame: NSRect) -> Self {
        Self {
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        }
    }
}

impl From<Rect> for NSRect {
    fn from(frame: Rect) -> Self {
        Self::new(
            NSPoint::new(frame.x, frame.y),
            NSSize::new(frame.width, frame.height),
        )
    }
}

impl WindowState {
    fn valid(&self) -> bool {
        self.normal_frame.valid() && self.screen.frame.valid() && self.screen.visible.valid()
    }

    fn target_screen<'a>(&self, screens: &'a [Screen]) -> Option<&'a Screen> {
        screens
            .iter()
            .find(|screen| self.screen.id.is_some() && screen.id == self.screen.id)
            .or_else(|| {
                // If UUID lookup failed, use the name only when it is unambiguous.
                let mut named = screens
                    .iter()
                    .filter(|screen| screen.name == self.screen.name);
                let first = named.next()?;
                named.next().is_none().then_some(first)
            })
            // NSScreen lists the primary display first. A disconnected monitor
            // must never leave the window somewhere outside the current desktop.
            .or_else(|| screens.first())
    }

    fn normal_on(&self, screen: &Screen) -> Rect {
        Rect {
            x: self.normal_frame.x + screen.frame.x - self.screen.frame.x,
            y: self.normal_frame.y + screen.frame.y - self.screen.frame.y,
            ..self.normal_frame
        }
        .fitted(screen.visible)
    }
}

// Like winit, link through ApplicationServices, which re-exports ColorSync.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> core_foundation::uuid::CFUUIDRef;
}

fn display_uuid(display: u32) -> Option<String> {
    // SAFETY: CoreGraphics accepts a display ID and returns an owned CFUUID,
    // or null if the display has gone away. Both CF objects are released by RAII.
    unsafe {
        let uuid = CGDisplayCreateUUIDFromDisplayID(display);
        if uuid.is_null() {
            return None;
        }
        let uuid = CFUUID::wrap_under_create_rule(uuid);
        let string =
            core_foundation::uuid::CFUUIDCreateString(std::ptr::null(), uuid.as_concrete_TypeRef());
        if string.is_null() {
            return None;
        }
        Some(CFString::wrap_under_create_rule(string).to_string())
    }
}

/// The display behind an NSScreen, from its `NSScreenNumber`.
fn display_id(screen: &NSScreen) -> Option<u32> {
    use objc2::{msg_send, runtime::AnyObject};
    use objc2_foundation::NSString;
    let key = NSString::from_str("NSScreenNumber");
    // SAFETY: `deviceDescription` is an NSDictionary; `NSScreenNumber` maps
    // to an NSNumber holding the CGDirectDisplayID. Both are checked for nil.
    unsafe {
        let description: *mut AnyObject = msg_send![screen, deviceDescription];
        if description.is_null() {
            return None;
        }
        let number: *mut AnyObject = msg_send![description, objectForKey: &*key];
        if number.is_null() {
            return None;
        }
        let id: u32 = msg_send![number, unsignedIntValue];
        Some(id)
    }
}

/// Needs no window, so the first frame can be chosen before it exists.
fn screens() -> Vec<Screen> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Vec::new();
    };
    NSScreen::screens(mtm)
        .iter()
        .map(|screen| Screen {
            id: display_id(&screen).and_then(display_uuid),
            name: screen.localizedName().to_string(),
            frame: screen.frame().into(),
            visible: screen.visibleFrame().into(),
        })
        .filter(|screen| screen.frame.valid() && screen.visible.valid())
        .collect()
}

/// Where the window goes: (frame on screen, frame to un-zoom to), in AppKit
/// points with a bottom-left origin.
fn target(saved: &WindowState, available: &[Screen]) -> Option<(Rect, Rect)> {
    let screen = saved.target_screen(available)?;
    let normal = saved.normal_on(screen);
    Some((if saved.maximized { screen.visible } else { normal }, normal))
}

/// Give the window its saved frame before it is created, so it opens at its
/// final size and place instead of resizing once it is on screen.
pub fn place_before_show(window: &slint::Window, ctx: &Ctx) {
    let Some(saved) = ctx.settings.read().unwrap().window.clone() else {
        return;
    };
    if !saved.valid() {
        return;
    }
    let available = screens();
    // AppKit's origin is the bottom-left of the primary display, listed first;
    // winit measures from its top-left.
    let Some(primary) = available.iter().find(|s| s.frame.x == 0.0 && s.frame.y == 0.0)
    else {
        return;
    };
    let Some((frame, _)) = target(&saved, &available) else {
        return;
    };
    let top = primary.frame.height - (frame.y + frame.height);
    window.set_position(slint::LogicalPosition::new(frame.x as f32, top as f32));
    window.set_size(slint::LogicalSize::new(
        frame.width as f32,
        frame.height as f32,
    ));
}

pub fn prepare_restore() {
    READY.set(false);
    platform::cancel_zoom();
}

/// Retry from the UI ticker if the native window does not exist yet.
pub fn restore(window: &slint::Window, ctx: &Ctx) {
    if READY.get() || !window.is_visible() {
        return;
    }
    let saved = ctx.settings.read().unwrap().window.clone();
    let restored = window.with_winit_window(|w| {
        let Some(ns) = platform::ns_window(w) else {
            return false;
        };
        let Some(saved) = saved.filter(WindowState::valid) else {
            platform::UNZOOMED.set(None);
            return true;
        };
        let Some((frame, normal)) = target(&saved, &screens()) else {
            return false;
        };
        // The window normally opens at this frame already (place_before_show);
        // only move it when it did not, e.g. a display changed meanwhile.
        if !Rect::from(ns.frame()).same(frame) {
            ns.setFrame_display(frame.into(), true);
        }
        // Remember the normal frame so both AppKit and our animated zoom have
        // a sensible frame to return to, even after a display was disconnected.
        platform::UNZOOMED.set(Some(normal.into()));
        true
    });
    if restored == Some(true) {
        READY.set(true);
    }
}

/// Keep the latest frame in memory even if Quit follows a resize immediately.
/// Disk writes are coalesced by the UI ticker instead of blocking every resize.
pub fn observe(window: &slint::Window, ctx: &Ctx) {
    if !READY.get() || !window.is_visible() {
        return;
    }
    let saved = window.with_winit_window(|w| {
        // Minimized and full-screen Space frames aren't ordinary window frames.
        if w.is_minimized() == Some(true) || w.fullscreen().is_some() {
            return None;
        }
        let ns = platform::ns_window(w)?;
        let current_screen = ns.screen()?;
        // Remember the destination if Quit/Close happens during animated zoom,
        // never one of the temporary intermediate sizes.
        let frame = Rect::from(platform::zoom_target().unwrap_or_else(|| ns.frame()));
        let screen = screens()
            .into_iter()
            .find(|screen| screen.frame.same(current_screen.frame().into()))?;
        let maximized = frame.same(screen.visible);
        let normal_frame = if maximized {
            platform::UNZOOMED.get().map(Rect::from).unwrap_or(frame)
        } else {
            frame
        };
        Some(WindowState {
            normal_frame,
            screen,
            maximized,
        })
    });
    let Some(saved) = saved.flatten().filter(WindowState::valid) else {
        return;
    };
    let mut settings = ctx.settings.write().unwrap();
    if settings.window.as_ref() == Some(&saved) {
        return;
    }
    settings.window = Some(saved);
    DIRTY.set(true);
}

/// Flush at most four times a second while moving/resizing, and before closing.
pub fn remember(window: &slint::Window, ctx: &Ctx) {
    observe(window, ctx);
    if DIRTY.get() && flush(ctx).is_ok() {
        DIRTY.set(false);
    }
}

/// Also called by the exit handler, which may run off the UI thread. It uses
/// only the last observed frame, never AppKit or a destroyed native window.
pub fn flush(ctx: &Ctx) -> anyhow::Result<()> {
    let settings = ctx.settings.read().unwrap();
    let result = serde_json::to_string(&*settings)
        .map_err(anyhow::Error::from)
        .and_then(|json| {
            ctx.meta
                .put_json("settings", &json)
                .map_err(anyhow::Error::from)
        });
    if let Err(e) = &result {
        tracing::warn!("Could not save window placement: {e}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: &str, x: f64, y: f64, width: f64, height: f64) -> Screen {
        Screen {
            id: Some(id.into()),
            name: id.into(),
            frame: Rect {
                x,
                y,
                width,
                height,
            },
            visible: Rect {
                x,
                y,
                width,
                height: height - 30.0,
            },
        }
    }

    fn saved() -> WindowState {
        WindowState {
            normal_frame: Rect {
                x: 2100.0,
                y: 100.0,
                width: 1200.0,
                height: 800.0,
            },
            screen: screen("external", 1920.0, 0.0, 2560.0, 1440.0),
            maximized: true,
        }
    }

    #[test]
    fn follows_the_same_display_after_it_moves_to_the_left() {
        let available = [
            screen("primary", 0.0, 0.0, 1920.0, 1080.0),
            screen("external", -2560.0, 200.0, 2560.0, 1440.0),
        ];
        let saved = saved();
        let target = saved.target_screen(&available).unwrap();
        assert_eq!(target.id.as_deref(), Some("external"));
        let frame = saved.normal_on(target);
        assert_eq!(
            (frame.x, frame.y, frame.width, frame.height),
            (-2380.0, 300.0, 1200.0, 800.0)
        );
    }

    #[test]
    fn disconnected_display_falls_back_inside_the_primary_work_area() {
        let available = [screen("primary", 0.0, 0.0, 1024.0, 768.0)];
        let saved = saved();
        let target = saved.target_screen(&available).unwrap();
        let frame = saved.normal_on(target);
        assert_eq!(
            frame,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1024.0,
                height: 738.0
            }
        );
    }

    #[test]
    fn clamps_invalid_placement_and_respects_the_dock_and_menu_bar() {
        let visible = Rect {
            x: -1600.0,
            y: 60.0,
            width: 1600.0,
            height: 910.0,
        };
        let frame = Rect {
            x: -9000.0,
            y: 9000.0,
            width: 100.0,
            height: 100.0,
        }
        .fitted(visible);
        assert_eq!(
            frame,
            Rect {
                x: -1600.0,
                y: 350.0,
                width: 320.0,
                height: 620.0
            }
        );
    }

    #[test]
    fn persistence_preserves_maximized_and_normal_frames_across_logout() {
        let meta = tide_cache::MetadataCache::in_memory().unwrap();
        let settings = crate::controller::Settings {
            window: Some(saved()),
            ..Default::default()
        };
        meta.put_json("settings", &serde_json::to_string(&settings).unwrap())
            .unwrap();
        meta.clear_json().unwrap();
        let loaded = crate::controller::Settings::load(&meta);
        assert_eq!(loaded.window, settings.window);
        assert!(loaded.window.as_ref().unwrap().maximized);
    }

    #[test]
    fn ignores_invalid_saved_dimensions_and_accepts_old_settings() {
        let mut saved = saved();
        saved.normal_frame.width = -1.0;
        assert!(!saved.valid());
        saved.normal_frame.width = f64::NAN;
        assert!(!saved.valid());
        let old: crate::controller::Settings =
            serde_json::from_str(r#"{"quality":"high"}"#).unwrap();
        assert!(old.window.is_none());
    }
}
