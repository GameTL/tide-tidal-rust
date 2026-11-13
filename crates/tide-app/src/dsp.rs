//! DSP page: the user (USR) and speaker (SPK) equalisers.
//!
//! Both EQs are remembered per sound output, by its name: when playback moves
//! to another device, that device's pair is loaded. Presets, and the text
//! formats AutoEq and squig.link export, move an EQ between devices and apps.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use slint::{Color, Model, ModelRc, VecModel};
use tide_audio::eq::{self, Band, BandKind, EqProfile, MAX_BANDS};

use super::{Ui, on_ui};
use crate::alerts::notice;
use crate::{EqBand, EqHit};

/// The graphs span ±this many dB.
const GRAPH_DB: f32 = 15.0;
const CURVE_POINTS: usize = 240;
/// Pre-amp slider range, in dB.
const PREAMP_MIN: f32 = -20.0;
const PREAMP_MAX: f32 = 6.0;
const AUTOEQ_RESULTS: &str =
    "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results";
/// The AutoEq index is fetched again when the saved copy is older than this.
const AUTOEQ_MAX_AGE_SECS: u64 = 24 * 3600;
const AUTOEQ_SHOWN: usize = 80;
/// Slider / drag ticks closer than this share one undo step.
const UNDO_LIVE_GAP: Duration = Duration::from_millis(400);
const UNDO_LIMIT: usize = 50;
/// Dot colours, by band number.
const BAND_COLORS: [(u8, u8, u8); 8] = [
    (0x14, 0xb8, 0xa6),
    (0xf9, 0xa8, 0xd4),
    (0x81, 0x8c, 0xf8),
    (0xfa, 0xe0, 0x3c),
    (0xc0, 0x84, 0xfc),
    (0xfb, 0x92, 0x3c),
    (0x38, 0xbd, 0xf8),
    (0x86, 0xef, 0xac),
];

/// The two equalisers of one sound output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct DeviceDsp {
    enabled: bool,
    usr: EqProfile,
    spk: EqProfile,
    #[serde(default)]
    preamp: Option<f32>,
    auto_preamp: bool,
}

impl Default for DeviceDsp {
    fn default() -> Self {
        // On but flat: nothing is altered until a band or the pre-amp moves.
        let flat = || EqProfile {
            enabled: true,
            ..Default::default()
        };
        Self {
            enabled: true,
            usr: flat(),
            spk: flat(),
            preamp: Some(0.0),
            auto_preamp: false,
        }
    }
}

impl DeviceDsp {
    fn migrate_preamp(&mut self) {
        if self.preamp.is_none() {
            self.preamp = Some(
                (if self.usr.enabled {
                    self.usr.preamp
                } else {
                    0.0
                }) + (if self.spk.enabled {
                    self.spk.preamp
                } else {
                    0.0
                }),
            );
            self.usr.preamp = 0.0;
            self.spk.preamp = 0.0;
        }
    }

    fn gain(&self) -> f32 {
        self.preamp.unwrap_or(0.0)
    }

    fn playback_profiles(&self) -> Vec<EqProfile> {
        let mut spk = self.spk.clone();
        let mut usr = self.usr.clone();
        spk.enabled &= self.enabled;
        usr.enabled &= self.enabled;
        vec![
            spk,
            usr,
            EqProfile {
                enabled: self.enabled,
                preamp: self.gain(),
                ..Default::default()
            },
        ]
    }

    fn update_auto_preamp(&mut self) {
        if self.auto_preamp {
            let peak = eq::log_frequencies(1024)
                .map(|f| {
                    (if self.usr.enabled {
                        self.usr.response_db(f)
                    } else {
                        0.0
                    }) + (if self.spk.enabled {
                        self.spk.response_db(f)
                    } else {
                        0.0
                    })
                })
                .fold(0.0_f32, f32::max);
            self.preamp = Some((-peak * 10.0).floor() / 10.0);
        }
    }
}

/// Everything the DSP page saves.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct DspStore {
    /// Sound output name → its equalisers.
    devices: BTreeMap<String, DeviceDsp>,
    presets: Vec<EqProfile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AutoEqEntry {
    name: String,
    /// "oratory1990", "crinacle on 711", …
    source: String,
    /// Folder under `results/`, URL-encoded as in the index.
    path: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct AutoEqIndex {
    /// Unix seconds.
    fetched: u64,
    entries: Vec<AutoEqEntry>,
}

#[derive(Clone, PartialEq)]
struct DspSnap {
    tab: usize,
    selected: Option<usize>,
    device: DeviceDsp,
}

#[derive(Default)]
pub(super) struct DspState {
    store: DspStore,
    /// Name of the sound output the EQs on screen belong to.
    device: String,
    /// 0 = USR EQ, 1 = SPK EQ.
    tab: usize,
    selected: Option<usize>,
    /// Kept across updates so a dot being dragged is not rebuilt under the pointer.
    bands: Rc<VecModel<EqBand>>,
    /// Changed since the last save.
    dirty: bool,
    autoeq: AutoEqIndex,
    /// Index entries behind the rows on screen.
    hits: Vec<usize>,
    autoeq_busy: bool,
    undo: Vec<DspSnap>,
    redo: Vec<DspSnap>,
    /// Last live (slider / drag) edit, for coalescing one gesture into one undo.
    live_at: Option<Instant>,
}

impl DspState {
    fn device_mut(&mut self) -> &mut DeviceDsp {
        let device = self.store.devices.entry(self.device.clone()).or_default();
        device.migrate_preamp();
        device
    }

    fn current_mut(&mut self) -> &mut EqProfile {
        let tab = self.tab;
        let device = self.device_mut();
        if tab == 0 {
            &mut device.usr
        } else {
            &mut device.spk
        }
    }

    fn snap(&self) -> DspSnap {
        DspSnap {
            tab: self.tab,
            selected: self.selected,
            device: self
                .store
                .devices
                .get(&self.device)
                .cloned()
                .unwrap_or_default(),
        }
    }

    fn restore(&mut self, snap: DspSnap) {
        self.tab = snap.tab;
        self.selected = snap.selected;
        self.store.devices.insert(self.device.clone(), snap.device);
        self.dirty = true;
        self.live_at = None;
    }

    /// Remember the EQ as it is now, unless this is still the same live gesture.
    fn record_undo(&mut self, live: bool) {
        let now = Instant::now();
        let coalesce = live
            && self
                .live_at
                .is_some_and(|at| now.saturating_duration_since(at) < UNDO_LIVE_GAP);
        self.live_at = live.then_some(now);
        if coalesce {
            return;
        }
        let snap = self.snap();
        if self.undo.last() == Some(&snap) {
            return;
        }
        self.undo.push(snap);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn apply_undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snap());
        self.restore(prev);
        true
    }

    fn apply_redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snap());
        self.restore(next);
        true
    }

    fn forget_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.live_at = None;
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn tab_label(tab: usize) -> &'static str {
    if tab == 0 { "USR EQ" } else { "SPK EQ" }
}

