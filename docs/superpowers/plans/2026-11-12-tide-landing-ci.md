# TIDE Landing Page + Release CI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship an Astro landing page at `gametl.github.io/tide-tidal-rust` and a staging→main release pipeline that publishes a universal macOS zip and regenerates download links.

**Architecture:** `website/` Astro static site reads `src/data/releases.json`. GitHub Actions on `macos-latest` build universal `TIDE.app` zips; merge to `main` tags a release, writes `releases.json`, builds Astro, deploys Pages. Staging keeps the last 3 binary artifacts.

**Tech Stack:** Astro 5, vanilla CSS, GitHub Actions (`macos-latest` + `ubuntu-latest`), `scripts/bundle-macos.sh` (+ universal flag), `gh` CLI in workflows.

## Global Constraints

- Public URL base: `/tide-tidal-rust/`
- Branches: `staging` → release PR → `main` (never `prod`)
- Artifact: unsigned universal `TIDE.app` zip; Gatekeeper right-click note on site
- Linux/Windows: planning only
- No Roon mentions; bit-perfect without exclusive/hog
- Soft RAM copy (~100 MB vs multi-hundred-MB Electron)
- Privacy: “I don’t want your data.”
- Staging artifacts: keep last 3
- Logo: `crates/tide-app/assets/TIDE.png`

---

### Task 1: Astro site skeleton + landing UI

**Files:**
- Create: `website/package.json`, `website/astro.config.mjs`, `website/tsconfig.json`, `website/src/pages/index.astro`, `website/src/layouts/Layout.astro`, `website/src/styles/global.css`, `website/src/data/releases.json`, `website/src/components/{Hero,Download,Beats,Footer}.astro`, `website/public/brand/tide.png`
- Modify: none

**Interfaces:**
- Consumes: `releases.json` shape below
- Produces: static site buildable with `npm run build` in `website/`

`releases.json` shape:

```json
{
  "generatedAt": "2026-11-12T00:00:00Z",
  "releases": [
    {
      "version": "0.3.1",
      "tag": "v0.3.1",
      "publishedAt": "2026-11-12T00:00:00Z",
      "macUniversalUrl": "https://github.com/GameTL/tide-tidal-rust/releases/download/v0.3.1/TIDE-0.3.1-macos-universal.zip",
      "linuxUrl": null,
      "windowsUrl": null
    }
  ]
}
```

- [ ] **Step 1:** Scaffold Astro (`npm create astro@latest` or hand-write minimal files). Set `site: 'https://gametl.github.io'`, `base: '/tide-tidal-rust/'`.
- [ ] **Step 2:** Copy logo to `website/public/brand/tide.png`.
- [ ] **Step 3:** Implement dark T3-like landing: Hero (headline, subline, Download CTA, GitHub link, floating MAX/HIRES/LOSSLESS + TIDE marks, app screenshot slot), Beats (bit-perfect / RAM / privacy), Download (mac last 3, linux/windows planning, older cards), Footer (disclaimer + SECURITY link).
- [ ] **Step 4:** Seed empty or mock `releases.json` (empty `releases: []` → CTA points to GitHub Releases page).
- [ ] **Step 5:** `cd website && npm install && npm run build` — expect success.
- [ ] **Step 6:** Commit `feat(website): add Astro landing page skeleton`

---

### Task 2: Universal macOS zip script

**Files:**
- Modify: `scripts/bundle-macos.sh`
- Create: `scripts/package-macos-zip.sh`

**Interfaces:**
- `scripts/package-macos-zip.sh [release|debug] [--universal] [--name TIDE]` → prints path to zip
- Universal: build `aarch64-apple-darwin` + `x86_64-apple-darwin`, `lipo -create`, then bundle as `TIDE.app` (display name without `dev-` for release)

- [ ] **Step 1:** Add `--universal` path and release naming (`TIDE.app` / `TIDE-$VERSION-macos-universal.zip`).
- [ ] **Step 2:** Local smoke: `sh scripts/package-macos-zip.sh release` (native arch OK if cross targets missing; CI installs both).
- [ ] **Step 3:** Commit `feat(scripts): package universal macOS app zip`

---

### Task 3: GitHub Actions workflows

**Files:**
- Create: `.github/workflows/ci.yml`, `.github/workflows/staging-binary.yml`, `.github/workflows/pr-binary.yml`, `.github/workflows/release-pr.yml`, `.github/workflows/release.yml`
- Create: `scripts/write-releases-json.sh` (gh api → `website/src/data/releases.json`)

**Behaviour (summary):**
- `ci.yml`: cargo check/test on ubuntu; website build if `website/**` changed; branches `staging|main`
- `staging-binary.yml`: push to staging → macos universal zip artifact; prune artifacts older than newest 3 with same name prefix via gh api
- `pr-binary.yml`: one zip artifact per PR
- `release-pr.yml`: dispatch → PR staging→main with version bump (port from 3dfabth, `prod`→`main`)
- `release.yml`: on merged PR to main → build zip → tag → gh release → write releases.json → astro build → deploy pages

- [ ] **Step 1:** Implement workflows + `write-releases-json.sh`.
- [ ] **Step 2:** Commit `ci: add staging/main release and Pages workflows`

---

### Task 4: Capture screenshots + wire images

**Files:**
- Create: `website/public/screenshots/app-home.png`, `website/public/screenshots/bit-perfect.png`
- Modify: Hero/Beats components to use them

- [ ] **Step 1:** Run TIDE, capture home + signal-path bit-perfect UI (screencapture).
- [ ] **Step 2:** Drop into `website/public/screenshots/`, rebuild.
- [ ] **Step 3:** Commit `feat(website): add TIDE app screenshots`

---

### Task 5: Bootstrap `main` + Pages

- [ ] **Step 1:** Ensure `main` exists (`git branch main staging` / push if missing).
- [ ] **Step 2:** Enable GitHub Pages from Actions (document in README snippet); first deploy via workflow_dispatch or push site to `gh-pages` / Pages artifact.
- [ ] **Step 3:** Update root `README.md` with site URL + release flow one-liner.
- [ ] **Step 4:** Commit docs; push when user asks (do not force).

---

## Self-review

- Spec URL, Astro, CI shape, screenshots, copy rules, unsigned zip, retention 3 → covered in tasks 1–5.
- No notarization / Linux binaries / tide.github.io in plan.
