//! The editor's tools through a real `codex app-server` and the real `eitri-editor-bridge`: the model is the
//! scripted stand-in on `127.0.0.1`, the window's end is a [`BridgeHost`] and a [`Broker`] with an editor the
//! test answers for.
//!
//! Every test is `#[ignore]`d: it needs a `codex` on `PATH` (or `EITRI_CODEX`) and the bridge binary named by
//! `EITRI_TEST_BRIDGE`, and runs Codex in a private `CODEX_HOME` whose model provider is the scripted model. No
//! account is used, no request leaves the machine and no model turn is spent. Run them with
//!
//! ```text
//! cargo build -p shell --bin eitri-editor-bridge
//! EITRI_TEST_BRIDGE=$CARGO_TARGET_DIR/debug/eitri-editor-bridge \
//!   cargo test -p eitri-core --features test-support --test editor_tools_codex_real -- --ignored --test-threads=1
//! ```
//!
//! under an environment without `TMUX`. Scratch directories are under the test target's own directory, never
//! under `/tmp`. Each test prints the `codex --version` it ran against.
#![cfg(target_os = "linux")]

#[path = "../../agent/tests/support/codex_peer.rs"]
mod codex_peer;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::providers::codex::scripted_model::{ScriptedModel, Step};
use agent::providers::codex::tripwire::TrustWatch;
use agent::providers::codex::wire::{method, ThreadStartParams, TurnInterruptParams, TurnSteerParams};
use agent::providers::codex::BinaryKey;
use agent::setting_sources::ProjectTrust;
use agent::{DirTrust, EditorServerSpec, PermissionMode, Reviewer, TrustPlan};
use eitri_core::editor_context::EditorContext;
use eitri_core::editor_tools::bridge_host::{BridgeHost, BridgeNews};
use eitri_core::editor_tools::broker::{Broker, BrokerEnv, BrokerOut, Caller, TabFacts};
use eitri_core::editor_tools::cards::{CardDecision, CardOrigin};
use eitri_core::editor_tools::catalog::{self, ToolRow};
use eitri_core::editor_tools::stamp::Stamp;
use eitri_core::editor_tools::step::{BridgeCallId, EditorReach, ToolResult};
use eitri_core::editor_tools::switches::Switches;
use eitri_core::editor_tools::way::{EditorCall, EditorWay};
use eitri_core::editor_tools::{words, SERVER_NAME};
use eitri_core::nvim_rpc::{Answer, Pending};
use eitri_core::project_trust::{discover, eitri_server_taken, read_user_codex_config, Limits, Profile, ProfileEnv};
use eitri_core::tabs::TabId;
use rmpv::Value as Msgpack;
use serde_json::{json, Value};

use codex_peer::*;

const TAB: TabId = TabId(1);
const PROMPT: &str = "EDITOR-TOOLS-PROMPT";
/// How the model names the editor's server.
const NAMESPACE: &str = "mcp__eitri";
const STARTUP_BUDGET: Duration = Duration::from_secs(20);
/// The first Codex that tells a tool server it gave a call up: an interrupt drops the call's future and that sends
/// `notifications/cancelled`. An older one says nothing to the server, and the call ends with the host's end of the
/// turn. On every version the item is never completed and a reply is ignored.
const CANCELS_CALLS: (u32, u32, u32) = (0, 165, 0);

// ------------------------------------------------------------------------------------ the editor

/// An editor the test answers for. A `read_buffer` is never answered, which is a call stuck at a busy nvim.
struct FakeEditor {
    root: PathBuf,
    asked: RefCell<Vec<String>>,
    held: RefCell<Vec<Answer>>,
}

