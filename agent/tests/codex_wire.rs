//! The Codex wire types and the session config, against frames recorded from `codex app-server` 0.160.0
//! (`tests/fixtures/codex/`, copied from the measurement runs with every email redacted) and against
//! hand-built inputs for the shapes no recorded run produced.

use agent::providers::codex::config::{
    expected_echo, sandbox_mode, sandbox_policy, session_config, SessionConfigError, APPROVAL_POLICY, SANDBOX_MODE,
};
use agent::providers::codex::wire::{
    self, parse_notification, parse_server_request, AccountReadResponse, CodexErrorInfo, CommandApprovalKind, Decision,
    DecisionResponse, InitializeParams, ItemStatus, PatchChangeKind, PermissionsGrant, ServerNotification,
    ServerRequest, SubAgentKind, ThreadItem, ThreadResumeParams, ThreadStartParams, ThreadStartResponse,
    ThreadTurnsListResponse, TurnStartParams, TurnStartResponse, TurnStatus,
};
use agent::Sandbox;
use agent::{DirTrust, EditorServerSpec, Reviewer, TrustPlan};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn plan(dirs: &[(&str, DirTrust)], skills_off: bool) -> TrustPlan {
    TrustPlan {
        dirs: dirs.iter().map(|(d, t)| (PathBuf::from(d), *t)).collect(),
        skills_off,
        fingerprint: "f".into(),
        project_only: Vec::new(),
    }
}

fn untrusted_up_to_slash(root: &str) -> TrustPlan {
    let dirs: Vec<(PathBuf, DirTrust)> = Path::new(root)
        .ancestors()
        .map(|p| (p.to_path_buf(), DirTrust::Untrusted))
        .collect();
    TrustPlan {
        dirs,
        skills_off: true,
        fingerprint: "f".into(),
        project_only: Vec::new(),
    }
}

/// The config for a plan, as the JSON that goes on the wire.
fn config_json(p: &TrustPlan) -> Value {
    session_config(p, None).expect("config").as_value().clone()
}

fn projects(config: &Value) -> BTreeMap<String, String> {
    config["projects"]
        .as_object()
        .expect("projects is an object")
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v["trust_level"].as_str().expect("a trust_level word").to_string(),
            )
        })
        .collect()
}

#[test]
fn session_config_names_a_root_with_dot_and_space_exactly() {
    let config = config_json(&untrusted_up_to_slash("/x/proj.dot dir"));
    assert_eq!(config["projects"]["/x/proj.dot dir"]["trust_level"], "untrusted");
    let top = config.as_object().expect("the config is an object");
    for key in top.keys() {
        assert!(!key.contains('.'), "a top-level key would be split on '.': {key}");
    }
    // The root is one key of the nested `projects` object, never a dotted path.
    assert_eq!(
        projects(&config).keys().filter(|k| k.contains("proj.dot dir")).count(),
        1
    );
}

#[test]
fn session_config_covers_every_ancestor_up_to_root() {
    // Only the root is in the plan: every ancestor up to `/` still gets an explicit entry, untrusted, so
    // no directory Codex walks keeps a trust the user's own file gave it.
    let config = config_json(&plan(&[("/a/b/c", DirTrust::Untrusted)], true));
    let expected: BTreeMap<String, String> = ["/a/b/c", "/a/b", "/a", "/"]
        .iter()
        .map(|d| (d.to_string(), "untrusted".to_string()))
        .collect();
    assert_eq!(projects(&config), expected);

    let config = config_json(&untrusted_up_to_slash("/home/u/src/proj"));
    let keys: Vec<String> = projects(&config).into_keys().collect();
    assert_eq!(keys, vec!["/", "/home", "/home/u", "/home/u/src", "/home/u/src/proj"]);
}

