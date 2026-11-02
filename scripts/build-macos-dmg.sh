#!/bin/sh
# Builds target/SpotiPod-<version>.dmg with a drag-to-Applications window.
#   sh scripts/build-macos-dmg.sh           reuse dist/SpotiPod.app if its version matches
#   sh scripts/build-macos-dmg.sh --rebuild rebuild the app bundle first
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT_DIR"

if [ "$(uname -s)" != "Darwin" ]; then
    echo "The SpotiPod DMG can only be built on macOS." >&2
    exit 1
fi

# Single source of truth: [workspace.package] version in Cargo.toml.
VERSION=$(cargo metadata --locked --no-deps --format-version 1 |
    python3 -c "import json, sys; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name'] == 'spotipod-gui'))")

PLIST=dist/SpotiPod.app/Contents/Info.plist
APP_VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$PLIST" 2>/dev/null || true)

# Rebuild when asked, missing, or stale.
if [ "${1:-}" = "--rebuild" ] || [ "$APP_VERSION" != "$VERSION" ]; then
    sh scripts/build-macos-app.sh
fi
WORK_DIR="$ROOT_DIR/target/dmg"
VENV="$WORK_DIR/venv"
STAGE="$WORK_DIR/stage"
OUT="$ROOT_DIR/target/SpotiPod-$VERSION.dmg"

mkdir -p "$WORK_DIR"

# Isolated dmgbuild install, reused between runs.
if [ ! -x "$VENV/bin/dmgbuild" ]; then
    python3 -m venv "$VENV"
    "$VENV/bin/pip" install --quiet --upgrade pip
    "$VENV/bin/pip" install --quiet "dmgbuild>=1.6,<2"
fi

# Sign a staged copy so dist/ stays untouched.
rm -rf "$STAGE"
mkdir -p "$STAGE"
ditto dist/SpotiPod.app "$STAGE/SpotiPod.app"
xattr -cr "$STAGE/SpotiPod.app"
codesign --force --deep --sign - "$STAGE/SpotiPod.app"

# Merge 1x and 2x into one HiDPI TIFF for Retina.
tiffutil -cathidpicheck assets/dmg/background.png assets/dmg/background@2x.png \
    -out "$WORK_DIR/background.tiff"

rm -f "$OUT"
"$VENV/bin/dmgbuild" \
    -s assets/dmg/settings.py \
    -D app="$STAGE/SpotiPod.app" \
    -D background="$WORK_DIR/background.tiff" \
    -D volume_icon="$STAGE/SpotiPod.app/Contents/Resources/SpotiPod.icns" \
    "SpotiPod $VERSION" "$OUT"

echo "Built $OUT"
