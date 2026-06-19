# navgator — Sustainability, the Servo treadmill & resourcing

> Dimension owner deliverable. Scope: the #1 existential risk (staying in sync with
> Servo), the resourcing/funding/governance reality, and a credible timeline to a
> usable 1.0 vs "Chrome parity." Brutally honest, quantified, current as of **June 2026**.
>
> TL;DR: **The treadmill is real and it killed Verso — but the ground shifted in
> navgator's favour in April–May 2026.** Servo published to crates.io (`servo 0.1.0`,
> 13 Apr 2026) and launched an **LTS track with half-yearly migration cycles**. That
> converts the maintenance problem from "chase a HEAD that breaks the embedding API
> ~every month" into "do a scheduled, reviewed migration twice a year." navgator's
> private diff against Servo is already tiny (788 LOC, 5 delegate methods, ~30 API
> symbols, no self-owned compositor). If navgator adopts the LTS train, keeps the diff
> small, and upstreams rather than forks, a **solo-to-tiny team can sustain it** — but
> a *usable daily-driver 1.0 is a 2–4 year effort*, and *true Chrome parity is never*.

---

## 1. The thesis in one table: navgator vs. the project that died

Verso (archived **8 Oct 2025** — "unable to keep pace … due to limited manpower and
funding") is the control group. navgator was deliberately designed to invert every one
of Verso's failure modes. The contrast is the whole sustainability argument:

| Axis | Verso (archived) | navgator (today, verified) | Why it matters for the treadmill |
| --- | --- | --- | --- |
| Embedding level | ~30 individual Servo crates (`constellation`, `compositing_traits`, `script`, `layout_thread_2020`, `net`, `script_traits`, `webgpu`, …), all pinned to `5e2d42e` | **Single `servo` umbrella crate** at `ed1af70` (+ `servo-embedder-traits` for `EventLoopWaker`) | Verso tracked ~30 internal-API surfaces that change weekly; navgator tracks **one curated public API** that Servo now versions on crates.io |
| Compositor | **Own `compositor.rs`: 2,200 LOC / 89 KB** driving the constellation itself | None — uses `WindowRenderingContext` + `OffscreenRenderingContext` and lets Servo composite | The compositor is the single largest churn magnet in Servo internals; navgator carries **zero** of it |
| Total Rust embedder code | ~9,200 LOC across many modules | **single-file `main.rs`** (native egui chrome + Servo page renderer); **no HTML/CSS/JS chrome** | Less surface = less to re-port on every bump; the chrome no longer rides the web engine at all |
| Servo API symbols depended on | hundreds (internal) | **~30 public symbols** (`Servo`, `ServoBuilder`, `WebView`, `WebViewBuilder`, `RenderingContext`, `OffscreenRenderingContext`, 5 `WebViewDelegate` methods, input/key enums) | Quantifiable, auditable, greppable break surface |
| Servo pin freshness | stale (`stylo` branch `2025-03-15`, `webrender 0.66`) — fell *behind* and couldn't catch up | `ed1af70` — but Servo is now **0.2.0 (May 2026)**; navgator is ~1–2 months behind HEAD | Verso's lesson: falling behind compounds; the fix is *cadence*, not *freshness* |
| Funding/manpower | hobby/volunteer, "limited" by its own statement | solo/hobby today | Same constraint — so navgator must spend the manpower it has on *features*, not on re-porting a compositor |

**Conclusion:** navgator has already paid the architectural insurance premium Verso
didn't. The remaining risk is **process discipline** (cadence + small diff +
upstreaming), not architecture. That is good news, because process is the cheap part.

---

## 2. How fast is the treadmill, actually? (quantified Servo churn)

Servo is *not* slowing down — it is accelerating, which cuts both ways (more capability,
more churn). Verified figures:

| Metric | 2023 | 2024 | 2025 | 2026 (run-rate) |
| --- | --- | --- | --- | --- |
| PRs merged / year | — | 1,771 | **3,183** | ~6,300 (≈530/mo × 12) |
| PRs / month (recent) | — | ~148 | ~265 | **530 (Mar), 534 (Apr)** |
| Unique contributors / year | 54 | 129 | **146** | — |
| Avg contributors / month | — | — | **42.4** | — |
| Contributors with ≥10 PRs/mo | — | — | **8.5** | — |
| WPT subtest pass-rate | — | 69.9% | **93.4%** | ~92.7% (current dashboard) |