#[test]
fn trusted_plan_trusts_walked_dirs_and_untrusts_ancestors_above() {
    let p = plan(
        &[
            ("/w/repo/sub", DirTrust::Trusted),
            ("/w/repo", DirTrust::Trusted),
            ("/w", DirTrust::Untrusted),
            ("/", DirTrust::Untrusted),
        ],
        false,
    );
    let got = projects(&config_json(&p));
    let want: BTreeMap<String, String> = [
        ("/w/repo/sub", "trusted"),
        ("/w/repo", "trusted"),
        ("/w", "untrusted"),
        ("/", "untrusted"),
    ]
    .iter()
    .map(|(d, w)| (d.to_string(), w.to_string()))
    .collect();
    assert_eq!(got, want);

    // A directory named twice with different levels is untrusted: the narrower answer wins.
    let p = plan(
        &[("/w/repo", DirTrust::Trusted), ("/w/repo", DirTrust::Untrusted)],
        false,
    );
    assert_eq!(projects(&config_json(&p))["/w/repo"], "untrusted");
}

#[test]
fn untrusted_plan_turns_skills_off_trusted_does_not() {
    let off = config_json(&plan(&[("/p", DirTrust::Untrusted)], true));
    assert_eq!(off["skills"], json!({"include_instructions": false}));
    let on = config_json(&plan(&[("/p", DirTrust::Trusted)], false));
    assert!(on.get("skills").is_none(), "a trusted start leaves skills alone: {on}");
}

#[test]
fn features_and_sandbox_pin_always_present() {
    for p in [
        plan(&[("/p", DirTrust::Untrusted)], true),
        plan(&[("/p", DirTrust::Trusted)], false),
    ] {
        let config = config_json(&p);
        assert_eq!(
            config["features"],
            json!({"exec_permission_approvals": false, "request_permissions_tool": false})
        );
        assert_eq!(
            config["sandbox_workspace_write"],
            json!({"writable_roots": [], "network_access": false, "exclude_tmpdir_env_var": false,
                   "exclude_slash_tmp": false})
        );
        let allowed = ["projects", "skills", "features", "sandbox_workspace_write"];
        for key in config.as_object().expect("object").keys() {
            assert!(
                allowed.contains(&key.as_str()),
                "a top-level key outside the allowlist: {key}"
            );
        }
    }
}

#[test]
fn non_utf8_root_refuses_codex() {
    let root = PathBuf::from(OsStr::from_bytes(b"/x/caf\xff"));
    let p = TrustPlan {
        dirs: vec![
            (root.clone(), DirTrust::Untrusted),
            (PathBuf::from("/x"), DirTrust::Untrusted),
        ],
        skills_off: true,
        fingerprint: "f".into(),
        project_only: Vec::new(),
    };
    match session_config(&p, None) {
        Err(SessionConfigError::NonUtf8Root(path)) => assert_eq!(path, root),
        other => panic!("a non-UTF-8 directory must refuse the start, got {other:?}"),
    }
}

#[test]
fn a_plan_that_could_not_be_named_exactly_refuses() {
    assert!(matches!(
        session_config(&plan(&[], true), None),
        Err(SessionConfigError::NoDirectories)
    ));
    assert!(matches!(
        session_config(&plan(&[("relative/dir", DirTrust::Untrusted)], true), None),
        Err(SessionConfigError::NotCanonical(_))
    ));
    assert!(matches!(
        session_config(&plan(&[("/a/../b", DirTrust::Untrusted)], true), None),
        Err(SessionConfigError::NotCanonical(_))
    ));
    // Each refusal reads as a sentence a notice can carry.
    let text = SessionConfigError::NonUtf8Root(PathBuf::from("/x")).to_string();
    assert!(text.contains("Codex"), "{text}");
}

#[test]
fn tool_servers_are_turned_off_by_name_for_tests_only() {
    let config = session_config(&plan(&[("/p", DirTrust::Untrusted)], true), None)
        .expect("config")
        .with_tool_servers_off(&["computer.use", "probe"]);
    let servers = &config.as_value()["mcp_servers"];
    assert_eq!(
        servers,
        &json!({"computer.use": {"enabled": false}, "probe": {"enabled": false}}),
        "each name is one key, a dot included"
    );
    // The rest of the config is untouched.
    assert_eq!(config.as_value()["projects"]["/p"]["trust_level"], "untrusted");
}

