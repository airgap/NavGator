# Sandbox Rework: Replace gaol with a Maintained Rust Sandbox

> Status: PLAN. Keep the Servo engine; replace the abandoned `gaol` OS-confinement
> layer with a maintained, self-applied Rust sandbox (Landlock + seccompiler on
> Linux), default-on for Linux x86-64 as the 1.0 bar. macOS and Windows are
> post-1.0.
>
> Fork checkout referenced throughout (rev `33abd93`):
> `/home/nicole/.cargo/git/checkouts/swervo-2d3e259cb94efe40/33abd93/`
> NavGator embedder: `/raid/NavGator/crates/navgator/` and `crates/navgator-engine/`.

---

## 0. Context recap (verified against the fork at 33abd93)

- **Multiprocess content isolation is SHIPPED and default-on.** `Opts.multiprocess`
  is set `true` by NavGator's embedder (`crates/navgator/src/main.rs:4221`). The
  constellation re-execs the binary with `--content-process <token>`; `main.rs:81-83`
  hands off to `run_content_process` before GUI init. This half works and is the
  larger, always-safe security win.
- **OS confinement (gaol) is OPT-IN and broken on this class of host.**
  `sandbox_enabled()` (`main.rs:772-775`) gates `Opts.sandbox` on
  `linux+x86_64 && NAVGATOR_SANDBOX` set. It is opt-in precisely because gaol
  **hard-panics** the constellation when unprivileged user-namespace creation is
  denied.
- **Why gaol fails here, confirmed on this host:** gaol's Linux path = unprivileged
  user namespace + PID/IPC/UTS/NET namespaces + chroot tmpfs jail (parent side) +
  capset drop + seccomp-bpf (child side). The parent calls
  `unshare(CLONE_NEWUSER|CLONE_NEWPID)` wrapped in `assert!(...==0)`. This host has
  `kernel.unprivileged_userns_clone=1` and a large `max_user_namespaces`, yet
  `kernel.apparmor_restrict_unprivileged_userns=1` — Ubuntu's AppArmor mediation of
  `unshare(CLONE_NEWUSER)` — makes the `unshare` EPERM at a layer the sysctls do not
  reflect. The failure is therefore **undetectable in advance and unrecoverable**
  (it is an `assert!`/`.expect()`, not a `Result`).
- **Host facts that shape the plan:** kernel `6.8.0` (Landlock ABI v1–v4 available:
  filesystem + TCP network confinement); `cat /sys/kernel/security/lsm` =
  `lockdown,capability,landlock,yama,apparmor` (Landlock **is** in the active LSM
  list, so it will enforce); `media-gstreamer` is **enabled** in NavGator's build
  (`crates/navgator-engine/Cargo.toml:19`), so the content-process fingerprint
  includes GStreamer; `js_jit` stays on, so SpiderMonkey JIT W^X is in scope.

This is the central structural insight that makes the whole rework tractable:
**Landlock and seccomp are self-applied by the unprivileged process** (gated only by
`prctl(PR_SET_NO_NEW_PRIVS,1)`). They never call `unshare`, never create a namespace,
never `chroot`, never `capset`. The exact AppArmor mediation that EPERMs gaol is
**not on the Landlock code path at all.** Replacing gaol with Landlock+seccomp does
not "work around" the bug — it sidesteps the entire mechanism that triggers it, which
is why the replacement can be **default-on** where gaol could only ever be opt-in.

---

## 1. Goal & Non-Goals

### "Done" for 1.0
1. **Keep the Servo engine.** No engine swap; this is a confinement-layer change only.
2. **Remove `gaol` entirely** from the workspace (`Cargo.toml:98`,
   `components/constellation/Cargo.toml:71`, `components/servo/Cargo.toml:159`) once
   both the Linux and macOS code paths are off it (the gaol `Profile`/`Operation`
   types are shared across cfg branches, so removal must be atomic per platform pair).
3. **Linux x86-64 content sandbox is ON BY DEFAULT** (the 1.0 bar), via Landlock
   (filesystem + TCP) + seccompiler (syscall filter), self-applied in the content
   process. `NAVGATOR_SANDBOX` flips from an opt-in flag to a kill-switch (`=0`).
4. **Never panic on sandbox-unavailable.** If Landlock/seccomp cannot be installed
   (old kernel, LSM not in `lsm=`, container restriction), the process **logs and
   continues** (or refuses to start — see §3, a security decision), but it does
   **not** crash. This is the explicit anti-gaol requirement.
5. **Real content still renders** under the sandbox: a font-heavy page, WebGL, and a
   top-sites smoke corpus all pass with the sandbox on.
6. **Confinement is proven, not assumed:** a negative-capability self-test
   (read `~/.ssh`, connect an arbitrary socket, `fork`/`exec`) demonstrably DENIED
   inside a real content process, wired into CI.

### Non-goals (explicitly out of scope for 1.0)
- **macOS and Windows confinement.** macOS stays on gaol until the post-1.0 Seatbelt
  port (cheap); Windows stays unsupported (`sandbox=true` is a `panic!`/`exit(1)`
  today) until a dedicated post-1.0 milestone (expensive). Shipping 1.0 Linux-only is
  **no regression** — those platforms have no working content sandbox today either.
- **Restoring gaol's PID/IPC/NET-namespace isolation and chroot.** Landlock+seccomp
  give filesystem + syscall + TCP confinement, **not** PID-namespace process
  isolation, IPC-namespace isolation, full (UDP/raw) network-namespace isolation, or a
  `/` remap. This is a deliberate, documented scope reduction (see §9). The mitigation
  is the already-shipped multiprocess split, with an **optional** best-effort outer
  namespace layer deferred to "nice to have" (and, unlike gaol, allowed to fail
  silently).
- **A general syscall-interposition open()-broker** (Firefox/Chromium style). Servo
  already brokers by *service* (net, file-picker, fonts, WebGL, storage) over IPC, so
  no syscall broker is needed (see §5).
- **arm64 (Linux or Apple Silicon non-macOS) content sandbox.** Today's cfg stubs
  `exit(1)`/`panic!` there; out of scope until demanded.

---

## 2. Architecture: the swap

### 2.1 The four integration points (all in Servo crates → carried fork patch)

The entire gaol footprint is tiny and isolated. Verified:

| # | Role | Location | gaol today |
|---|------|----------|------------|
| 1 | **Child activation** | `components/servo/servo.rs:1318-1320` (gate) → `:1388-1392 create_sandbox()` | `ChildSandbox::new(content_process_sandbox_profile()).activate().expect("Failed to activate sandbox!")` — the hard-panic site |
| 2 | **Parent confined spawn** | `components/constellation/sandboxing.rs:190-248 spawn_multiprocess()`, branch at `:221` | `Sandbox::new(profile).start(&mut command)` → `Process::Sandboxed(pid)`; the parent-side `unshare` EPERM origin |
| 3 | **The profile** | `components/constellation/sandboxing.rs:54-129 content_process_sandbox_profile()` (macOS `:54-94`, Linux `:96-129`, stub `:131-146`) | Builds `gaol::profile::{Operation, PathPattern, Profile}` |
| 4 | **Platform stubs** | `sandboxing.rs:131-146` + `servo.rs:1394-1406` | `process::exit(1)` / `panic!` for Windows/arm/etc. |

