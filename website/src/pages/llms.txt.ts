import type { APIRoute } from 'astro';
import { site, features, faqs } from '../data/site';
import releasesData from '../data/releases.json';

// llms.txt (https://llmstxt.org): a plain Markdown summary that AI assistants
// and answer engines can read without parsing the page's HTML and CSS.
export const GET: APIRoute = ({ site: origin }) => {
  const home = new URL(import.meta.env.BASE_URL, origin).href;
  const latest = releasesData.releases?.[0] as { version: string; publishedAt: string | null } | undefined;
  const version = latest
    ? `Latest release: v${latest.version}${latest.publishedAt ? ` (${latest.publishedAt.slice(0, 10)})` : ''}.`
    : 'Latest release: see GitHub Releases.';

  const body = `# ${site.name}

> ${site.description}

${site.name} is an unofficial, independent project. It is not affiliated with, endorsed by, or supported by TIDAL. A TIDAL subscription is required; ${site.name} itself is free and open source (${site.license}).

- Website: ${home}
- Download (macOS universal, Apple Silicon and Intel): ${site.downloadUrl}
- Source code: ${site.github}
- ${version}

## Features

${features.map((f) => `- ${f}`).join('\n')}

## Audio quality

- MAX: up to 24-bit / 192 kHz FLAC
- HIGH: 16-bit / 44.1 kHz FLAC
- LOW: AAC, for slow connections

## Privacy

Login tokens, cache and play counts stay on the Mac. Outbound connections: api.tidal.com (music), resources.tidal.com (artwork), raw.githubusercontent.com (AutoEq profiles, only on request). No analytics, crash reports or update checks.

## Performance

Native UI in pure Rust (Slint, Symphonia decoding, CoreAudio output), GPU-rendered, with no Electron or WebView. Resident memory while playing is about 200 MB to 1 GB, versus about 500 MB to 1.2 GB for the official Electron app.

## FAQ

${faqs.map((f) => `### ${f.q}\n\n${f.a}`).join('\n\n')}

## Links

- [Releases](${site.github}/releases): all downloadable builds
- [README](${site.github}#readme): features, build instructions and crate layout
- [Bit-perfect playback](${site.github}/blob/staging/Bit-Perfect.md): how sample-rate matching works
- [Security](${site.github}/blob/staging/SECURITY.md): how login tokens are stored
`;
  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
