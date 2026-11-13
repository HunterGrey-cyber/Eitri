//! A small, ordered Messages API server with observable requests and driver-controlled holds.
pub mod driver;
pub mod gate_answer;
pub mod host_view;
mod http;
pub mod matchers;
pub mod proto_record;
pub mod questions;
pub mod replay;
pub mod sandbox;
pub mod script;
pub mod scrub;
pub mod side;
pub mod sse;
pub mod tripwire;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::{self, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Point {
    MessageStart,
    AfterBlock(usize),
    BeforeStop,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hold {
    pub name: String,
    pub at: Point,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpError {
    pub status: u16,
    #[serde(default = "one")]
    pub times: usize,
    #[serde(default)]
    pub retry_after: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn one() -> usize {
    1
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub id: String,
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub usage: Value,
    pub chunks: usize,
    pub pace_ms: u64,
    pub cut: Option<Point>,
    pub hold: Option<Hold>,
    pub error: Option<HttpError>,
    /// Headers on the 200 answer (rate-limit warnings ride a reply that succeeds).
    pub ok_headers: BTreeMap<String, String>,
}

impl Reply {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            content: vec![json!({"type":"text","text":text.into()})],
            stop_reason: "end_turn".into(),
            usage: json!({"input_tokens":1000,"cache_creation_input_tokens":0,
                "cache_read_input_tokens":0,"output_tokens":10}),
            chunks: 3,
            pace_ms: 0,
            cut: None,
            hold: None,
            error: None,
            ok_headers: BTreeMap::new(),
        }
    }
}

/// The log directory must be dedicated to this server; existing endpoint logs are refused.
///
/// A request is answered by a side row, else by the first step that takes it, else by the next ordered reply.
#[derive(Clone, Debug, Default)]
pub struct Script {
    pub key: String,
    /// Replies served in order to requests no step takes.
    pub replies: Vec<Reply>,
    pub log_dir: PathBuf,
    pub steps: Vec<script::Step>,
    /// Branch sets: set name to its alternative groups.
    pub branches: BTreeMap<String, Vec<String>>,
    /// Census mode: `main` matches any model, unlisted betas and fields are accepted and recorded, and a side
    /// request no row knows is answered and recorded.
    pub census: bool,
    pub mode: Option<script::Mode>,
    pub classifier: Option<script::Classifier>,
    pub main_model: Option<String>,
    pub small_model: Option<String>,
    /// What `{RUN}` and `{GATE name}` expand against; unset, they expand to the shell variable `$RUN`.
    pub run_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub arrival_index: usize,
    pub time: f64,
    pub path: String,
    pub headers: BTreeMap<String, Value>,
    pub matched_step: Option<String>,
    pub side_kind: Option<String>,
    pub unmatched: Option<String>,
    /// Census mode: what this request showed that the tables do not list.
    #[serde(default)]
    pub census: Vec<CensusEntry>,
    pub response_events: Vec<Value>,
    pub aborted_by_client: bool,
    pub body_file: String,
}

/// One thing census mode saw: `kind` is `beta`, `field` or `side`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CensusEntry {
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CensusRecord {
    pub arrival_index: usize,
    pub kind: String,
    pub value: String,
}

/// Hosts the network tripwire saw the CLI try to reach.
pub type TripwireHosts = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

struct Config {
    census: bool,
    mode: Option<script::Mode>,
    classifier: Option<script::Classifier>,
    main_model: Option<String>,
    small_model: Option<String>,
    run_dir: Option<PathBuf>,
}

/// A script file the server re-reads when it changes.
struct Source {
    path: PathBuf,
    stamp: Option<(SystemTime, u64)>,
}

struct State {
    config: Config,
    runtime: script::Runtime,
    side_counts: BTreeMap<String, usize>,
    classifier_used: usize,
    /// The blocking verdicts severity requests took that no block request has repeated yet, oldest first, each with
    /// the context its severity request carried. Judged calls can overlap (subagents), so several wait at once.
    classifier_pending: VecDeque<(String, script::Verdict)>,
    unlisted: Vec<String>,
    refused: BTreeSet<String>,
    tripwire: Option<TripwireHosts>,
    source: Option<Source>,
    reload_error: Option<String>,
    replies: VecDeque<Reply>,
    records: BTreeMap<usize, RequestRecord>,
    released: BTreeSet<String>,
    /// Hold names whose reply is waiting for a release right now.
    holding: BTreeSet<String>,
    /// Arrival indexes of the requests whose answer has been written out (or cut short).
    finished: BTreeSet<usize>,
    failures: Vec<String>,
    log: File,
    /// One line per request, written when it is routed, before its answer is sent.
    requests: File,
}

struct Shared {
    key: String,
    dir: PathBuf,
    state: Mutex<State>,
    stop: AtomicBool,
}

pub struct ScriptedModel {
    addr: SocketAddr,
    shared: Arc<Shared>,
    listener: Option<JoinHandle<()>>,
}

impl ScriptedModel {
    pub fn start(addr: impl ToSocketAddrs, mut script: Script) -> io::Result<Self> {
        script::validate_steps("script", &script.steps, &script.branches).map_err(|e| invalid(&e))?;
        for (index, reply) in script.replies.iter_mut().enumerate() {
            validate(reply)?;
            if reply.id.is_empty() {
                reply.id = format!("reply_{}", index + 1);
            }
        }
        crate::private_fs::create_private_dir_all(&script.log_dir, &script.log_dir)?;
        let log_path = script.log_dir.join("endpoint.jsonl");
        if log_path.try_exists()? {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "endpoint log exists"));
        }
        let log = crate::private_fs::open_private(&log_path)?;
        let requests_path = script.log_dir.join("requests.jsonl");
        if requests_path.try_exists()? {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "request log exists"));
        }
        let requests = crate::private_fs::open_private(&requests_path)?;
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            key: script.key,
            dir: script.log_dir,
            state: Mutex::new(State {
                config: Config {
                    census: script.census,
                    mode: script.mode,
                    classifier: script.classifier,
                    main_model: script.main_model,
                    small_model: script.small_model,
                    run_dir: script.run_dir,
                },
                runtime: script::Runtime::new(script.steps, script.branches),
                side_counts: BTreeMap::new(),
                classifier_used: 0,
                classifier_pending: VecDeque::new(),
                unlisted: Vec::new(),
                refused: BTreeSet::new(),
                tripwire: None,
                source: None,
                reload_error: None,
                replies: script.replies.into(),
                records: BTreeMap::new(),
                released: BTreeSet::new(),
                holding: BTreeSet::new(),
                finished: BTreeSet::new(),
                failures: Vec::new(),
                log,
                requests,
            }),
            stop: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let handle = thread::Builder::new().name("scripted-model".into()).spawn(move || {
            let mut connections = Vec::new();
            let mut index = 0;
            while !worker.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        index += 1;
                        let shared = worker.clone();
                        connections.push(thread::spawn(move || serve(stream, index, shared)));
                    }
                    Err(e) if transient_accept_error(&e) => thread::sleep(Duration::from_millis(10)),
                    Err(e) => {
                        worker.state.lock().unwrap().failures.push(e.to_string());
                        break;
                    }
                }
                let mut i = 0;
                while i < connections.len() {
                    if connections[i].is_finished() {
                        if connections.swap_remove(i).join().is_err() {
                            worker.state.lock().unwrap().failures.push("connection panicked".into());
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            for connection in connections {
                if connection.join().is_err() {
                    worker.state.lock().unwrap().failures.push("connection panicked".into());
                }
            }
        })?;
        Ok(Self {
            addr,
            shared,
            listener: Some(handle),
        })
    }

    /// Stop accepting requests and wait for connection workers to finish their logs.
    pub fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn release(&self, name: &str) {
        self.shared.state.lock().unwrap().released.insert(name.into());
    }
    /// The names of the holds a reply is waiting at right now.
    pub fn held(&self) -> Vec<String> {
        self.shared.state.lock().unwrap().holding.iter().cloned().collect()
    }
    /// Consumes each release marker once and mirrors active holds into private empty files. Keeping the caller's
    /// seen set means a release file can remain as evidence without issuing another release on every poll.
    pub fn poll_control_files(&self, released: &mut BTreeSet<String>) -> io::Result<()> {
        let dir = &self.shared.dir;
        let release_dir = dir.join("release");
        let held_dir = dir.join("held");
        crate::private_fs::create_private_dir_all(&release_dir, dir)?;
        crate::private_fs::create_private_dir_all(&held_dir, dir)?;
        let valid_name = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        for entry in std::fs::read_dir(&release_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(name) = name.to_str().filter(|n| valid_name(n)) {
                if entry.file_type()?.is_file() && released.insert(name.to_owned()) {
                    self.release(name);
                }
            }
        }
        let held: BTreeSet<_> = self.held().into_iter().filter(|n| valid_name(n)).collect();
        for name in &held {
            crate::private_fs::write_private(&held_dir.join(name), b"")?;
        }
        for entry in std::fs::read_dir(&held_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(name) = name.to_str().filter(|n| valid_name(n)) {
                if entry.file_type()?.is_file() && !held.contains(name) {
                    std::fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(())
    }
    /// Arrival indexes of the requests whose answer is complete: written to the end, cut on purpose, or dropped by
    /// the client.
    pub fn finished(&self) -> BTreeSet<usize> {
        self.shared.state.lock().unwrap().finished.clone()
    }
    pub fn records(&self) -> Vec<RequestRecord> {
        self.shared.state.lock().unwrap().records.values().cloned().collect()
    }
    pub fn unmatched(&self) -> Vec<RequestRecord> {
        self.records().into_iter().filter(|r| r.unmatched.is_some()).collect()
    }
    /// Fails with one line per kind of problem: unmatched requests, unused required steps, unused ordered replies,
    /// branch sets that did not run exactly one group, unlisted betas and fields, tripwire hosts (when a tripwire is
    /// attached) and transport errors.
    pub fn assert_clean(&self) -> Result<(), String> {
        let state = self.shared.state.lock().unwrap();
        let mut problems = Vec::new();
        let unmatched: Vec<String> = state
            .records
            .values()
            .filter_map(|r| {
                r.unmatched
                    .as_ref()
                    .map(|why| format!("#{} {} ({why})", r.arrival_index, r.path))
            })
            .collect();
        if !unmatched.is_empty() {
            problems.push(format!("unmatched requests: {}", unmatched.join("; ")));
        }
        let unused = state.runtime.unused_required();
        if !unused.is_empty() {
            problems.push(format!("unused required steps: {}", unused.join(", ")));
        }
        if !state.replies.is_empty() {
            problems.push(format!("unused replies: {}", state.replies.len()));
        }
        let branches = state.runtime.branch_problems();
        if !branches.is_empty() {
            problems.push(format!("branch groups: {}", branches.join("; ")));
        }
        if !state.unlisted.is_empty() {
            problems.push(format!("unlisted betas/fields: {}", state.unlisted.join("; ")));
        }
        if let Some(hosts) = state.tripwire.as_ref().map(|t| t()).filter(|h| !h.is_empty()) {
            problems.push(format!("tripwire hosts: {}", hosts.join(", ")));
        }
        let mut errors = state.failures.clone();
        errors.extend(state.reload_error.clone());
        if !errors.is_empty() {
            problems.push(format!("errors: {errors:?}"));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("\n"))
        }
    }

    /// Everything census mode saw that the tables do not list, in arrival order.
    pub fn census_records(&self) -> Vec<CensusRecord> {
        let state = self.shared.state.lock().unwrap();
        state
            .records
            .values()
            .flat_map(|r| {
                r.census.iter().map(|c| CensusRecord {
                    arrival_index: r.arrival_index,
                    kind: c.kind.clone(),
                    value: c.value.clone(),
                })
            })
            .collect()
    }

    /// Values captured from requests so far.
    pub fn captures(&self) -> BTreeMap<String, String> {
        self.shared.state.lock().unwrap().runtime.vars.clone()
    }

    /// How many requests each side row has answered, by row name.
    pub fn side_counts(&self) -> BTreeMap<String, usize> {
        self.shared.state.lock().unwrap().side_counts.clone()
    }

    /// How often each step has been used.
    pub fn step_uses(&self) -> BTreeMap<String, usize> {
        self.shared.state.lock().unwrap().runtime.uses()
    }

    /// For each branch set, the groups that ran.
    pub fn branches_used(&self) -> BTreeMap<String, Vec<String>> {
        self.shared.state.lock().unwrap().runtime.branches_used()
    }

    /// Betas and fields the policy table refused so far; a run manifest lists them.
    pub fn refused(&self) -> Vec<String> {
        self.shared.state.lock().unwrap().refused.iter().cloned().collect()
    }

    /// Makes `assert_clean` report the hosts a network tripwire has seen.
    pub fn attach_tripwire(&self, hosts: TripwireHosts) {
        self.shared.state.lock().unwrap().tripwire = Some(hosts);
    }

    /// Re-reads this script file (`ScriptFile` format) before the next request after it changes. A file that does
    /// not parse leaves the running script in place and is reported by `assert_clean` until a later read succeeds.
    pub fn watch_script(&self, path: impl Into<PathBuf>) {
        let path = path.into();
        let stamp = script_stamp(&path);
        self.shared.state.lock().unwrap().source = Some(Source { path, stamp });
    }
}

fn script_stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

impl Shared {
    fn reload_if_changed(&self) {
        let mut state = self.state.lock().unwrap();
        let Some(source) = state.source.as_mut() else { return };
        let stamp = script_stamp(&source.path);
        if stamp == source.stamp {
            return;
        }
        source.stamp = stamp;
        let path = source.path.clone();
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|text| script::ScriptFile::parse(&text));
        match parsed {
            Ok(file) => {
                state.config.mode = file.mode;
                state.config.classifier = file.classifier.clone();
                state.config.main_model = file.main_model.clone();
                state.config.small_model = file.small_model.clone();
                state.config.run_dir = file.run_dir.clone();
                state.runtime = script::Runtime::new(file.model, file.branches);
                state.classifier_used = 0;
                state.classifier_pending.clear();
                state.replies.clear();
                state.reload_error = None;
            }
            Err(e) => state.reload_error = Some(format!("script reload failed: {e}")),
        }
    }
}

