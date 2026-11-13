//! A remote observer's messages (a phone's): their own entry point into the panel, apart from the pages'.
//!
//! A remote message names the tab it is about by the tab's row key and never by a view: the observer is not a page,
//! so nothing it sends is handled as a page's (`page_message`), meets the page check or the window's gate, or is
//! answered with a page's `command_result`. It parses only as a [`RemoteInbound`], a closed list of what may come
//! from afar, so every kind a page may send and the list does not name (a card's answer, a question's, a trust
//! answer, a mode change, a shell line) fails to parse. What it caused is told to the page that shows its tab,
//! naming the device.
//!
//! The arms are written for the remote path, not borrowed from the pages': a phone's words go to a live Claude tab
//! in Manual or Auto only, as they came (never composed with the editor's context, so `@words` read no buffer), and
//! never as a command (`/`) or a shell line (`!`); the desk's draft and its history are not touched. A call that
//! waits on the provider (a stop, a send, a task's stop) runs off this thread: the message is answered
//! [`RemoteAccepted::Pending`] and its result follows from the tick ([`poll_remote`]), with the desk's notice.

use super::*;
use crate::remote_audit_sink::AuditSink;
use eitri_core::remote_audit::{text_digest, DeviceRef, Event as AuditEvent, Reason, TextDigest};
use eitri_core::remote_rv::COMMAND_ID_MAX_BYTES;
use eitri_core::tab_set::RemoteAsker;
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};

/// Who sent a remote message: the device it was enrolled as and the connection it came on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOrigin {
    /// The device's id in the device store, which its command ids are kept under.
    pub device_id: String,
    /// The device's name, as the page that shows the tab is told it.
    pub device: String,
    pub conn: u64,
    pub authority: Option<eitri_core::remote_devices::AuthorizationStamp>,
}

/// The work a stop was asked about, so a stop that arrives after that work has ended stops nothing: a turn by its id
/// and the session it ran in, or a stretch of work the panel cannot show by its number. Both are as the observer's
/// frames named them ([`eitri_core::tab_set::WorkMark`]); the session is needed because a turn id is the agent's and a
/// session that replaces another in the tab may reuse it, while the stretch number never repeats in a tab.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpectWork {
    Turn {
        id: String,
        /// The tab's backend generation ([`eitri_core::tab_set::Tab::backend_generation`]).
        generation: u64,
    },
    Unseen(u64),
}

/// Every message a remote observer may send. Anything else, and any field not named here, fails to parse.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteInbound {
    /// Stops the work running in the tab, if it is still the work named.
    Interrupt {
        command_id: String,
        expect_work: ExpectWork,
    },
    /// Sends `text` as the tab's next turn.
    Send { command_id: String, text: String },
    /// Queues `text` behind the turn running in the tab.
    Queue { command_id: String, text: String },
    /// Stops the tab's background work.
    StopBackground { command_id: String },
    /// Stops the tab's background task `task`.
    StopTask { command_id: String, task: String },
    /// Takes this device's queued items back out of the tab's queue.
    TakeBack { command_id: String },
}

impl RemoteInbound {
    /// The id the sender gave this command; each is taken once per device.
    pub fn command_id(&self) -> &str {
        match self {
            RemoteInbound::Interrupt { command_id, .. }
            | RemoteInbound::Send { command_id, .. }
            | RemoteInbound::Queue { command_id, .. }
            | RemoteInbound::StopBackground { command_id }
            | RemoteInbound::StopTask { command_id, .. }
            | RemoteInbound::TakeBack { command_id } => command_id,
        }
    }

    /// The kind's word, as the message names it.
    pub(super) fn kind(&self) -> &'static str {
        match self {
            RemoteInbound::Interrupt { .. } => "interrupt",
            RemoteInbound::Send { .. } => "send",
            RemoteInbound::Queue { .. } => "queue",
            RemoteInbound::StopBackground { .. } => "stop_background",
            RemoteInbound::StopTask { .. } => "stop_task",
            RemoteInbound::TakeBack { .. } => "take_back",
        }
    }

    /// The text the message carries, for the kinds that carry one.
    fn text(&self) -> Option<&str> {
        match self {
            RemoteInbound::Send { text, .. } | RemoteInbound::Queue { text, .. } => Some(text),
            RemoteInbound::Interrupt { .. }
            | RemoteInbound::StopBackground { .. }
            | RemoteInbound::StopTask { .. }
            | RemoteInbound::TakeBack { .. } => None,
        }
    }
}

/// Why a remote message did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteRefusal {
    /// Not a remote message: too long, not JSON, a kind or a field the list does not have, or an unusable command id.
    Parse,
    /// No tab of this window has that key.
    NoSuchTab,
    /// The tab's agent is not Claude.
    NotClaude,
    /// The tab holds no session, or is being moved to another window.
    NotLive,
    /// The work the message was about is no longer the work running.
    WorkChanged,
    /// This device sent this command id before.
    Duplicate,
    /// The agent turned the command down or failed it, in its own words.
    Agent(String),
    /// A kind that reached a path which does not carry it out.
    NotYet,
    /// The tab's mode is Bypass, or wider than when a queued message was taken.
    ModeNotAllowed,
    /// The tab's session has not started; a phone never starts one.
    NotStarted,
    /// The text's first non-blank character is `/` (a command) or `!` (a shell line).
    SlashOrBang,
    /// The text is longer than [`REMOTE_TEXT_MAX_BYTES`].
    TooLong,
    /// Nothing this device queued is waiting in the tab.
    NotYours,
    /// The tab cannot take it now, and why: a start or a send under way, a card or a `!` line holding it, its
    /// queue waiting to go, too many of the phone's calls out, or this device's queued messages in the tab at their
    /// cap (`eitri_core::tab_set::REMOTE_QUEUE_FULL`).
    Busy(String),
}

/// What became of a remote message that was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAccepted {
    /// Carried out.
    Done,
    /// A provider call is running; its result comes later from [`AgentPanelHandle::take_remote_results`].
    Pending,
}

/// The result of a remote command that came after it was taken: a call that was [`RemoteAccepted::Pending`], or a
/// queued message the tab took out unsent once its mode had widened (a second result for the same command, which
/// was answered [`RemoteAccepted::Done`] when it was queued).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteResult {
    /// The connection the command came on.
    pub conn: u64,
    pub command_id: String,
    pub outcome: Result<(), RemoteRefusal>,
}

/// The most of a window's remote calls out at once; past it a command is refused, and no thread is made for it.
pub(super) const REMOTE_CALLS_MAX: usize = 16;

/// The most results a window keeps for the socket to take; past it the oldest go, and their commands show on the
/// phone as not confirmed.
pub(super) const REMOTE_RESULTS_MAX: usize = 256;

/// The longest text a `send` or `queue` carries, in UTF-8 bytes.
pub const REMOTE_TEXT_MAX_BYTES: usize = 64 * 1024;

/// How many of a device's command ids are remembered: its last 1024. The phone never sends a command again by
/// itself (a result that never came shows as not confirmed, and the person decides), so an id older than that is
/// never a retry.
pub const COMMAND_LEDGER_PER_DEVICE: usize = 1024;

/// The longest remote message read; anything longer is refused unread. It holds a text of
/// [`REMOTE_TEXT_MAX_BYTES`] with every byte escaped (six bytes each on the wire), and the rest of the message.
pub(super) const REMOTE_MESSAGE_MAX_BYTES: usize = 6 * REMOTE_TEXT_MAX_BYTES + 4096;

/// Parses `raw` as a remote message, and nothing else.
pub(super) fn parse_remote_inbound(raw: &str) -> Result<RemoteInbound, RemoteRefusal> {
    if raw.len() > REMOTE_MESSAGE_MAX_BYTES {
        return Err(RemoteRefusal::Parse);
    }
    let message: RemoteInbound = serde_json::from_str(raw).map_err(|_| RemoteRefusal::Parse)?;
    let id = message.command_id();
    if id.is_empty() || id.len() > COMMAND_ID_MAX_BYTES {
        return Err(RemoteRefusal::Parse);
    }
    if message.text().is_some_and(|text| text.len() > REMOTE_TEXT_MAX_BYTES) {
        return Err(RemoteRefusal::TooLong);
    }
    Ok(message)
}

/// Whether `text` would run as a command (`/`) or a shell line (`!`) at the desk: its first non-blank character.
fn is_slash_or_bang(text: &str) -> bool {
    text.trim_start().starts_with(['/', '!'])
}

