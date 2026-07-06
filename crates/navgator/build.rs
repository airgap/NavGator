//! Bake a monotonic build number into the binary (LYK-1498).
//!
//! `NAVGATOR_BUILD` is the same number the packaging scripts stamp into the app-icon badge
//! (`scripts/package.sh`: `git rev-list --count HEAD`), so the running app can report the exact
//! build it is — and the auto-update check can tell two builds apart even without a semver bump,
//! which is what early development wants. Resolution order:
//!   1. the `NAVGATOR_BUILD` env var (CI sets this explicitly — deterministic, no git needed),
//!   2. `git rev-list --count HEAD` (local builds),
//!   3. `0` (tarball builds with no git and no env — the app then treats any published build as
//!      newer, which is the safe default for "you're probably behind").
use std::process::Command;

fn main() {
    let build = std::env::var("NAVGATOR_BUILD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(git_commit_count)
        .unwrap_or_else(|| "0".to_string());
    println!("cargo:rustc-env=NAVGATOR_BUILD={build}");

    // Re-bake when the build number could have changed: a new commit (reflog is appended on every
    // commit/checkout) or an explicit env override.
    println!("cargo:rerun-if-env-changed=NAVGATOR_BUILD");
    if let Some(logs_head) = git_path("logs/HEAD") {
        println!("cargo:rerun-if-changed={logs_head}");
    }
}

/// `git rev-list --count HEAD`, trimmed; None if git is unavailable or this isn't a repo.
fn git_commit_count() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let n = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!n.is_empty()).then_some(n)
}

/// Resolve a path inside the git dir (handles worktrees), so `rerun-if-changed` points at the real
/// reflog even when `.git` is a file or the repo root isn't the crate's parent.
fn git_path(rel: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", rel])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then_some(p)
}