impl Drop for ScriptedModel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Errors that say nothing about the listener itself: a connection reset before accept, a signal, or descriptor
/// exhaustion that clears once other connections close.
fn transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) || matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate(reply: &Reply) -> io::Result<()> {
    if reply.pace_ms > 60_000 {
        return Err(invalid("pace_ms must be between 0 and 60000"));
    }
    for block in &reply.content {
        let valid = match block["type"].as_str() {
            Some("text") => block["text"].is_string(),
            Some("thinking") => block["thinking"].is_string(),
            Some("tool_use") => block["id"].is_string() && block["name"].is_string() && block["input"].is_object(),
            _ => false,
        };
        if !valid {
            return Err(invalid("unsupported reply content block"));
        }
    }
    for point in reply.cut.iter().chain(reply.hold.iter().map(|h| &h.at)) {
        if matches!(point, Point::AfterBlock(k) if *k >= reply.content.len()) {
            return Err(invalid("block position out of range"));
        }
    }
    if reply.cut == Some(Point::BeforeStop) {
        return Err(invalid("cut must follow message_start or a block"));
    }
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if !reply.usage[field].is_u64() {
            return Err(invalid("usage must have nonnegative token counts"));
        }
    }
    for (key, value) in &reply.ok_headers {
        if !http::valid_header(key, value) || !key.to_ascii_lowercase().starts_with("anthropic-ratelimit-unified-") {
            return Err(invalid("unsupported rate limit header"));
        }
    }
    if let Some(error) = &reply.error {
        if !(error.status == 404 || error.status == 429 || (500..=599).contains(&error.status)) {
            return Err(invalid("unsupported error status"));
        }
        for (key, value) in &error.headers {
            if !http::valid_header(key, value) || !key.to_ascii_lowercase().starts_with("anthropic-ratelimit-unified-")
            {
                return Err(invalid("unsupported rate limit header"));
            }
        }
        if error
            .retry_after
            .as_ref()
            .is_some_and(|v| !http::valid_header("retry-after", v))
        {
            return Err(invalid("invalid retry-after"));
        }
    }
    Ok(())
}

