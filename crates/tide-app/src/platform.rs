//! Native window chrome: content extends under a transparent title bar, like the official client.

use slint::ComponentHandle;

use crate::MainWindow;

#[cfg(target_os = "macos")]
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

/// Must run before the first window is created.
pub fn configure_backend() {
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
        let mut selector = slint::BackendSelector::new().backend_name("winit".into());
        let renderer = std::env::var("TIDE_RENDERER").unwrap_or_else(|_| "skia".to_string());
        tracing::info!(%renderer, "Selecting UI renderer");
        if renderer == "skia" {
            selector = selector.require_metal();
        }
        selector = selector.renderer_name(renderer);
        let selected = selector
            .with_winit_window_attributes_hook(|attrs| {
                attrs
                    .with_titlebar_transparent(true)
                    .with_fullsize_content_view(true)
                    .with_title_hidden(true)
            })
            .select();
        if let Err(e) = selected {
            tracing::warn!("Falling back to the default window chrome: {e}");
        }
    }
}

// ---------------------------------------------------------------- close / reopen
//
// Closing the window (⌘W or the close button) keeps TIDE running, like Music:
// playback, media keys and Now Playing carry on, and the Dock icon brings the
// window back. ⌘Q quits. A plain hide would keep the NSWindow, its Metal
// drawables and Skia's GPU caches alive, so Slint is asked to destroy the
// native window instead and build a fresh one when it is shown again.

// ---------------------------------------------------------------- memory
//
// Skia keeps GPU resources (artwork textures, layers, glyph atlases, video
// frames) in a cache that upstream leaves at 256 MB and never trims, so the
// footprint only ever grows. TIDE's patched renderer gives it a budget and
// frees what no frame has used for a while.

/// Set the renderer's memory budget. Safe to call before the window exists
/// and again whenever the Low RAM setting changes.
pub fn apply_memory_profile(low_ram: bool) {
    #[cfg(target_os = "macos")]
    {
        use i_slint_renderer_skia::memory;
        use std::time::Duration;
        const MB: usize = 1024 * 1024;
        let (limit, idle) = if low_ram {
            (32 * MB, Duration::from_secs(3))
        } else {
            (96 * MB, Duration::from_secs(15))
        };
        memory::set_gpu_cache_limit(limit);
        memory::set_idle_purge(idle);
        memory::set_two_drawables(low_ram);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = low_ram;
}

/// Drop every GPU resource that is not on screen, after the next frame.
pub fn purge_gpu_cache() {
    #[cfg(target_os = "macos")]
    i_slint_renderer_skia::memory::purge_now();
}

/// Bytes in the renderer's GPU cache as of the last frame.
pub fn gpu_cache_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        i_slint_renderer_skia::memory::gpu_cache_usage() as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// Make hiding the window destroy it. Call first thing in `main`, before any
/// thread could be reading the environment.
pub fn destroy_window_on_hide() {
    // SAFETY: called before TIDE starts any thread of its own; the runtime's
    // idle worker threads don't read the environment.
    unsafe { std::env::set_var("SLINT_DESTROY_WINDOW_ON_HIDE", "1") };
}

/// Bring the window back when the Dock icon is clicked while it is closed.
/// Call after the main window exists, so winit's app delegate is registered.
#[cfg(target_os = "macos")]
pub fn install_reopen_handler() {
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    use objc2::sel;

    unsafe extern "C-unwind" fn should_handle_reopen(
        _this: *mut AnyObject,
        _cmd: Sel,
        _app: *mut AnyObject,
        has_visible_windows: Bool,
    ) -> Bool {
        if !has_visible_windows.as_bool() {
            // Leave AppKit's callback before Slint creates the new window.
            crate::controller::on_ui(|ui| ui.reopen_window());
        }
        // AppKit then deminiaturizes a minimized window.
        Bool::YES
    }

    // winit 0.30 doesn't let the app replace its delegate, so add the method
    // to winit's delegate class (Slint does the same for shortcut localization).
    let sel = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
    let Some(cls) = AnyClass::get(c"WinitApplicationDelegate") else {
        tracing::warn!("No winit app delegate; the Dock icon can't reopen the window");
        return;
    };
    if cls.instance_method(sel).is_some() {
        return;
    }
    // SAFETY: the function matches the selector's signature (BOOL, id self,
    // SEL, NSApplication *, BOOL) and lives for the whole process.
    unsafe {
        objc2::ffi::class_addMethod(
            (cls as *const AnyClass).cast_mut(),
            sel,
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool,
                Imp,
            >(should_handle_reopen),
            c"B@:@B".as_ptr(),
        );
    }
}

#[cfg(target_os = "macos")]
thread_local! {
    /// The window was rebuilt and still has the default title bar.
    static NEEDS_CHROME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The window was shown again after being closed. Slint rebuilds the native
/// window from its default attributes, without the backend's hook, so put
/// the transparent title bar back; the menu bar is rebuilt as well.
#[cfg(target_os = "macos")]
pub fn window_reopened(win: &MainWindow) {
    NEEDS_CHROME.set(true);
    // Normally the window's first event applies it; this covers a window that
    // gets none before it is drawn.
    let weak = win.as_weak();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        if let Some(win) = weak.upgrade() {
            restore_chrome(win.window());
        }
    });
    configure_menu();
}

