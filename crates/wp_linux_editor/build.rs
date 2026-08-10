//! Bakes a version string and short commit hash into the binary for the
//! "About" window (see `main.rs`'s `APP_VERSION`/`APP_GIT_HASH`).
//!
//! Deliberately emits no `cargo:rerun-if-changed`/`rerun-if-env-changed`:
//! doing so would replace cargo's default "rerun if anything in the
//! package changed" heuristic with only the listed paths, and short of
//! depending on a crate that already solves it properly (e.g. `vergen`),
//! reliably watching "the commit HEAD points to changed" needs more than
//! `.git/HEAD` (packed-refs, detached HEAD, etc). The default heuristic
//! already reruns this on every source change, which is good enough here.

use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=WP_LINUX_GIT_HASH={hash}");

    // Set by release.yml (resolved before `cargo build` runs) to the
    // release tag being built; absent for plain local/dev builds.
    let version = std::env::var("WP_LINUX_RELEASE_TAG").unwrap_or_else(|_| "dev".to_string());
    println!("cargo:rustc-env=WP_LINUX_VERSION={version}");
}
