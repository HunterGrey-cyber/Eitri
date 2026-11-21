//! The save hold: a `textDocument/didSave` the bridge sends with the laptop file's hash waits until the server's file
//! hashes the same, and the messages for that document wait behind it. Driven through `lsp::Servers` with the
//! scripted language server, as `lsp.rs` is.
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

use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use daemon::lsp::save_hold::{LOOKS_MAX, SAVE_HOLD_MAX};
use eitri_server_proto::*;
use lsp_support::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The daemon's sources, included above, name this crate-root value.
const REVISION: &str = env!("EITRI_BUILD_COMMIT");

/// Longer than the hang guard: a save held by mistake fails the test instead of being let go at its deadline.
const LONG_HOLD: Duration = Duration::from_secs(120);

/// The daemon's sources, included above, name this crate-root helper.
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

fn fixture() -> Fixture {
    Fixture::with(|servers| servers.with_save_hold(LONG_HOLD))
}

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn uri_of(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn did_save(uri: &str) -> Value {
    notification("textDocument/didSave", json!({"textDocument": {"uri": uri}}))
}

fn did_change(uri: &str, version: u32) -> Value {
    notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": "after"}]}),
    )
}

fn hover(id: i64, uri: &str) -> Value {
    request(
        id,
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
    )
}

/// The methods (and ids of requests) the server has read, in order.
fn seen(received: &Path) -> Vec<String> {
    lines(received)
        .into_iter()
        .filter_map(|line| {
            let method = line.get("method")?.as_str()?.to_owned();
            Some(match line.get("id") {
                Some(id) => format!("{method}#{id}"),
                None => method,
            })
        })
        .collect()
}

/// A request for no document: once its answer is back, the server has read everything the helper passed on before it.
async fn barrier(bridge: &mut Bridge, id: i64) {
    bridge.send(request(id, "test/echo", json!({}))).await;
    assert_eq!(bridge.message().await["id"], json!(id));
}

fn saves(received: &Path) -> usize {
    seen(received)
        .into_iter()
        .filter(|method| method == "textDocument/didSave")
        .count()
}

/// Every message the server has read, in order: methods (with request ids) and `response#<id>`.
fn order(received: &Path) -> Vec<String> {
    lines(received)
        .into_iter()
        .filter_map(
            |line| match (line.get("method").and_then(Value::as_str), line.get("id")) {
                (Some(method), Some(id)) => Some(format!("{method}#{id}")),
                (Some(method), None) => Some(method.to_owned()),
                (None, Some(id)) => Some(format!("response#{id}")),
                (None, None) => None,
            },
        )
        .collect()
}

/// How many of this process's inotify instances watch `dir`.
fn watching(dir: &Path) -> usize {
    let needle = format!(" ino:{:x} ", std::fs::metadata(dir).unwrap().ino());
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| std::fs::read_link(entry.path()).is_ok_and(|to| to.as_os_str() == "anon_inode:inotify"))
        .filter(|entry| {
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", entry.file_name().to_string_lossy()))
                .is_ok_and(|info| info.contains(&needle))
        })
        .count()
}

async fn opened_and_attached(fixture: &Fixture) -> (Opened, Bridge) {
    let opened = fixture.open(&[]).await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    (opened, bridge)
}

