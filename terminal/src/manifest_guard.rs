//! Asserts this crate's own `Cargo.toml` never grows a GTK/WebKit dependency.
//!
//! The same guard `eitri-core` has (`core/src/manifest_guard.rs`), for the same reason: the
//! property that matters is that a non-GTK host can build this crate -- the macOS track's NSView,
//! or a future right-side terminal backend -- and a toolkit dependency slipping into the manifest,
//! used or not, would break that silently. The compiler catches a `use gtk4::...` in `src/`; only
//! this catches the manifest line.
//!
//! Stricter than core's in three ways a review found core's could be walked past (2026-09-23,
//! finding 15): the list covers the whole gtk-rs family (`gdk4`, `gio`, `pango`, `cairo`, ...), not
//! only four names; a `[dependencies.gtk4]` sub-table is caught as well as a `gtk4 = ...` line; and so
//! are a dotted key (`gtk4.version = ...`) and a rename (`ui = { package = "gtk4" }`). What it still
//! cannot see is a toolkit arriving transitively through another dependency; `cargo tree -p
//! eitri-terminal -e normal` is the check for that (the plan's Task 2 runs it).

const MANIFEST: &str = include_str!("../Cargo.toml");

/// Package names (or the start of one, for a `gtk4-sys`-shaped transitive) that must never appear
/// as a dependency of this crate.
const FORBIDDEN: &[&str] = &[
    "gtk4",
    "gtk",
    "gdk4",
    "gdk",
    "gsk4",
    "glib",
    "gio",
    "gobject",
    "graphene",
    "pango",
    "cairo",
    "webkit6",
    "webkit2gtk",
    "javascriptcore6",
];

fn forbidden(name: &str) -> bool {
    let name = name.trim().trim_matches('"');
    FORBIDDEN.iter().any(|f| {
        name == *f
            || name
                .strip_prefix(f)
                .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('.'))
    })
}

/// Every line of `manifest` that makes a forbidden package a dependency, as `<line>: <text>`.
fn offenders(manifest: &str) -> Vec<String> {
    let mut in_dependency_table = false;
    let mut offenders = Vec::new();
    for (n, line) in manifest.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(header) = trimmed.strip_prefix('[') {
            let header = header.trim_end_matches(']').trim();
            // `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` and every
            // target-specific variant end in "dependencies"; a `[dependencies.<name>]` sub-table
            // names its package in the header itself. That sub-table's *body* is also a dependency
            // table -- it is the two-line spelling of the same thing `name = { ... }` says inline,
            // so `package = "gtk4"` on the line below `[dependencies.ui]` is exactly as much a
            // rename as `ui = { package = "gtk4" }` is, and must be scanned the same way. A review
            // (2026-09-23) found this shape passing with zero offenders because only the header's
            // own name was checked and `in_dependency_table` stayed `false` for it.
            if let Some((_, name)) = header.rsplit_once("dependencies.") {
                in_dependency_table = true;
                if forbidden(name) {
                    offenders.push(format!("{}: {trimmed}", n + 1));
                }
            } else {
                in_dependency_table = header.ends_with("dependencies");
            }
            continue;
        }
        if !in_dependency_table || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        // A rename is spelled two ways: inline, as `ui = { package = "gtk4", ... }` (the key is
        // the dependency's local name and "package" is embedded in the value); or as a sub-table
        // body line directly under `[dependencies.ui]`, as `package = "gtk4"` (the key itself IS
        // "package" and the value is the real name, with no `{ }` at all). The first shape has
        // "package" appear inside `value`; the second has it appear as `key`, so it must be
        // checked separately or the sub-table form of a rename is missed entirely (2026-09-23
        // review: this is exactly what was missed, since a sub-table's `package = "gtk4"` line
        // has no "package" substring in ITS OWN value to find).
        let renamed_to = if key.trim() == "package" {
            Some(value)
        } else {
            value
                .split("package")
                .nth(1)
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .and_then(|rest| rest.trim_start().strip_prefix('"'))
                .and_then(|rest| rest.split('"').next())
        };
        if forbidden(key) || renamed_to.is_some_and(forbidden) {
            offenders.push(format!("{}: {trimmed}", n + 1));
        }
    }
    offenders
}

#[test]
fn manifest_declares_no_gtk_or_webkit_dependency() {
    let offenders: Vec<String> = offenders(MANIFEST)
        .into_iter()
        .map(|o| format!("terminal/Cargo.toml:{o}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "eitri-terminal must build without a toolkit -- `shell/src/terminal/` is where GTK goes:\n{}",
        offenders.join("\n")
    );
}

/// Each shape a toolkit dependency can take in a manifest, and two that are not one.
#[test]
fn every_shape_of_a_toolkit_dependency_is_caught() {
    for manifest in [
        "[dependencies]\ngtk4 = \"0.11\"\n",
        "[dependencies]\ngdk4 = { version = \"0.11\" }\n",
        "[dependencies]\ngio-sys = \"0.22\"\n",
        "[dependencies]\ncairo-rs = \"0.22\"\n",
        "[dependencies]\ngtk4.version = \"0.11\"\n",
        "[dependencies]\nui = { package = \"gtk4\", version = \"0.11\" }\n",
        "[dependencies.webkit6]\nversion = \"0.5\"\n",
        "[target.'cfg(unix)'.dependencies.glib]\nversion = \"0.22\"\n",
        "[dev-dependencies]\npango = \"0.22\"\n",
        // A rename via the sub-table header form rather than the inline-table form: the same
        // `ui = { package = "gtk4" }` rename, spelled as `[dependencies.ui]` / `package = "gtk4"`.
        // Found by review (2026-09-23): `header.ends_with("dependencies")` is false for
        // "dependencies.ui" (it ends in "ui"), so `in_dependency_table` never became `true` for
        // this header and the `package = "gtk4"` line inside it was skipped outright.
        "[dependencies.ui]\npackage = \"gtk4\"\nversion = \"0.11\"\n",
    ] {
        assert_eq!(offenders(manifest).len(), 1, "not caught:\n{manifest}");
    }
    for manifest in [
        "[dependencies]\nglibc-version = \"1\"\nterminal-render = { path = \"../terminal-render\" }\n",
        "[package]\nname = \"gtk4\"\n",
    ] {
        assert_eq!(offenders(manifest), Vec::<String>::new(), "a false alarm:\n{manifest}");
    }
}
