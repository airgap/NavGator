//! One-time migration of the passphrase store into the Lyku vault (LYK-616, ADR-0060 Decision 10).
//!
//! The steps, in order, and where each lives:
//!
//! 1. Join the vault, or create it and print the recovery key: the vault core's `SyncEngine`.
//! 2. Pull the old `passwords` collection ([`crate::sync::pull_all_passwords`]) and decrypt it,
//!    `passwords.enc` and `autofill.enc` with the old passphrase, one last time: [`collect`].
//! 3. Import each login and the card and address profile as vault items: [`import`].
//! 4. Re-read them and compare with what was decrypted ([`verify`]); only then delete the local
//!    files and the remembered passphrase ([`destroy`]).
//! 5. Ask the server to hard-delete the old rows: LYK-617, below.
//!
//! **Not wired yet.** NavGator cannot link `lyku-vault-core`: the engine (swervo) pins
//! `p256 =0.14.0-rc.14` and `argon2 =0.6.0-rc.8`, the core pins the 0.14.0 and 0.6.0 releases,
//! and Cargo cannot hold a pre-release and its release of one crate in a single graph. Until the
//! engine moves to the releases (GATO-122), steps 1 and 5 have no code here and nothing calls this
//! module.
//! [`Vault`] is the part of `SyncEngine` steps 3 and 4 use, so the tests can run them against a
//! fake; `SyncEngine::create_item` and `SyncEngine::get` fill it once the core links.
#![allow(dead_code)]

use crate::autofill::AutofillProfile;
use crate::password::{self, Credential};
use crate::sync::PulledPassword;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Everything the passphrase opened, merged and ready to import.
pub struct Legacy {
    pub logins: Vec<Credential>,
    pub profile: Option<AutofillProfile>,
}

/// Step 2: decrypt the local `passwords.enc`, the local `autofill.enc` and every pulled row, and
/// merge the logins by `(origin, username)`, keeping the newest `updated`, which is the rule the
/// old sync servers applied. Any blob that does not open is an error, with nothing merged: a
/// partial import would delete what it skipped in step 4.
pub fn collect(
    passphrase: &str,
    passwords_enc: Option<&[u8]>,
    autofill_enc: Option<&[u8]>,
    rows: &[PulledPassword],
) -> Result<Legacy, String> {
    let mut merged: BTreeMap<(String, String), Credential> = BTreeMap::new();
    let mut keep_newest = |c: Credential| {
        let key = (c.origin.clone(), c.username.clone());
        match merged.get(&key) {
            Some(have) if have.updated >= c.updated => {}
            _ => {
                merged.insert(key, c);
            }
        }
    };

    if let Some(blob) = passwords_enc {
        let plain = password::open(passphrase, blob).map_err(|e| format!("passwords.enc: {e}"))?;
        let local: Vec<Credential> =
            serde_json::from_slice(&plain).map_err(|e| format!("passwords.enc: {e}"))?;
        local.into_iter().for_each(&mut keep_newest);
    }

    for (i, row) in rows.iter().enumerate() {
        // NavGator never pushed a deleted password row, so one is not a deletion this client
        // made. Dropping a login on its say-so would lose a password; keeping it costs a stale
        // entry the user can remove (ADR-0060 Decision 7 keeps the item for the same reason).
        if row.deleted {
            continue;
        }
        let blob = crate::hex_decode(&row.payload)
            .ok_or_else(|| format!("synced password {} of {}: payload is not hex", i + 1, rows.len()))?;
        let plain = password::open(passphrase, &blob)
            .map_err(|e| format!("synced password {} of {}: {e}", i + 1, rows.len()))?;
        let cred: Credential = serde_json::from_slice(&plain)
            .map_err(|e| format!("synced password {} of {}: {e}", i + 1, rows.len()))?;
        keep_newest(cred);
    }

    let profile = match autofill_enc {
        Some(blob) => {
            let plain = password::open(passphrase, blob).map_err(|e| format!("autofill.enc: {e}"))?;
            let p: AutofillProfile =
                serde_json::from_slice(&plain).map_err(|e| format!("autofill.enc: {e}"))?;
            (!p.is_blank()).then_some(p)
        }
        None => None,
    };

    Ok(Legacy {
        logins: merged.into_values().collect(),
        profile,
    })
}

