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

## Remaining for a runnable APK (next phase)
All tractable embedder-side work — no engine cross-compile risk left:
- **`android_main` entry**: restructure navgator from a bin (`fn main`) to a lib + cdylib
  with `#[no_mangle] android_main(AndroidApp)` (via `android-activity` / winit's
  `EventLoopBuilderExtAndroid::with_android_app`), keeping a thin desktop `main`.
- **Touch input** → Servo (currently only mouse is forwarded).
- **Mobile egui layout** (touch-sized toolbar/tabs; likely a bottom bar).
- **APK packaging** (`cargo-apk` or `cargo-ndk` + a gradle wrapper) with a manifest.
- **Emulator smoke test** via adb (the dev box has system-images + adb).
