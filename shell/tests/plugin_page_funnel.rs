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