fn host_of(origin: &str) -> String {
    url::Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| origin.to_string())
}

/// A `login` item (ADR-0060 Decisions 5 and 10). The origin becomes a **host** match URI, which
/// with the port and scheme rules stays close to the old exact-origin match.
pub fn login_item(c: &Credential) -> Value {
    json!({
        "type": "login",
        "v": 1,
        "title": host_of(&c.origin),
        "uris": [{ "uri": c.origin, "match": "host" }],
        "username": c.username,
        "password": c.password,
        "created": c.updated,
        "modified": c.updated,
    })
}

/// The profile's contact and address half as an `identity` item, or `None` when that half is blank.
pub fn identity_item(p: &AutofillProfile) -> Option<Value> {
    let fields = [
        ("fullName", &p.full_name),
        ("email", &p.email),
        ("phone", &p.phone),
        ("organization", &p.organization),
        ("address1", &p.address1),
        ("address2", &p.address2),
        ("city", &p.city),
        ("region", &p.region),
        ("postalCode", &p.postal_code),
        ("country", &p.country),
    ];
    item("identity", identity_title(p), &fields, p.updated)
}

/// The profile's card half as a `card` item, or `None` when no card was saved. The CVC was never
/// stored and is not now.
pub fn card_item(p: &AutofillProfile) -> Option<Value> {
    let fields = [
        ("cardholderName", &p.cc_name),
        ("number", &p.cc_number),
        ("expMonth", &p.cc_exp_month),
        ("expYear", &p.cc_exp_year),
    ];
    let last4: String = p.cc_number.chars().filter(char::is_ascii_digit).collect();
    let title = match last4.len() {
        0..4 => "Card".to_string(),
        n => format!("Card ending {}", &last4[n - 4..]),
    };
    item("card", title, &fields, p.updated)
}

fn identity_title(p: &AutofillProfile) -> String {
    if p.full_name.is_empty() {
        "Address".to_string()
    } else {
        p.full_name.clone()
    }
}

fn item(kind: &str, title: String, fields: &[(&str, &String)], updated: i64) -> Option<Value> {
    if fields.iter().all(|(_, v)| v.is_empty()) {
        return None;
    }
    let mut o = json!({ "type": kind, "v": 1, "title": title, "created": updated, "modified": updated });
    for (k, v) in fields {
        o[*k] = json!(v);
    }
    Some(o)
}

/// What steps 3 and 4 need from the vault: write a new item, read one back.
pub trait Vault {
    type Id: Clone;
    fn create_item(&mut self, plaintext: &[u8]) -> Result<Self::Id, String>;
    fn read_item(&self, id: &Self::Id) -> Result<Option<Vec<u8>>, String>;
}

/// The vault items written for one migration, in the order [`import`] wrote them.
pub struct Imported<Id> {
    pub items: Vec<(Id, Value)>,
}

/// Step 3: write every login and the profile's items.
// TODO(LYK-616): ADR-0060 Decision 10 merges a login that duplicates an existing vault item by
// registrable domain and username under Decision 7's rules. That needs the core's PSL matching
// and a sync first, so it comes with the core link; until then every login becomes a new item.
pub fn import<V: Vault>(vault: &mut V, legacy: &Legacy) -> Result<Imported<V::Id>, String> {
    let mut wanted: Vec<Value> = legacy.logins.iter().map(login_item).collect();
    if let Some(p) = &legacy.profile {
        wanted.extend(identity_item(p));
        wanted.extend(card_item(p));
    }
    let mut items = Vec::with_capacity(wanted.len());
    for v in wanted {
        let bytes = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        items.push((vault.create_item(&bytes)?, v));
    }
    Ok(Imported { items })
}

