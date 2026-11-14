#!/bin/sh
# TIDE installer for macOS.
#
#   curl -fsSL https://gametl.github.io/tide-tidal-rust/install.sh | sh
#
# Downloads the latest TIDE release (a universal dmg from GitHub Releases),
# copies TIDE.app into /Applications (or ~/Applications when /Applications is
# not writable) and cleans up. Files fetched with curl are not quarantined, so
# macOS opens TIDE without the "Apple could not verify" prompt.
#
# Options (environment variables):
#   TIDE_INSTALL_DIR  install somewhere else, e.g. TIDE_INSTALL_DIR=~/Apps
#   TIDE_DMG_URL      install a specific dmg instead of the latest release
set -eu

DMG_URL="${TIDE_DMG_URL:-https://github.com/GameTL/tide-tidal-rust/releases/latest/download/TIDE-macos-universal.dmg}"

# ── look ────────────────────────────────────────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then
    fancy=1
    b=$(printf '\033[1m'); d=$(printf '\033[2m'); r=$(printf '\033[0m')
    # Cyan to green, like the sea.
    cyan=$(printf '\033[38;5;51m'); aqua=$(printf '\033[38;5;50m')
    mint=$(printf '\033[38;5;49m'); green=$(printf '\033[38;5;48m')
    red=$(printf '\033[31m'); clr=$(printf '\033[K')
else
    fancy=0; b=; d=; r=; cyan=; aqua=; mint=; green=; red=; clr=
fi

# A little wave rolling by while we work, shading from cyan to green.
wave="▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▂▃▄▅▆▇█▇▆▅▄▃▂"
frames=14
if [ "$fancy" = 1 ]; then
    # Precompute the coloured frames as f0..f13 (3 bytes per block character).
    eval "$(printf '%s\n' "$wave" | LC_ALL=C awk -v n="$frames" -v q="'" '
        BEGIN { split("51 51 50 50 49 49 48 48", c, " ") }
        {
            for (f = 0; f < n; f++) {
                out = ""
                for (k = 0; k < 8; k++)
                    out = out "\033[38;5;" c[k + 1] "m" substr($0, (f + k) * 3 + 1, 3)
                printf "f%d=%s%s\033[0m%s\n", f, q, out, q
            }
        }')"
fi

tmp=""
mnt=""
cleanup() {
    if [ -n "$mnt" ] && [ -d "$mnt" ]; then
        hdiutil detach "$mnt" -quiet -force >/dev/null 2>&1 || true
    fi
    if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
    if [ "$fancy" = 1 ]; then printf '\033[?25h'; fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

fail() {
    printf '\r  %s✗%s %s%s\n' "$red" "$r" "$1" "$clr" >&2
    if [ -n "${2:-}" ] && [ -s "$2" ]; then
        sed 's/^/    /' "$2" >&2
    fi
    exit 1
}

# step LABEL COMMAND...: run COMMAND in the background with the wave spinner.
step() {
    label=$1
    shift
    log="$tmp/step.log"
    "$@" >"$log" 2>&1 &
    pid=$!
    if [ "$fancy" = 1 ]; then
        i=0
        while kill -0 "$pid" 2>/dev/null; do
            eval "frame=\$f$i"
            extra=""
            if [ -n "${progress_file:-}" ] && [ -f "$progress_file" ]; then
                extra=$(stat -f %z "$progress_file" 2>/dev/null |
                    awk '{ if ($1 > 0) printf "  %.1f MB", $1 / 1048576 }')
            fi
            printf '\r  %s  %s%s%s%s' "$frame" "$label" "$cyan" "$extra" "$r$clr"
            i=$(((i + 1) % frames))
            sleep 0.08
        done
    fi
    if wait "$pid"; then
        printf '\r  %s✓%s %s%s\n' "$green" "$r" "$label" "$clr"
    else
        fail "$label failed" "$log"
    fi
}

# ── checks ──────────────────────────────────────────────────────────────────
[ "$(uname -s)" = Darwin ] || fail "TIDE runs on macOS only."
command -v hdiutil >/dev/null 2>&1 || fail "hdiutil is missing; is this macOS?"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/tide-install.XXXXXX")
dmg="$tmp/TIDE.dmg"

if [ -n "${TIDE_INSTALL_DIR:-}" ]; then
    dest_dir=$TIDE_INSTALL_DIR
elif [ -w /Applications ]; then
    dest_dir=/Applications
else
    dest_dir="$HOME/Applications"
fi
app="$dest_dir/TIDE.app"

if [ "$fancy" = 1 ]; then printf '\033[?25l'; fi
printf '\n  %s≈ %s~ %s≈  %sTIDE%s %s· native TIDAL player for macOS%s\n\n' \
    "$cyan" "$aqua" "$green" "$b$cyan" "$r" "$d" "$r"

# ── download ────────────────────────────────────────────────────────────────
progress_file=$dmg
step "Downloading the latest release" curl -fsSL --retry 2 -o "$dmg" "$DMG_URL"
progress_file=""

# ── unpack ──────────────────────────────────────────────────────────────────
mnt="$tmp/mnt"
mkdir -p "$mnt"
step "Opening the disk image" hdiutil attach "$dmg" -nobrowse -readonly -noautoopen -mountpoint "$mnt"
[ -d "$mnt/TIDE.app" ] || fail "The disk image has no TIDE.app inside."

# ── install ─────────────────────────────────────────────────────────────────
install_app() {
    mkdir -p "$dest_dir"
    rm -rf "$app"
    ditto "$mnt/TIDE.app" "$app"
    # Belt and braces: curl does not quarantine, but clear it if anything did.
    /usr/bin/xattr -dr com.apple.quarantine "$app" 2>/dev/null || true
}
step "Copying TIDE.app to $dest_dir" install_app

hdiutil detach "$mnt" -quiet >/dev/null 2>&1 || true
mnt=""

version=$(/usr/bin/defaults read "$app/Contents/Info" CFBundleShortVersionString 2>/dev/null || echo "")

# ── done ────────────────────────────────────────────────────────────────────
printf '\n  🌊 %sTIDE%s%s%s is installed.%s\n\n' "$b$cyan" "$green" "${version:+ $version}" "$r$b" "$r"
printf '     %sLocation%s   %s%s%s\n' "$d" "$r" "$cyan" "$app" "$r"
printf '     %sOpen it%s    %sopen -a "%s"%s   %s(or search TIDE in Spotlight)%s\n' "$d" "$r" "$green" "$app" "$r" "$d" "$r"
if pgrep -x TIDE >/dev/null 2>&1; then
    printf '\n     %sTIDE is running: quit and reopen it to use this version.%s\n' "$d" "$r"
fi
printf '\n'
