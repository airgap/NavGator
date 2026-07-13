//! NavGator ↔ Lyku sync (early access).
//!
//! Pushes/pulls bookmarks + history to a Lyku platform (`api.lyku.org` consumer, `api.lyku.co`
//! business) over HTTPS+JSON. Auth is either a legacy `lyk_` API key or an OAuth access token
//! (`lyt_`) obtained by binding the profile to an account ([`crate::oauth`]); a profile syncs to
//! exactly one account/platform. Bookmark/history payloads are plaintext JSON; `passwords` are
//! E2EE (encrypted on the UI thread — only ciphertext reaches this module). The network runs on a
//! background thread, so `run_sync` takes a plain `Send` snapshot (no NavGator/egui types) and
//! returns a `SyncOutcome` the UI thread merges into the `Profile`. Conflicts resolve
//! last-write-wins by each item's `updated` (ms): items are pushed with their *stored* mtime, so
//! re-pushing an unchanged item is idempotent and never clobbers another device's newer edit.
//! When an OAuth access token is expired/near-expiry the sync thread refreshes it first and hands
//! the rotated tokens back in the outcome for the UI thread to persist. Deletes don't propagate
//! yet (no local tombstones) — an early-access limitation.

use crate::oauth;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct WireItem {
    collection: String,
    #[serde(rename = "itemId")]
    item_id: String,
    payload: String,
    deleted: bool,
    updated: i64,
}
#[derive(Serialize)]
struct PushReq {
    items: Vec<WireItem>,
}
#[derive(Deserialize)]
struct PushResp {
    accepted: i64,
}
#[derive(Serialize)]
struct PullReq {
    collections: Vec<String>,
    since: i64,
    limit: i64,
}
#[derive(Deserialize)]
struct WireItemIn {
    #[serde(rename = "itemId")]
    item_id: String,
    payload: String,
    deleted: bool,
    updated: i64,
}
#[derive(Deserialize)]
struct PullResp {
    items: Vec<WireItemIn>,
}

#[derive(Serialize, Deserialize)]
struct BookmarkPayload {
    url: String,
    title: String,
}
#[derive(Serialize, Deserialize)]
struct HistoryPayload {
    url: String,
    title: String,
    visits: u32,
}

/// How the sync thread authenticates to the platform.
pub enum SyncAuth {
    /// Legacy `lyk_` API key pasted into settings.conf.
    ApiKey(String),
    /// OAuth credentials from binding the profile to an account. The thread refreshes the access
    /// token if it's expired/near-expiry before syncing.
    OAuth {
        access_token: String,
        refresh_token: String,
        /// ms-epoch access-token deadline.
        expires_at: i64,
    },
}

/// OAuth tokens the sync thread rotated mid-run; the UI thread persists these into settings.conf.
#[derive(Debug)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

/// Local data + config handed to the background sync thread. Plain owned data only (`Send`).
pub struct SyncSnapshot {
    /// Platform key (`lyku.org` | `lyku.co`) — selects the API base URL and OAuth endpoints.
    pub platform: String,
    pub auth: SyncAuth,
    pub sync_bookmarks: bool,
    pub sync_history: bool,
    pub bookmarks: Vec<(String, String, i64)>, // (url, title, updated)
    pub history: Vec<(String, String, u32, i64)>, // (url, title, visits, updated)
    pub sync_passwords: bool,
    /// Pre-encrypted on the UI thread: (itemId, hex ciphertext, updated). The sync thread only
    /// moves opaque ciphertext — the passphrase + plaintext never leave the UI thread.
    pub passwords: Vec<(String, String, i64)>,
    pub cursor_bookmarks: i64,
    pub cursor_history: i64,
    pub cursor_passwords: i64,
}

#[derive(Debug)]
pub struct PulledBookmark {
    pub url: String,
    pub title: String,
    pub updated: i64,
    pub deleted: bool,
}
#[derive(Debug)]
pub struct PulledHistory {
    pub url: String,
    pub title: String,
    pub visits: u32,
    pub updated: i64,
    pub deleted: bool,
}

