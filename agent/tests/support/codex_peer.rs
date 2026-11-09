//! The harness shared by the real-Codex suites: scratch directories, the `codex app-server` peer, and
//! the thread and turn calls Eitri makes. Included with `#[path = "support/codex_peer.rs"] mod codex_peer;`.

#![allow(dead_code, unused_imports)]

use agent::external::{DirTrust, Reviewer, TrustPlan};
use agent::jsonrpc_link::{Framing, Incoming, JsonRpcLink, LinkSpec, Skipped};
use agent::providers::codex::config::{expected_echo, session_config};
use agent::providers::codex::family::{Family, FamilyEdge};
use agent::providers::codex::scripted_model::{ScriptedModel, Step};
use agent::providers::codex::self_check::SCRIPTED_MODEL_SLUG;
use agent::providers::codex::translate::version_text;
use agent::providers::codex::tripwire;
use agent::providers::codex::wire::{
    method, InitializeParams, ThreadResumeParams, ThreadStartParams, ThreadStartResponse, TurnStartParams,
};
use agent::providers::codex::{locate_codex, BinaryKey, MIN_CODEX};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

/// The namespace Codex's multi-agent tools live in.
pub const COLLAB: &str = "collaboration";
/// One request's answer.
pub const CALL_BUDGET: Duration = Duration::from_secs(30);
/// A turn, children included.
pub const TURN_BUDGET: Duration = Duration::from_secs(60);
/// The grace a shutdown is given before the captured pid is killed.
pub const GRACE: Duration = Duration::from_secs(6);

// ------------------------------------------------------------------------------------ scratch

pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codex-real-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(std::fs::canonicalize(&dir).expect("canonical scratch dir"))
    }

    /// A directory under the scratch, made.
    pub fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).expect("dir");
        path
    }

    /// A path under the scratch, not made.
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Set to read a run's logs after it.
        if std::env::var_os("EITRI_KEEP_SCRATCH").is_some() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(path, text).expect("write");
}

pub fn text_of(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}

/// A script that records that it ran, with its pid, in the file named by its first argument.
pub fn marker_script(s: &Scratch) -> PathBuf {
    let path = s.path("marker.sh");
    write_file(&path, "#!/bin/sh\necho \"$$\" > \"$1\"\nexit 0\n");
    make_executable(&path);
    path
}

/// A tool server that answers the protocol, records its pid in `pid_file` and, with `linger`, keeps running
/// after its input closes (a server that outlives the process that started it).
pub fn local_tool_server(s: &Scratch, name: &str, pid_file: &Path, linger: bool) -> String {
    let script = s.path("tool-server.py");
    write_file(&script, LOCAL_TOOL_SERVER);
    format!(
        "\n[mcp_servers.{name}]\ncommand = \"python3\"\nargs = [\"{}\", \"{}\"{}]\n",
        text_of(&script),
        text_of(pid_file),
        if linger { ", \"linger\"" } else { "" }
    )
}

pub fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// The project's `.codex/config.toml`: a reasoning effort a project can be told apart by, then `extra`.
pub fn write_project(dir: &Path, extra: &str) {
    write_file(
        &dir.join(".codex/config.toml"),
        &format!("model_reasoning_effort = \"minimal\"\n{extra}"),
    );
    write_file(&dir.join("README.md"), "probe\n");
}

/// The `[mcp_servers.<name>]` table of a project tool server that runs `script <marker file>`.
pub fn tool_server(name: &str, script: &Path, marker: &Path) -> String {
    format!(
        "\n[mcp_servers.{name}]\ncommand = \"/bin/sh\"\nargs = [\"{}\", \"{}\"]\nstartup_timeout_sec = 2\n",
        text_of(script),
        text_of(marker)
    )
}

/// Instructions and skills a project (and the home) would hand the model if they were loaded.
pub const AGENTS_MARKER: &str = "EITRI-REAL-AGENTS-MARKER";
pub const SKILL_A: &str = "eitri-real-skill-a";
pub const SKILL_B: &str = "eitri-real-skill-b";
pub const SKILL_USER: &str = "eitri-real-skill-user";

pub fn write_skill(dir: &Path, name: &str) {
    write_file(
        &dir.join(name).join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: marker skill {name}.\n---\nMarker.\n"),
    );
}