/// The server a session is asked to run, with absolute paths as the panel gives them.
fn editor_spec() -> EditorServerSpec {
    EditorServerSpec {
        name: "eitri".into(),
        command: PathBuf::from("/opt/eitri/eitri-editor-bridge"),
        args: vec!["/run/user/1000/eitri/ed-00".into(), "a-key".into()],
        cwd: PathBuf::from("/p"),
    }
}

#[test]
fn the_editor_server_entry_is_nested_and_states_every_field() {
    let trusted = plan(&[("/p", DirTrust::Trusted)], false);
    let config = session_config(&trusted, Some(&editor_spec())).expect("config");
    assert_eq!(
        config.as_value()["mcp_servers"],
        json!({"eitri": {
            "command": "/opt/eitri/eitri-editor-bridge",
            "args": ["/run/user/1000/eitri/ed-00", "a-key"],
            "env": {},
            "cwd": "/p",
            "enabled": true,
            "startup_timeout_sec": 15,
            "tool_timeout_sec": 900,
            "default_tools_approval_mode": "approve",
        }}),
        "an unstated field would fall to whatever the user's or Codex's default says"
    );
    // Nothing else in the config moves for it.
    let mut rest = config.as_value().clone();
    rest.as_object_mut().unwrap().remove("mcp_servers");
    assert_eq!(rest, config_json(&trusted));
}

#[test]
fn no_editor_server_sends_no_mcp_servers() {
    let config = config_json(&plan(&[("/p", DirTrust::Trusted)], false));
    assert!(config.get("mcp_servers").is_none(), "{config}");
}

#[test]
fn resume_sends_it_again() {
    let config = session_config(&plan(&[("/p", DirTrust::Trusted)], false), Some(&editor_spec())).expect("config");
    let resume = serde_json::to_value(ThreadResumeParams::new(
        "t1".into(),
        "/p".into(),
        Reviewer::User,
        Sandbox::Project,
        config.clone(),
    ))
    .unwrap();
    assert_eq!(resume["config"]["mcp_servers"], config.as_value()["mcp_servers"]);
    assert!(resume["config"]["mcp_servers"]["eitri"].is_object(), "{resume}");
}

#[test]
fn no_dotted_key_appears() {
    let mut spec = editor_spec();
    // A dot in a path or a name stays inside one key or one value: the app-server splits only top-level keys.
    spec.name = "ei.tri".into();
    spec.cwd = PathBuf::from("/a.b/c");
    let config = session_config(&plan(&[("/a.b/c", DirTrust::Trusted)], false), Some(&spec)).expect("config");
    let keys: Vec<&String> = config.as_value().as_object().unwrap().keys().collect();
    assert!(keys.iter().all(|key| !key.contains('.')), "{keys:?}");
    assert_eq!(config.as_value()["mcp_servers"]["ei.tri"]["cwd"], "/a.b/c");
}

#[test]
fn a_relative_cwd_is_refused() {
    let trusted = plan(&[("/p", DirTrust::Trusted)], false);
    let relative_cwd = EditorServerSpec {
        cwd: PathBuf::from("p"),
        ..editor_spec()
    };
    assert_eq!(
        session_config(&trusted, Some(&relative_cwd)),
        Err(SessionConfigError::NotCanonical(PathBuf::from("p")))
    );
    let relative_command = EditorServerSpec {
        command: PathBuf::from("eitri-editor-bridge"),
        ..editor_spec()
    };
    assert_eq!(
        session_config(&trusted, Some(&relative_command)),
        Err(SessionConfigError::NotCanonical(PathBuf::from("eitri-editor-bridge")))
    );
}

#[test]
fn turning_tool_servers_off_keeps_the_editor_server() {
    let config = session_config(&plan(&[("/p", DirTrust::Trusted)], false), Some(&editor_spec()))
        .expect("config")
        .with_tool_servers_off(&["probe"]);
    let servers = &config.as_value()["mcp_servers"];
    assert_eq!(servers["probe"], json!({"enabled": false}));
    assert_eq!(servers["eitri"]["enabled"], true);
}

