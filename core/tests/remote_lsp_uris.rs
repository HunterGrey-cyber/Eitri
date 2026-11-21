//! The `file://` <-> `eitri-remote://` rewrite: which of a language server's URIs Neovim opens as read-only buffers read
//! from the server, and the same URIs on the way back, alone (`uris::Rewriter`) and through the bridge's two exits.
#![cfg(target_os = "linux")]

use std::path::Path;

use agent::remote_client::terminal_transport::wire::LspAttached;
use eitri_core::remote_lsp::bridge::{Action, Bridge, Config, Disk, Out};
use eitri_core::remote_lsp::uris::Rewriter;
use eitri_core::remote_sync::ignores::Matcher;
use serde_json::{json, Value};

const ROOT: &str = "/srv/project";
const HOST: &str = "example-host";

fn excludes() -> Matcher {
    Matcher::new(&["/target/".to_owned(), "*.generated.rs".to_owned()])
}

fn rewriter() -> Rewriter {
    Rewriter::new(HOST, Path::new(ROOT), excludes())
}

fn to_nvim(value: Value) -> Value {
    let mut value = value;
    rewriter().to_nvim(&mut value);
    value
}

fn to_server(value: Value) -> Value {
    let mut value = value;
    rewriter().to_server(&mut value);
    value
}

/// A URI as a `Location`'s `uri` carries it, rewritten for Neovim.
fn uri_to_nvim(uri: &str) -> Value {
    to_nvim(json!({"uri": uri}))["uri"].clone()
}