#[cfg(target_os = "macos")]
fn restore_chrome(window: &slint::Window) {
    use objc2_app_kit::{NSWindowStyleMask, NSWindowTitleVisibility};

    if !NEEDS_CHROME.get() {
        return;
    }
    let Some(ns) = window.with_winit_window(ns_window).flatten() else {
        return;
    };
    NEEDS_CHROME.set(false);
    ns.setTitlebarAppearsTransparent(true);
    ns.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    ns.setStyleMask(ns.styleMask() | NSWindowStyleMask::FullSizeContentView);
    tracing::debug!("Restored the window's transparent title bar");
}

/// Called when empty chrome is pressed: a double-click zooms the window to fill
/// the screen (and back), like a native title bar; any other press starts
/// moving the window.
pub fn drag_window(win: &MainWindow) {
    use slint::winit_030::WinitWindowAccessor;
    win.window().with_winit_window(|w| {
        if w.fullscreen().is_some() {
            return;
        }
        if is_double_click() {
            zoom(w);
        } else {
            start_window_move(w);
        }
    });
}

/// Toggle between the window's own frame and filling the screen (Window ▸ Zoom),
/// without entering a full-screen Space.
///
/// AppKit's `zoom:` animates in a blocking loop, so the UI can't re-lay out
/// until it ends: the old frame is stretched, then snaps. Stepping the frame
/// from a timer instead keeps the event loop running, so every intermediate
/// size is laid out and drawn.
#[cfg(target_os = "macos")]
fn zoom(w: &slint::winit_030::winit::window::Window) {
    let Some(window) = ns_window(w) else {
        w.set_maximized(!w.is_maximized());
        return;
    };
    let Some(filled) = window.screen().map(|s| s.visibleFrame()) else {
        window.zoom(None);
        return;
    };
    // While an animation runs, a double-click reverses it.
    let to_filled = match ZOOM.with_borrow(|z| {
        z.as_ref()
            .filter(|z| z.timer.running())
            .map(|z| z.to_filled)
    }) {
        Some(was_filling) => !was_filling,
        None => !same_frame(window.frame(), filled),
    };
    let target = if to_filled {
        if ZOOM.with_borrow(|z| z.as_ref().is_none_or(|z| !z.timer.running())) {
            UNZOOMED.set(Some(window.frame()));
        }
        filled
    } else {
        match UNZOOMED.get() {
            Some(frame) => frame,
            // Filled by hand, so there is no frame to return to: let AppKit pick one.
            None => return window.zoom(None),
        }
    };
    animate_frame(window, target, to_filled);
}