#[test]
fn params_carry_the_allowlisted_policy() {
    let config = session_config(&plan(&[("/p", DirTrust::Untrusted)], true), None).expect("config");
    let start = serde_json::to_value(ThreadStartParams::new(
        "/p".into(),
        Reviewer::Agent,
        Sandbox::Project,
        config.clone(),
    ))
    .unwrap();
    assert_eq!(
        start,
        json!({"cwd": "/p", "approvalPolicy": "on-request", "approvalsReviewer": "auto_review",
               "sandbox": "workspace-write", "config": config})
    );
    let resume = serde_json::to_value(ThreadResumeParams::new(
        "t1".into(),
        "/p".into(),
        Reviewer::User,
        Sandbox::Project,
        config.clone(),
    ))
    .unwrap();
    assert_eq!(
        resume,
        json!({"threadId": "t1", "cwd": "/p", "approvalPolicy": "on-request", "approvalsReviewer": "user",
               "sandbox": "workspace-write", "config": config, "excludeTurns": true})
    );
    let turn = serde_json::to_value(TurnStartParams::new(
        "t1".into(),
        "hello".into(),
        Reviewer::User,
        Sandbox::Project,
    ))
    .unwrap();
    assert_eq!(
        turn,
        json!({"threadId": "t1", "input": [{"type": "text", "text": "hello"}], "approvalPolicy": "on-request",
               "approvalsReviewer": "user",
               "sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
                                 "excludeTmpdirEnvVar": false, "excludeSlashTmp": false}})
    );
    assert_eq!(APPROVAL_POLICY, "on-request");
    assert_eq!(SANDBOX_MODE, "workspace-write");
    let init = serde_json::to_value(InitializeParams::eitri("0.2.1")).unwrap();
    assert_eq!(
        init,
        json!({"clientInfo": {"name": "eitri", "title": "Eitri", "version": "0.2.1"}})
    );
    assert!(
        init.get("capabilities").is_none(),
        "no experimental API is ever asked for"
    );
}

#[test]
fn answers_are_only_accept_decline_cancel_and_an_empty_turn_grant() {
    // No wildcard arm: a new answer stops this test from compiling until its word is checked here.
    fn word(d: Decision) -> &'static str {
        match d {
            Decision::Accept => "accept",
            Decision::Decline => "decline",
            Decision::Cancel => "cancel",
        }
    }
    for d in [Decision::Accept, Decision::Decline, Decision::Cancel] {
        let w = word(d);
        assert_eq!(
            serde_json::to_value(DecisionResponse { decision: d }).unwrap(),
            json!({"decision": w})
        );
    }
    assert_eq!(
        serde_json::to_value(PermissionsGrant::none_for_turn()).unwrap(),
        json!({"permissions": {}, "scope": "turn"})
    );
}

/// What Codex 0.163.0's schema calls `readOnly` carries only `networkAccess`, and it is what a start, a resume and a
/// turn all say at a root that is no boundary.
#[test]
fn the_read_only_sandbox_on_the_wire() {
    let config = session_config(&plan(&[("/p", DirTrust::Untrusted)], true), None).expect("config");
    let start = ThreadStartParams::new("/p".into(), Reviewer::User, Sandbox::ReadOnly, config.clone());
    assert_eq!(serde_json::to_value(start).unwrap()["sandbox"], "read-only");
    let resume = ThreadResumeParams::new("t1".into(), "/p".into(), Reviewer::User, Sandbox::ReadOnly, config);
    assert_eq!(serde_json::to_value(resume).unwrap()["sandbox"], "read-only");
    let turn = TurnStartParams::new("t1".into(), "hello".into(), Reviewer::User, Sandbox::ReadOnly);
    let read_only = json!({"type": "readOnly", "networkAccess": false});
    assert_eq!(serde_json::to_value(turn).unwrap()["sandboxPolicy"], read_only);
    assert_eq!(sandbox_policy(Sandbox::ReadOnly).to_value(), read_only);
    assert_eq!(sandbox_mode(Sandbox::ReadOnly), "read-only");
    assert_eq!(sandbox_mode(Sandbox::Project), SANDBOX_MODE);
}

