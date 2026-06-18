# swerve — Competitive positioning & the independence thesis

> Dimension owner doc. Honest assessment of whether "go independent on Servo" is a
> sound strategy, who the realistic user is, what the wedge is, and what
> "parity-ish without bloat" should concretely mean. Current as of June 2026.
> Sources are cited inline; numbers are quantified, not hand-waved.

---

## 0. TL;DR (the call)

The **independence thesis is directionally correct and the marketing line is true** — every
mainstream "alt" browser does trace back to Google money or Google's engine. But the thesis
is **necessary, not sufficient**, and on its own it is a *weak* wedge: most users do not buy
a browser engine, they buy an experience, and "independent engine" is invisible until it
breaks a site.

The honest framing:

- **Independence is the moat and the liability at the same time.** It's the one thing no
  Chromium fork or Gecko reskin can copy — and it's the thing that will churn users when a
  banking site or Google Docs renders wrong. Servo today passes **62% of Web Platform
  Tests** and is **19.8% "Baseline Widely Available"-ready** (87 of 439 categorized BWA
  features fully implemented = 19.8%; 333 partial = 75.9%; 19 unsupported = 4.3%; the
  upstream catalog lists 593 BWA features total, but the readiness percentages are computed
  over the 439 with a recorded status — see note), and at the current velocity (≈22 features
  completed/yr vs ≈52 new features added/yr) it is **projected to plateau near 80% readiness
  around 2037** absent a step-change in funding.[^readiness] You cannot ship a *daily-driver*
  general browser on that and keep users.
- **Therefore the wedge is NOT "a Chrome replacement that happens to be independent."** It is
  **"the most customizable, zero-telemetry, genuinely-independent browser for people who
  already keep a Chromium browser around for the 10% of sites that break."** Win on
  *customization + zero telemetry + the independence story + performance/footprint*, treat
  web-compat as a managed liability with an explicit escape hatch, and let Servo's compat
  curve come up underneath you over years.
- **The single biggest existential risk is not Servo's compat — it's the Servo-embedding
  maintenance treadmill that already killed Verso (archived Oct 2025).** Strategy must be
  built around surviving that treadmill, not around feature breadth. This is covered in the
  architecture docs but it *is* the dominant positioning constraint: a browser nobody can
  keep building is worse than a browser that renders 80% of sites.

Recommendation: **adopt an explicit "second browser, by choice" positioning for v1**, with a
credible roadmap to "only browser" as Servo matures. Do not market parity. Market
*independence + control + no spyware*, and be ruthlessly honest about compat in-product.

---

## 1. The independence thesis — is it sound?

### 1.1 The claim, restated

> Every alternative browser ultimately depends on Google — via the Blink/Chromium engine, or
> via Google search-default money. swerve breaks the chain by being built on Servo, an
> independent Rust engine with no Google funding.

### 1.2 The claim is factually true today

The browser ecosystem in 2026 is a near-total monoculture with two funding/engine
chokepoints, both leading to Google:

| Browser | Engine | Independent of Blink? | Independent of Google money? |
|---|---|---|---|
| Chrome | Blink (Google) | No | No (it *is* Google) |
| Edge | Blink (Chromium) | No | No (rides Blink; default-deal economics) |
| Opera / Opera GX | Blink (Chromium) | No | No |
| Brave | Blink (Chromium) | No | No (engine); search is its own, but Blink is Google's |
| Vivaldi | Blink (Chromium) | No | No (engine) |
| Arc / Dia / most "AI browsers" | Blink (Chromium) | No | No |
| Thorium / ungoogled-chromium | Blink (Chromium) | No (de-Googled build, Google's engine) | Engine still Google's |
| Firefox | Gecko (Mozilla) | **Yes (engine)** | **No** — ~85% of Mozilla revenue is the Google search default deal; that deal expires end of 2026[^mozilla] |
| LibreWolf / Waterfox / Mullvad | Gecko (Mozilla) | Yes (engine) | Downstream of Mozilla, downstream of Google's money |
| **Ladybird** | **LibWeb (own, from scratch)** | **Yes** | **Yes** — donor/sponsor funded (Cloudflare, others) |
| **swerve** | **Servo (Rust, LF Europe / Igalia)** | **Yes** | **Yes** — no Google money in Servo |

Sources: 2026 share/funding figures.[^share][^mozilla][^privacy] "Every Chromium browser
(Brave, Edge, Opera, Vivaldi, Arc) relies on Google's Blink engine," producing what
observers call "a dangerous monoculture."[^privacy] Chromium/Blink powers **>3/4 of every web
session served globally** (Chrome alone 65.1% all-device, 76.39% desktop).[^share]

So the *engine* monoculture is real: every viable engine that is NOT Google's own is either
Mozilla's Gecko (kept alive by Google's money) or two genuinely-clean-room newcomers, Servo
and Ladybird. swerve and Ladybird are on the only two engines in the world that are both
not-Blink and not-Google-funded. **The independence claim survives scrutiny.**

