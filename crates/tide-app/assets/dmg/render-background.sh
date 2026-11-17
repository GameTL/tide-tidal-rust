#!/bin/sh
# Re-render the DMG window background from background.html.
# Writes background.tiff, the HiDPI (1x + 2x) image the DMG ships.
# Needs Google Chrome; run it after editing the HTML.
set -eu

cd "$(dirname "$0")"
chrome="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

render() {
    "$chrome" --headless=new --hide-scrollbars --window-size=800,620 \
        --force-device-scale-factor="$1" --screenshot="$tmp/$2" \
        "file://$PWD/background.html" >/dev/null 2>&1
}

render 1 background.png
render 2 background@2x.png
tiffutil -cathidpicheck "$tmp/background.png" "$tmp/background@2x.png" -out background.tiff >/dev/null
echo "$PWD/background.tiff"
