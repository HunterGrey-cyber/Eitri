//! `gd` on a name: the host asks the language servers attached to the project's loaded buffers where the name is
//! defined, and answers the asking page with places inside the project, which it opens the way `gf` opens a path.
//!
//! Nothing here waits. Every editor call is a `Pending` the panel tick polls; resolving a cited file and reading its
//! line, judging where a server's answer leads, and the rust-analyzer text search run on workers, all counted against
//! one per-window cap ([`MAX_WORKERS`]) that a hung filesystem cannot get past. A lookup lasts
//! [`BUDGET`] from its request (its reply may wait [`GRACE`] more for an answer already on its way); the page gives up
//! a second after that.
//!
//! The order: a `path:line` cited beside the name, when that line on disk holds the name, asks for the definition at
//! the name there, which does not depend on how much of a workspace a server's symbol search covers; then
//! `workspace/symbol`, worded per server, asked again while an answer has no exact match and time is left (servers
//! answer empty or in part while they index); then, for rust-analyzer only, the definition lines the text search
//! found (`eitri_core::symbol_text`): its symbol search misses short and common function names, and it holds every
//! request while its workspace loads. No buffer is ever loaded, so `gd` never starts a server. The name reaches the
//! editor only as a msgpack string argument of the editor tools' constant chunks.

use super::*;
use eitri_core::agent_bridge::{EditorSymbolError, EditorSymbolMatch, SymbolCitation};
use eitri_core::editor_tools::stamp::{wall_ms, Sequencer};
use eitri_core::editor_tools::step::EditorReach;
use eitri_core::editor_tools::tools::lsp::{self, AskArgs, Asked, Collected, LspKind, RawItem, UsagePart};
use eitri_core::editor_tools::way::{EditorCall, EditorWay, LocalWay};
use eitri_core::nvim_rpc::{Pending, RpcError};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// How long a lookup asks for, from its request.
pub(super) const BUDGET: Duration = Duration::from_secs(3);
/// How long past [`BUDGET`] a reply waits for an answer already asked for. The page waits a second past the budget.
const GRACE: Duration = Duration::from_millis(500);
/// How long after an answer with no exact match the servers are asked again.
const RETRY: Duration = Duration::from_millis(300);
/// How long an ask rests between collects.
const POLL: Duration = lsp::POLL;
/// How long a started ask rests before its first collect: a warm server answers in tens of milliseconds.
const FIRST_COLLECT: Duration = Duration::from_millis(30);
/// How long the definition at a cited line may take before the symbol search goes ahead without it.
const CITED_SHARE: Duration = Duration::from_millis(1500);
/// How long before a step's end its last collect goes out, so what has arrived is read in time.
const LAST_LEAD: Duration = Duration::from_millis(250);
/// The most lookups a window has out at once.
const MAX_LOOKUPS: usize = 4;
/// The most places one reply offers.
const MAX_MATCHES: usize = 50;
/// The most file workers (a cited file's resolution, a judgement of places, a text search) running at once in a
/// window, those of lookups already answered or dropped included, until they return. A lookup at most has three out.
const MAX_WORKERS: usize = 12;
/// The most of a cited file read to find its line.
const CITED_FILE_MAX: u64 = 1_000_000;
/// The most characters of a failed editor call's reason the reply carries.
const REASON_MAX: usize = 160;
const GOPLS: &str = "gopls";
const RUST_ANALYZER: &str = "rust-analyzer";
/// What nvim-lspconfig (and `vim.lsp.config`) names the rust-analyzer client; rustaceanvim names it `rust-analyzer`.
const RUST_ANALYZER_LSPCONFIG: &str = "rust_analyzer";

/// Whether a client is rust-analyzer, under either spelling and in any case.
fn is_rust_analyzer(client: &str) -> bool {
    client.eq_ignore_ascii_case(RUST_ANALYZER) || client.eq_ignore_ascii_case(RUST_ANALYZER_LSPCONFIG)
}

/// A name as the page sent it, and what servers are asked and matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    name: String,
    /// The part after the last `.` or `::`: what a symbol is called.
    last: String,
    /// The segment just before `last` (`Type` of `pkg.Type.Method`), which filters the matches.
    qualifier: Option<String>,
}

impl Wanted {
    /// The qualifier when the name was cut at a `::` (a path: `Type::method`), else none: before a `.` stands a value
    /// or a package, which says nothing about the container of a Rust definition.
    fn path_qualifier(&self) -> Option<&str> {
        let qualifier = self.qualifier.as_deref()?;
        let head = self.name.strip_suffix(self.last.as_str())?;
        head.ends_with("::").then_some(qualifier)
    }
}

fn wanted(name: &str) -> Wanted {
    let cut = [name.rfind("::").map(|at| (at, 2)), name.rfind('.').map(|at| (at, 1))]
        .into_iter()
        .flatten()
        .max_by_key(|(at, _)| *at);
    match cut {
        Some((at, width)) if at + width < name.len() => Wanted {
            name: name.to_owned(),
            last: name[at + width..].to_owned(),
            qualifier: name[..at]
                .rsplit(['.', ':'])
                .find(|segment| !segment.is_empty())
                .map(str::to_owned),
        },
        _ => Wanted {
            name: name.to_owned(),
            last: name.trim_matches(['.', ':']).to_owned(),
            qualifier: None,
        },
    }
}

/// The query every server is asked, and the servers worded otherwise: gopls takes a qualified name itself
/// (`pkg.Name`, `Type.Method`); rust-analyzer's `name#` searches all its symbols, functions included, and goes under
/// both of the names its client is given.
fn queries(wanted: &Wanted) -> (String, Vec<(String, String)>) {
    let gopls = if wanted.name.contains("::") {
        wanted.last.clone()
    } else {
        wanted.name.clone()
    };
    (
        wanted.last.clone(),
        vec![
            (GOPLS.to_owned(), gopls),
            (RUST_ANALYZER.to_owned(), format!("{}#", wanted.last)),
            (RUST_ANALYZER_LSPCONFIG.to_owned(), format!("{}#", wanted.last)),
        ],
    )
}

fn last_segment(text: &str) -> &str {
    text.rsplit(['.', ':', '/']).find(|s| !s.is_empty()).unwrap_or(text)
}

/// The items that name exactly what was asked for. A result's name is compared without a trailing `()` (vtsls names
/// functions so) and, with gopls attached, without what comes before its last `.` (gopls names methods `Type.Method`).
/// A qualified name keeps the items whose container is its qualifier, or (gopls) whose name is `<qualifier>.<name>`;
/// when no exact item has a container, all of them stay.
fn exact<'a>(items: &'a [RawItem], wanted: &Wanted, gopls: bool) -> Vec<&'a RawItem> {
    let named = |item: &&RawItem| {
        let Some(name) = item.name.as_deref() else {
            return false;
        };
        let name = name.strip_suffix("()").unwrap_or(name);
        name == wanted.last || gopls && name.rsplit('.').next() == Some(wanted.last.as_str())
    };
    let exact: Vec<&RawItem> = items.iter().filter(named).collect();
    let Some(qualifier) = wanted.qualifier.as_deref() else {
        return exact;
    };
    let qualified = format!("{qualifier}.{}", wanted.last);
    let kept: Vec<&RawItem> = exact
        .iter()
        .copied()
        .filter(|item| {
            let container = item
                .container
                .as_deref()
                .is_some_and(|container| container == qualifier || last_segment(container) == qualifier);
            let name = item.name.as_deref().unwrap_or_default();
            container || gopls && (name == qualified || name.ends_with(&format!(".{qualified}")))
        })
        .collect();
    if kept.is_empty() && exact.iter().all(|item| item.container.is_none()) {
        exact
    } else {
        kept
    }
}

/// Where a place leads, when that is a regular file under the canonical project root: its path from there.
fn inside(canonical_root: &Path, path: &Path) -> Option<String> {
    let real = path.canonicalize().ok()?;
    if !real.is_file() {
        return None;
    }
    let relative = real.strip_prefix(canonical_root).ok()?.to_str()?;
    (!relative.is_empty()).then(|| relative.to_owned())
}

/// The places among `items` (an absolute path, a one-based line, a one-based column) that lead to a regular file
/// inside the project, each once. Dependencies, the standard library and anything else outside go: `gf` would refuse
/// them. It walks paths, so it runs on a worker.
fn place(root: &Path, items: Vec<(PathBuf, u32, u32)>, source: &'static str) -> Vec<EditorSymbolMatch> {
    let Ok(canonical_root) = root.canonicalize() else {
        return Vec::new();
    };
    let mut judged: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut places = Vec::new();
    for (path, line, col) in items {
        if !path.is_absolute() {
            continue;
        }
        let relative = judged
            .entry(path.clone())
            .or_insert_with(|| inside(&canonical_root, &path))
            .clone();
        let Some(relative) = relative else { continue };
        if !seen.insert((relative.clone(), line)) {
            continue;
        }
        places.push(EditorSymbolMatch {
            path: relative,
            line,
            col,
            source,
        });
        if places.len() >= MAX_MATCHES {
            break;
        }
    }
    places
}

fn lsp_places(items: &[&RawItem]) -> Vec<(PathBuf, u32, u32)> {
    items
        .iter()
        .map(|item| (PathBuf::from(&item.path), item.line, item.character.saturating_add(1)))
        .collect()
}

/// The cited file, when it is a regular file inside the project and its `line` on disk holds `name`. A FIFO, a device
/// or a path leading out of the project is none, and none of them is opened.
fn cited_file(root: &Path, cited: &SymbolCitation, name: &str) -> Option<PathBuf> {
    use std::io::Read;
    let file = super::path_lookup::resolve_inside(root, &cited.path)?;
    let line = usize::try_from(cited.line).ok()?.checked_sub(1)?;
    let handle = eitri_core::scratch::open_regular_file(&file)?;
    let mut bytes = Vec::new();
    handle.take(CITED_FILE_MAX).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    text.lines().nth(line)?.contains(name).then_some(file)
}

fn error(kind: &'static str, server: Option<String>) -> EditorSymbolError {
    EditorSymbolError {
        kind,
        server,
        message: None,
    }
}

fn refused(message: String) -> EditorSymbolError {
    EditorSymbolError {
        kind: "refused",
        server: None,
        message: Some(message),
    }
}

/// Why an editor call failed, as a reply may name it: the editor out of reach (its connection closed, no editor to
/// ask, a call that could not be sent) or, over the link, window 1's answer, which is its refusal of the call. One
/// line of at most [`REASON_MAX`] characters, since it goes in the band. An error nvim raised running this window's
/// own chunk is none: it tells the person nothing they can act on, and the lookup comes to what it would have
/// without the call.
fn failure_reason(error: &RpcError, reach: EditorReach) -> Option<String> {
    if matches!(error, RpcError::Nvim(_)) && reach == EditorReach::Own {
        return None;
    }
    let text = error.to_string();
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("the editor call failed");
    let mut reason: String = line.chars().take(REASON_MAX).collect();
    if line.chars().nth(REASON_MAX).is_some() {
        reason.push('…');
    }
    Some(reason)
}

type Reply = Result<Vec<EditorSymbolMatch>, EditorSymbolError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The definition at the cited line.
    Cited,
    /// The servers' symbol search.
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    Collect,
    Ask,
}

enum Stage {
    Start,
    /// The cited file being resolved and its line read.
    Citing(mpsc::Receiver<Option<PathBuf>>),
    Asking(Phase, Pending),
    Resting(Phase, Instant, Next),
    Collecting(Phase, Pending),
    /// Exact matches being judged against the project.
    Placing(Phase, mpsc::Receiver<Vec<EditorSymbolMatch>>),
    /// The servers have had their time; the text search is still out.
    Texting,
    Over,
}