### 1.3 But "independent engine" is a weak *consumer* wedge on its own

Three hard truths:

1. **Users don't buy engines; they buy not-breaking.** The engine is invisible right up until
   a site renders wrong, and then it is the *only* thing the user notices. Servo at 62% WPT
   means a meaningful fraction of real sites will misbehave. Independence is a *values*
   purchase, and values purchases are a small slice of the market (cf. Firefox at **2.26%**
   all-device despite a decade of "independent + privacy" marketing[^share]).
2. **"Anti-Google" is already a crowded shelf.** Brave (privacy + its own ad engine),
   Mullvad/Tor (anti-fingerprinting), LibreWolf (de-telemetried Firefox), Vivaldi (explicitly
   anti-AI, deep customization) all sell some flavor of "escape Google." swerve's
   differentiator inside that shelf has to be **the engine is *also* not Google's**, which is
   a strictly stronger claim than any Chromium fork can make — but only Ladybird can match it.
3. **The funding asymmetry is brutal.** Servo runs on order-of-$30k/yr in community donations
   plus Igalia staff time and a Sovereign Tech Fund grant (Igalia made 26% of PRs; 40% other
   contributors; rest bots).[^servofund] Chrome is funded by Google's ad business. swerve is a
   single-developer project on top of that. "Independence" must be sold as a *deliberate
   trade*, not a free win.

### 1.4 Verdict

The thesis is **sound as a foundation and as a story, but cannot be the whole pitch.**
Independence is the *credibility* layer (it's why the privacy/customization claims are not
hypocritical the way a Chromium fork's are). It is not, by itself, the thing that makes
someone switch. The switch has to be earned on **control, footprint, and the absence of
telemetry/AI cram-down** — areas where swerve can plausibly *beat* the incumbents, not merely
match them.

---

## 2. Servo as a differentiator vs. the cost — honest math

### 2.1 What independence genuinely buys you

- **A claim no competitor can copy.** "Not Blink, not Gecko, no Google money, no telemetry,
  Rust, memory-safe by construction" is a coherent, defensible identity. Ladybird is the only
  other entrant who can say it, and Ladybird is *not* shipping a customization-first consumer
  browser — it's a standards-conformance engine project (alpha 2026, beta 2027, stable
  2028).[^ladybird] swerve and Ladybird are **complementary, not duplicative**: different
  engine, different goal (Ladybird = conformance; swerve = experience/customization).
- **No Manifest-V3 cage.** Chrome's MV3 transition (completed late 2024) gutted content
  blocking: `webRequest` → `declarativeNetRequest`, no dynamic filtering, full uBlock Origin
  dead on Chrome.[^mv3] swerve owns its entire stack — it can build blocking *into the engine*
  (Brave-style) with no extension-API politics and no Google able to revoke it.
- **No telemetry by construction, and it's believable.** A Chromium fork claiming "no
  telemetry" is fighting its own upstream forever. swerve's telemetry story is true because
  there's nothing phoning home unless swerve adds it.
- **Smaller, hackable surface.** Servo + an HTML chrome is dramatically smaller than the
  ~40M-line Chromium tree. That's what makes Opera-GX-class theming *performant and deep*
  rather than bolted-on.

### 2.2 What it costs — quantified

