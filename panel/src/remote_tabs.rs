//! A remote project's tabs in this window: how they start, and the questions only a remote tab asks -- what to do
//! about requests left waiting on the server after a gap, how to close a tab whose server cannot be reached, and
//! whether to continue a session whose agent the server no longer runs. Each question is a `remote_prompt` for the
//! tab's page, answered by key; nothing here waits on the network (the work a choice starts runs on a worker).
//!
//! Stops of agents that could not be stopped when their tab closed are kept in the project's record and sent by the
//! window's stop sender whenever the server can be reached again.

use super::*;
use eitri_core::agent_bridge::{
    serialize_remote_prompt_clear_for_js, serialize_remote_prompt_for_js, RemotePromptAction, RemotePromptChoice,
};
use eitri_core::remote_project::RemoteProjects;

/// Why a remote tab could not start in this window yet: its connection to the server is not open.
pub(super) fn no_connection(host: &str) -> String {
    format!("this window has no connection to {host} yet")
}

/// A remote tab's agent as the questions name it: its sidecar and session on the server. A question is about one
/// such agent, never about whatever the tab holds later.
type AgentKey = (String, String);

/// The remote questions open in this window, and what the choices made started.
#[derive(Default)]
pub(super) struct RemotePrompts {
    next_nonce: u64,
    open: BTreeMap<TabId, Asked>,
    /// Agents whose session's end was asked about, or needed no question, by tab: each end is asked about once. A
    /// tab that reaches another agent (a resume, a fresh start) is asked about that one's end afresh.
    endings_seen: BTreeSet<(TabId, AgentKey)>,
    /// Resumes the person chose after the server lost a tab's agent, by tab: the CLI's session that may be
    /// continued through a new sidecar, and nothing else.
    resumes: BTreeMap<TabId, String>,
    /// "Deny them and stop the agent" workers, each with the agent it was chosen for: what it reports acts on that
    /// agent only, never on one the tab holds since.
    running: Vec<(
        TabId,
        AgentKey,
        mpsc::Receiver<Result<agent::DenyAndStopOutcome, String>>,
    )>,
}

struct Asked {
    nonce: u64,
    about: About,
    /// The agent the question is about; `None` for a close, which is about the tab.
    agent: Option<AgentKey>,
    /// The question as its page reads it, sent again to a page that comes up without it.
    payload: String,
}

enum About {
    /// Requests the server listed as waiting after a gap, by permission id.
    Gap { waiting: Vec<String> },
    /// A close of a tab whose server could not be reached.
    Close,
    /// The server no longer runs the tab's agent; the CLI's session can be continued.
    Gone { cli_session: String },
}

impl About {
    fn offers(&self, action: RemotePromptAction) -> bool {
        matches!(
            (self, action),
            (
                About::Gap { .. },
                RemotePromptAction::DenyAndStop | RemotePromptAction::LeaveWaiting
            ) | (
                About::Close,
                RemotePromptAction::StopWhenBack | RemotePromptAction::LeaveRunning
            ) | (About::Gone { .. }, RemotePromptAction::Resume)
        )
    }
}

impl RemotePrompts {
    /// Whether the person chose to continue `provider_session_id` in `tab`: the one resume a remote tab starts.
    pub(super) fn take_resume(&mut self, tab: TabId, provider_session_id: &str) -> bool {
        if self
            .resumes
            .get(&tab)
            .is_some_and(|session| session == provider_session_id)
        {
            self.resumes.remove(&tab);
            true
        } else {
            false
        }
    }

    /// The tab is gone: its questions and choices go with it.
    pub(super) fn forget_tab(&mut self, tab: TabId) {
        self.open.remove(&tab);
        self.endings_seen.retain(|(seen, _)| *seen != tab);
        self.resumes.remove(&tab);
    }

    /// Whether a choice's worker is still running, so the tick keeps reading it.
    pub(super) fn needs_polling(&self) -> bool {
        !self.running.is_empty()
    }

    /// Opens a question for `tab` (replacing one open there) and returns the envelope its page is sent.
    fn ask(
        &mut self,
        tab: TabId,
        about: About,
        agent: Option<AgentKey>,
        lines: &[String],
        choices: &[RemotePromptChoice],
    ) -> String {
        self.next_nonce += 1;
        let payload = serialize_remote_prompt_for_js(tab.0, self.next_nonce, lines, choices);
        self.open.insert(
            tab,
            Asked {
                nonce: self.next_nonce,
                about,
                agent,
                payload: payload.clone(),
            },
        );
        payload
    }
}

fn host_of(state: &AgentPanelState, tab: TabId) -> String {
    match state.tabs.get(tab).map(|t| t.origin()) {
        Some(eitri_core::remote_project::TabOrigin::Remote { host, .. }) => host.clone(),
        _ => "the server".to_string(),
    }
}

fn number_of(state: &AgentPanelState, tab: TabId) -> u16 {
    state.tabs.get(tab).map_or(0, |t| t.number)
}

/// The agent `tab` holds now, as a question names it.
/// The agent `tab` holds: its live provider's, or, before its start reached it, the saved agent it names.
fn agent_key(state: &AgentPanelState, tab: TabId) -> Option<AgentKey> {
    let t = state.tabs.get(tab)?;
    match t.live() {
        Some(backend) => {
            let handoff = backend.helper_handoff()?;
            Some((handoff.stop_key(), handoff.session_id))
        }
        None => {
            let (handoff, _) = t.remote_agent.as_ref()?;
            Some((handoff.stop_key(), handoff.session_id.clone()))
        }
    }
}

fn choice(key: char, label: String, action: RemotePromptAction) -> RemotePromptChoice {
    RemotePromptChoice { key, label, action }
}

/// Every tick: the remote tabs' questions, the work the person's choices started, the stops to keep and send.
pub(super) fn tick(state: &mut AgentPanelState, out: &mut Outbox) {
    state.retiring.keep_at = window_root(state);
    drop_questions_about_other_agents(state, out);
    ask_about_endings(state, out);
    collect_choices(state, out);
    keep_what_teardowns_did_not_stop(state, out);
    state.remote_stops.collect_writes(out);
    state.remote_stops.watch_links(&state.tabs);
}

/// After the pump: a remote tab whose session ended on the helper's word is asked about it once per agent, when the
/// tab shows the end. After a gap with requests waiting there, the question lists them; after the agent is gone,
/// it offers to continue the CLI's session (as the agent named it, or as the tab was saved with). A gap with nothing
/// waiting, or an end whose session id is unknown, asks nothing. The question names its tab; the page draws it
/// only while that tab is shown.
pub(super) fn ask_about_endings(state: &mut AgentPanelState, out: &mut Outbox) {
    let ended: Vec<(TabId, AgentKey, agent::RemoteEnding, Option<String>)> = state
        .tabs
        .tabs()
        .iter()
        .filter(|t| t.is_remote())
        .filter_map(|t| {
            let backend = t.live()?;
            let ending = backend.remote_ending()?;
            // Its own statement: the projection's guard is gone before the session id is read.
            let shown = matches!(backend.projection().status, agent::ProjectionStatus::Unavailable { .. });
            if !shown {
                return None;
            }
            let key = backend
                .helper_handoff()
                .map(|h| (h.stop_key(), h.session_id))
                .unwrap_or_default();
            if state.remote_prompts.endings_seen.contains(&(t.id, key.clone())) {
                return None;
            }
            let session = backend
                .provider_session_id()
                .or_else(|| t.remote_agent.as_ref().and_then(|(_, session)| session.clone()));
            Some((t.id, key, ending, session))
        })
        .collect();
    for (tab, key, ending, cli_session) in ended {
        state.remote_prompts.endings_seen.insert((tab, key.clone()));
        let host = host_of(state, tab);
        let number = number_of(state, tab);
        let (lines, choices, about) = match ending {
            agent::RemoteEnding::Gap { waiting } if !waiting.is_empty() => {
                let mut lines = vec![format!(
                    "tab {number}: the agent on {host} may still be running; these requests wait there for an answer:"
                )];
                lines.extend(
                    waiting
                        .iter()
                        .map(|request| format!("{}: {}", request.tool_name, request.input_line)),
                );
                // Keys the page's BROWSE table leaves unbound, as the close question's are: its `d` is the deny habit
                // it refuses and its `l` moves to the next control, and either would answer this for good.
                (
                    lines,
                    vec![
                        choice(
                            's',
                            "deny them and stop the agent".into(),
                            RemotePromptAction::DenyAndStop,
                        ),
                        choice(
                            'L',
                            format!("leave it waiting on {host}"),
                            RemotePromptAction::LeaveWaiting,
                        ),
                    ],
                    About::Gap {
                        waiting: waiting.into_iter().map(|request| request.permission_id).collect(),
                    },
                )
            }
            agent::RemoteEnding::AgentGone => match cli_session {
                // A shifted `R`: BROWSE's `r` restarts an ended session afresh, which is what this tab shows.
                Some(cli_session) => (
                    vec![format!("tab {number}: {host} no longer runs this tab's agent")],
                    vec![choice(
                        'R',
                        format!("resume the session on {host}"),
                        RemotePromptAction::Resume,
                    )],
                    About::Gone { cli_session },
                ),
                None => continue,
            },
            agent::RemoteEnding::Gap { .. } => continue,
        };
        // A question names the agent it is about when the tab knows it; one it cannot name is about the tab.
        let agent = (key != AgentKey::default()).then_some(key);
        let payload = state.remote_prompts.ask(tab, about, agent, &lines, &choices);
        send_to_tabs_page(state, tab, payload, out);
    }
}

