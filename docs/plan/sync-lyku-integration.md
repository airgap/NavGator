# swerve ⇄ Lyku — sync integration design

> Concrete integration design for syncing swerve (Servo-based browser, `/raid/swerve`)
> user data through **Lyku** (lyku.org, `/raid/lyku`) as the default `SyncProvider`,
> with a self-hostable server speaking the same protocol.
>
> Status date: 2026-06-18. **Lyku side verified against the working tree at `/raid/lyku`**
> (auth gateway, lockstep-core, pg-models/pg-config, mapi-models, r2-client, nats-client,
> and the existing `synced<T>` framework). swerve side per `docs/plan/sync.md`,
> `docs/plan/security.md`, repo @ working tree.
>
> This document **supersedes the "Lyku is unidentifiable" framing in `docs/plan/sync.md`**.
> Everything in `sync.md` about the *client-side* crypto/CRDT/local-store design still
> holds; this doc grounds the *server contract* in Lyku's actual code and names exact
> tables, routes, and patterns to add. Read `sync.md` first for the local-store and
> crypto rationale; this doc is the Lyku-binding layer.

---

## 0. TL;DR

1. **Lyku is real and is a near-ideal sync backend.** It already has: opaque
   server-side session tokens *and* `lyk_`-prefixed scoped API keys *and* a full
   OAuth2/OIDC provider (`requester: bigint` is injected into every authenticated
   handler); a Cloudflare-R2 presigned-upload→confirm blob flow with per-user key
   namespacing and quotas (`zoidSaves`); NATS pub/sub with a per-user subject
   convention (`<domain>.${userId}`); and — critically — **a generic `synced<T>`
   replication framework** (`registerSynced` / `publishSynced` / `streamSynced`,
   `{value, schema_version, sequence}` envelopes, a trigger-bumped monotonic `sequence`
   column) that is exactly the delta-sync primitive swerve needs. swerve should **ride
   these existing rails**, not invent parallel ones.

2. **swerve must do all crypto client-side; Lyku stores only ciphertext.** Lyku never
   sees plaintext. We add new tables whose payloads are `bytea`/`text` ciphertext and
   whose only plaintext columns are structural metadata (`userId`, `datatype`, version
   vector, `sequence`, sizes, timestamps). This is compatible with — and invisible to —
   Lyku's `synced<T>` machinery, which treats the row opaquely except for the `sequence`
   it bumps.

3. **Auth: OAuth2/OIDC for first identity hand-off, then a scoped Lyku API key for the
   sync session.** Lyku's OAuth2 `access_token` is **not** accepted by the MessagePack
   API gateway (verified: the gateway only accepts `lyk_` API keys or `sessions`/cookie
   tokens). So the browser uses OIDC to establish identity once, then mints a
   sync-scoped `apiKey` (`createApiKey`) that the sync engine uses for all subsequent
   MessagePack calls. The Lyku account credential is **not** the encryption root —
   a separate sync passphrase is (see §4).

4. **Wire format is MessagePack** (`application/x-msgpack`, msgpackr-compatible), bigint
   IDs are native. The Rust client needs an `rmp-serde` (or `rmpv`) codec, not JSON, for
   the Lyku provider. Self-host provider can use the same.

5. **New Lyku surface is small and idiomatic**: ~3 pg-models tables + their pg-config
   table files, ~7 mapi-models contracts + handlers, one `registerSynced` entry, one new
   R2 bucket (or key prefix). Everything follows patterns that already exist verbatim in
   the repo (§1, §2).

6. **Self-host parity is free**: because Lyku stores only ciphertext + a `sequence`
   cursor, the self-host server is the same five endpoints over SQLite/Postgres. Switching
   backends = re-auth + re-point URL; no data reformatting (§7).

---

## 1. What Lyku already provides vs. what swerve-sync needs Lyku to add

### 1.1 Already provided (verified, reusable as-is)

| Capability | Lyku mechanism | Source |
| --- | --- | --- |
| **Account identity** | `users` table, `id: snowflake` (bigint). No email/password on it; email + PII live in the `userHashes` vault. | `libs/pg-models/src/user.ts`, `hashdoc.ts` |
| **Session tokens** | `sessions` table; PK *is* the bearer token (256-bit base64url, `varchar(44)`). Cookie `sessionId=…; Secure; HttpOnly; SameSite=Lax` **or** `Authorization: Bearer <token>`. Tiered Redis+PG validation, expiry-checked. | `libs/route-helpers/src/{serveMultiHttp,getSessionsTiered,generateSessionId}.ts`, `pg-models/src/{session,sessionId}.ts` |
| **Scoped API keys** | `apiKeys` table; `lyk_<64hex>` raw key, sha256 stored as `keyHash`, `scopes: text[]`, `expiresAt`/`revokedAt`. `createApiKey` returns the raw key once. The gateway validates `Authorization: Bearer lyk_…` directly and enforces `scopes` (`'*'` or route name). | `pg-models/src/apiKey.ts`, `routes/src/core/handlers/createApiKey.ts`, `serveMultiHttp.ts:363-406` |
| **OAuth2 / OIDC provider** | `/oauth/authorize`, `/oauth/token` (auth-code + PKCE S256 + refresh), `/oauth/userinfo`, `/.well-known/openid-configuration`, JWKS. `oauthClients` (`registerOAuthClient`), `oauthTokens`. | `routes/src/core/raw/oauth*.ts`, `pg-models/src/{oauthClient,oauthToken}.ts` |
| **Auth injection into handlers** | `authenticated: true` ⇒ handler 2nd arg is `{ requester: bigint, session: string }` (non-optional). Unauth ⇒ optional. | `route-helpers/src/Contexts.ts`, `serveMultiHttp.ts:522-533` |
| **R2 presigned blob storage** | `getUploadUrl(key, contentType?, contentLength?, bucket)` / `getDownloadUrl` / `fileExists` / `deleteFile`, 15-min presign, buckets `media|saves`, key namespacing `…/${userId}/…`, **`requestChecksumCalculation:'WHEN_REQUIRED'`** (mandatory or R2 PUT 403s). | `libs/r2-client/src/index.ts` |
| **Blob upload→confirm pattern** | `requestZoidSaveUpload`→client PUTs→`confirmZoidSaveUpload` (verifies via `fileExists`, flips `status`). pg row records `storageKey`, `sizeBytes`, `md5Checksum`, `status`. Per-file + per-user quota (`507`) by subscription tier. | `routes/src/games/handlers/{requestZoidSaveUpload,confirmZoidSaveUpload,requestZoidSaveDownload,deleteZoidSave}.ts`, `pg-models/src/zoidSave.ts` |
| **NATS realtime** | Core NATS (no JetStream — **no persistence/replay**). `nats.publish(subject, Uint8Array.from(pack(payload)))`; subject convention `<domain>.${id}` (e.g. `notifications.${userId}`, `users.${id}`). | `libs/nats-client/src/index.ts`, `routes/src/social/handlers/sendChatMessage.ts` |
| **WS↔NATS bridge** | `serveMultiWebsocket` authenticates the socket (`sessionId` cookie/Bearer), injects `{requester, emit, onClose}`. Listen-handlers do `sub=nats.subscribe(subject); onEach(sub, m=>emit(m.data)); onClose(()=>sub.unsubscribe())`. | `route-helpers/src/serveMultiWebsocket.ts`, `routes/src/social/handlers/listenForNotifications.ts` |
| **Generic `synced<T>` replication** | `registerSynced({prefix, subjectOf, schema, authorize, project?})`; `publishSynced(prefix, row)` builds `{value, schema_version, sequence}` and publishes on the subject; `streamSynced` is one WS endpoint for *all* registered models. `sequence` is a `managedColumns` bigint (`default 1n`) bumped by the `sync_sequence` BEFORE-UPDATE trigger. | `route-helpers/src/{syncedRegistry,syncedModels,publishSynced}.ts`, `routes/src/core/handlers/{streamSynced,streamCurrentUser}.ts`, `pg-config/src/updateSequence.ts` |
| **Push notifications (mobile/offline)** | `sendNotification(...)` writes a durable `notifications` row + NATS + `sendPushToUser` (FCM/WebPush). | `route-helpers/src/sendNotification.ts`, `pg-models/src/deviceToken.ts` |

