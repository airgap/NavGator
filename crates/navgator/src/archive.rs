//! Record/replay HTTP archive for deterministic rendering-regression fixtures.
//!
//! When `NAVGATOR_ARCHIVE_DIR` and `NAVGATOR_ARCHIVE_MODE` (`record`|`replay`) are both set,
//! NavGator's `load_web_resource` interceptor routes every http(s) load through this archive
//! instead of the live network:
//!   - **record**: fetch the resource (via `ureq`), store `(status, headers, body)` under a key
//!     derived from method+URL, and return it to the engine. Captures the document plus every
//!     loader-driven subresource (CSS, JS, `<img>`, `@font-face`, …).
//!   - **replay**: serve the stored response; a request not in the archive is a *miss* — logged to
//!     `misses.txt` and failed as a network error, so replay never touches the network.
//!
//! This makes a real page replayable byte-for-byte for regression tests, so a render can be
//! diffed against a frozen baseline without content drift (ads/articles/scroll) or a network.
//!
//! **Limitations (documented, v1):** JS-initiated `fetch()`/XHR requests do NOT reach
//! `load_web_resource` (Servo routes them through its own net stack), so they are neither captured
//! nor replayed. URLs with cache-busting query params (timestamps/random) also miss on replay.
//! Both surface in `misses.txt`. Cookies are not carried between capture requests.

use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Record,
    Replay,
}

/// A stored (or freshly captured) HTTP response — enough to rebuild a `WebResourceResponse`.
pub struct Stored {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    method: String,
    url: String,
    status: u16,
    headers: Vec<(String, String)>,
}

pub struct ResourceArchive {
    dir: PathBuf,
    mode: Mode,
    agent: ureq::Agent,
}

impl ResourceArchive {
    /// Construct from `NAVGATOR_ARCHIVE_DIR` + `NAVGATOR_ARCHIVE_MODE` (`record`|`replay`).
    /// Returns `None` (feature off → normal live loading) if either is unset/invalid.
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("NAVGATOR_ARCHIVE_DIR")?);
        let mode = match std::env::var("NAVGATOR_ARCHIVE_MODE").ok()?.as_str() {
            "record" => Mode::Record,
            "replay" => Mode::Replay,
            other => {
                eprintln!("[archive] ignoring NAVGATOR_ARCHIVE_MODE={other:?} (want record|replay)");
                return None;
            },
        };
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("[archive] cannot create {}: {e}", dir.display());
            return None;
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .redirects(8)
            .build();
        eprintln!(
            "[archive] {} at {}",
            if mode == Mode::Record { "RECORD" } else { "REPLAY" },
            dir.display()
        );
        Some(Self { dir, mode, agent })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Deterministic per-resource key. `DefaultHasher::new()` has a fixed initial state, so the
    /// same method+URL always maps to the same key across processes (unlike `RandomState`).
    fn key(method: &str, url: &str) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        method.hash(&mut h);
        0u8.hash(&mut h);
        url.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    fn meta_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn body_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.body"))
    }

    /// Replay: read a stored response for (method, url), or `None` if not archived.
    pub fn lookup(&self, method: &str, url: &str) -> Option<Stored> {
        let key = Self::key(method, url);
        let meta: Meta = serde_json::from_slice(&fs::read(self.meta_path(&key)).ok()?).ok()?;
        let body = fs::read(self.body_path(&key)).unwrap_or_default();
        Some(Stored {
            status: meta.status,
            headers: meta.headers,
            body,
        })
    }

    /// Record: return the already-archived response if present, else fetch it live, store it, and
    /// return it. `None` only on a transport failure (DNS/connect/timeout/read error).
    pub fn capture(&self, method: &str, url: &str, user_agent: Option<&str>) -> Option<Stored> {
        let key = Self::key(method, url);
        if self.meta_path(&key).exists() {
            return self.lookup(method, url);
        }
        // `accept-encoding: identity` discourages compression so the stored body is plain bytes;
        // the `gzip` feature still decompresses any gzip a server sends regardless. Forward the
        // engine's User-Agent so UA-sniffing servers return the same markup Servo would get.
        let mut req = self.agent.request(method, url).set("accept-encoding", "identity");
        if let Some(ua) = user_agent {
            req = req.set("user-agent", ua);
        }
        let resp = match req.call() {
            Ok(r) => r,
            // Capture error responses (404/500/…) too — they are part of the page's reality.
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => {
                eprintln!("[archive] record fetch failed {url}: {e}");
                return None;
            },
        };
        let status = resp.status();
        let mut headers = Vec::new();
        for name in resp.headers_names() {
            // Drop hop-by-hop / length / encoding headers: ureq hands us an identity body, so a
            // stale content-encoding/length would mislead the engine. Length is implicit in the
            // replayed body chunks.
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "content-encoding" | "content-length" | "transfer-encoding" | "connection"
            ) {
                continue;
            }
            if let Some(v) = resp.header(&name) {
                headers.push((name.clone(), v.to_string()));
            }
        }
        let mut body = Vec::new();
        if let Err(e) = resp.into_reader().read_to_end(&mut body) {
            eprintln!("[archive] record body read failed {url}: {e}");
            return None;
        }
        let meta = Meta {
            method: method.to_string(),
            url: url.to_string(),
            status,
            headers: headers.clone(),
        };
        if let Ok(json) = serde_json::to_vec(&meta) {
            let _ = fs::write(self.meta_path(&key), json);
            let _ = fs::write(self.body_path(&key), &body);
            self.append_line("index.txt", &format!("{key}\t{status}\t{}\t{method}\t{url}", body.len()));
        }
        Some(Stored {
            status,
            headers,
            body,
        })
    }

    /// Replay: record a request that wasn't in the archive (so the gap is visible).
    pub fn note_miss(&self, method: &str, url: &str) {
        eprintln!("[archive] REPLAY MISS {method} {url}");
        self.append_line("misses.txt", &format!("{method} {url}"));
    }

    fn append_line(&self, file: &str, line: &str) {
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(file))
        {
            let _ = writeln!(f, "{line}");
        }
    }
}