#[cfg(target_os = "macos")]
struct ZoomAnimation {
    timer: slint::Timer,
    to_filled: bool,
}

#[cfg(target_os = "macos")]
thread_local! {
    /// The frame to return to when the window is zoomed back.
    static UNZOOMED: std::cell::Cell<Option<objc2_foundation::NSRect>> =
        const { std::cell::Cell::new(None) };
    static ZOOM: std::cell::RefCell<Option<ZoomAnimation>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_os = "macos")]
fn same_frame(a: objc2_foundation::NSRect, b: objc2_foundation::NSRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 1.0
        && (a.origin.y - b.origin.y).abs() < 1.0
        && (a.size.width - b.size.width).abs() < 1.0
        && (a.size.height - b.size.height).abs() < 1.0
}

/// Move the window to `target` with an ease-out over about a quarter second.
#[cfg(target_os = "macos")]
fn animate_frame(
    window: objc2::rc::Retained<objc2_app_kit::NSWindow>,
    target: objc2_foundation::NSRect,
    to_filled: bool,
) {
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use std::time::{Duration, Instant};

    const DURATION: Duration = Duration::from_millis(250);
    if reduce_motion() {
        ZOOM.set(None);
        window.setFrame_display(target, true);
        return;
    }
    let from = window.frame();
    let started = Instant::now();
    let step = move || {
        let t = (started.elapsed().as_secs_f64() / DURATION.as_secs_f64()).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let lerp = |a: f64, b: f64| (a + (b - a) * e).round();
        let frame = if t >= 1.0 {
            target
        } else {
            NSRect::new(
                NSPoint::new(
                    lerp(from.origin.x, target.origin.x),
                    lerp(from.origin.y, target.origin.y),
                ),
                NSSize::new(
                    lerp(from.size.width, target.size.width),
                    lerp(from.size.height, target.size.height),
                ),
            )
        };
        window.setFrame_display(frame, true);
        if t >= 1.0 {
            ZOOM.with_borrow(|z| {
                if let Some(z) = z {
                    z.timer.stop();
                }
            });
        }
    };
    let timer = slint::Timer::default();
    // Faster than any display refresh; each tick lands on the curve by wall time.
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(4), step);
    ZOOM.set(Some(ZoomAnimation { timer, to_filled }));
}

#[cfg(not(target_os = "macos"))]
fn zoom(w: &slint::winit_030::winit::window::Window) {
    w.set_maximized(!w.is_maximized());
}

// ---------------------------------------------------------------- window move
//
// AppKit's `performWindowDragWithEvent` moves the window by the pointer's
// deltas. When the pointer crosses onto another display, macOS remaps it into
// that display's coordinates and the deltas no longer add up to where it
// lands, so the window falls behind the pointer. Instead, TIDE pins the
// window to the pointer's absolute screen position for the whole drag.

#[cfg(target_os = "macos")]
thread_local! {
    /// Pointer position relative to the window's origin while a move is in progress.
    static WINDOW_MOVE: std::cell::Cell<Option<objc2_foundation::NSPoint>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(target_os = "macos")]
fn start_window_move(w: &slint::winit_030::winit::window::Window) {
    use objc2_app_kit::NSEvent;
    use objc2_foundation::NSPoint;

    let Some(window) = ns_window(w) else {
        let _ = w.drag_window();
        return;
    };
    let mouse = NSEvent::mouseLocation();
    let origin = window.frame().origin;
    WINDOW_MOVE.set(Some(NSPoint::new(mouse.x - origin.x, mouse.y - origin.y)));
}

#[cfg(not(target_os = "macos"))]
fn start_window_move(w: &slint::winit_030::winit::window::Window) {
    let _ = w.drag_window();
}

