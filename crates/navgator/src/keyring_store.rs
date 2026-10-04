//! Wrapper around the OS keyring (Linux Secret Service, macOS Keychain, Windows Credential Manager)
//! through keyring-core, used to remember the sync passphrase for auto-unlock on launch.
//!
//! `store` and `fetch` are best-effort: with no secret-service (headless, no D-Bus, no
//! gnome-keyring/KWallet), `store` returns `false` and `fetch` returns `None`, and the caller
//! falls back to manual unlock. `forget` is not: the vault migration (LYK-616) must know the
//! passphrase is gone before it reports the old store retired, so it returns the error.
//!
//! keyring-core's stores find an entry by service and user, the attributes keyring 3 wrote, so a
//! passphrase remembered by an earlier build is found and deleted here.

use keyring_core::{CredentialStore, Entry, Error};
use std::sync::{Arc, OnceLock};

/// Keyring service name (groups our entries in the OS credential store).
const SERVICE: &str = "navgator";
/// Keyring account/user under which the single sync passphrase lives.
const USER: &str = "sync-passphrase";

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
fn platform_store() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(dbus_secret_service_keyring_store::Store::new()?)
}

#[cfg(target_os = "macos")]
fn platform_store() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(apple_native_keyring_store::keychain::Store::new()?)
}

#[cfg(target_os = "windows")]
fn platform_store() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(windows_native_keyring_store::Store::new()?)
}

/// The passphrase entry in the platform store, which is opened once per process.
fn entry() -> Result<Entry, String> {
    static STORE: OnceLock<Result<(), String>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            platform_store()
                .map(keyring_core::set_default_store)
                .map_err(|e| e.to_string())
        })
        .clone()?;
    Entry::new(SERVICE, USER).map_err(|e| e.to_string())
}

/// Best-effort store; `false` on any failure (no secret-service, headless, etc.).
pub fn store(passphrase: &str) -> bool {
    entry().is_ok_and(|e| e.set_password(passphrase).is_ok())
}

/// Best-effort fetch; `None` if absent OR unavailable (caller falls back to manual unlock).
pub fn fetch() -> Option<String> {
    entry().ok().and_then(|e| e.get_password().ok())
}

/// Delete the remembered passphrase. An absent entry is success; anything else, including an
/// unreachable keyring, is an error, because the caller is about to say the passphrase is gone.
#[allow(dead_code)] // the vault migration's caller, which waits on the core link (crate::migrate)
pub fn forget() -> Result<(), String> {
    forget_entry(&entry()?)
}

fn forget_entry(e: &Entry) -> Result<(), String> {
    match e.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(err) => Err(format!("could not remove the remembered passphrase: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring_core::api::CredentialStoreApi;
    use keyring_core::mock;

    // Built straight from its own mock store, so no test touches the process-wide default store
    // or the real OS keyring.
    fn mock_entry() -> Entry {
        mock::Store::new().unwrap().build(SERVICE, USER, None).unwrap()
    }

    #[test]
    fn forget_deletes_and_is_idempotent() {
        let e = mock_entry();
        e.set_password("old passphrase").unwrap();
        forget_entry(&e).unwrap();
        assert!(matches!(e.get_password(), Err(Error::NoEntry)));
        forget_entry(&e).unwrap();
    }

    #[test]
    fn forget_reports_a_keyring_failure() {
        let e = mock_entry();
        e.set_password("old passphrase").unwrap();
        let cred: &mock::Cred = e.as_any().downcast_ref().unwrap();
        cred.set_error(Error::NoStorageAccess("locked".into()));
        assert!(forget_entry(&e).is_err());
    }
}
