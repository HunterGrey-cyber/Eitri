//! The bridge between Neovim's language client and the helper's `Lsp` service: the state machine on its own for the
//! rules, and the whole session (framing, link, resync) against a fake `Lsp` service served in this process. No ssh,
//! no real language server and no helper binary: the fake service answers what each test scripts.
#![cfg(target_os = "linux")]

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent::remote_client::in_process;
use agent::remote_client::terminal_transport::wire::lsp_client_frame::Frame as Up;
use agent::remote_client::terminal_transport::wire::lsp_server::{Lsp, LspServer};
use agent::remote_client::terminal_transport::wire::lsp_server_frame::Frame as Down;
use agent::remote_client::terminal_transport::{tokio, tokio_stream, tonic, wire};
use eitri_core::remote_host::cli_lsp;
use eitri_core::remote_lsp::bridge::{Action, Bridge, Config, Disk, Failure, Out};
use eitri_core::remote_lsp::framing::{chunks, encode_frame, Frame, FrameError, FrameReader, Probe};
use eitri_core::remote_lsp::link::{Dial, DialError, Redial};
use eitri_core::remote_lsp::session::{self, Io, Options};
use eitri_core::remote_lsp::uris::Rewriter;
use eitri_core::remote_sync::ignores::Matcher;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tonic::{Request, Response, Status, Streaming};
use wire::{LspAttached, LspEndReason, LspEnded, LspRegistration};

const ROOT: &str = "/srv/project";
const HANG: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------------------------
// The state machine

#[derive(Clone, Default)]
struct FakeDisk(Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>);

impl FakeDisk {
    fn put(&self, rel: &str, text: &str) {
        self.0
            .lock()
            .unwrap()
            .insert(Path::new(ROOT).join(rel), text.as_bytes().to_vec());
    }

    fn remove(&self, rel: &str) {
        self.0.lock().unwrap().remove(&Path::new(ROOT).join(rel));
    }
}

impl Disk for FakeDisk {
    fn sha256(&self, path: &Path) -> std::io::Result<String> {
        let bytes = self.0.lock().unwrap().get(path).cloned();
        let bytes = bytes.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        Ok(format!("{:x}", Sha256::digest(&bytes)))
    }

    fn carried(&self, path: &Path) -> bool {
        path.starts_with(ROOT) && !path.starts_with(Path::new(ROOT).join("target"))
    }

    fn rewriter(&self, host: &str) -> Rewriter {
        Rewriter::new(host, Path::new(ROOT), Matcher::new(&["/target/".to_owned()]))
    }
}

fn uri(rel: &str) -> String {
    format!("file://{ROOT}/{rel}")
}

fn bridge_with(disk: &FakeDisk) -> Bridge {
    bridge_on("example-host", disk)
}

fn bridge_on(host: &str, disk: &FakeDisk) -> Bridge {
    Bridge::new(
        Config {
            host: host.into(),
            root: ROOT.into(),
            command: vec!["rust-analyzer".into()],
        },
        Box::new(disk.clone()),
    )
}

fn request(id: impl Into<Value>, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.into(), "method": method, "params": params})
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn response(id: impl Into<Value>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.into(), "result": result})
}

fn attached(reattached: bool) -> LspAttached {
    LspAttached {
        reattached,
        ..Default::default()
    }
}

fn ended(reason: LspEndReason, stderr: &str) -> LspEnded {
    LspEnded {
        reason: reason as i32,
        stderr_tail: stderr.as_bytes().to_vec(),
        ..Default::default()
    }
}

fn registration(method: &str, params: Value) -> LspRegistration {
    LspRegistration {
        method: method.into(),
        params: serde_json::to_vec(&params).unwrap(),
    }
}

fn did_open(uri: &str, text: &str) -> Value {
    notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri, "languageId": "rust", "version": 1, "text": text}}),
    )
}

fn did_change(uri: &str, version: i64, changes: Value) -> Value {
    notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": changes}),
    )
}

fn did_save(uri: &str) -> Value {
    notification("textDocument/didSave", json!({"textDocument": {"uri": uri}}))
}

fn methods(out: &Out) -> Vec<String> {
    out.to_server
        .iter()
        .map(|sent| sent.message["method"].as_str().unwrap_or("<response>").to_owned())
        .collect()
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// A bridge that has been through `initialize` and `initialized` against a server that answered.
fn live(disk: &FakeDisk) -> Bridge {
    let mut bridge = bridge_with(disk);
    let out = bridge.from_nvim(request(
        1,
        "initialize",
        json!({"processId": 4242, "rootUri": uri(""), "capabilities": {}}),
    ));
    assert_eq!(out.actions, [Action::OpenServer]);
    let out = bridge.link_back(&attached(false));
    assert_eq!(methods(&out), ["initialize"]);
    let capabilities = json!({"capabilities": {"textDocumentSync": {"openClose": true, "change": 2, "save": {}}}});
    let out = bridge.from_server(response(1, capabilities));
    assert_eq!(out.to_nvim.len(), 1, "the answer reaches Neovim");
    let out = bridge.from_nvim(notification("initialized", json!({})));
    assert_eq!(methods(&out), ["initialized"]);
    bridge
}

fn open_doc(bridge: &mut Bridge, disk: &FakeDisk, rel: &str, text: &str) {
    disk.put(rel, text);
    let out = bridge.from_nvim(did_open(&uri(rel), text));
    assert_eq!(methods(&out), ["textDocument/didOpen"]);
}

#[test]
fn framing_reads_split_and_joined_messages() {
    let first = br#"{"jsonrpc":"2.0","id":1,"method":"a"}"#;
    let second = br#"{"jsonrpc":"2.0","method":"b","params":{"x":"e"}}"#;
    let mut wire_bytes = format!(
        "Content-Length: {}\r\ncontent-type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
        first.len()
    )
    .into_bytes();
    wire_bytes.extend_from_slice(first);
    wire_bytes.extend_from_slice(&encode_frame(second));
    let expected = [Frame::Message(first.to_vec()), Frame::Message(second.to_vec())];
    // Joined, then split at every byte, then in pieces of 7.
    for size in [wire_bytes.len(), 1, 7] {
        let mut reader = FrameReader::new(1 << 20);
        let mut got = Vec::new();
        for piece in wire_bytes.chunks(size) {
            reader.push(piece);
            while let Some(frame) = reader.next_frame().unwrap() {
                got.push(frame);
            }
        }
        assert_eq!(got, expected, "pieces of {size}");
    }
    // Headers longer than 8 KiB, and one with no length, are refused.
    let mut reader = FrameReader::new(1 << 20);
    reader.push(&vec![b'a'; 9000]);
    assert_eq!(reader.next_frame(), Err(FrameError::HeadersTooLong));
    let mut reader = FrameReader::new(1 << 20);
    reader.push(b"Content-Type: application/vscode-jsonrpc\r\n\r\n{}");
    assert_eq!(reader.next_frame(), Err(FrameError::NoContentLength));
}

#[test]
fn a_malformed_header_ends_the_bridge_with_a_line() {
    for bad in [
        "Content-Length: twelve\r\n\r\n{}",
        "Content-Type: x\r\n\r\n{}",
        "no colon here\r\n\r\n",
    ] {
        let mut reader = FrameReader::new(1 << 20);
        reader.push(bad.as_bytes());
        assert!(reader.next_frame().is_err(), "{bad:?}");
    }
    // Through a session: the bridge ends with exit code 1 and one line on stderr.
    let (stdin, mut writer) = std::io::pipe().unwrap();
    let (stdout, stderr) = (Shared::default(), Shared::default());
    let io = Io {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout.clone()),
        stderr: Box::new(stderr.clone()),
    };
    let redial: Redial = Box::new(|| Err(DialError::Unreachable("no helper in this test".into())));
    let session =
        std::thread::spawn(move || session::run(quick_options(), io, None, redial, Box::new(FakeDisk::default())));
    writer.write_all(b"Content-Length: twelve\r\n\r\n").unwrap();
    assert_eq!(session.join().unwrap(), 1);
    let text = stderr.text();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(
        text.contains("eitri host lsp:") && text.contains("Content-Length"),
        "{text}"
    );
    assert!(stdout.text().is_empty());
}

#[test]
fn in_flight_requests_get_content_modified_when_the_link_drops() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    let out = bridge.from_nvim(request(7, "textDocument/hover", json!({})));
    assert_eq!(methods(&out), ["textDocument/hover"]);
    let out = bridge.link_lost();
    assert_eq!(out.to_nvim.len(), 1);
    assert_eq!(out.to_nvim[0]["id"], 7);
    assert_eq!(out.to_nvim[0]["error"]["code"], -32801);
    // A late answer to it, after the link is back, is dropped.
    bridge.link_back(&attached(true));
    let out = bridge.from_server(response(7, json!(null)));
    assert!(out.to_nvim.is_empty());
}

#[test]
fn requests_while_detached_are_answered_at_once() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    bridge.link_lost();
    let out = bridge.from_nvim(request(9, "textDocument/completion", json!({})));
    assert!(out.to_server.is_empty());
    assert_eq!(out.to_nvim.len(), 1);
    assert_eq!(out.to_nvim[0]["id"], 9);
    assert_eq!(out.to_nvim[0]["error"]["code"], -32801);
}

