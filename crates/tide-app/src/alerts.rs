//! In-app view of the log. Every event the layer sees is appended to the debug
//! panel's log tail. ERROR events, from any crate, also become the one in-app
//! alert (a new one replaces the one on screen) and an entry in the panel's
//! error list.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{Model, ModelRc, VecModel};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, Layer};

use crate::controller::{on_ui, with_ui};
use crate::{App, ErrorEntry, LogEntry};

/// How long an alert stays up before it dismisses itself.
const ALERT_TTL: Duration = Duration::from_secs(8);
/// Errors kept in the debug panel, newest first.
const KEPT_ERRORS: usize = 4;
/// Log lines kept in the debug panel, newest first.
const KEPT_LOGS: usize = 60;

thread_local! {
    static ERRORS: Rc<VecModel<ErrorEntry>> = Rc::new(VecModel::default());
    static LOGS: Rc<VecModel<LogEntry>> = Rc::new(VecModel::default());
    static LOGS_BOUND: Cell<bool> = const { Cell::new(false) };
    static DISMISS: slint::Timer = slint::Timer::default();
    static SERIAL: RefCell<i32> = const { RefCell::new(0) };
}

struct Line {
    time: String,
    level: Level,
    source: String,
    message: String,
    fields: String,
}

/// Lines logged since the UI last picked them up, oldest first.
static PENDING: Mutex<Vec<Line>> = Mutex::new(Vec::new());
/// A flush to the UI thread is already queued.
static SCHEDULED: AtomicBool = AtomicBool::new(false);

/// What the debug panel shows: TIDE's own crates down to DEBUG, everything else from INFO.
pub fn panel_filter() -> Targets {
    let mut targets = Targets::new().with_default(Level::INFO);
    for own in ["TIDE", "tide_api", "tide_audio", "tide_cache", "tide_macos"] {
        targets = targets.with_target(own, Level::DEBUG);
    }
    targets
}

/// Tracing layer that forwards events to the UI thread.
pub struct AlertLayer;

impl<S: Subscriber> Layer<S> for AlertLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let line = Line {
            time: utc_clock(),
            level: *event.metadata().level(),
            source: event.metadata().target().to_string(),
            message: fields.message,
            fields: fields.rest,
        };
        let Ok(mut pending) = PENDING.lock() else { return };
        pending.push(line);
        // A burst before the UI is up must not grow without bound.
        if pending.len() > KEPT_LOGS * 4 {
            let excess = pending.len() - KEPT_LOGS * 4;
            pending.drain(..excess);
        }
        drop(pending);
        // One hop to the UI thread carries every line logged until it runs.
        if !SCHEDULED.swap(true, Ordering::SeqCst) && slint::invoke_from_event_loop(flush).is_err() {
            SCHEDULED.store(false, Ordering::SeqCst);
        }
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
}

impl Fields {
    fn push(&mut self, field: &Field, value: std::fmt::Arguments<'_>) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value}");
        } else {
            if !self.rest.is_empty() {
                self.rest.push(' ');
            }
            let _ = write!(self.rest, "{}={value}", field.name());
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, format_args!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, format_args!("{value}"));
    }
}

/// Wall-clock time as the log lines print it (UTC).
fn utc_clock() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Move pending lines into the panel. UI thread only.
fn flush() {
    SCHEDULED.store(false, Ordering::SeqCst);
    // Until the window exists the lines stay pending; the next event retries.
    let _ = with_ui(|ui| {
        let lines = PENDING.lock().map(|mut pending| std::mem::take(&mut *pending)).unwrap_or_default();
        let app = ui.app();
        LOGS.with(|logs| {
            for line in &lines {
                let mut text = line.message.clone();
                if !line.fields.is_empty() {
                    let _ = write!(text, "  {}", line.fields);
                }
                logs.insert(0, LogEntry { time: line.time.as_str().into(), level: line.level.as_str().into(), text: text.into() });
            }
            while logs.row_count() > KEPT_LOGS {
                logs.remove(logs.row_count() - 1);
            }
            if !LOGS_BOUND.replace(true) {
                app.set_debug_logs(ModelRc::from(logs.clone()));
            }
        });
        for line in lines.into_iter().filter(|line| line.level == Level::ERROR) {
            show_error(&app, line);
        }
    });
}

/// Replace the alert on screen and log the error in the debug panel.
fn show_error(app: &App<'_>, line: Line) {
    ERRORS.with(|errors| {
        errors.insert(
            0,
            ErrorEntry {
                time: format!("{} UTC", line.time).into(),
                source: line.source.into(),
                message: line.message.as_str().into(),
                fields: line.fields.into(),
            },
        );
        while errors.row_count() > KEPT_ERRORS {
            errors.remove(errors.row_count() - 1);
        }
        if errors.row_count() == 1 {
            // First error: hand the panel the shared model; later ones update it in place.
            app.set_debug_errors(ModelRc::from(errors.clone()));
        }
    });
    post(app, &line.message, true);
}

/// Show a short confirmation in the alert's place ("Added to playlist").
pub fn notice(app: &App<'_>, text: &str) {
    post(app, text, false);
}

fn post(app: &App<'_>, text: &str, error: bool) {
    let serial = SERIAL.with(|s| {
        let mut s = s.borrow_mut();
        *s = s.wrapping_add(1);
        *s
    });
    app.set_alert_error(error);
    app.set_alert_text(text.into());
    app.set_alert_serial(serial);
    // Restarting the timer drops the previous alert's deadline.
    DISMISS.with(|timer| {
        timer.start(slint::TimerMode::SingleShot, ALERT_TTL, || {
            on_ui(|ui| ui.app().set_alert_text("".into()));
        });
    });
}