- **Web compat is years behind and the curve is slow.** 62% WPT / 19.8% BWA today; 141
  features at "zero velocity" needing architectural work; 51 features *regressed* >5 points;
  plateau ~80% by ~2037 at current funding.[^readiness] swerve does not control this curve —
  it rides it.
- **The embedding treadmill is the proven killer.** Verso did *exactly this* (Servo-based
  browser, HTML-ish UI ambitions) and was **archived Oct 2025** because it "was unable to keep
  pace with significant revisions to Servo due to limited manpower and funding."[^verso] Servo
  is unversioned and renames crates freely (swerve already hit `embedder_traits` →
  `servo-embedder-traits`; see `docs/ARCHITECTURE.md`). swerve's mitigation — pin an exact rev,
  use the high-level `libservo` umbrella crate rather than ~30 component crates, bump
  deliberately — is the *correct* lesson from Verso, but it does not remove the cost; it
  reduces the surface and converts churn into scheduled, reviewed work.
- **No actively-maintained general-purpose Servo browser exists in 2026.** The "Made With
  Servo" roster (Verso, Moto, Kumo, servoshell, Servo-GTK/Qt, Slint-Servo, Beaver, Cuervo) is
  mostly embedding experiments or kiosks; servoshell (egui, first-party) is the only reliably
  maintained UI and it's a *test harness*, not a consumer browser.[^madewith] **swerve would
  be attempting something nobody has currently sustained.** That is both the opportunity and
  the warning.

### 2.3 Where Servo is a *genuine* differentiator (not just a constraint)

Servo is winning or co-leading in a few real places that map directly to swerve's pitch:

- **Bleeding-edge crypto/standards:** Servo leads on Web Cryptography (ML-KEM, ML-DSA
  post-quantum), shipped 0.0.5 with post-quantum crypto.[^servo2026] A "secure, modern,
  Rust-from-the-ground-up" story has teeth.
- **Footprint & process model:** 0.0.5 cut four threads per instance and made IPC/single-process
  mode faster.[^servo2026] The "light on resources" pitch (the thing Opera GX *fakes* with
  RAM limiters) can be *real* on Servo.
- **Embeddability is Servo's actual mission.** Servo's north star is "embed web tech in apps,"
  and the embedding API now exposes proxies, root certs, localStorage/sessionStorage, cookies,
  dialogs, console, and GDPR `clear_site_data()`.[^servo2026] swerve's "engine reusable by
  other apps" goal (M5) is *with the grain* of where Servo invests, not against it.

**Net:** Servo is a genuine differentiator on *identity, security posture, footprint, and
embeddability*, and a genuine liability on *web compat and maintenance load*. The strategy
must spend the differentiators to buy patience on the liabilities.

---

## 3. The realistic target user

Do not aim at "everyone who uses Chrome." Aim at the intersection where independence is a
*feature*, not a tax:

### 3.1 Primary persona — "The Power Customizer / Anti-Google Tinkerer"

- Already runs a non-default browser (Vivaldi, Brave, Arc, Firefox, Opera GX) and *enjoys
  configuring it*. Probably keeps two browsers anyway.
- Ideologically or practically anti-Google: hates telemetry, MV3, forced AI features, the
  monoculture. Vivaldi's anti-AI stance and the Brave/Mullvad/LibreWolf audience prove this
  segment is real and reachable.[^privacy]
- Linux-comfortable / developer-adjacent / tech-enthusiast (this is exactly where Servo,
  Ladybird, and Omarchy-style projects get oxygen and where Cloudflare-sponsored independence
  resonates).[^ladybird]
- **Tolerates** a "second browser" with rough edges *if* it's theirs to shape and spies on no
  one.

### 3.2 Secondary persona — "The Aesthetics-First / Opera-GX refugee"

- Opera GX grew **0 → 34M+ users** on customization + a gaming aesthetic + RAM/CPU/network
  limiters (which are mostly UX theater over Chromium).[^operagx] That audience *demonstrably*
  values deep theming over raw compat. They are reachable by an Opera-GX-class theming system
  that is **actually performant** because it sits on a lean Rust engine, not faked with a
  limiter on a heavy one — and that is *not* funded by ads/telemetry.

### 3.3 Tertiary / future — "Embedder developer"