**Reading the numbers:** ~530 PRs/month land in Servo. The *active core* is ~8–9
heavy contributors per month — i.e. the engine that navgator rides is itself maintained
by a team you could fit in one room. That is simultaneously the opportunity (navgator
can become a meaningful voice with a few good PRs) and the risk (if Igalia's funding
for Servo wavers, the engine slows and navgator inherits the slowdown).

### Embedding-API breakage rate (the part that actually hurts navgator)

Generic web-feature PRs don't touch navgator. **Embedder-facing API changes do.** Counting
only those, from Servo's own monthly reports:

| Month (2026) | Breaking embedding-API changes | Examples |
| --- | --- | --- |
| March | **~6** | `Servo::set_accessibility_active` → `WebView::…`; `WebView::pinch_zoom()` → `adjust_pinch_zoom()`; **delegate-setters moved from `WebView` to `WebViewBuilder`**; `GamepadProvider` → `GamepadDelegate`; `EventLoopWaker::wake` default impl removed; `Opts::nonincremental_layout`/`user_stylesheets` removed |
| April | **3** breaking + 5 additive | `WebView::animating()` now `&self`; `Servo::site_data_manager()` returns `&SiteDataManager` not `Ref<…>`; gamepad-haptic delegate methods removed; (additive: `load_request()` custom headers, async cookies, `temporary_storage`) |

**Run-rate: roughly 3–6 breaking embedding-API changes per month.** Across a 6-month
LTS gap that is ~20–35 individual breaks to absorb in one migration — but they are
*concentrated, documented in the monthly reports, and mechanical* (renames, `self`→`&self`,
moved methods). Note the March "delegate-setters moved to `WebViewBuilder`" change is
exactly the API navgator **already uses** (`WebViewBuilder::new(...).delegate(...)`), so
navgator at `ed1af70` is already on the *post*-churn shape for that one. This is what a
small diff buys you: most months, zero of those changes touch navgator's 30 symbols.

---

## 3. The strategic shift that changes everything: crates.io + LTS

Two Servo decisions in April–May 2026 materially de-risk navgator:

1. **`servo` published to crates.io — `0.1.0`, 13 Apr 2026** (`0.2.0` by end of May).
   You can now `cargo add servo` against a *semver-versioned* crate instead of a raw
   git `rev`. Caveat from the Servo Book: "releases will be published to crates.io if
   possible, but embedders should expect that **git dependencies might be required**"
   (CVE-backport cases pull patched transitive deps via git).

2. **An LTS track exists.** Per the Servo Book / 0.1.0 announcement: embedders do
   "major upgrades on a scheduled **half-yearly basis** while still receiving security
   updates." LTS policy specifics:
   - **Security fixes only** on the LTS line; no new features.
   - **No fixed patch schedule** — patch releases "as needed."
   - **Scope = the `servo` library + its deps.** `servoshell` (the demo browser) is
     explicitly **out of scope**.
   - Regular monthly releases keep shipping breaking changes; 1.0 semantics are
     "still being discussed."

### What this means for navgator's release strategy

This is the single highest-leverage decision in this whole document:

> **Ride the Servo LTS line. Do one planned migration every ~6 months. Cherry-pick
> security patches in between. Do not chase HEAD.**

Verso died chasing HEAD with no funding. The LTS train didn't exist for Verso; it does
for navgator. Adopting it converts the dominant cost from "continuous, unbounded,
surprise breakage" to "two scheduled, scoped, reviewable migrations per year" — a load
a solo maintainer can actually carry.

**The catch (be honest):** LTS is *security-only*. Riding LTS means navgator's *web-platform
features lag the engine by up to 6 months.* For a from-scratch browser chasing parity
that is a feature, not a bug — you want a stable base while you build chrome/UX, not a
moving target. But it does mean navgator will never showcase Servo's newest features on
day one. Accept that trade explicitly.

---

## 4. Recommended sync architecture (concrete, buildable now)

### 4.1 Dependency form

- **Short term (now):** keep the exact-`rev` git pin (`ed1af70`) — it works and the
  854-package lock is reproducible. Do **not** float; never use a branch.
