# navgator — Security, Sandboxing & Site Isolation

Status: design + current-state assessment. Author dimension: security & process model.
Date: 2026-06-18. Servo pin: `ed1af70`. Verified against the cached Servo source at
`/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70` and the navgator repo.

> **One-line verdict:** navgator today is a **single-process, unsandboxed** browser whose
> own privileged UI is **native egui** (no longer a `file://` document). That is acceptable for a prototype and
> **disqualifying for shipping to real users.** The single hardest, most load-bearing
> requirement before any public release is a **sandboxed multiprocess content model
> with at least site-per-process isolation** — and the engine code to do it exists in
> Servo but is **off by default, partially stubbed, and built on an unmaintained
> sandbox crate (`gaol` 0.2.1)**. This gates everything.

---

## 1. Current state — verified facts

### 1.1 navgator (the embedder) today

| Property | State | Evidence |
| --- | --- | --- |
| Process model | **Single process.** `src/main.rs` never sets `opts.multiprocess`, never calls `run_content_process`, has no `--content-process` arg branch. | `grep multiprocess/sandbox/content-process src/main.rs` → 0 hits. |
| Sandbox | **None.** No seccomp, no namespaces, no entitlements. Web JS runs in the same address space as the chrome UI and the embedder. | same. |
| Privileged UI | Chrome is **native egui**, not a web document — there is no `file://` chrome, `chrome_url()`, or `src/chrome/index.html`. | `crates/navgator/src/main.rs`. |
| Chrome ↔ content boundary | The egui chrome and Servo are the same process; web-origin title/URL flow into chrome **state (Rust strings rendered by egui)**, not into a privileged JS context. There is no `evaluate_javascript` chrome push. (`evaluate_javascript` is now used only for find-in-page inside page content.) | `crates/navgator/src/main.rs`. |
| `js_string` escaping | Escapes `" \ \n \r <`. **Does not escape U+2028/U+2029** (JS line terminators) — a real, if narrow, injection edge in the privileged context. | `src/main.rs:104-119`. |
| External control socket | `NAVGATOR_IPC` Unix socket, **no auth, default umask perms**, full navigation/tab control. Off unless env var set. | `src/main.rs:763-772`. |
| TLS / network | Inherited from Servo `net` (rustls + aws-lc-rs). No navgator-side policy. | below. |
| Auto-update / signing | **None.** No updater, no code signing, no release integrity. | repo has no such code. |

**Two embedder-level findings that matter even before multiprocess:**

1. **The chrome is native egui, not a web document** (post-pivot). It no longer has a
   `file://` origin, a DOM, or a JS context, so the classic "web-controlled string → chrome
   DOM → `innerHTML`/`eval` → UI code execution" escalation path described in the original
   threat model is **closed by construction**. The remaining single-process exposure stands:
   in a single-process build a Servo DOM/JS exploit in *any* tab still shares the embedder's
   address space (G1) — the multiprocess/sandbox requirement below is unchanged. (Web-origin
   title/URL strings still reach the chrome, but as Rust data rendered by egui, not as JS.)

2. **`NAVGATOR_IPC` is an unauthenticated local control plane.** Any local process /
   user that can reach the socket path can drive navigation and open tabs. For a
   shipping browser this must be off by default (it is), permission-restricted
   (`0600` + per-user runtime dir), and ideally capability-token gated.

### 1.2 Servo's process model (the engine we depend on)

Servo *has* a real multiprocess architecture, but it is **opt-in and incomplete**:

- **Topology:** one privileged **constellation** process (owns navigation, the
  browsing-context/pipeline graph, session history) + N **content (script+layout)
  processes**, wired with `ipc-channel`. `compositor`, `net` (fetch/resource thread),
  GPU/WebRender, fonts all run in the **main/privileged process**, not isolated.
  (`components/constellation/`, `components/servo/servo.rs`.)
