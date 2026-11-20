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
    /// Resumes the person chose on a remote tab, by tab: after the server lost the tab's agent (a rebuild among
    /// them), or a session picked in the chooser. Each is the CLI's session that may be continued through a new
    /// sidecar, and nothing else.
    resumes: BTreeMap<TabId, String>,
    /// Tabs whose resume rebuilds the history after a gap: the tab says so once the session is back, if its history
    /// was read from the CLI's transcript.
    rebuilds: BTreeSet<TabId>,
    /// The config directory the tab's own agent reported, kept from before its reset for the resume that follows: the
    /// transcripts a resume reads are matched against it, and the reset takes the handshake it came from.
    config_dirs: BTreeMap<TabId, String>,
    /// The sidecar of the agent a tab's reset stopped, kept for the resume that follows: the resume goes on only once
    /// the server no longer runs it, so two CLIs never continue one session.
    old_sidecars: BTreeMap<TabId, String>,
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
    /// The server no longer runs the tab's agent, or no longer holds the history of its session; the CLI's session
    /// can be continued, its history read back from the server's transcript. `rebuild`: the second of those.
    Gone { cli_session: String, rebuild: bool },
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
        self.forget_resume(tab);
    }

    /// What was kept for a resume in `tab`: the choice itself, whether it rebuilds, the config directory carried.
    fn forget_resume(&mut self, tab: TabId) {
        self.resumes.remove(&tab);
        self.rebuilds.remove(&tab);
        self.config_dirs.remove(&tab);
        self.old_sidecars.remove(&tab);
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

/// The host `tab`'s project is on, as typed; "the server" for a tab that is not remote.
pub(super) fn host_of(state: &AgentPanelState, tab: TabId) -> String {
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
    super::remote_sessions::tick(state, out);
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
    let offered = super::remote_sessions::transcripts_offered(state);
    let transcripts = offered == Some(true);
    for (tab, key, ending, cli_session) in ended {
        // A gap with nothing waiting is asked about only once the window knows whether the server offers
        // transcripts: taken as seen before its Hello arrived, the rebuild question would never be asked.
        let undecided = offered.is_none()
            && cli_session.is_some()
            && matches!(&ending, agent::RemoteEnding::Gap { waiting } if waiting.is_empty());
        if undecided {
            continue;
        }
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
                    About::Gone {
                        cli_session,
                        rebuild: false,
                    },
                ),
                None => continue,
            },
            // The journal no longer reaches what the tab missed and nothing waits for an answer: with the server's
            // transcripts the panel can be rebuilt from the CLI's own record of the session.
            agent::RemoteEnding::Gap { .. } => match cli_session {
                Some(cli_session) if transcripts => (
                    vec![format!(
                        "tab {number}: {host} no longer holds what this tab missed; its history can be read back from the CLI's transcript there"
                    )],
                    // The agent may still be running there, and the rebuild ends it first: the label says so.
                    vec![choice(
                        'R',
                        format!("stop the agent on {host} and rebuild from the CLI's transcript"),
                        RemotePromptAction::Resume,
                    )],
                    About::Gone {
                        cli_session,
                        rebuild: true,
                    },
                ),
                _ => continue,
            },
        };
        // A question names the agent it is about when the tab knows it; one it cannot name is about the tab.
        let agent = (key != AgentKey::default()).then_some(key);
        let payload = state.remote_prompts.ask(tab, about, agent, &lines, &choices);
        send_to_tabs_page(state, tab, payload, out);
    }
}

/// Whether the end of `tab`'s session is [`ask_about_endings`]' to report: the helper said how it ended, the tab
/// shows the end, and that end is still to be asked about or its question is open. A tab taken up from the saved
/// tabs never sees its session open again (nothing replays the opening on a reattach), so an end folded right away
/// reads as a session that never opened; failing the tab then would retire the agent the question is about before
/// the question is asked. An end that needed no question is the never-opened report's again.
pub(super) fn ending_still_asked_about(state: &AgentPanelState, tab: TabId) -> bool {
    let Some(backend) = state.tabs.get(tab).filter(|t| t.is_remote()).and_then(|t| t.live()) else {
        return false;
    };
    let shown = matches!(backend.projection().status, agent::ProjectionStatus::Unavailable { .. });
    if !shown || backend.remote_ending().is_none() {
        return false;
    }
    let key = backend
        .helper_handoff()
        .map(|h| (h.stop_key(), h.session_id))
        .unwrap_or_default();
    !state.remote_prompts.endings_seen.contains(&(tab, key)) || state.remote_prompts.open.contains_key(&tab)
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
    state.remote_sessions.forget_tab(tab);
    out.send(
        view,
        eitri_core::agent_bridge::serialize_notice_for_js(&format!(
            "the agent of tab {number} keeps running on {host} until {host} is back; then it is stopped"
        )),
    );
    Ok(())
}