/// What a request is answered with.
enum Answer {
    Json {
        status: u16,
        body: Value,
        headers: BTreeMap<String, String>,
    },
    Reply(Reply),
}

fn not_found() -> Answer {
    Answer::Json {
        status: 404,
        body: json!({"error":{"type":"not_found_error"}}),
        headers: BTreeMap::new(),
    }
}

/// A normal 200 stream whose only block says why nothing matched, so that no retry or healing ladder reacts.
fn sentinel(index: usize, request: &http::Request, body: &Value) -> Reply {
    let kinds: Vec<_> = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .and_then(|m| m["content"].as_array())
        .map(|blocks| blocks.iter().map(|b| b["type"].as_str().unwrap_or("unknown")).collect())
        .unwrap_or_default();
    let preview: String = String::from_utf8_lossy(&request.body).chars().take(120).collect();
    Reply::text(format!(
        "SCRIPTED-UNMATCHED {index}: {}, {}, {kinds:?}, {preview}",
        request.path, body["model"]
    ))
}

fn unmatched(
    index: usize,
    request: &http::Request,
    body: &Value,
    record: &mut RequestRecord,
    why: impl Into<String>,
) -> Answer {
    let why = why.into();
    let mut reply = sentinel(index, request, body);
    if let Some(Value::String(text)) = reply.content[0].get_mut("text") {
        text.push_str(" -- ");
        text.push_str(&why);
    }
    record.unmatched.get_or_insert(why);
    Answer::Reply(reply)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Checks the request's betas and top-level body fields against the policy tables. A value no table entry covers is
/// recorded in the census or, in strict mode, in the run's unlisted list and the request's `unmatched`. A refused
/// value is noted in the manifest; the first refusal's status and body come back.
fn apply_policy(
    state: &mut State,
    index: usize,
    request: &http::Request,
    body: &Value,
    record: &mut RequestRecord,
    census: bool,
) -> Option<(u16, Value)> {
    let beta = request.headers.get("anthropic-beta").map_or("", String::as_str);
    let mut refusal = None;
    for finding in side::check(beta, body) {
        match finding {
            side::Finding::Refused {
                value, status, body, ..
            } => {
                state.refused.insert(value);
                refusal.get_or_insert((status, body));
            }
            side::Finding::Unlisted { kind, value } if census => record.census.push(CensusEntry {
                kind: kind.into(),
                value,
            }),
            side::Finding::Unlisted { kind, value } => {
                state.unlisted.push(format!("{kind} `{value}` (request {index})"));
                record
                    .unmatched
                    .get_or_insert_with(|| format!("unlisted {kind} `{value}`"));
            }
        }
    }
    refusal
}

/// Decides the answer to one request: the path rows, the beta and field policy, the side rows, the steps, and last
/// the ordered replies. Fills in the record's `matched_step`, `side_kind`, `census` and `unmatched`.
fn route(
    shared: &Shared,
    index: usize,
    request: &http::Request,
    body: &Value,
    known: bool,
    record: &mut RequestRecord,
) -> Answer {
    let path = request.path.split('?').next().unwrap_or("");
    if !known {
        return not_found();
    }
    let mut guard = shared.state.lock().unwrap();
    let state = &mut *guard;
    let census = state.config.census;
    if record.unmatched.is_none() && matches!(path, "/v1/messages/count_tokens" | "/v1/messages/cache_touch") {
        let kind = if path.ends_with("count_tokens") {
            "count_tokens"
        } else {
            "cache_touch"
        };
        record.side_kind = Some(kind.into());
        *state.side_counts.entry(kind.into()).or_default() += 1;
        if let Some((status, body)) = apply_policy(state, index, request, body, record, census) {
            record.side_kind = Some("refused".into());
            return Answer::Json {
                status,
                body,
                headers: BTreeMap::new(),
            };
        }
        if kind == "cache_touch" {
            return not_found();
        }
        let count = String::from_utf8_lossy(&request.body).chars().count().div_ceil(4);
        return Answer::Json {
            status: 200,
            body: json!({"input_tokens":count}),
            headers: BTreeMap::new(),
        };
    }
    if let Some(why) = record.unmatched.clone() {
        return unmatched(index, request, body, record, why);
    }
    let view = matchers::View::new(&request.path, body);

    // The policy for the betas and body fields the request carries.
    let refusal = apply_policy(state, index, request, body, record, census);
    if let Some((status, body)) = refusal {
        record.side_kind = Some("refused".into());
        return Answer::Json {
            status,
            body,
            headers: BTreeMap::new(),
        };
    }
    if let Some(why) = record.unmatched.clone() {
        return unmatched(index, request, body, record, why);
    }

    let cfg = script::Cfg {
        main_model: state.config.main_model.as_deref(),
        small_model: state.config.small_model.as_deref(),
        census,
        run_dir: state.config.run_dir.as_deref(),
        now: unix_now(),
    };
    let side_kind = side::classify(&view, cfg.small_model);
    if let Some(kind) = side_kind {
        let count = {
            let n = state.side_counts.entry(kind.name().into()).or_default();
            *n += 1;
            *n
        };
        record.side_kind = Some(kind.name().into());
        match kind {
            side::SideKind::Compaction | side::SideKind::ToolUseSummary => {
                match state.runtime.select(&view, &cfg, true) {
                    Some(Ok(picked)) => {
                        record.matched_step = Some(picked.step_id);
                        return Answer::Reply(picked.reply);
                    }
                    Some(Err(why)) => return unmatched(index, request, body, record, why),
                    None => {}
                }
                let text = if kind == side::SideKind::Compaction {
                    side::compaction_text(count)
                } else {
                    side::label_text(count)
                };
                return Answer::Reply(Reply::text(text));
            }
            side::SideKind::AutoClassifier => {
                // A subagent's tool calls reach the classifier in any tab mode, so a scenario that scripts a
                // classifier gets its requests answered whatever its mode is.
                if state.config.mode != Some(script::Mode::Auto) && state.config.classifier.is_none() {
                    return unmatched(
                        index,
                        request,
                        body,
                        record,
                        "classifier request outside an auto scenario that scripts no classifier",
                    );
                }
                let stage = side::classifier_stage(&view);
                // A block question repeats the verdict its severity question took: one verdict per judged call. With
                // several calls judged at once, the block question takes the pending verdict whose severity request
                // carried the same earlier context, and the oldest pending one when none does.
                let context = view.context_key();
                let pending = if stage == side::ClassifierStage::Block {
                    let at = state
                        .classifier_pending
                        .iter()
                        .position(|(key, _)| !context.is_empty() && *key == context)
                        .or((!state.classifier_pending.is_empty()).then_some(0));
                    at.and_then(|i| state.classifier_pending.remove(i))
                } else {
                    None
                };
                let verdict = match pending {
                    Some((_, v)) => Some(v),
                    None => {
                        let v = match &state.config.classifier {
                            Some(script::Classifier::DefaultAllow) => Some(script::Verdict {
                                allow: true,
                                reason: "scripted allow".into(),
                            }),
                            Some(script::Classifier::Verdicts(list)) => list.get(state.classifier_used).cloned(),
                            None => None,
                        };
                        state.classifier_used += 1;
                        v
                    }
                };
                return match verdict {
                    Some(v) => {
                        if stage == side::ClassifierStage::Severity && !v.allow {
                            state.classifier_pending.push_back((context, v.clone()));
                        }
                        record.matched_step = Some(format!("auto_classifier#{count}"));
                        Answer::Reply(Reply::text(side::verdict_text(stage, v.allow, &v.reason)))
                    }
                    None => unmatched(index, request, body, record, "no classifier verdict is scripted"),
                };
            }
            side::SideKind::SmallFast => {
                let quote = side::quote(&view);
                if census {
                    record.census.push(CensusEntry {
                        kind: "side".into(),
                        value: quote,
                    });
                    return Answer::Reply(sentinel(index, request, body));
                }
                return unmatched(
                    index,
                    request,
                    body,
                    record,
                    format!("small-model request no row fits: {quote}"),
                );
            }
            side::SideKind::CountTokens | side::SideKind::CacheTouch => {}
        }
    }

    match state.runtime.select(&view, &cfg, false) {
        Some(Ok(picked)) => {
            record.matched_step = Some(picked.step_id);
            return Answer::Reply(picked.reply);
        }
        Some(Err(why)) => return unmatched(index, request, body, record, why),
        None => {}
    }
    if let Some(front) = state.replies.front_mut() {
        let reply = front.clone();
        if let Some(error) = front.error.as_mut().filter(|e| e.times > 0) {
            error.times -= 1;
        } else {
            state.replies.pop_front();
        }
        record.matched_step = Some(reply.id.clone());
        return Answer::Reply(reply);
    }
    let why = if state.runtime.has_steps() {
        "no scripted step takes this request"
    } else {
        "no ordered reply remains"
    };
    unmatched(index, request, body, record, why)
}

fn serve(mut stream: TcpStream, index: usize, shared: Arc<Shared>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let request = http::read_request(&mut stream);
    let (request, parse_error) = match request {
        Ok(request) => (request, None),
        Err(error) => (error.request, Some(error.reason)),
    };
    let mut record = RequestRecord {
        arrival_index: index,
        time,
        path: request.path.clone(),
        headers: BTreeMap::new(),
        matched_step: None,
        side_kind: None,
        unmatched: parse_error,
        census: Vec::new(),
        response_events: Vec::new(),
        aborted_by_client: false,
        body_file: format!("body_{index}.json"),
    };
    for name in ["anthropic-version", "anthropic-beta"] {
        if let Some(value) = request.headers.get(name) {
            record.headers.insert(name.into(), json!(value));
        }
    }
    record.headers.insert(
        "x-api-key-present".into(),
        json!(request.headers.contains_key("x-api-key")),
    );
    if let Err(e) = crate::private_fs::write_private(&shared.dir.join(&record.body_file), &request.body) {
        shared.state.lock().unwrap().failures.push(e.to_string());
    }
    if request.headers.get("x-api-key") != Some(&shared.key)
        || request.headers.contains_key("authorization")
        || !request.headers.contains_key("anthropic-version")
    {
        record
            .unmatched
            .get_or_insert_with(|| "invalid request credentials or version".into());
    }
    if request.headers.contains_key("content-encoding") {
        record
            .unmatched
            .get_or_insert_with(|| "content-encoding present".into());
    }
    shared.reload_if_changed();
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let path = request.path.split('?').next().unwrap_or("");
    let known = request.method == "POST"
        && matches!(
            path,
            "/v1/messages" | "/v1/messages/count_tokens" | "/v1/messages/cache_touch"
        );
    if !known {
        record.unmatched.get_or_insert_with(|| "unknown endpoint".into());
    }
    if path == "/v1/messages"
        && (!body.is_object()
            || !body["model"].is_string()
            || !(body["stream"].is_null() || body["stream"].is_boolean()))
    {
        record.unmatched.get_or_insert_with(|| "invalid messages body".into());
    }
    shared.state.lock().unwrap().records.insert(index, record.clone());
    let answer = route(&shared, index, &request, &body, known, &mut record);
    {
        let mut state = shared.state.lock().unwrap();
        state.records.insert(index, record.clone());
        let line = json!({"arrival_index":index,"time":record.time,"path":record.path,
            "matched_step":record.matched_step,"side_kind":record.side_kind,"unmatched":record.unmatched,
            "census":record.census});
        if let Err(e) = writeln!(state.requests, "{line}").and_then(|_| state.requests.flush()) {
            state.failures.push(e.to_string());
        }
    }
    let result = (|| -> io::Result<()> {
        let reply = match answer {
            Answer::Json { status, body, headers } => {
                record.response_events.push(json!({"status":status,"body":body}));
                return http::json_response(&mut stream, index, status, &body, &headers);
            }
            Answer::Reply(reply) => reply,
        };
        if let Some(error) = reply.error.as_ref().filter(|e| e.times > 0) {
            let mut headers = error.headers.clone();
            if let Some(retry_after) = &error.retry_after {
                headers
                    .entry("retry-after".into())
                    .or_insert_with(|| retry_after.clone());
            }
            if error.status == 429 {
                headers.entry("retry-after".into()).or_insert_with(|| "1".into());
                headers
                    .entry("anthropic-ratelimit-unified-status".into())
                    .or_insert_with(|| "rejected".into());
            }
            // 404 is what the API answers for a model id it does not know.
            let kind = match error.status {
                404 => "not_found_error",
                429 => "rate_limit_error",
                _ => "overloaded_error",
            };
            let body = json!({"type":"error","error":{"type":kind,"message":"scripted error"}});
            record.response_events.push(json!({"status":error.status,"body":body}));
            return http::json_response(&mut stream, index, error.status, &body, &headers);
        }
        let model = body["model"].as_str().unwrap_or("unknown");
        if body["stream"] != true {
            record
                .response_events
                .push(json!({"status":200,"body":sse::message(&reply, index, model)}));
            return http::json_response(
                &mut stream,
                index,
                200,
                &sse::message(&reply, index, model),
                &reply.ok_headers,
            );
        }
        http::stream_headers(&mut stream, index, &reply.ok_headers)?;
        for frame in sse::frames(&reply, index, model) {
            frame.write_to(&mut stream, |delay| {
                // Sliced so a shutdown never waits out a long pace.
                let end = Instant::now() + delay;
                while !shared.stop.load(Ordering::Acquire) {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    std::thread::sleep(left.min(Duration::from_millis(20)));
                }
            })?;
            record
                .response_events
                .push(json!({"event":frame.event,"data":frame.data}));
            if let Some(point) = frame.after {
                if reply.cut.as_ref() == Some(&point) {
                    break;
                }
                if let Some(hold) = reply.hold.as_ref().filter(|h| h.at == point) {
                    shared.state.lock().unwrap().holding.insert(hold.name.clone());
                    let held = http::hold(&mut stream, &shared, &hold.name, &mut record);
                    shared.state.lock().unwrap().holding.remove(&hold.name);
                    held?;
                }
            }
        }
        Ok(())
    })();
    let mut state = shared.state.lock().unwrap();
    match &result {
        Err(e) if e.kind() == io::ErrorKind::Interrupted => state
            .failures
            .push(format!("request {index} was still held when the server stopped")),
        Err(_) => record.aborted_by_client = true,
        Ok(()) => {}
    }
    state.records.insert(index, record.clone());
    state.finished.insert(index);
    if let Err(e) = serde_json::to_writer(&mut state.log, &record)
        .map_err(io::Error::other)
        .and_then(|_| state.log.write_all(b"\n"))
        .and_then(|_| state.log.flush())
    {
        state.failures.push(e.to_string());
    }
}