**Crucial finding:** the `sequence`/`synced<T>` system is the missing piece `sync.md`
worried about ("a monotonic cursor for incremental pull"). It already exists. Each row
carries a per-row monotonic `sequence`; the envelope publishes it; a client can order and
gate on it. swerve's delta-sync cursor *is* `max(sequence)` seen — no new cursor
infrastructure required.

**Crucial caveat (auth):** the OAuth2 `access_token` from `/oauth/token` is validated only
by the OIDC `userinfo` endpoint, **not** by `serveMultiHttp` (the MessagePack gateway). So
OAuth alone cannot drive the sync API. Use OAuth/OIDC to *identify*, then `createApiKey` (or
reuse the active `sessionId`) to *operate*. This matches `sync.md` §8.1's warning that
"OIDC alone is insufficient."

### 1.2 What swerve-sync needs Lyku to add

Three new tables, ~7 routes+handlers, one `registerSynced` entry, one R2 bucket/prefix.
All in Lyku's exact idiom. Concrete snippets in §2.

| Add | Why |
| --- | --- |
| `syncRecord` table (+`syncRecords` config) | one row per (user, datatype, recordId): ciphertext + version vector + `sequence`. The opaque per-record store. |
| `syncKeyBlob` table (+`syncKeyBlobs` config) | per-user wrapped-keys blob (the E2EE key bundle) + recovery wrap + KDF params. One row per user (or per key-epoch). |
| `syncBlob` table (+`syncBlobs` config) | metadata for large content-addressed encrypted assets in R2 (theme wallpapers/sounds). Mirrors `zoidSave`. |
| `syncDevice` table (+`syncDevices` config) | device registry (label, X25519 pubkey for device-to-device key transfer, last-seen, revoked). Distinct from `e2eeDeviceKeys` (that's Signal chat) and `deviceToken` (that's push). |
| Routes: `pushSyncRecords`, `pullSyncRecords`, `getSyncKeyBlob`, `putSyncKeyBlob`, `authorizeSyncBlobUpload`, `confirmSyncBlobUpload`, `requestSyncBlobDownload`, `listenForSyncUpdates`, `registerSyncDevice`, `listSyncDevices`, `revokeSyncDevice` | the SyncProvider surface (§5). |
| `registerSynced({prefix:'syncRecord', subjectOf: id => \`syncRecords.${id}\`, ...})` | so `publishSynced` after each `pushSyncRecords` write nudges the user's devices through the existing `streamSynced`/NATS path. |
| New R2 bucket `R2_SYNC_BUCKET` (or `sync/` prefix in `media`) | encrypted asset blobs, key `sync/${userId}/${sha256}.bin`. Add `'sync'` to `R2Bucket`. |
| API-key scopes `read:sync`, `write:sync` | add to the `createApiKey` scope enum so a sync key can't touch posts/messages. |

---

## 2. Lyku-side data model (new tables + routes, in Lyku's exact style)

### 2.1 `syncRecord` — the opaque per-record store

The universal record envelope from `sync.md` §3.3, server-side. Plaintext columns are
structural only; `ciphertext` is `bytea` (Lyku's binary type — maps to `Buffer`; first
real use, but fully supported by lockstep-core and SQL gen). `sequence` is the managed
monotonic column the `synced<T>` system reads.

`libs/pg-models/src/syncRecord.ts`:
```ts
import type { PostgresRecordModel } from '@lyku/lockstep-core';

export const syncDatatype = {
	type: 'enum',
	enum: ['settings', 'theme', 'bookmark', 'history', 'tab', 'vault', 'autofill'],
} as const;

export const syncRecord = {
	description: 'One end-to-end-encrypted sync record. Server stores only ciphertext + structural metadata; it never holds keys or plaintext.',
	properties: {
		// Composite identity: a sync record is (userId, recordId). recordId is a
		// client-generated UUIDv7 string (stable, never reused).
		userId: { type: 'bigint' },
		recordId: { type: 'text', maxLength: 36, pattern: '^[0-9a-f-]{36}$' },
		datatype: syncDatatype,
		// Version vector {deviceId -> counter} for CRDT/LWW merge. Opaque to the
		// server; stored as jsonb so it CAN be range/equality-checked for optimistic
		// concurrency but is never interpreted semantically.
		versionVector: { type: 'jsonb', description: '{ deviceId: counter } dotted version vector' },
		// E2EE payload: XChaCha20-Poly1305 ciphertext of the record body. Opaque.
		ciphertext: { type: 'bytea', description: 'Client-encrypted record body. Server cannot read.' },
		// AEAD nonce (24B for XChaCha) carried alongside; not secret.
		nonce: { type: 'bytea' },
		deleted: { type: 'timestamptz', description: 'Tombstone time. Kept for a TTL so deletes propagate, then GC.' },
		updatedAt: { type: 'bigint', description: 'Client clock ms — LWW tie-break only, never authoritative.' },
		created: { type: 'timestamptz', default: { sql: 'CURRENT_TIMESTAMP' } },
		updated: { type: 'timestamptz' },
	},
	required: ['userId', 'recordId', 'datatype', 'versionVector', 'ciphertext', 'nonce', 'created'],
} as const satisfies PostgresRecordModel;
```