/// The resume of `provider_session_id` the person chose from the server's list, in `tab`: a start the gate puts off
/// until the project's check, then the one resume a remote tab starts (`start_deferred` takes the choice back out).
/// Refused when the helper offers no transcripts, for an agent other than Claude, and while the tab's folder on the
/// server is not named by a trust check, since the server's answers cannot be matched to the project without it.
pub(super) fn resume_from_server(
    state: &mut AgentPanelState,
    tab: TabId,
    request_id: &str,
    provider_session_id: String,
) -> Result<(), String> {
    if !super::remote_sessions::offers_transcripts(state) {
        return Err(super::detail::REMOTE_RESUME_UNAVAILABLE.to_string());
    }
    if state.tabs.get(tab).is_some_and(|t| t.agent().is_external()) {
        return Err("only Claude sessions on the server can be resumed from this list".to_string());
    }
    // The id comes from the page, and is sent to the server and used in paths there: only the form the CLI names its
    // sessions in goes on, and a refusal does not repeat what was sent.
    agent::remote_answers::checked_session_uuid("the session to resume", &provider_session_id)?;
    state.remote_prompts.rebuilds.remove(&tab);
    let start = DeferredStart::Resume {
        request_id: request_id.to_string(),
        resumed_title: state.remote_sessions.title_of(tab, &provider_session_id),
        provider_session_id: provider_session_id.clone(),
        resumed_name: None,
    };
    state.remote_prompts.resumes.insert(tab, provider_session_id);
    let gated = gate_remote_resume(state, tab, start);
    if gated.is_err() {
        state.remote_prompts.forget_resume(tab);
    }
    gated
}

/// A rebuilt tab whose session is back says so, when its history really was read from the CLI's transcript on the
/// server (Eitri's own copy, or none, is not a rebuild). Called when a start installed its backend in `tab`.
pub(super) fn note_rebuilt(state: &mut AgentPanelState, tab: TabId, out: &mut Outbox) {
    if !state.remote_prompts.rebuilds.remove(&tab) {
        return;
    }
    let host = host_of(state, tab);
    // Its own statement: the projection's guard is gone before the note is sent.
    let note = state.tabs.get(tab).and_then(|t| t.live()).and_then(|backend| {
        eitri_core::remote_project::resume::rebuilt_note_if_read_from_the_transcript(
            &host,
            backend.projection().history.as_ref(),
        )
    });
    if let Some(note) = note {
        send_to_tabs_page(state, tab, tab_notice(state, tab, &note), out);
    }
}

/// A start in `tab` did not come up: a rebuild it was to be is over.
pub(super) fn start_failed(state: &mut AgentPanelState, tab: TabId) {
    state.remote_prompts.rebuilds.remove(&tab);
}

