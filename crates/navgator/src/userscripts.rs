//! NavGator userscript system — pure, dependency-light core.
//!
//! This module is deliberately free of all `egui` and `servo`/embedder types so it
//! can be unit-tested in isolation and so the engine/UI glue (in `main.rs`, written
//! by a later agent) is the *only* place that touches Servo's `UserContentManager`,
//! `WebView`, `evaluate_javascript`, etc.
//!
//! It implements the forward-compatible data model from
//! `docs/plan/userscripts-design.md` §2 (the unified `Addon` registry + `Permission`
//! enum), the Greasemonkey metadata parser (§3), Chrome match-pattern / glob matching
//! (§4 / engine-gap), the registry persistence (serde_json — the same structured-serde
//! path `password.rs` already uses), and the `GM_*` capability-bridge shim builder
//! (§5).
//!
//! Persistence note: the design doc names `addons.toml`, but the established
//! *structured* serialization path in this codebase is `serde_json` (`password.rs`
//! `PasswordStore::save`/`unlock` use `serde_json::to_vec`/`from_slice`; `serde_json`
//! is already a workspace dep). `Settings` is a bespoke key=value file and `Profile`
//! is TSV — neither is a structured-serde format we should mimic for a registry. So
//! this registry uses `serde_json` and persists to `addons.json`. (The integration
//! agent can name the file `config_file("addons.json")`.)

#![allow(dead_code)]

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// §2 data model — the unified add-on registry (forward-compat core)
// ---------------------------------------------------------------------------

/// Stable identity for an add-on.
///
/// For userscripts this is derived from `@namespace` + `@name` (falling back to the
/// file path) — see [`AddonId::for_userscript`]. The newtype keeps the registry,
/// consent UI and persistence written against an opaque id so a future
/// `WebExtension` kind slots in without a rewrite.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AddonId(pub String);

impl AddonId {
    /// Derive a stable id for a userscript from its metadata.
    ///
    /// Prefers `@namespace` + `\u{0}` + `@name`; if neither name nor namespace is
    /// present, falls back to the on-disk path string. The result is hashed and
    /// hex-encoded so the id is opaque and filesystem-safe.
    pub fn for_userscript(meta: &Metadata, path: &Path) -> AddonId {
        let basis = match (&meta.namespace, &meta.name) {
            (Some(ns), Some(name)) => format!("{ns}\u{0}{name}"),
            (None, Some(name)) => name.clone(),
            (Some(ns), None) => ns.clone(),
            (None, None) => path.to_string_lossy().into_owned(),
        };
        let mut h = DefaultHasher::new();
        basis.hash(&mut h);
        AddonId(format!("us-{:016x}", h.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The kind of add-on. Only `Userscript` is implemented today; the others are
/// declared now so the registry/consent/injection code is kind-agnostic (design §2,
/// §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddonKind {
    Userscript,
    /// Future: pure block-list / CSS / redirect bundles (extensions.md §3.4).
    Declarative,
    /// Future: unpacked MV3 dir (extensions.md Phase E) — gated on isolated worlds.
    WebExtension,
}

/// Where an add-on's code/data comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddonSource {
    Userscript { path: PathBuf, content_hash: u64 },
    // WebExtension { dir: PathBuf, manifest_hash: u64 },  // later
}

/// When a userscript runs (design §2; emulated in the shim per §4/§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunAt {
    DocumentStart,
    DocumentEnd,
    DocumentIdle,
}

impl Default for RunAt {
    fn default() -> Self {
        // Greasemonkey default is document-end / document-idle; we use document-idle
        // as the conventional Tampermonkey default.
        RunAt::DocumentIdle
    }
}

impl RunAt {
    /// Parse a `@run-at` token. Unknown tokens fall back to the default.
    pub fn parse(token: &str) -> RunAt {
        match token.trim() {
            "document-start" | "document_start" => RunAt::DocumentStart,
            "document-end" | "document_end" => RunAt::DocumentEnd,
            "document-idle" | "document_idle" => RunAt::DocumentIdle,
            _ => RunAt::default(),
        }
    }
}

/// A capability an add-on can request/be granted.
///
/// `RunOnSite(MatchPattern)` is *conceptual* per the design — host access is the core
/// permission, but in [`Addon`] the actual match globs are stored separately
/// (`matches`/`excludes`) so they round-trip cleanly. This enum holds the
/// *capability* permissions that map onto the `GM_*` grants and (later) MV3
/// `permissions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Permission {
    /// `GM_xmlhttpRequest` / cross-origin fetch, scoped to `@connect` hosts. The one
    /// capability that genuinely bypasses page CORS and needs embedder cooperation.
    CrossOriginFetch,
    /// `GM_setValue`/`getValue`/`deleteValue` — per-addon key-value store.
    Storage,
    /// `GM_notification`.
    Notifications,
    /// `GM_openInTab` / future tab control.
    TabControl,
    /// Clipboard access.
    Clipboard,
}

impl Permission {
    /// One-line human-readable summary for the consent dialog / settings page.
    pub fn describe(&self) -> &'static str {
        match self {
            Permission::CrossOriginFetch => "Fetch from other sites (cross-origin)",
            Permission::Storage => "Store data",
            Permission::Notifications => "Show notifications",
            Permission::TabControl => "Open and control tabs",
            Permission::Clipboard => "Read and write the clipboard",
        }
    }
}

/// A set of [`Permission`]s. Backed by a `BTreeSet` for deterministic ordering
/// (stable serialization, stable consent-dialog text).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSet {
    perms: BTreeSet<Permission>,
}

impl PermissionSet {
    pub fn new() -> PermissionSet {
        PermissionSet {
            perms: BTreeSet::new(),
        }
    }

