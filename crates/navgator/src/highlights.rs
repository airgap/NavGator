//! Encrypted text highlights (LYK-1281). Per-origin highlights are stored in a passphrase-locked
//! file (`highlights.enc`) using the SAME crypto as the password/autofill vault
//! ([`crate::password::seal`]/[`open`]) and unlocked with the SAME passphrase — one unlock opens
//! all three. Each highlight is a text-quote anchor (the exact text + a little prefix/suffix
//! context) so it can be re-found and re-drawn on the next visit.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::password::{open, random_salt, seal};

/// One saved highlight: a text-quote anchor plus its color.
#[derive(Clone, Serialize, Deserialize)]
pub struct Highlight {
    pub text: String,
    pub prefix: String,
    pub suffix: String,
    pub color: String,
}

/// The on-disk, passphrase-locked highlight collection, keyed by origin.
pub struct HighlightStore {
    path: PathBuf,
    by_origin: HashMap<String, Vec<Highlight>>,
    unlocked: bool,
}

impl HighlightStore {
    pub fn load(path: PathBuf) -> Self {
        HighlightStore {
            path,
            by_origin: HashMap::new(),
            unlocked: false,
        }
    }

    pub fn is_unlocked(&self) -> bool {
        self.unlocked
    }

    /// Highlights saved for `origin` (empty slice if none / locked).
    pub fn for_origin(&self, origin: &str) -> &[Highlight] {
        self.by_origin.get(origin).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Add a highlight to `origin` (persist with [`save`]).
    pub fn add(&mut self, origin: &str, hl: Highlight) {
        self.by_origin.entry(origin.to_string()).or_default().push(hl);
    }

    pub fn unlock(&mut self, passphrase: &str) -> Result<(), String> {
        if let Ok(blob) = std::fs::read(&self.path) {
            if !blob.is_empty() {
                let plaintext = open(passphrase, &blob)?;
                self.by_origin = serde_json::from_slice(&plaintext).unwrap_or_default();
            }
        }
        self.unlocked = true;
        Ok(())
    }

    pub fn lock(&mut self) {
        self.by_origin.clear();
        self.unlocked = false;
    }

    pub fn save(&self, passphrase: &str) -> Result<(), String> {
        let plaintext = serde_json::to_vec(&self.by_origin).map_err(|e| e.to_string())?;
        let blob = seal(passphrase, &random_salt(), &plaintext)?;
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&self.path, blob).map_err(|e| e.to_string())
    }
}