#[derive(Debug)]
pub struct PulledPassword {
    pub item_id: String,
    pub payload: String, // hex ciphertext — decrypted on the UI thread into the store
    pub updated: i64,
    pub deleted: bool,
}

/// Result of a sync, applied to the `Profile`/password store on the UI thread.
#[derive(Debug)]
pub struct SyncOutcome {
    pub ok: bool,
    pub message: String,
    pub pushed: usize,
    pub bookmarks: Vec<PulledBookmark>,
    pub history: Vec<PulledHistory>,
    pub passwords: Vec<PulledPassword>,
    pub cursor_bookmarks: i64,
    pub cursor_history: i64,
    pub cursor_passwords: i64,
    /// Present iff the sync thread refreshed OAuth tokens; the UI thread must persist them.
    pub refreshed: Option<RefreshedTokens>,
}

fn err_str(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("HTTP {code}: {}", body.chars().take(160).collect::<String>())
        }
        ureq::Error::Transport(t) => format!("network error: {t}"),
    }
}

fn push(base: &str, bearer: &str, items: Vec<WireItem>) -> Result<usize, String> {
    if items.is_empty() {
        return Ok(0);
    }
    let resp: PushResp = ureq::post(&format!("{base}/sync-push"))
        .set("Authorization", &format!("Bearer {bearer}"))
        .send_json(PushReq { items })
        .map_err(err_str)?
        .into_json()
        .map_err(|e| e.to_string())?;
    Ok(resp.accepted as usize)
}

