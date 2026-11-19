//! Integer upsampler: a polyphase windowed-sinc interpolator.
//!
//! The filter is a Nyquist (L-th band) design, so every original sample comes
//! out unchanged and only the samples in between are computed. It is linear
//! phase; the pass band runs to about 0.455 × the source rate (20 kHz for a
//! 44.1 kHz track) and images are down by about 90 dB from 0.545 × onwards.

/// Source frames each output frame is computed from.
const TAPS: usize = 64;
/// Kaiser window shape: about 90 dB of image rejection.
const KAISER_BETA: f64 = 9.0;

/// Multiples the upsampler handles. Every standard rate is ×2, ×4 or ×8 of a
/// lower one in its family.
pub const FACTORS: [u32; 3] = [2, 4, 8];

/// `Some(L)` when `to` is `from` × 2, 4 or 8.
pub fn factor(from: u32, to: u32) -> Option<u32> {
    if from == 0 || to <= from || !to.is_multiple_of(from) {
        return None;
    }
    Some(to / from).filter(|l| FACTORS.contains(l))
}

/// Rate to upsample `source` to: the highest of `source` × 2, 4 or 8 the
/// device supports that stays within `tier` × the family's base rate
/// (44.1 or 48 kHz). `None` when there is nothing to do: upsampling is off,
/// the track is already at or above the target, or the device has no such rate.
pub fn target_rate(source: u32, tier: u32, supported: &[u32]) -> Option<u32> {
    let base = [44_100, 48_000]
        .into_iter()
        .find(|base| source != 0 && source.is_multiple_of(*base))?;
    let ceiling = base.checked_mul(tier)?;
    FACTORS
        .iter()
        .rev()
        .filter_map(|l| source.checked_mul(*l))
        .find(|rate| *rate <= ceiling && supported.contains(rate))
}

/// Zeroth-order modified Bessel function of the first kind.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..64 {
        term *= half / k as f64;
        let add = term * term;
        sum += add;
        if add < sum * 1e-18 {
            break;
        }
    }
    sum
}

pub struct Upsampler {
    factor: usize,
    /// `factor` rows of `TAPS` coefficients, each in oldest-to-newest order.
    taps: Vec<f32>,
    /// The last `TAPS` source frames, written twice so they always read as
    /// one contiguous run.
    history: [[f32; 2]; TAPS * 2],
    at: usize,
    /// Output frames still to drop: the filter's delay, so the first frame
    /// out lines up with the first frame in.
    skip: usize,
}

