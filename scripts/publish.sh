#!/usr/bin/env bash
# Upload dist/ artifacts to R2 and register them with lyku.org/apps.
#
# Runs ONCE on the linux Jenkins agent (the only one with the Doppler token at
# /etc/default/jenkins-doppler). The matrix cells stash their dist/ per platform and the
# Publish stage unstashes everything here, so this single invocation publishes both the
# linux and macOS artifacts — mirroring lyku's desktop job (mac builds, linux publishes).
# Versioning matches lyku/jenkins/Jenkinsfile.zoid-desktop: v<ver> / dev-<sha> / latest.
# No-ops gracefully if creds are unavailable.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep '^version' crates/navgator/Cargo.toml | head -1 | sed 's/.*"\([^"]*\)".*/\1/')"
SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
DIST="$ROOT/dist"
[ -d "$DIST" ] || { echo "no dist/ to publish"; exit 0; }

# parabun provides `parabun x wrangler`, as lyku/jenkins/build-desktop.sh does.
export PATH="$HOME/.parabun/bin:$PATH"
command -v parabun >/dev/null || {
    curl -fsSL https://raw.githubusercontent.com/airgap/parabun/main/install.sh | bash -s parabun-68d367fda3 || true
}

# Doppler ci-deploy/prd creds — piggyback on lyku's CI creds on this Jenkins host.
if [ -f /etc/default/jenkins-doppler ]; then
    export DOPPLER_TOKEN="$(grep '^DOPPLER_TOKEN_CI_DEPLOY=' /etc/default/jenkins-doppler | cut -d= -f2)"
fi
if ! command -v doppler >/dev/null || [ -z "${DOPPLER_TOKEN:-}" ]; then
    echo "doppler/creds unavailable — skipping R2 publish (artifacts archived in Jenkins)"; exit 0
fi
TH="$(mktemp -d)"
export CLOUDFLARE_ACCOUNT_ID="$(HOME=$TH doppler secrets get CLOUDFLARE_ACCOUNT_ID --project ci-deploy --config prd --plain)"
export CLOUDFLARE_API_TOKEN="$(HOME=$TH doppler secrets get CLOUDFLARE_API_TOKEN --project ci-deploy --config prd --plain)"
R2_BUCKET="$(HOME=$TH doppler secrets get R2_BUCKET --project ci-deploy --config prd --plain)"
export CI_RELEASE_TOKEN="$(HOME=$TH doppler secrets get CI_RELEASE_TOKEN --project ci-deploy --config prd --plain 2>/dev/null || true)"
rm -rf "$TH"
export REGISTER_URL="${REGISTER_URL:-https://api.lyku.org/register-app-release}"

# wrangler@3 supports Node 18 (the agent's node); wrangler@4 needs Node 20+.
# In v3 `r2 object put` writes to the real bucket by default (no `--remote` flag; the
# opt-in for local is `--local`), unlike v4 which needs `--remote`.
wr() { parabun x wrangler@3 "$@"; }

published=0
for f in "$DIST"/navgator-*.tar.gz "$DIST"/navgator-*.AppImage "$DIST"/navgator-*.dmg "$DIST"/navgator-*.apk "$DIST"/navgator-*.aab; do
    [ -f "$f" ] || continue
    name="$(basename "$f")"
    size="$(stat -c%s "$f" 2>/dev/null || stat -f%z "$f")"
    case "$name" in
        *-linux-*) plat=linux ;;
        *-macos-*) plat=macos ;;
        *-android-*) plat=android ;;
        *) plat=linux ;;
    esac
    for chan in "v$VERSION" "dev-$SHA" "latest"; do
        wr r2 object put "$R2_BUCKET/navgator/$chan/$name" \
            --file "$f" --content-type application/octet-stream
    done
    # Register the versioned release so it surfaces at lyku.org/apps.
    python3 scripts/register-release.py navgator "$plat" "$VERSION" "navgator/v$VERSION/$name" "$size" "$SHA" \
        || echo "WARN: registerAppRelease failed for $name (is CI_RELEASE_TOKEN set in Doppler?)"
    echo "✓ published $name ($plat)"
    published=$((published + 1))
done

# --- update manifest (LYK-1495 / LYK-1498) -----------------------------------
# A tiny JSON the running app polls (default DEFAULT_UPDATE_URL =
# dl.lyku.org/navgator/latest/latest.json) to detect a newer build. `version` is
# <cargo ver>-<commit count> — the SAME string build.rs bakes into the binary
# (build_version()) and scripts/package.sh stamps on the app-icon badge — so every
# build is announced even without a semver bump, which is what early dev wants.
# Uploaded to each channel next to the artifacts; the app defaults to `latest`.
BUILD="$(git rev-list --count HEAD 2>/dev/null || echo 0)"
SUBJECT="$(git log -1 --pretty=%s 2>/dev/null || echo '')"
MANIFEST="$(mktemp)"
python3 - "$VERSION-$BUILD" "$SHA" "$SUBJECT" > "$MANIFEST" <<'PY'
import json, sys
version, sha, subject = (sys.argv + ["", "", ""])[1:4]
note = f"Build {version} ({sha})" + (f": {subject}" if subject else "")
json.dump({
    "version": version,
    "url": "https://lyku.org/apps/NavGator",
    "notes": note,
    "commit": sha,
}, sys.stdout, indent=2)
PY
for chan in "v$VERSION" "dev-$SHA" "latest"; do
    wr r2 object put "$R2_BUCKET/navgator/$chan/latest.json" \
        --file "$MANIFEST" --content-type application/json
done
rm -f "$MANIFEST"
echo "✓ published update manifest ($VERSION-$BUILD) to R2"

echo "✓ published $published artifact(s) to R2 + registered with lyku.org/apps"
