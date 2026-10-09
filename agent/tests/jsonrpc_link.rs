//! `JsonRpcLink` against a scripted child (`eitri-agent-standin`).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent::jsonrpc_link::{resolve_program, Framing, Incoming, JsonRpcLink, LinkError, LinkSpec, Skipped, MAX_LINE};
use serde_json::{json, Value};

fn link(framing: Framing, script: Value) -> JsonRpcLink {
    JsonRpcLink::spawn(LinkSpec {
        program: PathBuf::from(env!("CARGO_BIN_EXE_eitri-agent-standin")),
        args: vec![OsString::from(script.to_string())],
        cwd: std::env::temp_dir(),
        framing,
        env: Vec::new(),
        env_remove: Vec::new(),
    })
    .expect("the stand-in starts")
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

fn next(link: &JsonRpcLink) -> Incoming {
    let end = soon();
    loop {
        if let Some(message) = link.try_recv() {
            return message;
        }
        assert!(Instant::now() < end, "no message from the stand-in");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only asks whether the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn request_and_response_correlate() {
    let link = link(
        Framing::WithJsonrpcMember,
        json!([
            {"expect": r#"^\{"jsonrpc":"2\.0","id":1,"method":"first","params":\{"a":1\}\}$"#},
            {"expect": r#"^\{"jsonrpc":"2\.0","id":2,"method":"second","params":\{\}\}$"#},
            // Answered out of order.
            {"write": r#"{"jsonrpc":"2.0","id":2,"result":{"which":"second"}}"#},
            {"write": r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"nope"}}"#},
        ]),
    );
    let first = link.request("first", json!({"a": 1}));
    let second = link.request("second", json!({}));
    assert_eq!((first.id(), second.id()), (1, 2));
    assert_eq!(second.wait(soon()), Ok(json!({"which": "second"})));
    assert_eq!(
        first.wait(soon()),
        Err(LinkError::Rpc {
            code: -32000,
            message: "nope".into()
        })
    );
}

#[test]
fn bare_framing_omits_the_jsonrpc_member() {
    let link = link(
        Framing::Bare,
        json!([
            {"expect": r#"^\{"id":1,"method":"ping","params":\{\}\}$"#},
            {"expect": r#"^\{"method":"initialized"\}$"#},
            {"expect": r#"^\{"id":"srv-1","result":\{"ok":true\}\}$"#},
            {"write": r#"{"id":1,"result":{"pong":true}}"#},
        ]),
    );
    let ping = link.request("ping", json!({}));
    link.notify("initialized", Value::Null);
    link.respond(json!("srv-1"), json!({"ok": true}));
    assert_eq!(ping.wait(soon()), Ok(json!({"pong": true})));
}

#[test]
fn a_line_split_across_reads_is_one_message() {
    let link = link(
        Framing::Bare,
        json!([
            {"write_raw": r#"{"method":"turn/start"#},
            {"sleep_ms": 150},
            {"write_raw": r#"ed","params":{"threadId":"t1"}}"#},
            {"sleep_ms": 100},
            {"write": ""},
        ]),
    );
    assert_eq!(
        next(&link),
        Incoming::Notification {
            method: "turn/started".into(),
            params: json!({"threadId": "t1"})
        }
    );
}

#[test]
fn non_json_line_is_dropped_and_the_link_lives() {
    let link = link(
        Framing::Bare,
        json!([
            {"write": "this is not json"},
            {"write": "[1,2,3]"},
            {"write": r#"{"neither":"method nor id"}"#},
            {"write": r#"{"method":"after","params":{}}"#},
            {"write": r#"{"id":7,"method":"srv/ask","params":{"q":1}}"#},
        ]),
    );
    assert_eq!(
        next(&link),
        Incoming::Notification {
            method: "after".into(),
            params: json!({})
        }
    );
    assert_eq!(
        next(&link),
        Incoming::Request {
            id: json!(7),
            method: "srv/ask".into(),
            params: json!({"q": 1})
        }
    );
}

#[test]
fn over_cap_server_request_is_answered_at_once() {
    let link = link(
        Framing::Bare,
        json!([
            {"write_big": {
                "prefix": r#"{"id":91,"method":"item/commandExecution/requestApproval","params":{"command":""#,
                "bytes": MAX_LINE + (1 << 20),
                "suffix": r#""}}"#,
            }},
            {"expect": r#"^\{"id":91,"result":\{"decision":"decline"\}\}$"#},
            {"write_big": {
                "prefix": r#"{"id":92,"method":"item/permissions/requestApproval","params":{"x":""#,
                "bytes": MAX_LINE + 1,
            }},
            {"expect": r#"^\{"id":92,"result":\{"permissions":\{\},"scope":"turn"\}\}$"#},
            {"write_big": {
                "prefix": r#"{"id":"s-3","method":"item/tool/call","params":{"x":""#,
                "bytes": MAX_LINE + 1,
            }},
            {"expect": r#"^\{"id":"s-3","error":\{.*"code":-32600.*\}\}$"#},
            {"write": r#"{"method":"alive","params":{}}"#},
        ]),
    );
    for method in [
        "item/commandExecution/requestApproval",
        "item/permissions/requestApproval",
        "item/tool/call",
    ] {
        assert_eq!(
            next(&link),
            Incoming::Skipped(Skipped::RequestAnswered { method: method.into() })
        );
    }
    assert_eq!(
        next(&link),
        Incoming::Notification {
            method: "alive".into(),
            params: json!({})
        }
    );
}

#[test]
fn over_cap_response_fails_only_its_request() {
    let link = link(
        Framing::Bare,
        json!([
            {"expect": r#"^\{"id":1,"method":"big""#},
            {"expect": r#"^\{"id":2,"method":"small""#},
            {"write_big": {"prefix": r#"{"id":1,"result":{"x":""#, "bytes": MAX_LINE + 1}},
            {"write": r#"{"id":2,"result":{"fine":true}}"#},
        ]),
    );
    let big = link.request("big", json!({}));
    let small = link.request("small", json!({}));
    assert_eq!(small.wait(soon()), Ok(json!({"fine": true})));
    assert_eq!(big.wait(soon()), Err(LinkError::TooLarge));
    assert_eq!(next(&link), Incoming::Skipped(Skipped::ResponseFailed { id: 1 }));
}

#[test]
fn over_cap_turn_completed_reports_turn_too_large() {
    let link = link(
        Framing::Bare,
        json!([
            {"write_big": {
                "prefix": r#"{"method":"turn/completed","params":{"threadId":"thr_1","turn":{"items":[""#,
                "bytes": MAX_LINE + 1,
            }},
            {"write_big": {
                "prefix": r#"{"method":"item/completed","params":{"item":{"type":"commandExecution","id":"it_9","output":""#,
                "bytes": MAX_LINE + 1,
            }},
            {"write_big": {"prefix": r#"{"method":"thread/tokenUsage/updated","params":{"x":""#, "bytes": MAX_LINE + 1}},
            {"write": r#"{"method":"alive","params":{}}"#},
        ]),
    );
    assert_eq!(
        next(&link),
        Incoming::Skipped(Skipped::TurnTooLarge {
            method: "turn/completed".into(),
            thread_id: Some("thr_1".into())
        })
    );
    assert_eq!(
        next(&link),
        Incoming::Skipped(Skipped::ItemTooLarge {
            method: "item/completed".into(),
            item_id: Some("it_9".into()),
            thread_id: None
        })
    );
    // An over-sized notification of any other kind is dropped, and the link goes on.
    assert_eq!(
        next(&link),
        Incoming::Notification {
            method: "alive".into(),
            params: json!({})
        }
    );
}

#[test]
fn over_cap_line_with_a_stray_brace_in_a_list_does_not_wedge_the_reader() {
    let link = link(
        Framing::Bare,
        json!([
            {"write_big": {
                "prefix": r#"{"method":"item/started","params":{"threadId":"t1","list":[1}"#,
                "bytes": MAX_LINE + 1,
            }},
            {"write": r#"{"method":"alive","params":{}}"#},
        ]),
    );
    assert_eq!(
        next(&link),
        Incoming::Skipped(Skipped::ItemTooLarge {
            method: "item/started".into(),
            item_id: None,
            thread_id: Some("t1".into())
        })
    );
    assert_eq!(
        next(&link),
        Incoming::Notification {
            method: "alive".into(),
            params: json!({})
        }
    );
}

#[test]
fn prefix_without_method_or_id_closes() {
    let link = link(
        Framing::Bare,
        json!([
            {"expect": r#"^\{"id":1,"method":"ask""#},
            {"write_big": {"prefix": r#"{"params":{"x":""#, "bytes": MAX_LINE + 1}},
            {"sleep_ms": 3000},
        ]),
    );
    let pending = link.request("ask", json!({}));
    assert!(matches!(next(&link), Incoming::Closed { .. }));
    assert_eq!(pending.wait(soon()), Err(LinkError::Closed));
    // After the end, a new request fails at once.
    assert_eq!(link.request("late", json!({})).wait(soon()), Err(LinkError::Closed));
}

#[test]
fn a_child_that_exits_closes_the_link_and_fails_pending_calls() {
    let link = link(Framing::Bare, json!([{"expect": r#"^\{"id":1,"#}, {"exit": 0}]));
    let pending = link.request("ask", json!({}));
    assert_eq!(pending.wait(soon()), Err(LinkError::Closed));
    assert!(matches!(next(&link), Incoming::Closed { .. }));
}

#[test]
fn writer_never_blocks_the_caller() {
    let link = link(Framing::Bare, json!([{"sleep_ms": 2000}]));
    let filler = "y".repeat(1024);
    let started = Instant::now();
    for _ in 0..1000 {
        link.notify("noise", json!({"filler": filler}));
    }
    let took = started.elapsed();
    assert!(took < Duration::from_millis(100), "1000 notifications took {took:?}");
    // The queue holds about 1000 lines and a pipe's worth: far more than that is refused, not waited on.
    let started = Instant::now();
    let mut refused = 0;
    for _ in 0..3000 {
        if link.request("more", json!({"filler": filler})).try_take() == Some(Err(LinkError::Busy)) {
            refused += 1;
        }
    }
    assert!(refused > 0, "a full queue should refuse");
    assert!(started.elapsed() < Duration::from_millis(500));
    link.shutdown(Duration::from_millis(100));
}

#[test]
fn shutdown_by_eof_then_sigkill() {
    // Exits on EOF.
    let polite = link(Framing::Bare, json!([]));
    let pid = polite.pid();
    assert!(alive(pid));
    let started = Instant::now();
    polite.shutdown(Duration::from_secs(5));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "EOF should end the child at once"
    );
    assert!(!alive(pid), "the child is reaped");

    // Ignores EOF: killed after the grace.
    let stubborn = link(Framing::Bare, json!([{"ignore_eof": true}]));
    let pid = stubborn.pid();
    let started = Instant::now();
    stubborn.shutdown(Duration::from_millis(300));
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(300), "the grace is waited out: {took:?}");
    assert!(took < Duration::from_secs(3));
    assert!(!alive(pid), "the child is killed and reaped");
}

#[test]
fn stderr_tail_redacts_emails() {
    let link = link(
        Framing::Bare,
        json!([{"stderr": "login failed for someone.name+tag@example.com, retry"}, {"stderr": "plain line"}]),
    );
    let end = soon();
    let tail = loop {
        let tail = link.stderr_tail();
        if tail.contains("plain line") {
            break tail;
        }
        assert!(Instant::now() < end, "no stderr: {tail:?}");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(tail.contains("login failed for <email>, retry"), "{tail:?}");
    assert!(!tail.contains("example.com") && !tail.contains("someone"), "{tail:?}");
}

#[test]
fn resolve_program_skips_relative_path_entries() {
    let files = |paths: &'static [&'static str]| move |p: &Path| paths.contains(&p.to_str().unwrap_or(""));
    let path = OsString::from("::relative/bin:./here:/usr/local/bin:/usr/bin");

    // The relative and empty entries hold a match, and are skipped.
    let found = resolve_program(
        OsStr::new("codex"),
        Some(&path),
        files(&["relative/bin/codex", "./here/codex", "codex", "/usr/bin/codex"]),
    );
    assert_eq!(found, Some(PathBuf::from("/usr/bin/codex")));

    // The first absolute entry wins.
    let found = resolve_program(
        OsStr::new("codex"),
        Some(&path),
        files(&["/usr/local/bin/codex", "/usr/bin/codex"]),
    );
    assert_eq!(found, Some(PathBuf::from("/usr/local/bin/codex")));

    // Only relative entries hold it: nothing.
    assert_eq!(
        resolve_program(
            OsStr::new("codex"),
            Some(&path),
            files(&["relative/bin/codex", "./here/codex"])
        ),
        None
    );
    // An absolute name stands as it is; a relative path with a slash is not looked up; no PATH, no program.
    assert_eq!(
        resolve_program(OsStr::new("/opt/codex"), None, files(&[])),
        Some(PathBuf::from("/opt/codex"))
    );
    assert_eq!(
        resolve_program(OsStr::new("./codex"), Some(&path), files(&["/usr/bin/./codex"])),
        None
    );
    assert_eq!(
        resolve_program(OsStr::new("codex"), None, files(&["/usr/bin/codex"])),
        None
    );
}

#[test]
fn shutdown_kills_the_children_the_agent_started_too() {
    let link = JsonRpcLink::spawn(LinkSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from("sleep 300 & echo $! >&2; wait")],
        cwd: std::env::temp_dir(),
        framing: Framing::Bare,
        env: Vec::new(),
        env_remove: Vec::new(),
    })
    .expect("sh starts");
    let end = soon();
    let grandchild: i32 = loop {
        if let Ok(pid) = link.stderr_tail().trim().parse() {
            break pid;
        }
        assert!(Instant::now() < end, "the shell never named its child");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(alive(grandchild as u32));
    link.shutdown(Duration::ZERO);
    let end = Instant::now() + Duration::from_secs(2);
    let mut gone = false;
    while Instant::now() < end {
        // A killed child that nobody waits for stays a zombie until init reaps it, and signal 0
        // still reaches a zombie, so read its state.
        gone = std::fs::read_to_string(format!("/proc/{grandchild}/stat")).map_or(true, |stat| {
            stat.rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z'))
        });
        if gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: cleanup of a process this test started, by its captured pid.
    unsafe { libc::kill(grandchild, libc::SIGKILL) };
    assert!(gone, "the agent's own child outlived the link");
}
