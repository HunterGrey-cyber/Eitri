//! The bridge's state: what Neovim's language client has told its server, kept so a server on another machine can be
//! brought back to the same place after a link drops, a helper restarts or a server dies. A pure state machine: each
//! event gives the messages to send each way and what to do next ([`Out`]); the I/O is the session's
//! (`remote_lsp::session`).
//!
//! The shape (Neovim's `initialize`, the open documents, the dynamic registrations and the ids of requests in flight
//! kept for a replay on a fresh connection) follows remote-ssh.nvim (MIT, b0fc5c8), rewritten here for a helper that
//! keeps the server alive across the link's loss.
//!
//! Every message to Neovim passes [`Bridge::emit_to_nvim`] and every message to the server [`Bridge::emit_to_server`], the
//! bridge's own as well as the ones it forwards: the one place a rewrite of what they carry belongs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent::remote_answers::{AnswerBound, ITEM_OVERHEAD_BYTES};
use agent::remote_client::terminal_transport::wire::{
    LspAttached, LspEndReason, LspEnded, LspRegistration, LSP_DROPPED_SAVES_MAX, LSP_REGISTRATIONS_MAX,
    LSP_REGISTRATIONS_PARAMS_MAX, LSP_STDERR_TAIL, LSP_URI_MAX,
};
use serde_json::{json, Value};

use super::documents::{file_uri_path, Document, Encoding};
use super::framing::Probe;
use super::uris::{names_the_scheme, Rewriter};
use crate::remote_sync::terminal_text;

/// JSON-RPC and LSP error codes the bridge answers with.
pub const CONTENT_MODIFIED: i64 = -32801;
pub const REQUEST_FAILED: i64 = -32803;
const SERVER_NOT_INITIALIZED: i64 = -32002;
const INVALID_REQUEST: i64 = -32600;

/// A second exit of the server within this long of the first ends the bridge.
pub const RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);

/// What the helper may tell a returning bridge: counts and bytes, refused whole past either.
const REGISTRATIONS: AnswerBound = AnswerBound::new(
    "the language server's registrations",
    LSP_REGISTRATIONS_MAX,
    LSP_REGISTRATIONS_PARAMS_MAX + LSP_REGISTRATIONS_MAX * ITEM_OVERHEAD_BYTES,
);
const DROPPED_SAVES: AnswerBound = AnswerBound::new(
    "the held saves the language server dropped",
    LSP_DROPPED_SAVES_MAX,
    LSP_DROPPED_SAVES_MAX * (LSP_URI_MAX + ITEM_OVERHEAD_BYTES),
);
/// A registration's method is `client/registerCapability` or `client/unregisterCapability`.
const REGISTRATION_METHOD_MAX: usize = 64;
/// How many files' changes Neovim reports while the link is down are kept for the server, merged per file.
pub const WATCHED_CHANGES_MAX: usize = 16 * 1024;
const WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";

pub struct Config {
    /// The host as the person typed it, for the lines shown.
    pub host: String,
    pub root: PathBuf,
    /// The server's command line as the editor gave it.
    pub command: Vec<String>,
}

/// The laptop's files, as far as the bridge looks at them.
pub trait Disk {
    /// The SHA-256 of the file at `path` as lowercase hex, read in bounded pieces; an error for a file that cannot be
    /// read (not a regular file, a link, past the size limit).
    fn sha256(&self, path: &Path) -> io::Result<String>;
    /// Whether sync carries the file at `path`: inside the root and not excluded.
    fn carried(&self, path: &Path) -> bool;
    /// The rewrite of the URIs the mirror does not hold, for the project this disk is the mirror of, on `host`.
    fn rewriter(&self, host: &str) -> Rewriter;
}

/// What the session does besides sending messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open a server (a fresh one) and attach to it.
    OpenServer,
    /// Stop the server, if there is one.
    StopServer,
    /// End the process with this exit code once what is queued is written.
    Exit(i32),
    /// End the process with this exit code once Neovim has sent nothing for the session's quiet wait (any message from
    /// it starts the wait again): a client that cannot use the bridge and never says `exit` does not keep it alive.
    ExitWhenQuiet(i32),
    /// Ask the project's sync to carry the laptop's changes now: a save the helper holds until its file arrives would
    /// otherwise wait for sync to notice the file by itself, which can take longer than the hold.
    FlushSync,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToServer {
    pub message: Value,
    /// Set on a `textDocument/didSave` of a file sync carries: the hash of the saved bytes.
    pub saved_sha256: Option<String>,
}

#[derive(Debug, Default)]
pub struct Out {
    pub to_nvim: Vec<Value>,
    pub to_server: Vec<ToServer>,
    pub actions: Vec<Action>,
    /// Lines for the bridge's stderr, already escaped.
    pub log: Vec<String>,
}

/// Why a server could not be had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The helper looked for the command and did not find it.
    Missing(String),
    /// The helper refused to start it.
    Refused(String),
    /// The helper could not be asked.
    Unreachable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Before Neovim's `initialize`.
    Idle,
    /// Neovim's `initialize` is held until a server is attached.
    Opening,
    Live,
    /// The link is down; the same server is expected back.
    Detached,
    /// A fresh server is being opened for a client that was initialized before.
    Restarting,
    /// The fresh server has Neovim's `initialize` and its answer is awaited.
    Replaying {
        id: String,
    },
    /// No server could be had; waiting for Neovim to stop.
    Failed,
    /// Neovim's `shutdown` was answered here while no server could answer it, and the server stopped; waiting for its
    /// `exit`.
    Closed,
    Ended,
}

struct Initialize {
    id: Value,
    params: Value,
}

