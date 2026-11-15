use eitri_core::agent_bridge::{parse_inbound_message, validate_diagram_source, InboundMessage};
use serde_json::json;

#[test]
fn renderer_source_dispatch_waits_for_its_own_page_ready_message() {
    use eitri_core::source_scan as scan;
    let source = scan::read_required("shell/src/diagram_renderer.rs");
    let code = scan::code_only(&source.text);
    let code = scan::without(&code, &scan::modules(&code, "tests"));
    assert!(
        !code.contains("connect_load_changed"),
        "load completion cannot authorize rendering"
    );
    let execute = scan::functions(&code, "execute").into_iter().next().unwrap();
    let execute: String = execute.split_whitespace().collect();
    assert!(execute.contains("if!state.loads.is_ready(){return;"));
    let ready = scan::functions(&code, "page_message")
        .into_iter()
        .next()
        .expect("page readiness handler");
    let ready: String = ready.split_whitespace().collect();
    assert!(ready.contains("state.borrow().view.as_ref()!=Some(view)"));
    assert!(ready.contains("message.is_string()"));
    assert!(ready.contains("if!state.borrow().loads.ready(){return;}"));
    assert!(ready.find("loads.ready()").unwrap() < ready.find("execute(state)").unwrap());
    let build = scan::functions(&code, "build").into_iter().next().unwrap();
    assert!(build.contains("connect_script_message_received"));
    assert!(build.contains("page_message"));
    let render = scan::functions(&code, "render").into_iter().next().unwrap();
    let render: String = render.split_whitespace().collect();
    assert!(render.contains(
        "loads.build_and_install(startup_deadline,||self.build(),|view|self.state.borrow_mut().view=Some(view.clone())"
    ));
    assert!(!render.contains("loads.stopped()"));
    assert!(render.find("loads.build_and_install").unwrap() < render.find("view.load_uri(").unwrap());

    let compact_body = |name| {
        let bodies = scan::functions(&code, name);
        assert_eq!(bodies.len(), 1, "one renderer {name} body");
        bodies[0].split_whitespace().collect::<String>()
    };
    assert_eq!(compact_body("cancel"), "{self.end(true);}");
    assert_eq!(compact_body("drop_idle"), "{self.cancel();}");
    assert!(compact_body("end").contains("end_view(&self.state,None,deliberate)"));
    assert!(compact_body("end_view").contains("ifdeliberate{state.loads.cancelled();}else{state.loads.stopped();}"));
}

#[test]
fn diagram_wire_is_text_only_and_window_level() {
    let wire = |id: &str, source: &str| json!({"type":"render_diagram", "id": id, "source": source}).to_string();
    let message = parse_inbound_message(&wire("d-1", "flowchart LR\nA-->B")).unwrap();
    assert!(matches!(message, InboundMessage::RenderDiagram { .. }));
    assert_eq!(message.tab_ref(), eitri_core::agent_bridge::TabRef::WindowLevel);
    for id in ["", "UPPER", "with spaces", &"a".repeat(41)] {
        assert!(parse_inbound_message(&wire(id, "A")).is_none());
    }
    assert!(parse_inbound_message(&wire("a", &"a".repeat(20_001))).is_none());
    assert!(parse_inbound_message(&wire("a", &"a\n".repeat(500))).is_none());
    assert_eq!(validate_diagram_source("a\0b"), Err("it holds hidden characters"));
}

#[test]
fn diagrams_setting_and_display_capability_are_independent_of_backend() {
    use eitri_core::{
        agent_bridge::serialize_display_with_diagrams_for_js,
        agent_prefs::{parse_diagrams, ThinkingDisplay},
    };
    assert_eq!(parse_diagrams(None), Ok(true));
    assert_eq!(parse_diagrams(Some("on")), Ok(true));
    assert_eq!(parse_diagrams(Some("off")), Ok(false));
    assert!(parse_diagrams(Some("yes")).is_err());
    let value: serde_json::Value = serde_json::from_str(&serialize_display_with_diagrams_for_js(
        ThinkingDisplay::Collapsed,
        false,
    ))
    .unwrap();
    assert_eq!(
        value,
        json!({"kind":"display", "thinking":"collapsed", "diagrams":false})
    );
    assert_eq!(validate_diagram_source("A\u{202e}B"), Err("it holds hidden characters"));
}

#[test]
fn transient_call_result_retires_its_view_before_running_completion() {
    use eitri_core::source_scan as scan;
    let source = scan::read_required("shell/src/diagram_renderer.rs");
    let code = scan::code_only(&source.text);
    let execute = scan::functions(&code, "execute").into_iter().next().unwrap();
    let retirement = scan::functions(&code, "end_view")
        .into_iter()
        .next()
        .expect("shared view retirement");
    let retirement: String = retirement.split_whitespace().collect();
    assert!(retirement.find("view.take()").unwrap() < retirement.find("loads.stopped()").unwrap());
    assert!(retirement.contains("call.attempt!=attempt"));
    let compact: String = execute.split_whitespace().collect();
    let retired = compact
        .find("end_view(")
        .or_else(|| compact.find("view.take()"))
        .expect("call result retires its view");
    assert!(
        retired < compact.find("finish(").unwrap(),
        "completion can start another load inline"
    );
}

#[test]
fn renderer_handler_documentation_names_ready_and_failed() {
    let text = eitri_core::source_scan::read_required("shell/src/agent_panel.rs").text;
    assert!(!text.contains("readiness-only"));
    assert!(text.contains("readiness message handler (`ready`/`failed`)"));
}