/// Follow the pointer while a window move is in progress; ends it on release.
#[cfg(target_os = "macos")]
fn follow_window_move(window: &slint::Window, event: &WindowEvent) {
    use objc2_app_kit::{NSEvent, NSScreen};
    use objc2_foundation::{MainThreadMarker, NSPoint};
    use slint::winit_030::winit::event::{ElementState, MouseButton};

    let Some(offset) = WINDOW_MOVE.get() else {
        return;
    };
    match event {
        WindowEvent::CursorMoved { .. } => {}
        WindowEvent::MouseInput {
            state: ElementState::Released,
            button: MouseButton::Left,
            ..
        }
        | WindowEvent::Focused(false) => {
            WINDOW_MOVE.set(None);
            return;
        }
        _ => return,
    }
    // A release that never reached the window (e.g. it went to a menu).
    if NSEvent::pressedMouseButtons() & 1 == 0 {
        WINDOW_MOVE.set(None);
        return;
    }
    let Some(ns) = window.with_winit_window(ns_window).flatten() else {
        return;
    };
    let mouse = NSEvent::mouseLocation();
    let mut origin = NSPoint::new(mouse.x - offset.x, mouse.y - offset.y);
    // Like a native title bar, the window's top never goes under the menu bar
    // of the display the pointer is on.
    if let Some(mtm) = MainThreadMarker::new() {
        let screens = NSScreen::screens(mtm);
        let under_pointer = screens.iter().find(|s| {
            let f = s.frame();
            (f.origin.x..f.origin.x + f.size.width).contains(&mouse.x)
                && (f.origin.y..f.origin.y + f.size.height).contains(&mouse.y)
        });
        if let Some(screen) = under_pointer {
            let visible = screen.visibleFrame();
            let top = visible.origin.y + visible.size.height;
            let height = ns.frame().size.height;
            origin.y = origin.y.min(top - height);
        }
    }
    ns.setFrameOrigin(origin);
}

#[cfg(target_os = "macos")]
fn ns_window(
    w: &slint::winit_030::winit::window::Window,
) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = w.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: winit's AppKit handle is a live NSView for as long as the window exists.
    let view: &objc2_app_kit::NSView = unsafe { handle.ns_view.cast().as_ref() };
    view.window()
}

/// True when the press being handled is the second (or later) click of a double-click.
#[cfg(target_os = "macos")]
fn is_double_click() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEventType};

    MainThreadMarker::new()
        .and_then(|mtm| NSApplication::sharedApplication(mtm).currentEvent())
        // Click counts only exist on mouse events; asking any other event raises.
        .filter(|e| e.r#type() == NSEventType::LeftMouseDown)
        .is_some_and(|e| e.clickCount() >= 2)
}

#[cfg(not(target_os = "macos"))]
fn is_double_click() -> bool {
    false
}

/// Use the system's momentum for trackpads and other precise scrolling devices,
/// and drive window moves started from the chrome.
#[cfg(target_os = "macos")]
pub fn configure_input(window: &slint::Window) {
    let mut scroll = macos_scroll_filter();
    window.on_winit_window_event(move |window, event| {
        restore_chrome(window);
        follow_window_move(window, event);
        scroll(window, event)
    });
}

#[cfg(target_os = "macos")]
fn macos_scroll_filter() -> impl FnMut(&slint::Window, &WindowEvent) -> EventResult {
    use slint::winit_030::winit::event::MouseScrollDelta;

    let mut cursor = None;
    let mut swipe = SwipeTracker::default();
    move |window, event| {
        match *event {
            // Winit reports the scroll event's location as CursorMoved before MouseWheel.
            WindowEvent::CursorMoved { position, .. } => cursor = Some(position),
            WindowEvent::MouseWheel {
                delta: MouseScrollDelta::PixelDelta(delta),
                phase,
                ..
            } => {
                let scale = f64::from(window.scale_factor());
                let delta = delta.to_logical::<f32>(scale);
                if swipe.handle(scroll_phase(phase), delta.x, delta.y) {
                    return EventResult::PreventDefault;
                }
                let Some(position) = cursor else {
                    return EventResult::Propagate;
                };
                if delta.x != 0.0 || delta.y != 0.0 {
                    let position = position.to_logical::<f32>(scale);
                    // macOS already sends the momentum deltas. Apply each directly so
                    // Slint doesn't start a second coast when the gesture ends.
                    window.dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                        position: slint::LogicalPosition::new(position.x, position.y),
                        delta_x: delta.x,
                        delta_y: delta.y,
                    });
                }
                return EventResult::PreventDefault;
            }
            _ => {}
        }
        // Discrete mouse wheels and all non-scroll input keep Slint's normal handling.
        EventResult::Propagate
    }
}