pub struct Bridge {
    config: Config,
    disk: Box<dyn Disk + Send>,
    rewriter: Rewriter,
    phase: Phase,
    initialize: Option<Initialize>,
    /// Whether Neovim has its `initialize` answer.
    init_answered: bool,
    initialized: bool,
    initialized_delivered: bool,
    capabilities: Value,
    /// The position encoding Neovim's client agreed at its `initialize`, which every edit it sends uses.
    encoding: Encoding,
    config_params: Option<Value>,
    config_dirty: bool,
    /// The documents Neovim has open.
    docs: BTreeMap<String, Document>,
    /// The documents the current server holds open, as the bridge sent them.
    server_open: BTreeSet<String>,
    /// Documents whose text the bridge does not know (a message about them was too large to pass on): not given to a
    /// server until Neovim opens them again or replaces their whole text.
    poisoned: BTreeSet<String>,
    /// File changes Neovim reported while the link was down, by uri: the newest type of change.
    watched: BTreeMap<String, Value>,
    watched_overflow: bool,
    /// The person was told the first open is waiting for the server.
    waiting_shown: bool,
    next_seq: u64,
    /// The server's registrations Neovim holds: id and method, in order.
    registrations: Vec<(String, String)>,
    /// Neovim's requests the server has not answered: id as JSON text, method.
    in_flight: HashMap<String, String>,
    /// The server's requests Neovim has not answered.
    server_requests: HashSet<String>,
    /// The bridge's own requests to Neovim (their answers are swallowed).
    own_pending: HashSet<String>,
    own_next: u64,
    /// Documents saved while the link was down.
    owed_saves: BTreeSet<String>,
    /// Documents whose `didSave` went out on the current attach: the helper does not say which arrived, and one sent
    /// just before a loss may not have, so they are saved again after a resync (a repeated save is harmless).
    saves_sent: BTreeSet<String>,
    /// The server saw Neovim's `initialize` on a link that is gone, and its answer with it.
    server_unusable: bool,
    last_exit: Option<Instant>,
}