`libs/pg-config/src/tables/syncRecords.ts` — note the **`sequence` managed column +
`updateSequence` trigger** (so each write advances a per-row cursor for delta pull) and the
**composite primary key**:
```ts
import { PostgresTableModel } from '@lyku/lockstep-core';
import { syncRecord } from '@lyku/pg-models';
import { updateUpdated } from '../updateUpdated';
import { updateSequence } from '../updateSequence';

export const syncRecords = {
	schema: syncRecord,
	primaryKey: ['userId', 'recordId'],
	// Delta pull is: WHERE userId = ? AND sequence > ? ORDER BY sequence.
	indexes: [
		'userId',
		['userId', 'sequence'],
		['userId', 'datatype'],
		{ columns: ['userId', 'deleted'], name: 'idx_sync_records_tombstones', where: '"deleted" IS NOT NULL' },
	],
	triggers: [updateUpdated, updateSequence],
	// Per-row monotonic cursor for synced<T>; DEFAULT 1 covers inserts, the
	// sync_sequence trigger bumps on update.
	managedColumns: { sequence: { type: 'bigint', default: 1n } },
} as const satisfies PostgresTableModel<typeof syncRecord>;
```

> Note on cursor scope: Lyku's `sequence` is **per-row**, not per-account-monotonic.
> `sync.md` §10 assumed a per-account `server_seq`. Per-row `sequence` still works for
> delta pull (`WHERE userId=? AND sequence > cursor`) **as long as the index is
> `(userId, sequence)` and the client tracks the max sequence it has seen**. If a future
> need for a strictly account-global monotonic counter arises (e.g. exactly-once cursor
> semantics across record deletes), add an `accountSeq` from a per-user counter; for v1
> the per-row `(userId, sequence)` index is sufficient and idiomatic.

### 2.2 `syncKeyBlob` — wrapped E2EE keys + recovery

`libs/pg-models/src/syncKeyBlob.ts`:
```ts
import type { PostgresRecordModel } from '@lyku/lockstep-core';

export const syncKeyBlob = {
	description: 'Per-user wrapped E2EE key bundle. Opaque ciphertext + non-secret KDF params. Lets a new device that knows the sync passphrase recover keys after login.',
	properties: {
		userId: { type: 'bigint' },
		keyEpoch: { type: 'integer', default: 1, description: 'Bumped on key rotation; old data stays decryptable by old epoch keys held in the bundle.' },
		// Argon2id params + salt — NOT secret, must be identical on every device.
		kdfParams: { type: 'jsonb', description: '{ alg:"argon2id", m, t, p, salt(b64) }' },
		// Profile+Vault keys wrapped under the passphrase-derived KEK. Opaque.
		wrappedKeys: { type: 'bytea' },
		// MK wrapped under the recovery key (BIP39). Opaque. Optional escrow variant elsewhere.
		recoveryWrap: { type: 'bytea' },
		// Auth verifier is NOT here — Lyku auth is the account session/API key (§3),
		// independent of the E2EE passphrase. This blob is pure key escrow.
		created: { type: 'timestamptz', default: { sql: 'CURRENT_TIMESTAMP' } },
		updated: { type: 'timestamptz' },
	},
	required: ['userId', 'keyEpoch', 'kdfParams', 'wrappedKeys', 'created'],
} as const satisfies PostgresRecordModel;
```
`libs/pg-config/src/tables/syncKeyBlobs.ts`:
```ts
import { PostgresTableModel } from '@lyku/lockstep-core';
import { syncKeyBlob } from '@lyku/pg-models';
import { updateUpdated } from '../updateUpdated';

export const syncKeyBlobs = {
	schema: syncKeyBlob,
	primaryKey: ['userId', 'keyEpoch'],
	indexes: ['userId', ['userId', 'keyEpoch']],
	triggers: [updateUpdated],
} as const satisfies PostgresTableModel<typeof syncKeyBlob>;
```

### 2.3 `syncBlob` — large encrypted assets in R2 (mirrors `zoidSave`)

`libs/pg-models/src/syncBlob.ts`:
```ts
import type { PostgresRecordModel } from '@lyku/lockstep-core';

export const syncBlob = {
	description: 'Metadata for a large content-addressed encrypted sync asset (theme wallpaper/sound). Bytes live in R2; this row records key + status. Content is ciphertext.',
	properties: {
		id: { type: 'snowflake' },
		userId: { type: 'bigint' },
		// sha256 of the CIPHERTEXT (content-addressed for dedup). Hex.
		contentHash: { type: 'text', maxLength: 64, pattern: '^[0-9a-f]{64}$' },
		// R2 key: sync/${userId}/${contentHash}.bin
		storageKey: { type: 'text', maxLength: 512 },
		sizeBytes: { type: 'integer', description: 'Ciphertext size, for quota.' },
		refcount: { type: 'integer', default: 1, description: 'How many sync records reference this blob; GC at 0.' },
		status: { type: 'enum', enum: ['pending', 'complete', 'failed'], default: 'pending' },
		created: { type: 'timestamptz', default: { sql: 'CURRENT_TIMESTAMP' } },
		updated: { type: 'timestamptz' },
	},
	required: ['id', 'userId', 'contentHash', 'storageKey', 'sizeBytes', 'status', 'created'],
} as const satisfies PostgresRecordModel;
```
`libs/pg-config/src/tables/syncBlobs.ts`:
```ts
import { PostgresTableModel } from '@lyku/lockstep-core';
import { syncBlob } from '@lyku/pg-models';
import { updateUpdated } from '../updateUpdated';

export const syncBlobs = {
	schema: syncBlob,
	primaryKey: 'id',
	indexes: ['userId', ['userId', 'contentHash'], 'status'],
	unique: ['userId', 'contentHash'],
	triggers: [updateUpdated],
} as const satisfies PostgresTableModel<typeof syncBlob>;
```

