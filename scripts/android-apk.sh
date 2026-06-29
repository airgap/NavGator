#!/usr/bin/env bash
# Build a signed NavGator APK (arm64) and stage it as dist/navgator-<ver>-android-arm64.apk.
#
# Requires the Android SDK + NDK (ANDROID_HOME, e.g. ~/Android/Sdk with an ndk/<ver>), a JDK
# (apksigner), the rust aarch64-linux-android target, and cargo-apk. It installs the rust
# target + cargo-apk if missing. **No-ops gracefully** if the Android SDK/NDK isn't present,
# so CI on runners without Android tooling doesn't fail (publishing just skips the APK).
#
# Two standard NDK fixes are applied (see docs/ANDROID.md): a libgcc->libunwind shim (NDK r23+
# dropped libgcc) and BINDGEN_EXTRA_CLANG_ARGS pointing bindgen at the NDK sysroot (cargo-apk,
# unlike cargo-ndk, doesn't set it).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"

ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
NDK="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
if [ -z "$NDK" ] || [ ! -d "$NDK" ]; then
    echo "android-apk: no NDK under $ANDROID_HOME/ndk — skipping APK build (provision the Android SDK/NDK to enable)"
    exit 0
fi
export ANDROID_HOME ANDROID_NDK_HOME="$NDK"
VERSION="$(grep '^version' crates/navgator/Cargo.toml | head -1 | sed 's/.*"\([^"]*\)".*/\1/')"
echo "android-apk: NDK=$NDK  version=$VERSION"

# Toolchain (idempotent).
rustup target add aarch64-linux-android >/dev/null 2>&1 || true
command -v cargo-apk >/dev/null 2>&1 || cargo install cargo-apk

# NDK r23+ removed libgcc (uses libunwind); some native deps still request -lgcc.
NDK_TC="$NDK/toolchains/llvm/prebuilt/linux-x86_64"
echo 'INPUT(-lunwind)' > "$NDK_TC/sysroot/usr/lib/aarch64-linux-android/libgcc.a"

# bindgen (for native build scripts) needs the NDK sysroot for the cross target.
BINDGEN_ARGS="--sysroot=$NDK_TC/sysroot --target=aarch64-linux-android30"

env "BINDGEN_EXTRA_CLANG_ARGS_aarch64-linux-android=$BINDGEN_ARGS" \
    cargo apk build --release --manifest-path crates/navgator/Cargo.toml --lib

APK="$(find target -name navgator.apk -path '*release*' 2>/dev/null | head -1)"
[ -n "$APK" ] && [ -f "$APK" ] || { echo "android-apk: APK not found after build" >&2; exit 1; }
mkdir -p dist
OUT="dist/navgator-$VERSION-android-arm64.apk"
cp "$APK" "$OUT"
echo "android-apk: staged $OUT ($(du -h "$OUT" | cut -f1))"
