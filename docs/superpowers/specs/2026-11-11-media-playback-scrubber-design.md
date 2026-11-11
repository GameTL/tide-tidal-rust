# Design Spec: TIDAL Media Playback Scrubber

## Overview
Replace the generic timeline `Slider` in `PlayerBar` (`crates/tide-app/ui/chrome.slint`) with a dedicated TIDAL-style `Scrubber` component featuring quality-accented hover/drag states, cursor hover preview shade, live scrubbed timestamp preview, clickable remaining-time toggle, and reliable seeking across progressive `.tmp` and finalized `.audio` downloads.

---

## 1. Visual & Interaction Design (`chrome.slint` & `common.slint`)

### Slint State (`App` global in `common.slint`)
- `in property <int> duration-secs: 0;` — populated in `Ui::present_current` from `track.duration_secs`.
- `in-out property <bool> show-remaining: false;` — toggles right-hand timestamp between total duration (`3:08`) and remaining time (`-1:36`).
- `pure public function format-time(secs: int) -> string` — formats seconds into `m:ss` (`floor(max(0, secs) / 60)` + `:` + zero-padded `mod(max(0, secs), 60)`), matching Rust's `fmt_time` (`format!("{}:{:02}", secs / 60, secs % 60)`).

### `Scrubber` Component (`chrome.slint`)
Replaces the timeline `Slider` and flanking `Text` elements in `PlayerBar` (both normal bottom bar and `NowPlayingView` overlay):
- **Hitbox**: `height: 20px` full-width `TouchArea` (`enabled: App.has-track`, `mouse-cursor: App.has-track ? pointer : default`).
- **Idle State (`!active`, where `active = App.has-track && (ta.has-hover || ta.pressed)`)**:
  - Track height: `2px`, `border-radius: 1px`, background `#2e2e2e`.
  - Played fill: `2px`, background `#b3b3b3`, width `bar-w * App.progress`.
  - Thumb: hidden (`opacity: 0`).
- **Active State (`active`, hovered or pressed)**:
  - Track height smoothly animates (`120ms`) to `4px` (`border-radius: 2px`), background `#383838`.
  - **Hover preview bar**: `4px` high, background `#ffffff24`, extending from `0` to `clamp(ta.mouse-x, 0px, bar-w)` when `ta.has-hover`.
  - **Played / scrub fill**: `4px` high, background `App.accent` (`#ffd432` MAX, `#33ffee` Lossless, `#ffffff` High/Low), width `bar-w * shown` where `shown = ta.pressed ? drag : App.progress`.
  - **Thumb**: `12px × 12px` circle (`border-radius: 6px`) in `App.accent`, centered vertically at `clamp(bar-w * shown - 6px, 0px, bar-w - 12px)`.
- **Scrubbing & Seeking**:
  - Pressing and dragging updates `drag = clamp(ta.mouse-x / bar-w, 0, 1)` locally at full UI frame rate without invoking Rust on every pixel.
  - Releasing (`PointerEventKind.up`) emits `App.seek(drag)` to jump audio and update macOS `MPNowPlayingInfoCenter`.
- **Flanking Timestamps**:
  - Let `shown-secs = round((ta.pressed ? drag : App.progress) * App.duration-secs)`.
  - **Left timestamp (elapsed / scrub preview)**:
    - Displays `ta.pressed ? App.format-time(shown-secs) : App.elapsed`.
    - Color: `ta.pressed ? App.accent : active ? Theme.text : #6e6e6e`.
  - **Right timestamp (total / remaining toggle)**:
    - Displays `App.show-remaining && App.has-track ? "-" + App.format-time(max(0, App.duration-secs - shown-secs)) : App.total`.
    - Wrapped in a `TouchArea` (`mouse-cursor: pointer`) that toggles `App.show-remaining = !App.show-remaining` on click; brightens from `#6e6e6e` to `Theme.text` on hover.

---

## 2. Controller & Audio Seeking Reliability

### Controller (`crates/tide-app/src/controller.rs`)
- In `Ui::present_current()`: call `app.set_duration_secs(track.duration_secs as i32);` alongside `set_total` / `set_elapsed` / `set_progress`, and reset `app.set_duration_secs(0)` on logout.
- Verify `fmt_time` in Rust and `App.format-time` in Slint produce identical `m:ss` strings (`0:00`, `0:05`, `3:08`, `62:03`).

### Progressive Download Seeking (`crates/tide-audio/src/lib.rs`)
Currently, tracks started progressively open `.tmp` via `GrowingFile` (which returns `Unsupported` on `Seek::seek` and `is_seekable() == false`). Even if `.tmp` has already finished downloading and been renamed to `.audio`, `AudioSource::seek` fails until playback hits natural EOF.
- **Upgrade finalized files on seek**: At the start of `AudioSource::seek(to)`, if `self.growing` and `self.item.file_path.with_extension("audio").exists()`, reopen the `.audio` file as a normal seekable `AudioSource` before executing `reader.seek(...)`.
- **Seek within downloaded bytes on active `.tmp`**: Implement `Seek` and `is_seekable() -> true` on `GrowingFile` delegating to `self.file.seek(pos)` (with `byte_len()` querying `self.file.metadata().ok().map(|m| m.len())`) while keeping the `UnexpectedEof` progressive wait loop in `read_stereo`. If seeking past the currently downloaded byte range fails in the demuxer, `AudioSource::seek` returns `Err` cleanly without crashing or corrupting current playback.

---

## 3. Testing Strategy
- **Audio unit tests (`crates/tide-audio/src/lib.rs`)**:
  - Seeking on an `AudioSource` opened from `.tmp` once `.audio` exists upgrades transparently and lands at the target timestamp.
  - Seeking within an active `.tmp` (`GrowingFile`) succeeds within already-written bytes.
- **Slint / App verification (`crates/tide-app`)**:
  - Compile Slint UI and verify `App.format-time` formatting (`0:00`, `0:05`, `3:08`, `62:03`) and scrubber interaction tests.