- **Process boundary granularity = registered domain (eTLD+1-ish `Host`), not site.**
  The constellation keys script **event loops by `Host`** (`event_loops:
  HashMap<Host, Weak<EventLoop>>`, `constellation.rs:260-261`) and reuses an event
  loop for same-`Host` pipelines (`get_event_loop_for_new_pipeline`,
  `constellation.rs:926-969`). In multiprocess mode **one event loop ≈ one process**
  (`EventLoop::spawn` → `spawn_in_process` when `opts.multiprocess`,
  `event_loop.rs:116-119,153`). So Servo's de-facto isolation unit is
  **process-per-registered-domain**, *not* Chrome's site-per-process with
  cross-origin-iframe (OOPIF) isolation. Sandboxed origins correctly force a fresh
  loop (`constellation.rs:933-939`); `about:blank`/`srcdoc` correctly inherit the
  creator's loop (`941-960`).
- **It is OFF by default.** `opts.multiprocess = false`, `opts.sandbox = false`
  (`config/opts.rs:249,254`). servoshell exposes `--multiprocess`/`--sandbox` prefs;
  the API exposes `opts.multiprocess`, `ServoBuilder`, and `run_content_process(token)`.
- **The sandbox is `gaol` 0.2.1** (`components/constellation/sandboxing.rs`,
  `components/servo/Cargo.toml:159`). Activated in the child by
  `ChildSandbox::new(content_process_sandbox_profile()).activate()`
  (`servo.rs:1377`), but **only if `opts.sandbox`**, and the parent only enters the
  `gaol::sandbox::Command` path when `content.opts().sandbox` (`sandboxing.rs`).
- **Platform coverage of the sandbox is narrow and partly stubbed:**

| Platform | Servo sandbox status (at `ed1af70`) |
| --- | --- |
| Linux x86-64 | `gaol` seccomp-bpf **hard allowlist of 21 syscalls** + namespace (user/pid/net/mount) isolation. Functional but brittle (see 1.3). |
| Linux arm/aarch64/riscv | **Not supported** — `content_process_sandbox_profile()` is the `process::exit(1)` stub. (`sandboxing.rs` cfg gates.) |
| macOS x86-64 | `gaol` Seatbelt profile (font dirs, `/dev/urandom`, MachLookup FontServer). |
| macOS aarch64 (Apple Silicon) | **Not supported** — excluded by `all(target_arch="aarch64", not(target_os="macos"))`… inverted: aarch64-macos falls into the `exit(1)` stub. **Apple Silicon has no content sandbox.** |
| Windows | **Not supported.** `content_process_sandbox_profile()` → `log::error + exit(1)`. No AppContainer, no job objects. Servo is actively *seeking contributors* for this. |
| Android / iOS / OHOS | Not supported / N/A. |

- **`Process::wait()` for sandboxed children is a TODO** (`process_manager.rs:28-32`:
  "wait() is not yet implemented for sandboxed processes"). Reaping/lifetime mgmt is
  incomplete → zombie/again-spawn hazards under churn.

### 1.3 `gaol` reality check (this is a strategic risk)

`gaol` 0.2.1 (crates.io, **edition 2021, last release ~2021**, self-described as
"only lightly reviewed for correctness and security… not mature or battle-tested"):

- Linux Layer 1 = **user/PID/network/mount namespaces** via `clone()` (`platform/linux/namespace.rs`) — the real confinement.
- Linux Layer 2 = **seccomp-bpf** with `SECCOMP_RET_KILL` on a **21-syscall allowlist**
  (`platform/linux/seccomp.rs`: `brk,close,exit,exit_group,futex,getrandom,getuid,
  mmap,mprotect,munmap,poll,read,recvfrom,recvmsg,rt_sigreturn,sched_getaffinity,
  sendmmsg,sendto,set_robust_list,sigaltstack,write` + a few for file-read/network
  profiles). **`open` (not `openat`) is conditionally allowed; `openat2`, `clone3`,
  `rseq`, `membarrier`, `statx`, `getrandom`-newer paths are absent.**

Why this is a problem for a *2026* browser:
- Modern glibc and the Rust/tokio runtime call `clone3`, `rseq`, `openat2`,
  `statx`, `membarrier` etc. A `SECCOMP_RET_KILL` allowlist that predates these will
  **SIGSYS-kill content processes** on current toolchains/distros unless Servo's
  profile is updated — fragile and a moving target.
- It is a **default-kill allowlist of raw syscall numbers**, x86-64/x86/arm-only arch
  dispatch, no seccomp-notify, no Landlock, no flag-arg filtering for `clone`/`mmap`
  beyond a couple of cases. This is **1st-generation sandboxing**; Chrome/Firefox have
  moved far past it (broker process, Landlock LSM, `seccomp-notify`, GPU/net brokers).