/// What the connect worker of a resume in `tab` is given: the tab's canonical root as its trust scan named it and the
/// config directory a handshake reported, from the window's other tabs or kept from the agent this resume replaces.
pub(super) fn resume_scope(state: &mut AgentPanelState, tab: TabId) -> ResumeScope {
    let carried = state.remote_prompts.config_dirs.remove(&tab);
    ResumeScope {
        canonical_root: super::remote_sessions::canonical_root_of(state, tab),
        config_dir: super::remote_sessions::reported_config_dir_of(state, tab).or(carried),
        old_sidecar: state.remote_prompts.old_sidecars.remove(&tab),
    }
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
        (About::Gone { cli_session, rebuild }, _) => {
            // Read before the reset takes the tab's agent, and its handshake, away.
            let config_dir = super::remote_sessions::reported_config_dir_of(state, tab);
            let old_sidecar = state
                .tabs
                .get(tab)
                .and_then(|t| t.live())
                .and_then(|backend| backend.helper_handoff())
                .map(|agent| agent.sidecar_id);
            let unreached = unreached_agent(state, tab);
            // A wait an earlier choice left for this tab is over: its start did not happen.
            state.resume_after.remove(&tab);
            match state.tabs.reset(tab) {
                Ok(old) => {
                    keep_unreached(state, unreached);
                    if let Some(old) = old {
                        // The old agent is stopped on the server (after a gap it may still be running) before the
                        // new sidecar continues its session: the start waits for that stop.
                        let gone = state.retiring.backend_watched(old);
                        state.resume_after.insert(tab, (cli_session.clone(), gone));
                    }
                }
                Err(why) => return Err(why.to_string()),
            }
            state.trust.forget_tab(tab);
            state.remote_prompts.resumes.insert(tab, cli_session.clone());
            if let Some(dir) = config_dir {
                state.remote_prompts.config_dirs.insert(tab, dir);
            }
            match old_sidecar {
                Some(sidecar) => state.remote_prompts.old_sidecars.insert(tab, sidecar),
                None => state.remote_prompts.old_sidecars.remove(&tab),
            };
            if rebuild {
                state.remote_prompts.rebuilds.insert(tab);
            }
            let start = DeferredStart::Resume {
                request_id: request_id.to_string(),
                provider_session_id: cli_session,
                resumed_title: None,
                resumed_name: None,
            };
            if let Err(why) = gate_remote_resume(state, tab, start) {
                state.remote_prompts.forget_resume(tab);
                state.resume_after.remove(&tab);
                return Err(why);
            }
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

/// What a resume on a server needs to know of the tab: its canonical root as its trust scan named it (empty before one
/// did) and the config directory a handshake reported for its account there, when one did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ResumeScope {
    pub(super) canonical_root: String,
    pub(super) config_dir: Option<String>,
    /// The sidecar of the tab's previous agent, which must no longer run on the server before its session is
    /// continued.
    pub(super) old_sidecar: Option<String>,
}

/// How long a resume waits for the tab's previous agent to be stopped on the server: the stop's own bounds (the
/// session's close, then `StopSidecar` within the helper's quick-call limit), with room to spare.
pub(super) const REMOTE_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The connect worker of a remote tab: starts its agent on the server through the window's connection, spending the
/// window's account, or continues `resume` there through a new sidecar, once `after_old` (the teardown of the tab's
/// previous agent, which stops it there) is done.
pub(super) fn spawn_remote_connect(
    connection: eitri_core::remote_project::RemoteConnection,
    resume: Option<String>,
    scope: ResumeScope,
    project: ProjectTrust,
    client_tools: Option<agent::ClientToolSet>,
    after_old: Option<mpsc::Receiver<()>>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The tab's previous agent is stopped on the server before its session is continued through a new sidecar,
        // and nothing on the server refuses a second CLI on one session: so the resume goes on only once the helper
        // no longer runs the old sidecar, and is refused when it still does or cannot say.
        if let Some(after_old) = after_old {
            let _ = after_old.recv_timeout(REMOTE_STOP_WAIT);
        }
        if let (Some(_), Some(sidecar)) = (&resume, &scope.old_sidecar) {
            if let Some(message) = old_agent_refusal(connection.host(), connection.sidecar_running(sidecar)) {
                let _ = result_tx.send(Err(BackendError { message, benign: false }));
                return;
            }
        }
        let account = agent::account::configured().map(|account| account.name().to_string());
        // A start that fails leaving its sidecar running keeps that sidecar's stop here; the failure wakes the
        // window's stop sender ([`RemoteStops::send_now`]).
        let store = RemoteProjects::new(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        );
        let result = match resume {
            None => connection.start_tab(account.as_deref(), project, client_tools, &store),
            Some(session) => connection.resume_tab(
                &session,
                account.as_deref(),
                &scope.canonical_root,
                scope.config_dir.as_deref(),
                project,
                client_tools,
                &store,
            ),
        };
        let _ = result_tx.send(result);
    });
    result_rx
}

