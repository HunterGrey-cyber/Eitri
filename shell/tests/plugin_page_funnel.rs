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