/// A question about an agent the tab no longer holds (it was started over, or holds a new session) goes, from its
/// page too: nothing it offers would act on the agent it named.
fn drop_questions_about_other_agents(state: &mut AgentPanelState, out: &mut Outbox) {
    let stale: Vec<(TabId, u64)> = state
        .remote_prompts
        .open
        .iter()
        .filter(|(tab, asked)| asked.agent.is_some() && asked.agent != agent_key(state, **tab))
        .map(|(tab, asked)| (*tab, asked.nonce))
        .collect();
    for (tab, nonce) in stale {
        state.remote_prompts.open.remove(&tab);
        if state.tabs.get(tab).is_some() {
            send_to_tabs_page(state, tab, serialize_remote_prompt_clear_for_js(tab.0, nonce), out);
        }
    }
}

/// A page that came up (a reload, a tab broken out into a view of its own) is sent the questions open for the tabs
/// it shows, as they were asked: the document that was there took them with it.
pub(super) fn resend_open_questions(state: &AgentPanelState, view: ViewId, out: &mut Outbox) {
    for (tab, asked) in &state.remote_prompts.open {
        if state.tabs.get(*tab).is_some() && state.tabs.view_of(*tab) == view {
            out.send(view, asked.payload.clone());
        }
    }
}

/// Where the window's remote project is on its server, for a stop kept in its record; `None` in a local window.
fn window_root(state: &AgentPanelState) -> Option<std::path::PathBuf> {
    match state.tabs.project_origin() {
        eitri_core::remote_project::TabOrigin::Remote { root, .. } => Some(root.clone()),
        _ => None,
    }
}

/// The agent a remote tab names only through its saved entry (a reattach still connecting, or one whose start
/// failed): where its project is, its host and its sidecar. Such a tab closed or started over has nothing that ends
/// the agent, so its stop is kept ([`keep_unreached`]).
pub(super) fn unreached_agent(
    state: &AgentPanelState,
    tab: TabId,
) -> Option<(std::path::PathBuf, agent::HelperHandoff)> {
    let t = state.tabs.get(tab)?;
    if t.live().is_some() {
        return None;
    }
    let (helper, _) = t.remote_agent.as_ref()?;
    match t.origin() {
        eitri_core::remote_project::TabOrigin::Remote { root, .. } => Some((root.clone(), helper.clone())),
        _ => None,
    }
}

/// Keeps the stop of an agent [`unreached_agent`] named, to be sent once the server can be reached (a helper that
/// no longer knows the sidecar counts it as stopped).
pub(super) fn keep_unreached(state: &mut AgentPanelState, agent: Option<(std::path::PathBuf, agent::HelperHandoff)>) {
    if let Some((root, handoff)) = agent {
        state.remote_stops.keep(root, handoff);
    }
}

/// Each "deny them and stop the agent" worker says what did not go through; one that succeeded leaves its tab to
/// say so. An agent that was stopped is not saved again, whatever became of a deny -- but only while the tab still
/// holds that agent: a tab started over meanwhile holds another, which stays saved.
fn collect_choices(state: &mut AgentPanelState, out: &mut Outbox) {
    let mut running = std::mem::take(&mut state.remote_prompts.running);
    running.retain(|(tab, agent, rx)| {
        let (stopped, failures) = match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return true,
            Err(mpsc::TryRecvError::Disconnected) => return false,
            Ok(Ok(outcome)) => (outcome.stopped, outcome.failures),
            Ok(Err(why)) => (false, vec![why]),
        };
        let same_agent = agent_key(state, *tab).as_ref() == Some(agent);
        if stopped && same_agent {
            state.tabs.remote_agent_stopped(*tab);
        }
        if !failures.is_empty() {
            let why = failures.join("; ");
            if state.tabs.get(*tab).is_some() && same_agent {
                send_to_tabs_page(state, *tab, tab_notice(state, *tab, &why), out);
            } else {
                window_notice(
                    out,
                    &format!("the agent stopped earlier on {}: {why}", host_of(state, *tab)),
                );
            }
        }
        false
    });
    state.remote_prompts.running.extend(running);
}

/// A notice for the whole window, on the base view's page: about a tab that is gone.
fn window_notice(out: &mut Outbox, text: &str) {
    eprintln!("[agent_panel] {text}");
    out.send(ViewId::BASE, eitri_core::agent_bridge::serialize_notice_for_js(text));
}

/// Whether `tab` is a remote tab whose agent may still be running while its server cannot be reached: the link is
/// neither live nor catching up (a replay runs over a working connection).
fn unreachable(state: &AgentPanelState, tab: TabId) -> bool {
    state.tabs.get(tab).is_some_and(|t| {
        t.is_remote()
            && t.live().is_some_and(|backend| {
                backend.remote_ending().is_none()
                    && !matches!(
                        backend.link_state(),
                        None | Some(agent::RemoteLinkState::Live) | Some(agent::RemoteLinkState::CatchingUp { .. })
                    )
            })
    })
}

/// A close asked for a remote tab whose agent may still be running while its server cannot be reached: the person
/// is asked first whether to stop it once the server is back or leave it running. `true` when the question went
/// out, and the close waits for its answer.
pub(super) fn ask_before_closing(state: &mut AgentPanelState, tab: TabId, out: &mut Outbox) -> bool {
    if !unreachable(state, tab) {
        return false;
    }
    let host = host_of(state, tab);
    let number = number_of(state, tab);
    let lines = vec![format!(
        "tab {number}: {host} cannot be reached; its agent keeps running there"
    )];
    // Keys the page's own BROWSE table leaves unbound, so a key meant for moving about never answers it: leaving
    // the agent running with nothing to stop it later takes a shifted `L`.
    let choices = vec![
        choice(
            's',
            format!("stop it when {host} is back"),
            RemotePromptAction::StopWhenBack,
        ),
        choice(
            'L',
            "leave it running, with nothing to stop it later".into(),
            RemotePromptAction::LeaveRunning,
        ),
    ];
    // About the agent the tab holds now: one that replaces it (a start over, a new session) drops the question.
    let agent = agent_key(state, tab);
    let payload = state.remote_prompts.ask(tab, About::Close, agent, &lines, &choices);
    send_to_tabs_page(state, tab, payload, out);
    true
}

/// A close nobody asked about on this one tab (closing the others, killing a view): a remote tab whose server
/// cannot be reached has its stop kept for when the server is back and closes letting its agent run until then,
/// and the window says so; every other tab closes as [`close_tab`] closes it.
pub(super) fn close_tab_unasked(state: &mut AgentPanelState, tab: TabId, out: &mut Outbox) -> Result<(), &'static str> {
    if !unreachable(state, tab) {
        return close_tab(state, tab, out);
    }
    let host = host_of(state, tab);
    let number = number_of(state, tab);
    let view = state.tabs.view_of(tab);
    keep_stop_of_tab(state, tab).map_err(|_| "this tab's agent cannot be named to stop it later; close it alone")?;
    close_tab_as(state, tab, out, Ending::LetGo)?;
    state.remote_prompts.forget_tab(tab);
    out.send(
        view,
        eitri_core::agent_bridge::serialize_notice_for_js(&format!(
            "the agent of tab {number} keeps running on {host} until {host} is back; then it is stopped"
        )),
    );
    Ok(())
}

/// The person's choice on a remote question: only the question open for `tab` under `nonce` is answered, only with a
/// choice it offered, and only while the tab still holds the agent it was asked about. `Err` says why nothing was
/// done.
pub(super) fn answer_prompt(
    state: &mut AgentPanelState,
    tab: TabId,
    nonce: u64,
    action: RemotePromptAction,
    request_id: &str,
    view: ViewId,
    out: &mut Outbox,
) -> Result<(), String> {
    let open = state.remote_prompts.open.get(&tab);
    if !open.is_some_and(|asked| asked.nonce == nonce && asked.about.offers(action)) {
        return Err("that question is no longer open".to_string());
    }
    let asked_for = open.and_then(|asked| asked.agent.clone());
    if asked_for.is_some() && asked_for != agent_key(state, tab) {
        state.remote_prompts.open.remove(&tab);
        out.send(view, serialize_remote_prompt_clear_for_js(tab.0, nonce));
        return Err("that question was about an agent this tab no longer holds".to_string());
    }
    let Asked { about, agent, .. } = state.remote_prompts.open.remove(&tab).expect("checked above");
    match (about, action) {
        (About::Gap { waiting }, RemotePromptAction::DenyAndStop) => {
            let backend = state
                .tabs
                .get(tab)
                .and_then(|t| t.live())
                .ok_or_else(|| "this tab has no session".to_string())?;
            // The ordinary answer, request by request, then the stop, on the conversation's own worker.
            let rx = backend.deny_waiting_and_stop(waiting);
            state.remote_prompts.running.push((tab, agent.unwrap_or_default(), rx));
        }
        (About::Gap { .. }, _) => {}
        (About::Close, how) => {
            if how == RemotePromptAction::StopWhenBack {
                keep_stop_of_tab(state, tab)?;
            }
            close_tab_asked_as(state, tab, out, Ending::LetGo).map_err(str::to_string)?;
        }
        (About::Gone { cli_session }, _) => {
            state.remote_prompts.resumes.insert(tab, cli_session.clone());
            let unreached = unreached_agent(state, tab);
            match state.tabs.reset(tab) {
                Ok(old) => {
                    keep_unreached(state, unreached);
                    if let Some(old) = old {
                        state.retiring.backend(old);
                    }
                }
                Err(why) => {
                    state.remote_prompts.resumes.remove(&tab);
                    return Err(why.to_string());
                }
            }
            state.trust.forget_tab(tab);
            let start = DeferredStart::Resume {
                request_id: request_id.to_string(),
                provider_session_id: cli_session,
                resumed_title: None,
                resumed_name: None,
            };
            gate_remote_resume(state, tab, start)?;
            note_start_page(state, tab, request_id, view);
            return Ok(());
        }
    }
    out.send(view, serialize_command_result_for_js(request_id, Ok(())));
    Ok(())
}

