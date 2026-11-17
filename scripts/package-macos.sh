#!/bin/sh
# Build TIDE.app (optionally universal) and package it as a zip or a dmg.
#
# Signing: by default the app is ad-hoc signed, which macOS Gatekeeper blocks
# ("Apple could not verify TIDE is free of malware"). Set these to sign with a
# Developer ID certificate and notarize, so the app opens without a warning:
#   APPLE_SIGNING_IDENTITY  e.g. "Developer ID Application: Name (TEAMID)"
#   APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD  (app-specific password)
#     -> notarize and staple (needs APPLE_SIGNING_IDENTITY)
set -eu

profile=release
universal=0
app_display=TIDE
format=zip
usage="Usage: $0 [release|debug] [--universal] [--dmg] [--name NAME]"

while [ $# -gt 0 ]; do
    case "$1" in
        release|debug)
            profile="$1"
            shift
            ;;
        --universal)
            universal=1
            shift
            ;;
        --dmg)
            format=dmg
            shift
            ;;
        --name)
            shift
            app_display="$1"
            shift
            ;;
        -h|--help)
            echo "$usage" >&2
            exit 0
            ;;
        *)
            echo "$usage" >&2
            exit 2
            ;;
    esac
done

if [ "$(uname -s)" != Darwin ]; then
    echo "macOS packaging requires Darwin." >&2
    exit 1
fi

cd "$(dirname "$0")/.."
metadata=$(cargo metadata --locked --no-deps --format-version 1)
target_dir=$(printf '%s\n' "$metadata" | plutil -extract target_directory raw -o - -)
package_id=$(cargo pkgid --locked -p tide-app)
version=${package_id##*#}
version=${version##*@}
version=${version%%[-+]*}

cargo_args=""
if [ "$profile" = "release" ]; then
    cargo_args="--release"
fi
out_subdir="$profile"
if [ "$universal" = 1 ]; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
    cargo build --locked -p tide-app --bin TIDE $cargo_args --target aarch64-apple-darwin
    cargo build --locked -p tide-app --bin TIDE $cargo_args --target x86_64-apple-darwin
    lipo_dir="$target_dir/universal/$profile"
    mkdir -p "$lipo_dir"
    lipo -create \
        "$target_dir/aarch64-apple-darwin/$profile/TIDE" \
        "$target_dir/x86_64-apple-darwin/$profile/TIDE" \
        -output "$lipo_dir/TIDE"
    binary="$lipo_dir/TIDE"
    bundle_parent="$lipo_dir"
else
    cargo build --locked -p tide-app --bin TIDE $cargo_args
    binary="$target_dir/$profile/TIDE"
    bundle_parent="$target_dir/$profile"
fi

# Release / CI zips ship as TIDE.app. Set TIDE_DEV_BUNDLE=1 for a versioned display name.
app_name="$app_display"
if [ "${TIDE_DEV_BUNDLE:-0}" = "1" ]; then
    app_name="TIDE (dev-$version)"
fi

staging=$(mktemp -d "$bundle_parent/.tide-zip.XXXXXX")
trap 'rm -rf "$staging"' EXIT
app="$staging/$app_name.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/TIDE"
cp crates/tide-app/assets/TIDE.icns "$app/Contents/Resources/TIDE.icns"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>TIDE</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.GameTL.tide-tidal-rust</string>
    <key>CFBundleName</key>
    <string>TIDE</string>
    <key>CFBundleDisplayName</key>
    <string>$app_name</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleIconFile</key>
    <string>TIDE.icns</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSAppleEventsUsageDescription</key>
    <string>TIDE asks your browser for the TIDAL login result so you don't have to copy it by hand. Only the TIDAL login address is read.</string>
</dict>
</plist>
EOF

identity="${APPLE_SIGNING_IDENTITY:-}"
notarize=0
if [ -n "$identity" ] && [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
    notarize=1
fi

if [ -n "$identity" ]; then
    codesign --force --timestamp --options runtime \
        --entitlements crates/tide-app/assets/TIDE.entitlements \
        --sign "$identity" "$app" >&2
    codesign --verify --strict --verbose=2 "$app" >&2
else
    codesign --force --sign - "$app" >/dev/null
fi

notarize_file() {
    xcrun notarytool submit "$1" \
        --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD" \
        --wait >&2
}

if [ "$universal" = 1 ]; then
    arch_label=macos-universal
else
    arch_label="macos-$(uname -m)"
fi

if [ "$format" = dmg ]; then
    out_path="$bundle_parent/TIDE-${version}-${arch_label}.dmg"
    rm -f "$out_path"
    # Drag-to-install layout: TIDE.app next to a link to /Applications.
    ln -s /Applications "$staging/Applications"
    hdiutil create -volname "$app_name" -srcfolder "$staging" -fs HFS+ \
        -format UDZO -ov "$out_path" >&2
    if [ -n "$identity" ]; then
        codesign --force --timestamp --sign "$identity" "$out_path" >&2
    fi
    if [ "$notarize" = 1 ]; then
        notarize_file "$out_path"
        xcrun stapler staple "$out_path" >&2
    fi
else
    if [ "$notarize" = 1 ]; then
        # Notarize the app, staple the ticket, then zip the stapled app.
        notary_zip="$staging/notarize.zip"
        ditto -c -k --keepParent "$app" "$notary_zip"
        notarize_file "$notary_zip"
        rm -f "$notary_zip"
        xcrun stapler staple "$app" >&2
    fi
    out_path="$bundle_parent/TIDE-${version}-${arch_label}.zip"
    rm -f "$out_path"
    # ditto preserves .app structure better than zip for Finder
    ditto -c -k --sequesterRsrc --keepParent "$app" "$out_path"
fi
printf '%s\n' "$out_path"
