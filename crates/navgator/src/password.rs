//! E2EE password store (security-critical core).
//!
//! Credentials are encrypted — at rest on disk AND for Lyku sync — with XChaCha20-Poly1305
//! under a 256-bit key derived from the user's **sync passphrase** via Argon2id. Neither the
//! disk file nor the Lyku server ever sees plaintext (zero-knowledge): the passphrase is never
//! stored and never leaves the device. While the store is *unlocked*, the passphrase and the
//! decrypted credentials live in memory (required to autofill); `lock()` clears them.
//!
//! Blob format: `[salt(16)][nonce(24)][ciphertext+tag]`. The salt is per-store (stable);
//! every encryption uses a fresh random nonce. Wrong passphrase or any tampering fails the
//! Poly1305 tag → `open()` returns Err.
#![allow(dead_code)] // a few helpers (remove/is_empty/exists) await the gator://passwords manager

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;

/// One saved login. `origin` is the site (scheme://host[:port]); `updated` (ms) drives sync LWW.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub origin: String,
    pub username: String,
    pub password: String,
    pub updated: i64,
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| format!("key derivation failed: {e}"))?;
    Ok(key)
}

/// 16 random bytes for a new store's salt (reuses the cipher's CSPRNG nonce generator).
pub fn random_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    salt
}

