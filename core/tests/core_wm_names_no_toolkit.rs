use eitri_core::source_scan::{code_only, read_required, rust_sources};

#[test]
fn core_wm_names_no_toolkit() {
    let mut sources = rust_sources(&["core/src/wm"]);
    sources.push(read_required("core/src/wm.rs"));
    for source in sources {
        let path = &source.path;
        let code = code_only(&source.text);
        let lower = code.to_ascii_lowercase();
        for name in ["gtk", "gdk", "glib", "webkit", "objc", "winit"] {
            assert!(!lower.contains(name), "{path} names {name}");
        }
        for name in ["NSView", "Widget"] {
            assert!(!code.contains(name), "{path} names {name}");
        }
    }
}