/// Why a resume must not go on, given whether the helper on `host` still runs the tab's previous agent: still running,
/// or no answer, is a refusal; gone is none.
fn old_agent_refusal(host: &str, running: Result<bool, String>) -> Option<String> {
    let why = match running {
        Ok(false) => return None,
        Ok(true) => "it is still running there".to_owned(),
        Err(why) => format!("the helper could not say whether it still runs ({why})"),
    };
    let host = eitri_core::onboarding::display::display_name(host);
    Some(format!(
        "the agent on {host} could not be stopped, so its session is not continued by a second one: {why}"
    ))
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
        // The tab starts without a trust question, and a page load scans no server for a tab that waits on nothing,
        // so its project is scanned here, once: the scan names the canonical root its path facts and transcripts
        // are matched against. Its result asks nothing; it only becomes the project's newest.
        let subject = super::trust::trust_subject(state, id);
        if state.trust.bound_discovery(subject.clone()).is_none() {
            state.trust.refresh(subject);
        }
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
pub(super) mod tests {
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
            late_lines: Vec::new(),
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

    /// A helper that offers transcripts and answers nothing: enough for the tab to know it may rebuild from them.
    struct IdleTranscripts;

    type Items<T> = std::pin::Pin<
        Box<
            dyn agent::remote_client::terminal_transport::tokio_stream::Stream<
                    Item = Result<T, agent::remote_client::terminal_transport::tonic::Status>,
                > + Send,
        >,
    >;

    #[agent::remote_client::terminal_transport::tonic::async_trait]
    impl agent::remote_client::terminal_transport::wire::transcripts_server::Transcripts for IdleTranscripts {
        async fn list_transcripts(
            &self,
            _: agent::remote_client::terminal_transport::tonic::Request<agent::remote_client::TranscriptScope>,
        ) -> Result<
            agent::remote_client::terminal_transport::tonic::Response<agent::remote_client::ListTranscriptsResponse>,
            agent::remote_client::terminal_transport::tonic::Status,
        > {
            Err(agent::remote_client::terminal_transport::tonic::Status::unimplemented(
                "idle",
            ))
        }
        type ReadTranscriptTailStream = Items<agent::remote_client::TranscriptChunk>;
        async fn read_transcript_tail(
            &self,
            _: agent::remote_client::terminal_transport::tonic::Request<agent::remote_client::ReadTranscriptRequest>,
        ) -> Result<
            agent::remote_client::terminal_transport::tonic::Response<Self::ReadTranscriptTailStream>,
            agent::remote_client::terminal_transport::tonic::Status,
        > {
            Err(agent::remote_client::terminal_transport::tonic::Status::unimplemented(
                "idle",
            ))
        }
        async fn transcript_stable(
            &self,
            _: agent::remote_client::terminal_transport::tonic::Request<agent::remote_client::ReadTranscriptRequest>,
        ) -> Result<
            agent::remote_client::terminal_transport::tonic::Response<agent::remote_client::TranscriptStableResponse>,
            agent::remote_client::terminal_transport::tonic::Status,
        > {
            Err(agent::remote_client::terminal_transport::tonic::Status::unimplemented(
                "idle",
            ))
        }
        type ReadDenySettingsStream = Items<agent::remote_client::ReadDenySettingsEvent>;
        async fn read_deny_settings(
            &self,
            _: agent::remote_client::terminal_transport::tonic::Request<agent::remote_client::TranscriptScope>,
        ) -> Result<
            agent::remote_client::terminal_transport::tonic::Response<Self::ReadDenySettingsStream>,
            agent::remote_client::terminal_transport::tonic::Status,
        > {
            Err(agent::remote_client::terminal_transport::tonic::Status::unimplemented(
                "idle",
            ))
        }
    }

    pub(crate) fn connection_offering_transcripts() -> eitri_core::remote_project::RemoteConnection {
        connection_offering(&[agent::remote_client::CAP_TRANSCRIPTS])
    }

    /// A connection whose helper's Hello, already held, lists `capabilities`.
    fn connection_offering(capabilities: &[&str]) -> eitri_core::remote_project::RemoteConnection {
        let client = agent::remote_client::HelperClient::serving_transcripts_for_test(
            "example-host",
            "SHA256:example",
            capabilities,
            IdleTranscripts,
        );
        let project = RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "0.4.0".into(),
            sync: None,
            created_at: 1,
            queued_stops: Vec::new(),
        };
        eitri_core::remote_project::RemoteConnection::with_client(client, project, String::new(), String::new())
    }

    const SESSION: &str = "0b7a7d3e-5f0c-4c52-9a0e-3a1f0c6d2b11";

    /// The tab with a live agent whose CLI session is known, as a rebuild or a resume asks for it.
    fn with_known_cli_session(state: &Rc<RefCell<AgentPanelState>>, tab: TabId, provider: &RecordingProvider) {
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
    }

    /// A start the fake spawner was asked for: the session to continue and the wait for the tab's old agent.
    type Spawned = Rc<RefCell<Vec<(Option<String>, Option<mpsc::Receiver<()>>)>>>;

    /// Makes the window's connect workers a recording spawner whose answers the test sends by hand.
    fn spawning_by_hand(
        state: &Rc<RefCell<AgentPanelState>>,
    ) -> (Spawned, mpsc::Sender<Result<AgentBackend, BackendError>>) {
        let spawned: Spawned = Rc::default();
        let into = spawned.clone();
        let (answer, result_rx) = mpsc::channel();
        let result_rx = RefCell::new(Some(result_rx));
        state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| match call {
            SpawnCall::Remote { resume, after_old, .. } => {
                into.borrow_mut().push((resume, after_old));
                result_rx.borrow_mut().take().expect("one start")
            }
            _ => panic!("a remote tab starts through the window's connection"),
        })));
        (spawned, answer)
    }

    /// A helper whose Hello lists no transcripts has nothing to rebuild from: the gap is seen and nothing is asked.
    #[test]
    fn a_gap_is_offered_no_rebuild_by_a_helper_without_transcripts() {
        let (state, tab, provider) = remote_panel("remote-gap-no-transcripts");
        with_known_cli_session(&state, tab, &provider);
        provider.set_remote(Some(agent::RemoteEnding::Gap { waiting: Vec::new() }), Some(handoff()));
        end_session(&state, tab, &provider);
        state.borrow_mut().server = Some(connection_offering(&[]));
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        assert!(prompt_in(&out).is_none(), "no transcripts, nothing to rebuild from");
        assert_eq!(state.borrow().remote_prompts.endings_seen.len(), 1);
    }

    /// A tab taken up from the saved tabs never sees its session open again, so a gap folded at once reads as a
    /// session that never opened. The never-opened report leaves it to the rebuild question while that end is still
    /// to be asked about (before the Hello) and while the question is open; an end that needed no question is
    /// reported as before.
    #[test]
    fn a_reattached_tab_ending_on_a_gap_before_it_opens_keeps_its_agent_for_the_rebuild_question() {
        let report = super::super::starts::report_sessions_that_never_opened;
        let live = |state: &Rc<RefCell<AgentPanelState>>, tab| state.borrow().tabs.get(tab).unwrap().live().is_some();
        let (state, tab, provider) = remote_panel("remote-reattach-gap");
        state.borrow_mut().tabs.get_mut(tab).unwrap().remote_agent = Some((handoff(), Some(SESSION.to_string())));
        provider.set_remote(Some(agent::RemoteEnding::Gap { waiting: Vec::new() }), Some(handoff()));
        end_session(&state, tab, &provider);
        assert!(state
            .borrow()
            .tabs
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .terminated_before_opening()
            .is_some());

        // Before the Hello: not asked yet, so not reported as a session that never opened.
        ask_about_endings(&mut state.borrow_mut(), &mut Outbox::default());
        report(
            &mut state.borrow_mut(),
            &Screen::nothing_shown(),
            &mut Outbox::default(),
        );
        assert!(live(&state, tab), "the agent stays for the question");

        // The Hello lists transcripts: the rebuild is offered, and the open question keeps the agent.
        state.borrow_mut().server = Some(connection_offering_transcripts());
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let prompt = prompt_in(&out).expect("the gap is offered a rebuild");
        assert_eq!(keys(&prompt), ["R"]);
        report(
            &mut state.borrow_mut(),
            &Screen::nothing_shown(),
            &mut Outbox::default(),
        );
        assert!(live(&state, tab), "the question is still open");
        let mut again = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut again);
        assert!(prompt_in(&again).is_none(), "asked once");

        // A helper without transcripts: the end needs no question, so the tab is reported as before.
        let (state, tab, provider) = remote_panel("remote-reattach-gap-no-transcripts");
        state.borrow_mut().tabs.get_mut(tab).unwrap().remote_agent = Some((handoff(), Some(SESSION.to_string())));
        provider.set_remote(Some(agent::RemoteEnding::Gap { waiting: Vec::new() }), Some(handoff()));
        end_session(&state, tab, &provider);
        state.borrow_mut().server = Some(connection_offering(&[]));
        ask_about_endings(&mut state.borrow_mut(), &mut Outbox::default());
        report(
            &mut state.borrow_mut(),
            &Screen::nothing_shown(),
            &mut Outbox::default(),
        );
        assert!(
            !live(&state, tab),
            "nothing to ask: the never-opened report fails the tab"
        );
    }

    /// A tab whose journal no longer reaches what it missed, with nothing waiting for an answer, is offered a rebuild
    /// from the CLI's transcript on the server once the window knows its helper has transcripts. The agent may
    /// still be running there, so the choice says it stops it, stops it before the session is continued through a new
    /// sidecar, and says the history was rebuilt only once the session is back with a history read from the
    /// transcript.
    #[test]
    fn a_gap_with_nothing_waiting_offers_a_rebuild_that_stops_the_agent_first() {
        let (state, tab, provider) = remote_panel("remote-gap-rebuild");
        with_known_cli_session(&state, tab, &provider);
        provider.set_remote(Some(agent::RemoteEnding::Gap { waiting: Vec::new() }), Some(handoff()));
        end_session(&state, tab, &provider);

        // No Hello yet: whether the server offers transcripts is not known, so the end waits to be asked about.
        let mut before_hello = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut before_hello);
        assert!(prompt_in(&before_hello).is_none());
        assert!(
            state.borrow().remote_prompts.endings_seen.is_empty(),
            "not taken as seen before the Hello"
        );

        // The Hello arrives, listing transcripts: the same end is offered a rebuild.
        state.borrow_mut().server = Some(connection_offering_transcripts());
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let prompt = prompt_in(&out).expect("the gap is offered a rebuild");
        assert_eq!(keys(&prompt), ["R"]);
        assert!(
            prompt["choices"][0]["label"]
                .as_str()
                .unwrap()
                .contains("stop the agent on example-host and rebuild from the CLI's transcript"),
            "{prompt}"
        );
        let (spawned, answer) = spawning_by_hand(&state);
        let release = provider.hold_remote_stop();
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
        assert!(
            !notices(&out).iter().any(|text| text.contains("rebuilt")),
            "nothing is said of a rebuild before the session is back: {:?}",
            notices(&out)
        );
        assert_eq!(
            state.borrow().remote_prompts.old_sidecars.get(&tab),
            Some(&handoff().sidecar_id),
            "the resume checks the old agent's sidecar is gone before it goes on"
        );
        let start = state
            .borrow_mut()
            .tabs
            .take_deferred(tab)
            .expect("the rebuild is a gated resume");
        assert!(
            matches!(start, DeferredStart::Resume { ref provider_session_id, .. } if provider_session_id == "claude-1")
        );
        let _ = start_deferred(&mut state.borrow_mut(), tab, start, ProjectTrust::Untrusted);
        let (resume, after_old) = spawned.borrow_mut().pop().expect("the resume was started");
        assert_eq!(resume.as_deref(), Some("claude-1"));
        let after_old = after_old.expect("the new sidecar waits for the old agent's stop");
        assert_eq!(
            after_old.try_recv(),
            Err(mpsc::TryRecvError::Empty),
            "the stop of the old agent has not gone yet"
        );
        assert_eq!(provider.remote_stops(), 0);
        release.send(()).unwrap();
        assert_eq!(after_old.recv_timeout(std::time::Duration::from_secs(10)), Ok(()));
        assert_eq!(
            provider.remote_stops(),
            1,
            "the old agent was stopped before the resume"
        );

        // A start that comes up with Eitri's own copy of the history, or none, is no rebuild from the transcript.
        let backend = {
            let dir = agent::state_dirs::test_workspace_dir("remote-gap-rebuilt");
            let provider = std::sync::Arc::new(RecordingProvider::default());
            let conversation = agent::AgentConversation::create(provider, &dir, ProjectTrust::Untrusted).unwrap();
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }
        };
        answer.send(Ok(backend)).unwrap();
        let mut out = Outbox::default();
        wait_until("the start installed", || {
            super::super::starts::collect_pending_starts(&mut state.borrow_mut(), &Screen::nothing_shown(), &mut out);
            matches!(state.borrow().tabs.get(tab).unwrap().backend(), TabBackend::Live(_))
        });
        assert!(
            !notices(&out).iter().any(|text| text.contains("rebuilt")),
            "{:?}",
            notices(&out)
        );
        assert!(state.borrow().remote_prompts.rebuilds.is_empty());
    }

    #[test]
    fn a_resume_goes_on_only_once_the_old_agent_is_gone_on_the_server() {
        assert_eq!(old_agent_refusal("example-host", Ok(false)), None);
        let running = old_agent_refusal("example-host", Ok(true)).unwrap();
        assert!(
            running.starts_with("the agent on example-host could not be stopped"),
            "{running}"
        );
        assert!(running.ends_with("it is still running there"), "{running}");
        let unknown = old_agent_refusal("example\nhost", Err("unreachable".into())).unwrap();
        assert!(
            unknown.contains("could not say whether it still runs (unreachable)"),
            "{unknown}"
        );
        assert!(!unknown.contains('\n'), "{unknown}");
    }

    /// A rebuilt tab says so when its history was read from the CLI's transcript on the server, and a rebuild whose
    /// start failed says nothing and is forgotten.
    #[test]
    fn a_rebuild_is_noted_when_the_session_is_back_with_the_transcripts_history() {
        let (state, tab, provider) = remote_panel("remote-gap-noted");
        let _ = provider;
        let transcript = format!(
            "{}\n{}\n",
            r#"{"type":"user","uuid":"u-1","version":"2.1.272","promptSource":"typed","message":{"role":"user","content":"question"}}"#,
            r#"{"type":"assistant","uuid":"a-1","version":"2.1.272","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#
        );
        let seed =
            agent::history::seed_from_tail(transcript.as_bytes(), false, "conversation", SESSION, "example-host:/t");
        let resumed = std::sync::Arc::new(Resuming(RecordingProvider::default()));
        let conversation = agent::AgentConversation::resume_remote_with_tools(
            resumed,
            std::path::Path::new("/srv/remote-gap-noted"),
            SESSION,
            ProjectTrust::Untrusted,
            None,
            Some(seed),
        )
        .unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        // Another tab, empty, is the one the rebuild starts in.
        let other = state.borrow_mut().tabs.open(AgentId::Claude);
        state.borrow_mut().remote_prompts.rebuilds.insert(other);
        state
            .borrow_mut()
            .tabs
            .get_mut(other)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        let mut out = Outbox::default();
        note_rebuilt(&mut state.borrow_mut(), other, &mut out);
        assert_eq!(
            notices(&out)
                .iter()
                .filter(|text| text.contains("rebuilt from the CLI's transcript on example-host"))
                .count(),
            1,
            "{:?}",
            notices(&out)
        );
        let mut again = Outbox::default();
        note_rebuilt(&mut state.borrow_mut(), other, &mut again);
        assert!(notices(&again).is_empty(), "said once");

        state.borrow_mut().remote_prompts.rebuilds.insert(tab);
        start_failed(&mut state.borrow_mut(), tab);
        assert!(state.borrow().remote_prompts.rebuilds.is_empty());
    }

    /// A provider that resumes sessions, which the recording one does not.
    struct Resuming(RecordingProvider);

    impl agent::AgentProvider for Resuming {
        fn capabilities(&self) -> agent::ProviderCapabilities {
            agent::ProviderCapabilities {
                resume: true,
                ..agent::AgentProvider::capabilities(&self.0)
            }
        }
        fn info(&self) -> agent::ProviderInfo {
            agent::AgentProvider::info(&self.0)
        }
        fn create_session(&self, request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
            agent::AgentProvider::create_session(&self.0, request)
        }
        fn resume_session(&self, _request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
            Ok("session-resumed".into())
        }
        fn send_turn(&self, request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
            agent::AgentProvider::send_turn(&self.0, request)
        }
        fn interrupt_turn(&self, request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
            agent::AgentProvider::interrupt_turn(&self.0, request)
        }
        fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
            agent::AgentProvider::resolve_permission(&self.0, request)
        }
        fn close_session(&self, request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
            agent::AgentProvider::close_session(&self.0, request)
        }
        fn answer_client_tool_call(&self, answer: agent::ClientToolAnswer) -> Result<(), agent::ProviderError> {
            agent::AgentProvider::answer_client_tool_call(&self.0, answer)
        }
        fn client_tool_call_state(
            &self,
            session_id: &str,
        ) -> Result<agent::TaskReply<agent::ClientToolCallState>, agent::ProviderError> {
            agent::AgentProvider::client_tool_call_state(&self.0, session_id)
        }
        fn pump(&self) -> Vec<agent::AgentDomainEvent> {
            agent::AgentProvider::pump(&self.0)
        }
    }

    /// A window over a remote project with one empty tab, its connection to a helper that offers transcripts (or one
    /// that is not given a connection yet), and a page.
    fn remote_window(offering: bool) -> (AgentPanelHandle, Rc<RecordingPage>, TabId) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let tab = {
            let mut s = state.borrow_mut();
            s.tabs.set_project_origin(origin()).unwrap();
            if offering {
                s.server = Some(connection_offering_transcripts());
            }
            s.tabs.active()
        };
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state,
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        (handle, page, tab)
    }

    /// What `request_id`'s command was refused with, if it was.
    fn refusal_of(page: &RecordingPage, request_id: &str) -> Option<String> {
        page.sent
            .borrow()
            .iter()
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            .find(|v| v["kind"] == "command_result" && v["requestId"] == request_id && v["ok"] == false)
            .and_then(|v| v["error"].as_str().map(str::to_string))
    }

    fn resume_message(tab: TabId, request_id: &str, id: &str) -> String {
        format!(
            r#"{{"type":"resume","request_id":"{request_id}","tab":{},"provider_session_id":"{id}"}}"#,
            tab.0
        )
    }

    /// The chooser's pick on a remote tab reaches the server's resume: the page's `resume` message is not refused
    /// for being on a remote tab, holds no lease on this machine, and puts the resume behind the trust question. A
    /// window whose helper offers no transcripts refuses it as before, and an id that is not a session's name
    /// goes nowhere.
    #[test]
    fn a_session_picked_in_the_chooser_on_a_remote_tab_reaches_the_resume() {
        let (handle, page, tab) = remote_window(true);
        let tabs_before = handle.state.borrow().tabs.tabs().len();
        handle.page_message(ViewId::BASE, &resume_message(tab, "bad", "not a session"));
        assert_eq!(
            handle.state.borrow().tabs.tabs().len(),
            tabs_before,
            "a malformed id opens no tab of its own"
        );
        let why = refusal_of(&page, "bad").expect("an id the CLI would not name is refused");
        assert!(
            why.contains("lowercase hyphenated UUID") && !why.contains("not a session"),
            "{why}"
        );
        assert!(matches!(
            handle.state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::NotStarted
        ));

        // A lease on this machine for the same id is no reason to refuse: the session is on the server.
        let canonical = handle.state.borrow().canonical_project_dir.clone();
        let _lease = agent::lease::SessionLease::try_acquire("claude", &canonical, SESSION).unwrap();
        handle.page_message(ViewId::BASE, &resume_message(tab, "pick", SESSION));
        assert_eq!(refusal_of(&page, "pick"), None);
        let state = handle.state.borrow();
        match state.tabs.get(tab).unwrap().backend() {
            TabBackend::AwaitingTrust(waiting) => assert!(
                matches!(&waiting.start, DeferredStart::Resume { provider_session_id, .. } if provider_session_id == SESSION)
            ),
            _ => panic!("the resume waits for the trust question"),
        }
        drop(state);

        let (handle, page, tab) = remote_window(false);
        handle.page_message(ViewId::BASE, &resume_message(tab, "pick", SESSION));
        assert_eq!(
            refusal_of(&page, "pick").as_deref(),
            Some(super::super::detail::REMOTE_RESUME_UNAVAILABLE)
        );
        assert!(matches!(
            handle.state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::NotStarted
        ));
    }

    /// The config directory the tab's own agent reported goes with the resume that replaces it: the reset takes the
    /// handshake it came from, and the transcripts the resume reads are matched against that directory.
    #[test]
    fn a_resume_after_a_lost_agent_carries_the_config_dir_its_agent_reported() {
        let provider = RecordingProvider::default();
        provider.set_info(agent::ProviderInfo {
            account_config_dir: Some("/home/example/.claude".into()),
            ..Default::default()
        });
        let (state, tab, provider) = remote_panel_with("remote-gone-config-dir", provider);
        with_known_cli_session(&state, tab, &provider);
        provider.set_remote(Some(agent::RemoteEnding::AgentGone), Some(handoff()));
        end_session(&state, tab, &provider);
        let mut out = Outbox::default();
        ask_about_endings(&mut state.borrow_mut(), &mut out);
        let prompt = prompt_in(&out).expect("the lost agent is asked about");
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
        assert!(
            state.borrow().tabs.get(tab).unwrap().live().is_none(),
            "the tab was reset: its handshake is gone"
        );
        let scope = resume_scope(&mut state.borrow_mut(), tab);
        assert_eq!(scope.config_dir.as_deref(), Some("/home/example/.claude"));
        assert_eq!(
            resume_scope(&mut state.borrow_mut(), tab).config_dir,
            None,
            "carried for one resume only"
        );
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
        let scratch = agent::state_dirs::test_workspace_dir("remote-reattach-saved-trust");
        state.borrow_mut().trust = super::super::trust_gate_for(None, None, &scratch);
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
                late_lines: Vec::new(),
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
        // A tab taken up asks no trust question, so its project is scanned for it: the scan names the canonical root
        // its path facts and transcripts are matched against.
        let taken_up = s.tabs.tabs().iter().find(|t| t.remote_agent.is_some()).unwrap().id;
        assert_eq!(
            s.trust
                .server_checks_of(super::super::trust::trust_subject(&s, taken_up)),
            1,
            "one scan for the project"
        );
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
            late_lines: Vec::new(),
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
                late_lines: Vec::new(),
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
