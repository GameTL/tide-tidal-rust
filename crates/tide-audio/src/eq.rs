//! Parametric equaliser: RBJ biquads, a pre-amp, and the two text formats
//! AutoEq and squig.link export (Equalizer APO parametric and `GraphicEQ:`).
//!
//! A `GraphicEQ:` curve is not played back as-is: it is fitted with peaking
//! bands on import, so every profile is a list of bands that can be edited.

use serde::{Deserialize, Serialize};

/// Bands in one profile.
pub const MAX_BANDS: usize = 20;
/// Rate the response curves and the GraphicEQ export are computed at.
const DISPLAY_RATE: f64 = 48_000.0;
pub const MIN_FREQ: f32 = 20.0;
pub const MAX_FREQ: f32 = 20_000.0;
pub const MAX_GAIN: f32 = 24.0;
pub const MIN_Q: f32 = 0.1;
pub const MAX_Q: f32 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BandKind {
    Peak,
    LowShelf,
    HighShelf,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub kind: BandKind,
    /// Centre (or corner) frequency in Hz.
    pub freq: f32,
    /// Gain in dB.
    pub gain: f32,
    pub q: f32,
    pub enabled: bool,
}

impl Band {
    pub fn peak(freq: f32, gain: f32, q: f32) -> Self {
        Self {
            kind: BandKind::Peak,
            freq,
            gain,
            q,
            enabled: true,
        }
    }

    /// Bring every field into the range the filters are stable in.
    pub fn clamped(mut self) -> Self {
        self.freq = self.freq.clamp(MIN_FREQ, MAX_FREQ);
        self.gain = self.gain.clamp(-MAX_GAIN, MAX_GAIN);
        self.q = self.q.clamp(MIN_Q, MAX_Q);
        self
    }

    /// Normalised biquad `[b0, b1, b2, a1, a2]` (Audio EQ Cookbook).
    fn coefficients(&self, rate: f64) -> [f64; 5] {
        let freq = (self.freq as f64).clamp(1.0, rate * 0.49);
        let a = 10f64.powf(self.gain as f64 / 40.0);
        let w0 = std::f64::consts::TAU * freq / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * (self.q as f64).max(0.01));
        let (b0, b1, b2, a0, a1, a2) = match self.kind {
            BandKind::Peak => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            BandKind::LowShelf => {
                let k = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - k),
                    (a + 1.0) + (a - 1.0) * cos + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - k,
                )
            }
            BandKind::HighShelf => {
                let k = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + k),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - k),
                    (a + 1.0) - (a - 1.0) * cos + k,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - k,
                )
            }
        };
        [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
    }

    /// Gain of this band alone at `freq`, in dB.
    pub fn response_db(&self, freq: f32) -> f32 {
        magnitude_db(&self.coefficients(DISPLAY_RATE), freq as f64) as f32
    }
}

fn magnitude_db(c: &[f64; 5], freq: f64) -> f64 {
    let w = std::f64::consts::TAU * freq / DISPLAY_RATE;
    let (c1, c2) = (w.cos(), (2.0 * w).cos());
    let [b0, b1, b2, a1, a2] = *c;
    let num = b0 * b0 + b1 * b1 + b2 * b2 + 2.0 * (b0 * b1 + b1 * b2) * c1 + 2.0 * b0 * b2 * c2;
    let den = 1.0 + a1 * a1 + a2 * a2 + 2.0 * (a1 + a1 * a2) * c1 + 2.0 * a2 * c2;
    10.0 * (num / den).max(1e-12).log10()
}

/// One equaliser: a pre-amp and its bands.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EqProfile {
    pub name: String,
    pub enabled: bool,
    /// Gain applied before the bands, in dB. Negative to leave headroom.
    pub preamp: f32,
    pub bands: Vec<Band>,
}

impl EqProfile {
    /// True when switching this profile on would change the samples.
    pub fn alters_audio(&self) -> bool {
        self.enabled
            && (self.preamp.abs() > 0.005
                || self.bands.iter().any(|b| b.enabled && b.gain.abs() > 0.005))
    }

    /// Gain of the bands at `freq`, in dB. The pre-amp is not included.
    pub fn response_db(&self, freq: f32) -> f32 {
        self.bands
            .iter()
            .filter(|b| b.enabled)
            .map(|b| b.response_db(freq))
            .sum()
    }