/// Graph x (0..1) ↔ frequency, on a log axis from 20 Hz to 20 kHz.
fn freq_to_x(freq: f32) -> f32 {
    ((freq / eq::MIN_FREQ).ln() / (eq::MAX_FREQ / eq::MIN_FREQ).ln()).clamp(0.0, 1.0)
}

fn x_to_freq(x: f32) -> f32 {
    let freq = eq::MIN_FREQ * (eq::MAX_FREQ / eq::MIN_FREQ).powf(x.clamp(0.0, 1.0));
    // Whole hertz, then tens: finer steps cannot be heard or read.
    if freq < 1000.0 {
        freq.round()
    } else {
        (freq / 10.0).round() * 10.0
    }
}

/// Graph y (0 at the top) ↔ gain.
fn gain_to_y(gain: f32) -> f32 {
    (0.5 - gain / (2.0 * GRAPH_DB)).clamp(0.0, 1.0)
}

fn y_to_gain(y: f32) -> f32 {
    let gain = (0.5 - y.clamp(0.0, 1.0)) * 2.0 * GRAPH_DB;
    (gain * 10.0).round() / 10.0
}

/// Q slider position (0..1) ↔ Q, on a log scale.
fn q_to_pos(q: f32) -> f32 {
    ((q / eq::MIN_Q).ln() / (eq::MAX_Q / eq::MIN_Q).ln()).clamp(0.0, 1.0)
}

fn pos_to_q(pos: f32) -> f32 {
    let q = eq::MIN_Q * (eq::MAX_Q / eq::MIN_Q).powf(pos.clamp(0.0, 1.0));
    (q * 100.0).round() / 100.0
}

/// SVG path of a response curve in a 1000 × 1000 box.
fn curve_path(min_db: f32, max_db: f32, gain_at: impl Fn(f32) -> f32) -> String {
    let mut path = String::with_capacity(CURVE_POINTS * 14);
    for (i, freq) in eq::log_frequencies(CURVE_POINTS).enumerate() {
        let x = i as f32 / (CURVE_POINTS - 1) as f32 * 1000.0;
        let y = ((max_db - gain_at(freq)) / (max_db - min_db)).clamp(0.0, 1.0) * 1000.0;
        path.push_str(&format!("{}{x:.1} {y:.1} ", if i == 0 { "M" } else { "L" }));
    }
    path
}

fn trim_number(value: f32, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// Entries of AutoEq's `results/INDEX.md`:
/// `- [Name](./source/rig/Name) by source on rig`.
fn parse_autoeq_index(markdown: &str) -> Vec<AutoEqEntry> {
    markdown
        .lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix("- [")?;
            let (name, rest) = line.split_once("](./")?;
            let (path, source) = rest.rsplit_once(") by ")?;
            Some(AutoEqEntry {
                name: name.to_string(),
                source: source.trim().to_string(),
                path: path.to_string(),
            })
        })
        .collect()
}

/// Address of an entry's parametric EQ file.
fn autoeq_file_url(entry: &AutoEqEntry) -> String {
    let folder = entry.path.replace('#', "%23").replace('?', "%3F");
    let name = folder.rsplit('/').next().unwrap_or_default();
    format!("{AUTOEQ_RESULTS}/{folder}/{name}%20ParametricEQ.txt")
}

fn autoeq_matches(entries: &[AutoEqEntry], query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            let hay = format!("{} {}", e.name, e.source).to_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .map(|(i, _)| i)
        .take(AUTOEQ_SHOWN)
        .collect()
}

async fn fetch_text(url: &str) -> Result<String, String> {
    reqwest::get(url)
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())
}

impl Ui {
    /// Load the saved EQs and switch on the pair for the current output.
    pub(super) fn dsp_boot(&self) {
        {
            let mut dsp = self.dsp.borrow_mut();
            dsp.store = self
                .ctx
                .meta
                .get_json("dsp")
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();
            dsp.device = self.ctx.player.output_info().name;
            self.app().set_dsp_bands(ModelRc::from(dsp.bands.clone()));
        }
        self.app().set_dsp_range(GRAPH_DB);
        self.dsp_apply();
        self.dsp_present();
    }

