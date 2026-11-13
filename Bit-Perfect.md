# Bit-perfect playback

How TIDE decides whether the DAC gets the file's samples unchanged, and what
happens when it cannot.

This describes the code as it is. The player-bar sparkle opens the signal path;
the toggle at the bottom is the same setting as Settings → Playback → Match
output sample rate (on by default).

## Bit-perfect

The output is bit-perfect when all of these hold:

- The stream is lossless (FLAC), not AAC.
- TIDE's own volume is 100%. Use the device's volume instead.
- The output is a wired device TIDE is allowed to re-clock (not Bluetooth,
  AirPlay, or a virtual device).
- The device is running at the track's sample rate, so nothing is resampled.

The signal path then reads **Lossless** (16-bit / 44.1 kHz) or **Hi-Res
Lossless** (more than that). Any other path is **High quality** (lossless
source, processed) or **Low quality** (AAC or Bluetooth).

TIDE never takes exclusive (hog) access. Other apps can still play.

## When the device cannot run at the track rate

Matching stays on, but the DAC does not offer 88.2 kHz, 96 kHz, or whatever
the file is. TIDE does **not** interpolate across clock families when an
integer downsample exists.

It reduces `source:device` to lowest terms with the **highest common factor
(HCF)**. When that ratio is `M:1`, the device is switched to that rate and
each group of `M` source frames is box-averaged into one output frame.

| Source | Device offers | HCF | Ratio | What TIDE does |
| --- | --- | --- | --- | --- |
| 88.2 kHz | 44.1 kHz | 44.1 kHz | 2:1 | Run at 44.1, average pairs |
| 96 kHz | 48 kHz | 48 kHz | 2:1 | Run at 48, average pairs |
| 176.4 kHz | 88.2 or 44.1 kHz | that rate | 2:1 or 4:1 | Highest of those it has |
| 192 kHz | 96 or 48 kHz | that rate | 2:1 or 4:1 | Highest of those it has |

So a 24-bit / 96 kHz track on a USB DAC that only does 44.1 and 48 kHz plays
at **48 kHz**, not 44.1. 96→44.1 is 320:147 after HCF — not an integer
downsample — and is only used when the device has no 48 / 96 / 192 kHz rate.

The signal path still shows a sample-rate conversion step and **High
quality**. Integer downsample is not bit-perfect: the samples that leave TIDE
are not the samples in the file.

## Cross-family rates

When HCF does not reduce to `M:1` (96 kHz → 44.1 kHz, 88.2 kHz → 48 kHz),
TIDE keeps the device where it is and uses linear interpolation. Same if
sample-rate matching is off, or the output is Bluetooth / AirPlay / virtual:
the device is not re-clocked.

## Gapless

A following track is only joined gaplessly when it wants the same device rate
as the one playing, including this HCF fallback. 96 kHz then 48 kHz on a
48 kHz DAC stay in one stream; 96 kHz then 88.2 kHz re-clocks (44.1 family)
and is not joined.