/// Where a trackpad scroll event sits in its gesture.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq)]
enum ScrollPhase {
    /// Fingers touched down.
    Began,
    /// Fingers moving on the trackpad.
    Changed,
    /// Fingers lifted (`true` when the system called the gesture off).
    Ended(bool),
    /// The coast after the fingers lifted.
    Momentum,
    MomentumEnded,
}

/// Winit folds finger and momentum phases into one value, so read them from
/// the AppKit event being handled; fall back to Winit's when there is none.
#[cfg(target_os = "macos")]
fn scroll_phase(winit_phase: slint::winit_030::winit::event::TouchPhase) -> ScrollPhase {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEventPhase, NSEventType};
    use slint::winit_030::winit::event::TouchPhase;

    let event = MainThreadMarker::new()
        .and_then(|mtm| NSApplication::sharedApplication(mtm).currentEvent());
    // Phases only exist on scroll events; asking any other event raises.
    if let Some(event) = event.filter(|e| e.r#type() == NSEventType::ScrollWheel) {
        let (phase, momentum) = (event.phase(), event.momentumPhase());
        return if momentum.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled) {
            ScrollPhase::MomentumEnded
        } else if !momentum.is_empty() {
            ScrollPhase::Momentum
        } else if phase.intersects(NSEventPhase::Began | NSEventPhase::MayBegin) {
            ScrollPhase::Began
        } else if phase.contains(NSEventPhase::Cancelled) {
            ScrollPhase::Ended(true)
        } else if phase.contains(NSEventPhase::Ended) {
            ScrollPhase::Ended(false)
        } else {
            ScrollPhase::Changed
        };
    }
    match winit_phase {
        TouchPhase::Started => ScrollPhase::Began,
        TouchPhase::Moved => ScrollPhase::Changed,
        TouchPhase::Ended => ScrollPhase::Ended(false),
        TouchPhase::Cancelled => ScrollPhase::Ended(true),
    }
}

/// Turns two-finger horizontal scrolling into back / forward swipes, the way
/// AppKit's own page swiping behaves: the gesture is claimed only when it
/// starts out sideways, it then follows the fingers until they lift, and the
/// coast that follows is swallowed.
#[cfg(target_os = "macos")]
#[derive(Default)]
enum SwipeTracker {
    #[default]
    Idle,
    /// Fingers are down; not yet clear which way the gesture goes.
    Deciding { dx: f32, dy: f32 },
    /// This gesture drives a page transition.
    Swiping,
    /// The swipe is over; its momentum must not scroll the page.
    Coasting,
}

#[cfg(target_os = "macos")]
impl SwipeTracker {
    /// Movement before the gesture's direction is judged, in logical pixels.
    const SLOP: f32 = 8.0;