pub fn write_instructions_and_skills(project: &Path, home: &Path) {
    write_file(&project.join("AGENTS.md"), &format!("{AGENTS_MARKER} instructions\n"));
    write_skill(&project.join(".codex/skills"), SKILL_A);
    write_skill(&project.join(".agents/skills"), SKILL_B);
    write_skill(&home.join("skills"), SKILL_USER);
}

/// The features a test home turns off so that Codex's own start-up does not go looking for plugins and
/// apps on the network: the suite promises that nothing leaves the machine.
pub const QUIET_FEATURES: &str = "plugins = false\napps = false\n";

/// The home's `config.toml`: the scripted model, then `tables` (each one a table of its own).
pub fn write_home(home: &Path, model: &ScriptedModel, tables: &str) {
    write_home_for(home, model, SCRIPTED_MODEL_SLUG, tables)
}

/// [`write_home`] for a model slug of the caller's: one Codex has no catalog entry for is offered the plain
/// tool list (no code mode), where a tool server's tools are plain functions.
pub fn write_home_for(home: &Path, model: &ScriptedModel, slug: &str, tables: &str) {
    // One `[features]` table only: the test's own, with the quiet switches added, or a table of ours.
    let tables = if tables.contains("[features]\n") {
        tables.replace("[features]\n", &format!("[features]\n{QUIET_FEATURES}"))
    } else {
        format!("{tables}\n[features]\n{QUIET_FEATURES}")
    };
    write_file(
        &home.join("config.toml"),
        &format!("{}{}", model.provider_toml(slug), tables),
    );
}

// ------------------------------------------------------------------------------------ codex

/// `codex --version` sets its helpers up under `CODEX_HOME`, and every version probe, the product's included, inherits
/// this process's environment. So a suite that gave its app-servers private homes would still write to the home the
/// caller has: this replaces it for the whole process, whatever was exported, before the first child. Under the
/// target, because Codex refuses a helper directory under the system's temporary directory.
fn keep_codex_children_out_of_the_callers_home() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codex-real-probe-home");
        std::fs::create_dir_all(&home).expect("the probes' home");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("a private home");
        std::env::set_var("CODEX_HOME", std::fs::canonicalize(&home).expect("a canonical home"));
    });
}

/// The way every real-Codex suite reaches its binary.
pub fn real_codex() -> PathBuf {
    keep_codex_children_out_of_the_callers_home();
    let program = locate_codex(None).expect("a codex on PATH (or EITRI_CODEX)");
    let version = Command::new(&program)
        .arg("--version")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"));
    eprintln!(
        "codex: {version} at {}; scripted model slug {SCRIPTED_MODEL_SLUG}",
        program.display()
    );
    require_supported_codex(&program);
    program
}

/// Fails at once, saying why, when `program` is older than the oldest Codex Eitri starts. Eitri refuses
/// such a binary before any session, so what one does with these tests says nothing about Eitri, and
/// letting them run would only bury that under failures that look like protocol defects.
pub fn require_supported_codex(program: &Path) {
    let key = BinaryKey::of(program).unwrap_or_else(|e| panic!("{}: {e}", program.display()));
    match key.version_numbers() {
        Some(found) if found >= MIN_CODEX => {}
        found => panic!(
            "{} reports {:?} ({found:?}); these tests need Codex {} or newer, the oldest Eitri starts: \
             put a newer codex first on PATH or name it in EITRI_CODEX",
            program.display(),
            key.version(),
            version_text(MIN_CODEX),
        ),
    }
}

pub fn json_str<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

/// Something Codex announced.
#[derive(Debug, Clone)]
pub struct Note {
    pub at: Instant,
    pub method: String,
    pub params: Value,
}