- **Next step (do this milestone):** migrate the `servo`/`embedder_traits` deps to the
  **crates.io LTS release** (e.g. `servo = "0.1"` pinned to the LTS minor), keeping
  `Cargo.lock` committed. Keep a documented escape hatch to a git `rev` for CVE-patch
  cases (the Book warns this will happen).
- Keep `[patch.crates-io]` empty unless a future Servo rev re-introduces patches
  (`ARCHITECTURE.md` already tracks this gotcha — cargo ignores a dependency's own
  `[patch]`, so they'd have to be copied into navgator's `Cargo.toml`).

### 4.2 CI: a two-lane treadmill detector (the thing navgator is missing today)

navgator currently has **no CI** (`/raid/navgator/.github` does not exist; only the
vendored `reference/verso/.gitlab-ci.yml`). This is the most urgent process gap. Build:

| Lane | Trigger | Pins to | Purpose |
| --- | --- | --- | --- |
| **stable** | every PR/push | the committed LTS pin | green-must-stay; gates merges |
| **canary** | nightly cron | Servo **HEAD** (or latest monthly) | *early-warning* — tells you what the next migration will cost, weeks before you do it |

The canary lane is the core sustainability instrument: it turns "surprise, the bump
broke everything" into a continuously-updated diff of *exactly which of navgator's ~30
symbols moved*. When canary goes red, the failure log **is** the migration checklist.
Build cost is the constraint — see §4.4.

### 4.3 Diff-minimization policy (codify it)

A written rule, enforced in review:

1. **Depend only on the `servo` umbrella crate's public API.** Never reach into
   internal Servo crates (that is precisely what sank Verso). If you need something
   the public API doesn't expose, **upstream it** (§5), don't fork to reach inside.
2. **Carry zero patches to Servo by default.** Every private patch is a line you
   re-rebase forever. Budget: aim for a **0-patch** steady state; any patch must have a
   filed upstream PR and a removal trigger.
3. **Quarantine the API surface.** Wrap all `servo::` usage behind a thin
   `engine/` module (today it's inlined in `main.rs`). One file to re-port per bump;
   the rest of navgator (chrome bridge, tabs, IPC, theming) is insulated.
4. **A Servo bump is its own reviewed commit** — `Cargo.toml` + `rust-toolchain.toml`
   + `Cargo.lock` + the `engine/` re-port move together, with the monthly-report
   breaking-change list pasted into the commit message. (ARCHITECTURE.md already says
   this; make it a CI-enforced checklist.)

### 4.4 The build-cost reality (don't gloss over this)

The dependency graph is **854 packages** including SpiderMonkey (`mozjs_sys`) and
ANGLE (`mozangle`), which compile native C/C++ and run `bindgen` against a
version-matched LLVM (README documents the clang-18/LLVM-21 footguns). A clean first
build is *minutes-to-tens-of-minutes and several GB in `target/`.* Implications:

- A nightly canary lane needs `sccache`/`cargo` registry+target caching or it will be
  too slow/expensive to run on free CI. Budget a beefy self-hosted or cached runner.
- LLVM version pinning (`LIBCLANG_PATH`, bare `llvm-objdump` on `PATH`) must be encoded
  in the CI image, not left to docs.
- This build weight is itself a *contributor-onboarding* tax: every new contributor
  pays the multi-GB, long-first-build cost. It depresses the bus-factor. Mitigate with
  a prebuilt dev container / `target/` cache artifact.

---

## 5. Upstream-first policy (turn the dependency into leverage)

The cheapest way to keep the diff at zero is to **make the things you need exist in
Servo's public API**, so you don't have to carry them.

- **Default: upstream, don't fork.** If navgator needs an embedding capability (e.g.
  sub-rectangle webview placement — the limitation ARCHITECTURE.md documents, where a
  `WebView` fills its `RenderingContext` with no sub-rect API), the highest-leverage
  move is a Servo PR adding it to the public API. Then it ships for *everyone* and
  navgator carries no patch.