/// Step 4a: read every imported item back and require the count and every
/// `(origin, username, password)` triple to match what was decrypted, and each profile item to
/// read back exactly as written. On any mismatch nothing may be deleted.
pub fn verify<V: Vault>(vault: &V, legacy: &Legacy, imported: &Imported<V::Id>) -> Result<(), String> {
    let mut expected_logins: Vec<(String, String, String)> = legacy
        .logins
        .iter()
        .map(|c| (c.origin.clone(), c.username.clone(), c.password.clone()))
        .collect();
    let mut got_logins = Vec::new();
    for (id, written) in &imported.items {
        let bytes = vault
            .read_item(id)?
            .ok_or("an imported item is missing from the vault")?;
        let read: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if read["type"] == "login" {
            let s = |k: &str| read[k].as_str().map(str::to_string);
            let origin = read["uris"][0]["uri"].as_str().map(str::to_string);
            match (origin, s("username"), s("password")) {
                (Some(o), Some(u), Some(p)) => got_logins.push((o, u, p)),
                _ => return Err("an imported login is missing its origin, username or password".into()),
            }
        } else if &read != written {
            return Err(format!("the imported {} item does not match", read["type"]));
        }
    }
    expected_logins.sort();
    got_logins.sort();
    if got_logins.len() != expected_logins.len() {
        return Err(format!(
            "imported {} logins, decrypted {}",
            got_logins.len(),
            expected_logins.len()
        ));
    }
    if got_logins != expected_logins {
        return Err("an imported login does not match what was decrypted".into());
    }
    let profile_items = legacy
        .profile
        .as_ref()
        .map_or(0, |p| identity_item(p).iter().count() + card_item(p).iter().count());
    if imported.items.len() != expected_logins.len() + profile_items {
        return Err("the number of imported items does not match".into());
    }
    Ok(())
}

