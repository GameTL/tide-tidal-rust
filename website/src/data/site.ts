// One source of truth for what the site says about TIDE. The page, the
// structured data (JSON-LD), llms.txt and the sitemap all read from here, so
// search engines and AI assistants see the same facts as people do.

import { github, latestMacDmg } from './links';

export const site = {
  name: 'TIDE',
  title: 'TIDE: native, bit-perfect TIDAL player for macOS',
  tagline: 'The native player for your TIDAL subscription.',
  description:
    'TIDE is a fast, native TIDAL player for macOS, written in pure Rust. Instant loading, bit-perfect playback without exclusive mode, a parametric EQ (DSP) with AutoEq profiles, and a music video quality selector. Free and open source.',
  keywords: [
    'TIDE for TIDAL',
    'TIDAL player',
    'TIDAL subscription',
    'TIDAL desktop app',
    'TIDAL for macOS',
    'native TIDAL client',
    'Rust TIDAL client',
    'Rust implementation',
    'fast',
    'instant loading',
    'bit-perfect playback',
    'bit-perfect without exclusive mode',
    'hi-res FLAC',
    'parametric EQ',
    'DSP',
    'AutoEq',
    'music video quality selector',
    'open source music player',
  ],
  license: 'MIT OR Apache-2.0',
  author: { name: 'GameTL', url: 'https://github.com/GameTL' },
  github,
  downloadUrl: latestMacDmg,
  ogImage: { path: 'og.png', width: 1200, height: 630, alt: 'TIDE, a native TIDAL player for macOS' },
} as const;

export const features = [
  'Bit-perfect playback: switches the output device to the track’s sample rate, without exclusive mode',
  'TIDAL MAX (up to 24-bit / 192 kHz FLAC), HIGH (16-bit / 44.1 kHz FLAC) and LOW (AAC) quality',
  'Signal path view that explains whether playback is bit-perfect',
  'Two parametric EQs per output: SPK for headphone or speaker correction, USR for taste',
  'AutoEq profile search, plus Equalizer APO and GraphicEQ import',
  'Up to 20 peak and shelf bands, undo, and automatic preamp to prevent clipping',
  'Fast, instant loading: no Electron or WebView to start up',
  'Music videos with a quality selector: Auto, or a fixed rendition such as 1080p or 720p',
  'Gapless playback, synced lyrics, playlists, mixes and artist pages',
  'Local cache: played music starts without touching the network',
  'macOS media keys and Now Playing integration',
  'No telemetry, analytics, crash reports or update checks',
  'Native UI in pure Rust, GPU-rendered at the display’s refresh rate; no Electron or WebView',
];

export type Faq = { q: string; a: string };

export const faqs: Faq[] = [
  {
    q: 'What is TIDE?',
    a: 'TIDE is a free, open-source desktop player for TIDAL, written in pure Rust for macOS. It plays your TIDAL library bit-perfect, adds a parametric EQ, and replaces the Electron-based official app with a native one.',
  },
  {
    q: 'Is TIDE an official TIDAL app?',
    a: 'No. TIDE is an unofficial, independent project. It is not affiliated with, endorsed by, or supported by TIDAL.',
  },
  {
    q: 'Do I need a TIDAL subscription?',
    a: 'Yes. You sign in with your own TIDAL account, and TIDE plays what your subscription allows. TIDE itself is free.',
  },
  {
    q: 'Does TIDE play hi-res and bit-perfect audio?',
    a: 'Yes, when the output allows it. TIDE plays TIDAL MAX up to 24-bit / 192 kHz FLAC and switches your output device to the track’s sample rate, so nothing is resampled. It does not need exclusive mode, so other apps can still play. The signal path view shows whether playback is bit-perfect and why.',
  },
  {
    q: 'Can I use AutoEq headphone profiles?',
    a: 'Yes. Each output has two parametric EQs: SPK for headphone or speaker correction and USR for taste. You can search and load AutoEq profiles in the app, or import Equalizer APO and GraphicEQ files. Turning the EQ off restores bit-perfect playback.',
  },
  {
    q: 'Does TIDE collect data or use telemetry?',
    a: 'No. Login tokens, cache and play counts stay on your Mac. TIDE only connects to TIDAL for music and artwork, and to GitHub for AutoEq profiles when you ask for one. There are no analytics, crash reports or update checks.',
  },
  {
    q: 'Which platforms does TIDE support?',
    a: 'macOS, as a universal binary for Apple Silicon and Intel Macs. Linux and Windows builds are planned.',
  },
  {
    q: 'Why does macOS say “Apple could not verify TIDE”?',
    a: 'TIDE is a free, personal project, so it is not signed through Apple’s paid Developer Program ($99 a year), and macOS warns about apps it cannot verify. Click Done, then open System Settings › Privacy & Security and click Open Anyway; you only do this once. Or install from Terminal with `curl -fsSL https://gametl.github.io/tide-tidal-rust/install.sh | sh`, which opens without the prompt.',
  },
];
