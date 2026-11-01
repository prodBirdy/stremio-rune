#!/bin/sh
# Release build wrapped as "target/release/Stremio Rune.app": the binary, an icon made from
# assets/icon.ico, an Info.plist, and an ad-hoc signature. Needs Stremio.app installed (its
# libmpv and streaming server); copy the bundle to /Applications to keep it.
set -e
cd "$(dirname "$0")/.."
# Source paths in panic messages read ~/… rather than this machine's home folder.
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=~" cargo build --release

app="target/release/Stremio Rune.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/stremio-rune "$app/Contents/MacOS/stremio-rune"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
sips -s format png assets/icon.ico --out "$work/icon.png" >/dev/null
mkdir "$work/icon.iconset"
for size in 16 32 128 256; do
    sips -z $size $size "$work/icon.png" --out "$work/icon.iconset/icon_${size}x${size}.png" >/dev/null
    half=$((size / 2))
    [ $half -ge 16 ] && cp "$work/icon.iconset/icon_${size}x${size}.png" "$work/icon.iconset/icon_${half}x${half}@2x.png"
done
iconutil -c icns "$work/icon.iconset" -o "$app/Contents/Resources/icon.icns"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Stremio Rune</string>
    <key>CFBundleDisplayName</key><string>Stremio Rune</string>
    <key>CFBundleIdentifier</key><string>dev.prodbirdy.stremio-rune</string>
    <key>CFBundleExecutable</key><string>stremio-rune</string>
    <key>CFBundleIconFile</key><string>icon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.entertainment</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>Stremio Rune looks for TVs on your network to cast to.</string>
</dict>
</plist>
EOF

codesign --force --sign - "$app"
echo "$app"