    pub fn contains(&self, p: Permission) -> bool {
        self.perms.contains(&p)
    }

    /// Insert a permission; returns true if it was newly added.
    pub fn insert(&mut self, p: Permission) -> bool {
        self.perms.insert(p)
    }

    pub fn remove(&mut self, p: Permission) -> bool {
        self.perms.remove(&p)
    }

    pub fn iter(&self) -> impl Iterator<Item = Permission> + '_ {
        self.perms.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.perms.is_empty()
    }

    pub fn len(&self) -> usize {
        self.perms.len()
    }

    /// True if every permission in `self` is also in `other` (i.e. `self ⊆ other`).
    /// Used to check that a `granted` set is a subset of `requested`, or to detect a
    /// permission set that *grew* (design §3 — re-prompt on growth).
    pub fn is_subset(&self, other: &PermissionSet) -> bool {
        self.perms.is_subset(&other.perms)
    }

    /// Human-readable, multi-capability description (one phrase per granted
    /// capability, joined). Empty set yields an empty string.
    pub fn describe(&self) -> String {
        self.perms
            .iter()
            .map(|p| p.describe())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl FromIterator<Permission> for PermissionSet {
    fn from_iter<I: IntoIterator<Item = Permission>>(iter: I) -> Self {
        PermissionSet {
            perms: iter.into_iter().collect(),
        }
    }
}

/// A single add-on in the registry (design §2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addon {
    pub id: AddonId,
    pub kind: AddonKind,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    /// `@match` / `@include` — host access (conceptually `Permission::RunOnSite`).
    pub matches: Vec<MatchPattern>,
    /// `@exclude`.
    pub excludes: Vec<MatchPattern>,
    pub run_at: RunAt,
    /// What the script's metadata asked for.
    pub requested: PermissionSet,
    /// What the user approved (a subset of `requested`).
    pub granted: PermissionSet,
    /// `@connect` allow-list for cross-origin fetch (hosts).
    pub connect: Vec<String>,
    pub source: AddonSource,
}

impl Addon {
    /// Build an `Addon` from parsed [`Metadata`] and the script's on-disk path +
    /// content hash. `enabled` defaults to false and `granted` is empty — the consent
    /// flow (in `main.rs`) flips `enabled` and copies `requested` into `granted`.
    pub fn from_metadata(meta: &Metadata, path: &Path, content_hash: u64) -> Addon {
        Addon {
            id: AddonId::for_userscript(meta, path),
            kind: AddonKind::Userscript,
            name: meta
                .name
                .clone()
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            version: meta.version.clone().unwrap_or_else(|| "0".to_string()),
            author: meta.author.clone(),
            description: meta.description.clone(),
            enabled: false,
            matches: meta.match_patterns(),
            excludes: meta.exclude_patterns(),
            run_at: meta.run_at,
            requested: meta.permissions(),
            granted: PermissionSet::new(),
            connect: meta.connect.clone(),
            source: AddonSource::Userscript { path: path.to_path_buf(), content_hash },
        }
    }

    /// True if this enabled add-on should run on `url`: at least one `matches` pattern
    /// accepts the URL and no `excludes` pattern matches it.
    pub fn accepts_url(&self, url: &str) -> bool {
        if self.excludes.iter().any(|p| p.matches(url)) {
            return false;
        }
        self.matches.iter().any(|p| p.matches(url))
    }
}

// ---------------------------------------------------------------------------
// §3 Greasemonkey metadata parsing
// ---------------------------------------------------------------------------

/// Parsed Greasemonkey `// ==UserScript== ... // ==/UserScript==` metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub matches: Vec<String>,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub run_at: RunAt,
    /// Raw `@grant` tokens (e.g. `GM_xmlhttpRequest`, `none`).
    pub grants: Vec<String>,
    /// `@connect` hosts.
    pub connect: Vec<String>,
}

impl Metadata {
    /// Parse `@match` + `@include` into [`MatchPattern`]s. `@match` uses Chrome
    /// match-pattern syntax; `@include` uses simple `*`-glob syntax.
    pub fn match_patterns(&self) -> Vec<MatchPattern> {
        let mut out: Vec<MatchPattern> = Vec::new();
        for m in &self.matches {
            out.push(MatchPattern::parse_match(m));
        }
        for i in &self.includes {
            out.push(MatchPattern::parse_include(i));
        }
        out
    }

    pub fn exclude_patterns(&self) -> Vec<MatchPattern> {
        // @exclude historically uses @include (glob) semantics in Greasemonkey.
        self.excludes
            .iter()
            .map(|e| MatchPattern::parse_include(e))
            .collect()
    }

    /// Map `@grant` tokens to the capability [`PermissionSet`] (design §3 table).
    /// `GM_addStyle` and `none` map to no capability (pure in-page / host-only).
    pub fn permissions(&self) -> PermissionSet {
        let mut set = PermissionSet::new();
        for g in &self.grants {
            match g.trim() {
                "GM_xmlhttpRequest" | "GM.xmlHttpRequest" | "GM_xmlHttpRequest" => {
                    set.insert(Permission::CrossOriginFetch);
                }
                "GM_setValue" | "GM_getValue" | "GM_deleteValue" | "GM_listValues"
                | "GM.setValue" | "GM.getValue" | "GM.deleteValue" => {
                    set.insert(Permission::Storage);
                }
                "GM_notification" | "GM.notification" => {
                    set.insert(Permission::Notifications);
                }
                "GM_openInTab" | "GM.openInTab" => {
                    set.insert(Permission::TabControl);
                }
                "GM_setClipboard" | "GM.setClipboard" => {
                    set.insert(Permission::Clipboard);
                }
                // GM_addStyle => pure in-page CSS, no capability.
                // none        => host-only.
                _ => {}
            }
        }
        set
    }
}

