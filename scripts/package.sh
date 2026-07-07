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

# --- build-stamped app icon --------------------------------------------------
# NavGator is in active development, so every build stamps its {version}-{commit}
# into a badge on the top-right of the app icon (Dock/Finder/task bar tell builds
# apart at a glance). stamp-icon.py emits stamped icons into a temp dir WITHOUT
# touching the committed clean icons; the variables below point the packaging
# steps at the stamped files, falling back to the committed ones if Pillow or the
# script is unavailable (a build never fails over the badge).
BUILD="$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 0)"
LABEL="$VERSION-$BUILD"
STAMPDIR="$(mktemp -d)"
trap 'rm -rf "$STAMPDIR"' EXIT
ICNS_SRC="$ROOT/packaging/navgator.icns"       # defaults: committed clean icons
ICON_BUNDLE="$ROOT/packaging/navgator.icon"
LINUX_PNG="$ROOT/packaging/navgator.png"
if python3 "$ROOT/scripts/stamp-icon.py" --label "$LABEL" \
      --art "$ROOT/packaging/navgator.icon/Assets/gator3.png" \
      --icon-bundle "$ROOT/packaging/navgator.icon" --out-dir "$STAMPDIR"; then
    [ -f "$STAMPDIR/navgator.png" ] && LINUX_PNG="$STAMPDIR/navgator.png"
    [ -d "$STAMPDIR/navgator.icon" ] && ICON_BUNDLE="$STAMPDIR/navgator.icon"
    # macOS: pack the stamped iconset into a .icns (iconutil is macOS-only).
    if command -v iconutil >/dev/null && [ -d "$STAMPDIR/navgator.iconset" ] \
        && iconutil -c icns "$STAMPDIR/navgator.iconset" -o "$STAMPDIR/navgator.icns"; then
        ICNS_SRC="$STAMPDIR/navgator.icns"
    fi
    echo "icon: stamped build badge '$LABEL'"
else
    echo "icon: stamp-icon skipped (Pillow missing?) — shipping clean unstamped icons" >&2
fi
# -----------------------------------------------------------------------------

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

    # Import the Developer ID cert into a keychain in the STANDARD ~/Library/Keychains location
    # and make it the default. A keychain in a temp dir does NOT integrate with codesign's trust
    # evaluation: the chain won't build to the Apple root, `find-identity -v` shows nothing, and
    # codesign dies with the opaque "errSecInternalComponent". Standard location + default fixes
    # all of it (this is what Tauri's signer does). The trap restores the prior default keychain
    # and deletes ours on every exit path.
    local kc kpw cert origdef
    kc="navgator-sign-$$.keychain"; kpw="navgator-$$-${RANDOM}"; cert="$(mktemp)"
    origdef="$(security default-keychain -d user | sed -E 's/[[:space:]]*"//g')"
    trap 'security default-keychain -s "$origdef" 2>/dev/null; security delete-keychain "$kc" 2>/dev/null; rm -f "$cert"' EXIT
    printf '%s' "$APPLE_CERTIFICATE" | base64 --decode > "$cert"
    security create-keychain -p "$kpw" "$kc" || { echo "macOS signing: create-keychain failed — unsigned." >&2; exit 0; }
    security default-keychain -s "$kc"
    security set-keychain-settings -lut 21600 "$kc"
    security unlock-keychain -p "$kpw" "$kc"
    # -f pkcs12: $cert is an extension-less mktemp file; without it security can't infer the
    # PKCS#12 format ("Unknown format in import"). Surface the real error on failure.
    if ! _imperr="$(security import "$cert" -f pkcs12 -k "$kc" -P "${APPLE_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign 2>&1)"; then
      echo "macOS signing: cert import failed — ${_imperr} — unsigned." >&2; exit 0
    fi
    # Let codesign use the key without a GUI prompt (headless agent).
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kpw" "$kc" >/dev/null 2>&1 \
      || echo "macOS signing: set-key-partition-list warning (continuing)." >&2
    rm -f "$cert"

    local identity
    identity="$(security find-identity -v -p codesigning "$kc" | grep -m1 'Developer ID Application' | sed -E 's/^[^"]*"([^"]+)".*/\1/')"
    if [ -z "$identity" ]; then
      echo "macOS signing: no valid 'Developer ID Application' identity — unsigned. Present:" >&2
      security find-identity -p codesigning "$kc" >&2; exit 0
    fi
    echo "macOS signing: identity = $identity"

    # $kc is the default keychain, so codesign resolves the identity without --keychain.
    _cserr="$(codesign --force --deep --options runtime --timestamp \
        --entitlements packaging/macos-entitlements.plist --sign "$identity" "$app" 2>&1)"; _csrc=$?
    if [ "$_csrc" != 0 ]; then
      echo "macOS signing: codesign FAILED — ${_cserr} — unsigned." >&2; exit 0
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
    # keychain + default-keychain restore handled by the EXIT trap above
  )
  return 0
}
# -----------------------------------------------------------------------------------------