- **navgator becomes a real embedder voice.** With Verso archived and
  `tauri-runtime-verso` dependent on it, there is a **vacuum for a serious downstream
  embedder driving the `[meta] embedding` (#30593) agenda.** Servo's core is ~8–9
  heavy contributors/month; a downstream that lands even 1–2 well-scoped embedding PRs
  a month would be among the most active embedder voices. That earns API-stability
  goodwill and early warning of breaking changes.
- **Concrete upstream targets** (prioritized for navgator's roadmap): sub-rect/region
  webview compositing API; richer `WebViewDelegate` hooks for prompts/menus/downloads
  (navgator's M3 TODO); a stable command/IPC surface for the "engine as a service" goal
  (M5) for the external engine-as-a-service surface. (The old in-chrome `navgator:`-scheme
  command bridge is already gone — the chrome is native egui and calls the engine directly.) Each, if upstreamed, *removes* a
  future maintenance burden instead of adding one.
- **Track Servo's deprecations.** The monthly reports are the canonical changelog;
  treat each one as required reading. Subscribe a bot to post the "Changes for web
  developers / embedders" section into navgator's issue tracker.

---

## 6. Resourcing, funding & governance (brutally honest)

### 6.1 The headcount asymmetry — name it plainly

- **Chrome/Blink:** thousands of engineers, Google-funded, effectively unlimited.
- **WebKit, Gecko:** hundreds.
- **Servo (the engine navgator *rents*):** ~146 contributors/year but an active core of
  **~8–9 heavy contributors/month**, funded mainly via Igalia + **~$7,349/month** in
  recurring community donations (April 2026; goal $10k/mo). That is a rounding error
  next to Chrome's budget.
- **navgator (the chrome navgator *builds*):** today, effectively **one person.**

So navgator is a one-person UX/chrome layer on top of a ~10-person engine that is itself
a rounding error against Chrome. **Any plan that assumes otherwise is fiction.** The
entire strategy must be built around *leverage* (let Servo build the engine) and
*scope discipline* (don't rebuild Chrome).

### 6.2 What headcount navgator actually needs, by ambition tier

| Tier | What it is | Realistic team | Funding/yr (loaded) | Feasible? |
| --- | --- | --- | --- | --- |
| **Hobby / hackable** | Daily-driver-for-the-author; chrome, tabs, theming, settings, basic sync | **1–2** people | $0–250k | **Yes** — this is where navgator is and can stay healthy |
| **Niche product** | Opera-GX-class theming, Lyku sync, polished UX, a real user base, security-update SLA | **3–6** eng + 1 design/PM | $0.6–1.5M | Plausible *with funding*; the LTS train makes the eng count realistic |
| **"Industry-standard" browser** | Extensions, DRM/Widevine-class media, full devtools, profile/sync infra, security response team, multi-OS | **15–40+** | $5–15M+ | Only with serious backing; **mostly gated by Servo's own capability ceiling**, not navgator's |
| **Chrome parity** | Everything, every site, every API, instantly | **hundreds–thousands** | $100M+ | **Never.** State this in the README. |

The honest framing for stakeholders: **navgator can be an excellent Tier-1/Tier-2
product.** Tier 3 is contingent on Servo reaching Tier-3 capability *and* navgator
getting funded. Tier 4 is not a goal; it is a myth used to kill projects by comparison.

### 6.3 Funding options (ranked by realism for a Rust/Servo-aligned project)

1. **Stay donation/sponsor-funded & hobby-scoped (default).** GitHub Sponsors /
   Open Collective, mirroring Servo's own model. Sustains Tier 1 indefinitely; costs
   nothing but time. **This is the base case and it is fine.**
2. **Lyku as the revenue flywheel.** The planned sync service ("Lyku", self-hostable
   later) is navgator's most credible *non-ad, non-telemetry* revenue: hosted sync
   subscription + self-host for free. This is the Tier-2 funding path and it aligns
   with the no-bloat/no-telemetry ethos (you sell *a service*, not *the user*).
3. **Grants / foundation money.** NLnet/NGI (EU, Rust- and Servo-adjacent), Linux
   Foundation Europe proximity (Servo is an LFE project), Sovereign Tech Fund. Good
   fit for "independent, privacy-respecting, Rust browser." Lumpy and competitive.
4. **Contract/consulting on the engine** (the Igalia model): fund navgator dev by doing
   paid Servo/embedding work. Doubles as upstreaming.
5. **Avoid:** ad/affiliate/telemetry monetization — it contradicts the entire premise
   and would forfeit the differentiator.

### 6.4 Governance / bus-factor

- **Bus-factor is 1 today.** This is the quiet killer; Verso's effective bus-factor
  was also tiny. Mitigations: keep everything in-repo and documented (already strong —
  `ARCHITECTURE.md` is unusually good), keep the diff small enough that a *second*
  person can pick it up, and recruit even one co-maintainer before Tier 2.
- **License:** MPL-2.0 (matches Servo) — correct; keeps upstreaming frictionless.
- **Don't build private governance overhead** at Tier 1. The "governance" that matters
  is the written sync/diff policy in §4.3 and a public roadmap.

---

## 7. Credible timeline (no hand-waving)

Assumes Tier-1 resourcing (1–2 people), LTS adoption, current Servo trajectory.

| Horizon | What "done" looks like | Confidence |
| --- | --- | --- |
| **Now (M1–M5 done + native-chrome pivot)** | Native-egui chrome, Servo page renderer, multi-tab compositing, `gator://` internal pages, IPC control plane | shipped/verified |
| **+0–3 mo** | **CI (stable + canary lanes)**; migrate to crates.io LTS pin; quarantine `servo::` into `engine/`; written sync/diff policy | high |
| **+3–9 mo** | First *scheduled* LTS migration executed cleanly (proves the cadence works); downloads, prompts/menus/context-menu via upstreamed delegate hooks; settings UI; first theming pass | medium-high |
| **+9–18 mo** | **Usable daily-driver for the author**: history/bookmarks, profiles, Lyku sync v1, password/form basics, deep theming (Opera-GX-class), 2 LTS migrations behind it | medium |
| **+18–36 mo** | **"1.0": daily-driver for sympathetic users.** Handles the long tail of common sites *to the extent Servo does*; extensions story TBD; security-update SLA via LTS backports | medium-low (gated by Servo) |
| **Ever** | **Chrome parity** | **No.** Capability ceiling is Servo's, and Servo is ~10 core devs vs Chrome's thousands |

**The load-bearing honesty:** navgator's *own* milestones are achievable on this timeline.
What navgator **cannot** outrun is **Servo's web-platform completeness** (WPT ~93% of the
tests *it runs* — but it doesn't run everything; missing/partial: full WebGPU in all
contexts, the long tail of CSS/DOM, DRM media, extensions). navgator's "1.0" is therefore
"a usable browser for people who accept that some sites won't fully work," not "Chrome
that doesn't track you." Sell it as the former.

---

## 8. Risk register (seed)

| ID | Risk | Likelihood | Impact | Score | Mitigation | Owner trigger |
| --- | --- | --- | --- | --- | --- | --- |
| R1 | **Servo-sync treadmill** — bumps break the embedding API faster than navgator can absorb (the Verso death) | Med | Critical | **High** | Ride LTS (§3); small diff (§4.3); canary CI (§4.2); upstream not fork (§5) | canary lane red >2 weeks |
| R2 | **Bus-factor = 1** — single maintainer stalls/leaves | High | Critical | **High** | Docs-in-repo; small diff so it's pickup-able; recruit co-maintainer pre-Tier-2 | no commits 60d |
| R3 | **Servo itself loses funding / slows** (donations ~$7.3k/mo, below $10k goal; Igalia-dependent) | Low-Med | Critical | **High** | navgator LTS pin survives a Servo pause; contribute $ + PRs to Servo; have a "freeze on last good LTS" plan | Servo monthly report cadence breaks |
| R4 | **No CI today** — regressions/bump-breakage found late | High (current) | High | **High** | Build stable+canary lanes *now* (§4.2) | — (open) |
| R5 | **crates.io LTS needs git deps for CVE patches** (Servo Book warns) — breaks the clean semver story | Med | Med | Med | Keep git-rev escape hatch documented; pin + audit transitively | LTS patch requires git dep |
| R6 | **Build weight** (854 deps, mozjs/mozangle, LLVM pinning) taxes CI cost + onboarding, depressing bus-factor | High | Med | Med | sccache + cached runners; prebuilt dev container; encode LLVM pins in CI image | CI minutes/cost spike |
| R7 | **Scope creep toward "Chrome parity"** burns the tiny team on unwinnable fronts | Med | High | **High** | Written Tier-1/2 scope; "parity = never" in README; roadmap discipline | feature requests citing Chrome |
| R8 | **Capability ceiling**: sites that need features Servo lacks (DRM media, full WebGPU, extensions) just don't work | High | Med | Med | Set expectations ("usable, not universal"); upstream priorities; per-site fallbacks later | user reports of broken sites |
| R9 | **LTS lag**: riding LTS means web features trail HEAD by ≤6 mo; could feel stale | High | Low | Low | Accept trade explicitly; optionally run a HEAD "preview" build off the canary lane | — |
| R10 | **No revenue → can't reach Tier 2** | Med | Med | Med | Lyku sync as flywheel (§6.3); grants (NLnet/STF); keep Tier-1 viable with $0 | Lyku slips / no grant |
| R11 | **LLVM/toolchain drift** breaks builds on contributor machines (already observed: clang18/LLVM21) | Med | Med | Med | Pin in CI image + `rust-toolchain.toml`; document (done); dev container | new contributor build fails |

---

## 9. Prioritized recommendations

1. **(P0) Stand up CI now — stable + nightly canary-against-Servo-HEAD.** This is the
   missing instrument; without it the treadmill is invisible until it has already run
   you over. The canary log *is* your migration plan. (Addresses R1, R4.)
2. **(P0) Adopt the Servo LTS train; migrate the dep to the crates.io LTS pin.** Single
   biggest risk reducer available — it's the thing Verso never had. Schedule the
   half-yearly migration as a recurring, reviewed event. (R1.)
3. **(P0) Quarantine all `servo::` usage behind an `engine/` module** (today inlined in
   `main.rs`). One file to re-port per bump; insulate chrome/tabs/IPC/theming. (R1.)
4. **(P1) Write and enforce the diff-minimization + upstream-first policy** (§4.3, §5):
   0-patch steady state, public-API-only, every gap becomes a Servo PR. (R1, R8.)
5. **(P1) Recruit one co-maintainer before committing to Tier 2.** Bus-factor 1 is the
   second existential risk and the cheapest to start fixing. (R2.)
6. **(P1) Put the honest scope in the README:** Tier-1/2 product, "usable not
   universal," "Chrome parity = never." Manages every downstream expectation and kills
   R7 at the source.
7. **(P2) Make navgator a visible Servo embedder-voice** — land 1–2 embedding PRs/month
   (sub-rect compositing, delegate hooks, stable IPC surface). Buys early warning +
   API goodwill and *removes* future private diff. (R1, R3, R8.)
8. **(P2) Treat the monthly Servo reports as required reading**; bot the
   embedder/web-dev changelog sections into the tracker. (R1.)
9. **(P2) Stand up the funding base early and ethically:** Sponsors/Open Collective now;
   design Lyku as the Tier-2 revenue flywheel; line up NLnet/STF grants. No ads, no
   telemetry — that's the product. (R3, R10.)
10. **(P3) Invest in build ergonomics** — sccache, cached/self-hosted runner, prebuilt
    dev container with LLVM pinned — to keep CI affordable and onboarding survivable.
    (R6, R11.)

---

## Sources

- Servo 2025 stats (PRs, contributors, WPT): https://blogs.igalia.com/mrego/servo-2025-stats/
- Servo on crates.io / 0.1.0 + LTS announcement (13 Apr 2026): https://servo.org/blog/2026/04/13/servo-0.1.0-release/
- Servo Book — LTS Release policy: https://book.servo.org/embedding/lts-release.html
- March in Servo 2026 (530 commits; breaking embedding-API changes; $7,167/mo): https://servo.org/blog/2026/04/30/march-in-servo/
- April in Servo 2026 (534 commits; breaking changes; $7,349/mo, goal $10k): https://servo.org/blog/2026/05/31/april-in-servo/
- Servo joins Linux Foundation Europe (governance/funding context): https://linuxfoundation.eu/newsroom/servo-web-rendering-engine-joins-linux-foundation-europe
- Verso archived Oct 2025 (manpower/funding reason): https://github.com/versotile-org/verso/ ; https://github.com/versotile-org/tauri-runtime-verso
- Servo WPT pass-rate dashboard: https://servo.org/wpt/
- navgator repo (verified): `/raid/navgator/Cargo.toml`, `/raid/navgator/src/main.rs` (788 LOC), `/raid/navgator/docs/ARCHITECTURE.md`, `/raid/navgator/Cargo.lock` (854 pkgs), `/raid/navgator/reference/verso/` (~9,200 LOC, 2,200-LOC compositor.rs, ~30 Servo crate deps @ `5e2d42e`)