impl FakeEditor {
    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

fn map(fields: Vec<(&str, Msgpack)>) -> Msgpack {
    Msgpack::Map(
        fields
            .into_iter()
            .map(|(name, value)| (Msgpack::from(name), value))
            .collect(),
    )
}

impl EditorWay for FakeEditor {
    fn call(&self, _session: &str, call: &EditorCall, _stamp: &Stamp) -> Pending {
        let (answer, pending) = Pending::pair();
        let lib = self.root.join("src/lib.rs");
        let lib = lib.to_str().expect("a UTF-8 path");
        match call {
            EditorCall::EditorState { .. } => {
                self.asked.borrow_mut().push("editor_state".to_owned());
                answer.send(Ok(map(vec![
                    (
                        "buffers",
                        Msgpack::Array(vec![map(vec![
                            ("path", Msgpack::from(lib)),
                            ("read_from", Msgpack::from(lib)),
                            ("modified", Msgpack::from(false)),
                            ("lines", Msgpack::from(12)),
                            ("filetype", Msgpack::from("rust")),
                            ("shown", Msgpack::from(true)),
                        ])]),
                    ),
                    ("other", Msgpack::from(0)),
                    ("left_out", Msgpack::from(0)),
                    (
                        "current",
                        map(vec![
                            ("line", Msgpack::from(3)),
                            ("path", Msgpack::from(lib)),
                            ("read_from", Msgpack::from(lib)),
                        ]),
                    ),
                    ("mode", Msgpack::from("n")),
                ])));
            }
            EditorCall::OpenFile { path, .. } => {
                self.asked.borrow_mut().push(format!("open_file {}", path.display()));
                answer.send(Ok(map(vec![
                    ("shown", Msgpack::from(true)),
                    ("where", Msgpack::from("existing")),
                    ("line", Msgpack::from(3)),
                ])));
            }
            EditorCall::ReadBuffer { .. } => {
                self.asked.borrow_mut().push("read_buffer".to_owned());
                self.held.borrow_mut().push(answer);
            }
            EditorCall::Diagnostics { .. } => {
                self.asked.borrow_mut().push("diagnostics".to_owned());
                self.held.borrow_mut().push(answer);
            }
            EditorCall::Baseline { .. } => {
                self.asked.borrow_mut().push("baseline".to_owned());
                self.held.borrow_mut().push(answer);
            }
            EditorCall::Exec(_) => panic!("no scenario here declares nvim_exec"),
            EditorCall::LspTargets { .. } => panic!("no scenario here applies a proposal"),
            EditorCall::LspAsk(_) | EditorCall::LspCollect { .. } => {
                self.asked.borrow_mut().push("lsp".to_owned());
                self.held.borrow_mut().push(answer);
            }
        }
        pending
    }

    fn reach(&self) -> EditorReach {
        EditorReach::Own
    }
}

struct Env {
    editor: FakeEditor,
    root: PathBuf,
    mode: Cell<PermissionMode>,
    trust: ProjectTrust,
    declared: Rc<BTreeSet<String>>,
}

impl BrokerEnv for Env {
    fn way(&self) -> Option<&dyn EditorWay> {
        Some(&self.editor)
    }

