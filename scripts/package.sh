#!/usr/bin/env bash
# Package navgator for distribution into dist/.
#   linux : navgator-<ver>-linux-x86_64.tar.gz  +  navgator-<ver>-x86_64.AppImage
#   macos : navgator-<ver>-macos-<arch>.tar.gz  +  navgator-<ver>-macos-<arch>.dmg
#
# The web assets (chrome/content) ship in resources/ NEXT TO the binary; navgator resolves
# them via current_exe() (crates/navgator/src/main.rs::resources_dir), so a distributed
# binary finds its UI. AppImage/dmg formats are produced only if their tools are present.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep '^version' crates/navgator/Cargo.toml | head -1 | sed 's/.*"\([^"]*\)".*/\1/')"
OS="$(uname -s)"
ARCH="$(uname -m)"; [ "$ARCH" = "arm64" ] && ARCH="aarch64"
DIST="$ROOT/dist"
rm -rf "$DIST"; mkdir -p "$DIST"

echo "=== building release (navgator $VERSION) ==="
cargo build --release -p navgator --locked
BIN="$ROOT/target/release/navgator"
[ -f "$BIN" ] || { echo "release binary not found at $BIN"; exit 1; }

# Copy the web assets into <dir>/resources/{chrome,content}.
stage_resources() {
    mkdir -p "$1/resources"
    cp -R crates/navgator/src/chrome "$1/resources/chrome"
    cp -R crates/navgator/src/content "$1/resources/content"
}

case "$OS" in
Linux)
    # tarball
    T="$DIST/navgator-$VERSION-linux-x86_64"
    mkdir -p "$T"; cp "$BIN" "$T/navgator"; stage_resources "$T"
    tar -C "$DIST" -czf "$DIST/navgator-$VERSION-linux-x86_64.tar.gz" "$(basename "$T")"
    rm -rf "$T"

    # AppImage
    if command -v appimagetool >/dev/null; then
        APPDIR="$DIST/navgator.AppDir"
        mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
        cp "$BIN" "$APPDIR/usr/bin/navgator"; stage_resources "$APPDIR/usr/bin"
        cp packaging/navgator.desktop "$APPDIR/usr/share/applications/"
        cp packaging/navgator.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/" 2>/dev/null || true
        cp packaging/navgator.desktop "$APPDIR/"
        cp packaging/navgator.png "$APPDIR/" 2>/dev/null || true
        ln -sf usr/bin/navgator "$APPDIR/AppRun"
        ( cd "$DIST" && ARCH=x86_64 appimagetool "$APPDIR" "navgator-$VERSION-x86_64.AppImage" )
        rm -rf "$APPDIR"
    else
        echo "appimagetool not found; skipping AppImage"
    fi
    ;;
Darwin)
    # .app bundle (binary + resources next to it so resources_dir() resolves)
    APP="$DIST/NavGator.app"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    cp "$BIN" "$APP/Contents/MacOS/navgator"; stage_resources "$APP/Contents/MacOS"
    sed "s/0\.1\.0/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"
    cp packaging/navgator.png "$APP/Contents/Resources/" 2>/dev/null || true
    tar -C "$DIST" -czf "$DIST/navgator-$VERSION-macos-$ARCH.tar.gz" NavGator.app

    # dmg
    if command -v create-dmg >/dev/null; then
        create-dmg --volname navgator --skip-jenkins "$DIST/navgator-$VERSION-macos-$ARCH.dmg" "$APP" \
            || hdiutil create -volname navgator -srcfolder "$APP" -ov -format UDZO "$DIST/navgator-$VERSION-macos-$ARCH.dmg"
    else
        hdiutil create -volname navgator -srcfolder "$APP" -ov -format UDZO "$DIST/navgator-$VERSION-macos-$ARCH.dmg"
    fi
    rm -rf "$APP"
    ;;
*)
    echo "unsupported OS: $OS"; exit 1;;
esac

echo "=== dist/ ==="
ls -lh "$DIST"