#[test]
fn expected_echo_is_the_recorded_narrow_echo() {
    // A recorded `thread/resume` answer echoed exactly the pinned sandbox; the expectation must equal it.
    let frames = load("m4-i1-resume.jsonl");
    let result = response_to(&frames, "thread/resume").expect("a recorded thread/resume answer");
    let resp: ThreadStartResponse = serde_json::from_value(result).expect("a resume response");
    let echo = resp.echo();
    let want = expected_echo(Reviewer::User, Sandbox::Project);
    assert_eq!(
        echo.sandbox.as_ref(),
        Some(&serde_json::to_value(&want.sandbox).unwrap())
    );
    assert_eq!(echo.approval_policy, Some(json!(want.approval_policy)));
    assert_eq!(
        echo.approvals_reviewer,
        Some(json!(wire::reviewer_word(want.approvals_reviewer)))
    );
    assert_eq!(
        serde_json::to_value(sandbox_policy(Sandbox::Project)).unwrap(),
        serde_json::to_value(&want.sandbox).unwrap()
    );
}

#[test]
fn codex_error_info_both_forms() {
    let named: CodexErrorInfo = serde_json::from_value(json!("usageLimitExceeded")).unwrap();
    assert_eq!(named, CodexErrorInfo::Named("usageLimitExceeded".into()));
    assert_eq!(named.name(), "usageLimitExceeded");
    assert_eq!(named.http_status(), None);

    let with: CodexErrorInfo =
        serde_json::from_value(json!({"httpConnectionFailed": {"httpStatusCode": 502}})).unwrap();
    assert_eq!(
        with,
        CodexErrorInfo::WithStatus {
            name: "httpConnectionFailed".into(),
            http_status_code: Some(502)
        }
    );
    assert_eq!(with.http_status(), Some(502));

    let null_status: CodexErrorInfo =
        serde_json::from_value(json!({"responseStreamDisconnected": {"httpStatusCode": null}})).unwrap();
    assert_eq!(
        null_status,
        CodexErrorInfo::WithStatus {
            name: "responseStreamDisconnected".into(),
            http_status_code: None
        }
    );
    // A variant whose payload carries no status, and one this build has never heard of.
    let other: CodexErrorInfo =
        serde_json::from_value(json!({"activeTurnNotSteerable": {"turnKind": "review"}})).unwrap();
    assert_eq!(other.name(), "activeTurnNotSteerable");
    let unknown: CodexErrorInfo = serde_json::from_value(json!("somethingNew")).unwrap();
    assert_eq!(unknown.name(), "somethingNew");
    // A shape that is neither form is still an error info, never a parse failure of the turn.
    let odd: CodexErrorInfo = serde_json::from_value(json!({"a": 1, "b": 2})).unwrap();
    assert!(!odd.name().is_empty());
    let huge_status: CodexErrorInfo = serde_json::from_value(json!({"x": {"httpStatusCode": 70000}})).unwrap();
    assert_eq!(huge_status.http_status(), None);
}