    /// Follow playback to another sound output: save, then load that output's EQs.
    pub(super) fn dsp_sync_device(&self) {
        let name = self.ctx.player.output_info().name;
        if name == self.dsp.borrow().device {
            return;
        }
        self.dsp_flush();
        let active = {
            let mut dsp = self.dsp.borrow_mut();
            dsp.device = name.clone();
            dsp.selected = None;
            dsp.forget_history();
            let device = dsp.device_mut();
            device.enabled
                && (device.usr.alters_audio()
                    || device.spk.alters_audio()
                    || device.gain().abs() > 0.005)
        };
        self.dsp_apply();
        self.dsp_present();
        if active {
            notice(&self.app(), &format!("EQ for {name} is on"));
        }
    }

    /// Hand the current output's EQs to the player. SPK runs first, then USR.
    fn dsp_apply(&self) {
        let device = self.dsp.borrow_mut().device_mut().clone();
        self.ctx.player.set_eq(device.playback_profiles());
    }

    /// Write changes to disk. Called from the tick, so dragging a dot does not
    /// save on every pointer move.
    pub(super) fn dsp_flush(&self) {
        let mut dsp = self.dsp.borrow_mut();
        if !std::mem::take(&mut dsp.dirty) {
            return;
        }
        let json = serde_json::to_string(&dsp.store).unwrap_or_default();
        if let Err(e) = self.ctx.meta.put_json("dsp", &json) {
            tracing::warn!("could not save the EQs: {e}");
        }
    }

    /// What the signal path says about the EQs, when any is altering the sound.
    pub(super) fn dsp_signal_detail(&self) -> Option<String> {
        let device = self.dsp.borrow_mut().device_mut().clone();
        if !device.enabled {
            return None;
        }
        let active: Vec<(&str, &EqProfile)> = [("SPK EQ", &device.spk), ("USR EQ", &device.usr)]
            .into_iter()
            .filter(|(_, eq)| eq.alters_audio())
            .collect();
        if active.is_empty() && device.gain().abs() <= 0.005 {
            return None;
        }
        let names: Vec<&str> = active.iter().map(|(name, _)| *name).collect();
        let bands: usize = active
            .iter()
            .map(|(_, eq)| eq.bands.iter().filter(|b| b.enabled).count())
            .sum();
        let preamp = device.gain();
        Some(format!(
            "{} · {bands} band{} · preamp {preamp:+.1} dB",
            if names.is_empty() {
                "Preamp".to_string()
            } else {
                names.join(" + ")
            },
            if bands == 1 { "" } else { "s" }
        ))
    }

    /// Change the EQ on screen, then make it heard, shown and saved.
    fn dsp_edit(&self, change: impl FnOnce(&mut EqProfile, &mut Option<usize>)) {
        self.dsp_edit_inner(false, change);
    }

    fn dsp_edit_live(&self, change: impl FnOnce(&mut EqProfile, &mut Option<usize>)) {
        self.dsp_edit_inner(true, change);
    }

    fn dsp_edit_inner(&self, live: bool, change: impl FnOnce(&mut EqProfile, &mut Option<usize>)) {
        {
            let mut dsp = self.dsp.borrow_mut();
            dsp.record_undo(live);
            let mut selected = dsp.selected;
            let current = dsp.current_mut();
            change(current, &mut selected);
            current.preamp = current.preamp.clamp(-eq::MAX_GAIN, eq::MAX_GAIN);
            current.bands.truncate(MAX_BANDS);
            for band in &mut current.bands {
                *band = band.clamped();
            }
            let count = current.bands.len();
            let imported_preamp = current.preamp;
            current.preamp = 0.0;
            let device = dsp.device_mut();
            if imported_preamp != 0.0 {
                device.preamp = Some(device.gain() + imported_preamp);
            }
            device.update_auto_preamp();
            dsp.selected = selected.filter(|i| *i < count);
            dsp.dirty = true;
        }
        self.dsp_apply();
        self.dsp_present();
        self.present_signal();
    }

    fn dsp_undo(&self) {
        if !self.dsp.borrow_mut().apply_undo() {
            return;
        }
        self.dsp_apply();
        self.dsp_present();
        self.present_signal();
    }

    fn dsp_redo(&self) {
        if !self.dsp.borrow_mut().apply_redo() {
            return;
        }
        self.dsp_apply();
        self.dsp_present();
        self.present_signal();
    }

    /// Change the selected band.
    fn dsp_edit_band(&self, change: impl FnOnce(&mut Band)) {
        self.dsp_edit(|eq, selected| {
            if let Some(band) = selected.and_then(|i| eq.bands.get_mut(i)) {
                change(band);
            }
        });
    }

