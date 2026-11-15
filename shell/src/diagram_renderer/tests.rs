use super::*;

#[test]
fn scheme_serves_exactly_three_fixed_names() {
    for name in ["diagram-renderer.html", "diagram-renderer.js", "mermaid.js"] {
        assert!(asset_for_uri(&format!("eitri-diagram://renderer/{name}")).is_some());
    }
    for path in [
        "../mermaid.js",
        "%2e%2e/mermaid.js",
        "/mermaid.js",
        "preview.js",
        "mermaid.js?x",
        "mermaid.js#x",
    ] {
        assert!(asset_for_uri(&format!("eitri-diagram://renderer/{path}")).is_none());
    }
    assert!(asset_for_uri("https://renderer/mermaid.js").is_none());
}
#[test]
fn renderer_policy_is_isolated_and_matches_the_document() {
    let html = include_str!("../../../plugins/markdown/web/diagram-renderer.html");
    assert!(html.contains(RENDERER_CSP));
    for directive in [
        "script-src 'self'",
        "connect-src 'none'",
        "img-src 'none'",
        "base-uri 'none'",
    ] {
        assert!(RENDERER_CSP.contains(directive));
    }
    assert!(!RENDERER_CSP.contains("unsafe-eval"));
}
#[test]
fn renderer_calls_only_a_constant_body_with_variant_arguments() {
    use eitri_core::source_scan as scan;
    let source = scan::read_required("shell/src/diagram_renderer.rs");
    let code = scan::code_only(&source.text);
    let production = scan::without(&code, &scan::modules(&code, "tests"));
    assert!(production.contains("call_async_javascript_function("));
    let compact: String = production.split_whitespace().collect();
    assert!(compact.contains("call_async_javascript_function(RENDER_BODY,Some(&arguments)"));
    assert!(compact.contains("constRENDER_BODY:&str="));
    let ready = scan::functions(&production, "page_message")
        .into_iter()
        .next()
        .expect("readiness function");
    let ready: String = ready.split_whitespace().collect();
    assert!(ready.find("loads.ready()").unwrap() < ready.find("execute(state)").unwrap());
    let render = scan::functions(&production, "render")
        .into_iter()
        .next()
        .expect("renderer entry");
    let render: String = render.split_whitespace().collect();
    assert!(render.contains("loads.build_and_install(startup_deadline,||self.build(),|view|"));
    assert!(!render.contains("loads.stopped()"));
    assert!(!production.contains("format!"));
    assert!(!production.contains("push_str"));
    assert_eq!(production.matches("register_script_message_handler").count(), 1);
    let raw_compact: String = source.text.split_whitespace().collect();
    assert!(raw_compact.contains("register_script_message_handler(\"eitriDiagram\",None)"));
}