### 2.4 `syncDevice` — device registry

`libs/pg-models/src/syncDevice.ts`:
```ts
import type { PostgresRecordModel } from '@lyku/lockstep-core';

export const syncDevice = {
	description: 'A swerve device enrolled in sync for this account. Used for device-to-device key transfer and revocation. Distinct from e2eeDeviceKeys (Signal chat) and deviceTokens (push).',
	properties: {
		userId: { type: 'bigint' },
		deviceId: { type: 'text', maxLength: 36, pattern: '^[0-9a-f-]{36}$' },
		label: { type: 'text', maxLength: 100 },
		platform: { type: 'enum', enum: ['linux', 'macos', 'windows'] },
		// X25519 public key (b64) so an existing device can wrap keys TO this device
		// at enrollment without re-typing the passphrase.
		publicKey: { type: 'text', maxLength: 64 },
		lastSeen: { type: 'timestamptz' },
		revoked: { type: 'timestamptz' },
		created: { type: 'timestamptz', default: { sql: 'CURRENT_TIMESTAMP' } },
	},
	required: ['userId', 'deviceId', 'platform', 'publicKey', 'created'],
} as const satisfies PostgresRecordModel;
```
`libs/pg-config/src/tables/syncDevices.ts`: `primaryKey: ['userId','deviceId']`,
`indexes: ['userId', ['userId','deviceId']]`, `triggers: [updateUpdated]` (same idiom).

### 2.5 Registration steps (the standard recipe)

- Add `export * from './syncRecord';` (etc.) to `libs/pg-models/src/index.ts`.
- Add `export * from './syncRecords';` (etc.) to `libs/pg-config/src/tables/index.ts`
  (`pg-config/src/index.ts` auto-picks up everything from `./tables`).
- `nx build @lyku/json-models` then `nx build @lyku/mapi-types`, migrate via `apps/dataform`.

### 2.6 Routes (mapi-models contracts) — Lyku idiom

**`pushSyncRecords`** — `libs/mapi-models/src/pushSyncRecords.pts`:
```ts
import type { TsonHandlerModel } from '@lyku/lockstep-core';
export const pushSyncRecords = {
	request: schema {
		type: 'object',
		properties: {
			deviceId: { type: 'string', maxLength: 36 },
			records: {
				type: 'array',
				maxItems: 500,
				items: {
					type: 'object',
					properties: {
						recordId: { type: 'string', maxLength: 36 },
						datatype: { enum: ['settings','theme','bookmark','history','tab','vault','autofill'] },
						versionVector: { type: 'object' }, // { deviceId: counter }
						// ciphertext + nonce as bigint-free byte arrays. MessagePack carries
						// raw bytes natively; in TsonSchema these are typed 'object'/'array'
						// of integers OR a base64 'string' — see note below.
						ciphertext: { type: 'string', description: 'base64 XChaCha20-Poly1305 ciphertext' },
						nonce: { type: 'string', description: 'base64 24-byte nonce' },
						deleted: { type: 'boolean' },
						updatedAt: { type: 'bigint' },
						// Optional base-version for optimistic concurrency: the sequence the
						// client last saw for this record (0 = expect-absent / insert).
						baseSequence: { type: 'bigint' },
					},
					required: ['recordId','datatype','versionVector','ciphertext','nonce','updatedAt'],
				},
			},
		},
		required: ['deviceId','records'],
	},
	response: schema {
		type: 'object',
		properties: {
			// Per-record outcome; rejected rows tell the client the current server
			// sequence so it can re-pull+re-merge+re-push (CRDT/LWW makes this converge).
			results: { type: 'array', items: { type: 'object', properties: {
				recordId: { type: 'string' },
				accepted: { type: 'boolean' },
				sequence: { type: 'bigint' },
			}, required: ['recordId','accepted'] } },
		},
		required: ['results'],
	},
	authenticated: true,
	throws: [400, 401, 409, 413, 429, 500],
	rateLimit: { requests: 60, period: '1m', scope: 'user' },
	title: 'Push Sync Records',
	category: 'Sync',
	tags: ['sync','e2ee'],
	since: '1.0.0',
} satisfies TsonHandlerModel
```

> **Binary-in-contract note (important):** TsonSchema has **no `bytea`/`Uint8Array`
> request type** (verified). Two options for ciphertext on the wire: (a) base64 `string`
> fields (simplest, ~33% overhead, shown above), or (b) carry the whole record batch as
> raw MessagePack bytes by having the client pack the byte arrays and the server store
> them — but the gateway validates against the declared TsonSchema, so the typed-`string`
> base64 approach is the honest, validatable one. For the *large asset* path, bytes never
> go through a contract at all — they go to R2 via presigned PUT (§2.7), so this overhead
> only touches small records.

**`pullSyncRecords`** — cursor pagination by `sequence` (Lyku's `before`/`limit` idiom,
adapted to `sinceSequence`):
```ts
export const pullSyncRecords = {
	request: schema {
		type: 'object',
		properties: {
			sinceSequence: { type: 'bigint', default: 0, description: 'Return records with sequence > this.' },
			limit: { type: 'integer', minimum: 1, maximum: 500, default: 200 },
			datatypes: { type: 'array', items: { enum: ['settings','theme','bookmark','history','tab','vault','autofill'] } },
		},
		required: [],
	},
	response: schema {
		type: 'object',
		properties: {
			records: { type: 'array', items: { type: 'object', properties: {
				recordId: { type: 'string' }, datatype: { type: 'string' },
				versionVector: { type: 'object' }, ciphertext: { type: 'string' },
				nonce: { type: 'string' }, deleted: { type: 'boolean' },
				sequence: { type: 'bigint' }, updatedAt: { type: 'bigint' },
			}, required: ['recordId','datatype','versionVector','ciphertext','nonce','sequence'] } },
			cursor: { type: 'bigint', description: 'Max sequence returned; pass back as sinceSequence.' },
			hasMore: { type: 'boolean' },
		},
		required: ['records','cursor','hasMore'],
	},
	authenticated: true,
	throws: [400, 401, 429, 500],
	title: 'Pull Sync Records', category: 'Sync', tags: ['sync','e2ee'], since: '1.0.0',
} satisfies TsonHandlerModel
```