fn error_reply(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

enum Kind<'a> {
    Request(&'a Value, &'a str),
    Notification(&'a str),
    Response(&'a Value),
    Invalid,
}

fn classify(message: &Value) -> Kind<'_> {
    let id = message.get("id").filter(|id| id.is_string() || id.is_number());
    match (message.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Kind::Request(id, method),
        (Some(method), None) if message.get("id").is_none() => Kind::Notification(method),
        (None, Some(id)) if message.get("result").is_some() || message.get("error").is_some() => Kind::Response(id),
        _ => Kind::Invalid,
    }
}

impl Bridge {
    pub fn new(config: Config, disk: Box<dyn Disk + Send>) -> Self {
        let rewriter = disk.rewriter(&config.host);
        Self {
            config,
            disk,
            rewriter,
            phase: Phase::Idle,
            initialize: None,
            init_answered: false,
            initialized: false,
            initialized_delivered: false,
            capabilities: Value::Null,
            encoding: Encoding::default(),
            config_params: None,
            config_dirty: false,
            docs: BTreeMap::new(),
            server_open: BTreeSet::new(),
            poisoned: BTreeSet::new(),
            watched: BTreeMap::new(),
            watched_overflow: false,
            waiting_shown: false,
            next_seq: 0,
            registrations: Vec::new(),
            in_flight: HashMap::new(),
            server_requests: HashSet::new(),
            own_pending: HashSet::new(),
            own_next: 0,
            owed_saves: BTreeSet::new(),
            saves_sent: BTreeSet::new(),
            server_unusable: false,
            last_exit: None,
        }
    }

    /// The command as shown in a line: its first word, escaped.
    fn command_name(&self) -> String {
        terminal_text(
            self.config
                .command
                .first()
                .map_or("the language server", String::as_str),
        )
    }

    // --- the two places every message passes ---

    fn emit_to_nvim(&mut self, mut message: Value, out: &mut Out) {
        if names_the_scheme(&message) {
            return self.refuse_to_nvim(&message, out);
        }
        self.rewriter.to_nvim(&mut message);
        out.to_nvim.push(message);
    }

    /// A message for Neovim that names an `eitri-remote://` file: the names are the bridge's to write (one from the
    /// server could name another host's file, which Neovim would read there and show to this server), so it is not
    /// passed on. An answer Neovim waits for becomes an error, a request of the server's is refused, a notification
    /// is dropped; the bridge's own requests carry the server's registrations, and are dropped too.
    fn refuse_to_nvim(&mut self, message: &Value, out: &mut Out) {
        const WHY: &str = "names an eitri-remote:// file, which only the bridge writes";
        match classify(message) {
            Kind::Response(id) => {
                out.log.push(format!("refused the language server's answer: it {WHY}"));
                let reply = error_reply(id, REQUEST_FAILED, &format!("the language server's answer {WHY}"));
                out.to_nvim.push(reply);
            }
            Kind::Request(id, method) => {
                out.log.push(format!(
                    "refused the language server's request {}: it {WHY}",
                    terminal_text(method)
                ));
                let key = id_key(id);
                if self.own_pending.remove(&key) {
                    return;
                }
                self.server_requests.remove(&key);
                let reply = error_reply(id, REQUEST_FAILED, &format!("the request {WHY}"));
                self.emit_to_server(reply, None, out);
            }
            Kind::Notification(method) => out.log.push(format!(
                "dropped the language server's {}: it {WHY}",
                terminal_text(method)
            )),
            Kind::Invalid => out
                .log
                .push(format!("dropped a message from the language server: it {WHY}")),
        }
    }

    /// A `didSave` with a hash is held on the server until sync brings its file, so each one also asks sync to carry
    /// the file now (once per [`Out`]).
    fn emit_to_server(&mut self, mut message: Value, saved_sha256: Option<String>, out: &mut Out) {
        self.rewriter.to_server(&mut message);
        if saved_sha256.is_some() && !out.actions.contains(&Action::FlushSync) {
            out.actions.push(Action::FlushSync);
        }
        out.to_server.push(ToServer { message, saved_sha256 });
    }

    // --- events ---

    /// A message from Neovim.
    pub fn from_nvim(&mut self, message: Value) -> Out {
        let mut out = Out::default();
        if self.rewriter.names_another_host(&message) {
            self.refuse_from_nvim(&message, &mut out);
            return out;
        }
        match classify(&message) {
            Kind::Request(id, method) => {
                let (id, method) = (id.clone(), method.to_owned());
                self.nvim_request(id, &method, message, &mut out);
            }
            Kind::Notification(method) => {
                let method = method.to_owned();
                self.nvim_notification(&method, message, &mut out);
            }
            Kind::Response(id) => {
                let key = id_key(id);
                if self.server_requests.remove(&key) {
                    if matches!(self.phase, Phase::Live | Phase::Replaying { .. }) {
                        self.emit_to_server(message, None, &mut out);
                    }
                } else {
                    // The bridge's own request (swallowed) or one the helper already answered (dropped).
                    self.own_pending.remove(&key);
                }
            }
            Kind::Invalid => out
                .log
                .push("dropped a message from the editor that is not JSON-RPC".into()),
        }
        out
    }

    /// A message from Neovim about another host's `eitri-remote://` buffer (a client of this server attached to it):
    /// that file and its text are not this server's. A request fails, a notification is dropped, an answer to a
    /// server request becomes an error answer.
    fn refuse_from_nvim(&mut self, message: &Value, out: &mut Out) {
        const WHY: &str = "names a file of another host";
        match classify(message) {
            Kind::Request(id, method) => {
                out.log
                    .push(format!("refused the editor's {}: it {WHY}", terminal_text(method)));
                let text = format!(
                    "the request {WHY}; the language server on {} does not serve it",
                    terminal_text(&self.config.host)
                );
                self.emit_to_nvim(error_reply(id, REQUEST_FAILED, &text), out);
            }
            Kind::Notification(method) => out
                .log
                .push(format!("dropped the editor's {}: it {WHY}", terminal_text(method))),
            Kind::Response(id) => {
                out.log.push(format!("refused an answer from the editor: it {WHY}"));
                let key = id_key(id);
                if self.server_requests.remove(&key) && matches!(self.phase, Phase::Live | Phase::Replaying { .. }) {
                    let reply = error_reply(id, REQUEST_FAILED, &format!("the editor's answer {WHY}"));
                    self.emit_to_server(reply, None, out);
                } else {
                    self.own_pending.remove(&key);
                }
            }
            Kind::Invalid => out.log.push(format!("dropped a message from the editor: it {WHY}")),
        }
    }

    /// A message from Neovim whose body was past the size cap and was skipped: a request fails, a notification is
    /// dropped, an answer to a server request becomes an error answer.
    pub fn from_nvim_oversized(&mut self, probe: Probe) -> Out {
        let mut out = Out::default();
        let id = probe
            .id
            .as_deref()
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        let id = id.filter(|id| id.is_string() || id.is_number());
        let size = format!("{} bytes", probe.length);
        match (probe.method.as_deref(), id) {
            (Some(_), Some(id)) => {
                let text = format!(
                    "the message is {size}, over the limit of the bridge to {}",
                    terminal_text(&self.config.host)
                );
                self.emit_to_nvim(error_reply(&id, REQUEST_FAILED, &text), &mut out);
            }
            (Some(method), None) => {
                out.log.push(format!(
                    "dropped {} from the editor: {size}, over the limit",
                    terminal_text(method)
                ));
                if matches!(method, "textDocument/didOpen" | "textDocument/didChange") {
                    if let Some(uri) = probe.uri.as_deref() {
                        self.poison(uri, true, &mut out);
                    }
                }
            }
            (None, Some(id)) => {
                let key = id_key(&id);
                if self.server_requests.remove(&key) && matches!(self.phase, Phase::Live | Phase::Replaying { .. }) {
                    let reply = error_reply(&id, REQUEST_FAILED, "the editor's answer was too large to pass on");
                    self.emit_to_server(reply, None, &mut out);
                }
                out.log
                    .push(format!("dropped an answer from the editor: {size}, over the limit"));
            }
            (None, None) => out
                .log
                .push(format!("dropped a message from the editor: {size}, over the limit")),
        }
        out
    }

    /// A message from the server.
    pub fn from_server(&mut self, message: Value) -> Out {
        let mut out = Out::default();
        if !matches!(self.phase, Phase::Live | Phase::Replaying { .. }) {
            return out;
        }
        match classify(&message) {
            Kind::Response(id) => {
                let key = id_key(id);
                if matches!(&self.phase, Phase::Replaying { id: replay } if json!(replay) == *id) {
                    self.finish_replay(message, &mut out);
                } else if let Some(method) = self.in_flight.remove(&key) {
                    if method == "initialize" {
                        self.initialize_answered(&message);
                    }
                    self.emit_to_nvim(message, &mut out);
                }
                // Anything else answers a request Neovim was already told failed.
            }
            Kind::Request(id, method) => {
                if id.as_str().is_some_and(|id| id.starts_with("eitri-")) {
                    let reply = error_reply(
                        id,
                        INVALID_REQUEST,
                        "the id is one the bridge keeps for its own requests",
                    );
                    self.emit_to_server(reply, None, &mut out);
                    return out;
                }
                self.server_requests.insert(id_key(id));
                let method = method.to_owned();
                // A request refused on its way to Neovim registers nothing there.
                if !names_the_scheme(&message) {
                    self.note_registration(&method, &message);
                }
                self.emit_to_nvim(message, &mut out);
            }
            Kind::Notification(_) => self.emit_to_nvim(message, &mut out),
            Kind::Invalid => out
                .log
                .push("dropped a message from the language server that is not JSON-RPC".into()),
        }
        out
    }

    /// A message from the server that is not JSON (or nests past what the parser takes), with what a scan of its
    /// bytes found: the request of Neovim's it answers fails, a request of the server's is refused, so neither side
    /// waits for an answer that never comes.
    pub fn from_server_unreadable(&mut self, probe: Probe, why: &str) -> Out {
        let mut out = Out::default();
        if !matches!(self.phase, Phase::Live | Phase::Replaying { .. }) {
            return out;
        }
        out.log.push(format!(
            "dropped a message from the language server that is not JSON: {}",
            terminal_text(why)
        ));
        let id = probe
            .id
            .as_deref()
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        let Some(id) = id.filter(|id| id.is_string() || id.is_number()) else {
            return out;
        };
        if probe.method.is_some() {
            let reply = error_reply(&id, REQUEST_FAILED, "the bridge could not read the request");
            self.emit_to_server(reply, None, &mut out);
        } else if matches!(&self.phase, Phase::Replaying { id: replay } if json!(replay) == id) {
            let text = format!(
                "the language server {} on {} answered its start with a message that cannot be read",
                self.command_name(),
                terminal_text(&self.config.host)
            );
            self.fatal(text, &mut out);
        } else if let Some(method) = self.in_flight.remove(&id_key(&id)) {
            if method == "initialize" {
                self.init_answered = true;
            }
            let reply = error_reply(&id, REQUEST_FAILED, "the language server's answer could not be read");
            self.emit_to_nvim(reply, &mut out);
        }
        out
    }

    /// A message the session could not send to the server (past the size cap once written out): a document it was
    /// about is no longer known to the server.
    pub fn unsent(&mut self, message: &Value) -> Out {
        let mut out = Out::default();
        out.log
            .push("dropped a message to the language server over the size limit".into());
        let method = message.get("method").and_then(Value::as_str);
        if let (Some(method @ ("textDocument/didOpen" | "textDocument/didChange")), Some(uri)) =
            (method, text_document_uri(message))
        {
            // A `didOpen` that never went leaves nothing open to close.
            let opened = method == "textDocument/didOpen";
            let uri = uri.to_owned();
            if opened {
                self.server_open.remove(&uri);
            }
            self.poison(&uri, !opened, &mut out);
        }
        out
    }

    /// The link is down. Requests Neovim is waiting on fail now; the same server is expected back.
    pub fn link_lost(&mut self) -> Out {
        let mut out = Out::default();
        match self.phase {
            Phase::Live | Phase::Replaying { .. } => {
                self.server_unusable |= !self.init_answered || matches!(self.phase, Phase::Replaying { .. });
                let shut_down = self.fail_in_flight(&mut out);
                self.server_requests.clear();
                self.own_pending.clear();
                self.owed_saves.append(&mut self.saves_sent);
                self.phase = Phase::Detached;
                if shut_down {
                    self.close(&mut out);
                }
            }
            _ => {}
        }
        out
    }

    /// Something the link wants the person to know while it cannot reach the server. Logged; while Neovim's first
    /// `initialize` waits for a server, shown once as well, so the person sees why nothing happens.
    pub fn link_note(&mut self, text: &str) -> Out {
        let mut out = Out::default();
        out.log.push(text.to_owned());
        if self.phase == Phase::Opening && !self.init_answered && !self.waiting_shown {
            self.waiting_shown = true;
            let message = format!(
                "the language server {} is waiting for the Eitri server: {}; still trying",
                self.command_name(),
                terminal_text(text)
            );
            self.emit_to_nvim(
                notification("window/showMessage", json!({"type": 2, "message": message})),
                &mut out,
            );
        }
        out
    }

    /// The bridge was told to stop (SIGTERM, SIGHUP): the server is stopped and the bridge ends with `128 + signal`.
    pub fn signalled(&mut self, signal: i32) -> Out {
        let mut out = Out::default();
        if self.phase == Phase::Ended {
            return out;
        }
        out.log.push(format!("stopped by signal {signal}"));
        if !matches!(self.phase, Phase::Idle | Phase::Failed | Phase::Closed) {
            out.actions.push(Action::StopServer);
        }
        out.actions.push(Action::Exit(128 + signal));
        self.phase = Phase::Ended;
        out
    }

    /// The server is attached (its `LspAttached` is read): first for a server just opened, again after a loss.
    pub fn link_back(&mut self, attached: &LspAttached) -> Out {
        let mut out = Out::default();
        if let Err(why) = check_attached(attached) {
            let text = format!(
                "the Eitri server on {} sent an answer that is over its bounds: {why}",
                terminal_text(&self.config.host)
            );
            self.fatal(text, &mut out);
            return out;
        }
        match self.phase.clone() {
            Phase::Opening => {
                let Some((id, params)) = self
                    .initialize
                    .as_ref()
                    .map(|init| (init.id.clone(), init.params.clone()))
                else {
                    return out;
                };
                self.in_flight.insert(id_key(&id), "initialize".into());
                self.emit_to_server(
                    json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params}),
                    None,
                    &mut out,
                );
                self.phase = Phase::Live;
            }
            Phase::Restarting => {
                let Some(mut params) = self.initialize.as_ref().map(|init| init.params.clone()) else {
                    return out;
                };
                // Neovim's client keeps the encoding it agreed with the first server: the fresh one is offered only
                // that one.
                if let Some(offered) = params.pointer_mut("/capabilities/general/positionEncodings") {
                    *offered = json!([self.encoding.name()]);
                }
                let id = self.own_id();
                let message = json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params});
                self.emit_to_server(message, None, &mut out);
                self.phase = Phase::Replaying { id };
            }
            Phase::Detached if self.server_unusable => {
                self.server_unusable = false;
                self.begin_fresh(&mut out);
                out.actions.insert(0, Action::StopServer);
            }
            Phase::Detached => self.resync(attached, &mut out),
            _ => {}
        }
        out
    }

    /// The server's end, from its `LspEnded` (or an attach that found it gone).
    pub fn server_ended(&mut self, ended: &LspEnded, now: Instant) -> Out {
        let mut out = Out::default();
        if matches!(self.phase, Phase::Idle | Phase::Failed | Phase::Closed | Phase::Ended) {
            return out;
        }
        let reason = LspEndReason::try_from(ended.reason).unwrap_or(LspEndReason::Unspecified);
        match reason {
            LspEndReason::Replaced => {
                let text = format!(
                    "another connection took over the language server {} on {}",
                    self.command_name(),
                    terminal_text(&self.config.host)
                );
                self.finish(text, false, false, &mut out);
            }
            // Someone stopped it on purpose (the bridge's own stops are not reported here): not undone.
            LspEndReason::Stopped => {
                let text = format!(
                    "the language server {} on {} was stopped",
                    self.command_name(),
                    terminal_text(&self.config.host)
                );
                self.finish(text, false, false, &mut out);
            }
            LspEndReason::Exited | LspEndReason::Unspecified => {
                if self
                    .last_exit
                    .is_some_and(|before| now.saturating_duration_since(before) < RESTART_WINDOW)
                {
                    let tail = stderr_tail(&ended.stderr_tail);
                    let text = format!(
                        "the language server {} on {} stopped twice: {}",
                        self.command_name(),
                        terminal_text(&self.config.host),
                        if tail.is_empty() {
                            "(it wrote nothing to stderr)".into()
                        } else {
                            tail
                        }
                    );
                    self.fatal(text, &mut out);
                } else {
                    self.last_exit = Some(now);
                    self.restart(&mut out);
                }
            }
            LspEndReason::Grace | LspEndReason::QueueFull | LspEndReason::Daemon | LspEndReason::Gone => {
                self.restart(&mut out)
            }
        }
        out
    }

    /// No server could be had at an open.
    pub fn open_failed(&mut self, failure: Failure) -> Out {
        let mut out = Out::default();
        let (host, name) = (terminal_text(&self.config.host), self.command_name());
        let text = match failure {
            Failure::Missing(missing) => {
                let missing = if missing.is_empty() {
                    name
                } else {
                    terminal_text(&missing)
                };
                format!("{missing} is not installed on {host} (looked on the login PATH and in mason)")
            }
            Failure::Refused(why) => terminal_text(&why),
            Failure::Unreachable(why) => format!(
                "the language server {name} cannot be started on {host}: {}",
                terminal_text(&why)
            ),
        };
        match self.phase {
            Phase::Opening if !self.init_answered => {
                if let Some(id) = self.initialize.as_ref().map(|init| init.id.clone()) {
                    self.emit_to_nvim(error_reply(&id, REQUEST_FAILED, &text), &mut out);
                }
                self.init_answered = true;
                out.log.push(text);
                self.phase = Phase::Failed;
                // Neovim's `shutdown` and `exit` end the bridge as usual; a client that never sends them (nvim 0.12
                // asserts on the failed `initialize` and leaves the client as it is) does not keep it.
                out.actions.push(Action::ExitWhenQuiet(1));
            }
            Phase::Idle | Phase::Failed | Phase::Closed | Phase::Ended => {}
            _ => self.fatal(text, &mut out),
        }
        out
    }

    /// Neovim closed its end.
    pub fn nvim_closed(&mut self) -> Out {
        let mut out = Out::default();
        self.stop(&mut out);
        out
    }

    // --- Neovim's messages ---

    fn nvim_request(&mut self, id: Value, method: &str, message: Value, out: &mut Out) {
        match self.phase {
            Phase::Idle if method == "initialize" => {
                let mut params = message.get("params").cloned().unwrap_or(Value::Null);
                // Neovim's pid means nothing on the server, and servers that poll it would stop.
                if let Some(params) = params.as_object_mut() {
                    params.insert("processId".into(), Value::Null);
                }
                self.initialize = Some(Initialize { id, params });
                self.phase = Phase::Opening;
                out.actions.push(Action::OpenServer);
            }
            Phase::Idle => {
                let reply = error_reply(&id, SERVER_NOT_INITIALIZED, "the server is not initialized");
                self.emit_to_nvim(reply, out);
            }
            Phase::Live => {
                self.in_flight.insert(id_key(&id), method.to_owned());
                self.emit_to_server(message, None, out);
            }
            Phase::Failed | Phase::Closed => {
                let reply = match method {
                    "shutdown" => json!({"jsonrpc": "2.0", "id": id, "result": null}),
                    _ if self.phase == Phase::Closed => error_reply(&id, INVALID_REQUEST, "the client was shut down"),
                    _ => error_reply(&id, REQUEST_FAILED, "the language server could not be started"),
                };
                self.emit_to_nvim(reply, out);
            }
            // No server can answer `shutdown` now: it is answered here and the server stopped, so the editor's
            // client ends cleanly instead of killing the bridge and leaving the server to its grace.
            Phase::Opening | Phase::Detached | Phase::Restarting | Phase::Replaying { .. } if method == "shutdown" => {
                self.emit_to_nvim(json!({"jsonrpc": "2.0", "id": id, "result": null}), out);
                self.fail_in_flight(out);
                self.close(out);
            }
            Phase::Opening | Phase::Detached | Phase::Restarting | Phase::Replaying { .. } => {
                let reply = error_reply(
                    &id,
                    CONTENT_MODIFIED,
                    "the language server connection is being restored",
                );
                self.emit_to_nvim(reply, out);
            }
            Phase::Ended => {}
        }
    }

    fn nvim_notification(&mut self, method: &str, message: Value, out: &mut Out) {
        let live = self.phase == Phase::Live;
        match method {
            "initialized" => {
                self.initialized = true;
                if live {
                    self.initialized_delivered = true;
                    self.emit_to_server(message, None, out);
                }
            }
            "exit" => {
                if live {
                    self.emit_to_server(message, None, out);
                }
                self.stop(out);
            }
            "textDocument/didOpen" => {
                let Some(uri) = self.track_open(&message) else {
                    return self.forward(message, out);
                };
                if live {
                    self.server_open.insert(uri);
                    self.emit_to_server(message, None, out);
                }
            }
            "textDocument/didChange" => self.nvim_change(message, out),
            "textDocument/didClose" => {
                let Some(uri) = text_document_uri(&message).map(str::to_owned) else {
                    return self.forward(message, out);
                };
                self.docs.remove(&uri);
                self.owed_saves.remove(&uri);
                self.poisoned.remove(&uri);
                // Only what the server holds is closed there; the rest is settled at the next resync.
                if live && self.server_open.remove(&uri) {
                    self.emit_to_server(message, None, out);
                }
            }
            WATCHED_FILES if self.phase == Phase::Detached => self.keep_watched(&message, out),
            "textDocument/didSave" => {
                // A document whose text the server does not have (poisoned, or one Neovim has that the live server
                // does not hold) is not saved there either. While the link is down a document opened meanwhile is
                // still owed its save: the resync opens it first.
                let unheld = text_document_uri(&message).is_some_and(|uri| {
                    self.poisoned.contains(uri)
                        || (live && self.docs.contains_key(uri) && !self.server_open.contains(uri))
                });
                if unheld {
                    return;
                }
                if live {
                    let hash = text_document_uri(&message).and_then(|uri| self.saved_hash(uri));
                    if let Some(uri) = text_document_uri(&message) {
                        self.saves_sent.insert(uri.to_owned());
                    }
                    self.emit_to_server(message, hash, out);
                } else if let Some(uri) = text_document_uri(&message) {
                    self.owed_saves.insert(uri.to_owned());
                }
            }
            "workspace/didChangeConfiguration" => {
                self.config_params = message.get("params").cloned();
                self.config_dirty |= !live;
                self.forward(message, out);
            }
            _ => self.forward(message, out),
        }
    }

    /// Sent while attached, applied to what is kept either way.
    fn forward(&mut self, message: Value, out: &mut Out) {
        if self.phase == Phase::Live {
            self.emit_to_server(message, None, out);
        }
    }

    /// A `didChange`: applied to the kept text and passed on, unless the document's text is not known here. Then a
    /// change that replaces the whole text makes it known again, and the server is given the document anew.
    fn nvim_change(&mut self, message: Value, out: &mut Out) {
        let Some(uri) = text_document_uri(&message).map(str::to_owned) else {
            return self.forward(message, out);
        };
        let healed = self.poisoned.contains(&uri) && whole_text_change(&message) && self.docs.contains_key(&uri);
        self.track_change(&message);
        if !self.poisoned.contains(&uri) {
            return self.forward(message, out);
        }
        if !healed {
            return;
        }
        self.poisoned.remove(&uri);
        out.log.push(format!(
            "the text of {} is known again and goes to the language server",
            terminal_text(&uri)
        ));
        if self.phase == Phase::Live {
            if let Some(doc) = self.docs.get(&uri).cloned() {
                self.server_open.insert(uri);
                self.emit_to_server(open_notification(&doc), None, out);
            }
        }
    }

    /// A document whose text the bridge cannot know: closed on the server when it holds it (`close`), not given to a
    /// server again until Neovim opens it again or replaces its whole text.
    fn poison(&mut self, uri: &str, close: bool, out: &mut Out) {
        if self.poisoned.insert(uri.to_owned()) {
            out.log.push(format!(
                "the text of {} is too large to pass on; the language server does not see it until it is opened \
                 again or replaced whole",
                terminal_text(uri)
            ));
        }
        self.owed_saves.remove(uri);
        // While the link is down the server keeps the document: it stays in `server_open`, so the resync closes it.
        if close && self.phase == Phase::Live && self.server_open.remove(uri) {
            let message = notification("textDocument/didClose", json!({"textDocument": {"uri": uri}}));
            self.emit_to_server(message, None, out);
        }
    }

    /// A file change Neovim reports while the link is down, kept for the same server: one entry per file, the newest
    /// type of change winning.
    fn keep_watched(&mut self, message: &Value, out: &mut Out) {
        let changes = message.pointer("/params/changes").and_then(Value::as_array);
        for change in changes.into_iter().flatten() {
            let Some(uri) = change.get("uri").and_then(Value::as_str) else {
                continue;
            };
            if self.watched.len() >= WATCHED_CHANGES_MAX && !self.watched.contains_key(uri) {
                if !self.watched_overflow {
                    self.watched_overflow = true;
                    out.log.push(format!(
                        "more than {WATCHED_CHANGES_MAX} files changed while the link was down; the language server \
                         is not told of the rest"
                    ));
                }
                continue;
            }
            self.watched.insert(uri.to_owned(), change.clone());
        }
    }

    /// The document's uri when it is one the bridge keeps.
    fn track_open(&mut self, message: &Value) -> Option<String> {
        let item = message.pointer("/params/textDocument")?;
        let uri = item.get("uri").and_then(Value::as_str)?;
        let text = item.get("text").and_then(Value::as_str)?;
        self.poisoned.remove(uri);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.docs.insert(
            uri.to_owned(),
            Document {
                uri: uri.to_owned(),
                language_id: item.get("languageId").and_then(Value::as_str).unwrap_or("").to_owned(),
                version: item.get("version").and_then(Value::as_i64).unwrap_or(0),
                text: text.to_owned(),
                seq,
            },
        );
        Some(uri.to_owned())
    }

    fn track_change(&mut self, message: &Value) {
        let Some(uri) = text_document_uri(message) else { return };
        let Some(doc) = self.docs.get_mut(uri) else { return };
        if let Some(version) = message.pointer("/params/textDocument/version").and_then(Value::as_i64) {
            doc.version = version;
        }
        let changes = message.pointer("/params/contentChanges").and_then(Value::as_array);
        for change in changes.into_iter().flatten() {
            doc.apply_change(change, self.encoding);
        }
    }

    /// The hash of the saved file, when sync carries it and it can be read.
    fn saved_hash(&self, uri: &str) -> Option<String> {
        let path = file_uri_path(uri)?;
        if !self.disk.carried(&path) {
            return None;
        }
        self.disk.sha256(&path).ok()
    }

    // --- the server's messages ---

    fn initialize_answered(&mut self, response: &Value) {
        self.init_answered = true;
        self.take_capabilities(response);
        self.encoding = self.server_encoding();
    }

    fn take_capabilities(&mut self, response: &Value) {
        self.capabilities = response.pointer("/result/capabilities").cloned().unwrap_or(Value::Null);
    }

    /// The position encoding the server's capabilities chose.
    fn server_encoding(&self) -> Encoding {
        Encoding::from_capability(self.capabilities.get("positionEncoding").and_then(Value::as_str))
    }

    /// A registration the server made goes through to Neovim: kept so it can be withdrawn.
    fn note_registration(&mut self, method: &str, message: &Value) {
        match method {
            "client/registerCapability" => {
                for (id, method, _) in entries(message, "registrations") {
                    self.registrations.push((id, method));
                }
            }
            "client/unregisterCapability" => {
                for (id, _, _) in entries(message, "unregisterations") {
                    self.registrations.retain(|(held, _)| *held != id);
                }
            }
            _ => {}
        }
    }

    fn finish_replay(&mut self, response: Value, out: &mut Out) {
        if response.get("error").is_some() {
            let why = response.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
            let text = format!(
                "the language server {} on {} refused to start again: {}",
                self.command_name(),
                terminal_text(&self.config.host),
                terminal_text(why)
            );
            self.fatal(text, out);
            return;
        }
        let before = std::mem::take(&mut self.capabilities);
        self.take_capabilities(&response);
        let chosen = self.server_encoding();
        if chosen != self.encoding {
            // Neovim's client counts every position in the encoding it agreed; a server counting another way would
            // put every edit in the wrong place.
            let text = format!(
                "the language server {} on {} started again counting positions in {}, not the {} the editor's client \
                 uses; restart the client",
                self.command_name(),
                terminal_text(&self.config.host),
                chosen.name(),
                self.encoding.name()
            );
            self.fatal(text, out);
            return;
        }
        if before != self.capabilities {
            out.log.push(format!(
                "the language server {} started again with other capabilities than before; restart the editor's \
                 client to use them",
                self.command_name()
            ));
        }
        self.phase = Phase::Live;
        self.send_state(out);
        self.send_owed_saves(&[], out);
        self.owed_saves.clear();
        self.config_dirty = false;
    }

    // --- bringing a server back ---

    /// The same server again: what it may have missed, in order.
    fn resync(&mut self, attached: &LspAttached, out: &mut Out) {
        self.phase = Phase::Live;
        self.replay_registrations(&attached.registrations, out);
        if self.initialized && !self.initialized_delivered {
            self.initialized_delivered = true;
            self.emit_to_server(notification("initialized", json!({})), None, out);
        }
        if self.config_dirty {
            if let Some(params) = self.config_params.clone() {
                self.emit_to_server(notification("workspace/didChangeConfiguration", params), None, out);
            }
        }
        self.config_dirty = false;
        self.reopen_documents(out);
        self.send_owed_saves(&attached.dropped_saves, out);
        self.owed_saves.clear();
        self.send_watched(out);
    }

    /// A fresh server for the client Neovim already has: the original `initialize` goes first (`link_back`), the rest
    /// when it answers.
    fn send_state(&mut self, out: &mut Out) {
        if self.initialized {
            self.initialized_delivered = true;
            self.emit_to_server(notification("initialized", json!({})), None, out);
        }
        if let Some(params) = self.config_params.clone() {
            self.emit_to_server(notification("workspace/didChangeConfiguration", params), None, out);
        }
        self.reopen_documents(out);
    }

    /// Brings the server's open documents to Neovim's: what the server holds and Neovim closed is closed there, and
    /// every document Neovim has (with a text the bridge knows) is opened with its current text, after a `didClose`
    /// when the server holds it already. A document is opened whether or not its file exists on the laptop: a buffer
    /// not yet written, or one whose file moved, is still Neovim's, and its later changes need the server to have it.
    fn reopen_documents(&mut self, out: &mut Out) {
        let mut docs: Vec<Document> = self
            .docs
            .values()
            .filter(|doc| !self.poisoned.contains(&doc.uri))
            .cloned()
            .collect();
        docs.sort_by_key(|doc| doc.seq);
        let departed: Vec<String> = self
            .server_open
            .iter()
            .filter(|uri| !docs.iter().any(|doc| doc.uri == **uri))
            .cloned()
            .collect();
        for uri in departed {
            self.server_open.remove(&uri);
            let close = notification("textDocument/didClose", json!({"textDocument": {"uri": uri}}));
            self.emit_to_server(close, None, out);
        }
        for doc in docs {
            if self.server_open.contains(&doc.uri) {
                let close = notification("textDocument/didClose", json!({"textDocument": {"uri": doc.uri}}));
                self.emit_to_server(close, None, out);
            }
            self.server_open.insert(doc.uri.clone());
            self.emit_to_server(open_notification(&doc), None, out);
        }
    }

    /// The file changes Neovim reported while the link was down, in one notification, when the server watches files
    /// through Neovim.
    fn send_watched(&mut self, out: &mut Out) {
        let changes: Vec<Value> = std::mem::take(&mut self.watched).into_values().collect();
        self.watched_overflow = false;
        let watching = self.registrations.iter().any(|(_, method)| method == WATCHED_FILES);
        if watching && !changes.is_empty() {
            self.emit_to_server(notification(WATCHED_FILES, json!({ "changes": changes })), None, out);
        }
    }

    /// A `didSave` for each document saved while the link was down and each the helper dropped, of those still open.
    fn send_owed_saves(&mut self, dropped: &[String], out: &mut Out) {
        let include_text = self.capabilities.pointer("/textDocumentSync/save/includeText") == Some(&Value::Bool(true));
        let uris: BTreeSet<String> = self.owed_saves.iter().chain(dropped).cloned().collect();
        for uri in uris {
            let Some(doc) = self.docs.get(&uri).filter(|_| !self.poisoned.contains(&uri)) else {
                continue;
            };
            let mut params = json!({"textDocument": {"uri": uri}});
            if include_text {
                params["text"] = json!(doc.text);
            }
            let hash = self.saved_hash(&uri);
            self.saves_sent.insert(uri.clone());
            self.emit_to_server(notification("textDocument/didSave", params), hash, out);
        }
    }

    /// What the helper accepted from the server while the link was down, to Neovim as requests of the bridge's own.
    /// A registration whose id Neovim holds is not sent again (a second one leaks a file watcher there), and an
    /// unregistration of one it does not hold is not sent.
    fn replay_registrations(&mut self, registrations: &[LspRegistration], out: &mut Out) {
        for registration in registrations {
            let Ok(params) = serde_json::from_slice::<Value>(&registration.params) else {
                out.log
                    .push("dropped a registration of the language server that is not JSON".into());
                continue;
            };
            let register = registration.method == "client/registerCapability";
            let key = if register { "registrations" } else { "unregisterations" };
            let mut send = Vec::new();
            for (id, method, entry) in entries(&json!({"params": params}), key) {
                let held = self.registrations.iter().any(|(held, _)| *held == id);
                if register && !held {
                    self.registrations.push((id, method));
                    send.push(entry);
                } else if !register && held {
                    self.registrations.retain(|(held, _)| *held != id);
                    send.push(entry);
                }
            }
            if !send.is_empty() {
                let method = if register {
                    "client/registerCapability"
                } else {
                    "client/unregisterCapability"
                };
                self.own_request(method, json!({ key: send }), out);
            }
        }
    }

    /// Takes the registrations back from Neovim, all of them, before a new server can make its own.
    fn withdraw_registrations(&mut self, out: &mut Out) {
        if self.registrations.is_empty() {
            return;
        }
        let all: Vec<Value> = std::mem::take(&mut self.registrations)
            .into_iter()
            .map(|(id, method)| json!({"id": id, "method": method}))
            .collect();
        self.own_request("client/unregisterCapability", json!({"unregisterations": all}), out);
    }

    fn own_id(&mut self) -> String {
        self.own_next += 1;
        format!("eitri-{}", self.own_next)
    }

    fn own_request(&mut self, method: &str, params: Value, out: &mut Out) {
        let id = self.own_id();
        self.own_pending.insert(id_key(&json!(id)));
        self.emit_to_nvim(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            out,
        );
    }

    /// Every request of Neovim's the server will not answer is answered now (the initialize still owed is kept for
    /// the server that comes next). A `shutdown` among them is answered as done: whether the bridge then stops the
    /// server instead of bringing it back is what this returns.
    fn fail_in_flight(&mut self, out: &mut Out) -> bool {
        let mut waiting: Vec<(String, String)> = self.in_flight.drain().collect();
        waiting.sort();
        let mut shut_down = false;
        for (key, method) in waiting {
            if method == "initialize" && !self.init_answered {
                continue;
            }
            let Ok(id) = serde_json::from_str::<Value>(&key) else {
                continue;
            };
            let reply = if method == "shutdown" {
                shut_down = true;
                json!({"jsonrpc": "2.0", "id": id, "result": null})
            } else {
                error_reply(&id, CONTENT_MODIFIED, "the language server connection was lost")
            };
            self.emit_to_nvim(reply, out);
        }
        shut_down
    }

    /// Neovim's client is shut down while no server answers it: the server is stopped and the bridge waits for `exit`.
    /// An `initialize` still waiting is answered with why.
    fn close(&mut self, out: &mut Out) {
        if let Some(id) = self
            .initialize
            .as_ref()
            .map(|init| init.id.clone())
            .filter(|_| !self.init_answered)
        {
            self.init_answered = true;
            self.emit_to_nvim(error_reply(&id, REQUEST_FAILED, "the client was shut down"), out);
        }
        self.server_open.clear();
        out.actions.push(Action::StopServer);
        self.phase = Phase::Closed;
    }

    /// The server is gone: a fresh one is opened for the client Neovim has.
    fn restart(&mut self, out: &mut Out) {
        self.server_unusable = false;
        self.begin_fresh(out);
    }

    fn begin_fresh(&mut self, out: &mut Out) {
        let shut_down = self.fail_in_flight(out);
        self.saves_sent.clear();
        self.server_requests.clear();
        self.own_pending.clear();
        self.withdraw_registrations(out);
        // A fresh server holds nothing and reads the files as they are.
        self.server_open.clear();
        self.watched.clear();
        self.watched_overflow = false;
        if shut_down {
            return self.close(out);
        }
        self.phase = if self.init_answered {
            Phase::Restarting
        } else {
            Phase::Opening
        };
        out.actions.push(Action::OpenServer);
    }

    /// The bridge ends with `text` shown to the person and logged; Neovim's `initialize` is answered with it when it
    /// is still waiting.
    fn fatal(&mut self, text: String, out: &mut Out) {
        if let Some(id) = self
            .initialize
            .as_ref()
            .map(|init| init.id.clone())
            .filter(|_| !self.init_answered)
        {
            self.emit_to_nvim(error_reply(&id, REQUEST_FAILED, &text), out);
            self.init_answered = true;
        }
        self.finish(text, true, true, out);
    }

    /// The bridge ends with `text` logged, shown to the person when `show`, and its server stopped when `stop_server`
    /// (not one another connection took over).
    fn finish(&mut self, text: String, show: bool, stop_server: bool, out: &mut Out) {
        if show {
            self.emit_to_nvim(
                notification("window/showMessage", json!({"type": 1, "message": text})),
                out,
            );
        }
        out.log.push(text);
        if stop_server {
            out.actions.push(Action::StopServer);
        }
        out.actions.push(Action::Exit(1));
        self.phase = Phase::Ended;
    }

    /// Neovim's `exit` (already passed on) or its end: the server is stopped and the bridge ends.
    fn stop(&mut self, out: &mut Out) {
        let code = match self.phase {
            Phase::Failed => 1,
            Phase::Ended => return,
            Phase::Idle | Phase::Closed => 0,
            _ => {
                out.actions.push(Action::StopServer);
                0
            }
        };
        out.actions.push(Action::Exit(code));
        self.phase = Phase::Ended;
    }
}

