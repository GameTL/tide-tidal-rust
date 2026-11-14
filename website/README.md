# TIDE website

Astro landing page for GitHub Pages at `/tide-tidal-rust/`.

```bash
npm ci
npm run dev    # http://localhost:4321/tide-tidal-rust/
npm run build
```

`src/data/releases.json` is regenerated on release by `scripts/write-releases-json.sh`.

## SEO

- Page copy, features and FAQ live in `src/data/site.ts`; the meta tags, JSON-LD
  (`SoftwareApplication`, `FAQPage`), `llms.txt`, `robots.txt` and `sitemap.xml` are all
  generated from it at build time.
- Screenshots live in `src/assets/` so Astro serves responsive AVIF/WebP.
- `public/og.png` (social preview) is rendered by `python3 scripts/make-og.py` (Pillow);
  rerun it after changing the hero screenshot.