**`getSyncKeyBlob`** (`response: { kdfParams, wrappedKeys(b64), recoveryWrap(b64), keyEpoch }`,
no request) and **`putSyncKeyBlob`** (request the same; called on setup, passphrase change,
rotation) — straight key/value, `authenticated: true`.

**`listenForSyncUpdates`** — the realtime nudge, exactly the `listenForNotifications` shape:
```ts
export const listenForSyncUpdates = {
	response: schema {
		type: 'object',
		properties: {
			// Lightweight nudge OR the synced<T> envelope. Carry the changed
			// datatypes + the new max sequence so the client pulls only what changed.
			datatypes: { type: 'array', items: { type: 'string' } },
			sequence: { type: 'bigint' },
		},
		required: ['sequence'],
	},
	stream: true,
	authenticated: true,
	throws: [400, 401, 500],
	title: 'Listen For Sync Updates', category: 'Sync', tags: ['sync','realtime','websocket'], since: '1.0.0',
} satisfies TsonHandlerModel
```

**Asset upload trio** (mirrors `authorizeMediaUpload`/`confirmMediaUpload`/`requestZoidSaveDownload`):
```ts
// authorizeSyncBlobUpload
request: schema { type:'object', properties:{ contentHash:{type:'string',maxLength:64}, sizeBytes:{type:'integer'} }, required:['contentHash','sizeBytes'] }
response: schema { type:'object', properties:{ id:{type:'bigint'}, url:{type:'string'}, alreadyExists:{type:'boolean'} }, required:['id','url','alreadyExists'] }
// confirmSyncBlobUpload: request bigint id, response boolean (fileExists → status 'complete')
// requestSyncBlobDownload: request { contentHash }, response { url, sizeBytes }
```

**Device routes**: `registerSyncDevice` (`{deviceId,label,platform,publicKey}`→bool),
`listSyncDevices` (→ array), `revokeSyncDevice` (`{deviceId}`→bool).

### 2.7 Handlers (Lyku idiom) — two representative examples

**`pushSyncRecords` handler** — `libs/routes/src/core/handlers/pushSyncRecords.ts`:
```ts
import { handlePushSyncRecords } from '@lyku/handles';
import { Err } from '@lyku/helpers';
import { client as pg } from '@lyku/pg-client';
import { publishSynced } from '@lyku/route-helpers';

export const pushSyncRecords = handlePushSyncRecords(
	async ({ deviceId, records }, { requester }) => {
		if (records.length > 500) throw new Err(413, 'Too many records');
		const results = [];
		await pg.transaction().execute(async (trx) => {
			for (const r of records) {
				// Optimistic concurrency: reject if server moved past the client's base.
				const existing = await trx.selectFrom('syncRecords')
					.select(['sequence'])
					.where('userId','=',requester).where('recordId','=',r.recordId)
					.executeTakeFirst();
				const serverSeq = existing?.sequence ?? 0n;
				if (r.baseSequence != null && serverSeq > r.baseSequence) {
					results.push({ recordId: r.recordId, accepted: false, sequence: serverSeq });
					continue;
				}
				const row = await trx.insertInto('syncRecords').values({
					userId: requester, recordId: r.recordId, datatype: r.datatype,
					versionVector: r.versionVector,
					ciphertext: Buffer.from(r.ciphertext, 'base64'),
					nonce: Buffer.from(r.nonce, 'base64'),
					deleted: r.deleted ? new Date() : null,
					updatedAt: r.updatedAt, created: new Date(),
				})
				.onConflict((oc) => oc.columns(['userId','recordId']).doUpdateSet({
					versionVector: r.versionVector,
					ciphertext: Buffer.from(r.ciphertext, 'base64'),
					nonce: Buffer.from(r.nonce, 'base64'),
					deleted: r.deleted ? new Date() : null,
					updatedAt: r.updatedAt, updated: new Date(),
				}))
				.returningAll().executeTakeFirstOrThrow();
				results.push({ recordId: r.recordId, accepted: true, sequence: row.sequence });
			}
		});
		// After commit: nudge the user's other devices via the synced<T> path.
		publishSynced('syncRecord', { id: requester, /* envelope value is opaque */ });
		return { results };
	},
);
```
> The `publishSynced('syncRecord', …)` call requires a `registerSynced` entry (§2.8). The
> envelope's `value` should be the *nudge* (datatypes + max sequence), **not** the
> ciphertext — keep the realtime channel cheap and metadata-only. Alternatively skip
> `synced<T>` and do a bare `nats.publish(\`syncUpdates.${requester}\`, …)` paired with a
> hand-written `listenForSyncUpdates` (the `listenForTypingIndicators` pattern). Either
> works; `synced<T>` is the more idiomatic, reuses `streamSynced`.

**`authorizeSyncBlobUpload` handler** — mirrors `authorizeMediaUpload` + `zoidSave` dedup:
```ts
import { handleAuthorizeSyncBlobUpload } from '@lyku/handles';
import { client as pg } from '@lyku/pg-client';
import { getUploadUrl } from '@lyku/r2-client';
import { Err } from '@lyku/helpers';

const MAX_BLOB = 50 * 1024 * 1024; // 50MB; quota by tier like zoidSaves

export const authorizeSyncBlobUpload = handleAuthorizeSyncBlobUpload(
	async ({ contentHash, sizeBytes }, { requester }) => {
		if (sizeBytes > MAX_BLOB) throw new Err(413, 'Blob too large');
		// Content-addressed dedup: if (user, hash) already complete, no upload needed.
		const existing = await pg.selectFrom('syncBlobs')
			.selectAll().where('userId','=',requester).where('contentHash','=',contentHash)
			.executeTakeFirst();
		if (existing?.status === 'complete')
			return { id: existing.id, url: '', alreadyExists: true };
		const id = existing?.id ?? /* snowflake */ genId();
		const storageKey = `sync/${requester}/${contentHash}.bin`;
		await pg.insertInto('syncBlobs').values({
			id, userId: requester, contentHash, storageKey, sizeBytes,
			status: 'pending', created: new Date(),
		}).onConflict((oc)=>oc.columns(['userId','contentHash']).doNothing()).execute();
		const { url } = await getUploadUrl(storageKey, 'application/octet-stream', sizeBytes, 'sync');
		return { id, url, alreadyExists: false };
	},
);
```

### 2.8 `registerSynced` entry — `libs/route-helpers/src/syncedModels.ts`

