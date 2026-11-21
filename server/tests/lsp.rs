//! The daemon's language servers: `lsp::Servers` driven directly with a scripted language server
//! (`core/tests/fixtures/lsp/scripted-server`), the `Lsp` service end to end over a socket for the round trip and an
//! attach to a server that is gone, and a fixture daemon (this test binary's `fixture_daemon_entry`) for idleness and
//! SIGTERM.
#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
mod lsp_support;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use daemon::login_env::{Captured, Environment};
use eitri_server_proto::lsp_client::LspClient;
use eitri_server_proto::lsp_client_frame::Frame as ClientFrame;
use eitri_server_proto::lsp_server_frame::Frame;
use eitri_server_proto::*;
use lsp_support::*;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tonic::transport::Channel;

const REVISION: &str = env!("EITRI_BUILD_COMMIT");

/// The daemon's sources, included above, name this crate-root helper.
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

#[tokio::test]
async fn open_starts_in_the_root_with_the_login_path() {
    let fixture = Fixture::new();
    let opened = fixture.open(&[]).await;
    let started = &lines(&opened.received)[0]["started"];
    assert_eq!(started["cwd"], json!(fixture.path("project").to_str().unwrap()));
    assert_eq!(started["path"], json!(login_path(&fixture.base).to_str().unwrap()));
    assert_eq!(
        opened.response.resolved_command,
        fixture.path("bin/scripted-lsp").to_str().unwrap()
    );
    assert!(fixture.audit().contains(&format!(
        "lsp-start scripted-lsp root={} id={}",
        fixture.path("project").display(),
        opened.id
    )));
    let log = fixture.servers.log_dir().join(format!("{}.log", opened.id));
    assert_eq!(mode(&log), 0o600);
    assert_eq!(mode(&fixture.servers.log_dir()), 0o700);
    let listed = fixture.info(&opened.id).unwrap();
    assert_eq!(listed.root, fixture.path("project").to_str().unwrap());
    assert_eq!(listed.command, opened.response.resolved_command);
    assert_eq!(listed.pid as i32, opened.pid);
    assert!(!listed.attached);
    fixture.servers.stop(&opened.id).await;
    assert!(fixture.info(&opened.id).is_none());
    assert!(ended(opened.pid));
    assert!(fixture.audit().contains(&format!("lsp-end {} stopped", opened.id)));
}

#[tokio::test]
async fn a_missing_command_is_named_not_started() {
    let fixture = Fixture::new();
    let (request, received) = fixture.request("no-such-language-server", &[], "client-a");
    let response = fixture.servers.open(request).await.unwrap();
    assert_eq!(response.missing_command, "no-such-language-server");
    assert!(response.server_id.is_empty() && response.refusal.is_empty());
    assert!(fixture.servers.list().servers.is_empty());
    assert!(!received.exists());
    let (mut request, _) = fixture.request("scripted-lsp", &[], "client-a");
    request.root = "project/relative".into();
    let response = fixture.servers.open(request).await.unwrap();
    assert!(response.refusal.contains("absolute"), "{response:?}");
    let (mut request, _) = fixture.request("scripted-lsp", &[], "client-a");
    request.root = format!("/{}", "r".repeat(LSP_PATH_MAX));
    let response = fixture.servers.open(request).await.unwrap();
    assert!(response.refusal.contains("longer"), "{response:?}");
    assert!(fixture.servers.list().servers.is_empty());
}

#[tokio::test]
async fn a_mason_path_is_resolved_by_name_on_the_server() {
    let fixture = Fixture::new();
    let (request, received) = fixture.request(
        "/home/laptop-user/.local/share/nvim/mason/bin/scripted-lsp",
        &[],
        "client-a",
    );
    let opened = fixture.open_request(request, received).await;
    assert_eq!(
        opened.response.resolved_command,
        fixture.path("bin/scripted-lsp").to_str().unwrap()
    );
    let mason = fixture.path("home/.local/share/nvim/mason/bin");
    std::fs::create_dir_all(&mason).unwrap();
    daemon::test_exec::install_executable(Path::new(SCRIPTED), &mason.join("mason-only-lsp"));
    let (request, received) = fixture.request("mason-only-lsp", &[], "client-b");
    let from_mason = fixture.open_request(request, received).await;
    assert_eq!(
        from_mason.response.resolved_command,
        mason.join("mason-only-lsp").to_str().unwrap()
    );
    fixture.servers.stop_all().await;
    assert!(!fixture.servers.any_live());
}

/// A tonic client of the `Lsp` service served over a socket in the fixture.
async fn lsp_client(fixture: &Fixture) -> (LspClient<Channel>, tokio::task::JoinHandle<()>) {
    let socket = fixture.path(&format!("{}.sock", fixture.unique("l")));
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let incoming = futures_util::stream::unfold(listener, |listener| async move {
        Some((listener.accept().await.map(|(stream, _)| stream), listener))
    });
    let service = daemon::serve::lsp_server(fixture.servers.clone());
    let server = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await;
    });
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(socket).await?,
                ))
            }
        }))
        .await
        .unwrap();
    (LspClient::new(channel), server)
}

fn hello(server_id: &str) -> LspClientFrame {
    LspClientFrame {
        frame: Some(ClientFrame::Hello(LspAttachHello {
            server_id: server_id.into(),
            client_id: "client-a".into(),
        })),
    }
}