/// What a lookup needs from its window for one poll.
struct Cx<'a> {
    way: Option<&'a dyn EditorWay>,
    sequencer: &'a mut Sequencer,
    asks: &'a mut u64,
    workers: &'a Arc<AtomicUsize>,
    now: Instant,
}

/// Counts a file worker until its closure returns or unwinds.
struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// [`lsp::worker`] counted in `workers`; none when the window already has [`MAX_WORKERS`] out.
fn counted_worker<T: Send + 'static>(
    workers: &Arc<AtomicUsize>,
    job: impl FnOnce() -> T + Send + 'static,
) -> Option<mpsc::Receiver<T>> {
    if workers.load(Ordering::SeqCst) >= MAX_WORKERS {
        return None;
    }
    workers.fetch_add(1, Ordering::SeqCst);
    let counted = Counted(workers.clone());
    Some(lsp::worker(move || {
        let _counted = counted;
        job()
    }))
}

enum Flow {
    Wait,
    Reply(Reply),
}

struct Lookup {
    view: ViewId,
    request_id: String,
    /// The asking tab's provider session, which names the lookup's calls to a linked editor.
    session: String,
    wanted: Wanted,
    cited: Option<SymbolCitation>,
    root: PathBuf,
    started: Instant,
    stage: Stage,
    /// The ask out, whose collects name it.
    id: String,
    /// The servers the last ask reached, and those of them still reporting unfinished work.
    clients: Vec<String>,
    busy: Vec<String>,
    /// The editor answered the symbol search with a client list (the cited step's list says nothing of it).
    searched: bool,
    /// A symbol search came back from every server asked.
    answered: bool,
    /// Some symbol search answered with items.
    had_items: bool,
    /// The last symbol-search collect still had a server's part out.
    held: bool,
    /// The last symbol search answered with items, none of them exact.
    inexact: bool,
    text: Option<mpsc::Receiver<Vec<EditorSymbolMatch>>>,
    text_found: Option<Vec<EditorSymbolMatch>>,
    /// Why the lookup's last editor call failed, when it failed for a reason [`failure_reason`] names; none once
    /// another call goes out. The reply when nothing else settles it.
    failed: Option<String>,
}

impl Lookup {
    fn new(
        view: ViewId,
        request_id: String,
        name: &str,
        cited: Option<SymbolCitation>,
        root: PathBuf,
        now: Instant,
    ) -> Lookup {
        Lookup {
            view,
            request_id,
            session: String::new(),
            wanted: wanted(name),
            cited,
            root,
            started: now,
            stage: Stage::Start,
            id: String::new(),
            clients: Vec::new(),
            busy: Vec::new(),
            searched: false,
            answered: false,
            had_items: false,
            held: false,
            inexact: false,
            text: None,
            text_found: None,
            failed: None,
        }
    }

    fn end(&self) -> Instant {
        self.started + BUDGET
    }

    fn has(&self, client: &str) -> bool {
        self.clients.iter().any(|name| name == client)
    }

    /// rust-analyzer's client among those asked, as the client names itself.
    fn analyzer(&self) -> Option<&str> {
        self.clients
            .iter()
            .map(String::as_str)
            .find(|name| is_rust_analyzer(name))
    }