#[test]
fn turn_completed_carries_the_error_info() {
    let n = parse_notification(
        "turn/completed",
        json!({"threadId": "t", "turn": {"id": "u", "items": [], "status": "failed",
               "error": {"message": "limit", "codexErrorInfo": "usageLimitExceeded"}}}),
    );
    match n {
        ServerNotification::TurnCompleted(c) => {
            assert_eq!(c.turn.status, TurnStatus::Failed);
            let err = c.turn.error.expect("an error");
            assert_eq!(err.message, "limit");
            assert_eq!(
                err.codex_error_info,
                Some(CodexErrorInfo::Named("usageLimitExceeded".into()))
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_methods_and_values_are_unknown_not_errors() {
    assert!(matches!(
        parse_notification("thread/somethingNew", json!({"x": 1})),
        ServerNotification::Unknown { ref method, .. } if method == "thread/somethingNew"
    ));
    assert!(matches!(
        parse_server_request("item/somethingNew/requestApproval", json!({})),
        ServerRequest::Unknown { ref method, .. } if method == "item/somethingNew/requestApproval"
    ));
    let n = parse_notification(
        "turn/completed",
        json!({"threadId": "t", "turn": {"id": "u", "items": [{"type": "hologram", "id": "h1"}],
               "status": "paused", "itemsView": "sideways"}}),
    );
    match n {
        ServerNotification::TurnCompleted(c) => {
            assert_eq!(c.turn.status, TurnStatus::Unknown("paused".into()));
            match &c.turn.items[0] {
                ThreadItem::Unknown { item_type, id, .. } => {
                    assert_eq!(item_type, "hologram");
                    assert_eq!(id.as_deref(), Some("h1"));
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
    // A known method whose payload is missing what it must carry is reported, not dropped silently.
    assert!(matches!(
        parse_notification("turn/completed", json!({"nope": 1})),
        ServerNotification::Malformed { .. }
    ));
}

#[test]
fn shapes_no_recorded_run_produced() {
    // A file change item and its approval, from the 0.160.0 protocol source.
    let item: ThreadItem = serde_json::from_value(json!({
        "type": "fileChange", "id": "f1", "status": "inProgress",
        "changes": [
            {"path": "/p/a.txt", "kind": {"type": "add"}, "diff": "+a\n"},
            {"path": "/p/b.txt", "kind": {"type": "update", "move_path": "/p/c.txt"}, "diff": "-b\n+c\n"},
            {"path": "/p/d.txt", "kind": {"type": "delete"}, "diff": ""},
            {"path": "/p/e.txt", "kind": {"type": "teleport"}, "diff": ""}
        ]
    }))
    .unwrap();
    match item {
        ThreadItem::FileChange(f) => {
            assert_eq!(f.status, ItemStatus::InProgress);
            assert_eq!(f.changes[0].kind, PatchChangeKind::Add);
            assert_eq!(
                f.changes[1].kind,
                PatchChangeKind::Update {
                    move_path: Some("/p/c.txt".into())
                }
            );
            assert_eq!(f.changes[2].kind, PatchChangeKind::Delete);
            assert_eq!(f.changes[3].kind, PatchChangeKind::Unknown("teleport".into()));
        }
        other => panic!("{other:?}"),
    }
    match parse_server_request(
        "item/fileChange/requestApproval",
        json!({"threadId": "t", "turnId": "u", "itemId": "f1", "startedAtMs": 1, "reason": null, "grantRoot": "/p"}),
    ) {
        ServerRequest::FileChangeApproval(p) => assert_eq!(p.grant_root.as_deref(), Some("/p")),
        other => panic!("{other:?}"),
    }
    match parse_server_request(
        "item/commandExecution/requestApproval",
        json!({"kind": "writeStdin", "threadId": "t", "turnId": "u", "itemId": "c1", "startedAtMs": 1,
               "networkApprovalContext": {"host": "example.org", "protocol": "https"}}),
    ) {
        ServerRequest::CommandApproval(p) => {
            assert_eq!(p.kind, CommandApprovalKind::WriteStdin);
            let net = p.network_approval_context.expect("network context");
            assert_eq!((net.host.as_str(), net.protocol.as_str()), ("example.org", "https"));
        }
        other => panic!("{other:?}"),
    }
    match parse_server_request(
        "item/permissions/requestApproval",
        json!({"threadId": "t", "turnId": "u", "itemId": "p1", "startedAtMs": 1, "cwd": "/p", "reason": null,
               "permissions": {"network": {"enabled": true}}}),
    ) {
        ServerRequest::PermissionsApproval(p) => assert_eq!(p.item_id, "p1"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        parse_server_request("mcpServer/elicitation/request", json!({"threadId": "t"})),
        ServerRequest::McpElicitation(_)
    ));
    assert!(matches!(
        parse_server_request("item/tool/requestUserInput", json!({"threadId": "t"})),
        ServerRequest::ToolRequestUserInput(_)
    ));
    let signed_out: AccountReadResponse =
        serde_json::from_value(json!({"account": null, "requiresOpenaiAuth": true})).unwrap();
    assert!(signed_out.signed_out());
    let signed_in: AccountReadResponse = serde_json::from_value(
        json!({"account": {"type": "chatgpt", "email": "<redacted>", "planType": "pro"}, "requiresOpenaiAuth": true}),
    )
    .unwrap();
    assert!(!signed_in.signed_out());
    // The account's details are never kept, so they can never be logged.
    assert!(!format!("{signed_in:?}").contains("redacted"));
    let api_key: AccountReadResponse =
        serde_json::from_value(json!({"account": null, "requiresOpenaiAuth": false})).unwrap();
    assert!(!api_key.signed_out());
}

// ---- the recorded fixtures ----

struct Frame {
    dir: String,
    msg: Value,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex")
}

fn load(name: &str) -> Vec<Frame> {
    let text = std::fs::read_to_string(fixtures_dir().join(name)).expect("fixture");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("a JSON line");
            Frame {
                dir: v["dir"].as_str().expect("dir").to_string(),
                msg: v["msg"].clone(),
            }
        })
        .collect()
}

fn response_to(frames: &[Frame], method: &str) -> Option<Value> {
    let id = frames.iter().find(|f| f.dir == "out" && f.msg["method"] == method)?.msg["id"].clone();
    frames
        .iter()
        .find(|f| f.dir == "in" && f.msg.get("method").is_none() && f.msg["id"] == id)
        .and_then(|f| f.msg.get("result").cloned())
}

/// Methods this module types: a recorded frame of one of them must never come back as `Unknown`.
const TYPED_NOTIFICATIONS: &[&str] = &[
    "thread/started",
    "turn/started",
    "turn/completed",
    "item/started",
    "item/completed",
    "item/agentMessage/delta",
    "item/commandExecution/outputDelta",
    "thread/tokenUsage/updated",
    "account/rateLimits/updated",
    "mcpServer/startupStatus/updated",
    "hook/started",
    "hook/completed",
    "error",
    "serverRequest/resolved",
];

#[test]
fn fixtures_parse() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("the fixtures directory")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    assert!(files.len() >= 10, "the recorded fixtures are missing: {files:?}");

    let mut typed = 0usize;
    let mut items = 0usize;
    let mut responses = 0usize;
    let mut kinds_seen = std::collections::BTreeSet::new();
    for file in &files {
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        let frames = load(&name);
        let mut asked: BTreeMap<String, String> = BTreeMap::new();
        for f in &frames {
            let line = &f.msg;
            assert_emails_redacted(&name, line);
            if f.dir == "out" {
                if let (Some(id), Some(m)) = (line.get("id"), line["method"].as_str()) {
                    asked.insert(id.to_string(), m.to_string());
                }
                continue;
            }
            let method = line["method"].as_str();
            let params = line.get("params").cloned().unwrap_or(Value::Null);
            match (method, line.get("id")) {
                (Some(m), Some(_)) => {
                    let r = parse_server_request(m, params);
                    assert!(!matches!(r, ServerRequest::Malformed { .. }), "{name}: {m}: {r:?}");
                    assert!(
                        !matches!(r, ServerRequest::Unknown { .. }),
                        "{name}: {m} is typed: {r:?}"
                    );
                    if let ServerRequest::CommandApproval(p) = &r {
                        assert_eq!(p.kind, CommandApprovalKind::Command);
                        assert!(p.command.is_some() && p.cwd.is_some(), "{name}");
                    }
                    typed += 1;
                }
                (Some(m), None) => {
                    let n = parse_notification(m, params);
                    assert!(!matches!(n, ServerNotification::Malformed { .. }), "{name}: {m}: {n:?}");
                    if TYPED_NOTIFICATIONS.contains(&m) {
                        assert!(!matches!(n, ServerNotification::Unknown { .. }), "{name}: {m} is typed");
                        typed += 1;
                    }
                    if let ServerNotification::ItemStarted(i) | ServerNotification::ItemCompleted(i) = &n {
                        items += 1;
                        kinds_seen.insert(item_kind(&i.item));
                        assert!(!matches!(i.item, ThreadItem::Malformed { .. }), "{name}: {:?}", i.item);
                        if let ThreadItem::SubAgentActivity(s) = &i.item {
                            assert!(!s.agent_thread_id.is_empty());
                            assert_ne!(s.kind, SubAgentKind::Unknown(String::new()));
                        }
                    }
                }
                (None, Some(id)) => {
                    let Some(m) = asked.get(&id.to_string()) else { continue };
                    let Some(result) = line.get("result").cloned() else {
                        continue;
                    };
                    responses += 1;
                    parse_response(&name, m, result);
                }
                (None, None) => panic!("{name}: a frame that is neither request, notification nor response"),
            }
        }
    }
    assert!(typed > 200, "too few typed frames parsed: {typed}");
    assert!(items > 50, "too few items parsed: {items}");
    assert!(responses > 30, "too few responses parsed: {responses}");
    for kind in [
        "userMessage",
        "agentMessage",
        "commandExecution",
        "subAgentActivity",
        "collabAgentToolCall",
    ] {
        assert!(
            kinds_seen.contains(kind),
            "no recorded {kind} item was typed: {kinds_seen:?}"
        );
    }
}

/// The fixtures were recorded on a signed-in account: an email may only appear redacted.
fn assert_emails_redacted(name: &str, v: &Value) {
    match v {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "email" {
                    assert!(v.is_null() || v == "<redacted>", "{name}: an unredacted email");
                }
                assert_emails_redacted(name, v);
            }
        }
        Value::Array(a) => a.iter().for_each(|v| assert_emails_redacted(name, v)),
        Value::String(s) => assert_eq!(
            agent::jsonrpc_link::redact_emails(s),
            *s,
            "{name}: an email in a string"
        ),
        _ => {}
    }
}

fn item_kind(item: &ThreadItem) -> &'static str {
    match item {
        ThreadItem::UserMessage(_) => "userMessage",
        ThreadItem::AgentMessage(_) => "agentMessage",
        ThreadItem::Reasoning(_) => "reasoning",
        ThreadItem::CommandExecution(_) => "commandExecution",
        ThreadItem::FileChange(_) => "fileChange",
        ThreadItem::McpToolCall(_) => "mcpToolCall",
        ThreadItem::CollabAgentToolCall(_) => "collabAgentToolCall",
        ThreadItem::SubAgentActivity(_) => "subAgentActivity",
        ThreadItem::Unknown { .. } => "unknown",
        ThreadItem::Malformed { .. } => "malformed",
    }
}

fn parse_response(name: &str, method: &str, result: Value) {
    match method {
        "initialize" => {
            let r: wire::InitializeResponse = serde_json::from_value(result).expect(name);
            // Recorded on 0.160.0, and the MCP approval shapes on 0.161.0: never below the floor.
            let version = agent::providers::codex::parse_user_agent_version(&r.user_agent);
            assert!(
                version.is_some_and(|v| v >= agent::providers::codex::MIN_CODEX),
                "{name}: {}",
                r.user_agent
            );
        }
        "thread/start" | "thread/resume" => {
            let r: ThreadStartResponse = serde_json::from_value(result).expect(name);
            assert!(!r.thread.id.is_empty());
            let echo = r.echo();
            assert!(echo.approval_policy.is_some() && echo.approvals_reviewer.is_some() && echo.sandbox.is_some());
        }
        "turn/start" => {
            let r: TurnStartResponse = serde_json::from_value(result).expect(name);
            assert!(!r.turn.id.is_empty());
        }
        "thread/turns/list" => {
            let r: ThreadTurnsListResponse = serde_json::from_value(result).expect(name);
            assert!(!r.data.is_empty());
        }
        _ => {}
    }
}