#[test]
fn resync_reopens_every_open_document_with_its_current_text() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    open_doc(&mut bridge, &disk, "a.rs", "fn a() {}");
    open_doc(&mut bridge, &disk, "b.rs", "fn b() {}");
    bridge.from_nvim(did_change(&uri("a.rs"), 2, json!([{"text": "fn a2() {}"}])));
    bridge.link_lost();
    let out = bridge.link_back(&attached(true));
    assert_eq!(
        methods(&out),
        [
            "textDocument/didClose",
            "textDocument/didOpen",
            "textDocument/didClose",
            "textDocument/didOpen"
        ]
    );
    let open = &out.to_server[1].message["params"]["textDocument"];
    assert_eq!(open["uri"], uri("a.rs"));
    assert_eq!(open["text"], "fn a2() {}");
    assert_eq!(open["version"], 2);
    assert_eq!(open["languageId"], "rust");
    assert_eq!(out.to_server[3].message["params"]["textDocument"]["uri"], uri("b.rs"));
    // `initialized` was delivered before the loss, so it is not sent again.
    assert!(!methods(&out).contains(&"initialized".to_owned()));
}

/// Each message to the server as (method, document uri).
fn documents_sent(out: &Out) -> Vec<(String, String)> {
    out.to_server
        .iter()
        .map(|sent| {
            (
                sent.message["method"].as_str().unwrap_or("<response>").to_owned(),
                sent.message["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect()
}

fn pair(method: &str, rel: &str) -> (String, String) {
    (format!("textDocument/{method}"), uri(rel))
}

#[test]
fn resync_reopens_only_documents_nvim_still_has() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    for rel in ["a.rs", "b.rs", "c.rs"] {
        open_doc(&mut bridge, &disk, rel, "text");
    }
    bridge.link_lost();
    // While the link was down Neovim closed one (`:bd!` of an unsaved buffer), the agent moved another's file away on
    // the server and the mirror followed (the buffer stays open), and Neovim opened a buffer not yet written.
    let quiet = bridge.from_nvim(notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri("b.rs")}}),
    ));
    disk.remove("c.rs");
    let quiet_open = bridge.from_nvim(did_open(&uri("d.rs"), "not yet written"));
    assert!(quiet.to_server.is_empty() && quiet_open.to_server.is_empty());
    let out = bridge.link_back(&attached(true));
    // The server's open documents follow Neovim's: the closed one is closed there, the others are opened with their
    // text whether or not their file exists, and the new one gets no close it never had an open for.
    assert_eq!(
        documents_sent(&out),
        [
            pair("didClose", "b.rs"),
            pair("didClose", "a.rs"),
            pair("didOpen", "a.rs"),
            pair("didClose", "c.rs"),
            pair("didOpen", "c.rs"),
            pair("didOpen", "d.rs"),
        ]
    );
    // Live again: a close of a document the server holds goes, and the server's set stays Neovim's on the next resync.
    let out = bridge.from_nvim(notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri("c.rs")}}),
    ));
    assert_eq!(documents_sent(&out), [pair("didClose", "c.rs")]);
    bridge.link_lost();
    let out = bridge.link_back(&attached(true));
    assert_eq!(
        documents_sent(&out),
        [
            pair("didClose", "a.rs"),
            pair("didOpen", "a.rs"),
            pair("didClose", "d.rs"),
            pair("didOpen", "d.rs"),
        ]
    );
}

#[test]
fn incremental_changes_while_detached_are_applied_utf16() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    // The pair counts as two UTF-16 units: character 3 is the `b`.
    open_doc(&mut bridge, &disk, "a.rs", "a\u{1F600}b\nsecond line\n");
    bridge.link_lost();
    let range = |line: u64, from: u64, to: u64| json!({"start": {"line": line, "character": from}, "end": {"line": line, "character": to}});
    bridge.from_nvim(did_change(
        &uri("a.rs"),
        2,
        json!([{"range": range(0, 3, 4), "text": "XY"}]),
    ));
    bridge.from_nvim(did_change(
        &uri("a.rs"),
        3,
        json!([{"range": range(1, 0, 6), "text": "THIRD"}]),
    ));
    let out = bridge.link_back(&attached(true));
    let open = &out.to_server[1].message["params"]["textDocument"];
    assert_eq!(open["text"], "a\u{1F600}XY\nTHIRD line\n");
    assert_eq!(open["version"], 3);
}

#[test]
fn a_save_while_detached_is_saved_again_after_resync() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    open_doc(&mut bridge, &disk, "a.rs", "old");
    bridge.link_lost();
    bridge.from_nvim(did_change(&uri("a.rs"), 2, json!([{"text": "new"}])));
    disk.put("a.rs", "new");
    let out = bridge.from_nvim(did_save(&uri("a.rs")));
    assert!(out.to_server.is_empty(), "nothing is sent while the link is down");
    let out = bridge.link_back(&attached(true));
    assert_eq!(
        methods(&out),
        ["textDocument/didClose", "textDocument/didOpen", "textDocument/didSave"]
    );
    assert_eq!(out.to_server[1].message["params"]["textDocument"]["text"], "new");
    assert_eq!(out.to_server[2].saved_sha256.as_deref(), Some(sha("new").as_str()));
}

#[test]
fn did_save_carries_the_files_hash() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    disk.put("src/a.rs", "saved bytes");
    let out = bridge.from_nvim(did_save(&uri("src/a.rs")));
    assert_eq!(
        out.to_server[0].saved_sha256.as_deref(),
        Some(sha("saved bytes").as_str())
    );
    // A path sync does not carry (excluded), and a file that cannot be read, go without a hash.
    disk.put("target/debug/out.rs", "generated");
    let out = bridge.from_nvim(did_save(&uri("target/debug/out.rs")));
    assert_eq!(out.to_server[0].saved_sha256, None);
    let out = bridge.from_nvim(did_save(&uri("missing.rs")));
    assert_eq!(out.to_server[0].saved_sha256, None);
    let out = bridge.from_nvim(did_save("file:///elsewhere/a.rs"));
    assert_eq!(out.to_server[0].saved_sha256, None);
}

#[test]
fn a_save_the_helper_holds_asks_sync_to_flush_once_and_waits_for_nothing() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    disk.put("src/a.rs", "saved bytes");
    let out = bridge.from_nvim(did_save(&uri("src/a.rs")));
    // The save goes now, in the same output; the flush is only asked for.
    assert_eq!(methods(&out), ["textDocument/didSave"]);
    assert!(out.to_server[0].saved_sha256.is_some());
    assert_eq!(out.actions, [Action::FlushSync]);
    // A save the helper does not hold (no hash: excluded, unreadable, outside the root) asks for nothing.
    disk.put("target/debug/out.rs", "generated");
    for rel in ["target/debug/out.rs", "missing.rs"] {
        let out = bridge.from_nvim(did_save(&uri(rel)));
        assert_eq!(methods(&out), ["textDocument/didSave"]);
        assert!(out.actions.is_empty(), "{rel}: {:?}", out.actions);
    }
    let out = bridge.from_nvim(did_save("file:///elsewhere/a.rs"));
    assert!(out.actions.is_empty(), "{:?}", out.actions);
    // Saves owed across a loss are sent again at the resync with one flush for all of them.
    open_doc(&mut bridge, &disk, "a.rs", "a");
    open_doc(&mut bridge, &disk, "b.rs", "b");
    bridge.link_lost();
    assert!(
        bridge.from_nvim(did_save(&uri("a.rs"))).actions.is_empty(),
        "nothing to flush for while down"
    );
    bridge.from_nvim(did_save(&uri("b.rs")));
    let out = bridge.link_back(&attached(true));
    assert_eq!(
        methods(&out)
            .iter()
            .filter(|method| *method == "textDocument/didSave")
            .count(),
        2,
        "a.rs and b.rs, the documents Neovim has: {:?}",
        methods(&out)
    );
    assert_eq!(out.actions, [Action::FlushSync]);
}