#[tokio::test]
async fn did_save_waits_for_the_file_and_goes_when_it_arrives() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    std::fs::write(&file, "before").unwrap();
    bridge.send_save(did_save(&uri_of(&file)), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    assert!(!seen(&opened.received).contains(&"textDocument/didSave".to_owned()));
    std::fs::write(&file, "saved").unwrap();
    let save = until_received(&opened.received, method_is("textDocument/didSave")).await;
    assert_eq!(save["params"]["textDocument"]["uri"], json!(uri_of(&file)));
    // A save whose file already matches goes at once (held, it would wait the 120 s hold).
    bridge.send_save(did_save(&uri_of(&file)), &sha256("saved")).await;
    until(|| saves(&opened.received) == 2).await;
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_change_after_the_save_never_overtakes_it() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    let uri = uri_of(&file);
    std::fs::write(&file, "before").unwrap();
    bridge.send_save(did_save(&uri), &sha256("saved")).await;
    bridge.send(did_change(&uri, 2)).await;
    bridge.send(hover(2, &uri)).await;
    barrier(&mut bridge, 3).await;
    assert_eq!(seen(&opened.received), ["test/echo#3"]);
    std::fs::write(&file, "saved").unwrap();
    assert_eq!(bridge.message().await["id"], json!(2));
    assert_eq!(
        seen(&opened.received),
        [
            "test/echo#3",
            "textDocument/didSave",
            "textDocument/didChange",
            "textDocument/hover#2"
        ]
    );
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn other_documents_flow_while_one_is_held() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let (held, other) = (fixture.path("project/a.rs"), fixture.path("project/b.rs"));
    bridge.send_save(did_save(&uri_of(&held)), &sha256("saved")).await;
    bridge.send(did_change(&uri_of(&other), 2)).await;
    bridge.send(hover(4, &uri_of(&other))).await;
    assert_eq!(bridge.message().await["id"], json!(4));
    // The editor's answer to a server request carries no document either.
    bridge.send(json!({"jsonrpc": "2.0", "id": 77, "result": null})).await;
    barrier(&mut bridge, 5).await;
    let methods = seen(&opened.received);
    assert_eq!(
        methods,
        ["textDocument/didChange", "textDocument/hover#4", "test/echo#5"]
    );
    assert!(until_received(&opened.received, response_to(77))
        .await
        .get("result")
        .is_some());
    std::fs::write(&held, "saved").unwrap();
    until_received(&opened.received, method_is("textDocument/didSave")).await;
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn the_hold_ends_at_five_seconds_and_says_so() {
    assert_eq!(SAVE_HOLD_MAX, Duration::from_secs(5));
    let fixture = Fixture::new();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    std::fs::write(&file, "never what the laptop saved").unwrap();
    let uri = uri_of(&file);
    let start = Instant::now();
    bridge.send_save(did_save(&uri), &sha256("saved")).await;
    bridge.send(hover(6, &uri)).await;
    assert_eq!(bridge.message().await["id"], json!(6));
    assert!(start.elapsed() >= SAVE_HOLD_MAX, "{:?}", start.elapsed());
    assert_eq!(seen(&opened.received), ["textDocument/didSave", "textDocument/hover#6"]);
    assert!(fixture.audit().contains(&format!("lsp-save-late {}", opened.id)));
    let log = std::fs::read_to_string(fixture.servers.log_dir().join(format!("{}.log", opened.id))).unwrap();
    assert!(log.contains("before its file matched"), "{log}");
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_save_outside_the_root_is_not_held() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    std::fs::create_dir(fixture.path("project/dir")).unwrap();
    let long = format!(
        "file://{}/{}",
        fixture.path("project").display(),
        "n".repeat(LSP_URI_MAX)
    );
    let uris = [
        uri_of(&fixture.path("work/elsewhere.rs")),
        format!("file://{}/../work/elsewhere.rs", fixture.path("project").display()),
        // Not a regular file.
        uri_of(&fixture.path("project/dir")),
        long,
        "untitled:Untitled-1".to_owned(),
    ];
    for (at, uri) in uris.iter().enumerate() {
        bridge.send_save(did_save(uri), &sha256("saved")).await;
        bridge.send(hover(10 + at as i64, uri)).await;
        assert_eq!(bridge.message().await["id"], json!(10 + at as i64));
    }
    let saves = seen(&opened.received)
        .into_iter()
        .filter(|method| method == "textDocument/didSave")
        .count();
    assert_eq!(saves, uris.len());
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_rename_into_place_releases_the_hold() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    std::fs::write(&file, "before").unwrap();
    bridge.send_save(did_save(&uri_of(&file)), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    assert_eq!(seen(&opened.received), ["test/echo#1"]);
    let staged = fixture.path("project/.a.rs.staged");
    std::fs::write(&staged, "saved").unwrap();
    std::fs::rename(&staged, &file).unwrap();
    until_received(&opened.received, method_is("textDocument/didSave")).await;
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_save_in_a_directory_made_later_is_released_when_its_file_arrives() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/new/deeper/a.rs");
    bridge.send_save(did_save(&uri_of(&file)), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    std::fs::create_dir(fixture.path("project/new")).unwrap();
    std::fs::create_dir(fixture.path("project/new/deeper")).unwrap();
    let staged = fixture.path("project/new/deeper/.a.rs.staged");
    std::fs::write(&staged, "saved").unwrap();
    std::fs::rename(&staged, &file).unwrap();
    until_received(&opened.received, method_is("textDocument/didSave")).await;
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_detach_lists_the_held_saves_it_dropped() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let (a, b, c) = (
        fixture.path("project/a.rs"),
        fixture.path("project/b.rs"),
        fixture.path("project/c.rs"),
    );
    bridge.send_save(did_save(&uri_of(&a)), &sha256("saved a")).await;
    bridge.send(did_change(&uri_of(&a), 2)).await;
    bridge.send(hover(6, &uri_of(&a))).await;
    bridge.send_save(did_save(&uri_of(&b)), &sha256("saved b")).await;
    // A second save of a held document waits behind the first and is not listed twice.
    bridge.send_save(did_save(&uri_of(&a)), &sha256("saved a")).await;
    barrier(&mut bridge, 1).await;
    drop(bridge);
    fixture.until_detached(&opened.id).await;
    // The files arriving after the detach send nothing: what was held went with the bridge.
    std::fs::write(&a, "saved a").unwrap();
    std::fs::write(&b, "saved b").unwrap();
    let mut again = fixture.attach(&opened.id).await;
    let attached = again.attached().await;
    assert_eq!(attached.dropped_saves, [uri_of(&a), uri_of(&b)]);
    assert_eq!(attached.requests_answered, 0);
    barrier(&mut again, 2).await;
    // The held request was never passed on, so the server is sent no cancellation for it.
    assert_eq!(seen(&opened.received), ["test/echo#1", "test/echo#2"]);
    // A replaced bridge's held saves go to the attach that replaced it, and back again if that one never speaks.
    again.send_save(did_save(&uri_of(&c)), &sha256("saved c")).await;
    barrier(&mut again, 3).await;
    let mut silent = fixture.attach(&opened.id).await;
    assert_eq!(silent.attached().await.dropped_saves, [uri_of(&c)]);
    drop(silent);
    fixture.until_detached(&opened.id).await;
    let mut next = fixture.attach(&opened.id).await;
    assert_eq!(next.attached().await.dropped_saves, [uri_of(&c)]);
    barrier(&mut next, 4).await;
    drop(next);
    fixture.until_detached(&opened.id).await;
    let mut last = fixture.attach(&opened.id).await;
    assert!(last.attached().await.dropped_saves.is_empty());
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn quick_saves_that_sync_coalesced_go_together() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    // Both saves are held when the file arrives as the second.
    let a = fixture.path("project/a.rs");
    std::fs::write(&a, "before").unwrap();
    bridge.send_save(did_save(&uri_of(&a)), &sha256("first")).await;
    bridge.send(did_change(&uri_of(&a), 2)).await;
    bridge.send_save(did_save(&uri_of(&a)), &sha256("second")).await;
    barrier(&mut bridge, 1).await;
    assert_eq!(saves(&opened.received), 0);
    std::fs::write(&a, "second").unwrap();
    until(|| saves(&opened.received) == 2).await;
    // The file is there as the second before its save comes: the save that joins the queue has it looked at.
    let b = fixture.path("project/b.rs");
    std::fs::write(&b, "before").unwrap();
    bridge.send_save(did_save(&uri_of(&b)), &sha256("first")).await;
    barrier(&mut bridge, 2).await;
    std::fs::write(&b, "second").unwrap();
    barrier(&mut bridge, 3).await;
    bridge.send_save(did_save(&uri_of(&b)), &sha256("second")).await;
    until(|| saves(&opened.received) == 4).await;
    assert_eq!(
        seen(&opened.received)
            .into_iter()
            .filter(|method| !method.starts_with("test/echo"))
            .collect::<Vec<_>>(),
        [
            "textDocument/didSave",
            "textDocument/didChange",
            "textDocument/didSave",
            "textDocument/didSave",
            "textDocument/didSave"
        ]
    );
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_second_save_of_a_held_document_waits_for_its_own_file() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    let uri = uri_of(&file);
    std::fs::write(&file, "before").unwrap();
    bridge.send_save(did_save(&uri), &sha256("first")).await;
    bridge.send(did_change(&uri, 2)).await;
    bridge.send_save(did_save(&uri), &sha256("second")).await;
    bridge.send(hover(2, &uri)).await;
    std::fs::write(&file, "first").unwrap();
    until(|| saves(&opened.received) == 1).await;
    barrier(&mut bridge, 3).await;
    assert_eq!(
        seen(&opened.received),
        ["textDocument/didSave", "textDocument/didChange", "test/echo#3"]
    );
    std::fs::write(&file, "second").unwrap();
    assert_eq!(bridge.message().await["id"], json!(2));
    assert_eq!(
        seen(&opened.received)[3..],
        ["textDocument/didSave", "textDocument/hover#2"]
    );
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_save_that_went_before_a_detach_is_not_listed() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    bridge.send_save(did_save(&uri_of(&file)), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    std::fs::write(&file, "saved").unwrap();
    until(|| saves(&opened.received) == 1).await;
    drop(bridge);
    fixture.until_detached(&opened.id).await;
    let mut again = fixture.attach(&opened.id).await;
    assert!(again.attached().await.dropped_saves.is_empty());
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_percent_encoded_uri_is_held_for_its_file() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a b%.rs");
    let uri = format!("file://{}", fixture.path("project/a%20b%25.rs").display());
    bridge.send_save(did_save(&uri), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    assert_eq!(saves(&opened.received), 0);
    std::fs::write(&file, "saved").unwrap();
    let save = until_received(&opened.received, method_is("textDocument/didSave")).await;
    assert_eq!(save["params"]["textDocument"]["uri"], json!(uri));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_link_or_a_fifo_at_the_save_path_is_not_held_and_not_opened() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    // Followed, the link would lead to a file that differs, and the save would be held.
    let target = fixture.path("work/target.rs");
    std::fs::write(&target, "other").unwrap();
    let link = fixture.path("project/link.rs");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let fifo = fixture.path("project/pipe.rs");
    let c_fifo = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
    // A writer waiting in open(): a reader opening the FIFO would let it through.
    let (tid_sender, tid) = std::sync::mpsc::channel();
    let (through_sender, through) = std::sync::mpsc::channel();
    let writer_path = fifo.clone();
    std::thread::spawn(move || {
        // SAFETY: gettid takes no arguments.
        let _ = tid_sender.send(unsafe { libc::syscall(libc::SYS_gettid) });
        let file = std::fs::OpenOptions::new().write(true).open(&writer_path);
        let _ = through_sender.send(file.is_ok());
    });
    let tid = tid.recv().unwrap();
    until(|| {
        std::fs::read_to_string(format!("/proc/self/task/{tid}/stat"))
            .is_ok_and(|stat| stat.rsplit(')').next().unwrap().split_whitespace().next() == Some("S"))
    })
    .await;
    for (at, path) in [&link, &fifo].into_iter().enumerate() {
        let uri = uri_of(path);
        bridge.send_save(did_save(&uri), &sha256("saved")).await;
        bridge.send(hover(20 + at as i64, &uri)).await;
        assert_eq!(bridge.message().await["id"], json!(20 + at as i64));
    }
    assert_eq!(saves(&opened.received), 2);
    assert!(through.try_recv().is_err(), "the FIFO's writer was let through");
    // Let the writer go.
    let _reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    assert!(through.recv_timeout(WAIT).unwrap());
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_change_of_attributes_alone_releases_the_hold() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    std::fs::write(&file, "before").unwrap();
    let alias = fixture.path("work/alias.rs");
    std::fs::hard_link(&file, &alias).unwrap();
    bridge.send_save(did_save(&uri_of(&file)), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    // Written through a name in another directory: the save's directory reports nothing.
    std::fs::write(&alias, "saved").unwrap();
    barrier(&mut bridge, 2).await;
    assert_eq!(saves(&opened.received), 0);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    until(|| saves(&opened.received) == 1).await;
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_root_named_through_a_link_holds_saves_under_either_path() {
    let fixture = fixture();
    std::os::unix::fs::symlink(fixture.path("project"), fixture.path("linked")).unwrap();
    let (mut request, received) = fixture.request("scripted-lsp", &[], "client-a");
    request.root = fixture.path("linked").to_str().unwrap().into();
    let opened = fixture.open_request(request, received).await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    let (named, resolved) = (fixture.path("linked/a.rs"), fixture.path("project/b.rs"));
    bridge.send_save(did_save(&uri_of(&named)), &sha256("saved")).await;
    bridge.send_save(did_save(&uri_of(&resolved)), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    assert_eq!(saves(&opened.received), 0);
    std::fs::write(&named, "saved").unwrap();
    std::fs::write(&resolved, "saved").unwrap();
    until(|| saves(&opened.received) == 2).await;
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_watched_directory_moved_away_and_made_again_is_watched_again() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    std::fs::create_dir(fixture.path("project/sub")).unwrap();
    let file = fixture.path("project/sub/a.rs");
    bridge.send_save(did_save(&uri_of(&file)), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    std::fs::rename(fixture.path("project/sub"), fixture.path("project/old")).unwrap();
    std::fs::create_dir(fixture.path("project/sub")).unwrap();
    std::fs::write(&file, "saved").unwrap();
    until(|| saves(&opened.received) == 1).await;
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_slow_look_holds_up_no_other_document_and_no_deadline() {
    // Every look takes longer than the hold and its last look together.
    let delay = Duration::from_secs(6);
    let fixture = Fixture::with(|servers| servers.with_save_hold(Duration::from_secs(1)).with_look_delay(delay));
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let (held, other) = (fixture.path("project/a.rs"), fixture.path("project/b.rs"));
    let start = Instant::now();
    bridge.send_save(did_save(&uri_of(&held)), &sha256("saved")).await;
    bridge.send(hover(4, &uri_of(&other))).await;
    assert_eq!(bridge.message().await["id"], json!(4));
    // The other document went while the save was still held, not when its deadline let it go.
    assert_eq!(saves(&opened.received), 0);
    until(|| saves(&opened.received) == 1).await;
    assert!(start.elapsed() < delay, "{:?}", start.elapsed());
    // The audit line follows the save's write to the server.
    until(|| fixture.audit().contains(&format!("lsp-save-late {}", opened.id))).await;
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn an_edit_answer_and_a_cancellation_wait_behind_the_held_document() {
    let fixture = fixture();
    let edit = json!({"jsonrpc": "2.0", "id": "edit-1", "method": "workspace/applyEdit",
        "params": {"edit": {"changes": {}}}});
    let opened = fixture.open(&[json!({"recv": "test/go"}), json!({"send": edit})]).await;
    let mut bridge = fixture.attach(&opened.id).await;
    bridge.attached().await;
    let file = fixture.path("project/a.rs");
    let uri = uri_of(&file);
    bridge.send_save(did_save(&uri), &sha256("saved")).await;
    bridge.send(notification("test/go", json!({}))).await;
    assert_eq!(bridge.message().await["id"], json!("edit-1"));
    bridge.send(did_change(&uri, 2)).await;
    bridge
        .send(json!({"jsonrpc": "2.0", "id": "edit-1", "result": {"applied": true}}))
        .await;
    bridge.send(hover(5, &uri)).await;
    bridge.send(notification("$/cancelRequest", json!({"id": 5}))).await;
    barrier(&mut bridge, 6).await;
    assert_eq!(order(&opened.received), ["test/go", "test/echo#6"]);
    std::fs::write(&file, "saved").unwrap();
    assert_eq!(bridge.message().await["id"], json!(5));
    until(|| order(&opened.received).len() == 7).await;
    assert_eq!(
        order(&opened.received)[2..],
        [
            "textDocument/didSave",
            "textDocument/didChange",
            "response#\"edit-1\"",
            "textDocument/hover#5",
            "$/cancelRequest"
        ]
    );
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_replaced_attach_lets_go_of_what_it_held_and_its_watch() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let project = fixture.path("project");
    bridge
        .send_save(did_save(&uri_of(&project.join("a.rs"))), &sha256("saved"))
        .await;
    barrier(&mut bridge, 1).await;
    assert_eq!(watching(&project), 1);
    let mut next = fixture.attach(&opened.id).await;
    next.attached().await;
    // The replaced bridge's next save finds its attach replaced: what it held goes, and its watch with it.
    bridge
        .send_save(did_save(&uri_of(&project.join("b.rs"))), &sha256("saved"))
        .await;
    until(|| watching(&project) == 0).await;
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_replaced_attach_reports_no_late_save_it_never_sent() {
    let fixture = Fixture::with(|servers| servers.with_save_hold(Duration::from_secs(1)));
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let project = fixture.path("project");
    let uri = uri_of(&project.join("a.rs"));
    bridge.send_save(did_save(&uri), &sha256("saved")).await;
    barrier(&mut bridge, 1).await;
    assert_eq!(watching(&project), 1);
    let mut next = fixture.attach(&opened.id).await;
    assert_eq!(next.attached().await.dropped_saves, [uri]);
    // The replaced bridge says nothing more: its orderer lets the save go at the deadline, and `to_server` drops it.
    until(|| watching(&project) == 0).await;
    barrier(&mut next, 2).await;
    assert_eq!(saves(&opened.received), 0);
    assert!(!fixture.audit().contains("lsp-save-late"));
    let log = std::fs::read_to_string(fixture.servers.log_dir().join(format!("{}.log", opened.id))).unwrap_or_default();
    assert!(!log.contains("before its file matched"), "{log}");
    drop(bridge);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn looks_running_at_once_are_capped() {
    // Every look takes far longer than the saves below take to send, as on a stuck mount.
    let fixture = Fixture::with(|servers| {
        servers
            .with_save_hold(LONG_HOLD)
            .with_look_delay(Duration::from_secs(5))
    });
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    for n in 0..LOOKS_MAX + 4 {
        let uri = uri_of(&fixture.path(&format!("project/{n}.rs")));
        bridge.send_save(did_save(&uri), &sha256("saved")).await;
    }
    barrier(&mut bridge, 1).await;
    // The holds past the cap started no look; a look's place is taken before it is spawned.
    assert_eq!(fixture.servers.looks_running(&opened.id), LOOKS_MAX);
    assert_eq!(saves(&opened.received), 0);
    fixture.servers.stop(&opened.id).await;
}

#[tokio::test]
async fn a_reverted_save_does_not_match_the_file_it_came_back_to() {
    let fixture = fixture();
    let (opened, mut bridge) = opened_and_attached(&fixture).await;
    let file = fixture.path("project/a.rs");
    let uri = uri_of(&file);
    std::fs::write(&file, "A").unwrap();
    // Saved as B, then reverted and saved as A again, before sync brought either.
    bridge.send_save(did_save(&uri), &sha256("B")).await;
    bridge.send_save(did_save(&uri), &sha256("A")).await;
    barrier(&mut bridge, 1).await;
    // Both looks (the held save's and the one the second save asked for) have answered.
    until(|| fixture.servers.looks_running(&opened.id) == 0).await;
    std::fs::write(&file, "B").unwrap();
    until(|| saves(&opened.received) >= 1).await;
    barrier(&mut bridge, 2).await;
    // Only the first went: the file it already had did not let the second go with it.
    assert_eq!(saves(&opened.received), 1);
    std::fs::write(&file, "A").unwrap();
    until(|| saves(&opened.received) == 2).await;
    assert!(!fixture.audit().contains("lsp-save-late"));
    fixture.servers.stop(&opened.id).await;
}