```ts
registerSynced({
	prefix: 'syncRecord',
	subjectOf: (id) => `syncUpdates.${id}`, // id is the userId here
	schema: { version: '1.0', properties: { datatypes: {}, sequence: {} } } as SyncedSchema,
	// Only the owner may stream their own sync nudges. requester must equal the key id.
	authorize: ({ requester, id }) => requester !== undefined && requester === id,
});
```

### 2.9 R2 bucket

Add `'sync'` to `R2Bucket` in `libs/r2-client/src/index.ts` and a `R2_SYNC_BUCKET` env var
(or reuse `media` with a `sync/` prefix — but a separate bucket eases quota/lifecycle and
keeps E2EE blobs out of the public media bucket). Keep the existing
`requestChecksumCalculation:'WHEN_REQUIRED'` config — presigned PUTs 403 without it.

---

## 3. Auth integration — reusing Lyku accounts

### 3.1 The flow (browser → sync API)

1. **Identify once via OIDC.** swerve registers a public OAuth client
   (`registerOAuthClient`, `requirePkce: true`, `confidential: false`, redirect URI
   `swerve://oauth/callback` or a loopback `http://127.0.0.1:<port>/callback`). User logs
   into Lyku in a webview/system browser; swerve runs auth-code + PKCE S256 against
   `/oauth/authorize` → `/oauth/token`, gets an `id_token` (account identity) and an
   `access_token`. This proves "this swerve install belongs to Lyku user `sub`."
2. **Mint a sync-scoped API key.** Because the OAuth `access_token` is **not** accepted by
   the MessagePack gateway (verified), swerve cannot call sync routes with it. Instead,
   immediately after OIDC, swerve calls `createApiKey({ name: 'swerve-<device-label>',
   scopes: ['read:sync','write:sync'] })` (using the active Lyku **session** established
   by the same login, i.e. the `sessionId` cookie/Bearer) and stores the returned
   `lyk_…` key in the **OS keychain**. All subsequent sync calls use
   `Authorization: Bearer lyk_…`.
   - Add `read:sync`/`write:sync` to the `createApiKey` scope enum and have the new sync
     routes register their scope requirement (`requiredPermissions` on the contract, or
     scope-name == route-name as the gateway already supports).
3. **Operate.** The sync engine sends MessagePack requests with the API-key bearer. The
   gateway resolves `requester = apiKeyRow.userId`; every sync handler reads `requester`.
4. **Revoke.** Revoking the key (or the device, §5) cuts that install off without touching
   the user's password or other devices.

**Simpler alternative for v1:** if swerve already drives a Lyku login (e.g. it embeds the
Lyku webui), it can read the `sessionId` and use it directly as the bearer — exactly what
`monolith-ts-api` does. The API-key path is preferred because it's scoped (a sync key
can't post or read messages) and independently revocable. Recommend API key for the
default; allow session-token fallback.

### 3.2 What auth is NOT

The Lyku account credential is **not** the E2EE root. Lyku does passwordless OTP / OAuth —
there is no user "master password" to derive keys from (the `userHashes.hash` is empty for
all current login methods). So swerve **must** introduce a *separate sync passphrase* as the
crypto root (§4). This is actually cleaner: account auth (who you are) and content
encryption (what you can decrypt) are fully decoupled — a Lyku session compromise yields
only ciphertext.

### 3.3 Transport specifics the Rust client must honor

- `Content-Type: application/x-msgpack`; body = `pack(request)`; response = `unpack(bytes)`.
- `Authorization: Bearer lyk_…` (API key) or `Bearer <sessionId>`.
- Native bigint in MessagePack (Lyku IDs are bigint). Rust: use `rmpv::Value` or
  `rmp-serde` with an i64/u64 mapping; preserve bigint where IDs round-trip.
- `498` response ⇒ session/key invalid ⇒ re-auth.
- WebSocket: connect to the WS service (`:3001` in dev), send the bearer in the first
  message or the upgrade cookie; `listenForSyncUpdates` then pushes nudges.

---

## 4. End-to-end crypto design

This refines `sync.md` §5–7 with the Lyku-specific fact that **there is no account
password to derive from** — so the crypto root is a dedicated **sync passphrase**, and the
wrapped-key bundle lives in `syncKeyBlob` (§2.2).

### 4.1 Key hierarchy

```
            sync passphrase  (user-chosen; NEVER sent to Lyku; not the Lyku login)
                   │
   Argon2id(passphrase, per-account salt, m/t/p from syncKeyBlob.kdfParams) ──► Master Key (MK, 32B)
                   │
        ┌──────────┴───────────────┐
   HKDF "profile"             HKDF "vault"
        │                          │
   Profile Key (PK)           Vault Key (VK)
        │                          │
   per-record key =           per-entry content key CK (random 32B),
   HKDF(PK, info=recordId)     wrapped under VK (AES-KW); entry encrypted with CK
        │                          │
        └──── XChaCha20-Poly1305 (24B random nonce) ────┘
              AAD = recordId ‖ datatype ‖ versionVector
```

- **KDF**: `argon2id` (in graph, `argon2 0.6.0-rc.8`). Params + 16B salt stored in
  `syncKeyBlob.kdfParams` (not secret; must be identical across devices). Calibrate at
  setup (~250–500ms).
- **Subkeys**: `HKDF-SHA256` (`hkdf 0.12.4`) with distinct `info` labels (`"profile"`,
  `"vault"`). Never reuse a derived key across purposes.
- **AEAD**: `XChaCha20-Poly1305` (`chacha20poly1305 0.11.0-rc.3`); per-record key
  `HKDF(PK, info=recordId)`; random 24B nonce; **AAD binds `recordId‖datatype‖
  versionVector`** so the server cannot swap ciphertexts between records, change a
  record's datatype, or roll its version back undetectably (the AEAD tag fails).
- **Vault (passwords/secrets)**: separate VK + per-entry CK envelope (AES-KW wrap, `aes-kw`
  in graph) + field-level structure + a **second-factor gate** (vault PIN/biometric wraps
  VK again so unlocking the browser ≠ unlocking the vault). Vault records use
  `datatype: 'vault'` and are stored in `syncRecord` like everything else (Lyku sees only
  ciphertext) but client-side live in a separate `vault.db` with the stricter regime from
  `sync.md` §6.
- **Memory hygiene**: `zeroize::Zeroizing` on MK/PK/VK/CK; `subtle` for constant-time
  compares. All in graph.

