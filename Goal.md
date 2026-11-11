# Tidal Rust Desktop Player for macOS: Goal & Architecture Specification

## 1. High-Level Objective
Build a lightweight, native macOS desktop music player for TIDAL in **100% pure Rust** (no Electron, no WebView, no JavaScript, no React).

Primary user milestone: **Able to log in (OAuth PKCE / Device Flow), browse or search, and play a full song in MAX Hi-Res Lossless (up to 24-bit/192kHz FLAC) with gapless playback, instant local caching, and low RAM footprint (<50MB).**

---

## 2. Core Pillars & Principles

### A. Exact 1:1 Tidal Desktop UI (Pure Rust Native Slint)
- **Slint (Native Cocoa / Skia / Software backend)**: Built on the high-performance pure-Rust native desktop UI framework, providing 120 FPS rendering, native Cocoa window, low CPU, and minimal RAM footprint (<20MB).
- **Visual Design**:
  - Dark theme matching Tidal's authentic palette: `#121212` main background, `#181818` card surfaces, `#242424` borders/hover states, `#ffffff` primary typography, `#a0a0a0` secondary text, and `#00ffff` / `#00e5ff` cyan active accents.
  - Left navigation sidebar: Home, Explore, Search, My Collection (Playlists, Mixes & Radio, Artists, Albums, Tracks).
  - Main viewport: Hero banner, scrollable media carousels, track listing tables (Index, Title + Artist, Album, Heart/Favorite, Duration, Context menu).
  - Bottom Now Playing Bar:
    - Left: Album cover art thumbnail, track title, artist name, favorite heart toggle.
    - Center: Transport controls (Shuffle, Previous, Play/Pause, Next, Repeat), interactive timeline scrubber with elapsed and total time.
    - Right: Audio Quality badge (`MAX` / `HI-RES` / `LOSSLESS` / `HIGH`), Volume slider with mute button, Queue drawer toggle, Output device selector.
  - Interactive Login screen with copyable device code / PKCE browser launcher.

### B. Fast Local-First Cache (Zero Phone-Home)
- **SQLite Metadata Store (`tidal-cache`)**:
  - Located in `~/Library/Application Support/tidal-rust/library.db`.
  - Caches user favorites, playlists, followed artists, recent play history, and catalog search results.
  - Boot time is sub-50ms because UI renders directly from local SQLite.
  - No network requests needed to display already-cached playlists or albums.
- **Transparent Audio Stream Cache**:
  - Located in `~/Library/Caches/tidal-rust/audio/`.
  - When streaming any track, raw audio chunks/files (FLAC / DASH) are written transparently to disk.
  - If a track exists in local cache, it streams directly from SSD storage. **Zero network calls, zero bandwidth waste, zero phoning home.**
  - LRU (Least Recently Used) cache eviction policy with configurable max size (e.g. 20 GB default).
- **Artwork Cache**:
  - Stored in `~/Library/Caches/tidal-rust/images/` with on-disk sizing to prevent redownloading image assets.

### C. Low RAM & Gapless Audio Engine (`tidal-audio`)
- **Memory Footprint**: Target <50 MB resident memory (Tidal's official Electron client regularly consumes 500MB–1.2GB).
- **Decoding**: Pure Rust audio decoding using `symphonia` (supports FLAC up to 24-bit/192kHz, ALAC, AAC).
- **Audio Output**: `cpal` targeting macOS native CoreAudio with exclusive/bit-perfect device stream matching.
- **Sample-Accurate Gapless Playback**:
  - Double-buffered audio stream preloader.
  - When track $N$ is within 10 seconds of completion, track $N+1$ is fetched from cache or network, decoded ahead of time, and buffered into a ring buffer.
  - Seamless boundary handover with zero clicks, pops, or audio gaps.

### D. Full macOS Integration (`tidal-macos`)
- **Media Keys**: Native listener for Play/Pause, Previous, Next via `MPRemoteCommandCenter`.
- **Now Playing / Control Center**: `MPNowPlayingInfoCenter` updating track title, artist, album art, elapsed playback time, and playback rate.
- **Menu Bar Companion**: Minimal status item in macOS top menu bar with quick transport controls.

---

## 3. Architecture & Workspace Structure

```
tidal-rust/
├── Cargo.toml                    # Virtual workspace
├── Goal.md                       # This specification & milestone roadmap
├── crates/
│   ├── tidal-api/                # Tidal API v1/v2, PKCE & Device Auth, DASH Parser, Playback Info
│   ├── tidal-cache/              # SQLite metadata DB & LRU chunked audio disk store
│   ├── tidal-audio/              # Symphonia decoder + cpal CoreAudio engine + gapless preloader
│   ├── tidal-macos/              # objc2 bridge for MPRemoteCommandCenter & Now Playing
│   └── tidal-app/                # Native desktop client (Slint) implementing 1:1 Tidal UI
```

---

## 4. Immediate Milestone Checklist: First Playable Prototype
1. [x] Workspace initialization & `Goal.md` architecture setup.
2. [x] `tidal-api`: Implement OAuth2 Device Code and PKCE login, token refresh, and Hi-Res playback stream resolution (DASH manifest + direct FLAC URLs).
3. [x] `tidal-cache`: Implement transparent local audio cache (stream-and-save to SSD) and SQLite catalog storage.
4. [x] `tidal-audio`: Implement pure Rust audio decoder & player with ring buffer and gapless preloading.
5. [x] `tidal-macos`: Wire media keys and Now Playing metadata.
6. [x] `tidal-app`: Build native Slint main window with 1:1 Tidal dark styling, auth screen, track list, and now-playing transport bar.
7. [x] End-to-end verification: Build, verify auth flow, cache hit flow, and HiRes playback pipeline.