    pub(super) fn dsp_present(&self) {
        let app = self.app();
        let mut dsp = self.dsp.borrow_mut();
        let tab = dsp.tab;
        let device = dsp.device_mut().clone();
        let current = if tab == 0 { &device.usr } else { &device.spk };
        let selected = dsp.selected.filter(|i| *i < current.bands.len());

        app.set_dsp_device(dsp.device.as_str().into());
        app.set_dsp_outputs(dsp.store.devices.len() as i32);
        app.set_dsp_tab(tab as i32);
        app.set_dsp_enabled(device.enabled);
        app.set_dsp_usr_enabled(device.usr.enabled);
        app.set_dsp_spk_enabled(device.spk.enabled);
        app.set_dsp_preamp_text(trim_number(device.gain(), 1).into());
        app.set_dsp_preamp_pos(
            ((device.gain() - PREAMP_MIN) / (PREAMP_MAX - PREAMP_MIN)).clamp(0.0, 1.0),
        );
        app.set_dsp_auto_preamp(device.auto_preamp);
        app.set_dsp_usr_on(device.usr.alters_audio());
        app.set_dsp_spk_on(device.spk.alters_audio());
        app.set_dsp_active(
            device.enabled
                && (device.usr.alters_audio()
                    || device.spk.alters_audio()
                    || device.gain().abs() > 0.005),
        );

        // What reaches the device: both EQs and their pre-amps.
        let with_preamp = |eq: &EqProfile, freq: f32| -> f32 {
            if eq.enabled {
                eq.response_db(freq)
            } else {
                0.0
            }
        };
        // Fit all visible curves with a little headroom, using round grid bounds.
        let mut low: f32 = 0.0;
        let mut high: f32 = 0.0;
        let mut editor_peak: f32 = GRAPH_DB;
        for f in eq::log_frequencies(CURVE_POINTS) {
            let usr = with_preamp(&device.usr, f);
            let spk = with_preamp(&device.spk, f);
            for value in [usr, spk, usr + spk + device.gain()] {
                low = low.min(value);
                high = high.max(value);
            }
            editor_peak = editor_peak.max(current.response_db(f).abs());
            if let Some(band) = selected.map(|i| current.bands[i]).filter(|b| b.enabled) {
                editor_peak = editor_peak.max(band.response_db(f).abs());
            }
        }
        for band in &current.bands {
            editor_peak = editor_peak.max(band.gain.abs());
        }
        let mut step = ((high - low + 4.0) / 4.0 / 3.0).ceil().max(1.0) * 3.0;
        let (min_db, max_db) = loop {
            let min = ((low - 1.0) / step).floor() * step;
            let max = min + 4.0 * step;
            if max >= high + 1.0 {
                break (min, max);
            }
            step += 3.0;
        };
        let editor_range = (editor_peak / 6.0).ceil() * 6.0;
        app.set_dsp_result_min(min_db);
        app.set_dsp_result_max(max_db);
        app.set_dsp_range(editor_range);
        app.set_dsp_total_path(
            curve_path(min_db, max_db, |f| {
                if device.enabled {
                    with_preamp(&device.usr, f) + with_preamp(&device.spk, f) + device.gain()
                } else {
                    0.0
                }
            })
            .into(),
        );
        app.set_dsp_usr_path(curve_path(min_db, max_db, |f| with_preamp(&device.usr, f)).into());
        app.set_dsp_spk_path(curve_path(min_db, max_db, |f| with_preamp(&device.spk, f)).into());
        // The editor leaves the pre-amp out, so each dot sits at its band's gain.
        app.set_dsp_curve_path(
            curve_path(-editor_range, editor_range, |f| current.response_db(f)).into(),
        );
        app.set_dsp_band_path(
            selected
                .map(|i| current.bands[i])
                .filter(|b| b.enabled)
                .map(|b| curve_path(-editor_range, editor_range, |f| b.response_db(f)))
                .unwrap_or_default()
                .into(),
        );

        let rows: Vec<EqBand> = current
            .bands
            .iter()
            .enumerate()
            .map(|(i, band)| {
                let (r, g, b) = BAND_COLORS[i % BAND_COLORS.len()];
                EqBand {
                    x: freq_to_x(band.freq),
                    y: (0.5 - band.gain / (2.0 * editor_range)).clamp(0.0, 1.0),
                    kind: match band.kind {
                        BandKind::Peak => 0,
                        BandKind::LowShelf => 1,
                        BandKind::HighShelf => 2,
                    },
                    enabled: band.enabled,
                    tint: Color::from_rgb_u8(r, g, b),
                }
            })
            .collect();
        if dsp.bands.row_count() == rows.len() {
            for (i, row) in rows.into_iter().enumerate() {
                if dsp.bands.row_data(i).as_ref() != Some(&row) {
                    dsp.bands.set_row_data(i, row);
                }
            }
        } else {
            dsp.bands.set_vec(rows);
        }

        app.set_dsp_selected(selected.map_or(-1, |i| i as i32));
        if let Some(band) = selected.map(|i| current.bands[i]) {
            app.set_dsp_band_kind(match band.kind {
                BandKind::Peak => 0,
                BandKind::LowShelf => 1,
                BandKind::HighShelf => 2,
            });
            app.set_dsp_band_enabled(band.enabled);
            app.set_dsp_freq_text(trim_number(band.freq, 0).into());
            app.set_dsp_gain_text(trim_number(band.gain, 1).into());
            app.set_dsp_q_text(trim_number(band.q, 2).into());
            app.set_dsp_freq_pos(freq_to_x(band.freq));
            app.set_dsp_gain_pos(1.0 - gain_to_y(band.gain));
            app.set_dsp_q_pos(q_to_pos(band.q));
        }

        // Above 0 dB a full-scale signal clips at the device.
        let peak = eq::log_frequencies(CURVE_POINTS)
            .map(|f| with_preamp(&device.usr, f) + with_preamp(&device.spk, f) + device.gain())
            .fold(f32::MIN, f32::max);
        app.set_dsp_overshoot(if device.enabled && peak > 0.05 {
            peak
        } else {
            0.0
        });

        let presets: Vec<slint::SharedString> = dsp
            .store
            .presets
            .iter()
            .map(|p| p.name.as_str().into())
            .collect();
        app.set_dsp_presets(ModelRc::new(VecModel::from(presets)));
        app.set_dsp_can_undo(!dsp.undo.is_empty());
        app.set_dsp_can_redo(!dsp.redo.is_empty());
    }

