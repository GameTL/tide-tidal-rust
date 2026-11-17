//! Native window chrome: content extends under a transparent title bar, like the official client.

use slint::ComponentHandle;

use crate::MainWindow;

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
        // After the winit backend is loaded so QuartzCore (CADisplayLink) exists.
        install_max_refresh_rate();
    }
}

/// Hz to ask a display link for, given the screen's `maximumFramesPerSecond`.
/// Never below 60: some APIs report 0 when they cannot read the panel.
pub(crate) fn preferred_refresh_hz(max_fps: i64) -> f32 {
    max_fps.max(60) as f32
}

/// One frame on a panel that reports `max_fps`. Never shorter than that frame
/// (and never shorter than 60 Hz when the panel cannot be read).
pub(crate) fn frame_interval_from_hz(max_fps: i64) -> std::time::Duration {
    std::time::Duration::from_secs_f64(1.0 / f64::from(preferred_refresh_hz(max_fps)))
}

/// One frame on the window's screen, or 60 Hz if there is no screen yet or
/// macOS Low Power Mode is on.
pub(crate) fn frame_interval() -> std::time::Duration {
    if low_power_mode() {
        return frame_interval_from_hz(60);
    }
    frame_interval_from_hz(screen_max_fps().unwrap_or(60))
}

/// macOS Low Power Mode. TIDE then leaves the frame rate to the system
/// instead of asking for the panel's maximum.
#[cfg(target_os = "macos")]
pub(crate) fn low_power_mode() -> bool {
    objc2_foundation::NSProcessInfo::processInfo().isLowPowerModeEnabled()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn low_power_mode() -> bool {
    false
}

#[cfg(test)]
mod refresh_tests {
    use std::time::Duration;

    #[test]
    fn prefers_the_screen_maximum_and_never_below_60() {
        assert_eq!(super::preferred_refresh_hz(120), 120.0);
        assert_eq!(super::preferred_refresh_hz(144), 144.0);
        assert_eq!(super::preferred_refresh_hz(60), 60.0);
        assert_eq!(super::preferred_refresh_hz(0), 60.0);
        assert_eq!(super::preferred_refresh_hz(-1), 60.0);
    }

    #[test]
    fn visual_ticks_match_the_panel_and_never_outrun_it() {
        assert_eq!(
            super::frame_interval_from_hz(120),
            Duration::from_secs_f64(1.0 / 120.0)
        );
        assert_eq!(
            super::frame_interval_from_hz(60),
            Duration::from_secs_f64(1.0 / 60.0)
        );
        let hz = 144i64;
        let interval = super::frame_interval_from_hz(hz);
        assert_eq!(interval, Duration::from_secs_f64(1.0 / 144.0));
        assert!(interval >= Duration::from_secs_f64(1.0 / hz as f64));
        assert!(super::frame_interval_from_hz(0) >= Duration::from_secs_f64(1.0 / 60.0));
    }
}

// ---------------------------------------------------------------- display refresh
//
// Slint's CADisplayLink (macOS 14+) defaults to 60 Hz. A 250 ms expand then
// only gets about 15 frames on a 120 Hz panel, which reads as stutter. Ask
// for the screen's maximum whenever the link is unpaused so property
// animations match the display. Custom visual timers use frame_interval()
// so they neither outrun the panel nor sit at a hardcoded 8 ms.

#[cfg(target_os = "macos")]
type SetPausedFn = unsafe extern "C-unwind" fn(
    *mut objc2::runtime::AnyObject,
    objc2::runtime::Sel,
    objc2::runtime::Bool,
);

#[cfg(target_os = "macos")]
static ORIGINAL_SET_PAUSED: std::sync::OnceLock<SetPausedFn> = std::sync::OnceLock::new();

#[cfg(target_os = "macos")]
fn install_max_refresh_rate() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use objc2::runtime::Imp;
    use objc2::sel;

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    let Some(cls) = cadisplay_link_class() else {
        tracing::warn!("CADisplayLink is not loaded; animations stay at the timer rate");
        return;
    };
    let Some(method) = cls.instance_method(sel!(setPaused:)) else {
        return;
    };
    let Some(original) =
        (unsafe { objc2::ffi::method_getImplementation(std::ptr::from_ref(method)) })
    else {
        return;
    };
    // SAFETY: CADisplayLink's setPaused: is (self, SEL, BOOL).
    let original = unsafe { std::mem::transmute::<Imp, SetPausedFn>(original) };
    let _ = ORIGINAL_SET_PAUSED.set(original);
    let hooked = unsafe { std::mem::transmute::<SetPausedFn, Imp>(on_set_paused) };
    unsafe { objc2::ffi::method_setImplementation(std::ptr::from_ref(method), hooked) };
    tracing::info!(
        hz = preferred_refresh_hz(screen_max_fps().unwrap_or(60)),
        "Animations follow the screen refresh rate"
    );
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn on_set_paused(
    this: *mut objc2::runtime::AnyObject,
    cmd: objc2::runtime::Sel,
    paused: objc2::runtime::Bool,
) {
    if paused.is_false() {
        apply_max_refresh(this);
        drawing_started();
    } else {
        drawing_stopped();
    }
    if let Some(original) = ORIGINAL_SET_PAUSED.get() {
        unsafe { original(this, cmd, paused) };
    }
}

// ---------------------------------------------------------------- drawables
//
// The lean profile keeps two drawables: one window-sized buffer fewer (about
// 44 MB for a full-screen Retina window). But with two, the next frame can't
// start until the display hands one back, so any frame that takes more than
// a few ms lands a refresh late: a track list scrolled at 60 Hz on a 120 Hz
// panel. So the third drawable only exists while frames are being drawn: it
// is added when Slint's display link starts and given back a second after it
// stops. Fancy Animation keeps three throughout.

#[cfg(target_os = "macos")]
static LEAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// Three drawables are in use (lean profile, while drawing).
#[cfg(target_os = "macos")]
static THIRD_DRAWABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The next start is the one frame that gives the third drawable back.
#[cfg(target_os = "macos")]
static TRIMMING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "macos")]
thread_local! {
    static IDLE_TRIM: slint::Timer = slint::Timer::default();
}