- Tauri-style app builders who want a non-Chromium, memory-safe webview they control. This is
  Servo's own mission, validates the M5 external-engine track, and is a B2B/OSS-credibility
  flywheel rather than a consumer play.

### 3.4 Who swerve is explicitly NOT for (state it, don't apologize)

- People who need 100% of every site to work on the first try (banking-only users,
  enterprise SSO captives, "my browser is whatever came with the laptop").
- People for whom the browser is invisible plumbing. They will never notice independence and
  will churn at the first broken checkout.

Sizing reality: Firefox at 2.26% is the ceiling of the pure values-driven audience; Opera GX
at 34M shows the customization audience is larger and more capturable. **swerve's realistic
addressable market is the *overlap* of those two — small in absolute share, but real,
loyal, vocal, and currently *unserved by an independent engine*.**

---

## 4. The wedge — what we win on (and what we refuse to fight on)

> Rule: **Do not try to beat Chrome on raw parity. You will lose, and chasing it is what kills
> Servo embedders.** Win on the four axes where an independent, owned, lean stack is
> structurally advantaged.

| Axis | Incumbent reality | swerve's structural advantage | Win condition for v1 |
|---|---|---|---|
| **Independence** | Only Ladybird can match; nobody ships it for consumers | Engine + funding both clean of Google | The story is *true* and prominent; the only such consumer browser |
| **Customization / theming** | Opera GX fakes performance; Vivaldi is deep but Chromium-heavy | Chrome is HTML rendered by Servo → theming is first-class & cheap | Opera-GX-class theming that's *native*, scriptable, performant |
| **Zero telemetry / no AI cram-down** | Chromium forks fight upstream; Chrome adds AI by default | Nothing phones home unless swerve adds it; believable | Verifiably zero outbound telemetry; AI strictly opt-in |
| **Footprint / performance** | GX uses limiters as a band-aid on a heavy engine | Lean Rust engine, fewer threads, single-process option | Lower idle RAM & process count than Chrome on the same tabs |

What "winning" looks like concretely:

1. **Theming as the headline feature**, not a settings sub-page. The chrome being HTML-in-Servo
   (already built: M1–M4) is the unique enabler — full CSS/JS theming of the browser UI with no
   privileged-extension hoops. This is the demo that sells the browser.