### 4.2 What Lyku stores (zero-knowledge guarantee)

| Lyku column | Content | Can Lyku read content? |
| --- | --- | --- |
| `syncRecord.ciphertext` / `.nonce` | XChaCha20-Poly1305 of the record body | **No** |
| `syncRecord.versionVector` | `{deviceId: counter}` | Yes (structural; reveals edit cadence/device count, not content) |
| `syncRecord.datatype`, `sequence`, timestamps | structural | Yes (reveals "N bookmarks, synced at T") |
| `syncKeyBlob.wrappedKeys` / `.recoveryWrap` | MK/PK/VK wrapped under passphrase/recovery KEK | **No** (cannot unwrap without passphrase/recovery key) |
| `syncKeyBlob.kdfParams` | Argon2 params + salt | Yes (not secret) |
| `syncBlob` bytes (in R2) | XChaCha20-Poly1305 of the asset | **No** |

Lyku learns metadata (counts, sizes, timing, datatypes, device count) — documented and
accepted, same as `sync.md` §11. It never learns plaintext. The passphrase never leaves the
device; MK/PK/VK never leave the device unwrapped.

### 4.3 Device enrollment, recovery, rotation

- **First device**: user sets sync passphrase → derive MK → wrap PK/VK → `putSyncKeyBlob`.
  Generate a BIP39 recovery key, wrap MK under it → store as `recoveryWrap`. Show the
  recovery words once, force acknowledgement (true ZK: lost passphrase + lost recovery =
  unrecoverable data).
- **New device**: OIDC login → `getSyncKeyBlob` → prompt passphrase → derive MK → unwrap
  PK (VK after second factor). Nicer UX: an existing device wraps keys to the new device's
  `syncDevice.publicKey` (X25519, `x25519-dalek` in graph) so no passphrase re-entry —
  device approves in-app.
- **Passphrase change**: re-derive MK′, re-wrap PK/VK, `putSyncKeyBlob`. Record ciphertext
  untouched (keyed by PK, not MK). Cheap.
- **Compromised device**: `revokeSyncDevice` + revoke its API key (cuts its sessions). For
  forward secrecy, bump `keyEpoch`: generate PK′/VK′, re-encrypt lazily on next write; old
  devices can't read new-epoch records.
- **Backend migration (Lyku → self-host)**: copy `syncRecord`/`syncKeyBlob`/R2 blobs (all
  ciphertext) to the new server, re-point provider URL, re-auth. Keys unchanged — a
  *feature* of ZK.

---

## 5. The Rust `SyncProvider` client in swerve

Refines `sync.md` §8. The trait is unchanged in spirit; the Lyku impl is now concrete.

```rust
#[async_trait]
pub trait SyncProvider: Send + Sync {
    async fn authenticate(&self, creds: &AccountAuth) -> Result<Session>;
    async fn get_key_blob(&self, s: &Session) -> Result<Option<KeyBlob>>;   // syncKeyBlob
    async fn put_key_blob(&self, s: &Session, b: &KeyBlob) -> Result<()>;
    async fn pull(&self, s: &Session, since: Cursor, types: &[Datatype]) -> Result<PullPage>;
    async fn push(&self, s: &Session, recs: &[EncRecord]) -> Result<Vec<PushResult>>;
    async fn authorize_blob_upload(&self, s: &Session, hash: &str, size: u64) -> Result<BlobUpload>;
    async fn confirm_blob_upload(&self, s: &Session, id: BlobId) -> Result<bool>;
    async fn blob_download_url(&self, s: &Session, hash: &str) -> Result<String>;
    async fn subscribe(&self, s: &Session) -> Result<SyncUpdateStream>;     // listenForSyncUpdates
    async fn register_device(&self, s: &Session, d: &DeviceInfo) -> Result<()>;
    async fn list_devices(&self, s: &Session) -> Result<Vec<DeviceInfo>>;
    async fn revoke_device(&self, s: &Session, id: &DeviceId) -> Result<()>;
}
```

Implementations:
- **`LykuProvider`** (default): HTTPS + MessagePack to lyku.org. `authenticate` runs the
  OIDC+`createApiKey` flow (§3); subsequent calls send `Authorization: Bearer lyk_…` with
  `Content-Type: application/x-msgpack`. `pull`/`push` hit `pullSyncRecords`/
  `pushSyncRecords`; blobs do `authorizeSyncBlobUpload` → presigned R2 PUT (raw HTTP, no
  MessagePack) → `confirmSyncBlobUpload`; `subscribe` opens the `listenForSyncUpdates` WS.
- **`SelfHostProvider`**: identical wire protocol against the user's own server (§7). One
  URL difference; auth can be a simpler token since OAuth/OIDC may be absent.
