#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    echo "TIDE app bundles must be built on macOS." >&2
    exit 1
fi

profile=release
install_dir=""
remove_previous=0
while [ $# -gt 0 ]; do
    case "$1" in
        release|debug)
            profile="$1"
            shift
            ;;
        --install)
            shift
            if [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; then
                install_dir="$1"
                shift
            else
                install_dir="/Applications"
            fi
            ;;
        -r|--remove-previous)
            remove_previous=1
            shift
            ;;
        *)
            echo "Usage: $0 [release|debug] [--install [DEST_DIR]] [-r|--remove-previous]" >&2
            exit 2
            ;;
    esac
done

if [ "$remove_previous" = 1 ] && [ -z "$install_dir" ]; then
    install_dir="/Applications"
fi

cargo_args=""
if [ "$profile" = "release" ]; then
    cargo_args="--release"
fi

cd "$(dirname "$0")/.."
metadata=$(cargo metadata --locked --no-deps --format-version 1)
target_dir=$(printf '%s\n' "$metadata" | plutil -extract target_directory raw -o - -)
package_id=$(cargo pkgid --locked -p tide-app)
version=${package_id##*#}
version=${version##*@}
version=${version%%[-+]*}

app_name="TIDE (dev-$version)"

if [ -n "$cargo_args" ]; then
    cargo build --locked -p tide-app --bin TIDE $cargo_args
else
    cargo build --locked -p tide-app --bin TIDE
fi
output_dir="$target_dir/$profile"
bundle="$output_dir/$app_name.app"
staging=$(mktemp -d "$output_dir/.tide-bundle.XXXXXX")
trap 'rm -rf "$staging"' EXIT

app="$staging/$app_name.app"
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

codesign --force --sign - "$app"
rm -rf "$bundle"
mv "$app" "$bundle"

rm -rf "$output_dir/TIDE.app"
ln -sf "$app_name.app" "$output_dir/TIDE.app"

printf '%s\n' "$bundle"

if [ -n "$install_dir" ]; then
    mkdir -p "$install_dir"
    installed_app="$install_dir/$app_name.app"
    if [ "$remove_previous" = 1 ]; then
        for old_app in "$install_dir"/TIDE\ \(dev-*.app "$install_dir"/TIDE\(dev-*.app "$install_dir"/TIDE.app; do
            if [ -e "$old_app" ] && [ "$old_app" != "$installed_app" ]; then
                printf 'Removing previous version: %s\n' "$old_app"
                rm -rf "$old_app"
            fi
        done
    fi
    rm -rf "$installed_app"
    cp -R "$bundle" "$installed_app"
    touch "$installed_app"
    if [ -x /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister ]; then
        /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$installed_app" 2>/dev/null || true
    fi
    printf 'Installed: %s\n' "$installed_app"
fi
