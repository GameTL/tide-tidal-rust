#!/bin/sh
# Enforce .cursor/rules/display-refresh.mdc
#
# Every on-screen motion is one frame of the window's monitor. Custom visual
# loops use crate::platform::frame_interval() (or frame_interval_from_hz).
# Repeated timers must not tick faster than a 60 Hz frame, and must not lock
# motion to a fixed ~16 ms either — that misses faster panels.
#
# Non-motion polls (progress, debug stats) may be slower than the display;
# this check only rejects intervals at or under one 60 Hz frame.
#
#   sh scripts/lint-display-refresh.sh [path…]
#
# Defaults to crates/tide-app. Exit 1 on any hit.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$root"

if [ "$#" -eq 0 ]; then
  set -- crates/tide-app
fi

# One 60 Hz frame is ~16.67 ms. Anything at or under 16 ms either outruns a
# 60 Hz panel or hard-codes 60 Hz instead of frame_interval().
max_ms=16

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

# Collect .rs under the given paths (skip build artefacts).
find "$@" \( -path '*/target/*' -o -path '*/.git/*' \) -prune -o \
  -type f -name '*.rs' -print |
while IFS= read -r file; do
  # Portable awk (no gawk match arrays): after TimerMode::Repeated, look for
  # Duration::from_millis(N) within a few lines.
  awk -v file="$file" -v max_ms="$max_ms" '
    function flush() { pending = 0; start_line = 0 }
    function check_line(line,   n, rest, ms) {
      n = index(line, "Duration::from_millis(")
      if (n == 0) return 0
      rest = substr(line, n + length("Duration::from_millis("))
      if (rest !~ /^[0-9]+\)/) return 0
      ms = rest + 0
      if (ms <= max_ms) {
        printf "%s:%d: Repeated timer interval %dms (max %dms) — use frame_interval() or a slower poll\n", \
          file, NR, ms, max_ms
        bad = 1
      }
      return 1
    }
    /TimerMode::Repeated/ {
      pending = 1
      start_line = NR
      if (check_line($0)) flush()
      next
    }
    pending {
      if (check_line($0)) { flush(); next }
      if ($0 ~ /frame_interval/ || $0 ~ /Duration::from_secs/) flush()
      if (NR - start_line > 6) flush()
    }
    END { exit bad ? 1 : 0 }
  ' "$file" >>"$tmp" || true
done

if [ -s "$tmp" ]; then
  cat "$tmp"
  echo >&2 "display-refresh: use crate::platform::frame_interval() for visual ticks (see .cursor/rules/display-refresh.mdc)"
  exit 1
fi

echo "display-refresh: ok"
