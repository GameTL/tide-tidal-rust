#!/bin/sh
# Report which parts of the repo changed between two git revisions, as
# GitHub Actions outputs (key=true|false), so CI and releases can skip
# rebuilding the app when only the website or docs changed.
#
#   sh scripts/changed.sh <base> [head]   # head defaults to HEAD
#
#   app=       anything that goes into the TIDE binary or its packaging:
#              everything except website/, docs/, Markdown, .github/ and the
#              website-only releases.json script
#   website=   website/ or scripts/write-releases-json.sh
#   workflows= .github/
#
# An unknown base (a new branch, the all-zero "before" SHA, a missing tag)
# reports everything as changed, so the safe default is to build.
set -eu

base=${1:-}
head=${2:-HEAD}

if [ -z "$base" ] || ! git rev-parse -q --verify "${base}^{commit}" >/dev/null; then
  printf 'app=true\nwebsite=true\nworkflows=true\n'
  exit 0
fi

# Two-dot compares the two trees directly. Release PRs are squash-merged, so
# staging never contains the release tag and a merge-base diff would overstate.
any() {
  if [ -n "$(git diff --name-only "$base" "$head" -- "$@")" ]; then echo true; else echo false; fi
}

echo "app=$(any . \
  ':(exclude)website' \
  ':(exclude)docs' \
  ':(exclude).github' \
  ':(exclude,glob)**/*.md' \
  ':(exclude)scripts/write-releases-json.sh')"
echo "website=$(any website scripts/write-releases-json.sh)"
echo "workflows=$(any .github)"