    pub(super) fn dsp_wire(self: &Rc<Self>) {
        let app = self.app();
        macro_rules! bind {
            ($setter:ident, |$ui:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(self);
                app.$setter(move |$($arg),*| {
                    if let Some($ui) = weak.upgrade() {
                        $body
                    }
                });
            }};
        }
        bind!(on_dsp_select_tab, |ui, tab| {
            {
                let mut dsp = ui.dsp.borrow_mut();
                dsp.tab = usize::from(tab != 0);
                dsp.selected = None;
            }
            ui.dsp_present();
        });
        bind!(on_dsp_set_eq_enabled, |ui, tab, on| {
            {
                let mut dsp = ui.dsp.borrow_mut();
                let device = dsp.device_mut();
                if tab == 0 {
                    device.usr.enabled = on;
                } else {
                    device.spk.enabled = on;
                }
                device.update_auto_preamp();
                dsp.dirty = true;
            }
            ui.dsp_apply();
            ui.dsp_present();
            ui.present_signal();
        });
        bind!(on_dsp_set_preamp, |ui, text, pos| {
            let value = if pos >= 0.0 {
                Some(
                    ((PREAMP_MIN + (PREAMP_MAX - PREAMP_MIN) * pos.clamp(0.0, 1.0)) * 10.0).round()
                        / 10.0,
                )
            } else {
                text.trim().parse::<f32>().ok().filter(|v| v.is_finite())
            };
            if let Some(value) = value {
                {
                    let mut dsp = ui.dsp.borrow_mut();
                    dsp.record_undo(pos >= 0.0);
                    let device = dsp.device_mut();
                    device.preamp = Some(value.clamp(PREAMP_MIN, PREAMP_MAX));
                    device.auto_preamp = false;
                    dsp.dirty = true;
                }
                ui.dsp_apply();
                ui.present_signal();
            }
            ui.dsp_present();
        });
        bind!(on_dsp_toggle_auto_preamp, |ui| {
            {
                let mut dsp = ui.dsp.borrow_mut();
                dsp.record_undo(false);
                let device = dsp.device_mut();
                device.auto_preamp = !device.auto_preamp;
                device.update_auto_preamp();
                dsp.dirty = true;
            }
            ui.dsp_apply();
            ui.dsp_present();
            ui.present_signal();
        });
        bind!(on_dsp_set_enabled, |ui, on| {
            {
                let mut dsp = ui.dsp.borrow_mut();
                dsp.record_undo(false);
                let device = dsp.device_mut();
                device.enabled = on;
                dsp.dirty = true;
            }
            ui.dsp_apply();
            ui.dsp_present();
            ui.present_signal();
        });
        bind!(on_dsp_select_band, |ui, index| {
            ui.dsp.borrow_mut().selected = usize::try_from(index).ok();
            ui.dsp_present();
        });
        bind!(on_dsp_move_band, |ui, index, x, y| ui.dsp_edit_live(
            |eq, selected| {
                let Some(band) = usize::try_from(index)
                    .ok()
                    .and_then(|i| eq.bands.get_mut(i))
                else {
                    return;
                };
                band.freq = x_to_freq(x);
                band.gain = ((0.5 - y.clamp(0.0, 1.0)) * 2.0 * ui.app().get_dsp_range() * 10.0)
                    .round()
                    / 10.0;
                *selected = Some(index as usize);
            }
        ));
        bind!(on_dsp_add_band, |ui, x, y| {
            if ui.dsp.borrow_mut().current_mut().bands.len() >= MAX_BANDS {
                notice(&ui.app(), &format!("An EQ holds at most {MAX_BANDS} bands"));
                return;
            }
            ui.dsp_edit(|eq, selected| {
                // From the button: 1 kHz, flat. From a double click: where it landed.
                let (freq, gain) = if x < 0.0 {
                    (1000.0, 0.0)
                } else {
                    (
                        x_to_freq(x),
                        ((0.5 - y.clamp(0.0, 1.0)) * 2.0 * ui.app().get_dsp_range() * 10.0).round()
                            / 10.0,
                    )
                };
                eq.bands.push(Band::peak(freq, gain, 1.41));
                *selected = Some(eq.bands.len() - 1);
            });
        });
        bind!(on_dsp_remove_band, |ui| ui.dsp_edit(|eq, selected| {
            if let Some(i) = selected.filter(|i| *i < eq.bands.len()) {
                eq.bands.remove(i);
                *selected = None;
            }
        }));
        bind!(on_dsp_set_kind, |ui, kind| ui.dsp_edit_band(|band| {
            band.kind = match kind {
                1 => BandKind::LowShelf,
                2 => BandKind::HighShelf,
                _ => BandKind::Peak,
            };
        }));
        bind!(on_dsp_set_band_enabled, |ui, on| ui
            .dsp_edit_band(|band| band.enabled = on));
        bind!(on_dsp_slide, |ui, field, pos| match field.as_str() {
            "preamp" => ui.dsp_edit_live(|eq, _| {
                let db = PREAMP_MIN + (PREAMP_MAX - PREAMP_MIN) * pos.clamp(0.0, 1.0);
                eq.preamp = (db * 10.0).round() / 10.0;
            }),
            "freq" => ui.dsp_edit_live(|eq, selected| {
                if let Some(band) = selected.and_then(|i| eq.bands.get_mut(i)) {
                    band.freq = x_to_freq(pos);
                }
            }),
            "gain" => ui.dsp_edit_live(|eq, selected| {
                if let Some(band) = selected.and_then(|i| eq.bands.get_mut(i)) {
                    band.gain = y_to_gain(1.0 - pos);
                }
            }),
            "q" => ui.dsp_edit_live(|eq, selected| {
                if let Some(band) = selected.and_then(|i| eq.bands.get_mut(i)) {
                    band.q = pos_to_q(pos);
                }
            }),
            _ => {}
        });
        bind!(on_dsp_edit, |ui, field, text| {
            if field == "name" {
                ui.dsp_edit(|eq, _| eq.name = text.trim().to_string());
                return;
            }
            // A field left with something that is not a number goes back to its value.
            let Ok(value) = text.trim().replace(',', ".").parse::<f32>() else {
                ui.dsp_present();
                return;
            };
            match field.as_str() {
                "preamp" => ui.dsp_edit(|eq, _| eq.preamp = value),
                "freq" => ui.dsp_edit_band(|band| band.freq = value),
                "gain" => ui.dsp_edit_band(|band| band.gain = value),
                "q" => ui.dsp_edit_band(|band| band.q = value),
                _ => {}
            }
        });
        bind!(on_dsp_action, |ui, action| match action.as_str() {
            "import" => ui.dsp_import(),
            "export-parametric" => ui.dsp_export(false),
            "export-graphic" => ui.dsp_export(true),
            "save-preset" => ui.dsp_save_preset(),
            "reset" => ui.dsp_edit(|eq, selected| {
                eq.preamp = 0.0;
                eq.bands.clear();
                *selected = None;
            }),
            // Lower this EQ's pre-amp until the combined response peaks at 0 dB.
            "fix-headroom" => {
                let over = ui.app().get_dsp_overshoot();
                {
                    let mut dsp = ui.dsp.borrow_mut();
                    dsp.record_undo(false);
                    let device = dsp.device_mut();
                    device.preamp = Some(((device.gain() - over) * 10.0).floor() / 10.0);
                    dsp.dirty = true;
                }
                ui.dsp_apply();
                ui.dsp_present();
                ui.present_signal();
            }
            "autoeq" => ui.autoeq_open(false),
            "autoeq-refresh" => ui.autoeq_open(true),
            "autoeq-close" => ui.app().set_autoeq_open(false),
            "undo" => ui.dsp_undo(),
            "redo" => ui.dsp_redo(),
            _ => {}
        });
        bind!(on_dsp_load_preset, |ui, index| {
            let preset = usize::try_from(index)
                .ok()
                .and_then(|i| ui.dsp.borrow().store.presets.get(i).cloned());
            if let Some(preset) = preset {
                let name = preset.name.clone();
                ui.dsp_edit(|eq, selected| {
                    *eq = EqProfile {
                        enabled: eq.enabled,
                        ..preset
                    };
                    *selected = None;
                });
                notice(&ui.app(), &format!("Loaded \"{name}\""));
            }
        });
        bind!(on_dsp_delete_preset, |ui, index| {
            {
                let mut dsp = ui.dsp.borrow_mut();
                if let Some(i) = usize::try_from(index)
                    .ok()
                    .filter(|i| *i < dsp.store.presets.len())
                {
                    dsp.store.presets.remove(i);
                    dsp.dirty = true;
                }
            }
            ui.dsp_present();
        });
        bind!(on_autoeq_search, |ui, query| ui.autoeq_search(&query));
        bind!(on_autoeq_pick, |ui, index| ui
            .autoeq_pick(index.max(0) as usize));
    }

    /// Keep the EQ on screen under its name, replacing a preset of the same name.
    fn dsp_save_preset(&self) {
        let name = {
            let mut dsp = self.dsp.borrow_mut();
            let fallback = format!("{} · {}", tab_label(dsp.tab), dsp.device);
            let current = dsp.current_mut();
            if current.name.trim().is_empty() {
                current.name = fallback;
            }
            let preset = current.clone();
            let name = preset.name.clone();
            match dsp.store.presets.iter_mut().find(|p| p.name == name) {
                Some(existing) => *existing = preset,
                None => dsp.store.presets.push(preset),
            }
            dsp.store.presets.sort_by_key(|p| p.name.to_lowercase());
            dsp.dirty = true;
            name
        };
        self.dsp_present();
        notice(&self.app(), &format!("Saved preset \"{name}\""));
    }

    /// Replace the EQ on screen with one read from a parametric or GraphicEQ file.
    fn dsp_import(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .set_title("Import EQ")
                .add_filter("EQ text file", &["txt"])
                .pick_file()
                .await
            else {
                return;
            };
            let text = String::from_utf8_lossy(&file.read().await).into_owned();
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match EqProfile::parse(&text) {
                Ok(mut profile) => {
                    let file_name = file.file_name();
                    profile.name = file_name
                        .strip_suffix(".txt")
                        .unwrap_or(&file_name)
                        .to_string();
                    let summary = format!(
                        "Imported \"{}\": {} band{}",
                        profile.name,
                        profile.bands.len(),
                        if profile.bands.len() == 1 { "" } else { "s" }
                    );
                    ui.dsp_edit(|eq, selected| {
                        profile.enabled = eq.enabled;
                        *eq = profile;
                        *selected = None;
                    });
                    notice(&ui.app(), &summary);
                }
                Err(e) => tracing::error!("Could not import {}: {e}", file.file_name()),
            }
        });
        if let Err(e) = spawned {
            tracing::error!("Could not open the file dialog: {e}");
        }
    }

    /// Save the EQ on screen as Equalizer APO parametric text, or as a
    /// `GraphicEQ:` line for Wavelet.
    fn dsp_export(self: &Rc<Self>, graphic: bool) {
        let (profile, fallback) = {
            let mut dsp = self.dsp.borrow_mut();
            let fallback = format!("{} {}", dsp.device, tab_label(dsp.tab));
            (dsp.current_mut().clone(), fallback)
        };
        let base = if profile.name.trim().is_empty() {
            fallback
        } else {
            profile.name.clone()
        };
        // The name becomes a file name: no path separators.
        let base = base.replace(['/', ':', '\\'], "-");
        let (suffix, text) = if graphic {
            ("GraphicEQ", profile.to_graphic())
        } else {
            ("ParametricEQ", profile.to_parametric())
        };
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .set_title("Export EQ")
                .set_file_name(format!("{base} {suffix}.txt"))
                .add_filter("EQ text file", &["txt"])
                .save_file()
                .await
            else {
                return;
            };
            let written = file.write(text.as_bytes()).await;
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match written {
                Ok(()) => notice(&ui.app(), &format!("Exported {}", file.file_name())),
                Err(e) => tracing::error!("Could not export the EQ: {e}"),
            }
        });
        if let Err(e) = spawned {
            tracing::error!("Could not open the file dialog: {e}");
        }
    }

    // ---------------------------------------------------------- AutoEq

    /// Show the AutoEq browser. The index comes from GitHub when there is no
    /// saved copy, the copy is a day old, or `refresh` asks for it.
    fn autoeq_open(self: &Rc<Self>, refresh: bool) {
        let app = self.app();
        app.set_autoeq_open(true);
        let stale = {
            let mut dsp = self.dsp.borrow_mut();
            if dsp.autoeq.entries.is_empty() {
                dsp.autoeq = self
                    .ctx
                    .meta
                    .get_json("autoeq_index")
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default();
            }
            dsp.autoeq.entries.is_empty()
                || now_secs().saturating_sub(dsp.autoeq.fetched) > AUTOEQ_MAX_AGE_SECS
        };
        self.autoeq_search(&app.get_autoeq_query());
        if !(stale || refresh) || std::mem::replace(&mut self.dsp.borrow_mut().autoeq_busy, true) {
            return;
        }
        app.set_autoeq_status("Getting the latest AutoEq index from GitHub…".into());
        self.spawn(|ctx| async move {
            let fetched = fetch_text(&format!("{AUTOEQ_RESULTS}/INDEX.md"))
                .await
                .map(|markdown| parse_autoeq_index(&markdown));
            let fetched = match fetched {
                Ok(entries) if entries.is_empty() => Err("the index was empty".to_string()),
                other => other,
            };
            let index = fetched.map(|entries| AutoEqIndex {
                fetched: now_secs(),
                entries,
            });
            if let Ok(index) = &index
                && let Ok(json) = serde_json::to_string(index)
                && let Err(e) = ctx.meta.put_json("autoeq_index", &json)
            {
                tracing::warn!("could not save the AutoEq index: {e}");
            }
            on_ui(move |ui| {
                ui.dsp.borrow_mut().autoeq_busy = false;
                match index {
                    Ok(index) => ui.dsp.borrow_mut().autoeq = index,
                    Err(e) => tracing::error!("Could not get the AutoEq index: {e}"),
                }
                ui.autoeq_search(&ui.app().get_autoeq_query());
            });
        });
    }

    fn autoeq_search(&self, query: &str) {
        let app = self.app();
        let mut dsp = self.dsp.borrow_mut();
        dsp.hits = autoeq_matches(&dsp.autoeq.entries, query);
        let rows: Vec<EqHit> = dsp
            .hits
            .iter()
            .map(|i| &dsp.autoeq.entries[*i])
            .map(|e| EqHit {
                name: e.name.as_str().into(),
                source: e.source.as_str().into(),
            })
            .collect();
        let total = dsp.autoeq.entries.len();
        app.set_autoeq_status(
            if total == 0 {
                if dsp.autoeq_busy {
                    "Getting the latest AutoEq index from GitHub…".to_string()
                } else {
                    "The AutoEq index could not be loaded. Check the connection and refresh."
                        .to_string()
                }
            } else if rows.is_empty() {
                format!("Nothing matches in {total} profiles.")
            } else if rows.len() == AUTOEQ_SHOWN {
                format!("{total} profiles. Showing the first {AUTOEQ_SHOWN}: type to narrow down.")
            } else {
                format!("{} of {total} profiles", rows.len())
            }
            .into(),
        );
        app.set_autoeq_hits(ModelRc::new(VecModel::from(rows)));
    }

    /// Download a headphone's parametric EQ and make it the SPK EQ.
    fn autoeq_pick(self: &Rc<Self>, row: usize) {
        let entry = {
            let dsp = self.dsp.borrow();
            dsp.hits
                .get(row)
                .and_then(|i| dsp.autoeq.entries.get(*i))
                .cloned()
        };
        let Some(entry) = entry else {
            return;
        };
        self.app()
            .set_autoeq_status(format!("Getting {}…", entry.name).into());
        self.spawn(|_| async move {
            let parsed = fetch_text(&autoeq_file_url(&entry))
                .await
                .and_then(|text| EqProfile::parse(&text));
            on_ui(move |ui| match parsed {
                Ok(mut profile) => {
                    profile.name = format!("{} ({})", entry.name, entry.source);
                    let name = profile.name.clone();
                    ui.dsp.borrow_mut().tab = 1;
                    ui.dsp_edit(|eq, selected| {
                        profile.enabled = eq.enabled;
                        *eq = profile;
                        *selected = None;
                    });
                    ui.autoeq_search(&ui.app().get_autoeq_query());
                    notice(&ui.app(), &format!("SPK EQ: {name}"));
                }
                Err(e) => {
                    tracing::error!("Could not get the AutoEq profile for {}: {e}", entry.name);
                    ui.autoeq_search(&ui.app().get_autoeq_query());
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_autoeq_index_and_builds_file_addresses() {
        let entries = parse_autoeq_index(
            "# Index\n\n- [1MORE Aero (ANC Off)](./HypetheSonics/GRAS%20RA0045%20in-ear/1MORE%20Aero%20(ANC%20Off)) by HypetheSonics on GRAS RA0045\n- [Sennheiser HD 660S2](./oratory1990/over-ear/Sennheiser%20HD%20660S2) by oratory1990\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "1MORE Aero (ANC Off)");
        assert_eq!(entries[0].source, "HypetheSonics on GRAS RA0045");
        assert_eq!(
            autoeq_file_url(&entries[1]),
            "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/oratory1990/over-ear/Sennheiser%20HD%20660S2/Sennheiser%20HD%20660S2%20ParametricEQ.txt"
        );
        assert_eq!(autoeq_matches(&entries, "660s2 oratory"), vec![1]);
        assert_eq!(autoeq_matches(&entries, ""), vec![0, 1]);
        assert!(autoeq_matches(&entries, "focal").is_empty());
    }

    #[test]
    fn graph_axes_round_trip() {
        assert_eq!(x_to_freq(freq_to_x(1000.0)), 1000.0);
        assert_eq!(x_to_freq(0.0), 20.0);
        assert_eq!(x_to_freq(1.0), 20_000.0);
        assert_eq!(y_to_gain(gain_to_y(-3.5)), -3.5);
        assert_eq!(y_to_gain(0.5), 0.0);
        assert!((pos_to_q(q_to_pos(2.0)) - 2.0).abs() < 0.01);
        assert!(curve_path(-15.0, 15.0, |_| 0.0).starts_with("M0.0 500.0 L"));
    }

    #[test]
    fn shared_preamp_migrates_and_auto_tracks_enabled_eqs() {
        let mut device: DeviceDsp = serde_json::from_str(
            r#"{"usr":{"enabled":true,"preamp":-5.0},"spk":{"enabled":true,"preamp":-7.0}}"#,
        )
        .unwrap();
        device.migrate_preamp();
        assert_eq!(device.gain(), -12.0);
        assert_eq!(device.usr.preamp, 0.0);
        assert_eq!(device.spk.preamp, 0.0);
        device.usr.bands.push(Band::peak(1000.0, 6.0, 1.0));
        device.auto_preamp = true;
        device.update_auto_preamp();
        assert!(device.gain() <= -6.0);
        device.usr.enabled = false;
        device.update_auto_preamp();
        assert_eq!(device.gain(), 0.0);
        let saved = serde_json::to_string(&device).unwrap();
        let restored: DeviceDsp = serde_json::from_str(&saved).unwrap();
        assert!(restored.auto_preamp);
    }

    #[test]
    fn master_bypass_preserves_profiles_and_bypasses_preamp() {
        let mut device = DeviceDsp::default();
        device.usr.bands.push(Band::peak(1000.0, 6.0, 1.0));
        device.spk.enabled = false;
        device.preamp = Some(-5.0);
        device.enabled = false;
        assert!(
            device
                .playback_profiles()
                .iter()
                .all(|profile| !profile.alters_audio())
        );
        let saved = serde_json::to_string(&device).unwrap();
        let mut restored: DeviceDsp = serde_json::from_str(&saved).unwrap();
        assert!(!restored.enabled);
        restored.enabled = true;
        let profiles = restored.playback_profiles();
        assert!(!profiles[0].enabled);
        assert!(profiles[1].alters_audio());
        assert!(profiles[2].alters_audio());
        assert!(serde_json::from_str::<DeviceDsp>("{}").unwrap().enabled);
    }

    #[test]
    fn a_new_output_starts_flat_and_bit_perfect() {
        let device = DeviceDsp::default();
        assert!(device.usr.enabled && device.spk.enabled);
        assert!(!device.usr.alters_audio() && !device.spk.alters_audio());
    }

    #[test]
    fn undo_restores_the_eq_and_redo_puts_it_back() {
        let mut dsp = DspState {
            device: "DAC".into(),
            ..DspState::default()
        };
        dsp.record_undo(false);
        dsp.current_mut().preamp = -3.0;
        dsp.record_undo(false);
        dsp.current_mut().preamp = -6.0;
        assert!(dsp.apply_undo());
        assert_eq!(dsp.current_mut().preamp, -3.0);
        assert!(dsp.apply_redo());
        assert_eq!(dsp.current_mut().preamp, -6.0);
        assert!(!dsp.apply_redo());
    }

    #[test]
    fn live_edits_share_one_undo_step() {
        let mut dsp = DspState {
            device: "DAC".into(),
            ..DspState::default()
        };
        dsp.record_undo(true);
        dsp.current_mut().preamp = -1.0;
        dsp.record_undo(true);
        dsp.current_mut().preamp = -2.0;
        dsp.record_undo(true);
        dsp.current_mut().preamp = -3.0;
        assert_eq!(dsp.undo.len(), 1);
        assert!(dsp.apply_undo());
        assert_eq!(dsp.current_mut().preamp, 0.0);
    }
}