#[tokio::test]
async fn messages_round_trip_in_chunks() {
    let fixture = Fixture::new();
    let (mut client, server) = lsp_client(&fixture).await;
    let (request_message, received) = fixture.request("scripted-lsp", &[], "client-a");
    let opened = client.open_server(request_message).await.unwrap().into_inner();
    assert!(!opened.server_id.is_empty(), "{opened:?}");
    let (input, receiver) = mpsc::channel(8);
    input.send(hello(&opened.server_id)).await.unwrap();
    let mut frames = client
        .attach(tokio_stream::wrappers::ReceiverStream::new(receiver))
        .await
        .unwrap()
        .into_inner();
    let text = "y".repeat(5 * LSP_CHUNK_MAX / 2);
    let bytes = serde_json::to_vec(&request(1, "test/echo", json!({ "text": text }))).unwrap();
    for chunk in chunks(&bytes) {
        input
            .send(LspClientFrame {
                frame: Some(ClientFrame::Chunk(chunk)),
            })
            .await
            .unwrap();
    }
    match frames.message().await.unwrap().unwrap().frame.unwrap() {
        Frame::Attached(attached) => assert!(!attached.reattached),
        _ => panic!("the first frame is LspAttached"),
    }
    let (mut data, mut pieces) = (Vec::new(), 0);
    loop {
        match frames.message().await.unwrap().unwrap().frame.unwrap() {
            Frame::Chunk(chunk) => {
                assert!(chunk.data.len() <= LSP_CHUNK_MAX);
                pieces += 1;
                data.extend(chunk.data);
                if chunk.last {
                    break;
                }
            }
            _ => panic!("expected the answer's chunks"),
        }
    }
    assert!(pieces >= 3, "{pieces}");
    let answer: Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(answer["id"], json!(1));
    assert_eq!(answer["result"]["method"], json!("test/echo"));
    assert_eq!(answer["result"]["params"]["text"].as_str().unwrap().len(), text.len());
    let seen = until_received(&received, method_is("test/echo")).await;
    assert_eq!(seen["params"]["text"].as_str().unwrap().len(), text.len());
    client
        .stop_server(StopServerRequest {
            server_id: opened.server_id.clone(),
            command_id: "stop-1".into(),
        })
        .await
        .unwrap();
    match frames.message().await.unwrap().unwrap().frame.unwrap() {
        Frame::Ended(ended) => assert_eq!(ended.reason, LspEndReason::Stopped as i32),
        _ => panic!("expected LspEnded"),
    }
    let refused = client
        .read_path(ReadPathRequest {
            path: "/etc".into(),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();
    // `ReadPath` is served (`read_path.rs` has its own tests): a directory is refused in one line.
    assert!(refused.refusal.contains("is a directory"), "{refused:?}");
    server.abort();
}

#[tokio::test]
async fn an_attach_to_an_unknown_server_is_told_it_is_gone() {
    let fixture = Fixture::new();
    let (mut client, server) = lsp_client(&fixture).await;
    let (input, receiver) = mpsc::channel(2);
    input.send(hello("0123456789abcdef0123456789abcdef")).await.unwrap();
    let mut frames = client
        .attach(tokio_stream::wrappers::ReceiverStream::new(receiver))
        .await
        .unwrap()
        .into_inner();
    match frames.message().await.unwrap().unwrap().frame.unwrap() {
        Frame::Ended(ended) => assert_eq!(ended.reason, LspEndReason::Gone as i32),
        _ => panic!("expected LspEnded"),
    }
    assert!(frames.message().await.unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn a_message_over_the_cap_fails_its_request_and_the_stream_goes_on() {
    let fixture = Fixture::new();
    let over = LSP_MESSAGE_MAX + 16;
    let opened = fixture
        .open(&[
            json!({"recv": "test/big"}),
            json!({"send_big": {"id": 1, "bytes": over}}),
            json!({"send_big": {"method": "test/big-note", "bytes": over}}),
            json!({"send": notification("test/after", json!({}))}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    bridge.send(request(1, "test/big", json!({}))).await;
    bridge.send(request(2, "test/echo", json!({}))).await;
    let failed = bridge.message().await;
    assert_eq!(failed["id"], json!(1));
    assert_eq!(failed["error"]["code"], json!(-32803));
    assert_eq!(bridge.message().await["method"], json!("test/after"));
    assert_eq!(bridge.message().await["result"]["method"], json!("test/echo"));
    // The editor's direction: a request past the cap is answered with an error, and the next one goes through.
    let big = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"test/echo","params":{{"pad":"{}"}}}}"#,
        "z".repeat(over)
    );
    bridge.send_bytes(big.as_bytes()).await;
    bridge.send(request(4, "test/echo", json!({}))).await;
    // The helper's error and the server's echo travel different ways; either may come first.
    let mut answers = vec![bridge.message().await, bridge.message().await];
    answers.sort_by_key(|answer| answer["id"].as_i64());
    assert_eq!(answers[0]["id"], json!(3));
    assert_eq!(answers[0]["error"]["code"], json!(-32803));
    assert_eq!(answers[1]["id"], json!(4));
    assert_eq!(answers[1]["result"]["method"], json!("test/echo"));
    assert!(!lines(&opened.received).iter().any(response_to(3)));
    let log = std::fs::read_to_string(fixture.servers.log_dir().join(format!("{}.log", opened.id))).unwrap();
    assert!(log.contains("dropped a message from the server"), "{log}");
    assert!(log.contains("dropped a message from the bridge"), "{log}");
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detaching_cancels_in_flight_requests_and_drops_their_answers() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let opened = fixture
        .open(&[
            json!({"wait_file": go}),
            json!({"send": request(9, "custom/ask", json!({}))}),
            json!({"recv": "test/slow"}),
            json!({"recv": "$/cancelRequest"}),
            json!({"send": {"jsonrpc": "2.0", "id": 5, "result": "late"}}),
            json!({"send": notification("test/marker", json!({}))}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    std::fs::write(&go, "").unwrap();
    assert_eq!(bridge.message().await["id"], json!(9));
    bridge.send(request(5, "test/slow", json!({}))).await;
    until_received(&opened.received, method_is("test/slow")).await;
    drop(bridge);
    let cancel = until_received(&opened.received, method_is("$/cancelRequest")).await;
    assert_eq!(cancel["params"]["id"], json!(5));
    let answered = until_received(&opened.received, response_to(9)).await;
    assert_eq!(answered["error"]["code"], json!(-32803));
    let mut again = fixture.attach(&opened.id).await;
    let attached = again.attached().await;
    assert!(attached.reattached);
    assert_eq!(attached.requests_answered, 1);
    // The server's late answer to the cancelled request never reaches the editor.
    assert_eq!(again.message().await["method"], json!("test/marker"));
    // The editor's late answer to the request the helper answered never reaches the server.
    again.send(json!({"jsonrpc": "2.0", "id": 9, "result": "late"})).await;
    again.send(request(6, "test/echo", json!({}))).await;
    assert_eq!(again.message().await["id"], json!(6));
    until_received(&opened.received, method_is("test/echo")).await;
    let answers: Vec<_> = lines(&opened.received).into_iter().filter(response_to(9)).collect();
    assert_eq!(answers.len(), 1, "{answers:?}");
    fixture.servers.stop(&opened.id).await;
}

/// Opens a server that, once told, sends each of `requests` while no bridge is attached; answers what it got back,
/// by id.
async fn detached_answers(fixture: &Fixture, requests: &[(&str, Value)]) -> (Opened, Vec<Value>) {
    let go = fixture.file("go");
    let mut steps = vec![json!({"wait_file": go})];
    for (at, (method, params)) in requests.iter().enumerate() {
        steps.push(json!({"send": request(at as i64 + 1, method, params.clone())}));
    }
    for at in 0..requests.len() {
        steps.push(json!({"recv_id": at + 1}));
    }
    let opened = fixture.open(&steps).await;
    std::fs::write(&go, "").unwrap();
    let mut answers = Vec::new();
    for at in 0..requests.len() {
        answers.push(until_received(&opened.received, response_to(at as i64 + 1)).await);
    }
    (opened, answers)
}

fn assert_not_connected(answer: &Value) {
    assert_eq!(answer["error"]["code"], json!(-32803), "{answer}");
    assert_eq!(answer["error"]["message"], json!("the editor is not connected"));
}

#[tokio::test]
async fn detached_workspace_configuration_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let (attached_go, go) = (fixture.file("go"), fixture.file("go"));
    let asked = json!({"items": [{"section": "rust-analyzer", "scopeUri": "file:///srv/project"}]});
    let reordered = json!({"items": [{"scopeUri": "file:///srv/project", "section": "rust-analyzer"}]});
    let opened = fixture
        .open(&[
            json!({"wait_file": attached_go}),
            json!({"send": request(100, "workspace/configuration", asked.clone())}),
            json!({"recv_id": 100}),
            json!({"wait_file": go}),
            json!({"send": request(1, "workspace/configuration", reordered)}),
            json!({"send": request(2, "workspace/configuration", json!({"items": [{"section": "other"}]}))}),
            json!({"recv_id": 1}),
            json!({"recv_id": 2}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    std::fs::write(&attached_go, "").unwrap();
    assert_eq!(bridge.message().await["id"], json!(100));
    bridge
        .send(json!({"jsonrpc": "2.0", "id": 100, "result": [{"cargo": {"features": "all"}}]}))
        .await;
    until_received(&opened.received, response_to(100)).await;
    drop(bridge);
    fixture.until_detached(&opened.id).await;
    std::fs::write(&go, "").unwrap();
    let remembered = until_received(&opened.received, response_to(1)).await;
    assert_eq!(remembered["result"], json!([{"cargo": {"features": "all"}}]));
    assert_not_connected(&until_received(&opened.received, response_to(2)).await);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_workspace_folders_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let (first, second, third) = (fixture.file("go"), fixture.file("go"), fixture.file("go"));
    let opened = fixture
        .open(&[
            json!({"wait_file": first}),
            json!({"send": request(1, "workspace/workspaceFolders", Value::Null)}),
            json!({"recv_id": 1}),
            json!({"wait_file": second}),
            json!({"send": request(100, "workspace/workspaceFolders", Value::Null)}),
            json!({"recv_id": 100}),
            json!({"wait_file": third}),
            json!({"send": request(2, "workspace/workspaceFolders", Value::Null)}),
            json!({"recv_id": 2}),
        ])
        .await;
    std::fs::write(&first, "").unwrap();
    let none_yet = until_received(&opened.received, response_to(1)).await;
    assert_eq!(none_yet["result"], Value::Null);
    assert!(none_yet.get("error").is_none());
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    std::fs::write(&second, "").unwrap();
    assert_eq!(bridge.message().await["id"], json!(100));
    let folders = json!([{"uri": "file:///srv/project", "name": "project"}]);
    bridge
        .send(json!({"jsonrpc": "2.0", "id": 100, "result": folders.clone()}))
        .await;
    until_received(&opened.received, response_to(100)).await;
    drop(bridge);
    fixture.until_detached(&opened.id).await;
    std::fs::write(&third, "").unwrap();
    assert_eq!(
        until_received(&opened.received, response_to(2)).await["result"],
        folders
    );
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_register_capability_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let register = json!({"registrations": [{"id": "w1", "method": "workspace/didChangeWatchedFiles"}]});
    let unregister = json!({"unregisterations": [{"id": "w1", "method": "workspace/didChangeWatchedFiles"}]});
    let (opened, answers) = detached_answers(
        &fixture,
        &[
            ("client/registerCapability", register.clone()),
            ("client/unregisterCapability", unregister.clone()),
        ],
    )
    .await;
    for answer in &answers {
        assert_eq!(answer["result"], Value::Null);
        assert!(answer.get("error").is_none(), "{answer}");
    }
    let mut bridge = fixture.attach(&opened.id).await;
    let attached = bridge.attached().await;
    assert!(!attached.reattached);
    assert_eq!(attached.requests_answered, 2);
    let kept: Vec<(String, Value)> = attached
        .registrations
        .iter()
        .map(|kept| (kept.method.clone(), serde_json::from_slice(&kept.params).unwrap()))
        .collect();
    assert_eq!(
        kept,
        [
            ("client/registerCapability".to_owned(), register),
            ("client/unregisterCapability".to_owned(), unregister)
        ]
    );
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_work_done_progress_create_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let (opened, answers) =
        detached_answers(&fixture, &[("window/workDoneProgress/create", json!({"token": "t"}))]).await;
    assert_eq!(answers[0]["result"], Value::Null);
    assert!(answers[0].get("error").is_none());
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_apply_edit_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let edit = json!({"edit": {"changes": {"file:///srv/project/src/lib.rs": []}}});
    let (opened, answers) = detached_answers(&fixture, &[("workspace/applyEdit", edit)]).await;
    assert_eq!(
        answers[0]["result"],
        json!({"applied": false, "failureReason": "the editor is not connected"})
    );
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_show_message_request_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let asked = json!({"type": 3, "message": "Reload?", "actions": [{"title": "Yes"}]});
    let (opened, answers) = detached_answers(&fixture, &[("window/showMessageRequest", asked)]).await;
    assert_eq!(answers[0]["result"], Value::Null);
    assert!(answers[0].get("error").is_none());
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_show_document_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let shown = json!({"uri": "file:///srv/project/src/lib.rs", "takeFocus": true});
    let (opened, answers) = detached_answers(&fixture, &[("window/showDocument", shown)]).await;
    assert_eq!(answers[0]["result"], json!({"success": false}));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_refresh_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let refreshes: Vec<_> = [
        "workspace/semanticTokens/refresh",
        "workspace/inlayHint/refresh",
        "workspace/codeLens/refresh",
        "workspace/diagnostic/refresh",
        "workspace/inlineValue/refresh",
        "workspace/foldingRange/refresh",
    ]
    .into_iter()
    .map(|method| (method, Value::Null))
    .collect();
    let (opened, answers) = detached_answers(&fixture, &refreshes).await;
    for answer in &answers {
        assert_eq!(answer["result"], Value::Null, "{answer}");
        assert!(answer.get("error").is_none(), "{answer}");
    }
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn detached_unknown_method_is_answered_as_the_table_says() {
    let fixture = Fixture::new();
    let (opened, answers) = detached_answers(&fixture, &[("experimental/serverStatus", json!({}))]).await;
    assert_not_connected(&answers[0]);
    fixture.servers.stop(&opened.id).await;
}

fn diagnostics(uri: &str, version: u32) -> Value {
    notification(
        "textDocument/publishDiagnostics",
        json!({"uri": uri, "version": version, "diagnostics": []}),
    )
}

fn progress(token: &str, kind: &str) -> Value {
    notification("$/progress", json!({"token": token, "value": {"kind": kind}}))
}

/// A request the helper answers while detached; once the server has its answer, everything the server sent before
/// it has been through the helper.
fn barrier() -> [Value; 2] {
    [
        json!({"send": request(50, "window/workDoneProgress/create", json!({"token": "barrier"}))}),
        json!({"recv_id": 50}),
    ]
}

#[tokio::test]
async fn diagnostics_and_progress_collapse_while_detached() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let log = notification("window/logMessage", json!({"type": 4, "message": "indexing"}));
    let mut steps = vec![json!({"wait_file": go})];
    for message in [
        diagnostics("file:///srv/project/a.rs", 1),
        diagnostics("file:///srv/project/b.rs", 1),
        progress("index", "begin"),
        diagnostics("file:///srv/project/a.rs", 2),
        progress("index", "report"),
        log.clone(),
        progress("index", "end"),
    ] {
        steps.push(json!({ "send": message }));
    }
    steps.extend(barrier());
    let opened = fixture.open(&steps).await;
    std::fs::write(&go, "").unwrap();
    until_received(&opened.received, response_to(50)).await;
    let mut bridge = fixture.attach(&opened.id).await;
    let attached = bridge.attached().await;
    assert_eq!(attached.notifications_dropped, 3);
    assert_eq!(attached.requests_answered, 1);
    for expected in [
        diagnostics("file:///srv/project/b.rs", 1),
        diagnostics("file:///srv/project/a.rs", 2),
        log,
        progress("index", "end"),
    ] {
        assert_eq!(bridge.message().await, expected);
    }
    bridge.send(request(7, "test/echo", json!({}))).await;
    assert_eq!(bridge.message().await["id"], json!(7));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn the_queue_over_its_cap_stops_the_server() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let mut steps = vec![json!({"wait_file": go})];
    let piece = 6 * 1024 * 1024;
    for _ in 0..LSP_DETACHED_QUEUE_MAX / piece + 1 {
        steps.push(json!({"send_big": {"method": "test/noise", "bytes": piece}}));
    }
    steps.push(json!({"sleep": 60}));
    let opened = fixture.open(&steps).await;
    std::fs::write(&go, "").unwrap();
    fixture.until_gone(&opened.id).await;
    assert!(ended(opened.pid));
    assert!(fixture.audit().contains(&format!("lsp-end {} queue-full", opened.id)));
    let log = std::fs::read_to_string(fixture.servers.log_dir().join(format!("{}.log", opened.id))).unwrap();
    assert!(log.contains("queued for the editor"), "{log}");
    let mut bridge = fixture.attach(&opened.id).await;
    assert!(bridge.sink.is_none());
    assert_eq!(bridge.ended().await.reason, LspEndReason::Gone as i32);
}

#[tokio::test]
async fn reattach_delivers_attached_then_the_queue_in_order() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let register = json!({"registrations": [{"id": "r1", "method": "textDocument/didSave"}]});
    let mut steps = vec![
        json!({"recv": "test/first"}),
        json!({"wait_file": go}),
        json!({"send": notification("test/one", json!({}))}),
        json!({"send": notification("test/two", json!({}))}),
        json!({"send": request(20, "client/registerCapability", register.clone())}),
    ];
    steps.extend(barrier());
    let opened = fixture.open(&steps).await;
    let mut bridge = fixture.attach(&opened.id).await;
    assert!(!bridge.attached().await.reattached);
    bridge.send(notification("test/first", json!({}))).await;
    until_received(&opened.received, method_is("test/first")).await;
    drop(bridge);
    fixture.until_detached(&opened.id).await;
    std::fs::write(&go, "").unwrap();
    until_received(&opened.received, response_to(50)).await;
    assert!(fixture.info(&opened.id).unwrap().queued_bytes > 0);
    let mut again = fixture.attach(&opened.id).await;
    let attached = again.attached().await;
    assert!(attached.reattached);
    assert_eq!(attached.requests_answered, 2);
    assert_eq!(attached.registrations.len(), 1);
    assert_eq!(attached.registrations[0].method, "client/registerCapability");
    assert_eq!(
        serde_json::from_slice::<Value>(&attached.registrations[0].params).unwrap(),
        register
    );
    assert_eq!(again.message().await["method"], json!("test/one"));
    assert_eq!(again.message().await["method"], json!("test/two"));
    again.send(request(3, "test/echo", json!({}))).await;
    assert_eq!(again.message().await["id"], json!(3));
    assert_eq!(fixture.info(&opened.id).unwrap().queued_bytes, 0);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn grace_expiry_stops_the_server_and_says_why() {
    let fixture = Fixture::new();
    let (mut request, received) = fixture.request("scripted-lsp", &[], "client-a");
    // Clamped up to this fixture's shortest grace when shorter; long enough to attach once first.
    request.grace_ms = 3000;
    let opened = fixture.open_request(request, received).await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    drop(bridge);
    fixture.until_gone(&opened.id).await;
    assert!(ended(opened.pid));
    assert!(fixture.audit().contains(&format!("lsp-end {} grace", opened.id)));
    let mut late = fixture.attach(&opened.id).await;
    assert_eq!(late.ended().await.reason, LspEndReason::Gone as i32);
}

#[tokio::test]
async fn a_second_open_gets_its_own_server() {
    let fixture = Fixture::new();
    let (first_request, received) = fixture.request("scripted-lsp", &[], "client-a");
    let retry = first_request.clone();
    let first = fixture.open_request(first_request, received).await;
    let mut bridge = fixture.attach(&first.id).await;
    bridge.attached().await;
    // The same editor opening again while it is attached is refused.
    let (same_client, _) = fixture.request("scripted-lsp", &[], "client-a");
    let refused = fixture.servers.open(same_client).await.unwrap();
    assert!(refused.refusal.contains("already open"), "{refused:?}");
    // Another editor gets a server of its own.
    let (other_client, received) = fixture.request("scripted-lsp", &[], "client-b");
    let second = fixture.open_request(other_client, received).await;
    assert_ne!(second.id, first.id);
    assert_ne!(second.pid, first.pid);
    // A retried open is the same server, not another.
    let retried = fixture.servers.open(retry).await.unwrap();
    assert_eq!(retried.server_id, first.id);
    assert_eq!(fixture.servers.list().servers.len(), 2);
    // A detached editor's next open is a new server; the old one lives on until its grace ends.
    drop(bridge);
    fixture.until_detached(&first.id).await;
    let (after_detach, received) = fixture.request("scripted-lsp", &[], "client-a");
    let third = fixture.open_request(after_detach, received).await;
    assert_ne!(third.id, first.id);
    assert_eq!(fixture.servers.list().servers.len(), 3);
    fixture.servers.stop_all().await;
    assert!(fixture.servers.list().servers.is_empty());
}

#[tokio::test]
async fn a_dying_server_sends_ended_with_its_stderr_tail() {
    let fixture = Fixture::new();
    let opened = fixture
        .open(&[
            json!({"recv": "test/die"}),
            json!({"stderr": "thread 'main' panicked at src/main.rs:1:1:\nboom\n"}),
            json!({"exit": 3}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    bridge.send(notification("test/die", json!({}))).await;
    let ended = bridge.ended().await;
    assert_eq!(ended.reason, LspEndReason::Exited as i32);
    assert_eq!(ended.code, Some(3));
    assert_eq!(ended.signal, None);
    let tail = String::from_utf8(ended.stderr_tail).unwrap();
    assert!(tail.contains("panicked at src/main.rs:1:1:\nboom"), "{tail:?}");
    assert!(fixture
        .audit()
        .contains(&format!("lsp-end {} exited exit=3", opened.id)));
    let log = fixture.servers.log_dir().join(format!("{}.log", opened.id));
    assert!(std::fs::read_to_string(&log).unwrap().contains("boom"));
    assert_eq!(mode(&log), 0o600);
    assert!(fixture.info(&opened.id).is_none());
}

const PENDING_MAX: i64 = daemon::lsp::PENDING_MAX as i64;

#[tokio::test]
async fn a_bridge_past_its_unanswered_bound_still_gets_answers() {
    let fixture = Fixture::new();
    let opened = fixture.open(&[json!({"recv": "test/go"})]).await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    // The server reads these and answers none of them.
    for id in 1..=PENDING_MAX + 1 {
        bridge.send(request(id, "test/ignored", json!({}))).await;
    }
    bridge.send(request(100_000, "test/go", json!({}))).await;
    bridge.send(request(100_001, "test/echo", json!({}))).await;
    assert_eq!(bridge.message().await["id"], json!(100_001));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn the_oldest_server_request_past_the_bound_is_answered_by_the_table() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let count = PENDING_MAX + 1;
    let opened = fixture
        .open(&[
            json!({"wait_file": go}),
            json!({"send_many": {"method": "window/workDoneProgress/create", "first_id": 1, "count": count}}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    std::fs::write(&go, "").unwrap();
    for _ in 0..count {
        bridge.message().await;
    }
    // The bridge answers none of them: past the bound the oldest is answered at once, the rest at the detach.
    let oldest = until_received(&opened.received, response_to(1)).await;
    assert_eq!(oldest["result"], Value::Null);
    drop(bridge);
    let newest = until_received(&opened.received, response_to(count)).await;
    assert_eq!(newest["result"], Value::Null);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn pending_configuration_keys_past_their_byte_budget_are_answered_as_unknown() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    // Each key is close to the largest kept; 70 of them are more than the keys' budget holds.
    let count = 70;
    let params = json!({"items": [{"section": "x".repeat(64_000)}]});
    let opened = fixture
        .open(&[
            json!({"wait_file": go}),
            json!({"send_many": {"method": "workspace/configuration", "first_id": 1, "count": count, "params": params}}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    std::fs::write(&go, "").unwrap();
    for _ in 0..count {
        bridge.message().await;
    }
    bridge
        .send(json!({"jsonrpc": "2.0", "id": 1, "result": [{"answered": true}]}))
        .await;
    until_received(&opened.received, response_to(1)).await;
    drop(bridge);
    // At the detach, a request that kept its key gets the editor's answer; one past the budget kept none.
    let within = until_received(&opened.received, response_to(2)).await;
    assert_eq!(within["result"], json!([{"answered": true}]));
    assert_not_connected(&until_received(&opened.received, response_to(count)).await);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_bridge_request_with_an_overlong_id_is_failed_not_forwarded() {
    let fixture = Fixture::new();
    let opened = fixture.open(&[]).await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    let long = "i".repeat(daemon::lsp::message::ID_MAX);
    bridge
        .send(json!({"jsonrpc": "2.0", "id": long, "method": "test/echo", "params": {}}))
        .await;
    let answer = bridge.message().await;
    assert_eq!(answer["id"], json!(long));
    assert_eq!(answer["error"]["code"], json!(-32803), "{answer}");
    bridge.send(request(7, "test/echo", json!({}))).await;
    assert_eq!(bridge.message().await["id"], json!(7));
    assert!(!lines(&opened.received)
        .iter()
        .any(|line| line.get("id") == Some(&json!(long))));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_server_request_with_an_overlong_id_is_failed_not_forwarded() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let long = "i".repeat(daemon::lsp::message::ID_MAX);
    let opened = fixture
        .open(&[
            json!({"wait_file": go}),
            json!({"send": {"jsonrpc": "2.0", "id": long, "method": "custom/ask", "params": {}}}),
            json!({"recv_id": long}),
            json!({"send": notification("test/after", json!({}))}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    std::fs::write(&go, "").unwrap();
    assert_eq!(bridge.message().await["method"], json!("test/after"));
    let answer = until_received(&opened.received, |line| {
        line.get("method").is_none() && line.get("id") == Some(&json!(long))
    })
    .await;
    assert_eq!(answer["error"]["code"], json!(-32803), "{answer}");
    fixture.servers.stop(&opened.id).await;
}

/// A server that, once told, queues `count` notifications `test/<n>` and a registration while no bridge is attached.
async fn queued_for_the_next_attach(fixture: &Fixture, count: usize) -> (Opened, Value) {
    let go = fixture.file("go");
    let register = json!({"registrations": [{"id": "r1", "method": "textDocument/didSave"}]});
    let mut steps = vec![json!({"wait_file": go})];
    for n in 0..count {
        steps.push(json!({"send": notification(&format!("test/{n}"), json!({}))}));
    }
    steps.push(json!({"send": request(20, "client/registerCapability", register.clone())}));
    steps.extend(barrier());
    let opened = fixture.open(&steps).await;
    std::fs::write(&go, "").unwrap();
    until_received(&opened.received, response_to(50)).await;
    (opened, register)
}

async fn assert_whole_replay(bridge: &mut Bridge, count: usize, register: &Value) {
    let attached = bridge.attached().await;
    assert!(!attached.reattached);
    assert_eq!(attached.requests_answered, 2);
    assert_eq!(attached.registrations.len(), 1);
    assert_eq!(
        &serde_json::from_slice::<Value>(&attached.registrations[0].params).unwrap(),
        register
    );
    for n in 0..count {
        assert_eq!(bridge.message().await["method"], json!(format!("test/{n}")));
    }
}

#[tokio::test]
async fn a_bridge_lost_during_the_replay_leaves_it_for_the_next() {
    let fixture = Fixture::new();
    let count = 20;
    let (opened, register) = queued_for_the_next_attach(&fixture, count).await;
    // The bridge takes nothing; once its frames are full it goes, in the middle of the replay.
    let lost = fixture.servers.attach(&opened.id, "client-a").await;
    until(|| lost.frames.len() == lost.frames.max_capacity()).await;
    drop(lost);
    let mut again = fixture.attach(&opened.id).await;
    assert_whole_replay(&mut again, count, &register).await;
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_bridge_that_never_spoke_hands_its_replay_to_the_next() {
    let fixture = Fixture::new();
    let count = 3;
    let (opened, register) = queued_for_the_next_attach(&fixture, count).await;
    let mut silent = fixture.attach(&opened.id).await;
    assert_whole_replay(&mut silent, count, &register).await;
    drop(silent);
    let mut spoke = fixture.attach(&opened.id).await;
    assert_whole_replay(&mut spoke, count, &register).await;
    spoke.send(request(7, "test/echo", json!({}))).await;
    assert_eq!(spoke.message().await["id"], json!(7));
    drop(spoke);
    // A bridge that sent a message had read what it was handed: the next gets none of it again.
    let mut next = fixture.attach(&opened.id).await;
    let attached = next.attached().await;
    assert!(attached.reattached);
    assert_eq!(attached.requests_answered, 0);
    assert!(attached.registrations.is_empty());
    next.send(request(8, "test/echo", json!({}))).await;
    assert_eq!(next.message().await["id"], json!(8));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn live_news_to_a_bridge_that_never_spoke_is_handed_to_the_next() {
    let fixture = Fixture::new();
    let (go, later) = (fixture.file("go"), fixture.file("later"));
    let uri = "file:///srv/project/a.rs";
    let mut steps = vec![json!({"wait_file": go}), json!({"send": diagnostics(uri, 1)})];
    steps.extend(barrier());
    steps.push(json!({"wait_file": later}));
    steps.push(json!({"send": diagnostics(uri, 2)}));
    steps.push(json!({"send": progress("index", "begin")}));
    let opened = fixture.open(&steps).await;
    std::fs::write(&go, "").unwrap();
    until_received(&opened.received, response_to(50)).await;
    let mut silent = fixture.attach(&opened.id).await;
    silent.attached().await;
    assert_eq!(silent.message().await, diagnostics(uri, 1));
    std::fs::write(&later, "").unwrap();
    assert_eq!(silent.message().await, diagnostics(uri, 2));
    assert_eq!(silent.message().await, progress("index", "begin"));
    drop(silent);
    // The replay given back keeps the newest diagnostics and the progress that began after the replay.
    let mut next = fixture.attach(&opened.id).await;
    next.attached().await;
    assert_eq!(next.message().await, diagnostics(uri, 2));
    assert_eq!(next.message().await, progress("index", "begin"));
    next.send(request(7, "test/echo", json!({}))).await;
    assert_eq!(next.message().await["id"], json!(7));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn an_attach_overtaken_by_a_newer_one_is_told_it_was_replaced() {
    let fixture = Fixture::new();
    let (opened, register) = queued_for_the_next_attach(&fixture, 2).await;
    // Both are asked for before the server's task takes either.
    let mut older = fixture.attach(&opened.id).await;
    let mut newer = fixture.attach(&opened.id).await;
    match older.frame().await {
        Frame::Ended(ended) => assert_eq!(ended.reason, LspEndReason::Replaced as i32),
        other => panic!("expected LspEnded, got {other:?}"),
    }
    assert_whole_replay(&mut newer, 2, &register).await;
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_replaced_bridges_waiting_message_never_reaches_the_server() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let opened = fixture
        .open(&[json!({"wait_file": go}), json!({"recv": "test/fresh"})])
        .await;
    let mut older = fixture.attach(&opened.id).await;
    older.attached().await;
    // The server is not reading: this fills what the helper lets wait for its input.
    older
        .send(notification("test/big", json!({"pad": "x".repeat(5 * 1024 * 1024)})))
        .await;
    let waiting = tokio::spawn(async move {
        older.send(notification("test/stale", json!({}))).await;
        older
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished(), "the older bridge's message waits for room");
    let mut newer = fixture.attach(&opened.id).await;
    newer.attached().await;
    std::fs::write(&go, "").unwrap();
    newer.send(notification("test/fresh", json!({}))).await;
    newer.send(request(9, "test/echo", json!({}))).await;
    assert_eq!(newer.message().await["id"], json!(9));
    let _older = waiting.await.unwrap();
    let methods: Vec<_> = lines(&opened.received)
        .into_iter()
        .filter_map(|line| line.get("method").cloned())
        .collect();
    assert!(!methods.contains(&json!("test/stale")), "{methods:?}");
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_replaced_bridge_that_stopped_reading_does_not_hold_up_the_next() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let mut steps = vec![json!({"wait_file": go})];
    for n in 0..20 {
        steps.push(json!({"send": notification(&format!("test/{n}"), json!({}))}));
    }
    let opened = fixture.open(&steps).await;
    let mut older = fixture.attach(&opened.id).await;
    older.attached().await;
    // It has spoken, so what it took is not handed to the next bridge again.
    older.send(notification("test/hello", json!({}))).await;
    std::fs::write(&go, "").unwrap();
    until(|| older.frames.len() == older.frames.max_capacity()).await;
    let mut newer = fixture.attach(&opened.id).await;
    newer.attached().await;
    newer.send(request(9, "test/echo", json!({}))).await;
    let mut methods = Vec::new();
    loop {
        let message = newer.message().await;
        if message["id"] == json!(9) {
            break;
        }
        methods.push(message["method"].clone());
    }
    // The older bridge took the first eight; the one that was waiting for it goes to the newer, then the rest.
    let expected: Vec<_> = (8..20).map(|n| json!(format!("test/{n}"))).collect();
    assert_eq!(methods, expected);
    // What the older bridge had not taken is still there, and after it, why it ends.
    assert_eq!(older.ended().await.reason, LspEndReason::Replaced as i32);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_notification_in_chunks_cut_off_by_a_replace_reaches_the_next_bridge_whole_and_once() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let size = 2 * LSP_CHUNK_MAX + 10;
    let mut steps = vec![json!({"wait_file": go})];
    for n in 0..7 {
        steps.push(json!({"send": notification(&format!("test/{n}"), json!({}))}));
    }
    steps.push(json!({"send_big": {"method": "test/big", "bytes": size}}));
    steps.push(json!({"send": notification("test/after", json!({}))}));
    let opened = fixture.open(&steps).await;
    let mut older = fixture.attach(&opened.id).await;
    older.attached().await;
    older.send(notification("test/hello", json!({}))).await;
    std::fs::write(&go, "").unwrap();
    // Seven small notifications and the big one's first chunk fill its frames; the second chunk waits for it.
    until(|| older.frames.len() == older.frames.max_capacity()).await;
    let mut newer = fixture.attach(&opened.id).await;
    newer.attached().await;
    newer.send(request(9, "test/echo", json!({}))).await;
    let big = newer.message().await;
    assert_eq!(big["method"], json!("test/big"));
    assert_eq!(big["params"]["pad"].as_str().unwrap().len(), size);
    assert_eq!(newer.message().await["method"], json!("test/after"));
    assert_eq!(newer.message().await["id"], json!(9));
    fixture.servers.stop(&opened.id).await;
}

/// `count` notifications `test/big` of one chunk's worth of padding each, for a bridge to take live.
fn big_notifications(count: usize) -> Vec<Value> {
    (0..count)
        .map(|_| json!({"send_big": {"method": "test/big", "bytes": LSP_CHUNK_MAX}}))
        .collect()
}

#[tokio::test]
async fn a_silent_bridge_sent_past_the_queue_bound_still_gives_back_its_registrations() {
    let fixture = Fixture::new();
    let (go, first, second) = (fixture.file("go"), fixture.file("first"), fixture.file("second"));
    let register = json!({"registrations": [{"id": "r1", "method": "textDocument/didSave"}]});
    let mut steps = vec![
        json!({"wait_file": go}),
        json!({"send": request(20, "client/registerCapability", register.clone())}),
    ];
    steps.extend(barrier());
    // Seventeen messages of a little over 1 MiB pass `LSP_DETACHED_QUEUE_MAX` at the sixteenth.
    steps.push(json!({"wait_file": first}));
    steps.extend(big_notifications(17));
    steps.push(json!({"wait_file": second}));
    steps.extend(big_notifications(17));
    let opened = fixture.open(&steps).await;
    std::fs::write(&go, "").unwrap();
    until_received(&opened.received, response_to(50)).await;
    let mut silent = fixture.attach(&opened.id).await;
    assert_eq!(silent.attached().await.registrations.len(), 1);
    std::fs::write(&first, "").unwrap();
    for _ in 0..17 {
        assert_eq!(silent.message().await["method"], json!("test/big"));
    }
    drop(silent);
    // It never spoke, so it may not have read what it was handed: the registration and the counters go to the next,
    // with the notifications let go past the bound counted and the one kept after them replayed.
    let mut next = fixture.attach(&opened.id).await;
    let attached = next.attached().await;
    assert!(!attached.reattached);
    assert_eq!(attached.registrations.len(), 1);
    assert_eq!(
        &serde_json::from_slice::<Value>(&attached.registrations[0].params).unwrap(),
        &register
    );
    assert_eq!(attached.requests_answered, 2);
    assert_eq!(attached.notifications_dropped, 16);
    assert_eq!(next.message().await["method"], json!("test/big"));
    std::fs::write(&second, "").unwrap();
    for _ in 0..17 {
        assert_eq!(next.message().await["method"], json!("test/big"));
    }
    // This one speaks after passing the bound: it read what it was handed, so nothing counts as dropped.
    next.send(request(7, "test/echo", json!({}))).await;
    assert_eq!(next.message().await["id"], json!(7));
    drop(next);
    let mut last = fixture.attach(&opened.id).await;
    let attached = last.attached().await;
    assert!(attached.reattached);
    assert!(attached.registrations.is_empty());
    assert_eq!(attached.notifications_dropped, 0);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_replaced_bridges_oversized_answer_is_dropped() {
    let fixture = Fixture::new();
    let go = fixture.file("go");
    let opened = fixture
        .open(&[
            json!({"wait_file": go}),
            json!({"send": request(9, "custom/ask", json!({}))}),
        ])
        .await;
    let mut older = fixture.attach(&opened.id).await;
    older.attached().await;
    let mut newer = fixture.attach(&opened.id).await;
    newer.attached().await;
    assert_eq!(older.ended().await.reason, LspEndReason::Replaced as i32);
    std::fs::write(&go, "").unwrap();
    assert_eq!(newer.message().await["id"], json!(9));
    let big = format!(
        r#"{{"jsonrpc":"2.0","id":9,"result":"{}"}}"#,
        "z".repeat(LSP_MESSAGE_MAX + 16)
    );
    older.send_bytes(big.as_bytes()).await;
    newer.send(json!({"jsonrpc": "2.0", "id": 9, "result": "ok"})).await;
    newer.send(request(10, "test/echo", json!({}))).await;
    assert_eq!(newer.message().await["id"], json!(10));
    let answers: Vec<_> = lines(&opened.received).into_iter().filter(response_to(9)).collect();
    assert_eq!(answers, [json!({"jsonrpc": "2.0", "id": 9, "result": "ok"})]);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn an_attached_server_outlives_its_grace() {
    let fixture = Fixture::new();
    let (mut request_message, received) = fixture.request("scripted-lsp", &[], "client-a");
    request_message.grace_ms = 1000;
    let opened = fixture.open_request(request_message, received).await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    bridge.send(request(1, "test/echo", json!({}))).await;
    assert_eq!(bridge.message().await["id"], json!(1));
    assert!(fixture.info(&opened.id).is_some_and(|info| info.attached));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_server_that_ignores_sigterm_is_killed() {
    let fixture = Fixture::new();
    let ready = fixture.file("ready");
    let opened = fixture
        .open(&[json!({"ignore_sigterm": true}), json!({"touch": ready})])
        .await;
    until(|| ready.exists()).await;
    fixture.servers.stop(&opened.id).await;
    assert!(ended(opened.pid));
    assert!(fixture
        .audit()
        .contains(&format!("lsp-end {} stopped signal=9", opened.id)));
}

#[tokio::test]
async fn what_a_server_started_goes_with_it_when_it_exits() {
    let fixture = Fixture::new();
    let child = fixture.file("child");
    let opened = fixture
        .open(&[
            json!({"recv": "test/fork"}),
            json!({"spawn_child": child}),
            json!({"exit": 0}),
        ])
        .await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    bridge.send(notification("test/fork", json!({}))).await;
    // The child keeps the server's stdout open after the server has exited: the server still ends.
    let ended_frame = bridge.ended().await;
    assert_eq!(ended_frame.reason, LspEndReason::Exited as i32);
    assert_eq!(ended_frame.code, Some(0));
    let child_pid: i32 = std::fs::read_to_string(&child).unwrap().parse().unwrap();
    until(|| ended(child_pid)).await;
}

#[tokio::test]
async fn ended_servers_logs_are_removed_after_their_retention() {
    let fixture = Fixture::new();
    let opened = fixture.open(&[]).await;
    let dir = fixture.servers.log_dir();
    let now = std::time::SystemTime::now();
    let hours_ago = |hours: u64| now - Duration::from_secs(hours * 3600);
    let set_modified = |path: &Path, at| {
        std::fs::File::options()
            .append(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };
    let log = |name: String, at| {
        let path = dir.join(name);
        std::fs::write(&path, "x").unwrap();
        set_modified(&path, at);
        path
    };
    // A live server's log stays however old it is; a file that is not a server's log is not touched.
    let live = dir.join(format!("{}.log", opened.id));
    set_modified(&live, hours_ago(30 * 24));
    let other = log("notes.txt".into(), hours_ago(30 * 24));
    let expired = log(format!("{:032x}.log", 1), hours_ago(8 * 24));
    let fresh = log(format!("{:032x}.log", 2), now);
    // Newest first, past the count kept the older ones go even inside the retention.
    let kept = daemon::lsp::LOGS_KEPT as u64;
    let recent: Vec<_> = (0..kept + 6)
        .map(|n| log(format!("{:032x}.log", 100 + n), hours_ago(1 + n)))
        .collect();
    fixture.servers.trim_logs();
    assert!(live.exists() && other.exists() && fresh.exists());
    assert!(!expired.exists());
    // `fresh` is the newest ended log, so `kept - 1` of these stay.
    let (stay, go) = recent.split_at(kept as usize - 1);
    assert!(stay.iter().all(|path| path.exists()));
    assert!(go.iter().all(|path| !path.exists()));
    fixture.servers.stop(&opened.id).await;
}

#[test]
fn fixture_daemon_entry() {
    if std::env::var_os("EITRI_TEST_DAEMON").is_none() {
        return;
    }
    run_dir::save_inherited_umask();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let paths = run_dir::Paths::from_env(VERSION).unwrap();
        let idle = std::env::var("EITRI_TEST_IDLE").unwrap().parse().unwrap();
        let env = Environment::from([
            ("PATH".into(), std::env::var_os("EITRI_TEST_LOGIN_PATH").unwrap()),
            ("HOME".into(), std::env::var_os("HOME").unwrap()),
            ("LANG".into(), "C.UTF-8".into()),
        ]);
        daemon::run_with_environment(
            paths,
            Duration::from_secs(idle),
            daemon::OwnedSidecars::default(),
            async move { Ok(Captured { env, fallback: None }) },
        )
        .await
        .unwrap();
    });
}

/// A fixture daemon of this test binary, with its own homes in the fixture.
struct Daemon {
    child: Child,
}

impl Daemon {
    async fn start(fixture: &Fixture, idle_seconds: u64) -> Daemon {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("EITRI_TEST_DAEMON", "1")
            .env("EITRI_TEST_IDLE", idle_seconds.to_string())
            .env("EITRI_TEST_LOGIN_PATH", login_path(&fixture.base))
            .env("HOME", fixture.path("home"))
            .env("XDG_DATA_HOME", fixture.path("d"))
            .env("XDG_STATE_HOME", fixture.path("s"))
            .env("TMPDIR", &fixture.base)
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(fixture.path("daemon.err")).unwrap())
            .spawn()
            .unwrap();
        let mut daemon = Daemon { child };
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "the daemon exited during startup"
            );
            let published = fixture.paths.published().is_ok_and(|p| p.info.pid == daemon.child.id());
            if published && daemon.client(fixture).await.is_some() {
                return daemon;
            }
            assert!(Instant::now() < deadline, "the daemon did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn client(&self, fixture: &Fixture) -> Option<LspClient<Channel>> {
        let socket = fixture.paths.socket().ok()?;
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let socket = socket.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        tokio::net::UnixStream::connect(socket).await?,
                    ))
                }
            }))
            .await
            .ok()?;
        Some(LspClient::new(channel))
    }

    async fn exited(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the daemon did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn open_through(client: &mut LspClient<Channel>, fixture: &Fixture) -> Opened {
    let (request, received) = fixture.request("scripted-lsp", &[], "client-a");
    let response = client.open_server(request).await.unwrap().into_inner();
    assert!(!response.server_id.is_empty(), "{response:?}");
    let started = until_received(&received, |line| line.get("started").is_some()).await;
    Opened {
        id: response.server_id.clone(),
        pid: started["started"]["pid"].as_i64().unwrap() as i32,
        received,
        response,
    }
}

#[tokio::test]
async fn the_daemon_is_not_idle_while_a_server_lives() {
    let fixture = Fixture::new();
    let mut daemon = Daemon::start(&fixture, 1).await;
    let opened = {
        let mut client = daemon.client(&fixture).await.unwrap();
        open_through(&mut client, &fixture).await
    };
    // Well past the idle interval, with no connection open: the live server keeps the daemon.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        daemon.child.try_wait().unwrap().is_none(),
        "the daemon exited with a server alive"
    );
    assert!(!ended(opened.pid));
    {
        let mut client = daemon.client(&fixture).await.unwrap();
        client
            .stop_server(StopServerRequest {
                server_id: opened.id.clone(),
                command_id: "stop-1".into(),
            })
            .await
            .unwrap();
    }
    assert!(ended(opened.pid));
    assert!(daemon.exited().await.success());
}

#[tokio::test]
async fn sigterm_stops_servers() {
    let fixture = Fixture::new();
    let mut daemon = Daemon::start(&fixture, 600).await;
    let mut client = daemon.client(&fixture).await.unwrap();
    let opened = open_through(&mut client, &fixture).await;
    let (input, receiver) = mpsc::channel(2);
    input.send(hello(&opened.id)).await.unwrap();
    let mut frames = client
        .attach(tokio_stream::wrappers::ReceiverStream::new(receiver))
        .await
        .unwrap()
        .into_inner();
    match frames.message().await.unwrap().unwrap().frame.unwrap() {
        Frame::Attached(attached) => assert!(!attached.reattached),
        _ => panic!("expected LspAttached"),
    }
    // SAFETY: kill takes no pointers; the daemon is this test's unreaped child.
    unsafe {
        libc::kill(daemon.child.id() as i32, libc::SIGTERM);
    }
    // The daemon says why while it still serves; it does not wait for the frame to be written before it exits.
    match frames.message().await {
        Ok(Some(LspServerFrame {
            frame: Some(Frame::Ended(ended)),
        })) => assert_eq!(ended.reason, LspEndReason::Daemon as i32),
        Ok(None) | Err(_) => {}
        Ok(Some(other)) => panic!("expected LspEnded, got {other:?}"),
    }
    assert!(daemon.exited().await.success());
    assert!(ended(opened.pid));
    assert!(fixture.audit().contains(&format!("lsp-end {} daemon", opened.id)));
    drop(input);
}