    /// Highest gain of bands plus pre-amp across the audible range. Above 0 dB
    /// a full-scale signal can clip.
    pub fn peak_db(&self) -> f32 {
        log_frequencies(256)
            .map(|f| self.response_db(f))
            .fold(f32::MIN, f32::max)
            + self.preamp
    }

    /// Equalizer APO parametric text, as exported by AutoEq and squig.link.
    pub fn to_parametric(&self) -> String {
        let mut out = format!("Preamp: {:.1} dB\n", self.preamp);
        for (i, band) in self.bands.iter().enumerate() {
            let kind = match band.kind {
                BandKind::Peak => "PK",
                BandKind::LowShelf => "LSC",
                BandKind::HighShelf => "HSC",
            };
            let freq = if band.freq.fract().abs() < 0.05 {
                format!("{:.0}", band.freq)
            } else {
                format!("{:.1}", band.freq)
            };
            out.push_str(&format!(
                "Filter {}: {} {kind} Fc {freq} Hz Gain {:.1} dB Q {:.3}\n",
                i + 1,
                if band.enabled { "ON" } else { "OFF" },
                band.gain,
                band.q
            ));
        }
        out
    }

    /// `GraphicEQ:` line on Wavelet's 127 frequencies. The pre-amp is part of
    /// the curve, as in AutoEq's own files.
    pub fn to_graphic(&self) -> String {
        let points: Vec<String> = graphic_frequencies()
            .map(|f| format!("{f} {:.1}", self.response_db(f as f32) + self.preamp))
            .collect();
        format!("GraphicEQ: {}\n", points.join("; "))
    }

    /// Read either text format. `Err` says what was wrong with the file.
    pub fn parse(text: &str) -> Result<Self, String> {
        if let Some(line) = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("GraphicEQ:"))
        {
            let points: Vec<(f32, f32)> = line
                .split(';')
                .filter_map(|pair| {
                    let mut it = pair.split_whitespace();
                    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
                })
                .filter(|(f, g): &(f32, f32)| f.is_finite() && g.is_finite() && *f > 0.0)
                .collect();
            if points.len() < 2 {
                return Err("The GraphicEQ line has no frequency / gain pairs.".into());
            }
            return Ok(fit_graphic(&points));
        }

        let mut profile = EqProfile {
            enabled: true,
            ..Default::default()
        };
        let mut seen = false;
        let mut skipped = 0;
        for line in text.lines() {
            let line = line.trim();
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("preamp:") {
                if let Some(db) = rest.split_whitespace().next().and_then(|t| t.parse().ok()) {
                    profile.preamp = db;
                    seen = true;
                }
                continue;
            }
            if !lower.starts_with("filter") {
                continue;
            }
            let Some((_, spec)) = line.split_once(':') else {
                continue;
            };
            let tokens: Vec<&str> = spec.split_whitespace().collect();
            let after = |key: &str| -> Option<f32> {
                let at = tokens.iter().position(|t| t.eq_ignore_ascii_case(key))?;
                tokens.get(at + 1)?.parse().ok()
            };
            let kind = match tokens.get(1).map(|t| t.to_ascii_uppercase()).as_deref() {
                Some("PK" | "PEQ" | "PEAK") => BandKind::Peak,
                Some("LS" | "LSC" | "LSQ") => BandKind::LowShelf,
                Some("HS" | "HSC" | "HSQ") => BandKind::HighShelf,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            seen = true;
            let (Some(freq), Some(gain)) = (after("Fc"), after("Gain")) else {
                continue;
            };
            // Unused slots are written as "PK Fc 0 Hz Gain 0.0 dB Q 0.000".
            if freq <= 0.0 || profile.bands.len() >= MAX_BANDS {
                continue;
            }
            let q = after("Q").filter(|q| *q > 0.0).unwrap_or(0.707);
            profile.bands.push(
                Band {
                    kind,
                    freq,
                    gain,
                    q,
                    enabled: tokens.first().is_some_and(|t| t.eq_ignore_ascii_case("ON")),
                }
                .clamped(),
            );
        }
        if !seen {
            return Err(if skipped > 0 {
                "This file only has filter types TIDE does not support (peak, low shelf and high shelf are).".into()
            } else {
                "Not an EQ file: expected \"Preamp:\" / \"Filter:\" lines or a \"GraphicEQ:\" line."
                    .into()
            });
        }
        Ok(profile)
    }
}

