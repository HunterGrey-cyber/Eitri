//! Asserts this crate's own `Cargo.toml` never grows a GTK/WebKit dependency.
//!
//! `cargo tree -p eitri-core | grep -ci gtk` checks the dependency tree, but the compiler alone cannot reject an unused
//! toolkit dependency added to the manifest. A GTK dependency, used or not, prevents the macOS host from building on
//! this crate. This scan makes that boundary a test failure; it lives apart from the code it checks. The manifest is
//! read as TOML through `source_scan::manifest_dependency_offenders`, so a dependency in any table of either spelling,
//! a `[dependencies.gtk4]` sub-table, a dotted key and a rename (`ui = { package = "gtk4" }`) are caught. A rename
//! inherited from the root's `[workspace.dependencies]` is not: this manifest names only the key. `cargo tree -p
//! eitri-core -e normal | grep -E 'gtk|gdk|glib|webkit'` is the check for a toolkit arriving transitively through
//! another dependency.

use crate::source_scan::{is_package_in_family, manifest_dependency_offenders};
#[cfg(test)]
use crate::source_scan::{AWKWARD_DEPENDENCY_SPELLINGS, NAMES_INSIDE_OTHER_WORDS};

const MANIFEST: &str = include_str!("../Cargo.toml");

/// Package names (or the start of one, for a `gtk4-sys`-shaped transitive) that must never appear
/// as a dependency of this crate. `gdk4` needs its own entry: neither `gdk` nor `gdk-` matches it.
const FORBIDDEN: &[&str] = &["gtk4", "webkit6", "glib", "gio", "gdk", "gdk4", "gsk4"];

/// Every dependency of `manifest` that is, or renames, a forbidden package, as `[table] name`.
fn forbidden_dependencies(manifest: &str) -> Vec<String> {
    manifest_dependency_offenders(manifest, |name| is_package_in_family(name, FORBIDDEN))
}

#[test]
fn manifest_declares_no_gtk_or_webkit_dependency() {
    let offenders: Vec<String> = forbidden_dependencies(MANIFEST)
        .into_iter()
        .map(|o| format!("core/Cargo.toml: {o}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "eitri-core's whole reason to exist is having no GTK/WebKit dependency -- this crate \
         is what the macOS port builds instead of `shell`, and a toolkit dependency here (used or \
         not) would silently break that:\n{}",
        offenders.join("\n")
    );
}

/// Every shape the scan must see, one manifest each, and the shapes it must not mistake for one.
/// Sub-tables and both rename forms must be checked as well as ordinary dependency keys.
#[test]
fn every_way_a_manifest_can_declare_a_dependency_is_seen() {
    let flagged = [
        "[dependencies]\ngtk4 = \"0.11\"\n",
        "[dependencies]\ngtk4 = { version = \"0.11\", features = [\"v4_14\"] }\n",
        "[dependencies]\ngtk4.version = \"0.11\"\n",
        "[dev-dependencies]\nwebkit6 = \"0.5\"\n",
        "[build-dependencies]\nglib-build-tools = \"0.20\"\n",
        "[target.'cfg(unix)'.dependencies]\ngdk4 = \"0.11\"\n",
        "[dependencies.gtk4]\nversion = \"0.11\"\n",
        "[target.'cfg(target_os = \"linux\")'.dependencies.gtk4]\nversion = \"0.11\"\n",
        "[dependencies]\nui = { package = \"gtk4\", version = \"0.11\" }\n",
        "[dependencies.ui]\npackage = \"gtk4\"\nversion = \"0.11\"\n",
        "[dependencies.gtk4] # comment\nversion = \"0.11\"\n",
    ];
    for manifest in flagged {
        assert_eq!(forbidden_dependencies(manifest).len(), 1, "not flagged:\n{manifest}");
    }
    let clean = [
        "[package]\nname = \"gtk4\"\n",
        "[features]\ngtk4 = []\n",
        "[dependencies]\nmlua = { version = \"0.10\", features = [\"lua54\"] }\n",
        "[dependencies.serde]\nversion = \"1\"\npackage = \"serde\"\n",
        "[dependencies]\n# gtk4 = \"0.11\"\n",
        "[dependencies]\ngtkish = \"1\"\n",
        "[dependencies] # comment\nmlua = \"0.10\"\n",
    ];
    for manifest in clean {
        assert!(forbidden_dependencies(manifest).is_empty(), "flagged:\n{manifest}");
    }
}

/// [`AWKWARD_DEPENDENCY_SPELLINGS`]: each read through this crate's own list as the one dependency it is.
#[test]
fn awkward_spellings_are_still_caught() {
    let missed: Vec<&str> = AWKWARD_DEPENDENCY_SPELLINGS
        .iter()
        .copied()
        .filter(|manifest| forbidden_dependencies(manifest).len() != 1)
        .collect();
    assert!(missed.is_empty(), "not caught:\n{}", missed.join("\n--\n"));
}

#[test]
fn a_forbidden_name_inside_another_word_is_not_flagged() {
    assert_eq!(forbidden_dependencies(NAMES_INSIDE_OTHER_WORDS), Vec::<String>::new());
}