/// A request Codex made of the client.
#[derive(Debug, Clone)]
pub struct Req {
    pub at: Instant,
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// Decides a request for a decision: `Some` answers it, `None` leaves it unanswered.
pub type Answer = Box<dyn FnMut(&Req) -> Option<Value>>;

pub fn decline_all() -> Answer {
    Box::new(|_| Some(json!({"decision": "decline"})))
}

pub fn is_decision_request(method: &str) -> bool {
    method.ends_with("requestApproval")
}

/// One `codex app-server` child and everything it said.
pub struct Peer {
    pub link: JsonRpcLink,
    pub notes: Vec<Note>,
    pub requests: Vec<Req>,
    pub skipped: Vec<Skipped>,
    pub closed: Option<String>,
    pub answer: Answer,
}

impl Peer {
    /// Starts the app-server in `cwd` with `home` as its `CODEX_HOME` and does the handshake. Nothing
    /// reaches the network beyond loopback and no API key is passed on.
    pub fn start(codex: &Path, home: &Path, cwd: &Path, answer: Answer) -> Peer {
        let no_proxy = OsString::from("127.0.0.1,localhost");
        let mut peer = Peer::spawn(
            codex,
            cwd,
            answer,
            vec![
                (OsString::from("CODEX_HOME"), home.as_os_str().to_os_string()),
                (OsString::from("NO_PROXY"), no_proxy.clone()),
                (OsString::from("no_proxy"), no_proxy),
            ],
            &["OPENAI_API_KEY", "CODEX_API_KEY"],
        );
        let init = peer.handshake();
        // Nothing may be sent to a Codex that reads some other home than the private one.
        let reported = json_str(&init, "/codexHome").and_then(|p| std::fs::canonicalize(p).ok());
        assert_eq!(
            reported,
            std::fs::canonicalize(home).ok(),
            "codex does not use the private home it was given"
        );
        peer
    }

    /// Starts the app-server in `cwd` under the account the machine's own environment gives it (the
    /// machine's Codex login), for the tests that spend real turns. Only the variables that would point it at a
    /// terminal multiplexer or an editor are taken away.
    pub fn start_on_login(codex: &Path, cwd: &Path, answer: Answer) -> Peer {
        let mut peer = Peer::spawn(codex, cwd, answer, Vec::new(), &[]);
        peer.handshake();
        peer
    }

    fn spawn(codex: &Path, cwd: &Path, answer: Answer, env: Vec<(OsString, OsString)>, remove: &[&str]) -> Peer {
        let mut env_remove: Vec<OsString> = ["TMUX", "TMUX_PANE", "NVIM", "NVIM_LISTEN_ADDRESS"]
            .iter()
            .map(OsString::from)
            .collect();
        env_remove.extend(remove.iter().map(OsString::from));
        let link = JsonRpcLink::spawn(LinkSpec {
            program: codex.to_path_buf(),
            args: ["app-server", "--listen", "stdio://"]
                .iter()
                .map(OsString::from)
                .collect(),
            cwd: cwd.to_path_buf(),
            framing: Framing::Bare,
            env,
            env_remove,
            over_cap: agent::providers::codex::approvals::over_cap_reply,
        })
        .expect("codex app-server starts");
        Peer {
            link,
            notes: Vec::new(),
            requests: Vec::new(),
            skipped: Vec::new(),
            closed: None,
            answer,
        }
    }

    /// `initialize`, then `initialized`; returns the answer.
    fn handshake(&mut self) -> Value {
        let init = self
            .call(
                method::INITIALIZE,
                serde_json::to_value(InitializeParams::eitri(env!("CARGO_PKG_VERSION"))).expect("params"),
            )
            .expect("initialize");
        let _ = self.link.notify(method::INITIALIZED, Value::Null);
        init
    }

    pub fn pump(&mut self) {
        while let Some(message) = self.link.try_recv() {
            match message {
                Incoming::Notification { method, params } => self.notes.push(Note {
                    at: Instant::now(),
                    method,
                    params,
                }),
                Incoming::Request { id, method, params } => {
                    let req = Req {
                        at: Instant::now(),
                        id,
                        method,
                        params,
                    };
                    if is_decision_request(&req.method) || req.method == method::MCP_ELICITATION {
                        if let Some(answer) = (self.answer)(&req) {
                            self.link.respond(req.id.clone(), answer);
                        }
                    } else {
                        let _ = self.link.respond_error(req.id.clone(), -32601, "method not found");
                    }
                    self.requests.push(req);
                }
                Incoming::Skipped(what) => self.skipped.push(what),
                Incoming::Closed { reason } => self.closed = Some(reason),
            }
        }
    }

