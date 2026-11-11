#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    echo "TIDE app bundles must be built on macOS." >&2
    exit 1
fi

profile=${1:-release}
case "$profile" in
    release) set -- --release ;;
    debug) set -- ;;
    *) echo "Usage: $0 [release|debug]" >&2; exit 2 ;;
esac

cd "$(dirname "$0")/.."
metadata=$(cargo metadata --locked --no-deps --format-version 1)
target_dir=$(printf '%s\n' "$metadata" | plutil -extract target_directory raw -o - -)
package_id=$(cargo pkgid --locked -p tide-app)
version=${package_id##*#}
version=${version##*@}
version=${version%%[-+]*}

cargo build --locked -p tide-app --bin TIDE "$@"
output_dir="$target_dir/$profile"
bundle="$output_dir/TIDE.app"
staging=$(mktemp -d "$output_dir/.tide-bundle.XXXXXX")
trap 'rm -rf "$staging"' EXIT

app="$staging/TIDE.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$output_dir/TIDE" "$app/Contents/MacOS/TIDE"
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

codesign --force --sign - "$app"
rm -rf "$bundle"
mv "$app" "$bundle"
printf '%s\n' "$bundle"