    /// The one server a message names: none when several were asked, since an answer does not say whose it is.
    fn server(&self) -> Option<String> {
        match self.clients.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// Whatever comes first: this lookup's reply, or `None` while something is still out. Never blocks.
    fn poll(&mut self, cx: &mut Cx<'_>) -> Option<Reply> {
        self.take_text();
        if cx.now >= self.end() + GRACE {
            self.stage = Stage::Over;
            return Some(self.verdict());
        }
        let stage = std::mem::replace(&mut self.stage, Stage::Over);
        match self.step(stage, cx) {
            Flow::Wait => None,
            Flow::Reply(reply) => {
                self.stage = Stage::Over;
                Some(reply)
            }
        }
    }

    fn step(&mut self, stage: Stage, cx: &mut Cx<'_>) -> Flow {
        let now = cx.now;
        match stage {
            Stage::Start => {
                if cx.way.is_none() {
                    return Flow::Reply(Err(refused(format!(
                        "gd: {}",
                        eitri_core::editor_tools::words::NO_EDITOR
                    ))));
                }
                let Some(cited) = self.cited.clone() else {
                    return self.ask_workspace(cx);
                };
                let (root, name) = (self.root.clone(), self.wanted.last.clone());
                let Some(file) = counted_worker(cx.workers, move || cited_file(&root, &cited, &name)) else {
                    return self.ask_workspace(cx);
                };
                self.stage = Stage::Citing(file);
                Flow::Wait
            }
            Stage::Citing(file) => match file.try_recv() {
                Ok(Some(file)) => self.ask_cited(file, cx),
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => self.ask_workspace(cx),
                Err(mpsc::TryRecvError::Empty) if now >= self.started + CITED_SHARE => self.ask_workspace(cx),
                Err(mpsc::TryRecvError::Empty) => {
                    self.stage = Stage::Citing(file);
                    Flow::Wait
                }
            },
            Stage::Asking(phase, pending) => match pending.try_take() {
                None => self.held_up(phase, Stage::Asking(phase, pending), cx),
                Some(Err(why)) => {
                    self.failed_with(&why, cx);
                    self.phase_failed(phase, cx)
                }
                Some(Ok(value)) => match lsp::parse_asked(&value) {
                    Ok(Asked::Started { clients, busy, .. }) => {
                        self.searched |= phase == Phase::Workspace;
                        self.heard_of(clients, busy, cx);
                        self.stage = Stage::Resting(phase, now + FIRST_COLLECT, Next::Collect);
                        Flow::Wait
                    }
                    Ok(Asked::NoClient { .. }) if phase == Phase::Workspace && !self.searched => {
                        Flow::Reply(Err(error("no_client", None)))
                    }
                    _ => self.phase_failed(phase, cx),
                },
            },
            Stage::Resting(phase, at, next) => {
                if now < at {
                    self.stage = Stage::Resting(phase, at, next);
                    return Flow::Wait;
                }
                match next {
                    Next::Ask => self.ask_workspace(cx),
                    Next::Collect => {
                        let ends = match phase {
                            Phase::Cited => self.started + CITED_SHARE,
                            Phase::Workspace => self.end(),
                        };
                        let last = now + LAST_LEAD >= ends;
                        let Some(way) = cx.way else {
                            return self.phase_failed(phase, cx);
                        };
                        let call = EditorCall::LspCollect {
                            id: self.id.clone(),
                            last,
                        };
                        self.stage = Stage::Collecting(phase, self.call(way, &call, cx));
                        Flow::Wait
                    }
                }
            }
            Stage::Collecting(phase, pending) => match pending.try_take() {
                None => self.held_up(phase, Stage::Collecting(phase, pending), cx),
                Some(Err(why)) => {
                    self.failed_with(&why, cx);
                    self.phase_failed(phase, cx)
                }
                Some(Ok(value)) => match lsp::parse_collected(&value) {
                    Ok(Collected::Pending) => {
                        if phase == Phase::Workspace {
                            self.held = true;
                        }
                        self.stage = Stage::Resting(phase, now + POLL, Next::Collect);
                        Flow::Wait
                    }
                    Ok(Collected::Done { parts, missing, .. }) => {
                        let items: Vec<RawItem> = parts.into_iter().flat_map(|part| part.items).collect();
                        self.answered_with(phase, items, missing, cx)
                    }
                    _ => self.phase_failed(phase, cx),
                },
            },
            Stage::Placing(phase, places) => match places.try_recv() {
                Ok(found) if !found.is_empty() => Flow::Reply(Ok(found)),
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => match phase {
                    Phase::Cited => self.ask_workspace(cx),
                    Phase::Workspace => {
                        self.inexact = true;
                        self.no_exact(cx)
                    }
                },
                Err(mpsc::TryRecvError::Empty) => {
                    self.stage = Stage::Placing(phase, places);
                    Flow::Wait
                }
            },
            Stage::Texting => self.finish(now),
            Stage::Over => Flow::Wait,
        }
    }

    /// An editor call not answered yet: the cited step gives way to the symbol search at its share of the budget, and
    /// the symbol search ends with the budget.
    fn held_up(&mut self, phase: Phase, stage: Stage, cx: &mut Cx<'_>) -> Flow {
        match phase {
            Phase::Cited if cx.now >= self.started + CITED_SHARE => {
                self.release(cx);
                self.ask_workspace(cx)
            }
            Phase::Workspace if cx.now >= self.end() => {
                self.release(cx);
                self.held = true;
                self.finish(cx.now)
            }
            _ => {
                self.stage = stage;
                Flow::Wait
            }
        }
    }

    /// Sends `call` as the lookup's newest: an earlier call's failure no longer speaks for the lookup.
    fn call(&mut self, way: &dyn EditorWay, call: &EditorCall, cx: &mut Cx<'_>) -> Pending {
        self.failed = None;
        let stamp = self.stamp(cx);
        way.call(&self.session, call, &stamp)
    }

    fn failed_with(&mut self, error: &RpcError, cx: &Cx<'_>) {
        let reach = cx.way.map_or(EditorReach::Own, |way| way.reach());
        self.failed = failure_reason(error, reach);
    }

    fn phase_failed(&mut self, phase: Phase, cx: &mut Cx<'_>) -> Flow {
        match phase {
            Phase::Cited => self.ask_workspace(cx),
            Phase::Workspace => self.finish(cx.now),
        }
    }

    fn stamp(&self, cx: &mut Cx<'_>) -> eitri_core::editor_tools::stamp::Stamp {
        // A chunk that runs after the lookup has replied does nothing.
        let left = (self.end() + GRACE).saturating_duration_since(cx.now);
        cx.sequencer.stamp(left.max(Duration::from_millis(1)))
    }

    /// Gives up the ask out: its entry in the editor tools' table goes now, not at its deadline, so an abandoned ask
    /// does not push another tool's out of the table. Nothing waits on the call.
    fn release(&self, cx: &mut Cx<'_>) {
        let Some(way) = cx.way else { return };
        if self.id.is_empty() {
            return;
        }
        let stamp = self.stamp(cx);
        drop(way.call(
            &self.session,
            &EditorCall::LspCollect {
                id: self.id.clone(),
                last: true,
            },
            &stamp,
        ));
    }

    fn template(&mut self, cx: &mut Cx<'_>, kind: LspKind) -> AskArgs {
        *cx.asks += 1;
        self.id = format!("gd-{}", cx.asks);
        // The cited step lives for its share of the budget, the symbol search for the rest of the lookup; a margin
        // keeps the entry past the last collect.
        let until = match kind {
            LspKind::Usages => self.started + CITED_SHARE,
            _ => self.end() + GRACE,
        };
        let life = until.saturating_duration_since(cx.now) + Duration::from_secs(1);
        AskArgs {
            id: self.id.clone(),
            kind,
            root: self.root.clone(),
            budget_ms: u32::try_from(life.as_millis()).unwrap_or(u32::MAX),
            load: false,
            ..Default::default()
        }
    }

    fn ask_cited(&mut self, file: PathBuf, cx: &mut Cx<'_>) -> Flow {
        let Some(way) = cx.way else {
            return self.ask_workspace(cx);
        };
        let line = self.cited.as_ref().map(|cited| cited.line);
        let mut args = self.template(cx, LspKind::Usages);
        args.path = Some(file);
        args.line = line;
        args.symbol = Some(self.wanted.last.clone());
        args.include = vec![UsagePart::Definitions];
        self.stage = Stage::Asking(Phase::Cited, self.call(way, &EditorCall::LspAsk(args), cx));
        Flow::Wait
    }

    fn ask_workspace(&mut self, cx: &mut Cx<'_>) -> Flow {
        if cx.now + LAST_LEAD >= self.end() {
            return self.finish(cx.now);
        }
        let Some(way) = cx.way else {
            return self.finish(cx.now);
        };
        let (query, query_for) = queries(&self.wanted);
        let mut args = self.template(cx, LspKind::WorkspaceSymbols);
        args.query = Some(query);
        args.query_for = query_for;
        self.stage = Stage::Asking(Phase::Workspace, self.call(way, &EditorCall::LspAsk(args), cx));
        Flow::Wait
    }

    /// What the ask said of its clients; rust-analyzer among them starts the text search.
    fn heard_of(&mut self, clients: Vec<String>, busy: Vec<String>, cx: &mut Cx<'_>) {
        if !clients.is_empty() {
            self.clients = clients;
        }
        self.busy = busy;
        if self.text.is_some() || self.text_found.is_some() || self.analyzer().is_none() {
            return;
        }
        let (root, name) = (self.root.clone(), self.wanted.last.clone());
        // A path name keeps only the definitions its qualifier contains: `Foo::wired` is not `Other::wired`. A `.`
        // name (`state.tabs.get`) is a value's method: its qualifier names a field or a variable, not a container,
        // so every definition of the last segment stays. `Self`, `self`, `super` and `crate` name a place by where
        // the reference stands, which the text search does not know, so they keep every definition too.
        let within = self
            .wanted
            .path_qualifier()
            .filter(|qualifier| !["Self", "self", "super", "crate"].contains(qualifier))
            .map(str::to_owned);
        self.text = counted_worker(cx.workers, move || {
            let limits = eitri_core::symbol_text::Limits::default();
            let found = eitri_core::symbol_text::search_within(&root, &name, within.as_deref(), limits);
            let places = found
                .into_iter()
                .map(|found| (root.join(&found.path), found.line, found.col))
                .collect();
            place(&root, places, "text")
        });
    }

    fn take_text(&mut self) {
        let Some(text) = &self.text else { return };
        match text.try_recv() {
            Ok(found) => self.text_found = Some(found),
            Err(mpsc::TryRecvError::Disconnected) => self.text_found = Some(Vec::new()),
            Err(mpsc::TryRecvError::Empty) => return,
        }
        self.text = None;
    }

    fn answered_with(&mut self, phase: Phase, items: Vec<RawItem>, missing: Vec<String>, cx: &mut Cx<'_>) -> Flow {
        let root = self.root.clone();
        match phase {
            Phase::Cited => {
                if items.is_empty() {
                    return self.ask_workspace(cx);
                }
                let places = lsp_places(&items.iter().collect::<Vec<_>>());
                self.place_on_worker(phase, root, places, cx)
            }
            Phase::Workspace => {
                self.held = !missing.is_empty();
                self.answered |= missing.is_empty() || !items.is_empty();
                self.had_items |= !items.is_empty();
                let exact = exact(&items, &self.wanted, self.has(GOPLS));
                if exact.is_empty() {
                    self.inexact = !items.is_empty();
                    return self.no_exact(cx);
                }
                let places = lsp_places(&exact);
                self.place_on_worker(phase, root, places, cx)
            }
        }
    }

    /// Judges `places` on a worker. With the window's workers all out they are judged by none, which comes to what a
    /// worker that found nothing comes to.
    fn place_on_worker(
        &mut self,
        phase: Phase,
        root: PathBuf,
        places: Vec<(PathBuf, u32, u32)>,
        cx: &mut Cx<'_>,
    ) -> Flow {
        match counted_worker(cx.workers, move || place(&root, places, "lsp")) {
            Some(placed) => {
                self.stage = Stage::Placing(phase, placed);
                Flow::Wait
            }
            None => match phase {
                Phase::Cited => self.ask_workspace(cx),
                Phase::Workspace => {
                    self.inexact = true;
                    self.no_exact(cx)
                }
            },
        }
    }

    /// A symbol search without an exact match inside the project: rust-analyzer's text matches when it answered with
    /// other names (a qualified name waits for the end: for a `::` path its text matches are those whose enclosing
    /// `impl`, `trait` or module bears the qualifier's name, for a `.` name every definition of the last segment, a
    /// guess from the text that the server may still better), else the servers are asked again while time is left.
    fn no_exact(&mut self, cx: &mut Cx<'_>) -> Flow {
        self.take_text();
        if self.analyzer().is_some() && self.inexact && self.wanted.qualifier.is_none() {
            if let Some(found) = self.text_found.as_ref().filter(|found| !found.is_empty()) {
                return Flow::Reply(Ok(found.clone()));
            }
        }
        if cx.now + RETRY + LAST_LEAD < self.end() {
            self.stage = Stage::Resting(Phase::Workspace, cx.now + RETRY, Next::Ask);
            return Flow::Wait;
        }
        self.finish(cx.now)
    }

    /// The servers have had their time. A text search still out is waited for until the reply's last moment.
    fn finish(&mut self, now: Instant) -> Flow {
        self.take_text();
        if self.analyzer().is_some() && self.text.is_some() && now < self.end() + GRACE {
            self.stage = Stage::Texting;
            return Flow::Wait;
        }
        Flow::Reply(self.verdict())
    }

    /// The server a lookup reports as indexing: rust-analyzer when it is among those reporting work, else the first.
    fn busy_server(&self) -> Option<String> {
        if let Some(analyzer) = self
            .analyzer()
            .filter(|analyzer| self.busy.iter().any(|name| name == analyzer))
        {
            return Some(analyzer.to_owned());
        }
        self.busy.first().cloned()
    }

    /// What the lookup comes to with no exact match from a server: rust-analyzer's text matches; rust-analyzer still
    /// holding its answer, `indexing`; an answer without the name, `no_symbol` (but while a server reports work and
    /// no answer held any item, `indexing`: servers answer empty while they index); a server still reporting work,
    /// `indexing`; the lookup's last editor call failed out of reach of the editor or was refused over the link, its
    /// reason as `refused`; otherwise `timeout`.
    fn verdict(&self) -> Reply {
        if let Some(analyzer) = self.analyzer() {
            if let Some(found) = self.text_found.as_ref().filter(|found| !found.is_empty()) {
                return Ok(found.clone());
            }
            if self.held {
                return Err(error("indexing", Some(analyzer.to_owned())));
            }
        }
        if self.answered && (self.had_items || self.busy.is_empty()) {
            return Err(error("no_symbol", self.server()));
        }
        if let Some(busy) = self.busy_server() {
            return Err(error("indexing", Some(busy)));
        }
        if let Some(why) = &self.failed {
            return Err(refused(format!("gd: {why}")));
        }
        Err(error("timeout", self.server()))
    }
}

/// The window's lookups in flight, oldest first.
#[derive(Default)]
pub(super) struct SymbolLookup {
    lookups: Vec<Lookup>,
    sequencer: Option<Sequencer>,
    asks: u64,
    workers: Arc<AtomicUsize>,
}

impl SymbolLookup {
    /// Whether a lookup is out: the tick polls on its steady cadence until none is.
    pub(super) fn is_running(&self) -> bool {
        !self.lookups.is_empty()
    }
}

/// Starts `view`'s lookup of `name`, answered later by [`poll`]. A name the page let through is checked again here.
pub(super) fn start(
    state: &mut AgentPanelState,
    view: ViewId,
    request_id: String,
    name: &str,
    cited: Option<SymbolCitation>,
    tab: TabId,
) -> Result<(), EditorSymbolError> {
    let looked_up = wanted(name);
    if !eitri_core::agent_bridge::valid_symbol_name(name) || looked_up.last.is_empty() {
        return Err(refused(format!("gd: {name} is not a name gd can look up")));
    }
    if state.editor_way.is_none() && state.editor_rpc.is_none() {
        return Err(refused(format!("gd: {}", eitri_core::editor_tools::words::NO_EDITOR)));
    }
    if state.symbol_lookup.workers.load(Ordering::SeqCst) >= MAX_WORKERS {
        return Err(refused(
            "gd: file lookups are still running; try again in a moment".to_owned(),
        ));
    }
    if state.symbol_lookup.lookups.len() >= MAX_LOOKUPS {
        return Err(refused(format!(
            "gd: {MAX_LOOKUPS} lookups are still out; try again in a moment"
        )));
    }
    let mut lookup = Lookup::new(view, request_id, name, cited, state.project_dir.clone(), Instant::now());
    lookup.session = state
        .tabs
        .get(tab)
        .and_then(|tab| tab.provider_session_id())
        .unwrap_or_default();
    state.symbol_lookup.lookups.push(lookup);
    Ok(())
}

/// Steps every lookup and sends each reply that is ready to the page that asked.
pub(super) fn poll(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.symbol_lookup.lookups.is_empty() {
        return;
    }
    let way = state.editor_way.clone().or_else(|| {
        state
            .editor_rpc
            .clone()
            .map(|rpc| Rc::new(LocalWay(rpc)) as Rc<dyn EditorWay>)
    });
    let SymbolLookup {
        lookups,
        sequencer,
        asks,
        workers,
    } = &mut state.symbol_lookup;
    let mut cx = Cx {
        way: way.as_deref(),
        sequencer: sequencer
            .get_or_insert_with(|| Sequencer::new(format!("gd-{}-{:x}", std::process::id(), wall_ms()))),
        asks,
        workers,
        now: Instant::now(),
    };
    let mut index = 0;
    while index < lookups.len() {
        match lookups[index].poll(&mut cx) {
            Some(reply) => {
                let lookup = lookups.remove(index);
                let (matches, error) = match &reply {
                    Ok(matches) => (matches.as_slice(), None),
                    Err(error) => (&[][..], Some(error)),
                };
                out.send(
                    lookup.view,
                    eitri_core::agent_bridge::serialize_editor_symbol_result_for_js(&lookup.request_id, matches, error),
                );
            }
            None => index += 1,
        }
    }
}

/// Forgets what `view`'s page asked: a page that went idle or was reloaded numbers its requests afresh, and an answer
/// to the old document could match a request of the new.
pub(super) fn drop_view(state: &mut AgentPanelState, view: ViewId) {
    state.symbol_lookup.lookups.retain(|lookup| lookup.view != view);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use eitri_core::editor_rpc::EditorRpc;
    use eitri_core::editor_tools::testing::FakeEditor;
    use serde_json::json;
    use std::cell::RefCell;

    /// What the tests read of the fake editor.
    trait Sent {
        fn count(&self) -> usize;
        fn last_kind(&self) -> &'static str;
        fn last_arg(&self, key: &str) -> Option<serde_json::Value>;
        fn answer(&self, reply: serde_json::Value);
    }
    impl Sent for FakeEditor {
        fn count(&self) -> usize {
            self.asked().len()
        }
        fn last_kind(&self) -> &'static str {
            let code = *self.asked().last().expect("a chunk was sent");
            if code == lsp::LSP_ASK_LUA {
                "ask"
            } else if code == lsp::LSP_COLLECT_LUA {
                "collect"
            } else {
                "other"
            }
        }
        fn last_arg(&self, key: &str) -> Option<serde_json::Value> {
            self.args(self.count() - 1).get(key).cloned()
        }
        /// Answers the chunk sent last.
        fn answer(&self, reply: serde_json::Value) {
            self.answer_newest(&reply);
        }
    }

