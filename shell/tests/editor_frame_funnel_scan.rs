#[test]
fn queue_render_is_called_only_inside_the_frame_funnel() {
    let src = eitri_core::source_scan::read_required("neovide-editor/src/lib.rs");
    let code = eitri_core::source_scan::code_only(&src.text);
    let (_, funnel) = eitri_core::source_scan::function_in(std::slice::from_ref(&src), "request_frame_on");
    let total = eitri_core::source_scan::word_ends(&code, "queue_render").len();
    let inside = eitri_core::source_scan::word_ends(&eitri_core::source_scan::code_only(&funnel), "queue_render").len();
    assert_eq!(
        (total, inside),
        (1, 1),
        "every queue_render in lib.rs must be the funnel's"
    );
}

#[test]
fn the_panes_color_and_focus_setters_reach_the_frame_funnel() {
    let src = eitri_core::source_scan::read_required("neovide-editor/src/lib.rs");
    let sources = std::slice::from_ref(&src);
    for (setter, operation) in [
        ("set_clear_color", "set_clear_color_and_request"),
        ("set_focused", "set_focused_and_request"),
    ] {
        let (_, body) = eitri_core::source_scan::function_in(sources, setter);
        assert_eq!(eitri_core::source_scan::word_ends(&body, operation).len(), 1);
        assert_eq!(eitri_core::source_scan::word_ends(&body, "request_frame").len(), 1);
    }
    let (_, body) = eitri_core::source_scan::function_in(sources, "request_frame");
    assert_eq!(eitri_core::source_scan::word_ends(&body, "request_frame_on").len(), 1);
}

#[test]
fn the_fail_variable_is_read_only_under_the_fault_injection_feature() {
    let src = eitri_core::source_scan::read_required("neovide-editor/src/subsurface/log.rs");
    let (_, body) = eitri_core::source_scan::function_in_raw(std::slice::from_ref(&src), "fault_requested");
    assert!(body.contains("EITRI_EDITOR_SUBSURFACE_FAIL"));
    let compact: String = src.text.chars().filter(|c| !c.is_whitespace()).collect();
    let start = compact.find("pub(super)fnfault_requested").unwrap();
    assert!(compact[..start].ends_with("#[cfg(feature=\"fault-injection\")]"));
    assert_eq!(src.text.matches("EITRI_EDITOR_SUBSURFACE_FAIL").count(), 1);
    let sources = eitri_core::source_scan::rust_sources(&["neovide-editor/src", "shell/src"]);
    let readers: usize = sources
        .iter()
        .map(|source| source.text.matches("EITRI_EDITOR_SUBSURFACE_FAIL").count())
        .sum();
    assert_eq!(readers, 1);
    let shell = eitri_core::source_scan::read_required("shell/Cargo.toml");
    assert!(shell
        .text
        .contains("editor-fault-injection = [\"neovide-editor/fault-injection\"]"));
}
