# navgator sync — settings & data sync (Lyku + self-hostable)

> Dimension owner doc. Scope: what syncs, the data model + versioning, conflict
> resolution per data type, end-to-end encryption (especially secrets/passwords),
> transport/protocol, offline + merge, and the self-hosted server design.
>
> Status date: 2026-06-18. Repo state verified against `/raid/navgator` @ working tree
> and Servo source @ `ed1af70` (`/home/nicole/.cargo/git/checkouts/servo-e53a6e7b994a25fe/ed1af70`).

---

## 0. TL;DR / recommendations (prioritized)

1. **Build the local data layer FIRST, sync second.** navgator today has **zero
   persistence** — it uses `ServoBuilder::default()` with no `config_dir`, so even
   cookies/localStorage are not written to a stable on-disk location, and there is no
   settings/bookmarks/history store at all (verified: only `Arc`/`Mutex` state in
   `src/main.rs`; no `sqlite`/`serde`/`config` references). Sync is a *replication*
   layer over a local store that does not yet exist. Do not design sync as the first
   thing; design the **local profile store** (SQLite, see §3) and make every syncable
   datatype a clean table with a stable id + version columns from day one.

2. **Lyku is not an identifiable existing product** (3 web searches returned Logitech
   Sync, PlatformSync, xBrowserSync, Twilio Sync — none named "Lyku"). Treat Lyku as
   *the user's own account/backend platform, to be built or specified*. Design a
   **`SyncProvider` trait** with two implementations from the start: `LykuProvider`
   (the hosted default) and `SelfHostProvider` (the same wire protocol, your server
   binary). This makes "self-hostable later" a config flag, not a rewrite. See §8 for
   the **exact contract navgator needs from Lyku** (auth, blob API, storage model).

3. **The crypto is nearly free — it's already in the dependency graph.** Servo's
   transitive deps already pull in, at these exact versions:
   `argon2 0.6.0-rc.8`, `chacha20poly1305 0.11.0-rc.3`, `aes-gcm`, `hkdf 0.12.4`,
   `hmac`, `sha2`, `blake2 0.11.0-rc.6`, `x25519-dalek 2.0.1`, `curve25519-dalek`,
   `zeroize 1.9.0`, `subtle 2.6.1`, `password-hash 0.6.1`, `ring`, `aws-lc-rs`,
   plus `rusqlite 0.37.0`, `serde_json 1.0.150`, `tokio 1.52.3`, `hyper 1.10.1`.
   That is the *entire* toolkit for zero-knowledge E2EE sync with **no new heavy
   dependencies**. (Caveat: several are `-rc` versions pinned by Servo's churn — see
   the risk in §11.) Notably **absent** and to be chosen: a CRDT crate (`automerge`/
   `yrs`), an HTTP *client* (`reqwest`/`ureq` — Servo gives `hyper` the server/low
   level, not a client wrapper), a binary serializer (`ciborium`/`rmp-serde`), and an
   OS-keychain crate (`keyring`/`secret-service`).

4. **Zero-knowledge by default, with a hard split between secrets and everything
   else.** Two key tiers: a **Profile Key** (covers settings/themes/bookmarks/history/
   tabs) and a separately-derived **Vault Key** (covers passwords/secrets, gated by a
   second factor or re-prompt). The server stores **only ciphertext + opaque metadata
   (record id, datatype, version vector, ciphertext, MACs)** and can never read
   content. See §5–6.

5. **Conflict resolution is per-datatype, not one-size-fits-all** (§4): LWW for scalar
   settings; OR-Set / add-wins CRDT for bookmarks & open-tabs collections; append-only
   log (no merge needed) for history; field-level LWW with a per-record vault for
   passwords. Avoid a global CRDT document — it bloats and leaks structure.

6. **Sequencing**: this is a *post-M5* feature. Realistic order — (a) local profile
   store + settings UI, (b) `SyncEngine` + `SyncProvider` trait with a **local-folder
   provider** for testing (no server), (c) self-host server binary speaking the wire
   protocol, (d) E2EE for non-secret data, (e) the password vault with its stricter
   crypto + recovery, (f) hosted Lyku. Ship encryption *before* the first networked
   provider so plaintext never leaves the device, ever.

