# NavGator

A web browser with a **native (Rust / [egui](https://github.com/emilk/egui)) UI and
[Servo](https://servo.org) as the page renderer** — engine meant to be reusable by other
apps (Tauri-style) down the line. **Security and performance are the pitch.**

The browser chrome (tabs, toolbar, address bar, menus, dialogs) is drawn natively with
egui directly over the page; Servo renders web content into an `OffscreenRenderingContext`
that egui composites beneath the chrome. This is how servoshell — Servo's own reference
shell — is built: native chrome keeps the UI out of the web engine (a cleaner privilege
boundary) and avoids running a second engine document for the UI.

> **Architecture note (M6):** earlier milestones rendered the chrome as a *second Servo
> webview* of HTML/CSS/JS bridged to the engine over a `navgator:` URL scheme. That was
> replaced by the native egui chrome on the `native-chrome` branch — it dissolved the
> two-webview compositor problem (context menus, dialogs, and pickers are now trivial
> native overlays, with no engine fork patch). See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Status

✅ **Milestones 1–4 complete — verified end-to-end** (headless Xvfb + software GL +
synthetic `xdotool` input). On Linux, `cargo build` succeeds (stable Rust 1.95,
848-package graph, `Cargo.lock` committed); the binary boots Servo and:

- **M1** — renders the HTML chrome via Servo.
- **M2** — composites two webviews in one window (HTML chrome on top, web content
  below via an `OffscreenRenderingContext`), and routes mouse input by region.
- **M3** — a chrome ↔ engine bridge: keyboard input, the omnibox drives navigation
  (`swerve:` command URLs intercepted in `request_navigation`), and content state
  (URL/title/back-forward) is pushed back into the chrome via `evaluate_javascript`.
  Typing a URL + Enter navigates the content webview and updates the address bar.
- **M4** — tabs: multiple content webviews sharing one offscreen context (only the
  active one shown/painted); the chrome renders a tab strip from an engine-pushed
  model; new/select/close work, with per-tab title and navigation. Plus dynamic
  content-rect (the chrome reports its content-region top; no more hardcoded chrome
  height).

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and the roadmap.

Building Servo is still a multi-GB, long, system-dependency-heavy job — install
Servo's native build deps first (below), including the LLVM toolchain note.

The chrome+content compositing, the chrome↔engine bridge, tabs, and the external
engine are Milestones 2–5 in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Prerequisites

- **Rust** — handled by [`rust-toolchain.toml`](rust-toolchain.toml) (stable
  `1.95.0`; `rustup` installs it automatically). Servo no longer needs nightly.
- **Servo's system build dependencies.** Because we depend on Servo as a crate, you
  need the same native libraries Servo itself does. Easiest path: follow the
  [Servo Book → "Setting up your environment"](https://book.servo.org/hacking/setting-up-your-environment.html),
  or run `./mach bootstrap` once in a Servo checkout to install them.
- **LLVM toolchain must be a single, consistent version.** SpiderMonkey
  (`mozjs_sys`) and ANGLE (`mozangle`) build native code + run `bindgen`, and both
  break if your LLVM tooling is mismatched. Two failure modes seen here (clang 18,
  with LLVM 21 also installed):
  1. `mozjs_sys` → "Cannot find llvm-objdump": the bare `llvm-objdump` isn't on
     `PATH` (only versioned `llvm-objdump-18`).
  2. `mozjs_sys`/`mozangle` → `mmintrin.h: use of undeclared identifier
     '__builtin_ia32_*'`: `bindgen`'s `libclang` (picked up as the unversioned
     `libclang.so` → LLVM **21**) doesn't match the clang **18** resource headers.

  Fix both by pinning everything to the LLVM version matching `clang --version`:
  ```bash
  export PATH="/usr/lib/llvm-18/bin:$PATH"   # bare llvm-objdump, etc.
  export LIBCLANG_PATH="/usr/lib/llvm-18/lib" # bindgen uses matching libclang
  cargo build
  ```
  (Media via gstreamer is off because `media-gstreamer` is a *non-default* `servo`
  feature we don't enable, so those libs aren't needed.)
- **Disk & patience.** The dependency graph is large; the first build downloads and
  compiles Servo (expect minutes-to-tens-of-minutes and several GB in `target/`).

## Build & run

```bash
# Launch the browser (chrome + a home tab):
cargo run

# Open a specific page in the first tab:
cargo run -- https://servo.org

# Expose an IPC control socket so another process can drive the engine (M5):
SWERVE_IPC=/tmp/swerve.sock cargo run
#   …then from anywhere:
#   printf 'navigate https://servo.org\n' | socat - UNIX-CONNECT:/tmp/swerve.sock
#   printf 'new-tab\n'                     | socat - UNIX-CONNECT:/tmp/swerve.sock
#   stream events:  socat UNIX-CONNECT:/tmp/swerve.sock -
```

### If the first build fails to resolve dependencies

Servo is unversioned and its transitive deps (webrender, stylo, …) must match the
pinned rev exactly. If cargo resolves wrong versions, copy Servo's lockfile for the
pinned rev and rebuild:

```bash
REV=ed1af70e712aa7ae0df4611241f10f6204389b70
curl -L "https://raw.githubusercontent.com/servo/servo/$REV/Cargo.lock" -o Cargo.lock
cargo build
```

## Project layout

```
swerve/                       # Cargo workspace
├── Cargo.toml                # [workspace] + engine [patch] tables (see docs/FORK.md)
├── rust-toolchain.toml       # stable 1.95.0
├── Jenkinsfile               # tri-platform CI (Linux/macOS/Windows runners)
├── scripts/sync-forks.sh     # maintained-fork merge tooling (--check / --merge)
├── crates/
│   ├── swerve-protocol/      # servo-free IPC wire types
│   ├── swerve-engine/        # the ONLY crate that touches the Servo fork
│   └── swerve/               # the browser binary
│       └── src/
│           ├── main.rs       # winit app, compositor, chrome↔engine bridge, tabs, IPC
│           ├── chrome/       # the browser UI (HTML/CSS/JS, rendered by Servo)
│           └── content/      # home / about pages
├── docs/                     # ROADMAP.md, FORK.md, ARCHITECTURE.md, plan/*
└── reference/verso/          # vendored Verso source (gitignored)
```

## Reference: Verso

[Verso](https://github.com/versotile-org/verso) was a Servo-based browser archived
in Oct 2025. It's vendored under `reference/verso/` (gitignored) as the best worked
example of the hard parts. Re-fetch it with:

```bash
git clone --depth 1 https://github.com/versotile-org/verso.git reference/verso
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for why swerve embeds Servo
differently (high-level `libservo`, pinned rev) than Verso did, and what that buys.
