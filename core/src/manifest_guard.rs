//! Asserts this crate's `Cargo.toml` has no GTK or WebKit dependency.
//!
//! Compiler errors catch uses of missing dependencies, but adding an unused toolkit dependency
//! would still break this crate's toolkit independence and its macOS build.
//! This test checks the manifest so `cargo test` catches such additions even without a `use` line.
//!
//! Like `socket_path_guard.rs`, this is a self-contained scan with no crate-parsing dependency.

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