#[test]
fn a_location_outside_the_root_becomes_eitri_remote() {
    let dependency = "file:///home/user/.cargo/registry/src/index/dep-1.0.0/src/lib.rs";
    let remote = "eitri-remote://example-host/home/user/.cargo/registry/src/index/dep-1.0.0/src/lib.rs";
    // A Location, a Location[] and a LocationLink[]: wherever the string sits.
    let range = json!({"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}});
    assert_eq!(
        to_nvim(json!({"uri": dependency, "range": range})),
        json!({"uri": remote, "range": range})
    );
    assert_eq!(
        to_nvim(json!([{"uri": dependency, "range": range}, {"uri": "file:///srv/project/src/a.rs", "range": range}])),
        json!([{"uri": remote, "range": range}, {"uri": "file:///srv/project/src/a.rs", "range": range}])
    );
    assert_eq!(
        to_nvim(json!([{"targetUri": dependency, "targetRange": range, "targetSelectionRange": range}]))[0]
            ["targetUri"],
        remote
    );
    // A directory that only starts like the root is not the root.
    assert_eq!(
        uri_to_nvim("file:///srv/project-other/x.rs"),
        json!("eitri-remote://example-host/srv/project-other/x.rs")
    );
    // A path that does not stay inside the root, or does not decode, is read from the server.
    assert_eq!(
        uri_to_nvim("file:///srv/project/../etc/passwd"),
        json!("eitri-remote://example-host/srv/project/../etc/passwd")
    );
    assert_eq!(
        uri_to_nvim("file:///srv/project/%ff.rs"),
        json!("eitri-remote://example-host/srv/project/%ff.rs")
    );
    // The path is written with the escapes every name of the file uses; a fragment or query stays where it was.
    assert_eq!(
        uri_to_nvim("file:///home/user/my%20dir/a.rs#L12"),
        json!("eitri-remote://example-host/home/user/my%20dir/a.rs#L12")
    );
    // A map keyed by URI (`WorkspaceEdit.changes`) is rewritten in its keys.
    let edit = to_nvim(json!({"changes": {dependency: [{"newText": "x"}], "file:///srv/project/a.rs": []}}));
    assert_eq!(edit["changes"][remote], json!([{"newText": "x"}]));
    assert!(edit["changes"].get(dependency).is_none());
    assert!(edit["changes"].get("file:///srv/project/a.rs").is_some());
}

#[test]
fn a_servers_own_escapes_give_the_name_open_and_the_panel_give() {
    use eitri_core::remote_lsp::uris::remote_uri;
    // A server that escapes `+` and `~` and writes lowercase hex names the same buffer as `:EitriRemote open` and the
    // panel do for that file, not a second one.
    assert_eq!(
        uri_to_nvim("file:///opt/dep/a%2Bb%7ec%20d%c3%a9.rs"),
        json!(remote_uri(HOST, "/opt/dep/a+b~c d\u{e9}.rs"))
    );
    assert_eq!(
        remote_uri(HOST, "/opt/dep/a+b~c d\u{e9}.rs"),
        "eitri-remote://example-host/opt/dep/a+b~c%20d%C3%A9.rs"
    );
    // A query or fragment stays as the server wrote it.
    assert_eq!(
        uri_to_nvim("file:///opt/dep/a%2Bb.rs?x=%2B#L3"),
        json!("eitri-remote://example-host/opt/dep/a+b.rs?x=%2B#L3")
    );
    // Back to the server it is a `file:///` URI of the same path.
    assert_eq!(
        to_server(json!({"uri": "eitri-remote://example-host/opt/dep/a+b~c%20d%C3%A9.rs"})),
        json!({"uri": "file:///opt/dep/a+b~c%20d%C3%A9.rs"})
    );
}

#[test]
fn the_host_is_written_as_typed_and_percent_encoded() {
    let mut value = json!({"uri": "file:///opt/dep/lib.rs"});
    Rewriter::new("user@example-host:2222", Path::new(ROOT), excludes()).to_nvim(&mut value);
    assert_eq!(
        value,
        json!({"uri": "eitri-remote://user%40example-host%3A2222/opt/dep/lib.rs"})
    );
    let mut back = value.clone();
    Rewriter::new("user@example-host:2222", Path::new(ROOT), excludes()).to_server(&mut back);
    assert_eq!(back, json!({"uri": "file:///opt/dep/lib.rs"}));
    // Another host's buffer is not this server's file.
    let mut other = json!({"uri": "eitri-remote://other-host/opt/dep/lib.rs"});
    rewriter().to_server(&mut other);
    assert_eq!(other, json!({"uri": "eitri-remote://other-host/opt/dep/lib.rs"}));
}

#[test]
fn an_excluded_path_inside_the_root_becomes_eitri_remote() {
    for uri in [
        "file:///srv/project/target/debug/build/out.rs",
        "file:///srv/project/src/schema.generated.rs",
        // `.git` is never carried.
        "file:///srv/project/.git/config",
    ] {
        let remote = uri.replacen("file://", "eitri-remote://example-host", 1);
        assert_eq!(uri_to_nvim(uri), json!(remote), "{uri}");
    }
    // The excluded name written with escapes is the same name, and names the same buffer.
    assert_eq!(
        uri_to_nvim("file:///srv/project/%74arget/debug/out.rs"),
        json!("eitri-remote://example-host/srv/project/target/debug/out.rs")
    );
}

#[test]
fn a_path_inside_the_mirror_is_left_alone() {
    for uri in [
        "file:///srv/project",
        "file:///srv/project/src/lib.rs",
        "file:///srv/project/src/target.rs",
        // `/target/` is the root's own directory, not any directory of that name.
        "file:///srv/project/crates/a/target/x.rs",
        "file:///srv/project/my%20dir/a%C3%A9.rs",
        "file:///srv/project/src/lib.rs#L3",
        // Not a `file:///` URI: another scheme, a named host.
        "untitled:Untitled-1",
        "https://example.com/a.rs",
        "file://other-host/home/user/x.rs",
        "file://localhost/home/user/x.rs",
    ] {
        assert_eq!(uri_to_nvim(uri), json!(uri), "{uri}");
    }
}

#[test]
fn only_uri_members_are_rewritten_never_text() {
    let outside = "file:///home/user/dep/lib.rs";
    let remote = "eitri-remote://example-host/home/user/dep/lib.rs";
    // To Neovim: text that begins with a file URI outside the mirror (an edit's new text, a completion, a message, an
    // argument that holds more than the URI) is inserted or shown as the server wrote it.
    let message = json!({
        "message": outside,
        "command": {"command": "open", "arguments": [format!("--uri={outside}"), 7, {"text": outside}]},
        "contents": {"kind": "markdown", "value": outside},
        "edits": [{"range": {}, "newText": outside}],
        "items": [{"label": outside, "insertText": outside, "filterText": outside, "detail": outside,
                   "documentation": outside, "textEdit": {"newText": outside}}],
        "uris": [outside],
        "n": 7,
        "flag": true,
        "nothing": null,
    });
    assert_eq!(to_nvim(message.clone()), message);
    // To the server: a document's text that begins with this host's eitri-remote:// name is the document's text.
    let opened = json!({"textDocument": {"uri": remote, "languageId": "rust", "version": 1, "text": remote}});
    assert_eq!(
        to_server(opened),
        json!({"textDocument": {"uri": outside, "languageId": "rust", "version": 1, "text": remote}})
    );
    let changed = json!({"textDocument": {"uri": remote, "version": 2}, "contentChanges": [{"text": remote},
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": remote}]});
    let sent = to_server(changed);
    assert_eq!(sent["textDocument"]["uri"], outside);
    assert_eq!(sent["contentChanges"][0]["text"], remote);
    assert_eq!(sent["contentChanges"][1]["text"], remote);
    // Text that only holds a URI somewhere is left alone in a URI member too: only a prefix is a URI.
    assert_eq!(
        to_nvim(json!({"uri": format!("x{outside}")})),
        json!({"uri": format!("x{outside}")})
    );
    // Every member the protocol types as a URI, and a map keyed by URI.
    for member in [
        "uri",
        "targetUri",
        "oldUri",
        "newUri",
        "rootUri",
        "scopeUri",
        "baseUri",
        "target",
        "href",
    ] {
        assert_eq!(to_nvim(json!({member: outside})), json!({member: remote}), "{member}");
        assert_eq!(to_server(json!({member: remote})), json!({member: outside}), "{member}");
    }
    assert_eq!(
        to_nvim(json!({"baseUri": {"uri": outside, "name": "dep"}, "pattern": "**/*.rs"})),
        json!({"baseUri": {"uri": remote, "name": "dep"}, "pattern": "**/*.rs"})
    );
    let report =
        to_nvim(json!({"kind": "full", "items": [], "relatedDocuments": {outside: {"kind": "full", "items": []}}}));
    assert!(report["relatedDocuments"].get(remote).is_some(), "{report}");
    // A map keyed by URI is rewritten in its keys only where the protocol has one.
    assert_eq!(to_nvim(json!({"other": {outside: 1}})), json!({"other": {outside: 1}}));
}

#[test]
fn a_string_with_the_eitri_remote_scheme_is_found_anywhere() {
    use eitri_core::remote_lsp::uris::names_the_scheme;
    for found in [
        json!({"uri": "eitri-remote://other-host/home/user/.ssh/id_ed25519"}),
        json!({"params": {"uri": "eitri-remote://example-host/srv/project/src/a.rs"}}),
        json!({"result": [{"targetUri": "EITRI-REMOTE://other-host/x"}]}),
        json!({"command": {"arguments": ["Eitri-Remote://other-host/x"]}}),
        json!({"contents": [{"uri": "eitri-remote:other-host/x"}]}),
        json!({"message": {"uri": "eitri-remote://other-host/x"}}),
        json!({"changes": {"eitri-remote://other-host/x": []}}),
    ] {
        assert!(names_the_scheme(&found), "{found}");
    }
    for not_found in [
        json!({"uri": "file:///home/user/x"}),
        json!({"message": "see eitri-remote://other-host/x"}),
        json!({"value": " eitri-remote://other-host/x"}),
        json!({"uri": "eitri-remot"}),
        json!(7),
        // Text that begins like a name is text: Eitri's own docs and tests have such lines.
        json!({"contents": {"kind": "markdown", "value": "eitri-remote://other-host/x"}}),
        json!({"contents": ["eitri-remote://other-host/x"]}),
        json!({"message": "eitri-remote://other-host/x is not a file", "uri": "file:///srv/project/a.rs"}),
        json!({"newText": "eitri-remote://other-host/x\n", "insertText": "eitri-remote://x", "label": "eitri-remote:"}),
        json!({"items": [{"label": "eitri-remote://x", "filterText": "eitri-remote://x", "sortText": "eitri-remote:",
                          "detail": "eitri-remote://x", "documentation": "eitri-remote://x"}]}),
        json!({"textDocument": {"uri": "file:///srv/project/a.rs", "text": "eitri-remote://x"}}),
        json!({"name": "eitri-remote://x", "location": {"uri": "file:///srv/project/a.rs"}}),
    ] {
        assert!(!names_the_scheme(&not_found), "{not_found}");
    }
    // A text member does not hide a name nested in it, nor a member that is not text.
    assert!(names_the_scheme(
        &json!({"message": {"uri": "eitri-remote://other-host/x"}})
    ));
    assert!(names_the_scheme(
        &json!({"message": "fine", "location": {"uri": "eitri-remote://other-host/x"}})
    ));
    // From Neovim: this host's names are its own; another host's, in a URI member, are not.
    let rewriter = rewriter();
    assert!(!rewriter.names_another_host(&json!({"textDocument": {"uri": "eitri-remote://example-host/opt/x.rs"}})));
    assert!(rewriter.names_another_host(&json!({"textDocument": {"uri": "eitri-remote://other-host/opt/x.rs"}})));
    assert!(rewriter.names_another_host(&json!({"textDocument": {"uri": "EITRI-REMOTE://example-host/opt/x.rs"}})));
    assert!(rewriter.names_another_host(&json!({"changes": {"eitri-remote://other-host/opt/x.rs": []}})));
    // A document's text is not a name.
    assert!(
        !rewriter.names_another_host(&json!({"textDocument": {"uri": "file:///srv/project/a.rs",
        "text": "eitri-remote://other-host/opt/x.rs"}}))
    );
}

#[test]
fn a_bare_file_uri_in_a_commands_arguments_is_rewritten_both_ways() {
    let outside = "file:///home/user/dep/lib.rs";
    let remote = "eitri-remote://example-host/home/user/dep/lib.rs";
    let inside = "file:///srv/project/src/main.rs";
    // rust-analyzer.showReferences carries the URI as an argument: a client handler opens it.
    assert_eq!(
        to_nvim(json!({"command": {"command": "rust-analyzer.showReferences", "arguments": [outside, inside, 3]}})),
        json!({"command": {"command": "rust-analyzer.showReferences", "arguments": [remote, inside, 3]}})
    );
    assert_eq!(
        to_server(json!({"command": "x", "arguments": [remote, "eitri-remote://other-host/x", {"uri": remote}]})),
        json!({"command": "x", "arguments": [outside, "eitri-remote://other-host/x", {"uri": outside}]})
    );
    // Only a string that is the argument itself, and only a prefix: text inside a string stays.
    assert_eq!(
        to_nvim(json!({"arguments": [format!("x{outside}"), [outside], {"insert": outside}]})),
        json!({"arguments": [format!("x{outside}"), [outside], {"insert": outside}]})
    );
    // A bare string in an array under another member is data.
    assert_eq!(to_nvim(json!({"args": [outside]})), json!({"args": [outside]}));
    // From Neovim: another host's name as an argument is another host's.
    let rewriter = rewriter();
    assert!(rewriter.names_another_host(&json!({"arguments": ["eitri-remote://other-host/x"]})));
    assert!(!rewriter.names_another_host(&json!({"arguments": [remote, "text"]})));
}

// ---------------------------------------------------------------------------------------------------------------
// Through the bridge

struct Mirror;

impl Disk for Mirror {
    fn sha256(&self, _: &Path) -> std::io::Result<String> {
        Ok("a".repeat(64))
    }

    fn carried(&self, path: &Path) -> bool {
        path.starts_with(ROOT) && !path.starts_with(Path::new(ROOT).join("target"))
    }

    fn rewriter(&self, host: &str) -> Rewriter {
        Rewriter::new(host, Path::new(ROOT), excludes())
    }
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// A bridge through `initialize` and `initialized` against a server that answered.
fn live() -> Bridge {
    let mut bridge = Bridge::new(
        Config {
            host: HOST.into(),
            root: ROOT.into(),
            command: vec!["rust-analyzer".into()],
        },
        Box::new(Mirror),
    );
    let out = bridge.from_nvim(request(
        1,
        "initialize",
        json!({"processId": 4242, "rootUri": "file:///srv/project", "capabilities": {}}),
    ));
    assert_eq!(out.actions, [Action::OpenServer]);
    let attached = LspAttached {
        reattached: false,
        ..Default::default()
    };
    let out = bridge.link_back(&attached);
    assert_eq!(out.to_server[0].message["params"]["rootUri"], "file:///srv/project");
    let capabilities = json!({"capabilities": {"textDocumentSync": {"openClose": true, "change": 2, "save": {}}}});
    bridge.from_server(json!({"jsonrpc": "2.0", "id": 1, "result": capabilities}));
    bridge.from_nvim(notification("initialized", json!({})));
    bridge
}

fn sent(out: &Out) -> Vec<&Value> {
    out.to_server.iter().map(|sent| &sent.message).collect()
}

#[test]
fn requests_from_an_eitri_remote_buffer_reach_the_server_as_file_uris() {
    let mut bridge = live();
    let remote = "eitri-remote://example-host/home/user/dep/lib.rs";
    let file = "file:///home/user/dep/lib.rs";
    let out = bridge.from_nvim(notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": remote, "languageId": "rust", "version": 1, "text": "pub fn dep() {}\n"}}),
    ));
    assert_eq!(sent(&out)[0]["params"]["textDocument"]["uri"], file);
    let position = json!({"textDocument": {"uri": remote}, "position": {"line": 0, "character": 8}});
    let out = bridge.from_nvim(request(2, "textDocument/definition", position));
    assert_eq!(sent(&out)[0]["params"]["textDocument"]["uri"], file);
    // What the server answers with is rewritten for Neovim: a location in the same dependency, one in the mirror.
    let answer = json!({"jsonrpc": "2.0", "id": 2, "result": [
        {"uri": file, "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 10}}},
        {"uri": "file:///srv/project/src/main.rs", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}},
    ]});
    let out = bridge.from_server(answer);
    assert_eq!(out.to_nvim[0]["result"][0]["uri"], remote);
    assert_eq!(out.to_nvim[0]["result"][1]["uri"], "file:///srv/project/src/main.rs");
    // A save of a buffer that is not on this machine carries no hash, so the helper holds nothing for it.
    let out = bridge.from_nvim(notification(
        "textDocument/didSave",
        json!({"textDocument": {"uri": remote}}),
    ));
    assert!(out.to_server.iter().all(|sent| sent.saved_sha256.is_none()));
}

#[test]
fn a_replay_names_an_eitri_remote_document_by_its_file_uri() {
    let mut bridge = live();
    let remote = "eitri-remote://example-host/home/user/dep/lib.rs";
    bridge.from_nvim(notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": remote, "languageId": "rust", "version": 1, "text": "pub fn dep() {}\n"}}),
    ));
    bridge.from_nvim(notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": "file:///srv/project/src/a.rs", "languageId": "rust", "version": 1, "text": "fn a() {}"}}),
    ));
    bridge.link_lost();
    let out = bridge.link_back(&LspAttached {
        reattached: true,
        ..Default::default()
    });
    let uris: Vec<&str> = sent(&out)
        .into_iter()
        .filter(|message| message["method"] == "textDocument/didOpen")
        .map(|message| message["params"]["textDocument"]["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, ["file:///home/user/dep/lib.rs", "file:///srv/project/src/a.rs"]);
}

#[test]
fn a_server_that_names_an_eitri_remote_file_is_refused() {
    let mut bridge = live();
    let other = "eitri-remote://other-host/home/user/.ssh/id_ed25519";
    // A request of the server's (a jump with no keypress): Neovim never sees it, the server gets an error.
    let out = bridge.from_server(request(
        70,
        "window/showDocument",
        json!({"uri": other, "takeFocus": true}),
    ));
    assert!(out.to_nvim.is_empty(), "{:?}", out.to_nvim);
    let refused = sent(&out);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["id"], 70);
    assert!(refused[0]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("eitri-remote://"));
    assert!(
        out.log.iter().any(|line| line.contains("window/showDocument")),
        "{:?}",
        out.log
    );
    // An answer to Neovim's request becomes an error for it; the scheme in any case, in any member.
    let position =
        json!({"textDocument": {"uri": "file:///srv/project/src/a.rs"}, "position": {"line": 0, "character": 0}});
    bridge.from_nvim(request(3, "textDocument/definition", position.clone()));
    let out = bridge.from_server(
        json!({"jsonrpc": "2.0", "id": 3, "result": [{"targetUri": "EITRI-remote://other-host/x",
        "targetRange": {}, "targetSelectionRange": {}}]}),
    );
    assert_eq!(out.to_nvim.len(), 1);
    assert_eq!(out.to_nvim[0]["id"], 3);
    assert!(out.to_nvim[0].get("result").is_none() && out.to_nvim[0]["error"].is_object());
    // Its own host's name is no more the server's to write than another's, wherever it stands as a name; a hover's
    // text that merely begins like one is text and arrives.
    bridge.from_nvim(request(4, "textDocument/hover", position.clone()));
    let out = bridge.from_server(
        json!({"jsonrpc": "2.0", "id": 4, "result": {"contents": {"kind": "markdown",
        "value": "eitri-remote://example-host/home/user/x"}}}),
    );
    assert_eq!(
        out.to_nvim[0]["result"]["contents"]["value"],
        "eitri-remote://example-host/home/user/x"
    );
    bridge.from_nvim(request(5, "textDocument/definition", position));
    let out = bridge.from_server(json!({"jsonrpc": "2.0", "id": 5, "result": [{"uri":
        "eitri-remote://example-host/home/user/x", "range": {}}]}));
    assert!(out.to_nvim[0]["error"].is_object(), "{:?}", out.to_nvim);
    // A notification is dropped.
    let out = bridge.from_server(notification(
        "textDocument/publishDiagnostics",
        json!({"uri": other, "diagnostics": []}),
    ));
    assert!(out.to_nvim.is_empty() && out.to_server.is_empty());
    // What the server writes as a file URI still reaches Neovim, rewritten.
    let out = bridge.from_server(notification(
        "textDocument/publishDiagnostics",
        json!({"uri": "file:///home/user/dep/lib.rs", "diagnostics": []}),
    ));
    assert_eq!(
        out.to_nvim[0]["params"]["uri"],
        "eitri-remote://example-host/home/user/dep/lib.rs"
    );
}

#[test]
fn another_hosts_buffer_never_reaches_this_server() {
    let mut bridge = live();
    let other = "eitri-remote://other-host/home/user/.ssh/id_ed25519";
    let out = bridge.from_nvim(notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": other, "languageId": "text", "version": 1, "text": "secret\n"}}),
    ));
    assert!(out.to_server.is_empty(), "{:?}", sent(&out));
    let out = bridge.from_nvim(notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": other, "version": 2}, "contentChanges": [{"text": "secret 2\n"}]}),
    ));
    assert!(out.to_server.is_empty(), "{:?}", sent(&out));
    let out = bridge.from_nvim(request(
        5,
        "textDocument/hover",
        json!({"textDocument": {"uri": other}, "position": {"line": 0, "character": 0}}),
    ));
    assert!(out.to_server.is_empty());
    assert_eq!(out.to_nvim[0]["id"], 5);
    assert!(out.to_nvim[0]["error"].is_object());
    // This host's buffers still go, and a reconnect replays nothing of the other host's.
    let own = "eitri-remote://example-host/home/user/dep/lib.rs";
    let out = bridge.from_nvim(notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": own, "languageId": "rust", "version": 1, "text": "pub fn dep() {}\n"}}),
    ));
    assert_eq!(
        sent(&out)[0]["params"]["textDocument"]["uri"],
        "file:///home/user/dep/lib.rs"
    );
    bridge.link_lost();
    let out = bridge.link_back(&LspAttached {
        reattached: true,
        ..Default::default()
    });
    assert!(
        sent(&out)
            .iter()
            .all(|message| !message.to_string().contains("other-host")),
        "{:?}",
        sent(&out)
    );
}

#[test]
fn document_text_crosses_the_bridge_unchanged_both_ways() {
    let mut bridge = live();
    let own = "eitri-remote://example-host/home/user/dep/lib.rs";
    let outside = "file:///home/user/dep/lib.rs";
    // A file in the mirror whose text starts like this host's eitri-remote:// name.
    let text = format!("{own}\n");
    let out = bridge.from_nvim(notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": "file:///srv/project/src/a.rs", "languageId": "rust", "version": 1, "text": text}}),
    ));
    assert_eq!(sent(&out)[0]["params"]["textDocument"]["text"], text);
    let out = bridge.from_nvim(notification(
        "textDocument/didChange",
        json!({"textDocument": {"uri": "file:///srv/project/src/a.rs", "version": 2},
               "contentChanges": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                                   "text": own}]}),
    ));
    assert_eq!(sent(&out)[0]["params"]["contentChanges"][0]["text"], own);
    // An edit and a completion whose text starts like a file URI outside the mirror are inserted as written.
    let out = bridge.from_server(request(
        80,
        "workspace/applyEdit",
        json!({"edit": {"changes": {outside: [{"range": {}, "newText": outside}]}}}),
    ));
    let changes = &out.to_nvim[0]["params"]["edit"]["changes"];
    assert_eq!(changes[own][0]["newText"], outside, "{changes}");
    bridge.from_nvim(request(
        6,
        "textDocument/completion",
        json!({"textDocument": {"uri": "file:///srv/project/src/a.rs"}, "position": {"line": 0, "character": 0}}),
    ));
    let out =
        bridge.from_server(json!({"jsonrpc": "2.0", "id": 6, "result": [{"label": outside, "insertText": outside}]}));
    assert_eq!(
        out.to_nvim[0]["result"][0],
        json!({"label": outside, "insertText": outside})
    );
}