/// Parse the Greasemonkey metadata block. Returns `None` if there is no
/// `// ==UserScript==` ... `// ==/UserScript==` block. Unknown `@keys` are ignored;
/// multi-valued keys (`@match`, `@include`, `@exclude`, `@grant`, `@connect`)
/// accumulate.
pub fn parse_metadata(src: &str) -> Option<Metadata> {
    let mut in_block = false;
    let mut meta = Metadata::default();
    let mut saw_block = false;

    for raw in src.lines() {
        let line = raw.trim();
        // Lines in the block are `// @key value` (optionally with leading spaces
        // before `//`). The open/close markers are `// ==UserScript==` /
        // `// ==/UserScript==`.
        let body = match strip_comment_prefix(line) {
            Some(b) => b.trim(),
            None => {
                // A non-comment line ends a (possibly malformed) block early; but
                // tolerate blank lines and keep scanning until the close marker.
                if in_block {
                    // Non-comment content inside the block is unusual; ignore the line.
                    continue;
                }
                continue;
            }
        };

        if body == "==UserScript==" {
            in_block = true;
            saw_block = true;
            continue;
        }
        if body == "==/UserScript==" {
            // Stop after the first complete block.
            break;
        }
        if !in_block {
            continue;
        }
        // Inside the block: parse `@key value`.
        if let Some(rest) = body.strip_prefix('@') {
            let (key, value) = match rest.split_once(char::is_whitespace) {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (rest.trim(), ""),
            };
            apply_meta_key(&mut meta, key, value);
        }
    }

    if saw_block {
        Some(meta)
    } else {
        None
    }
}

/// Strip a leading `//` (with optional surrounding whitespace) comment prefix,
/// returning the comment body. Returns `None` for non-`//` lines.
fn strip_comment_prefix(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix("//")
}

fn apply_meta_key(meta: &mut Metadata, key: &str, value: &str) {
    let v = value.trim();
    match key {
        "name" => {
            if meta.name.is_none() && !v.is_empty() {
                meta.name = Some(v.to_string());
            }
        }
        "namespace" => {
            if meta.namespace.is_none() && !v.is_empty() {
                meta.namespace = Some(v.to_string());
            }
        }
        "version" => {
            if meta.version.is_none() && !v.is_empty() {
                meta.version = Some(v.to_string());
            }
        }
        "description" => {
            if meta.description.is_none() && !v.is_empty() {
                meta.description = Some(v.to_string());
            }
        }
        "author" => {
            if meta.author.is_none() && !v.is_empty() {
                meta.author = Some(v.to_string());
            }
        }
        "match" => {
            if !v.is_empty() {
                meta.matches.push(v.to_string());
            }
        }
        "include" => {
            if !v.is_empty() {
                meta.includes.push(v.to_string());
            }
        }
        "exclude" => {
            if !v.is_empty() {
                meta.excludes.push(v.to_string());
            }
        }
        "run-at" => {
            if !v.is_empty() {
                meta.run_at = RunAt::parse(v);
            }
        }
        "grant" => {
            if !v.is_empty() {
                meta.grants.push(v.to_string());
            }
        }
        "connect" => {
            if !v.is_empty() {
                meta.connect.push(v.to_string());
            }
        }
        // Unknown keys are ignored.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// §4 match patterns — Chrome match-pattern syntax + simple `*` globs
// ---------------------------------------------------------------------------

/// A URL match pattern. Two flavours:
///
/// * [`MatchPattern::Match`] — Chrome match-pattern syntax
///   (`<scheme>://<host>/<path>`, with `*` scheme, `*.`-host wildcard, and `*` path
///   wildcards). The special `<all_urls>` is also accepted.
/// * [`MatchPattern::Glob`] — a simple `*`-glob over the whole URL string (the
///   Greasemonkey `@include`/`@exclude` flavour).
///
/// All matching is hand-rolled; no regex / heavy dependency.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchPattern {
    /// Chrome match pattern, pre-split into (scheme, host, path).
    Match {
        /// `"*"` means http or https; otherwise an exact scheme like `"https"`.
        scheme: String,
        /// `"*"` = any host; `"*.example.com"` = example.com or any subdomain;
        /// otherwise an exact host. Empty for `<all_urls>`.
        host: String,
        /// Path glob (may contain `*`). Empty path is treated as `/`.
        path: String,
        /// `<all_urls>` — matches any http(s)/ftp/file URL regardless of host/path.
        all_urls: bool,
    },
    /// Whole-URL `*`-glob (Greasemonkey `@include`).
    Glob(String),
}

impl MatchPattern {
    /// Parse Chrome match-pattern syntax. `<all_urls>` is special-cased. A pattern
    /// that cannot be split into `scheme://host/path` is degraded to a [`Glob`] so it
    /// still does something sensible rather than silently never matching.
    pub fn parse_match(pat: &str) -> MatchPattern {
        let pat = pat.trim();
        if pat == "<all_urls>" {
            return MatchPattern::Match {
                scheme: "*".to_string(),
                host: String::new(),
                path: "/*".to_string(),
                all_urls: true,
            };
        }
        // Split scheme.
        let Some((scheme, rest)) = pat.split_once("://") else {
            // Not a real match pattern — treat as a glob.
            return MatchPattern::Glob(pat.to_string());
        };
        // rest = host[/path]
        let (host, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        MatchPattern::Match {
            scheme: scheme.to_string(),
            host: host.to_string(),
            path: if path.is_empty() { "/".to_string() } else { path.to_string() },
            all_urls: false,
        }
    }

    /// Parse a Greasemonkey `@include`/`@exclude` glob. If it looks like a real
    /// match pattern (`scheme://...`) we still parse it as a match pattern (TM accepts
    /// both forms in @include); otherwise it's a whole-URL `*`-glob.
    pub fn parse_include(pat: &str) -> MatchPattern {
        let pat = pat.trim();
        if pat == "*" {
            // A bare `*` include == all URLs.
            return MatchPattern::Glob("*".to_string());
        }
        if pat.contains("://") {
            return MatchPattern::parse_match(pat);
        }
        MatchPattern::Glob(pat.to_string())
    }

    /// Does this pattern accept `url`?
    pub fn matches(&self, url: &str) -> bool {
        match self {
            MatchPattern::Glob(g) => glob_match(g, url),
            MatchPattern::Match { scheme, host, path, all_urls } => {
                let Some(parts) = split_url(url) else {
                    return false;
                };
                // scheme
                if !scheme_matches(scheme, parts.scheme) {
                    return false;
                }
                if *all_urls {
                    return true;
                }
                if !host_matches(host, parts.host) {
                    return false;
                }
                // path glob — match against path (+ query, mirroring Chrome which
                // matches path-and-query against the path pattern's tail wildcard).
                let path_and_query = parts.path_and_query;
                glob_match(path, path_and_query)
            }
        }
    }
}

struct UrlParts<'a> {
    scheme: &'a str,
    host: &'a str,
    path_and_query: &'a str,
}

/// Decompose a URL into (scheme, host, path+query). Hand-rolled, tolerant; returns
/// `None` only if there is no `scheme://`.
fn split_url(url: &str) -> Option<UrlParts<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    // rest = host[:port][/path][?query][#frag]
    // Strip fragment.
    let rest = rest.split('#').next().unwrap_or(rest);
    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    // authority = userinfo@host:port — strip userinfo and port for host.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority.split(':').next().unwrap_or(authority);
    Some(UrlParts {
        scheme,
        host,
        path_and_query: if path_and_query.is_empty() { "/" } else { path_and_query },
    })
}