impl Upsampler {
    pub fn new(factor: u32) -> Self {
        let l = factor as usize;
        let centre = (TAPS / 2 * l) as f64;
        let mut taps = vec![0.0f32; TAPS * l];
        for phase in 0..l {
            // Tap `k` weighs the source frame `k` back from the newest one.
            let mut row = [0.0f64; TAPS];
            for (k, tap) in row.iter_mut().enumerate() {
                let d = (phase + l * k) as f64 - centre;
                let x = d / l as f64;
                // Exact at the zero crossings, so the original samples pass untouched.
                let sinc = if d == 0.0 {
                    1.0
                } else if (phase + l * k).is_multiple_of(l) {
                    0.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let edge = (d / centre).clamp(-1.0, 1.0);
                let window =
                    bessel_i0(KAISER_BETA * (1.0 - edge * edge).sqrt()) / bessel_i0(KAISER_BETA);
                *tap = sinc * window;
            }
            // Unity gain at DC for every phase, so a steady level stays steady.
            let sum: f64 = row.iter().sum();
            for (k, tap) in row.iter().enumerate() {
                taps[phase * TAPS + (TAPS - 1 - k)] = (tap / sum) as f32;
            }
        }
        Self {
            factor: l,
            taps,
            history: [[0.0; 2]; TAPS * 2],
            at: 0,
            skip: TAPS / 2 * l,
        }
    }

    pub fn factor(&self) -> u32 {
        self.factor as u32
    }

    /// Forget what was played: the next frame starts a new stretch of audio.
    pub fn reset(&mut self) {
        self.history = [[0.0; 2]; TAPS * 2];
        self.at = 0;
        self.skip = TAPS / 2 * self.factor;
    }

    /// Take one source frame, append `factor` interleaved stereo frames to `out`.
    pub fn push(&mut self, frame: [f32; 2], out: &mut Vec<f32>) {
        self.history[self.at] = frame;
        self.history[self.at + TAPS] = frame;
        self.at = (self.at + 1) % TAPS;
        if self.skip >= self.factor {
            // Still inside the filter's delay: nothing of the track is due yet.
            self.skip -= self.factor;
            return;
        }
        let window = &self.history[self.at..self.at + TAPS];
        for row in self.taps.as_chunks::<TAPS>().0 {
            let mut l = 0.0f32;
            let mut r = 0.0f32;
            for (tap, frame) in row.iter().zip(window) {
                l += tap * frame[0];
                r += tap * frame[1];
            }
            out.push(l);
            out.push(r);
        }
    }

    /// The track is over: emit the frames still inside the filter.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        for _ in 0..TAPS / 2 {
            self.push([0.0; 2], out);
        }
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, i: usize) -> f32 {
        (i as f64 * freq * std::f64::consts::TAU / rate).sin() as f32 * 0.5
    }

    fn upsample(input: &[f32], factor: u32) -> Vec<f32> {
        let mut up = Upsampler::new(factor);
        let mut out = Vec::new();
        for &s in input {
            up.push([s, -s], &mut out);
        }
        up.flush(&mut out);
        out
    }

    #[test]
    fn factors_and_targets_follow_the_rate_family() {
        assert_eq!(factor(44_100, 88_200), Some(2));
        assert_eq!(factor(48_000, 384_000), Some(8));
        assert_eq!(factor(44_100, 96_000), None);
        assert_eq!(factor(44_100, 132_300), None);
        assert_eq!(factor(96_000, 48_000), None);
        assert_eq!(factor(44_100, 44_100), None);

        let dac = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
        assert_eq!(target_rate(44_100, 4, &dac), Some(176_400));
        assert_eq!(target_rate(48_000, 4, &dac), Some(192_000));
        assert_eq!(target_rate(44_100, 2, &dac), Some(88_200));
        assert_eq!(target_rate(96_000, 4, &dac), Some(192_000));
        // Already at or above the target: left alone.
        assert_eq!(target_rate(96_000, 2, &dac), None);
        assert_eq!(target_rate(192_000, 4, &dac), None);
        // The device tops out below the target: as far as it goes.
        assert_eq!(target_rate(44_100, 8, &dac), Some(176_400));
        assert_eq!(target_rate(44_100, 4, &[44_100, 48_000, 96_000]), None);
        assert_eq!(target_rate(44_100, 0, &dac), None);
        assert_eq!(target_rate(32_000, 4, &dac), None);
    }

    #[test]
    fn output_is_factor_times_as_long_and_keeps_the_original_samples() {
        for factor in FACTORS {
            let input: Vec<f32> = (0..2000).map(|i| sine(1000.0, 44_100.0, i)).collect();
            let out = upsample(&input, factor);
            assert_eq!(out.len(), input.len() * factor as usize * 2);
            for (i, &s) in input.iter().enumerate() {
                let at = i * factor as usize * 2;
                assert_eq!(out[at], s, "×{factor}: frame {i} left");
                assert_eq!(out[at + 1], -s, "×{factor}: frame {i} right");
            }
        }
    }

    #[test]
    fn interpolated_samples_follow_the_waveform() {
        // Up to 20 kHz the samples in between land on the original sine.
        for freq in [100.0, 1000.0, 10_000.0, 19_000.0, 20_000.0] {
            for factor in FACTORS {
                let input: Vec<f32> = (0..4000).map(|i| sine(freq, 44_100.0, i)).collect();
                let out = upsample(&input, factor);
                let rate = 44_100.0 * f64::from(factor);
                let worst = (200 * factor as usize..3800 * factor as usize)
                    .map(|i| (out[i * 2] - sine(freq, rate, i)).abs())
                    .fold(0.0f32, f32::max);
                assert!(worst < 1e-4, "{freq} Hz ×{factor}: error {worst}");
            }
        }
    }

    #[test]
    fn a_steady_level_stays_steady() {
        let out = upsample(&[0.25; 500], 4);
        assert!(
            out.chunks(2)
                .skip(400)
                .take(1000)
                .all(|f| (f[0] - 0.25).abs() < 1e-6)
        );
    }
}