#[test]
fn registrations_made_while_detached_are_replayed_to_nvim_and_their_replies_swallowed() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    bridge.link_lost();
    let mut back = attached(true);
    back.registrations.push(registration(
        "client/registerCapability",
        json!({"registrations": [{"id": "w1", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": []}}]}),
    ));
    let out = bridge.link_back(&back);
    assert_eq!(out.to_nvim.len(), 1);
    let replayed = &out.to_nvim[0];
    assert_eq!(replayed["method"], "client/registerCapability");
    let id = replayed["id"].as_str().unwrap();
    assert!(id.starts_with("eitri-"), "{id}");
    assert_eq!(replayed["params"]["registrations"][0]["id"], "w1");
    // Neovim's reply to it goes nowhere.
    let out = bridge.from_nvim(response(id, json!(null)));
    assert!(out.to_server.is_empty() && out.to_nvim.is_empty());
}

#[test]
fn a_duplicated_replay_registers_once() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    let mut back = attached(true);
    back.registrations.push(registration(
        "client/registerCapability",
        json!({"registrations": [{"id": "w1", "method": "workspace/didChangeWatchedFiles"}]}),
    ));
    bridge.link_lost();
    assert_eq!(bridge.link_back(&back).to_nvim.len(), 1);
    // The helper's replay arrives again after a failed attach: Neovim already holds `w1`.
    bridge.link_lost();
    assert!(bridge.link_back(&back).to_nvim.is_empty());
}

#[test]
fn an_unregistration_replays_as_an_unregistration() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    let out = bridge.from_server(request(
        50,
        "client/registerCapability",
        json!({"registrations": [{"id": "w1", "method": "workspace/didChangeWatchedFiles"}]}),
    ));
    assert_eq!(out.to_nvim.len(), 1);
    bridge.from_nvim(response(50, json!(null)));
    bridge.link_lost();
    let mut back = attached(true);
    back.registrations.push(registration(
        "client/unregisterCapability",
        json!({"unregisterations": [{"id": "w1", "method": "workspace/didChangeWatchedFiles"}]}),
    ));
    let out = bridge.link_back(&back);
    assert_eq!(out.to_nvim.len(), 1);
    assert_eq!(out.to_nvim[0]["method"], "client/unregisterCapability");
    assert_eq!(out.to_nvim[0]["params"]["unregisterations"][0]["id"], "w1");
}

#[test]
fn a_fresh_server_gets_the_original_initialize_and_the_documents() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    bridge.from_nvim(notification(
        "workspace/didChangeConfiguration",
        json!({"settings": {"a": 1}}),
    ));
    open_doc(&mut bridge, &disk, "a.rs", "fn a() {}");
    open_doc(&mut bridge, &disk, "b.rs", "fn b() {}");
    bridge.from_nvim(request(7, "textDocument/hover", json!({})));
    let out = bridge.server_ended(&ended(LspEndReason::Grace, ""), Instant::now());
    assert_eq!(out.actions, [Action::OpenServer]);
    assert_eq!(out.to_nvim[0]["id"], 7);
    assert_eq!(out.to_nvim[0]["error"]["code"], -32801);
    // A request while the fresh server is being made waits for nothing.
    let during = bridge.from_nvim(request(8, "textDocument/hover", json!({})));
    assert_eq!(during.to_nvim[0]["error"]["code"], -32801);
    let out = bridge.link_back(&attached(false));
    assert_eq!(methods(&out), ["initialize"]);
    let init = &out.to_server[0].message;
    let init_id = init["id"].as_str().unwrap().to_owned();
    assert!(init_id.starts_with("eitri-"), "{init_id}");
    assert_eq!(init["params"]["rootUri"], uri(""));
    assert_eq!(init["params"]["processId"], Value::Null);
    let out = bridge.from_server(response(
        init_id.as_str(),
        json!({"capabilities": {"textDocumentSync": {"openClose": true, "change": 2, "save": {}}}}),
    ));
    assert!(out.to_nvim.is_empty(), "the fresh server's answer is swallowed");
    assert_eq!(
        methods(&out),
        [
            "initialized",
            "workspace/didChangeConfiguration",
            "textDocument/didOpen",
            "textDocument/didOpen"
        ]
    );
    assert_eq!(out.to_server[1].message["params"], json!({"settings": {"a": 1}}));
    assert_eq!(out.to_server[2].message["params"]["textDocument"]["text"], "fn a() {}");
    // Traffic goes on as before.
    let out = bridge.from_nvim(request(9, "textDocument/hover", json!({})));
    assert_eq!(methods(&out), ["textDocument/hover"]);
}

#[test]
fn the_old_servers_registrations_are_withdrawn_before_the_new_ones() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    let mut nvim_saw: Vec<Value> = Vec::new();
    let out = bridge.from_server(request(
        50,
        "client/registerCapability",
        json!({"registrations": [{"id": "old", "method": "workspace/didChangeWatchedFiles"}]}),
    ));
    nvim_saw.extend(out.to_nvim);
    bridge.from_nvim(response(50, json!(null)));
    let out = bridge.server_ended(&ended(LspEndReason::Gone, ""), Instant::now());
    nvim_saw.extend(out.to_nvim);
    let out = bridge.link_back(&attached(false));
    let init_id = out.to_server[0].message["id"].clone();
    let out = bridge.from_server(response(init_id, json!({"capabilities": {}})));
    nvim_saw.extend(out.to_nvim);
    let out = bridge.from_server(request(
        51,
        "client/registerCapability",
        json!({"registrations": [{"id": "new", "method": "workspace/didChangeWatchedFiles"}]}),
    ));
    nvim_saw.extend(out.to_nvim);
    let position = |method: &str, id: &str| {
        let key = if method == "client/registerCapability" {
            "registrations"
        } else {
            "unregisterations"
        };
        nvim_saw
            .iter()
            .position(|message| message["method"] == method && message["params"][key][0]["id"] == id)
            .unwrap_or_else(|| panic!("{method} for {id} is not among {nvim_saw:?}"))
    };
    assert!(position("client/unregisterCapability", "old") < position("client/registerCapability", "new"));
}

#[test]
fn a_server_that_dies_twice_ends_the_bridge_with_its_stderr() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    let first = Instant::now();
    let out = bridge.server_ended(&ended(LspEndReason::Exited, "first"), first);
    assert_eq!(out.actions, [Action::OpenServer], "one restart");
    let out = bridge.link_back(&attached(false));
    let init_id = out.to_server[0].message["id"].clone();
    bridge.from_server(response(init_id, json!({"capabilities": {}})));
    let out = bridge.server_ended(
        &ended(LspEndReason::Exited, "thread 'main' panicked\n\u{1b}[31mboom"),
        first + Duration::from_secs(60),
    );
    assert!(out.actions.contains(&Action::Exit(1)), "{:?}", out.actions);
    assert!(!out.actions.contains(&Action::OpenServer));
    let line = out.log.join("\n");
    assert!(line.contains("stopped twice"), "{line}");
    assert!(
        line.contains("rust-analyzer") && line.contains("example-host"),
        "{line}"
    );
    assert!(line.contains("panicked") && line.contains("boom"), "{line}");
    assert_eq!(out.log.len(), 1, "one line");
    assert!(!line.contains('\u{1b}'), "hidden characters are escaped: {line:?}");
    let shown = out
        .to_nvim
        .iter()
        .find(|message| message["method"] == "window/showMessage")
        .unwrap();
    assert!(shown["params"]["message"].as_str().unwrap().contains("stopped twice"));
    // Two exits more than five minutes apart are two restarts, not two deaths.
    let mut slow = live(&disk);
    let out = slow.server_ended(&ended(LspEndReason::Exited, ""), first);
    assert_eq!(out.actions, [Action::OpenServer]);
    let out = slow.link_back(&attached(false));
    let init_id = out.to_server[0].message["id"].clone();
    slow.from_server(response(init_id, json!({"capabilities": {}})));
    let out = slow.server_ended(&ended(LspEndReason::Exited, ""), first + Duration::from_secs(6 * 60));
    assert_eq!(out.actions, [Action::OpenServer]);
    // A grace that ran out is not a death.
    let mut graceful = live(&disk);
    for _ in 0..3 {
        let out = graceful.server_ended(&ended(LspEndReason::Grace, ""), first);
        assert_eq!(out.actions, [Action::OpenServer]);
        let out = graceful.link_back(&attached(false));
        let init_id = out.to_server[0].message["id"].clone();
        graceful.from_server(response(init_id, json!({"capabilities": {}})));
    }
}

#[test]
fn a_missing_command_answers_initialize_with_the_host_and_name() {
    let disk = FakeDisk::default();
    let mut bridge = bridge_with(&disk);
    let out = bridge.from_nvim(request(1, "initialize", json!({"processId": 1, "capabilities": {}})));
    assert_eq!(out.actions, [Action::OpenServer]);
    let out = bridge.open_failed(Failure::Missing("rust-analyzer".into()));
    // A client that never stops (nvim 0.12 asserts on the error) does not keep the bridge: it ends once Neovim is quiet.
    assert_eq!(out.actions, [Action::ExitWhenQuiet(1)]);
    assert_eq!(out.to_nvim.len(), 1);
    assert_eq!(out.to_nvim[0]["id"], 1);
    assert_eq!(out.to_nvim[0]["error"]["code"], -32803);
    assert_eq!(
        out.to_nvim[0]["error"]["message"],
        "rust-analyzer is not installed on example-host (looked on the login PATH and in mason)"
    );
    // Neovim stops its client: `shutdown` is answered, and `exit` ends the bridge with 1.
    let out = bridge.from_nvim(request(2, "shutdown", json!(null)));
    assert_eq!(out.to_nvim[0]["result"], Value::Null);
    let out = bridge.from_nvim(notification("exit", json!(null)));
    assert_eq!(out.actions, [Action::Exit(1)]);
}

#[test]
fn gone_takes_the_fresh_server_path_and_replaced_does_not() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    bridge.link_lost();
    let out = bridge.server_ended(&ended(LspEndReason::Gone, ""), Instant::now());
    assert_eq!(out.actions, [Action::OpenServer]);
    let out = bridge.link_back(&attached(false));
    assert_eq!(methods(&out), ["initialize"]);

    let mut stale = live(&disk);
    let out = stale.server_ended(&ended(LspEndReason::Replaced, ""), Instant::now());
    assert!(!out.actions.contains(&Action::OpenServer), "{:?}", out.actions);
    assert!(
        !out.actions.contains(&Action::StopServer),
        "the server is another connection's now"
    );
    assert!(out.actions.contains(&Action::Exit(1)));
}

