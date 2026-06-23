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

# --- macOS Developer ID signing + notarization -------------------------------------------
# Fetch one secret from Doppler ci-deploy/prd (isolated HOME, same source as publish.sh).
# Retries on empty: a transient doppler hiccup under build load would otherwise return
# "" (errors are swallowed), and an empty APPLE_CERTIFICATE_PASSWORD silently breaks the
# cert import ("cert import failed (wrong password?)") with no real password problem.
_dopsec() {
  local _v _i
  for _i in 1 2 3 4 5; do
    if [ -n "${DOP_TOKEN:-}" ]; then _v="$(HOME="$DOP_TMPHOME" DOPPLER_TOKEN="$DOP_TOKEN" doppler secrets get "$1" --project ci-deploy --config prd --plain 2>/dev/null)"
    else _v="$(doppler secrets get "$1" --project ci-deploy --config prd --plain 2>/dev/null)"; fi
    [ -n "$_v" ] && { printf '%s' "$_v"; return 0; }
    sleep 2
  done
  return 0
}

# Sign (Developer ID Application + hardened runtime + JIT entitlements), notarize (App Store
# Connect API key) and staple the .app in place. Creds come from the environment, else from
# Doppler ci-deploy/prd when a token is available. With no creds it logs and leaves the app UNSIGNED
# (the build still succeeds, so credential-less dev builds keep working). Runs in a subshell so its
# many fallible `security`/`xcrun` calls never abort the outer `set -e` build.
sign_and_notarize_macos() {
  ( set +e
    local app="$1"

    if [ -z "${APPLE_CERTIFICATE:-}" ]; then
      DOP_TOKEN="${DOPPLER_TOKEN:-}"
      [ -z "$DOP_TOKEN" ] && [ -f /etc/default/jenkins-doppler ] \
        && DOP_TOKEN="$(grep '^DOPPLER_TOKEN_CI_DEPLOY=' /etc/default/jenkins-doppler | cut -d= -f2)"
      if command -v doppler >/dev/null; then   # token from env/file, else the doppler CLI's own login
        DOP_TMPHOME="$(mktemp -d)"
        APPLE_CERTIFICATE="$(_dopsec APPLE_CERTIFICATE)"
        APPLE_CERTIFICATE_PASSWORD="$(_dopsec APPLE_CERTIFICATE_PASSWORD)"
        APPLE_API_KEY_ID="$(_dopsec APPLE_API_KEY_ID)"
        APPLE_API_ISSUER_ID="$(_dopsec APPLE_API_ISSUER_ID)"
        APPLE_API_KEY_P8="$(_dopsec APPLE_API_KEY_P8)"
        rm -rf "$DOP_TMPHOME"
      fi
    fi

    if [ -z "${APPLE_CERTIFICATE:-}" ]; then
      echo "macOS signing: no Apple cert (APPLE_CERTIFICATE) — shipping UNSIGNED; downloads will hit Gatekeeper." >&2
      exit 0
    fi
    local can_notarize=1 v
    for v in APPLE_API_KEY_ID APPLE_API_ISSUER_ID APPLE_API_KEY_P8; do
      [ -n "${!v:-}" ] || { echo "macOS signing: $v missing — will codesign but NOT notarize (downloads still warn)." >&2; can_notarize=0; }
    done

    # Import the Developer ID cert into a throwaway keychain (referenced directly; search list untouched).
    local kcdir kc kpw cert
    kcdir="$(mktemp -d)"; kc="$kcdir/navgator-sign.keychain-db"; kpw="navgator-$$-${RANDOM}"; cert="$(mktemp)"
    printf '%s' "$APPLE_CERTIFICATE" | base64 --decode > "$cert"
    security create-keychain -p "$kpw" "$kc" || { echo "macOS signing: create-keychain failed — unsigned." >&2; exit 0; }
    security set-keychain-settings -lut 21600 "$kc"
    security unlock-keychain -p "$kpw" "$kc"
    # -f pkcs12: security import infers format from the file EXTENSION, and $cert is an
    # extension-less mktemp file — without this it fails "Unknown format in import" even
    # with a perfectly valid .p12 + password. Surface the real security error if it fails.
    if ! _imperr="$(security import "$cert" -f pkcs12 -k "$kc" -P "${APPLE_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign 2>&1)"; then
      echo "macOS signing: cert import failed — ${_imperr} — unsigned." >&2; security delete-keychain "$kc"; rm -f "$cert"; exit 0
    fi
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kpw" "$kc" >/dev/null 2>&1
    rm -f "$cert"

    local identity
    identity="$(security find-identity -v -p codesigning "$kc" | grep -m1 'Developer ID Application' | sed -E 's/^[^"]*"([^"]+)".*/\1/')"
    if [ -z "$identity" ]; then
      echo "macOS signing: cert has no 'Developer ID Application' identity (App Store cert?) — unsigned." >&2
      security delete-keychain "$kc"; exit 0
    fi
    echo "macOS signing: identity = $identity"

    if ! codesign --force --deep --options runtime --timestamp \
        --entitlements packaging/macos-entitlements.plist --sign "$identity" --keychain "$kc" "$app"; then
      echo "macOS signing: codesign FAILED — unsigned." >&2; security delete-keychain "$kc"; exit 0
    fi
    codesign --verify --strict --verbose=2 "$app" || echo "macOS signing: codesign --verify warned (continuing)." >&2

    if [ "$can_notarize" = 1 ]; then
      local zipdir zip p8; zipdir="$(mktemp -d)"; zip="$zipdir/NavGator.zip"
      p8="$(mktemp)"; printf '%s' "$APPLE_API_KEY_P8" | base64 --decode > "$p8"; chmod 600 "$p8"
      ditto -c -k --keepParent "$app" "$zip"
      echo "macOS signing: submitting to notarytool (waits for Apple, up to 30m)…"
      if xcrun notarytool submit "$zip" --key "$p8" --key-id "$APPLE_API_KEY_ID" \
           --issuer "$APPLE_API_ISSUER_ID" --wait --timeout 30m; then
        xcrun stapler staple "$app" && xcrun stapler validate "$app" && echo "macOS signing: notarized + stapled ✓"
      else
        echo "macOS signing: notarization FAILED — signed but not notarized (downloads will warn)." >&2
      fi
      rm -f "$p8"; rm -rf "$zipdir"
    fi
    security delete-keychain "$kc" 2>/dev/null || true
    rm -rf "$kcdir"
  )
  return 0
}
# -----------------------------------------------------------------------------------------

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
    # macOS uses the .icns named by CFBundleIconFile (Info.plist) — a bare PNG is ignored.
    # Fail loudly rather than the old silent `|| true`: an icon-less .app is a release defect
    # (this is exactly how a pre-icon build shipped with no Dock/Finder icon).
    if [ -f packaging/navgator.icns ]; then
        cp packaging/navgator.icns "$APP/Contents/Resources/navgator.icns"
    else
        echo "ERROR: packaging/navgator.icns missing — refusing to ship an icon-less .app" >&2
        exit 1
    fi

    # Developer ID sign + notarize + staple in place (no-op + warning if creds absent), BEFORE
    # packaging so both the .tar.gz and .dmg carry the signed, stapled app.
    sign_and_notarize_macos "$APP"

    tar -C "$DIST" -czf "$DIST/navgator-$VERSION-macos-$ARCH.tar.gz" NavGator.app

    # dmg. create-dmg attaches a scratch volume named 'navgator' and detaches it when done — but
    # if Jenkins SIGKILLs the build mid-package (abortPrevious), the detach never runs and the
    # mount leaks. Accumulated stale mounts jam diskarbitrationd until even mkdirs hangs and the
    # agent wedges. SIGKILL can't be trapped, so reap any stale 'navgator' mount up front; this
    # bounds the leak to at most the one in-flight build.
    for v in /Volumes/navgator*; do
        [ -d "$v" ] && hdiutil detach -force "$v" >/dev/null 2>&1 || true
    done
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
