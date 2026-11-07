//! The panel's side of the editor tools: what a session is given when it starts, the tick's round of the broker, a
//! person's answer to one of its cards, and the bridge sockets of the tabs whose agent runs a tool server of its own.
//! The broker itself is `eitri_core::editor_tools::broker::Broker`, held by the panel state.

use eitri_core::agent_bridge::{serialize_editor_cards_for_js, TabStateWire};
use eitri_core::editor_context::{ContextSource, EditorContext};
#[cfg(target_os = "linux")]
pub(super) use eitri_core::editor_tools::bridge_host::{BridgeHost, BridgeName, BridgeNews};
use eitri_core::editor_tools::broker::{Broker, BrokerEnv, BrokerOut, Caller, TabFacts};
use eitri_core::editor_tools::cards::{CardDecision, CardOrigin};
use eitri_core::editor_tools::catalog::{self, ToolRow};
use eitri_core::editor_tools::follow::FollowRun;
use eitri_core::editor_tools::step::{Step, StepEvent, ToolCx, ToolResult, ToolRun};
use eitri_core::editor_tools::tools::tool_named;
use eitri_core::editor_tools::way::{EditorCall, EditorWay, LocalWay};
use eitri_core::editor_tools::words;
use eitri_core::project_trust::EditorServerName;
use eitri_core::tab_set::{ClientCallNews, TabSet, NAME_TAKEN_LINE, NAME_UNKNOWN_LINE};
use eitri_core::turn_review::flow::FollowEnds;
use eitri_core::turn_review::{review_dir_for, ProposalWriter, TurnReview};
use std::collections::BTreeSet;
#[cfg(not(target_os = "linux"))]
pub(super) use unbridged::{BridgeHost, BridgeName, BridgeNews};

use super::*;

/// The tool server, beside the program that started it.
const BRIDGE_BINARY: &str = "eitri-editor-bridge";

/// A tab's bridge socket by name, and what its session was given: what a move hands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TabBridge {
    pub(super) name: BridgeName,
    pub(super) tools: Vec<String>,
}

/// The shapes the glue is written against where no bridge socket exists: a host never starts, so no tab is given a
/// server and nothing here runs.
#[cfg(not(target_os = "linux"))]
mod unbridged {
    use eitri_core::editor_tools::step::{BridgeCallId, ToolResult};
    use std::convert::Infallible;
    use std::io;
    use std::path::PathBuf;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct BridgeName {
        pub hex: String,
        pub key: String,
    }

    impl BridgeName {
        pub fn check(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[allow(dead_code)]
    pub enum BridgeNews {
        Call {
            id: BridgeCallId,
            socket: String,
            tool: String,
            arguments: String,
            row_id: Option<String>,
        },
        Cancelled {
            id: BridgeCallId,
        },
    }

    pub struct BridgeHost(Infallible);

    impl BridgeHost {
        pub fn start(_dir: PathBuf, _wake: agent::UiWake) -> io::Result<BridgeHost> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub fn open(&self, _tab: u64, _tools: Vec<serde_json::Value>) -> io::Result<(BridgeName, PathBuf)> {
            match self.0 {}
        }

        pub fn adopt(&self, _tab: u64, _name: &BridgeName, _tools: Vec<serde_json::Value>) -> io::Result<PathBuf> {
            match self.0 {}
        }

        pub fn close(&self, _tab: u64) {
            match self.0 {}
        }

        pub fn take_news(&self) -> Vec<BridgeNews> {
            match self.0 {}
        }

        pub fn answer(&self, _id: &BridgeCallId, _result: &ToolResult) {
            match self.0 {}
        }
    }
}

/// The tools a session started now, with `project` trust, is declared: what the window's switches leave on, cut
/// to what a session refused at the trust question may have. `None` when nothing is declared, so the field is left out. The
/// list is read here once and kept by the session; the legacy backend ignores it.
pub(super) fn tools_for_start(state: &AgentPanelState, project: ProjectTrust) -> Option<agent::ClientToolSet> {
    eitri_core::editor_tools::catalog::client_tool_set(&state.editor_switches, project)
}

/// The tool server an external session started now, with `trust`, is to run, or `None`, with the agent then given no
/// tools. Each session gets a socket and a key of its own, made here. What it was declared is stored for
/// [`BrokerEnv::facts`], so the broker refuses any other name: the bridge passes whatever it is sent, and its key is on
/// the agent's command line, so it is no permission. `name` says whether a Codex configuration may already hold a
/// server of this name: Codex would merge the two, so such a tab goes without, and says which of the two it was once.
/// Only Codex's configuration is known to Eitri, so nothing else is held to it.
pub(super) fn server_for_start(
    state: &mut AgentPanelState,
    tab: TabId,
    agent: &AgentId,
    trust: ProjectTrust,
    name: EditorServerName,
) -> Option<agent::EditorServerSpec> {
    tab_gone(state, tab);
    state.tabs.set_editor_declared(tab, BTreeSet::new());
    if !agent.is_external() {
        return None;
    }
    let rows = catalog::declared(&state.editor_switches, trust);
    let taken = match name {
        EditorServerName::Free => None,
        EditorServerName::Defined => Some(NAME_TAKEN_LINE),
        EditorServerName::Unknown => Some(NAME_UNKNOWN_LINE),
    };
    if let Some(line) = taken.filter(|_| *agent == AgentId::Codex && !rows.is_empty()) {
        state.tabs.owe_name_taken_note(tab, line);
        return None;
    }
    server_of_rows(state, tab, &rows)
}

/// [`server_for_start`] once the rows are known.
fn server_of_rows(
    state: &mut AgentPanelState,
    tab: TabId,
    rows: &[&'static ToolRow],
) -> Option<agent::EditorServerSpec> {
    if rows.is_empty() {
        return None;
    }
    let command = match bridge_binary(state) {
        Ok(command) => command,
        Err(why) => {
            eprintln!("[agent_panel] tab {}: no editor tools for this session: {why}", tab.0);
            return None;
        }
    };
    let (name, socket) = match bridge_host(state)?.open(tab.0, tool_objects(rows)) {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("[agent_panel] tab {}: the bridge socket could not bind: {error}", tab.0);
            return None;
        }
    };
    let tools: Vec<String> = rows.iter().map(|row| row.name.to_owned()).collect();
    state.tabs.set_editor_declared(tab, tools.iter().cloned().collect());
    let key = name.key.clone();
    state.editor_bridge_tabs.insert(tab, TabBridge { name, tools });
    let Some(socket) = socket.to_str().map(str::to_owned) else {
        eprintln!(
            "[agent_panel] tab {}: the bridge socket's path is not text: {}",
            tab.0,
            socket.display()
        );
        tab_gone(state, tab);
        state.tabs.set_editor_declared(tab, BTreeSet::new());
        return None;
    };
    Some(agent::EditorServerSpec {
        name: eitri_core::editor_tools::SERVER_NAME.to_owned(),
        command,
        args: vec![socket, key],
        cwd: state.project_dir.clone(),
    })
}

/// The bridge beside this program, or why there is none to run.
fn bridge_binary(state: &AgentPanelState) -> Result<PathBuf, String> {
    let beside = this_program(state).map_err(|error| format!("this program's path: {error}"))?;
    let binary = beside.with_file_name(BRIDGE_BINARY);
    if binary.is_file() {
        Ok(binary)
    } else {
        Err(format!("{} is not a file", binary.display()))
    }
}

#[cfg(not(test))]
fn this_program(_state: &AgentPanelState) -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

#[cfg(test)]
fn this_program(state: &AgentPanelState) -> std::io::Result<PathBuf> {
    match &state.bridge_beside {
        Some(path) => Ok(path.clone()),
        None => std::env::current_exe(),
    }
}

/// The window's bridge host, made the first time a session needs one. A host that cannot start leaves that session
/// without tools, and the next start tries again.
fn bridge_host(state: &mut AgentPanelState) -> Option<&BridgeHost> {
    if state.editor_bridge.is_none() {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR");
        let dir = eitri_core::panel_control::control_dir(runtime.as_deref(), &std::env::temp_dir());
        let wake = state.wake.as_ref().map_or_else(
            || std::sync::Arc::new(|| ()) as agent::UiWake,
            crate::panel_wake::PanelWake::waker,
        );
        match BridgeHost::start(dir, wake) {
            Ok(host) => state.editor_bridge = Some(host),
            Err(error) => eprintln!("[agent_panel] the bridge sockets could not start: {error}"),
        }
    }
    state.editor_bridge.as_ref()
}

/// The MCP tool objects a bridge serves, `{name, description, inputSchema}`.
fn tool_objects(rows: &[&'static ToolRow]) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|row| {
            // Every row's schema is a JSON object, which the catalogue's own test holds.
            let schema: serde_json::Value = serde_json::from_str(row.schema).unwrap_or_default();
            serde_json::json!({ "name": row.name, "description": row.description, "inputSchema": schema })
        })
        .collect()
}

/// Binds the tab's socket under its name again, here, with the rows of the tools it was given. A failure leaves the
/// tab without tools until its session ends, and the bridge tells its client so after its reconnect window.
fn bind_again(state: &mut AgentPanelState, tab: TabId, bridge: &TabBridge) {
    let rows: Vec<&'static ToolRow> = bridge
        .tools
        .iter()
        .filter_map(|name| tool_named(name).map(|tool| tool.row()))
        .collect();
    let Some(host) = bridge_host(state) else {
        return;
    };
    if let Err(error) = host.adopt(tab.0, &bridge.name, tool_objects(&rows)) {
        eprintln!("[agent_panel] tab {}: its bridge socket could not bind: {error}", tab.0);
    }
}

/// `tab` arrived from the other window with a bridge: it keeps its name, so the server its agent runs finds it, and
/// the list its session was given, whatever this window's switches say.
pub(super) fn tab_brought(state: &mut AgentPanelState, tab: TabId, bridge: TabBridge) {
    state
        .tabs
        .set_editor_declared(tab, bridge.tools.iter().cloned().collect());
    bind_again(state, tab, &bridge);
    state.editor_bridge_tabs.insert(tab, bridge);
}

/// `tab`'s provider froze for a move: the socket's name is free for the other window to bind. Its entry stays, for
/// the record and for [`bridge_back`]. What was queued for a connection is written first, so an answer given in
/// [`tab_leaving`] is read before the connection ends.
pub(super) fn bridge_leaving(state: &AgentPanelState, tab: TabId) {
    if let Some(host) = &state.editor_bridge {
        host.close(tab.0);
    }
}

/// A move of `tab` was called off: its socket is bound here again under the same name.
pub(super) fn bridge_back(state: &mut AgentPanelState, tab: TabId) {
    if let Some(bridge) = state.editor_bridge_tabs.get(&tab).cloned() {
        bind_again(state, tab, &bridge);
    }
}

/// `tab` left this window, or the session its socket was for ended: the socket ends with it, and a bridge left over
/// fails.
pub(super) fn tab_gone(state: &mut AgentPanelState, tab: TabId) {
    state.editor_follower.forget(tab.0);
    if state.editor_bridge_tabs.remove(&tab).is_some() {
        if let Some(host) = &state.editor_bridge {
            host.close(tab.0);
        }
    }
}

/// Ends the socket of each tab that is gone or has no session any more: a failed start, a lost session, an agent that
/// went by itself. A start in flight has its socket already, and a tab moving away keeps its entry until the move ends.
fn end_sockets_of_ended_sessions(state: &mut AgentPanelState) {
    let tabs = &state.tabs;
    let ended: Vec<TabId> = state
        .editor_bridge_tabs
        .keys()
        .copied()
        .filter(|tab| {
            let session = tabs
                .get(*tab)
                .is_some_and(|t| matches!(t.wire_state(), TabStateWire::Live | TabStateWire::Starting));
            !session && !tabs.is_moving(*tab)
        })
        .collect();
    for tab in ended {
        tab_gone(state, tab);
    }
}

/// What the broker reads of the panel, for one round.
struct PanelEnv<'a> {
    tabs: &'a TabSet,
    way: Option<Rc<dyn EditorWay>>,
    root: &'a Path,
    home: Option<&'a Path>,
    context: &'a ContextSource,
    review_roots: &'a ReviewRoots,
}

impl BrokerEnv for PanelEnv<'_> {
    fn way(&self) -> Option<&dyn EditorWay> {
        self.way.as_deref()
    }

    fn facts(&self, tab: TabId) -> Option<TabFacts> {
        self.tabs.broker_facts(tab)
    }

    fn root(&self) -> &Path {
        self.root
    }

    fn home(&self) -> Option<&Path> {
        self.home
    }

    fn context(&self) -> Option<EditorContext> {
        (self.context)()
    }

    /// The store of this window's turn review, and the same directory when `review.enabled` left it with none: a
    /// proposal needs that store's blobs, not the review.
    fn proposal_writer(&self) -> Option<ProposalWriter> {
        let roots = self.review_roots;
        let writer = self.tabs.turn_review().map(TurnReview::proposal_writer);
        Some(writer.unwrap_or_else(|| {
            let review_dir = review_dir_for(
                roots.state_home.as_deref(),
                roots.home.as_deref(),
                &roots.canonical_root,
            );
            ProposalWriter::new(review_dir, &roots.canonical_root)
        }))
    }
}

