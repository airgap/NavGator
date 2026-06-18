#!/usr/bin/env bash
# Package swerve for distribution into dist/.
#   linux : swerve-<ver>-linux-x86_64.tar.gz  +  swerve-<ver>-x86_64.AppImage
#   macos : swerve-<ver>-macos-<arch>.tar.gz  +  swerve-<ver>-macos-<arch>.dmg
#
# The web assets (chrome/content) ship in resources/ NEXT TO the binary; swerve resolves
# them via current_exe() (crates/swerve/src/main.rs::resources_dir), so a distributed
# binary finds its UI. AppImage/dmg formats are produced only if their tools are present.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep '^version' crates/swerve/Cargo.toml | head -1 | sed 's/.*"\([^"]*\)".*/\1/')"
OS="$(uname -s)"
ARCH="$(uname -m)"; [ "$ARCH" = "arm64" ] && ARCH="aarch64"
DIST="$ROOT/dist"
rm -rf "$DIST"; mkdir -p "$DIST"

echo "=== building release (swerve $VERSION) ==="
cargo build --release -p swerve --locked
BIN="$ROOT/target/release/swerve"
[ -f "$BIN" ] || { echo "release binary not found at $BIN"; exit 1; }

# Copy the web assets into <dir>/resources/{chrome,content}.
stage_resources() {
    mkdir -p "$1/resources"
    cp -R crates/swerve/src/chrome "$1/resources/chrome"
    cp -R crates/swerve/src/content "$1/resources/content"
}

case "$OS" in
Linux)
    # tarball
    T="$DIST/swerve-$VERSION-linux-x86_64"
    mkdir -p "$T"; cp "$BIN" "$T/swerve"; stage_resources "$T"
    tar -C "$DIST" -czf "$DIST/swerve-$VERSION-linux-x86_64.tar.gz" "$(basename "$T")"
    rm -rf "$T"

    # AppImage
    if command -v appimagetool >/dev/null; then
        APPDIR="$DIST/swerve.AppDir"
        mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
        cp "$BIN" "$APPDIR/usr/bin/swerve"; stage_resources "$APPDIR/usr/bin"
        cp packaging/swerve.desktop "$APPDIR/usr/share/applications/"
        cp packaging/swerve.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/" 2>/dev/null || true
        cp packaging/swerve.desktop "$APPDIR/"
        cp packaging/swerve.png "$APPDIR/" 2>/dev/null || true
        ln -sf usr/bin/swerve "$APPDIR/AppRun"
        ( cd "$DIST" && ARCH=x86_64 appimagetool "$APPDIR" "swerve-$VERSION-x86_64.AppImage" )
        rm -rf "$APPDIR"
    else
        echo "appimagetool not found; skipping AppImage"
    fi
    ;;
Darwin)
    # .app bundle (binary + resources next to it so resources_dir() resolves)
    APP="$DIST/Swerve.app"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    cp "$BIN" "$APP/Contents/MacOS/swerve"; stage_resources "$APP/Contents/MacOS"
    sed "s/0\.1\.0/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"
    cp packaging/swerve.png "$APP/Contents/Resources/" 2>/dev/null || true
    tar -C "$DIST" -czf "$DIST/swerve-$VERSION-macos-$ARCH.tar.gz" Swerve.app

    # dmg
    if command -v create-dmg >/dev/null; then
        create-dmg --volname swerve --skip-jenkins "$DIST/swerve-$VERSION-macos-$ARCH.dmg" "$APP" \
            || hdiutil create -volname swerve -srcfolder "$APP" -ov -format UDZO "$DIST/swerve-$VERSION-macos-$ARCH.dmg"
    else
        hdiutil create -volname swerve -srcfolder "$APP" -ov -format UDZO "$DIST/swerve-$VERSION-macos-$ARCH.dmg"
    fi
    rm -rf "$APP"
    ;;
*)
    echo "unsupported OS: $OS"; exit 1;;
esac

echo "=== dist/ ==="
ls -lh "$DIST"
