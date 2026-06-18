# The swervo engine fork

swerve builds on **[`airgap/swervo`](https://github.com/airgap/swervo)** — our
**maintained fork** of [`servo/servo`](https://github.com/servo/servo). This is the
engine-strategy decision locked in [`ROADMAP.md` §R2 / D1](ROADMAP.md): we own the
engine and implement web-platform features ourselves, rather than embedding upstream
and filing requests.

> **Maintained fork, not hard fork.** We never file changes *upstream*, but we *do*
> merge *from* upstream on a cadence. A hard fork that stops tracking upstream rots:
> it forfeits Servo/Igalia's ongoing engine work and makes every later merge
> exponentially harder. The discipline below keeps the merge cost bounded.

## Engine repositories (all under github.com/airgap)

We fork the **entire** Servo engine surface, not just the umbrella crate:

| Fork | Upstream | swerve pins | Consumed via |
| --- | --- | --- | --- |
| [`airgap/swervo`](https://github.com/airgap/swervo) | `servo/servo` | `ed1af70` (`main`) | `Cargo.toml` git deps (`servo`, `embedder_traits`) |
| [`airgap/stylo`](https://github.com/airgap/stylo) | `servo/stylo` | `49e912cf` | `[patch."…/servo/stylo"]` (8 crates) |
| [`airgap/webrender`](https://github.com/airgap/webrender) | `servo/webrender` | `dcfd5424` (branch `0.69`) | `[patch.crates-io]` (webrender / webrender_api / wr_malloc_size_of) |

`stylo` is consumed by swervo over **git**, so a top-level `[patch."https://github.com/servo/stylo"]`
in swerve's `Cargo.toml` redirects all 8 stylo crates to our fork at the same rev. `webrender` is
consumed from **crates.io** (`0.69`/`0.2.2`), so a `[patch.crates-io]` redirects it to our fork's
`0.69` branch HEAD. Cargo honours top-level patches across the whole (swervo-transitive) graph, so
neither requires editing the swervo fork. All three are pinned to upstream-identical revs today;
bump them as fork patches land. Each fork follows the same maintained-fork discipline below.

## Repository model

| Branch / remote | Role |
| --- | --- |
| `airgap/swervo` `main` | Our **integration line**. Starts identical to upstream `ed1af70`; our patches land here. swerve's `Cargo.toml` pins a commit on this branch. |
| `upstream` = `servo/servo` | Tracked read-only. We `git fetch upstream` and merge on a cadence; we never push to it. |
| `patches/<feature>` | Topic branches for each fork patch (one concern each), merged into `main`. Keeps the diff legible and rebasable. |

swerve consumes the fork through `Cargo.toml`:

```toml
servo           = { git = "https://github.com/airgap/swervo", rev = "<commit on main>" }
embedder_traits = { git = "https://github.com/airgap/swervo", rev = "<same commit>", package = "servo-embedder-traits" }
```

Bump `rev` after each upstream merge or patch; the canary CI lane (below) must be green first.

## Merge cadence

Target: merge upstream on a **fixed cadence** (≈monthly, or aligned to Servo's
crates.io LTS train — see [`plan/sustainability.md`](plan/sustainability.md)).

`scripts/sync-forks.sh --check` reports drift across all three forks; `--merge` performs
the merge (clones into `$SWERVE_FORKS_DIR`, fetches upstream, merges, pushes). Per repo it does:

```bash
git remote add upstream https://github.com/servo/servo   # once
git fetch upstream
git switch main
git merge upstream/main            # resolve conflicts in OUR patches only
# run the full build + headless smoke + top-sites compat corpus
# then bump swerve's Cargo.toml rev to the new main commit
```

**Diff-minimization is the merge-cost lever:** keep each patch small, isolated, and
behind the narrowest change that works; prefer additive modules over edits to
upstream files; record every patch in `PATCHES.md` (feature, files touched, why, merge
hazards). The smaller and better-isolated the diff, the cheaper every future merge.

## Applied fork patches

| Branch | Repo | Commit | What |
| --- | --- | --- | --- |
| `patches/swerve-ua` | swervo | `e559b12` | Brands the default User-Agent — appends a `Swerve/0.1.0` token to all 6 per-OS UA strings in `components/config/prefs.rs` (keeps the Firefox/Servo compat tokens). The **first fork patch**: proves the patch → pin → build → ship pipeline end-to-end. swerve-engine pins this commit. |

(In-repo ledger; the canonical per-patch detail lives on each `patches/*` branch in the fork.)

## Fork patch backlog (toward D5 "full web rendering")

The features upstream Servo gates or lacks become **our** engine work (sequenced by
real-world site usage, not WPT %). Each is a `patches/<feature>` branch:

- **Layout-gated CSS** (`layout.unimplemented`): `text-overflow`, `user-select`,
  masks, `backdrop-filter`, anchor positioning, view-transitions, …
- **Engine-blocked product features:** streaming **downloads** API, **find-in-page**
  API, **IndexedDB** hardening, **service workers**, **WebRTC**.
- **Graphics:** WebGL2, WebGPU maturation.
- **Auth:** **WebAuthn / passkeys** (`PublicKeyCredential` is absent today).
- **Privacy:** state partitioning / anti-fingerprinting (cookie partitioning is a stub).
- **DRM:** **EME plumbing** to host a licensed Widevine/PlayReady CDM (the binary is
  proprietary — the one allowed external dependency; build-flag gated). See §R2/D5a.
- **Sandboxing:** a pluggable sandbox (Servo's spawn lives in the constellation) for
  Linux seccomp+userns, macOS Seatbelt, Windows AppContainer — all first-class (§R2/D2).

## Build notes per platform

All three OSes are first-class (§R2/D2). The engine build needs a consistent LLVM
toolchain and the mozjs/SpiderMonkey + ANGLE native deps.

- **Linux (validated):** stable Rust 1.95 (via `rust-toolchain.toml`); a single LLVM
  version on `PATH` + `LIBCLANG_PATH` (mozjs needs bare `llvm-objdump`; bindgen needs a
  matching `libclang`). See the README troubleshooting section.
- **macOS / Windows (to validate in CI):** same toolchain discipline; Servo's own
  macOS/Windows build quirks apply and several sandboxing pieces are weak/absent
  upstream — expect fork patches.

CI is **Jenkins** (`Jenkinsfile`, on the Linux/macOS/Windows runners): a matrix builds
all three; Linux is the required gate, macOS/Windows run non-blocking (UNSTABLE) until
green, then flip to required. A scheduled **Upstream canary** stage runs
`scripts/sync-forks.sh --check` to surface fork drift early.

The `swerve-ci` Pipeline job is **job-as-code** (`jenkins/job-configs/swerve-ci.xml`,
created/updated via `jenkins/setup-jenkins.sh`): it builds `dev` from
`github.com/airgap/swerve.git` via the root `Jenkinsfile` and polls every 5 min (a
localhost Jenkins can't receive a GitHub push webhook). Agents: `linux` (Built-In),
`macos` (mac-mini), `windows` (windows-strix). Runner provisioning (a single LLVM, Rust,
sccache, warm `~/.cargo`) is the gating factor for the first green build — provision a
`swerve-ci-base` image as lyku does with `lyku-ci-base`.