#[test]
fn process_id_is_null_on_the_server() {
    let disk = FakeDisk::default();
    let mut bridge = bridge_with(&disk);
    bridge.from_nvim(request(1, "initialize", json!({"processId": 4242, "capabilities": {}})));
    let out = bridge.link_back(&attached(false));
    assert_eq!(out.to_server[0].message["params"]["processId"], Value::Null);
    assert_eq!(out.to_server[0].message["id"], 1);
}

#[test]
fn a_dropped_held_save_is_saved_again() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    open_doc(&mut bridge, &disk, "a.rs", "saved text");
    bridge.link_lost();
    let mut back = attached(true);
    back.dropped_saves.push(uri("a.rs"));
    back.dropped_saves.push(uri("not-open.rs"));
    let out = bridge.link_back(&back);
    assert_eq!(
        methods(&out),
        ["textDocument/didClose", "textDocument/didOpen", "textDocument/didSave"]
    );
    assert_eq!(out.to_server[2].message["params"]["textDocument"]["uri"], uri("a.rs"));
    assert_eq!(
        out.to_server[2].saved_sha256.as_deref(),
        Some(sha("saved text").as_str())
    );
}

#[test]
fn a_message_over_the_cap_from_nvim_fails_that_request() {
    // The id comes after a long `params`: the whole body has to pass to find it.
    let long = "x".repeat(300);
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/hover","params":{{"pad":"{long}","n":[1,{{"id":5}}]}},"id":12}}"#
    );
    let notification_body = format!(r#"{{"jsonrpc":"2.0","method":"$/progress","params":{{"pad":"{long}"}}}}"#);
    let small = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
    let mut bytes = encode_frame(body.as_bytes());
    bytes.extend(encode_frame(notification_body.as_bytes()));
    bytes.extend(encode_frame(small.as_bytes()));
    let mut reader = FrameReader::new(100);
    let mut frames = Vec::new();
    for piece in bytes.chunks(13) {
        reader.push(piece);
        while let Some(frame) = reader.next_frame().unwrap() {
            frames.push(frame);
        }
    }
    assert_eq!(frames.len(), 3, "the stream goes on past what it skipped");
    assert_eq!(frames[2], Frame::Message(small.as_bytes().to_vec()));
    let (Frame::Oversized(request_probe), Frame::Oversized(notification_probe)) = (&frames[0], &frames[1]) else {
        panic!("{frames:?}")
    };
    assert_eq!(request_probe.id.as_deref(), Some("12"));

    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    let out = bridge.from_nvim_oversized(request_probe.clone());
    assert!(out.to_server.is_empty());
    assert_eq!(out.to_nvim[0]["id"], 12);
    assert_eq!(out.to_nvim[0]["error"]["code"], -32803);
    // A notification is dropped with a line.
    let out = bridge.from_nvim_oversized(notification_probe.clone());
    assert!(out.to_nvim.is_empty() && out.to_server.is_empty());
    assert_eq!(out.log.len(), 1);
}

#[test]
fn an_attach_answer_past_its_bounds_ends_the_bridge() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    bridge.link_lost();
    let mut back = attached(true);
    back.registrations = (0..300)
        .map(|n| {
            registration(
                "client/registerCapability",
                json!({"registrations": [{"id": n.to_string(), "method": "m"}]}),
            )
        })
        .collect();
    let out = bridge.link_back(&back);
    assert!(out.actions.contains(&Action::Exit(1)));
    assert!(out.to_server.is_empty());
    assert!(out.log.join(" ").contains("bounds"));
}

#[test]
fn a_save_owed_and_dropped_is_sent_once_and_one_sent_before_the_loss_again() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    open_doc(&mut bridge, &disk, "a.rs", "a");
    open_doc(&mut bridge, &disk, "b.rs", "b");
    // Sent while attached, perhaps lost with the link: the helper only lists the saves it held.
    let out = bridge.from_nvim(did_save(&uri("b.rs")));
    assert_eq!(methods(&out), ["textDocument/didSave"]);
    bridge.link_lost();
    bridge.from_nvim(did_save(&uri("a.rs")));
    let mut back = attached(true);
    back.dropped_saves.push(uri("a.rs"));
    let out = bridge.link_back(&back);
    let saves: Vec<(String, String)> = documents_sent(&out)
        .into_iter()
        .filter(|(method, _)| method == "textDocument/didSave")
        .collect();
    assert_eq!(saves, [pair("didSave", "a.rs"), pair("didSave", "b.rs")]);
    // Re-hashed from the laptop's file at this moment.
    let a_save = out
        .to_server
        .iter()
        .find(|sent| sent.message["method"] == "textDocument/didSave")
        .unwrap();
    assert_eq!(a_save.saved_sha256.as_deref(), Some(sha("a").as_str()));
}

#[test]
fn shutdown_while_no_server_answers_is_answered_and_stops_the_server() {
    let disk = FakeDisk::default();
    // The link is down.
    let mut bridge = live(&disk);
    bridge.link_lost();
    let out = bridge.from_nvim(request(5, "shutdown", json!(null)));
    assert_eq!(out.to_nvim.len(), 1);
    assert_eq!(
        (&out.to_nvim[0]["id"], &out.to_nvim[0]["result"]),
        (&json!(5), &Value::Null)
    );
    assert!(out.to_nvim[0].get("error").is_none());
    assert_eq!(out.actions, [Action::StopServer]);
    assert!(
        bridge.link_back(&attached(true)).to_server.is_empty(),
        "nothing goes to a stopped server"
    );
    let out = bridge.from_nvim(notification("exit", json!(null)));
    assert_eq!(out.actions, [Action::Exit(0)]);
    // The first server is still being opened: the waiting `initialize` is answered too.
    let mut opening = bridge_with(&disk);
    opening.from_nvim(request(1, "initialize", json!({"processId": 1, "capabilities": {}})));
    let out = opening.from_nvim(request(2, "shutdown", json!(null)));
    let ids: Vec<&Value> = out.to_nvim.iter().map(|message| &message["id"]).collect();
    assert_eq!(ids, [&json!(2), &json!(1)]);
    assert_eq!(out.to_nvim[1]["error"]["code"], -32803);
    assert_eq!(out.actions, [Action::StopServer]);
    // Sent to the server, then the link drops before its answer.
    let mut in_flight = live(&disk);
    assert_eq!(
        methods(&in_flight.from_nvim(request(9, "shutdown", json!(null)))),
        ["shutdown"]
    );
    let out = in_flight.link_lost();
    assert_eq!(out.to_nvim[0]["result"], Value::Null);
    assert_eq!(out.actions, [Action::StopServer]);
}

#[test]
fn watched_file_changes_while_detached_reach_the_server_after_resync() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    bridge.from_server(request(
        50,
        "client/registerCapability",
        json!({"registrations": [{"id": "w", "method": "workspace/didChangeWatchedFiles"}]}),
    ));
    bridge.from_nvim(response(50, json!(null)));
    let changes = |list: Value| notification("workspace/didChangeWatchedFiles", json!({ "changes": list }));
    let before = |bridge: &mut Bridge| {
        bridge.link_lost();
        let a = bridge.from_nvim(changes(
            json!([{"uri": uri("a.rs"), "type": 1}, {"uri": uri("b.rs"), "type": 2}]),
        ));
        let b = bridge.from_nvim(changes(json!([{"uri": uri("a.rs"), "type": 3}])));
        assert!(a.to_server.is_empty() && b.to_server.is_empty());
    };
    before(&mut bridge);
    let out = bridge.link_back(&attached(true));
    let sent: Vec<&Value> = out
        .to_server
        .iter()
        .map(|sent| &sent.message)
        .filter(|message| message["method"] == "workspace/didChangeWatchedFiles")
        .collect();
    assert_eq!(sent.len(), 1, "one notification, merged per file");
    assert_eq!(
        sent[0]["params"]["changes"],
        json!([{"uri": uri("a.rs"), "type": 3}, {"uri": uri("b.rs"), "type": 2}])
    );
    // A server that does not watch files through Neovim is not told.
    let mut unwatched = live(&disk);
    before(&mut unwatched);
    assert!(!methods(&unwatched.link_back(&attached(true))).contains(&"workspace/didChangeWatchedFiles".to_owned()));
}

#[test]
fn a_fresh_server_keeps_the_clients_position_encoding() {
    let disk = FakeDisk::default();
    let start = |disk: &FakeDisk| {
        let mut bridge = bridge_with(disk);
        bridge.from_nvim(request(
            1,
            "initialize",
            json!({"processId": 1, "capabilities": {"general": {"positionEncodings": ["utf-8", "utf-16"]}}}),
        ));
        bridge.link_back(&attached(false));
        bridge.from_server(response(1, json!({"capabilities": {"positionEncoding": "utf-8"}})));
        bridge.from_nvim(notification("initialized", json!({})));
        bridge.server_ended(&ended(LspEndReason::Grace, ""), Instant::now());
        let out = bridge.link_back(&attached(false));
        let init = out.to_server[0].message.clone();
        assert_eq!(
            init["params"]["capabilities"]["general"]["positionEncodings"],
            json!(["utf-8"])
        );
        (bridge, init["id"].clone())
    };
    // A fresh server that counts another way ends the bridge with a message.
    let (mut bridge, init_id) = start(&disk);
    let out = bridge.from_server(response(init_id, json!({"capabilities": {}})));
    assert!(out.actions.contains(&Action::Exit(1)), "{:?}", out.actions);
    let line = out.log.join(" ");
    assert!(line.contains("utf-16") && line.contains("utf-8"), "{line}");
    // One that agrees goes on, and edits are still applied in UTF-8: the pair is four bytes.
    let (mut bridge, init_id) = start(&disk);
    bridge.from_server(response(
        init_id,
        json!({"capabilities": {"positionEncoding": "utf-8"}}),
    ));
    open_doc(&mut bridge, &disk, "a.rs", "a\u{1F600}b");
    bridge.link_lost();
    let range = json!({"start": {"line": 0, "character": 5}, "end": {"line": 0, "character": 6}});
    bridge.from_nvim(did_change(&uri("a.rs"), 2, json!([{"range": range, "text": "X"}])));
    let out = bridge.link_back(&attached(true));
    assert_eq!(
        out.to_server[1].message["params"]["textDocument"]["text"],
        "a\u{1F600}X"
    );
}

