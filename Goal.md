# TIDE Desktop Player for macOS: Goal & Architecture Specification

## 1. High-Level Objective
Build a lightweight, native macOS desktop music player for TIDAL in **100% pure Rust** (no Electron, no WebView, no JavaScript, no React).

Primary user milestone: **Able to log in (OAuth PKCE in the browser, device code as a fallback), browse or search, and play a full song in MAX Hi-Res Lossless (up to 24-bit/192kHz FLAC) with gapless playback, instant local caching, and low RAM footprint (<50MB).**

---

## 2. Core Pillars & Principles

### A. Exact 1:1 Tidal Desktop UI (Pure Rust Native Slint)
- **Slint (Native Cocoa / Skia / Software backend)**: Built on the high-performance pure-Rust native desktop UI framework, with a native Cocoa window and low CPU use.
- **Refresh rate comes first**: the GPU renderer is the default and draws at the display's native refresh rate (120 Hz on ProMotion, 240 Hz on a 240 Hz monitor). Smoothness takes priority over memory.
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
  - "Match output sample rate" is on by default (Settings → Playback). While a track plays and it is on, the output device is switched to the track's sample rate, so nothing is resampled. Tracks with a different rate are not joined gaplessly; the device is re-clocked between them.
  - The same switch is the "Bit-perfect playback" toggle at the bottom of the signal path popup.
  - Never for Bluetooth, AirPlay or virtual devices: those links re-encode or re-clock the audio anyway, so the device is left at its own rate and the track is resampled.
  - If the device does not support the track's rate, TIDE switches to the highest integer-related rate it does support (HCF downsample: 88.2→44.1, 96→48, and the same families at 176.4/192) instead of interpolating across clock families. The UI still shows a conversion step. Details: [Bit-Perfect.md](Bit-Perfect.md).
  - TIDE does not take exclusive (hog) access; other apps can still play sound.
- **Respect other apps**: the sample rate a device had before TIDE changed it is remembered. "Restore original sample rate" is on by default and lives only in Settings → Playback. When it is on, that rate is restored when playback has been paused for a few seconds and whenever TIDE quits (window close, Quit, Ctrl+C), and taken again on resume. When it is off, TIDE leaves the device at the track's sample rate across pause and quit.
- **Signal path indicator** (in the style of Roon): the quality badge opens a signal path showing source format, any sample-rate conversion or software volume step, and the output device. Purple "Lossless" means bit-perfect, green "High quality" means a lossless source that is processed on the way, yellow "Low quality" means a lossy step (AAC source or Bluetooth). It states why bit-perfect is or isn't possible. Under that note, a divider and a "Bit-perfect playback" toggle turn sample-rate matching on or off without leaving the player.
- **Follows the system output**: plugging in headphones or connecting Bluetooth moves playback to the new device and re-evaluates the signal path.
- **Sample-Accurate Gapless Playback**:
  - Double-buffered audio stream preloader.
  - When track $N$ is within 25 seconds of completion, track $N+1$ is fetched from cache or network, decoded ahead of time, and buffered into a ring buffer.
  - Seamless boundary handover with zero clicks, pops, or audio gaps.