/// One device's remembered command ids: the set to look them up, and their order to forget the oldest.
#[derive(Default)]
struct DeviceLedger {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl DeviceLedger {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

/// The last [`COMMAND_LEDGER_PER_DEVICE`] command ids each device has sent, kept for the life of the process: a
/// command whose result never reached the phone may be sent again by the person, never by a retry that repeats the
/// id. Devices come from the store, so their number is bounded there.
#[derive(Default)]
struct RemoteCommands {
    by_device: HashMap<String, DeviceLedger>,
}

impl RemoteCommands {
    /// Records `id` for `device`, forgetting that device's oldest when it has more than the bound; false when the
    /// device sent it among its last ids.
    fn first_use(&mut self, device: &str, id: &str) -> bool {
        let ledger = self.by_device.entry(device.to_owned()).or_default();
        if !ledger.ids.insert(id.to_owned()) {
            return false;
        }
        ledger.order.push_back(id.to_owned());
        if ledger.order.len() > COMMAND_LEDGER_PER_DEVICE {
            if let Some(oldest) = ledger.order.pop_front() {
                ledger.ids.remove(&oldest);
            }
        }
        true
    }
}

thread_local! {
    /// Every window's ledger: a panel handle never leaves the thread that made it, and the process runs its windows
    /// on its one UI thread, so this thread's ledger is the process's, and a window closed or opened again forgets
    /// nothing.
    static REMOTE_COMMANDS: std::cell::RefCell<RemoteCommands> = std::cell::RefCell::default();
}

/// Records command `id` from `device` in the process's ledger; false when that device sent it before.
pub(super) fn first_command_use(device: &str, id: &str) -> bool {
    REMOTE_COMMANDS.with_borrow_mut(|commands| commands.first_use(device, id))
}

/// A parsed remote message from `origin` about the tab named `key`: admitted ([`admit_remote`] for a stop of the
/// turn, [`admit_remote_arm`] for the rest), then its arm. No view is taken and no request is answered on a page; the
/// page that shows the tab gets a notice of what was done, once it was.
pub(super) fn handle_remote_message(
    state: &mut AgentPanelState,
    screen: &Screen,
    origin: &RemoteOrigin,
    key: &eitri_core::tab_set::RowKey,
    message: RemoteInbound,
    out: &mut Outbox,
) -> Result<RemoteAccepted, RemoteRefusal> {
    let tab = match &message {
        RemoteInbound::Interrupt { .. } => admit_remote(state, &message, key)?,
        _ => admit_remote_arm(state, &message, key)?,
    };
    // A phone's send is a provider call too, kept by the tab set while it is out.
    let calls_full = state.remote.calls.len() + state.tabs.remote_sends_out() >= REMOTE_CALLS_MAX;
    let full = || RemoteRefusal::Busy("too many of the phone's calls are out: wait for their results".to_owned());
    let asker = |command_id: String| RemoteAsker {
        device_id: origin.device_id.clone(),
        device: origin.device.clone(),
        command_id,
        conn: Some(origin.conn),
        authority: origin.authority.clone(),
    };
    match message {
        RemoteInbound::Interrupt { command_id, .. } => {
            if calls_full {
                return Err(full());
            }
            // The agent's work only: a `!` line the person started at the desk is theirs to stop.
            let stopped_id = running_turn(state, tab);
            let answer = spawned_answer(state);
            match interrupt_agent_spawned(state, tab, answer.done) {
                Ok(()) => {
                    let generation = generation_of(state, tab);
                    state.remote.wait(
                        tab,
                        generation,
                        origin,
                        command_id,
                        Call::Stop,
                        stopped_id,
                        Answer::Spawned(answer.rx),
                    );
                    Ok(RemoteAccepted::Pending)
                }
                Err(error) => {
                    let why = RemoteRefusal::Agent(error.message.clone());
                    apply_outcome(state, screen, tab, None, Err(error), out);
                    Err(why)
                }
            }
        }
        RemoteInbound::Send { command_id, text } => {
            if state.tabs.get(tab).is_some_and(Tab::turn_running) {
                return Err(RemoteRefusal::WorkChanged);
            }
            if calls_full {
                return Err(full());
            }
            state
                .tabs
                .send_remote(tab, &text, asker(command_id))
                .map_err(RemoteRefusal::Busy)?;
            Ok(RemoteAccepted::Pending)
        }
        RemoteInbound::Queue { command_id, text } => {
            let Some(turn) = running_turn(state, tab) else {
                return Err(RemoteRefusal::WorkChanged);
            };
            let now = super::inbound::now_ms();
            state
                .tabs
                .queue_remote(tab, &text, asker(command_id), now)
                .map_err(RemoteRefusal::Busy)?;
            // Into the running turn when the agent can take it, else held for its end; never waits on the agent.
            state.tabs.steer_held(tab, now, Some(&turn));
            send_queue(state, tab, out);
            tell_desk(state, tab, &format!("queued from {}", origin.device), out);
            Ok(RemoteAccepted::Done)
        }
        RemoteInbound::StopBackground { .. } => {
            // The grammar names no count: what runs now is what is stopped.
            let shown = state
                .tabs
                .get(tab)
                .and_then(|t| t.live())
                .map_or(0, |backend| backend.projection().background_calls);
            let stopped_ids = state
                .tabs
                .get(tab)
                .and_then(Tab::live)
                .map(|backend| {
                    backend
                        .projection()
                        .background_tasks
                        .rows
                        .iter()
                        .filter(|row| row.live && !row.task.ambient)
                        .map(|row| row.task.task_id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            match state.tabs.stop_background(tab, shown) {
                Ok(()) => {
                    for id in stopped_ids {
                        note_stop(state, tab, id, origin);
                    }
                    tell_desk(
                        state,
                        tab,
                        &format!("background work stopped from {}", origin.device),
                        out,
                    );
                    Ok(RemoteAccepted::Done)
                }
                Err(error) => {
                    let why = RemoteRefusal::Agent(error.message.clone());
                    apply_outcome(state, screen, tab, None, Err(error), out);
                    Err(why)
                }
            }
        }
        RemoteInbound::StopTask { command_id, task } => {
            if calls_full {
                return Err(full());
            }
            let reply = backend_for(&mut state.tabs, tab)
                .and_then(|backend| backend.stop_task(&task))
                .map_err(|error| RemoteRefusal::Agent(error.message))?;
            let generation = generation_of(state, tab);
            state.remote.wait(
                tab,
                generation,
                origin,
                command_id,
                Call::StopTask,
                Some(task),
                Answer::Task(reply),
            );
            Ok(RemoteAccepted::Pending)
        }
        RemoteInbound::TakeBack { .. } => {
            let taken = state.tabs.take_back_remote(tab, &origin.device_id);
            if taken.is_empty() {
                return Err(RemoteRefusal::NotYours);
            }
            send_queue(state, tab, out);
            let told = match taken.len() {
                1 => format!("{} took back its queued message", origin.device),
                n => format!("{} took back its {n} queued messages", origin.device),
            };
            tell_desk(state, tab, &told, out);
            Ok(RemoteAccepted::Done)
        }
    }
}

/// Admits every remote kind but `interrupt` (which [`admit_remote`] admits): a message resolves its tab by key, and
/// only a Claude tab takes one. Words (`send`, `queue`) go only to a live session in Manual or Auto, never as a
/// command or a shell line; a stop goes to a live session in any mode, a task's only while that task is listed;
/// `take_back` needs only the tab, since it finds nothing of this device's anywhere else. Changes nothing.
pub(super) fn admit_remote_arm(
    state: &AgentPanelState,
    message: &RemoteInbound,
    key: &eitri_core::tab_set::RowKey,
) -> Result<TabId, RemoteRefusal> {
    if message.text().is_some_and(is_slash_or_bang) {
        return Err(RemoteRefusal::SlashOrBang);
    }
    let tab = state.tabs.tab_by_key(key).ok_or(RemoteRefusal::NoSuchTab)?;
    let held = state.tabs.get(tab).ok_or(RemoteRefusal::NoSuchTab)?;
    if matches!(message, RemoteInbound::TakeBack { .. }) {
        return Ok(tab);
    }
    if *held.agent() != AgentId::Claude {
        return Err(RemoteRefusal::NotClaude);
    }
    let live = match held.backend() {
        TabBackend::NotStarted => return Err(RemoteRefusal::NotStarted),
        TabBackend::Live(backend) if !state.tabs.is_moving(tab) && held.pending_handoff.is_none() => backend,
        _ => return Err(RemoteRefusal::NotLive),
    };
    match message {
        RemoteInbound::Send { .. } | RemoteInbound::Queue { .. } => match held.mode() {
            SessionModeChoice::Manual | SessionModeChoice::Auto => Ok(tab),
            SessionModeChoice::Bypass => Err(RemoteRefusal::ModeNotAllowed),
        },
        RemoteInbound::StopBackground { .. } => Ok(tab),
        RemoteInbound::StopTask { task, .. } => {
            let listed = live
                .projection()
                .background_tasks
                .rows
                .iter()
                .any(|row| row.task.task_id == *task);
            if listed {
                Ok(tab)
            } else {
                Err(RemoteRefusal::WorkChanged)
            }
        }
        RemoteInbound::Interrupt { .. } | RemoteInbound::TakeBack { .. } => {
            unreachable!("admitted by admit_remote and above")
        }
    }
}

/// The backend generation of `tab` now ([`Tab::backend_generation`]).
fn generation_of(state: &AgentPanelState, tab: TabId) -> u64 {
    state.tabs.get(tab).map_or(0, Tab::backend_generation)
}

/// The turn running in `tab` as its projection shows it, if one is.
fn running_turn(state: &AgentPanelState, tab: TabId) -> Option<String> {
    let backend = state.tabs.get(tab)?.live()?;
    let turn = backend.projection().active_turn_id.clone();
    turn
}

/// Tells the page that shows `tab` what a phone did there; while no ready page shows it, the notice waits until
/// one does ([`send_owed_notices`]).
fn tell_desk(state: &mut AgentPanelState, tab: TabId, text: &str, out: &mut Outbox) {
    match state.page_of(tab) {
        Some(view) => out.send(view, eitri_core::agent_bridge::serialize_notice_for_js(text)),
        None => state.remote.owe_notice(tab, text),
    }
}

/// The notices about a phone's doings owed to tabs a ready page shows now, oldest first; those of tabs that have
/// left the window are forgotten.
fn send_owed_notices(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.remote.notices.is_empty() {
        return;
    }
    let owed = std::mem::take(&mut state.remote.notices);
    for (tab, text) in owed {
        if state.tabs.get(tab).is_none() {
            continue;
        }
        match state.page_of(tab) {
            Some(view) => out.send(view, eitri_core::agent_bridge::serialize_notice_for_js(&text)),
            None => state.remote.notices.push_back((tab, text)),
        }
    }
}

/// [`interrupt_agent`] for a remote stop, with the provider's call off this thread (`AgentBackend::interrupt_spawned`).
fn interrupt_agent_spawned(
    state: &mut AgentPanelState,
    tab: TabId,
    done: impl FnOnce(Result<(), BackendError>) + Send + 'static,
) -> Result<(), BackendError> {
    if state.tabs.is_moving(tab) {
        return Err(BackendError {
            message: eitri_core::tab_set::MOVING.to_string(),
            benign: true,
        });
    }
    backend_for(&mut state.tabs, tab)?.interrupt_spawned(done);
    Ok(())
}

/// A channel for a spawned call's outcome, whose sender raises the panel's wake once it is told.
struct SpawnedAnswer<F> {
    done: F,
    rx: mpsc::Receiver<Result<(), BackendError>>,
}

fn spawned_answer(state: &AgentPanelState) -> SpawnedAnswer<impl FnOnce(Result<(), BackendError>) + Send + 'static> {
    let (tx, rx) = mpsc::channel();
    let wake = state.wake.as_ref().map(crate::panel_wake::PanelWake::waker);
    let done = move |outcome| {
        let _ = tx.send(outcome);
        if let Some(wake) = wake {
            wake();
        }
    };
    SpawnedAnswer { done, rx }
}

/// What a remote call that waits on the provider was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Stop,
    StopTask,
}

/// Where a waiting call's outcome comes from.
enum Answer {
    Spawned(mpsc::Receiver<Result<(), BackendError>>),
    Task(agent::TaskReply<()>),
}

/// One remote call out to the provider.
struct Waiting {
    tab: TabId,
    /// The tab's backend generation when the call was made: an answer about a session the tab no longer holds is
    /// the phone's alone.
    generation: u64,
    origin: RemoteOrigin,
    command_id: String,
    call: Call,
    answer: Answer,
    stopped_id: Option<String>,
}

/// The most notices a window keeps for tabs no ready page shows; past it the oldest go.
pub(super) const REMOTE_NOTICES_MAX: usize = 64;

/// A window's remote calls out to the provider, the results the socket has not taken yet, and the desk's notices
/// owed to tabs no ready page shows.
#[derive(Default)]
pub(super) struct RemoteCalls {
    /// The window's audit log, once its `rv-` socket has pumped: what every command taken and every refusal is
    /// recorded in.
    pub(super) audit: Option<AuditSink>,
    calls: Vec<Waiting>,
    refusals: HashMap<(u64, String), (std::time::Instant, u64)>,
    results: std::collections::VecDeque<RemoteResult>,
    notices: std::collections::VecDeque<(TabId, String)>,
}

impl RemoteCalls {
    fn wait(
        &mut self,
        tab: TabId,
        generation: u64,
        origin: &RemoteOrigin,
        command_id: String,
        call: Call,
        stopped_id: Option<String>,
        answer: Answer,
    ) {
        self.calls.push(Waiting {
            tab,
            generation,
            origin: origin.clone(),
            command_id,
            call,
            answer,
            stopped_id,
        });
    }

    /// Keeps a result for the socket; the oldest goes past [`REMOTE_RESULTS_MAX`].
    fn result(&mut self, conn: Option<u64>, command_id: String, outcome: Result<(), RemoteRefusal>) {
        let Some(conn) = conn else {
            return;
        };
        if self.results.len() >= REMOTE_RESULTS_MAX {
            if let Some(dropped) = self.results.pop_front() {
                eprintln!(
                    "[agent_panel] remote: the result of command {} on connection {} was dropped untaken",
                    dropped.command_id, dropped.conn
                );
            }
        }
        self.results.push_back(RemoteResult {
            conn,
            command_id,
            outcome,
        });
    }

    /// Keeps a notice for `tab` until a ready page shows it; the oldest goes past [`REMOTE_NOTICES_MAX`].
    fn owe_notice(&mut self, tab: TabId, text: &str) {
        if self.notices.len() >= REMOTE_NOTICES_MAX {
            self.notices.pop_front();
        }
        self.notices.push_back((tab, text.to_owned()));
    }

    /// Every result kept so far, oldest first.
    pub(super) fn take_results(&mut self) -> Vec<RemoteResult> {
        self.results.drain(..).collect()
    }

    /// Whether a call is out: a task's stop raises no wake when it is answered.
    pub(super) fn needs_polling(&self) -> bool {
        !self.calls.is_empty()
    }
}

/// Called from the tick: the remote calls the provider answered, the phone's sends the tab set let go or refused,
/// and the phone's queued messages its mode no longer lets go. Each tells the page that shows its tab and keeps a
/// result for the socket; a result kept raises the wake, so the socket's pump takes it whichever of the two ran
/// first. Notices owed to tabs a page shows now go too. Never waits.
pub(super) fn poll_remote(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    send_owed_notices(state, out);
    let results_before = state.remote.results.len();
    poll_remote_answers(state, screen, out);
    let mut counts: Vec<(TabId, String, String, usize)> = Vec::new();
    for (tab, asker) in state.tabs.take_remote_withdrawn() {
        audit_refused(
            state,
            tab,
            &asker.device_id,
            &asker.device,
            "queue",
            Reason::RevokedToken,
        );
        if let Some((_, _, _, count)) = counts
            .iter_mut()
            .find(|(t, id, _, _)| *t == tab && *id == asker.device_id)
        {
            *count += 1;
        } else {
            counts.push((tab, asker.device_id, asker.device, 1));
        }
        state.remote.result(
            asker.conn,
            asker.command_id,
            Err(RemoteRefusal::Agent("device steering permission withdrawn".into())),
        );
    }
    for (tab, _, name, count) in counts {
        let noun = if count == 1 { "message" } else { "messages" };
        let told = format!("Withdrew {count} queued {noun} from {name}: device steering permission withdrawn");
        if let Some(backend) = state.tabs.get_mut(tab).and_then(Tab::live_mut) {
            backend.fold_notice(told);
        } else {
            tell_desk(state, tab, &told, out);
        }
        send_queue(state, tab, out);
    }
    send_owed_notices(state, out);
    if state.remote.results.len() != results_before {
        if let Some(wake) = &state.wake {
            wake.raise();
        }
    }
}

fn poll_remote_answers(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    let mut answered = Vec::new();
    state.remote.calls.retain_mut(|waiting| {
        use std::sync::mpsc::TryRecvError;
        let outcome = match &waiting.answer {
            Answer::Spawned(rx) => match rx.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Err(BackendError {
                    message: "the call ended before the agent answered".to_owned(),
                    benign: true,
                }),
            },
            Answer::Task(rx) => match rx.try_recv() {
                Ok(outcome) => outcome.map_err(|error| BackendError {
                    message: agent::providers::claude_sidecar::task_refusal(error),
                    benign: true,
                }),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Err(BackendError {
                    message: "task request ended before an answer arrived".to_owned(),
                    benign: true,
                }),
            },
        };
        answered.push((
            waiting.tab,
            waiting.generation,
            waiting.origin.clone(),
            waiting.command_id.clone(),
            waiting.call,
            waiting.stopped_id.clone(),
            outcome,
        ));
        false
    });
    for (tab, generation, origin, command_id, call, stopped_id, outcome) in answered {
        // An answer about a session the tab no longer holds (it closed, or another took its place) is the phone's
        // alone: applied, a failure would end the session that replaced it.
        let same_session = state
            .tabs
            .get(tab)
            .is_some_and(|t| t.backend_generation() == generation);
        let result = match outcome {
            Ok(()) => {
                if same_session {
                    if let Some(id) = stopped_id {
                        note_stop(state, tab, id, &origin);
                    }
                    let told = match call {
                        Call::Stop => format!("stopped from {}", origin.device),
                        Call::StopTask => format!("a background task stopped from {}", origin.device),
                    };
                    tell_desk(state, tab, &told, out);
                }
                Ok(())
            }
            Err(error) => {
                let why = RemoteRefusal::Agent(error.message.clone());
                if same_session {
                    apply_outcome(state, screen, tab, None, Err(error), out);
                }
                Err(why)
            }
        };
        if same_session {
            if let Err(why) = &result {
                refusal_notice_for(
                    state,
                    tab,
                    &origin.device_id,
                    &origin.device,
                    refusal_words(why),
                    std::time::Instant::now(),
                );
            }
        }
        log_refusal(&origin.device, &origin.device_id, origin.conn, &result);
        if let Err(why) = &result {
            let kind = match call {
                Call::Stop => "interrupt",
                Call::StopTask => "stop_task",
            };
            audit_refused(
                state,
                tab,
                &origin.device_id,
                &origin.device,
                kind,
                audit_reason(why, RefusedAt::Send),
            );
        }
        state.remote.result(Some(origin.conn), command_id, result);
    }
    for sent in state.tabs.poll_remote_sends() {
        let eitri_core::tab_set::RemoteSent {
            tab,
            asker,
            typed,
            outcome,
            same_session,
        } = sent;
        let result = match &outcome {
            Ok(()) => Ok(()),
            Err(error) if error.is_turn_busy() => Err(RemoteRefusal::WorkChanged),
            Err(error) => Err(RemoteRefusal::Agent(error.message.clone())),
        };
        if same_session && state.tabs.get(tab).is_some() {
            if result.is_ok() {
                tell_desk(state, tab, &format!("sent from {}", asker.device), out);
            }
            // As a desk's send: the tab's title, a failure that ends the session, the queue.
            super::routing::prompt_sent(state, screen, tab, None, &typed, outcome, out);
        }
        if same_session {
            if let Err(why) = &result {
                refusal_notice_for(
                    state,
                    tab,
                    &asker.device_id,
                    &asker.device,
                    refusal_words(why),
                    std::time::Instant::now(),
                );
            }
        }
        let conn = asker.conn.unwrap_or_default();
        log_refusal(&asker.device, &asker.device_id, conn, &result);
        if let Err(why) = &result {
            audit_refused(
                state,
                tab,
                &asker.device_id,
                &asker.device,
                "send",
                audit_reason(why, RefusedAt::Send),
            );
        }
        state.remote.result(asker.conn, asker.command_id, result);
    }
    for refused in state.tabs.take_remote_refused() {
        let eitri_core::tab_set::RemoteRefused {
            tab,
            asker,
            accepted_mode,
            mode,
        } = refused;
        let told = format!(
            "a message from {} was not sent: this tab is in {} now, wider than {} when it was taken",
            asker.device,
            mode_name(mode),
            mode_name(accepted_mode)
        );
        refusal_notice_for(
            state,
            tab,
            &asker.device_id,
            &asker.device,
            &told,
            std::time::Instant::now(),
        );
        send_queue(state, tab, out);
        audit_refused(
            state,
            tab,
            &asker.device_id,
            &asker.device,
            "queue",
            audit_reason(&RemoteRefusal::ModeNotAllowed, RefusedAt::Flush),
        );
        state
            .remote
            .result(asker.conn, asker.command_id, Err(RemoteRefusal::ModeNotAllowed));
    }
}

fn note_stop(state: &mut AgentPanelState, tab: TabId, id: String, origin: &RemoteOrigin) {
    if let Some(tab) = state.tabs.get_mut(tab) {
        tab.note_remote_stop(
            id,
            eitri_core::agent_bridge::SentFrom {
                device_id: origin.device_id.clone(),
                name: origin.device.clone(),
            },
        );
    }
}

/// Leaves the targeted tab a bounded refusal notice naming `device`, counted under the store's `device_id` with every
/// other refusal of that device on the tab, wherever it was decided.
pub fn refusal_notice(state: &mut AgentPanelState, tab: TabId, device_id: &str, device: &str, reason: &str) {
    refusal_notice_for(state, tab, device_id, device, reason, std::time::Instant::now());
}

pub(super) fn refusal_notice_for(
    state: &mut AgentPanelState,
    tab: TabId,
    device_id: &str,
    device: &str,
    reason: &str,
    now: std::time::Instant,
) {
    if state.tabs.get(tab).is_none() {
        return;
    }
    // Forget counters for closed tabs; a flood cannot retain their identities forever.
    state
        .remote
        .refusals
        .retain(|(id, _), _| state.tabs.get(TabId(*id)).is_some());
    let key = (tab.0, device_id.to_owned());
    if let Some((last, folded)) = state.remote.refusals.get_mut(&key) {
        if now.saturating_duration_since(*last) < std::time::Duration::from_secs(10) {
            *folded = folded.saturating_add(1);
            return;
        }
    }
    let folded = state.remote.refusals.insert(key, (now, 0)).map_or(0, |(_, n)| n);
    let suffix = if folded == 0 {
        String::new()
    } else {
        format!(" (and {folded} more)")
    };
    state
        .remote
        .owe_notice(tab, &format!("refused from {device}: {reason}{suffix}"));
}

pub(super) fn refusal_words(why: &RemoteRefusal) -> &str {
    match why {
        RemoteRefusal::Parse => "the command is malformed",
        RemoteRefusal::NoSuchTab => "the tab is gone",
        RemoteRefusal::NotClaude => "the tab is not a Claude tab",
        RemoteRefusal::NotLive => "the session is not live",
        RemoteRefusal::WorkChanged => "the work has changed",
        RemoteRefusal::Duplicate => "this command was already received",
        RemoteRefusal::Agent(reason) | RemoteRefusal::Busy(reason) => reason,
        RemoteRefusal::NotYet => "this command is unavailable",
        RemoteRefusal::ModeNotAllowed => "the tab's mode does not allow this message",
        RemoteRefusal::NotStarted => "the session has not started",
        RemoteRefusal::SlashOrBang => "commands and shell lines must come from the desk",
        RemoteRefusal::TooLong => "the message is too long",
        RemoteRefusal::NotYours => "this device has no held messages to take back",
    }
}

fn mode_name(mode: SessionModeChoice) -> &'static str {
    match mode {
        SessionModeChoice::Manual => "Manual",
        SessionModeChoice::Auto => "Auto",
        SessionModeChoice::Bypass => "Bypass",
    }
}

/// Where a refusal was decided: at the message's own handling (or its provider's answer), or at a flush that took a
/// held phone message out unsent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefusedAt {
    Send,
    Flush,
}