#[cfg(target_os = "macos")]
fn drawing_started() {
    use std::sync::atomic::Ordering::Relaxed;
    if !LEAN.load(Relaxed) || TRIMMING.swap(false, Relaxed) {
        return;
    }
    IDLE_TRIM.with(|t| t.stop());
    if !THIRD_DRAWABLE.swap(true, Relaxed) {
        // The surface switches at the start of the next frame.
        i_slint_renderer_skia::memory::set_two_drawables(false);
    }
}

#[cfg(target_os = "macos")]
fn drawing_stopped() {
    use std::sync::atomic::Ordering::Relaxed;
    if !LEAN.load(Relaxed) || !THIRD_DRAWABLE.load(Relaxed) {
        return;
    }
    IDLE_TRIM.with(|t| {
        t.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_secs(1),
            || {
                if !LEAN.load(Relaxed) || !THIRD_DRAWABLE.swap(false, Relaxed) {
                    return;
                }
                i_slint_renderer_skia::memory::set_two_drawables(true);
                // The surface only reconfigures when it draws: one frame.
                TRIMMING.store(true, Relaxed);
                crate::controller::request_redraw();
            },
        )
    });
}

#[cfg(target_os = "macos")]
fn cadisplay_link_class() -> Option<&'static objc2::runtime::AnyClass> {
    use objc2::runtime::AnyClass;
    if let Some(cls) = AnyClass::get(c"CADisplayLink") {
        return Some(cls);
    }
    let handle = unsafe {
        libc::dlopen(
            c"/System/Library/Frameworks/QuartzCore.framework/QuartzCore".as_ptr(),
            libc::RTLD_LAZY,
        )
    };
    if handle.is_null() {
        return None;
    }
    AnyClass::get(c"CADisplayLink")
}

/// Matches QuartzCore's `CAFrameRateRange` so we can set it without the crate.
#[cfg(target_os = "macos")]
#[repr(C)]
struct FrameRateRange {
    minimum: f32,
    maximum: f32,
    preferred: f32,
}

#[cfg(target_os = "macos")]
unsafe impl objc2::encode::Encode for FrameRateRange {
    const ENCODING: objc2::encode::Encoding = objc2::encode::Encoding::Struct(
        "CAFrameRateRange",
        &[
            objc2::encode::Encoding::Float,
            objc2::encode::Encoding::Float,
            objc2::encode::Encoding::Float,
        ],
    );
}

#[cfg(not(target_os = "macos"))]
fn screen_max_fps() -> Option<i64> {
    None
}

#[cfg(target_os = "macos")]
fn screen_max_fps() -> Option<i64> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSScreen};

    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    app.keyWindow()
        .and_then(|w| w.screen())
        .or_else(|| app.mainWindow().and_then(|w| w.screen()))
        .or_else(|| NSScreen::mainScreen(mtm))
        .map(|s| s.maximumFramesPerSecond() as i64)
}