/// Keeps the tab's agent in its project's record, to be stopped once the server can be reached
/// ([`RemoteStops::keep`]).
fn keep_stop_of_tab(state: &mut AgentPanelState, tab: TabId) -> Result<(), String> {
    let t = state.tabs.get(tab).ok_or_else(|| "no such tab".to_string())?;
    let eitri_core::remote_project::TabOrigin::Remote { root, .. } = t.origin().clone() else {
        return Err("this tab's agent does not run on a server".to_string());
    };
    let handoff = t
        .live()
        .and_then(AgentBackend::helper_handoff)
        .ok_or_else(|| "this tab's agent cannot be named to stop it later".to_string())?;
    state.remote_stops.keep(root, handoff);
    Ok(())
}

/// A close whose stop did not go through (the server stopped answering between the person's close and the stop):
/// the stop is kept as "stop it when <host> is back" keeps it, and the window says so.
fn keep_what_teardowns_did_not_stop(state: &mut AgentPanelState, out: &mut Outbox) {
    for NotStopped { agent, why, kept } in state.retiring.take_not_stopped() {
        if kept {
            // Its teardown kept the stop before ending it: the sender sends it once the server can be reached.
            window_notice(
                out,
                &format!(
                    "the agent on {} was not stopped ({why}); it will be stopped when {} is back",
                    agent.host, agent.host
                ),
            );
            state.remote_stops.send_now();
            continue;
        }
        let root = state
            .server
            .as_ref()
            .map(|server| server.project())
            .filter(|project| project.host_key_sha256 == agent.host_key_sha256)
            .map(|project| project.root.clone())
            .or_else(|| {
                state.tabs.tabs().iter().find_map(|t| match t.origin() {
                    eitri_core::remote_project::TabOrigin::Remote {
                        host_key_sha256, root, ..
                    } if *host_key_sha256 == agent.host_key_sha256 => Some(root.clone()),
                    _ => None,
                })
            });
        let host = agent.host.clone();
        match root {
            Some(root) => {
                window_notice(
                    out,
                    &format!("the agent on {host} was not stopped ({why}); it will be stopped when {host} is back"),
                );
                state.remote_stops.keep(root, agent);
            }
            None => window_notice(
                out,
                &format!("the agent on {host} was not stopped ({why}), and no project here can keep its stop"),
            ),
        }
    }
}

/// The stops this window keeps for agents on its server that could not be stopped when their tab closed: each is
/// written to the project's record on a worker, and the window's stop sender sends them whenever the server can be
/// reached again (the window is given its connection, a write lands, a tab's link comes back).
#[derive(Default)]
pub(super) struct RemoteStops {
    /// Record writes on their way, each with the host it is about.
    writes: Vec<(String, mpsc::Receiver<Result<(), String>>)>,
    sender: Option<StopSender>,
    /// Whether each remote tab's server could be reached at the last tick.
    reachable: BTreeMap<TabId, bool>,
}

/// What sends the kept stops: a worker that sends them when woken, and again on its own while any failed.
pub(super) struct StopSender {
    wake: mpsc::Sender<()>,
}

/// How long the stop sender waits before trying again stops that did not go out.
const STOP_RETRY: std::time::Duration = std::time::Duration::from_secs(15);

impl StopSender {
    /// Starts the worker. `send` sends every kept stop and says which went or why some did not; it runs at once,
    /// then whenever woken, and every [`STOP_RETRY`] while the last run left some unsent. The worker ends once this
    /// sender is dropped (the window is gone).
    pub(super) fn start(send: impl Fn() -> Result<Vec<String>, String> + Send + 'static) -> Self {
        Self::start_retrying_after(send, STOP_RETRY)
    }

    /// [`Self::start`], trying again after `retry` instead.
    fn start_retrying_after(
        send: impl Fn() -> Result<Vec<String>, String> + Send + 'static,
        retry: std::time::Duration,
    ) -> Self {
        let (wake, woken) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            loop {
                let failed = match send() {
                    Ok(_) => false,
                    Err(why) => {
                        eprintln!("[agent_panel] stops kept for later: {why}");
                        true
                    }
                };
                let next = if failed {
                    woken.recv_timeout(retry)
                } else {
                    woken.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match next {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {
                        // Wakes that came meanwhile are one more run, not one each.
                        while woken.try_recv().is_ok() {}
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Self { wake }
    }

    fn wake(&self) {
        let _ = self.wake.send(());
    }
}

impl RemoteStops {
    /// The window's sender of kept stops, given with its connection.
    pub(super) fn set_sender(&mut self, sender: StopSender) {
        self.sender = Some(sender);
    }

    /// Keeps the agent's stop in its matching project record on a worker. A write that fails is
    /// said to the window (the agent keeps running then, with no record of it here).
    pub(super) fn keep(&mut self, root: std::path::PathBuf, agent: agent::HelperHandoff) {
        let host = agent.host.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(write_stop(&root, &agent, true));
        });
        self.writes.push((host, rx));
    }

    /// Whether a record write is still on its way.
    pub(super) fn needs_polling(&self) -> bool {
        !self.writes.is_empty()
    }

    /// The record writes still on their way, for the window's close to wait for: a stop half kept is no stop.
    pub(super) fn take_writes(&mut self) -> Vec<mpsc::Receiver<Result<(), String>>> {
        std::mem::take(&mut self.writes).into_iter().map(|(_, rx)| rx).collect()
    }

    fn collect_writes(&mut self, out: &mut Outbox) {
        let mut landed = false;
        self.writes.retain(|(host, rx)| match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => true,
            Ok(Ok(())) => {
                landed = true;
                false
            }
            Ok(Err(why)) => {
                window_notice(
                    out,
                    &format!("the stop of an agent on {host} could not be kept ({why}); it keeps running there"),
                );
                false
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                window_notice(
                    out,
                    &format!("the stop of an agent on {host} could not be kept; it keeps running there"),
                );
                false
            }
        });
        if landed {
            self.send_now();
        }
    }

    /// A remote tab whose server could not be reached and now can (its link is live, or catching up) wakes the
    /// sender: the kept stops go out with it.
    fn watch_links(&mut self, tabs: &eitri_core::tab_set::TabSet) {
        let mut back = false;
        let mut now = BTreeMap::new();
        for t in tabs.tabs().iter().filter(|t| t.is_remote()) {
            let Some(link) = t.live().and_then(AgentBackend::link_state) else {
                continue;
            };
            let reachable = matches!(
                link,
                agent::RemoteLinkState::Live | agent::RemoteLinkState::CatchingUp { .. }
            );
            back |= reachable && self.reachable.get(&t.id) == Some(&false);
            now.insert(t.id, reachable);
        }
        self.reachable = now;
        if back {
            self.send_now();
        }
    }

    pub(super) fn send_now(&self) {
        if let Some(sender) = &self.sender {
            sender.wake();
        }
    }
}

/// Writes (`keep` true) or clears a stop under the agent's host and key in the project at `root`. Blocking: a
/// worker's. Another window may hold the record's lock for a moment: a few tries before giving up.
pub(super) fn write_stop(root: &std::path::Path, agent: &agent::HelperHandoff, keep: bool) -> Result<(), String> {
    let store = RemoteProjects::new(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let write = || store.write_stop_for(root, &agent.host, &agent.host_key_sha256, &agent.stop_key(), keep);
    let mut written = write();
    for _ in 0..3 {
        match &written {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
                written = write();
            }
            _ => break,
        }
    }
    written.map_err(|error| error.to_string())
}

/// The window's stop sender over its connection: each kept stop of the project's record goes out through it.
pub(super) fn stop_sender_for(connection: eitri_core::remote_project::RemoteConnection) -> StopSender {
    StopSender::start(move || {
        let store = RemoteProjects::new(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        );
        connection
            .send_queued_stops(&store)
            .map_err(|why| format!("{}: {why}", connection.host()))
    })
}

/// The connect worker of a remote tab: starts its agent on the server through the window's connection, spending the
/// window's account, or continues `resume` there through a new sidecar.
pub(super) fn spawn_remote_connect(
    connection: eitri_core::remote_project::RemoteConnection,
    resume: Option<String>,
    project: ProjectTrust,
    client_tools: Option<agent::ClientToolSet>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let account = agent::account::configured().map(|account| account.name().to_string());
        let result = match resume {
            None => connection.start_tab(account.as_deref(), project, client_tools),
            Some(session) => connection.resume_tab(&session, account.as_deref(), project, client_tools),
        };
        let _ = result_tx.send(result);
    });
    result_rx
}

