#!/usr/bin/env bash
# Builds Multipak-<version>-x86_64.AppImage from a release binary.
#
# Usage: packaging/appimage/build-appimage.sh <path/to/multipak> <version> [output-dir]
#
# The AppImage bundles only the Multipak binary. Graphics, Wayland/X11 and xkbcommon
# libraries are loaded from the host at runtime, exactly like the plain binary, so no
# libraries are bundled that could clash with the host's drivers.
set -euo pipefail

BIN=${1:?path to the multipak binary}
VERSION=${2:?version, e.g. 0.1.0}
OUT=${3:-.}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
APPIMAGETOOL_VERSION=1.9.0

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
APPDIR="$WORK/Multipak.AppDir"

install -Dm755 "$BIN" "$APPDIR/usr/bin/multipak"
install -Dm644 "$ROOT/packaging/multipak.desktop" "$APPDIR/usr/share/applications/multipak.desktop"
install -Dm644 "$ROOT/packaging/multipak.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/multipak.svg"
install -Dm644 "$ROOT/crates/multipak-gui/assets/multipak-256.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/multipak.png"
install -Dm644 "$ROOT/packaging/io.github.minasaad47.Multipak.metainfo.xml" "$APPDIR/usr/share/metainfo/io.github.minasaad47.Multipak.metainfo.xml"
cp "$ROOT/packaging/multipak.desktop" "$APPDIR/multipak.desktop"
cp "$ROOT/packaging/multipak.svg" "$APPDIR/multipak.svg"
cp "$ROOT/crates/multipak-gui/assets/multipak-256.png" "$APPDIR/.DirIcon"

cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
exec "$HERE/usr/bin/multipak" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

TOOL=${APPIMAGETOOL:-}
if [ -z "$TOOL" ]; then
    TOOL="$WORK/appimagetool"
    curl -fsSL -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
    chmod 755 "$TOOL"
fi

mkdir -p "$OUT"
# Extract-and-run avoids needing FUSE on CI machines.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 VERSION="$VERSION" \
    "$TOOL" --no-appstream "$APPDIR" "$OUT/Multipak-${VERSION}-x86_64.AppImage"
echo "Built $OUT/Multipak-${VERSION}-x86_64.AppImage"
