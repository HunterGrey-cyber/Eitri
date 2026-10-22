//! Every script the host runs in a plugin page goes through one call: `WebKitPluginPage`'s `send`.

use eitri_core::source_scan as scan;

fn code(path: &str) -> String {
    let source = scan::read_required(path);
    let code = scan::code_only(&source.text);
    scan::without(&code, &scan::modules(&code, "tests"))
}

#[test]
fn the_plugin_page_runs_script_only_through_its_page_port() {
    let page = code("shell/src/plugin_page.rs");
    assert_eq!(
        page.matches(".evaluate_javascript(").count(),
        1,
        "a second way to run script in the page"
    );
    let flat: String = page.split_whitespace().collect();
    let at = flat
        .find("implPagePortforWebKitPluginPage")
        .expect("the PagePort implementation");
    let body = scan::braced(&flat, at + flat[at..].find('{').unwrap());
    assert_eq!(
        body.matches(".evaluate_javascript(").count(),
        1,
        "the one call is inside PagePort::send"
    );
}

#[test]
fn the_plugin_panels_never_run_script_in_a_page() {
    assert_eq!(
        code("shell/src/plugin_panels.rs")
            .matches(".evaluate_javascript(")
            .count(),
        0
    );
}

/// The theme reaches the view in one place: `apply_theme` sets the view's background and swaps the user style
/// sheet, so the first frame, a reload and a colorscheme change all paint from the same text.
#[test]
fn the_theme_reaches_the_view_only_through_apply_theme() {
    let page = code("shell/src/plugin_page.rs");
    let apply = scan::functions(&page, "apply_theme");
    assert_eq!(apply.len(), 1, "one apply_theme");
    for call in [
        ".add_style_sheet(",
        ".set_background_color(",
        ".remove_all_style_sheets(",
    ] {
        assert_eq!(page.matches(call).count(), 1, "{call} appears once in plugin_page.rs");
        assert_eq!(apply[0].matches(call).count(), 1, "{call} is inside apply_theme");
    }
    let panels = code("shell/src/plugin_panels.rs");
    for call in [
        ".add_style_sheet(",
        ".set_background_color(",
        ".remove_all_style_sheets(",
    ] {
        assert_eq!(panels.matches(call).count(), 0, "{call} in plugin_panels.rs");
    }
}

/// A page's text size reaches its view in one place: `set_zoom`, which the window's text size calls for every
/// page. A second `set_zoom_level` elsewhere would be a level the text-size model does not hold.
#[test]
fn the_zoom_level_reaches_a_page_only_through_set_zoom() {
    let page = code("shell/src/plugin_page.rs");
    let set_zoom = scan::functions(&page, "set_zoom");
    assert_eq!(set_zoom.len(), 1, "one set_zoom");
    assert_eq!(page.matches(".set_zoom_level(").count(), 1);
    assert_eq!(set_zoom[0].matches(".set_zoom_level(").count(), 1, "inside set_zoom");
    // Every other file of the shell: the window's wiring is spread over `main.rs` and the modules it hands parts of
    // its window to (`gtk_host/`, `companion/`), so a list of the files that call `set_zoom` today would miss a
    // second call added where a host or a view is made.
    let others = scan::rust_sources(&["shell/src"]);
    scan::require_anchors(&others, &["fn set_zoom(", "fn attach_page("]);
    let mut read = 0;
    for source in others.iter().filter(|s| s.path != "shell/src/plugin_page.rs") {
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        assert_eq!(
            code.matches(".set_zoom_level(").count(),
            0,
            "set_zoom_level in {}",
            source.path
        );
        read += 1;
    }
    assert!(read > 3, "the scan read the shell's sources ({read} files)");
    let text_size = code("shell/src/text_size.rs");
    assert!(
        text_size.matches("crate::plugin_page::set_zoom(").count() >= 2,
        "the text size sets a page's level when it is attached and whenever the state moves"
    );
}