#[test]
fn a_diagnostic_whose_message_begins_with_the_scheme_reaches_neovim() {
    let mut bridge = live();
    let diagnostics = notification(
        "textDocument/publishDiagnostics",
        json!({"uri": "file:///srv/project/docs.md", "diagnostics": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
            "message": "eitri-remote://other-host/x is not a link",
            "relatedInformation": [{"message": "eitri-remote://x", "location": {"uri": "file:///home/user/dep/lib.rs",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}}]}]}),
    );
    let out = bridge.from_server(diagnostics);
    assert_eq!(out.to_nvim.len(), 1, "{:?}", out.log);
    let sent = &out.to_nvim[0]["params"];
    assert_eq!(
        sent["diagnostics"][0]["message"],
        "eitri-remote://other-host/x is not a link"
    );
    // The location inside the same message is still a name the bridge rewrites.
    assert_eq!(
        sent["diagnostics"][0]["relatedInformation"][0]["location"]["uri"],
        "eitri-remote://example-host/home/user/dep/lib.rs"
    );
    // A formatting edit that inserts such a line, a completion and a hover.
    let id = 9;
    bridge.from_nvim(request(
        id,
        "textDocument/formatting",
        json!({"textDocument": {"uri": "file:///srv/project/a.md"}}),
    ));
    let out = bridge.from_server(json!({"jsonrpc": "2.0", "id": id, "result": [{"range":
        {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "eitri-remote://x\n"}]}));
    assert_eq!(
        out.to_nvim[0]["result"][0]["newText"], "eitri-remote://x\n",
        "{:?}",
        out.log
    );
    // A location that names the scheme is still refused.
    bridge.from_nvim(request(
        10,
        "textDocument/definition",
        json!({"textDocument": {"uri": "file:///srv/project/a.rs"},
        "position": {"line": 0, "character": 0}}),
    ));
    let out = bridge.from_server(json!({"jsonrpc": "2.0", "id": 10, "result":
        [{"uri": "eitri-remote://other-host/x", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]}));
    assert!(out.to_nvim[0].get("error").is_some(), "{:?}", out.to_nvim);
    assert!(out.to_nvim[0].get("result").is_none());
}
