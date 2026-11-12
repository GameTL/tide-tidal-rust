# TIDE Desktop Player for macOS: Goal & Architecture Specification

## 1. High-Level Objective
Build a lightweight, native macOS desktop music player for TIDAL in **100% pure Rust** (no Electron, no WebView, no JavaScript, no React).

Primary user milestone: **Able to log in (OAuth PKCE in the browser, device code as a fallback), browse or search, and play a full song in MAX Hi-Res Lossless (up to 24-bit/192kHz FLAC) with gapless playback, instant local caching, and low RAM footprint (<50MB).**

---

## 2. Core Pillars & Principles

### A. Exact 1:1 Tidal Desktop UI (Pure Rust Native Slint)
- **Slint (Native Cocoa / Skia / Software backend)**: Built on the high-performance pure-Rust native desktop UI framework, with a native Cocoa window and low CPU use.
- **Refresh rate comes first**: the GPU renderer is the default and draws at the display's native refresh rate (120 Hz on ProMotion, 240 Hz on a 240 Hz monitor). Smoothness takes priority over memory. The low-memory (CPU) renderer is an opt-in setting, off by default, for when memory matters more than frame rate.
- **Visual Design**:
  - Black background, `#151515` selected navigation, `#ffffff` primary text, and `#909090` secondary text. Use Nationale for desktop typography when the local font files are available.
  - Desktop navigation follows the TIDAL references. The sidebar groups Music, Explore, and Feed above Collection and All playlists. The top bar provides history, search, and profile controls.
  - Main viewport: Hero banner, scrollable media carousels, track listing tables (Index, Title + Artist, Album, Heart/Favorite, Duration, Context menu).
  - Bottom Now Playing Bar:
    - Left: Album cover art thumbnail, track title, artist name, favorite heart toggle.
    - Center: Transport controls (Shuffle, Previous, Play/Pause, Next, Repeat), interactive timeline scrubber with elapsed and total time.
    - Right: Audio Quality badge (`MAX` / `HI-RES` / `LOSSLESS` / `HIGH`), Volume slider with mute button, Queue drawer toggle, Output device selector.
  - Quality colours. One colour per quality tier, used wherever a tier is shown or chosen:
    - **Low: white** (`#ffffff`, `Theme.text`).
    - **High: blue** (`#33ffee`, `Theme.lossless`).
    - **Max: gold** (`#ffd432`, `Theme.max`).
    - Settings → Audio quality: the selected row's radio dot is in its tier's colour, and the row carries a faint wash of the same hue (grey for Low) from its lower-left corner. Unselected radios stay neutral grey.
    - Player-bar quality pill: text and glow follow the stream that is playing. Gold for hi-res, white for lossy, and for 16-bit lossless the green the official client uses there (`#19d8a7`). The sparkle keeps its own signal-path colour.
  - Login screen: one button starts TIDAL's PKCE login in the default browser, which is the only login that unlocks Hi-Res. TIDE finishes the login by itself: it asks scriptable browsers (Safari, Chromium family) for the redirect address, and in other browsers (Firefox, Zen) picks it up from the clipboard once copied, with a paste box as a last resort. It never reads browser profiles or history. The device-code login stays as a fallback and is limited by TIDAL to 16-bit / 44.1 kHz.

### B. Fast Local-First Cache (Zero Phone-Home)
- **SQLite Metadata Store (`tide-cache`)**:
  - Located in `~/Library/Application Support/tidal-rust/library.db`.
  - Caches user favorites, playlists, followed artists, recent play history, and catalog search results.
  - Keeps a local-only record of play counts and audio-cache hits and misses. It is never sent anywhere.
  - Boot time is sub-50ms because UI renders directly from local SQLite.
  - No network requests needed to display already-cached playlists or albums.