/// `count` frequencies from 20 Hz to 20 kHz, evenly spaced on a log axis.
pub fn log_frequencies(count: usize) -> impl Iterator<Item = f32> {
    let last = count.max(2) - 1;
    (0..=last).map(move |i| MIN_FREQ * (MAX_FREQ / MIN_FREQ).powf(i as f32 / last as f32))
}

/// The 127 frequencies of a Wavelet / AutoEq GraphicEQ line (20 Hz, then ×1.0563).
fn graphic_frequencies() -> impl Iterator<Item = u32> {
    (0..127).map(|i| (20.0 * 1.0563f64.powi(i)) as u32)
}

/// Approximate a graphic-EQ curve with a pre-amp and peaking bands.
fn fit_graphic(points: &[(f32, f32)]) -> EqProfile {
    const FIT_BANDS: usize = 12;
    let mut points: Vec<(f32, f32)> = points
        .iter()
        .copied()
        .filter(|(f, _)| (MIN_FREQ..=MAX_FREQ).contains(f))
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Resample onto an even log grid so no part of the range outweighs another.
    let at = |freq: f32| -> f32 {
        let i = points.partition_point(|(f, _)| *f < freq);
        if i == 0 {
            return points[0].1;
        }
        if i == points.len() {
            return points[i - 1].1;
        }
        let ((f0, g0), (f1, g1)) = (points[i - 1], points[i]);
        g0 + (g1 - g0) * ((freq / f0).ln() / (f1 / f0).ln().max(1e-9))
    };
    let mut profile = EqProfile {
        enabled: true,
        ..Default::default()
    };
    if points.is_empty() {
        return profile;
    }
    let grid: Vec<f32> = log_frequencies(160).collect();
    let target: Vec<f32> = grid.iter().map(|f| at(*f)).collect();
    // Bands cannot make a level offset; that part becomes the pre-amp.
    let mut sorted = target.clone();
    sorted.sort_by(f32::total_cmp);
    let offset = sorted[sorted.len() / 2];

    let model = |bands: &[Band]| -> Vec<f32> {
        grid.iter()
            .map(|f| bands.iter().map(|b| b.response_db(*f)).sum::<f32>())
            .collect()
    };
    let error = |bands: &[Band]| -> f32 {
        model(bands)
            .iter()
            .zip(&target)
            .map(|(m, t)| (t - offset - m).powi(2))
            .sum()
    };
    // Nudge one parameter at a time while it lowers the squared error.
    let refine = |bands: &mut Vec<Band>, rounds: usize| {
        let mut best = error(bands);
        let mut step = 1.0f32;
        for _ in 0..rounds {
            for i in 0..bands.len() {
                for param in 0..3 {
                    for dir in [-1.0f32, 1.0] {
                        let mut trial = bands.clone();
                        let b = &mut trial[i];
                        match param {
                            0 => b.gain += dir * 0.5 * step,
                            1 => b.freq *= 1.0 + dir * 0.06 * step,
                            _ => b.q *= 1.0 + dir * 0.15 * step,
                        }
                        *b = b.clamped();
                        let e = error(&trial);
                        if e < best {
                            best = e;
                            *bands = trial;
                        }
                    }
                }
            }
            step *= 0.8;
        }
    };

    let mut bands: Vec<Band> = Vec::new();
    while bands.len() < FIT_BANDS {
        let fitted = model(&bands);
        let residual: Vec<f32> = target
            .iter()
            .zip(&fitted)
            .map(|(t, m)| t - offset - m)
            .collect();
        let (peak, gain) = residual
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .expect("grid is not empty");
        if gain.abs() < 0.15 {
            break;
        }
        // Width where the residual stays above half the peak gives the Q.
        let above = |i: usize| residual[i] * gain.signum() > gain.abs() * 0.5;
        let mut lo = peak;
        while lo > 0 && above(lo - 1) {
            lo -= 1;
        }
        let mut hi = peak;
        while hi + 1 < grid.len() && above(hi + 1) {
            hi += 1;
        }
        let octaves = (grid[hi] / grid[lo]).log2().max(0.12);
        let ratio = 2f32.powf(octaves);
        bands.push(Band::peak(grid[peak], gain, ratio.sqrt() / (ratio - 1.0)).clamped());
        refine(&mut bands, 6);
    }
    refine(&mut bands, 40);
    bands.retain(|b| b.gain.abs() >= 0.1);
    bands.sort_by(|a, b| a.freq.total_cmp(&b.freq));
    for band in &mut bands {
        band.freq = if band.freq < 1000.0 {
            band.freq.round()
        } else {
            (band.freq / 10.0).round() * 10.0
        };
        band.gain = (band.gain * 10.0).round() / 10.0;
        band.q = (band.q * 100.0).round() / 100.0;
    }
    profile.preamp = (offset * 10.0).round() / 10.0;
    profile.bands = bands;
    profile
}