#[test]
fn stopped_unasked_ends_the_bridge_without_a_restart() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    let out = bridge.server_ended(&ended(LspEndReason::Stopped, ""), Instant::now());
    assert!(!out.actions.contains(&Action::OpenServer), "{:?}", out.actions);
    assert!(!out.actions.contains(&Action::StopServer));
    assert!(out.actions.contains(&Action::Exit(1)));
    assert!(out.log.join(" ").contains("was stopped"));
}

/// A `didChange` of `rel` past a reader's cap, as the reader reports it.
fn oversized_change(rel: &str) -> Probe {
    // The document's uri sits after its long text: the whole body passes the scan.
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"contentChanges":[{{"text":"{}"}}],"textDocument":{{"version":2,"uri":"{}"}}}}}}"#,
        "x".repeat(300),
        uri(rel)
    );
    let mut reader = FrameReader::new(100);
    reader.push(&encode_frame(body.as_bytes()));
    let Some(Frame::Oversized(probe)) = reader.next_frame().unwrap() else {
        panic!("skipped")
    };
    probe
}

#[test]
fn an_oversized_change_from_nvim_closes_that_document_on_the_server() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    open_doc(&mut bridge, &disk, "a.rs", "old");
    let out = bridge.from_nvim_oversized(oversized_change("a.rs"));
    assert_eq!(documents_sent(&out), [pair("didClose", "a.rs")]);
    // Its later edits have no base the server shares: not passed on, and a resync does not reopen it.
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
    let out = bridge.from_nvim(did_change(&uri("a.rs"), 3, json!([{"range": range, "text": "y"}])));
    assert!(out.to_server.is_empty());
    bridge.link_lost();
    assert!(documents_sent(&bridge.link_back(&attached(true))).is_empty());
    // A change that replaces the whole text makes it known again: the server gets the document anew.
    let out = bridge.from_nvim(did_change(&uri("a.rs"), 4, json!([{"text": "fresh"}])));
    assert_eq!(documents_sent(&out), [pair("didOpen", "a.rs")]);
    assert_eq!(out.to_server[0].message["params"]["textDocument"]["text"], "fresh");
    assert_eq!(out.to_server[0].message["params"]["textDocument"]["version"], 4);
    let out = bridge.from_nvim(did_change(&uri("a.rs"), 5, json!([{"range": range, "text": "F"}])));
    assert_eq!(documents_sent(&out), [pair("didChange", "a.rs")]);
    // A change the session could not send (too large once written out) does the same.
    let out = bridge.unsent(&did_change(&uri("a.rs"), 6, json!([{"text": "big"}])));
    assert_eq!(documents_sent(&out), [pair("didClose", "a.rs")]);
}

#[test]
fn a_document_poisoned_while_detached_is_closed_at_resync() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    open_doc(&mut bridge, &disk, "a.rs", "old");
    open_doc(&mut bridge, &disk, "b.rs", "kept");
    bridge.link_lost();
    let out = bridge.from_nvim_oversized(oversized_change("a.rs"));
    assert!(out.to_server.is_empty(), "nothing is sent while the link is down");
    // The server still holds the old text of a.rs: the resync closes it there and does not reopen it.
    let out = bridge.link_back(&attached(true));
    assert_eq!(
        documents_sent(&out),
        [
            pair("didClose", "a.rs"),
            pair("didClose", "b.rs"),
            pair("didOpen", "b.rs")
        ]
    );
}

#[test]
fn a_save_of_a_poisoned_document_is_not_sent() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    open_doc(&mut bridge, &disk, "a.rs", "old");
    let out = bridge.from_nvim_oversized(oversized_change("a.rs"));
    assert_eq!(documents_sent(&out), [pair("didClose", "a.rs")]);
    // The server holds no a.rs: a save of it is not passed on, now or after a resync.
    let out = bridge.from_nvim(did_save(&uri("a.rs")));
    assert!(out.to_server.is_empty(), "{:?}", methods(&out));
    bridge.link_lost();
    assert!(methods(&bridge.link_back(&attached(true))).is_empty());
    // One the server does hold is still saved.
    open_doc(&mut bridge, &disk, "b.rs", "kept");
    let out = bridge.from_nvim(did_save(&uri("b.rs")));
    assert_eq!(documents_sent(&out), [pair("didSave", "b.rs")]);
}

