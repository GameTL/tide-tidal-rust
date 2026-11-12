#!/bin/sh
# Build TIDE.app (optionally universal) and zip it for distribution.
set -eu

profile=release
universal=0
app_display=TIDE

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
        --name)
            shift
            app_display="$1"
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [release|debug] [--universal] [--name NAME]" >&2
            exit 0
            ;;
        *)
            echo "Usage: $0 [release|debug] [--universal] [--name NAME]" >&2
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

codesign --force --sign - "$app" >/dev/null

arch_label=macos
if [ "$universal" = 1 ]; then
    arch_label=macos-universal
else
    arch_label="macos-$(uname -m)"
fi
zip_name="TIDE-${version}-${arch_label}.zip"
zip_path="$bundle_parent/$zip_name"
rm -f "$zip_path"
# ditto preserves .app structure better than zip for Finder
ditto -c -k --sequesterRsrc --keepParent "$app" "$zip_path"
printf '%s\n' "$zip_path"
