# TIDE landing page + release CI design

Date: 2026-11-12  
Status: draft for review  
Repo: `GameTL/tide-tidal-rust`

## Goal

Ship a T3-codes-inspired dark landing page for **TIDE**, and a 3dfabth-style release pipeline where merging a release PR into `main` publishes a macOS universal binary and updates the website download list. Public URL for v1: `https://gametl.github.io/tide-tidal-rust/`.

## Decisions (locked)

| Topic | Choice |
| --- | --- |
| Public URL | `gametl.github.io/tide-tidal-rust` (accept long path; no rename / mirror repo) |
| Site location | `website/` inside this monorepo |
| Site stack | Astro |
| Branches | `staging` (integration) → release PR → `main` (prod; replaces 3dfabth `prod`) |
| Staging binaries | Build on push to `staging`; **keep last 3** artifacts, delete older |
| PR / dev build | Keep a single disposable PR binary artifact |
| macOS artifact | Universal `TIDE.app` zipped; unsigned; page notes right-click → Open |
| Linux / Windows | Listed as **planning** only |
| Download freshness | Release CI regenerates site data + deploys Pages (not live GitHub API in browser) |
| RAM copy | Soft: typically under ~100 MB vs multi-hundred-MB Electron clients |
| Signing | Unsigned for now |
| Screenshots | Capture by running the app (home + bit-perfect / signal path) |
| Hero ornaments | Floating MAX / HIRES / LOSSLESS badges + light TIDE logo marks |
| Competitor framing | No Roon mentions; bit-perfect without exclusive/hog takeover |
| Positioning | Unofficial, opinionated TIDAL API music player |

## 1. Site & information architecture

Single Astro landing page under `website/`, deployed as GitHub Pages with base path `/tide-tidal-rust/`.

**Above the fold**

- TIDE logo (from `crates/tide-app/assets/TIDE.png` / brand assets)
- One headline + one short supporting line
- Primary CTA: **Download for macOS** (links to latest GitHub Release universal zip)
- Secondary: link to GitHub repo
- Floating quality badges (MAX / HIRES / LOSSLESS) + light TIDE marks
- Large product screenshot of the running TIDE app (not T3 content chrome)

**Mid page (short beats, not a content farm)**

- Bit-perfect: match output sample rate when the device allows; **does not take exclusive/hog access**; other apps can still play. No Roon comparison.
- RAM: soft under-~100 MB vs multi-hundred-MB Electron clients.
- Privacy: **“I don’t want your data.”** Local session/cache only; no telemetry / analytics / crash phone-home (aligned with `SECURITY.md`). Contacts TIDAL only.

**Download block**

1. **macOS** (first): show the **latest 3** release versions with download links (universal zip).
2. **Linux** — planning (no binary).
3. **Windows** — planning (no binary).
4. **Older versions**: card grid; each card has the version and OS boxes (mac download; linux/windows planning).

**Footer**

- Unofficial disclaimer: not affiliated with, endorsed by, or supported by TIDAL; requires the user’s own subscription.
- GitHub link; optional pointer to `SECURITY.md` on GitHub.

**Out of scope for v1**

- Testimonials, blog, phone/mobile app claims, DMG, notarization, real Linux/Windows builds, custom domain / `tide.github.io`.

## 2. Visual system

- Dark T3-like surface: near-black background, high-contrast white/gray type, one strong primary CTA (blue or TIDE accent — prefer existing TIDE UI gold/cyan quality colours sparingly, not purple-gradient AI clichés).
- Logo: replace any T3 mark with TIDE assets.
- Typography: purposeful non-default stack (not Inter/Roboto/system-only); keep the composition one hero idea, not a dashboard.
- Hero budget: brand, one headline, one subline, CTA group, ornaments, one dominant app image.
- No Roon-styled marketing language; signal-path UI may be shown as TIDE’s own.

## 3. CI / release pipeline

Adapted from `3dfabth-full` (`.github/workflows/{ci,release-pr,release,deploy-staging}.yml`), with `main` **instead of** `prod`, Rust/macOS artifacts instead of Go/Cloudflare deploy.

### Branches

- `staging`: default integration branch (already default today).
- `main`: production; create from `staging` at bootstrap if missing. Pages deploy and GitHub Releases come from here.

### Workflows

