//! OAuth 2.0 (Authorization Code + PKCE) for binding a NavGator profile to a Lyku account.
//!
//! NavGator is a browser, so the loopback-redirect native-app flow (RFC 8252) is the natural fit:
//! we bind an ephemeral `127.0.0.1` port, open the platform's authorize page in a tab, and capture
//! the redirect on that loopback socket. Access/refresh tokens are opaque (validated server-side);
//! the `lyt_` access token authenticates the sync routes as a `Bearer`. A profile binds to exactly
//! one account, and different profiles can bind to different accounts across platforms.
//!
//! Everything here is plain `Send` data — no egui/NavGator types — so `begin`/`complete`/`refresh`
//! run on background threads. Nothing panics; failures come back as `Err(String)`.
//!
//! Platforms: `lyku.org` (consumer) is live; `lyku.co` (business) is wired but `available: false`
//! until its sync backend ships (LYK-1510) — the client is already platform-aware so it slots in.

use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// A Lyku sync platform: consumer (`lyku.org`) or business (`lyku.co`).
pub struct Platform {
    pub key: &'static str,
    pub label: &'static str,
    /// API origin: OAuth + sync endpoints hang off this (`/oauth-authorize`, `/sync-push`, …).
    pub api_base: &'static str,
    pub client_id: &'static str,
    /// False until the platform's sync backend exists (lyku.co: LYK-1510).
    pub available: bool,
}

pub const PLATFORMS: &[Platform] = &[
    Platform {
        key: "lyku.org",
        label: "Lyku",
        api_base: "https://api.lyku.org",
        client_id: "navgator",
        available: true,
    },
    Platform {
        key: "lyku.co",
        label: "Lyku Business",
        api_base: "https://api.lyku.co",
        client_id: "navgator",
        available: false,
    },
];

/// Resolve a platform by key, defaulting to the consumer platform for unknown/empty keys.
pub fn platform(key: &str) -> &'static Platform {
    PLATFORMS.iter().find(|p| p.key == key).unwrap_or(&PLATFORMS[0])
}

fn authorize_endpoint(p: &Platform) -> String {
    format!("{}/oauth-authorize", p.api_base)
}
fn token_endpoint(p: &Platform) -> String {
    format!("{}/oauth-token", p.api_base)
}
fn userinfo_endpoint(p: &Platform) -> String {
    format!("{}/oauth-userinfo", p.api_base)
}

/// `openid`+`profile` identify which account a profile is bound to; `sync` grants the sync routes.
const SCOPE: &str = "openid profile sync";

/// OAuth tokens for a bound account. `expires_at` is the ms-epoch access-token deadline.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

/// A completed connect: which platform, the tokens, and who was bound.
#[derive(Debug)]
pub struct ConnectResult {
    pub platform: String,
    pub tokens: Tokens,
    pub account_id: String,
    pub account_name: String,
}

/// A begun authorization: the loopback listener + PKCE verifier, awaiting the redirect.
pub struct Pending {
    listener: TcpListener,
    verifier: String,
    state: String,
    redirect_uri: String,
    platform_key: &'static str,
}

// ── small self-contained encoders (avoid pulling in base64/hex crates) ──────────────

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// URL-safe base64 without padding (RFC 4648 §5) — the encoding PKCE S256 requires.
fn base64url_nopad(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        }
    }
    out
}

/// N random bytes as a lowercase hex string (unreserved per RFC 3986 — safe unencoded in a URL).
fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    OsRng.fill_bytes(&mut b);
    hex(&b)
}

/// Percent-encode a query-parameter value, leaving only RFC 3986 unreserved characters.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decode application/x-www-form-urlencoded (percent-escapes + `+` → space) from a redirect query.
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn err_str(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("HTTP {code}: {}", body.chars().take(200).collect::<String>())
        }
        ureq::Error::Transport(t) => format!("network error: {t}"),
    }
}

// ── flow ────────────────────────────────────────────────────────────────────────────

/// Phase 1 (UI thread): bind a loopback port and build the authorize URL. The caller opens the
/// returned URL in a tab, then hands `Pending` to [`complete`] on a background thread.
pub fn begin(platform_key: &str) -> Result<(String, Pending), String> {
    let p = platform(platform_key);
    if !p.available {
        return Err(format!("{} sync isn't available yet.", p.label));
    }
    // Ephemeral loopback port (RFC 8252 §7.3): the server matches the redirect ignoring the port.
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("could not open loopback port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let verifier = rand_hex(32); // 64 hex chars — within PKCE's 43..128
    let challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
    let state = rand_hex(16);

    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        authorize_endpoint(p),
        q(p.client_id),
        q(&redirect_uri),
        q(SCOPE),
        q(&state),
        q(&challenge),
    );
    Ok((
        url,
        Pending {
            listener,
            verifier,
            state,
            redirect_uri,
            platform_key: p.key,
        },
    ))
}