/// Scheme match: pattern `"*"` matches http/https; otherwise exact (case-insensitive).
fn scheme_matches(pattern: &str, scheme: &str) -> bool {
    if pattern == "*" {
        return scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    }
    pattern.eq_ignore_ascii_case(scheme)
}

/// Host match per Chrome rules:
/// * `"*"`            → any host
/// * `"*.example.com"`→ `example.com` OR any subdomain of it
/// * exact host       → case-insensitive equality
///
/// Host comparison is case-insensitive.
fn host_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let host = host.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // matches the bare domain and any subdomain.
        return host == suffix || host.ends_with(&format!(".{suffix}"));
    }
    host == pattern
}

/// Glob match: `*` matches any run of characters (including empty / `/`). Anchored at
/// both ends. Hand-rolled greedy backtracking matcher — no regex.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(p: &[char], t: &[char]) -> bool {
    // Classic two-pointer wildcard match with backtracking on `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_t = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_t = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// content hash
// ---------------------------------------------------------------------------

/// Stable content hash of a script source (used to diff against the registry — design
/// §3). `std`'s `DefaultHasher` is sufficient (not a security hash).
pub fn content_hash(src: &str) -> u64 {
    let mut h = DefaultHasher::new();
    src.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// §2 registry persistence (serde_json — the structured-serde path)
// ---------------------------------------------------------------------------

/// The add-on registry: the persisted list of add-ons + their consent state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub addons: Vec<Addon>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry { addons: Vec::new() }
    }

    /// Load the registry from `path`. A missing file yields an empty registry (not an
    /// error) so first run just works. A malformed file is an error.
    pub fn load(path: impl AsRef<Path>) -> Result<Registry, String> {
        let path = path.as_ref();
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("parse {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::new()),
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
    }

    /// Persist the registry to `path` (pretty JSON), creating parent dirs.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| format!("serialize registry: {e}"))?;
        std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn get(&self, id: &AddonId) -> Option<&Addon> {
        self.addons.iter().find(|a| &a.id == id)
    }

    pub fn get_mut(&mut self, id: &AddonId) -> Option<&mut Addon> {
        self.addons.iter_mut().find(|a| &a.id == id)
    }

    /// Insert a new add-on or replace the existing one with the same id, returning the
    /// previous value if any.
    pub fn upsert(&mut self, addon: Addon) -> Option<Addon> {
        if let Some(slot) = self.addons.iter_mut().find(|a| a.id == addon.id) {
            Some(std::mem::replace(slot, addon))
        } else {
            self.addons.push(addon);
            None
        }
    }

    pub fn remove(&mut self, id: &AddonId) -> Option<Addon> {
        if let Some(pos) = self.addons.iter().position(|a| &a.id == id) {
            Some(self.addons.remove(pos))
        } else {
            None
        }
    }

    /// All enabled add-ons that should run on `url`: enabled, a `matches` pattern
    /// accepts the URL, and no `excludes` pattern matches. Order preserves registry
    /// order. This is the per-navigation script-selection primitive (design §4).
    pub fn enabled_matching(&self, url: &str) -> Vec<&Addon> {
        self.addons
            .iter()
            .filter(|a| a.enabled && a.accepts_url(url))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// §5 GM_* capability bridge shim builder
// ---------------------------------------------------------------------------

/// Build the injected JS for a userscript (design §5).
///
/// The output is a single IIFE that:
/// 1. captures pristine `fetch`/`XMLHttpRequest` refs **before** page script runs,
/// 2. embeds the per-injection capability `cap_token`,
/// 3. defines **only the granted** `GM_*` functions — privileged ones route through
///    `__nativeFetch("navgator://gm/<cap_token>/<call>", ...)`; `GM_addStyle` is pure
///    in-page (no capability),
/// 4. emulates `@run-at` (document-start = run now; document-end = `DOMContentLoaded`;
///    idle = `requestIdleCallback`),
/// 5. runs the original source.
///
/// Pure String builder — fully unit-testable. `source` is embedded as the function
/// body of `__run` verbatim (it is page-trusted code being injected into its own
/// world; we do not try to sandbox it — see §5 security notes). `cap_token` is
/// JS-string-escaped before embedding.
pub fn wrap_userscript(addon: &Addon, source: &str, cap_token: &str) -> String {
    let cap = js_escape(cap_token);
    let granted = &addon.granted;

    let mut gm = String::new();

    // GM_addStyle is always available when requested as a grant, but it needs no
    // capability — we provide it unconditionally since it is pure in-page CSS and
    // harmless. (It is keyed off the grant in real TM; providing it always is benign.)
    gm.push_str(GM_ADDSTYLE);

    if granted.contains(Permission::CrossOriginFetch) {
        gm.push_str(GM_XHR);
    }
    if granted.contains(Permission::Storage) {
        gm.push_str(GM_STORAGE);
    }
    if granted.contains(Permission::Notifications) {
        gm.push_str(GM_NOTIFICATION);
    }
    if granted.contains(Permission::TabControl) {
        gm.push_str(GM_OPENINTAB);
    }
    if granted.contains(Permission::Clipboard) {
        gm.push_str(GM_SETCLIPBOARD);
    }

    let run_dispatch = match addon.run_at {
        RunAt::DocumentStart => "__run();",
        RunAt::DocumentEnd => {
            "if (document.readyState === 'loading') { \
               document.addEventListener('DOMContentLoaded', __run, { once: true }); \
             } else { __run(); }"
        }
        RunAt::DocumentIdle => {
            "if (typeof requestIdleCallback === 'function') { \
               requestIdleCallback(__run); \
             } else if (document.readyState === 'complete') { \
               __run(); \
             } else { \
               window.addEventListener('load', function(){ \
                 (typeof requestIdleCallback === 'function') ? requestIdleCallback(__run) : __run(); \
               }, { once: true }); \
             }"
        }
    };

    // The original source goes inside __run as a function body. We do NOT escape it —
    // it is the script's own code being injected into its own (page) world.
    format!(
        r#"(function () {{
  "use strict";
  // 1. Pristine references captured before page script can shadow them.
  const __nativeFetch = window.fetch.bind(window);
  const __XHR = window.XMLHttpRequest;
  // 2. Capability token (per-process secret, bound to this add-on), validated server-side
  //    in load_web_resource. NOT secret from the page sharing this world (no isolated world).
  const __cap = "{cap}";
  // Servo's WebResourceRequest exposes no request BODY to the embedder intercept
  // (load_web_resource sees only method/headers/url), so call args travel in the URL
  // query — readable there via url.query_pairs(). GET keeps it a plain readable request.
  const __ep = (call, args, cb) =>
    "navgator://gm/" + __cap + "/" + call +
    "?a=" + encodeURIComponent(JSON.stringify(args == null ? {{}} : args)) +
    (cb != null ? "&cb=" + cb : "");
  // Transport: an Image beacon, NOT fetch()/XHR. A live run proved Servo routes NEITHER fetch nor
  // XHR of a custom (navgator://) scheme to the embedder interceptor (both throw NetworkError) —
  // but a subresource <img> load DOES reach load_web_resource (same path gator://font/* uses).
  // Fire-and-forget only: the "image" load fails and the page reads no response body. handle_gm_bridge
  // still performs the side effect (e.g. storage.set), which is all these calls need.
  const __bridge = (call, args) => {{ try {{ (new Image()).src = __ep(call, args); }} catch (e) {{}} return Promise.resolve(); }};
  // Data-returning calls (storage.get/list, net.fetch): register a callback keyed by a per-page id,
  // fire the beacon carrying that id, and let handle_gm_bridge push the result back through a native
  // evaluate_javascript(window.__ngGmResolve(id, ok, json)). The registry is page-global (defined
  // once across all userscripts). A 5s timeout rejects if no push arrives.
  if (!window.__ngGmResolve) {{
    window.__ngGmCb = {{}};
    window.__ngGmSeq = 0;
    window.__ngGmResolve = function (id, ok, json) {{
      var cb = window.__ngGmCb[id]; if (!cb) return; delete window.__ngGmCb[id];
      try {{ ok ? cb.res(json ? JSON.parse(json) : null) : cb.rej(new Error(json || "gm error")); }}
      catch (e) {{ cb.rej(e); }}
    }};
  }}
  const __bridgeJson = (call, args) => new Promise(function (res, rej) {{
    try {{
      var id = ++window.__ngGmSeq;
      window.__ngGmCb[id] = {{ res: res, rej: rej }};
      (new Image()).src = __ep(call, args, id);
      setTimeout(function () {{ if (window.__ngGmCb[id]) {{ delete window.__ngGmCb[id]; rej(new Error("gm bridge timeout")); }} }}, 5000);
    }} catch (e) {{ rej(e); }}
  }});
{gm}  // 3. Original userscript source.
  function __run() {{
{source}
  }}
  // 4. @run-at emulation.
  {run_dispatch}
}})();
"#,
        cap = cap,
        gm = gm,
        source = source,
        run_dispatch = run_dispatch,
    )
}