    /// True when the event belongs to a swipe and must not scroll anything.
    fn handle(&mut self, phase: ScrollPhase, dx: f32, dy: f32) -> bool {
        use crate::controller::with_ui;

        match phase {
            ScrollPhase::Began => {
                *self = Self::Deciding { dx: 0.0, dy: 0.0 };
                false
            }
            ScrollPhase::Changed => match self {
                Self::Deciding {
                    dx: sum_x,
                    dy: sum_y,
                } => {
                    *sum_x += dx;
                    *sum_y += dy;
                    let (x, y) = (*sum_x, *sum_y);
                    if x.abs() + y.abs() < Self::SLOP {
                        return false;
                    }
                    // Mostly sideways, with a page in that direction: content
                    // moving right uncovers the previous page.
                    let sideways = x.abs() > 2.0 * y.abs();
                    if sideways
                        && swipe_between_pages()
                        && with_ui(|ui| ui.swipe_begin(x > 0.0)) == Some(true)
                    {
                        with_ui(|ui| ui.swipe_move(x));
                        *self = Self::Swiping;
                        true
                    } else {
                        *self = Self::Idle;
                        false
                    }
                }
                Self::Swiping => {
                    with_ui(|ui| ui.swipe_move(dx));
                    true
                }
                _ => false,
            },
            ScrollPhase::Ended(cancelled) => {
                let swiping = matches!(self, Self::Swiping);
                if swiping {
                    with_ui(|ui| ui.swipe_end(cancelled));
                }
                *self = if swiping { Self::Coasting } else { Self::Idle };
                swiping
            }
            ScrollPhase::Momentum => matches!(self, Self::Coasting),
            ScrollPhase::MomentumEnded => matches!(std::mem::take(self), Self::Coasting),
        }
    }
}

/// The "Swipe between pages" trackpad setting.
#[cfg(target_os = "macos")]
fn swipe_between_pages() -> bool {
    objc2_app_kit::NSEvent::isSwipeTrackingFromScrollEventsEnabled()
}

/// The "Reduce motion" accessibility setting.
pub fn reduce_motion() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Give standalone Cargo builds the same Dock icon as the app bundle.
#[cfg(target_os = "macos")]
pub fn configure_app_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    // The app bundle already has this icon. Setting it again makes AppKit
    // keep a 33 MB bitmap of it for as long as TIDE runs.
    let bundled = std::env::current_exe()
        .is_ok_and(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"));
    if bundled {
        return;
    }
    let main_thread = MainThreadMarker::new().expect("TIDE starts its GUI on the main thread");
    let data = NSData::with_bytes(include_bytes!("../assets/TIDE.icns"));
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) else {
        tracing::warn!("Could not load the TIDE application icon");
        return;
    };
    let app = NSApplication::sharedApplication(main_thread);
    // AppKit requires a non-null image.
    unsafe { app.setApplicationIconImage(Some(&icon)) };
}

/// Slint builds the standard macOS app menu, but exposes no app-menu roles.
/// Move its existing Settings action into the app menu after the window is shown.
#[cfg(target_os = "macos")]
pub fn configure_menu() {
    fn install_when_ready(attempts_left: u8) {
        slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
            if move_settings_to_app_menu().is_none() && attempts_left > 1 {
                install_when_ready(attempts_left - 1);
            }
        });
    }

    // Winit creates a default app menu first. Slint adds the window's menus
    // after attaching the native window.
    install_when_ready(25);
}

