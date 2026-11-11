# TIDE

A lightweight, native macOS desktop player for TIDAL, written in pure Rust
(Slint UI, Symphonia decoding, CoreAudio output). No Electron, no WebView.

> TIDE is an unofficial, independent project. It is not affiliated with,
> endorsed by, or supported by TIDAL. You need your own TIDAL subscription.

## Features

- Home feed, favourite tracks, playlists (with folders), mixes, albums, search
- Lossless FLAC playback at the track's native sample rate, with seek,
  shuffle, repeat and gapless hand-over to the next track
- Synced lyrics in the full-screen now-playing view
- Local-first: library metadata, artwork and audio are cached on disk, so
  already-played music starts without touching the network
- macOS media keys and Now Playing integration

## Run

```bash
cargo run --release -p tidal-app
```

The first launch shows a login screen (TIDAL device-code flow in your browser).

## Layout

| Crate | Purpose |
| --- | --- |
| `crates/tidal-api` | Login, session handling, TIDAL web API |
| `crates/tidal-cache` | SQLite metadata store, audio and artwork caches |
| `crates/tidal-audio` | Decoder and gapless CoreAudio output engine |
| `crates/tidal-macos` | Media keys and Now Playing |
| `crates/tidal-app` | The Slint application (`tide` binary) |

See [Goal.md](Goal.md) for the design goals.
