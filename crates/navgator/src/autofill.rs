//! Form-autofill profile (addresses + payment cards), LYK-1371.
//!
//! Stored in its own passphrase-locked file (`autofill.enc`) using the SAME crypto as the
//! password vault ([`crate::password::seal`]/[`open`], Argon2id + XChaCha20-Poly1305) and unlocked
//! with the SAME passphrase — so one unlock opens both. Kept as a separate file rather than
//! extending [`crate::password::PasswordStore`] so the existing `Vec<Credential>` store format
//! isn't broken.
//!
//! A single profile for v1 (one address + one card). **The card CVC is never stored** (PCI/DSS +
//! crypto risk) — the user types it each time. Expiry year is stored 4-digit.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::password::{open, random_salt, seal};

/// One saved identity: contact + postal address + a payment card (no CVC). All fields optional;
/// blank fields are skipped both in the manager and when filling a page.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct AutofillProfile {
    pub full_name: String,
    pub email: String,
    pub phone: String,
    pub organization: String,
    pub address1: String,
    pub address2: String,
    pub city: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
    pub cc_name: String,
    pub cc_number: String,
    pub cc_exp_month: String,
    pub cc_exp_year: String,
    pub updated: i64,
}

impl AutofillProfile {
    /// True when every field is blank (nothing to fill / nothing to show).
    pub fn is_blank(&self) -> bool {
        [
            &self.full_name,
            &self.email,
            &self.phone,
            &self.organization,
            &self.address1,
            &self.address2,
            &self.city,
            &self.region,
            &self.postal_code,
            &self.country,
            &self.cc_name,
            &self.cc_number,
            &self.cc_exp_month,
            &self.cc_exp_year,
        ]
        .iter()
        .all(|s| s.is_empty())
    }
}

/// The on-disk, passphrase-locked autofill profile.
pub struct AutofillStore {
    path: PathBuf,
    profile: AutofillProfile,
    unlocked: bool,
}

impl AutofillStore {
    pub fn load(path: PathBuf) -> Self {
        AutofillStore {
            path,
            profile: AutofillProfile::default(),
            unlocked: false,
        }
    }

    pub fn is_unlocked(&self) -> bool {
        self.unlocked
    }

    pub fn profile(&self) -> &AutofillProfile {
        &self.profile
    }

    /// Replace the profile in memory (persist with [`save`]).
    pub fn set_profile(&mut self, profile: AutofillProfile) {
        self.profile = profile;
    }

    /// Unlock with the vault passphrase: decrypt the profile if a file exists. A wrong passphrase
    /// returns Err and leaves the store locked; a missing file unlocks an empty profile.
    pub fn unlock(&mut self, passphrase: &str) -> Result<(), String> {
        if let Ok(blob) = std::fs::read(&self.path) {
            if !blob.is_empty() {
                let plaintext = open(passphrase, &blob)?;
                self.profile = serde_json::from_slice(&plaintext).unwrap_or_default();
            }
        }
        self.unlocked = true;
        Ok(())
    }

    /// Drop the decrypted profile from memory.
    pub fn lock(&mut self) {
        self.profile = AutofillProfile::default();
        self.unlocked = false;
    }

    /// Re-encrypt + persist. `passphrase` is the vault passphrase (held by the password store while
    /// unlocked). A fresh salt is generated each write; `seal` frames `[salt][nonce][ct]`.
    pub fn save(&self, passphrase: &str) -> Result<(), String> {
        let plaintext = serde_json::to_vec(&self.profile).map_err(|e| e.to_string())?;
        let blob = seal(passphrase, &random_salt(), &plaintext)?;
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&self.path, blob).map_err(|e| e.to_string())
    }
}
