//! Asserts this crate's own `Cargo.toml` never grows a toolkit, a windowing crate or a host crate.
//!
//! `eitri-desk` is the toolkit-free half of a desktop window and builds on every OS, so a GTK or WebKitGTK crate,
//! an AppKit binding (`objc2` and its family, `block2`, `dispatch2`, `cocoa`), a windowing crate (`winit`, `tao`,
//! `wry`) or the editor's GL renderer would tie it to one host. A host crate (`shell`, `eitri-mac`) would invert the
//! dependency direction. The compiler catches a `use gtk4::...` in `src/`; only this catches the manifest line,
//! dev-dependencies included. What it cannot see is a toolkit arriving transitively through another dependency;
//! `core/tests/f1_no_toolkit_below_shell.rs` checks that with `cargo tree`.
//!
use eitri_core::source_scan::manifest_dependency_offenders;

const MANIFEST: &str = include_str!("../Cargo.toml");

/// Package names (or the start of one, for a `gtk4-sys` or `objc2-foundation`-shaped family) that must never
/// appear as a dependency of this crate.
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
    "objc",
    "objc2",
    "block2",
    "dispatch2",
    "cocoa",
    "winit",
    "wry",
    "tao",
    "neovide",
    "neovide-editor",
    "eitri-mac",
];

fn forbidden(name: &str) -> bool {
    name == "shell"
        || FORBIDDEN
            .iter()
            .any(|family| name == *family || name.strip_prefix(family).is_some_and(|rest| rest.starts_with('-')))
}

fn offenders(manifest: &str) -> Vec<String> {
    manifest_dependency_offenders(manifest, forbidden)
}

#[test]
fn manifest_declares_no_toolkit_or_host_dependency() {
    let offenders: Vec<String> = offenders(MANIFEST)
        .into_iter()
        .map(|o| format!("desk/Cargo.toml:{o}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "eitri-desk must not depend on a toolkit, a windowing crate or a host crate on any target -- it is built and tested on every OS:\n{}",
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
        "[dependencies]\nobjc2-foundation = \"0.3\"\n",
        "[dev-dependencies]\nwinit = \"0.30\"\n",
        "[dependencies]\nshell = { path = \"../shell\" }\n",
        "[dependencies]\ncairo-rs = \"0.22\"\n",
        "[dependencies]\ngtk4.version = \"0.11\"\n",
        "[dependencies]\nui = { package = \"gtk4\", version = \"0.11\" }\n",
        "[dependencies.webkit6]\nversion = \"0.5\"\n",
        "[target.'cfg(unix)'.dependencies.glib]\nversion = \"0.22\"\n",
        "[dev-dependencies]\npango = \"0.22\"\n",
        // A rename via the sub-table header form rather than the inline-table form: the same
        // `ui = { package = "gtk4" }` rename, spelled as `[dependencies.ui]` / `package = "gtk4"`.
        // `header.ends_with("dependencies")` is false for "dependencies.ui" (it ends in "ui"), so
        // without the sub-table branch the `package = "gtk4"` line inside it would be skipped.
        "[dependencies.ui]\npackage = \"gtk4\"\nversion = \"0.11\"\n",
    ] {
        assert_eq!(offenders(manifest).len(), 1, "not caught:\n{manifest}");
    }
    for manifest in [
        "[dependencies]\nglibc-version = \"1\"\nterminal-render = { path = \"../render\" }\nshellexpand = \"3\"\n",
        "[package]\nname = \"gtk4\"\n",
    ] {
        assert_eq!(offenders(manifest), Vec::<String>::new(), "a false alarm:\n{manifest}");
    }
}

#[test]
fn dependency_headers_allow_trailing_comments() {
    assert_eq!(
        offenders("[dev-dependencies] # desktop tests\nwinit = '0.30'\n"),
        ["[dev-dependencies] winit"]
    );
    assert_eq!(
        offenders("[target.'cfg(unix)'.build-dependencies.native] # renamed\npackage = 'gtk4'\n"),
        ["[target.\"cfg(unix)\".build-dependencies] native (package gtk4)"]
    );
}

#[test]
fn every_dependency_table_spelling_is_checked() {
    for kind in [
        "dependencies",
        "dev-dependencies",
        "dev_dependencies",
        "build-dependencies",
        "build_dependencies",
    ] {
        for prefix in ["", "target.'cfg(unix)'."] {
            let table = format!("{prefix}{kind}");
            for manifest in [
                format!("[{table}]\nwinit = '0.30'\n"),
                format!("[{table}.windowing]\npackage = 'winit'\nversion = '0.30'\n"),
            ] {
                assert_eq!(offenders(&manifest).len(), 1, "{manifest}");
            }
        }
    }
}

#[test]
fn legacy_bindings_are_banned_but_shell_words_is_not_a_host() {
    assert_eq!(
        offenders("[dependencies]\nobjc = '0.2'\nshell-words = '1'\n"),
        ["[dependencies] objc"]
    );
}
