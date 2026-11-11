//! Builds `agent-ui/web` into its single-file `dist/index.html` before the Mac host compiles, since
//! `src/appkit/page.rs` embeds that file at compile time, exactly as `shell` does. The AppKit half exists
//! only on macOS, so on any other target this script does nothing and needs no Node.js: the host-neutral
//! half builds and tests on Linux without the page.

use std::path::{Path, PathBuf};

// The same build rule as `shell`'s (a sha256 fingerprint over the bundle's inputs, npm only when it
// differs), read from the one file both build scripts include.
include!("../shell/build_web.rs");

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        // Without any `rerun-if` line cargo would rerun this on every change in the package.
        println!("cargo:rerun-if-changed=build.rs");
        return;
    }
    let web_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web");
    if !web_dir.is_dir() {
        panic!(
            "agent-ui/web is not at {} -- this build script was compiled for a checkout that is no \
             longer there, and cargo reused it from a shared target directory. `cargo clean -p \
             eitri-mac` rebuilds it against this one.",
            web_dir.display()
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
    for path in web_build_watch_paths(&web_dir, "dist/index.html") {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=EITRI_WEB_CLEAN_BUILD");
    let clean_build = std::env::var("EITRI_WEB_CLEAN_BUILD").as_deref() == Ok("1");
    ensure_web_bundle_built(&web_dir, "dist/index.html", None, clean_build);
}
