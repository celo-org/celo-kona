//! Build script that records the celo-kona git SHA so `celo-reth --version` can show
//! it alongside the upstream reth commit.
//!
//! Resolution order (first hit wins):
//!   1. `CELO_KONA_GIT_SHA` env var passed by the build environment (Docker build-arg or explicit
//!      shell override). This is the path Docker/CI builds use, because `.dockerignore` excludes
//!      `.git/` and the in-container source has no git repo.
//!   2. `git rev-parse HEAD` against the workspace. Works for local `cargo build` invocations where
//!      `.git/` is on disk.
//!   3. The literal string `"unknown"`, with a build warning.
//!
//! Output env vars (set via `cargo:rustc-env`):
//!   * `CELO_KONA_GIT_SHA`       — 7-char short SHA (with optional `-dirty` suffix when git reports
//!     uncommitted changes), e.g. `6ee40f2`.
//!   * `CELO_KONA_GIT_SHA_LONG`  — full 40-char SHA.

use std::{env, process::Command};

fn main() {
    let (sha_long, dirty) = resolve_sha();
    let sha_short = sha_long.get(..7).unwrap_or(sha_long.as_str()).to_string();
    let sha_display = if dirty { format!("{sha_short}-dirty") } else { sha_short };

    println!("cargo:rustc-env=CELO_KONA_GIT_SHA_LONG={sha_long}");
    println!("cargo:rustc-env=CELO_KONA_GIT_SHA={sha_display}");

    // Do not add `cargo:rerun-if-changed=<path>` here: emitting any path directive
    // disables Cargo's default package-wide change detection and lets the `-dirty`
    // flag go stale on local edits. Env-changed alone is enough.
    println!("cargo:rerun-if-env-changed=CELO_KONA_GIT_SHA");
}

fn resolve_sha() -> (String, bool) {
    if let Ok(s) = env::var("CELO_KONA_GIT_SHA") {
        let s = s.trim().to_string();
        if !s.is_empty() && s != "unknown" {
            return (s, false);
        }
    }

    if let Some((sha, dirty)) = git_head_sha() {
        return (sha, dirty);
    }

    println!(
        "cargo:warning=celo-reth build.rs: CELO_KONA_GIT_SHA not set and `git rev-parse HEAD` \
         failed; --version will report `unknown`. Set CELO_KONA_GIT_SHA at build time to fix."
    );
    ("unknown".to_string(), false)
}

/// Returns `(sha, is_dirty)` if `git` resolves the workspace HEAD, else `None`.
fn git_head_sha() -> Option<(String, bool)> {
    let head = Command::new("git").args(["rev-parse", "HEAD"]).output().ok()?;
    if !head.status.success() {
        return None;
    }
    let sha = String::from_utf8(head.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    // `--porcelain` is empty iff the working tree is clean.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    Some((sha, dirty))
}