- **Transparent Audio Stream Cache**:
  - Located in `~/Library/Caches/tidal-rust/audio/`.
  - When streaming any track, raw audio chunks/files (FLAC / DASH) are written transparently to disk.
  - If a track exists in local cache, it streams directly from SSD storage. **Zero network calls, zero bandwidth waste, zero phoning home.** There is no telemetry; the only background network use is the optional "Keep most played offline" download described below.
  - Size limit set in Settings (20 GB by default). When the cache is full, the tracks played least are removed first: every play counts, and a play loses half its weight every 30 days, so the cache follows what is listened to now. Ties fall back to least recently used. The track that is playing is never removed.
  - "Keep most played offline" (on by default) re-downloads, in the background, the most played tracks that are missing from the cache, for the best cache hit rate. Settings shows the share of plays served from the cache.
- **Artwork Cache**:
  - Stored in `~/Library/Caches/tidal-rust/images/` with on-disk sizing to prevent redownloading image assets.

### C. Low RAM & Gapless Audio Engine (`tide-audio`)
- **Memory Footprint**: Target <50 MB resident memory (Tidal's official Electron client regularly consumes 500MB–1.2GB).
- **Decoding**: Pure Rust audio decoding using `symphonia` (supports FLAC up to 24-bit/192kHz, ALAC, AAC).
- **Stereo only**: streams are always requested in stereo. Dolby Atmos (E-AC-3) and 360 Reality Audio are declined because the decoder cannot play them.
- **Audio Output**: `cpal` targeting macOS native CoreAudio.
- **Bit-perfect when the output allows it**:
  - While a track plays, the output device is switched to the track's sample rate, so nothing is resampled. Tracks with a different rate are not joined gaplessly; the device is re-clocked between them.
  - Never for Bluetooth, AirPlay or virtual devices: those links re-encode or re-clock the audio anyway, so the device is left at its own rate and the track is resampled.
  - If the device does not support the track's rate, the track is resampled and the UI says so.
  - TIDE does not take exclusive (hog) access; other apps can still play sound.
- **Respect other apps**: the sample rate a device had before TIDE changed it is remembered. It is restored when playback has been paused for a few seconds and whenever TIDE quits (window close, Quit, Ctrl+C), and taken again on resume.
- **Signal path indicator** (in the style of Roon): the quality badge opens a signal path showing source format, any sample-rate conversion or software volume step, and the output device. Purple "Lossless" means bit-perfect, green "High quality" means a lossless source that is processed on the way, yellow "Low quality" means a lossy step (AAC source or Bluetooth). It states why bit-perfect is or isn't possible.
- **Follows the system output**: plugging in headphones or connecting Bluetooth moves playback to the new device and re-evaluates the signal path.
- **Sample-Accurate Gapless Playback**:
  - Double-buffered audio stream preloader.
  - When track $N$ is within 25 seconds of completion, track $N+1$ is fetched from cache or network, decoded ahead of time, and buffered into a ring buffer.
  - Seamless boundary handover with zero clicks, pops, or audio gaps.

### D. Full macOS Integration (`tide-macos`)
- **Media Keys**: Native listener for Play/Pause, Previous, Next, Stop via `MPRemoteCommandCenter`.
- **Now Playing / Control Center**: `MPNowPlayingInfoCenter` updating track title, artist, album art from local image cache, elapsed playback time, and playback rate (1.0 playing, 0.0 paused) so the system scrubber advances live.
- **Timeline Scrubbing**: `changePlaybackPositionCommand` enables seeking from the macOS Control Center and Lock Screen Now Playing widget.

---

## 3. Architecture & Workspace Structure

```
tidal-rust/
├── Cargo.toml                    # Virtual workspace
├── Goal.md                       # This specification & milestone roadmap
├── crates/
│   ├── tide-api/                 # Tidal API v1/v2, PKCE & Device Auth, DASH Parser, Playback Info
│   ├── tide-cache/               # SQLite metadata DB & play-count-ranked audio disk store
│   ├── tide-audio/               # Symphonia decoder + cpal CoreAudio engine + gapless preloader
│   ├── tide-macos/               # objc2 bridge for MPRemoteCommandCenter & Now Playing
│   └── tide-app/                 # Native desktop client (Slint) implementing 1:1 Tidal UI
```

---

## 4. Immediate Milestone Checklist: First Playable Prototype
1. [x] Workspace initialization & `Goal.md` architecture setup.
2. [x] `tide-api`: Implement OAuth2 Device Code and PKCE login, token refresh, and Hi-Res playback stream resolution (DASH manifest + direct FLAC URLs).
3. [x] `tide-cache`: Implement transparent local audio cache (stream-and-save to SSD) and SQLite catalog storage.
4. [x] `tide-audio`: Implement pure Rust audio decoder & player with ring buffer and gapless preloading.
5. [x] `tide-macos`: Wire media keys and Now Playing metadata.
6. [x] `tide-app`: Build native Slint main window with 1:1 Tidal dark styling, auth screen, track list, and now-playing transport bar.
7. [x] End-to-end verification: build, device-code auth flow, cache hit flow, and lossless (16-bit / 44.1 kHz) playback with seek and track changes.
8. [ ] Hi-Res verification with a real account: the PKCE browser login and a real 24-bit TIDAL stream. So far the Hi-Res decode path is verified only with a generated 24-bit / 96 kHz DASH test file, and bit-perfect rate switching only on a Bluetooth output (where it is correctly skipped).

## 5. Desktop Navigation, Catalog, and Settings

- [x] Use **TIDE** as the app name in the macOS menu bar, window title, and executable. Match TIDAL's menu order and sidebar styling, and support sidebar collapse and playlist sorting.
- [x] Add artist pages with top tracks, albums, EPs and singles, and biography when available. Add album pages with artwork, release information, track lists, Play, and Shuffle. Open both from catalog search and track links.
- [x] Use Playtype's **Nationale** in Light, Regular, Medium, DemiBold, and Bold. Local font setup is documented in [README.md](README.md). Builds without the font files use Helvetica Neue.
- [x] Keep Settings focused on supported controls and local information. Persist the audio-quality, renderer, audio-metadata, and collection-visibility preferences, and expose audio-cache controls and system output information.
- [x] Hi-Res login: PKCE in the default browser is the default login and completes without a paste step where the browser allows it. Not yet verified with a real sign-in (see milestone 8).
- [x] Offline cache: size limit, play-count-based eviction, "Keep most played offline", and cache hit rate in Settings.
- [x] Bit-perfect output with a signal path indicator, and the output device handed back at its original sample rate on pause and quit.
- [x] macOS Now Playing & Control Center: Native pure-Rust `objc2` integration with `MPNowPlayingInfoCenter` and `MPRemoteCommandCenter`. Publishes live playback rate (1.0 playing, 0.0 paused) so the system scrubber advances live, elapsed playback time, track metadata, and album artwork loaded directly from the local disk cache into `MPMediaItemArtwork`. Supports media keys (Play, Pause, Toggle, Next, Previous, Stop) and timeline scrubbing via `changePlaybackPositionCommand`.
- [x] How login tokens and local data are handled is documented in [SECURITY.md](SECURITY.md).
- [x] Back / forward page transitions. Each page is a pane:
  - Going back, the current page slides off to the right and the previous page grows from 75% to 100% while fading in from dark. Forward runs the same motion in reverse. Buttons, the menu and ⌘[ / ⌘] run it over 0.3 s with ease-in-out. The top bar, sidebar and player bar never move, and only position, scale and opacity animate.
  - A two-finger trackpad swipe drives the panes directly, following Apple's conventions: a gesture is claimed only when it starts sideways, release past halfway or a flick completes it, the momentum afterwards is ignored, and the system "Swipe between pages" setting is respected. With "Reduce motion" on, buttons and shortcuts switch pages instantly.
  - Pages up to two steps away in history are kept in memory as snapshots, along with each history entry's scroll position. Swiping back shows the page as it was left, and the live page comes back at the same scroll position, so no reload jumps to the top.
  - Esc goes to the home page as a new history entry, so Back returns to where you were. Going to the page already on screen never adds a second history entry.

Explore, Feed, Videos, Profiles, and Save for Later remain navigation placeholders.