/// The remote tabs of the saved set this window was given, moved to the window's own keeping
/// ([`AgentPanelState::unclaimed_remote`]): the saved set itself goes once a restore runs.
pub(super) fn claim_saved(state: &mut AgentPanelState) {
    if let Some(source) = state.restore_source.as_mut() {
        state.unclaimed_remote.append(&mut source.remote);
    }
}

/// A reopened remote window takes its saved remote tabs up again once its connection is given: each in a tab of its
/// own ([`TabSet::open_reattaching`]: the saved mode, bypass excepted, and the tab on screen stays the one that
/// was), its history folded afresh from the server's journal and nothing resumed (`RemoteConnection::reattach_tab`,
/// on a worker). No trust question is asked: no session starts and nothing new loads. A tab whose agent is open here
/// already is left alone, and nothing comes back when the window keeps no tabs (`agent.restore` off, where closing
/// the window stops its remote agents). The tab keeps the saved entry until its reattach reaches the agent, so a
/// write meanwhile, a failed reattach or the window's close keeps the agent in the file.
pub(super) fn reattach_saved(state: &mut AgentPanelState, out: &mut Outbox) -> usize {
    if state.restore_policy == RestorePolicy::Off {
        return 0;
    }
    let Some(host_key) = reattach_host_key(state) else {
        return 0;
    };
    claim_saved(state);
    let saved = std::mem::take(&mut state.unclaimed_remote);
    let open: Vec<agent::HelperHandoff> = state
        .tabs
        .tabs()
        .iter()
        .filter(|t| t.is_remote())
        .filter_map(|t| {
            t.live()
                .and_then(|backend| backend.helper_handoff())
                .or_else(|| t.remote_agent.as_ref().map(|(helper, _)| helper.clone()))
        })
        .collect();
    let mut started = 0;
    for tab in saved {
        let here = open.iter().any(|h| {
            h.daemon == tab.helper.daemon
                && h.sidecar_id == tab.helper.sidecar_id
                && h.session_id == tab.helper.session_id
        });
        if here {
            continue;
        }
        // Another server's tab is not this window's to take up: it stays in the file.
        if tab.helper.host_key_sha256 != host_key {
            state.unclaimed_remote.push(tab);
            continue;
        }
        let Some(result_rx) = reattach_connect(state, tab.helper.clone()) else {
            state.unclaimed_remote.push(tab);
            continue;
        };
        let id = state.tabs.open_reattaching(&tab);
        state.tabs.start(
            id,
            PendingStart {
                request_id: format!("reattach-{}", tab.helper.sidecar_id),
                result_rx,
                first_turn: None,
                resume: None,
                resumed_title: None,
                resumed_name: tab.name.clone(),
            },
        );
        started += 1;
    }
    if started > 0 {
        switch_payloads(state, out);
    }
    started
}

/// The host key whose saved tabs this window takes up: its connection's (a test's stand-in spawner takes the
/// window's own origin's). `None` while the window has no connection.
fn reattach_host_key(state: &AgentPanelState) -> Option<String> {
    if let Some(connection) = &state.server {
        return Some(connection.project().host_key_sha256.clone());
    }
    match (&state.spawner, state.tabs.project_origin()) {
        #[cfg(test)]
        (Spawner::Fake(_), eitri_core::remote_project::TabOrigin::Remote { host_key_sha256, .. }) => {
            Some(host_key_sha256.clone())
        }
        _ => None,
    }
}