Cargo pins (3): `Cargo.toml:98 gaol="0.2.1"`,
`components/constellation/Cargo.toml:71`, `components/servo/Cargo.toml:159` (the
latter two `gaol = { workspace = true }` under verbose, **identical**
`[target.'cfg(...)'.dependencies]` predicates).

Export surface (1): `components/constellation/lib.rs:27`
`pub use crate::sandboxing::{UnprivilegedContent, content_process_sandbox_profile};`
— `content_process_sandbox_profile` is the **only** sandbox symbol crossing the crate
boundary (consumed by `servo.rs:65`). `spawn_multiprocess` is crate-private.

Timing (load-bearing): `create_sandbox()` runs in `run_content_process` **after**
opts/prefs arrive over IPC (`servo.rs:1314-1315`) but **before** `script::init()`
(`servo.rs:1322`). So SpiderMonkey JIT bring-up, thread creation, fetch/BHM/script
threads all happen **inside** the cage. The replacement self-restriction must stay at
exactly this point.

Already-pluggable inputs (reuse unchanged): the path allow-list comes from the
embedder via `components/shared/embedder/resources.rs:88-93`
`sandbox_access_files()` / `sandbox_access_files_dirs()` (trait `:171-177`, default
impl empty). A Landlock policy consumes these exact two functions, so resource-path
plumbing needs **zero change**.

### 2.2 The seam: pluggable-but-minimal

We adopt **option (b) hard-replace shaped with one internal seam** — the most
maintainable cut, and the recommendation every research stream converged on:

1. **New fork-local module** (e.g. `components/constellation/sandbox_backend.rs`) owns
   *all* OS-specific policy + apply code and a backend-neutral `Policy` type. New files
   **never conflict on rebase**.
2. **`content_process_sandbox_profile()` and `create_sandbox()` become thin
   one-line delegators** into that module. These ~4 delegating lines are the only
   conflict-prone surface, and they conflict trivially.
3. Internally the module exposes a thin seam:
   ```
   pub struct Policy { /* fs read paths, fs exec paths, net intent, seccomp posture */ }
   pub fn content_process_policy() -> Policy;          // built from resources::sandbox_access_files[_dirs]() + built-ins
   pub fn apply_sandbox(policy: &Policy) -> SandboxOutcome;  // Linux: landlock restrict_self + seccomp apply_filter
   ```
   `SandboxOutcome` is `{ fs: RulesetStatus, seccomp: applied|skipped, degraded: bool }`
   — inspected, logged, never `.expect()`ed.
4. **Defer the full public embedder `trait Sandbox`** (option (a)) until the macOS
   backend lands. The internal seam above is already most of the way there; promoting
   it to a public trait is cheap when a second backend exists to justify the
   abstraction.

This gives option (b)'s diff size with option (a)'s isolation. Do **not** reformat the
verbose duplicated `target.cfg` predicates — touching them maximizes rebase conflicts
against upstream churn for zero functional gain.

### 2.3 Parent side simplifies (and a latent bug gets fixed for free)

Because Landlock+seccomp **self-apply in the child**, the parent has nothing to do.
`spawn_multiprocess` drops the gaol `Sandbox::start` branch (`sandboxing.rs:221-231`)
and **always** uses the plain `process::Command` spawn (the existing `else` branch at
`:232-242`, and the unsupported-platform variant at `:157-178` is a ready template).
This returns a real `std::process::Child`, so:

- The child becomes `Process::Unsandboxed(Child)`, letting us **delete the
  `Process::Sandboxed(u32)` arm** (`process_manager.rs:13,20,29-32`) whose `wait()` is
  a no-op `warn!("wait() is not yet implemented for sandboxed processes.")` — a latent
  **zombie/reaping bug**. Real `Child::wait()` is restored for free. (Note: the variant
  name `Unsandboxed` becomes a misnomer once the child self-confines; rename to
  `Spawned(Child)` or similar, but keep the diff small.)

### 2.4 What is a carried fork patch vs. upstreamable

**Carried fork patch** (lives in `components/servo` + `components/constellation`):
the Landlock/seccomp backend module + the delegating one-liners + the Cargo dep swap.
Keep it additive, on a dedicated `patches/sandbox-landlock` topic branch, with a
`PATCHES.md` ledger entry per the fork's diff-minimization policy, and covered by the
fork-drift canary (`scripts/sync-forks.sh --check`).

**Upstreamable to Servo now, independent of NavGator** (shrinks the carried patch
regardless of backend choice — do these as standalone PRs):
1. **Remove the `.expect("Failed to activate sandbox!")` hard-panic**
   (`servo.rs:1390`) in favor of graceful degradation. This is a real upstream bug:
   sandbox-unavailable should not crash the browser.
2. **Implement `Process::wait()` for the sandboxed variant** /
   collapse to `Child` reaping (`process_manager.rs:24-34`).

**Upstreamable later, opinionated (needs an RFC/discussion):** the gaol→Landlock
migration itself, and especially the **pluggable `Sandbox` trait** (option (a)). The
trait shape is the most upstream-palatable form because it lets Servo keep behavior
while embedders choose a backend; it is also the single highest-leverage maintenance
move (if upstreamed, NavGator carries a trait *impl* in `navgator-engine` and **zero**
engine-core patch). This conflicts with NavGator's standing "no upstreaming" posture
and needs a deliberate owner exception — flag it, don't assume it.

---

## 3. Linux mechanism: Landlock + seccompiler

### 3.1 Landlock — why it dodges the userns panic

Landlock is a stackable LSM. The unprivileged process restricts **itself** via three
syscalls — `landlock_create_ruleset(2)`, `landlock_add_rule(2)`,
`landlock_restrict_self(2)` — the last gated only by `prctl(PR_SET_NO_NEW_PRIVS,1)`,
which any process may set. **No `unshare`, no `CLONE_NEWUSER`, no namespace, no
`chroot`, no `mount`, no `capset`.** The AppArmor/container userns mediation that
EPERMs gaol is simply not reachable. `restrict_self()` returns success for an ordinary
user on a stock kernel where Landlock is in the active LSM list — which this host
already satisfies (`lsm=...,landlock,...`).

This maps cleanly onto the existing **child** hook (`create_sandbox`): "the process
restricts itself after IPC bootstrap, before `script::init()`" is exactly where
`restrict_self()` belongs.

**`rust-landlock` API shape** (builder over the three syscalls):
```rust
let abi = ABI::V4; // best the host supports; see compat below
let status = Ruleset::default()
    .set_compatibility(CompatLevel::BestEffort)        // default for the high-level Ruleset
    .handle_access(AccessFs::from_all(abi))?
    .handle_access(AccessNet::from_all(abi))?          // ABI v4+ (kernel 6.7), TCP only
    .create()?
    .add_rule(PathBeneath::new(PathFd::new("/usr/share/fonts")?, AccessFs::ReadFile | AccessFs::ReadDir))?
    // ... one add_rule per allowed path ...
    .restrict_self()?;                                  // -> RestrictionStatus
```
Default semantics: among the access types you *handle*, anything not granted by a rule
is **denied**. Filesystem is inode/path-based via `PathFd`.