/// The audit log's word for a refusal. Every variant is named, so a new refusal cannot reach the log without one; an
/// agent's or the host's own words (`Agent`, `Busy`) are never recorded, since an agent's can quote the conversation.
pub(super) fn audit_reason(why: &RemoteRefusal, at: RefusedAt) -> Reason {
    match why {
        RemoteRefusal::Parse | RemoteRefusal::NotYet => Reason::NotAllowed,
        RemoteRefusal::NoSuchTab | RemoteRefusal::NotLive => Reason::TabNotLive,
        RemoteRefusal::NotClaude => Reason::NotClaude,
        RemoteRefusal::NotStarted => Reason::NotStarted,
        RemoteRefusal::WorkChanged => Reason::WorkChanged,
        RemoteRefusal::Duplicate => Reason::DuplicateCommand,
        RemoteRefusal::ModeNotAllowed => match at {
            RefusedAt::Send => Reason::Bypass,
            RefusedAt::Flush => Reason::ModeWidened,
        },
        RemoteRefusal::SlashOrBang => Reason::SlashOrBang,
        RemoteRefusal::TooLong => Reason::TooLong,
        RemoteRefusal::NotYours => Reason::NotYours,
        RemoteRefusal::Busy(_) => Reason::Busy,
        RemoteRefusal::Agent(_) => Reason::Agent,
    }
}

/// What a remote message was, read once it parsed, for its record: its kind, its command id, its text's length and
/// hash, and the tab's number and revision when the message was taken.
pub(super) struct Taken {
    pub(super) kind: &'static str,
    pub(super) command_id: String,
    pub(super) text: Option<TextDigest>,
    pub(super) at: Option<(u16, u64)>,
}

impl Taken {
    pub(super) fn of(message: &RemoteInbound) -> Taken {
        Taken {
            kind: message.kind(),
            command_id: message.command_id().to_owned(),
            text: message.text().map(text_digest),
            at: None,
        }
    }

    /// Notes the number and revision of the tab `key` names now, if one does.
    pub(super) fn read_tab(&mut self, state: &AgentPanelState, key: &eitri_core::tab_set::RowKey) {
        self.at = state
            .tabs
            .tab_by_key(key)
            .and_then(|tab| state.tabs.get(tab))
            .map(|held| {
                let revision = held.live().map_or(0, |backend| backend.projection().last_revision);
                (held.number, revision)
            });
    }
}

fn device_ref(device_id: &str, device: &str) -> DeviceRef {
    DeviceRef {
        id: device_id.to_owned(),
        name: device.to_owned(),
    }
}

/// Records what became of a remote message at its own handling ([`AgentPanelHandle::remote_message`]): a `cmd` for a
/// message taken (its text only as length and hash), else a `refused` with the kind when it parsed. Every message
/// passes here, whatever refused it.
pub(super) fn record_remote_message(
    state: &AgentPanelState,
    origin: &RemoteOrigin,
    key: &eitri_core::tab_set::RowKey,
    taken: Option<Taken>,
    outcome: &Result<RemoteAccepted, RemoteRefusal>,
) {
    let Some(audit) = &state.remote.audit else { return };
    match (outcome, taken) {
        (Ok(_), Some(taken)) => {
            let (tab, revision) = taken.at.unwrap_or((0, 0));
            audit.record(AuditEvent::Cmd {
                device: device_ref(&origin.device_id, &origin.device),
                kind: taken.kind.to_owned(),
                instance: audit.instance().to_owned(),
                key: key.as_str().to_owned(),
                tab,
                revision,
                command_id: taken.command_id,
                text: taken.text,
            })
        }
        (Ok(_), None) => {}
        (Err(why), taken) => audit.record(AuditEvent::Refused {
            reason: audit_reason(why, RefusedAt::Send),
            device: Some(device_ref(&origin.device_id, &origin.device)),
            listener: None,
            peer: None,
            key: Some(key.as_str().to_owned()),
            kind: taken.map(|t| t.kind.to_owned()),
            dropped: None,
        }),
    }
}

/// Records a refusal decided after the message was taken (a provider's answer, a send the tab set let go of, a held
/// message taken out at a flush), about `tab` when the window still holds it.
fn audit_refused(
    state: &AgentPanelState,
    tab: TabId,
    device_id: &str,
    device: &str,
    kind: &'static str,
    reason: Reason,
) {
    let Some(audit) = &state.remote.audit else { return };
    audit.record(AuditEvent::Refused {
        reason,
        device: Some(device_ref(device_id, device)),
        listener: None,
        peer: None,
        key: state.tabs.get(tab).map(|held| held.row_key().as_str().to_owned()),
        kind: Some(kind.to_owned()),
        dropped: None,
    });
}