/// Step 4b: delete `passwords.enc` and `autofill.enc` and the remembered passphrase. A file that
/// is already gone is fine; any other failure stops here and is reported, since the caller is
/// about to tell the user the old store is retired.
pub fn destroy(
    passwords_enc: &Path,
    autofill_enc: &Path,
    forget_passphrase: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    for path in [passwords_enc, autofill_enc] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("could not delete {}: {e}", path.display())),
        }
    }
    forget_passphrase()
    // TODO(LYK-617): step 5, the endpoint that hard-deletes this account's `passwords` rows on the
    // platform the profile is bound to (`syncItems` on lyku.org, `browserSyncItem` on lyku.co).
    // It does not exist yet. Never push tombstones instead: a tombstone keeps the cleartext
    // `origin\x1fusername` item id, and that id is the leak (GATO-121).
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const PASS: &str = "old sync passphrase";

    fn cred(origin: &str, user: &str, pw: &str, updated: i64) -> Credential {
        Credential {
            origin: origin.into(),
            username: user.into(),
            password: pw.into(),
            updated,
        }
    }

    fn sealed<T: serde::Serialize>(v: &T) -> Vec<u8> {
        password::seal(PASS, &password::random_salt(), &serde_json::to_vec(v).unwrap()).unwrap()
    }

    fn row(c: &Credential) -> PulledPassword {
        let hex: String = sealed(c).iter().map(|b| format!("{b:02x}")).collect();
        PulledPassword {
            item_id: format!("{}\u{1f}{}", c.origin, c.username),
            payload: hex,
            updated: c.updated,
            deleted: false,
        }
    }

    fn profile() -> AutofillProfile {
        AutofillProfile {
            full_name: "Alice Example".into(),
            city: "Springfield".into(),
            cc_name: "A EXAMPLE".into(),
            cc_number: "4111 1111 1111 1234".into(),
            cc_exp_month: "04".into(),
            cc_exp_year: "2031".into(),
            updated: 9,
            ..Default::default()
        }
    }

    /// An in-memory vault. `corrupt` changes what one read returns, to stand in for an import
    /// that did not land as written.
    #[derive(Default)]
    struct FakeVault {
        items: Vec<Vec<u8>>,
        corrupt: Option<usize>,
        refuse_writes: bool,
    }

    impl Vault for FakeVault {
        type Id = usize;
        fn create_item(&mut self, plaintext: &[u8]) -> Result<usize, String> {
            if self.refuse_writes {
                return Err("409 conflict".into());
            }
            self.items.push(plaintext.to_vec());
            Ok(self.items.len() - 1)
        }
        fn read_item(&self, id: &usize) -> Result<Option<Vec<u8>>, String> {
            let Some(bytes) = self.items.get(*id) else {
                return Ok(None);
            };
            if self.corrupt == Some(*id) {
                let mut v: Value = serde_json::from_slice(bytes).unwrap();
                v["password"] = json!("something else");
                v["city"] = json!("somewhere else");
                return Ok(Some(serde_json::to_vec(&v).unwrap()));
            }
            Ok(Some(bytes.clone()))
        }
    }

    #[test]
    fn collect_merges_local_and_synced_keeping_the_newest() {
        let local = vec![
            cred("https://a.example", "alice", "local-old", 1),
            cred("https://b.example", "bob", "local-new", 9),
        ];
        let rows = [
            row(&cred("https://a.example", "alice", "synced-new", 5)),
            row(&cred("https://b.example", "bob", "synced-old", 2)),
            row(&cred("https://c.example", "carol", "only-synced", 3)),
        ];
        let got = collect(PASS, Some(&sealed(&local)), Some(&sealed(&profile())), &rows).unwrap();
        let triples: Vec<_> = got
            .logins
            .iter()
            .map(|c| (c.origin.as_str(), c.username.as_str(), c.password.as_str()))
            .collect();
        assert_eq!(
            triples,
            [
                ("https://a.example", "alice", "synced-new"),
                ("https://b.example", "bob", "local-new"),
                ("https://c.example", "carol", "only-synced"),
            ]
        );
        assert_eq!(got.profile.unwrap().cc_number, "4111 1111 1111 1234");
    }

    #[test]
    fn collect_fails_on_any_row_the_passphrase_does_not_open() {
        let mut bad = row(&cred("https://a.example", "alice", "pw", 1));
        bad.payload = password::seal("another passphrase", &password::random_salt(), b"[]")
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let good = row(&cred("https://b.example", "bob", "pw", 1));
        assert!(collect(PASS, None, None, &[good, bad]).is_err());
    }

    #[test]
    fn collect_fails_on_a_corrupt_autofill_profile_rather_than_emptying_it() {
        let not_a_profile = password::seal(PASS, &password::random_salt(), b"not json").unwrap();
        assert!(collect(PASS, None, Some(&not_a_profile), &[]).is_err());
        assert!(collect("wrong passphrase", None, Some(&sealed(&profile())), &[]).is_err());
    }

    #[test]
    fn collect_skips_deleted_rows_and_drops_a_blank_profile() {
        let mut gone = row(&cred("https://a.example", "alice", "pw", 7));
        gone.deleted = true;
        gone.payload = String::new();
        let got = collect(PASS, None, Some(&sealed(&AutofillProfile::default())), &[gone]).unwrap();
        assert!(got.logins.is_empty());
        assert!(got.profile.is_none());
    }

    #[test]
    fn items_carry_host_matching_and_never_a_cvc() {
        let login = login_item(&cred("https://accounts.example.com:8443", "alice", "pw", 4));
        assert_eq!(login["title"], "accounts.example.com");
        assert_eq!(login["uris"][0], json!({ "uri": "https://accounts.example.com:8443", "match": "host" }));
        assert_eq!(login["modified"], 4);

        let card = card_item(&profile()).unwrap();
        assert_eq!(card["title"], "Card ending 1234");
        assert!(card.get("cvc").is_none());
        let identity = identity_item(&profile()).unwrap();
        assert_eq!(identity["fullName"], "Alice Example");
        assert_eq!(identity["postalCode"], "");

        let no_card = AutofillProfile { full_name: "A".into(), ..Default::default() };
        assert!(card_item(&no_card).is_none());
    }

    fn legacy() -> Legacy {
        Legacy {
            logins: vec![
                cred("https://a.example", "alice", "pw-a", 1),
                cred("https://b.example", "bob", "pw-b", 2),
            ],
            profile: Some(profile()),
        }
    }

    #[test]
    fn import_then_verify_passes_when_every_item_reads_back() {
        let mut vault = FakeVault::default();
        let legacy = legacy();
        let imported = import(&mut vault, &legacy).unwrap();
        assert_eq!(imported.items.len(), 4);
        verify(&vault, &legacy, &imported).unwrap();
    }

    #[test]
    fn verify_fails_when_a_login_reads_back_wrong() {
        let mut vault = FakeVault::default();
        let legacy = legacy();
        let imported = import(&mut vault, &legacy).unwrap();
        vault.corrupt = Some(1);
        assert!(verify(&vault, &legacy, &imported).is_err());
    }

    #[test]
    fn verify_fails_when_the_profile_reads_back_wrong() {
        let mut vault = FakeVault::default();
        let legacy = legacy();
        let imported = import(&mut vault, &legacy).unwrap();
        vault.corrupt = Some(2);
        assert!(verify(&vault, &legacy, &imported).is_err());
    }

    #[test]
    fn verify_fails_when_an_item_is_missing_or_a_login_was_never_imported() {
        let mut vault = FakeVault::default();
        let legacy = legacy();
        let mut imported = import(&mut vault, &legacy).unwrap();
        vault.items.pop();
        assert!(verify(&vault, &legacy, &imported).is_err());

        let mut vault = FakeVault::default();
        imported = import(&mut vault, &legacy).unwrap();
        imported.items.remove(0);
        assert!(verify(&vault, &legacy, &imported).is_err());
    }

    #[test]
    fn a_refused_write_stops_the_import() {
        let mut vault = FakeVault { refuse_writes: true, ..Default::default() };
        assert!(import(&mut vault, &legacy()).is_err());
    }

    #[test]
    fn destroy_removes_both_files_then_forgets_the_passphrase() {
        let dir = std::env::temp_dir().join(format!("ng-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (pw, af) = (dir.join("passwords.enc"), dir.join("autofill.enc"));
        std::fs::write(&pw, b"x").unwrap();
        let forgot = Cell::new(false);
        destroy(&pw, &af, || {
            forgot.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!pw.exists() && !af.exists());
        assert!(forgot.get());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn destroy_reports_a_file_it_cannot_delete_and_keeps_the_passphrase() {
        let dir = std::env::temp_dir().join(format!("ng-migrate-dir-{}", std::process::id()));
        // A directory where the file should be: remove_file fails with something other than
        // NotFound.
        let pw = dir.join("passwords.enc");
        std::fs::create_dir_all(&pw).unwrap();
        let forgot = Cell::new(false);
        let res = destroy(&pw, &dir.join("autofill.enc"), || {
            forgot.set(true);
            Ok(())
        });
        assert!(res.is_err());
        assert!(!forgot.get());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn destroy_reports_a_keyring_failure() {
        let dir = std::env::temp_dir().join(format!("ng-migrate-kr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let res = destroy(&dir.join("passwords.enc"), &dir.join("autofill.enc"), || {
            Err("keyring locked".into())
        });
        assert!(res.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
