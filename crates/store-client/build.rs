//! Bakes the store API's base URL into the binary (see `src/lib.rs`'s
//! `DEFAULT_API_URL`), the same way `wp_linux_editor/build.rs` bakes the
//! version/commit hash: `WP_LINUX_API_URL`, if set, wins; otherwise this
//! falls back to the current dev server so a plain `cargo build` keeps
//! working without any setup. CI overrides it to the real server once one
//! exists.
//!
//! Unlike `wp_linux_editor/build.rs` (which deliberately tracks no
//! rerun-if inputs), this one *does* declare `rerun-if-env-changed`:
//! switching `WP_LINUX_API_URL` between local dev servers with no source
//! changes is a normal thing to do here, and without this Cargo has no
//! file-based signal that would otherwise catch that change.

fn main() {
    println!("cargo:rerun-if-env-changed=WP_LINUX_API_URL");
    let api_url = std::env::var("WP_LINUX_API_URL")
        .unwrap_or_else(|_| "http://192.168.4.106:8020".to_string());
    println!("cargo:rustc-env=WP_LINUX_API_URL={api_url}");
}
