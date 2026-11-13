//! Signal path: what happens to the audio between the file and the output
//! device, and whether the result is bit-perfect.

use tide_audio::{FormatInfo, OutputInfo, OutputKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// An equaliser (DSP) shapes the sound on purpose.
    Enhanced,
    /// Bit-perfect and the file is high resolution (more than 16-bit / 44.1 kHz).
    HiRes,
    /// The device receives exactly the samples in the file.
    Lossless,
    /// Lossless source, but the samples are altered on the way (resampled, volume).
    High,
    /// A lossy step is involved (AAC source or a Bluetooth link).
    Low,
}

impl Quality {
    pub fn key(self) -> &'static str {
        match self {
            Quality::Enhanced => "enhanced",
            Quality::HiRes => "max",
            Quality::Lossless => "lossless",
            Quality::High => "high",
            Quality::Low => "low",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Quality::Enhanced => "Enhanced",
            Quality::HiRes => "Hi-Res Lossless",
            Quality::Lossless => "Lossless",
            Quality::High => "High quality",
            Quality::Low => "Low quality",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    /// "source" | "process" | "output"
    pub kind: &'static str,
    pub title: String,
    pub detail: String,
    pub quality: Quality,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalPath {
    pub quality: Quality,
    pub stages: Vec<Stage>,
    /// One sentence on why the path is (or is not) bit-perfect.
    pub note: String,
}

pub fn khz(rate: u32) -> String {
    format!("{} kHz", rate as f32 / 1000.0)
}

/// `volume` is TIDE's own (software) volume, 0.0..=1.0. `dsp` describes the
/// equalisers in the path, when any is changing the samples.
pub fn signal_path(
    format: FormatInfo,
    output: &OutputInfo,
    volume: f32,
    rate_matching: bool,
    released: bool,
    dsp: Option<&str>,
) -> SignalPath {
    let lossy_source = format.bits_per_sample == 0;
    let resampled = format.sample_rate != output.sample_rate;
    let attenuated = volume < 0.999;
    let hires_source =
        !lossy_source && (format.bits_per_sample > 16 || format.sample_rate > 44_100);
    let mut stages = vec![Stage {
        kind: "source",
        title: "Source".into(),
        detail: {
            let base = if lossy_source {
                format!("AAC {}", khz(format.sample_rate))
            } else {
                format!(
                    "FLAC {}-bit {}",
                    format.bits_per_sample,
                    khz(format.sample_rate)
                )
            };
            if format.bitrate_kbps > 0 {
                format!("{base} · {}kbps", format.bitrate_kbps)
            } else {
                base
            }
        },
        quality: if lossy_source {
            Quality::Low
        } else if hires_source {
            Quality::HiRes
        } else {
            Quality::Lossless
        },
    }];
    if resampled {
        stages.push(Stage {
            kind: "process",
            title: "Sample rate conversion".into(),
            detail: format!("{} to {}", khz(format.sample_rate), khz(output.sample_rate)),
            quality: Quality::High,
        });
    }
    if let Some(detail) = dsp {
        stages.push(Stage {
            kind: "process",
            title: "DSP".into(),
            detail: detail.into(),
            quality: Quality::Enhanced,
        });
    }
    if attenuated {
        stages.push(Stage {
            kind: "process",
            title: "Volume".into(),
            detail: format!("Software volume {:.0}%", volume * 100.0),
            quality: Quality::High,
        });
    }
    let link_quality = match output.kind {
        OutputKind::Bluetooth => Quality::Low,
        // The link or driver decides what reaches the speaker; it cannot be verified.
        OutputKind::Network | OutputKind::Virtual => Quality::High,
        _ if hires_source && !resampled => Quality::HiRes,
        _ => Quality::Lossless,
    };
    stages.push(Stage {
        kind: "output",
        title: output.name.clone(),
        detail: format!("{} · {}", output.kind.label(), khz(output.sample_rate)),
        quality: link_quality,
    });

    let quality = stages
        .iter()
        .map(|s| s.quality)
        .fold(Quality::Lossless, |worst, q| match (worst, q) {
            (Quality::Enhanced, _) | (_, Quality::Enhanced) => Quality::Enhanced,
            (Quality::Low, _) | (_, Quality::Low) => Quality::Low,
            (Quality::High, _) | (_, Quality::High) => Quality::High,
            _ => Quality::Lossless,
        });
    // Bit-perfect all the way: it is "Hi-Res" when the file itself is.
    let quality = if quality == Quality::Lossless && hires_source {
        Quality::HiRes
    } else {
        quality
    };

    let note = if released {
        "Paused: the device is back at the sample rate it had before TIDE changed it.".to_string()
    } else if dsp.is_some() {
        "Enhanced: TIDE's equaliser is shaping the sound, so the output is not bit-perfect. Turn the EQs off in DSP for an untouched signal.".to_string()
    } else if output.kind == OutputKind::Bluetooth {
        "Bluetooth re-encodes the audio, so bit-perfect output isn't possible. TIDE leaves the device at its own sample rate.".to_string()
    } else if lossy_source {
        "The stream is AAC, a lossy format.".to_string()
    } else if resampled && !output.kind.allows_rate_matching() {
        format!(
            "This kind of output can't be switched to {}, so the track is resampled.",
            khz(format.sample_rate)
        )
    } else if resampled && !rate_matching {
        "Sample-rate matching is turned off in Settings, so the track is resampled.".to_string()
    } else if resampled {
        format!(
            "This device doesn't support {}, so the track is resampled.",
            khz(format.sample_rate)
        )
    } else if attenuated {
        "Volume below 100% changes the samples. Set TIDE to 100% and use the device's own volume for bit-perfect output.".to_string()
    } else if quality == Quality::High {
        "The device runs at the track's sample rate, but this kind of output may process the audio further.".to_string()
    } else {
        "Bit-perfect: the device runs at the track's sample rate and TIDE does not alter the samples.".to_string()
    };
    SignalPath {
        quality,
        stages,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(kind: OutputKind, rate: u32) -> OutputInfo {
        OutputInfo {
            id: "x".into(),
            name: "DAC".into(),
            kind,
            sample_rate: rate,
            supported_rates: vec![44_100, 48_000, 96_000],
        }
    }

    const HIRES: FormatInfo = FormatInfo {
        bits_per_sample: 24,
        sample_rate: 96_000,
        bitrate_kbps: 0,
    };

    #[test]
    fn matched_rate_and_full_volume_is_bit_perfect() {
        let path = signal_path(HIRES, &out(OutputKind::Usb, 96_000), 1.0, true, false, None);
        assert_eq!(path.quality, Quality::HiRes);
        assert_eq!(path.stages.len(), 2);
        assert_eq!(path.stages[1].quality, Quality::HiRes);
        assert!(path.note.starts_with("Bit-perfect"));
    }

    #[test]
    fn cd_quality_is_lossless_not_hires() {
        let cd = FormatInfo {
            bits_per_sample: 16,
            sample_rate: 44_100,
            bitrate_kbps: 0,
        };
        let path = signal_path(cd, &out(OutputKind::Usb, 44_100), 1.0, true, false, None);
        assert_eq!(path.quality, Quality::Lossless);
        assert_eq!(path.quality.key(), "lossless");
    }

    #[test]
    fn resampling_or_volume_downgrades_to_high_quality() {
        let resampled = signal_path(
            HIRES,
            &out(OutputKind::BuiltIn, 48_000),
            1.0,
            true,
            false,
            None,
        );
        assert_eq!(resampled.quality, Quality::High);
        assert_eq!(resampled.stages[1].detail, "96 kHz to 48 kHz");

        let quiet = signal_path(HIRES, &out(OutputKind::Usb, 96_000), 0.5, true, false, None);
        assert_eq!(quiet.quality, Quality::High);
        assert_eq!(quiet.stages[1].detail, "Software volume 50%");
    }

    #[test]
    fn an_equaliser_in_the_path_reads_enhanced() {
        let path = signal_path(
            HIRES,
            &out(OutputKind::Usb, 96_000),
            1.0,
            true,
            false,
            Some("SPK EQ"),
        );
        assert_eq!(path.quality, Quality::Enhanced);
        assert_eq!(path.quality.key(), "enhanced");
        assert_eq!(path.stages[1].title, "DSP");
        assert!(path.note.starts_with("Enhanced"));
        // Also over Bluetooth: the star says what TIDE is doing to the sound.
        let bluetooth = signal_path(
            HIRES,
            &out(OutputKind::Bluetooth, 48_000),
            1.0,
            true,
            false,
            Some("SPK EQ"),
        );
        assert_eq!(bluetooth.quality, Quality::Enhanced);
    }

    #[test]
    fn bluetooth_and_lossy_sources_are_never_lossless() {
        let bluetooth = signal_path(
            HIRES,
            &out(OutputKind::Bluetooth, 96_000),
            1.0,
            true,
            false,
            None,
        );
        assert_eq!(bluetooth.quality, Quality::Low);
        assert!(bluetooth.note.contains("Bluetooth"));

        let aac = FormatInfo {
            bits_per_sample: 0,
            sample_rate: 44_100,
            bitrate_kbps: 0,
        };
        assert_eq!(
            signal_path(aac, &out(OutputKind::Usb, 44_100), 1.0, true, false, None).quality,
            Quality::Low
        );
    }
}
