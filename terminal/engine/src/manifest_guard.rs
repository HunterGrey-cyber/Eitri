//! Asserts this crate's own `Cargo.toml` never grows a GTK/WebKit dependency.
//!
//! The property that matters is that a non-GTK host can build this crate -- the macOS track's NSView, or a future
//! right-side terminal backend -- and a toolkit dependency slipping into the manifest, used or not, would break that
//! silently. The compiler catches a `use gtk4::...` in `src/`; only this catches the manifest line. The list covers the
//! whole gtk-rs family (`gdk4`, `gio`, `pango`, `cairo`, ...), and the manifest is read as TOML through
//! `eitri_core::source_scan::manifest_dependency_offenders` (a dev-dependency of this crate, so it adds nothing to a
//! build of it): a dependency in any table of either spelling, a `[dependencies.gtk4]` sub-table, a dotted key
//! (`gtk4.version = ...`) and a rename (`ui = { package = "gtk4" }`) are caught. A rename inherited from the root's
//! `[workspace.dependencies]` is not: this manifest names only the key. What it cannot see is a toolkit arriving
//! transitively through another dependency; `cargo tree -p eitri-terminal -e normal` is the check for that.

use eitri_core::source_scan::{is_package_in_family, manifest_dependency_offenders, manifest_table};
#[cfg(test)]
use eitri_core::source_scan::{AWKWARD_DEPENDENCY_SPELLINGS, NAMES_INSIDE_OTHER_WORDS};

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

/// Every dependency of `manifest` that is, or renames, a forbidden package, as `[table] name`.
fn offenders(manifest: &str) -> Vec<String> {
    manifest_dependency_offenders(manifest, |name| is_package_in_family(name, FORBIDDEN))
}

#[test]
fn manifest_declares_no_gtk_or_webkit_dependency() {
    let offenders: Vec<String> = offenders(MANIFEST)
        .into_iter()
        .map(|o| format!("terminal/engine/Cargo.toml: {o}"))
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

/// Skia is one optional dependency that only `paint` turns on, and `paint` is a default feature: a build without
/// default features (the remote server's headless terminals) links no Skia.
#[test]
fn skia_is_optional_and_only_behind_paint() {
    assert_eq!(
        manifest_dependency_offenders(MANIFEST, |name| name == "skia-safe"),
        ["[dependencies] skia-safe"],
        "Skia is declared once, as a normal dependency"
    );
    let manifest = manifest_table(MANIFEST);
    let skia = &manifest["dependencies"]["skia-safe"];
    assert_eq!(skia.get("optional").and_then(|optional| optional.as_bool()), Some(true));
    let features = manifest["features"].as_table().expect("a [features] table");
    let enables = |feature: &str| -> Vec<&str> {
        features[feature]
            .as_array()
            .expect("a feature is a list")
            .iter()
            .map(|entry| entry.as_str().expect("a feature entry is a string"))
            .collect()
    };
    assert_eq!(enables("default"), ["paint"]);
    assert_eq!(enables("paint"), ["dep:skia-safe"]);
    for feature in features.keys().filter(|feature| *feature != "paint") {
        assert!(
            !enables(feature).iter().any(|entry| entry.contains("skia-safe")),
            "`{feature}` turns Skia on; only `paint` may"
        );
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
