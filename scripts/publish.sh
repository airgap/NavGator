#!/usr/bin/env bash
# Upload dist/ artifacts to R2 and register them with lyku.org/apps.
# Mirrors lyku/jenkins/Jenkinsfile.zoid-desktop: same versioning (v<ver>/dev-<sha>/latest),
# same Doppler ci-deploy/prd creds on the Jenkins host. Requires parabun (for wrangler),
# the Doppler CLI, and python3. No-ops gracefully if creds/tools are missing.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep '^version' crates/swerve/Cargo.toml | head -1 | sed 's/.*"\([^"]*\)".*/\1/')"
SHA="$(git rev-parse --short HEAD)"
case "$(uname -s)" in Linux) PLATFORM=linux ;; Darwin) PLATFORM=macos ;; *) echo "unsupported OS"; exit 0 ;; esac
DIST="$ROOT/dist"
[ -d "$DIST" ] || { echo "no dist/ — run scripts/package.sh first"; exit 1; }

# parabun (provides `parabun x wrangler`), as lyku/jenkins/build-desktop.sh does.
export PATH="$HOME/.parabun/bin:$PATH"
command -v parabun >/dev/null || curl -fsSL https://raw.githubusercontent.com/airgap/parabun/main/install.sh | bash -s parabun-68d367fda3

# Doppler ci-deploy/prd creds — piggyback on lyku's CI creds on this Jenkins host.
if [ -f /etc/default/jenkins-doppler ]; then
    export DOPPLER_TOKEN="$(grep '^DOPPLER_TOKEN_CI_DEPLOY=' /etc/default/jenkins-doppler | cut -d= -f2)"
fi
if ! command -v doppler >/dev/null || [ -z "${DOPPLER_TOKEN:-}" ]; then
    echo "doppler/creds unavailable — skipping R2 publish (artifacts still in dist/ + archived)"; exit 0
fi
TH="$(mktemp -d)"
export CLOUDFLARE_ACCOUNT_ID="$(HOME=$TH doppler secrets get CLOUDFLARE_ACCOUNT_ID --project ci-deploy --config prd --plain)"
export CLOUDFLARE_API_TOKEN="$(HOME=$TH doppler secrets get CLOUDFLARE_API_TOKEN --project ci-deploy --config prd --plain)"
R2_BUCKET="$(HOME=$TH doppler secrets get R2_BUCKET --project ci-deploy --config prd --plain)"
export CI_RELEASE_TOKEN="$(HOME=$TH doppler secrets get CI_RELEASE_TOKEN --project ci-deploy --config prd --plain)"
rm -rf "$TH"
export REGISTER_URL="${REGISTER_URL:-https://api.lyku.org/register-app-release}"

for f in "$DIST"/swerve-*.tar.gz "$DIST"/swerve-*.AppImage "$DIST"/swerve-*.dmg; do
    [ -f "$f" ] || continue
    name="$(basename "$f")"
    size="$(stat -c%s "$f" 2>/dev/null || stat -f%z "$f")"
    for chan in "v$VERSION" "dev-$SHA" "latest"; do
        parabun x wrangler r2 object put "$R2_BUCKET/swerve/$chan/$name" \
            --file "$f" --content-type application/octet-stream --remote
    done
    # Register the versioned release so it surfaces at lyku.org/apps.
    python3 scripts/register-release.py swerve "$PLATFORM" "$VERSION" "swerve/v$VERSION/$name" "$size" "$SHA" \
        || echo "WARN: registerAppRelease failed for $name"
    echo "✓ published $name"
done
echo "✓ swerve $PLATFORM artifacts uploaded to R2 + registered"
