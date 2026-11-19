//! Asserts this crate's own `Cargo.toml` never grows a GTK/WebKitGTK dependency.
//!
//! The Mac host draws with AppKit, which arrives under a macOS target table (an `objc2` crate is not on the forbidden
//! list); a GTK or WebKitGTK crate, used or not, would tie the Mac host to the Linux toolkit and break its build on a
//! Mac. The compiler catches a `use gtk4::...` in `src/`; only this catches the manifest line. The list covers the
//! whole gtk-rs family (`gdk4`, `gio`, `pango`, `cairo`, ...), and the manifest is read as TOML through
//! `source_scan::manifest_dependency_offenders`: a dependency in any table of either spelling, a `[dependencies.gtk4]`
//! sub-table, a dotted key (`gtk4.version = ...`) and a rename (`ui = { package = "gtk4" }`) are caught. A rename
//! inherited from the root's `[workspace.dependencies]` is caught too: the root manifest is read with this one. What it
//! cannot see is a toolkit arriving transitively through another dependency; `cargo tree -p eitri-mac -e normal
//! --target all` is the check for that.

use eitri_core::source_scan::{is_package_in_family, manifest_dependency_offenders};
#[cfg(test)]
use eitri_core::source_scan::{AWKWARD_DEPENDENCY_SPELLINGS, NAMES_INSIDE_OTHER_WORDS};

const MANIFEST: &str = include_str!("../Cargo.toml");
/// The root manifest, whose `[workspace.dependencies]` an inherited dependency of this crate resolves through.
const WORKSPACE: &str = include_str!("../../Cargo.toml");

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

/// Every dependency of `manifest` that is, or renames, a forbidden package, as `[table] name`.
fn offenders(manifest: &str) -> Vec<String> {
    manifest_dependency_offenders(manifest, WORKSPACE, |name| is_package_in_family(name, FORBIDDEN))
}

#[test]
fn manifest_declares_no_gtk_or_webkit_dependency() {
    let offenders: Vec<String> = offenders(MANIFEST)
        .into_iter()
        .map(|o| format!("mac/Cargo.toml: {o}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "eitri-mac must not depend on a GTK or WebKitGTK crate on any target -- its macOS half uses AppKit and Apple's WebKit and is compiled on macOS only:\n{}",
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
        "[dependencies.ui]\npackage = \"gtk4\"\nversion = \"0.11\"\n",
    ] {
        assert_eq!(offenders(manifest).len(), 1, "not caught:\n{manifest}");
    }
    for manifest in [
        "[dependencies]\nglibc-version = \"1\"\nterminal-render = { path = \"../render\" }\n",
        "[package]\nname = \"gtk4\"\n",
    ] {
        assert_eq!(offenders(manifest), Vec::<String>::new(), "a false alarm:\n{manifest}");
    }
}

/// [`AWKWARD_DEPENDENCY_SPELLINGS`]: each read through this crate's own list as the one dependency it is.
#[test]
fn awkward_spellings_are_still_caught() {
    let missed: Vec<&str> = AWKWARD_DEPENDENCY_SPELLINGS
        .iter()
        .copied()
        .filter(|manifest| offenders(manifest).len() != 1)
        .collect();
    assert!(missed.is_empty(), "not caught:\n{}", missed.join("\n--\n"));
}

#[test]
fn a_forbidden_name_inside_another_word_is_not_flagged() {
    assert_eq!(offenders(NAMES_INSIDE_OTHER_WORDS), Vec::<String>::new());
}

/// The guard reads an inherited dependency through the root manifest: `claude-runtime-protocol` is the root's one
/// `[workspace.dependencies]` entry, which an empty or wrong workspace text could not resolve.
#[test]
fn an_inherited_dependency_resolves_through_the_root_manifest() {
    let manifest = "[dependencies]\nclaude-runtime-protocol = { workspace = true }\n";
    assert_eq!(offenders(manifest), Vec::<String>::new());
}
