# swerve

A web browser whose **UI is HTML rendered by [Servo](https://servo.org)** — and
whose engine is meant to be reusable by other apps (Tauri-style) down the line.

The browser chrome (tabs, toolbar, address bar) is an HTML/CSS/JS document that
Servo paints, and web pages are rendered by Servo alongside it. Not via an
`<iframe>` (the web blocks that — see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)),
but as a separate webview composited into the window.

## Status

✅ **Milestone 1 complete — verified end-to-end.** swerve builds, links, launches,
and renders its HTML chrome via Servo. On Linux: `cargo build` succeeds (stable Rust
1.95, 848-package graph, `Cargo.lock` committed); the binary boots Servo's
constellation, loads `src/chrome/index.html`, and paints the tabs/toolbar/omnibox —
confirmed by a headless Xvfb + software-GL screenshot. The code mirrors Servo's
maintained `winit_minimal` example at the pinned rev.

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
# Render the HTML chrome (Servo painting our own UI):
cargo run

# Or load a page in the (single, M1) webview:
cargo run -- https://servo.org
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
swerve/
├── Cargo.toml            # libservo pinned to an exact rev (see ARCHITECTURE)
├── rust-toolchain.toml   # stable 1.95.0, matched to servo
├── src/
│   ├── main.rs           # M1: winit + libservo, single webview (verified API)
│   └── chrome/           # the browser UI — HTML/CSS/JS, rendered by Servo
│       ├── index.html
│       ├── chrome.css
│       └── chrome.js
├── docs/
│   └── ARCHITECTURE.md   # the design, the compositing model, the roadmap
└── reference/verso/      # vendored Verso source as reference (gitignored)
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
