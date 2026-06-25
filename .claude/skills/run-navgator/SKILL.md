---
name: run-navgator
description: Build, launch, screenshot, and drive the NavGator desktop browser headlessly. Use when asked to run / start / launch / smoke-test / screenshot / drive / validate the NavGator app (the native egui + Servo browser binary).
---

# Run NavGator

NavGator is a **native winit + egui + Servo GUI** — the chrome is egui (immediate-mode, drawn
over the page), the page is a Servo fork. It is **not** Electron and **not** CDP-driveable: there
is no DevTools protocol on the chrome. So it's driven the only way a native X11 app can be driven
headlessly — a virtual display (Xvfb + Mesa **llvmpipe software GL**), `xdotool` for input,
`ffmpeg x11grab` for screenshots. All of that is wrapped in **`.claude/skills/run-navgator/driver.sh`**.

Paths below are relative to the repo root (`/raid/NavGator`). Run `cargo`/`driver.sh build` with
the Bash tool's **`dangerouslyDisableSandbox: true`** (the engine build shells out to libclang/codegen).

## Prerequisites

Runtime tools (already present on this workstation; on a clean box):
```bash
apt-get install -y xvfb xdotool ffmpeg x11-utils mesa-utils libgl1-mesa-dri
```
The binary build also needs the repo's build env — rustc 1.95 (pinned via `rust-toolchain.toml`)
and the **gitignored `.cargo/config.toml` libclang pin**. If `cargo build` fails in `bindgen`, that
pin is missing — see the `navgator-build-env` memory. A from-scratch Servo build is huge; here it's
incremental.

## Run (agent path) — use the driver

```bash
D=.claude/skills/run-navgator/driver.sh

$D build                       # cargo build -p navgator (incremental ~3-10s)
$D start gator://welcome       # start Xvfb :99 + launch; waits for the window, prints wid+pid
$D shot /tmp/welcome.png       # screenshot the display -> PNG (then Read it)
$D nav 'gator://why'           # focus address bar, type url, Enter
$D shot /tmp/why.png
$D stop                        # kill ONLY what the driver started (never touches :0)
```
Other subcommands: `$D palette` (Ctrl+K command palette), `$D key ctrl+t` (any xdotool keyspec),
`$D type <text>`, `$D click <x> <y>` (display coords, 1280x800). Every input subcommand
`windowfocus`es first (see Gotchas). **Always `Read` the screenshot** — a blank/stale frame means
it didn't actually do what you think.

`$D start` takes any URL as argv[1] (`http://…`, `gator://export`, a `data:` URL). State (pids,
logs, default screenshot) lives in `/tmp/navgator-run/`; the engine log is `/tmp/navgator-run/nav.log`.

### Driving a real page flow
`start` it on the page (`$D start http://localhost:8899/`), then `$D click <x> <y>` the element,
`$D shot`, Read it. To verify a page↔native bridge, watch `/tmp/navgator-run/nav.log` for native
eprintln output after the interaction.

## Run (human path)
`cargo run -p navgator -- <url>` opens a real window on `$DISPLAY`. Useless headless, and on this
workstation `$DISPLAY=:0` is the user's actual desktop — **don't launch there**, and never kill
NavGator by name (the user runs their own `*.AppImage` builds on :0). The driver only ever touches
its own Xvfb `:99` and the pid it recorded.

## Gotchas (battle scars — all hit on a live run)

- **No window manager under Xvfb**, so a click sets *pointer* focus but not X *input* focus.
  You MUST `xdotool windowfocus <wid>` before keystrokes register (the driver does this in `focus()`).
  Symptom: keys silently do nothing.
- **`pkill -f navgator` self-kills** — the matching shell's own args contain "navgator", so it kills
  itself → **exit 144**. (Those 144s are harmless but confusing.) The driver kills by recorded PID only.
- **`cargo test -p navgator` does NOT rebuild the `[[bin]]`** (only the test harness). Always
  `cargo build -p navgator` / `$D build` before relaunching, or you screenshot stale code.
- **Servo CSS Grid is incomplete** → `display:grid` collapses items to full width. Use **flexbox**
  in any `gator://` HTML. (Bit the new-tab top-sites/cards; fixed.)
- **`fetch('navgator://…')` does NOT reach the WebResourceLoad interceptor** (Servo Fetch doesn't
  route custom schemes), but **top-level navigation and subresource loads (img, @font-face) DO**.
  Page→native bridges must use an `<img>`/subresource beacon, not fetch.
- **Servo doesn't dispatch `focusin`/`focus` reliably** — use `click`/`keydown` for field sensors.
- **The X cursor is an ✕ shape and Xvfb parks it at screen-center (640,400) on launch.** `ffmpeg
  x11grab` captures it by default, so it looks like a "✕ glitch" mid-page on sparse pages (it's
  absent on pages where you'd clicked into the toolbar, which moved the cursor away — hence the
  false "only on sparse pages" pattern). The `shot` subcommand grabs with **`-draw_mouse 0`** so the
  cursor is never captured. It is NOT a NavGator bug (this was mis-filed as #41, then closed).
- **Isolated profile.** `start` exports `XDG_CONFIG_HOME=/tmp/navgator-run/profile` so test runs
  never touch the user's real `~/.config/navgator` (history/permissions/session/passwords). Upshot:
  the test app starts with a FRESH profile — top-sites show the demo set, not the user's real
  history, and saved-password/permission state is empty until you seed it in that run. Don't launch
  `./target/debug/navgator` by hand without this export, or you'll pollute the real profile.
- **Software GL prints `libEGL … DRI3` warnings** to nav.log — harmless (llvmpipe fallback). The
  multiprocess + Landlock/seccomp content-process sandbox starts fine under Xvfb.

## Troubleshooting

- `$D start` prints `FAILED to map window` → read `/tmp/navgator-run/nav.log`; usually a missing
  `lib*.so` (install it) or `:99` already taken by a dead Xvfb (`$D stop` then retry).
- Keystrokes ignored → the window lost input focus; the driver re-`windowfocus`es each command, but
  if you call `xdotool` directly, `windowfocus` the `NavGator` window first.
- Build fails in `bindgen`/`clang-sys` → the gitignored `.cargo/config.toml` libclang pin is absent
  (see `navgator-build-env`).
- Screenshot is all one color → app crashed or never mapped; check it's still alive
  (`ps -p $(cat /tmp/navgator-run/nav.pid)`).