- **Maintenance treadmill:** this is exactly the Verso failure mode applied to
  security — an unmaintained dep at the center of the trust boundary.

### 1.4 Network security in Servo `net` (what we inherit — mostly good)

| Mechanism | State | Evidence |
| --- | --- | --- |
| TLS | **rustls + aws-lc-rs** crypto provider, ALPN h2/http1. | `net/connector.rs:27-28`. |
| Root trust | `rustls-platform-verifier` (OS trust store) by default; `webpki_roots` fallback (always on Android). Pref `network_use_webpki_roots`. | `connector.rs:480-520`. |
| Cert-error override | `CertificateVerificationOverrideVerifier` — supports user "accept this cert" overrides **and** an `ignore_certificate_errors` kill-switch that accepts *anything*. | `connector.rs:460-585`. |
| HSTS | Implemented (`net/hsts.rs`), incl. a preload list. | `net/hsts.rs`. |
| Mixed content | Implemented per W3C spec — upgrade + block for requests and responses. | `net/fetch/methods.rs:450-497,1211-1381`. |
| CSP | Implemented via the `content_security_policy` crate; enforced in `script` (document policy container, navigation checks). | `script/dom/document/document.rs:19,1729-1745,3680+`. |
| CORS | Implemented — fetch CORS cache, header checks, cross-origin gating. | `net/fetch/cors_cache.rs`, `fetch/methods.rs`. |
| Subresource Integrity | Implemented. | `net/subresource_integrity.rs`. |
| Safe Browsing / URL reputation | **Does not exist.** No phishing/malware/anti-download protection anywhere in Servo. | `grep safe.browsing/phishing/malware components/` → 0. |
| Cookies / SameSite | Cookie jar + storage exists; need to audit SameSite default + partitioning. | `net/cookie.rs`, `cookie_storage.rs`. |

**Concern to track:** `ignore_certificate_errors` and the cert-override path are
embedder-controllable. navgator must (a) never expose an "ignore all cert errors" toggle
in release builds, (b) make per-cert overrides a deliberate, scary, non-sticky UI flow,
and (c) treat the override store as security state to be synced carefully (Lyku) or not
at all.

---

## 2. Gap ranking (engine + embedder), worst first

| # | Gap | Severity | Why it gates shipping |
| --- | --- | --- | --- |
| **G1** | **No content sandbox + single process.** Any memory-safety bug in SpiderMonkey, WebRender, image/font/media decoders, or `unsafe` Servo code = full RCE in the embedder, the chrome UI, and the user's session. Rust does **not** make the JS engine (C++) or `unsafe` FFI safe. | **Critical / blocker** | This is the table-stakes browser security property. Without it navgator is a remote-code-execution delivery vehicle. |
| **G2** | **(Largely resolved by the native-egui pivot.)** The privileged UI is no longer a `file://` web document with a DOM/JS context, so the UXSS escalation path (content string/XSS → chrome DOM → UI control) is closed by construction. Residual: web-origin strings still reach the chrome as data, and the chrome still shares the single process with content (folds into G1). | **Largely resolved / residual under G1** | Native chrome removes the classic privileged-`file://`-chrome surface; only the single-process sharing remains, tracked by G1. |
| **G3** | **No auto-update + no code signing + no release integrity.** | **Critical / blocker** | You cannot ship a browser you can't security-patch within hours, and unsigned binaries are both unrunnable (macOS/Win SmartScreen) and trivially trojanable. |
| **G4** | **Sandbox tech debt: `gaol` 0.2.1 unmaintained, Windows/Apple-Silicon/Linux-arm unsupported, brittle seccomp, sandboxed-process reaping is a TODO.** | **High** | The thing that provides G1 is itself the maintenance risk that killed Verso, plus it doesn't cover the platforms most users are on (Windows, M-series Macs). |
| **G5** | **Isolation granularity = process-per-registered-domain, no OOPIF / no Site Isolation for cross-origin iframes; compositor/net/GPU unisolated.** | **High** | Spectre-class cross-origin reads, and a compromised content process still talks to a privileged net/compositor in-process. Below Chrome's bar. |
| **G6** | **No Safe-Browsing-equivalent.** No phishing/malware/dangerous-download protection. | **High (UX-critical)** | The #1 real-world threat to normal users is social-engineering/phishing, not 0-days. Chrome/Firefox/Safari all ship this. |
| **G7** | **No exploit-mitigation posture defined** (CFI, stack clash, RELRO/BIND_NOW, ASLR/PIE, W^X JIT, arena hardening, `-Z sanitizer` CI, fuzzing). | **Medium-High** | Reduces exploitability of the inevitable bugs; cheap-ish to adopt; expected of a "v1 industry-standard" browser. |
| **G8** | **No security UI / indicators** (origin display, cert info, permission prompts, downloads-are-dangerous, HTTPS-Only mode, mixed-content UI). | **Medium** | Users can't make trust decisions; phishing/spoofing trivially succeeds. |
| **G9** | **Unauthenticated `NAVGATOR_IPC` + permissive socket perms.** | **Medium (local)** | Local privilege/automation surface; must be hardened or release-gated off. |
| **G10** | **`js_string` misses U+2028/2029.** ("No CSP on the chrome doc" no longer applies — the native-egui chrome is not a document.) The gap affects only find-in-page strings interpolated into *content* JS via `js_string`; `gator://` internal pages are HTML-escaped by a separate `html_escape`. | **Low** | Defense-in-depth for the residual find-in-page content-JS path. |
| **G11** | **No sync-security model for Lyku** (E2E encryption, key handling, what is *never* synced — cert overrides, cookies?). | **Medium (future)** | Sync is a credential/PII firehose; designing it insecure now is hard to undo. |