#[cfg(target_os = "macos")]
fn move_settings_to_app_menu() -> Option<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let app = NSApplication::sharedApplication(MainThreadMarker::new()?);
    let bar = app.mainMenu()?;
    let app_item = bar.itemAtIndex(0)?;
    let app_menu = app_item.submenu()?;
    let view = bar.itemWithTitle(&NSString::from_str("View"))?.submenu()?;
    let settings = view.itemWithTitle(&NSString::from_str("Settings…"))?;
    let name = NSString::from_str(env!("CARGO_BIN_NAME"));
    app_item.setTitle(&name);
    app_menu.setTitle(&name);
    view.removeItem(&settings);
    app_menu.insertItem_atIndex(&settings, 1);
    tracing::debug!("Moved Settings into the TIDE application menu");
    Some(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter, WindowEvent as SlintEvent};
    use slint::winit_030::winit::{
        dpi::PhysicalPosition,
        event::{DeviceId, MouseScrollDelta, TouchPhase},
    };

    use super::{ComponentHandle, EventResult, WindowEvent, macos_scroll_filter};

    slint::slint! {
        import { ScrollView } from "std-widgets.slint";
        export component ScrollProbe inherits Window {
            width: 200px;
            height: 200px;
            out property <length> scroll-y: scroller.content-y;
            scroller := ScrollView { content-height: 1000px; }
        }
    }

    struct TestPlatform(Rc<MinimalSoftwareWindow>, Rc<Cell<u64>>);

    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }

        fn duration_since_start(&self) -> Duration {
            Duration::from_millis(self.1.get())
        }
    }

    #[test]
    fn precise_scroll_uses_only_macos_momentum() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        adapter.set_size(slint::PhysicalSize::new(400, 400));
        let clock = Rc::new(Cell::new(0));
        slint::platform::set_platform(Box::new(TestPlatform(adapter, clock.clone()))).unwrap();
        let probe = ScrollProbe::new().unwrap();
        let window = probe.window();
        window.dispatch_event(SlintEvent::ScaleFactorChanged { scale_factor: 2.0 });
        probe.show().unwrap();

        let mut filter = macos_scroll_filter();
        let device_id = DeviceId::dummy();
        filter(
            window,
            &WindowEvent::CursorMoved {
                device_id,
                position: PhysicalPosition::new(300.0, 300.0),
            },
        );

        use TouchPhase::{Ended, Moved, Started};
        // Retina input: finger movement, release, then the OS momentum stream.
        for (i, (phase, dy, expected_y)) in [
            (Started, 0.0, 0.0),
            (Moved, -60.5, -30.25),
            (Moved, -59.5, -60.0),
            (Ended, 0.0, -60.0),
            (Started, 0.0, -60.0),
            (Moved, -40.0, -80.0),
            (Moved, -20.0, -90.0),
            (Moved, -10.0, -95.0),
            (Ended, 0.0, -95.0),
        ]
        .into_iter()
        .enumerate()
        {
            clock.set(i as u64 * 16);
            let delta = MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, dy));
            let wheel = WindowEvent::MouseWheel {
                device_id,
                delta,
                phase,
            };
            assert_eq!(filter(window, &wheel), EventResult::PreventDefault);
            assert_eq!(probe.get_scroll_y(), expected_y);
        }

        clock.set(1000);
        slint::platform::update_timers_and_animations();
        assert_eq!(probe.get_scroll_y(), -95.0);

        let delta = MouseScrollDelta::LineDelta(0.0, -1.0);
        let wheel = WindowEvent::MouseWheel {
            device_id,
            delta,
            phase: Moved,
        };
        assert_eq!(filter(window, &wheel), EventResult::Propagate);
    }
}

// ---------------------------------------------------------------- login redirect capture
//
// TIDAL's Max-quality (PKCE) login ends on a fixed web address that cannot
// point back at a desktop app. To finish the login without asking the user to
// paste that address, TIDE looks for it in two places, and only while a login
// is in progress:
//   * the tabs of the default browser, when it is scriptable (Safari and
//     Chromium-family browsers; macOS asks for permission the first time);
//   * the clipboard, so copying the address is enough in any other browser.
// Both checks compare against the redirect prefix inside the helper script, so
// no other tab address or clipboard content ever reaches TIDE.

/// Where TIDAL sends the browser after a successful Max-quality login.
pub const LOGIN_REDIRECT_PREFIX: &str = "https://tidal.com/android/login/auth";
pub const SAFARI: &str = "com.apple.Safari";

const CHROMIUM_BROWSERS: [&str; 12] = [
    "com.google.Chrome",
    "com.google.Chrome.beta",
    "com.google.Chrome.canary",
    "org.chromium.Chromium",
    "company.thebrowser.Browser",
    "company.thebrowser.dia",
    "com.brave.Browser",
    "com.microsoft.edgemac",
    "com.vivaldi.Vivaldi",
    "com.operasoftware.Opera",
    "ai.perplexity.comet",
    "com.openai.atlas",
];

