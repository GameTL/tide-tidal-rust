#!/bin/sh
# Write the over-the-air update manifest (latest.json) for a TIDE.app zip.
#
#   sh scripts/sign-update.sh ZIP VERSION URL > latest.json
#
# The ed25519 private key comes from TIDE_UPDATE_KEY (PEM text, as the GitHub
# secret holds it) or TIDE_UPDATE_KEY_FILE (a PEM file). TIDE checks the
# signature with the public key in crates/tide-app/src/update.rs, over the same
# message as `signed_message` there: the version and the zip's SHA-256.
#
#   sh scripts/sign-update.sh --check-key
#
# exits non-zero unless that private key belongs to the public key TIDE embeds.
# Installed copies reject updates signed by any other key, so the release
# workflow runs this before it tags anything.
set -eu

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "${TIDE_UPDATE_KEY:-}" ]; then
    (umask 077; printf '%s\n' "$TIDE_UPDATE_KEY" > "$tmp/key.pem")
    key="$tmp/key.pem"
elif [ -n "${TIDE_UPDATE_KEY_FILE:-}" ]; then
    key=$TIDE_UPDATE_KEY_FILE
else
    echo "Set TIDE_UPDATE_KEY or TIDE_UPDATE_KEY_FILE" >&2
    exit 1
fi

if [ "${1:-}" = "--check-key" ]; then
    source_file="$(dirname "$0")/../crates/tide-app/src/update.rs"
    embedded=$(sed -n '/^const PUBLIC_KEY/,/^];/p' "$source_file" |
        grep -o '0x[0-9a-fA-F][0-9a-fA-F]' | sed 's/^0x//' | tr -d '\n' | tr 'A-F' 'a-f')
    # An ed25519 public key in DER ends with the 32 raw key bytes.
    actual=$(openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')
    if [ ${#embedded} -ne 64 ] || [ "$embedded" != "$actual" ]; then
        echo "The update key does not match PUBLIC_KEY in crates/tide-app/src/update.rs:" >&2
        echo "  embedded in TIDE: $embedded" >&2
        echo "  TIDE_UPDATE_KEY:  $actual" >&2
        exit 1
    fi
    echo "The update key matches the public key TIDE embeds."
    exit 0
fi

[ $# -eq 3 ] || { echo "usage: $0 ZIP VERSION URL | --check-key" >&2; exit 2; }
zip=$1
version=${2#v}
url=$3

sha=$(openssl dgst -sha256 -r "$zip" | cut -d' ' -f1)
printf 'TIDE update v1\n%s\n%s\n' "$version" "$sha" > "$tmp/message"
signature=$(openssl pkeyutl -sign -rawin -inkey "$key" -in "$tmp/message" | base64 | tr -d '\n')

printf '{\n  "version": "%s",\n  "url": "%s",\n  "sha256": "%s",\n  "signature": "%s"\n}\n' \
    "$version" "$url" "$sha" "$signature"