    fn started(clients: &[&str], busy: &[&str]) -> serde_json::Value {
        json!({"started": true, "unsupported": [], "clients": clients, "busy": busy})
    }
    fn done(part: &str, items: Vec<serde_json::Value>, missing: &[&str]) -> serde_json::Value {
        let total = items.len();
        json!({"status": "done", "parts": [{"name": part, "items": items, "total": total}], "missing": missing,
            "actions": [], "ticks": []})
    }
    fn item(path: &Path, line: u32, character: u32, name: Option<&str>, container: Option<&str>) -> serde_json::Value {
        let mut item = json!({"path": path.to_str().unwrap(), "line": line, "character": character});
        if let Some(name) = name {
            item["name"] = json!(name);
        }
        if let Some(container) = container {
            item["container"] = json!(container);
        }
        item
    }
    fn raw(name: &str, container: Option<&str>) -> RawItem {
        RawItem {
            path: "/p/a".into(),
            line: 1,
            character: 0,
            text: None,
            text_from: None,
            read_from: None,
            name: Some(name.into()),
            kind: None,
            container: container.map(str::to_owned),
        }
    }

    /// One lookup on a clock the test moves; its workers run in real time.
    struct Run {
        lookup: Lookup,
        rpc: Rc<FakeEditor>,
        sequencer: Sequencer,
        asks: u64,
        workers: Arc<AtomicUsize>,
        start: Instant,
        now: Instant,
        reply: Option<Reply>,
        /// The window has no editor link any more.
        editor_gone: bool,
        /// The editor is the other window's, reached over the link.
        linked: bool,
    }
    /// The fake editor as the other window's: the calls are the same, only the reach differs.
    struct OverLink(LocalWay);
    impl EditorWay for OverLink {
        fn call(&self, session: &str, call: &EditorCall, stamp: &eitri_core::editor_tools::stamp::Stamp) -> Pending {
            self.0.call(session, call, stamp)
        }
        fn reach(&self) -> EditorReach {
            EditorReach::Link
        }
    }
    impl Run {
        fn new(root: &Path, name: &str, cited: Option<(&str, u32)>) -> Run {
            let start = Instant::now();
            let cited = cited.map(|(path, line)| SymbolCitation {
                path: path.to_owned(),
                line,
            });
            Run {
                lookup: Lookup::new(ViewId::BASE, "gd1".into(), name, cited, root.to_path_buf(), start),
                rpc: Rc::default(),
                sequencer: Sequencer::new("gd-test".into()),
                asks: 0,
                workers: Arc::default(),
                start,
                now: start,
                reply: None,
                editor_gone: false,
                linked: false,
            }
        }
        fn poll(&mut self) {
            let local = LocalWay(self.rpc.clone());
            let linked = OverLink(LocalWay(self.rpc.clone()));
            let way: &dyn EditorWay = if self.linked { &linked } else { &local };
            let mut cx = Cx {
                way: (!self.editor_gone).then_some(way),
                sequencer: &mut self.sequencer,
                asks: &mut self.asks,
                workers: &self.workers,
                now: self.now,
            };
            if let Some(reply) = self.lookup.poll(&mut cx) {
                assert!(self.reply.is_none(), "one reply only");
                self.reply = Some(reply);
            }
        }
        /// Moves the clock to `ms` after the request and polls once.
        fn at(&mut self, ms: u64) {
            self.now = self.start + Duration::from_millis(ms);
            self.poll();
        }
        /// Polls with the clock standing until a chunk goes out or the reply comes: a worker takes real time.
        fn settle(&mut self) {
            let before = self.rpc.count();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.reply.is_none() && self.rpc.count() == before {
                assert!(Instant::now() < deadline, "nothing came of the lookup");
                self.poll();
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        /// Waits in real time for the text search to finish.
        fn text_settles(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.lookup.text.is_some() {
                assert!(Instant::now() < deadline, "the text search did not end");
                self.lookup.take_text();
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        /// Answers every collect pending until the budget's last one, which finds `last` (a server still out).
        /// Returns the milliseconds after the request at which the reply came.
        fn hold_to_the_end(&mut self, last: serde_json::Value) -> u64 {
            let mut ms = 0;
            while self.reply.is_none() {
                ms += 50;
                assert!(ms <= 4000, "no reply by the page's own deadline");
                self.at(ms);
                while self.rpc.unanswered() > 0 && self.rpc.last_kind() == "collect" {
                    let is_last = self.rpc.last_arg("last").and_then(|v| v.as_bool()) == Some(true);
                    self.rpc.answer(if is_last {
                        last.clone()
                    } else {
                        json!({"status": "pending"})
                    });
                    self.poll();
                }
                // Placing and the text search are workers.
                if self.reply.is_none() && matches!(self.lookup.stage, Stage::Placing(..) | Stage::Texting) {
                    self.settle();
                }
            }
            ms
        }
        fn matches(&self) -> Vec<(String, u32, &'static str)> {
            match self.reply.as_ref().expect("a reply") {
                Ok(found) => found.iter().map(|m| (m.path.clone(), m.line, m.source)).collect(),
                Err(error) => panic!("an error: {error:?}"),
            }
        }
        fn error(&self) -> (&'static str, Option<String>) {
            match self.reply.as_ref().expect("a reply") {
                Err(error) => (error.kind, error.server.clone()),
                Ok(found) => panic!("matches: {found:?}"),
            }
        }
        /// Answers the workspace ask with `clients` and its first collect with `items`.
        fn workspace_answers(&mut self, clients: &[&str], items: Vec<serde_json::Value>) {
            self.workspace_answers_busy(clients, &[], items);
        }
        /// [`Run::workspace_answers`] with `busy` of the clients reporting unfinished work.
        fn workspace_answers_busy(&mut self, clients: &[&str], busy: &[&str], items: Vec<serde_json::Value>) {
            assert_eq!(self.rpc.last_kind(), "ask");
            assert_eq!(self.rpc.last_arg("kind").unwrap().as_str(), Some("workspace_symbols"));
            self.rpc.answer(started(clients, busy));
            let ms = self.now.duration_since(self.start).as_millis() as u64;
            self.at(ms);
            self.at(ms + 100);
            assert_eq!(self.rpc.last_kind(), "collect");
            self.rpc.answer(done("workspace_symbols", items, &[]));
            self.at(ms + 100);
        }
    }

    fn project(label: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = agent::state_dirs::test_workspace_dir(label);
        for (name, text) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    #[test]
    fn a_name_is_worded_for_each_server() {
        for (name, query, gopls, analyzer) in [
            (
                "Server.RegisterPreview",
                "RegisterPreview",
                "Server.RegisterPreview",
                "RegisterPreview#",
            ),
            ("Type::name", "name", "name", "name#"),
            ("M.call", "call", "M.call", "call#"),
            ("effective", "effective", "effective", "effective#"),
        ] {
            let (asked, worded) = queries(&wanted(name));
            assert_eq!(
                asked, query,
                "{name}: lua_ls, vtsls, pyright and the rest get the last segment"
            );
            assert_eq!(
                worded,
                [
                    (GOPLS.to_owned(), gopls.to_owned()),
                    (RUST_ANALYZER.to_owned(), analyzer.to_owned()),
                    (RUST_ANALYZER_LSPCONFIG.to_owned(), analyzer.to_owned())
                ],
                "{name}"
            );
        }
        assert_eq!(wanted("pkg.Type.Method").qualifier.as_deref(), Some("Type"));
        assert_eq!(wanted("a::b::c").qualifier.as_deref(), Some("b"));
        assert_eq!(wanted("plain").qualifier, None);
        assert_eq!(wanted("::").last, "");
    }

    #[test]
    fn result_names_are_normalised_per_server() {
        let names = |items: &[RawItem], name: &str, gopls: bool| -> Vec<String> {
            exact(items, &wanted(name), gopls)
                .iter()
                .map(|item| {
                    format!(
                        "{}@{}",
                        item.name.as_deref().unwrap(),
                        item.container.as_deref().unwrap_or("")
                    )
                })
                .collect()
        };
        let vtsls = [raw("resolveKey()", None), raw("resolveKeys()", None)];
        assert_eq!(names(&vtsls, "resolveKey", false), ["resolveKey()@"]);

        let gopls = [raw("Server.RegisterPreview", None), raw("Client.RegisterPreview", None)];
        assert_eq!(
            names(&gopls, "RegisterPreview", true),
            ["Server.RegisterPreview@", "Client.RegisterPreview@"]
        );
        assert_eq!(
            names(&gopls, "Server.RegisterPreview", true),
            ["Server.RegisterPreview@"]
        );
        assert!(
            names(&gopls, "RegisterPreview", false).is_empty(),
            "a dotted name is only a method's form when gopls is attached"
        );

        let lua = [raw("call", Some("M")), raw("call", Some("N")), raw("caller", Some("M"))];
        assert_eq!(names(&lua, "M.call", false), ["call@M"]);
        assert_eq!(names(&lua, "call", false), ["call@M", "call@N"]);
        assert!(
            names(&lua, "Q.call", false).is_empty(),
            "a qualifier no container has filters all out"
        );
        let bare = [raw("call", None), raw("call", None)];
        assert_eq!(
            names(&bare, "M.call", false).len(),
            2,
            "with no container on any item, all stay"
        );
        let go_package = [raw("Connect", Some("git.example/grey/app/web"))];
        assert_eq!(
            names(&go_package, "web.Connect", true).len(),
            1,
            "a package path's last segment"
        );
    }

    #[test]
    fn a_cited_line_that_holds_the_name_asks_for_the_definition_there() {
        let root = project(
            "gd-cited",
            &[("a.go", "package a\n\nfunc Foo() {}\n"), ("b.go", "package a\n")],
        );
        let mut run = Run::new(&root, "Foo", Some(("a.go", 3)));
        run.settle();
        assert_eq!(run.rpc.last_kind(), "ask");
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("usages"));
        assert_eq!(run.rpc.last_arg("path").unwrap().as_str(), root.join("a.go").to_str());
        assert_eq!(run.rpc.last_arg("line").unwrap().as_u64(), Some(3));
        assert_eq!(run.rpc.last_arg("symbol").unwrap().as_str(), Some("Foo"));
        assert_eq!(
            run.rpc.last_arg("load").unwrap().as_bool(),
            Some(false),
            "gd never loads a buffer"
        );
        let include = run.rpc.last_arg("include").unwrap();
        assert_eq!(include, json!(["definitions"]));
        run.rpc.answer(started(&["gopls"], &[]));
        run.at(0);
        run.at(100);
        assert_eq!(run.rpc.last_kind(), "collect");
        run.rpc.answer(done(
            "definitions",
            vec![item(&root.join("b.go"), 1, 5, None, None)],
            &[],
        ));
        run.at(100);
        run.settle();
        assert_eq!(run.matches(), [("b.go".to_owned(), 1, "lsp")]);
        assert_eq!(run.rpc.count(), 2, "the definition was the answer: no symbol search");
    }

    #[test]
    fn a_cited_line_without_the_name_goes_to_the_symbol_search() {
        let root = project("gd-cited-not", &[("a.go", "package a\n\nfunc Bar() {}\n")]);
        let mut run = Run::new(&root, "Foo", Some(("a.go", 3)));
        run.settle();
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("workspace_symbols"));
    }

    #[test]
    fn a_cited_file_not_loaded_falls_through_to_the_symbol_search() {
        let root = project("gd-needs-load", &[("a.go", "func Foo() {}\n")]);
        let mut run = Run::new(&root, "pkg.Foo", Some(("a.go", 1)));
        run.settle();
        assert_eq!(
            run.rpc.last_arg("symbol").unwrap().as_str(),
            Some("Foo"),
            "the last segment"
        );
        run.rpc.answer(json!({"needs_load": true}));
        run.at(0);
        assert_eq!(run.rpc.count(), 2);
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("workspace_symbols"));
        assert_eq!(run.rpc.last_arg("query").unwrap().as_str(), Some("Foo"));
        assert_eq!(run.rpc.last_arg("load").unwrap().as_bool(), Some(false));
        let per = run.rpc.last_arg("query_for").unwrap();
        assert_eq!(
            per,
            json!({"gopls": "pkg.Foo", "rust-analyzer": "Foo#", "rust_analyzer": "Foo#"})
        );
    }

    #[test]
    fn a_fifo_or_a_path_out_of_the_project_is_no_cited_position() {
        let outside = project("gd-outside", &[("o.go", "func Foo() {}\n")]);
        let root = project("gd-fifo", &[]);
        let made = std::process::Command::new("mkfifo")
            .arg(root.join("f.go"))
            .status()
            .unwrap();
        assert!(made.success());
        std::os::unix::fs::symlink(outside.join("o.go"), root.join("out.go")).unwrap();
        for cited in ["f.go", "out.go", outside.join("o.go").to_str().unwrap()] {
            let mut run = Run::new(&root, "Foo", Some((cited, 1)));
            // A FIFO opened to read would hold this worker until a writer came: settling proves it was not.
            run.settle();
            assert_eq!(run.rpc.count(), 1, "{cited}");
            assert_eq!(
                run.rpc.last_arg("kind").unwrap().as_str(),
                Some("workspace_symbols"),
                "{cited}"
            );
        }
    }

    #[test]
    fn an_empty_answer_is_asked_again_after_300_ms() {
        let root = project("gd-retry", &[("a.lua", "function M.call() end\n")]);
        let mut run = Run::new(&root, "call", None);
        run.settle();
        run.workspace_answers(&["lua_ls"], vec![]);
        assert!(run.reply.is_none(), "an empty answer is not 'no symbol'");
        run.at(399);
        assert_eq!(run.rpc.count(), 2, "nothing asked before 300 ms");
        run.at(400);
        assert_eq!(run.rpc.count(), 3);
        run.workspace_answers(
            &["lua_ls"],
            vec![item(&root.join("a.lua"), 1, 11, Some("call"), Some("M"))],
        );
        run.settle();
        assert_eq!(run.matches(), [("a.lua".to_owned(), 1, "lsp")]);
    }

    #[test]
    fn an_answer_without_the_name_by_the_end_is_no_symbol_from_that_server() {
        let root = project("gd-none", &[("a.go", "x\n")]);
        let mut run = Run::new(&root, "Missing", None);
        run.settle();
        run.workspace_answers(
            &["gopls"],
            vec![item(&root.join("a.go"), 1, 0, Some("MissingNot"), None)],
        );
        let mut ms = 100;
        while run.reply.is_none() {
            ms += 50;
            assert!(ms < 4000);
            run.at(ms);
            if run.rpc.unanswered() > 0 && run.rpc.last_kind() == "ask" {
                run.workspace_answers(&["gopls"], vec![]);
            }
        }
        assert_eq!(run.error(), ("no_symbol", Some("gopls".to_owned())));
        assert!(ms <= 3100, "it ends with the budget: {ms}");
    }

    #[test]
    fn no_client_attached_says_so() {
        let root = project("gd-no-client", &[]);
        let mut run = Run::new(&root, "Foo", None);
        run.settle();
        run.rpc.answer(json!({"no_client": true, "loaded_now": false}));
        run.at(0);
        assert_eq!(run.error(), ("no_client", None));
    }

    #[test]
    fn rust_analyzer_held_past_the_budget_offers_its_text_matches() {
        let root = project(
            "gd-ra-text",
            &[
                ("src/lib.rs", "mod a;\n"),
                ("src/a.rs", "pub(crate) fn effective() {}\n"),
            ],
        );
        let mut run = Run::new(&root, "effective", None);
        run.settle();
        run.rpc.answer(started(&["rust-analyzer"], &["rust-analyzer"]));
        run.at(0);
        run.text_settles();
        run.hold_to_the_end(done("workspace_symbols", vec![], &["workspace_symbols"]));
        assert_eq!(run.matches(), [("src/a.rs".to_owned(), 1, "text")]);
    }

    #[test]
    fn rust_analyzer_held_with_no_text_match_is_still_indexing() {
        let root = project("gd-ra-indexing", &[("src/lib.rs", "fn other() {}\n")]);
        let mut run = Run::new(&root, "effective", None);
        run.settle();
        run.rpc.answer(started(&["rust-analyzer"], &[]));
        run.at(0);
        run.text_settles();
        run.hold_to_the_end(done("workspace_symbols", vec![], &["workspace_symbols"]));
        assert_eq!(run.error(), ("indexing", Some(RUST_ANALYZER.to_owned())));
    }

    #[test]
    fn rust_analyzer_answering_other_names_offers_the_text_matches_at_once() {
        let root = project("gd-ra-inexact", &[("src/lib.rs", "fn wire() {}\n")]);
        let mut run = Run::new(&root, "wire", None);
        run.settle();
        run.rpc.answer(started(&["rust-analyzer"], &[]));
        run.at(0);
        run.text_settles();
        run.at(100);
        run.rpc.answer(done(
            "workspace_symbols",
            vec![item(&root.join("src/lib.rs"), 9, 0, Some("wired"), None)],
            &[],
        ));
        run.at(100);
        assert_eq!(run.matches(), [("src/lib.rs".to_owned(), 1, "text")]);
    }

    #[test]
    fn rust_analyzer_under_lspconfigs_name_is_rust_analyzer() {
        for name in ["rust_analyzer", "rust-analyzer", "Rust_Analyzer"] {
            assert!(is_rust_analyzer(name), "{name}");
        }
        assert!(!is_rust_analyzer("rust") && !is_rust_analyzer("rust-analyzer-proxy"));
        // Held past the budget: its text matches, as for the hyphenated name.
        let root = project(
            "gd-ra-underscore",
            &[
                ("src/lib.rs", "mod a;\n"),
                ("src/a.rs", "pub(crate) fn effective() {}\n"),
            ],
        );
        let mut run = Run::new(&root, "effective", None);
        run.settle();
        run.rpc.answer(started(&["rust_analyzer"], &["rust_analyzer"]));
        run.at(0);
        run.text_settles();
        run.hold_to_the_end(done("workspace_symbols", vec![], &["workspace_symbols"]));
        assert_eq!(run.matches(), [("src/a.rs".to_owned(), 1, "text")]);
    }

    #[test]
    fn rust_analyzer_under_lspconfigs_name_held_with_no_text_match_is_indexing_by_its_own_name() {
        let root = project("gd-ra-underscore-indexing", &[("src/lib.rs", "fn other() {}\n")]);
        let mut run = Run::new(&root, "effective", None);
        run.settle();
        run.rpc.answer(started(&["rust_analyzer"], &[]));
        run.at(0);
        run.text_settles();
        run.hold_to_the_end(done("workspace_symbols", vec![], &["workspace_symbols"]));
        assert_eq!(run.error(), ("indexing", Some("rust_analyzer".to_owned())));
    }

    #[test]
    fn another_server_held_past_the_budget_is_a_timeout() {
        let root = project("gd-held", &[("src/lib.rs", "fn effective() {}\n")]);
        let mut run = Run::new(&root, "effective", None);
        run.settle();
        run.rpc.answer(started(&["pyright"], &[]));
        run.at(0);
        run.hold_to_the_end(done("workspace_symbols", vec![], &["workspace_symbols"]));
        assert_eq!(run.error(), ("timeout", Some("pyright".to_owned())));
        assert!(
            run.lookup.text.is_none() && run.lookup.text_found.is_none(),
            "no fallback but rust-analyzer's"
        );
    }

    #[test]
    fn a_held_lookup_replies_within_the_budget_and_its_grace() {
        let root = project("gd-held-clock", &[("src/lib.rs", "fn effective() {}\n")]);
        let mut run = Run::new(&root, "effective", None);
        run.settle();
        run.rpc.answer(started(&["pyright"], &[]));
        run.at(0);
        let ms = run.hold_to_the_end(done("workspace_symbols", vec![], &["workspace_symbols"]));
        // The loop steps the injected clock by 50 ms.
        let latest = (BUDGET + GRACE).as_millis() as u64 + 50;
        assert!(ms <= latest, "replied {ms} ms after the request, past {latest}");
    }

    #[test]
    fn a_server_reporting_work_that_answers_empty_to_the_end_is_indexing_not_no_symbol() {
        let root = project("gd-busy-empty", &[("a.ts", "x\n")]);
        let mut run = Run::new(&root, "Missing", None);
        run.settle();
        run.workspace_answers_busy(&["vtsls"], &["vtsls"], vec![]);
        let mut ms = 100;
        while run.reply.is_none() {
            ms += 50;
            assert!(ms < 4000, "no reply by the page's own deadline");
            run.at(ms);
            if run.rpc.unanswered() > 0 && run.rpc.last_kind() == "ask" {
                run.workspace_answers_busy(&["vtsls"], &["vtsls"], vec![]);
            }
        }
        assert_eq!(run.error(), ("indexing", Some("vtsls".to_owned())));
    }

    #[test]
    fn an_answer_with_other_names_stays_no_symbol_while_a_server_reports_work() {
        let root = project("gd-busy-items", &[("a.go", "x\n")]);
        let mut run = Run::new(&root, "Missing", None);
        run.settle();
        let other = vec![item(&root.join("a.go"), 1, 0, Some("MissingNot"), None)];
        run.workspace_answers_busy(&["gopls"], &["gopls"], other);
        let mut ms = 100;
        while run.reply.is_none() {
            ms += 50;
            assert!(ms < 4000);
            run.at(ms);
            if run.rpc.unanswered() > 0 && run.rpc.last_kind() == "ask" {
                run.workspace_answers_busy(&["gopls"], &["gopls"], vec![]);
            }
        }
        assert_eq!(run.error(), ("no_symbol", Some("gopls".to_owned())));
    }

    #[test]
    fn an_ask_nvim_never_answers_ends_with_the_budget() {
        let root = project("gd-ask-unanswered", &[]);
        let mut run = Run::new(&root, "Foo", None);
        run.settle();
        run.at(2999);
        assert!(run.reply.is_none());
        run.at(3000);
        assert_eq!(run.error(), ("timeout", None));
        assert_eq!(run.rpc.last_kind(), "collect", "the ask given up is released");
        assert_eq!(run.rpc.last_arg("last").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(run.rpc.last_arg("id"), Some(json!("gd-1")));
    }

    #[test]
    fn an_editor_call_that_fails_ends_the_lookup_at_once() {
        let root = project("gd-call-fails", &[]);
        let mut run = Run::new(&root, "Foo", None);
        run.settle();
        run.rpc.answer(started(&["gopls"], &[]));
        run.at(0);
        run.at(100);
        assert_eq!(run.rpc.last_kind(), "collect");
        run.rpc.fail_newest();
        run.at(100);
        assert_eq!(run.error(), ("refused", None));
        let Some(Err(error)) = &run.reply else {
            panic!("an error")
        };
        assert_eq!(error.message.as_deref(), Some("gd: the connection to nvim closed"));
    }

    /// The symbol search's ask answered with gopls, then its collect failed with `error`.
    fn collect_fails(label: &str, linked: bool, error: RpcError) -> Run {
        let root = project(label, &[]);
        let mut run = Run::new(&root, "Foo", None);
        run.linked = linked;
        run.settle();
        run.rpc.answer(started(&["gopls"], &[]));
        run.at(0);
        run.at(100);
        assert_eq!(run.rpc.last_kind(), "collect");
        run.rpc.fail_newest_with(error);
        run.at(100);
        run
    }

    fn refusal(run: &Run) -> String {
        assert_eq!(run.error(), ("refused", None));
        let Some(Err(error)) = &run.reply else {
            panic!("an error")
        };
        error.message.clone().expect("a refusal says why")
    }

    #[test]
    fn a_lua_error_in_this_windows_editor_ends_as_a_timeout() {
        let traceback = "E5108: Error executing lua: [string \"lsp\"]:12: attempt to index a nil value\n\
                         stack traceback:\n\t[C]: in function 'error'";
        let run = collect_fails("gd-lua-error", false, RpcError::Nvim(traceback.into()));
        assert_eq!(run.error(), ("timeout", Some("gopls".to_owned())));
    }

    #[test]
    fn an_editor_out_of_reach_is_refused_on_one_bounded_line() {
        let long = format!("{}\nsecond line", "w".repeat(REASON_MAX * 2));
        let run = collect_fails("gd-unreachable", false, RpcError::Unavailable(long));
        let message = refusal(&run);
        assert!(!message.contains('\n'), "{message}");
        assert!(message.starts_with("gd: no editor to ask: www"), "{message}");
        assert_eq!(message.chars().count(), "gd: ".len() + REASON_MAX + 1, "{message}");
        assert!(message.ends_with('…'));
    }

    #[test]
    fn window_1s_refusal_over_the_link_is_named_on_its_first_line() {
        let run = collect_fails(
            "gd-link-refused",
            true,
            RpcError::Nvim("session s1 is not held by window 2\nmore".into()),
        );
        assert_eq!(refusal(&run), "gd: session s1 is not held by window 2");
    }

    #[test]
    fn a_failed_cited_ask_does_not_speak_for_a_symbol_search_never_answered() {
        let root = project("gd-cited-fails", &[("a.go", "func Foo() {}\n")]);
        let mut run = Run::new(&root, "Foo", Some(("a.go", 1)));
        run.settle();
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("usages"));
        run.rpc.fail_newest();
        run.at(0);
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("workspace_symbols"));
        run.at(2999);
        assert!(run.reply.is_none());
        run.at(3000);
        assert_eq!(run.error(), ("timeout", None));
    }

