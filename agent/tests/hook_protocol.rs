use agent::hook_protocol::{format_decision, parse_pretooluse_input};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
}

#[test]
fn parses_real_pretooluse_stdin_shape() {
    let input = parse_pretooluse_input(&fixture("v2_hook_pretooluse_stdin.json")).unwrap();
    assert_eq!(input.tool_name, "Bash");
    assert!(!input.tool_use_id.is_empty());
    assert!(!input.session_id.is_empty());
}

#[test]
fn format_decision_allow_matches_real_confirmed_shape() {
    let expected: serde_json::Value = serde_json::from_str(&fixture("v2_hook_decision_allow.json")).unwrap();
    let actual: serde_json::Value = serde_json::from_str(&format_decision(true, None)).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn format_decision_deny_matches_real_confirmed_shape() {
    let expected: serde_json::Value = serde_json::from_str(&fixture("v2_hook_decision_deny.json")).unwrap();
    let actual: serde_json::Value =
        serde_json::from_str(&format_decision(false, Some("blocked by capture test"))).unwrap();
    assert_eq!(actual, expected);
}