fn pull(base: &str, bearer: &str, collection: &str, since: i64) -> Result<Vec<WireItemIn>, String> {
    let resp: PullResp = ureq::post(&format!("{base}/sync-pull"))
        .set("Authorization", &format!("Bearer {bearer}"))
        .send_json(PullReq {
            collections: vec![collection.to_string()],
            since,
            limit: 1000,
        })
        .map_err(err_str)?
        .into_json()
        .map_err(|e| e.to_string())?;
    Ok(resp.items)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run a full push+pull for the opted-in collections. Never panics; failures come back as
/// `ok: false` with a message. Runs on a background thread.
pub fn run_sync(snap: SyncSnapshot) -> SyncOutcome {
    let mut out = SyncOutcome {
        ok: true,
        message: String::new(),
        pushed: 0,
        bookmarks: Vec::new(),
        history: Vec::new(),
        passwords: Vec::new(),
        cursor_bookmarks: snap.cursor_bookmarks,
        cursor_history: snap.cursor_history,
        cursor_passwords: snap.cursor_passwords,
        refreshed: None,
    };

    let base = oauth::platform(&snap.platform).api_base.to_string();

    // Resolve the Bearer credential. OAuth access tokens are refreshed first when within 60s of
    // expiry; the rotated tokens ride back in `out.refreshed` so they persist even if a later
    // push/pull fails (an early return still carries them via `..out`).
    let bearer: String = match &snap.auth {
        SyncAuth::ApiKey(k) => {
            if k.trim().is_empty() {
                return SyncOutcome {
                    ok: false,
                    message: "No Lyku account connected (connect one in Settings).".into(),
                    ..out
                };
            }
            k.clone()
        }
        SyncAuth::OAuth {
            access_token,
            refresh_token,
            expires_at,
        } => {
            if now_ms() >= *expires_at - 60_000 {
                match oauth::refresh(&snap.platform, refresh_token) {
                    Ok(t) => {
                        let bearer = t.access_token.clone();
                        out.refreshed = Some(RefreshedTokens {
                            access_token: t.access_token,
                            refresh_token: t.refresh_token,
                            expires_at: t.expires_at,
                        });
                        bearer
                    }
                    Err(e) => {
                        return SyncOutcome {
                            ok: false,
                            message: format!("Session expired — reconnect your account. ({e})"),
                            ..out
                        };
                    }
                }
            } else {
                access_token.clone()
            }
        }
    };

    if snap.sync_bookmarks {
        let items = snap
            .bookmarks
            .iter()
            .map(|(url, title, updated)| WireItem {
                collection: "bookmarks".into(),
                item_id: url.clone(),
                payload: serde_json::to_string(&BookmarkPayload {
                    url: url.clone(),
                    title: title.clone(),
                })
                .unwrap_or_default(),
                deleted: false,
                updated: *updated,
            })
            .collect();
        match push(&base, &bearer, items) {
            Ok(n) => out.pushed += n,
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("push bookmarks: {e}"),
                    ..out
                };
            }
        }
        match pull(&base, &bearer, "bookmarks", snap.cursor_bookmarks) {
            Ok(items) => {
                for it in items {
                    if it.updated > out.cursor_bookmarks {
                        out.cursor_bookmarks = it.updated;
                    }
                    if let Ok(p) = serde_json::from_str::<BookmarkPayload>(&it.payload) {
                        out.bookmarks.push(PulledBookmark {
                            url: p.url,
                            title: p.title,
                            updated: it.updated,
                            deleted: it.deleted,
                        });
                    } else if it.deleted {
                        out.bookmarks.push(PulledBookmark {
                            url: it.item_id,
                            title: String::new(),
                            updated: it.updated,
                            deleted: true,
                        });
                    }
                }
            }
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("pull bookmarks: {e}"),
                    ..out
                };
            }
        }
    }

    if snap.sync_history {
        let items = snap
            .history
            .iter()
            .map(|(url, title, visits, updated)| WireItem {
                collection: "history".into(),
                item_id: url.clone(),
                payload: serde_json::to_string(&HistoryPayload {
                    url: url.clone(),
                    title: title.clone(),
                    visits: *visits,
                })
                .unwrap_or_default(),
                deleted: false,
                updated: *updated,
            })
            .collect();
        match push(&base, &bearer, items) {
            Ok(n) => out.pushed += n,
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("push history: {e}"),
                    ..out
                };
            }
        }
        match pull(&base, &bearer, "history", snap.cursor_history) {
            Ok(items) => {
                for it in items {
                    if it.updated > out.cursor_history {
                        out.cursor_history = it.updated;
                    }
                    if let Ok(p) = serde_json::from_str::<HistoryPayload>(&it.payload) {
                        out.history.push(PulledHistory {
                            url: p.url,
                            title: p.title,
                            visits: p.visits,
                            updated: it.updated,
                            deleted: it.deleted,
                        });
                    }
                }
            }
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("pull history: {e}"),
                    ..out
                };
            }
        }
    }

    if snap.sync_passwords {
        let items = snap
            .passwords
            .iter()
            .map(|(id, payload, updated)| WireItem {
                collection: "passwords".into(),
                item_id: id.clone(),
                payload: payload.clone(),
                deleted: false,
                updated: *updated,
            })
            .collect();
        match push(&base, &bearer, items) {
            Ok(n) => out.pushed += n,
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("push passwords: {e}"),
                    ..out
                };
            }
        }
        match pull(&base, &bearer, "passwords", snap.cursor_passwords) {
            Ok(items) => {
                for it in items {
                    if it.updated > out.cursor_passwords {
                        out.cursor_passwords = it.updated;
                    }
                    out.passwords.push(PulledPassword {
                        item_id: it.item_id,
                        payload: it.payload,
                        updated: it.updated,
                        deleted: it.deleted,
                    });
                }
            }
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("pull passwords: {e}"),
                    ..out
                };
            }
        }
    }

    out.message = format!(
        "Synced — pushed {}, pulled {} bookmark(s), {} history, {} password(s).",
        out.pushed,
        out.bookmarks.len(),
        out.history.len(),
        out.passwords.len(),
    );
    out
}