    #[test]
    fn an_editor_gone_between_polls_ends_the_lookup_at_once() {
        let root = project("gd-editor-gone", &[]);
        let mut run = Run::new(&root, "Foo", None);
        run.settle();
        run.rpc.answer(started(&["gopls"], &[]));
        run.at(0);
        run.editor_gone = true;
        run.at(100);
        assert_eq!(run.error(), ("timeout", Some("gopls".to_owned())));
    }

    #[test]
    fn no_file_worker_is_started_when_the_window_has_all_of_its_workers_out() {
        let root = project(
            "gd-workers-cap",
            &[("src/lib.rs", "fn effective() {}\n"), ("a.go", "func Foo() {}\n")],
        );
        // The cited step is skipped: the symbol search goes ahead at once.
        let mut run = Run::new(&root, "Foo", Some(("a.go", 1)));
        run.workers.store(MAX_WORKERS, Ordering::SeqCst);
        run.at(0);
        assert_eq!(run.rpc.count(), 1);
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("workspace_symbols"));
        // No text search for rust-analyzer, and the places an answer holds are judged by none.
        let mut run = Run::new(&root, "effective", None);
        run.workers.store(MAX_WORKERS, Ordering::SeqCst);
        run.at(0);
        run.workspace_answers(
            &["rust-analyzer"],
            vec![item(&root.join("src/lib.rs"), 1, 3, Some("effective"), None)],
        );
        assert!(run.lookup.text.is_none() && run.lookup.text_found.is_none());
        assert!(matches!(
            run.lookup.stage,
            Stage::Resting(Phase::Workspace, _, Next::Ask)
        ));
        assert_eq!(run.workers.load(Ordering::SeqCst), MAX_WORKERS, "nothing was counted");
        // One below the cap, a worker is counted while it runs and not after.
        let mut run = Run::new(&root, "Foo", Some(("a.go", 1)));
        run.workers.store(MAX_WORKERS - 1, Ordering::SeqCst);
        run.settle();
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("usages"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while run.workers.load(Ordering::SeqCst) != MAX_WORKERS - 1 {
            assert!(Instant::now() < deadline, "the worker was never uncounted");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_lookup_is_refused_while_the_window_has_all_of_its_workers_out() {
        let (handle, base, _) = handle_with_two_pages();
        let editor = Rc::new(eitri_core::editor_tools::testing::FakeEditor::default());
        let tab = {
            let mut state = handle.state.borrow_mut();
            state.project_dir = agent::state_dirs::test_workspace_dir("gd-workers-refused");
            state.editor_rpc = Some(editor.clone());
            state.symbol_lookup.workers.store(MAX_WORKERS, Ordering::SeqCst);
            state.tabs.active()
        };
        handle.page_message(ViewId::BASE, &symbol_message("g1", tab, "Foo"));
        let replies = symbol_replies(&base);
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0]["error"]["kind"], "refused");
        assert!(!handle.state.borrow().symbol_lookup.is_running());
        assert_eq!(editor.asked().len(), 0);
    }