/// Most biquads the output can run: two full profiles.
const MAX_STAGES: usize = MAX_BANDS * 2;

#[derive(Clone, Copy)]
struct Stage {
    c: [f64; 5],
    /// Filter memory per channel.
    z: [[f64; 2]; 2],
}

/// The filters as they run in the output callback. Fixed size: configuring it
/// never allocates.
pub struct EqChain {
    stages: [Stage; MAX_STAGES],
    len: usize,
    gain: f64,
    active: bool,
}

impl Default for EqChain {
    fn default() -> Self {
        Self {
            stages: [Stage {
                c: [1.0, 0.0, 0.0, 0.0, 0.0],
                z: [[0.0; 2]; 2],
            }; MAX_STAGES],
            len: 0,
            gain: 1.0,
            active: false,
        }
    }
}

impl EqChain {
    /// Load the enabled profiles for a device running at `rate`. Filter memory
    /// is kept, so changing a band while music plays does not click.
    pub fn configure(&mut self, profiles: &[EqProfile], rate: u32) {
        let mut len = 0;
        let mut preamp = 0.0f64;
        for profile in profiles.iter().filter(|p| p.alters_audio()) {
            preamp += profile.preamp as f64;
            for band in profile
                .bands
                .iter()
                .filter(|b| b.enabled && b.gain.abs() > 0.005)
            {
                if len == MAX_STAGES {
                    break;
                }
                self.stages[len].c = band.coefficients(rate.max(8_000) as f64);
                len += 1;
            }
        }
        for stage in &mut self.stages[len..] {
            stage.z = [[0.0; 2]; 2];
        }
        self.len = len;
        self.gain = 10f64.powf(preamp / 20.0);
        self.active = len > 0 || preamp.abs() > 0.005;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    #[inline]
    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        let mut out = [0.0f32; 2];
        for (ch, sample) in frame.into_iter().enumerate() {
            let mut x = sample as f64 * self.gain;
            for stage in &mut self.stages[..self.len] {
                let [b0, b1, b2, a1, a2] = stage.c;
                let z = &mut stage.z[ch];
                let y = b0 * x + z[0];
                z[0] = b1 * x - a1 * y + z[1];
                z[1] = b2 * x - a2 * y;
                x = y;
            }
            out[ch] = x as f32;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d3v_profile_preserves_narrow_bands() {
        let profile = EqProfile::parse(include_str!(
            "../../../profiles/ADAM Audio D3V Calibrated.txt"
        ))
        .unwrap();
        assert_eq!(profile.bands.len(), 16);
        assert_eq!(profile.preamp, -10.0);
        assert!((profile.bands[5].q - 14.424).abs() < 0.001);
        assert_eq!(profile.bands[15].kind, BandKind::HighShelf);
    }

    /// squig.link "Export Parametric EQ", as saved by the browser.
    const PARAMETRIC: &str = "Preamp: -3.5 dB
Filter 1: OFF PK Fc 23 Hz Gain -0.5 dB Q 0.600
Filter 2: ON PK Fc 64 Hz Gain -1.5 dB Q 2.000
Filter 3: ON PK Fc 180 Hz Gain 0.5 dB Q 1.100
Filter 4: ON PK Fc 390 Hz Gain -3.1 dB Q 1.500
Filter 5: ON PK Fc 810 Hz Gain 3.9 dB Q 2.000
Filter 6: ON PK Fc 1500 Hz Gain 1.3 dB Q 2.000
Filter 7: ON PK Fc 2600 Hz Gain -3.5 dB Q 2.000
Filter 8: ON PK Fc 6100 Hz Gain 3.9 dB Q 2.000
Filter 9: ON PK Fc 8000 Hz Gain -4.0 dB Q 2.000
Filter 10: OFF PK Fc 0 Hz Gain 0.0 dB Q 0.000
";

    /// The same EQ from squig.link "Export Graphic EQ (for Wavelet)".
    const GRAPHIC: &str = "GraphicEQ: 20 -3.6; 21 -3.6; 22 -3.6; 23 -3.6; 24 -3.6; 26 -3.6; 27 -3.6; 29 -3.6; 30 -3.6; 32 -3.7; 34 -3.7; 36 -3.7; 38 -3.8; 40 -3.8; 43 -3.9; 45 -4.0; 48 -4.1; 50 -4.3; 53 -4.4; 56 -4.7; 59 -4.9; 63 -5.0; 66 -4.9; 70 -4.8; 74 -4.6; 78 -4.4; 83 -4.2; 87 -4.0; 92 -3.9; 97 -3.8; 103 -3.7; 109 -3.7; 115 -3.6; 121 -3.5; 128 -3.5; 136 -3.5; 143 -3.4; 151 -3.4; 160 -3.4; 169 -3.4; 178 -3.4; 188 -3.5; 199 -3.6; 210 -3.7; 222 -3.8; 235 -4.0; 248 -4.1; 262 -4.4; 277 -4.6; 292 -4.9; 309 -5.2; 326 -5.5; 345 -5.8; 364 -6.1; 385 -6.1; 406 -6.1; 429 -5.8; 453 -5.4; 479 -5.0; 506 -4.5; 534 -4.1; 565 -3.6; 596 -3.0; 630 -2.5; 665 -1.9; 703 -1.2; 743 -0.6; 784 -0.1; 829 0.0; 875 -0.2; 924 -0.7; 977 -1.1; 1032 -1.5; 1090 -1.8; 1151 -2.0; 1216 -2.1; 1284 -2.2; 1357 -2.2; 1433 -2.2; 1514 -2.4; 1599 -2.6; 1689 -3.0; 1784 -3.4; 1885 -3.8; 1991 -4.3; 2103 -4.9; 2221 -5.4; 2347 -6.0; 2479 -6.4; 2618 -6.6; 2766 -6.4; 2921 -6.0; 3086 -5.5; 3260 -5.0; 3443 -4.6; 3637 -4.2; 3842 -3.9; 4058 -3.6; 4287 -3.3; 4528 -3.0; 4783 -2.6; 5052 -2.2; 5337 -1.8; 5637 -1.4; 5955 -1.2; 6290 -1.6; 6644 -2.6; 7018 -3.8; 7414 -5.0; 7831 -5.9; 8272 -6.2; 8738 -5.9; 9230 -5.4; 9749 -5.0; 10298 -4.5; 10878 -4.3; 11490 -4.1; 12137 -3.9; 12821 -3.8; 13543 -3.7; 14305 -3.7; 15110 -3.6; 15961 -3.6; 16860 -3.6; 17809 -3.6; 18812 -3.5; 19871 -3.5";

    fn graphic_points(text: &str) -> Vec<(f32, f32)> {
        text.trim()
            .trim_start_matches("GraphicEQ:")
            .split(';')
            .map(|pair| {
                let mut it = pair.split_whitespace();
                (
                    it.next().unwrap().parse().unwrap(),
                    it.next().unwrap().parse().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn reads_a_squig_parametric_export() {
        let eq = EqProfile::parse(PARAMETRIC).unwrap();
        assert_eq!(eq.preamp, -3.5);
        // The unused "Fc 0" slot is dropped; the OFF band is kept, switched off.
        assert_eq!(eq.bands.len(), 9);
        assert!(!eq.bands[0].enabled);
        assert_eq!(eq.bands[4], Band::peak(810.0, 3.9, 2.0));
        // A peaking band has its full gain at its centre.
        assert!((Band::peak(810.0, 3.9, 2.0).response_db(810.0) - 3.9).abs() < 0.01);
    }

    #[test]
    fn parametric_text_round_trips() {
        let eq = EqProfile::parse(PARAMETRIC).unwrap();
        let again = EqProfile::parse(&eq.to_parametric()).unwrap();
        assert_eq!(eq, again);
        assert!(
            eq.to_parametric()
                .contains("Filter 5: ON PK Fc 810 Hz Gain 3.9 dB Q 2.000")
        );
    }

    #[test]
    fn reads_autoeq_shelves() {
        let eq = EqProfile::parse(
            "Preamp: -6.4 dB\nFilter 1: ON LSC Fc 105 Hz Gain 7.6 dB Q 0.70\nFilter 2: ON HSC Fc 10000 Hz Gain -3.8 dB Q 0.70\n",
        )
        .unwrap();
        assert_eq!(eq.bands[0].kind, BandKind::LowShelf);
        assert_eq!(eq.bands[1].kind, BandKind::HighShelf);
        // A shelf reaches its gain well past the corner and leaves the other end alone.
        assert!((eq.bands[0].response_db(20.0) - 7.6).abs() < 0.3);
        assert!(eq.bands[0].response_db(5000.0).abs() < 0.05);
        assert!((eq.bands[1].response_db(19_000.0) + 3.8).abs() < 0.4);
        assert!(eq.to_parametric().contains("LSC Fc 105 Hz"));
    }

    #[test]
    fn graphic_export_matches_the_squig_wavelet_file() {
        let eq = EqProfile::parse(PARAMETRIC).unwrap();
        let ours = graphic_points(&eq.to_graphic());
        let theirs = graphic_points(GRAPHIC);
        assert_eq!(ours.len(), 127);
        for (a, b) in ours.iter().zip(&theirs) {
            assert_eq!(a.0, b.0, "frequency grid differs");
            assert!((a.1 - b.1).abs() <= 0.25, "{} Hz: {} vs {}", a.0, a.1, b.1);
        }
    }

    #[test]
    fn graphic_import_is_fitted_with_bands() {
        let eq = EqProfile::parse(GRAPHIC).unwrap();
        assert!(eq.enabled && !eq.bands.is_empty() && eq.bands.len() <= 12);
        let worst = graphic_points(GRAPHIC)
            .iter()
            .map(|(f, g)| (eq.response_db(*f) + eq.preamp - g).abs())
            .fold(0.0, f32::max);
        assert!(worst < 0.5, "fit is off by {worst} dB: {eq:?}");
        // The curve never goes above 0 dB, so neither may the fit by much.
        assert!(eq.peak_db() < 0.5);
    }

    #[test]
    fn rejects_text_that_is_not_an_eq() {
        assert!(EqProfile::parse("hello").is_err());
        assert!(EqProfile::parse("GraphicEQ: ").is_err());
    }

    #[test]
    fn chain_applies_the_gain_of_its_bands() {
        let eq = EqProfile {
            name: String::new(),
            enabled: true,
            preamp: -2.0,
            bands: vec![Band::peak(1000.0, 6.0, 1.0)],
        };
        let mut chain = EqChain::default();
        chain.configure(std::slice::from_ref(&eq), 48_000);
        assert!(chain.is_active());
        let rms = |chain: &mut EqChain, freq: f32| -> f32 {
            let mut sum = 0.0;
            for i in 0..48_000 {
                let x = (i as f32 * freq * std::f32::consts::TAU / 48_000.0).sin();
                let y = chain.process([x, x]);
                assert_eq!(y[0], y[1]);
                if i >= 24_000 {
                    sum += y[0] * y[0];
                }
            }
            10.0 * (sum / 24_000.0 * 2.0).log10()
        };
        assert!((rms(&mut chain, 1000.0) - 4.0).abs() < 0.05);
        assert!((rms(&mut chain, 60.0) + 2.0).abs() < 0.1);

        // Switched off, or flat: the samples pass through untouched.
        chain.configure(
            &[EqProfile {
                enabled: false,
                ..eq.clone()
            }],
            48_000,
        );
        assert!(!chain.is_active());
        assert_eq!(chain.process([0.25, -0.5]), [0.25, -0.5]);
    }
}