# --- Linux packaging helpers -------------------------------------------------------------
# The GStreamer *codec plugins* are dlopen'd at runtime, so dpkg-shlibdeps (which only sees
# LINKED libs) can't infer them — yet they are exactly what a bare AppImage wrongly assumed
# already installed. Declare their packages explicitly on both the .deb Depends and (bundled)
# in the AppImage.
GST_PLUGIN_PKGS="gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-libav, gstreamer1.0-nice"

# Runtime Depends for the .deb: dpkg-shlibdeps maps the binary's linked libraries to their
# packages (libgstreamer1.0-0, libgstreamer-plugins-base1.0-0, libglib2.0-0t64, …); we append
# the dlopen'd plugin packages above. Falls back to a hand-listed core set if shlibdeps is absent.
compute_deb_depends() {
    local bin="$1" shlib="" tmpd
    if command -v dpkg-shlibdeps >/dev/null; then
        tmpd="$(mktemp -d)"; mkdir -p "$tmpd/debian"
        printf 'Source: navgator\nPackage: navgator\nArchitecture: amd64\n' > "$tmpd/debian/control"
        shlib="$( cd "$tmpd" && dpkg-shlibdeps -O --ignore-missing-info "$bin" 2>/dev/null | sed 's/^shlibs:Depends=//' )"
        rm -rf "$tmpd"
    fi
    if [ -n "$shlib" ]; then
        printf '%s, %s' "$shlib" "$GST_PLUGIN_PKGS"
    else
        printf 'libc6, libgstreamer1.0-0, libgstreamer-plugins-base1.0-0, libglib2.0-0t64, %s' "$GST_PLUGIN_PKGS"
    fi
}

# appimagetool is not preinstalled on the build host; fetch the continuous build once into a
# build cache. --appimage-extract-and-run avoids needing FUSE on the (headless) agent.
ensure_appimagetool() {
    if command -v appimagetool >/dev/null; then echo appimagetool; return 0; fi
    local cache="${XDG_CACHE_HOME:-$HOME/.cache}/navgator-build" at
    mkdir -p "$cache"; at="$cache/appimagetool-x86_64.AppImage"
    if [ ! -x "$at" ]; then
        curl -fsSL -o "$at" \
          "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" \
          && chmod +x "$at" || { rm -f "$at"; return 1; }
    fi
    echo "$at"
}
# -----------------------------------------------------------------------------------------

case "$OS" in
Linux)
    # tarball
    T="$DIST/navgator-$VERSION-linux-x86_64"
    mkdir -p "$T"; cp "$BIN" "$T/navgator"; stage_resources "$T"
    tar -C "$DIST" -czf "$DIST/navgator-$VERSION-linux-x86_64.tar.gz" "$(basename "$T")"
    rm -rf "$T"

    # .deb — declares the GStreamer runtime deps (via Depends) so `apt install ./navgator.deb`
    # pulls the plugin packages a bare download can't assume are present. The binary is
    # self-contained (gator:// pages, pdf.js and fonts are compiled in via include_str!/
    # include_bytes!), so no resources dir is needed alongside it.
    if command -v dpkg-deb >/dev/null; then
        DEB="navgator-$VERSION-linux-x86_64.deb"
        DEBROOT="$DIST/navgator-deb"; rm -rf "$DEBROOT"
        mkdir -p "$DEBROOT/DEBIAN" "$DEBROOT/usr/bin" \
                 "$DEBROOT/usr/share/applications" \
                 "$DEBROOT/usr/share/icons/hicolor/256x256/apps"
        cp "$BIN" "$DEBROOT/usr/bin/navgator"; chmod 755 "$DEBROOT/usr/bin/navgator"
        cp packaging/navgator.desktop "$DEBROOT/usr/share/applications/navgator.desktop"
        cp "$LINUX_PNG" "$DEBROOT/usr/share/icons/hicolor/256x256/apps/navgator.png" 2>/dev/null || true
        DEPENDS="$(compute_deb_depends "$DEBROOT/usr/bin/navgator")"
        ISIZE="$(du -ks "$DEBROOT/usr" | cut -f1)"
        cat > "$DEBROOT/DEBIAN/control" <<EOF