/// The tick's round of the broker: what the bridges sent and what the pump found about each tab's calls reach it, then
/// every line steps, and what comes out goes where it is owed. The pump read the calls from the projection, so a resync
/// loses none, and a lost stream reaches here as every call settling. A tab's calls share one queue, whichever way they
/// came. A turn's beginning is told before anything else: the pump pushes a call's `Opened` earlier in the same
/// drain than the clock it reads, so the baseline of a turn must be in the tab's queue ahead of that turn's calls,
/// whichever way they came. A bridge's news goes next, so a call sent before an interrupt the pump found ends with
/// that interrupt, and [`tell_broker_of_bridge`] answers one read after it.
pub(super) fn poll(state: &mut AgentPanelState, out: &mut Outbox) {
    read_arrived_calls(state, out);
    let now = std::time::Instant::now();
    let news = state.tabs.take_client_news();
    let edits = state.tabs.take_follow_edits();
    let bridge_news = state
        .editor_bridge
        .as_ref()
        .map_or_else(Vec::new, BridgeHost::take_news);
    let bridge_news = of_current_sessions(&state.editor_bridge_tabs, bridge_news);
    let AgentPanelState {
        tabs,
        editor_tools,
        editor_follower,
        editor_way,
        editor_rpc,
        project_dir,
        home,
        editor_context,
        review_roots,
        review_flow,
        ..
    } = state;
    let local = || {
        editor_rpc
            .clone()
            .map(|rpc| Rc::new(LocalWay(rpc)) as Rc<dyn EditorWay>)
    };
    let env = PanelEnv {
        tabs,
        way: editor_way.clone().or_else(local),
        root: project_dir,
        home: home.as_deref(),
        context: editor_context,
        review_roots,
    };
    let mut outs = Vec::new();
    for (tab, news) in &news {
        if let ClientCallNews::TurnBegan { turn_id } = news {
            outs.extend(editor_tools.turn_began(*tab, turn_id, &env, now));
        }
    }
    for news in bridge_news {
        outs.extend(tell_broker_of_bridge(editor_tools, &env, news, now));
    }
    for (tab, news) in news {
        outs.extend(tell_broker(editor_tools, &env, tab, news, now));
    }
    for (tab, path) in edits {
        editor_follower.edited(tab.0, path);
    }
    let first_change = |tab| {
        tabs.review_session(TabId(tab))
            .ok()
            .map(|(review, session)| review.first_change_job(&session))
    };
    let follow_ends = review_flow.follow_ends();
    for (tab, run) in editor_follower.poll(project_dir, home.as_deref(), &first_change, now) {
        let run = FollowEnding::new(run, tab, project_dir, follow_ends.clone());
        editor_tools.submit_run(TabId(tab), Box::new(run), now);
    }
    outs.extend(editor_tools.tick(&env, now));
    route(state, outs, out);
    end_sockets_of_ended_sessions(state);
}

/// A follow run that, when it ends, tells the review flow's live drawings which file it opened: a file the editor did not
/// show when the edit landed may show now, in the split the run opened.
pub(super) struct FollowEnding {
    run: FollowRun,
    tab: u64,
    root: PathBuf,
    /// The file the run asked the editor to open, relative to the project.
    opened: Option<PathBuf>,
    ends: FollowEnds,
}

impl FollowEnding {
    pub(super) fn new(run: FollowRun, tab: u64, root: &Path, ends: FollowEnds) -> FollowEnding {
        FollowEnding {
            run,
            tab,
            root: root.to_path_buf(),
            opened: None,
            ends,
        }
    }
}

impl ToolRun for FollowEnding {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
        let step = self.run.step(event, cx);
        if let Step::Call {
            call: EditorCall::OpenFile { path, .. },
            ..
        } = &step
        {
            self.opened = path.strip_prefix(&self.root).ok().map(Path::to_path_buf);
        }
        step
    }
}

impl Drop for FollowEnding {
    fn drop(&mut self) {
        if let Some(rel) = self.opened.take() {
            self.ends.ended(self.tab, rel);
        }
    }
}

/// A call that waited in the channel while its tab went or started another session is left out: under the tab's id it
/// would run with that session's declarations, trust and mode. A session's socket is never used again, so nothing
/// answers it either: its connection closed with the socket, and the bridge tells its client. A cancel of such a call
/// finds nothing.
fn of_current_sessions(bridges: &BTreeMap<TabId, TabBridge>, news: Vec<BridgeNews>) -> Vec<BridgeNews> {
    news.into_iter()
        .filter(|one| match one {
            BridgeNews::Call { id, socket, .. } => bridges
                .get(&TabId(id.tab))
                .is_some_and(|bridge| bridge.name.hex == *socket),
            BridgeNews::Cancelled { .. } => true,
        })
        .collect()
}

/// A person's answer on a card's own id, already admitted. The broker queues the call again and the tick runs it.
pub(super) fn answer_card(
    state: &mut AgentPanelState,
    tab: TabId,
    card: &str,
    decision: CardDecision,
    origin: CardOrigin,
    out: &mut Outbox,
) -> Result<(), String> {
    let outs = state
        .editor_tools
        .answer_card(tab, card, decision, origin, std::time::Instant::now())?;
    route(state, outs, out);
    Ok(())
}

/// `Ctrl+g` on the card of a proposal an editor tool computed: file `file` of proposal `proposal` of `tab`, both sides
/// read from the review store on a worker, then shown by the editor's read-only diff tab. It writes nothing. The page
/// names the proposal and the file; the texts are the store's, never the page's.
pub(super) fn view_proposal(
    state: &mut AgentPanelState,
    view: ViewId,
    request_id: &str,
    tab: TabId,
    proposal: &str,
    file: u32,
) -> Result<(), String> {
    if state.scratch.is_none() {
        return Err("the scratch directory could not be created at startup".to_string());
    }
    let memory = state.editor_tools.memory();
    let kept = memory
        .proposal(tab.0, proposal)
        .ok_or_else(|| format!("{proposal} is gone: ask the agent for it again"))?;
    let proposed = kept
        .files
        .get(file as usize)
        .cloned()
        .ok_or_else(|| format!("{proposal} has no file number {file}"))?;
    let writer = memory
        .writer()
        .ok_or_else(|| "no state directory to read the proposal from".to_string())?;
    let name = state.project_dir.join(&proposed.rel).to_string_lossy().into_owned();
    start_proposal_job(state, view, request_id, name, move || {
        writer
            .texts(&proposed)
            .map_err(|why| eitri_core::scratch::ProposalRefused::Io(why.to_string()))
    })
}

/// `tab` is leaving this window, and takes no call and no card with it: each of its calls is told to try again and
/// each card goes, while the provider still takes answers. A call the broker never heard of is told here too and stays
/// out of the broker, so nothing of the tab starts here. No later window can be relied on for it: a read can fail.
pub(super) fn tab_leaving(state: &mut AgentPanelState, tab: TabId, out: &mut Outbox) {
    state.editor_follower.forget(tab.0);
    let unreported = state.tabs.take_unreported_client_calls(tab);
    let mut outs = state.editor_tools.withdraw_tab(tab, words::MOVED);
    outs.extend(
        unreported
            .iter()
            .map(|call_id| told_to_try_again(tab, sidecar_call(call_id))),
    );
    route(state, outs, out);
}

/// `tab` arrived with a session folded through `folded_through`. Its open calls wait until the session says which are
/// still open: running one here could run it twice. A sidecar with no client tools has none to wait for.
pub(super) fn tab_arrived(state: &mut AgentPanelState, tab: TabId, folded_through: u64) {
    let Some(backend) = state.tabs.get(tab).and_then(Tab::live) else {
        return;
    };
    if !backend.capabilities().client_tools {
        return;
    }
    let read = backend.client_tool_call_state();
    state.tabs.hold_client_calls(tab, folded_through);
    match read {
        Ok(read) => state.client_call_reads.push((tab, read)),
        Err(error) => read_failed(state, tab, folded_through, &error.message),
    }
}

/// The session's answers about the calls of tabs that arrived. Each open call is told to try again, once, and it is
/// the first answer that counts, so one the other window answered too changes nothing. A read outlives a freeze, and
/// a tab that left, or whose session ended, has no call left to read.
fn read_arrived_calls(state: &mut AgentPanelState, out: &mut Outbox) {
    for (tab, read) in std::mem::take(&mut state.client_call_reads) {
        let Some(folded_through) = state.tabs.held_client_calls(tab) else {
            continue;
        };
        if state.tabs.is_moving(tab) {
            state.client_call_reads.push((tab, read));
            continue;
        }
        match read.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => state.client_call_reads.push((tab, read)),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                read_failed(state, tab, folded_through, "the session is gone");
            }
            Ok(Err(error)) => read_failed(state, tab, folded_through, &error.to_string()),
            Ok(Ok(snapshot)) => {
                let open: Vec<String> = snapshot.open.into_iter().map(|call| call.call_id).collect();
                let told = open
                    .iter()
                    .map(|call_id| told_to_try_again(tab, sidecar_call(call_id)))
                    .collect();
                route(state, told, out);
                state.tabs.release_client_calls(tab, snapshot.through_sequence, &open);
            }
        }
    }
}

fn told_to_try_again(tab: TabId, caller: Caller) -> BrokerOut {
    told(tab, caller, words::MOVED)
}

fn told(tab: TabId, caller: Caller, why: &str) -> BrokerOut {
    BrokerOut::Answer {
        tab,
        caller,
        result: ToolResult {
            text: why.to_owned(),
            is_error: true,
        },
    }
}

fn sidecar_call(call_id: &str) -> Caller {
    Caller::Sidecar {
        call_id: call_id.to_owned(),
    }
}

/// With no read, a call folded where the tab came from was answered there ([`tab_leaving`]), so it must not run here;
/// anything later runs, and the first answer wins.
fn read_failed(state: &mut AgentPanelState, tab: TabId, folded_through: u64, why: &str) {
    eprintln!("[agent_panel] tab {}: its open calls could not be read: {why}", tab.0);
    state.tabs.release_client_calls(tab, folded_through, &[]);
}

fn tell_broker(
    broker: &mut Broker,
    env: &PanelEnv<'_>,
    tab: TabId,
    news: ClientCallNews,
    now: std::time::Instant,
) -> Vec<BrokerOut> {
    match news {
        ClientCallNews::Opened(call) => {
            let caller = Caller::Sidecar { call_id: call.call_id };
            broker.submit(
                tab,
                caller,
                call.tool_use_id,
                &call.tool,
                &call.arguments_json,
                env,
                now,
            )
        }
        ClientCallNews::Settled { call_id } => broker.cancel(tab, &Caller::Sidecar { call_id }),
        ClientCallNews::TurnInterrupted => broker.turn_interrupted(tab),
        // Told ahead of the rest, in `poll`.
        ClientCallNews::TurnBegan { .. } => Vec::new(),
    }
}

/// A bridge's news for the broker, under the bridge's own id. `row_id` is the model's id for the call, when the agent
/// sends one, and so the row a card belongs to. Nothing of a tab frozen for a move starts here: the call arrived after
/// [`tab_leaving`] and is told what that told the others. Nor does a call of a turn already interrupted: the broker
/// heard of the interrupt before this call and will not hear of it again, and a Codex before 0.165 says nothing more about the call.
fn tell_broker_of_bridge(
    broker: &mut Broker,
    env: &PanelEnv<'_>,
    news: BridgeNews,
    now: std::time::Instant,
) -> Vec<BrokerOut> {
    match news {
        BridgeNews::Call {
            id,
            tool,
            arguments,
            row_id,
            ..
        } => {
            let tab = TabId(id.tab);
            let caller = Caller::Bridge { id };
            if env.tabs.is_moving(tab) {
                return vec![told_to_try_again(tab, caller)];
            }
            if env.tabs.last_turn_was_interrupted(tab) {
                return vec![told(tab, caller, words::INTERRUPTED)];
            }
            broker.submit(tab, caller, row_id, &tool, &arguments, env, now)
        }
        BridgeNews::Cancelled { id } => broker.cancel(TabId(id.tab), &Caller::Bridge { id }),
    }
}