### C2. Music Videos (`tide-macos::video` + `tide-app`)
- **Decoding**: AVFoundation's `AVPlayer` streams TIDAL's HLS playlists and decodes them in hardware through VideoToolbox (the same path IINA and VLC use on macOS). No ffmpeg / libmpv / libvlc to bundle, so the app stays small and needs no extra libraries.
- **Drawing**: frames are pulled from an `AVPlayerItemVideoOutput` as BGRA pixel buffers whenever a new one is due (checked every 8 ms, enough for 120 Hz displays) and drawn by Slint, so the player's own controls sit on top of the picture. Frames are only copied while the video is on screen. Video audio goes through TIDE's own signal path: AVFoundation ignores audio taps on HLS, so AVPlayer is muted and the app fetches the same rendition's MPEG-TS segments, pulls out the AAC (ADTS) stream (`tide_api::hls`) and plays it in `tide-audio` with DSP and sample-rate matching, nudged to stay within a few ms of the picture. If that fails, AVPlayer plays its own audio. The signal-path card shows the picture (codec, size, bitrate) and the soundtrack (AAC rate, bitrate); in video mode it gains a quality column on its left. Music and video never play at once.
- **Videos page** (sidebar → Videos): the user's favourite videos as a grid of 16:9 stills with title, artists and length ("4 MIN"). It keeps the collection-grid features of Albums: filter box, sort (Date added, A-Z, Artist, Release date), group-by (Artist, Year, Title), card size, all remembered.
- **Cover hover**: Play (bottom-left), More and Add/Remove My Collection (bottom-right), as on album cards. Right-click or More opens the shared context menu: Play now, Add to / Remove from My Collection, Share › Copy link.
- **Full-window player**: clicking a video plays it filling the whole window (sidebar, top bar and player bar hidden), letterboxed on black. Moving the mouse brings up the player bar and a close button over a light dark gradient at the bottom and top edges; the picture itself is never dimmed. While playing, the controls and the pointer hide after 2.5 s without movement. Click toggles play/pause; double-click, the close button or Esc leaves the view and the video keeps playing in the player bar.
- **Player bar in video mode**: a 16:9 thumbnail (click to return to the video), title, artists, "▶ My Videos" (or wherever it was started), heart, More, the usual transport and scrubber, and the rendition on screen in the quality pill ("1080P", white). Next / Previous step through the videos on the page it was started from; repeat applies.
- **Quality gear**: a gear button next to the pill (and the pill itself) opens a quality menu like a web video player's: Auto, then every rendition the stream offers (1080p, 720p, 480p, 360p, 180p). Switching reopens the stream at the same position. The pick is remembered; with none, the best rendition is used, since adaptive HLS starts at its lowest rendition and takes seconds to climb.
- **Similar videos**: the queue button in the full-window player moves the picture to the left half and lists TIDAL's recommendations on the right; clicking one plays it with that list as the queue.
- **Now Playing / media keys** follow the video while it plays.

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
│   ├── tide-macos/               # objc2 bridge for MPRemoteCommandCenter, Now Playing & AVFoundation video
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
- [x] Bit-perfect output with a signal path indicator. Sample-rate matching is on by default, and the device is handed back at its original sample rate on pause and quit.
- [x] Bit-perfect controls: a "Bit-perfect playback" toggle under the note in the signal path popup (the same setting as Settings → Playback → Match output sample rate), and "Restore original sample rate" in Settings only (on by default). Off keeps the track's sample rate across pause and quit.
- [x] macOS Now Playing & Control Center: Native pure-Rust `objc2` integration with `MPNowPlayingInfoCenter` and `MPRemoteCommandCenter`. Publishes live playback rate (1.0 playing, 0.0 paused) so the system scrubber advances live, elapsed playback time, track metadata, and album artwork loaded directly from the local disk cache into `MPMediaItemArtwork`. Supports media keys (Play, Pause, Toggle, Next, Previous, Stop) and timeline scrubbing via `changePlaybackPositionCommand`.
- [x] How login tokens and local data are handled is documented in [SECURITY.md](SECURITY.md).
- [x] Back / forward page transitions. Each page is a pane:
  - Going back, the current page slides off to the right and the previous page grows from 75% to 100% while fading in from dark. Forward runs the same motion in reverse. Buttons, the menu and ⌘[ / ⌘] run it over 0.15 s with ease-in-out. The top bar, sidebar and player bar never move, and only position, scale and opacity animate.
  - A two-finger trackpad swipe drives the panes directly, following Apple's conventions: a gesture is claimed only when it starts sideways, release past halfway or a flick completes it, the momentum afterwards is ignored, and the system "Swipe between pages" setting is respected. With "Reduce motion" on, buttons and shortcuts switch pages instantly.
  - Pages up to two steps away in history are kept in memory as snapshots, along with each history entry's scroll position. Swiping back shows the page as it was left, and the live page comes back at the same scroll position, so no reload jumps to the top.
  - Esc goes to the home page as a new history entry, so Back returns to where you were. Going to the page already on screen never adds a second history entry.