/// The worker that takes the saved agent `helper` names up again through the window's connection
/// (`RemoteConnection::reattach_tab`); nothing else starts here.
fn reattach_connect(
    state: &AgentPanelState,
    helper: agent::HelperHandoff,
) -> Option<mpsc::Receiver<Result<AgentBackend, BackendError>>> {
    match &state.spawner {
        Spawner::Real => {
            let connection = state.server.clone()?;
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(connection.reattach_tab(&helper));
            });
            Some(rx)
        }
        #[cfg(test)]
        Spawner::Fake(fake) => Some((fake.borrow_mut())(SpawnCall::Reattach { helper })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{answer_prompt, ask_about_endings};
    use crate::agent_panel::tests::*;
    use eitri_core::agent_backend::AgentId;
    use eitri_core::agent_bridge::SessionModeChoice;
    use eitri_core::remote_project::{RemoteProject, RemoteProjects, TabOrigin};
    use eitri_core::tab_set::TabSet;
    use eitri_core::test_providers::RecordingProvider;

    fn origin() -> TabOrigin {
        origin_at("/srv/project")
    }

    /// A remote origin whose project is at `root` on the server: tests that keep stops in the project's record each
    /// have their own, as they run side by side over one state directory.
    fn origin_at(root: &str) -> TabOrigin {
        TabOrigin::Remote {
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: root.into(),
        }
    }

    fn handoff() -> agent::HelperHandoff {
        agent::HelperHandoff {
            daemon: "older-installation".into(),
            declared_client_tools: Default::default(),
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            sidecar_id: "sc-1".into(),
            session_id: "session-1".into(),
            last_folded_sequence: 4,
        }
    }

    fn saved_remote() -> SavedTabs {
        SavedTabs::default().with_remote(vec![eitri_core::saved_tabs::SavedRemoteTab {
            helper: handoff(),
            provider_session_id: Some("claude-1".into()),
            name: Some("saved agent".into()),
            mode: SessionModeChoice::Auto,
            window: None,
        }])
    }

    #[test]
    fn saved_tabs_arriving_after_the_connection_are_reattached() {
        for given in [false, true] {
            for ready in [false, true] {
                let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
                state.borrow_mut().tabs.set_project_origin(origin()).unwrap();
                state.borrow_mut().page_mut(ViewId::BASE).document_ready = ready;
                let calls = Rc::new(RefCell::new(Vec::new()));
                let into = calls.clone();
                state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call| {
                    let SpawnCall::Reattach { helper } = call else {
                        panic!("reattach only")
                    };
                    into.borrow_mut().push(helper);
                    mpsc::channel().1
                })));
                let screen = Screen::nothing_shown();
                let mut out = Outbox::default();
                assert_eq!(
                    reattach_saved(&mut state.borrow_mut(), &mut out),
                    0,
                    "connection arrives first"
                );
                if given {
                    super::super::restore::take_given_tabs(&mut state.borrow_mut(), &screen, saved_remote(), &mut out);
                } else {
                    state.borrow_mut().restore_source = Some(saved_remote());
                }
                super::super::restore::restore_once_released(&mut state.borrow_mut(), &screen, &mut out);
                assert_eq!(calls.borrow().len(), 1, "given: {given}, ready: {ready}");
                assert_eq!(calls.borrow()[0], handoff());
                super::super::restore::restore_once_released(&mut state.borrow_mut(), &screen, &mut out);
                assert_eq!(
                    calls.borrow().len(),
                    1,
                    "a later tick never starts the same reattach twice"
                );
            }
        }
    }

    #[test]
    fn an_offline_groups_final_report_keeps_unclaimed_remote_agents() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        state.borrow_mut().tabs.set_project_origin(origin()).unwrap();
        state.borrow_mut().restore_source = Some(saved_remote());
        let reports = Rc::new(RefCell::new(Vec::new()));
        let into = reports.clone();
        state
            .borrow_mut()
            .group_tabs
            .report_to(Rc::new(move |tabs| into.borrow_mut().push(tabs.clone())));
        super::super::restore::remember_tabs(&mut state.borrow_mut());
        assert_eq!(reports.borrow().last().unwrap().remote, saved_remote().remote);
        assert!(state.borrow().restore_source.as_ref().unwrap().remote.is_empty());
        let closing = super::super::restore::tabs_to_keep(&state.borrow()).unwrap();
        assert_eq!(closing.remote, reports.borrow().last().unwrap().remote);
        assert_eq!(
            closing.remote,
            saved_remote().remote,
            "group_closing must not replace the last report with no agents"
        );
    }

    /// A remote window with one live tab whose provider stands for an agent on a server, its project at
    /// `/srv/<label>`.
    fn remote_panel(label: &str) -> (Rc<RefCell<AgentPanelState>>, TabId, std::sync::Arc<RecordingProvider>) {
        remote_panel_with(label, RecordingProvider::default())
    }

    /// [`remote_panel`] with `provider` standing for the agent.
    fn remote_panel_with(
        label: &str,
        provider: RecordingProvider,
    ) -> (Rc<RefCell<AgentPanelState>>, TabId, std::sync::Arc<RecordingProvider>) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let tab = {
            let mut s = state.borrow_mut();
            s.tabs.set_project_origin(origin_at(&format!("/srv/{label}"))).unwrap();
            s.tabs.active()
        };
        let provider = std::sync::Arc::new(provider);
        let conversation = agent::AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        provider.set_link(Some(agent::RemoteLinkState::Live));
        provider.set_remote(None, Some(handoff()));
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        (state, tab, provider)
    }

    /// The remote question an outbox holds, as the page reads it.
    fn prompt_in(out: &Outbox) -> Option<serde_json::Value> {
        out.payloads()
            .iter()
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            .find(|value| value["kind"] == "remote_prompt")
    }

    fn keys(prompt: &serde_json::Value) -> Vec<String> {
        prompt["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["key"].as_str().unwrap().to_string())
            .collect()
    }

    /// The session's loss, queued and folded, as the provider queues it once it has said how the session ended.
    fn end_session(state: &Rc<RefCell<AgentPanelState>>, tab: TabId, provider: &RecordingProvider) {
        provider.queue(agent::AgentDomainEvent::SessionUnavailable {
            reason: "the session is gone".into(),
        });
        wait_until("the tab ended", || {
            state.borrow_mut().tabs.pump(std::path::Path::new("/srv/project"), true);
            matches!(
                state
                    .borrow()
                    .tabs
                    .get(tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .status,
                agent::ProjectionStatus::Unavailable { .. }
            )
        });
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let started = std::time::Instant::now();
        while !done() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "{what} never happened"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Closing a remote tab whose server answers ends its agent there; nothing is asked.
    #[test]
    fn closing_a_remote_tab_stops_its_sidecar() {
        let (state, tab, provider) = remote_panel("remote-close-connected");
        state.borrow_mut().tabs.open(AgentId::Claude);
        let mut out = Outbox::default();
        close_tab_asked(&mut state.borrow_mut(), tab, &mut out).unwrap();
        assert!(prompt_in(&out).is_none(), "nothing asked while the server answers");
        assert!(state.borrow().tabs.get(tab).is_none(), "the tab is closed");
        wait_until("the agent stopped", || provider.remote_stops() == 1);
    }

    /// Closing a remote tab while its server cannot be reached asks first; "stop it when the server is back" keeps
    /// the stop in the project's record and closes the tab without ending anything now, and the stop goes out once
    /// a connection is given.
    #[test]
    fn closing_while_disconnected_asks_and_queues_the_stop() {
        let (state, tab, provider) = remote_panel("remote-close-disconnected");
        let store = RemoteProjects::new(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        );
        store
            .save(&RemoteProject {
                version: 1,
                host: "example-host".into(),
                host_key_sha256: "SHA256:example".into(),
                root: "/srv/remote-close-disconnected".into(),
                helper_version: "0.3.0".into(),
                sync: None,
                created_at: 1,
                queued_stops: Vec::new(),
            })
            .unwrap();
        state.borrow_mut().tabs.open(AgentId::Claude);
        provider.set_link(Some(agent::RemoteLinkState::Reconnecting {
            attempt: 3,
            next_in_ms: 4000,
        }));
        let mut out = Outbox::default();
        close_tab_asked(&mut state.borrow_mut(), tab, &mut out).unwrap();
        let prompt = prompt_in(&out).expect("the close asks first");
        assert_eq!(
            keys(&prompt),
            ["s", "L"],
            "no key the page moves about with leaves the agent running"
        );
        assert!(prompt["choices"][0]["label"].as_str().unwrap().contains("example-host"));
        assert!(
            state.borrow().tabs.get(tab).is_some(),
            "nothing closed before the answer"
        );
        let nonce = prompt["nonce"].as_u64().unwrap();

        let mut out = Outbox::default();
        assert!(answer_prompt(
            &mut state.borrow_mut(),
            tab,
            nonce + 1,
            RemotePromptAction::StopWhenBack,
            "r1",
            ViewId::BASE,
            &mut out
        )
        .is_err());
        assert!(
            answer_prompt(
                &mut state.borrow_mut(),
                tab,
                nonce,
                RemotePromptAction::DenyAndStop,
                "r1",
                ViewId::BASE,
                &mut out
            )
            .is_err(),
            "a choice the question did not offer is refused"
        );
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            nonce,
            RemotePromptAction::StopWhenBack,
            "r1",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        assert!(state.borrow().tabs.get(tab).is_none(), "the tab is closed");
        wait_until("the stop kept", || {
            store
                .find(std::path::Path::new("/srv/remote-close-disconnected"))
                .unwrap()
                .is_some_and(|record| record.queued_stops == ["older-installation/sc-1"])
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            provider.remote_stops(),
            0,
            "nothing is stopped while the server cannot be reached"
        );
    }

    /// A close question is about the agent the tab held when it was asked: once the tab holds another (started over,
    /// or a new session), the question goes and its answer is refused, so it never stops or lets go of the new one.
    #[test]
    fn a_close_question_is_dropped_when_the_tab_holds_another_agent() {
        let (state, tab, provider) = remote_panel("remote-close-rebound");
        provider.set_link(Some(agent::RemoteLinkState::Reconnecting {
            attempt: 1,
            next_in_ms: 1000,
        }));
        let mut out = Outbox::default();
        assert!(ask_before_closing(&mut state.borrow_mut(), tab, &mut out));
        let nonce = prompt_in(&out).expect("the close asks first")["nonce"]
            .as_u64()
            .unwrap();
        let mut newer = handoff();
        newer.sidecar_id = "sc-2".into();
        newer.session_id = "session-2".into();
        provider.set_remote(None, Some(newer));
        let mut out = Outbox::default();
        tick(&mut state.borrow_mut(), &mut out);
        assert!(
            out.payloads().iter().any(|p| p.contains("remote_prompt_clear")),
            "the page drops the question"
        );
        let mut out = Outbox::default();
        assert!(answer_prompt(
            &mut state.borrow_mut(),
            tab,
            nonce,
            RemotePromptAction::StopWhenBack,
            "r1",
            ViewId::BASE,
            &mut out
        )
        .is_err());
        assert!(state.borrow().tabs.get(tab).is_some(), "nothing closed");
    }

    /// After a gap the tab lists the requests waiting on the server; "deny them and stop the agent" answers each
    /// with the ordinary deny and then stops it.
    #[test]
    fn deny_and_stop_after_a_gap_denies_each_and_stops() {
        let (state, tab, provider) = remote_panel("remote-gap-deny");
        provider.set_remote(
            Some(agent::RemoteEnding::Gap {
                waiting: vec![agent::WaitingRequest {
                    permission_id: "p1".into(),
                    tool_name: "Bash".into(),
                    input_line: "rm -r build".into(),
                }],
            }),
            Some(handoff()),
        );
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        assert!(prompt_in(&out).is_none(), "not asked before the tab shows the end");
        end_session(&state, tab, &provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let prompt = prompt_in(&out).expect("the gap is asked about");
        assert_eq!(keys(&prompt), ["s", "L"], "no key the page moves about with answers it");
        let lines: Vec<&str> = prompt["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect();
        assert_eq!(lines[1], "Bash: rm -r build");
        let mut again = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut again);
        assert!(prompt_in(&again).is_none(), "asked once");

        let mut out = Outbox::default();
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            prompt["nonce"].as_u64().unwrap(),
            RemotePromptAction::DenyAndStop,
            "r1",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        wait_until("the agent stopped", || provider.remote_stops() == 1);
        assert_eq!(
            provider.decisions(),
            [("p1".to_string(), agent::PermissionDecision::Deny { reason: None })]
        );
        assert_eq!(provider.answers_at_stops(), [1], "the deny went before the stop");
        wait_until("the stopped agent left out of the saved tabs", || {
            let mut out = Outbox::default();
            tick(&mut state.borrow_mut(), &mut out);
            state.borrow().tabs.saved_snapshot("c").unwrap().remote.is_empty()
        });
    }

    /// When the server no longer runs the tab's agent, the tab offers to continue the CLI's session; the choice
    /// starts it on the server through the window's connection with the ordinary resume, after the trust question.
    #[test]
    fn a_lost_agent_offers_resume_through_a_new_sidecar() {
        let (state, tab, provider) = remote_panel("remote-gone-resume");
        provider.queue(agent::AgentDomainEvent::SessionOpened {
            session_id: "session-1".into(),
            provider_session_id: "claude-1".into(),
            model: "stand-in".into(),
            cwd: "/srv/project".into(),
        });
        wait_until("the CLI's session known", || {
            state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .provider_session_id()
                .is_some()
        });
        // As the provider does: the ending is set, then the session's loss is queued.
        provider.set_remote(Some(agent::RemoteEnding::AgentGone), Some(handoff()));
        end_session(&state, tab, &provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let prompt = prompt_in(&out).expect("the lost agent is asked about");
        assert_eq!(keys(&prompt), ["R"], "not the `r` that restarts an ended session");
        let mut out = Outbox::default();
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            prompt["nonce"].as_u64().unwrap(),
            RemotePromptAction::Resume,
            "r1",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        let start = state
            .borrow_mut()
            .tabs
            .take_deferred(tab)
            .expect("the resume waits for the trust question");
        let resumed: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        let into = resumed.clone();
        state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
            match call {
                SpawnCall::Remote {
                    resume, client_tools, ..
                } => {
                    let _ = client_tools;
                    into.borrow_mut().push(resume)
                }
                _ => panic!("a remote tab starts through the window's connection"),
            }
            mpsc::channel().1
        })));
        let _ = start_deferred(&mut state.borrow_mut(), tab, start, ProjectTrust::Untrusted);
        assert_eq!(*resumed.borrow(), [Some("claude-1".to_string())]);
    }

    fn store() -> RemoteProjects {
        RemoteProjects::new(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// The project's record, with no stop kept yet.
    fn record_the_project(root: &str) {
        store()
            .save(&RemoteProject {
                version: 1,
                host: "example-host".into(),
                host_key_sha256: "SHA256:example".into(),
                root: root.into(),
                helper_version: "0.3.0".into(),
                sync: None,
                created_at: 1,
                queued_stops: Vec::new(),
            })
            .unwrap();
    }

    fn kept_stops(root: &str) -> Vec<String> {
        store()
            .find(std::path::Path::new(root))
            .unwrap()
            .map(|record| record.queued_stops)
            .unwrap_or_default()
    }

    /// Runs the remote tabs' tick until `done`.
    fn tick_until(state: &Rc<RefCell<AgentPanelState>>, what: &str, mut done: impl FnMut(&Outbox) -> bool) -> Outbox {
        let mut out = Outbox::default();
        wait_until(what, || {
            {
                let mut s = state.borrow_mut();
                for tab in s.retiring.poll() {
                    s.tabs.retire_done(tab);
                }
                tick(&mut s, &mut out);
            }
            done(&out)
        });
        out
    }

    fn notices(out: &Outbox) -> Vec<String> {
        out.payloads()
            .iter()
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            .filter(|value| value["kind"] == "notice")
            .filter_map(|value| value["text"].as_str().map(str::to_string))
            .collect()
    }

    /// A remote tab mid-turn whose server cannot be reached.
    fn running_and_unreachable(state: &Rc<RefCell<AgentPanelState>>, tab: TabId, provider: &RecordingProvider) {
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        wait_until("the turn running", || {
            let mut s = state.borrow_mut();
            s.tabs.pump(std::path::Path::new("/srv/project"), true);
            s.tabs.get(tab).unwrap().turn_running()
        });
        provider.set_link(Some(agent::RemoteLinkState::Disconnected { since_unix_ms: 1 }));
    }

    /// "Leave it running" closes the tab and touches nothing on the server: no stop, no interrupt of the running
    /// turn, nothing kept for later. Closing it asks nothing on the GTK thread that crosses the network.
    #[test]
    fn leaving_it_running_closes_without_stopping_or_interrupting() {
        let (state, tab, provider) = remote_panel_with("remote-leave-running", RecordingProvider::interruptible());
        state.borrow_mut().tabs.open(AgentId::Claude);
        running_and_unreachable(&state, tab, &provider);
        let mut out = Outbox::default();
        close_tab_asked(&mut state.borrow_mut(), tab, &mut out).unwrap();
        let prompt = prompt_in(&out).expect("the close asks first");
        assert!(prompt["lines"][0].as_str().unwrap().starts_with("tab 1:"), "{prompt}");
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            prompt["nonce"].as_u64().unwrap(),
            RemotePromptAction::LeaveRunning,
            "r1",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        assert!(state.borrow().tabs.get(tab).is_none(), "the tab is closed");
        tick_until(&state, "the teardown done", |_| state.borrow().retiring.is_empty());
        assert_eq!(provider.remote_stops(), 0);
        assert_eq!(provider.interrupts(), 0, "the agent's turn goes on");
        let mut out = Outbox::default();
        let other = state.borrow().tabs.active();
        assert!(
            !ask_before_closing(&mut state.borrow_mut(), other, &mut out),
            "a later close of another tab is not let go by the earlier answer"
        );
    }

    /// While the server cannot be reached a replay is not under way, but once the link is catching up the server
    /// is reached: a close then stops the agent at once, asking nothing.
    #[test]
    fn a_link_catching_up_is_reachable() {
        let (state, tab, provider) = remote_panel("remote-close-catching-up");
        state.borrow_mut().tabs.open(AgentId::Claude);
        provider.set_link(Some(agent::RemoteLinkState::CatchingUp { done: 3, total: 40 }));
        let mut out = Outbox::default();
        close_tab_asked(&mut state.borrow_mut(), tab, &mut out).unwrap();
        assert!(prompt_in(&out).is_none(), "nothing asked");
        wait_until("the agent stopped", || provider.remote_stops() == 1);
    }

    /// A stop kept while the server could not be reached goes out once a remote tab's link is back, without a new
    /// connection: the window's stop sender is woken and sends it.
    #[test]
    fn a_kept_stop_goes_out_when_the_link_is_back() {
        let (state, tab, provider) = remote_panel("remote-kept-stop-sent");
        let root = "/srv/remote-kept-stop-sent";
        record_the_project(root);
        let reachable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sender = {
            let (reachable, runs) = (reachable.clone(), runs.clone());
            // No retry within the test: only a wake sends.
            StopSender::start_retrying_after(
                move || {
                    runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if !reachable.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err("example-host cannot be reached".into());
                    }
                    let root = std::path::Path::new("/srv/remote-kept-stop-sent");
                    let queued = store().find(root).unwrap().unwrap().queued_stops;
                    for id in &queued {
                        store().stop_sent(root, id).unwrap();
                    }
                    Ok(queued)
                },
                std::time::Duration::from_secs(3600),
            )
        };
        state.borrow_mut().remote_stops.set_sender(sender);
        let (other_provider, other_backend) = live_backend(&agent::state_dirs::test_workspace_dir("remote-kept-other"));
        other_provider.set_link(Some(agent::RemoteLinkState::Reconnecting {
            attempt: 1,
            next_in_ms: 500,
        }));
        {
            let mut s = state.borrow_mut();
            let other = s.tabs.open(AgentId::Claude);
            s.tabs
                .get_mut(other)
                .unwrap()
                .set_backend(TabBackend::Live(other_backend));
        }
        provider.set_link(Some(agent::RemoteLinkState::Reconnecting {
            attempt: 1,
            next_in_ms: 500,
        }));
        let mut out = Outbox::default();
        close_tab_asked(&mut state.borrow_mut(), tab, &mut out).unwrap();
        let nonce = prompt_in(&out).unwrap()["nonce"].as_u64().unwrap();
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            nonce,
            RemotePromptAction::StopWhenBack,
            "r1",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        tick_until(&state, "the stop kept and tried", |_| {
            kept_stops(root) == ["older-installation/sc-1"] && runs.load(std::sync::atomic::Ordering::SeqCst) >= 2
        });
        assert_eq!(
            provider.remote_stops(),
            0,
            "nothing stopped while the server cannot be reached"
        );
        reachable.store(true, std::sync::atomic::Ordering::SeqCst);
        other_provider.set_link(Some(agent::RemoteLinkState::Live));
        tick_until(&state, "the kept stop sent", |_| kept_stops(root).is_empty());
    }

    /// A close whose stop does not go through (the server stopped answering after the person's close) keeps the
    /// stop for when the server is back and says so on the window.
    #[test]
    fn a_stop_that_fails_on_close_is_kept_and_said() {
        let (state, tab, provider) = remote_panel("remote-stop-fails");
        let root = "/srv/remote-stop-fails";
        record_the_project(root);
        state.borrow_mut().tabs.open(AgentId::Claude);
        provider.fail_remote_stops(true);
        let mut out = Outbox::default();
        close_tab_asked(&mut state.borrow_mut(), tab, &mut out).unwrap();
        assert!(prompt_in(&out).is_none(), "the link read live: nothing asked");
        // Kept before the stop is tried, and said once it failed.
        let out = tick_until(&state, "the stop kept and the failure said", |out| {
            kept_stops(root) == ["older-installation/sc-1"]
                && notices(out).iter().any(|text| text.contains("was not stopped"))
        });
        let said = notices(&out);
        assert!(
            said.iter()
                .any(|text| text.contains("was not stopped") && text.contains("example-host")),
            "{said:?}"
        );
    }

    /// Closing the other tabs (or killing a view) closes a remote tab whose server cannot be reached the way the
    /// person's "stop it when back" does: its stop is kept, nothing waits on the network, and the window says so.
    #[test]
    fn closing_the_others_keeps_an_unreachable_tabs_stop() {
        let (state, tab, provider) = remote_panel_with("remote-close-others", RecordingProvider::interruptible());
        let root = "/srv/remote-close-others";
        record_the_project(root);
        state.borrow_mut().tabs.open(AgentId::Claude);
        running_and_unreachable(&state, tab, &provider);
        let mut out = Outbox::default();
        close_tab_unasked(&mut state.borrow_mut(), tab, &mut out).unwrap();
        assert!(prompt_in(&out).is_none(), "a close of many asks nothing per tab");
        assert!(state.borrow().tabs.get(tab).is_none());
        assert!(notices(&out)
            .iter()
            .any(|text| text.contains("until example-host is back")));
        tick_until(&state, "the stop kept", |_| {
            kept_stops(root) == ["older-installation/sc-1"]
        });
        tick_until(&state, "the teardown done", |_| state.borrow().retiring.is_empty());
        assert_eq!(provider.remote_stops(), 0);
        assert_eq!(provider.interrupts(), 0);
    }

    fn gap_with(provider: &RecordingProvider, sidecar: &str) {
        let mut agent = handoff();
        agent.sidecar_id = sidecar.into();
        agent.session_id = format!("session-{sidecar}");
        provider.set_remote(
            Some(agent::RemoteEnding::Gap {
                waiting: vec![agent::WaitingRequest {
                    permission_id: "p1".into(),
                    tool_name: "Bash".into(),
                    input_line: "rm -r build".into(),
                }],
            }),
            Some(agent),
        );
    }

    /// "Leave it waiting" changes nothing on the server: the agent keeps running, the tab stays, and the agent stays in
    /// the saved tabs for a later window. A "deny them and stop the agent" whose stop fails says so on the tab and
    /// keeps the agent saved.
    #[test]
    fn leaving_it_waiting_keeps_the_agent_and_a_failed_deny_and_stop_says_so() {
        let (state, tab, provider) = remote_panel("remote-gap-leave-waiting");
        gap_with(&provider, "sc-1");
        end_session(&state, tab, &provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let asked = prompt_in(&out).expect("asked");
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            asked["nonce"].as_u64().unwrap(),
            RemotePromptAction::LeaveWaiting,
            "r1",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        assert!(state.borrow().tabs.get(tab).is_some(), "the tab stays");
        assert_eq!(provider.remote_stops(), 0);
        assert!(provider.decisions().is_empty());
        assert_eq!(state.borrow().tabs.saved_snapshot("c").unwrap().remote.len(), 1);

        // The same agent's end asked again under a new question (as after a resume), now stopped and failing.
        gap_with(&provider, "sc-2");
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let asked = prompt_in(&out).expect("asked about the newer agent");
        provider.fail_remote_stops(true);
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            asked["nonce"].as_u64().unwrap(),
            RemotePromptAction::DenyAndStop,
            "r2",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        let out = tick_until(&state, "the failure said", |out| {
            notices(out)
                .iter()
                .any(|text| text.contains("the agent was not stopped"))
        });
        assert!(!notices(&out).is_empty());
        assert_eq!(
            state.borrow().tabs.saved_snapshot("c").unwrap().remote.len(),
            1,
            "an agent that was not stopped stays saved"
        );
    }

    /// Endings are asked about per agent: once the tab holds another agent (a resume, a fresh start) its end is
    /// asked about again, and an answer to the old agent's question acts on nothing.
    #[test]
    fn an_ending_is_asked_about_per_agent_and_a_stale_answer_does_nothing() {
        let (state, tab, provider) = remote_panel("remote-ending-per-agent");
        gap_with(&provider, "sc-1");
        end_session(&state, tab, &provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let first = prompt_in(&out).expect("asked");
        assert!(first["lines"][0].as_str().unwrap().starts_with("tab 1:"), "{first}");
        // The tab now holds another agent; the old question is answered.
        gap_with(&provider, "sc-2");
        let mut out = Outbox::default();
        let stale = answer_prompt(
            &mut state.borrow_mut(),
            tab,
            first["nonce"].as_u64().unwrap(),
            RemotePromptAction::DenyAndStop,
            "r1",
            ViewId::BASE,
            &mut out,
        );
        assert!(stale.unwrap_err().contains("no longer holds"));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(provider.remote_stops(), 0, "the newer agent is not stopped");
        assert!(provider.decisions().is_empty());
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        assert!(prompt_in(&out).is_some(), "the newer agent's end is asked about");
    }

    /// A reopened remote window takes up the saved tabs of its own server, each once, in its saved mode (bypass as
    /// manual), the tab on screen staying the one that was; another server's stay for a window of that server, and
    /// a window that keeps no tabs takes none up.
    #[test]
    fn a_reopened_window_takes_up_its_own_servers_saved_tabs() {
        let (state, _tab, _provider) = remote_panel("remote-reattach-saved");
        let taken: Rc<RefCell<Vec<String>>> = Rc::default();
        let into = taken.clone();
        state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
            match call {
                SpawnCall::Reattach { helper } => into.borrow_mut().push(helper.sidecar_id),
                _ => panic!("a saved remote tab is only taken up"),
            }
            mpsc::channel().1
        })));
        let saved = |sidecar: &str, key: &str, mode: SessionModeChoice| {
            let mut helper = handoff();
            helper.sidecar_id = sidecar.into();
            helper.session_id = format!("session-{sidecar}");
            helper.host_key_sha256 = key.into();
            eitri_core::saved_tabs::SavedRemoteTab {
                window: None,
                helper,
                provider_session_id: None,
                name: None,
                mode,
            }
        };
        let active = state.borrow().tabs.active();
        state.borrow_mut().unclaimed_remote = vec![
            saved("sc-1", "SHA256:example", SessionModeChoice::Manual),
            saved("sc-9", "SHA256:other", SessionModeChoice::Auto),
            saved("sc-2", "SHA256:example", SessionModeChoice::Bypass),
            // The agent the window's live tab already holds.
            {
                let mut open = saved("sc-1", "SHA256:example", SessionModeChoice::Auto);
                open.helper.session_id = "session-1".into();
                open
            },
        ];
        let mut out = Outbox::default();
        assert_eq!(reattach_saved(&mut state.borrow_mut(), &mut out), 2);
        assert_eq!(*taken.borrow(), ["sc-1", "sc-2"]);
        let s = state.borrow();
        assert_eq!(s.tabs.active(), active, "the tab on screen stays");
        let modes: Vec<SessionModeChoice> = s
            .tabs
            .tabs()
            .iter()
            .filter(|t| t.remote_agent.is_some())
            .map(|t| t.mode())
            .collect();
        assert_eq!(modes, [SessionModeChoice::Manual, SessionModeChoice::Manual]);
        assert_eq!(s.unclaimed_remote.len(), 1);
        assert_eq!(s.unclaimed_remote[0].helper.sidecar_id, "sc-9");
        assert_eq!(
            s.tabs.saved_snapshot("c").unwrap().remote.len(),
            3,
            "the live tab and both being taken up"
        );
        drop(s);
        state.borrow_mut().restore_policy = RestorePolicy::Off;
        state.borrow_mut().unclaimed_remote = vec![saved("sc-3", "SHA256:example", SessionModeChoice::Auto)];
        assert_eq!(reattach_saved(&mut state.borrow_mut(), &mut out), 0);
    }

    /// A window closing lets its remote agents go when it keeps its tabs (they are in the saved file for a later
    /// window), and stops them when it keeps none: nothing would ever reach them again.
    #[test]
    fn a_window_that_keeps_no_tabs_stops_its_remote_agents_on_close() {
        agent::state_dirs::redirect_state_to_a_test_root();
        for keeps in [true, false] {
            let (handle, _page) = settled_panel();
            let dir = agent::state_dirs::test_workspace_dir("remote-window-close");
            let (provider, backend) = live_backend(&dir);
            provider.set_link(Some(agent::RemoteLinkState::Live));
            provider.set_remote(None, Some(handoff()));
            {
                let mut s = handle.state.borrow_mut();
                s.tabs.set_project_origin(origin()).unwrap();
                let tab = s.tabs.active();
                s.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
            }
            if !keeps {
                handle.set_restore_policy(RestorePolicy::Off);
            }
            let mut watch = handle.shutdown();
            let started = std::time::Instant::now();
            while !watch.is_empty() {
                assert!(started.elapsed() < std::time::Duration::from_secs(30));
                watch.step(std::time::Instant::now());
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(provider.remote_stops(), usize::from(!keeps), "keeps tabs: {keeps}");
        }
    }

    /// A saved remote tab, taken up in a tab of its own in `state`'s window, standing as `backend` (starting or
    /// failed). Its agent is `sc-7`.
    fn unreached_tab(state: &Rc<RefCell<AgentPanelState>>, backend: TabBackend) -> TabId {
        let mut helper = handoff();
        helper.sidecar_id = "sc-7".into();
        helper.session_id = "session-7".into();
        let saved = eitri_core::saved_tabs::SavedRemoteTab {
            window: None,
            helper,
            provider_session_id: Some("claude-7".into()),
            name: None,
            mode: SessionModeChoice::Auto,
        };
        let mut s = state.borrow_mut();
        let tab = s.tabs.open_reattaching(&saved);
        s.tabs.get_mut(tab).unwrap().set_backend(backend);
        tab
    }

    fn failed() -> TabBackend {
        TabBackend::Failed {
            reason: "the master dropped".into(),
        }
    }

    /// A saved remote agent the tab never reached keeps its stop when the tab is closed, started over, or closed
    /// while its reattach still connects: nothing else would ever end it, and the file no longer names it.
    #[test]
    fn a_tab_that_never_reached_its_saved_agent_keeps_its_stop_when_it_goes() {
        for (label, how) in [
            ("remote-unreached-closed", "close"),
            ("remote-unreached-reset", "reset"),
            ("remote-unreached-starting", "starting"),
        ] {
            let (state, _tab, _provider) = remote_panel(label);
            let root = format!("/srv/{label}");
            record_the_project(&root);
            let tab = match how {
                "starting" => unreached_tab(
                    &state,
                    TabBackend::Starting(PendingStart {
                        request_id: "reattach-sc-7".into(),
                        result_rx: mpsc::channel().1,
                        first_turn: None,
                        resume: None,
                        resumed_title: None,
                        resumed_name: None,
                    }),
                ),
                _ => unreached_tab(&state, failed()),
            };
            let mut out = Outbox::default();
            match how {
                "reset" => {
                    crate::agent_panel::inbound::reset_tab(&mut state.borrow_mut(), tab, ViewId::BASE, "r1", &mut out)
                }
                _ => close_tab_asked(&mut state.borrow_mut(), tab, &mut out).unwrap(),
            }
            tick_until(&state, "the stop kept", |_| {
                kept_stops(&root) == ["older-installation/sc-7"]
            });
        }
    }

    /// With `agent.restore` off, a window closing while its server cannot be reached keeps every remote agent's stop
    /// in the project's record: the stop it tries fails, and nothing else would ever reach the agent.
    #[test]
    fn a_window_closing_while_the_server_is_out_of_reach_keeps_its_agents_stops() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let root = "/srv/remote-close-window-unreachable";
        record_the_project(root);
        let (handle, _page) = settled_panel();
        let dir = agent::state_dirs::test_workspace_dir("remote-close-window-unreachable");
        let (provider, backend) = live_backend(&dir);
        provider.set_link(Some(agent::RemoteLinkState::Reconnecting {
            attempt: 2,
            next_in_ms: 1000,
        }));
        provider.set_remote(None, Some(handoff()));
        provider.fail_remote_stops(true);
        handle.set_project_origin(origin_at(root)).unwrap();
        {
            let mut s = handle.state.borrow_mut();
            let tab = s.tabs.active();
            s.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        }
        handle.set_restore_policy(RestorePolicy::Off);
        let mut watch = handle.shutdown();
        let started = std::time::Instant::now();
        while !watch.is_empty() {
            assert!(started.elapsed() < std::time::Duration::from_secs(30));
            watch.step(std::time::Instant::now());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(provider.remote_stops(), 0, "the stop failed");
        assert_eq!(kept_stops(root), ["older-installation/sc-1"]);
    }

    /// A gap question for `state`'s tab, its sole request `p1`, asked and answered with "deny them and stop the
    /// agent" while the provider holds its answers.
    fn deny_and_stop_held(state: &Rc<RefCell<AgentPanelState>>, tab: TabId, provider: &RecordingProvider) {
        gap_with(provider, "sc-1");
        end_session(state, tab, provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let asked = prompt_in(&out).expect("asked");
        provider.hold_permission_replies();
        answer_prompt(
            &mut state.borrow_mut(),
            tab,
            asked["nonce"].as_u64().unwrap(),
            RemotePromptAction::DenyAndStop,
            "r1",
            ViewId::BASE,
            &mut out,
        )
        .unwrap();
        wait_until("the deny on its way", || provider.held_permissions() == 1);
    }

    /// A deny-and-stop that finishes after the tab was started over and holds a new agent marks nothing: the new
    /// agent stays saved.
    #[test]
    fn a_deny_and_stop_finishing_after_a_new_agent_leaves_it_saved() {
        let (state, tab, provider) = remote_panel("remote-deny-stop-new-agent");
        deny_and_stop_held(&state, tab, &provider);
        let mut out = Outbox::default();
        crate::agent_panel::inbound::reset_tab(&mut state.borrow_mut(), tab, ViewId::BASE, "r2", &mut out);
        let dir = agent::state_dirs::test_workspace_dir("remote-deny-stop-new-agent-2");
        let (newer, backend) = live_backend(&dir);
        newer.set_link(Some(agent::RemoteLinkState::Live));
        let mut agent = handoff();
        agent.sidecar_id = "sc-2".into();
        newer.set_remote(None, Some(agent));
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.reply_to_held_permissions(Ok(None));
        wait_until("the old agent stopped", || provider.remote_stops() >= 1);
        tick_until(&state, "the worker read", |_| {
            state.borrow().remote_prompts.running.is_empty()
        });
        let saved = state.borrow().tabs.saved_snapshot("c").unwrap().remote;
        assert_eq!(saved.len(), 1, "the new agent stays saved");
        assert_eq!(saved[0].helper.sidecar_id, "sc-2");
    }

    /// A stop that went through marks the agent stopped even when a deny before it failed; the failed deny is said.
    #[test]
    fn a_stopped_agent_is_not_saved_again_even_when_a_deny_failed() {
        let (state, tab, provider) = remote_panel("remote-deny-failed-stopped");
        deny_and_stop_held(&state, tab, &provider);
        provider.reply_to_held_permissions(Err(agent::ProviderError::Transport("the link blipped".into())));
        let out = tick_until(&state, "the worker read", |_| {
            state.borrow().remote_prompts.running.is_empty()
        });
        assert_eq!(provider.remote_stops(), 1);
        assert!(
            state.borrow().tabs.saved_snapshot("c").unwrap().remote.is_empty(),
            "a stopped agent is not saved"
        );
        assert!(
            notices(&out)
                .iter()
                .any(|text| text.contains("the deny of p1 did not go through")),
            "{:?}",
            notices(&out)
        );
    }

    /// Starting a tab over takes its open question away, from its page too: it was about the agent that is gone.
    #[test]
    fn starting_a_tab_over_takes_its_question_away() {
        let (state, tab, provider) = remote_panel("remote-reset-question");
        gap_with(&provider, "sc-1");
        end_session(&state, tab, &provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let nonce = prompt_in(&out).expect("asked")["nonce"].as_u64().unwrap();
        crate::agent_panel::inbound::reset_tab(&mut state.borrow_mut(), tab, ViewId::BASE, "r1", &mut out);
        let out = tick_until(&state, "the question taken away", |out| {
            out.payloads().iter().any(|p| {
                serde_json::from_str::<serde_json::Value>(p)
                    .is_ok_and(|v| v["kind"] == "remote_prompt_clear" && v["nonce"] == nonce)
            })
        });
        assert!(prompt_in(&out).is_none());
        assert!(state.borrow().remote_prompts.open.is_empty());
    }

    /// A page that comes up again (a reload) is sent the open question of the tab it shows, as it was asked.
    #[test]
    fn a_reloaded_page_is_sent_its_open_question_again() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let (handle, page) = settled_panel();
        let dir = agent::state_dirs::test_workspace_dir("remote-reload-question");
        let (provider, backend) = live_backend(&dir);
        provider.set_link(Some(agent::RemoteLinkState::Live));
        handle.set_project_origin(origin()).unwrap();
        let tab = {
            let mut s = handle.state.borrow_mut();
            let tab = s.tabs.active();
            s.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
            tab
        };
        gap_with(&provider, "sc-1");
        end_session(&handle.state, tab, &provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut handle.state.borrow_mut(), &mut out);
        let asked = prompt_in(&out).expect("asked");
        page.sent.borrow_mut().clear();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        let _ = handle.run_tick(std::time::Instant::now());
        let again: Vec<serde_json::Value> = page
            .sent
            .borrow()
            .iter()
            .filter_map(|p| serde_json::from_str::<serde_json::Value>(p).ok())
            .filter(|v| v["kind"] == "remote_prompt")
            .collect();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0]["nonce"], asked["nonce"]);
    }
    #[test]
    fn changing_a_windows_key_rekeys_loaded_and_unclaimed_tabs_before_reattaching() {
        use eitri_core::saved_tabs::{self, SavedRemoteTab, SavedTabs, TabWriter};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("remote-loaded-rekey");
        let state_home = dir.join("state");
        let old_origin = origin();
        let mut new_origin = old_origin.clone();
        if let TabOrigin::Remote { host_key_sha256, .. } = &mut new_origin {
            *host_key_sha256 = "SHA256:new".into();
        }
        let old_dir = saved_tabs::state_dir_for(Some(state_home.as_os_str()), None, &old_origin).unwrap();
        let new_dir = saved_tabs::state_dir_for(Some(state_home.as_os_str()), None, &new_origin).unwrap();
        let saved = |id: &str, key: &str| {
            let mut helper = handoff();
            helper.sidecar_id = id.into();
            helper.host_key_sha256 = key.into();
            SavedRemoteTab {
                helper,
                provider_session_id: None,
                name: None,
                mode: SessionModeChoice::Auto,
                window: None,
            }
        };
        let source = SavedTabs::default().with_remote(vec![saved("sc-7", "SHA256:example")]);
        saved_tabs::save(&old_dir, std::path::Path::new("/srv/project"), &source).unwrap();
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let handle = handle_for(&state);
        {
            let mut s = state.borrow_mut();
            s.project_dir = "/srv/project".into();
            s.review_roots.state_home = Some(state_home.into_os_string());
            s.review_roots.home = None;
            s.tab_memory = TabWriter::new(Some(old_dir.clone()), s.project_dir.clone());
        }
        handle.set_project_origin(old_origin).unwrap();
        let saved_tabs::Loaded::Saved(loaded) = saved_tabs::load(&old_dir, std::path::Path::new("/srv/project")) else {
            panic!("saved file")
        };
        {
            let mut s = state.borrow_mut();
            s.restore_source = Some(loaded.clone());
            s.unclaimed_remote = vec![saved("sc-8", "SHA256:example"), saved("sc-9", "SHA256:other")];
            s.tab_memory.observe(Some(loaded));
        }
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(60)));
        handle.set_project_origin(new_origin.clone()).unwrap();
        assert_eq!(state.borrow().tabs.project_origin(), &new_origin);
        let taken: Rc<RefCell<Vec<agent::HelperHandoff>>> = Rc::default();
        let into = taken.clone();
        state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call| {
            let SpawnCall::Reattach { helper } = call else {
                panic!("reattach only")
            };
            into.borrow_mut().push(helper);
            mpsc::channel().1
        })));
        let mut out = Outbox::default();
        assert_eq!(reattach_saved(&mut state.borrow_mut(), &mut out), 2);
        assert!(taken
            .borrow()
            .iter()
            .all(|helper| helper.host_key_sha256 == "SHA256:new"));
        assert_eq!(
            state.borrow().unclaimed_remote[0].helper.host_key_sha256,
            "SHA256:other"
        );
        super::super::restore::remember_tabs(&mut state.borrow_mut());
        state
            .borrow_mut()
            .tab_memory
            .finish()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap();
        let saved_tabs::Loaded::Saved(saved) = saved_tabs::load(&new_dir, std::path::Path::new("/srv/project")) else {
            panic!("moved file")
        };
        assert_eq!(saved.remote.len(), 3);
        assert!(saved
            .remote
            .iter()
            .filter(|t| t.helper.sidecar_id != "sc-9")
            .all(|t| t.helper.host_key_sha256 == "SHA256:new"));
    }
}