#[test]
fn an_unreadable_answer_fails_its_request() {
    let disk = FakeDisk::default();
    let mut bridge = live(&disk);
    bridge.from_nvim(request(7, "textDocument/hover", json!({})));
    let probe = eitri_core::remote_lsp::framing::probe(br#"{"jsonrpc":"2.0","id":7,"result":[[[["#);
    let out = bridge.from_server_unreadable(probe, "recursion limit exceeded");
    assert_eq!(out.to_nvim[0]["id"], 7);
    assert_eq!(out.to_nvim[0]["error"]["code"], -32803);
    // A request of the server's that cannot be read is refused, so the server does not wait either.
    let probe = eitri_core::remote_lsp::framing::probe(br#"{"jsonrpc":"2.0","id":40,"method":"m","params":[[["#);
    let out = bridge.from_server_unreadable(probe, "EOF");
    assert!(out.to_nvim.is_empty());
    assert_eq!(out.to_server[0].message["id"], 40);
    assert_eq!(out.to_server[0].message["error"]["code"], -32803);
}

#[test]
fn the_first_open_tells_the_person_it_is_waiting() {
    let disk = FakeDisk::default();
    let mut bridge = bridge_with(&disk);
    bridge.from_nvim(request(1, "initialize", json!({"processId": 1, "capabilities": {}})));
    let out = bridge.link_note("example-host cannot be reached: Connection refused");
    assert_eq!(out.log.len(), 1);
    assert_eq!(out.to_nvim.len(), 1);
    assert_eq!(out.to_nvim[0]["method"], "window/showMessage");
    let shown = out.to_nvim[0]["params"]["message"].as_str().unwrap();
    assert!(
        shown.contains("waiting") && shown.contains("Connection refused"),
        "{shown}"
    );
    // Once; and never once the client is live.
    assert!(bridge
        .link_note("example-host cannot be reached: timeout")
        .to_nvim
        .is_empty());
    let mut running = live(&disk);
    assert!(running
        .link_note("example-host cannot be reached: timeout")
        .to_nvim
        .is_empty());
}

#[test]
fn the_host_is_escaped_in_the_bounds_message() {
    let disk = FakeDisk::default();
    let mut bridge = bridge_on("example\u{1b}[31mhost", &disk);
    bridge.from_nvim(request(1, "initialize", json!({"processId": 1, "capabilities": {}})));
    let mut back = attached(false);
    back.dropped_saves = (0..2000).map(|n| format!("file:///srv/{n}")).collect();
    let out = bridge.link_back(&back);
    assert!(out.actions.contains(&Action::Exit(1)));
    for message in &out.to_nvim {
        let text = message.to_string();
        assert!(!text.contains('\u{1b}') && !text.contains("\\u001b"), "{text}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The session against a fake `Lsp` service

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn quick_options() -> Options {
    Options {
        host: "example-host".into(),
        root: ROOT.into(),
        argv: vec!["rust-analyzer".into()],
        grace_ms: 0,
        backoff: vec![Duration::from_millis(30)],
        open_deadline: Duration::from_secs(5),
        nvim_queue_max: session::NVIM_QUEUE_MAX,
        stop_signals: None,
        flush: None,
        quiet_exit: session::QUIET_EXIT,
    }
}

/// What the fake service saw.
enum Seen {
    Opened(wire::OpenServerRequest),
    Hello(usize, wire::LspAttachHello),
    Message {
        attach: usize,
        message: Value,
        sha: String,
        chunks: usize,
    },
    Stopped(wire::StopServerRequest),
}

struct Attach {
    to_client: Option<tokio::sync::mpsc::Sender<Result<wire::LspServerFrame, Status>>>,
}

#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<wire::LspServerFrame>>,
    open_response: Mutex<wire::OpenServerResponse>,
    attaches: Mutex<Vec<Attach>>,
}

#[derive(Clone)]
struct Fake {
    script: Arc<Script>,
    seen: Arc<Mutex<mpsc::Sender<Seen>>>,
}

fn attached_frame(reattached: bool) -> wire::LspServerFrame {
    wire::LspServerFrame {
        frame: Some(Down::Attached(attached(reattached))),
    }
}

#[tonic::async_trait]
impl Lsp for Fake {
    async fn open_server(
        &self,
        request: Request<wire::OpenServerRequest>,
    ) -> Result<Response<wire::OpenServerResponse>, Status> {
        let request = request.into_inner();
        let _ = self.seen.lock().unwrap().send(Seen::Opened(request));
        Ok(Response::new(self.script.open_response.lock().unwrap().clone()))
    }

    type AttachStream = tokio_stream::wrappers::ReceiverStream<Result<wire::LspServerFrame, Status>>;

    async fn attach(
        &self,
        request: Request<Streaming<wire::LspClientFrame>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let mut stream = request.into_inner();
        let hello = match stream.message().await?.and_then(|frame| frame.frame) {
            Some(Up::Hello(hello)) => hello,
            _ => return Err(Status::invalid_argument("the first frame is the hello")),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let number = {
            let mut attaches = self.script.attaches.lock().unwrap();
            attaches.push(Attach {
                to_client: Some(tx.clone()),
            });
            attaches.len() - 1
        };
        let seen = self.seen.lock().unwrap().clone();
        let _ = seen.send(Seen::Hello(number, hello));
        let first = self
            .script
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| attached_frame(false));
        tx.send(Ok(first)).await.map_err(|_| Status::cancelled("gone"))?;
        tokio::spawn(async move {
            let (mut bytes, mut parts) = (Vec::new(), 0usize);
            while let Ok(Some(frame)) = stream.message().await {
                let Some(Up::Chunk(chunk)) = frame.frame else { continue };
                bytes.extend_from_slice(&chunk.data);
                parts += 1;
                if chunk.last {
                    let message = serde_json::from_slice(&bytes).expect("the bridge sends JSON");
                    let _ = seen.send(Seen::Message {
                        attach: number,
                        message,
                        sha: chunk.saved_sha256,
                        chunks: parts,
                    });
                    (bytes, parts) = (Vec::new(), 0);
                }
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn list_servers(
        &self,
        _: Request<wire::ListServersRequest>,
    ) -> Result<Response<wire::ListServersResponse>, Status> {
        Err(Status::unimplemented("not in these tests"))
    }

    /// As the helper does: the attached bridge's stream gets `LspEnded{STOPPED}` before the stop is answered (the
    /// pause lets that frame reach the bridge first, as it does from a server that still has to reap its process).
    async fn stop_server(
        &self,
        request: Request<wire::StopServerRequest>,
    ) -> Result<Response<wire::StopServerResponse>, Status> {
        let _ = self.seen.lock().unwrap().send(Seen::Stopped(request.into_inner()));
        let attached = self
            .script
            .attaches
            .lock()
            .unwrap()
            .iter_mut()
            .rev()
            .find_map(|attach| attach.to_client.take());
        if let Some(stream) = attached {
            let frame = wire::LspServerFrame {
                frame: Some(Down::Ended(ended(LspEndReason::Stopped, ""))),
            };
            let _ = stream.send(Ok(frame)).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(Response::new(wire::StopServerResponse {}))
    }

    type ReadPathStream = tokio_stream::wrappers::ReceiverStream<Result<wire::ReadPathChunk, Status>>;

    async fn read_path(&self, _: Request<wire::ReadPathRequest>) -> Result<Response<Self::ReadPathStream>, Status> {
        Err(Status::unimplemented("not in these tests"))
    }
}

struct Fixture {
    served: in_process::InProcess,
    script: Arc<Script>,
    seen: mpsc::Receiver<Seen>,
    log: Vec<String>,
    opens: Vec<wire::OpenServerRequest>,
    hellos: Vec<wire::LspAttachHello>,
    stops: Vec<wire::StopServerRequest>,
}

impl Fixture {
    fn new() -> Self {
        let (tx, seen) = mpsc::channel();
        let script = Arc::new(Script::default());
        *script.open_response.lock().unwrap() = wire::OpenServerResponse {
            server_id: "s1".into(),
            resolved_command: "/usr/bin/rust-analyzer".into(),
            ..Default::default()
        };
        let fake = Fake {
            script: Arc::clone(&script),
            seen: Arc::new(Mutex::new(tx)),
        };
        Self {
            served: in_process::serve(LspServer::new(fake)),
            script,
            seen,
            log: Vec::new(),
            opens: Vec::new(),
            hellos: Vec::new(),
            stops: Vec::new(),
        }
    }

    fn dial(&self) -> Dial {
        Dial {
            channel: self.served.channel.clone(),
            handle: self.served.handle(),
            owner: Box::new(()),
        }
    }

    fn next(&mut self) -> Seen {
        let seen = self
            .seen
            .recv_timeout(HANG)
            .unwrap_or_else(|_| panic!("nothing came; so far: {:?}", self.log));
        self.log.push(match &seen {
            Seen::Opened(request) => {
                self.opens.push(request.clone());
                "open".into()
            }
            Seen::Hello(n, hello) => {
                assert_eq!(*n, self.hellos.len(), "attaches are numbered in order");
                self.hellos.push(hello.clone());
                format!("hello {n}")
            }
            Seen::Message { message, .. } => message["method"].as_str().unwrap_or("<response>").to_owned(),
            Seen::Stopped(request) => {
                self.stops.push(request.clone());
                "stop".into()
            }
        });
        seen
    }

    /// The next message the bridge sent, as (attach number, message, hash, chunks).
    fn message(&mut self) -> (usize, Value, String, usize) {
        loop {
            if let Seen::Message {
                attach,
                message,
                sha,
                chunks,
            } = self.next()
            {
                return (attach, message, sha, chunks);
            }
        }
    }

    fn message_named(&mut self, method: &str) -> (usize, Value, String, usize) {
        loop {
            let found = self.message();
            if found.1["method"] == method {
                return found;
            }
        }
    }

    /// The hello of attach `number`, waited for.
    fn hello(&mut self, number: usize) -> wire::LspAttachHello {
        while self.hellos.len() <= number {
            self.next();
        }
        self.hellos[number].clone()
    }

    /// The first stop the bridge asked for, waited for.
    fn stop(&mut self) -> wire::StopServerRequest {
        while self.stops.is_empty() {
            self.next();
        }
        self.stops[0].clone()
    }

    /// A message from the language server on attach `number`.
    fn serve(&self, number: usize, message: &Value) {
        self.serve_raw(number, &serde_json::to_vec(message).unwrap());
    }

    /// A body from the language server on attach `number`, as it is.
    fn serve_raw(&self, number: usize, body: &[u8]) {
        let sender = self.script.attaches.lock().unwrap()[number].to_client.clone().unwrap();
        for (data, last) in chunks(body) {
            let frame = wire::LspServerFrame {
                frame: Some(Down::Chunk(wire::LspChunk {
                    data: data.to_vec(),
                    last,
                    saved_sha256: String::new(),
                })),
            };
            sender.blocking_send(Ok(frame)).expect("the bridge reads its stream");
        }
    }

    /// The stream of attach `number` ends without an `LspEnded`: the link is lost.
    fn cut(&self, number: usize) {
        self.script.attaches.lock().unwrap()[number].to_client = None;
    }
}

struct Nvim {
    stdin: std::io::PipeWriter,
    out: Shared,
    err: Shared,
    taken: usize,
    reader: FrameReader,
    session: Option<std::thread::JoinHandle<i32>>,
}

impl Nvim {
    fn start(fixture: &Fixture, disk: &FakeDisk) -> Self {
        Self::start_with(fixture, disk, quick_options(), None)
    }

    /// With `stdout` in place of the shared buffer the test reads, when given.
    fn start_with(fixture: &Fixture, disk: &FakeDisk, options: Options, stdout: Option<Box<dyn Write + Send>>) -> Self {
        Self::start_owning(fixture, disk, options, stdout, Box::new(()))
    }

    /// With `owner` keeping the first connection, so a test sees when the bridge lets it go.
    fn start_owning(
        fixture: &Fixture,
        disk: &FakeDisk,
        options: Options,
        stdout: Option<Box<dyn Write + Send>>,
        owner: Box<dyn std::any::Any + Send>,
    ) -> Self {
        let (stdin_read, stdin) = std::io::pipe().unwrap();
        let (out, err) = (Shared::default(), Shared::default());
        let io = Io {
            stdin: Box::new(stdin_read),
            stdout: stdout.unwrap_or_else(|| Box::new(out.clone())),
            stderr: Box::new(err.clone()),
        };
        let (channel, handle) = (fixture.served.channel.clone(), fixture.served.handle());
        let redial: Redial = Box::new(move || {
            Ok(Dial {
                channel: channel.clone(),
                handle: handle.clone(),
                owner: Box::new(()),
            })
        });
        let first = Some(Dial {
            owner,
            ..fixture.dial()
        });
        let disk = Box::new(disk.clone());
        let session = std::thread::spawn(move || session::run(options, io, first, redial, disk));
        Self {
            stdin,
            out,
            err,
            taken: 0,
            reader: FrameReader::new(1 << 27),
            session: Some(session),
        }
    }

    fn send(&mut self, message: &Value) {
        self.stdin
            .write_all(&encode_frame(&serde_json::to_vec(message).unwrap()))
            .unwrap();
    }

    /// The next message the bridge wrote to its stdout.
    fn message(&mut self) -> Value {
        let until = Instant::now() + HANG;
        loop {
            if let Some(Frame::Message(body)) = self.reader.next_frame().unwrap() {
                return serde_json::from_slice(&body).unwrap();
            }
            assert!(Instant::now() < until, "nothing came; stderr: {}", self.err.text());
            let bytes = self.out.0.lock().unwrap().clone();
            self.reader.push(&bytes[self.taken..]);
            self.taken = bytes.len();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(mut self) -> i32 {
        drop(self.stdin);
        self.session.take().unwrap().join().unwrap()
    }
}

fn initialize_and_open(fixture: &mut Fixture, nvim: &mut Nvim, disk: &FakeDisk) {
    nvim.send(&request(
        1,
        "initialize",
        json!({"processId": 99, "rootUri": uri(""), "capabilities": {}}),
    ));
    let (attach, init, _, _) = fixture.message_named("initialize");
    assert_eq!(attach, 0);
    assert_eq!(init["params"]["processId"], Value::Null);
    fixture.serve(
        0,
        &response(
            init["id"].clone(),
            json!({"capabilities": {"textDocumentSync": {"openClose": true, "change": 2}}}),
        ),
    );
    let answer = nvim.message();
    assert_eq!(answer["id"], 1);
    nvim.send(&notification("initialized", json!({})));
    disk.put("a.rs", "fn a() {}");
    nvim.send(&did_open(&uri("a.rs"), "fn a() {}"));
    fixture.message_named("textDocument/didOpen");
}

#[test]
fn shutdown_stops_the_server_and_exits_zero() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let mut nvim = Nvim::start(&fixture, &disk);
    initialize_and_open(&mut fixture, &mut nvim, &disk);
    nvim.send(&request(2, "shutdown", json!(null)));
    let (_, shutdown, _, _) = fixture.message_named("shutdown");
    fixture.serve(0, &response(shutdown["id"].clone(), json!(null)));
    assert_eq!(nvim.message()["id"], 2);
    nvim.send(&notification("exit", json!(null)));
    fixture.message_named("exit");
    let stop = fixture.stop();
    assert_eq!(stop.server_id, "s1");
    assert_eq!(stop.command_id.len(), 32);
    assert_eq!(nvim.finish(), 0);
}

#[test]
fn the_link_dropping_reattaches_and_resyncs_through_the_service() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let mut nvim = Nvim::start(&fixture, &disk);
    initialize_and_open(&mut fixture, &mut nvim, &disk);
    // The open carried the client id and a command id of 128 bits, and the root and argv.
    let opened = fixture.opens[0].clone();
    assert_eq!(
        (opened.root.as_str(), opened.argv.as_slice()),
        (ROOT, ["rust-analyzer".to_owned()].as_slice())
    );
    assert_eq!((opened.client_id.len(), opened.command_id.len()), (32, 32));
    fixture.script.answers.lock().unwrap().push_back(attached_frame(true));
    fixture.cut(0);
    // The same server again, as the same client; then what the server may have missed.
    let hello = fixture.hello(1);
    assert_eq!(
        (hello.server_id.as_str(), hello.client_id.as_str()),
        ("s1", opened.client_id.as_str())
    );
    let (_, close, _, _) = fixture.message();
    assert_eq!(close["method"], "textDocument/didClose");
    let (attach, open, _, _) = fixture.message();
    assert_eq!((attach, open["method"].as_str()), (1, Some("textDocument/didOpen")));
    assert_eq!(open["params"]["textDocument"]["text"], "fn a() {}");
    // Traffic is live again.
    nvim.send(&request(3, "textDocument/hover", json!({})));
    let (attach, hover, _, _) = fixture.message_named("textDocument/hover");
    assert_eq!(attach, 1);
    fixture.serve(1, &response(hover["id"].clone(), json!({"contents": "ok"})));
    assert_eq!(nvim.message()["id"], 3);
    nvim.send(&notification("exit", json!(null)));
    assert_eq!(nvim.finish(), 0);
}

#[test]
fn a_gone_server_after_the_link_drops_gets_a_fresh_one() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let mut nvim = Nvim::start(&fixture, &disk);
    initialize_and_open(&mut fixture, &mut nvim, &disk);
    // The reattach finds the server gone (a daemon restart): one `GONE` and the stream ends.
    fixture.script.answers.lock().unwrap().push_back(wire::LspServerFrame {
        frame: Some(Down::Ended(ended(LspEndReason::Gone, ""))),
    });
    fixture.script.open_response.lock().unwrap().server_id = "s2".into();
    fixture.cut(0);
    // A fresh server: the original initialize again (under the bridge's own id), the result swallowed, the document.
    let (attach, init, _, _) = fixture.message_named("initialize");
    assert_eq!(attach, 2, "attach 1 was told GONE");
    assert!(init["id"].as_str().unwrap().starts_with("eitri-"));
    fixture.serve(2, &response(init["id"].clone(), json!({"capabilities": {}})));
    fixture.message_named("initialized");
    let (_, open, _, _) = fixture.message_named("textDocument/didOpen");
    assert_eq!(open["params"]["textDocument"]["uri"], uri("a.rs"));
    nvim.send(&notification("exit", json!(null)));
    assert_eq!(nvim.finish(), 0);
}

#[test]
fn big_messages_travel_in_chunks_both_ways() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let mut nvim = Nvim::start(&fixture, &disk);
    initialize_and_open(&mut fixture, &mut nvim, &disk);
    let text = "x".repeat(2_500_000);
    nvim.send(&did_change(&uri("a.rs"), 2, json!([{"text": text}])));
    let (_, change, _, parts) = fixture.message_named("textDocument/didChange");
    assert_eq!(
        change["params"]["contentChanges"][0]["text"].as_str().unwrap().len(),
        2_500_000
    );
    assert_eq!(parts, 3, "1 MiB at most in a chunk");
    let big = json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}});
    fixture.serve(0, &big);
    let got = nvim.message();
    assert_eq!(got["params"]["message"].as_str().unwrap().len(), 2_500_000);
    nvim.send(&notification("exit", json!(null)));
    assert_eq!(nvim.finish(), 0);
}

#[test]
fn a_missing_command_through_the_service_answers_initialize() {
    let fixture = Fixture::new();
    *fixture.script.open_response.lock().unwrap() = wire::OpenServerResponse {
        missing_command: "rust-analyzer".into(),
        ..Default::default()
    };
    let disk = FakeDisk::default();
    let mut nvim = Nvim::start(&fixture, &disk);
    nvim.send(&request(1, "initialize", json!({"processId": 99, "capabilities": {}})));
    let answer = nvim.message();
    assert_eq!(answer["id"], 1);
    assert_eq!(
        answer["error"]["message"],
        "rust-analyzer is not installed on example-host (looked on the login PATH and in mason)"
    );
    nvim.send(&request(2, "shutdown", json!(null)));
    assert_eq!(nvim.message()["result"], Value::Null);
    nvim.send(&notification("exit", json!(null)));
    assert_eq!(nvim.finish(), 1);
    assert!(fixture.hello_count() == 0);
}

/// Says on a channel when it is dropped: the connection it stands for was let go.
struct DropSignal(mpsc::Sender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
fn a_missing_command_lets_the_connection_go_and_ends_when_nvim_stays_quiet() {
    let fixture = Fixture::new();
    *fixture.script.open_response.lock().unwrap() = wire::OpenServerResponse {
        missing_command: "rust-analyzer".into(),
        ..Default::default()
    };
    let disk = FakeDisk::default();
    let (dropped_tx, dropped) = mpsc::channel();
    let options = Options {
        quiet_exit: Duration::from_secs(3),
        ..quick_options()
    };
    let mut nvim = Nvim::start_owning(&fixture, &disk, options, None, Box::new(DropSignal(dropped_tx)));
    nvim.send(&request(1, "initialize", json!({"processId": 99, "capabilities": {}})));
    assert!(nvim.message()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("is not installed"));
    // The helper connection goes with the answer, whatever Neovim does next.
    dropped
        .recv_timeout(HANG)
        .expect("the connection is let go after the failed open");
    // nvim 0.12 sends nothing after the failure; anything Neovim sends starts the wait again: a second after the
    // failure, so the bridge is still there half a second past the first wait's end.
    std::thread::sleep(Duration::from_millis(1_000));
    nvim.send(&request(2, "textDocument/hover", json!({})));
    assert_eq!(nvim.message()["id"], 2);
    std::thread::sleep(Duration::from_millis(2_500));
    assert!(
        !nvim.session.as_ref().unwrap().is_finished(),
        "the hover started the wait again; stderr: {}",
        nvim.err.text()
    );
    // Then nothing: the bridge ends by itself with 1, stdin still open.
    let session = nvim.session.take().unwrap();
    let until = Instant::now() + HANG;
    while !session.is_finished() {
        assert!(
            Instant::now() < until,
            "the bridge did not end; stderr: {}",
            nvim.err.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.join().unwrap(), 1);
    assert!(nvim.err.text().contains("sent nothing"), "{}", nvim.err.text());
    assert_eq!(fixture.hello_count(), 0);
}

/// A [`session::Flush`] that blocks until the test lets it go, counting its calls.
#[derive(Clone, Default)]
struct HeldFlush {
    calls: Arc<Mutex<usize>>,
    gate: Arc<(Mutex<usize>, std::sync::Condvar)>,
}

impl HeldFlush {
    fn flush(&self) -> session::Flush {
        let held = self.clone();
        Box::new(move || {
            *held.calls.lock().unwrap() += 1;
            let (lock, wake) = &*held.gate;
            let mut open = wake.wait_while(lock.lock().unwrap(), |open| *open == 0).unwrap();
            *open -= 1;
            Err("not this time".into())
        })
    }

    /// Lets one flush end.
    fn let_one_go(&self) {
        *self.gate.0.lock().unwrap() += 1;
        self.gate.1.notify_all();
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }

    fn wait_for_calls(&self, calls: usize) {
        let until = Instant::now() + HANG;
        while self.calls() < calls {
            assert!(Instant::now() < until, "{} flushes, not {calls}", self.calls());
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn a_held_save_asks_sync_to_flush_without_waiting_for_it_and_saves_meanwhile_ask_once_more() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let held = HeldFlush::default();
    let options = Options {
        flush: Some(held.flush()),
        ..quick_options()
    };
    let mut nvim = Nvim::start_with(&fixture, &disk, options, None);
    initialize_and_open(&mut fixture, &mut nvim, &disk);
    assert_eq!(held.calls(), 0, "opening and editing ask for no flush");
    disk.put("a.rs", "fn a() { 1 }");
    nvim.send(&did_save(&uri("a.rs")));
    let (_, _, sha_sent, _) = fixture.message_named("textDocument/didSave");
    assert_eq!(sha_sent, sha("fn a() { 1 }"));
    held.wait_for_calls(1);
    // The flush is still running: the bridge goes on with Neovim's traffic and the next saves meanwhile.
    nvim.send(&request(7, "textDocument/hover", json!({})));
    fixture.message_named("textDocument/hover");
    for text in ["fn a() { 2 }", "fn a() { 3 }"] {
        disk.put("a.rs", text);
        nvim.send(&did_save(&uri("a.rs")));
        assert_eq!(fixture.message_named("textDocument/didSave").2, sha(text));
    }
    assert_eq!(held.calls(), 1, "one flush at a time");
    // The two saves made during the first flush are answered by one more.
    held.let_one_go();
    held.wait_for_calls(2);
    held.let_one_go();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(held.calls(), 2);
    // A failure is said once on the bridge's stderr, not once per flush.
    assert_eq!(
        nvim.err.text().matches("did not (not this time)").count(),
        1,
        "{}",
        nvim.err.text()
    );
    nvim.send(&notification("exit", json!(null)));
    held.let_one_go();
    assert_eq!(nvim.finish(), 0);
}

impl Fixture {
    fn hello_count(&self) -> usize {
        self.script.attaches.lock().unwrap().len()
    }
}

#[test]
fn a_link_dropped_before_the_initialize_answer_gets_a_fresh_server() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let mut nvim = Nvim::start(&fixture, &disk);
    nvim.send(&request(1, "initialize", json!({"processId": 99, "capabilities": {}})));
    let (attach, _, _, _) = fixture.message_named("initialize");
    assert_eq!(attach, 0);
    // The link drops before the server answers; the reattach finds the same server, which answered into the void.
    fixture.script.answers.lock().unwrap().push_back(attached_frame(true));
    fixture.script.open_response.lock().unwrap().server_id = "s2".into();
    fixture.cut(0);
    assert_eq!(fixture.hello(1).server_id, "s1");
    // That server is stopped (its stream ends with `STOPPED`, as the helper's does) and a fresh one opened, which gets
    // Neovim's own `initialize`.
    assert_eq!(fixture.stop().server_id, "s1");
    let (attach, init, _, _) = fixture.message_named("initialize");
    assert_eq!((attach, init["id"].clone()), (2, json!(1)));
    assert_eq!(fixture.hellos[2].server_id, "s2");
    fixture.serve(2, &response(1, json!({"capabilities": {}})));
    assert_eq!(nvim.message()["id"], 1);
    nvim.send(&notification("exit", json!(null)));
    assert_eq!(nvim.finish(), 0);
}

#[test]
fn an_unreadable_answer_through_the_service_fails_its_request() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let mut nvim = Nvim::start(&fixture, &disk);
    initialize_and_open(&mut fixture, &mut nvim, &disk);
    nvim.send(&request(3, "textDocument/hover", json!({})));
    fixture.message_named("textDocument/hover");
    // Past the parser's nesting limit.
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":3,"result":{}{}}}"#,
        "[".repeat(300),
        "]".repeat(300)
    );
    fixture.serve_raw(0, body.as_bytes());
    let answer = nvim.message();
    assert_eq!(
        (answer["id"].clone(), answer["error"]["code"].clone()),
        (json!(3), json!(-32803))
    );
    nvim.send(&notification("exit", json!(null)));
    assert_eq!(nvim.finish(), 0);
}

/// A stdout that takes nothing until it is let go.
#[derive(Clone, Default)]
struct Stuck(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Stuck {
    fn let_go(&self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }
}

impl Write for Stuck {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (lock, wake) = &*self.0;
        let _open = wake.wait_while(lock.lock().unwrap(), |open| !*open).unwrap();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn an_editor_that_stops_reading_ends_the_bridge_and_stops_the_server() {
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let stuck = Stuck::default();
    let options = Options {
        nvim_queue_max: 1 << 20,
        ..quick_options()
    };
    let mut nvim = Nvim::start_with(&fixture, &disk, options, Some(Box::new(stuck.clone())));
    nvim.send(&request(1, "initialize", json!({"processId": 99, "capabilities": {}})));
    let (_, init, _, _) = fixture.message_named("initialize");
    fixture.serve(0, &response(init["id"].clone(), json!({"capabilities": {}})));
    let text = "x".repeat(600 * 1024);
    for _ in 0..4 {
        fixture.serve(
            0,
            &json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}),
        );
    }
    assert_eq!(fixture.stop().server_id, "s1");
    stuck.let_go();
    let code = nvim.session.take().unwrap().join().unwrap();
    assert_eq!(code, 1);
    assert!(nvim.err.text().contains("stopped reading"), "{}", nvim.err.text());
}

/// The child half of [`a_stop_signal_stops_the_server_and_ends_the_bridge`]: a session that watches the stop signals,
/// in a process of its own.
#[test]
fn stop_signal_child() {
    if std::env::var_os("EITRI_TEST_LSP_SIGNAL").is_none() {
        return;
    }
    let mut fixture = Fixture::new();
    let disk = FakeDisk::default();
    let options = Options {
        stop_signals: Some(session::stop_signal_pipe().unwrap()),
        ..quick_options()
    };
    let mut nvim = Nvim::start_with(&fixture, &disk, options, None);
    initialize_and_open(&mut fixture, &mut nvim, &disk);
    println!("ready");
    std::io::stdout().flush().unwrap();
    assert_eq!(fixture.stop().server_id, "s1");
    let code = nvim.session.take().unwrap().join().unwrap();
    assert_eq!(code, 128 + libc::SIGTERM);
    println!("stopped-by-signal");
}

#[test]
fn a_stop_signal_stops_the_server_and_ends_the_bridge() {
    use std::io::BufRead;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stop_signal_child", "--nocapture", "--test-threads=1"])
        .env("EITRI_TEST_LSP_SIGNAL", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "the child ended before it was ready: {seen}"
        );
        seen.push_str(&line);
        // The harness prints the test's name on the same line.
        if line.trim_end().ends_with("ready") {
            break;
        }
    }
    // SAFETY: a signal to the child this test started and has not reaped.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    // The rest of stdout is read here: the reader took the pipe.
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success() && rest.contains("stopped-by-signal"),
        "{:?}\n{seen}{rest}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------------------------------------------
// A helper without language servers

/// The child half of [`a_helper_without_lsp_runs_the_server_locally`]: `serve` finds no `lsp` in the helper's
/// capabilities and replaces this process with the command.
#[test]
fn local_run_child() {
    if std::env::var_os("EITRI_TEST_LSP_LOCAL").is_none() {
        return;
    }
    let options = Options {
        argv: vec!["sh".into(), "-c".into(), "echo ran-locally; cat".into()],
        ..quick_options()
    };
    // As `run_lsp` does: from here descriptor 1 is stderr, and only `serve`'s putting it back lets the command's
    // output reach the real stdout.
    let real_stdout = cli_lsp::stdout_off_the_stream().expect("stdout moves off the stream");
    let io = Io {
        stdin: Box::new(std::io::stdin()),
        stdout: Box::new(std::io::stdout()),
        stderr: Box::new(std::io::stderr()),
    };
    let redial: Redial = Box::new(|| Err(DialError::NoLsp));
    let code = cli_lsp::serve(options, Box::new(FakeDisk::default()), redial, io, Some(real_stdout));
    panic!("serve came back with {code}: the command did not replace the process");
}

#[test]
fn a_helper_without_lsp_runs_the_server_locally() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "local_run_child", "--nocapture", "--test-threads=1"])
        .env("EITRI_TEST_LSP_LOCAL", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"passed-through\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains("ran-locally") && stdout.contains("passed-through"),
        "{stdout}"
    );
    let notice: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("eitri host lsp:"))
        .collect();
    assert_eq!(notice.len(), 1, "{stderr}");
    assert!(
        notice[0].contains("example-host") && notice[0].contains("runs on this machine"),
        "{notice:?}"
    );
}
