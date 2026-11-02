#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT_DIR"

if [ "$(uname -s)" != "Darwin" ]; then
    echo "The SpotiPod app bundle can only be built on macOS." >&2
    exit 1
fi

cargo build --locked --release -p spotipod-gui

VERSION=$(cargo metadata --locked --no-deps --format-version 1 |
    python3 -c "import json, sys; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name'] == 'spotipod-gui'))")

mkdir -p dist
BUILD_DIR=$(mktemp -d "$ROOT_DIR/dist/.spotipod-build.XXXXXX")
trap 'rm -rf "$BUILD_DIR"' EXIT

APP="$BUILD_DIR/SpotiPod.app"
ICONSET="$BUILD_DIR/SpotiPod.iconset"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$ICONSET"

cp target/release/spotipod-gui "$APP/Contents/MacOS/spotipod-gui"
chmod 755 "$APP/Contents/MacOS/spotipod-gui"

cp assets/icon/mac-16.png "$ICONSET/icon_16x16.png"
cp assets/icon/mac-32.png "$ICONSET/icon_16x16@2x.png"
cp assets/icon/mac-32.png "$ICONSET/icon_32x32.png"
cp assets/icon/mac-64.png "$ICONSET/icon_32x32@2x.png"
cp assets/icon/mac-128.png "$ICONSET/icon_128x128.png"
cp assets/icon/mac-256.png "$ICONSET/icon_128x128@2x.png"
cp assets/icon/mac-256.png "$ICONSET/icon_256x256.png"
cp assets/icon/mac-512.png "$ICONSET/icon_256x256@2x.png"
cp assets/icon/mac-512.png "$ICONSET/icon_512x512.png"
cp assets/icon/mac-1024.png "$ICONSET/icon_512x512@2x.png"
iconutil --convert icns --output "$APP/Contents/Resources/SpotiPod.icns" "$ICONSET"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>spotipod-gui</string>
  <key>CFBundleIconFile</key>
  <string>SpotiPod</string>
  <key>CFBundleIdentifier</key>
  <string>org.spotipod.Spotipod</string>
  <key>CFBundleName</key>
  <string>SpotiPod</string>
  <key>CFBundleDisplayName</key>
  <string>SpotiPod</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${VERSION}</string>
  <key>CFBundleVersion</key>
  <string>${VERSION}</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.music</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist"

rm -rf dist/SpotiPod.app
mv "$APP" dist/SpotiPod.app
echo "Built dist/SpotiPod.app"