    /// A request and its answer, or the error text.
    pub fn call(&mut self, name: &str, params: Value) -> Result<Value, String> {
        let mut pending = self.link.request(name, params);
        let end = Instant::now() + CALL_BUDGET;
        loop {
            self.pump();
            if let Some(answer) = pending.try_take() {
                return answer.map_err(|e| e.to_string());
            }
            if let Some(reason) = &self.closed {
                return Err(format!("{name}: codex ended ({reason})"));
            }
            assert!(Instant::now() < end, "{name} was not answered in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until `done` holds; false on the time running out or the child ending.
    pub fn wait_for(&mut self, within: Duration, mut done: impl FnMut(&Peer) -> bool) -> bool {
        let end = Instant::now() + within;
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            if self.closed.is_some() || Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Keeps reading for this long.
    pub fn pump_for(&mut self, how_long: Duration) {
        self.wait_for(how_long, |_| false);
    }

    pub fn note(&self, name: &str) -> Option<&Note> {
        self.notes.iter().find(|n| n.method == name)
    }

    pub fn turn_started(&self, thread: &str) -> bool {
        self.notes
            .iter()
            .any(|n| n.method == method::TURN_STARTED && json_str(&n.params, "/threadId") == Some(thread))
    }

    /// The status of the turn that ended on `thread` (the first one at or after `skip` completions).
    pub fn turn_status(&self, thread: &str, turn: &str) -> Option<String> {
        self.notes
            .iter()
            .find(|n| {
                n.method == method::TURN_COMPLETED
                    && json_str(&n.params, "/threadId") == Some(thread)
                    && json_str(&n.params, "/turn/id") == Some(turn)
            })
            .and_then(|n| json_str(&n.params, "/turn/status").map(str::to_string))
    }

    pub fn wait_turn(&mut self, thread: &str, turn: &str, within: Duration) -> Option<String> {
        let (thread, turn) = (thread.to_string(), turn.to_string());
        self.wait_for(within, |p| p.turn_status(&thread, &turn).is_some());
        self.turn_status(&thread, &turn)
    }

    /// The thread an announced child runs on: the first `subAgentActivity` that started one.
    pub fn announced_children(&self) -> Vec<(String, String)> {
        self.notes
            .iter()
            .filter(|n| n.method == method::ITEM_STARTED || n.method == method::ITEM_COMPLETED)
            .filter(|n| json_str(&n.params, "/item/type") == Some("subAgentActivity"))
            .filter(|n| json_str(&n.params, "/item/kind") == Some("started"))
            .filter_map(|n| {
                Some((
                    json_str(&n.params, "/threadId")?.to_string(),
                    json_str(&n.params, "/item/agentThreadId")?.to_string(),
                ))
            })
            .collect()
    }

    /// Closes stdin and returns how long the child took to go; it is killed after `GRACE`.
    pub fn close(self) -> Duration {
        let at = Instant::now();
        self.link.shutdown(GRACE);
        at.elapsed()
    }

    pub fn stderr_tail(&self) -> String {
        self.link.stderr_tail()
    }
}

// ------------------------------------------------------------------------------------ the session

pub fn plan(dir: &Path, trust: DirTrust) -> TrustPlan {
    TrustPlan {
        dirs: vec![(dir.to_path_buf(), trust)],
        skills_off: trust == DirTrust::Untrusted,
        fingerprint: "real-suite".to_string(),
        project_only: Vec::new(),
    }
}

/// The `thread/start` params Eitri sends for this project and trust.
pub fn start_params(project: &Path, trust: DirTrust) -> Value {
    let config = session_config(&plan(project, trust), None).expect("config");
    serde_json::to_value(ThreadStartParams::new(
        text_of(project).to_string(),
        Reviewer::User,
        agent::Sandbox::Project,
        config,
    ))
    .expect("params")
}

/// The `thread/resume` params Eitri sends.
pub fn resume_params(thread: &str, project: &Path, trust: DirTrust) -> Value {
    let config = session_config(&plan(project, trust), None).expect("config");
    serde_json::to_value(ThreadResumeParams::new(
        thread.to_string(),
        text_of(project).to_string(),
        Reviewer::User,
        agent::Sandbox::Project,
        config,
    ))
    .expect("params")
}

/// A started thread: its id and the whole answer.
pub struct Thread {
    pub id: String,
    pub answer: Value,
}

pub fn start_thread(peer: &mut Peer, params: Value) -> Thread {
    let answer = peer.call(method::THREAD_START, params).expect("thread/start");
    let id = json_str(&answer, "/thread/id").expect("a thread id").to_string();
    Thread { id, answer }
}

/// Fails when a start or resume answer does not echo the narrow policy Eitri asked for.
pub fn assert_narrow_echo(answer: &Value) {
    assert_echo(answer, agent::Sandbox::Project);
}

/// [`assert_narrow_echo`] for a session asked to run under `sandbox`.
pub fn assert_echo(answer: &Value, sandbox: agent::Sandbox) {
    let parsed: ThreadStartResponse = serde_json::from_value(answer.clone()).expect("a thread answer");
    if let Err(mismatch) = tripwire::check_echo(&expected_echo(Reviewer::User, sandbox), &parsed.echo()) {
        panic!("the policy echo differs from what was asked: {mismatch}");
    }
}

pub fn instruction_sources(answer: &Value) -> Vec<String> {
    answer
        .get("instructionSources")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Starts a turn and returns its id.
pub fn send_turn(peer: &mut Peer, thread: &str, text: &str) -> String {
    send_turn_in(peer, thread, text, agent::Sandbox::Project)
}

/// [`send_turn`] under `sandbox`.
pub fn send_turn_in(peer: &mut Peer, thread: &str, text: &str, sandbox: agent::Sandbox) -> String {
    let params = serde_json::to_value(TurnStartParams::new(
        thread.to_string(),
        text.to_string(),
        Reviewer::User,
        sandbox,
    ))
    .expect("params");
    let answer = peer.call(method::TURN_START, params).expect("turn/start");
    json_str(&answer, "/turn/id").expect("a turn id").to_string()
}

/// Runs one turn to its end and returns the status.
pub fn run_turn(peer: &mut Peer, thread: &str, text: &str) -> String {
    let turn = send_turn(peer, thread, text);
    peer.wait_turn(thread, &turn, TURN_BUDGET)
        .unwrap_or_else(|| panic!("the turn did not end; stderr: {}", peer.stderr_tail()))
}

/// Whether the user's `config.toml` in the home gained a project entry.
pub fn home_has_project_entry(home: &Path) -> bool {
    std::fs::read_to_string(home.join("config.toml")).is_ok_and(|text| text.contains("[projects"))
}

// ------------------------------------------------------------------------------------ the model

/// Every `function_call_output` the model was sent, by call id.
pub fn call_outputs(model: &ScriptedModel) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for body in model.requests() {
        for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
            if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    let output = match item.get("output") {
                        Some(Value::String(text)) => text.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    out.insert(id.to_string(), output);
                }
            }
        }
    }
    out
}

/// The text of the requests from index `from` on.
pub fn requests_text(model: &ScriptedModel, from: usize) -> String {
    model.requests()[from..]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Waits for a file to exist.
pub fn appears(path: &Path, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

/// Waits until the model has been sent `count` requests. A turn is only really in progress once the
/// model has its request: a project tool server that is still starting holds the first one back.
pub fn model_has_requests(model: &ScriptedModel, count: usize, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if model.requests().len() >= count {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    model.requests().len() >= count
}

pub fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Kills a process by the pid it wrote down when the test ends, however it ends.
pub struct KillOnDrop(pub PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|t| t.trim().parse::<i32>().ok())
        {
            // SAFETY: the pid was written by the tool server this test started; at worst it is gone.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Kills a process by a pid captured earlier when the test ends.
pub struct KillPid(pub i32);

impl Drop for KillPid {
    fn drop(&mut self) {
        // SAFETY: the pid is a tool server this test started; at worst it is already gone.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

pub fn pid_in(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// A stdio tool server (newline-delimited JSON-RPC) answering `initialize`, `tools/list` and
/// `tools/call`. `argv[1]` is a file to write its pid to; with `argv[2] == "linger"` it keeps running
/// after its input closes, like a server that outlives the process that started it.
pub const LOCAL_TOOL_SERVER: &str = r#"#!/usr/bin/env python3
import json
import os
import sys
import time

with open(sys.argv[1], "w") as f:
    f.write(str(os.getpid()))
linger = len(sys.argv) > 2 and sys.argv[2] == "linger"


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    try:
        request = json.loads(line)
    except ValueError:
        continue
    method = request.get("method")
    if "id" not in request:
        continue
    if method == "initialize":
        wanted = (request.get("params") or {}).get("protocolVersion", "2025-03-26")
        send({"jsonrpc": "2.0", "id": request["id"], "result": {
            "protocolVersion": wanted,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "eitri-local", "version": "0"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"tools": [{
            "name": "ping",
            "description": "Answers pong. Takes no arguments.",
            "inputSchema": {"type": "object", "properties": {}}}]}})
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": request["id"], "result": {
            "content": [{"type": "text", "text": "pong"}]}})
    else:
        send({"jsonrpc": "2.0", "id": request["id"], "result": {}})
if linger:
    time.sleep(300)
"#;