---

## 3. Target architecture (design)

### 3.1 Process & privilege model

Goal: converge toward the **Chrome/Firefox broker model**, reusing Servo's
constellation/content split and *adding* what Servo lacks.

```
              ┌──────────────────────────────────────────────────────────┐
 PRIVILEGED   │  navgator browser process (broker)                          │
 (broker)     │   • winit window + compositor + WebRender (GPU)           │  ← keep minimal,
              │   • Servo constellation (nav graph, session history)      │     heavily audited
              │   • net/resource thread (TLS, cookies, cache)             │     candidates to split
              │   • chrome UI webview  (PRIVILEGED — see 3.3)             │     out later (G5)
              │   • update / IPC-broker / permission policy               │
              └───────────────┬───────────────────────┬──────────────────┘
                 ipc-channel   │                        │  ipc-channel
              ┌────────────────▼─────┐        ┌─────────▼────────────────┐
 UNPRIV       │ content process       │  ...   │ content process          │
 (sandboxed)  │ site = scheme+eTLD+1  │        │ another site             │
              │ SpiderMonkey + layout │        │ + cross-origin iframes   │
              │ seccomp + namespaces  │        │   (OOPIF, phase 2)       │
              └───────────────────────┘        └──────────────────────────┘
```

Phasing:
1. **Phase A (blocker for v1):** turn on Servo multiprocess + sandbox; one sandboxed
   content process **per registered domain** (Servo's existing granularity); chrome in
   the broker. This alone removes G1 for the common case.
2. **Phase B:** site-per-process with **scheme+eTLD+1** as the site key (tighten Servo's
   `Host` keying to include scheme + treat eTLD+1 via the public-suffix list); strict
   origin checks on all constellation IPC.
3. **Phase C:** OOPIF — out-of-process cross-origin iframes (G5). Large; Servo doesn't
   have this. Track as a multi-quarter engine project, likely upstream-collaborative.
4. **Phase D:** split **GPU** and **network** into their own brokered processes so a
   content compromise can't directly drive GPU drivers / the socket layer.

### 3.2 OS sandbox per platform (replaces/augments `gaol`)

**Decision: do not bet v1 security on `gaol` 0.2.1.** Either (a) fork+modernize the
Linux/macOS profiles and *own* them, or (b) move to maintained primitives. Recommended
concrete stack:

| Platform | v1 mechanism | Notes |
| --- | --- | --- |
| **Linux** | **Layer 1:** user + pid + net + mount + IPC namespaces, `no_new_privs`, drop all caps, empty mount/`/proc` hidden, `/dev` minimal. **Layer 2:** seccomp-bpf via the maintained **`seccompiler`** crate (Firecracker's, actively maintained, arg-filtering, sane arch handling) with a **modern allowlist** (incl. `openat2`, `clone3`, `rseq`, `membarrier`, `statx`). **Layer 3 (defense-in-depth):** **Landlock** LSM for filesystem path restriction on kernels ≥5.13. | Replaces gaol's seccomp with a maintained generator and a current syscall set; keeps the namespace approach but on owned code. Brokered file/socket access for the few legit needs (fonts, GPU dev nodes) instead of allowlisting `open`. |
| **macOS** | App Sandbox **entitlements** + per-process **`sandbox_init`/Seatbelt** profile (modeled on gaol's but maintained in-tree); **hardened runtime**; Apple-Silicon **must** be covered (Servo's gaol gate currently isn't). | Required for notarization anyway. Use `com.apple.security.app-sandbox` and a tight SBPL profile for content procs. |
| **Windows** | **AppContainer** (low-privilege SID, capability-scoped) **+ a restricted/Low-IL token + Job Object** (kill-on-close, process/limit caps) **+ mitigation policies** (ACG/CIG, no child-proc, no win32k where feasible). Servo has **none** of this today. | This is net-new and substantial. It is also where the most users are; cannot ship Windows without it. Win32k lockdown is hard with GPU — likely keep GPU in broker initially (3.1 Phase D inverse). |

All three feed a single embedder-side `sandbox` trait so the constellation's
`spawn_multiprocess` path can be navgator-controlled rather than gaol-hardcoded — but note
this likely requires **upstream Servo changes** (the spawn/sandbox code lives in
`components/constellation` and `components/servo`, not in the embedder). Plan for either
upstreaming a pluggable sandbox or carrying a small patch set (a *bounded*, reviewed
Servo-bump cost, consistent with the project's pinning discipline).

### 3.3 Privileged chrome hardening (fixes G2/G10)

- **(Mostly moot post-pivot: the chrome is native egui, not a webview, so it has no
  navigable origin to harden.)** What remains: navgator's internal *content* pages are
  served from the embedded **`gator://`** scheme (`AppState::load_web_resource` →
  `render_gator_welcome`, serving only embedded in-binary resources, no filesystem reach,
  no remote load). Keep that handler strictly embedded-only.
- The page renderer must never treat `gator://` (or other privileged internal) resources as
  web-script-reachable; keep internal pages embedded and origin-isolated from web content.
- **(Post-pivot: there is no chrome document, so chrome CSP and a chrome JS bridge are
  N/A.)** The native-egui chrome receives web→chrome data as typed Rust values, never as
  concatenated JS, which is what the original "typed bridge" item asked for. The only
  remaining string-into-JS interpolation is find-in-page (`evaluate_javascript(format!(...))`
  with `js_string`), so the residual hardening is just `js_string`: also escape U+2028/U+2029
  (prefer serialization over hand-rolled escaping). Internal `gator://` pages are templated
  HTML escaped separately by `html_escape`, not `js_string`.
- Long-term, run the chrome UI in its **own low-privilege "UI" process** distinct from
  the broker, so a chrome compromise still can't directly touch the socket/net layer.

### 3.4 Network security (mostly inherit, add policy)

- **Keep** rustls + aws-lc-rs + platform verifier; **add CT awareness** later, **enable
  OCSP/CRLite-style revocation** roadmap item (Servo doesn't do revocation robustly).
- **HTTPS-First/Only mode** as a shipping default (auto-upgrade, warn-on-fallback);
  Servo has the upgrade machinery (mixed-content), wire a UI policy on top.
- **Remove/CI-forbid `ignore_certificate_errors` in release builds.** Per-cert override
  = explicit, non-persistent-by-default, clearly-worded interstitial.
- Audit **cookie SameSite default** and adopt **cookie/storage partitioning** (state
  partitioning by top-level site) for anti-tracking parity.
- Define a **permissions model** (geolocation, camera/mic, notifications, clipboard) —
  Servo's `embedder` permission hooks exist; navgator must implement deny-by-default
  prompts.

### 3.5 Safe-Browsing-equivalent, privacy-preserving (fixes G6)

Do **not** call Google Safe Browsing (telemetry/dependency on the ecosystem navgator
rejects). Options, recommended order:

1. **Local hash-prefix blocklist** updated out-of-band (like GSB v4 *local* model but
   self-hosted): ship + periodically fetch a Bloom/prefix set of known phishing/malware
   hosts; check **locally**, zero per-navigation network calls. Source feeds:
   **URLhaus, PhishTank/OpenPhish, the Tranco-adjacent and community blocklists**;
   redistribute under "Lyku Safe-Lists." Privacy-preserving by construction.
2. **Optional** opt-in **k-anonymity / OHTTP-fronted** lookup for full-hash confirmation
   on a prefix hit (only the 32-bit prefix leaves the device, via an oblivious relay so
   the server can't link IP↔query). Off by default; user-enabled.
3. **Download protection:** mark-of-the-web equivalent + an executable-download warning;
   optional local hash check against the same lists.

This is a **must-have for real users** (phishing is the dominant threat) but is
*independent* of the engine and can be built embedder-side — schedule it alongside G1–G3.

### 3.6 Auto-update, code signing, release integrity (fixes G3)

- **Updater:** background differential updater (consider `cargo-dist` + a maintained
  framework like **Squirrel (mac/win)** / system packages (Linux) / or a custom
  **TUF-backed** channel). Must support **silent, prompt-less security patches** and
  **staged rollout + kill-switch**.
- **Signing:** Windows **Authenticode** (EV cert for SmartScreen reputation), macOS
  **Developer ID + notarization + hardened runtime + stapling**, Linux **detached
  GPG/minisign** + reproducible-ish builds.
- **Release integrity:** **The Update Framework (TUF)** for metadata signing
  (root/targets/snapshot/timestamp keys, offline root), so a compromised CDN can't push
  a malicious update. SBOM + provenance (SLSA), pinned `Cargo.lock` (already done),
  `cargo audit`/`cargo deny` in CI gating on RUSTSEC (note: Servo already carries
  unmaintained-dep advisories, e.g. `servo-fontconfig` RUSTSEC-2025-0059, and `gaol`
  itself — track and triage).

### 3.7 Exploit mitigations (fixes G7)

- Build flags: PIE/ASLR, full RELRO + BIND_NOW, stack-clash + stack-protector-strong,
  `-D warnings`, `-C overflow-checks` on security-sensitive crates, control-flow
  integrity where the toolchain allows.
- **Fuzzing**: stand up `cargo-fuzz`/libFuzzer + OSS-Fuzz-style continuous fuzzing of
  the parsers navgator is exposed through (URL, HTML/CSS via Servo, image/font decoders).
- **Sanitizer CI**: periodic ASan/UBSan/TSan runs of content-process code paths.
- **JIT hardening**: ensure SpiderMonkey ships with W^X / its JIT mitigations on; don't
  disable.
- Treat every `unsafe` block in navgator's own code as reviewed; minimize FFI surface.

---

## 4. What is REQUIRED before shipping to real users (the gate)

These are **non-negotiable for a v1 public release.** Everything else is "after."

1. **Sandboxed multiprocess content, on by default, on every shipping platform**
   (Linux x86-64+arm64, macOS x86-64+**Apple Silicon**, Windows). At minimum
   process-per-registered-domain (Servo's existing granularity) with a **real,
   maintained** OS sandbox — not gaol-as-is on the two platforms it half-supports.
   → resolves **G1, G4**.
2. **Native-egui chrome (done): no web origin, no DOM/JS, typed in-process data only;
   internal *content* pages served from the embedded `gator://` scheme.** This already
   resolves the G2 privileged-`file://`-chrome surface; remaining G10 work is just hardening
   `js_string` (the find-in-page content-JS interpolation). → resolves **G2**, narrows **G10**.
3. **Auto-update with signed releases + TUF (or equivalent) integrity, plus a tested
   emergency-patch path.** → resolves **G3**.
4. **A privacy-preserving Safe-Browsing-equivalent + dangerous-download warnings, on by
   default.** → resolves **G6**.
5. **Baseline security UI:** trustworthy origin display, cert/connection info, cert-error
   interstitial (no silent bypass, `ignore_certificate_errors` compiled out of release),
   HTTPS-First default, deny-by-default permission prompts. → resolves **G8** (baseline).
6. **`NAVGATOR_IPC` off by default + `0600` + per-user runtime dir + token auth** (or no
   external control plane in consumer builds). → resolves **G9**.
7. **CI security gates:** `cargo deny`/`cargo audit` failing the build on un-triaged
   advisories; basic exploit-mitigation build flags; a fuzzing harness in place.
   → starts **G7**, manages the Servo-dependency advisory backlog.

**Explicitly *not* required for v1 (but on the roadmap):** OOPIF/full Site Isolation
(G5 Phase C), separate GPU/network broker processes (Phase D), CT/revocation, Lyku E2E
sync (ship sync *after* security, with the model in 3.4/G11 designed up front).

---

## 5. Sequencing recommendation

| Order | Work | Depends on | Rough scale |
| --- | --- | --- | --- |
| 1 | Turn on Servo multiprocess+sandbox in navgator; implement the embedder `--content-process` branch and `ServoBuilder.opts(multiprocess+sandbox)`; verify per-domain processes spawn. | Servo API (`run_content_process`) | weeks (engine API is ready) |
| 2 | Replace gaol Linux/macOS profiles with owned `seccompiler`+namespaces (+Landlock) and a macOS Seatbelt profile **covering Apple Silicon**; fix sandboxed-process reaping. Likely needs an upstream Servo patch to make the sandbox pluggable. | (1) | 1–2 quarters; upstream coordination |
| 3 | Windows AppContainer + Job Object + token restriction + mitigation policies. Net-new. | (1) | 1–2 quarters |
| 4 | Native-egui chrome (done) + embedded `gator://` internal-content scheme; residual: `js_string` hardening for the find-in-page content-JS path. | — (parallel) | done / small residual |
| 5 | Auto-update + signing + TUF + CI security gates. | — (parallel) | 1 quarter + per-platform CA/cert setup |
| 6 | Safe-Browsing-equivalent (local prefix lists via Lyku) + download protection + security UI. | — (parallel) | 1 quarter |
| 7 | Phase B site-keying (scheme+eTLD+1), origin checks on IPC; then Phase C OOPIF; Phase D GPU/net brokers. | (1)(2)(3) | multi-quarter, partly upstream |

The unifying strategic point, consistent with the Verso lesson already baked into
`docs/ARCHITECTURE.md`: **the sandbox/process code lives in Servo's
`constellation`/`servo` crates, not the embedder.** navgator's security therefore inherits
the Servo-sync maintenance risk *at its most safety-critical layer*. Budget for either
**upstreaming a pluggable sandbox interface** (best) or **carrying a small, reviewed,
deliberately-bumped patch** — and never float the gaol/sandbox dependency.

---

## 6. Concrete file references (ground truth)

- navgator single-process / native-egui chrome / `gator://` internal scheme / IPC: `/raid/NavGator/crates/navgator/src/main.rs` (`load_web_resource`/`render_gator_welcome`, `js_string`, IPC `start_ipc`). (The old `file://` chrome / `chrome_url` references are obsolete post-pivot.)
- Servo sandbox profiles + spawn + gate stubs: `…/ed1af70/components/constellation/sandboxing.rs`.
- Servo process reaping TODO: `…/components/constellation/process_manager.rs:28-32`.
- Process-per-Host event-loop keying: `…/components/constellation/constellation.rs:260-261, 842-969`.
- Multiprocess on/off + in-process vs in-thread spawn: `…/components/constellation/event_loop.rs:116-119, 153-185`.
- Content-process entry + ChildSandbox activation: `…/components/servo/servo.rs:1292-1380`.
- Embedder multiprocess wiring example: `…/components/servo/tests/multiprocess.rs:46-61`.
- TLS / cert verifier / override / ignore-errors: `…/components/net/connector.rs:460-585`.
- Mixed content: `…/components/net/fetch/methods.rs:450-497, 1211-1381`.
- CSP enforcement: `…/components/script/dom/document/document.rs:19, 1729-1745, 3680+`.
- CORS / SRI / HSTS: `…/components/net/fetch/cors_cache.rs`, `subresource_integrity.rs`, `hsts.rs`.
- gaol seccomp allowlist: `…/registry/src/…/gaol-0.2.1/platform/linux/seccomp.rs:116-151`.