2. **A built-in content blocker in the engine** (MV3 can't touch it), shipped on by default.
3. **A "what swerve sends" page** that proves zero telemetry — turn the honesty into a feature.
4. **Sync via Lyku as a *trust* feature** (self-hostable, no-Google), not just convenience —
   reinforces the independence story instead of contradicting it.

### What we refuse to fight on in v1
- Raw site-compat parity (manage it; don't promise it).
- Extension-ecosystem breadth (Chrome Web Store is a moat; do not try to clone it for v1).
- Mobile (Servo mobile exists via Kumo etc., but it's a second front; defer).
- DRM/Widevine-gated streaming (Netflix-class) — out of scope for v1, flag it honestly.

---

## 5. "Parity-ish without bloat" — what it should concretely mean

"Parity-ish without bloat" is a *strategy*, not a slogan. Concretely:

### 5.1 Parity-ish = match the **interaction surface**, not the **render surface**
Users perceive "a real browser" through features they touch every day, most of which are
*chrome/UX*, not engine: tabs, history, bookmarks, find-in-page, downloads, password autofill,
session restore, omnibox suggestions, settings, sync. **These are achievable without Servo
parity** and are where v1 effort should go. swerve already has tabs, omnibox nav, back/fwd,
history bridge (M1–M5).

### 5.2 "Without bloat" = a hard, *enforced* exclusion list
Bloat is not vague; name it and ban it from v1:
- No telemetry/analytics/crash-phone-home-by-default.
- No bundled AI assistant, no "AI" in the omnibox by default (Vivaldi's anti-AI stance is a
  *selling point* in 2026).[^privacy]
- No ad/affiliate injection, no sponsored tiles, no "news feed."
- No account *required* for anything; Lyku sync is opt-in and self-hostable.
- No background services that run when the browser is closed.
- No shovelware (crypto wallets, VPN upsells, rewards tokens à la Brave).

### 5.3 Compat: manage the gap honestly with an explicit escape hatch
Because Servo is at 62% WPT, v1 must turn breakage from a betrayal into an expected,
gracefully-handled event:
- **A compat signal in the UI**: when a page hits known-unsupported features, say so plainly
  rather than rendering garbage.
- **One-click "open in system browser"** for the site that doesn't work yet — this is what makes
  "second browser by choice" honest and keeps users from rage-quitting.
- **A known-issues/compat page** maintained from Servo's BWA gaps so expectations are set
  *before* the user hits a wall.
- **Track and surface the Servo readiness curve** so users see the gap closing over time.

### 5.4 The phased ambition
- **v1 ("second browser, by choice"):** great chrome, deep theming, zero telemetry, blocker
  built in, honest compat + escape hatch. Daily-usable for the target persona's *primary*
  sites; explicitly not their bank.
- **v2 ("primary for the faithful"):** Lyku sync, extensions (a curated subset / WebExtensions
  if Servo supports it), richer compat as Servo's curve rises, mobile maybe.
- **v3 ("only browser" / SwerveOS optional):** only credible if Servo crosses ~90%+ BWA —
  i.e. years out and contingent on Servo funding, *not* on swerve's effort. Do not stake v1
  messaging on it.

---

## 6. Honest risks (positioning-level)

| # | Risk | Likelihood | Impact | Why it's a *positioning* risk | Mitigation |
|---|---|---|---|---|---|
| R1 | **Servo-embedding maintenance treadmill** (the Verso failure mode) | High | Fatal | Project death > any feature; killed the direct predecessor Oct 2025[^verso] | Pinned rev + high-level `libservo` + deliberate bumps (already adopted); budget the bump as recurring work, not a surprise |
| R2 | **Web-compat breakage churns users** | High | High | Independence is invisible until a site breaks, then it's *all* the user sees | "Second browser by choice" framing + open-in-system-browser escape hatch + honest compat page (§5.3) |
| R3 | **"Independent engine" doesn't move people; Firefox proves values alone ≈ 2%** | Medium-High | High | The thesis is true but a weak consumer hook on its own | Lead with customization + zero-telemetry; independence is the *credibility* layer, not the headline |
| R4 | **Ladybird out-executes on the independence story** (funded by Cloudflare et al., alpha 2026)[^ladybird] | Medium | Medium | Both own "independent, non-Google engine"; Ladybird has more funding/press | Differentiate on *experience/customization* (Ladybird = conformance engine, not a GX-class consumer browser); position as complementary |
| R5 | **Servo's compat curve plateaus** (~80% BWA ~2037 at current funding)[^readiness] | Medium | High | If Servo stalls, swerve's "only browser someday" promise evaporates | Never promise it; keep v1 viable at *today's* compat; track Servo funding as an external dependency |
| R6 | **Solo/tiny team can't sustain a "real browser" perception** | High | Medium | Users compare to Google-funded incumbents; rough edges read as "abandoned" | Scope to a *narrow excellent* product (theming-first) not a broad mediocre one; ship the demo that wins |
| R7 | **Independence/privacy story undercut by Lyku** (a sync service) | Low-Medium | Medium | A cloud service can look like the thing swerve claims to oppose | Self-hostable, E2E, opt-in, no account required for the browser; make Lyku *prove* the thesis, not dent it |
| R8 | **DRM/streaming & enterprise SSO gaps** read as "toy" | Medium | Medium | Netflix/Teams not working confirms "not a real browser" to some | Set expectations up front; out-of-scope-for-v1 honestly; escape hatch covers it |

---

## 7. Positioning statement (proposed)

> **swerve** is the independent browser for people who want their browser to be *theirs*.
> It's built on Servo — a Rust web engine that owes nothing to Google, Blink, or Chromium —
> so it spies on nothing, ships no AI you didn't ask for, and bends to your will: the entire
> interface is themeable like a web page, because it *is* one. It won't render every site on
> the internet yet — Servo is younger than Chrome and we're honest about that — so swerve makes
> it one click to hand a stubborn site to your other browser. Independent by construction.
> Customizable to the core. Zero telemetry. The browser you keep *because* you chose it.

**Tagline candidates:** "Your browser. Not Google's." / "Independent by construction." /
"The browser that's actually yours."

---

## 8. Prioritized recommendations

1. **(P0) Adopt "second browser, by choice" as the explicit v1 stance.** It defuses R2/R5,
   makes the compat gap honest, and is the only framing that survives 62%-WPT reality.
2. **(P0) Make deep theming the headline, not a setting.** The HTML-chrome-in-Servo
   architecture (already built) is swerve's one un-copyable consumer feature. The launch demo
   is "watch me re-skin the entire browser with CSS." Beat Opera GX on *real* performance.
3. **(P0) Treat the Servo bump as scheduled recurring work with an owner and a checklist** (rev
   + toolchain + `winit_minimal` API recheck + clean lock). This is the anti-Verso discipline;
   it is a *positioning* decision because it's what keeps the project alive (R1).
4. **(P1) Build the content blocker into the engine and ship it on by default.** Structural win
   vs. MV3 that no Chromium fork can fully match.[^mv3]
5. **(P1) Ship a verifiable "zero telemetry" guarantee** + an in-browser "what swerve sends"
   page. Turn honesty into a feature; it's the believable version of a claim Chromium forks
   can't make.
6. **(P1) Build the one-click "open in system browser" escape hatch + a live compat page.** This
   is the safety valve that makes R2 survivable.
7. **(P2) Frame Lyku as a trust feature**: self-hostable, E2E, opt-in, no-account-required.
   Make sync *reinforce* independence (R7).
8. **(P2) Position relative to Ladybird as complementary, not competitive** (different engine,
   conformance-vs-experience). Don't pick a fight with the other independent darling (R4).
9. **(P3) Defer mobile, DRM streaming, and any Chrome-Web-Store-scale extension ambition** out
   of v1; name them as out-of-scope honestly to protect the "no bloat" promise (§5.2).

---

## Appendix — key quantified facts (June 2026)

- Chrome **65.1%** all-device / **76.39%** desktop; Blink powers **>75%** of web sessions.
  Safari 18.4% all-device. Edge >5% all-device / 9.14% desktop. **Firefox 2.26% all-device**
  (the ceiling of the values-only audience).[^share]
- **Mozilla ≈85% of revenue** from the Google search default; that deal **expires end of
  2026**; the Sept 2025 antitrust remedy let Google keep paying but ended exclusivity.[^mozilla]
- **Servo: 62% WPT pass; 19.8% Baseline-Widely-Available ready** (87/439 categorized full =
  19.8%, 333 partial = 75.9%, 19 unsupported = 4.3%; 593 BWA features in the catalog total);
  141 features "zero velocity," 51 regressed >5pts; **~80%
  plateau projected ~2037** at ≈22 done/yr vs ≈52 added/yr.[^readiness]
- **Servo funding**: ~$33.6k raised 2024 (Open Collective + GH Sponsors, ~500 donors); Igalia
  staff + Sovereign Tech Fund grant; LF Europe governance; Igalia = 26% of PRs.[^servofund]
- **Servo 2026 momentum**: 0.0.5 shipped post-quantum crypto (ML-KEM/ML-DSA), color-mix(),
  contrast-color(), cyclic imports/import-attributes/JSON-modules, GDPR clear_site_data(),
  −4 threads/instance, faster single-process IPC; embedding API now covers proxies, root
  certs, storage, cookies, dialogs.[^servo2026]
- **Verso archived Oct 8 2025** — couldn't track Servo's churn with limited
  funding/manpower.[^verso] No actively-maintained general-purpose Servo consumer browser
  exists in 2026; servoshell (egui test harness) is the only reliably maintained UI.[^madewith]
- **Ladybird**: independent LibWeb engine, alpha 2026 / beta 2027 / stable 2028, ~90% of its
  own web tests (Oct 2025), Cloudflare-sponsored, stopped public PRs June 2026.[^ladybird]
- **Opera GX**: 0 → 34M+ users on customization + RAM/CPU/network limiters (UX theater over
  Chromium) + 10,000+ mods; reached Linux March 2026.[^operagx] Proof the customization
  audience is large and capturable.
- **MV3**: Chrome completed MV3 late 2024; full uBlock Origin dead on Chrome
  (`webRequest`→`declarativeNetRequest`, no dynamic filtering); only Lite remains.[^mv3]

[^share]: StatCounter / DigitalApplied / DemandSage browser market share 2026 —
  https://gs.statcounter.com/browser-market-share ,
  https://www.digitalapplied.com/blog/browser-market-share-2026-complete-statistics ,
  https://www.demandsage.com/browser-market-share/
[^mozilla]: OMG Ubuntu, Computerworld, PiunikaWeb on the Google–Mozilla search deal / 2025
  antitrust remedy / 2026 appeal —
  https://www.omgubuntu.co.uk/2025/09/google-antitrust-ruling-firefox-search-deal ,
  https://www.computerworld.com/article/3977372/mozilla-firefox-could-be-collateral-damage-in-googles-antitrust-battle.html ,
  https://piunikaweb.com/2026/06/02/mozilla-firefox-google-search-deal-not-exclusive-antitrust-appeal/
[^privacy]: GetDailyToolbox / Factually / bravebrowserstats on Brave/Vivaldi/Mullvad/LibreWolf
  positioning and the Blink monoculture —
  https://getdailytoolbox.com/security-privacy/best-privacy-browsers/ ,
  https://factually.co/product-reviews/electronics-tech/best-chromium-forks-privacy-2026-brave-vivaldi-others-63dc8e
[^readiness]: Servo Baseline Readiness — https://webtransitions.org/servo-readiness/ ; Servo
  WPT dashboard — https://servo.org/wpt/ . NOTE on the denominator (verified 2026-06-18):
  the source reports a 593-feature BWA catalog but computes its headline percentages over the
  439 features that carry a recorded status — 87 full (19.8%), 333 partial (75.9%), 19
  unsupported (4.3%). 87/439 = 19.8%; 87/593 = 14.7%. This plan pins the single canonical
  statement to **87/439 = 19.8% full**, and always names 593 as the catalog total, never as the
  percentage denominator. Earlier drafts that wrote "87/593 = 19.8%" were arithmetically
  inconsistent and have been corrected here, in engine-gap.md, and in ROADMAP.md.
[^servofund]: Servo sponsorship / Igalia / LF Europe / Open Collective; "Servo in 2024" —
  https://servo.org/sponsorship/ , https://opencollective.com/servo ,
  https://servo.org/blog/2025/01/31/servo-in-2024/ ,
  https://www.igalia.com/2025/10/09/Igalia,-Servo,-and-the-Sovereign-Tech-Fund.html
[^servo2026]: Phoronix "Servo January 2026"; heise "Servo 0.0.5"; HowToGeek "Servo 0.0.4" —
  https://www.phoronix.com/news/Servo-January-2026 ,
  https://www.heise.de/en/news/Browser-engine-Servo-0-0-5-released-with-post-quantum-cryptography-11195613.html
[^verso]: versotile-org/verso (archived); Verso 0.1 post —
  https://github.com/versotile-org/verso/ , https://wusyong.github.io/posts/verso-0-1/
[^madewith]: Servo "Made With" — https://servo.org/made-with/
[^ladybird]: Ladybird.org; Wikipedia; Cloudflare sponsorship; BrainDetox on the June 2026 PR
  policy — https://ladybird.org/ , https://en.wikipedia.org/wiki/Ladybird_(web_browser) ,
  https://blog.cloudflare.com/supporting-the-future-of-the-open-web/ ,
  https://braindetox.kr/en/posts/ladybird_browser_development_changes_2026.html
[^operagx]: Opera GX (opera.com/gx); perfcore review; Opera newsroom Linux launch —
  https://www.opera.com/gx , https://perfcore.com/opera-gx-gaming-browser-review-pros-cons-and-key-features/ ,
  https://press.opera.com/2026/03/19/opera-gx-gaming-browser-lands-on-linux/
[^mv3]: TheNextWeb / Ghostery / DEV on MV3 and uBlock Origin —
  https://thenextweb.com/news/chrome-manifest-v3-ublock-origin-content-blockers-disabled ,
  https://www.ghostery.com/blog/ublock-origin-not-supported-chrome