**Kernel ABI ladder** (the crate's `ABI` enum):
- v1 = 5.13: filesystem read/write/exec/make/remove on path hierarchies.
- v2 = 5.19: `Refer` (controlled cross-dir rename/link).
- v3 = 6.2: `Truncate`.
- **v4 = 6.7: NETWORK — `AccessNet::BindTcp` / `ConnectTcp` (TCP only).** This host
  (6.8) **has** it. Landlock network is **TCP bind/connect only** — it does **not**
  cover UDP, raw sockets, AF_UNIX, or DNS-over-UDP; those need seccomp if you want
  them blocked.
- v5 = 6.10: `IoctlDev`.
- v6 = 6.12: `Scope::AbstractUnixSocket` / `Scope::Signal` — a *partial* late
  substitute for an IPC namespace (abstract UDS + signal targets only; still no
  PID-namespace process isolation). Not on this host (6.8); treat as future.

### 3.2 seccompiler — the syscall-filter half

`seccompiler` (Firecracker/rust-vmm, Apache-2.0) is the maintained replacement for
gaol's hand-rolled BPF. Build a `SeccompFilter` from
`BTreeMap<i64 syscall_nr, Vec<SeccompRule>>` + a default action + a match action +
target arch (`std::env::consts::ARCH → TargetArch`). `SeccompRule`s match on argument
values/masks (`SeccompCondition` with `Eq/Le/MaskedEq/...` on args 0–5), reproducing
gaol's arg-matching (e.g. `socket(domain==AF_UNIX)`, `ioctl(req==FIONREAD|FIOCLEX)`).
Compile via `TryInto<BpfProgram>`, install via `seccompiler::apply_filter(&bpf)` (which
sets `PR_SET_NO_NEW_PRIVS` and `seccomp(SECCOMP_SET_MODE_FILTER)`).

Actions: `Allow`, `Errno(u32)`, **`Log`** (SECCOMP_RET_LOG — allow + kernel-audit),
`Trace`, `Trap`, `KillThread`, `KillProcess`. gaol shipped a fixed `SECCOMP_RET_KILL`
with a ~21-syscall list — brittle for a whole web engine and the reason gaol stayed
fragile. We use the **`Log → harvest → Errno → KillProcess` ramp** (see §4.3), never
shipping `KillProcess` from day one.

### 3.3 What Landlock+seccomp confine vs gaol

| Capability | gaol | Landlock + seccompiler | Net |
|---|---|---|---|
| Filesystem read/write confinement | chroot tmpfs bind-mount jail | inode-precise path rules (deny at open/stat) | **Parity (arguably tighter); different leakage** (full path namespace stays *visible*, denied at access time; no `/` remap) |
| Syscall surface reduction | seccomp-bpf KILL, ~21 syscalls | seccompiler, tuned + arg-aware, graduated actions | **Parity → better** |
| TCP network restriction | NET namespace (all interfaces gone) | Landlock v4 BindTcp/ConnectTcp (kernel 6.7+) | **Partial** (TCP only; UDP/raw via seccomp) |
| PID-namespace process isolation | `CLONE_NEWPID` | none (v6/6.12 only scopes signals) | **LOST** — mitigated by multiprocess split |
| IPC-namespace isolation | `CLONE_NEWIPC` | none pre-6.12 | **LOST** |
| `/` remap / path hiding | chroot | none | **LOST** (confidentiality of contents preserved; existence/metadata differs) |
| Requires unprivileged userns | **YES (the bug)** | **NO** | **Win** |
| Degrades gracefully | No (`assert!`) | Yes (`RulesetStatus` + BestEffort) | **Win** |

### 3.4 Runtime detection + graceful degradation (never panic)

This is the load-bearing behavioral change. Drive everything off rust-landlock's
`CompatLevel::BestEffort` + the `RestrictionStatus.ruleset` returned by
`restrict_self()`:

- `RulesetStatus::FullyEnforced` — all requested FS/net access types enforced. Log at
  info, proceed.
- `RulesetStatus::PartiallyEnforced` — kernel older than requested ABI; some access
  types silently dropped (e.g. ask v4 net on a 5.15 kernel → get v1 FS, net dropped).
  Log at warn with *what* dropped, proceed. (On those hosts, seccomp socket-arg
  filtering must independently carry network confinement.)
- `RulesetStatus::NotEnforced` — Landlock absent (kernel <5.13 or not in `lsm=`). Log
  at warn, proceed with seccomp-only.

seccomp install (`apply_filter`) is wrapped in its own `Result`: on `Err`, log and
continue (do not panic). `BestEffort` means we **request** the highest ABI (v6) and
take whatever the running kernel grants — no preflight version checks, no host
detection, no failure class like gaol's.

**Open policy decision (security-relevant, requires an explicit owner call):**
warn-and-continue (fail-open) vs refuse-to-start (fail-closed) when **both** layers
report NotEnforced/skipped. Recommendation: **default warn-and-continue** so the
container/AppArmor case that broke gaol can no longer crash, but expose
`NAVGATOR_SANDBOX=require` to force fail-closed for hardened deployments, and surface
the degraded state in `gator://` diagnostics so it is never silent to the operator.

---

## 4. The content-process policy

### 4.1 What a content process owns vs delegates (the fingerprint)

Verified in the fork. A Servo content process is **near-pure compute + IPC**:

- **Network/DNS/TLS: fully brokered.** The in-content "fetch thread"
  (`servo.rs:1329`) is only an IPC marshaller forwarding `CoreResourceMsg::Fetch`
  (`shared/net/lib.rs:891-897`) to the `CoreResourceThread` in the **parent**
  (`new_resource_threads` at `servo.rs:976-986`). hyper/TcpStream/TLS/cookies/HTTP
  cache all live in the parent. **Content needs no AF_INET socket, no
  connect/bind/sendto, no `/etc/resolv.conf`, `/etc/hosts`, `/etc/ssl`, no NSS.** This
  is the single biggest tightening win.
- **GPU/WebRender/WebGL/WebGPU: all parent.** `script`/`layout` import only
  `webrender_api` *data* types. The Renderer + surfman + WebGLThread + wgpu live in
  `components/paint` (main process); content reaches WebGL over `webgl_chan` IPC.
  **Grep confirms zero `/dev/dri`, `/dev/nvidia*`, `renderD*`, `card0` references
  under `components/`.** Content needs **no GPU/DRM device nodes or ioctls** — this
  removes the single hardest cage problem. (This holds *for the content process*;
  the parent/paint process is unconfined and does the GPU work.)
- **Fonts: discovery parent, byte-loading IN-CAGE (the one real FS need).**
  fontconfig enumeration runs only in the parent `SystemFontService`
  (`servo.rs:1220`). But the content-side `FontContext` loads **local** font BYTES
  itself: `get_font_data` returns `None` for `FontIdentifier::Local`
  (`font_context.rs:175-182`), and freetype does `File::open` + `Mmap::map` on the
  resolved path (`platform/freetype/font.rs:124-133`) plus `FreeTypeFace::new_from_file`
  (`:111`). **Web fonts** arrive as IPC shared memory
  (`FontData(Arc<GenericSharedMemory>)`, `shared/fonts/lib.rs:127`) and need no FS.
  → Content must read+mmap on-disk **system font files** (host-dependent dirs). This
  is the hard part; two designs in §5.
- **SpiderMonkey JIT W^X: PROT_EXEC is mandatory.** mozjs 140 ESR, JIT ON by default
  (`prefs.rs:485-487`; NavGator keeps `js_jit`). Linux JIT
  (`ProcessExecutableMemory.cpp`) reserves `mmap(PROT_NONE, MAP_ANON)` and flips
  `mprotect` between `PROT_READ|PROT_WRITE` and `PROT_READ|PROT_EXEC`
  (writeProtectCode=true, the W^X default), or maps RWX directly if writeProtectCode
  is false. **The seccomp filter MUST allow `mmap`/`mprotect` with `PROT_EXEC`** —
  Servo has no JIT-write broker. This materially limits how strict the syscall cage
  can be. The only way to deny PROT_EXEC is `js_disable_jit` (interpreter-only, large
  perf hit) — out of scope for default builds.
- **ipc-channel (0.22.0) transport: must allow.** Unix transport uses
  `socketpair(AF_UNIX, SOCK_SEQPACKET)`, `sendmsg`/`recvmsg` with `SCM_RIGHTS` fd
  passing, and `memfd_create` + `ftruncate` + `mmap(MAP_SHARED)` for large messages
  (**no `/dev/shm` fallback in this version**). Landlock does **not** govern AF_UNIX
  socketpair or anonymous memfd, so **IPC keeps working under Landlock
  automatically** — another reason Landlock fits cleanly. seccomp must allow these.
- **GStreamer (ENABLED in NavGator) balloons the fingerprint.**
  `media_platform::init()` runs **in-content** (`servo.rs:1326`); with
  `media-gstreamer` (NavGator's build) the Linux backend is
  `ServoMedia::init::<GStreamerBackend>()`. GStreamer registry init can `dlopen`
  plugins (`open`/`openat`/`mmap(PROT_EXEC)` of `/usr/lib/.../gstreamer-1.0/*.so`) and,
  for playback/capture, touch audio nodes (`/dev/snd`, PipeWire/Pulse sockets). This is
  a **hard, host-variable** part. Options: (a) widen the FS/socket policy to the
  GStreamer plugin dir + audio paths; (b) broker/relocate media out of the content
  process; (c) (best long-term) keep media in a separate, separately-confined process.
  For 1.0, **(a) with an explicit, audited allow-list** is the pragmatic path; flag
  media-out-of-process as a hardening follow-on.
- **BHM sampler: feature-gated, CONFIRM.** The OS sampler is behind the `sampler`
  feature (`background_hang_monitor/Cargo.toml:39`). If compiled in, content seccomp
  must allow `rt_sigaction(SIGPROF)`, `tgkill`, `gettid`, semaphores. **Action:
  confirm whether NavGator's build enables `sampler`** before finalizing the filter
  (it is not enabled by NavGator's direct `servo` feature set, but transitive
  enablement must be checked).
- **Baseline FS/syscalls every content process needs:** read `/dev/urandom` (and
  `getrandom(2)`); read `resources::sandbox_access_files[_dirs]()` (HSTS list,
  public-suffix list, bad-cert HTML, UA stylesheets — concrete paths from NavGator's
  `ResourceReader`, **confirm bundled-in-binary vs on-disk**); read+exec the self-exe
  (re-exec path) and its `.so` deps (freetype, fontconfig, harfbuzz, ICU, GStreamer
  plugins); threading/runtime syscalls (`clone`/`clone3`, `futex`, `mmap`/`munmap`/
  `mprotect`, `brk`, `rt_sigprocmask`/`rt_sigaction`, `sched_yield`,
  `nanosleep`/`clock_nanosleep`, `epoll`, `eventfd`, `prctl(PR_SET_NAME)`,
  `set_robust_list`, `gettid`, `rseq`, `membarrier`).

### 4.2 The allow-set (initial draft, to be made precise empirically)

**Filesystem (Landlock `PathBeneath` rules):**

| Path | Access | Why |
|---|---|---|
| `/dev/urandom` | ReadFile | randomness (or just allow `getrandom`) |
| install dir of the NavGator binary | ReadFile + Execute | self re-exec + dlopen |
| system lib dirs (`/usr/lib`, `/lib`, multiarch dirs) | ReadFile + Execute | dlopen freetype/fontconfig/harfbuzz/ICU/GStreamer |
| `resources::sandbox_access_files()` (Literal) | ReadFile | embedder resources |
| `resources::sandbox_access_files_dirs()` (Subpath) | ReadFile + ReadDir | embedder resource dirs |
| system font dirs (`/usr/share/fonts`, `/usr/local/share/fonts`, `~/.fonts`, `~/.local/share/fonts`, fontconfig cache) | ReadFile + ReadDir | local font byte-loading (design A; eliminated by design B) |
| GStreamer plugin dir (`/usr/lib/.../gstreamer-1.0`) | ReadFile + Execute | media plugin dlopen (media-gstreamer is ON) |
| audio: `/dev/snd/*` and/or PipeWire/Pulse socket dir | ReadFile + WriteFile | media playback (host-variable; audit) |

**Network (Landlock v4, kernel ≥6.7 — this host qualifies):** handle
`AccessNet::{BindTcp, ConnectTcp}` and add **no** allow rules → all TCP bind/connect
denied (content does no sockets; net is brokered). On <6.7 hosts this silently drops to
nothing and seccomp must deny `socket(AF_INET/AF_INET6)`/`connect`/`bind` instead.

**Syscalls (seccompiler) — default `Errno(ENOSYS)` (not Kill, initially), ALLOW:**
- IPC: `socketpair`, `sendmsg`, `recvmsg`, `sendmmsg`, `recvfrom`, `sendto`,
  `memfd_create`, `ftruncate`, `mmap`, `munmap`, `mprotect` (incl. PROT_EXEC),
  `close`, `dup`/`dup2`/`dup3`, `fcntl`.
- Threading/runtime: `clone`, `clone3`, `futex`, `set_robust_list`, `rseq`,
  `membarrier`, `sched_getaffinity`, `sched_yield`, `nanosleep`, `clock_nanosleep`,
  `clock_gettime`, `epoll_create1`/`epoll_ctl`/`epoll_wait`/`epoll_pwait`, `eventfd2`,
  `poll`/`ppoll`, `brk`, `getrandom`, `getuid`/`getpid`/`gettid`,
  `prctl(PR_SET_NAME|PR_SET_NO_NEW_PRIVS)`, `rt_sigaction`, `rt_sigprocmask`,
  `rt_sigreturn`, `sigaltstack`, `exit`, `exit_group`.
- File-read set: `openat`/`openat2`, `read`/`pread64`, `lseek`, `fstat`/`statx`/`stat`,
  `newfstatat`, `readlink`/`readlinkat`, `access`/`faccessat`/`faccessat2`,
  `getdents64`.
- arg-restricted: `socket` only `AF_UNIX` (deny `AF_INET`/`AF_INET6`/raw);
  `ioctl` only the small device-control set freetype/IPC actually need.
- If `sampler` is on: `tgkill`, `gettid` (already), `rt_sigaction(SIGPROF)`,
  `sem_*`/futex (already).
- If GStreamer audio is in-content: whatever `/dev/snd` ioctls and socket ops the audio
  backend issues (must be harvested, not guessed).

**DENY (default action target):** all `AF_INET`/`AF_INET6`/raw socket creation,
`connect`/`bind` to inet, `ptrace`, `process_vm_readv`/`writev`, `bpf`, `keyctl`,
`io_uring_*` (unless harvest proves tokio needs it — then allow narrowly), `mount`,
`unshare`, `setns`, `chroot`, arbitrary `ioctl`, GPU/DRM ioctls.

### 4.3 Deriving it empirically (mandatory — do not transcribe gaol's list)

gaol's old Linux profile (`sandboxing.rs:108-129`) is **misleadingly minimal** (only
`/dev/urandom` + resources) because gaol layered chroot+seccomp defaults underneath; a
naive 1:1 port would be far too permissive (no syscall filter) or far too restrictive.
Derive the real set empirically:

1. **seccomp Log-mode bring-up.** Build the filter with default action `Log`
   (`SECCOMP_RET_LOG`) — allow everything, audit it. Expose `NAVGATOR_SECCOMP=log|enforce`
   so this is switchable in the field for incident triage (gate `log` capability out of
   release builds if the owner requires zero debug surface — open question).
2. **Exercise real workloads:** the top-sites smoke corpus + a WPT subset + a
   font-heavy page + a `<video>`/`<audio>` page (GStreamer path) + IndexedDB
   (rusqlite) + WebGL2 (ANGLE). Run under `strace -f` in parallel for cross-check.
3. **Harvest** the logged syscall set from `dmesg`/`auditd` `SECCOMP` records. This
   catches the modern set gaol's 1970s list misses (`clone3`, `rseq`, `openat2`,
   `statx`, `membarrier`, `faccessat2`, `clock_nanosleep`, `io_uring` if present).