---

## 1. Current state (verified facts, not aspiration)

| Aspect | Reality in repo today | Source |
| --- | --- | --- |
| Settings store | None. No settings/prefs file, no schema. | `grep` of `src/main.rs`: no `settings`/`prefs`/`config` |
| Profile / data dir | None. `ServoBuilder::default()`, `opts.config_dir` never set. | `src/main.rs:544`; Servo `components/config/opts.rs:66,256` |
| Bookmarks / history | Do not exist as features or storage. | repo has no such code |
| Open-tabs persistence | Tabs are in-memory `Vec<Tab>`; lost on exit. | `docs/ARCHITECTURE.md` M4 |
| Password manager | None (Servo has no built-in credential UI we surface). | repo |
| Web storage (cookies/localStorage/IndexedDB) | Servo *can* persist these via its SQLite-backed storage thread, **but only when a `config_dir` is supplied**; navgator supplies none, so they are effectively ephemeral / default-located. | Servo `components/storage/client_storage.rs:10` (`use rusqlite::...`), `storage_thread.rs:18` (`config_dir`) |
| Crypto libs available | Full suite already transitively present (see §0.3). | `Cargo.lock` |
| Network client | None used by navgator; `hyper 1.10.1` is in-graph (Servo's net stack) but no ergonomic *client* wrapper. | `Cargo.lock` |
| IPC control surface | `NAVGATOR_IPC` Unix socket, text protocol — relevant later as a hook to *trigger* a sync or read sync status from outside. | `docs/ARCHITECTURE.md` M5 |

**Implication.** Two of the six "what syncs" categories (settings, bookmarks, history,
passwords, open-tabs are mostly *not implemented yet*; only open-tabs exists and only
in memory). Sync work is gated on building those features with sync-friendly storage.
The single highest-leverage decision is to **set `config_dir` to a real per-OS profile
path now** (so Servo's own cookie/localStorage SQLite lands somewhere stable) and to
put navgator's own data in a sibling SQLite DB in that same profile dir.

---

## 2. What syncs (and what must NOT)

| Datatype | Sync? | Sensitivity | Volume / shape | Default merge (see §4) |
| --- | --- | --- | --- | --- |
| **Settings / preferences** | Yes | Low | ~dozens of scalar keys | LWW per key |
| **Themes / mods** (Opera GX-class) | Yes | Low–med (CSS/JS payloads) | small–medium blobs; can be large with assets | LWW per mod; asset blobs content-addressed |
| **Bookmarks** | Yes | Med (URLs reveal interests) | hundreds–thousands of nodes, tree | OR-Set + tree-move CRDT |
| **History** | Opt-in (default ON, easy OFF) | **High** (most revealing) | high volume, append-heavy | Append-only log, dedup by (url,visit_time) |
| **Open tabs / session** | Yes (per-device + "send to device") | Med | tens of entries, churny | OR-Set keyed by tab uuid, per-device view |
| **Passwords / secrets** | Yes, **separate vault** | **Critical** | tens–hundreds | Field-level LWW inside per-record envelope |
| **Autofill (addresses, cards)** | Yes (in vault) | **Critical** | small | as passwords |
| **Extensions/permissions state** | Later | Med | small | LWW |
| **Cookies / localStorage / site data** | **No by default** | Critical, huge, fragile | very large, session-bound | n/a — see note |

**Do NOT sync:** raw cookies / session tokens / localStorage / IndexedDB by default.
They are large, security-sensitive (active session theft), and Servo already manages
them locally. Chrome syncs *some* site data behind opt-in; for v1, exclude entirely.
**Do NOT sync:** telemetry, device identifiers, anything that would reintroduce the
surveillance this project exists to avoid. The sync server must never need a stable
cross-device *user-content* identifier beyond the account id.

---

## 3. Local data model + the profile store

### 3.1 Profile directory

Set `opts.config_dir` to a per-OS path and reuse it for everything:

| OS | Path |
| --- | --- |
| Linux | `$XDG_DATA_HOME/navgator/<profile>` (fallback `~/.local/share/navgator/<profile>`) |
| macOS | `~/Library/Application Support/navgator/<profile>` |
| Windows | `%LOCALAPPDATA%\navgator\<profile>` |

(Use the `directories`/`dirs` crate — NOT yet in the graph; tiny, add it.) Multiple
named profiles supported from day one (`default`, others); each profile = one Lyku
account binding.

### 3.2 Storage layout in the profile dir

```
<profile>/
  servo/                 # config_dir handed to Servo → cookies/localStorage/IndexedDB SQLite
  navgator.db              # our data: settings, bookmarks, history, tabs, themes, sync state
  vault.db               # passwords/secrets — separate file, separate key (defence in depth)
  sync/
    device_id            # random 128-bit, never leaves device in plaintext
    keys.enc             # wrapped Profile/Vault keys (see §5)
```

Use **`rusqlite` (already at 0.37.0)** for `navgator.db` and `vault.db`. SQLite gives us
transactions, a stable on-disk schema, and—critically—lets sync be expressed as
"diff the table against the last-synced version vector."

### 3.3 Universal record envelope

Every syncable record, regardless of datatype, carries the same metadata so the sync
engine is datatype-generic:

```rust
struct SyncRecord {
    id: Uuid,            // stable, client-generated (v4 or v7); never reused
    datatype: Datatype,  // Settings | Bookmark | HistoryVisit | Tab | Theme | Vault…
    version: VersionVector, // {device_id -> counter}; bumped on local change
    deleted: bool,       // tombstone (kept for a TTL so deletes propagate)
    updated_at: i64,     // device clock, ms — TIE-BREAK ONLY, never authoritative
    payload: Vec<u8>,    // serialized datatype body (see §3.4) — encrypted before upload
}
```

`version` is a **version vector / dotted-version-vector**, not a wall clock.
`updated_at` is used only as a deterministic tie-break for LWW after the vector says
"concurrent." This is what makes merges correct under clock skew across devices.

### 3.4 Per-datatype payloads (local plaintext shape)

- **Settings**: `{ key: String, value: Json }` one row per key. Tiny.
- **Theme/mod**: `{ name, css: String, js: Option<String>, enabled: bool, assets: [Hash] }`.
  Assets (fonts/images) are **content-addressed blobs** (sha-256) stored once,
  referenced by hash — so a 4 MB theme background syncs as one blob, dedup'd.
- **Bookmark**: `{ parent: Uuid, kind: Folder|Link, title, url?, sort_key }`.
  Tree modeled by `parent` pointer + fractional-index `sort_key` (see §4.3).
- **HistoryVisit**: `{ url, title, visit_time, transition }`. Append-only.
- **Tab/session**: `{ tab_uuid, window_uuid, url, title, position, device_id }`.
- **Vault entry**: `{ origin, username, password, totp_secret?, notes? }` — never
  stored or transmitted in plaintext; see §6.

---

## 4. Conflict resolution — per datatype

The core principle: **pick the weakest mechanism that is still correct for the
datatype.** A global CRDT is overkill and leaks structure to the server.

| Datatype | Strategy | Why |
| --- | --- | --- |
| Settings (scalars) | **LWW per key** via version vector + `updated_at` tie-break | Last edit genuinely wins; no semantic merge wanted (you don't want "half a theme color"). |
| Theme/mod content | **LWW per mod**; assets immutable (content-addressed) | A mod is an atomic unit; concurrent edits to the same mod → newest wins, the loser is recoverable from version history (§4.4). |
| Bookmarks | **Add-wins OR-Set** for existence + **fractional indexing** for order + **last-writer-wins move** for re-parenting | Concurrent adds must both survive (OR-Set); ordering must converge without renumbering (fractional index); moving a node is a single-field LWW. Avoids the classic "bookmark resurrection / duplication" bugs. |
| History | **Append-only log, no merge** (CRDT-trivial: a grow-only set keyed by `(url, visit_time, device)`) | Visits are facts that happened; you never "merge" a visit. Dedup is exact-key. Deletes are tombstones with TTL. |
| Open tabs / session | **OR-Set keyed by tab uuid, partitioned by device** | Each device owns its window/tab set; you display others read-only ("tabs from <device>"). No cross-device write contention, so merge is trivial union. |
| Passwords / vault | **Field-level LWW inside a per-record encrypted envelope** | Concurrent edits to *different* fields (e.g., note vs. password) both survive; same field → newest wins, loser kept in encrypted history for recovery. Whole-record CRDT is unnecessary and the record is opaque to the server anyway. |

### 4.1 LWW correctness

LWW here is **version-vector LWW**, not naive timestamp LWW. Procedure on receiving a
remote record R for local record L:
1. If `R.version` dominates `L.version` → take R.
2. If `L.version` dominates `R.version` → keep L (server is stale).
3. If **concurrent** (neither dominates) → tie-break by `updated_at`, then by `id` as a
   final deterministic tie-break so all devices converge to the same winner. Surface a
   "this changed on another device" note for high-value records.

### 4.2 Why not one big CRDT (e.g. Automerge) for everything

- **Privacy/structure leak**: a single Automerge doc's op-log reveals editing structure
  even when payloads are encrypted; we want the server to see only opaque per-record
  ciphertext.
- **Size**: Automerge keeps full op history; for high-volume history this is unbounded
  growth. Append-only-log + tombstone-TTL is far cheaper.
- **Dependency cost**: adds a heavy crate; the per-datatype approach above needs only
  small, auditable code (OR-Set and fractional index are ~a few hundred LOC).
- **Where a CRDT *is* worth it**: bookmark ordering and the bookmark tree. Use a small,
  purpose-built OR-Set + fractional index there rather than a general engine.

### 4.3 Fractional indexing for ordered collections

`sort_key` is a string fractional index (LexoRank/`fractional-indexing`-style): inserting
between `a0` and `a1` yields `a0V`. Concurrent inserts at the same slot get distinct keys
(append device_id suffix to break ties). No renumbering, no write amplification, merges
by simple string sort.

### 4.4 Version history / undo

Keep N prior versions (or a time window) of mutable records (settings, bookmarks, vault
entries) locally so "restore previous" works after a bad merge. For the vault, prior
versions stay encrypted. This is the safety net that makes LWW acceptable for important
data.

---

## 5. Cryptography — keys & zero-knowledge architecture

### 5.1 Threat model

- Server (Lyku or self-hosted) is **honest-but-curious / potentially breached** — it
  must learn nothing about content. Zero-knowledge.
- Network is hostile (TLS still required for metadata protection + replay/MITM, but
  *content* security must not depend on TLS).
- Other devices on the account are trusted once enrolled.
- The user's master password is the root of trust; losing it = losing data unless a
  recovery key was created (§7).

### 5.2 Key hierarchy

```
              master password (never leaves device, never sent to server)
                       │
        Argon2id(password, per-account random salt)  ──►  Master Key (MK, 32B)
                       │
        ┌──────────────┼───────────────────────────────┐
        │              │                                │
   HKDF "auth"   HKDF "profile"                   (Vault Key, separate — §6)
        │              │
   Auth Secret    Profile Key (PK, 32B)
   (login proof)       │
                  per-record keys via HKDF(PK, record.id)  → XChaCha20-Poly1305
```

- **KDF**: `argon2id` (already in graph, `argon2 0.6.0-rc.8`). Params tuned to ~250–500ms
  on target hardware (e.g. m=64 MiB–256 MiB, t=3, p=1; calibrate at first run, store the
  params alongside the salt so all devices reproduce the same MK).
- **Salt**: 16B random per account, stored server-side *as account metadata* (it's not
  secret; it just must be the same on every device). Fetched at login.
- **Subkey derivation**: `HKDF-SHA256` (`hkdf 0.12.4`) with distinct `info` labels to
  split MK into independent purposes. Never reuse a derived key across purposes.
- **AEAD**: `XChaCha20-Poly1305` (`chacha20poly1305 0.11.0-rc.3`, XChaCha variant for
  192-bit random nonces → no nonce-reuse bookkeeping). Per-record key =
  `HKDF(PK, info=record.id)`; random 24B nonce per encryption; AAD binds
  `record.id || datatype || version` so the server can't swap ciphertexts between
  records or roll versions back undetectably.
- **Memory hygiene**: wrap MK/PK/VK in `zeroize::Zeroizing` (`zeroize 1.9.0` in graph);
  never log; never serialize plaintext keys.

### 5.3 Zero-knowledge authentication (no password to server)

Do **not** send the password or even MK to the server. Use an augmented PAKE-style or,
pragmatically for v1, an **OPAQUE-like / SRP-like** flow OR the simpler well-trodden
Bitwarden model:

- **Pragmatic v1 (Bitwarden-style, easy to get right):**
  - Client derives MK from password (Argon2id).
  - Client derives `Auth Secret = HKDF(MK, info="auth")`, then hashes it again with a
    *second* Argon2/PBKDF pass before sending → server stores only a hash of the
    auth secret (server-side hash again with its own salt). Server verifies login
    without ever seeing MK or password.
  - The actual data-encryption key (PK/VK) is **wrapped** by a key derived from MK and
    stored server-side as an opaque blob (`keys.enc`), so a new device that knows the
    password can decrypt it after login. Server never sees the unwrapping key.
- **Stronger (v2): OPAQUE** (aPAKE) so even the auth verifier doesn't enable offline
  brute force from a server breach. Heavier to implement; defer.

The point: **server breach yields only Argon2id-hardened verifiers + ciphertext.** No
plaintext, no usable keys.

### 5.4 Device enrollment

New device: user enters password → derives MK → authenticates → downloads wrapped keys
blob → unwraps PK (and VK after second factor). Optionally a QR/transfer enrollment
where an existing device wraps the keys to the new device's ephemeral X25519 public key
(`x25519-dalek 2.0.1` in graph) — this avoids re-entering the master password and is the
nicer UX. Device list is shown to the user; revoking a device rotates… (see §7.3).

---

## 6. The password / secrets vault (rigorous)

Passwords get a **stricter, separate** regime than the rest of sync.

1. **Separate key (Vault Key, VK).** `VK = HKDF(MK, info="vault")` *plus* an optional
   second factor: if the user sets a separate vault PIN/biometric, VK is additionally
   wrapped by a key derived from that, so unlocking the browser ≠ unlocking the vault.
   Recommended default: vault auto-locks after inactivity and on app start.
2. **Per-entry envelope encryption.** Each vault entry has its own random 32B
   content key `CK`, used with XChaCha20-Poly1305 to encrypt the entry payload. `CK` is
   wrapped by `VK` (AES-KW — `aes-kw` is in graph — or XChaCha). This means rotating VK
   re-wraps small CKs, not re-encrypting every entry's data, and field-level history is
   cheap.
3. **Field-level structure** so merges don't clobber: payload =
   `{ origin, username (enc), password (enc), totp (enc), notes (enc), per-field version }`.
   Field-level LWW (§4) inside the envelope.
4. **AAD binds origin + entry id + version** → server cannot move a credential from one
   site to another or roll back to an old (possibly cracked-elsewhere) password without
   the AEAD failing.
5. **OS keychain integration (optional, recommended).** Cache the unwrapped VK in the OS
   keychain (`keyring`/`secret-service` — NOT yet in graph, add it) so the user isn't
   re-prompted constantly, while the *encrypted-at-rest* guarantee for the DB file
   holds. On Linux this is Secret Service / kwallet; macOS Keychain; Windows DPAPI/CredMan.
6. **Never** put passwords in the same DB key domain as settings; never log; `Zeroizing`
   everywhere; constant-time compares via `subtle 2.6.1` (in graph).
7. **Breach checks** (k-anonymity HIBP-style) must be done **client-side** with range
   queries so the server never sees a password hash prefix tied to the account.

### 6.1 Why not store passwords as just "another synced record"
Because the blast radius of a vault-key compromise or a merge bug is account-takeover of
every site. The separate file, separate key, second-factor gate, per-entry envelope, and
AAD-binding are all defence-in-depth so that no single mistake in the generic sync path
exposes credentials.

---

## 7. Recovery, rotation, and key loss

| Scenario | Handling |
| --- | --- |
| Forgot master password | **Recovery key**: at setup, generate a 128-bit recovery key (BIP39-style words), wrap MK under it, store that wrap server-side. User stores the words offline. Without it, **data is unrecoverable by design** (zero-knowledge) — state this loudly in the UI. |
| Password change | Re-derive MK′; re-wrap PK/VK under MK′; upload new wrapped blob + new auth verifier. Data ciphertext untouched (it's keyed by PK, not MK). Cheap. |
| Compromised device | Revoke device server-side (drops its sessions). For true forward security, rotate PK/VK: generate PK′, re-encrypt records lazily (on next write) or eagerly; bump a `key_epoch` so old devices can't decrypt new data. |
| Lost recovery key + password | Unrecoverable. This is the cost of zero-knowledge; offer an optional, clearly-labeled "escrow to Lyku" mode for non-security-critical users (NOT for the vault). |
| Account migration (Lyku → self-host) | Since the server holds only ciphertext + metadata, migration = copy blobs to the new server, re-point the provider URL, re-auth. Keys never change. This is a *feature* of zero-knowledge. |

---

## 8. SyncProvider abstraction + EXACTLY what Lyku must provide

Because Lyku can't be identified, define the contract; any backend (Lyku, self-host,
even a local folder for tests) implements it.

```rust
#[async_trait]
trait SyncProvider {
    async fn login(&self, account: &str, auth_proof: AuthProof) -> Result<Session>;
    async fn keys_blob(&self, s: &Session) -> Result<Vec<u8>>;        // wrapped PK/VK
    async fn put_keys_blob(&self, s: &Session, blob: &[u8]) -> Result<()>;
    // Records are opaque ciphertext + metadata; server never decrypts.
    async fn pull(&self, s: &Session, since: &Cursor) -> Result<(Vec<EncRecord>, Cursor)>;
    async fn push(&self, s: &Session, recs: &[EncRecord]) -> Result<PushResult>;
    async fn blobs_put(&self, s: &Session, hash: Hash, data: &[u8]) -> Result<()>; // content-addressed assets
    async fn blobs_get(&self, s: &Session, hash: Hash) -> Result<Vec<u8>>;
    async fn subscribe(&self, s: &Session) -> Result<EventStream>;     // optional push (WS/SSE)
    async fn list_devices(&self, s: &Session) -> Result<Vec<DeviceInfo>>;
    async fn revoke_device(&self, s: &Session, id: DeviceId) -> Result<()>;
}
```

Implementations: `LykuProvider` (hosted default), `SelfHostProvider` (your server),
`LocalFolderProvider` (writes EncRecords to a directory — for tests + "sync via my own
Syncthing/Dropbox folder" power-user mode, zero server needed).

### 8.1 What navgator needs FROM Lyku (flag precisely)

If Lyku is to be the default backend, navgator needs Lyku to provide, **at minimum**:

1. **Account + auth**: an account identifier and an auth flow that proves possession of
   an *Argon2id-derived auth secret* without revealing the master password (i.e. Lyku
   must accept "store this opaque verifier; verify against it" — NOT "send me your
   password"). OAuth/OIDC alone is **insufficient** for zero-knowledge: OIDC
   authenticates the *user to Lyku* but does not establish the *content key*. We need
   either (a) Lyku to host the wrapped-keys blob + verifier (Bitwarden model), or (b)
   an OPAQUE endpoint. **Action: confirm which Lyku supports.**
2. **Opaque blob storage API**: per-account key/value where value is ciphertext, with
   (a) optimistic-concurrency (version/etag) on push, (b) a monotonic cursor for
   incremental pull, (c) per-record metadata fields (id, datatype, version vector,
   updated_at, deleted) stored *as-is* without server interpretation.
3. **Content-addressed blob store** for theme assets (sha-256 keyed, dedup, quota).
4. **Device registry + session/token management + revocation.**
5. **Optional realtime channel** (WebSocket/SSE) to push "you have updates" so we don't
   poll. If absent, fall back to interval polling + push-on-foreground.
6. **Quota + rate-limit semantics** surfaced to the client so we can backpressure.
7. **Storage model guarantee**: Lyku stores ciphertext only; documented zero-knowledge;
   no server-side content scanning.

If Lyku can't meet (1)+(2)+(7), it is not a viable *sync* backend (only an identity
provider), and the self-host server (§10) becomes the reference and Lyku is relegated to
auth/identity in front of it.

---

## 9. Transport / protocol

- **Transport**: HTTPS (TLS via `rustls 0.23` already in graph) for request/response;
  optional WebSocket/SSE for the "updates available" signal. Content security does NOT
  depend on TLS (everything is E2EE), but TLS protects metadata + prevents tampering.
- **HTTP client**: navgator has `hyper 1.10.1` (low-level) but no client wrapper. Add a
  thin client (`reqwest` with rustls, or `ureq` for a smaller/sync footprint). Prefer
  `reqwest` (rustls feature) since `tokio` is already in-graph; keep it behind the
  `sync` feature so non-sync builds don't pay for it.
- **Wire format**: JSON for the control/metadata envelope (`serde_json` in graph) is
  fine and debuggable; **record payloads are opaque base64/binary ciphertext**. For
  efficiency at scale, offer CBOR (`ciborium`) for the batch pull/push body. Records are
  batched (push/pull arrays), not one-request-per-record.
- **Sync algorithm (incremental)**:
  1. `pull(since=cursor)` → list of EncRecords changed since cursor + new cursor.
  2. Decrypt, merge each per §4 against local store (in a single SQLite txn).
  3. Collect locally-changed records since last push → encrypt → `push`.
  4. Server applies with optimistic concurrency: if a record's server-version moved,
     reject that record → client re-pulls + re-merges + re-pushes (CRDT/LWW makes this
     convergent, not a hard conflict).
  5. Persist new cursor + per-record last-synced version.
- **Cadence**: on change (debounced ~2–5s), on foreground, on interval (e.g. 5 min), and
  on the realtime signal. Backoff on errors. Coalesce bursts.

---

## 10. Self-hosted server design

Goal: a **single small Rust binary** that any user can run; same wire protocol as Lyku.

- **Stack**: `axum`/`hyper` (hyper already in graph) + `rusqlite`/`sqlx` for storage,
  `tokio` runtime (in graph). Single binary, single SQLite file by default; optional
  Postgres for multi-user/large deployments.
- **Schema** (server sees only opaque data):
  ```
  accounts(account_id, argon2_salt, argon2_params, auth_verifier, recovery_wrap, created_at)
  keys_blob(account_id, wrapped_keys, version)
  records(account_id, record_id, datatype, version_vector, updated_at, deleted,
          ciphertext, aad_tag, server_seq)        -- server_seq = monotonic cursor source
  blobs(account_id, sha256, bytes, refcount)      -- content-addressed assets
  devices(account_id, device_id, pubkey, last_seen, label, revoked)
  sessions(token_hash, account_id, device_id, expires_at)
  ```
- **Endpoints**: mirror the `SyncProvider` trait (`POST /login`, `GET/PUT /keys`,
  `GET /pull?since=`, `POST /push`, `PUT/GET /blobs/:hash`, `GET /events` (WS/SSE),
  `GET/DELETE /devices`).
- **Concurrency**: `server_seq` is a per-account monotonic counter; `pull(since)` returns
  rows with `server_seq > since`. `push` uses per-record optimistic concurrency on
  `version_vector` (reject + tell client current value).
- **Zero-knowledge invariants** (must hold in code + tests): server never receives a
  master password or unwrapped key; `ciphertext` is opaque; no content-based indexing;
  the only plaintext is structural metadata (ids, datatypes, vectors, timestamps,
  sizes). Document the metadata leakage explicitly (the server *can* infer "this account
  has N bookmarks and synced at time T" — acceptable; "what they are" — never).
- **Ops**: single-binary + Docker image; `navgator-sync-server --data ./data`; backups =
  copy the SQLite file (it's all ciphertext, safe to back up anywhere). Quotas + basic
  rate limiting. Federation/multi-tenant explicitly out of scope for v1.
- **Lyku ↔ self-host parity**: if both implement the identical protocol, "switch backend"
  is a settings change + re-auth; no data reformatting because the server only moved
  ciphertext.

---

## 11. Risks

- **Servo-pinned `-rc` crypto versions.** `argon2 0.6.0-rc.8`, `chacha20poly1305
  0.11.0-rc.3`, `blake2 0.11.0-rc.6` are *release candidates* pulled by Servo's lockfile.
  Building sync on them means our crypto API surface can shift when we bump the Servo
  rev. **Mitigation**: either pin these explicitly in navgator's `Cargo.toml` to the same
  rev Servo uses and wrap them behind a small internal `crypto` module (single choke
  point to fix on a bump), or vendor stable releases independently — but a divergent
  version doubles compile cost (two copies of the same crate). Track this on every Servo
  bump, same discipline as the Servo rev itself.
- **Crypto correctness is unforgiving.** A nonce-reuse, an unbound AAD, or a naive
  timestamp-LWW that loses a password is catastrophic and silent. **Mitigation**:
  isolate all crypto in one audited module; property-test the merge functions
  (convergence: any order of applying ops yields the same state); use XChaCha (random
  nonces) to remove nonce-management foot-guns; consider an external review before the
  vault ships.
- **Sync is built on unbuilt features.** Settings/bookmarks/history/password-manager
  don't exist yet. Sync timelines are really *feature + storage + sync* timelines.
  **Mitigation**: build each feature's local store with the §3.3 envelope from the
  start; never bolt sync on after.
- **Lyku is undefined.** Designing against an unknown backend risks rework if Lyku's
  real auth model is OIDC-only (insufficient for ZK). **Mitigation**: the `SyncProvider`
  trait + `LocalFolderProvider` + self-host server mean we can ship *real, useful,
  encrypted* sync with **zero dependency on Lyku**, and adopt Lyku as one provider once
  its contract (§8.1) is confirmed.
- **Recovery UX vs zero-knowledge.** True ZK means lost-password = lost-data; users hate
  this. **Mitigation**: mandatory recovery-key generation at setup with explicit
  acknowledgement; optional (clearly-labeled, non-vault) escrow.
- **Merge bugs eat user data.** **Mitigation**: per-record local version history (§4.4),
  tombstones with TTL (so deletes converge but resurrection is impossible), and a
  "sync paused / review changes" safety mode if divergence is detected.
- **Metadata leakage.** Even with E2EE, the server learns sync timing, record counts,
  datatypes, sizes. **Mitigation**: document it; optionally pad ciphertext sizes and
  batch to blur timing; don't pretend it's perfectly private.
- **Battery/bandwidth.** Naive polling/full-sync drains both. **Mitigation**: cursor-based
  incremental sync + debounce + realtime signal + content-addressed asset dedup.

---

## 12. Open questions

- **What is Lyku, concretely?** Auth model (OIDC? custom? OPAQUE-capable?), does it offer
  opaque blob + content-addressed storage, what's its zero-knowledge stance, quota model,
  realtime channel? §8.1 is the checklist to answer before committing Lyku as default.
- **CRDT crate vs hand-rolled?** Recommendation is hand-rolled OR-Set + fractional index
  (smaller, auditable), but if bookmarks/notes grow richer, is `automerge` worth the
  weight for *just* those datatypes?
- **HTTP client choice**: `reqwest` (tokio, already in graph) vs `ureq` (smaller, sync)?
  Affects binary size and the async story of the sync engine.
- **OS keychain dependency**: accept `keyring`/`secret-service` cross-platform variance,
  or roll a minimal per-OS shim? Affects the password-vault unlock UX.
- **Auth: ship Bitwarden-model v1 then migrate to OPAQUE, or do OPAQUE up front?** OPAQUE
  is stronger but heavier and the migration changes the verifier format.
- **History sync default on or off?** It's the most sensitive non-secret datatype;
  privacy-first positioning argues for default-off, parity-with-Chrome argues default-on.
- **Do we sync extension/permission state and site-specific zoom/perms in v1, or defer?**
- **Multi-account / multi-profile**: one Lyku account per profile, or one account with
  multiple profiles server-side?
- **Quota & abuse** on hosted Lyku: free tier limits, how the client surfaces "over quota."

---

## 13. Must-haves for a v1 industry-standard browser (this dimension)

(See structured summary.) In short: a stable local profile store with the universal
record envelope; end-to-end encryption with zero-knowledge by default; a separate,
hardened password vault with per-entry envelope encryption and a second-factor gate;
per-datatype conflict resolution (not timestamp-LWW everywhere); cursor-based incremental
sync; a pluggable `SyncProvider` with a working self-host server and a local-folder
provider so sync is useful without Lyku; and mandatory recovery-key UX.
