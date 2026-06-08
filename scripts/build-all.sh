#!/usr/bin/env bash
# Build release binaries for Linux and Windows, then package as .sdPlugin zip.
# Usage: docker run --rm -v "${PWD}:/workspace" -w /workspace opendeck-devcontainer bash scripts/build-all.sh
set -euo pipefail

PLUGIN_UUID="com.vibecodedbymrwyss.plugins.displaypad"
PLUGIN_DIR_NAME="${PLUGIN_UUID}.sdPlugin"
DIST="dist"
PKG="${DIST}/${PLUGIN_DIR_NAME}"

rm -rf "$DIST"
mkdir -p "$PKG"

echo "=== Building Linux release ==="
cargo build --workspace --release
cp target/release/adapter "$PKG/opendeck-displaypad-linux"

echo "=== Building Windows release (cross-compile) ==="
cargo build --workspace --release --target x86_64-pc-windows-gnu
cp target/x86_64-pc-windows-gnu/release/adapter.exe "$PKG/opendeck-displaypad-win.exe"

# Copy plugin metadata and assets
cp manifest.json "$PKG/"
if [ -d assets ]; then
  cp -r assets "$PKG/assets"
fi

# Create zip
cd "$DIST"
zip -r "${PLUGIN_DIR_NAME}.zip" "$PLUGIN_DIR_NAME"
cd ..

echo ""
echo "=== Package complete ==="
ls -lh "${DIST}/${PLUGIN_DIR_NAME}.zip"
echo ""
echo "Contents:"
unzip -l "${DIST}/${PLUGIN_DIR_NAME}.zip"
