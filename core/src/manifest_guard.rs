//! Asserts this crate's own `Cargo.toml` never grows a GTK/WebKit dependency.
//!
//! Every other check of the property this crate exists to guarantee -- L2's whole point,
//! "让编译器来守边界" (let the compiler guard the boundary) -- has so far been a human running
//! `cargo tree -p neovibe-core | grep -ci gtk` by hand and writing the result down in a commit
//! message or a doc comment. That checks the code, correctly: nothing in `core/src` can name
//! `gtk4`, because nothing here depends on it. But it checks nothing about the *manifest* --
//! there is nothing today stopping someone from adding `gtk4 = "0.11"` to `core/Cargo.toml` and
//! never having a single `use gtk4::...` line to trip a compiler error over. This crate existing
//! at all is what makes the macOS port possible (`shell` cannot build there; `neovibe-core` must
//! be able to); a GTK dependency slipping into the manifest, whether used or not, would silently
//! undo the one property the whole L2 effort was for. This test makes that failure loud and
//! immediate -- a red `cargo test` on the commit that adds it, not a fact someone has to remember
//! to go check by hand.
//!
//! Modeled on `socket_path_guard.rs`: a narrow, self-contained scan with no crate-parsing
//! dependency of its own, kept in its own file for the same reason that one is.

const MANIFEST: &str = include_str!("../Cargo.toml");

/// Package names (or the start of one, for a `gtk4-sys`-shaped transitive) that must never appear
/// as a dependency of this crate.
const FORBIDDEN: &[&str] = &["gtk4", "webkit6", "glib", "gdk"];

#[test]
fn manifest_declares_no_gtk_or_webkit_dependency() {
    let mut in_dependency_table = false;
    let mut offenders = Vec::new();
    for (n, line) in MANIFEST.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Matches `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, and any
            // target-specific variant (`[target.'cfg(...)'.dependencies]`) the same way -- any
            // table whose name ends in "dependencies]" is a table this crate's build graph
            // actually draws from.
            in_dependency_table = trimmed.ends_with("dependencies]");
            continue;
        }
        if !in_dependency_table || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A dependency line is `name = "1.0"` or `name = { version = "1.0", ... }`; the package
        // name is everything before the first `=`, trimmed. A `[dependencies.name]` sub-table
        // header (already excluded above, since it starts with `[`) is the only other shape a
        // Cargo.toml dependency can take, so this covers every real declaration.
        let Some(name) = trimmed.split('=').next() else {
            continue;
        };
        let name = name.trim();
        if FORBIDDEN
            .iter()
            .any(|forbidden| name == *forbidden || name.starts_with(&format!("{forbidden}-")))
        {
            offenders.push(format!("core/Cargo.toml:{}: {trimmed}", n + 1));
        }
    }
    assert!(
        offenders.is_empty(),
        "neovibe-core's whole reason to exist is having no GTK/WebKit dependency -- this crate \
         is what the macOS port builds instead of `shell`, and a toolkit dependency here (used or \
         not) would silently break that:\n{}",
        offenders.join("\n")
    );
}
