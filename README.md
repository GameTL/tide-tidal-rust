# TIDE

A lightweight, native macOS desktop player for TIDAL, written in pure Rust
(Slint UI, Symphonia decoding, CoreAudio output). No Electron, no WebView.

> TIDE is an unofficial, independent project. It is not affiliated with,
> endorsed by, or supported by TIDAL. You need your own TIDAL subscription.

**Site:** [gametl.github.io/tide-tidal-rust](https://gametl.github.io/tide-tidal-rust/)  
**Releases:** merge a release PR (`staging` → `main`) to publish a universal macOS zip and refresh the download page.

## Features

- Home feed, favourite tracks, playlists (with folders), mixes, artist pages, album details, and catalog search
- Lossless FLAC playback at the track's native sample rate, with seek,
  shuffle, repeat and gapless hand-over to the next track
- Synced lyrics in the full-screen now-playing view
- Local-first: library metadata, artwork and audio are cached on disk, so
  already-played music starts without touching the network
- macOS media keys and Now Playing integration

## Run

```bash
cargo run --release -p tide-app
```

The first launch shows a login screen (TIDAL device-code flow in your browser).

To build a macOS app bundle with the TIDE icon and launch it from Finder or the Dock:

```bash
sh scripts/bundle-macos.sh
open target/release/TIDE.app
```

Pass `debug` to build `target/debug/TIDE.app`.

### Desktop typography

For the desktop client's Nationale font, extract the `Fonts` folder from the
[TIDAL press kit](https://tidal.com/press) into `crates/tide-app/fonts`, or
set `TIDAL_FONT_DIR` to its absolute path before building. Builds without
these optional local files use Helvetica Neue.

## Layout

| Crate | Purpose |
| --- | --- |
| `crates/tide-api` | Login, session handling, TIDAL web API |
| `crates/tide-cache` | SQLite metadata store, audio and artwork caches |
| `crates/tide-audio` | Decoder and gapless CoreAudio output engine |
| `crates/tide-macos` | Media keys and Now Playing |
| `crates/tide-app` | The Slint application (`TIDE` binary) |

See [Goal.md](Goal.md) for the design goals and [SECURITY.md](SECURITY.md) for how
login tokens and local data are handled.