fn text_document_uri(message: &Value) -> Option<&str> {
    message.pointer("/params/textDocument/uri").and_then(Value::as_str)
}

/// Whether a `didChange` replaces the whole text in one of its entries (after which the text is known whatever came
/// before).
fn whole_text_change(message: &Value) -> bool {
    let changes = message.pointer("/params/contentChanges").and_then(Value::as_array);
    changes.into_iter().flatten().any(|change| {
        change.get("text").is_some_and(Value::is_string) && change.get("range").is_none_or(Value::is_null)
    })
}

fn open_notification(doc: &Document) -> Value {
    let open = json!({"textDocument": {
        "uri": doc.uri, "languageId": doc.language_id, "version": doc.version, "text": doc.text,
    }});
    notification("textDocument/didOpen", open)
}

/// The `(id, method, whole entry)` of each element of `params.<key>`.
fn entries(message: &Value, key: &str) -> Vec<(String, String, Value)> {
    let list = message.pointer(&format!("/params/{key}")).and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?;
            let method = entry.get("method").and_then(Value::as_str)?;
            Some((id.to_owned(), method.to_owned(), entry.clone()))
        })
        .collect()
}

/// The server's stderr tail, the last [`LSP_STDERR_TAIL`] bytes of it, with hidden characters escaped.
fn stderr_tail(bytes: &[u8]) -> String {
    let bytes = &bytes[bytes.len().saturating_sub(LSP_STDERR_TAIL)..];
    terminal_text(String::from_utf8_lossy(bytes).trim())
}

/// What the helper told a returning bridge, held to the contract's bounds before any of it is used.
fn check_attached(attached: &LspAttached) -> Result<(), String> {
    REGISTRATIONS.check(&attached.registrations)?;
    if attached
        .registrations
        .iter()
        .any(|registration| registration.method.len() > REGISTRATION_METHOD_MAX)
    {
        return Err("a registration names a method that is not one".into());
    }
    let bytes: usize = attached
        .dropped_saves
        .iter()
        .map(|uri| uri.len() + ITEM_OVERHEAD_BYTES)
        .sum();
    DROPPED_SAVES.admits(attached.dropped_saves.len(), bytes)?;
    if attached.dropped_saves.iter().any(|uri| uri.len() > LSP_URI_MAX) {
        return Err("a dropped save names a URI longer than the contract allows".into());
    }
    Ok(())
}
