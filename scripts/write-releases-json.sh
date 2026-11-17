#!/bin/sh
# Write website/src/data/releases.json from GitHub Releases for GameTL/tide-tidal-rust.
set -eu

repo="${GITHUB_REPOSITORY:-GameTL/tide-tidal-rust}"
out="${1:-website/src/data/releases.json}"
cd "$(dirname "$0")/.."

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required" >&2
    exit 1
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

gh api "repos/${repo}/releases" --paginate >"$tmp" 2>/dev/null || printf '[]\n' >"$tmp"

generated=$(date -u +%Y-%m-%dT%H:%M:%SZ)

jq -n --arg generated "$generated" --slurpfile raw "$tmp" '
  # Prefer the dmg; older releases only shipped a zip.
  def mac_url:
    (.assets // []) as $a
    | ([$a[] | select(.name | test("macos-universal\\.dmg$"; "i"))]
       + [$a[] | select(.name | test("macos-universal\\.zip$"; "i"))])
    | .[0].browser_download_url? // null;
  {
    generatedAt: $generated,
    releases: (
      $raw[0]
      | map(select(.draft == false and .prerelease == false))
      | map({
          version: (.tag_name | ltrimstr("v")),
          tag: .tag_name,
          publishedAt: .published_at,
          macUniversalUrl: mac_url,
          linuxUrl: null,
          windowsUrl: null
        })
    )
  }
' >"$out"

printf 'Wrote %s (%s releases)\n' "$out" "$(jq '.releases | length' "$out")"