fn log_refusal(device: &str, device_id: &str, conn: u64, result: &Result<(), RemoteRefusal>) {
    if let Err(why) = result {
        eprintln!("[agent_panel] remote {device} ({device_id}, connection {conn}): refused: {why:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use eitri_core::tab_set::RowKey;

    #[test]
    fn every_message_kind_is_a_recordable_command_kind() {
        let messages = [
            r#"{"type":"interrupt","command_id":"c","expect_work":{"unseen":1}}"#,
            r#"{"type":"send","command_id":"c","text":"t"}"#,
            r#"{"type":"queue","command_id":"c","text":"t"}"#,
            r#"{"type":"stop_background","command_id":"c"}"#,
            r#"{"type":"stop_task","command_id":"c","task":"b1"}"#,
            r#"{"type":"take_back","command_id":"c"}"#,
        ];
        let kinds: Vec<&str> = messages
            .iter()
            .map(|raw| serde_json::from_str::<RemoteInbound>(raw).unwrap().kind())
            .collect();
        assert_eq!(kinds, eitri_core::remote_audit::COMMAND_KINDS);
        for kind in kinds {
            assert_eq!(eitri_core::remote_audit::command_kind(kind), Some(kind));
        }
    }

    fn provenance_snapshot(handle: &AgentPanelHandle, tab: TabId) -> serde_json::Value {
        let payloads = handle.state.borrow_mut().tabs.state_payloads(tab);
        payloads
            .into_iter()
            .map(|s| serde_json::from_str::<serde_json::Value>(&s).unwrap())
            .find(|v| v["kind"] == "snapshot")
            .unwrap()
    }

    #[test]
    fn delivered_remote_send_is_noted_with_its_device() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("sent-from");
        let (handle, base, _) = handle_with_two_pages();
        let (tab, _, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "hello")), Ok(()));
        let snapshot = provenance_snapshot(&handle, tab);
        let seq = snapshot["state"]["userPrompts"][0]["seq"].as_u64().unwrap().to_string();
        assert_eq!(
            snapshot["state"]["sentFrom"][seq],
            serde_json::json!({"deviceId":"d1", "name":"Pixel 8"})
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn refusal_leaves_the_targeted_tab_a_notice() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("refusal-notice");
        let (handle, base, _) = handle_with_two_pages();
        let (_tab, _, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        clear(&[&base]);
        assert_eq!(
            handle.remote_message(&phone(), &key, &queue("q1", "hello")),
            Err(RemoteRefusal::WorkChanged)
        );
        handle.tick();
        assert_eq!(
            notices(&base.sent.borrow()),
            ["refused from Pixel 8: the work has changed"]
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn refusal_notices_are_coalesced() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("refusal-coalesced");
        let (handle, base, _) = handle_with_two_pages();
        let (tab, _, _) = interruptible_tab(&handle, &dir);
        handle.tick();
        clear(&[&base]);
        let now = std::time::Instant::now();
        refusal_notice(&mut handle.state.borrow_mut(), tab, "dev-1", "Pixel 8", "no grant");
        for _ in 0..99 {
            refusal_notice_for(&mut handle.state.borrow_mut(), tab, "dev-1", "Pixel 8", "no grant", now);
        }
        handle.tick();
        assert_eq!(notices(&base.sent.borrow()), ["refused from Pixel 8: no grant"]);
        refusal_notice_for(
            &mut handle.state.borrow_mut(),
            tab,
            "dev-1",
            "Pixel 8",
            "no grant",
            std::time::Instant::now() + std::time::Duration::from_secs(10),
        );
        handle.tick();
        assert_eq!(
            notices(&base.sent.borrow()),
            [
                "refused from Pixel 8: no grant",
                "refused from Pixel 8: no grant (and 99 more)"
            ],
            "the public entry and the window's own refusals share one counter per device id"
        );
        shut_down_every_tab(&handle);
    }

    fn sent_note(snapshot: &serde_json::Value, text: &str) -> serde_json::Value {
        let prompt = snapshot["state"]["userPrompts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["text"] == text)
            .unwrap();
        snapshot["state"]["sentFrom"][prompt["seq"].as_u64().unwrap().to_string()].clone()
    }

    #[test]
    fn steered_phone_message_is_noted() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-steer-note");
        let (handle, base, _) = handle_with_two_pages();
        let (tab, provider, key) = tab_of(&handle, &dir, RecordingProvider::steering());
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(settle(&handle, &phone(), &key, &queue("q1", "steer me")), Ok(()));
        let steer = provider.steers()[0].clone();
        provider.queue(agent::AgentDomainEvent::UserMessageDelivered {
            client_message_id: steer.client_message_id,
            turn_id: "t1".into(),
            text: "steer me".into(),
        });
        tick_handle_until(&handle, "the delivered message", || {
            queued_texts(&handle, tab).is_empty()
        });
        assert_eq!(
            sent_note(&provenance_snapshot(&handle, tab), "steer me")["deviceId"],
            "d1"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn flushed_phone_message_is_noted() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-flush-note");
        let (handle, base, _) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(settle(&handle, &phone(), &key, &queue("q1", "flush me")), Ok(()));
        end_turn(&handle, tab, &provider, "t1");
        tick_handle_until(&handle, "the flushed message", || provider.turns().len() == 1);
        assert_eq!(
            sent_note(&provenance_snapshot(&handle, tab), "flush me")["name"],
            "Pixel 8"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_and_stop_are_noted() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("stopped-from");
        let (handle, base, _) = handle_with_two_pages();
        let (tab, provider, key) = tab_of(&handle, &dir, RecordingProvider::task_controlling());
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        provider.queue_all(background_task("b7"));
        tick_handle_until(&handle, "the task", || {
            handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .background_calls
                > 0
        });
        assert_eq!(
            settle(
                &handle,
                &phone(),
                &key,
                &serde_json::json!({"type":"stop_task", "command_id":"k1", "task":"b7"}).to_string()
            ),
            Ok(())
        );
        assert_eq!(
            provenance_snapshot(&handle, tab)["state"]["stoppedFrom"]["b7"]["deviceId"],
            "d1"
        );
        assert_eq!(
            settle(
                &handle,
                &phone(),
                &key,
                &serde_json::json!({"type":"stop_background", "command_id":"k2"}).to_string()
            ),
            Ok(())
        );
        assert_eq!(
            provenance_snapshot(&handle, tab)["state"]["stoppedFrom"]["b7"]["name"],
            "Pixel 8"
        );
        shut_down_every_tab(&handle);

        let (handle, _, _) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(
            settle(&handle, &phone(), &key, &interrupt("i1", turn(&handle, tab, "t1"))),
            Ok(())
        );
        assert_eq!(
            provenance_snapshot(&handle, tab)["state"]["stoppedFrom"]["t1"]["name"],
            "Pixel 8"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn desk_send_has_no_note() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("desk-no-note");
        let (handle, _, _) = handle_with_two_pages();
        let (tab, _, _) = interruptible_tab(&handle, &dir);
        handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .live_mut()
            .unwrap()
            .send_turn_in(
                "desk",
                "desk",
                SessionModeChoice::Manual,
                None,
                agent::PromptOrigin::Typed,
                Vec::new(),
            )
            .unwrap();
        handle.tick();
        let snapshot = provenance_snapshot(&handle, tab);
        assert!(snapshot["state"].get("sentFrom").is_none());
        assert!(snapshot["state"].get("stoppedFrom").is_none());
        shut_down_every_tab(&handle);
    }

    #[test]
    fn note_survives_a_snapshot() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-note-snapshot");
        let (handle, _, _) = handle_with_two_pages();
        let (tab, _, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "hello")), Ok(()));
        let observer = handle.state.borrow_mut().tabs.observe(&key).unwrap();
        let frames = handle
            .state
            .borrow_mut()
            .tabs
            .take_observer_frames(observer, 1024 * 1024);
        let remote = frames
            .into_iter()
            .find_map(|frame| match frame {
                eitri_core::tab_set::ObserverFrame::Snapshot { json, .. } => {
                    Some(serde_json::from_str::<serde_json::Value>(&json).unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(sent_note(&remote, "hello")["name"], "Pixel 8");
        assert_eq!(
            sent_note(&remote, "hello"),
            sent_note(&provenance_snapshot(&handle, tab), "hello")
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn a_reset_forgets_the_old_conversations_phone_notes() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-note-reset");
        let (handle, _, _) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "before")), Ok(()));
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(
            settle(&handle, &phone(), &key, &interrupt("i1", turn(&handle, tab, "t1"))),
            Ok(())
        );
        end_turn(&handle, tab, &provider, "t1");
        let before = provenance_snapshot(&handle, tab);
        assert_eq!(sent_note(&before, "before")["name"], "Pixel 8");
        assert_eq!(before["state"]["stoppedFrom"]["t1"]["name"], "Pixel 8");
        provider.queue(agent::AgentDomainEvent::ConversationReset {
            new_conversation_id: "c2".into(),
            trigger: None,
        });
        // The projection folds the reset on the provider's thread; the tab's notes follow when its tick drains it.
        tick_handle_until(&handle, "the reset's drain", || {
            provenance_snapshot(&handle, tab)["state"].get("sentFrom").is_none()
        });
        let after = provenance_snapshot(&handle, tab);
        assert_eq!(after["state"]["userPrompts"], serde_json::json!([]), "{after}");
        assert!(after["state"].get("stoppedFrom").is_none(), "{after}");
        assert_eq!(settle(&handle, &phone(), &key, &send("s2", "after")), Ok(()));
        assert_eq!(
            sent_note(&provenance_snapshot(&handle, tab), "after")["name"],
            "Pixel 8",
            "a phone's message after the reset is noted"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn live_events_carry_the_note() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-note-events");
        let (handle, base, _) = handle_with_two_pages();
        let (_tab, _, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        let observer = handle.state.borrow_mut().tabs.observe(&key).unwrap();
        handle
            .state
            .borrow_mut()
            .tabs
            .take_observer_frames(observer, 1024 * 1024);
        clear(&[&base]);
        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "hello")), Ok(()));
        handle.tick();
        assert!(base.sent.borrow().iter().any(|raw| {
            let value: serde_json::Value = serde_json::from_str(raw).unwrap();
            value["kind"] == "events"
                && value["sentFrom"]
                    .as_object()
                    .is_some_and(|map| map.values().any(|from| from["deviceId"] == "d1"))
        }));
        let frames = handle
            .state
            .borrow_mut()
            .tabs
            .take_observer_frames(observer, 1024 * 1024);
        assert!(frames.iter().any(|frame| match frame {
            eitri_core::tab_set::ObserverFrame::Events { json, .. } => {
                let value: serde_json::Value = serde_json::from_str(json).unwrap();
                value["sentFrom"]
                    .as_object()
                    .is_some_and(|map| map.values().any(|from| from["deviceId"] == "d1"))
            }
            _ => false,
        }));
        shut_down_every_tab(&handle);
    }

    fn phone() -> RemoteOrigin {
        RemoteOrigin {
            device_id: "d1".into(),
            device: "Pixel 8".into(),
            conn: 1,
            authority: None,
        }
    }

    fn interrupt(command_id: &str, expect_work: serde_json::Value) -> String {
        serde_json::json!({ "type": "interrupt", "command_id": command_id, "expect_work": expect_work }).to_string()
    }

    fn send(command_id: &str, text: &str) -> String {
        serde_json::json!({ "type": "send", "command_id": command_id, "text": text }).to_string()
    }

    fn queue(command_id: &str, text: &str) -> String {
        serde_json::json!({ "type": "queue", "command_id": command_id, "text": text }).to_string()
    }

    /// A new tab of `provider`'s, opened (so selected) in the base view, and its row key.
    fn tab_of(
        handle: &AgentPanelHandle,
        dir: &Path,
        provider: RecordingProvider,
    ) -> (TabId, std::sync::Arc<RecordingProvider>, RowKey) {
        let provider = std::sync::Arc::new(provider);
        let conversation = agent::AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let mut state = handle.state.borrow_mut();
        let tab = state.tabs.open(AgentId::Claude);
        let held = state.tabs.get_mut(tab).unwrap();
        held.set_backend(TabBackend::Live(backend));
        let key = held.row_key().clone();
        (tab, provider, key)
    }

    /// A new Claude tab whose provider can interrupt, opened (so selected) in the base view, and its row key.
    fn interruptible_tab(handle: &AgentPanelHandle, dir: &Path) -> (TabId, std::sync::Arc<RecordingProvider>, RowKey) {
        tab_of(handle, dir, RecordingProvider::interruptible())
    }

    /// The work `turn` of the session `tab` holds now, as a phone names it from the frames it was sent.
    fn turn(handle: &AgentPanelHandle, tab: TabId, turn: &str) -> serde_json::Value {
        let generation = handle.state.borrow().tabs.get(tab).unwrap().backend_generation();
        serde_json::json!({ "turn": { "id": turn, "generation": generation } })
    }

    /// Puts a new session over a recording provider in `tab` in place of the one it held.
    fn replace_session(handle: &AgentPanelHandle, tab: TabId, dir: &Path) -> std::sync::Arc<RecordingProvider> {
        let provider = std::sync::Arc::new(RecordingProvider::interruptible());
        let conversation = agent::AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let old = handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        if let TabBackend::Live(mut old) = old {
            old.shutdown();
        }
        provider
    }

    fn active_turn(handle: &AgentPanelHandle, tab: TabId) -> Option<String> {
        let state = handle.state.borrow();
        let backend = state.tabs.get(tab).unwrap().live()?;
        let turn = backend.projection().active_turn_id.clone();
        turn
    }

    fn start_turn(handle: &AgentPanelHandle, tab: TabId, provider: &RecordingProvider, turn: &str) {
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        tick_handle_until(handle, "the turn", || active_turn(handle, tab).as_deref() == Some(turn));
    }

    fn end_turn(handle: &AgentPanelHandle, tab: TabId, provider: &RecordingProvider, turn: &str) {
        provider.queue(completed(turn));
        tick_handle_until(handle, "the turn's end", || active_turn(handle, tab).is_none());
    }

    fn completed(turn: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    fn unseen(handle: &AgentPanelHandle, tab: TabId) -> (bool, u64) {
        let state = handle.state.borrow();
        let held = state.tabs.get(tab).unwrap();
        (held.cli_unseen_busy(), held.unseen_stretch())
    }

    /// Cycles `tab`'s mode from the base page until it is `wanted`, confirming Bypass when it is asked.
    fn set_mode(handle: &AgentPanelHandle, base: &Rc<RecordingPage>, tab: TabId, wanted: SessionModeChoice) {
        for round in 0..4 {
            if mode_of(handle, tab) == wanted {
                return;
            }
            clear(&[base]);
            cycle_mode_from(handle, ViewId::BASE, &format!("m{round}"), tab);
            if let Some((_, nonce)) = last_bypass_prompt(base) {
                confirm_bypass_from(handle, ViewId::BASE, &format!("y{round}"), tab, nonce);
            }
        }
        assert_eq!(mode_of(handle, tab), wanted);
    }

    /// The result of command `command_id` from the socket's side, ticking until it comes; the other results taken
    /// meanwhile are dropped.
    fn result_of(handle: &AgentPanelHandle, command_id: &str) -> RemoteResult {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(found) = handle
                .take_remote_results()
                .into_iter()
                .find(|result| result.command_id == command_id)
            {
                return found;
            }
            assert!(std::time::Instant::now() < deadline, "no result for {command_id}");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A remote message carried out to its end: answered at once, or, when it went out to the provider, once its
    /// result came.
    fn settle(handle: &AgentPanelHandle, origin: &RemoteOrigin, key: &RowKey, raw: &str) -> Result<(), RemoteRefusal> {
        match handle.remote_message(origin, key, raw)? {
            RemoteAccepted::Done => Ok(()),
            RemoteAccepted::Pending => {
                let id: serde_json::Value = serde_json::from_str(raw).unwrap();
                let result = result_of(handle, id["command_id"].as_str().unwrap());
                assert_eq!(result.conn, origin.conn, "answered on the connection it came on");
                result.outcome
            }
        }
    }

    fn queued_texts(handle: &AgentPanelHandle, tab: TabId) -> Vec<String> {
        handle.state.borrow().tabs.queue_texts(tab)
    }

    fn not_started(handle: &AgentPanelHandle, tab: TabId) -> bool {
        matches!(
            handle.state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::NotStarted
        )
    }

    #[test]
    fn remote_inbound_refuses_desk_only_kinds() {
        let interrupt = r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":{"id":"t1","generation":1}}}"#;
        assert!(
            serde_json::from_str::<RemoteInbound>(interrupt).is_ok(),
            "the one remote kind parses"
        );
        for kind in [
            "permission_response",
            "question_answer",
            "run_shell",
            "cycle_mode",
            "confirm_bypass",
            "trust_answer",
            "send_message",
            "open_path",
            "ready",
            "turn_rendered",
        ] {
            let raw = format!(
                r#"{{"type":"{kind}","command_id":"c1","expect_work":{{"turn":{{"id":"t1","generation":1}}}}}}"#
            );
            assert!(
                serde_json::from_str::<RemoteInbound>(&raw).is_err(),
                "{kind} is the desk's alone"
            );
            assert_eq!(parse_remote_inbound(&raw), Err(RemoteRefusal::Parse), "{kind}");
        }
    }

    #[test]
    fn remote_inbound_refuses_unknown_fields() {
        let named_tab =
            r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":{"id":"t","generation":1}},"tab":3}"#;
        assert!(serde_json::from_str::<RemoteInbound>(named_tab).is_err());
        assert_eq!(parse_remote_inbound(named_tab), Err(RemoteRefusal::Parse));
        let both =
            r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":{"id":"t","generation":1},"unseen":1}}"#;
        assert!(serde_json::from_str::<RemoteInbound>(both).is_err(), "one kind of work");
        for turn in [
            r#"{"turn":"t"}"#,
            r#"{"turn":{"id":"t"}}"#,
            r#"{"turn":{"id":"t","generation":1,"tab":3}}"#,
        ] {
            let raw = format!(r#"{{"type":"interrupt","command_id":"c1","expect_work":{turn}}}"#);
            assert_eq!(parse_remote_inbound(&raw), Err(RemoteRefusal::Parse), "{raw}");
        }
        let without = r#"{"type":"interrupt","command_id":"c1","expect_work":{"turn":{"id":"t","generation":1}}}"#;
        assert_eq!(
            serde_json::from_str::<RemoteInbound>(without).unwrap(),
            RemoteInbound::Interrupt {
                command_id: "c1".into(),
                expect_work: ExpectWork::Turn {
                    id: "t".into(),
                    generation: 1
                },
            }
        );
        let stretch = r#"{"type":"interrupt","command_id":"c1","expect_work":{"unseen":2}}"#;
        assert!(matches!(
            serde_json::from_str::<RemoteInbound>(stretch).unwrap(),
            RemoteInbound::Interrupt {
                expect_work: ExpectWork::Unseen(2),
                ..
            }
        ));
        for kind in [
            serde_json::json!({ "type": "send", "command_id": "n1", "text": "hello" }),
            serde_json::json!({ "type": "queue", "command_id": "n2", "text": "later" }),
            serde_json::json!({ "type": "stop_background", "command_id": "n3" }),
            serde_json::json!({ "type": "stop_task", "command_id": "n4", "task": "b7" }),
            serde_json::json!({ "type": "take_back", "command_id": "n5" }),
        ] {
            let parsed = parse_remote_inbound(&kind.to_string()).unwrap_or_else(|e| panic!("{kind}: {e:?}"));
            assert_eq!(parsed.command_id(), kind["command_id"].as_str().unwrap());
            let mut extra = kind.clone();
            extra["tab"] = serde_json::json!(3);
            assert_eq!(
                parse_remote_inbound(&extra.to_string()),
                Err(RemoteRefusal::Parse),
                "{extra}"
            );
        }
    }

    #[test]
    fn command_ledger_keeps_the_last_1024_per_device() {
        assert_eq!(COMMAND_LEDGER_PER_DEVICE, 1024);
        let mut ledger = RemoteCommands::default();
        for n in 0..=COMMAND_LEDGER_PER_DEVICE {
            assert!(ledger.first_use("d1", &format!("c{n}")), "c{n} is new");
        }
        assert!(
            !ledger.first_use("d1", "c1"),
            "the oldest of the last 1024 is still known"
        );
        assert!(
            !ledger.first_use("d1", &format!("c{COMMAND_LEDGER_PER_DEVICE}")),
            "and the newest"
        );
        assert!(ledger.first_use("d1", "c0"), "one past the last 1024 is forgotten");
        assert!(ledger.first_use("d2", "c5"), "another device's ids are its own");
        assert!(!ledger.first_use("d2", "c5"));
        let held: usize = ledger.by_device.values().map(DeviceLedger::len).sum();
        assert_eq!(held, COMMAND_LEDGER_PER_DEVICE + 1, "nothing beyond the bound is kept");
    }

    #[test]
    fn remote_text_is_bounded() {
        assert_eq!(REMOTE_TEXT_MAX_BYTES, 64 * 1024);
        let fits = serde_json::json!({ "type": "send", "command_id": "t1", "text": "a".repeat(REMOTE_TEXT_MAX_BYTES) });
        assert!(
            parse_remote_inbound(&fits.to_string()).is_ok(),
            "64 KiB of text is taken"
        );
        // Every byte escaped is six on the wire, and still taken.
        let escaped =
            serde_json::json!({ "type": "queue", "command_id": "t2", "text": "\u{1}".repeat(REMOTE_TEXT_MAX_BYTES) });
        assert!(
            parse_remote_inbound(&escaped.to_string()).is_ok(),
            "escaped text of the limit is taken"
        );
        let over =
            serde_json::json!({ "type": "send", "command_id": "t3", "text": "a".repeat(REMOTE_TEXT_MAX_BYTES + 1) });
        assert_eq!(parse_remote_inbound(&over.to_string()), Err(RemoteRefusal::TooLong));
        let long_id = serde_json::json!({ "type": "take_back", "command_id": "c".repeat(65) });
        assert_eq!(parse_remote_inbound(&long_id.to_string()), Err(RemoteRefusal::Parse));
        let unread = format!(
            r#"{{"type":"send","command_id":"t4","text":"{}"}}"#,
            "a".repeat(REMOTE_MESSAGE_MAX_BYTES)
        );
        assert_eq!(
            parse_remote_inbound(&unread),
            Err(RemoteRefusal::Parse),
            "too long to read"
        );
    }

    #[test]
    fn remote_interrupt_resolves_by_key_not_tab_number() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-by-key");
        let (handle, _base, _second) = handle_with_two_pages();
        let (first, first_provider, first_key) = interruptible_tab(&handle, &dir);
        let (second, second_provider, _) = interruptible_tab(&handle, &dir);
        start_turn(&handle, first, &first_provider, "t1");
        start_turn(&handle, second, &second_provider, "t1");
        assert_eq!(
            handle.state.borrow().tabs.active(),
            second,
            "the other tab is the one on screen"
        );

        assert_eq!(
            settle(
                &handle,
                &phone(),
                &first_key,
                &interrupt("c1", turn(&handle, first, "t1"))
            ),
            Ok(())
        );
        assert_eq!(first_provider.interrupts(), 1, "the tab with that key stopped");
        assert_eq!(second_provider.interrupts(), 0, "the other running tab is untouched");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_unknown_key_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-unknown-key");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, _) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(
            handle.remote_message(&phone(), &RowKey::new(), &interrupt("c1", turn(&handle, tab, "t1"))),
            Err(RemoteRefusal::NoSuchTab)
        );
        assert_eq!(provider.interrupts(), 0, "nothing stopped");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_for_an_ended_turn_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-ended-turn");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        provider.queue(completed("t1"));
        start_turn(&handle, tab, &provider, "t2");

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Err(RemoteRefusal::WorkChanged)
        );
        assert_eq!(provider.interrupts(), 0, "nothing stopped");
        assert_eq!(active_turn(&handle, tab).as_deref(), Some("t2"), "t2 still runs");
        // A stretch of unseen work is not a turn: while one runs, a stop about a stretch stops nothing.
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c2", serde_json::json!({ "unseen": 0 }))),
            Err(RemoteRefusal::WorkChanged)
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_for_a_turn_of_a_replaced_session_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-replaced-session");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        let asked = turn(&handle, tab, "t1");

        // The session is replaced, and the next one names its first turn the same.
        let next = replace_session(&handle, tab, &dir);
        start_turn(&handle, tab, &next, "t1");
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", asked)),
            Err(RemoteRefusal::WorkChanged)
        );
        assert_eq!(next.interrupts(), 0, "the new session's turn was not stopped");
        assert_eq!(active_turn(&handle, tab).as_deref(), Some("t1"));
        assert_eq!(
            settle(&handle, &phone(), &key, &interrupt("c2", turn(&handle, tab, "t1"))),
            Ok(()),
            "a stop naming this session's turn stops it"
        );
        assert_eq!(next.interrupts(), 1);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_for_an_ended_unseen_stretch_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-ended-stretch");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        assert_eq!(unseen(&handle, tab), (false, 0), "no stretch yet");
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        tick_handle_until(&handle, "the first stretch", || unseen(&handle, tab) == (true, 1));
        // More of the same stretch is not a new one.
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        provider.queue(completed(agent::NO_OPEN_TURN_ID));
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        tick_handle_until(&handle, "the second stretch", || unseen(&handle, tab) == (true, 2));

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "unseen": 1 }))),
            Err(RemoteRefusal::WorkChanged)
        );
        assert_eq!(provider.interrupts(), 0, "nothing stopped");
        assert_eq!(
            settle(
                &handle,
                &phone(),
                &key,
                &interrupt("c2", serde_json::json!({ "unseen": 2 }))
            ),
            Ok(()),
            "the stretch running now is stopped"
        );
        assert_eq!(provider.interrupts(), 1);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_works_in_bypass() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-bypass");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Bypass);
        start_turn(&handle, tab, &provider, "t1");

        assert_eq!(
            settle(&handle, &phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Ok(())
        );
        assert_eq!(provider.interrupts(), 1, "the running turn was stopped");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_leaves_a_desk_shell_line_running() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-shell-line");
        let (handle, base, _second) = handle_with_two_pages();
        handle.state.borrow_mut().project_dir = dir.clone();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        let line = serde_json::json!({ "type": "run_shell", "request_id": "r1", "tab": tab.0, "text": "!sleep 20" });
        handle.page_message(ViewId::BASE, &line.to_string());
        assert_eq!(result_for(&base.sent.borrow(), "r1"), Some(Ok(())));
        assert!(handle.state.borrow().tabs.shell_line_runs(tab), "the desk's line runs");
        start_turn(&handle, tab, &provider, "t1");

        assert_eq!(
            settle(&handle, &phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Ok(())
        );
        assert_eq!(provider.interrupts(), 1, "the agent's turn was stopped");
        handle.tick();
        assert!(
            handle.state.borrow().tabs.shell_line_runs(tab),
            "the line typed at the desk runs on"
        );
        handle.state.borrow_mut().tabs.interrupt_shell_line(tab);
        tick_handle_until(&handle, "the line's end", || {
            !handle.state.borrow().tabs.shell_line_runs(tab)
        });
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_never_allows_a_pending_card() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-card");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || waiting(&handle, tab, "perm-1"));

        assert_eq!(
            settle(&handle, &phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Ok(())
        );
        handle.tick();
        let decisions = provider.decisions();
        assert!(
            decisions.iter().all(|(_, decision)| !decision.allows()),
            "the backend never received an allow: {decisions:?}"
        );
        let denied = decisions.iter().any(|(id, _)| id == "perm-1");
        assert!(
            waiting(&handle, tab, "perm-1") || denied,
            "the card is still waiting or was denied"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn remote_interrupt_sends_the_desk_no_command_result() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-no-result");
        let (handle, base, second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        clear(&[&base, &second]);

        assert_eq!(
            settle(&handle, &phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Ok(())
        );
        handle.tick();
        for page in [&base, &second] {
            assert!(
                page.sent.borrow().iter().all(|p| kind_of(p) != "command_result"),
                "no page is answered for a message it did not send"
            );
        }
        let told = notices(&base.sent.borrow());
        assert_eq!(told, ["stopped from Pixel 8"], "one notice, naming the device");
        assert!(
            notices(&second.sent.borrow()).is_empty(),
            "a page that does not show the tab is told nothing"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn repeated_command_id_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-repeated-id");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        let stop = interrupt("c1", turn(&handle, tab, "t1"));

        assert_eq!(settle(&handle, &phone(), &key, &stop), Ok(()));
        assert_eq!(
            handle.remote_message(&phone(), &key, &stop),
            Err(RemoteRefusal::Duplicate)
        );
        let on_another_connection = RemoteOrigin { conn: 2, ..phone() };
        assert_eq!(
            handle.remote_message(&on_another_connection, &key, &stop),
            Err(RemoteRefusal::Duplicate),
            "the same device on a new connection"
        );
        assert_eq!(provider.interrupts(), 1, "stopped once");
        let other = RemoteOrigin {
            device_id: "d2".into(),
            device: "Tablet".into(),
            conn: 3,
            authority: None,
        };
        assert_eq!(
            settle(&handle, &other, &key, &stop),
            Ok(()),
            "another device's ids are its own"
        );
        shut_down_every_tab(&handle);
    }

    /// The projection's fold revision, read without a tick.
    fn folded_through(handle: &AgentPanelHandle, tab: TabId) -> u64 {
        let state = handle.state.borrow();
        let revision = state.tabs.get(tab).unwrap().live().unwrap().projection().last_revision;
        revision
    }

    #[test]
    fn a_stop_for_unseen_work_folded_since_the_last_tick_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-stretch-unpumped");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        tick_handle_until(&handle, "the first stretch", || unseen(&handle, tab) == (true, 1));
        let counted = folded_through(&handle, tab);
        // The first stretch ends and a second begins, both folded and neither seen by a tick yet: the projection
        // shows work running, as it did for the first.
        provider.queue_all(vec![
            completed(agent::NO_OPEN_TURN_ID),
            agent::AgentDomainEvent::CliWorkUnseen,
        ]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while folded_through(&handle, tab) < counted + 2 {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the fold");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            unseen(&handle, tab),
            (true, 1),
            "no tick has counted the second stretch"
        );

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", serde_json::json!({ "unseen": 1 }))),
            Err(RemoteRefusal::WorkChanged)
        );
        assert_eq!(provider.interrupts(), 0, "the second stretch was not stopped");
        tick_handle_until(&handle, "the second stretch", || unseen(&handle, tab) == (true, 2));
        assert_eq!(
            settle(
                &handle,
                &phone(),
                &key,
                &interrupt("c2", serde_json::json!({ "unseen": 2 }))
            ),
            Ok(()),
            "once counted, the stretch running now is stopped"
        );
        assert_eq!(provider.interrupts(), 1);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn command_ids_outlive_the_handle_that_took_them() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-ids-across-handles");
        let (first, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&first, &dir);
        start_turn(&first, tab, &provider, "t1");
        let stop = interrupt("c1", turn(&first, tab, "t1"));
        assert_eq!(settle(&first, &phone(), &key, &stop), Ok(()));

        let (other, _other_base, _other_second) = handle_with_two_pages();
        let (other_tab, other_provider, other_key) = interruptible_tab(&other, &dir);
        start_turn(&other, other_tab, &other_provider, "t1");
        assert_eq!(
            other.remote_message(&phone(), &other_key, &stop),
            Err(RemoteRefusal::Duplicate),
            "another window of the process knows the id"
        );
        shut_down_every_tab(&first);
        drop(first);
        assert_eq!(
            other.remote_message(&phone(), &other_key, &stop),
            Err(RemoteRefusal::Duplicate),
            "the window that took it is gone, the id is still spent"
        );
        assert_eq!(other_provider.interrupts(), 0, "nothing stopped by a repeated id");
        shut_down_every_tab(&other);
    }

    // ---- the remaining arms ------------------------------------------------------------------------------

    #[test]
    fn send_reaches_an_idle_manual_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-send-manual");
        let (handle, base, second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        clear(&[&base, &second]);

        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "fix the test")), Ok(()));
        assert_eq!(provider.turns(), ["fix the test"], "sent as it came");
        let stamps = provider.turn_stamps();
        assert!(
            stamps.iter().all(|stamp| !stamp.typed_by_person),
            "not marked as typed at the desk: {stamps:?}"
        );
        handle.tick();
        assert_eq!(notices(&base.sent.borrow()), ["sent from Pixel 8"]);
        assert!(
            base.sent.borrow().iter().all(|p| kind_of(p) != "command_result"),
            "no page is answered for it"
        );
        assert!(notices(&second.sent.borrow()).is_empty());
        shut_down_every_tab(&handle);
    }

    #[test]
    fn send_reaches_an_auto_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-send-auto");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Auto);

        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "go on")), Ok(()));
        assert_eq!(provider.turns(), ["go on"]);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn send_refused_in_bypass() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-send-bypass");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Bypass);

        assert_eq!(
            handle.remote_message(&phone(), &key, &send("s1", "go on")),
            Err(RemoteRefusal::ModeNotAllowed)
        );
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(
            handle.remote_message(&phone(), &key, &queue("q1", "and then")),
            Err(RemoteRefusal::ModeNotAllowed)
        );
        handle.tick();
        assert!(provider.turns().is_empty(), "nothing was sent");
        assert!(queued_texts(&handle, tab).is_empty(), "nothing was queued");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn send_refused_to_a_codex_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, key) = {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Codex);
            let key = state.tabs.get(tab).unwrap().row_key().clone();
            (tab, key)
        };
        for message in [send("s1", "hello"), queue("q1", "hello")] {
            assert_eq!(
                handle.remote_message(&phone(), &key, &message),
                Err(RemoteRefusal::NotClaude),
                "{message}"
            );
        }
        assert!(not_started(&handle, tab), "no session starts");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn send_refused_to_an_unstarted_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, key) = {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Claude);
            let key = state.tabs.get(tab).unwrap().row_key().clone();
            (tab, key)
        };
        for message in [send("s1", "hello"), queue("q1", "hello")] {
            assert_eq!(
                handle.remote_message(&phone(), &key, &message),
                Err(RemoteRefusal::NotStarted),
                "{message}"
            );
        }
        for _ in 0..3 {
            handle.tick();
        }
        assert!(not_started(&handle, tab), "no session starts");
        assert!(
            !handle.state.borrow().tabs.awaiting_trust().contains(&tab),
            "no trust question is asked for it"
        );
        assert_eq!(
            notices(&base.sent.borrow()),
            ["refused from Pixel 8: the session has not started"]
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn slash_and_bang_are_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-slash-bang");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        for (n, text) in [
            "/model opus",
            "!rm -rf build",
            "   /compact",
            "\n\t !ls",
            "\u{3000}/help",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                handle.remote_message(&phone(), &key, &send(&format!("s{n}"), text)),
                Err(RemoteRefusal::SlashOrBang),
                "{text:?}"
            );
        }
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(
            handle.remote_message(&phone(), &key, &queue("q1", "  /clear")),
            Err(RemoteRefusal::SlashOrBang)
        );
        handle.tick();
        assert!(provider.turns().is_empty(), "nothing was sent");
        assert!(queued_texts(&handle, tab).is_empty(), "nothing was queued");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn at_words_stay_literal_and_read_no_buffers() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-at-words");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        // The context fails the test if it is read at all; the editor records any question it is asked.
        let editor = Rc::new(eitri_core::editor_tools::testing::FakeEditor::default());
        let context = {
            let mut state = handle.state.borrow_mut();
            state.editor_rpc = Some(editor.clone());
            std::mem::replace(
                &mut state.editor_context,
                Rc::new(|| panic!("a phone's message read the editor's context")),
            )
        };
        let text = "look at @buffers and @diagnostics";
        let accepted = handle.remote_message(&phone(), &key, &send("s1", text));
        handle.state.borrow_mut().editor_context = context;
        assert_eq!(accepted, Ok(RemoteAccepted::Pending));
        assert!(editor.asked().is_empty(), "the editor was asked: {:?}", editor.asked());
        assert!(
            !handle.state.borrow().mentions.is_waiting(),
            "nothing waits for the editor"
        );
        assert_eq!(result_of(&handle, "s1").outcome, Ok(()));
        assert_eq!(provider.turns(), [text], "the words went as they came");
        assert!(editor.asked().is_empty(), "the editor was asked: {:?}", editor.asked());
        assert!(handle.state.borrow().tabs.get(tab).is_some());
        shut_down_every_tab(&handle);
    }

    #[test]
    fn desk_draft_and_history_untouched() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-draft-history");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        handle.state.borrow_mut().tabs.set_draft(tab, "half a thought");
        let history = handle.state.borrow().history.clone();
        clear(&[&base]);

        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "from afar")), Ok(()));
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        tick_handle_until(&handle, "the turn", || active_turn(&handle, tab).is_some());
        assert_eq!(settle(&handle, &phone(), &key, &queue("q1", "and more")), Ok(()));
        handle.tick();

        let state = handle.state.borrow();
        assert_eq!(
            state.tabs.get(tab).unwrap().draft(),
            "half a thought",
            "the desk's draft"
        );
        assert_eq!(state.history, history, "the desk's history");
        assert!(
            base.sent.borrow().iter().all(|p| kind_of(p) != "history"),
            "no history goes to the page"
        );
        drop(state);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn queue_waits_for_the_turn_and_goes_at_its_end() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-queue-goes");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Auto);
        assert_eq!(
            handle.remote_message(&phone(), &key, &queue("q0", "nothing runs")),
            Err(RemoteRefusal::WorkChanged),
            "a queue with no turn running"
        );
        start_turn(&handle, tab, &provider, "t1");
        clear(&[&base]);
        assert_eq!(
            handle.remote_message(&phone(), &key, &queue("q1", "then this")),
            Ok(RemoteAccepted::Done)
        );
        assert_eq!(
            handle.remote_message(&phone(), &key, &send("s1", "now")),
            Err(RemoteRefusal::WorkChanged),
            "a send while a turn runs"
        );
        assert_eq!(queued_texts(&handle, tab), ["then this"]);
        assert_eq!(notices(&base.sent.borrow()), ["queued from Pixel 8"]);
        // Narrower is no wider: it still goes.
        set_mode(&handle, &base, tab, SessionModeChoice::Bypass);
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        end_turn(&handle, tab, &provider, "t1");
        tick_handle_until(&handle, "the queued message", || provider.turns().len() == 1);
        assert_eq!(provider.turns(), ["then this"]);
        assert!(queued_texts(&handle, tab).is_empty());
        shut_down_every_tab(&handle);
    }

    #[test]
    fn queued_phone_message_is_refused_when_the_mode_widened() {
        agent::state_dirs::redirect_state_to_a_test_root();
        for (from, to) in [
            (SessionModeChoice::Manual, SessionModeChoice::Auto),
            (SessionModeChoice::Auto, SessionModeChoice::Bypass),
        ] {
            let dir = agent::state_dirs::test_workspace_dir(&format!("remote-widened-{from:?}"));
            let (handle, base, _second) = handle_with_two_pages();
            let (tab, provider, key) = interruptible_tab(&handle, &dir);
            handle.tick();
            set_mode(&handle, &base, tab, from);
            start_turn(&handle, tab, &provider, "t1");
            let id = format!("q-{from:?}");
            assert_eq!(
                handle.remote_message(&phone(), &key, &queue(&id, "do it")),
                Ok(RemoteAccepted::Done)
            );
            // A desk message queued behind it is the desk's, and goes whatever the mode.
            handle.page_message(
                ViewId::BASE,
                &serde_json::json!({ "type": "queue_message", "request_id": "r1", "tab": tab.0, "text": "mine" })
                    .to_string(),
            );
            set_mode(&handle, &base, tab, to);
            clear(&[&base]);
            end_turn(&handle, tab, &provider, "t1");
            let refused = result_of(&handle, &id);
            assert_eq!(
                refused.outcome,
                Err(RemoteRefusal::ModeNotAllowed),
                "{from:?} -> {to:?}"
            );
            assert_eq!(refused.conn, phone().conn);
            tick_handle_until(&handle, "the desk's message", || !provider.turns().is_empty());
            assert_eq!(provider.turns(), ["mine"], "only the desk's went: {from:?} -> {to:?}");
            let told = notices(&base.sent.borrow());
            assert!(
                told.iter()
                    .any(|n| n.contains("Pixel 8") && n.contains("not sent") && n.contains(&format!("{to:?}"))),
                "the desk is told: {told:?}"
            );
            shut_down_every_tab(&handle);
        }
    }

    #[test]
    fn take_back_returns_only_this_devices_items() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-take-back");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        start_turn(&handle, tab, &provider, "t1");
        let tablet = RemoteOrigin {
            device_id: "d2".into(),
            device: "Pixel 8".into(),
            conn: 2,
            authority: None,
        };
        assert_eq!(
            handle.remote_message(
                &phone(),
                &key,
                &serde_json::json!({ "type": "take_back", "command_id": "b0" }).to_string()
            ),
            Err(RemoteRefusal::NotYours),
            "nothing queued yet"
        );
        handle.remote_message(&phone(), &key, &queue("q1", "mine 1")).unwrap();
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({ "type": "queue_message", "request_id": "r1", "tab": tab.0, "text": "the desk's" })
                .to_string(),
        );
        handle
            .remote_message(&tablet, &key, &queue("q2", "same name, other device"))
            .unwrap();
        handle.remote_message(&phone(), &key, &queue("q3", "mine 2")).unwrap();
        clear(&[&base]);

        let take_back = serde_json::json!({ "type": "take_back", "command_id": "b1" }).to_string();
        assert_eq!(
            handle.remote_message(&phone(), &key, &take_back),
            Ok(RemoteAccepted::Done)
        );
        assert_eq!(
            queued_texts(&handle, tab),
            ["the desk's", "same name, other device"],
            "only device d1's items left"
        );
        let told = notices(&base.sent.borrow());
        assert_eq!(told, ["Pixel 8 took back its 2 queued messages"]);
        assert!(
            base.sent.borrow().iter().any(|p| kind_of(p) == "queue"),
            "the page is sent the queue"
        );
        assert_eq!(
            handle.remote_message(
                &phone(),
                &key,
                &serde_json::json!({ "type": "take_back", "command_id": "b2" }).to_string()
            ),
            Err(RemoteRefusal::NotYours),
            "nothing of d1's is left"
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn stop_background_refused_on_codex() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let (handle, _base, _second) = handle_with_two_pages();
        let key = {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Codex);
            state.tabs.get(tab).unwrap().row_key().clone()
        };
        for message in [
            serde_json::json!({ "type": "stop_background", "command_id": "b1" }),
            serde_json::json!({ "type": "stop_task", "command_id": "b2", "task": "t" }),
        ] {
            assert_eq!(
                handle.remote_message(&phone(), &key, &message.to_string()),
                Err(RemoteRefusal::NotClaude),
                "{message}"
            );
        }
        shut_down_every_tab(&handle);
    }

    /// One command left running in the background, as the roster and the count report it.
    fn background_task(id: &str) -> Vec<agent::AgentDomainEvent> {
        vec![
            agent::AgentDomainEvent::BackgroundTasksChanged {
                tasks: vec![agent::BackgroundTask {
                    task_id: id.into(),
                    run_id: None,
                    kind: agent::TaskKind::Shell,
                    task_type: "shell".into(),
                    description: "watch the build".into(),
                    subagent_type: None,
                    parent_task_id: None,
                    tool_use_id: None,
                    ambient: false,
                    workflow_name: None,
                }],
                occurred_at: None,
            },
            agent::AgentDomainEvent::BackgroundCalls { running: 1 },
        ]
    }

    #[test]
    fn stop_background_and_stop_task_leave_the_desk_a_notice() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-stop-background");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = tab_of(&handle, &dir, RecordingProvider::task_controlling());
        handle.tick();
        assert_eq!(
            handle.remote_message(
                &phone(),
                &key,
                &serde_json::json!({ "type": "stop_task", "command_id": "k0", "task": "b9" }).to_string()
            ),
            Err(RemoteRefusal::WorkChanged),
            "a task that is not listed"
        );
        provider.queue_all(background_task("b7"));
        tick_handle_until(&handle, "the task", || {
            let state = handle.state.borrow();
            let listed = state
                .tabs
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .background_calls
                > 0;
            listed
        });
        clear(&[&base]);
        let stop_task = serde_json::json!({ "type": "stop_task", "command_id": "k1", "task": "b7" }).to_string();
        assert_eq!(settle(&handle, &phone(), &key, &stop_task), Ok(()));
        assert_eq!(provider.stopped_tasks(), ["b7"]);
        let stop_all = serde_json::json!({ "type": "stop_background", "command_id": "k2" }).to_string();
        assert_eq!(settle(&handle, &phone(), &key, &stop_all), Ok(()));
        assert_eq!(provider.stopped_tasks(), ["b7", "b7"]);
        handle.tick();
        assert_eq!(
            notices(&base.sent.borrow()),
            [
                "a background task stopped from Pixel 8",
                "background work stopped from Pixel 8"
            ]
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn interrupt_returns_pending_while_the_provider_blocks() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-interrupt-blocks");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        clear(&[&base]);
        let release = provider.hold_calls();

        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Ok(RemoteAccepted::Pending)
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while provider.calls_waiting() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the call never reached the provider"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for _ in 0..3 {
            handle.tick();
        }
        assert!(
            handle.take_remote_results().is_empty(),
            "no result while the provider waits"
        );
        assert_eq!(provider.interrupts(), 0, "the provider has not answered");
        assert!(
            notices(&base.sent.borrow()).is_empty(),
            "nothing told before it was done"
        );
        // The ticks above came back while the call waited; the call is released only now.
        drop(release);
        let result = result_of(&handle, "c1");
        assert_eq!(result.outcome, Ok(()));
        assert_eq!(result.conn, phone().conn);
        assert_eq!(provider.interrupts(), 1);
        handle.tick();
        assert_eq!(notices(&base.sent.borrow()), ["stopped from Pixel 8"]);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn send_returns_pending_while_the_provider_blocks() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-send-blocks");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        clear(&[&base]);
        let release = provider.hold_calls();

        assert_eq!(
            handle.remote_message(&phone(), &key, &send("s1", "hello")),
            Ok(RemoteAccepted::Pending)
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while provider.calls_waiting() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the call never reached the provider"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for _ in 0..3 {
            handle.tick();
        }
        assert!(
            handle.take_remote_results().is_empty(),
            "no result while the provider waits"
        );
        assert!(handle.state.borrow().tabs.remote_send_waits(tab));
        assert_eq!(
            handle.remote_message(&phone(), &key, &send("s2", "again")),
            Err(RemoteRefusal::Busy(eitri_core::tab_set::SEND_HELD.to_string())),
            "one send of a tab at a time"
        );
        assert!(provider.turns().is_empty(), "the provider has not taken it");
        drop(release);
        assert_eq!(result_of(&handle, "s1").outcome, Ok(()));
        assert_eq!(provider.turns(), ["hello"]);
        assert!(!handle.state.borrow().tabs.remote_send_waits(tab));
        handle.tick();
        assert_eq!(
            notices(&base.sent.borrow()),
            [
                format!("refused from Pixel 8: {}", eitri_core::tab_set::SEND_HELD),
                "sent from Pixel 8".to_string()
            ]
        );
        shut_down_every_tab(&handle);
    }

    #[test]
    fn a_failed_remote_stop_reaches_the_phone() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-stop-unsupported");
        let (handle, base, _second) = handle_with_two_pages();
        // A provider that cannot interrupt: refused at once, through the same path as the provider's answer, and
        // handled as the desk's stop handles it.
        let (tab, provider, key) = tab_of(&handle, &dir, RecordingProvider::default());
        start_turn(&handle, tab, &provider, "t1");
        clear(&[&base]);
        let refused = settle(&handle, &phone(), &key, &interrupt("c1", turn(&handle, tab, "t1")));
        assert!(matches!(refused, Err(RemoteRefusal::Agent(_))), "{refused:?}");
        handle.tick();
        assert!(
            notices(&base.sent.borrow()).iter().all(|n| !n.contains("stopped from")),
            "nothing was stopped"
        );
        assert_eq!(provider.interrupts(), 0);
        shut_down_every_tab(&handle);
    }

    /// A Claude tab over `provider`, opened (so selected) in the base view, out of Bypass, and its row key.
    fn tab_over(
        handle: &AgentPanelHandle,
        base: &Rc<RecordingPage>,
        dir: &Path,
        provider: &std::sync::Arc<RecordingProvider>,
    ) -> (TabId, RowKey) {
        let conversation = agent::AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let (tab, key) = {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Claude);
            let held = state.tabs.get_mut(tab).unwrap();
            held.set_backend(TabBackend::Live(backend));
            (tab, held.row_key().clone())
        };
        handle.tick();
        if mode_of(handle, tab) == SessionModeChoice::Bypass {
            set_mode(handle, base, tab, SessionModeChoice::Manual);
        }
        (tab, key)
    }

    #[test]
    fn the_phones_sends_count_against_the_windows_bound_on_calls() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-calls-bound");
        let (handle, base, _second) = handle_with_two_pages();
        let provider = std::sync::Arc::new(RecordingProvider::interruptible());
        let release = provider.hold_calls();
        let tabs: Vec<(TabId, RowKey)> = (0..=REMOTE_CALLS_MAX)
            .map(|_| tab_over(&handle, &base, &dir, &provider))
            .collect();
        for (n, (_, key)) in tabs.iter().take(REMOTE_CALLS_MAX).enumerate() {
            assert_eq!(
                handle.remote_message(&phone(), key, &send(&format!("s{n}"), "hello")),
                Ok(RemoteAccepted::Pending),
                "send {n}"
            );
        }
        let (_, last) = tabs.last().unwrap();
        assert!(
            matches!(
                handle.remote_message(&phone(), last, &send("over", "hello")),
                Err(RemoteRefusal::Busy(_))
            ),
            "a send past the bound makes no call"
        );
        drop(release);
        let mut results = HashMap::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while results.len() < REMOTE_CALLS_MAX {
            assert!(std::time::Instant::now() < deadline, "results so far: {results:?}");
            handle.tick();
            results.extend(
                handle
                    .take_remote_results()
                    .into_iter()
                    .map(|result| (result.command_id, result.outcome)),
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(results.values().all(Result::is_ok), "{results:?}");
        assert!(!results.contains_key("over"));
        assert_eq!(provider.turns().len(), REMOTE_CALLS_MAX);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn a_notice_for_a_tab_no_page_shows_waits_until_one_does() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-notice-owed");
        let (handle, base, _second) = handle_with_two_pages();
        let provider = std::sync::Arc::new(RecordingProvider::interruptible());
        let (first, key) = tab_over(&handle, &base, &dir, &provider);
        let (_shown, _) = tab_over(
            &handle,
            &base,
            &dir,
            &std::sync::Arc::new(RecordingProvider::interruptible()),
        );
        clear(&[&base]);
        assert_eq!(settle(&handle, &phone(), &key, &send("s1", "from afar")), Ok(()));
        handle.tick();
        assert!(
            notices(&base.sent.borrow()).iter().all(|n| !n.contains("Pixel 8")),
            "the tab is in the background"
        );
        assert!(handle.select_tab(first));
        handle.tick();
        let told = notices(&base.sent.borrow());
        assert_eq!(
            told.iter().filter(|n| *n == "sent from Pixel 8").count(),
            1,
            "said once the tab is shown: {told:?}"
        );
        handle.tick();
        assert_eq!(notices(&base.sent.borrow()), told, "and only once");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn a_stop_answered_after_its_session_was_replaced_leaves_the_new_session_alone() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-stop-replaced");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        let release = provider.hold_calls();
        provider.fail_interrupts(true);
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Ok(RemoteAccepted::Pending)
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while provider.calls_waiting() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the call never reached the provider"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let fresh = replace_session(&handle, tab, &dir);
        clear(&[&base]);
        drop(release);
        let result = result_of(&handle, "c1");
        assert!(matches!(result.outcome, Err(RemoteRefusal::Agent(_))), "{result:?}");
        handle.tick();
        assert!(
            matches!(
                handle.state.borrow().tabs.get(tab).unwrap().backend(),
                TabBackend::Live(_)
            ),
            "the old session's failure did not end the new one"
        );
        assert_eq!(fresh.closes(), 0);
        assert!(notices(&base.sent.borrow()).iter().all(|n| !n.contains("Pixel 8")));
        shut_down_every_tab(&handle);
    }

    #[test]
    fn a_result_kept_raises_the_wake() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-result-wake");
        let (handle, _base, _second) = handle_with_two_pages();
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        let release = provider.hold_calls();
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("c1", turn(&handle, tab, "t1"))),
            Ok(RemoteAccepted::Pending)
        );
        drop(release);
        {
            let mut state = handle.state.borrow_mut();
            let Answer::Spawned(rx) = &state.remote.calls[0].answer else {
                panic!("a spawned stop")
            };
            let outcome = rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the provider answered");
            assert!(
                matches!(
                    rx.recv_timeout(std::time::Duration::from_secs(60)),
                    Err(mpsc::RecvTimeoutError::Disconnected)
                ),
                "the callback returned after raising its wake"
            );
            let (tx, ready) = mpsc::channel();
            tx.send(outcome).unwrap();
            state.remote.calls[0].answer = Answer::Spawned(ready);
        }
        let woken = || handle.state.borrow().wake.as_ref().unwrap().take();
        woken();
        handle.tick();
        assert_eq!(
            handle.state.borrow().remote.results.len(),
            1,
            "the tick kept the result"
        );
        assert!(woken(), "the pump is woken to take it");
        shut_down_every_tab(&handle);
    }

    /// A small fixed-seed generator: the same inputs on every run, and no new dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
            &from[self.below(from.len())]
        }
    }

    /// The fields each kind may carry, and what `expect_work` may hold.
    fn known_fields(kind: &str) -> &'static [&'static str] {
        match kind {
            "interrupt" => &["type", "command_id", "expect_work"],
            "send" | "queue" => &["type", "command_id", "text"],
            "stop_background" | "take_back" => &["type", "command_id"],
            "stop_task" => &["type", "command_id", "task"],
            _ => &[],
        }
    }

    /// Whether `value` uses only fields the grammar names, at every level.
    fn only_known_fields(value: &serde_json::Value) -> bool {
        let Some(object) = value.as_object() else { return false };
        let kind = object.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let fields = known_fields(kind);
        if !object.keys().all(|key| fields.contains(&key.as_str())) {
            return false;
        }
        match object.get("expect_work") {
            None => true,
            Some(work) => match work.as_object() {
                Some(work) if work.len() == 1 => match (work.get("turn"), work.get("unseen")) {
                    (Some(turn), None) => turn
                        .as_object()
                        .is_some_and(|turn| turn.keys().all(|key| key == "id" || key == "generation")),
                    (None, Some(_)) => true,
                    _ => false,
                },
                _ => false,
            },
        }
    }

    fn random_value(rng: &mut Lcg, depth: usize) -> serde_json::Value {
        let strings = ["", "t1", "/compact", "!ls", " /x", "c1", "\u{0}", "\u{1f600}", "a b"];
        match rng.below(if depth > 1 { 5 } else { 7 }) {
            0 => serde_json::Value::Null,
            1 => serde_json::json!(rng.below(2) == 1),
            2 => serde_json::json!(rng.next() as i64 - (1 << 30)),
            3 => serde_json::json!(*rng.pick(&strings)),
            4 => serde_json::json!(rng.below(1 << 20) as u64),
            5 => serde_json::Value::Array((0..rng.below(3)).map(|_| random_value(rng, depth + 1)).collect()),
            _ => random_object(rng, depth + 1),
        }
    }

    fn random_object(rng: &mut Lcg, depth: usize) -> serde_json::Value {
        let keys = [
            "type",
            "command_id",
            "expect_work",
            "text",
            "task",
            "turn",
            "unseen",
            "id",
            "generation",
            "tab",
            "decision",
            "permission_id",
            "mode",
            "v",
        ];
        let kinds = [
            "interrupt",
            "send",
            "queue",
            "stop_background",
            "stop_task",
            "take_back",
            "permission_response",
            "cycle_mode",
            "confirm_bypass",
            "run_shell",
            "trust_answer",
            "send_message",
        ];
        let mut object = serde_json::Map::new();
        for _ in 0..rng.below(6) {
            let key = *rng.pick(&keys);
            let value = match key {
                "type" => serde_json::json!(*rng.pick(&kinds)),
                "command_id" if rng.below(4) > 0 => serde_json::json!(format!("c{}", rng.below(1000))),
                "expect_work" if rng.below(3) > 0 => {
                    if rng.below(2) == 0 {
                        serde_json::json!({ "unseen": rng.below(9) })
                    } else {
                        serde_json::json!({ "turn": { "id": "t1", "generation": rng.below(3) } })
                    }
                }
                _ => random_value(rng, depth),
            };
            object.insert(key.to_owned(), value);
        }
        serde_json::Value::Object(object)
    }

    /// A message the grammar takes, of a random kind.
    fn valid_message(rng: &mut Lcg) -> serde_json::Value {
        let id = format!("v{}", rng.below(1 << 16));
        match rng.below(6) {
            0 => serde_json::json!({ "type": "interrupt", "command_id": id, "expect_work": { "unseen": 1 } }),
            1 => serde_json::json!({ "type": "send", "command_id": id, "text": "fix it" }),
            2 => serde_json::json!({ "type": "queue", "command_id": id, "text": "then this" }),
            3 => serde_json::json!({ "type": "stop_background", "command_id": id }),
            4 => serde_json::json!({ "type": "stop_task", "command_id": id, "task": "b7" }),
            _ => serde_json::json!({ "type": "take_back", "command_id": id }),
        }
    }

    #[test]
    fn remote_inbound_parsing_never_panics_and_never_takes_an_unknown_field() {
        let mut rng = Lcg(0x5eed_7a5c_0007);
        let mut taken = 0;
        let extra = ["tab", "decision", "mode", "v", "seq", "answer", "allow"];
        for round in 0..100_000 {
            let raw = match round % 4 {
                // Objects built from the grammar's own words and others.
                0 | 1 => random_object(&mut rng, 0).to_string(),
                // A message the grammar takes, with one field it does not name.
                2 => {
                    let mut message = valid_message(&mut rng);
                    message[*rng.pick(&extra)] = random_value(&mut rng, 1);
                    let raw = message.to_string();
                    assert_eq!(parse_remote_inbound(&raw), Err(RemoteRefusal::Parse), "{raw}");
                    raw
                }
                // A message the grammar takes, cut short or with bytes changed.
                _ => {
                    let mut bytes = valid_message(&mut rng).to_string().into_bytes();
                    if rng.below(2) == 0 {
                        bytes.truncate(rng.below(bytes.len() + 1));
                    }
                    for _ in 0..rng.below(4) {
                        let at = rng.below(bytes.len().max(1));
                        if let Some(byte) = bytes.get_mut(at) {
                            *byte = rng.next() as u8;
                        }
                    }
                    String::from_utf8_lossy(&bytes).into_owned()
                }
            };
            if parse_remote_inbound(&raw).is_ok() {
                taken += 1;
                let value: serde_json::Value = serde_json::from_str(&raw).expect("what was taken is JSON");
                assert!(only_known_fields(&value), "an unknown field was taken: {raw}");
            }
        }
        assert!(taken > 1000, "the inputs reach the parser's accepting paths: {taken}");
    }

    // The window's audit records of remote messages.

    const AUDIT_INSTANCE: &str = "c0ffee00d15ea5e1";

    /// Gives the panel a sink writing in a fresh state directory; the sink and its audit directory.
    fn audited(handle: &AgentPanelHandle, label: &str) -> (crate::remote_audit_sink::AuditSink, PathBuf) {
        let state_dir = agent::state_dirs::test_workspace_dir(label);
        let sink = crate::remote_audit_sink::AuditSink::open(&state_dir, AUDIT_INSTANCE, std::sync::Arc::new(|| {}));
        handle.state.borrow_mut().remote.audit = Some(sink.clone());
        (sink, eitri_core::remote_audit::audit_dir(&state_dir))
    }

    /// Every record so far, oldest first.
    fn audit_records(sink: &crate::remote_audit_sink::AuditSink, dir: &Path) -> Vec<eitri_core::remote_audit::Record> {
        sink.flush();
        let mut records = eitri_core::remote_audit::read(dir, &Default::default(), None).unwrap_or_default();
        records.reverse();
        records
    }

    fn field<'a>(record: &'a eitri_core::remote_audit::Record, name: &str) -> Option<&'a str> {
        record.fields.get(name).and_then(serde_json::Value::as_str)
    }

    /// The refusals' reasons and kinds, oldest first.
    fn refusals(records: &[eitri_core::remote_audit::Record]) -> Vec<(String, Option<String>)> {
        records
            .iter()
            .filter(|r| r.event == "refused")
            .map(|r| {
                (
                    field(r, "reason").unwrap().to_owned(),
                    field(r, "kind").map(str::to_owned),
                )
            })
            .collect()
    }

    #[test]
    fn every_refusal_maps_to_a_reason() {
        use eitri_core::remote_audit::Reason as R;
        let at_send = |why: RemoteRefusal| audit_reason(&why, RefusedAt::Send);
        assert_eq!(at_send(RemoteRefusal::Parse), R::NotAllowed);
        assert_eq!(at_send(RemoteRefusal::NotYet), R::NotAllowed);
        assert_eq!(at_send(RemoteRefusal::NoSuchTab), R::TabNotLive);
        assert_eq!(at_send(RemoteRefusal::NotLive), R::TabNotLive);
        assert_eq!(at_send(RemoteRefusal::NotClaude), R::NotClaude);
        assert_eq!(at_send(RemoteRefusal::NotStarted), R::NotStarted);
        assert_eq!(at_send(RemoteRefusal::WorkChanged), R::WorkChanged);
        assert_eq!(at_send(RemoteRefusal::Duplicate), R::DuplicateCommand);
        assert_eq!(at_send(RemoteRefusal::ModeNotAllowed), R::Bypass);
        assert_eq!(
            audit_reason(&RemoteRefusal::ModeNotAllowed, RefusedAt::Flush),
            R::ModeWidened
        );
        assert_eq!(at_send(RemoteRefusal::SlashOrBang), R::SlashOrBang);
        assert_eq!(at_send(RemoteRefusal::TooLong), R::TooLong);
        assert_eq!(at_send(RemoteRefusal::NotYours), R::NotYours);
        assert_eq!(at_send(RemoteRefusal::Busy("a card waits".into())), R::Busy);
        assert_eq!(at_send(RemoteRefusal::Agent("the agent's words".into())), R::Agent);
    }

    #[test]
    fn sync_refusal_is_recorded() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("audit-sync-refusal");
        let (handle, base, _second) = handle_with_two_pages();
        let (sink, audit) = audited(&handle, "audit-sync-refusal-state");
        let (tab, _provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        assert_eq!(
            handle.remote_message(&phone(), &key, &send("a1", "/compact")),
            Err(RemoteRefusal::SlashOrBang)
        );
        assert_eq!(
            handle.remote_message(&phone(), &key, "not a message"),
            Err(RemoteRefusal::Parse)
        );
        assert_eq!(
            handle.remote_message(&phone(), &key, &send("a1", "again")),
            Err(RemoteRefusal::Duplicate)
        );
        let elsewhere = RowKey::parse("k-0000000000000000").unwrap();
        assert_eq!(
            handle.remote_message(&phone(), &elsewhere, &send("a2", "hello")),
            Err(RemoteRefusal::NoSuchTab)
        );
        let records = audit_records(&sink, &audit);
        assert_eq!(
            refusals(&records),
            [
                ("slash_or_bang".to_owned(), Some("send".to_owned())),
                ("not_allowed".to_owned(), None),
                ("duplicate_command".to_owned(), Some("send".to_owned())),
                ("tab_not_live".to_owned(), Some("send".to_owned())),
            ]
        );
        let first = &records[0];
        assert_eq!(first.writer, AUDIT_INSTANCE);
        assert_eq!(
            (field(first, "device"), field(first, "device_name"), field(first, "key")),
            (Some("d1"), Some("Pixel 8"), Some(key.as_str()))
        );
        assert!(records.iter().all(|r| r.event != "cmd"), "nothing was taken");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn provider_refusal_is_recorded() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("audit-provider-refusal");
        let (handle, _base, _second) = handle_with_two_pages();
        let (sink, audit) = audited(&handle, "audit-provider-refusal-state");
        // A provider that cannot interrupt answers the stop with its own words.
        let (tab, provider, key) = tab_of(&handle, &dir, RecordingProvider::default());
        start_turn(&handle, tab, &provider, "t1");
        let refused = settle(&handle, &phone(), &key, &interrupt("c1", turn(&handle, tab, "t1")));
        let Err(RemoteRefusal::Agent(words)) = refused else {
            panic!("the agent refused: {refused:?}")
        };
        let records = audit_records(&sink, &audit);
        let events: Vec<&str> = records.iter().map(|r| r.event.as_str()).collect();
        assert_eq!(events, ["cmd", "refused"], "taken, then refused by the agent");
        assert_eq!(refusals(&records), [("agent".to_owned(), Some("interrupt".to_owned()))]);
        let line = &records[1].line;
        assert!(!line.contains(&words), "the agent's words are not recorded: {line}");
        let mut fields: Vec<&str> = records[1].fields.keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(fields, ["device", "device_name", "key", "kind", "reason"]);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn mode_widened_flush_is_recorded() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("audit-mode-widened");
        let (handle, base, _second) = handle_with_two_pages();
        let (sink, audit) = audited(&handle, "audit-mode-widened-state");
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        start_turn(&handle, tab, &provider, "t1");
        assert_eq!(
            handle.remote_message(&phone(), &key, &queue("q1", "do it")),
            Ok(RemoteAccepted::Done)
        );
        set_mode(&handle, &base, tab, SessionModeChoice::Auto);
        end_turn(&handle, tab, &provider, "t1");
        assert_eq!(result_of(&handle, "q1").outcome, Err(RemoteRefusal::ModeNotAllowed));
        let records = audit_records(&sink, &audit);
        assert_eq!(records[0].event, "cmd");
        assert_eq!(field(&records[0], "kind"), Some("queue"));
        assert_eq!(
            refusals(&records),
            [("mode_widened".to_owned(), Some("queue".to_owned()))]
        );
        assert_eq!(field(records.last().unwrap(), "key"), Some(key.as_str()));
        shut_down_every_tab(&handle);
    }

    fn stamped_queue(handle: &AgentPanelHandle, tab: TabId, count: usize) -> PathBuf {
        use eitri_core::remote_devices::{authorize_act, token_hash_hex};
        use eitri_core::tab_set::{Queued, QueuedFrom};
        let dir = agent::state_dirs::test_workspace_dir("withdrawal-stamp");
        let store = dir.join("devices.json");
        agent::private_fs::write_private(
            &store,
            serde_json::json!({"v": 1, "devices": [{
                "id": "d1", "name": "Pixel 8", "token_sha256": token_hash_hex("tok"),
                "grant": "watch+steer"
            }]})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        let (_, _, stamp) = authorize_act(&store, "d1", "tok").unwrap().unwrap();
        let mut state = handle.state.borrow_mut();
        let tab = state.tabs.get_mut(tab).unwrap();
        for n in 0..count {
            let mut item = Queued::held("SECRET", "SECRET".into(), 1);
            item.from = QueuedFrom::Remote {
                asker: RemoteAsker {
                    device_id: "d1".into(),
                    device: "Pixel 8".into(),
                    command_id: format!("q{n}"),
                    conn: None,
                    authority: Some(stamp.clone()),
                },
                accepted_mode: SessionModeChoice::Auto,
            };
            tab.session.queue.push(item);
        }
        store
    }

    #[test]
    fn a_refused_stamped_item_uses_a_slow_tick() {
        use crate::panel_wake::NextTick;
        use std::time::{Duration, Instant};
        let (handle, _) = settled_panel();
        let tab = handle.state.borrow().tabs.active();
        let store = stamped_queue(&handle, tab, 1);
        let now = Instant::now();
        assert_eq!(handle.run_tick(now), NextTick::At(now + AgentPanelHandle::TICK));
        handle.state.borrow_mut().tabs.get_mut(tab).unwrap().session.queue[0].refused = true;
        assert!(!handle.state.borrow().tabs.needs_polling());
        assert_eq!(handle.run_tick(now), NextTick::At(now + Duration::from_secs(1)));
        eitri_core::remote_devices::update(&store, |s| {
            s.remove("d1");
        })
        .unwrap();
        handle.run_tick(now + Duration::from_secs(1));
        assert!(handle.state.borrow().tabs.queue_texts(tab).is_empty());
        assert_eq!(handle.run_tick(now + Duration::from_secs(2)), NextTick::OnWake);
    }

    #[test]
    fn withdrawn_messages_without_a_backend_are_told_or_owed() {
        for hidden in [false, true] {
            for count in [1, 2] {
                let (handle, base, _) = handle_with_two_pages();
                let tab = handle.state.borrow().tabs.active();
                let store = stamped_queue(&handle, tab, count);
                if hidden {
                    handle.state.borrow_mut().tabs.open(AgentId::Claude);
                }
                clear(&[&base]);
                eitri_core::remote_devices::update(&store, |s| {
                    s.remove("d1");
                })
                .unwrap();
                handle.tick();
                if hidden {
                    assert!(notices(&base.sent.borrow()).is_empty());
                    assert_eq!(handle.state.borrow().remote.notices.len(), 1);
                    assert!(handle.select_tab(tab));
                    handle.tick();
                }
                let noun = if count == 1 { "message" } else { "messages" };
                let expected =
                    format!("Withdrew {count} queued {noun} from Pixel 8: device steering permission withdrawn");
                assert_eq!(notices(&base.sent.borrow()), vec![expected]);
                clear(&[&base]);
                handle.tick();
                assert!(notices(&base.sent.borrow()).is_empty());
            }
        }
    }

    #[test]
    fn one_withdrawn_message_folds_a_singular_row() {
        let dir = agent::state_dirs::test_workspace_dir("withdrawal-singular");
        let (handle, _, _) = handle_with_two_pages();
        let (tab, _) = live_tab(&handle, &dir);
        let store = stamped_queue(&handle, tab, 1);
        eitri_core::remote_devices::update(&store, |s| {
            s.remove("d1");
        })
        .unwrap();
        handle.tick();
        let state = handle.state.borrow();
        let notices = state
            .tabs
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .notices
            .clone();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].content.as_deref(),
            Some("Withdrew 1 queued message from Pixel 8: device steering permission withdrawn")
        );
        drop(state);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn each_withdrawn_message_is_audited_without_its_text() {
        for revoke in [false, true] {
            let (handle, _, _) = handle_with_two_pages();
            let tab = handle.state.borrow().tabs.active();
            let key = handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .row_key()
                .as_str()
                .to_owned();
            let store = stamped_queue(&handle, tab, 2);
            let (sink, audit) = audited(&handle, "audit-withdrawal");
            eitri_core::remote_devices::update(&store, |s| {
                if revoke {
                    s.remove("d1");
                } else {
                    s.get_mut("d1").unwrap().grant = eitri_core::remote_devices::Grant::Watch;
                }
            })
            .unwrap();
            handle.tick();
            handle.tick();
            let records = audit_records(&sink, &audit);
            assert_eq!(
                refusals(&records),
                vec![("revoked_token".to_owned(), Some("queue".to_owned())); 2]
            );
            for record in &records {
                assert_eq!(field(record, "key"), Some(key.as_str()));
                assert_eq!(field(record, "device"), Some("d1"));
                assert_eq!(field(record, "device_name"), Some("Pixel 8"));
            }
            assert!(!std::fs::read_to_string(audit.join("audit.jsonl"))
                .unwrap()
                .contains("SECRET"));
        }
    }

    #[test]
    fn accepted_send_records_cmd_with_length_and_hash() {
        const SENTINEL: &str = "sentinel-prompt-9f3c1e7a never in the log";
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("audit-accepted-send");
        let (handle, base, _second) = handle_with_two_pages();
        let (sink, audit) = audited(&handle, "audit-accepted-send-state");
        let (tab, _provider, key) = interruptible_tab(&handle, &dir);
        handle.tick();
        set_mode(&handle, &base, tab, SessionModeChoice::Manual);
        assert_eq!(settle(&handle, &phone(), &key, &send("s1", SENTINEL)), Ok(()));
        let records = audit_records(&sink, &audit);
        let cmd: Vec<_> = records.iter().filter(|r| r.event == "cmd").collect();
        assert_eq!(cmd.len(), 1, "one command taken");
        let digest = eitri_core::remote_audit::text_digest(SENTINEL);
        assert_eq!(
            cmd[0].fields.get("text_len").and_then(serde_json::Value::as_u64),
            Some(SENTINEL.len() as u64)
        );
        assert_eq!(field(cmd[0], "text_sha256"), Some(digest.sha256.as_str()));
        assert_eq!(
            (field(cmd[0], "kind"), field(cmd[0], "command_id")),
            (Some("send"), Some("s1"))
        );
        let bytes = std::fs::read(audit.join("audit.jsonl")).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(SENTINEL), "the text never reaches the file");
        shut_down_every_tab(&handle);
    }

    #[test]
    fn cmd_names_tab_key_number_and_revision() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("audit-cmd-fields");
        let (handle, _base, _second) = handle_with_two_pages();
        let (sink, audit) = audited(&handle, "audit-cmd-fields-state");
        // Another tab first, so the number is not the first one.
        let _first = interruptible_tab(&handle, &dir);
        let (tab, provider, key) = interruptible_tab(&handle, &dir);
        start_turn(&handle, tab, &provider, "t1");
        let (number, revision) = {
            let state = handle.state.borrow();
            let held = state.tabs.get(tab).unwrap();
            let read = (held.number, held.live().unwrap().projection().last_revision);
            read
        };
        assert_eq!(
            handle.remote_message(&phone(), &key, &interrupt("i1", turn(&handle, tab, "t1"))),
            Ok(RemoteAccepted::Pending)
        );
        let records = audit_records(&sink, &audit);
        let cmd = records.iter().find(|r| r.event == "cmd").expect("the stop is recorded");
        assert_eq!(
            (
                field(cmd, "kind"),
                field(cmd, "key"),
                field(cmd, "instance"),
                field(cmd, "command_id"),
                field(cmd, "device"),
            ),
            (
                Some("interrupt"),
                Some(key.as_str()),
                Some(AUDIT_INSTANCE),
                Some("i1"),
                Some("d1")
            )
        );
        assert_eq!(
            (
                cmd.fields.get("tab").and_then(serde_json::Value::as_u64),
                cmd.fields.get("revision").and_then(serde_json::Value::as_u64)
            ),
            (Some(u64::from(number)), Some(revision))
        );
        assert!(!cmd.fields.contains_key("text_len"), "a stop carries no text");
        let _ = result_of(&handle, "i1");
        shut_down_every_tab(&handle);
    }
}
