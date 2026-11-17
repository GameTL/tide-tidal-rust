import type { APIRoute } from 'astro';
import releasesData from '../data/releases.json';

// A one-page site doesn't need @astrojs/sitemap. lastmod follows the newest
// release, which is when the page content actually changes.
export const GET: APIRoute = ({ site }) => {
  const home = new URL(import.meta.env.BASE_URL, site).href;
  const latest = releasesData.releases?.[0] as { publishedAt?: string | null } | undefined;
  const lastmod = (latest?.publishedAt ?? new Date().toISOString()).slice(0, 10);
  const body = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url>
    <loc>${home}</loc>
    <lastmod>${lastmod}</lastmod>
    <changefreq>weekly</changefreq>
    <priority>1.0</priority>
  </url>
</urlset>
`;
  return new Response(body, { headers: { 'Content-Type': 'application/xml; charset=utf-8' } });
};
