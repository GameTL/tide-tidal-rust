//! Signal path: what happens to the audio between the file and the output
//! device, and whether the result is bit-perfect.

use tide_audio::{FormatInfo, Loudness, Normalization, OutputInfo, OutputKind, TARGET_LUFS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// TIDE's DSP (an equaliser, the upsampler) changes the sound on purpose.
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

/// Volume normalisation and crossfade, as the player runs them.
pub struct Leveling {
    pub normalization: Normalization,
    /// The audible track's loudness measurements.
    pub loudness: Option<Loudness>,
    /// 0 when tracks follow gaplessly.
    pub crossfade_secs: u32,
}

pub fn khz(rate: u32) -> String {
    format!("{} kHz", rate as f32 / 1000.0)
}

/// "24-bit 96 kHz": what the device runs at. Only the rate while the depth is unknown.
pub fn output_format(output: &OutputInfo) -> String {
    if output.bit_depth == 0 {
        khz(output.sample_rate)
    } else {
        format!("{}-bit {}", output.bit_depth, khz(output.sample_rate))
    }
}

/// `volume` is TIDE's own (software) volume, 0.0..=1.0. `dsp` describes the
/// equalisers in the path, when any is changing the samples. `upsampling` is
/// the multiple the listener's upsampler (DSP page) raises this track by.
#[allow(clippy::too_many_arguments)]
pub fn signal_path(
    format: FormatInfo,
    output: &OutputInfo,
    volume: f32,
    rate_matching: bool,
    released: bool,
    dsp: Option<&str>,
    upsampling: Option<u32>,
    leveling: &Leveling,
) -> SignalPath {
    let lossy_source = format.bits_per_sample == 0;
    let upsampling = upsampling.filter(|_| output.sample_rate > format.sample_rate);
    // A conversion the device forces, as opposed to one the listener chose.
    let resampled = format.sample_rate != output.sample_rate && upsampling.is_none();
    let attenuated = volume < 0.999;
    // The device takes fewer bits than the file has: the lowest ones are lost.
    let truncated = output.bit_depth != 0 && output.bit_depth < format.bits_per_sample;
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
    if let Some(factor) = upsampling {
        stages.push(Stage {
            kind: "process",
            title: "Upsampling".into(),
            detail: format!(
                "{} to {} · {factor}× linear phase",
                khz(format.sample_rate),
                khz(output.sample_rate)
            ),
            quality: Quality::Enhanced,
        });
    }
    // The decoder levels each track, then blends it into the next.
    let album = leveling.normalization == Normalization::Album;
    let normalized = leveling.normalization != Normalization::Off;
    if normalized {
        stages.push(match leveling.loudness {
            Some(loudness) => Stage {
                kind: "process",
                title: "Volume normalisation".into(),
                detail: format!(
                    "{} {:.1} LUFS · {:+.1} dB",
                    if album { "Album" } else { "Track" },
                    loudness.lufs(album),
                    loudness.gain_db(album)
                ),
                quality: Quality::High,
            },
            None => Stage {
                kind: "process",
                title: "Volume normalisation".into(),
                detail: "No loudness data · unchanged".into(),
                quality: Quality::Lossless,
            },
        });
    }
    let crossfade = leveling.crossfade_secs > 0;
    if crossfade {
        stages.push(Stage {
            kind: "process",
            title: "Crossfade".into(),
            detail: format!("{} s into the next track", leveling.crossfade_secs),
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
    if truncated {
        stages.push(Stage {
            kind: "process",
            title: "Bit depth reduction".into(),
            detail: format!("{}-bit to {}-bit", format.bits_per_sample, output.bit_depth),
            quality: Quality::High,
        });
    }
    let link_quality = match output.kind {
        OutputKind::Bluetooth => Quality::Low,
        // The link or driver decides what reaches the speaker; it cannot be verified.
        OutputKind::Network | OutputKind::Virtual => Quality::High,
        _ if hires_source && !resampled && upsampling.is_none() && !truncated => Quality::HiRes,
        _ => Quality::Lossless,
    };
    stages.push(Stage {
        kind: "output",
        title: output.name.clone(),
        detail: format!("{} · {}", output.kind.label(), output_format(output)),
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
        "Paused: the device is back at the format it had before TIDE changed it.".to_string()
    } else if dsp.is_some() && upsampling.is_some() {
        "Enhanced: TIDE's equaliser and upsampler are changing the samples, so the output is not bit-perfect. Turn both off in DSP for an untouched signal.".to_string()
    } else if dsp.is_some() {
        "Enhanced: TIDE's equaliser is shaping the sound, so the output is not bit-perfect. Turn the EQs off in DSP for an untouched signal.".to_string()
    } else if upsampling.is_some() {
        format!(
            "Enhanced: TIDE upsamples the track to {}, so the output is not bit-perfect. The original samples are kept and the ones in between are computed. Turn upsampling off in DSP for an untouched signal.",
            khz(output.sample_rate)
        )
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
    } else if truncated && !output.kind.allows_rate_matching() {
        format!(
            "This kind of output stays at {}-bit, so the {}-bit track loses its lowest bits.",
            output.bit_depth, format.bits_per_sample
        )
    } else if truncated && !rate_matching {
        format!(
            "Bit-perfect playback is turned off, so the device stays at {}-bit and the {}-bit track loses its lowest bits.",
            output.bit_depth, format.bits_per_sample
        )
    } else if truncated {
        format!(
            "This device has no {}-bit mode at {}, so the track's lowest bits are lost.",
            format.bits_per_sample,
            khz(output.sample_rate)
        )
    } else if normalized && leveling.loudness.is_some() {
        format!(
            "Volume normalisation brings every {} to {TARGET_LUFS} LUFS, so the samples are scaled and the output is not bit-perfect.",
            if album { "album" } else { "track" }
        )
    } else if crossfade {
        "Crossfade blends the end of each track into the next, so the output is not bit-perfect where tracks meet.".to_string()
    } else if attenuated {
        "Volume below 100% changes the samples. Set TIDE to 100% and use the device's own volume for bit-perfect output.".to_string()
    } else if quality == Quality::High {
        "The device runs at the track's sample rate, but this kind of output may process the audio further.".to_string()
    } else {
        "Bit-perfect: the device runs at the track's sample rate, takes every bit of it, and TIDE does not alter the samples.".to_string()
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

    /// The path without normalisation or crossfade.
    fn signal_path(
        format: FormatInfo,
        output: &OutputInfo,
        volume: f32,
        rate_matching: bool,
        released: bool,
        dsp: Option<&str>,
        upsampling: Option<u32>,
    ) -> SignalPath {
        super::signal_path(
            format,
            output,
            volume,
            rate_matching,
            released,
            dsp,
            upsampling,
            &Leveling {
                normalization: Normalization::Off,
                loudness: None,
                crossfade_secs: 0,
            },
        )
    }

    #[test]
    fn normalisation_and_crossfade_show_as_stages_with_the_loudness() {
        let loudness = Loudness {
            track_gain_db: -10.0,
            track_peak: 0.99,
            album_gain_db: -9.0,
            album_peak: 0.99,
        };
        let path = |normalization, loudness, crossfade_secs| {
            super::signal_path(
                HIRES,
                &out(OutputKind::Usb, 96_000),
                1.0,
                true,
                false,
                None,
                None,
                &Leveling {
                    normalization,
                    loudness,
                    crossfade_secs,
                },
            )
        };
        let track = path(Normalization::Track, Some(loudness), 0);
        assert_eq!(track.quality, Quality::High);
        assert_eq!(track.stages[1].title, "Volume normalisation");
        assert_eq!(track.stages[1].detail, "Track -8.0 LUFS · -6.0 dB");
        assert!(track.note.contains("every track to -14 LUFS"));
        let album = path(Normalization::Album, Some(loudness), 6);
        assert_eq!(album.stages[1].detail, "Album -9.0 LUFS · -5.0 dB");
        assert_eq!(album.stages[2].title, "Crossfade");
        assert_eq!(album.stages[2].detail, "6 s into the next track");
        // No measurements: nothing is scaled, so the path stays bit-perfect.
        let unmeasured = path(Normalization::Track, None, 0);
        assert_eq!(unmeasured.quality, Quality::HiRes);
        assert_eq!(unmeasured.stages[1].detail, "No loudness data · unchanged");
        let fading = path(Normalization::Off, None, 3);
        assert_eq!(fading.quality, Quality::High);
        assert!(fading.note.starts_with("Crossfade blends"));
    }

    fn out(kind: OutputKind, rate: u32) -> OutputInfo {
        OutputInfo {
            id: "x".into(),
            name: "DAC".into(),
            kind,
            sample_rate: rate,
            supported_rates: vec![44_100, 48_000, 96_000],
            bit_depth: 24,
            supported_depths: vec![16, 24],
        }
    }

    const HIRES: FormatInfo = FormatInfo {
        bits_per_sample: 24,
        sample_rate: 96_000,
        bitrate_kbps: 0,
    };

    #[test]
    fn matched_rate_and_full_volume_is_bit_perfect() {
        let path = signal_path(
            HIRES,
            &out(OutputKind::Usb, 96_000),
            1.0,
            true,
            false,
            None,
            None,
        );
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
        let path = signal_path(
            cd,
            &out(OutputKind::Usb, 44_100),
            1.0,
            true,
            false,
            None,
            None,
        );
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
            None,
        );
        assert_eq!(resampled.quality, Quality::High);
        assert_eq!(resampled.stages[1].detail, "96 kHz to 48 kHz");

        let quiet = signal_path(
            HIRES,
            &out(OutputKind::Usb, 96_000),
            0.5,
            true,
            false,
            None,
            None,
        );
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
            None,
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
            None,
        );
        assert_eq!(bluetooth.quality, Quality::Enhanced);
    }

    #[test]
    fn upsampling_is_its_own_stage_and_never_bit_perfect() {
        let cd = FormatInfo {
            bits_per_sample: 16,
            sample_rate: 44_100,
            bitrate_kbps: 0,
        };
        let dac = OutputInfo {
            supported_rates: vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000],
            ..out(OutputKind::Usb, 176_400)
        };
        let path = signal_path(cd, &dac, 1.0, true, false, None, Some(4));
        assert_eq!(path.quality, Quality::Enhanced);
        assert_eq!(path.stages.len(), 3);
        assert_eq!(path.stages[1].title, "Upsampling");
        assert_eq!(
            path.stages[1].detail,
            "44.1 kHz to 176.4 kHz · 4× linear phase"
        );
        // The device no longer gets the file's samples, hi-res or not.
        assert_eq!(path.stages[2].quality, Quality::Lossless);
        assert!(path.note.contains("upsamples the track to 176.4 kHz"));

        // With an EQ as well: upsampling first, as the audio meets them.
        let both = signal_path(cd, &dac, 1.0, true, false, Some("USR EQ"), Some(4));
        assert_eq!(both.stages[1].title, "Upsampling");
        assert_eq!(both.stages[2].title, "DSP");
        assert!(both.note.contains("equaliser and upsampler"));

        // A higher device rate the listener did not ask for is a plain conversion.
        let forced = signal_path(cd, &dac, 1.0, false, false, None, None);
        assert_eq!(forced.quality, Quality::High);
        assert_eq!(forced.stages[1].title, "Sample rate conversion");
    }

    #[test]
    fn a_device_with_fewer_bits_than_the_file_is_not_bit_perfect() {
        let shallow = OutputInfo {
            bit_depth: 16,
            ..out(OutputKind::Usb, 96_000)
        };
        let path = signal_path(HIRES, &shallow, 1.0, true, false, None, None);
        assert_eq!(path.quality, Quality::High);
        assert_eq!(path.stages[1].title, "Bit depth reduction");
        assert_eq!(path.stages[1].detail, "24-bit to 16-bit");
        assert_eq!(path.stages[2].detail, "USB · 16-bit 96 kHz");
        assert_eq!(path.stages[2].quality, Quality::Lossless);
        assert!(path.note.contains("no 24-bit mode at 96 kHz"));
        let off = signal_path(HIRES, &shallow, 1.0, false, false, None, None);
        assert!(off.note.starts_with("Bit-perfect playback is turned off"));

        // More bits than the file has is padding: every sample arrives whole.
        let cd = FormatInfo {
            bits_per_sample: 16,
            sample_rate: 44_100,
            bitrate_kbps: 0,
        };
        let padded = signal_path(
            cd,
            &out(OutputKind::Usb, 44_100),
            1.0,
            true,
            false,
            None,
            None,
        );
        assert_eq!(padded.quality, Quality::Lossless);
        // A depth the system does not report is not held against the path.
        let unknown = OutputInfo {
            bit_depth: 0,
            ..out(OutputKind::Usb, 96_000)
        };
        let path = signal_path(HIRES, &unknown, 1.0, true, false, None, None);
        assert_eq!(path.quality, Quality::HiRes);
        assert_eq!(path.stages[1].detail, "USB · 96 kHz");
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
            signal_path(
                aac,
                &out(OutputKind::Usb, 44_100),
                1.0,
                true,
                false,
                None,
                None
            )
            .quality,
            Quality::Low
        );
    }
}
