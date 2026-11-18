//! This crate's sources name no toolkit, as `core/tests/core_wm_names_no_toolkit.rs` holds for `core::wm`: no
//! GTK, GDK, GLib, WebKit, AppKit/Objective-C or windowing name in code, so the crate cannot grow a use of
//! one that the manifest guard would also refuse. Read through `source_scan`, which fails on a missing or empty
//! `desk/src`; comments and string literals are blanked first, so the guards' own lists and explanations do
//! not trip it.

use eitri_core::source_scan::{code_only, rust_sources, word_ends};

/// Toolkit names to look for in identifiers and paths.
const TOOLKIT_FRAGMENTS: &[&str] = &[
    "gtk", "gdk", "glib", "gio", "webkit", "objc", "winit", "appkit", "cocoa",
];

/// Case-sensitive type names of the toolkits' widgets and views.
const TOOLKIT_TYPES: &[&str] = &["NSView", "NSWindow", "Widget"];

/// What `code` names that a toolkit-free crate must not, as `fragment` strings.
fn toolkit_names(code: &str) -> Vec<&'static str> {
    let lower = code.to_ascii_lowercase();
    let mut found: Vec<&'static str> = TOOLKIT_FRAGMENTS
        .iter()
        .copied()
        .filter(|name| match *name {
            "gio" => {
                !word_ends(&lower, "gio").is_empty()
                    || !word_ends(&lower, "gio_sys").is_empty()
                    || code
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .filter_map(|ident| ident.strip_prefix("GIO"))
                        .any(|suffix| suffix.starts_with(|c: char| c.is_ascii_uppercase()))
            }
            _ => lower.contains(name),
        })
        .collect();
    found.extend(TOOLKIT_TYPES.iter().copied().filter(|name| code.contains(name)));
    found
}

#[test]
fn desk_sources_name_no_toolkit() {
    let read = rust_sources(&["desk/src"]);
    let offenders: Vec<String> = read
        .iter()
        .flat_map(|source| {
            toolkit_names(&code_only(&source.text))
                .into_iter()
                .map(|name| format!("{} names {name}", source.path))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "eitri-desk is toolkit-free:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scan_sees_a_toolkit_name_in_code_and_not_in_a_comment_or_a_string() {
    assert_eq!(toolkit_names("use gtk4::Button;"), ["gtk"]);
    assert_eq!(toolkit_names("let w: gio::File;"), ["gio"]);
    assert_eq!(toolkit_names("struct HostWidget;"), ["Widget"]);
    assert_eq!(toolkit_names("let v: objc2::rc::Retained<NSView>;"), ["objc", "NSView"]);
    assert!(toolkit_names(&code_only("// gtk4 and WebKit are the host's\nlet s = \"winit\";")).is_empty());
}

#[test]
fn gio_matches_toolkit_identifiers_but_not_regions() {
    assert!(toolkit_names("fn damage_region(region: Region) {}").is_empty());
    assert_eq!(toolkit_names("use GIO::File; use gio_sys::GFile;"), ["gio"]);
    for name in ["GIOStream", "GIOExtensionPoint"] {
        assert_eq!(toolkit_names(&format!("let value: bindings::{name};")), ["gio"]);
    }
}