4. **Promote** the default action `Log → Errno(ENOSYS)` once the allow-list is
   complete, then — only after soak — to `KillProcess`. Likewise Landlock: probe ABI
   with BestEffort and confirm `FullyEnforced` on the supported-kernel matrix.
5. Repeat per supported-kernel/distro/GPU-driver point in the CI matrix; a syscall
   missed only on certain hosts causes intermittent, host-specific crashes.

---

## 5. The broker

**Do NOT build a Firefox/Chromium syscall-interposition `open()`-broker.** Servo
already brokers privileged ops **by service**, not by syscall: every privileged
handle a content process holds is an IPC endpoint to a parent-side thread, carried in
`InitialScriptState` (`shared/script/lib.rs:368-413`) and auto-upgraded to real
cross-process `ipc-channel` in multiprocess mode (`generic_channel/mod.rs:42,90`;
it **panics** if an in-process Crossbeam channel reaches a multiprocess child,
`:120-127`). The "broker" *is* the existing constellation/resource/embedder service
threads. Adding a brokered op = adding a typed variant to an existing `*ThreadMsg`
enum, not standing up new plumbing.

**Already brokered — do nothing:** networking/DNS/TLS (CoreResourceThread),
user-picked `<input type=file>` + blob/file reads (FileManager opens files in the
parent, streams `ReadFileProgress` chunks back), WebGL/GPU (paint side), storage,
cookies, cache, bluetooth.