/// Encrypt `plaintext` → `[salt][nonce][ciphertext]`.
pub fn seal(passphrase: &str, salt: &[u8; SALT_LEN], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = derive_key(passphrase, salt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a `[salt][nonce][ciphertext]` blob. Err on short/corrupt input or wrong passphrase.
pub fn open(passphrase: &str, blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < SALT_LEN + NONCE_LEN {
        return Err("store too short / corrupted".into());
    }
    let salt = &blob[..SALT_LEN];
    let nonce = &blob[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ct = &blob[SALT_LEN + NONCE_LEN..];
    let key = derive_key(passphrase, salt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    cipher
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| "wrong passphrase or corrupted store".to_string())
}

/// The on-disk, passphrase-locked credential store.
pub struct PasswordStore {
    path: PathBuf,
    creds: Vec<Credential>,
    /// Held only while unlocked (needed to re-encrypt on save + autofill).
    passphrase: Option<String>,
    salt: [u8; SALT_LEN],
    /// Whether a store file already exists on disk.
    pub exists: bool,
}

impl PasswordStore {
    pub fn load(path: PathBuf) -> Self {
        let exists = path.exists();
        PasswordStore {
            path,
            creds: Vec::new(),
            passphrase: None,
            salt: random_salt(),
            exists,
        }
    }

    pub fn is_unlocked(&self) -> bool {
        self.passphrase.is_some()
    }

    /// The live passphrase while unlocked (`None` if locked). In-crate accessor used solely to
    /// hand the plaintext to the OS keyring when "Remember passphrase" is toggled on while already
    /// unlocked. The plaintext never leaves the process except into the OS keyring.
    pub fn passphrase(&self) -> Option<&str> {
        self.passphrase.as_deref()
    }

    pub fn len(&self) -> usize {
        self.creds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.creds.is_empty()
    }

    /// Unlock with the passphrase. For an existing store this decrypts it (a wrong passphrase
    /// returns Err and leaves the store locked). A new store unlocks empty with a fresh salt.
    pub fn unlock(&mut self, passphrase: &str) -> Result<(), String> {
        if let Ok(blob) = std::fs::read(&self.path) {
            if blob.len() >= SALT_LEN + NONCE_LEN {
                let plaintext = open(passphrase, &blob)?;
                let creds: Vec<Credential> =
                    serde_json::from_slice(&plaintext).map_err(|e| format!("decode: {e}"))?;
                self.salt.copy_from_slice(&blob[..SALT_LEN]);
                self.creds = creds;
            }
        }
        self.passphrase = Some(passphrase.to_string());
        Ok(())
    }

    /// Drop the passphrase + decrypted credentials from memory.
    pub fn lock(&mut self) {
        self.creds.clear();
        self.passphrase = None;
    }

    /// Re-encrypt + persist. Requires the store to be unlocked.
    pub fn save(&self) -> Result<(), String> {
        let pass = self.passphrase.as_deref().ok_or("store is locked")?;
        let plaintext = serde_json::to_vec(&self.creds).map_err(|e| e.to_string())?;
        let blob = seal(pass, &self.salt, &plaintext)?;
        if let Some(d) = self.path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::fs::write(&self.path, blob).map_err(|e| e.to_string())
    }

    /// Add or update a credential (keyed by origin + username).
    pub fn upsert(&mut self, cred: Credential) {
        if let Some(c) = self
            .creds
            .iter_mut()
            .find(|c| c.origin == cred.origin && c.username == cred.username)
        {
            c.password = cred.password;
            c.updated = cred.updated;
        } else {
            self.creds.push(cred);
        }
    }

    pub fn remove(&mut self, origin: &str, username: &str) {
        self.creds
            .retain(|c| !(c.origin == origin && c.username == username));
    }

    /// Credentials saved for an origin (for autofill).
    pub fn for_origin(&self, origin: &str) -> Vec<&Credential> {
        self.creds.iter().filter(|c| c.origin == origin).collect()
    }

    pub fn all(&self) -> &[Credential] {
        &self.creds
    }

    /// Encrypt one credential for sync (the opaque ciphertext blob). None if locked.
    pub fn encrypt_credential(&self, c: &Credential) -> Option<Vec<u8>> {
        let pass = self.passphrase.as_deref()?;
        let json = serde_json::to_vec(c).ok()?;
        seal(pass, &self.salt, &json).ok()
    }

    /// Decrypt a synced credential blob (the salt comes from the blob, so a credential sealed
    /// on another device decrypts here under the same passphrase). None if locked / bad blob.
    pub fn decrypt_credential(&self, blob: &[u8]) -> Option<Credential> {
        let pass = self.passphrase.as_deref()?;
        let pt = open(pass, blob).ok()?;
        serde_json::from_slice(&pt).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_wrong_passphrase() {
        let salt = random_salt();
        let secret = b"correct-horse-battery-staple credentials blob";
        let blob = seal("my sync passphrase", &salt, secret).unwrap();
        // round-trips with the right passphrase
        assert_eq!(open("my sync passphrase", &blob).unwrap(), secret);
        // fails with the wrong passphrase
        assert!(open("not the passphrase", &blob).is_err());
        // the blob is opaque (no plaintext leak)
        assert!(!blob.windows(11).any(|w| w == b"credentials"));
    }

    #[test]
    fn store_persist_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ng-pwtest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("passwords.enc");
        let _ = std::fs::remove_file(&path);

        // create a new store, unlock, add a credential, persist
        let mut s = PasswordStore::load(path.clone());
        s.unlock("testpass").unwrap();
        s.upsert(Credential {
            origin: "https://example.com".into(),
            username: "alice".into(),
            password: "hunter2".into(),
            updated: 1,
        });
        s.save().unwrap();

        // a fresh store unlocks with the right passphrase and sees the credential
        let mut s2 = PasswordStore::load(path.clone());
        assert!(s2.exists);
        s2.unlock("testpass").unwrap();
        let creds = s2.for_origin("https://example.com");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].username, "alice");
        assert_eq!(creds[0].password, "hunter2");

        // the wrong passphrase fails to unlock (Poly1305 tag)
        let mut s3 = PasswordStore::load(path.clone());
        assert!(s3.unlock("wrongpass").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credential_sync_crosses_devices() {
        let tmp = std::env::temp_dir();
        // device A encrypts a credential for sync
        let mut a = PasswordStore::load(tmp.join(format!("ng-a-{}", std::process::id())));
        a.unlock("shared passphrase").unwrap();
        let c = Credential {
            origin: "https://x.com".into(),
            username: "u".into(),
            password: "p".into(),
            updated: 5,
        };
        let blob = a.encrypt_credential(&c).unwrap();
        // device B (a different store with a different salt, same passphrase) decrypts it —
        // because the salt travels in the blob, not the store.
        let mut b = PasswordStore::load(tmp.join(format!("ng-b-{}", std::process::id())));
        b.unlock("shared passphrase").unwrap();
        let got = b.decrypt_credential(&blob).unwrap();
        assert_eq!(got.origin, "https://x.com");
        assert_eq!(got.password, "p");
        // a wrong passphrase cannot decrypt it
        let mut c2 = PasswordStore::load(tmp.join(format!("ng-c-{}", std::process::id())));
        c2.unlock("wrong passphrase").unwrap();
        assert!(c2.decrypt_credential(&blob).is_none());
    }

    #[test]
    fn fresh_nonce_each_time() {
        let salt = random_salt();
        let a = seal("p", &salt, b"same plaintext").unwrap();
        let b = seal("p", &salt, b"same plaintext").unwrap();
        assert_ne!(a, b, "ciphertext must differ (fresh nonce)");
    }
}
