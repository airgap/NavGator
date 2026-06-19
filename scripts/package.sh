#!/usr/bin/env bash
# Package navgator for distribution into dist/.
#   linux : navgator-<ver>-linux-x86_64.tar.gz  +  navgator-<ver>-linux-x86_64.AppImage
#   macos : navgator-<ver>-macos-<arch>.tar.gz  +  navgator-<ver>-macos-<arch>.dmg
#
# The web content pages (home/about) ship in resources/content/ NEXT TO the binary; navgator
# resolves them via current_exe() (crates/navgator/src/main.rs::resources_dir). The chrome is
# native (egui), not web assets. AppImage/dmg formats are produced only if their tools exist.
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

# Copy the web content pages into <dir>/resources/content.
stage_resources() {
    mkdir -p "$1/resources"
    cp -R crates/navgator/src/content "$1/resources/content"
}

case "$OS" in
Linux)
    # tarball
    T="$DIST/navgator-$VERSION-linux-x86_64"
    mkdir -p "$T"; cp "$BIN" "$T/navgator"; stage_resources "$T"
    tar -C "$DIST" -czf "$DIST/navgator-$VERSION-linux-x86_64.tar.gz" "$(basename "$T")"
    rm -rf "$T"

    # AppImage (optional; needs appimagetool OR linuxdeploy on PATH — degrades to a
    # clean skip when neither is present, like the dmg/sccache steps elsewhere).
    APPIMAGE="navgator-$VERSION-linux-x86_64.AppImage"
    if command -v appimagetool >/dev/null || command -v linuxdeploy >/dev/null; then
        APPDIR="$DIST/navgator.AppDir"
        rm -rf "$APPDIR"
        mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
        cp "$BIN" "$APPDIR/usr/bin/navgator"; stage_resources "$APPDIR/usr/bin"
        # AppImage spec wants the .desktop + icon at the AppDir root; keep the
        # FHS copies too so the integrated/installed form is well-formed.
        cp packaging/navgator.desktop "$APPDIR/usr/share/applications/"
        cp packaging/navgator.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/" 2>/dev/null || true
        cp packaging/navgator.desktop "$APPDIR/"
        cp packaging/navgator.png "$APPDIR/" 2>/dev/null || true
        cp packaging/navgator.png "$APPDIR/.DirIcon" 2>/dev/null || true
        # Prefer the committed AppRun launcher (exec-forwards args, keeps resources
        # resolvable via current_exe); fall back to a symlink if it is missing.
        if [ -f packaging/AppRun ]; then
            cp packaging/AppRun "$APPDIR/AppRun"; chmod +x "$APPDIR/AppRun"
        else
            ln -sf usr/bin/navgator "$APPDIR/AppRun"
        fi
        if command -v appimagetool >/dev/null; then
            ( cd "$DIST" && ARCH=x86_64 appimagetool "$APPDIR" "$APPIMAGE" ) \
                || echo "appimagetool failed; skipping AppImage (tarball still published)" >&2
        else
            ( cd "$DIST" && linuxdeploy --appdir "$APPDIR" --output appimage \
                && mv navgator*.AppImage "$APPIMAGE" 2>/dev/null || true )
        fi
        rm -rf "$APPDIR"
        [ -f "$DIST/$APPIMAGE" ] && chmod +x "$DIST/$APPIMAGE" || true
    else
        echo "appimagetool/linuxdeploy not found; skipping AppImage"
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