**The one real in-cage FS need: local system fonts.** Two designs:

- **Design A (1.0, simplest): widen the Landlock policy** to read-allow the font dirs
  the parent `SystemFontService` already enumerates (plus fontconfig cache). Mirrors
  the macOS gaol profile's font whitelist. **Risk:** host-specific/symlinked/XDG-custom
  font dirs can be missed → missing-glyph rendering that is hard to reproduce. The dir
  list **must be derived from the same source `SystemFontService` uses**, not
  hardcoded. **Confirm** fontconfig does **not** initialize inside the content process
  (it should be parent-only; if it runs in-cage it would need `~/.cache/fontconfig` +
  `/etc/fonts` reads).
- **Design B (hardened mode, post-1.0): a real font-bytes broker.** Add
  `SystemFontServiceMessage::GetLocalFontData(LocalFontIdentifier) -> IpcSharedMemory`
  (parallel to existing `GetFontTemplates`/`GetFontInstance`), have the parent do the
  `File::open`+read, and route `new_from_local_font_identifier`
  (`freetype/font.rs:100-128`) through the existing `new_from_data` byte path instead
  of opening a file. This removes **all** font FS access from the cage (zero FS read
  surface beyond binary+resources), mirroring Firefox. **Cost:** touches the freetype
  platform impl, adds synchronous IPC round-trips on the font hot path (needs a
  parent-side byte cache to avoid layout/paint latency regression), and the
  `IpcSharedMemory` buffer must satisfy FreeType's memory-face lifetime without
  copying. **Open:** whether the legacy canvas `font_data_and_index` path
  (`font.rs:366-380`) opens additional font files at runtime.

**Randomness:** `/dev/urandom` Landlock read-allow or `getrandom(2)` seccomp allow —
no broker.