- [x] Collection grids (Mixes & Radio, Playlists, Albums, Profiles, folders):
  - A filter box narrows the grid as you type ("Filter playlists", "Filter albums", …).
  - A sort button (↑↓) next to the title: Date added, A-Z, Artist and Release date for albums; A-Z, Date added and Creator for playlists; Date added and A-Z for mixes and profiles.
  - A group-by button next to it, in the style of a library view: a rule, the group name and its item count above each block. Albums group by Artist, Year or Title; playlists by Creator or Title; mixes and profiles by Title. Folders always lead.
  - Sort and grouping are remembered per kind of page.
- [x] Create playlists and folders: a "+" on the Playlists page and beside "All playlists" in the sidebar offers Playlist or Folder, then asks for a name. Inside a folder page the new item is created in that folder. Not yet verified against a real account (it writes to the collection).
- [x] Profiles: the artists the user follows, as a grid of round portraits that open the artist page.
- [x] Album page, following the TIDAL client:
  - Add (to My Collection), Credits, Share and More on the right of Play / Shuffle. More opens the album's context menu (below). Report and Save for Later are not offered.
  - The artist name carries the artist's picture, underlines on hover and opens the artist page.
  - Under the track list: release date, track count and length, the copyright line, "More Albums by …" and "Related Artists".
- [x] Playlist and mix pages carry the same buttons: Share and More, plus Add / Added for playlists made by someone else (the user's own are always in the collection).
- [x] One context menu for everything (`context-menu.slint`): right-click a track, album, playlist, mix, artist or folder wherever it is shown (track rows, the player bar, grids, carousels, home tiles, search results, the sidebar), or press its More button. The look is shared (cover, title and subtitle on top, then entries with their icons, flyouts to the side, full keyboard control); the entries depend on what was clicked:
  - Track: Play next, Add to playlist ›, Add to / Remove from My Collection, Go to album, Go to artist, Share › Copy link.
  - Album: Play now, Play next, Add to playlist ›, Add to / Remove from My Collection, Go to artist, Share › Copy link.
  - Playlist: Play now, Play next, Add to playlist ›, Add to / Remove from My Collection (other people's playlists only), Share › Copy link.
  - Mix: Play now, Play next, Add to playlist ›, Share › Copy link. Artist: Go to artist, Share › Copy link.
  - Folder: Rename folder, Delete folder, Create playlist. Header is a teal swatch (no artwork).
  - "Add to playlist" lists the user's own top-level playlists.
- [x] Artist page: a Follow / Following button and a "Related Artists" row.
- [x] Error alerts: every error-level log line (playback failure, expired token, audio stream error) shows as one alert above the player bar. A new one replaces the one on screen; it dismisses itself after 8 seconds. Confirmations ("Album link copied") use the same slot in a neutral style.
- [x] Debug panel (computer button in the top bar): a plain overlay that toggles with the button and never blocks the window. It shows the chunk download grid, the last 4 errors with their log fields, and a verbose log tail (the last 60 lines, newest first; TIDE's own crates down to DEBUG, everything else from INFO).
- [x] `TIDE --get <url> [key=value …]` prints the JSON of a TIDAL API GET, for checking response shapes during development.

- [x] Cover hover on catalog cards (Play, Add/Remove My Collection, More). Mixes omit the heart; artist portraits play top tracks and leave Follow on the artist page; folders keep the teal tile and only show More.
- [x] Explore: Featured carousels with promo art, plus genre/mood/decade tiles from the v1 explore page (nested pages still open).
- [x] Feed: time-grouped list (Last week / This month / Last month / Older) from v2 activities.
- [x] Save for Later: mixed bookmarks when the account endpoint exists; empty state when it does not.
- [x] Home Staff Picks and Uploads tabs (no extra history).
- [x] Music videos (section C2): Videos page with 16:9 grid and album-style filter/sort/group, cover hover, full-window hardware-decoded player with auto-hiding controls over a light gradient, quality gear (Auto + each rendition), Similar videos, and a video mode for the player bar. Verified with real playback through the snapshot tour (`TIDE --snapshot <dir> --play`). Picture-in-picture and AirPlay for video are not done.