Package: navgator
Version: $VERSION
Architecture: amd64
Maintainer: Lyku <apps@lyku.org>
Installed-Size: $ISIZE
Depends: $DEPENDS
Section: web
Priority: optional
Homepage: https://lyku.org/apps/NavGator
Description: NavGator - a fast, private web browser
 A native chrome compositing the Servo engine. Media (<video>/<audio>)
 decodes via GStreamer, whose runtime plugin packages are pulled in as
 dependencies so playback works out of the box.
EOF
        # postinst: refresh the desktop icon + .desktop caches so the launcher shows THIS
        # build's icon (each preview build stamps its version into the icon) instead of a
        # stale cached one.
        cat > "$DEBROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
fi
exit 0
EOF
        chmod 755 "$DEBROOT/DEBIAN/postinst"
        if dpkg-deb --root-owner-group --build "$DEBROOT" "$DIST/$DEB" >/dev/null; then
            echo "deb: $DEB"
            echo "     Depends: $DEPENDS"
        else
            echo "dpkg-deb failed; skipping .deb (tarball still published)" >&2
        fi
        rm -rf "$DEBROOT"
    else
        echo "dpkg-deb not found; skipping .deb"
    fi

    # AppImage — SELF-CONTAINED: bundle GStreamer plugins + every non-host shared lib into
    # usr/lib so it launches on machines with no gstreamer1.0-plugins-* installed (the
    # reported "AppImage won't start" bug). Needs appimagetool (fetched if absent).
    APPIMAGE="navgator-$VERSION-linux-x86_64.AppImage"
    ATOOL="$(ensure_appimagetool || true)"
    if [ -n "$ATOOL" ]; then
        APPDIR="$DIST/navgator.AppDir"; rm -rf "$APPDIR"
        mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" \
                 "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
        cp "$BIN" "$APPDIR/usr/bin/navgator"; stage_resources "$APPDIR/usr/bin"
        # AppImage spec wants the .desktop + icon at the AppDir root; keep the FHS copies too.
        cp packaging/navgator.desktop "$APPDIR/usr/share/applications/"
        cp "$LINUX_PNG" "$APPDIR/usr/share/icons/hicolor/256x256/apps/navgator.png" 2>/dev/null || true
        cp packaging/navgator.desktop "$APPDIR/"
        cp "$LINUX_PNG" "$APPDIR/navgator.png" 2>/dev/null || true
        cp "$LINUX_PNG" "$APPDIR/.DirIcon" 2>/dev/null || true
        cp packaging/AppRun "$APPDIR/AppRun"; chmod +x "$APPDIR/AppRun"
        # Bundle GStreamer + non-host deps into usr/lib (AppRun points LD_LIBRARY_PATH +
        # GST_PLUGIN_SYSTEM_PATH at it). GL/GPU/X11 driver libs stay on the host.
        GST_PLUGINDIR="$(pkg-config --variable=pluginsdir gstreamer-1.0 2>/dev/null || true)"
        [ -d "$GST_PLUGINDIR" ] || GST_PLUGINDIR="/usr/lib/x86_64-linux-gnu/gstreamer-1.0"
        GST_SCANDIR="$(pkg-config --variable=pluginscannerdir gstreamer-1.0 2>/dev/null || true)"
        GST_SCANNER="$GST_SCANDIR/gst-plugin-scanner"
        [ -x "$GST_SCANNER" ] || GST_SCANNER="/usr/lib/x86_64-linux-gnu/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner"
        PLUGLISTS="$(ls -d "$HOME"/.cargo/git/checkouts/swervo-*/*/components/servo/gstreamer_plugin_lists 2>/dev/null | head -1)"
        if [ -f scripts/linux-bundle-gst.py ]; then
            python3 scripts/linux-bundle-gst.py --binary "$APPDIR/usr/bin/navgator" \
                --lib-dir "$APPDIR/usr/lib" --gst-plugin-dir "$GST_PLUGINDIR" \
                --scanner "$GST_SCANNER" --plugin-lists "${PLUGLISTS:-}" \
                || echo "Linux: GStreamer bundling failed — the AppImage may need host GStreamer." >&2
        else
            echo "Linux: scripts/linux-bundle-gst.py missing — AppImage NOT self-contained." >&2
        fi
        ( cd "$DIST" && APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$ATOOL" navgator.AppDir "$APPIMAGE" ) \
            || echo "appimagetool failed; skipping AppImage (tarball + .deb still published)" >&2
        rm -rf "$APPDIR"
        [ -f "$DIST/$APPIMAGE" ] && chmod +x "$DIST/$APPIMAGE" || true
    else
        echo "appimagetool unavailable (not installed, no network to fetch); skipping AppImage"
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
    if [ -f "$ICNS_SRC" ]; then
        cp "$ICNS_SRC" "$APP/Contents/Resources/navgator.icns"
    else
        echo "ERROR: $ICNS_SRC missing — refusing to ship an icon-less .app" >&2
        exit 1
    fi

    # macOS 26 (Tahoe) ignores the raster .icns and reads CFBundleIconName -> an
    # AppIcon in a compiled Assets.car. Build it from the Icon Composer .icon with
    # actool (FULL XCODE only — Command Line Tools have no actool). Where actool is
    # absent (e.g. a CLT-only dev box) we skip this and Tahoe falls back to the
    # legacy .icns on its grey "squircle jail" plate. Must run BEFORE signing so the
    # Assets.car + Info.plist change get sealed by codesign.
    ACTOOL="$(xcrun --find actool 2>/dev/null || true)"
    if [ -n "$ACTOOL" ] && [ -d "$ICON_BUNDLE" ]; then
        CARDIR="$(mktemp -d)"
        if "$ACTOOL" "$ICON_BUNDLE" --compile "$CARDIR" --app-icon navgator \
              --enable-on-demand-resources NO --development-region en \
              --target-device mac --platform macosx \
              --enable-icon-stack-fallback-generation=disabled --include-all-app-icons \
              --minimum-deployment-target 10.14 --output-partial-info-plist /dev/null >/dev/null 2>&1 \
           && [ -f "$CARDIR/Assets.car" ]; then
            cp "$CARDIR/Assets.car" "$APP/Contents/Resources/Assets.car"
            plutil -replace CFBundleIconName -string navgator "$APP/Contents/Info.plist"
            echo "macOS icon: Assets.car compiled (Tahoe squircle) + navgator.icns (pre-Tahoe) ✓"
        else
            echo "macOS icon: actool failed — Tahoe will show the legacy plate (.icns only)." >&2
        fi
        rm -rf "$CARDIR"
    else
        echo "macOS icon: no actool (needs full Xcode) — Tahoe will show the legacy plate (.icns only)." >&2
    fi

    # Make the .app self-contained: copy GStreamer + every non-system dylib the engine links
    # (plus the curated plugins libservo loads from <exe>/lib at runtime) into Contents/MacOS/lib
    # with @executable_path/lib install names. Without this the download crashes at launch on any
    # machine lacking Homebrew GStreamer ("Library not loaded: …/libgstplay-1.0.0.dylib"). Must
    # run BEFORE signing so codesign --deep seals the bundled dylibs.
    if command -v otool >/dev/null 2>&1 && [ -f scripts/macos-bundle-gst.py ]; then
        GSTLIBS="/opt/homebrew/lib"; [ -d "$GSTLIBS/gstreamer-1.0" ] || GSTLIBS="$(brew --prefix 2>/dev/null)/lib"
        PLUGLISTS="$(ls -d "$HOME"/.cargo/git/checkouts/swervo-*/*/components/servo/gstreamer_plugin_lists 2>/dev/null | head -1)"
        python3 scripts/macos-bundle-gst.py --binary "$APP/Contents/MacOS/navgator" \
            --lib-dir "$APP/Contents/MacOS/lib" --gst-libs "$GSTLIBS" --plugin-lists "${PLUGLISTS:-}" \
            || echo "macOS: GStreamer dylib bundling failed — the downloaded app may crash on launch." >&2
    else
        echo "macOS: skipping GStreamer bundling (otool/bundler missing) — app may need Homebrew." >&2
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