/// Bundle identifier of the app that opens https links, e.g. "com.apple.Safari".
pub fn default_browser_id() -> Option<String> {
    let script = r#"ObjC.import("AppKit");
        var app = $.NSWorkspace.sharedWorkspace.URLForApplicationToOpenURL($.NSURL.URLWithString("https://tidal.com"));
        app.isNil() ? "" : $.NSBundle.bundleWithURL(app).bundleIdentifier.js"#;
    let out = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", script])
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    is_bundle_id(&id).then_some(id)
}

fn is_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// True when TIDE can ask this browser for the login redirect itself.
pub fn browser_is_scriptable(bundle_id: &str) -> bool {
    bundle_id == SAFARI
        || bundle_id == "com.apple.SafariTechnologyPreview"
        || CHROMIUM_BROWSERS.contains(&bundle_id)
}

fn redirect_script(bundle_id: &str) -> String {
    // The comparison happens inside the browser's own scripting context: only
    // an address that starts with the login redirect is ever returned.
    format!(
        r#"if application id "{bundle_id}" is running then
    tell application id "{bundle_id}"
        repeat with w in windows
            repeat with t in tabs of w
                set u to URL of t
                if u is not missing value and u starts with "{LOGIN_REDIRECT_PREFIX}" then return u
            end repeat
        end repeat
    end tell
end if
return """#
    )
}

/// Look for the login redirect in the browser's tabs.
/// `Err` means the browser cannot be asked (not permitted or not scriptable).
pub fn redirect_from_browser(bundle_id: &str) -> Result<Option<String>, String> {
    if !is_bundle_id(bundle_id) || !browser_is_scriptable(bundle_id) {
        return Err("browser is not scriptable".into());
    }
    let out = std::process::Command::new("osascript")
        .args(["-e", &redirect_script(bundle_id)])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(url.starts_with(LOGIN_REDIRECT_PREFIX).then_some(url))
}

/// Look for the login redirect on the clipboard. `last_change` remembers the
/// pasteboard change counter so unchanged content is never read twice.
pub fn redirect_from_clipboard(last_change: &mut i64) -> Option<String> {
    let script = format!(
        r#"ObjC.import("AppKit");
        function run(argv) {{
            var pb = $.NSPasteboard.generalPasteboard;
            var count = pb.changeCount;
            if (String(count) === argv[0]) return count + "\n";
            var s = pb.stringForType($.NSPasteboardTypeString);
            var text = s.isNil() ? "" : s.js.trim();
            return count + "\n" + (text.indexOf("{LOGIN_REDIRECT_PREFIX}") === 0 ? text : "");
        }}"#
    );
    let out = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script, &last_change.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (count, url) = text.split_once('\n')?;
    *last_change = count.trim().parse().ok()?;
    let url = url.trim();
    url.starts_with(LOGIN_REDIRECT_PREFIX)
        .then(|| url.to_string())
}

/// Open `url` in a specific browser instead of the default one.
pub fn open_in_browser(bundle_id: &str, url: &str) -> bool {
    is_bundle_id(bundle_id)
        && std::process::Command::new("open")
            .args(["-b", bundle_id, url])
            .status()
            .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod login_capture_tests {
    use super::*;

    #[test]
    fn only_known_scriptable_browsers_are_asked() {
        assert!(browser_is_scriptable("com.apple.Safari"));
        assert!(browser_is_scriptable("com.google.Chrome"));
        assert!(!browser_is_scriptable("org.mozilla.firefox"));
        assert!(redirect_from_browser("org.mozilla.firefox").is_err());
        // Anything that is not a plain bundle id never reaches a script.
        assert!(!is_bundle_id("com.apple.Safari\" then do shell script \"x"));
        assert!(redirect_from_browser("x\" & (do shell script \"id\") & \"").is_err());
    }

    #[test]
    fn redirect_script_filters_by_prefix_inside_the_browser() {
        let script = redirect_script(SAFARI);
        assert!(script.contains(r#"starts with "https://tidal.com/android/login/auth""#));
        assert!(script.contains(r#"application id "com.apple.Safari" is running"#));
    }
}