    fn facts(&self, tab: TabId) -> Option<TabFacts> {
        (tab == TAB).then(|| TabFacts {
            mode: self.mode.get(),
            trust: self.trust,
            session: "session-1".to_owned(),
            process: "process-1".to_owned(),
            revision: 1,
            declared: self.declared.clone(),
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn home(&self) -> Option<&Path> {
        None
    }

    fn context(&self) -> Option<EditorContext> {
        None
    }
}

// ------------------------------------------------------------------------------------ the window's end

/// What the bridge delivered, in order.
struct Seen {
    id: BridgeCallId,
    tool: String,
    row_id: Option<String>,
}

/// A bridge host and a broker serving one tab, as the panel serves it: the news goes in as calls and cancels, the
/// broker's answers go back out by the host.
struct Rig {
    host: BridgeHost,
    broker: Broker,
    env: Env,
    spec: EditorServerSpec,
    seen: Vec<Seen>,
    cancelled: Vec<BridgeCallId>,
    answers: Vec<ToolResult>,
}

fn bridge_binary() -> PathBuf {
    let path = std::env::var_os("EITRI_TEST_BRIDGE")
        .map(PathBuf::from)
        .expect("EITRI_TEST_BRIDGE names a built eitri-editor-bridge");
    assert!(path.is_file(), "{} is not a file", path.display());
    path
}

fn tool_objects(rows: &[&'static ToolRow]) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            let schema: Value = serde_json::from_str(row.schema).expect("a JSON schema");
            json!({ "name": row.name, "description": row.description, "inputSchema": schema })
        })
        .collect()
}

fn every_row() -> Vec<&'static ToolRow> {
    catalog::declared(&Switches::on_by_default(), ProjectTrust::Trusted)
}

impl Rig {
    fn new(s: &Scratch, project: &Path, rows: &[&'static ToolRow], mode: PermissionMode, trust: ProjectTrust) -> Rig {
        let dir = s.path("bridge");
        eitri_core::panel_control::ensure_dir(&dir).expect("a private directory for the bridge sockets");
        let host = BridgeHost::start(dir, Arc::new(|| ())).expect("the bridge host");
        let (name, socket) = host.open(TAB.0, tool_objects(rows)).expect("the tab's bridge socket");
        let spec = EditorServerSpec {
            name: SERVER_NAME.to_owned(),
            command: bridge_binary(),
            args: vec![text_of(&socket).to_owned(), name.key],
            cwd: project.to_path_buf(),
        };
        let declared: BTreeSet<String> = rows.iter().map(|row| row.name.to_owned()).collect();
        Rig {
            host,
            broker: Broker::new(),
            env: Env {
                editor: FakeEditor {
                    root: project.to_path_buf(),
                    asked: RefCell::new(Vec::new()),
                    held: RefCell::new(Vec::new()),
                },
                root: project.to_path_buf(),
                mode: Cell::new(mode),
                trust,
                declared: Rc::new(declared),
            },
            spec,
            seen: Vec::new(),
            cancelled: Vec::new(),
            answers: Vec::new(),
        }
    }

    /// One pass of what the panel does on a wake and a tick.
    fn service(&mut self) {
        let now = Instant::now();
        let mut outs = Vec::new();
        for news in self.host.take_news() {
            match news {
                BridgeNews::Call {
                    id,
                    tool,
                    arguments,
                    row_id,
                    ..
                } => {
                    self.seen.push(Seen {
                        id: id.clone(),
                        tool: tool.clone(),
                        row_id: row_id.clone(),
                    });
                    outs.extend(self.broker.submit(
                        TAB,
                        Caller::Bridge { id },
                        row_id,
                        &tool,
                        &arguments,
                        &self.env,
                        now,
                    ));
                }
                BridgeNews::Cancelled { id } => {
                    self.cancelled.push(id.clone());
                    outs.extend(self.broker.cancel(TAB, &Caller::Bridge { id }));
                }
            }
        }
        outs.extend(self.broker.tick(&self.env, now));
        self.deliver(outs);
    }

    fn deliver(&mut self, outs: Vec<BrokerOut>) {
        for out in outs {
            if let BrokerOut::Answer {
                caller: Caller::Bridge { id },
                result,
                ..
            } = out
            {
                self.host.answer(&id, &result);
                self.answers.push(result);
            }
        }
    }

    /// The tab's turn ended interrupted, as the provider tells the panel.
    fn turn_interrupted(&mut self) {
        let outs = self.broker.turn_interrupted(TAB);
        self.deliver(outs);
    }
}

/// Whether this build tells a tool server when it gives a call up.
fn cancels_calls(codex: &Path) -> bool {
    let key = BinaryKey::of(codex).expect("the binary's version");
    key.version_numbers().expect("a version that reads") >= CANCELS_CALLS
}

/// Reads the peer and serves the rig until `done` holds; false when the time ran out or the child ended.
fn drive(peer: &mut Peer, rig: &mut Rig, within: Duration, mut done: impl FnMut(&Peer, &Rig) -> bool) -> bool {
    let end = Instant::now() + within;
    loop {
        peer.pump();
        rig.service();
        if done(peer, rig) {
            return true;
        }
        if peer.closed.is_some() || Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A turn to its end, the rig serving the bridge's calls meanwhile; the turn's status.
fn serve_turn(peer: &mut Peer, rig: &mut Rig, thread: &str, text: &str) -> String {
    let turn = send_turn(peer, thread, text);
    let ended = drive(peer, rig, TURN_BUDGET, |p, _| p.turn_status(thread, &turn).is_some());
    assert!(ended, "the turn did not end; stderr: {}", peer.stderr_tail());
    peer.turn_status(thread, &turn).expect("a status")
}

// ------------------------------------------------------------------------------------ the session

/// A project directory with one source file, and `config` as its `.codex/config.toml` when there is any.
fn make_project(s: &Scratch, config: &str) -> PathBuf {
    let project = s.dir("proj");
    write_file(&project.join("README.md"), "probe\n");
    write_file(&project.join("src/lib.rs"), "pub fn answer() -> u32 {\n    42\n}\n");
    if !config.is_empty() {
        write_file(&project.join(".codex/config.toml"), config);
    }
    project
}

fn plan_for(project: &Path, trust: DirTrust, project_only: &[&str]) -> TrustPlan {
    TrustPlan {
        project_only: project_only.iter().map(|name| (*name).to_owned()).collect(),
        ..plan(project, trust)
    }
}

/// The status Codex last gave the tool server `name`, once it left `starting`.
fn server_status(peer: &Peer, name: &str) -> Option<String> {
    peer.notes
        .iter()
        .filter(|n| n.method == method::MCP_STARTUP_STATUS && json_str(&n.params, "/name") == Some(name))
        .filter_map(|n| json_str(&n.params, "/status"))
        .find(|status| *status != "starting")
        .map(str::to_owned)
}

struct Session {
    peer: Peer,
    thread: String,
    home: PathBuf,
}

/// Starts Codex and a thread the way Eitri does, with `spec` as the editor's server, and waits for that server to
/// leave `starting`: a turn started before that would not have the tool.
fn open_session(
    codex: &Path,
    s: &Scratch,
    model: &ScriptedModel,
    project: &Path,
    plan: &TrustPlan,
    spec: Option<&EditorServerSpec>,
    rig: Option<&mut Rig>,
) -> Session {
    let home = s.dir("home");
    write_home(&home, model, "");
    let mut peer = Peer::start(codex, &home, project, decline_all());
    let config = agent::providers::codex::config::session_config(plan, spec).expect("the session's configuration");
    let params = serde_json::to_value(ThreadStartParams::new(
        text_of(project).to_owned(),
        Reviewer::User,
        agent::Sandbox::Project,
        config,
    ))
    .expect("params");
    let thread = start_thread(&mut peer, params);
    assert_narrow_echo(&thread.answer);
    if let (Some(_), Some(rig)) = (spec, rig) {
        let started = drive(&mut peer, rig, STARTUP_BUDGET, |p, _| {
            server_status(p, SERVER_NAME).is_some()
        });
        assert!(
            started,
            "the editor's server did not finish starting; stderr: {}",
            peer.stderr_tail()
        );
        assert_eq!(
            server_status(&peer, SERVER_NAME).as_deref(),
            Some("ready"),
            "the editor's server did not come up; stderr: {}",
            peer.stderr_tail()
        );
    }
    Session {
        peer,
        thread: thread.id,
        home,
    }
}

/// The scripted call of `tool` under the editor's namespace, made once on the turn that carries [`PROMPT`].
fn call_once(call_id: &str, tool: &str, arguments: &Value) -> Step {
    Step::call(call_id, Some(NAMESPACE), tool, arguments)
        .when_body_has(PROMPT)
        .unless_body_has(&format!("\"{call_id}\""))
}

/// Whether Codex asked its client anything about a call: a form for the server, or a command approval.
fn asked_about_a_call(peer: &Peer) -> Vec<String> {
    peer.requests
        .iter()
        .filter(|r| r.method == method::MCP_ELICITATION || is_decision_request(&r.method))
        .map(|r| r.method.clone())
        .collect()
}

// ------------------------------------------------------------------------------------ the tests

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn one_tool_call_reaches_the_broker_and_its_answer_the_model() {
    let codex = real_codex();
    let s = Scratch::new("et-one-call");
    let project = make_project(&s, "");
    let model = ScriptedModel::start(vec![call_once("c1", "editor_state", &json!({}))]).expect("model");
    let mut rig = Rig::new(
        &s,
        &project,
        &every_row(),
        PermissionMode::Manual,
        ProjectTrust::Trusted,
    );
    let spec = rig.spec.clone();
    let plan = plan_for(&project, DirTrust::Trusted, &[]);
    let mut session = open_session(&codex, &s, &model, &project, &plan, Some(&spec), Some(&mut rig));

    assert_eq!(
        serve_turn(&mut session.peer, &mut rig, &session.thread, PROMPT),
        "completed"
    );

    assert_eq!(rig.env.editor.asked(), ["editor_state"], "the editor was asked once");
    assert_eq!(rig.answers.len(), 1, "the broker answered once");
    assert!(!rig.answers[0].is_error, "the answer: {}", rig.answers[0].text);
    assert!(
        rig.answers[0].text.contains("src/lib.rs"),
        "the answer: {}",
        rig.answers[0].text
    );
    let heard = call_outputs(&model)
        .remove("c1")
        .expect("the model was sent the call's output");
    assert!(
        heard.contains("src/lib.rs"),
        "the model was not sent the editor's answer: {heard}"
    );
    assert!(rig.broker.is_idle(), "the broker still holds the call");
    session.peer.close();
}

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn no_codex_card_comes_for_it() {
    let codex = real_codex();
    let s = Scratch::new("et-no-card");
    let project = make_project(&s, "");
    // `open_file` is not read-only: the one kind of tool Codex's own approval mode would stop for.
    let model = ScriptedModel::start(vec![call_once(
        "c1",
        "open_file",
        &json!({"path": "src/lib.rs", "line": 3}),
    )])
    .expect("model");
    let mut rig = Rig::new(
        &s,
        &project,
        &every_row(),
        PermissionMode::Bypass,
        ProjectTrust::Trusted,
    );
    let spec = rig.spec.clone();
    let plan = plan_for(&project, DirTrust::Trusted, &[]);
    let mut session = open_session(&codex, &s, &model, &project, &plan, Some(&spec), Some(&mut rig));

    assert_eq!(
        serve_turn(&mut session.peer, &mut rig, &session.thread, PROMPT),
        "completed"
    );

    assert_eq!(
        asked_about_a_call(&session.peer),
        Vec::<String>::new(),
        "Codex asked about the call"
    );
    assert_eq!(
        rig.env.editor.asked().len(),
        1,
        "the editor was asked {:?}",
        rig.env.editor.asked()
    );
    assert!(rig.env.editor.asked()[0].starts_with("open_file "));
    assert_eq!(rig.answers.len(), 1);
    assert!(!rig.answers[0].is_error, "the answer: {}", rig.answers[0].text);
    assert!(
        rig.broker.cards(TAB).is_empty(),
        "the broker raised a card for a call that needs none"
    );
    session.peer.close();
}

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn the_row_id_is_the_mcp_item_id() {
    let codex = real_codex();
    let s = Scratch::new("et-row-id");
    let project = make_project(&s, "");
    let model = ScriptedModel::start(vec![call_once("c1", "editor_state", &json!({}))]).expect("model");
    let mut rig = Rig::new(
        &s,
        &project,
        &every_row(),
        PermissionMode::Manual,
        ProjectTrust::Trusted,
    );
    let spec = rig.spec.clone();
    let plan = plan_for(&project, DirTrust::Trusted, &[]);
    let mut session = open_session(&codex, &s, &model, &project, &plan, Some(&spec), Some(&mut rig));

    assert_eq!(
        serve_turn(&mut session.peer, &mut rig, &session.thread, PROMPT),
        "completed"
    );

    let items: Vec<String> = session
        .peer
        .notes
        .iter()
        .filter(|n| n.method == method::ITEM_STARTED && json_str(&n.params, "/item/type") == Some("mcpToolCall"))
        .filter_map(|n| json_str(&n.params, "/item/id").map(str::to_owned))
        .collect();
    assert_eq!(items, ["c1"], "Codex's item for the call");
    assert_eq!(rig.seen.len(), 1);
    assert_eq!(rig.seen[0].tool, "editor_state");
    assert_eq!(
        rig.seen[0].row_id.as_deref(),
        Some("c1"),
        "the bridge's row id is the item's id, which a tool row is keyed by"
    );
    session.peer.close();
}

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn the_project_mcp_tripwire_does_not_trip() {
    let codex = real_codex();
    let s = Scratch::new("et-tripwire");
    let marker = s.path("project-server-ran");
    let project = make_project(&s, &tool_server("project_srv", &marker_script(&s), &marker));
    let model = ScriptedModel::start(vec![call_once("c1", "editor_state", &json!({}))]).expect("model");
    let mut rig = Rig::new(
        &s,
        &project,
        &every_row(),
        PermissionMode::Manual,
        ProjectTrust::Untrusted,
    );
    let spec = rig.spec.clone();
    // The project names a server of its own, which an untrusted session must never start; the editor's is no
    // part of the project's configuration.
    let plan = plan_for(&project, DirTrust::Untrusted, &["project_srv"]);
    let mut session = open_session(&codex, &s, &model, &project, &plan, Some(&spec), Some(&mut rig));

    assert_eq!(
        serve_turn(&mut session.peer, &mut rig, &session.thread, PROMPT),
        "completed"
    );

    let watch = TrustWatch::for_plan(&plan, &session.home);
    let reports: Vec<&Note> = session
        .peer
        .notes
        .iter()
        .filter(|n| n.method == method::MCP_STARTUP_STATUS)
        .collect();
    assert!(
        reports
            .iter()
            .any(|n| json_str(&n.params, "/name") == Some(SERVER_NAME)),
        "Codex reported nothing for the editor's server, so the watch was never asked"
    );
    for report in reports {
        let name = json_str(&report.params, "/name").expect("a server name");
        assert!(
            watch.mcp_started(name).is_ok(),
            "the tripwire stops a session over the server {name:?}"
        );
    }
    assert!(!marker.exists(), "the project's own server ran in an untrusted session");
    assert_eq!(rig.env.editor.asked(), ["editor_state"], "the call went through");
    session.peer.close();
}

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn a_collision_attaches_nothing() {
    let codex = real_codex();
    // (name, the project's .codex/config.toml, the user's config.toml tables, whether the name is taken)
    let marker_of = |s: &Scratch| s.path("server-ran");
    for (label, in_project, in_user, taken) in [
        ("project", true, false, true),
        ("user", false, true, true),
        ("none", false, false, false),
    ] {
        let s = Scratch::new(&format!("et-collision-{label}"));
        let marker = marker_of(&s);
        let project_config = if in_project {
            tool_server(SERVER_NAME, &marker_script(&s), &marker)
        } else {
            String::new()
        };
        let project = make_project(&s, &project_config);
        let model = ScriptedModel::start(vec![call_once("c1", "editor_state", &json!({}))]).expect("model");
        let home = s.dir("home");
        let user_tables = if in_user {
            // Switched off, which still merges per key with a runtime entry of the same name.
            format!(
                "\n[mcp_servers.{SERVER_NAME}]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"exit 0\"]\nenabled = false\n"
            )
        } else {
            String::new()
        };
        write_home(&home, &model, &user_tables);

        // What the trust worker reads, and what it decides from it.
        let user = read_user_codex_config(&home.join("config.toml"));
        let env = ProfileEnv {
            fallback_names: user.fallback_names,
            user_mcp_servers: user.mcp_servers,
            user_mcp_names: user.all_server_names,
            user_config_unreadable: user.unreadable,
            codex_home: Some(home.clone()),
        };
        let found = discover(&project, Some(&s.0), &Limits::default(), &Profile::Codex, &env);
        assert_eq!(eitri_server_taken(&found), taken, "{label}: the name's state");

        // A tab whose name is taken is started with no server, so none is bound and none is handed to Codex.
        let mut rig = Rig::new(
            &s,
            &project,
            &every_row(),
            PermissionMode::Manual,
            ProjectTrust::Untrusted,
        );
        let spec = rig.spec.clone();
        let plan = plan_for(&project, DirTrust::Untrusted, &[]);
        let given = (!taken).then_some(&spec);
        let mut session = open_session(&codex, &s, &model, &project, &plan, given, Some(&mut rig));
        let sent = agent::providers::codex::config::session_config(&plan, given).expect("config");
        let sent = serde_json::to_value(sent).expect("json");
        assert_eq!(
            sent.pointer("/mcp_servers").is_some(),
            !taken,
            "{label}: the servers Codex was given"
        );

        assert_eq!(
            serve_turn(&mut session.peer, &mut rig, &session.thread, PROMPT),
            "completed"
        );

        // The model calls the editor's tool either way: it is there when the name was free, and the call is
        // refused by Codex, before any bridge, when it was not.
        let heard = call_outputs(&model).remove("c1").unwrap_or_default();
        assert_eq!(
            heard.contains("src/lib.rs"),
            !taken,
            "{label}: the model's call of the editor's tool came back {heard}"
        );
        assert_eq!(rig.env.editor.asked().is_empty(), taken, "{label}: the editor's calls");
        if taken {
            assert!(rig.seen.is_empty(), "{label}: a bridge reached the window");
            assert!(!marker.exists(), "{label}: the project's server of that name ran");
        }
        session.peer.close();
    }
}

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn an_interrupt_ends_the_call_and_its_card() {
    let codex = real_codex();
    let s = Scratch::new("et-interrupt");
    let project = make_project(&s, "");
    // A file outside the project: in Manual mode the tool raises a card and waits on the person.
    let outside = s.path("outside.txt");
    write_file(&outside, "outside\n");
    let next_turn = "after the interrupt";
    let model = ScriptedModel::start(vec![
        call_once("c1", "open_file", &json!({"path": text_of(&outside)})),
        Step::call("c2", Some(NAMESPACE), "editor_state", &json!({}))
            .when_body_has(next_turn)
            .unless_body_has("\"c2\""),
    ])
    .expect("model");
    let mut rig = Rig::new(
        &s,
        &project,
        &every_row(),
        PermissionMode::Manual,
        ProjectTrust::Trusted,
    );
    let spec = rig.spec.clone();
    let plan = plan_for(&project, DirTrust::Trusted, &[]);
    let mut session = open_session(&codex, &s, &model, &project, &plan, Some(&spec), Some(&mut rig));

    let turn = send_turn(&mut session.peer, &session.thread, PROMPT);
    let carded = drive(&mut session.peer, &mut rig, TURN_BUDGET, |_, rig| {
        !rig.broker.cards(TAB).is_empty()
    });
    assert!(carded, "no card was raised; stderr: {}", session.peer.stderr_tail());
    assert!(
        rig.env.editor.asked().is_empty(),
        "the editor was asked before the person"
    );
    let call = rig.seen[0].id.clone();

    let params = serde_json::to_value(TurnInterruptParams {
        thread_id: session.thread.clone(),
        turn_id: turn.clone(),
    })
    .expect("params");
    session
        .peer
        .call(method::TURN_INTERRUPT, params)
        .expect("turn/interrupt");
    let ended = drive(&mut session.peer, &mut rig, TURN_BUDGET, |p, _| {
        p.turn_status(&session.thread, &turn).is_some()
    });
    assert!(
        ended,
        "the interrupted turn did not end; stderr: {}",
        session.peer.stderr_tail()
    );
    assert_eq!(
        session.peer.turn_status(&session.thread, &turn).as_deref(),
        Some("interrupted")
    );

    // What differs by version is only who ends the call: Codex's cancel, or the host at the turn's end.
    let cancels = cancels_calls(&codex);
    if cancels {
        let told = drive(&mut session.peer, &mut rig, TURN_BUDGET, |_, rig| {
            !rig.cancelled.is_empty()
        });
        assert!(told, "this Codex sent no cancel for the interrupted call");
        assert_eq!(rig.cancelled, [call.clone()]);
        assert!(rig.broker.cards(TAB).is_empty(), "the cancel left the card");
        assert!(rig.answers.is_empty(), "a cancelled call was answered");
    } else {
        assert!(rig.cancelled.is_empty(), "this Codex cancelled the call");
        assert_eq!(
            rig.broker.cards(TAB).len(),
            1,
            "the card stays until the host ends the turn's calls"
        );
    }
    rig.turn_interrupted();
    assert!(rig.broker.cards(TAB).is_empty(), "the card outlived the turn");
    assert!(rig.broker.is_idle(), "the broker still holds the call");
    if cancels {
        assert!(rig.answers.is_empty(), "the turn's end answered a cancelled call");
    } else {
        assert_eq!(rig.answers.len(), 1);
        assert!(rig.answers[0].is_error);
        assert_eq!(rig.answers[0].text, words::INTERRUPTED);
    }
    assert!(
        rig.env.editor.asked().is_empty(),
        "the editor was asked for a call that was never allowed"
    );

    // An answer that crossed the interrupt on the way: it reaches Codex when the bridge still holds the call, and the
    // model never hears it.
    let stale = "STALE-ANSWER";
    rig.host.answer(
        &call,
        &ToolResult {
            text: stale.to_owned(),
            is_error: false,
        },
    );

    // The tab goes on: the next turn runs, and the same bridge carries its call.
    assert_eq!(
        serve_turn(&mut session.peer, &mut rig, &session.thread, next_turn),
        "completed"
    );
    assert_eq!(rig.env.editor.asked(), ["editor_state"]);
    let heard = call_outputs(&model);
    assert!(
        heard.get("c2").is_some_and(|text| text.contains("src/lib.rs")),
        "{heard:?}"
    );
    assert!(
        !requests_text(&model, 0).contains(stale),
        "the model heard a stale answer"
    );
    assert!(
        !session
            .peer
            .notes
            .iter()
            .any(|n| n.method == method::ITEM_COMPLETED && json_str(&n.params, "/item/id") == Some("c1")),
        "Codex completed the interrupted call after all"
    );
    eprintln!("what the model was told of the interrupted call: {:?}", heard.get("c1"));
    session.peer.close();
}

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn a_steer_leaves_the_card_and_the_call_runs() {
    let codex = real_codex();
    let s = Scratch::new("et-steer");
    let project = make_project(&s, "");
    let outside = s.path("outside.txt");
    write_file(&outside, "outside\n");
    let model =
        ScriptedModel::start(vec![call_once("c1", "open_file", &json!({"path": text_of(&outside)}))]).expect("model");
    let mut rig = Rig::new(
        &s,
        &project,
        &every_row(),
        PermissionMode::Manual,
        ProjectTrust::Trusted,
    );
    let spec = rig.spec.clone();
    let plan = plan_for(&project, DirTrust::Trusted, &[]);
    let mut session = open_session(&codex, &s, &model, &project, &plan, Some(&spec), Some(&mut rig));

    let turn = send_turn(&mut session.peer, &session.thread, PROMPT);
    let carded = drive(&mut session.peer, &mut rig, TURN_BUDGET, |_, rig| {
        !rig.broker.cards(TAB).is_empty()
    });
    assert!(carded, "no card was raised; stderr: {}", session.peer.stderr_tail());

    let steer = "STEER-TEXT";
    let params = serde_json::to_value(TurnSteerParams::new(
        session.thread.clone(),
        "steer-1".to_owned(),
        steer.to_owned(),
        turn.clone(),
    ))
    .expect("params");
    session.peer.call(method::TURN_STEER, params).expect("turn/steer");

    // A message into the running turn gives no call up, so no cancel comes and the card stays.
    let settled = Instant::now() + Duration::from_millis(500);
    let waited = drive(&mut session.peer, &mut rig, TURN_BUDGET, |p, rig| {
        Instant::now() >= settled || !rig.cancelled.is_empty() || p.turn_status(&session.thread, &turn).is_some()
    });
    assert!(waited, "Codex stopped; stderr: {}", session.peer.stderr_tail());
    assert!(rig.cancelled.is_empty(), "the steer cancelled the call");
    assert_eq!(rig.broker.cards(TAB).len(), 1, "the steer took the card");
    assert_eq!(session.peer.turn_status(&session.thread, &turn), None);

    // The person's answer still counts: the call runs and the model reads its result.
    let card = rig.broker.cards(TAB).remove(0).id;
    rig.broker
        .answer_card(TAB, &card, CardDecision::Allow, CardOrigin::Key, Instant::now())
        .expect("the card is open");
    let ended = drive(&mut session.peer, &mut rig, TURN_BUDGET, |p, _| {
        p.turn_status(&session.thread, &turn).is_some()
    });
    assert!(ended, "the turn did not end; stderr: {}", session.peer.stderr_tail());
    assert_eq!(
        session.peer.turn_status(&session.thread, &turn).as_deref(),
        Some("completed")
    );
    assert_eq!(rig.env.editor.asked().len(), 1, "{:?}", rig.env.editor.asked());
    assert!(rig.env.editor.asked()[0].starts_with("open_file"));
    assert_eq!(rig.answers.len(), 1);
    assert!(!rig.answers[0].is_error, "the answer: {}", rig.answers[0].text);
    assert!(rig.cancelled.is_empty(), "a cancel came after the steer");
    // Codex's own output for a call it dropped would be there too, so the text is what says the answer arrived.
    let heard = call_outputs(&model).remove("c1").unwrap_or_default();
    assert!(
        heard.contains("shown"),
        "the model did not read the call's answer: {heard}"
    );
    assert!(
        requests_text(&model, 0).contains(steer),
        "the model never read the steer"
    );
    assert!(rig.broker.is_idle(), "the broker still holds the call");
    session.peer.close();
}

#[test]
#[ignore = "runs the real codex (on PATH) and the built bridge against the scripted model; no login, no network"]
fn a_tool_that_is_off_is_refused_though_codex_calls_it() {
    let codex = real_codex();
    let s = Scratch::new("et-off");
    let project = make_project(&s, "");
    // The list the session was given leaves `read_buffer` out, and the scripted model names it anyway.
    let rows: Vec<&'static ToolRow> = every_row()
        .into_iter()
        .filter(|row| row.name != "read_buffer")
        .collect();
    assert_eq!(
        rows.len(),
        every_row().len() - 1,
        "the catalogue has a read_buffer to leave out"
    );
    let model =
        ScriptedModel::start(vec![call_once("c1", "read_buffer", &json!({"path": "src/lib.rs"}))]).expect("model");
    let mut rig = Rig::new(&s, &project, &rows, PermissionMode::Manual, ProjectTrust::Trusted);
    let spec = rig.spec.clone();
    let plan = plan_for(&project, DirTrust::Trusted, &[]);
    let mut session = open_session(&codex, &s, &model, &project, &plan, Some(&spec), Some(&mut rig));

    assert_eq!(
        serve_turn(&mut session.peer, &mut rig, &session.thread, PROMPT),
        "completed"
    );

    assert!(
        rig.env.editor.asked().is_empty(),
        "an editor call was made for a tool the session was not given: {:?}",
        rig.env.editor.asked()
    );
    let heard = call_outputs(&model).remove("c1").unwrap_or_default();
    assert!(!heard.is_empty(), "the model was told nothing of the call");
    assert!(!heard.contains("pub fn answer"), "the model was sent the file: {heard}");
    // Whichever side refused, the model hears a refusal; when the call did reach the window, the broker's is it.
    if let Some(seen) = rig.seen.first() {
        assert_eq!(seen.tool, "read_buffer");
        assert_eq!(rig.answers.len(), 1);
        assert!(rig.answers[0].is_error, "the broker's answer: {}", rig.answers[0].text);
        assert!(
            rig.answers[0].text.contains("not available in this session"),
            "the broker's answer: {}",
            rig.answers[0].text
        );
    }
    eprintln!(
        "the call to a tool that is off was refused by {}; the model heard: {heard}",
        if rig.seen.is_empty() { "Codex" } else { "the broker" }
    );
    session.peer.close();
}