/// JS-escape a string for embedding inside a `"..."` literal.
fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            '<' => out.push_str("\\x3c"), // avoid </script> breakouts
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// --- GM_* shim fragments (each ends with a newline; injected only when granted) ---

const GM_ADDSTYLE: &str = r#"  const GM_addStyle = (css) => {
    const el = document.createElement("style");
    el.textContent = css;
    (document.head || document.documentElement).appendChild(el);
    return el;
  };
"#;

const GM_XHR: &str = r#"  const GM_xmlhttpRequest = (opts) => {
    const o = opts || {};
    const p = __bridgeJson("net.fetch", {
      method: o.method || "GET",
      url: o.url,
      headers: o.headers || {},
      data: o.data,
    });
    p.then((res) => { if (typeof o.onload === "function") o.onload(res); })
     .catch((err) => { if (typeof o.onerror === "function") o.onerror(err); });
    return p;
  };
  window.GM = window.GM || {};
  window.GM.xmlHttpRequest = GM_xmlhttpRequest;
"#;

const GM_STORAGE: &str = r#"  const GM_setValue = (k, v) => __bridge("storage.set", { key: String(k), value: v });
  const GM_getValue = (k, d) => __bridgeJson("storage.get", { key: String(k) })
    .then((r) => (r && "value" in r) ? r.value : d);
  const GM_deleteValue = (k) => __bridge("storage.delete", { key: String(k) });
  const GM_listValues = () => __bridgeJson("storage.list", {}).then((r) => (r && r.keys) || []);
