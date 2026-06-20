//! NavGator ↔ Lyku sync (early access).
//!
//! Pushes/pulls bookmarks + history to `api.lyku.org` over HTTPS+JSON, authenticated with a
//! Lyku API key (`lyk_` bearer). Bookmark/history payloads are plaintext JSON; the `passwords`
//! collection (E2EE) lands once a password store exists. The network runs on a background
//! thread, so `run_sync` takes a plain `Send` snapshot (no NavGator/egui types) and returns a
//! `SyncOutcome` the UI thread merges into the `Profile`. Conflicts resolve last-write-wins by
//! each item's `updated` (ms): items are pushed with their *stored* mtime, so re-pushing an
//! unchanged item is idempotent and never clobbers another device's newer edit. Deletes don't
//! propagate yet (no local tombstones) — an early-access limitation.

use serde::{Deserialize, Serialize};

const API: &str = "https://api.lyku.org";

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

/// Local data + config handed to the background sync thread. Plain owned data only (`Send`).
pub struct SyncSnapshot {
    pub api_key: String,
    pub sync_bookmarks: bool,
    pub sync_history: bool,
    pub bookmarks: Vec<(String, String, i64)>, // (url, title, updated)
    pub history: Vec<(String, String, u32, i64)>, // (url, title, visits, updated)
    pub cursor_bookmarks: i64,
    pub cursor_history: i64,
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

/// Result of a sync, applied to the `Profile` on the UI thread.
#[derive(Debug)]
pub struct SyncOutcome {
    pub ok: bool,
    pub message: String,
    pub pushed: usize,
    pub bookmarks: Vec<PulledBookmark>,
    pub history: Vec<PulledHistory>,
    pub cursor_bookmarks: i64,
    pub cursor_history: i64,
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

fn push(api_key: &str, items: Vec<WireItem>) -> Result<usize, String> {
    if items.is_empty() {
        return Ok(0);
    }
    let resp: PushResp = ureq::post(&format!("{API}/sync-push"))
        .set("Authorization", &format!("Bearer {api_key}"))
        .send_json(PushReq { items })
        .map_err(err_str)?
        .into_json()
        .map_err(|e| e.to_string())?;
    Ok(resp.accepted as usize)
}

fn pull(api_key: &str, collection: &str, since: i64) -> Result<Vec<WireItemIn>, String> {
    let resp: PullResp = ureq::post(&format!("{API}/sync-pull"))
        .set("Authorization", &format!("Bearer {api_key}"))
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

/// Run a full push+pull for the opted-in collections. Never panics; failures come back as
/// `ok: false` with a message. Runs on a background thread.
pub fn run_sync(snap: SyncSnapshot) -> SyncOutcome {
    let mut out = SyncOutcome {
        ok: true,
        message: String::new(),
        pushed: 0,
        bookmarks: Vec::new(),
        history: Vec::new(),
        cursor_bookmarks: snap.cursor_bookmarks,
        cursor_history: snap.cursor_history,
    };
    if snap.api_key.trim().is_empty() {
        return SyncOutcome {
            ok: false,
            message: "No Lyku API key set (paste one in Settings).".into(),
            ..out
        };
    }

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
        match push(&snap.api_key, items) {
            Ok(n) => out.pushed += n,
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("push bookmarks: {e}"),
                    ..out
                };
            }
        }
        match pull(&snap.api_key, "bookmarks", snap.cursor_bookmarks) {
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
        match push(&snap.api_key, items) {
            Ok(n) => out.pushed += n,
            Err(e) => {
                return SyncOutcome {
                    ok: false,
                    message: format!("push history: {e}"),
                    ..out
                };
            }
        }
        match pull(&snap.api_key, "history", snap.cursor_history) {
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

    out.message = format!(
        "Synced — pushed {}, pulled {} bookmark(s) + {} history entr{}.",
        out.pushed,
        out.bookmarks.len(),
        out.history.len(),
        if out.history.len() == 1 { "y" } else { "ies" }
    );
    out
}
