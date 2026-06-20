//! Panic-free wrapper around the OS keyring (Linux Secret Service / KWallet) used to remember
//! the sync passphrase for auto-unlock on launch.
//!
//! Every function returns owned data / `bool` / `()` — `keyring::Error` never escapes this
//! module, so the rest of the crate never names the keyring error type and the dependency stays
//! contained. All calls degrade gracefully: if there is no secret-service (headless, no D-Bus,
//! no gnome-keyring/KWallet), `store` returns `false` and `fetch` returns `None`, and the caller
//! falls back to manual unlock. No call site can panic.

/// Keyring service name (groups our entries in the OS credential store).
const SERVICE: &str = "navgator";
/// Keyring account/user under which the single sync passphrase lives.
const USER: &str = "sync-passphrase";

/// Build an `Entry` handle, swallowing construction errors (e.g. no platform store).
fn entry() -> Option<keyring::Entry> {
    keyring::Entry::new(SERVICE, USER).ok()
}

/// Best-effort store; `false` on any failure (no secret-service, headless, etc.).
pub fn store(passphrase: &str) -> bool {
    entry()
        .map(|e| e.set_password(passphrase).is_ok())
        .unwrap_or(false)
}

/// Best-effort fetch; `None` if absent OR unavailable (caller falls back to manual unlock).
pub fn fetch() -> Option<String> {
    entry().and_then(|e| e.get_password().ok())
}

/// Best-effort delete; `NoEntry` is treated as success (idempotent). Any other error is ignored.
pub fn clear() {
    if let Some(e) = entry() {
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(_) => {}
        }
    }
}