**Guards to add** (so the broker assumption can't silently rot): a test asserting the
content process opens **no** inet sockets; documentation that net stays in the parent
`CoreResourceThread`; any new brokered message type must be IPC-serializable
end-to-end or the content process hard-crashes (`generic_channel` panic).

---

## 6. Phased milestones (M0..M5)

Each milestone is independently shippable behind the existing Jenkins matrix
(Linux required-gate; macOS/Windows UNSTABLE-until-green). Every gate is a **hard
exit criterion**, not a checklist.

### M0 — Today (baseline, SHIPPED)
- **State:** multiprocess default-on; gaol opt-in behind `NAVGATOR_SANDBOX`
  (`main.rs:772-775`), broken on this host (userns EPERM panic).
- **Honest claim:** "Process isolation per tab; OS confinement experimental and
  unavailable on AppArmor-restricted hosts."

### M1 — Upstream-shaped fixes + escape-test harness (no behavior change yet)
- **Deliverables:**
  - The **`--sandbox-selftest` content-process branch** in `main.rs` (mirrors the
    existing `--content-process` branch at `main.rs:81-83`): self-applies the
    production policy, runs the negative-capability battery, exits 0 iff all forbidden
    ops are DENIED. This is the primary, deterministic, headless CI gate. (Must run
    inside a **real content process** — a `gator://` probe page would run in the
    **broker** at `main.rs` `load_web_resource` and falsely "pass" while testing
    nothing.)
  - Standalone upstream PRs: remove the `create_sandbox` `.expect` hard-panic; fix
    `Process::wait()` reaping.
- **Exit gate:** selftest harness builds and runs; the panic-removal PR is filed; CI
  green with gaol still wired (no functional change).

### M2 — Landlock + seccomp **opt-in** (Linux x86-64), behind `NAVGATOR_SANDBOX`
- **Deliverables:**
  - New `sandbox_backend.rs` module: `Policy`, `content_process_policy()`,
    `apply_sandbox()` (landlock `restrict_self` + seccompiler `apply_filter`).
  - `create_sandbox()` and `content_process_sandbox_profile()` delegate in one line
    each; **no `.expect`** — inspect `SandboxOutcome`, log, continue.
  - `spawn_multiprocess` drops the gaol branch → plain `Command` spawn →
    `Process::Unsandboxed(Child)`; delete the `Sandboxed(u32)` arm.
  - Cargo: keep gaol for now (A/B), add `landlock` + `seccompiler` under the Linux
    cfg.
  - seccomp in **Log mode**; FS allow-set per §4.2 (design A fonts).
- **Exit gate (the headline):**
  1. `--sandbox-selftest` passes: read `~/.ssh/id_rsa`, read `/etc/shadow`, read an
     unauthorized `$HOME` file, `connect()` an arbitrary TCP host, `fork`+`execve` a
     helper — **all DENIED**; exits nonzero if any succeeds.
  2. **Runs to completion on THIS host** (kernel 6.8, `apparmor_restrict_unprivileged_userns=1`)
     **without panicking** — the exact environment that crashes gaol. This single
     result is the proof the rework works.
  3. Headless smoke with sandbox forced on renders a font-heavy + WebGL page
     (non-blank pixels, no content-process crash).
  4. Top-sites corpus shows **zero** sandbox-on-only crashes vs sandbox-off.
  5. Tab-churn test (open/close 200 tabs) leaves no zombie/leaked PIDs (proves the
     `wait()` fix).

### M3 — Linux x86-64 **default-on** (the 1.0 bar)
- **Deliverables:**
  - Flip `Opts.sandbox` default to true on linux-x86_64 in NavGator's embedder
    (`main.rs:4225` → not gated on the env var); `NAVGATOR_SANDBOX=0` becomes a
    kill-switch, `NAVGATOR_SANDBOX=require` forces fail-closed.
  - seccomp promoted `Log → Errno`, then (post-soak) consider `KillProcess`.
  - Graceful-degradation path proven on a no-Landlock kernel (seccomp-only + log,
    never panic).
  - **Remove gaol from Linux** (keep it only on the macOS cfg branch until M4).
  - `cargo deny`/`cargo audit` updated; the gaol RUSTSEC advisory cleared for the
    Linux build.
- **Exit gate:** N weeks of M2 dogfood with zero confinement-caused render breakage on
  the supported-GPU matrix **and** the llvmpipe software-render fallback; the WPT
  enabled-feature bar passes with sandbox on (IndexedDB/WebGL2/media don't regress);
  graceful degradation demonstrated on an old-kernel CI runner; selftest still green.

### M4 — macOS (x86-64 + Apple Silicon) — post-1.0, cheap, do first
- **Deliverables:** lift gaol's ~50-line `.sb`-profile generator + `sandbox_init` FFI
  into `sandbox_backend.rs` (it's MIT/Apache), replacing only the gaol `Profile` type.
  Keep Servo's Mach-lookup (`com.apple.FontServer`)/`sysctl-read`/framework-subpath
  grants verbatim (`sandboxing.rs:54-94` is the spec). **No new parent-side launch
  path** — macOS confinement is entirely in-process (gaol's macOS `start()` is just
  `spawn()`). Cover Apple Silicon (today the `exit(1)` stub). **Remove gaol entirely**
  once macOS is ported (all three Cargo pins gone).
- **Do NOT use birdcage's macOS backend** — it is coarse (path allow/deny + net on/off)
  and lacks Mach-lookup/`sysctl-read`/per-framework grants Servo needs; it would
  **regress** macOS capability vs the working gaol profile.
- **Exit gate:** negative battery passes on macOS (both arches); fonts/framework text
  rendering intact; hardened-runtime/notarization compatible.

### M5 — Windows AppContainer — post-1.0, expensive, do last (own milestone)
- **Deliverables (greenfield — grep confirms NO CreateProcess/JobObject/integrity/
  AppContainer/restricted-token/Win32k code in `components/`):** parent-side broker on
  `windows-rs`: restricted token (Low-IL) + Job Object (kill-on-close) + AppContainer
  (low-box SID) + process-creation mitigation policies (ACG/CIG, no-child-process,
  Win32k lockdown). This gets a **coarse** sandbox quickly.
- **The hard 80%** (deferred unless renderer-grade confinement is required): brokered
  IPC + ntdll interception (the part that makes Chromium's policy granular). Servo has
  no broker IPC for FS/font/GPU, so a tight Windows sandbox either builds that plumbing
  or ships a deliberately loose policy. Vendoring Chromium's `sandbox/win` (Mozilla
  style: `chromium-shim` base + MSVC + FFI) is the alternative, person-months of work,
  and conflicts with the single-binary/pure-Rust preference (open question for the
  owner).
- **Exit gate:** AppContainer content process renders + negative battery (no FS outside
  profile, no arbitrary socket).

---

## 7. Cross-platform summary & sequencing

| Platform | Mechanism | Parent-side work | Effort | When |
|---|---|---|---|---|
| **Linux x86-64** | Landlock + seccompiler (self-applied) | none (plain spawn) | weeks | **1.0 (M2→M3)** |
| **macOS (both arches)** | Seatbelt `.sb` + `sandbox_init` (lifted from gaol) | none (in-process) | weeks | post-1.0 (M4) |
| **Windows** | AppContainer + Job + token + mitigations (`windows-rs`); maybe vendor Chromium | broker + target dance | **months** | post-1.0 (M5) |
| Linux/macOS arm64 stubs | (unchanged `exit(1)`/`panic!`) | — | — | as demanded |

Sequencing rationale: the cfg split already isolates platforms, so each ships
independently with no regression. Linux is cheap and unblocks the 1.0 security claim;
macOS is cheap (in-process, code liftable); Windows is the long pole and highest-risk
(no Servo precedent, no maintained pure-Rust parity crate). `sandbox_init` is
deprecation-annotated on macOS but is what gaol/Chromium/Firefox already use — **no
regression**; the modern App-Sandbox-entitlements path is a notarization/packaging
concern, flagged for the macOS roadmap, not a runtime change.

---

## 8. Verification strategy

Two independent halves, both wired into the existing Jenkins matrix; a too-tight
policy must surface as a **test regression, never a field crash**.

### 8.1 Confinement is proven (negative-capability)
- **Primary gate — `--sandbox-selftest` content-process mode** (deterministic,
  headless, no GPU, no page load): self-applies the production policy, then runs the
  battery and exits nonzero unless **every** op is DENIED:
  - read `~/.ssh/id_rsa`, `/etc/shadow`, an unauthorized `$HOME` file → EACCES/EPERM
    (Landlock) or SIGSYS/ENOSYS (seccomp on `openat2`).
  - `connect()` an arbitrary TCP host and an arbitrary Unix socket → denied.
  - `fork`/`execve` a helper binary → denied (no-new-privs + seccomp).
  - write outside the allowed set → denied.
- **Per-layer assertions** (not just end-to-end): on a no-Landlock kernel, assert the
  **seccomp-only** config still DENIES sockets/exec — Landlock silently no-ops on
  <5.13, so seccomp must independently hold the line and the test must prove it.
- **Must run inside a real content process.** A `gator://` page runs in the broker
  (`main.rs` `load_web_resource`) and would falsely pass. The selftest branch (or a
  debug-only in-content host hook, cfg'd out of release) is the only valid form.
- **Seccomp Log-mode harvest** (§4.3) as the bring-up instrument, not a gate.

### 8.2 Render-still-works (positive)
- **Headless smoke, sandbox ON** — extend the existing `xvfb-run ... LIBGL_ALWAYS_SOFTWARE=1
  GALLIUM_DRIVER=llvmpipe navgator` smoke stage with a sandbox-forced-on invocation
  loading a font-heavy + WebGL page; assert non-blank pixels / no content-process crash.
- **Top-sites compat corpus** — run twice (off vs on), diff render/crash outcomes; any
  on-only crash is a profile gap.
- **WPT subset under sandbox** — the enabled-feature WPT bar with sandbox on, so a
  too-tight profile breaking IndexedDB (rusqlite file I/O), WebGL2 (ANGLE), or
  media-gstreamer (decoder device access) surfaces as a WPT regression.
- **Reaping/lifetime** — the 200-tab churn test (no zombies/leaked PIDs).

---

## 9. Risks & mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| **Isolation regression vs gaol:** lose PID/IPC namespace isolation + chroot; content can still see host PIDs and (subject to Yama/DAC) signal/ptrace them. | High (threat-model) | **Document explicitly** as deliberate scope reduction. Mitigated by the already-shipped multiprocess split + seccomp denying `ptrace`/`process_vm_readv`. Optional later: a best-effort, **non-asserting** outer userns/bubblewrap layer allowed to fail silently (the inverse of gaol's `assert!`). Landlock v6 (6.12) recovers abstract-UDS/signal scoping when hosts catch up. |
| **JIT W^X tension:** seccomp must allow `mprotect(PROT_EXEC)` (and possibly RWX) because Servo has no JIT-write broker → cage can't enforce no-W^X. | Medium | Accept it for default builds; the only stricter option is `js_disable_jit` (perf hit), out of scope. Document. |
| **Local-font FS read surface** (design A) leaks host font layout into the policy; too-narrow breaks text, too-broad weakens isolation. | Medium | Derive the font-dir list from `SystemFontService`'s own source, not hardcode; offer design B (font-bytes broker) for hardened mode. |
| **seccomp completeness for a whole web engine + GStreamer**; a missed syscall under `KillProcess` crashes tabs host-specifically. | High (self-inflicted outage) | Mandatory `Log → harvest → Errno → Kill` ramp; never ship `KillProcess` from day one; matrix-wide harvest incl. media/IndexedDB/WebGL2 paths. |
| **GStreamer in-content** (NavGator enables `media-gstreamer`) → plugin dlopen + audio device/socket access widens the cage. | Medium-High | Audit the exact plugin dir + audio paths via Log-mode; long-term move media to a separate confined process. |
| **Kernel-version dependence:** Landlock net needs 6.7; <5.13 = no Landlock at all; silent downgrade. | Medium | BestEffort + `RulesetStatus` inspection + per-layer selftest; seccomp carries network confinement on old kernels. This host (6.8) is fine. |
| **Default-on startup failures on the GPU/driver long tail** — a profile gap becomes a silent no-start in a telemetry-free browser. | Medium-High | Supported-GPU matrix + llvmpipe fallback gate + never-panic degradation + `gator://` diagnostic surfacing the sandbox outcome. |
| **`sampler` BHM feature uncertainty** — wrong assumption → crash or over-broad cage. | Low | **Confirm** the build's feature set before finalizing the filter (§4.1). |
| **fail-open vs fail-closed** is a real behavior change (security-relevant). | Medium | Explicit owner decision; default warn-and-continue + `NAVGATOR_SANDBOX=require` opt-in fail-closed; never silent. |
| **Editing the verbose duplicated `target.cfg` predicates** maximizes rebase conflicts. | Low | Leave them alone despite looking like cleanup bait. |
| **Half-migration won't compile** (gaol `Profile`/`Operation` shared across cfg branches). | Low | Remove gaol per platform-pair atomically (Linux at M3 keeps it on macOS branch; full removal at M4). |
| **Windows is easy to underestimate**; the granular part is the hard 80%, deeply entangled with the process/IPC model. | High (schedule) | Budget months; ship coarse AppContainer first; defer Chromium-vendoring decision to the owner. |
| **birdcage temptation** (one crate, Linux+macOS) — but its macOS backend is too coarse and some versions used namespaces for net rules (could reintroduce the userns dependency). | Low-Medium | Use birdcage only as a Linux PoC if at all; **prefer explicit landlock+seccompiler** for owned policy granularity; do not use its macOS backend. |

---

## 10. Effort & team estimate (per phase)

Calibrated against a ~1-engineer reality where Servo-fork-capable + LSM/Seatbelt/
Windows-sandbox skills are the binding constraint (hiring, not eng-weeks, is the real
schedule risk; the safety-critical sandbox work should not ship solo).

| Phase | Effort | Notes |
|---|---|---|
| M1 (selftest harness + upstream fixes) | ~1–2 eng-weeks | selftest ~1 week; panic/wait PRs small. |
| M2 (Landlock+seccomp opt-in) | ~4–7 eng-weeks | Cost is the Log-mode bring-up loop across glibc/tokio/SpiderMonkey/stylo/WebRender/GStreamer/ANGLE + widening font/media profile. |
| M3 (default-on + degradation + gates) | ~2–4 eng-weeks | Calendar-dominated (soak/dogfood + GPU matrix + llvmpipe), not effort-dominated. |
| M4 (macOS incl. Apple Silicon) | ~4–8 eng-weeks | Seatbelt SBPL finicky; Apple Silicon net-new. Code largely liftable from gaol. |
| M5 (Windows AppContainer) | ~2–4 eng-**months** | Long pole; coarse first; full Chromium-vendoring is its own project. |
| Upstreaming pluggable `Sandbox` trait (parallel, optional) | ~3–6 eng-weeks | Design + Servo review cycles; pays back by zeroing the carried patch. |

**Totals:** Linux default-on (M1→M3) ≈ **1 engineer-quarter**; tri-platform ≈ **2 more
quarters**, with Windows dominating.

**Skills needed:** Linux LSM/seccomp engineer (M2/M3); macOS Seatbelt/SBPL +
notarization (M4, rare); Windows sandbox / AppContainer / possibly Chromium-vendoring
(M5, rarest — likely a contractor); a Servo-internals engineer for patch placement +
rebase cadence + the upstreaming PR (overlaps the existing fork maintainer).

---

## 11. Maintenance / rebase story

- **It is a carried fork patch by construction** — all four attach points are in
  `components/servo` + `components/constellation`, not the embedder.
- **Keep it additive.** All real logic in the new `sandbox_backend.rs` (new files
  never conflict on rebase); `content_process_sandbox_profile()` and `create_sandbox()`
  delegate in one line each (the only conflict-prone surface; conflicts trivially).
  Do **not** touch the verbose cfg predicate blocks.
- **Dedicated topic branch** `patches/sandbox-landlock` + a `PATCHES.md` ledger entry;
  fork-drift canary (`scripts/sync-forks.sh --check`) surfaces conflicts in the sandbox
  files specifically before each rebase.
- **Dependency-maintenance win, quantified:** gaol 0.2.1 (last release ~2021, "lightly
  reviewed… not mature", carries a RUSTSEC advisory, stale ~21-syscall KILL list that
  would SIGSYS-kill on `openat2`/`clone3`/`statx`/`rseq`) → `landlock` + `seccompiler`
  (Firecracker/AWS, actively maintained, current-syscall aware). The swap moves the
  most safety-critical dep from abandoned → maintained and clears the `cargo deny`/
  `cargo audit` 1.0 gate.
- **Burden-zeroing play (optional, owner-gated):** upstream a pluggable `Sandbox`
  trait. If accepted, NavGator carries only a trait *impl* in `navgator-engine` and
  **zero** engine-core patch — the highest-leverage maintenance move, but it needs a
  deliberate exception to the standing no-upstreaming posture.

---

## 12. Interim security posture (what to honestly claim before each milestone)

| After | Honest claim | Do NOT claim |
|---|---|---|
| **M0 (today)** | "Per-tab process isolation (multiprocess) is on by default. OS confinement is experimental, opt-in, and unavailable on AppArmor-restricted hosts (it crashes)." | Any working OS confinement on this host class. |
| **M2** | "Experimental OS confinement (filesystem + syscall + TCP) available opt-in via Landlock+seccomp; works on hosts where the old sandbox could not; runs in audit/log mode." | That it is enforcing by default, or complete. |
| **M3 (1.0)** | "Linux: per-tab process isolation **plus** filesystem + syscall + TCP confinement, **on by default**, with graceful degradation on unsupported kernels. Network/GPU/file-picker are brokered to the parent." | PID/IPC-namespace isolation or chroot (we don't have them). Any macOS/Windows OS confinement. A no-W^X JIT cage. |
| **M4** | "macOS: content sandbox via Seatbelt (both arches)." | Windows confinement. |
| **M5** | "Windows: AppContainer content confinement (coarse — content still reads fonts/GPU directly unless brokering lands)." | Renderer-grade Windows confinement parity with Chromium unless the broker/interception layer is built. |

**Standing honest caveats at all milestones:** the content process keeps any
already-open fd (the IPC channel — desired); Landlock denies at access-time and does
not hide the path namespace (existence/metadata side channels differ from a chroot);
JIT requires `PROT_EXEC` so the cage cannot enforce W^X; the strongest isolation
guarantee remains the multiprocess split, with Landlock+seccomp as defense-in-depth on
top — not a substitute for it.

---

## 13. Review addendum — additional risks from the critique pass

Surfaced reviewing the synthesized plan; not fully covered in §9, and each could move
the schedule or weaken a claim:

1. **GStreamer hardware decode can re-open the "no GPU in the content process" win.**
   §4.1 correctly finds WebRender/WebGL never touch `/dev/dri` *in content* — but that
   is the **rendering** path. With `media-gstreamer` ON, `decodebin`/autoplug may select
   **VA-API/VDPAU** hardware decoders, which **do** open `/dev/dri/renderD*` and issue
   GPU ioctls **inside the content process**. If so, the single hardest cage problem
   returns through the media door. **Action:** confirm whether HW decode is active
   (vs software `avdec_*`); for 1.0 prefer **forcing software decode in-content** or
   moving media to a separate confined process (§4.1 option c) rather than widening the
   content cage to GPU device nodes. Do not assume "no GPU in content" holds for media.

2. **Losing the PID namespace is broader than blocking `ptrace`.** §9 mitigates with
   seccomp-deny of `ptrace`/`process_vm_readv`, but without `CLONE_NEWPID` a compromised
   content process can still **read `/proc/<other-pid>/` (maps, environ, cmdline, status)**
   — an info-leak/ASLR-defeat side channel. Landlock is path-based and cannot cleanly
   deny `/proc/<others>` while preserving the `/proc/self` access the process needs.
   Treat host-`/proc` visibility as an explicit **residual** in the threat model; the
   only real closers are the deferred outer namespace layer or hiding `/proc` (chroot,
   which we gave up). Don't imply the syscall denials neutralize it.

3. **`Errno(ENOSYS)` as the pre-`Kill` default has its own failure mode.** A library
   that gets `ENOSYS` for a syscall it actually relies on can **misbehave subtly**
   (silent fallback, corruption, hang) rather than fail loudly — sometimes worse to
   diagnose than a clean `KillProcess`. The `Log → Errno → Kill` ramp needs the **same
   soak rigor at the Errno step**, and the right errno is per-syscall (sometimes
   `EPERM`/`EACCES`, not `ENOSYS`). Budget the Errno phase as real work, not a formality.

4. **Seccomp completeness is the classic schedule underestimate.** The §10 M2 figure
   (4–7 eng-weeks) is optimistic: the syscall long tail across kernel/glibc/distro/driver
   combinations (and transitive deps — tokio's `io_uring`, allocator `madvise`, NSS,
   ICU) tends to dominate. Plan for a **host-matrix-driven tail**, keep the field
   `NAVGATOR_SECCOMP=log` switch available for incident triage, and treat M3 default-on
   as **calendar-gated by harvest coverage**, not a date.

5. **Self-test ↔ production policy drift = security theater.** The `--sandbox-selftest`
   gate only proves anything if it applies the **byte-identical production policy** via
   the **same builder** (`content_process_policy()` / `apply_sandbox()`), not a parallel
   copy. Enforce structurally: one policy source, both the selftest and `create_sandbox`
   call it; add a test that fails if a second policy-construction path appears.

6. **Landlock-net is near-redundant given the seccomp inet ban — keep both anyway.**
   Since content does no sockets and seccomp restricts `socket` to `AF_UNIX`, the
   Landlock v4 TCP layer is belt-and-suspenders (and only present on ≥6.7). Useful as
   defense-in-depth, but the **load-bearing** network confinement is the seccomp inet
   denial; do not let "Landlock has net" imply network confinement on the <6.7 hosts
   where it silently drops.