"#;

const GM_NOTIFICATION: &str = r#"  const GM_notification = (text, title) => {
    const o = (typeof text === "object") ? text : { text: text, title: title };
    return __bridge("notify.show", o);
  };
"#;

const GM_OPENINTAB: &str = r#"  const GM_openInTab = (url, opts) => {
    const o = (typeof opts === "object") ? opts : { active: opts !== true };
    return __bridge("tabs.open", { url: url, active: o.active !== false });
  };
"#;

const GM_SETCLIPBOARD: &str = r#"  const GM_setClipboard = (data, info) =>
    __bridge("clipboard.set", { data: String(data), info: info || "text/plain" });
"#;

// ===========================================================================
// tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const GITHUB_DARK: &str = r#"// ==UserScript==
// @name         GitHub Dark
// @namespace    https://example.com/scripts
// @version      1.4
// @description  Dark theme for GitHub
// @author       Octocat
// @match        https://github.com/*
// @match        https://gist.github.com/*
// @exclude      https://github.com/settings/*
// @run-at       document-start
// @grant        GM_addStyle
// @grant        GM_xmlhttpRequest
// @connect      api.github.com
// ==/UserScript==

console.log("hello from GitHub Dark");
"#;

    // ---- parse_metadata ----

    #[test]
    fn parse_real_world_header() {
        let m = parse_metadata(GITHUB_DARK).expect("block present");
        assert_eq!(m.name.as_deref(), Some("GitHub Dark"));
        assert_eq!(m.namespace.as_deref(), Some("https://example.com/scripts"));
        assert_eq!(m.version.as_deref(), Some("1.4"));
        assert_eq!(m.description.as_deref(), Some("Dark theme for GitHub"));
        assert_eq!(m.author.as_deref(), Some("Octocat"));
        assert_eq!(
            m.matches,
            vec![
                "https://github.com/*".to_string(),
                "https://gist.github.com/*".to_string()
            ]
        );
        assert_eq!(m.excludes, vec!["https://github.com/settings/*".to_string()]);
        assert_eq!(m.run_at, RunAt::DocumentStart);
        assert_eq!(m.connect, vec!["api.github.com".to_string()]);
        assert_eq!(m.grants.len(), 2);
    }

    #[test]
    fn parse_grant_none_yields_no_caps() {
        let src = r#"// ==UserScript==
// @name Simple
// @match *://*.example.org/*
// @grant none
// ==/UserScript==
alert(1);
"#;
        let m = parse_metadata(src).unwrap();
        assert!(m.permissions().is_empty());
        assert_eq!(m.run_at, RunAt::default());
        assert_eq!(m.matches, vec!["*://*.example.org/*".to_string()]);
    }

    #[test]
    fn parse_missing_block_is_none() {
        assert!(parse_metadata("// just a comment\nconsole.log(1);").is_none());
        assert!(parse_metadata("").is_none());
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let src = r#"// ==UserScript==
// @name Has Unknown
// @icon https://example.com/i.png
// @noframes
// @match https://x.test/*
// ==/UserScript==
"#;
        let m = parse_metadata(src).unwrap();
        assert_eq!(m.name.as_deref(), Some("Has Unknown"));
        assert_eq!(m.matches, vec!["https://x.test/*".to_string()]);
    }

    #[test]
    fn parse_include_and_multi_grant() {
        let src = r#"// ==UserScript==
// @name Multi
// @include http://old.site/*
// @include *
// @grant GM_setValue
// @grant GM_getValue
// @grant GM_notification
// @grant GM_openInTab
// ==/UserScript==
"#;
        let m = parse_metadata(src).unwrap();
        assert_eq!(m.includes.len(), 2);
        let perms = m.permissions();
        assert!(perms.contains(Permission::Storage));
        assert!(perms.contains(Permission::Notifications));
        assert!(perms.contains(Permission::TabControl));
        assert!(!perms.contains(Permission::CrossOriginFetch));
    }

    // ---- permission mapping ----

    #[test]
    fn grant_xhr_maps_to_crossoriginfetch() {
        let m = parse_metadata(GITHUB_DARK).unwrap();
        let perms = m.permissions();
        assert!(perms.contains(Permission::CrossOriginFetch));
        // GM_addStyle maps to no capability.
        assert!(!perms.contains(Permission::Storage));
        assert_eq!(perms.len(), 1);
    }

    #[test]
    fn permission_set_describe_is_deterministic() {
        let p1: PermissionSet =
            [Permission::Storage, Permission::CrossOriginFetch].into_iter().collect();
        let p2: PermissionSet =
            [Permission::CrossOriginFetch, Permission::Storage].into_iter().collect();
        // BTreeSet ordering => identical text regardless of insert order.
        assert_eq!(p1.describe(), p2.describe());
        assert!(p1.describe().contains("Fetch from other sites"));
        assert!(p1.describe().contains("Store data"));
    }

    #[test]
    fn permission_set_subset() {
        let req: PermissionSet =
            [Permission::Storage, Permission::CrossOriginFetch].into_iter().collect();
        let grant: PermissionSet = [Permission::Storage].into_iter().collect();
        assert!(grant.is_subset(&req));
        assert!(!req.is_subset(&grant));
    }

    // ---- MatchPattern ----

    #[test]
    fn match_scheme_and_host_path() {
        let p = MatchPattern::parse_match("https://github.com/*");
        assert!(p.matches("https://github.com/anthropics/repo"));
        assert!(p.matches("https://github.com/"));
        // wrong scheme
        assert!(!p.matches("http://github.com/x"));
        // wrong host
        assert!(!p.matches("https://gist.github.com/x"));
    }

    #[test]
    fn match_scheme_wildcard_is_http_or_https() {
        let p = MatchPattern::parse_match("*://example.com/*");
        assert!(p.matches("http://example.com/a"));
        assert!(p.matches("https://example.com/a"));
        assert!(!p.matches("ftp://example.com/a"));
    }

    #[test]
    fn match_subdomain_wildcard() {
        let p = MatchPattern::parse_match("https://*.example.com/*");
        assert!(p.matches("https://example.com/x"));
        assert!(p.matches("https://www.example.com/x"));
        assert!(p.matches("https://a.b.example.com/x"));
        assert!(!p.matches("https://notexample.com/x"));
        assert!(!p.matches("https://example.com.evil.com/x"));
    }

    #[test]
    fn match_path_wildcard() {
        let p = MatchPattern::parse_match("https://site.test/admin/*");
        assert!(p.matches("https://site.test/admin/panel"));
        assert!(p.matches("https://site.test/admin/"));
        assert!(!p.matches("https://site.test/public/x"));
    }

    #[test]
    fn match_all_urls() {
        let p = MatchPattern::parse_match("<all_urls>");
        assert!(p.matches("https://anything.test/x"));
        assert!(p.matches("http://localhost/"));
    }

    #[test]
    fn match_host_only_no_path_defaults_root() {
        let p = MatchPattern::parse_match("https://example.com");
        assert!(p.matches("https://example.com/"));
        assert!(p.matches("https://example.com"));
        // path "/" pattern doesn't match a deep path
        assert!(!p.matches("https://example.com/deep/page"));
    }

    #[test]
    fn match_strips_port_and_userinfo_and_query() {
        let p = MatchPattern::parse_match("https://example.com/*");
        assert!(p.matches("https://user:pass@example.com:8443/path?x=1#frag"));
    }

    #[test]
    fn include_glob_match() {
        let p = MatchPattern::parse_include("*://*.wikipedia.org/wiki/*");
        // contains :// so parsed as match pattern
        assert!(p.matches("https://en.wikipedia.org/wiki/Rust"));
        assert!(!p.matches("https://en.wikipedia.org/w/index.php"));
    }

    #[test]
    fn include_bare_glob() {
        let p = MatchPattern::parse_include("*google*");
        assert!(p.matches("https://www.google.com/search"));
        assert!(!p.matches("https://example.com/"));
    }

    #[test]
    fn bare_star_include_matches_all() {
        let p = MatchPattern::parse_include("*");
        assert!(p.matches("https://anything/at/all"));
    }

    #[test]
    fn glob_anchoring() {
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abcd"));
        assert!(glob_match("a*c", "axxxc"));
        assert!(glob_match("a*c", "ac"));
        assert!(!glob_match("a*c", "ab"));
        assert!(glob_match("*", ""));
        assert!(glob_match("**", "anything"));
    }

    // ---- content_hash ----

    #[test]
    fn content_hash_stable_and_distinct() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    // ---- Addon::accepts_url + exclude ----

    #[test]
    fn addon_accepts_with_exclude() {
        let m = parse_metadata(GITHUB_DARK).unwrap();
        let path = PathBuf::from("/scripts/github-dark.user.js");
        let mut addon = Addon::from_metadata(&m, &path, content_hash(GITHUB_DARK));
        addon.enabled = true;
        assert!(addon.accepts_url("https://github.com/anthropics"));
        assert!(addon.accepts_url("https://gist.github.com/x"));
        // excluded
        assert!(!addon.accepts_url("https://github.com/settings/profile"));
        // not matched
        assert!(!addon.accepts_url("https://example.com/"));
    }

    // ---- Registry serde round-trip + enabled_matching ----

    #[test]
    fn registry_roundtrip_and_enabled_matching() {
        let m = parse_metadata(GITHUB_DARK).unwrap();
        let path = PathBuf::from("/scripts/github-dark.user.js");
        let mut addon = Addon::from_metadata(&m, &path, content_hash(GITHUB_DARK));
        addon.enabled = true;
        addon.granted = m.permissions(); // grant requested

        let mut reg = Registry::new();
        assert!(reg.upsert(addon.clone()).is_none());

        // also a disabled one that would otherwise match everything
        let dis_src = "// ==UserScript==\n// @name Off\n// @match <all_urls>\n// @grant none\n// ==/UserScript==\n";
        let dm = parse_metadata(dis_src).unwrap();
        let disabled = Addon::from_metadata(&dm, &PathBuf::from("/s/off.user.js"), content_hash(dis_src));
        reg.upsert(disabled);

        // round-trip through a temp file
        let dir = std::env::temp_dir().join(format!("uscheck-{}", std::process::id()));
        let file = dir.join("addons.json");
        reg.save(&file).expect("save");
        let loaded = Registry::load(&file).expect("load");
        assert_eq!(loaded, reg);
        let _ = std::fs::remove_dir_all(&dir);

        // enabled_matching honors enabled + match + exclude
        let hits = loaded.enabled_matching("https://github.com/x");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "GitHub Dark");

        // disabled all_urls script does not surface
        let hits2 = loaded.enabled_matching("https://random.test/");
        assert!(hits2.is_empty());

        // excluded path
        let hits3 = loaded.enabled_matching("https://github.com/settings/x");
        assert!(hits3.is_empty());
    }

    #[test]
    fn registry_load_missing_is_empty() {
        let p = std::env::temp_dir().join("uscheck-does-not-exist-xyz.json");
        let _ = std::fs::remove_file(&p);
        let reg = Registry::load(&p).expect("missing => empty");
        assert!(reg.addons.is_empty());
    }

    #[test]
    fn registry_upsert_replaces_same_id() {
        let m = parse_metadata(GITHUB_DARK).unwrap();
        let path = PathBuf::from("/scripts/github-dark.user.js");
        let a1 = Addon::from_metadata(&m, &path, content_hash(GITHUB_DARK));
        let mut a2 = a1.clone();
        a2.version = "2.0".to_string();
        let mut reg = Registry::new();
        reg.upsert(a1);
        let prev = reg.upsert(a2);
        assert!(prev.is_some());
        assert_eq!(reg.addons.len(), 1);
        assert_eq!(reg.addons[0].version, "2.0");
    }

    // ---- wrap_userscript ----

    fn addon_with(grants: PermissionSet, run_at: RunAt) -> Addon {
        Addon {
            id: AddonId("us-test".to_string()),
            kind: AddonKind::Userscript,
            name: "T".to_string(),
            version: "1".to_string(),
            author: None,
            description: None,
            enabled: true,
            matches: vec![MatchPattern::parse_match("https://x.test/*")],
            excludes: vec![],
            run_at,
            requested: grants.clone(),
            granted: grants,
            connect: vec![],
            source: AddonSource::Userscript { path: PathBuf::from("/x.user.js"), content_hash: 0 },
        }
    }

    #[test]
    fn wrap_includes_only_granted_gm() {
        let grants: PermissionSet =
            [Permission::CrossOriginFetch, Permission::Storage].into_iter().collect();
        let addon = addon_with(grants, RunAt::DocumentStart);
        let js = wrap_userscript(&addon, "doThing();", "TOKEN123");

        // pristine refs + token
        assert!(js.contains("const __nativeFetch = window.fetch.bind(window);"));
        assert!(js.contains(r#"const __cap = "TOKEN123";"#));
        // bridge endpoint — args travel in the URL query (Servo exposes no request body)
        assert!(js.contains(r#""navgator://gm/" + __cap + "/" + call"#));
        assert!(js.contains(r#""?a=" + encodeURIComponent(JSON.stringify("#));
        // Transport is an Image beacon (Servo blocks custom-scheme fetch/XHR), not fetch options.
        assert!(js.contains("(new Image()).src = __ep(call, args)"));
        assert!(!js.contains(r#"method: "POST""#));
        // granted
        assert!(js.contains("GM_xmlhttpRequest"));
        assert!(js.contains("GM_setValue"));
        assert!(js.contains("net.fetch"));
        // GM_addStyle is always provided (pure in-page)
        assert!(js.contains("GM_addStyle"));
        // NOT granted
        assert!(!js.contains("GM_notification"));
        assert!(!js.contains("GM_openInTab"));
        assert!(!js.contains("GM_setClipboard"));
        // source embedded
        assert!(js.contains("doThing();"));
        // run-at document-start = run now
        assert!(js.contains("__run();"));
        assert!(!js.contains("DOMContentLoaded"));
    }

    #[test]
    fn wrap_no_grants_only_addstyle() {
        let addon = addon_with(PermissionSet::new(), RunAt::DocumentEnd);
        let js = wrap_userscript(&addon, "x();", "C");
        assert!(js.contains("GM_addStyle"));
        assert!(!js.contains("GM_xmlhttpRequest"));
        assert!(!js.contains("GM_setValue"));
        // run-at document-end
        assert!(js.contains("DOMContentLoaded"));
    }

    #[test]
    fn wrap_idle_uses_request_idle_callback() {
        let addon = addon_with(PermissionSet::new(), RunAt::DocumentIdle);
        let js = wrap_userscript(&addon, "x();", "C");
        assert!(js.contains("requestIdleCallback"));
    }

    #[test]
    fn wrap_escapes_cap_token_safely() {
        let addon = addon_with(PermissionSet::new(), RunAt::DocumentStart);
        // a hostile token attempting to break out of the string / inject </script>
        let js = wrap_userscript(&addon, "x();", "a\"); alert(1); //</script>");
        // the raw breakout sequence must not appear verbatim
        assert!(!js.contains(r#"a"); alert(1);"#));
        // quote was escaped
        assert!(js.contains(r#"\""#));
        // `<` escaped to \x3c
        assert!(js.contains(r"\x3c"));
        assert!(!js.contains("</script>"));
    }

    #[test]
    fn wrap_is_iife_wrapped() {
        let addon = addon_with(PermissionSet::new(), RunAt::DocumentStart);
        let js = wrap_userscript(&addon, "x();", "C");
        assert!(js.trim_start().starts_with("(function ()"));
        assert!(js.trim_end().ends_with("})();"));
    }
}
