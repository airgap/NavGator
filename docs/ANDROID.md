# NavGator on Android

**Status (2026-06-19):** the full app — native egui chrome **+ the Servo engine
(SpiderMonkey/mozjs, stylo, webrender)** — **compiles and links for `aarch64-linux-android`**.
A real arm64 ELF binary is produced. The native-egui chrome pivot is what makes the UI layer
portable to mobile (egui is touch-capable via winit; the old HTML-chrome-as-a-Servo-webview
would not have suited a phone). Targets: **Android + Linux + macOS** (iOS is out — Apple
enforces WebKit outside the EU; see the architecture notes).

## Prerequisites
- Android SDK + **NDK 27.1.x** (e.g. `ANDROID_HOME=~/Android/Sdk`, `ndk/27.1.12297006`).
- `rustup target add aarch64-linux-android x86_64-linux-android`
- `cargo install cargo-ndk`

## Building the engine + app
```sh
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.1.12297006"
cargo ndk --target arm64-v8a --platform 30 build --package navgator
```

Two NDK-specific fixes are required (both are standard, not NavGator-specific):

1. **`-lgcc` shim.** NDK r23+ removed libgcc (it uses libunwind). Some native deps still
   request `-lgcc`, so provide a redirect once:
   ```sh
   NDK_TC="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64"
   echo 'INPUT(-lunwind)' > "$NDK_TC/sysroot/usr/lib/aarch64-linux-android/libgcc.a"
   # (repeat for x86_64-linux-android when building the emulator target)
   ```
2. **`--platform 30` (minSdk 30).** Lower API levels lack `AHardwareBuffer_*` (26),
   `posix_madvise` (23), `__fread_chk` (24), etc., which mozjs/webrender reference → link
   errors. minSdk 30 resolves them.

`media-gstreamer` is **desktop-only** (see `crates/navgator-engine/Cargo.toml` —
`[target.'cfg(not(target_os = "android"))']`); Android has no desktop GStreamer, so the
engine builds without media there. Android `<video>`/`<audio>` will need a separate
servo-media backend later.

## APK — built, signed, installable ✅
`navgator` is now a lib (rlib + cdylib) with `desktop_main()` + an `android_main(AndroidApp)`
entry (winit `EventLoopBuilderExtAndroid::with_android_app`); `src/main.rs` is a thin desktop
binary. **cargo-apk produces a signed APK:**
```sh
cargo install cargo-apk
# bindgen needs the NDK sysroot (cargo-apk, unlike cargo-ndk, doesn't set it):
env "BINDGEN_EXTRA_CLANG_ARGS_aarch64-linux-android=--sysroot=$NDK_TC/sysroot --target=aarch64-linux-android30" \
  cargo apk build --release --manifest-path crates/navgator/Cargo.toml --lib
```
`scripts/android-apk.sh` wraps this (toolchain setup, the libgcc shim, the bindgen env) and
stages `dist/navgator-<ver>-android-arm64.apk`. The APK is `org.airgap.navgator` (in
`[package.metadata.android]`): NativeActivity → `android_main`, **`libc++_shared.so` bundled**,
**INTERNET permission**, minSdk 30. Verified well-formed via `aapt2 dump badging`.

## CI publishing ✅
The Jenkinsfile **Android APK** stage runs `scripts/android-apk.sh` on the linux agent and
stashes the APK; the Publish stage uploads it to R2 + registers it at `lyku.org/apps/NavGator`
(`publish.sh` globs `.apk`/`.aab`, platform `android`). The stage **no-ops gracefully** if the
runner lacks the Android SDK/NDK — so it never gates the desktop build. **To enable Android
publishing, the CI runner needs the Android SDK + NDK 27 provisioned** (and a JDK for apksigner).

## Remaining (runtime validation + polish)
- **Emulator/device smoke test**: confirm it launches + renders. The APK is arm64-v8a; on an
  x86_64 host emulator that needs an arm64 system-image (slow QEMU translation) or build an
  `x86_64-linux-android` APK to match an x86_64 emulator. Physical arm64 device is simplest.
- **Touch input** → Servo (currently only mouse is forwarded).
- **Mobile egui layout** (touch-sized toolbar/tabs; likely a bottom bar).
- **AAB** (Play Store) would need a gradle wrapper; the APK suffices for lyku.org/apps sideload.