| Workflow | Trigger | Behaviour |
| --- | --- | --- |
| `ci.yml` | PR / push to `staging` or `main` | `cargo` fmt/clippy/test (as feasible on runner); if `website/` changed, `npm`/`pnpm` install + `astro check`/`build` |
| Staging binary | Push to `staging` | Build universal macOS app on `macos-*` runner, zip `TIDE.app`, upload artifact named by SHA/version; **delete staging artifacts beyond the newest 3** |
| PR binary | `pull_request` | One disposable universal (or arch) zip artifact for testing |
| `release-pr.yml` | `workflow_dispatch` | Open/refresh PR `staging` → `main` with semver bump from labels / inputs (same idea as 3dfabth) |
| `release.yml` | PR merged into `main`, or `workflow_dispatch` | Resolve version → tag `vX.Y.Z` → build universal zip → GitHub Release with asset → write download manifest for Astro → `astro build` → deploy GitHub Pages |

### Download data flow

On each production release:

1. List published GitHub Releases (or the just-created one + history).
2. Write a generated file consumed by Astro (e.g. `website/src/data/releases.json`) with version, published date, mac asset URL, and flags for planning OSes.
3. Build site with correct `base: '/tide-tidal-rust/'`.
4. Deploy to GitHub Pages (Actions `peaceiris/actions-gh-pages` or `actions/upload-pages-artifact` + `actions/deploy-pages`).

Landing CTA always points at the newest release’s mac zip. “Last 3” and “older” are slices of that manifest.

### Runners & constraints

- Universal binary requires a **macOS** GitHub-hosted (or self-hosted Mac) runner and a dual-arch or `lipo` build path extending `scripts/bundle-macos.sh`.
- Unsigned: release notes / site footer mention Gatekeeper right-click → Open.
- No Cloudflare / self-hosted Linux deploy jobs from 3dfabth; those are not ported.

### Artifact retention

- Staging: keep **3** newest staging binary artifacts; prune older on each successful staging build.
- PR: single artifact per PR workflow run (GitHub’s default retention is fine; no long-term archive).
- Releases: permanent via GitHub Releases.

## 4. Screenshots & assets

Captured during implementation by running `TIDE` locally (logged-in session on the build Mac):

1. **Hero / primary:** main library or home surface showing the real app chrome + TIDE logo in-app if visible.
2. **Bit-perfect:** signal-path / quality UI showing bit-perfect or sample-rate match **without** implying exclusive device takeover.

Store under `website/public/` (or `website/src/assets/`). Use `crates/tide-app/assets/TIDE.png` (and icns-derived art if needed) for brand marks. Do not ship T3 assets or third-party vendor logos beyond abstract quality badges.

## 5. Copy guidelines

- Lead with unofficial, opinionated **TIDAL API** desktop player (pure Rust / native — short, not a feature dump).
- Bit-perfect: honest about when match-rate applies; never claim hog/exclusive mode; never mention Roon.
- RAM: soft range only until Activity Monitor evidence exists for a release build.
- Privacy punchline: **“I don’t want your data.”** Support with local-only cache/session and no telemetry (see `SECURITY.md`).
- CTA secondary can be direct (“GitHub”) rather than T3’s joke line, unless a short equivalent fits without clutter.
- Always include the unaffiliated-with-TIDAL disclaimer.

### Draft headline direction (final wording in implementation)

- Headline idea: native, unofficial TIDAL player — lightweight, local-first.
- Subline idea: bring your own subscription; Hi-Res when login allows; bit-perfect when the output allows; we don’t want your data.

## 6. Error handling & edge cases

- **No releases yet:** CTA disabled or points to GitHub Releases page; list shows empty/planning state, not broken links.
- **Pages base path:** all asset URLs must respect `/tide-tidal-rust/` or local preview breaks in prod.
- **Failed mac build on release:** fail the release job; do not publish a tag/Release without the zip (or roll back tag if tagging is last — prefer build-before-tag or delete tag on failure).
- **Staging prune failure:** log and continue; next run retries delete.
- **Unsigned Gatekeeper:** documented on download section so users are not surprised.

## 7. Testing / verification

- Local: `astro dev` with mock `releases.json`; visual check desktop + narrow viewport.
- CI: website build on PRs that touch `website/`; rust checks unchanged in spirit.
- After first release: open Pages URL, confirm CTA downloads latest zip, three recent versions listed, older cards render, Linux/Windows planning visible.
- Screenshot audit: bit-perfect panel readable; no Roon text; TIDE logo correct.

## 8. Implementation order (for the later plan)

1. Bootstrap `main` branch + Pages settings + `website/` Astro skeleton with base path.
2. Port/adapt workflows from 3dfabth; wire universal zip + retention.
3. Capture screenshots; drop in assets; write copy sections.
4. Release-manifest generation + download UI (last 3 + older cards).
5. End-to-end: staging build → release PR → merge `main` → Pages live.

## Non-goals

- Reserving `tide.github.io` / org `tide` (blocked by existing GitHub user `Tide`).
- Notarization, Sparkle auto-update, telemetry, Linux/Windows binaries, Electron.
- Porting 3dfabth Cloudflare staging/prod web deploys.