/// What the broker gave out goes where it is owed. An answer the provider will not take is ordinary: the call may
/// have ended first, and the first answer wins. It leaves one line, and the tab goes on. An answer to a bridge whose
/// connection has gone goes to nobody: the bridge told its client when the connection dropped. A
/// change of cards reaches the tab set, for the tray and for snapshots, and the page showing the tab; a tab on no
/// page gets them from the snapshot its next showing sends.
fn route(state: &mut AgentPanelState, outs: Vec<BrokerOut>, out: &mut Outbox) {
    for broker_out in outs {
        match broker_out {
            BrokerOut::Answer {
                tab,
                caller: Caller::Sidecar { call_id },
                result,
            } => {
                if let Err(error) = state.tabs.answer_client_call(tab, &call_id, &result) {
                    eprintln!(
                        "[agent_panel] tab {}: the answer to {call_id} was not taken: {}",
                        tab.0, error.message
                    );
                }
            }
            BrokerOut::Answer {
                caller: Caller::Bridge { id },
                result,
                ..
            } => {
                if let Some(host) = &state.editor_bridge {
                    host.answer(&id, &result);
                }
            }
            BrokerOut::Answer {
                caller: Caller::Host, ..
            } => {}
            BrokerOut::CardsChanged { tab } => {
                let cards = state.editor_tools.cards(tab);
                if let Some(view) = state.page_of(tab) {
                    out.send(view, serialize_editor_cards_for_js(tab, &cards));
                }
                state.tabs.set_editor_cards(tab, cards);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use agent::AgentDomainEvent;
    use eitri_core::editor_tools::step::BridgeCallId;
    use eitri_core::editor_tools::testing::FakeEditor;
    use serde_json::json;

    use super::*;
    use crate::agent_panel::tests::*;

    const STATE_TOOL: &str = "editor_state";
    const OPEN_TOOL: &str = "open_file";
    const EXEC_TOOL: &str = "nvim_exec";
    /// In the catalogue, and left out of the set these tests declare.
    const UNDECLARED_TOOL: &str = "read_buffer";

    /// `review.enabled`, as `init.lua` said, over a real project and state directory: a proposal's two blobs land in
    /// the store a window keeps for that project, with or without the review running.
    fn a_proposal_is_stored_with_review(enabled: bool, label: &str) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label).canonicalize().unwrap();
        let root = dir.join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.rs"), "before\n").unwrap();
        let state_home = dir.join("state").into_os_string();
        let review_dir = review_dir_for(Some(&state_home), None, &root).unwrap();

        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        state.borrow_mut().review_roots = ReviewRoots {
            state_home: Some(state_home),
            home: None,
            canonical_root: root,
        };
        state.borrow_mut().apply_review(ReviewConfig {
            enabled,
            hint: eitri_core::agent_prefs::ReviewHintLevel::Off,
            live: false,
        });
        let state = state.borrow();
        assert_eq!(state.tabs.turn_review().is_some(), enabled);

        let writer = PanelEnv {
            tabs: &state.tabs,
            way: None,
            root: &state.project_dir,
            home: None,
            context: &state.editor_context,
            review_roots: &state.review_roots,
        }
        .proposal_writer()
        .expect("a writer");
        let rel = Path::new("a.rs");
        let now = writer.read(rel).unwrap();
        let file = writer.store(rel, &now, b"after\n").unwrap();
        assert_eq!(writer.texts(&file).unwrap().1, "after\n");
        assert!(review_dir.is_dir(), "the blobs are in the project's review directory");
    }

    #[test]
    fn a_proposal_uses_the_turn_reviews_store() {
        a_proposal_is_stored_with_review(true, "glue-writer-review-on");
    }

    #[test]
    fn a_proposal_needs_no_turn_review() {
        a_proposal_is_stored_with_review(false, "glue-writer-review-off");
    }

    /// The proposal view: a stored two-file-capable proposal in the tab's memory, the panel's own scratch
    /// directory, and every editor request the panel makes recorded.
    struct ProposalPanel {
        panel: Fixture,
        asked: Rc<RefCell<Vec<eitri_core::scratch::ScratchRequest>>>,
        root: PathBuf,
        id: String,
    }

    impl ProposalPanel {
        fn new(label: &str) -> ProposalPanel {
            let panel = fixture(label);
            let root = panel.handle.state.borrow().project_dir.canonicalize().unwrap();
            std::fs::write(root.join("a.rs"), "before\n").unwrap();
            let writer = ProposalWriter::new(Some(root.join("review-store")), &root);
            let rel = Path::new("a.rs");
            let now = writer.read(rel).unwrap();
            let file = writer.store(rel, &now, b"after\n").unwrap();
            let memory = panel.handle.state.borrow().editor_tools.memory().clone();
            memory.set_writer(Some(writer));
            let id = memory.keep_proposal(panel.tab.0, "a change".to_string(), vec![file]);
            let scratch = eitri_core::scratch::ScratchDir::in_dir(&root).expect("a scratch directory");
            panel.handle.state.borrow_mut().scratch = Some(scratch);
            let asked: Rc<RefCell<Vec<eitri_core::scratch::ScratchRequest>>> = Rc::default();
            let recorder = asked.clone();
            panel.handle.on_editor_request(move |request| {
                recorder.borrow_mut().push(request.clone());
                Ok(())
            });
            ProposalPanel { panel, asked, root, id }
        }

        fn view(&self, proposal: &str, file: u32) {
            let message = json!({
                "type": "editor_card_view", "request_id": "v1", "tab": self.panel.tab.0,
                "proposal": proposal, "file": file,
            });
            self.panel.handle.page_message(ViewId::BASE, &message.to_string());
        }

        /// What the page was told of request `v1`: `Ok(())` for success, the sentence for a refusal.
        fn answer(&self) -> Option<Result<(), String>> {
            self.panel.page.sent.borrow().iter().find_map(|sent| {
                let value: serde_json::Value = serde_json::from_str(sent).ok()?;
                if value["requestId"] != "v1" {
                    return None;
                }
                Some(match value["ok"].as_bool() {
                    Some(true) => Ok(()),
                    _ => Err(value["error"].as_str().unwrap_or_default().to_string()),
                })
            })
        }

        fn editor_request(&self) -> serde_json::Value {
            let hex = self.asked.borrow()[0].hex().to_string();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    #[test]
    fn ctrl_g_on_a_proposal_reaches_the_editor_as_a_diff() {
        let view = ProposalPanel::new("glue-proposal-view");
        view.view(&view.id, 0);
        view.panel
            .tick_until("the editor to be asked", || !view.asked.borrow().is_empty());
        assert_eq!(view.asked.borrow().len(), 1, "one editor request");
        let request = view.editor_request();
        assert_eq!(request["op"], "diff");
        assert_eq!(request["name"], view.root.join("a.rs").to_string_lossy().as_ref());
        assert_eq!(
            std::fs::read_to_string(request["base"].as_str().unwrap()).unwrap(),
            "before\n"
        );
        assert_eq!(
            std::fs::read_to_string(request["proposed"].as_str().unwrap()).unwrap(),
            "after\n"
        );
        assert_eq!(view.answer(), Some(Ok(())), "the page's request was answered");
    }

    #[test]
    fn a_proposal_that_is_gone_is_refused() {
        let view = ProposalPanel::new("glue-proposal-gone");
        view.view("p-99", 0);
        view.panel.settle();
        let refused = view.answer().expect("an answer").expect_err("a refusal");
        assert!(refused.contains("p-99"), "{refused}");
        assert!(view.asked.borrow().is_empty(), "nothing reached the editor");
    }

    #[test]
    fn a_file_index_past_the_end_is_refused() {
        let view = ProposalPanel::new("glue-proposal-past-the-end");
        view.view(&view.id, 1);
        view.panel.settle();
        let refused = view.answer().expect("an answer").expect_err("a refusal");
        assert!(refused.contains("no file number 1"), "{refused}");
        assert!(view.asked.borrow().is_empty(), "nothing reached the editor");
    }

    #[test]
    fn nothing_is_written_by_a_view() {
        let view = ProposalPanel::new("glue-proposal-writes-nothing");
        view.view(&view.id, 0);
        view.panel
            .tick_until("the editor to be asked", || !view.asked.borrow().is_empty());
        assert_eq!(
            std::fs::read_to_string(view.root.join("a.rs")).unwrap(),
            "before\n",
            "the project's file is as it was"
        );
        let memory = view.panel.handle.state.borrow().editor_tools.memory().clone();
        assert!(
            memory.proposal(view.panel.tab.0, &view.id).is_some(),
            "viewing spends no proposal"
        );
    }

    /// A panel with a fake editor and one tab, whose session was given `editor_state` and `open_file` and is open.
    pub(in crate::agent_panel) struct Fixture {
        pub(in crate::agent_panel) handle: AgentPanelHandle,
        pub(in crate::agent_panel) page: Rc<RecordingPage>,
        pub(in crate::agent_panel) editor: Rc<FakeEditor>,
        pub(in crate::agent_panel) provider: Arc<RecordingProvider>,
        pub(in crate::agent_panel) tab: TabId,
        /// A file outside the tab's project: a call for it raises a card.
        outside: PathBuf,
    }

    pub(in crate::agent_panel) fn fixture(label: &str) -> Fixture {
        fixture_with(label, &[STATE_TOOL, OPEN_TOOL])
    }

    /// [`fixture`], its session given `nvim_exec` and started trusted, as a session must be for it to be offered.
    fn exec_fixture(label: &str) -> Fixture {
        let panel = fixture_with(label, &[STATE_TOOL, EXEC_TOOL]);
        let mut state = panel.handle.state.borrow_mut();
        let tab = state.tabs.get_mut(panel.tab).unwrap();
        tab.session.project_tiers = Some(ProjectTrust::Trusted);
        drop(state);
        panel
    }

    /// The fixture, in bypass by the way a person gets there: the mode key, then yes to its prompt.
    fn exec_fixture_in_bypass(label: &str) -> Fixture {
        let panel = exec_fixture(label);
        panel.enter_bypass();
        panel
    }

    /// [`fixture`] for a session given `tools`.
    fn fixture_with(label: &str, tools: &[&str]) -> Fixture {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let (handle, page) = settled_panel();
        let editor = Rc::new(FakeEditor::default());
        handle.set_editor_rpc(editor.clone());
        handle.state.borrow_mut().project_dir = dir.clone();
        let outside = agent::state_dirs::test_workspace_dir(&format!("{label}-outside")).join("notes.txt");
        std::fs::write(&outside, "notes\n").unwrap();
        let provider = Arc::new(RecordingProvider::client_tooling());
        let declared = |name: &str| agent::ClientToolDecl {
            name: name.into(),
            description: "reads the editor".into(),
            input_schema_json: r#"{"type":"object"}"#.into(),
            always_load: true,
        };
        let tools = agent::ClientToolSet {
            server_name: eitri_core::editor_tools::SERVER_NAME.into(),
            timeout_ms: eitri_core::editor_tools::CALL_TIMEOUT_MS,
            tools: tools.iter().copied().map(declared).collect(),
        };
        let conversation =
            agent::AgentConversation::create_with_tools(provider.clone(), &dir, ProjectTrust::Untrusted, Some(tools))
                .unwrap();
        let tab = {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Claude);
            state
                .tabs
                .get_mut(tab)
                .unwrap()
                .set_backend(TabBackend::Live(AgentBackend::Conversation {
                    agent: AgentId::Claude,
                    conversation: Box::new(conversation),
                }));
            tab
        };
        provider.open_session("claude-1", &dir);
        tick_handle_until(&handle, "the session's id", || {
            handle.state.borrow().tabs.broker_facts(tab).is_some()
        });
        Fixture {
            handle,
            page,
            editor,
            provider,
            tab,
            outside,
        }
    }

    impl Fixture {
        pub(in crate::agent_panel) fn tick_until(&self, what: &str, done: impl Fn() -> bool) {
            tick_handle_until(&self.handle, what, done);
        }

        /// A few rounds of the tick, for a test that says something did not happen.
        pub(in crate::agent_panel) fn settle(&self) {
            for _ in 0..4 {
                self.handle.tick();
            }
        }

        /// The cards of the newest `editor_cards` envelope the base page was sent.
        fn page_cards(&self) -> Vec<serde_json::Value> {
            let sent = self.page.sent.borrow();
            let newest = sent
                .iter()
                .rev()
                .map(|envelope| json_of(envelope))
                .find(|value| value["kind"] == "editor_cards" && value["tab"] == self.tab.0);
            newest.map_or_else(Vec::new, |value| value["cards"].as_array().unwrap().clone())
        }

        /// A call for the file outside the project; returns its card's id, once the base page shows it.
        pub(in crate::agent_panel) fn raise_a_card(&self, call_id: &str) -> String {
            self.provider.queue(AgentDomainEvent::ClientToolCall {
                call_id: call_id.into(),
                tool_use_id: Some(format!("toolu-{call_id}")),
                tool: OPEN_TOOL.into(),
                arguments_json: json!({ "path": self.outside }).to_string(),
                sequence: 1,
            });
            self.tick_until("the card", || !self.page_cards().is_empty());
            self.page_cards()[0]["id"].as_str().unwrap().to_owned()
        }

        /// An exec call; returns its card's id, once the base page shows it.
        fn raise_an_exec_card(&self, call_id: &str) -> String {
            self.provider.queue(AgentDomainEvent::ClientToolCall {
                call_id: call_id.into(),
                tool_use_id: Some(format!("toolu-{call_id}")),
                tool: EXEC_TOOL.into(),
                arguments_json: json!({ "kind": "lua", "code": "return 1", "why": "to see" }).to_string(),
                sequence: 1,
            });
            self.tick_until("the exec card", || !self.page_cards().is_empty());
            self.page_cards()[0]["id"].as_str().unwrap().to_owned()
        }

        fn enter_bypass(&self) {
            cycle_mode_from(&self.handle, ViewId::BASE, "mode-1", self.tab);
            let mut asked = last_bypass_prompt(&self.page);
            if asked.is_none() {
                cycle_mode_from(&self.handle, ViewId::BASE, "mode-2", self.tab);
                asked = last_bypass_prompt(&self.page);
            }
            let (_, nonce) = asked.expect("the bypass prompt");
            confirm_bypass_from(&self.handle, ViewId::BASE, "confirm", self.tab, nonce);
            assert_eq!(mode_of(&self.handle, self.tab), SessionModeChoice::Bypass);
        }

        /// A card waits, its call has not run and the agent has heard nothing.
        fn assert_waits_on_a_person(&self, card: &str) {
            self.settle();
            assert_eq!(self.page_cards()[0]["id"], card);
            assert_eq!(self.held_cards(), 1);
            assert!(self.editor.asked().is_empty(), "nothing ran");
            assert!(self.answers().is_empty(), "the agent has heard nothing");
        }

        /// A person's answer from `view`'s page.
        pub(in crate::agent_panel) fn answer_card(&self, view: ViewId, request: &str, card: &str, decision: &str) {
            let message = json!({
                "type": "editor_card_answer", "request_id": request, "tab": self.tab.0, "card_id": card,
                "decision": decision, "origin": "key",
            });
            self.handle.page_message(view, &message.to_string());
        }

        /// How many cards the broker holds for the tab.
        pub(in crate::agent_panel) fn held_cards(&self) -> usize {
            self.handle.state.borrow().editor_tools.cards(self.tab).len()
        }

        fn tray(&self) -> usize {
            let state = self.handle.state.borrow();
            state.tabs.get(self.tab).unwrap().attention.attention().pending
        }

        fn newest_snapshot(&self) -> serde_json::Value {
            let sent = self.page.sent.borrow();
            let newest = sent
                .iter()
                .rev()
                .map(|envelope| json_of(envelope))
                .find(|value| value["kind"] == "snapshot" && value["tab"] == self.tab.0);
            newest.expect("a snapshot of the tab")
        }

        /// The page reloads and is rebuilt from a snapshot.
        fn reload(&self) -> serde_json::Value {
            let before = self.page.sent.borrow().len();
            self.handle.page_message(ViewId::BASE, READY);
            let sent_since = || {
                let sent = self.page.sent.borrow();
                sent[before..]
                    .iter()
                    .any(|envelope| json_of(envelope)["kind"] == "snapshot")
            };
            self.tick_until("the snapshot", sent_since);
            self.newest_snapshot()
        }

        fn is_idle(&self) -> bool {
            self.handle.state.borrow().editor_tools.is_idle()
        }

        fn answers(&self) -> Vec<agent::ClientToolAnswer> {
            self.provider.client_answers()
        }

        /// The id the tab's projection names its conversation by.
        fn conversation_named(&self) -> Option<String> {
            let state = self.handle.state.borrow();
            let backend = state.tabs.get(self.tab).unwrap().live().unwrap();
            let named = backend.projection().provider_session_id.clone();
            named
        }

        fn open_calls(&self) -> usize {
            let state = self.handle.state.borrow();
            let backend = state.tabs.get(self.tab).unwrap().live().unwrap();
            backend.open_client_calls().len()
        }

        fn folded_resyncs(&self) -> u64 {
            let state = self.handle.state.borrow();
            let AgentBackend::Conversation { conversation, .. } = state.tabs.get(self.tab).unwrap().live().unwrap()
            else {
                unreachable!("the fixture holds a conversation")
            };
            conversation.ingest_stats().resyncs
        }

        /// Waits, ticking nothing, until the UI queue has overflowed: the next drain is a `Resync`.
        fn wait_for_overflow(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.folded_resyncs() == 0 {
                assert!(Instant::now() < deadline, "the queue never overflowed");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// What a `Bridge` caller would do: only a sidecar's calls come through the pump.
        fn submit_a_bridge_call(&self) {
            let mut state = self.handle.state.borrow_mut();
            let state = &mut *state;
            let env = PanelEnv {
                tabs: &state.tabs,
                way: None,
                root: &state.project_dir,
                home: None,
                context: &state.editor_context,
                review_roots: &state.review_roots,
            };
            let caller = Caller::Bridge {
                id: BridgeCallId {
                    tab: self.tab.0,
                    connection: 1,
                    call: 1,
                },
            };
            let outs = state
                .editor_tools
                .submit(self.tab, caller, None, STATE_TOOL, "{}", &env, Instant::now());
            assert!(outs.is_empty(), "the call was refused: {outs:?}");
        }
    }

    pub(in crate::agent_panel) fn call(id: &str, tool: &str) -> AgentDomainEvent {
        AgentDomainEvent::ClientToolCall {
            call_id: id.into(),
            tool_use_id: Some(format!("toolu-{id}")),
            tool: tool.into(),
            arguments_json: "{}".into(),
            sequence: 1,
        }
    }

    pub(in crate::agent_panel) fn state_call(id: &str) -> AgentDomainEvent {
        call(id, STATE_TOOL)
    }

    pub(in crate::agent_panel) fn state_reply() -> serde_json::Value {
        json!({"buffers": [], "other": 0, "left_out": 0, "current": {"line": 3}, "mode": "n"})
    }

    /// More than the UI queue holds, so the drain after `head` is a `Resync` and its events are gone.
    fn overflowing(head: Vec<AgentDomainEvent>) -> Vec<AgentDomainEvent> {
        let filler = (0..agent::UI_EVENT_QUEUE_CAPACITY + 20).map(|i| AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
        head.into_iter().chain(filler).collect()
    }

    fn started() -> AgentDomainEvent {
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() }
    }

    /// A card holds its call out of the editor until the person answers, and the answer lets it go on from there.
    #[test]
    fn an_outside_path_raises_a_card_and_its_answer_runs_the_call() {
        let panel = fixture("glue-card");
        let card = panel.raise_a_card("c1");
        let shown = panel.page_cards();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0]["tool"], OPEN_TOOL);
        assert_eq!(shown[0]["toolUseId"], "toolu-c1");
        assert_eq!(panel.tray(), 1);
        panel.settle();
        assert!(panel.editor.asked().is_empty(), "nothing ran before the answer");
        assert!(panel.answers().is_empty());

        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(result_for(&panel.page.sent.borrow(), "card-1"), Some(Ok(())));
        assert!(panel.page_cards().is_empty(), "the page drops the card it answered");
        assert_eq!(panel.tray(), 0);

        panel.tick_until("the call to reach the editor", || panel.editor.asked().len() == 1);
        assert_eq!(
            panel.editor.asked(),
            [eitri_core::editor_tools::tools::open_file::OPEN_FILE_LUA]
        );
        panel
            .editor
            .answer_oldest(&json!({"shown": true, "where": "agent_window", "line": 1}));
        panel.tick_until("the answer to reach the agent", || !panel.answers().is_empty());
        let answers = panel.answers();
        assert_eq!(answers[0].call_id, "c1");
        assert!(!answers[0].is_error, "{:?}", answers[0].text);
        assert!(panel.is_idle());
    }

    #[test]
    fn a_denied_card_runs_nothing_and_tells_the_agent() {
        let panel = fixture("glue-card-denied");
        let card = panel.raise_a_card("c1");
        panel.answer_card(ViewId::BASE, "card-1", &card, "deny");
        panel.tick_until("the refusal", || !panel.answers().is_empty());
        let answers = panel.answers();
        assert!(answers[0].is_error);
        assert_eq!(answers[0].text, [eitri_core::editor_tools::words::NOT_ALLOWED]);
        assert!(panel.editor.asked().is_empty());
    }

    /// The agent gave the call up, so the card asks about nothing, and a key already on its way meets no card.
    #[test]
    fn a_settled_call_takes_its_card_away() {
        let panel = fixture("glue-card-settled");
        let card = panel.raise_a_card("c1");
        panel
            .provider
            .queue(AgentDomainEvent::ClientToolCallSettled { call_id: "c1".into() });
        panel.tick_until("the card to go", || panel.page_cards().is_empty());
        assert_eq!(panel.tray(), 0);
        assert!(panel.is_idle());

        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(format!("no such card: {card}")))
        );
        panel.settle();
        assert!(panel.editor.asked().is_empty());
        assert!(panel.answers().is_empty());
    }

    /// No settled event comes when the stream is lost, and the card must not outlive the call.
    #[test]
    fn a_lost_stream_takes_its_cards_away() {
        let panel = fixture("glue-card-lost");
        panel.raise_a_card("c1");
        panel.provider.queue(AgentDomainEvent::SessionUnavailable {
            reason: "the stream ended".into(),
        });
        panel.tick_until("the card to go", || panel.page_cards().is_empty());
        assert_eq!(panel.tray(), 0);
        assert!(panel.is_idle());
        assert!(panel.answers().is_empty(), "nobody is left to answer");
    }

    /// `/clear` starts a new conversation in the same process, so an agent still running in the background keeps its
    /// calls, a card included. The id stands empty until the new conversation opens, which ends nothing either.
    #[test]
    fn a_clear_leaves_a_running_agents_calls_open() {
        let panel = fixture("glue-clear");
        let card = panel.raise_a_card("c1");
        panel.provider.queue(AgentDomainEvent::ConversationReset {
            new_conversation_id: "claude-2".into(),
            trigger: None,
        });
        panel.tick_until("the reset to fold", || {
            panel.conversation_named().as_deref() == Some("claude-2")
        });
        panel.settle();
        assert_eq!(panel.page_cards().len(), 1, "the card stays on the page");
        assert_eq!(panel.held_cards(), 1);
        assert_eq!(panel.tray(), 1);

        panel.provider.queue(state_call("c2"));
        panel.tick_until("a call made before the new conversation opens", || {
            panel.editor.asked().len() == 1
        });
        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("its answer", || !panel.answers().is_empty());
        assert_eq!(panel.answers()[0].call_id, "c2");
        assert!(!panel.answers()[0].is_error, "{:?}", panel.answers()[0].text);

        let cwd = panel.handle.state.borrow().project_dir.clone();
        panel.provider.open_session("claude-2", &cwd);
        panel.tick_until("the new conversation to open", || {
            let state = panel.handle.state.borrow();
            let facts = state.tabs.broker_facts(panel.tab);
            let backend = state.tabs.get(panel.tab).and_then(Tab::live);
            facts.is_some_and(|facts| facts.session == "claude-2")
                && backend.is_some_and(|backend| backend.provider_session_id().is_some())
        });
        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(result_for(&panel.page.sent.borrow(), "card-1"), Some(Ok(())));
        panel.tick_until("the call behind the card", || panel.editor.asked().len() == 2);
        panel
            .editor
            .answer_oldest(&json!({"shown": true, "where": "agent_window", "line": 1}));
        panel.tick_until("the first call's answer", || panel.answers().len() == 2);
        assert_eq!(panel.answers()[1].call_id, "c1");
        assert!(!panel.answers()[1].is_error, "{:?}", panel.answers()[1].text);
        assert!(panel.is_idle());
    }

    /// A reload rebuilds the page from a snapshot, and no projection holds a card.
    #[test]
    fn the_cards_come_back_in_a_snapshot() {
        let panel = fixture("glue-card-snapshot");
        let card = panel.raise_a_card("c1");
        let snapshot = panel.reload();
        assert_eq!(snapshot["state"]["editorCards"][0]["id"], card.as_str(), "{snapshot}");
        assert!(
            !snapshot["state"]["pendingPermissions"].to_string().contains(&card),
            "it is never a provider's card"
        );

        panel.answer_card(ViewId::BASE, "card-1", &card, "deny");
        let snapshot = panel.reload();
        assert!(snapshot["state"].get("editorCards").is_none(), "{snapshot}");
    }

    /// The tool runs in the editor beside nothing else, and what it read goes back under the call's own id.
    #[test]
    fn a_call_reaches_the_editor_and_its_answer_reaches_the_agent() {
        let panel = fixture("glue-call");
        panel.provider.queue(state_call("c1"));
        panel.tick_until("the call to reach the editor", || panel.editor.asked().len() == 1);
        assert_eq!(
            panel.editor.asked(),
            [eitri_core::editor_tools::tools::editor_state::EDITOR_STATE_LUA]
        );
        assert!(panel.answers().is_empty(), "the editor has not answered yet");

        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("the answer to reach the agent", || !panel.answers().is_empty());
        let answers = panel.answers();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].call_id, "c1");
        assert!(!answers[0].is_error, "{:?}", answers[0].text);
        assert!(answers[0].text[0].contains("normal"), "{:?}", answers[0].text);

        panel.settle();
        assert_eq!(panel.editor.asked().len(), 1, "a call still open is not run twice");
        assert_eq!(panel.answers().len(), 1);
    }

    fn finish_edit(panel: &Fixture, id: &str, path: &Path) {
        panel.provider.queue_all(vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: id.into(),
                name: "Write".into(),
                input: json!({"file_path":path}),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: id.into(),
                content: json!("ok"),
                is_error: false,
            },
        ]);
    }

    #[test]
    fn a_following_tab_shows_each_edit_through_the_broker() {
        use eitri_core::editor_tools::tools::open_file::OPEN_FILE_LUA;
        let panel = fixture("glue-follow");
        let root = panel.handle.state.borrow().project_dir.clone();
        panel
            .handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(panel.tab)
            .unwrap()
            .following = true;
        for n in 1..=2 {
            let path = root.join(format!("{n}.rs"));
            std::fs::write(&path, "changed\n").unwrap();
            finish_edit(&panel, &format!("edit-{n}"), &path);
            panel.tick_until("the file to reach the editor", || panel.editor.asked().len() == n);
            assert_eq!(panel.editor.asked().last(), Some(&OPEN_FILE_LUA));
            panel.editor.answer_oldest(&json!({"needs_load":true}));
            panel.tick_until("the follow to end", || panel.is_idle());
            assert_eq!(panel.held_cards(), 0);
            assert!(panel.answers().is_empty(), "following answers nobody");
        }
        let path = root.join("3.rs");
        std::fs::write(&path, "changed\n").unwrap();
        panel
            .handle
            .state
            .borrow_mut()
            .editor_follower
            .edited(panel.tab.0, path);
        let mut out = Outbox::default();
        tab_leaving(&mut panel.handle.state.borrow_mut(), panel.tab, &mut out);
        assert!(panel.handle.state.borrow().editor_follower.is_idle());
    }

    /// The end of a follow run reaches the review flow's live drawings with the file it opened, so a file the editor
    /// did not show at the edit is asked about again once following has opened it.
    #[test]
    fn live_a_follow_runs_end_tells_the_review_flow_the_file_it_opened() {
        use eitri_core::editor_tools::follow::{FollowRun, FollowTarget};
        use eitri_core::editor_tools::memory::Memory;
        use eitri_core::editor_tools::step::{AgentCall, EditorReach, EditorSight, Step, StepEvent, ToolCx, ToolRun};
        let flow = eitri_core::turn_review::ReviewFlow::new();
        let ends = flow.follow_ends();
        let root = PathBuf::from("/project");
        let memory = Memory::default();
        let cx = ToolCx {
            root: &root,
            home: None,
            memory: &memory,
            trust: ProjectTrust::Trusted,
            mode: agent::PermissionMode::Bypass,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 3,
            session: "s1",
            call: "follow",
            agent_call: AgentCall::Host,
        };
        let target = |rel: &str| FollowTarget {
            rel: PathBuf::from(rel),
            line: None,
        };

        let never_started = FollowEnding::new(FollowRun::new(&root, target("b.rs")), 3, &root, ends.clone());
        drop(never_started);
        assert!(ends.take().is_empty(), "a run that opened nothing tells nothing");

        let mut run = FollowEnding::new(FollowRun::new(&root, target("src/a.rs")), 3, &root, ends.clone());
        assert!(matches!(run.step(StepEvent::Start, &cx), Step::Call { .. }));
        assert!(ends.take().is_empty(), "nothing is told while the run is out");
        drop(run);
        assert_eq!(ends.take(), vec![(3, PathBuf::from("src/a.rs"))]);
    }

    /// The follow runs the glue's poll starts are the ones that tell the review flow, at their end, the file they
    /// opened.
    #[test]
    fn live_the_follow_runs_poll_starts_tell_the_review_flow_where_they_end() {
        use eitri_core::editor_tools::tools::open_file::OPEN_FILE_LUA;
        let panel = fixture("glue-follow-ends");
        let ends = panel.handle.state.borrow().review_flow.follow_ends();
        let path = panel.handle.state.borrow().project_dir.join("edited.rs");
        std::fs::write(&path, "changed\n").unwrap();
        panel
            .handle
            .state
            .borrow_mut()
            .editor_follower
            .edited(panel.tab.0, path);
        // The glue's own poll alone: the panel's tick would hand the end to the live drawings at once.
        let poll_once = || poll(&mut panel.handle.state.borrow_mut(), &mut Outbox::default());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while panel.editor.asked().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the follow never reached the editor"
            );
            poll_once();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(panel.editor.asked(), [OPEN_FILE_LUA]);
        assert!(ends.take().is_empty(), "nothing is told while the run is out");
        panel.editor.answer_oldest(&json!({"needs_load":true}));
        let mut told = Vec::new();
        while told.is_empty() {
            assert!(std::time::Instant::now() < deadline, "the follow's end was never told");
            poll_once();
            told.extend(ends.take());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(told, vec![(panel.tab.0, PathBuf::from("edited.rs"))]);
    }

    #[test]
    fn a_follow_queues_behind_a_call_in_flight() {
        use eitri_core::editor_tools::tools::open_file::OPEN_FILE_LUA;
        let panel = fixture("glue-follow-queue");
        panel.provider.queue(state_call("c1"));
        panel.tick_until("the first call", || panel.editor.asked().len() == 1);
        let path = panel.handle.state.borrow().project_dir.join("edited.rs");
        std::fs::write(&path, "changed\n").unwrap();
        panel
            .handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(panel.tab)
            .unwrap()
            .following = true;
        finish_edit(&panel, "edit", &path);
        panel.tick_until("the follow to queue", || {
            let state = panel.handle.state.borrow();
            let completed = state
                .tabs
                .get(panel.tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .tool_calls
                .iter()
                .any(|c| c.tool_use_id == "edit" && c.result.is_some());
            completed
        });
        panel.settle();
        assert_eq!(panel.editor.asked().len(), 1);
        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("the follow", || panel.editor.asked().len() == 2);
        assert_eq!(panel.editor.asked()[1], OPEN_FILE_LUA);
        panel.editor.answer_oldest(&json!({"needs_load":true}));
        panel.tick_until("the follow to end", || panel.is_idle());
        assert_eq!(panel.answers().len(), 1);
        assert_eq!(panel.held_cards(), 0);
    }

    /// A turn's beginning is found by the pump with the turn's first call, and the baseline of the turn goes to the
    /// editor ahead of that call.
    #[test]
    fn a_turn_start_takes_a_baseline_ahead_of_the_turns_first_call() {
        use eitri_core::editor_tools::tools::diagnostics::DIAGNOSTICS_LUA;
        use eitri_core::editor_tools::tools::editor_state::EDITOR_STATE_LUA;
        let panel = fixture_with("glue-baseline", &[STATE_TOOL, "diagnostics"]);
        panel.provider.queue_all(vec![
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            state_call("c1"),
        ]);
        panel.tick_until("the baseline to reach the editor", || !panel.editor.asked().is_empty());
        panel.settle();
        assert_eq!(
            panel.editor.asked(),
            [DIAGNOSTICS_LUA],
            "the call waits for the baseline"
        );

        panel
            .editor
            .answer_oldest(&json!({ "entries": [], "modified": [], "left_out": 0 }));
        panel.tick_until("the turn's call", || panel.editor.asked().len() == 2);
        assert_eq!(panel.editor.asked(), [DIAGNOSTICS_LUA, EDITOR_STATE_LUA]);
        assert!(panel.answers().is_empty(), "the baseline answered nobody");
        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("the call's answer", || !panel.answers().is_empty());
        assert_eq!(panel.answers().len(), 1);
        assert_eq!(panel.answers()[0].call_id, "c1");
    }

    /// A session that was not given `diagnostics` takes no baseline at a turn's start.
    #[test]
    fn a_session_without_diagnostics_takes_no_baseline() {
        let panel = fixture("glue-no-baseline");
        panel
            .provider
            .queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        panel.tick_until("the turn to be running", || {
            panel.handle.state.borrow().tabs.get(panel.tab).unwrap().turn_running()
        });
        panel.settle();
        assert!(panel.editor.asked().is_empty());
        assert!(panel.is_idle());
    }

    /// `tool_named` finds every row of the catalogue, so a name the session was never given would otherwise run.
    #[test]
    fn a_tool_not_declared_for_the_session_never_reaches_the_editor() {
        let panel = fixture("glue-undeclared");
        panel.provider.queue(call("c1", UNDECLARED_TOOL));
        panel.tick_until("the refusal", || !panel.answers().is_empty());
        let answers = panel.answers();
        assert!(answers[0].is_error);
        assert_eq!(answers[0].call_id, "c1");
        assert_eq!(
            answers[0].text,
            [format!("{UNDECLARED_TOOL} is not available in this session")]
        );
        panel.settle();
        assert!(panel.editor.asked().is_empty());
    }

    /// The overflow drops the batch's events, the call's among them; the projection still holds it.
    #[test]
    fn a_resync_loses_no_call() {
        let panel = fixture("glue-resync");
        panel.provider.queue_all(overflowing(vec![started(), state_call("c1")]));
        panel.wait_for_overflow();
        panel.tick_until("the call to reach the editor", || panel.editor.asked().len() == 1);
        assert_eq!(panel.open_calls(), 1);
        assert!(!panel.is_idle(), "the broker holds it");
    }

    #[test]
    fn a_settled_call_drops_a_queued_call_unrun() {
        let panel = fixture("glue-settled");
        panel.provider.queue(state_call("c1"));
        panel.provider.queue(state_call("c2"));
        panel.tick_until("the first call out", || {
            panel.editor.asked().len() == 1 && panel.open_calls() == 2
        });
        panel.handle.tick();

        panel
            .provider
            .queue(AgentDomainEvent::ClientToolCallSettled { call_id: "c2".into() });
        panel.tick_until("the settle to fold", || panel.open_calls() == 1);
        panel.handle.tick();

        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("the first answer", || !panel.answers().is_empty());
        panel.settle();
        assert_eq!(panel.editor.asked().len(), 1, "the queued call never ran");
        assert_eq!(panel.answers().len(), 1);
        assert_eq!(panel.answers()[0].call_id, "c1");
        assert!(panel.is_idle());
    }

    /// No settled event comes when the stream is lost: what ends these calls is the projection emptying.
    #[test]
    fn a_lost_stream_ends_the_tabs_calls() {
        let panel = fixture("glue-lost");
        panel.provider.queue(state_call("c1"));
        panel.provider.queue(state_call("c2"));
        panel.tick_until("one in flight and one queued", || {
            panel.editor.asked().len() == 1 && panel.open_calls() == 2
        });
        panel.handle.tick();
        assert!(!panel.is_idle());

        panel.provider.queue(AgentDomainEvent::SessionUnavailable {
            reason: "the stream ended".into(),
        });
        panel.tick_until("both calls to go", || panel.is_idle());
        panel.editor.answer_oldest(&state_reply());
        panel.settle();
        assert!(panel.answers().is_empty(), "nobody is left to answer");
        assert_eq!(panel.editor.asked().len(), 1, "the queued call never ran");
    }

    /// First answer wins, and the call may have ended while the editor ran it.
    #[test]
    fn a_refused_late_answer_is_not_a_failure() {
        let panel = fixture("glue-refused");
        panel
            .provider
            .refuse_client_answers(Some(agent::ProviderErrorCode::ClientToolCallAlreadyAnswered));
        panel.provider.queue(state_call("c1"));
        panel.tick_until("the call to reach the editor", || panel.editor.asked().len() == 1);
        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("the answer to be refused", || !panel.answers().is_empty());
        panel.settle();
        assert!(panel.is_idle());
        assert!(
            !sent_about(&panel.page, "error", panel.tab),
            "nothing to tell the person"
        );
        assert!(panel
            .handle
            .state
            .borrow()
            .tabs
            .get(panel.tab)
            .unwrap()
            .live()
            .is_some());

        panel.provider.refuse_client_answers(None);
        panel.provider.queue(state_call("c2"));
        panel.tick_until("the next call to run", || panel.editor.asked().len() == 2);
        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("its answer", || panel.answers().len() == 2);
        assert_eq!(panel.answers()[1].call_id, "c2");
    }

    /// The interrupt goes with the rest of the batch; the ending stays on the projection. Only a bridge call ends on
    /// it, so one waits here behind the sidecar's, which stays: a bridge call that outlived it would run next.
    #[test]
    fn an_interrupted_turn_reaches_the_broker_through_a_resync_too() {
        let panel = fixture("glue-interrupt");
        panel.provider.queue(state_call("c1"));
        panel.tick_until("the sidecar's call to reach the editor", || {
            panel.editor.asked().len() == 1
        });
        panel.submit_a_bridge_call();

        panel.provider.queue_all(overflowing(vec![
            started(),
            AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: agent::TurnOutcome::Interrupted,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
        ]));
        panel.wait_for_overflow();
        panel.handle.tick();

        panel.editor.answer_oldest(&state_reply());
        panel.tick_until("the sidecar's answer", || !panel.answers().is_empty());
        panel.settle();
        assert_eq!(panel.editor.asked().len(), 1, "the bridge call never ran");
        assert!(panel.is_idle());

        panel.submit_a_bridge_call();
        panel.settle();
        assert_eq!(
            panel.editor.asked().len(),
            2,
            "that interrupt ended the calls of its time, once"
        );
    }

    #[test]
    fn an_exec_card_in_bypass_waits_for_a_person() {
        let panel = exec_fixture_in_bypass("glue-exec-bypass");
        let card = panel.raise_an_exec_card("c1");
        let shown = panel.page_cards();
        assert_eq!(shown[0]["tool"], EXEC_TOOL);
        assert_eq!(shown[0]["toolUseId"], "toolu-c1");
        assert_eq!(panel.tray(), 1);
        panel.assert_waits_on_a_person(&card);
    }

    #[test]
    fn entering_bypass_leaves_an_exec_card_waiting() {
        let panel = exec_fixture("glue-exec-enter-bypass");
        let card = panel.raise_an_exec_card("c1");
        panel.provider.queue(write_card("perm-1"));
        panel.tick_until("the provider's card", || panel.tray() == 2);

        panel.enter_bypass();

        assert_eq!(
            panel.provider.resolutions(),
            vec![("perm-1".to_string(), true)],
            "entering bypass answered the provider's card and only that"
        );
        panel.assert_waits_on_a_person(&card);
    }

    #[test]
    fn a_bypass_resync_leaves_it_waiting() {
        let panel = exec_fixture_in_bypass("glue-exec-resync");
        let card = panel.raise_an_exec_card("c1");
        let mut events = overflowing(vec![started()]);
        events.push(write_card("lost-1"));
        panel.provider.queue_all(events);
        panel.wait_for_overflow();
        panel.tick_until("the resync's sweep", || !panel.provider.resolutions().is_empty());

        assert_eq!(panel.provider.resolutions(), vec![("lost-1".to_string(), true)]);
        panel.assert_waits_on_a_person(&card);
    }

    #[test]
    fn an_answer_from_the_card_path_runs_it_once() {
        let panel = exec_fixture_in_bypass("glue-exec-answer");
        let card = panel.raise_an_exec_card("c1");
        panel.assert_waits_on_a_person(&card);

        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(result_for(&panel.page.sent.borrow(), "card-1"), Some(Ok(())));
        panel.tick_until("the code to reach the editor", || panel.editor.asked().len() == 1);
        assert!(
            panel.editor.asked()[0].contains("=eitri-agent"),
            "the chunk that runs an approved text"
        );

        panel.answer_card(ViewId::BASE, "card-2", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-2"),
            Some(Err(format!("no such card: {card}")))
        );
        panel.editor.answer_oldest(&json!({ "output": "1" }));
        panel.tick_until("the answer to reach the agent", || !panel.answers().is_empty());
        panel.settle();

        let answers = panel.answers();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].text, ["1"]);
        assert!(!answers[0].is_error);
        assert_eq!(panel.editor.asked().len(), 1, "one answer ran it once");
        assert!(panel.is_idle());
    }

    #[test]
    fn a_tab_moving_withdraws_the_card_and_runs_nothing() {
        let panel = exec_fixture_in_bypass("glue-exec-moved");
        let card = panel.raise_an_exec_card("c1");
        let mut out = Outbox::default();
        tab_leaving(&mut panel.handle.state.borrow_mut(), panel.tab, &mut out);

        assert_eq!(panel.held_cards(), 0);
        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(format!("no such card: {card}")))
        );
        panel.settle();
        assert!(panel.editor.asked().is_empty());
        let answers = panel.answers();
        assert_eq!(answers.len(), 1);
        assert!(answers[0].is_error);
        assert_eq!(answers[0].text, [words::MOVED]);
    }

    #[test]
    fn the_ten_minute_limit_runs_nothing() {
        let panel = exec_fixture_in_bypass("glue-exec-limit");
        let card = panel.raise_an_exec_card("c1");
        {
            let mut state = panel.handle.state.borrow_mut();
            let state = &mut *state;
            let env = PanelEnv {
                tabs: &state.tabs,
                way: None,
                root: &state.project_dir,
                home: None,
                context: &state.editor_context,
                review_roots: &state.review_roots,
            };
            let past_it = Instant::now() + eitri_core::editor_tools::CARD_LIMIT + Duration::from_secs(1);
            let outs = state.editor_tools.tick(&env, past_it);
            route(state, outs, &mut Outbox::default());
        }

        assert_eq!(panel.held_cards(), 0);
        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(format!("no such card: {card}")))
        );
        panel.settle();
        assert!(panel.editor.asked().is_empty());
        let answers = panel.answers();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].text, [words::NOT_ANSWERED]);
    }

    #[test]
    fn a_settled_call_takes_the_card_and_runs_nothing() {
        let panel = exec_fixture_in_bypass("glue-exec-settled");
        let card = panel.raise_an_exec_card("c1");
        panel
            .provider
            .queue(AgentDomainEvent::ClientToolCallSettled { call_id: "c1".into() });
        panel.tick_until("the card to go", || panel.page_cards().is_empty());
        assert_eq!(panel.tray(), 0);

        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(format!("no such card: {card}")))
        );
        panel.settle();
        assert!(panel.editor.asked().is_empty());
        assert!(panel.answers().is_empty());
    }

    /// The path from a bridge's socket to the broker and back, over real sockets. Only Linux has the socket.
    #[cfg(target_os = "linux")]
    pub(in crate::agent_panel) mod bridged {
        use std::os::unix::net::UnixStream;

        use agent::EditorServerSpec;
        use eitri_core::editor_tools::bridge_wire::{read_frame, write_frame, Message, PROTOCOL};
        use eitri_core::editor_tools::switches::Switches;
        use eitri_core::editor_tools::tools::read_buffer;
        use eitri_core::test_providers::ExternalRecordingProvider;

        use super::*;

        /// `eitri-editor-bridge`, as far as a window can tell.
        pub(in crate::agent_panel) struct Peer {
            stream: UnixStream,
            /// Each frame the window sent, read on a thread of the peer's so a test can tick meanwhile; `None` at
            /// the connection's end.
            frames: mpsc::Receiver<Option<Message>>,
        }

        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
            }
        }

        impl Peer {
            /// Connects, says hello and returns the names of the tools the welcome lists.
            pub(in crate::agent_panel) fn connect(server: &EditorServerSpec) -> (Peer, Vec<String>) {
                let stream = UnixStream::connect(&server.args[0]).expect("the socket is bound");
                let mut reading = stream.try_clone().unwrap();
                let (sent, frames) = mpsc::channel();
                std::thread::spawn(move || {
                    while let Ok(Some(frame)) = read_frame(&mut reading) {
                        let message = Message::parse(frame).expect("a frame of the wire");
                        let _ = sent.send(Some(message));
                    }
                    let _ = sent.send(None);
                });
                let mut peer = Peer { stream, frames };
                peer.say(Message::Hello {
                    v: PROTOCOL,
                    key: server.args[1].clone(),
                });
                let Message::Welcome { tools } = peer.hear() else {
                    panic!("the first thing heard is the welcome")
                };
                let names = tools.iter().map(|tool| tool["name"].as_str().unwrap().to_owned());
                let names = names.collect();
                (peer, names)
            }

            fn say(&mut self, message: Message) {
                write_frame(&mut self.stream, &message.into_value()).unwrap();
            }

            pub(in crate::agent_panel) fn call(&mut self, id: u64, tool: &str, arguments: serde_json::Value) {
                self.say(Message::Call {
                    id,
                    name: tool.into(),
                    arguments,
                    meta: serde_json::Value::Null,
                });
            }

            /// A call as Codex makes it: `meta.callId` is the model's id, the row's.
            fn call_on_row(&mut self, id: u64, tool: &str, arguments: serde_json::Value, row: &str) {
                self.say(Message::Call {
                    id,
                    name: tool.into(),
                    arguments,
                    meta: json!({ "callId": row, "itemId": "fc_another_id" }),
                });
            }

            pub(in crate::agent_panel) fn hear(&mut self) -> Message {
                let heard = self.frames.recv_timeout(Duration::from_secs(5));
                heard.expect("the window said something").expect("a frame")
            }

            /// [`Peer::hear`] while the window ticks, for what a tick has to send.
            fn hear_while_ticking(&mut self, handle: &AgentPanelHandle) -> Message {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    handle.tick();
                    if let Ok(heard) = self.frames.try_recv() {
                        return heard.expect("a frame");
                    }
                    assert!(Instant::now() < deadline, "the window said nothing");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }

            /// The window ended the connection, with nothing left to read.
            pub(in crate::agent_panel) fn is_ended(&mut self) -> bool {
                matches!(self.frames.recv_timeout(Duration::from_secs(5)), Ok(None))
            }
        }

        fn refused(tool: &str) -> String {
            format!("{tool} is not available in this session")
        }

        /// A window whose program has a bridge beside it.
        pub(in crate::agent_panel) fn with_a_bridge_beside(handle: &AgentPanelHandle, dir: &Path) -> PathBuf {
            let beside = dir.join("program");
            std::fs::create_dir_all(&beside).unwrap();
            std::fs::write(beside.join(BRIDGE_BINARY), "").unwrap();
            let program = beside.join("eitri");
            handle.state.borrow_mut().bridge_beside = Some(program.clone());
            program
        }

        /// Opens a socket for `tab` directly, for a test about the socket and not about who starts a session.
        pub(in crate::agent_panel) fn serve(handle: &AgentPanelHandle, tab: TabId, dir: &Path) -> EditorServerSpec {
            with_a_bridge_beside(handle, dir);
            let rows = catalog::declared(
                &eitri_core::editor_tools::switches::Switches::on_by_default(),
                ProjectTrust::Trusted,
            );
            server_of_rows(&mut handle.state.borrow_mut(), tab, &rows).expect("a server")
        }

        /// A window with a fake editor and one live external tab, started as the panel starts one.
        struct External {
            handle: AgentPanelHandle,
            _page: Rc<RecordingPage>,
            editor: Rc<FakeEditor>,
            provider: Arc<ExternalRecordingProvider>,
            tab: TabId,
            server: EditorServerSpec,
        }

        fn empty_plan() -> agent::TrustPlan {
            agent::TrustPlan {
                dirs: Vec::new(),
                skills_off: false,
                fingerprint: String::new(),
                project_only: Vec::new(),
            }
        }

        fn external(
            label: &str,
            agent: AgentId,
            provider: ExternalRecordingProvider,
            trust: ProjectTrust,
            switches: Switches,
        ) -> External {
            external_of(label, agent, provider, trust, |state, tab, agent, trust| {
                state.editor_switches = switches;
                server_for_start(state, tab, agent, trust, EditorServerName::Free)
            })
        }

        fn external_of(
            label: &str,
            agent: AgentId,
            provider: ExternalRecordingProvider,
            trust: ProjectTrust,
            serve: impl FnOnce(&mut AgentPanelState, TabId, &AgentId, ProjectTrust) -> Option<EditorServerSpec>,
        ) -> External {
            agent::state_dirs::redirect_state_to_a_test_root();
            let dir = agent::state_dirs::test_workspace_dir(label);
            let (handle, page) = settled_panel();
            let editor = Rc::new(FakeEditor::default());
            handle.set_editor_rpc(editor.clone());
            with_a_bridge_beside(&handle, &agent::state_dirs::test_workspace_dir(&format!("{label}-bin")));
            let provider = Arc::new(provider);
            let (tab, server) = {
                let mut state = handle.state.borrow_mut();
                state.project_dir = dir.clone();
                let tab = state.tabs.open(agent.clone());
                let server = serve(&mut state, tab, &agent, trust).expect("the session is given a server");
                let backend = AgentBackend::start_external_serving(
                    agent,
                    provider.clone(),
                    &dir,
                    empty_plan(),
                    SessionModeChoice::Auto,
                    None,
                    Some(server.clone()),
                )
                .unwrap();
                let t = state.tabs.get_mut(tab).unwrap();
                t.session.project_tiers = Some(trust);
                t.set_backend(TabBackend::Live(backend));
                (tab, server)
            };
            tick_handle_until(&handle, "the session's id", || {
                handle.state.borrow().tabs.broker_facts(tab).is_some()
            });
            assert_eq!(provider.starts()[0].editor_server.as_ref(), Some(&server));
            External {
                handle,
                _page: page,
                editor,
                provider,
                tab,
                server,
            }
        }

        fn a_tool_off(tool: &str) -> Switches {
            Switches::parse(None, &[(format!("agent.editor.tool.{tool}"), "off".to_owned())]).unwrap()
        }

        fn codex(label: &str, switches: Switches) -> External {
            let provider = ExternalRecordingProvider::codex();
            external(label, AgentId::Codex, provider, ProjectTrust::Trusted, switches)
        }

        /// The whole way a start takes, from the gate to the installed backend, so what it stored is what the
        /// session then has. The name of the socket and its key are this session's and nobody else's.
        #[test]
        fn an_external_tab_is_given_a_server_with_its_socket_and_key() {
            let (state, fx) = trust_panel("glue-server");
            fx.write("AGENTS.md", b"codex instructions\n");
            let handed = Rc::new(RefCell::new(Vec::new()));
            let provider = Arc::new(ExternalRecordingProvider::codex());
            let program = agent::state_dirs::test_workspace_dir("glue-server-bin").join("eitri");
            std::fs::write(program.with_file_name(BRIDGE_BINARY), "").unwrap();
            let tab = {
                let mut s = state.borrow_mut();
                s.bridge_beside = Some(program.clone());
                let root = s.project_dir.clone();
                let (spawned, provider) = (handed.clone(), provider.clone());
                s.spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
                    let SpawnCall::External {
                        agent,
                        plan,
                        mode,
                        resume,
                        editor_server,
                        ..
                    } = call
                    else {
                        panic!("a Codex tab starts as another agent's")
                    };
                    spawned.borrow_mut().push(editor_server.clone());
                    let (sent, started) = mpsc::channel();
                    let provider: Arc<dyn agent::ExternalProvider + Send + Sync> = provider.clone();
                    let backend = AgentBackend::start_external_serving(
                        agent,
                        provider,
                        &root,
                        plan,
                        mode,
                        resume.as_deref(),
                        editor_server,
                    );
                    sent.send(backend).unwrap();
                    started
                })));
                let tab = s.tabs.open(AgentId::Codex);
                s.tabs.select(tab);
                tab
            };
            gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello codex")).unwrap();
            let prompt = next_prompt(&state);
            answer_prompt(&state, "a1", &prompt, true);
            tick_until(&state, |_| !handed.borrow().is_empty());
            let screen = Screen::read(&state, &Pages::default());
            collect_pending_starts(&mut state.borrow_mut(), &screen, &mut Outbox::default());
            tick_until(&state, |_| state.borrow().tabs.broker_facts(tab).is_some());

            let server = handed.borrow()[0].clone().expect("the start carries the server");
            assert_eq!(server.name, "eitri");
            assert_eq!(server.command, program.with_file_name(BRIDGE_BINARY));
            assert_eq!(server.cwd, fx.root);
            assert_eq!(server.args.len(), 2);
            let socket = Path::new(&server.args[0]).file_name().unwrap().to_str().unwrap();
            assert_eq!(socket.len(), "ed-.sock".len() + 16, "{socket}");
            assert!(socket.starts_with("ed-"), "{socket}");
            assert_eq!(server.args[1].len(), 32);
            assert_eq!(provider.starts()[0].editor_server.as_ref(), Some(&server));

            let every_tool: BTreeSet<String> = catalog::declared(&Switches::on_by_default(), ProjectTrust::Trusted)
                .iter()
                .map(|row| row.name.to_owned())
                .collect();
            let facts = state.borrow().tabs.broker_facts(tab).unwrap();
            assert_eq!(*facts.declared, every_tool, "the list outlives the backend's landing");

            let (_peer, tools) = Peer::connect(&server);
            assert_eq!(tools.into_iter().collect::<BTreeSet<_>>(), every_tool);

            let mut wrong = server.clone();
            wrong.args[1] = "0".repeat(32);
            let hello = Message::Hello {
                v: PROTOCOL,
                key: wrong.args[1].clone(),
            };
            let mut stranger = UnixStream::connect(&wrong.args[0]).unwrap();
            write_frame(&mut stranger, &hello.into_value()).unwrap();
            stranger.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            assert!(
                matches!(read_frame(&mut stranger), Ok(None)),
                "another key is not served"
            );
        }

        #[test]
        fn no_declared_tools_means_no_server() {
            let (handle, _page) = settled_panel();
            with_a_bridge_beside(&handle, &agent::state_dirs::test_workspace_dir("glue-no-tools"));
            let mut state = handle.state.borrow_mut();
            state.editor_switches = Switches::parse(Some("off"), &[]).unwrap();
            let tab = state.tabs.open(AgentId::Codex);
            assert_eq!(
                server_for_start(
                    &mut state,
                    tab,
                    &AgentId::Codex,
                    ProjectTrust::Trusted,
                    EditorServerName::Free
                ),
                None
            );
            assert!(state.editor_bridge.is_none(), "nothing was bound for it");
            assert!(state.tabs.editor_declared(tab).unwrap().is_empty());
        }

        /// The agent goes on without the tools, and the reason is what the log line says.
        #[test]
        fn no_binary_means_no_server_and_one_log_line() {
            let (handle, _page) = settled_panel();
            let empty = agent::state_dirs::test_workspace_dir("glue-no-binary");
            let mut state = handle.state.borrow_mut();
            state.bridge_beside = Some(empty.join("eitri"));
            let tab = state.tabs.open(AgentId::Codex);
            assert_eq!(
                server_for_start(
                    &mut state,
                    tab,
                    &AgentId::Codex,
                    ProjectTrust::Trusted,
                    EditorServerName::Free
                ),
                None
            );
            let why = bridge_binary(&state).unwrap_err();
            assert!(why.contains(BRIDGE_BINARY), "{why}");
            assert!(state.tabs.editor_declared(tab).unwrap().is_empty());
            assert!(state.editor_bridge_tabs.is_empty());
        }

        /// A configuration that went unread names no server, so the tab does not say one is there.
        #[test]
        fn a_name_only_unknown_says_the_configuration_went_unread() {
            let (handle, _page) = settled_panel();
            with_a_bridge_beside(&handle, &agent::state_dirs::test_workspace_dir("glue-unknown"));
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Codex);
            let start = server_for_start(
                &mut state,
                tab,
                &AgentId::Codex,
                ProjectTrust::Trusted,
                EditorServerName::Unknown,
            );
            assert_eq!(start, None);
            let said = state.tabs.take_editor_notes();
            assert_eq!(said.len(), 1);
            assert!(said[0].1.contains("could not read"), "{}", said[0].1);
            assert!(said[0].1.contains("may be in it"), "{}", said[0].1);
            assert!(!said[0].1.contains("a server named eitri is in"), "{}", said[0].1);
        }

        /// A runtime entry would merge with the one already there, so the tab goes without and says why, once.
        #[test]
        fn a_taken_name_leaves_a_codex_tab_without_a_server_and_it_says_so_once() {
            let (handle, _page) = settled_panel();
            with_a_bridge_beside(&handle, &agent::state_dirs::test_workspace_dir("glue-taken"));
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Codex);
            assert_eq!(
                server_for_start(
                    &mut state,
                    tab,
                    &AgentId::Codex,
                    ProjectTrust::Trusted,
                    EditorServerName::Defined
                ),
                None
            );
            assert!(state.editor_bridge.is_none(), "nothing was bound for it");
            assert!(state.tabs.editor_declared(tab).unwrap().is_empty());
            let said = state.tabs.take_editor_notes();
            assert_eq!(said.len(), 1);
            assert_eq!(said[0].0, tab);
            assert_eq!(
                said[0].1,
                "a server named eitri is in your Codex configuration, so the editor tools are off in this tab; rename it to have them"
            );
            state.tabs.owe_editor_note(tab);
            assert_eq!(
                state.tabs.take_editor_notes(),
                said,
                "a tab on no page keeps this line, not another"
            );
            assert_eq!(
                server_for_start(
                    &mut state,
                    tab,
                    &AgentId::Codex,
                    ProjectTrust::Trusted,
                    EditorServerName::Defined
                ),
                None
            );
            assert!(state.tabs.take_editor_notes().is_empty(), "once");
        }

        /// With every tool switched off there was nothing to give, so there is nothing to explain.
        #[test]
        fn a_taken_name_says_nothing_when_no_tool_was_on() {
            let (handle, _page) = settled_panel();
            let mut state = handle.state.borrow_mut();
            state.editor_switches = Switches::parse(Some("off"), &[]).unwrap();
            let tab = state.tabs.open(AgentId::Codex);
            assert_eq!(
                server_for_start(
                    &mut state,
                    tab,
                    &AgentId::Codex,
                    ProjectTrust::Trusted,
                    EditorServerName::Defined
                ),
                None
            );
            assert!(state.tabs.take_editor_notes().is_empty());
        }

        /// What Eitri knows is Codex's configuration; an ACP agent's own is not read, and its check reads as unknown.
        #[test]
        fn a_taken_name_holds_only_for_codex() {
            let panel = external_of(
                "glue-taken-acp",
                AgentId::Acp("gem".into()),
                ExternalRecordingProvider::named("acp:gem", false),
                ProjectTrust::Trusted,
                |state, tab, agent, trust| server_for_start(state, tab, agent, trust, EditorServerName::Defined),
            );
            assert_eq!(panel.server.name, "eitri");
        }

        /// The whole way a start takes, from the user's file read by the trust worker to what the provider is asked.
        #[test]
        fn a_switched_off_eitri_in_the_users_file_keeps_the_server_out_of_the_start() {
            let (state, fx) = trust_panel("glue-taken-file");
            fx.write("AGENTS.md", b"codex instructions\n");
            let codex_home = fx.outer.join("home/.codex");
            std::fs::create_dir_all(&codex_home).unwrap();
            std::fs::write(
                codex_home.join("config.toml"),
                "[mcp_servers.eitri]\nenabled = false\ncommand = \"mine\"\n",
            )
            .unwrap();
            let handed = Rc::new(RefCell::new(Vec::new()));
            let program = agent::state_dirs::test_workspace_dir("glue-taken-file-bin").join("eitri");
            std::fs::write(program.with_file_name(BRIDGE_BINARY), "").unwrap();
            let tab = {
                let mut s = state.borrow_mut();
                s.bridge_beside = Some(program.clone());
                let spawned = handed.clone();
                s.spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
                    let SpawnCall::External { editor_server, .. } = call else {
                        panic!("a Codex tab starts as an external one");
                    };
                    spawned.borrow_mut().push(editor_server);
                    let (_sent, started) = mpsc::channel();
                    started
                })));
                let tab = s.tabs.open(AgentId::Codex);
                s.tabs.select(tab);
                tab
            };
            gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello codex")).unwrap();
            let prompt = next_prompt(&state);
            answer_prompt(&state, "a1", &prompt, true);
            tick_until(&state, |_| !handed.borrow().is_empty());
            assert_eq!(
                handed.borrow()[0],
                None,
                "the file's entry would merge with a runtime one"
            );
            assert!(state.borrow().editor_bridge.is_none());
        }

        /// A new session never reuses what the last one was given.
        #[test]
        fn a_second_start_takes_a_new_socket_and_key() {
            let panel = codex("glue-second-start", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            let second = server_for_start(
                &mut panel.handle.state.borrow_mut(),
                panel.tab,
                &AgentId::Codex,
                ProjectTrust::Trusted,
                EditorServerName::Free,
            )
            .unwrap();
            assert_ne!(second.args, panel.server.args);
            assert!(peer.is_ended(), "a tab has one socket");
            assert!(UnixStream::connect(&panel.server.args[0]).is_err());
        }

        #[test]
        fn a_bridge_call_runs_through_the_broker_and_is_answered() {
            let panel = codex("glue-bridge-call", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            peer.call(7, STATE_TOOL, json!({}));
            tick_handle_until(&panel.handle, "the call to reach the editor", || {
                panel.editor.asked().len() == 1
            });
            panel.editor.answer_oldest(&state_reply());
            tick_handle_until(&panel.handle, "the broker to finish", || {
                panel.handle.state.borrow().editor_tools.is_idle()
            });
            let Message::Result { id, text, is_error } = peer.hear() else {
                panic!("a result")
            };
            assert_eq!(id, 7);
            assert!(!is_error, "{text}");
            assert!(text.contains("normal"), "{text}");
        }

        /// News from a real socket, taken here so that the window has not read it.
        fn news_of_a_call(panel: &External, peer: &mut Peer) -> Vec<BridgeNews> {
            peer.call(7, STATE_TOOL, json!({}));
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let state = panel.handle.state.borrow();
                let news = state.editor_bridge.as_ref().unwrap().take_news();
                if !news.is_empty() {
                    return news;
                }
                assert!(Instant::now() < until, "no news came");
                drop(state);
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        #[test]
        fn a_call_of_a_session_that_has_gone_reaches_no_broker() {
            let panel = codex("glue-old-session", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            let news = news_of_a_call(&panel, &mut peer);
            let mut bridges = panel.handle.state.borrow().editor_bridge_tabs.clone();
            assert_eq!(of_current_sessions(&bridges, news.clone()), news, "this session's own");

            let bridge = bridges.get_mut(&panel.tab).unwrap();
            bridge.name.hex = "0123456789abcdef".to_owned();
            assert!(
                of_current_sessions(&bridges, news.clone()).is_empty(),
                "its tab has another session's socket now"
            );
            bridges.clear();
            assert!(
                of_current_sessions(&bridges, news).is_empty(),
                "its tab's socket has gone"
            );
        }

        #[test]
        fn a_bridge_call_shares_the_tabs_one_flight() {
            let panel = fixture("glue-bridge-flight");
            let dir = panel.handle.state.borrow().project_dir.clone();
            let (mut peer, _) = Peer::connect(&serve(&panel.handle, panel.tab, &dir));
            panel.provider.queue(state_call("c1"));
            panel.tick_until("the sidecar's call to reach the editor", || {
                panel.editor.asked().len() == 1
            });
            peer.call(1, STATE_TOOL, json!({}));
            std::thread::sleep(Duration::from_millis(50));
            panel.settle();
            assert_eq!(panel.editor.asked().len(), 1, "the bridge's call waits its turn");

            panel.editor.answer_oldest(&state_reply());
            panel.tick_until("the bridge's call to reach the editor", || {
                panel.editor.asked().len() == 2
            });
            panel.editor.answer_oldest(&state_reply());
            panel.tick_until("both answers", || panel.is_idle());
            assert!(matches!(
                peer.hear(),
                Message::Result {
                    id: 1,
                    is_error: false,
                    ..
                }
            ));
            assert_eq!(panel.answers()[0].call_id, "c1");
        }

        /// One way of raising a card: a call through the bridge for the file outside the project. Returns the card.
        fn raise_a_bridge_card(panel: &Fixture, peer: &mut Peer, id: u64, row: &str) -> serde_json::Value {
            peer.call_on_row(id, OPEN_TOOL, json!({ "path": panel.outside }), row);
            panel.tick_until("the card", || !panel.page_cards().is_empty());
            panel.page_cards()[0].clone()
        }

        fn fixture_with_peer(label: &str) -> (Fixture, Peer) {
            let panel = fixture(label);
            let dir = panel.handle.state.borrow().project_dir.clone();
            let (peer, _) = Peer::connect(&serve(&panel.handle, panel.tab, &dir));
            (panel, peer)
        }

        /// The same card a sidecar's call raises: on the broker's list, in the tray and answered by its own id.
        #[test]
        fn a_bridge_card_outside_is_a_broker_card() {
            let (panel, mut peer) = fixture_with_peer("glue-bridge-card");
            let card = raise_a_bridge_card(&panel, &mut peer, 1, "call_1");
            assert_eq!(card["tool"], OPEN_TOOL);
            assert_eq!(panel.tray(), 1);
            assert_eq!(panel.held_cards(), 1);
            panel.settle();
            assert!(panel.editor.asked().is_empty(), "nothing ran before the answer");

            panel.answer_card(ViewId::BASE, "card-1", card["id"].as_str().unwrap(), "allow");
            panel.tick_until("the call to reach the editor", || panel.editor.asked().len() == 1);
            panel
                .editor
                .answer_oldest(&json!({"shown": true, "where": "agent_window", "line": 1}));
            panel.tick_until("the broker to finish", || panel.is_idle());
            assert!(matches!(
                peer.hear(),
                Message::Result {
                    id: 1,
                    is_error: false,
                    ..
                }
            ));
        }

        #[test]
        fn a_codex_call_id_puts_the_card_on_its_row() {
            let (panel, mut peer) = fixture_with_peer("glue-bridge-row");
            let card = raise_a_bridge_card(&panel, &mut peer, 1, "call_row_9");
            assert_eq!(card["toolUseId"], "call_row_9");
        }

        #[test]
        fn a_cancel_takes_a_bridge_card_away() {
            let (panel, mut peer) = fixture_with_peer("glue-bridge-cancel");
            raise_a_bridge_card(&panel, &mut peer, 1, "call_1");
            peer.say(Message::Cancel { id: 1 });
            panel.tick_until("the card to go", || panel.page_cards().is_empty());
            assert_eq!(panel.tray(), 0);
            assert!(panel.is_idle());

            peer.call(2, STATE_TOOL, json!({}));
            panel.tick_until("the next call to reach the editor", || panel.editor.asked().len() == 1);
            panel.editor.answer_oldest(&state_reply());
            panel.tick_until("the broker to finish", || panel.is_idle());
            assert!(
                matches!(peer.hear(), Message::Result { id: 2, .. }),
                "a cancelled call is answered nothing"
            );
        }

        fn end_the_turn_interrupted(panel: &Fixture) {
            panel.provider.queue(started());
            panel.provider.queue(AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: agent::TurnOutcome::Interrupted,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
        }

        /// A Codex before 0.165 sends no cancel for an interrupted call and none ends its item, so the turn's ending
        /// has to.
        #[test]
        fn an_interrupt_takes_a_bridge_card_away() {
            let (panel, mut peer) = fixture_with_peer("glue-bridge-interrupt");
            raise_a_bridge_card(&panel, &mut peer, 1, "call_1");
            end_the_turn_interrupted(&panel);
            panel.tick_until("the card to go", || panel.page_cards().is_empty());
            assert_eq!(panel.tray(), 0);
            let Message::Result { id, text, is_error } = peer.hear() else {
                panic!("a result")
            };
            assert_eq!(id, 1);
            assert!(is_error);
            assert_eq!(text, words::INTERRUPTED);
        }

        /// Codex from 0.165 cancels the call too, and the cancel and the turn's ending reach the panel by different
        /// ways, so either may come first. Cancel first: the turn's ending has nothing left to answer.
        #[test]
        fn the_turns_end_after_a_cancel_answers_nothing() {
            let (panel, mut peer) = fixture_with_peer("glue-bridge-cancel-first");
            raise_a_bridge_card(&panel, &mut peer, 1, "call_1");
            peer.say(Message::Cancel { id: 1 });
            panel.tick_until("the card to go", || panel.page_cards().is_empty());
            end_the_turn_interrupted(&panel);

            // Told `INTERRUPTED` because the turn ended so: heard first, so the cancelled call was answered nothing.
            peer.call(2, STATE_TOOL, json!({}));
            let Message::Result { id, text, .. } = peer.hear_while_ticking(&panel.handle) else {
                panic!("a result")
            };
            assert_eq!(id, 2);
            assert_eq!(text, words::INTERRUPTED);
            assert!(panel.is_idle());
            assert_eq!(panel.tray(), 0);
        }

        /// The turn's ending first: it answers the call once, and the cancel behind it finds nothing.
        #[test]
        fn a_cancel_after_the_turns_end_changes_nothing() {
            let (panel, mut peer) = fixture_with_peer("glue-bridge-cancel-last");
            raise_a_bridge_card(&panel, &mut peer, 1, "call_1");
            end_the_turn_interrupted(&panel);
            panel.tick_until("the card to go", || panel.page_cards().is_empty());
            let Message::Result { id, text, .. } = peer.hear_while_ticking(&panel.handle) else {
                panic!("a result")
            };
            assert_eq!(id, 1);
            assert_eq!(text, words::INTERRUPTED);

            peer.say(Message::Cancel { id: 1 });
            peer.call(2, STATE_TOOL, json!({}));
            let Message::Result { id, .. } = peer.hear_while_ticking(&panel.handle) else {
                panic!("a result")
            };
            assert_eq!(id, 2, "the cancelled call was answered twice");
            assert!(panel.is_idle());
            assert_eq!(panel.tray(), 0);
        }

        /// The news of a tab frozen for a move can only be late: nothing of it starts here.
        #[test]
        fn a_call_reaching_a_frozen_tab_starts_nothing() {
            let panel = codex("glue-bridge-frozen", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            tick_handle_until(&panel.handle, "the freeze", || {
                panel.handle.state.borrow_mut().tabs.freeze_for_move(panel.tab).is_ok()
            });
            peer.call(1, STATE_TOOL, json!({}));
            let Message::Result { id, text, is_error } = peer.hear_while_ticking(&panel.handle) else {
                panic!("a result")
            };
            assert_eq!(id, 1);
            assert!(is_error);
            assert_eq!(text, words::MOVED);
            assert!(panel.editor.asked().is_empty());
        }

        #[test]
        fn closing_a_tab_ends_its_socket() {
            let panel = codex("glue-bridge-close", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            panel.handle.run(|state, out| close_tab(state, panel.tab, out)).unwrap();
            assert!(peer.is_ended());
            assert!(UnixStream::connect(&panel.server.args[0]).is_err());
            assert!(panel.handle.state.borrow().editor_bridge_tabs.is_empty());
        }

        #[test]
        fn a_session_that_ends_takes_its_socket_with_it() {
            let panel = codex("glue-bridge-ended", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            {
                let mut state = panel.handle.state.borrow_mut();
                let retired = state.tabs.fail_session(
                    panel.tab,
                    "it stopped".into(),
                    eitri_core::tab_set::FailedHow::AfterStart,
                    true,
                );
                state.retiring.backend(retired.expect("the tab held a session"));
            }
            tick_handle_until(&panel.handle, "the socket to end", || {
                panel.handle.state.borrow().editor_bridge_tabs.is_empty()
            });
            assert!(peer.is_ended());
            assert!(UnixStream::connect(&panel.server.args[0]).is_err());
        }

        /// An agent that goes on its own leaves its backend in the tab, showing the end, and neither Codex nor an ACP
        /// agent says a call is over: this is all that ends one, and a card left up would still run what it approves.
        fn assert_the_end_takes_the_card_and_the_socket(label: &str, ending: AgentDomainEvent) {
            let panel = codex(label, Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            let outside = agent::state_dirs::test_workspace_dir(&format!("{label}-outside")).join("notes.txt");
            std::fs::write(&outside, "notes\n").unwrap();
            let cards = || panel.handle.state.borrow().editor_tools.cards(panel.tab);
            peer.call(1, OPEN_TOOL, json!({ "path": outside }));
            tick_handle_until(&panel.handle, "the card", || !cards().is_empty());
            let card = cards()[0].id.clone();

            panel.provider.inner.queue(ending);
            // A fold landing after the tick's pump ends the socket a tick ahead of the card; a person's answer in that
            // tick runs nothing, since the next one tells the broker before it steps the queue.
            tick_handle_until(&panel.handle, "the socket and the card to end", || {
                panel.handle.state.borrow().editor_bridge_tabs.is_empty() && cards().is_empty()
            });
            let answered = panel
                .handle
                .run(|state, out| answer_card(state, panel.tab, &card, CardDecision::Allow, CardOrigin::Key, out));
            assert!(answered.is_err(), "the card went with its session");
            for _ in 0..4 {
                panel.handle.tick();
            }
            assert!(panel.editor.asked().is_empty());
            assert!(peer.is_ended());
            assert!(UnixStream::connect(&panel.server.args[0]).is_err());
        }

        #[test]
        fn an_agent_that_exits_takes_its_card_and_socket() {
            let reason = "the child exited".to_owned();
            let ending = AgentDomainEvent::SessionUnavailable { reason };
            assert_the_end_takes_the_card_and_the_socket("glue-bridge-exit", ending);
        }

        #[test]
        fn an_agent_that_closes_takes_its_card_and_socket() {
            let reason = "the agent closed the session".to_owned();
            let ending = AgentDomainEvent::SessionClosed { reason };
            assert_the_end_takes_the_card_and_the_socket("glue-bridge-closed", ending);
        }

        /// What only the window's read of the socket has made late: the interrupt reached the broker a tick ago, with
        /// no sign of this call, and a Codex before 0.165 says nothing more about it.
        #[test]
        fn a_call_read_after_the_interrupt_starts_nothing() {
            let panel = codex("glue-bridge-late", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            let outside = agent::state_dirs::test_workspace_dir("glue-bridge-late-outside").join("notes.txt");
            std::fs::write(&outside, "notes\n").unwrap();
            interrupt_a_turn(&panel);

            peer.call(1, OPEN_TOOL, json!({ "path": outside }));
            let Message::Result { id, text, is_error } = peer.hear_while_ticking(&panel.handle) else {
                panic!("a result")
            };
            assert_eq!(id, 1);
            assert!(is_error);
            assert_eq!(text, words::INTERRUPTED);
            assert!(panel.handle.state.borrow().editor_tools.cards(panel.tab).is_empty());
            assert!(panel.editor.asked().is_empty());
        }

        #[test]
        fn a_new_turn_after_an_interrupt_takes_calls_again() {
            let panel = codex("glue-bridge-next-turn", Switches::on_by_default());
            let (mut peer, _) = Peer::connect(&panel.server);
            interrupt_a_turn(&panel);

            panel.provider.inner.queue(started());
            tick_handle_until(&panel.handle, "the next turn", || turn_state(&panel).0);
            peer.call(1, STATE_TOOL, json!({}));
            tick_handle_until(&panel.handle, "the call to reach the editor", || {
                panel.editor.asked().len() == 1
            });
        }

        /// Whether a turn runs, and whether one has ended other than by completing.
        fn turn_state(panel: &External) -> (bool, bool) {
            let state = panel.handle.state.borrow();
            let backend = state.tabs.get(panel.tab).unwrap().live().unwrap();
            let projection = backend.projection();
            (projection.active_turn_id.is_some(), !projection.turn_endings.is_empty())
        }

        /// A turn ends interrupted, and a few ticks hand that to the broker.
        fn interrupt_a_turn(panel: &External) {
            panel.provider.inner.queue(started());
            panel.provider.inner.queue(AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: agent::TurnOutcome::Interrupted,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
            tick_handle_until(&panel.handle, "the turn's end", || turn_state(panel).1);
            for _ in 0..4 {
                panel.handle.tick();
            }
        }

        /// A tool off by its switch is not in the list the bridge serves, and the bridge passes any name it is sent.
        /// Past the list, it would be found: it is a row of the catalogue.
        fn assert_refused_through_the_bridge(panel: &External, tool: &str) {
            let (mut peer, tools) = Peer::connect(&panel.server);
            assert!(!tools.iter().any(|name| name == tool), "{tools:?}");
            assert!(
                eitri_core::editor_tools::tools::tool_named(tool).is_some(),
                "a name the catalogue does not hold proves nothing"
            );
            peer.call(1, tool, json!({ "path": "main.rs" }));
            let Message::Result { id, text, is_error } = peer.hear_while_ticking(&panel.handle) else {
                panic!("a result")
            };
            assert_eq!(id, 1);
            assert!(is_error);
            assert_eq!(text, refused(tool));
            assert!(panel.editor.asked().is_empty(), "it never reached the editor");
        }

        #[test]
        fn a_bridge_call_naming_a_tool_that_is_off_is_refused() {
            let panel = codex("glue-off-tool", a_tool_off("read_buffer"));
            assert_refused_through_the_bridge(&panel, "read_buffer");
        }

        #[test]
        fn a_tool_off_is_refused_through_the_bridge() {
            let panel = codex("glue-exec-off", Switches::on_by_default());
            assert_refused_through_the_bridge(&panel, EXEC_TOOL);
        }

        #[test]
        fn an_acp_bridge_call_naming_an_undeclared_tool_is_refused() {
            let panel = external(
                "glue-acp-undeclared",
                AgentId::Acp("gem".into()),
                ExternalRecordingProvider::named("acp:gem", false),
                ProjectTrust::Trusted,
                a_tool_off("diagnostics"),
            );
            assert_refused_through_the_bridge(&panel, "diagnostics");
        }

        /// A row of a real tool, kept out of an `n` session as `nvim_exec` is, for a test of the gap between the two
        /// lists: a session's list leaves it out, and the catalogue still finds this name.
        static NOT_IN_UNTRUSTED: ToolRow = ToolRow {
            in_untrusted: false,
            ..read_buffer::ROW
        };
        static STATE_ROW: ToolRow = eitri_core::editor_tools::tools::editor_state::ROW;

        #[test]
        fn a_tool_an_n_session_leaves_out_is_refused_through_the_bridge() {
            let panel = external_of(
                "glue-n-session",
                AgentId::Codex,
                ExternalRecordingProvider::codex(),
                ProjectTrust::Untrusted,
                |state, tab, _, trust| {
                    let on = Switches::on_by_default();
                    let rows = catalog::declared_among(&[&NOT_IN_UNTRUSTED, &STATE_ROW], &on, trust);
                    assert_eq!(rows.len(), 1, "the row stays out of the session");
                    server_of_rows(state, tab, &rows)
                },
            );
            assert_refused_through_the_bridge(&panel, read_buffer::ROW.name);
        }
    }
}