- **`LocalFolderProvider`**: writes `EncRecord`s to a directory (tests + "sync via my own
  Syncthing/Dropbox folder", zero server).

New Rust deps (behind a `sync` feature, per `sync.md` §0.3): an HTTP client (`reqwest`,
rustls) + MessagePack (`rmp-serde`/`rmpv`) + CRDT helpers (hand-rolled OR-Set + fractional
index) + OS keychain (`keyring`). Crypto crates are already transitively present.

**Sync algorithm** (single SQLite txn per cycle):
1. `pull(since = max_sequence_seen)` → decrypt each (per-record key, verify AAD) → merge
   per §6 into `swerve.db`/`vault.db`.
2. Collect locally-changed records → encrypt → `push`. Rejected (stale `baseSequence`)
   rows ⇒ re-pull those, re-merge, re-push (converges by CRDT/LWW).
3. Persist new cursor (`max sequence`) + per-record last-synced version vector.
4. Large assets: on a theme change, content-address (sha256 of ciphertext) →
   `authorize_blob_upload` (dedup: `alreadyExists` short-circuits) → PUT to R2 → confirm.
5. **Cadence**: debounced on-change (~2–5s), on foreground, interval (~5min), and on the
   `listenForSyncUpdates` nudge. Backoff on error.

**Offline**: all writes land in local SQLite with bumped version vectors; the push queue
drains when connectivity returns. Merge is order-independent (CRDT/LWW), so offline edits
on multiple devices converge.

---

## 6. Conflict resolution per datatype

Unchanged from `sync.md` §4 (per-datatype, weakest-correct mechanism); restated with the
Lyku `sequence` as the optimistic-concurrency anchor:

| Datatype | Strategy | Concurrency anchor |
| --- | --- | --- |
| Settings (scalars) | **LWW per key** (version-vector LWW; `updatedAt` then `recordId` tie-break) | reject on stale `baseSequence`, re-merge |
| Theme/mod | **LWW per mod**; assets immutable, content-addressed in R2 (dedup via `syncBlob` unique `(userId,contentHash)`) | per-mod `syncRecord` sequence |
| Bookmarks | **Add-wins OR-Set** (existence) + **fractional index** (order) + **LWW move** (re-parent) | per-node `syncRecord` |
| History | **Append-only grow-only set** keyed `(url, visitTime, deviceId)`; tombstone+TTL deletes; no merge | inserts only; deletes set `deleted` |
| Open tabs/session | **OR-Set keyed by tab uuid, partitioned by `deviceId`** (each device owns its set; others read-only) | per-tab `syncRecord` |
| Passwords/vault | **Field-level LWW inside per-record encrypted envelope** (`datatype:'vault'`) | per-entry `syncRecord`; AAD-bound origin+id+version |

Version vectors live in `syncRecord.versionVector` (server stores them but never
interprets). `sequence` is the server-side optimistic-concurrency / cursor mechanism;
**version vectors are the merge truth** (`sequence` only orders the wire stream).

---

## 7. Self-hostable server

Because Lyku stores only ciphertext + a `sequence` cursor, the self-host server is the same
five operations. It does **not** need lockstep-core/Bun/Kysely — it just needs to implement
the wire protocol swerve's `SyncProvider` speaks. Two ways to ship it:

1. **A small Rust binary** (`swerve-sync-server`): `axum`/`hyper` + `rusqlite` (or Postgres),
   `tokio`. Tables mirror §2 (`sync_records`, `sync_key_blobs`, `sync_blobs`,
   `sync_devices`, plus `accounts` + `sessions` since there's no Lyku to delegate auth to).
   Endpoints mirror the trait. `sequence` = a per-(account) counter or per-row
   auto-increment; `pull(since)` = `WHERE account=? AND sequence > ?`. Blobs = local
   filesystem or any S3. Single binary, single SQLite file, backups = copy the file (all
   ciphertext). Auth: a simple bearer token from a setup step (no need for OIDC).
2. **A Lyku-compatible mode**: anyone running their *own Lyku* gets sync for free once the
   §2 tables/routes exist — it's just their instance. This makes "self-host" = "run Lyku"
   for power users, and the standalone Rust binary the minimal option for everyone else.

What the self-host server MUST implement (the protocol contract):
- `POST /push`, `GET /pull?since=`, `GET/PUT /keys`, asset `authorize/confirm/download`,
  `GET /events` (WS/SSE nudge), `GET/POST/DELETE /devices`.
- Per-record optimistic concurrency on `baseSequence`; monotonic `sequence` for the cursor.
- **Zero-knowledge invariants** (enforced + tested): never receives passphrase or unwrapped
  key; `ciphertext` opaque; no content indexing; only structural metadata in plaintext.
- Quotas + rate limits; document metadata leakage.

The wire format can be MessagePack (to share the Lyku codec) or JSON (debuggable) behind a
content-type switch — the Rust client supports both; the Lyku provider uses MessagePack.

---

## 8. Open questions

1. **Auth path confirmation.** Confirm OAuth2 `access_token` truly cannot reach the
   MessagePack gateway (research says yes) — if so, the OIDC→`createApiKey` two-step (§3.1)
   is mandatory. Is there appetite to make the gateway also accept OAuth bearer tokens
   (then the two-step collapses)? Or should swerve just reuse the `sessionId` directly?
2. **API-key scopes.** OK to add `read:sync`/`write:sync` to the `createApiKey` enum, and to
   let new sync routes gate on them? Should a sync key be auto-minted at OIDC time or
   user-initiated?
3. **`bytea` first use.** `syncRecord.ciphertext` would be the first real `bytea` column in
   pg-models (currently only E2EE keys-as-base64-`text` precedent). Confirm the
   lockstep-core → SQL → Kysely path handles `bytea`/`Buffer` end-to-end in production, or
   follow the base64-in-`text` precedent (simpler, ~33% bloat, already proven). Recommend
   verifying `bytea` once; fall back to `text` base64 if the pipeline has gaps.
4. **`synced<T>` vs bare NATS for the nudge.** Use `registerSynced('syncRecord', …)` +
   `publishSynced` (idiomatic, reuses `streamSynced` which is **not yet wired into any
   service's `wsRoutes`** — only `stream-current-user` is live), or a bare
   `nats.publish('syncUpdates.${userId}', …)` + a hand-written `listenForSyncUpdates`
   (the `listenForTypingIndicators` shape, already-wired pattern)? The bare path is less
   code to land first; `synced<T>` is the better long-term home.
5. **Cursor scope.** Lyku's `sequence` is per-row, not per-account-monotonic. Is
   `(userId, sequence)` index-based delta pull acceptable, or do we want a per-account
   `accountSeq` for stricter exactly-once cursor semantics (esp. across deletes)?
6. **R2 bucket.** New `R2_SYNC_BUCKET` vs a `sync/` prefix in the existing `media` bucket?
   Separate bucket recommended (quota/lifecycle isolation, keep E2EE blobs out of the
   public media bucket). Quota: reuse the `zoidSaves` tier model (5GB free / 50GB plus)?
7. **Crypto root UX.** The sync passphrase is separate from Lyku login (Lyku is
   passwordless). Acceptable product-wise to ask users for a *second* secret purely for
   sync E2EE? (It's the price of zero-knowledge; the alternative — deriving from a
   non-existent account password — isn't available.) Mandatory recovery-key generation?
8. **History default.** Default-on (Chrome parity) or default-off (privacy-first
   positioning)? Most-sensitive non-secret datatype.
9. **Quota surfacing.** Lyku enforces quota with `507`; swerve needs to surface
   "over quota" / backpressure. Is there a `getSyncStorageStatus` (à la
   `getZoidStorageStatus`) we should add?
10. **Sequencing dependency.** Per `sync.md`, swerve has *no* local persistence yet
    (settings/bookmarks/history/passwords don't exist). The Lyku-side tables/routes can be
    built in parallel, but client sync is gated on building those local stores first. Build
    the Lyku surface now (cheap, idiomatic) or wait until swerve's local stores land?
```