#[cfg(target_os = "macos")]
fn apply_max_refresh(link: *mut objc2::runtime::AnyObject) {
    use objc2::{msg_send, runtime::AnyObject, sel};

    if link.is_null() {
        return;
    }
    let link: &AnyObject = unsafe { &*link };
    // Checked on every start, so switching Low Power Mode applies to the next
    // animation or scroll. All zeros is CAFrameRateRangeDefault: the system's pick.
    let hz = if low_power_mode() {
        0.0
    } else {
        preferred_refresh_hz(screen_max_fps().unwrap_or(60))
    };
    let supports_range: bool =
        unsafe { msg_send![link, respondsToSelector: sel!(setPreferredFrameRateRange:)] };
    if supports_range {
        let range = FrameRateRange {
            minimum: hz,
            maximum: hz,
            preferred: hz,
        };
        let _: () = unsafe { msg_send![link, setPreferredFrameRateRange: range] };
    } else {
        let fps = hz as i64;
        let _: () = unsafe { msg_send![link, setPreferredFramesPerSecond: fps] };
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

/// Set the renderer's memory budget: lean (the default) unless Fancy
/// Animation is on. Safe to call before the window exists and again whenever
/// the setting changes.
pub fn apply_memory_profile(low_ram: bool) {
    #[cfg(target_os = "macos")]
    {
        use i_slint_renderer_skia::memory;
        use std::time::Duration;
        const MB: usize = 1024 * 1024;
        // Lean: Now Playing's art (up to 2048 px) and its cached shadow
        // (~14 MB) alone outgrow 32 MB; Skia then re-creates one of them
        // every frame and the expand drops to 60 Hz. The idle purge still
        // returns the memory 3 s after the animation.
        let (limit, idle) = if low_ram {
            (48 * MB, Duration::from_secs(3))
        } else {
            (96 * MB, Duration::from_secs(15))
        };
        memory::set_gpu_cache_limit(limit);
        memory::set_idle_purge(idle);
        // Lean starts on two drawables; see `drawing_started`.
        LEAN.store(low_ram, std::sync::atomic::Ordering::Relaxed);
        THIRD_DRAWABLE.store(false, std::sync::atomic::Ordering::Relaxed);
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

// ---------------------------------------------------------------- open / close animation
//
// The window scales up from 92% while fading in when it opens, and shrinks
// and fades away when it closes or TIDE quits. TIDE drives this itself rather
// than through NSWindow.animationBehavior: the window server's per-window
// transform and alpha are set from a thread of their own, because the main
// thread is busy laying out and drawing the first frames just as the window
// appears, and an animation stepped there stutters or skips to the end.
// Reduce Motion turns it off.

#[cfg(target_os = "macos")]
mod window_anim {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSScreen, NSWindow, NSWindowAnimationBehavior, NSWindowStyleMask};

    const OPEN: Duration = Duration::from_millis(260);
    const CLOSE: Duration = Duration::from_millis(180);
    const SMALL: f64 = 0.92;
    /// Longest the window stays hidden waiting for its first frames.
    const FIRST_FRAME_WAIT: Duration = Duration::from_millis(600);

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AffineTransform {
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        tx: f64,
        ty: f64,
    }

    type MainConnection = unsafe extern "C" fn() -> i32;
    type SetTransform = unsafe extern "C" fn(i32, u32, AffineTransform) -> i32;
    type SetAlpha = unsafe extern "C" fn(i32, u32, f32) -> i32;

    #[derive(Clone, Copy)]
    struct Server {
        connection: i32,
        set_transform: SetTransform,
        set_alpha: SetAlpha,
    }

    #[repr(C)]
    struct DispatchQueue {
        _private: [u8; 0],
    }
    unsafe extern "C" {
        static _dispatch_main_q: DispatchQueue;
        fn dispatch_async_f(
            queue: *const DispatchQueue,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
        fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
    }
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

    /// The window server's calls, looked up at run time: they are not public
    /// API, so without them the window simply appears.
    fn server() -> Option<Server> {
        static FNS: std::sync::OnceLock<Option<(usize, usize, usize)>> =
            std::sync::OnceLock::new();
        let (conn, transform, alpha) = (*FNS.get_or_init(|| {
            let find = |names: &[&std::ffi::CStr]| {
                names.iter().find_map(|name| {
                    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name.
                    let ptr = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
                    (!ptr.is_null()).then_some(ptr as usize)
                })
            };
            Some((
                find(&[c"SLSMainConnectionID", c"CGSMainConnectionID"])?,
                find(&[c"SLSSetWindowTransform", c"CGSSetWindowTransform"])?,
                find(&[c"SLSSetWindowAlpha", c"CGSSetWindowAlpha"])?,
            ))
        }))?;
        // SAFETY: these symbols have these C signatures on every macOS that has them.
        unsafe {
            let main_connection = std::mem::transmute::<usize, MainConnection>(conn);
            Some(Server {
                connection: main_connection(),
                set_transform: std::mem::transmute::<usize, SetTransform>(transform),
                set_alpha: std::mem::transmute::<usize, SetAlpha>(alpha),
            })
        }
    }

    pub(super) fn is_main_window(window: &NSWindow) -> bool {
        use objc2::runtime::{AnyClass, NSObjectProtocol};
        // Key-value observing swaps the class for a hidden subclass
        // (NSKVONotifying_WinitWindow), so match subclasses too.
        AnyClass::get(c"WinitWindow").is_some_and(|cls| window.isKindOfClass(cls))
            && window.styleMask().contains(NSWindowStyleMask::Titled)
    }

    /// Where the window is, in the window server's top-left screen points.
    #[derive(Clone, Copy)]
    struct Geometry {
        id: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    fn geometry(window: &NSWindow) -> Option<Geometry> {
        let mtm = MainThreadMarker::new()?;
        let primary = NSScreen::screens(mtm).iter().next()?.frame();
        let frame = window.frame();
        let id = u32::try_from(window.windowNumber()).ok().filter(|&id| id > 0)?;
        Some(Geometry {
            id,
            x: frame.origin.x,
            y: primary.size.height - (frame.origin.y + frame.size.height),
            width: frame.size.width,
            height: frame.size.height,
        })
    }

    /// Draw the window at `scale` around its centre and at `alpha`. Safe off
    /// the main thread: it talks to the window server only.
    fn apply(server: Server, at: Geometry, scale: f64, alpha: f64) {
        // The transform maps screen points to window points.
        let (cx, cy) = (at.x + at.width / 2.0, at.y + at.height / 2.0);
        let inv = 1.0 / scale;
        let transform = AffineTransform {
            a: inv,
            b: 0.0,
            c: 0.0,
            d: inv,
            tx: cx - at.x - cx * inv,
            ty: cy - at.y - cy * inv,
        };
        // SAFETY: our own connection and window number; a failure (say, the
        // window is already gone) leaves nothing behind.
        unsafe {
            (server.set_transform)(server.connection, at.id, transform);
            (server.set_alpha)(server.connection, at.id, alpha as f32);
        }
    }

    // While the open animation owns the window, nothing else may change its
    // opacity: Slint restores alpha 1 once the first frame is drawn, which
    // would flash the window at full opacity for a frame before the fade.
    static HOLD_ALPHA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static SET_ALPHA: std::sync::OnceLock<objc2::runtime::Imp> = std::sync::OnceLock::new();

    type SetAlphaValue =
        unsafe extern "C-unwind" fn(*mut objc2::runtime::AnyObject, objc2::runtime::Sel, f64);

    /// NSWindow's own setAlphaValue:, past the guard below.
    fn set_alpha_value(window: &NSWindow, alpha: f64) {
        let Some(original) = SET_ALPHA.get() else {
            window.setAlphaValue(alpha);
            return;
        };
        // SAFETY: NSWindow's implementation of this selector, on an NSWindow.
        unsafe {
            let original: SetAlphaValue = std::mem::transmute(*original);
            original(
                (window as *const NSWindow).cast_mut().cast(),
                objc2::sel!(setAlphaValue:),
                alpha,
            );
        }
    }

    /// Override setAlphaValue: on winit's window class, once.
    fn guard_alpha() {
        use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
        use objc2::{ClassType, sel};

        unsafe extern "C-unwind" fn guarded(this: *mut AnyObject, cmd: Sel, alpha: f64) {
            if HOLD_ALPHA.load(Ordering::SeqCst) {
                return;
            }
            if let Some(original) = SET_ALPHA.get() {
                // SAFETY: NSWindow's implementation of this very selector.
                unsafe {
                    let original: SetAlphaValue = std::mem::transmute(*original);
                    original(this, cmd, alpha);
                }
            }
        }

        if SET_ALPHA.get().is_some() {
            return;
        }
        let sel = sel!(setAlphaValue:);
        let (Some(method), Some(cls)) = (
            NSWindow::class().instance_method(sel),
            AnyClass::get(c"WinitWindow"),
        ) else {
            return;
        };
        let _ = SET_ALPHA.set(method.implementation());
        // SAFETY: the override has the selector's signature (void, id self,
        // SEL, CGFloat) and lives for the whole process. winit's class does
        // not define the method itself, so this adds an override.
        unsafe {
            objc2::ffi::class_addMethod(
                (cls as *const AnyClass).cast_mut(),
                sel,
                std::mem::transmute::<SetAlphaValue, Imp>(guarded),
                c"v@:d".as_ptr(),
            );
        }
    }

    struct Run {
        window: Retained<NSWindow>,
        opening: bool,
        done: Option<Box<dyn FnOnce()>>,
        /// Where the window is now, read by the animation thread on every step.
        at: Arc<Mutex<Geometry>>,
    }

    thread_local! {
        static RUN: RefCell<Option<(u64, Run)>> = const { RefCell::new(None) };
    }
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    /// Back on the main thread once an animation has played to the end.
    extern "C" fn finished(context: *mut c_void) {
        let generation = context as usize as u64;
        let Some(mut run) = RUN.with_borrow_mut(|slot| {
            if slot.as_ref().is_some_and(|(g, _)| *g == generation) {
                slot.take().map(|(_, run)| run)
            } else {
                None
            }
        }) else {
            return;
        };
        if run.opening {
            // The last step may have raced a frame change and put the window
            // back where it was; AppKit's frame has the final say.
            if let (Some(server), Some(at)) = (server(), geometry(&run.window)) {
                apply(server, at, 1.0, 1.0);
            }
            // The server already shows it at full opacity; bring AppKit's
            // own idea of the alpha back in line.
            HOLD_ALPHA.store(false, Ordering::SeqCst);
            set_alpha_value(&run.window, 1.0);
        } else {
            // Left clear and small on purpose: making it opaque again here
            // would show it for a frame before the window server removes it.
            // The next open sets both itself.
            run.window.orderOut(None);
        }
        if let Some(done) = run.done.take() {
            done();
        }
    }

    fn start(window: &NSWindow, opening: bool, done: Option<Box<dyn FnOnce()>>) -> bool {
        use objc2::Message;
        let (Some(server), Some(at)) = (server(), geometry(window)) else {
            return false;
        };
        let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        // A close cut short by a reopen, or the reverse, hands over to this one.
        if let Some((_, mut old)) = RUN.take()
            && let Some(old_done) = old.done.take()
        {
            old_done();
        }
        let at = Arc::new(Mutex::new(at));
        RUN.set(Some((
            generation,
            Run {
                window: window.retain(),
                opening,
                done,
                at: at.clone(),
            },
        )));
        let place = move || *at.lock().unwrap_or_else(|e| e.into_inner());
        if opening {
            // AppKit orders the window in at this alpha, so it is invisible
            // from its very first frame on screen.
            guard_alpha();
            set_alpha_value(window, 0.0);
            HOLD_ALPHA.store(true, Ordering::SeqCst);
        } else if HOLD_ALPHA.swap(false, Ordering::SeqCst) {
            // Closed while still opening: AppKit still thinks it is clear.
            set_alpha_value(window, 1.0);
        }
        let frame = super::frame_interval();
        let duration = if opening { OPEN } else { CLOSE };
        let frames_before = i_slint_renderer_skia::memory::frames_rendered();
        let spawned = std::thread::Builder::new()
            .name("window-anim".into())
            .spawn(move || {
                if opening {
                    // Stay hidden until frames drawn after the window went on
                    // screen have reached the glass; revealing sooner shows
                    // an empty window that then snaps to its content.
                    let waiting = Instant::now();
                    while i_slint_renderer_skia::memory::frames_rendered() < frames_before + 3
                        && waiting.elapsed() < FIRST_FRAME_WAIT
                    {
                        if GENERATION.load(Ordering::SeqCst) != generation {
                            return;
                        }
                        apply(server, place(), SMALL, 0.0);
                        std::thread::sleep(frame);
                    }
                }
                let started = Instant::now();
                loop {
                    if GENERATION.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    let t = (started.elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0);
                    let (scale, alpha) = if opening {
                        // Quick out of the gate, settling softly into place.
                        let e = 1.0 - (1.0 - t).powi(3);
                        (SMALL + (1.0 - SMALL) * e, (t / 0.55).min(1.0))
                    } else {
                        let e = t * t;
                        (1.0 - (1.0 - SMALL) * e, 1.0 - e)
                    };
                    apply(server, place(), scale, alpha);
                    if t >= 1.0 {
                        break;
                    }
                    std::thread::sleep(frame);
                }
                // SAFETY: the main queue lives for the whole process; the
                // context is a plain integer, never dereferenced.
                unsafe {
                    dispatch_async_f(
                        &raw const _dispatch_main_q,
                        generation as usize as *mut c_void,
                        finished,
                    );
                }
            });
        if spawned.is_err() {
            RUN.set(None);
            HOLD_ALPHA.store(false, Ordering::SeqCst);
            set_alpha_value(window, 1.0);
            return false;
        }
        true
    }

    /// The window was moved or resized. The transform places the window on
    /// screen as well as scaling it, so a running animation has to follow:
    /// stepped from where it started, it would draw the window there and leave
    /// it there, away from the frame AppKit has, until the next move. A window
    /// does change frame as it opens when its saved frame no longer fits, say
    /// after a display was unplugged.
    pub(super) fn frame_changed(window: &NSWindow) {
        RUN.with_borrow(|run| {
            if let Some((_, run)) = run
                && std::ptr::eq(&*run.window, window)
                && let Some(now) = geometry(window)
            {
                *run.at.lock().unwrap_or_else(|e| e.into_inner()) = now;
            }
        });
    }

    /// The window is about to be ordered front: start it small and clear.
    pub(super) fn will_open(window: &NSWindow) {
        // AppKit's own animation would play on top of ours where it does run.
        window.setAnimationBehavior(NSWindowAnimationBehavior::None);
        // Never show AppKit's white backing, even for a frame.
        window.setBackgroundColor(Some(&objc2_app_kit::NSColor::blackColor()));
        if super::reduce_motion() || window.isVisible() {
            return;
        }
        start(window, true, None);
    }

    /// Shrink the window away, then run `done`. False (and `done` dropped
    /// unrun) when there is nothing to animate.
    pub(super) fn close(window: &NSWindow, done: Box<dyn FnOnce()>) -> bool {
        if super::reduce_motion()
            || !is_main_window(window)
            || !window.isVisible()
            || window.isMiniaturized()
            || window.styleMask().contains(NSWindowStyleMask::FullScreen)
        {
            return false;
        }
        start(window, false, Some(done))
    }
}

#[cfg(target_os = "macos")]
static ORDER_FRONT: std::sync::OnceLock<objc2::runtime::Imp> = std::sync::OnceLock::new();

/// Call before the first window is shown.
#[cfg(target_os = "macos")]
pub fn install_window_animation() {
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{ClassType, sel};

    unsafe extern "C-unwind" fn make_key_and_order_front(
        this: *mut AnyObject,
        cmd: Sel,
        sender: *mut AnyObject,
    ) {
        // SAFETY: only installed on NSWindow, so `this` is a live NSWindow.
        let window = unsafe { &*this.cast::<objc2_app_kit::NSWindow>() };
        if window_anim::is_main_window(window) {
            restore_chrome(window);
            window_anim::will_open(window);
        }
        if let Some(original) = ORDER_FRONT.get() {
            // SAFETY: NSWindow's own implementation of this very selector.
            unsafe {
                let original: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) =
                    std::mem::transmute(*original);
                original(this, cmd, sender);
            }
        }
    }

    if ORDER_FRONT.get().is_some() {
        return;
    }
    let cls: &AnyClass = objc2_app_kit::NSWindow::class();
    let sel = sel!(makeKeyAndOrderFront:);
    let Some(method) = cls.instance_method(sel) else {
        return;
    };
    let _ = ORDER_FRONT.set(method.implementation());
    // SAFETY: the replacement has the selector's signature (void, id self,
    // SEL, id sender), calls the original, and lives for the whole process.
    unsafe {
        objc2::ffi::class_replaceMethod(
            (cls as *const AnyClass).cast_mut(),
            sel,
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
                Imp,
            >(make_key_and_order_front),
            c"v@:@".as_ptr(),
        );
    }
}

/// Shrink the window away, then run `done` on the UI thread. Runs `done`
/// right away when there is nothing to animate.
#[cfg(target_os = "macos")]
pub fn animate_window_out(window: &slint::Window, done: impl FnOnce() + 'static) {
    let done: Box<dyn FnOnce()> = Box::new(done);
    match window.with_winit_window(ns_window).flatten() {
        Some(ns) => {
            // `close` drops `done` when it does not animate, so keep a way back.
            let slot = std::rc::Rc::new(std::cell::Cell::new(Some(done)));
            let later = slot.clone();
            let animating = window_anim::close(
                &ns,
                Box::new(move || {
                    if let Some(done) = later.take() {
                        done();
                    }
                }),
            );
            if !animating && let Some(done) = slot.take() {
                done();
            }
        }
        None => done(),
    }
}

/// ⌘Q: shrink the window away first, then let AppKit terminate.
/// Call after the main window exists, so winit's app delegate is registered.
#[cfg(target_os = "macos")]
pub fn install_quit_animation() {
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::sel;

    const TERMINATE_NOW: usize = 1;
    const TERMINATE_LATER: usize = 2;

    unsafe extern "C-unwind" fn should_terminate(
        _this: *mut AnyObject,
        _cmd: Sel,
        _app: *mut AnyObject,
    ) -> usize {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return TERMINATE_NOW;
        };
        let windows = objc2_app_kit::NSApplication::sharedApplication(mtm).windows();
        let animating = windows.iter().any(|window| {
            window_anim::close(
                &window,
                Box::new(move || {
                    objc2_app_kit::NSApplication::sharedApplication(mtm)
                        .replyToApplicationShouldTerminate(true);
                }),
            )
        });
        if animating {
            TERMINATE_LATER
        } else {
            TERMINATE_NOW
        }
    }

    let sel = sel!(applicationShouldTerminate:);
    let Some(cls) = AnyClass::get(c"WinitApplicationDelegate") else {
        return;
    };
    if cls.instance_method(sel).is_some() {
        return;
    }
    // SAFETY: the function matches the selector's signature (NSUInteger, id
    // self, SEL, NSApplication *) and lives for the whole process.
    unsafe {
        objc2::ffi::class_addMethod(
            (cls as *const AnyClass).cast_mut(),
            sel,
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize,
                Imp,
            >(should_terminate),
            c"Q@:@".as_ptr(),
        );
    }
}

/// The window was shown again after being closed: the menu bar is rebuilt
/// with it. Its title bar is put back as it is ordered front, see
/// [`restore_chrome`].
#[cfg(target_os = "macos")]
pub fn window_reopened() {
    configure_menu();
}

/// Slint rebuilds a closed window from its default attributes, without the
/// backend's hook, so it comes back with a standard title bar. Put the
/// transparent one back, before the window is on screen.
///
/// Slint places the *content* at the saved frame, so the rebuilt window is a
/// title bar taller than that, its top under the menu bar. Left alone, AppKit
/// pushes it down as it orders it front, and a style change made afterwards
/// keeps the content where it then is: the window ends up a title bar lower
/// and shorter than it was closed. Under a transparent title bar the content
/// is the whole window, so its frame is the content rect Slint asked for.
#[cfg(target_os = "macos")]
fn restore_chrome(ns: &objc2_app_kit::NSWindow) {
    use objc2_app_kit::{NSWindowStyleMask, NSWindowTitleVisibility};

    let mask = ns.styleMask();
    if mask.contains(NSWindowStyleMask::FullSizeContentView) {
        return;
    }
    let wanted = ns.contentRectForFrameRect(ns.frame());
    ns.setTitlebarAppearsTransparent(true);
    ns.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    ns.setStyleMask(mask | NSWindowStyleMask::FullSizeContentView);
    if !same_frame(ns.frame(), wanted) {
        ns.setFrame_display(wanted, false);
    }
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
    target: objc2_foundation::NSRect,
}

#[cfg(target_os = "macos")]
thread_local! {
    /// The frame to return to when the window is zoomed back.
    pub(super) static UNZOOMED: std::cell::Cell<Option<objc2_foundation::NSRect>> =
        const { std::cell::Cell::new(None) };
    static ZOOM: std::cell::RefCell<Option<ZoomAnimation>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_os = "macos")]
pub(super) fn zoom_target() -> Option<objc2_foundation::NSRect> {
    ZOOM.with_borrow(|z| z.as_ref().filter(|z| z.timer.running()).map(|z| z.target))
}

#[cfg(target_os = "macos")]
pub(super) fn cancel_zoom() {
    ZOOM.set(None);
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
    let hz = window
        .screen()
        .map(|s| s.maximumFramesPerSecond() as i64)
        .unwrap_or(60);
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
    timer.start(slint::TimerMode::Repeated, frame_interval_from_hz(hz), step);
    ZOOM.set(Some(ZoomAnimation {
        timer,
        to_filled,
        target,
    }));
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

/// Make TIDE's chrome the only thing that moves or zooms the window.
///
/// The title bar is transparent and the content extends under it, but AppKit
/// still treats the top strip as a title bar wherever the view under the
/// pointer says `mouseDownCanMoveWindow` (NSView's default for a non-opaque
/// view such as winit's). A press there then starts a WindowServer drag *and*
/// reaches TIDE's DragArea, which moves the window as well; a double-click
/// runs AppKit's Zoom/Fill *and* TIDE's animated zoom. Two movers fighting over
/// one frame make drags jitter.
///
/// winit registers its view class with the first window, so this runs from the
/// window event hook; it adds the override once, for every window after it.
#[cfg(target_os = "macos")]
fn disable_native_title_bar_drag() {
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    use objc2::sel;

    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if DONE.get() {
        return;
    }
    let Some(cls) = AnyClass::get(c"WinitView") else {
        return;
    };
    DONE.set(true);

    unsafe extern "C-unwind" fn cannot_move_window(_: *mut AnyObject, _: Sel) -> Bool {
        Bool::NO
    }
    type MouseDownCanMoveWindow = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool;
    // SAFETY: the override has the selector's signature (BOOL, id self, SEL)
    // and lives for the whole process. winit's class does not define the
    // method itself, so this adds an override of NSView's.
    let added = unsafe {
        objc2::ffi::class_addMethod(
            (cls as *const AnyClass).cast_mut(),
            sel!(mouseDownCanMoveWindow),
            std::mem::transmute::<MouseDownCanMoveWindow, Imp>(cannot_move_window),
            c"B@:".as_ptr(),
        )
    };
    tracing::debug!(added = added.as_bool(), "Title bar strip no longer drags natively");
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
pub(super) fn ns_window(
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
pub fn configure_input(window: &slint::Window, ctx: std::sync::Arc<crate::controller::Ctx>) {
    use slint::winit_030::winit::{
        event::ElementState,
        keyboard::{Key, ModifiersState},
    };
    let mut modifiers = ModifiersState::empty();
    #[cfg(target_os = "macos")]
    let mut scroll = macos_scroll_filter();
    window.on_winit_window_event(move |window, event| {
        if let WindowEvent::ModifiersChanged(value) = event {
            modifiers = value.state();
        }
        // The menu advertises the unshifted '=' shortcut. Accept '+' too,
        // including keyboard layouts that produce it without Shift.
        if let WindowEvent::KeyboardInput { event, .. } = event
            && matches!(&event.logical_key, Key::Character(text) if text == "+")
            && if cfg!(target_os = "macos") {
                modifiers.super_key()
            } else {
                modifiers.control_key()
            }
            && !modifiers.alt_key()
        {
            if event.state == ElementState::Pressed {
                crate::controller::on_ui(|ui| ui.change_zoom(1));
            }
            return EventResult::PreventDefault;
        }
        let percent = ctx.settings.read().unwrap().zoom_percent;
        if crate::zoom::window_event(window, event, percent) == EventResult::PreventDefault {
            return EventResult::PreventDefault;
        }
        #[cfg(target_os = "macos")]
        {
            disable_native_title_bar_drag();
            follow_window_move(window, event);
            if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
                if let Some(ns) = window.with_winit_window(ns_window).flatten() {
                    window_anim::frame_changed(&ns);
                }
                crate::window_state::observe(window, &ctx);
            }
            if matches!(
                event,
                WindowEvent::Focused(false) | WindowEvent::CloseRequested
            ) {
                crate::window_state::remember(window, &ctx);
            }
            scroll(window, event)
        }
        #[cfg(not(target_os = "macos"))]
        EventResult::Propagate
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

/// Marks the app menu's own Settings item ("TIDE" in ASCII).
#[cfg(target_os = "macos")]
const SETTINGS_TAG: isize = 0x5449_4445;
/// Marks the app menu's own Check for Updates… item.
#[cfg(target_os = "macos")]
const UPDATES_TAG: isize = SETTINGS_TAG + 1;

#[cfg(target_os = "macos")]
objc2::define_class!(
    /// Target of the app menu's own items (Check for Updates…, Settings), and
    /// the observer that puts them back after Slint rebuilds the menu bar.
    #[unsafe(super(objc2::runtime::NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    #[name = "TIDEMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            crate::controller::on_ui(|ui| ui.app().invoke_navigate("settings".into()));
        }

        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            crate::controller::on_ui(|ui| ui.app().invoke_check_for_updates());
        }

        #[unsafe(method(menuDidAddItem:))]
        fn menu_did_add_item(&self, _note: &objc2_foundation::NSNotification) {
            // A rebuild adds every item; reapply once it has finished.
            if !MENU_FIX_PENDING.replace(true) {
                slint::Timer::single_shot(std::time::Duration::ZERO, || {
                    MENU_FIX_PENDING.set(false);
                    install_settings_item();
                });
            }
        }
    }
);

#[cfg(target_os = "macos")]
thread_local! {
    static MENU_TARGET: std::cell::OnceCell<objc2::rc::Retained<MenuTarget>> =
        const { std::cell::OnceCell::new() };
    static MENU_FIX_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(target_os = "macos")]
fn menu_target(mtm: objc2::MainThreadMarker) -> objc2::rc::Retained<MenuTarget> {
    use objc2::msg_send;
    MENU_TARGET.with(|cell| {
        cell.get_or_init(|| {
            let this = mtm.alloc::<MenuTarget>().set_ivars(());
            let target: objc2::rc::Retained<MenuTarget> = unsafe { msg_send![super(this), init] };
            // Slint rebuilds the whole menu bar whenever a menu binding changes
            // (an item's `enabled`, say), which also recreates the app menu.
            unsafe {
                objc2_foundation::NSNotificationCenter::defaultCenter()
                    .addObserver_selector_name_object(
                        &target,
                        objc2::sel!(menuDidAddItem:),
                        Some(objc2_app_kit::NSMenuDidAddItemNotification),
                        None,
                    );
            }
            target
        })
        .clone()
    })
}

/// Slint builds the standard macOS app menu, but exposes no app-menu roles.
/// Show Check for Updates… and Settings in the app menu once the window is
/// shown, and after every menu bar rebuild.
#[cfg(target_os = "macos")]
pub fn configure_menu() {
    fn install_when_ready(attempts_left: u8) {
        slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
            if install_settings_item().is_none() && attempts_left > 1 {
                install_when_ready(attempts_left - 1);
            }
        });
    }

    // Winit creates a default app menu first. Slint adds the window's menus
    // after attaching the native window.
    install_when_ready(25);
}

/// Slint's own items stay in View, hidden: a rebuild removes every item Slint
/// added from the menu it added it to, and AppKit aborts if one of them has
/// moved. The app menu gets separate items instead.
#[cfg(target_os = "macos")]
fn install_settings_item() -> Option<()> {
    use objc2::{MainThreadMarker, MainThreadOnly as _};
    use objc2_app_kit::{NSApplication, NSMenuItem};
    use objc2_foundation::NSString;

    let mtm = MainThreadMarker::new()?;
    let target = menu_target(mtm);
    let app = NSApplication::sharedApplication(mtm);
    let bar = app.mainMenu()?;
    let app_item = bar.itemAtIndex(0)?;
    let app_menu = app_item.submenu()?;
    let view = bar.itemWithTitle(&NSString::from_str("View"))?.submenu()?;
    let name = NSString::from_str(env!("CARGO_BIN_NAME"));
    app_item.setTitle(&name);
    app_menu.setTitle(&name);
    // About TIDE, Check for Updates…, Settings…, as other Mac apps order them.
    let mirrored = [
        (
            "Check for Updates…",
            UPDATES_TAG,
            objc2::sel!(checkForUpdates:),
        ),
        ("Settings…", SETTINGS_TAG, objc2::sel!(openSettings:)),
    ];
    for (index, (title, tag, action)) in mirrored.into_iter().enumerate() {
        let slint_item = view.itemWithTitle(&NSString::from_str(title))?;
        // AppKit won't give a second item the same shortcut, so ⌘, moves too.
        let key = slint_item.keyEquivalent();
        let mask = slint_item.keyEquivalentModifierMask();
        slint_item.setHidden(true);
        slint_item.setKeyEquivalent(&NSString::new());
        let item = match app_menu.itemWithTag(tag) {
            Some(item) => item,
            None => {
                let item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &slint_item.title(),
                        Some(action),
                        &NSString::new(),
                    )
                };
                item.setTag(tag);
                unsafe { item.setTarget(Some(&target)) };
                app_menu.insertItem_atIndex(&item, 1 + index as isize);
                tracing::debug!("Added {title} to the TIDE application menu");
                item
            }
        };
        if key.length() > 0 {
            item.setKeyEquivalent(&key);
            item.setKeyEquivalentModifierMask(mask);
        }
    }
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