/// Phase 2 (background thread): block for the loopback redirect, exchange the code for tokens, and
/// fetch userinfo for the display name. Times out after 5 minutes if the user never authorizes.
pub fn complete(p: Pending) -> Result<ConnectResult, String> {
    let code = accept_code(&p.listener, &p.state)?;
    let plat = platform(p.platform_key);
    let tokens = exchange_code(plat, &code, &p.redirect_uri, &p.verifier)?;
    let (account_id, account_name) =
        fetch_userinfo(plat, &tokens.access_token).unwrap_or_default();
    Ok(ConnectResult {
        platform: plat.key.to_string(),
        tokens,
        account_id,
        account_name,
    })
}

/// Wait (up to 5 min) for the browser to hit `http://127.0.0.1:<port>/callback?code=…&state=…`,
/// verify the state, ACK the browser with a friendly page, and return the code. Non-callback hits
/// (favicon, etc.) get a 404 and are ignored.
fn accept_code(listener: &TcpListener, expected_state: &str) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() >= deadline {
            return Err("Timed out waiting for authorization.".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut line = String::new();
                {
                    let cloned = match stream.try_clone() {
                        Ok(c) => c,
                        Err(e) => return Err(e.to_string()),
                    };
                    let mut reader = BufReader::new(cloned);
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                }
                // Request line: `GET /callback?code=…&state=… HTTP/1.1`
                let path = line.split_whitespace().nth(1).unwrap_or("");
                let (code, state, error) = parse_callback(path);

                if code.is_none() && error.is_none() {
                    // Not the redirect (e.g. /favicon.ico) — brush it off, keep waiting.
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }

                let ok = error.is_none();
                let (title, msg) = if ok {
                    (
                        "Connected",
                        "NavGator is now linked to your Lyku account. You can close this tab.",
                    )
                } else {
                    (
                        "Authorization failed",
                        "You can close this tab and try again from NavGator.",
                    )
                };
                let html = format!(
                    "<!doctype html><meta charset=utf-8><title>{title}</title>\
                     <body style=\"font-family:system-ui,sans-serif;background:#0b0d12;color:#e6e9ef;\
                     display:grid;place-items:center;height:100vh;margin:0\">\
                     <div style=\"text-align:center;max-width:22rem\">\
                     <h1 style=\"font-weight:600;font-size:1.4rem\">{title}</h1>\
                     <p style=\"opacity:.8;line-height:1.5\">{msg}</p></div>"
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();

                if let Some(e) = error {
                    return Err(format!("authorization denied: {e}"));
                }
                if state.as_deref() != Some(expected_state) {
                    return Err("state mismatch (possible CSRF) — aborted.".into());
                }
                if let Some(c) = code {
                    return Ok(c);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(e) => return Err(format!("loopback accept: {e}")),
        }
    }
}

/// Pull `code`, `state`, `error` out of a redirect path's query string.
fn parse_callback(path: &str) -> (Option<String>, Option<String>, Option<String>) {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let (mut code, mut state, mut error) = (None, None, None);
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let val = urldecode(v);
            match k {
                "code" => code = Some(val),
                "state" => state = Some(val),
                "error" => error = Some(val),
                _ => {}
            }
        }
    }
    (code, state, error)
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn tokens_from(resp: TokenResp, prev_refresh: &str) -> Tokens {
    let expires_at = now_ms() + resp.expires_in.unwrap_or(3600) * 1000;
    Tokens {
        access_token: resp.access_token,
        // The token endpoint returns the same refresh token; fall back to the prior one if omitted.
        refresh_token: resp
            .refresh_token
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| prev_refresh.to_string()),
        expires_at,
    }
}

fn exchange_code(
    p: &Platform,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<Tokens, String> {
    let resp: TokenResp = ureq::post(&token_endpoint(p))
        .send_form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", p.client_id),
            ("code_verifier", verifier),
        ])
        .map_err(err_str)?
        .into_json()
        .map_err(|e| e.to_string())?;
    Ok(tokens_from(resp, ""))
}

/// Exchange a refresh token for a fresh access token. Used by the sync thread when the access
/// token is expired/near-expiry. Returns the (possibly rotated) token set to persist.
pub fn refresh(platform_key: &str, refresh_token: &str) -> Result<Tokens, String> {
    let p = platform(platform_key);
    let resp: TokenResp = ureq::post(&token_endpoint(p))
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", p.client_id),
        ])
        .map_err(err_str)?
        .into_json()
        .map_err(|e| e.to_string())?;
    Ok(tokens_from(resp, refresh_token))
}

#[derive(Deserialize)]
struct UserInfo {
    sub: String,
    preferred_username: Option<String>,
    name: Option<String>,
}

/// Best-effort identity lookup for display ("Connected as …"). Never fatal to a connect.
fn fetch_userinfo(p: &Platform, access_token: &str) -> Option<(String, String)> {
    let info: UserInfo = ureq::get(&userinfo_endpoint(p))
        .set("Authorization", &format!("Bearer {access_token}"))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let name = info
        .preferred_username
        .or(info.name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| info.sub.clone());
    Some((info.sub, name))
}