    #[test]
    fn the_cited_ask_lives_for_its_share_not_the_whole_lookup() {
        let root = project("gd-cited-life", &[("a.go", "func Foo() {}\n")]);
        let mut run = Run::new(&root, "Foo", Some(("a.go", 1)));
        run.settle();
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("usages"));
        let life = run.rpc.last_arg("budget_ms").unwrap().as_u64().unwrap();
        assert!(
            life <= (CITED_SHARE + Duration::from_secs(1)).as_millis() as u64,
            "{life}"
        );
    }

    #[test]
    fn the_cited_step_giving_way_releases_its_ask() {
        let root = project("gd-cited-release", &[("a.go", "func Foo() {}\n")]);
        let mut run = Run::new(&root, "Foo", Some(("a.go", 1)));
        run.settle();
        run.rpc.answer(started(&["gopls"], &[]));
        run.at(0);
        run.at(100);
        assert_eq!(run.rpc.last_kind(), "collect");
        run.at(1500);
        // The abandoned ask's last collect, then the symbol search.
        let sent = run.rpc.count();
        assert_eq!(sent, 4);
        assert_eq!(run.rpc.args(sent - 2).get("last"), Some(&json!(true)));
        assert_eq!(run.rpc.args(sent - 2).get("id"), Some(&json!("gd-1")));
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("workspace_symbols"));
    }

    #[test]
    fn a_qualified_name_does_not_take_the_text_matches_early() {
        let root = project(
            "gd-qualified-text",
            &[("src/lib.rs", "impl Foo {\n    fn wired() {}\n}\n")],
        );
        let mut run = Run::new(&root, "Foo::wired", None);
        run.settle();
        run.rpc.answer(started(&["rust-analyzer"], &[]));
        run.at(0);
        run.text_settles();
        run.at(100);
        run.rpc.answer(done(
            "workspace_symbols",
            vec![item(&root.join("src/lib.rs"), 2, 7, Some("wired"), Some("Other"))],
            &[],
        ));
        run.at(100);
        assert!(run.reply.is_none(), "every `wired` is not what `Foo::wired` asked for");
        run.at(3500);
        assert_eq!(run.matches(), [("src/lib.rs".to_owned(), 2, "text")]);
    }

    #[test]
    fn a_qualifier_naming_where_the_reference_stands_keeps_every_text_match() {
        for (name, file, line) in [
            ("Self::wired", "impl Foo {\n    fn wired() {}\n}\n", 2),
            ("crate::wired", "pub fn wired() {}\n", 1),
            ("self::wired", "pub fn wired() {}\n", 1),
            ("super::wired", "mod inner {}\nfn wired() {}\n", 2),
        ] {
            let root = project("gd-qualified-relative", &[("src/lib.rs", file)]);
            let mut run = Run::new(&root, name, None);
            run.settle();
            run.rpc.answer(started(&["rust-analyzer"], &[]));
            run.at(0);
            run.text_settles();
            run.at(100);
            run.rpc.answer(done(
                "workspace_symbols",
                vec![item(&root.join("src/lib.rs"), 1, 1, Some("wired"), Some("Other"))],
                &[],
            ));
            run.at(100);
            assert!(run.reply.is_none(), "{name}: a qualified name waits for the end");
            run.at(3500);
            assert_eq!(run.matches(), [("src/lib.rs".to_owned(), line, "text")], "{name}");
        }
    }

    #[test]
    fn a_value_method_name_keeps_every_text_match() {
        let root = project(
            "gd-value-method",
            &[(
                "src/lib.rs",
                "impl A {\n    fn get() {}\n}\nimpl B {\n    fn get() {}\n}\n",
            )],
        );
        for name in ["state.tabs.get", "self.tabs.get", "tabs.get"] {
            let mut run = Run::new(&root, name, None);
            run.settle();
            run.rpc.answer(started(&["rust-analyzer"], &[]));
            run.at(0);
            run.text_settles();
            run.at(100);
            run.rpc.answer(done(
                "workspace_symbols",
                vec![item(&root.join("src/lib.rs"), 2, 7, Some("get"), Some("Other"))],
                &[],
            ));
            run.at(100);
            assert!(run.reply.is_none(), "{name}: a qualified name waits for the end");
            run.at(3500);
            assert_eq!(
                run.matches(),
                [
                    ("src/lib.rs".to_owned(), 2, "text"),
                    ("src/lib.rs".to_owned(), 5, "text")
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn a_qualified_name_takes_no_text_match_of_another_container() {
        let root = project(
            "gd-qualified-other",
            &[("src/lib.rs", "impl Other {\n    fn wired() {}\n}\n")],
        );
        let mut run = Run::new(&root, "Foo::wired", None);
        run.settle();
        run.rpc.answer(started(&["rust-analyzer"], &[]));
        run.at(0);
        run.text_settles();
        run.at(100);
        run.rpc.answer(done(
            "workspace_symbols",
            vec![item(&root.join("src/lib.rs"), 2, 7, Some("wired"), Some("Other"))],
            &[],
        ));
        run.at(100);
        assert!(run.reply.is_none());
        run.at(3500);
        assert_eq!(run.error(), ("no_symbol", Some(RUST_ANALYZER.to_owned())));
    }

    #[test]
    fn a_workspace_with_no_client_after_the_cited_step_found_one_is_no_client() {
        let root = project("gd-no-client-after-cited", &[("a.go", "func Foo() {}\n")]);
        let mut run = Run::new(&root, "Foo", Some(("a.go", 1)));
        run.settle();
        run.rpc.answer(started(&["gopls"], &[]));
        run.at(0);
        run.at(100);
        run.at(1500);
        assert_eq!(run.rpc.last_arg("kind").unwrap().as_str(), Some("workspace_symbols"));
        run.rpc.answer(json!({"no_client": true, "loaded_now": false}));
        run.at(1500);
        assert_eq!(run.error(), ("no_client", None));
    }

    #[test]
    fn only_places_inside_the_project_are_offered() {
        let registry = project("gd-registry", &[("serde/src/lib.rs", "pub trait Serialize {}\n")]);
        let root = project("gd-bounds", &[("src/lib.rs", "pub trait Serialize {}\n")]);
        std::os::unix::fs::symlink(registry.join("serde/src/lib.rs"), root.join("src/linked.rs")).unwrap();
        let mut run = Run::new(&root, "Serialize", None);
        run.settle();
        run.workspace_answers(
            &["rust-analyzer"],
            vec![
                item(&registry.join("serde/src/lib.rs"), 1, 10, Some("Serialize"), None),
                item(&root.join("src/linked.rs"), 1, 10, Some("Serialize"), None),
                item(&root.join("src/lib.rs"), 1, 10, Some("Serialize"), None),
                item(&root.join("src/lib.rs"), 1, 10, Some("Serialize"), None),
            ],
        );
        run.settle();
        assert_eq!(run.matches(), [("src/lib.rs".to_owned(), 1, "lsp")]);
    }

    /// `gd` against real language servers in a headless nvim, timed, on two small fixture projects (no user config:
    /// `nvim --clean`, the server started with only its command and root). Run with the servers' paths:
    ///
    /// ```sh
    /// EITRI_GD_E2E_GOPLS=$HOME/go/bin/gopls \
    /// EITRI_GD_E2E_RUST_ANALYZER=$HOME/.local/share/nvim/mason/bin/rust-analyzer \
    ///   cargo test -p eitri-panel -- --ignored gd_against_real_servers --nocapture
    /// ```
    ///
    /// Either variable may be left out to skip that server. It prints one line per lookup and asserts only what holds
    /// the warm answers' places. Elapsed durations are printed for diagnosis; waits use hang guards (the budget and its
    /// grace themselves are held by the injected-clock tests above).
    ///
    /// The real-nvim gate (`tools/real-nvim-gate.sh`) leaves this and the next test out on purpose: they need the
    /// variables, so they are run by name.
    #[test]
    #[ignore = "run by name with EITRI_GD_E2E_GOPLS and/or EITRI_GD_E2E_RUST_ANALYZER set (see the doc comment); drives a headless Neovim and the real servers; spends no tokens and needs no display"]
    fn gd_against_real_servers() {
        let gopls = std::env::var_os("EITRI_GD_E2E_GOPLS").map(PathBuf::from);
        let analyzer = std::env::var_os("EITRI_GD_E2E_RUST_ANALYZER").map(PathBuf::from);
        assert!(
            gopls.is_some() || analyzer.is_some(),
            "set EITRI_GD_E2E_GOPLS and/or EITRI_GD_E2E_RUST_ANALYZER"
        );
        if let Some(gopls) = gopls {
            let root = project(
                "gd-e2e-go",
                &[
                    ("go.mod", "module example.com/fix\n\ngo 1.27\n"),
                    (
                        "server.go",
                        "package fix\n\ntype Server struct{}\n\nfunc (s *Server) RegisterPreview() int { return 1 }\n\nfunc NewServer() *Server { return &Server{} }\n",
                    ),
                    (
                        "fix_test.go",
                        "package fix\n\nimport \"testing\"\n\nfunc TestMeshFieldsOmittedWhenZero(t *testing.T) {\n\tif NewServer().RegisterPreview() != 1 {\n\t\tt.Fatal(\"no\")\n\t}\n}\n",
                    ),
                ],
            );
            let editor = RealEditor::start(&root, "server.go", "go", "gopls", &gopls, false);
            let cold = editor.look(&root, "RegisterPreview", None);
            editor.print("gopls", "cold", "RegisterPreview", &cold);
            for (name, cited, want) in [
                ("RegisterPreview", None, Some(("server.go", 5))),
                ("Server.RegisterPreview", None, Some(("server.go", 5))),
                ("TestMeshFieldsOmittedWhenZero", None, Some(("fix_test.go", 5))),
                // fix_test.go is not loaded: gd does not load it, and the symbol search answers.
                ("NewServer", Some(("fix_test.go", 6)), Some(("server.go", 7))),
                ("Server", Some(("server.go", 7)), Some(("server.go", 3))),
                ("NoSuchName", None, None),
            ] {
                let got = editor.look(&root, name, cited);
                editor.print("gopls", "warm", name, &got);
                match want {
                    Some((path, line)) => assert!(
                        matches!(&got.1, Ok(found) if found.iter().any(|m| m.path == path && m.line == line)),
                        "{name}: {got:?}"
                    ),
                    None => assert!(matches!(&got.1, Err(e) if e.kind == "no_symbol"), "{name}: {got:?}"),
                }
            }
            editor.stop();
        }
        if let Some(analyzer) = analyzer {
            let root = project(
                "gd-e2e-rs",
                &[
                    ("Cargo.toml", "[package]\nname = \"fix\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
                    (
                        "src/lib.rs",
                        "pub mod a;\n\npub struct PanelKeymap;\n\nimpl PanelKeymap {\n    pub fn effective(&self) -> u8 {\n        1\n    }\n}\n\npub fn wire() -> u8 {\n    a::effective()\n}\n\nmacro_rules! chunk {\n    () => {};\n}\n\nchunk!();\n",
                    ),
                    ("src/a.rs", "pub(crate) fn effective() -> u8 {\n    2\n}\n"),
                ],
            );
            let editor = RealEditor::start(&root, "src/lib.rs", "rust", "rust-analyzer", &analyzer, false);
            for name in ["effective", "PanelKeymap"] {
                let cold = editor.look(&root, name, None);
                editor.print("rust-analyzer", "cold", name, &cold);
            }
            // Warm: until rust-analyzer itself answers a type, then the rest.
            let since = Instant::now();
            loop {
                let got = editor.look(&root, "PanelKeymap", None);
                if matches!(&got.1, Ok(found) if found.iter().any(|m| m.source == "lsp")) {
                    editor.print("rust-analyzer", "loaded", "PanelKeymap", &got);
                    eprintln!(
                        "gd-e2e rust-analyzer: answered by the server {} ms after the attach",
                        since.elapsed().as_millis()
                    );
                    break;
                }
                assert!(
                    since.elapsed() < Duration::from_secs(90),
                    "rust-analyzer never answered: {got:?}"
                );
            }
            for (name, cited, want) in [
                ("PanelKeymap", None, Some(("src/lib.rs", 3))),
                ("effective", None, Some(("src/a.rs", 1))),
                ("effective", Some(("src/a.rs", 1)), Some(("src/a.rs", 1))),
                ("PanelKeymap::effective", None, Some(("src/lib.rs", 6))),
                ("wire", None, Some(("src/lib.rs", 11))),
                ("chunk", None, Some(("src/lib.rs", 15))),
                ("NoSuchName", None, None),
            ] {
                let got = editor.look(&root, name, cited);
                editor.print("rust-analyzer", "warm", name, &got);
                match want {
                    Some((path, line)) => assert!(
                        matches!(&got.1, Ok(found) if found.iter().any(|m| m.path == path && m.line == line)),
                        "{name}: {got:?}"
                    ),
                    None => assert!(matches!(&got.1, Err(_)), "{name}: {got:?}"),
                }
            }
            // Cited on a line of the loaded file: the definition there is the one answer, where the symbol search
            // gives both functions of the name.
            let got = editor.look(&root, "effective", Some(("src/lib.rs", 12)));
            editor.print("rust-analyzer", "cited", "effective", &got);
            assert!(
                matches!(&got.1, Ok(found) if found.len() == 1 && found[0].path == "src/a.rs" && found[0].line == 1),
                "{got:?}"
            );
            editor.stop();
        }
    }

    /// rust-analyzer on a large workspace from cold, where it holds requests while the workspace loads and its symbol
    /// search misses short function names: the lookups meet `indexing` or the text matches, then the server's own
    /// answers. Point it at a checkout of this repository (a copy: rust-analyzer builds its build scripts into the
    /// checkout's `target/`):
    ///
    /// ```sh
    /// EITRI_GD_E2E_RUST_ANALYZER=$HOME/.local/share/nvim/mason/bin/rust-analyzer EITRI_GD_E2E_RUST_PROJECT=<checkout> \
    ///   cargo test -p eitri-panel -- --ignored gd_against_rust_analyzer_loading --nocapture
    /// ```
    #[test]
    #[ignore = "run by name with EITRI_GD_E2E_RUST_ANALYZER and EITRI_GD_E2E_RUST_PROJECT set (see the doc comment); drives a headless Neovim and rust-analyzer on a large Rust checkout; spends no tokens and needs no display"]
    fn gd_against_rust_analyzer_loading() {
        let analyzer =
            PathBuf::from(std::env::var_os("EITRI_GD_E2E_RUST_ANALYZER").expect("EITRI_GD_E2E_RUST_ANALYZER"));
        let root = PathBuf::from(std::env::var_os("EITRI_GD_E2E_RUST_PROJECT").expect("EITRI_GD_E2E_RUST_PROJECT"));
        let root = root.canonicalize().unwrap();
        let editor = RealEditor::start(
            &root,
            "core/src/keymap/panel.rs",
            "rust",
            "rust-analyzer",
            &analyzer,
            true,
        );
        let since = Instant::now();
        let mut loaded = false;
        let guard = since + Duration::from_secs(120);
        let mut round = 0;
        loop {
            for name in ["PanelKeymap", "effective", "reserved", "human", "wire", "parse_seq"] {
                let got = editor.look(&root, name, None);
                let phase = format!("t+{:.1}s", since.elapsed().as_secs_f32());
                editor.print("rust-analyzer", &phase, name, &got);
                if name == "PanelKeymap" && matches!(&got.1, Ok(found) if found.iter().any(|m| m.source == "lsp")) {
                    loaded = true;
                }
            }
            if loaded && round > 0 {
                break;
            }
            assert!(Instant::now() < guard, "rust-analyzer never answered PanelKeymap");
            round += 1;
        }
        editor.stop();
    }

    /// A directory only this fixture uses, removed (recursively) when dropped, so a failing run leaves none behind.
    struct OwnDir(PathBuf);

    impl OwnDir {
        /// Short, so the socket inside it binds under a long `TMPDIR`, and private to this user.
        fn new() -> OwnDir {
            let id = &uuid::Uuid::new_v4().simple().to_string()[..12];
            let dir = agent::state_dirs::test_temp_dir().join(format!("gd{id}"));
            agent::private_fs::create_private_dir_all(&dir, &dir).expect("a private directory for the editor socket");
            OwnDir(dir)
        }
    }

    impl Drop for OwnDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The `nvim` child this fixture started; killed and reaped when dropped, whatever stage the fixture got to.
    /// Declared after the directory it listens in, so the directory goes last.
    struct NvimProcess {
        child: std::process::Child,
        _dir: OwnDir,
    }

    impl Drop for NvimProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// A headless `nvim --clean` on a socket, with one language server started on one file of a fixture.
    struct RealEditor {
        link: eitri_core::nvim_rpc::NvimLink,
        _events: mpsc::Receiver<eitri_core::nvim_rpc::LinkEvent>,
        process: NvimProcess,
        sequencer: RefCell<Sequencer>,
        asks: RefCell<u64>,
        workers: Arc<AtomicUsize>,
    }

    const E2E_START: &str = "local file, root, cmd, name, ft, quiet = ... local b = vim.fn.bufadd(file) vim.fn.bufload(b) vim.bo[b].filetype = ft vim.api.nvim_set_current_buf(b) return vim.lsp.start({name = name, cmd = {cmd}, root_dir = root, init_options = quiet and {checkOnSave = false} or nil}, {bufnr = b})";

    /// What a chunk the test waited on answered.
    fn settled<T>(reply: Option<Result<T, eitri_core::nvim_rpc::RpcError>>) -> T {
        reply.expect("nvim answers").expect("the chunk runs")
    }
    const E2E_READY: &str = "local name = ... for _, c in ipairs(vim.lsp.get_clients({name = name})) do if c.initialized then return true end end return false";
    const E2E_STOP: &str = "vim.lsp.stop_client(vim.lsp.get_clients(), true) return true";
    const E2E_LEFT: &str = "return #vim.lsp.get_clients()";

    impl RealEditor {
        /// `own_tree`: a real project, built into its own `target/`, with rust-analyzer's check on save off (its
        /// workspace load is what a lookup meets; a whole-workspace `cargo check` is load nobody needs here).
        fn start(root: &Path, file: &str, filetype: &str, name: &str, server: &Path, own_tree: bool) -> RealEditor {
            let home = agent::state_dirs::test_workspace_dir("gd-e2e-home");
            let dir = OwnDir::new();
            let socket = agent::socket_path::in_dir(&dir.0, "nvim.sock").expect("a short socket path");
            let child = std::process::Command::new("nvim")
                .args(["--headless", "--clean", "--listen"])
                .arg(&socket)
                .env("XDG_CACHE_HOME", home.join("cache"))
                .env("XDG_STATE_HOME", home.join("state"))
                .env("XDG_DATA_HOME", home.join("data"))
                .env("XDG_CONFIG_HOME", home.join("config"))
                .env(
                    "CARGO_TARGET_DIR",
                    if own_tree {
                        root.join("target")
                    } else {
                        home.join("target")
                    },
                )
                .env("GOTOOLCHAIN", "local")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("nvim on PATH");
            let process = NvimProcess { child, _dir: dir };
            let deadline = Instant::now() + Duration::from_secs(10);
            while !socket.exists() {
                assert!(Instant::now() < deadline, "nvim did not listen");
                std::thread::sleep(Duration::from_millis(20));
            }
            let (link, events) =
                eitri_core::nvim_rpc::NvimLink::connect(&socket, Duration::from_secs(5)).expect("connect to nvim");
            let editor = RealEditor {
                link,
                _events: events,
                process,
                sequencer: RefCell::new(Sequencer::new("gd-e2e".into())),
                asks: RefCell::new(0),
                workers: Arc::default(),
            };
            let started = Instant::now();
            let id = settled(
                editor
                    .link
                    .exec_lua(
                        E2E_START,
                        vec![
                            root.join(file).to_str().unwrap().into(),
                            root.to_str().unwrap().into(),
                            server.to_str().unwrap().into(),
                            name.into(),
                            filetype.into(),
                            own_tree.into(),
                        ],
                    )
                    .wait(Duration::from_secs(10)),
            );
            assert!(id.as_u64().is_some(), "{name} did not start: {id:?}");
            while settled(
                editor
                    .link
                    .exec_lua(E2E_READY, vec![name.into()])
                    .wait(Duration::from_secs(10)),
            )
            .as_bool()
                != Some(true)
            {
                assert!(started.elapsed() < Duration::from_secs(30), "{name} never initialized");
                std::thread::sleep(Duration::from_millis(20));
            }
            eprintln!(
                "gd-e2e {name}: initialized {} ms after its start",
                started.elapsed().as_millis()
            );
            editor
        }

        /// One lookup driven at the panel tick's cadence: its time and its reply.
        fn look(&self, root: &Path, name: &str, cited: Option<(&str, u32)>) -> (Duration, Reply) {
            let started = Instant::now();
            let cited = cited.map(|(path, line)| SymbolCitation {
                path: path.to_owned(),
                line,
            });
            let mut lookup = Lookup::new(ViewId::BASE, "e2e".into(), name, cited, root.to_path_buf(), started);
            loop {
                let mut sequencer = self.sequencer.borrow_mut();
                let mut asks = self.asks.borrow_mut();
                let way = LocalWay(Rc::new(self.link.clone()));
                let mut cx = Cx {
                    way: Some(&way),
                    sequencer: &mut sequencer,
                    asks: &mut asks,
                    workers: &self.workers,
                    now: Instant::now(),
                };
                if let Some(reply) = lookup.poll(&mut cx) {
                    let took = started.elapsed();
                    return (took, reply);
                }
                drop((sequencer, asks));
                assert!(started.elapsed() < Duration::from_secs(60), "{name} never answered");
                std::thread::sleep(Duration::from_millis(33));
            }
        }

        fn print(&self, server: &str, phase: &str, name: &str, got: &(Duration, Reply)) {
            let said = match &got.1 {
                Ok(found) => found
                    .iter()
                    .map(|m| format!("{}:{} ({})", m.path, m.line, m.source))
                    .collect::<Vec<_>>()
                    .join(", "),
                Err(error) => format!("{} {}", error.kind, error.server.as_deref().unwrap_or("")),
            };
            eprintln!("gd-e2e {server} {phase} {name}: {} ms -> {said}", got.0.as_millis());
        }

        /// Stops the servers and waits for them to be gone; the nvim and its directory go when `self` drops.
        fn stop(self) {
            settled(self.link.exec_lua(E2E_STOP, vec![]).wait(Duration::from_secs(10)));
            let deadline = Instant::now() + Duration::from_secs(5);
            while settled(self.link.exec_lua(E2E_LEFT, vec![]).wait(Duration::from_secs(10))).as_u64() != Some(0)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for RealEditor {
        /// A failed run: ask the servers to stop (they are nvim's children, and killing nvim outright can leave
        /// them behind), briefly, then `process` kills nvim and removes its directory.
        fn drop(&mut self) {
            let _ = self.link.exec_lua(E2E_STOP, vec![]).wait(Duration::from_secs(2));
        }
    }

    fn symbol_message(request_id: &str, tab: TabId, name: &str) -> String {
        json!({"type": "editor_symbol", "request_id": request_id, "tab": tab.0, "name": name}).to_string()
    }

    fn symbol_replies(page: &RecordingPage) -> Vec<serde_json::Value> {
        page.sent
            .borrow()
            .iter()
            .map(|payload| json_of(payload))
            .filter(|reply| reply["kind"] == "editor_symbol_result")
            .collect()
    }

    #[test]
    fn a_linked_window_lookup_uses_the_editor_way_and_preserves_transport_errors() {
        use eitri_core::editor_tools::way::RemoteWay;
        use eitri_core::window_link::EditorOp;
        for (cited, fail_collect) in [(false, None), (true, None), (false, Some(false)), (false, Some(true))] {
            let root = project("gd-linked", &[("a.rs", "fn Name() {}")]);
            let (handle, page, _) = handle_with_recording_page();
            let sent = Rc::new(RefCell::new(Vec::new()));
            let record = sent.clone();
            let file = root.join("a.rs");
            let way = RemoteWay::new(
                &root,
                Box::new(move |op| {
                    let collecting = matches!(op, EditorOp::AgentLspCollect { .. });
                    let value = if collecting {
                        done(
                            if cited { "definitions" } else { "workspace_symbols" },
                            vec![item(&file, 7, 0, Some("Name"), None)],
                            &[],
                        )
                    } else {
                        started(&["gopls"], &[])
                    };
                    record.borrow_mut().push(serde_json::to_value(&op).unwrap());
                    if fail_collect == Some(collecting) {
                        return Pending::failed(eitri_core::nvim_rpc::RpcError::Unavailable(
                            "linked editor disconnected".into(),
                        ));
                    }
                    let editor = FakeEditor::default();
                    let pending = editor.exec_lua(lsp::LSP_ASK_LUA, vec![]);
                    editor.answer_oldest(&value);
                    pending
                }),
            );
            let tab = {
                let mut state = handle.state.borrow_mut();
                state.project_dir = root.clone();
                state.editor_rpc = None;
                state.editor_way = Some(Rc::new(way));
                state.tabs.active()
            };
            let mut message: serde_json::Value = serde_json::from_str(&symbol_message("linked", tab, "Name")).unwrap();
            if cited {
                message["cited"] = json!({"path": "a.rs", "line": 1});
            }
            handle.page_message(ViewId::BASE, &message.to_string());
            tick_handle_until(&handle, "linked symbol answer", || !symbol_replies(&page).is_empty());
            let reply = &symbol_replies(&page)[0];
            if fail_collect.is_some() {
                assert_eq!(reply["error"]["kind"], "refused");
                assert!(reply["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("linked editor disconnected"));
            } else {
                assert_eq!(reply["matches"][0]["path"], "a.rs");
                assert_eq!(reply["matches"][0]["line"], 7);
                let ops = sent.borrow();
                assert_eq!(ops.len(), 2);
                assert_eq!(ops[0]["load"], false);
                if cited {
                    assert_eq!(ops[0]["path"], "a.rs");
                    assert_eq!(ops[0]["include"], json!(["definitions"]));
                    assert_eq!(ops[0]["symbol"], "Name");
                } else {
                    assert_eq!(ops[0]["query_for"][0], json!(["gopls", "Name"]));
                }
                assert_eq!(ops[1]["id"], ops[0]["id"]);
            }
        }
    }
    #[test]
    fn the_reply_goes_to_the_asking_view_only() {
        let (handle, base, second) = handle_with_two_pages();
        let editor = Rc::new(eitri_core::editor_tools::testing::FakeEditor::default());
        let tab = {
            let mut state = handle.state.borrow_mut();
            state.project_dir = agent::state_dirs::test_workspace_dir("gd-views");
            state.editor_rpc = Some(editor.clone());
            state.tabs.open_in(AgentId::Claude, view(2)).unwrap()
        };
        handle.page_message(view(2), &symbol_message("g2", tab, "Foo"));
        tick_handle_until(&handle, "the ask", || editor.asked().len() == 1);
        assert!(handle.state.borrow().symbol_lookup.is_running());
        assert!(super::super::tick::needs_polling(&handle.state.borrow()));
        editor.answer_oldest(&json!({"no_client": true}));
        tick_handle_until(&handle, "the reply", || !symbol_replies(&second).is_empty());
        let reply = &symbol_replies(&second)[0];
        assert_eq!(reply["requestId"], "g2");
        assert_eq!(reply["error"]["kind"], "no_client");
        assert!(symbol_replies(&base).is_empty(), "nothing to the other page");
        assert!(!handle.state.borrow().symbol_lookup.is_running());
    }

    #[test]
    fn a_fifth_lookup_is_refused_and_a_gone_page_forgets_its_own() {
        let (handle, base, _) = handle_with_two_pages();
        let editor = Rc::new(eitri_core::editor_tools::testing::FakeEditor::default());
        let tab = {
            let mut state = handle.state.borrow_mut();
            state.project_dir = agent::state_dirs::test_workspace_dir("gd-fifth");
            state.editor_rpc = Some(editor.clone());
            state.tabs.active()
        };
        for n in 1..=5 {
            handle.page_message(ViewId::BASE, &symbol_message(&format!("g{n}"), tab, "Foo"));
        }
        let replies = symbol_replies(&base);
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0]["requestId"], "g5");
        assert_eq!(replies[0]["error"]["kind"], "refused");
        assert_eq!(
            replies[0]["error"]["message"],
            "gd: 4 lookups are still out; try again in a moment"
        );
        handle.page_message(ViewId::BASE, READY);
        assert!(
            !handle.state.borrow().symbol_lookup.is_running(),
            "a new document asked none of them"
        );
    }
}
