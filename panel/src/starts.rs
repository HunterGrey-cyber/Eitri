//! Starting a tab's session: `start_deferred`, the one place a session begins once the trust gate has
//! answered, the workers that connect a backend, and the collection of what they started.

use super::*;

/// How [`start_deferred`] begins a session.
pub(super) enum Spawner {
    /// The connect workers (`spawn_connect`, `resume_spawner`).
    Real,
    /// A test's stand-in, told what would have been spawned.
    #[cfg(test)]
    Fake(Rc<RefCell<FakeSpawn>>),
}

/// A test's stand-in spawner: given what would have been started, the receiver its result arrives on.
#[cfg(test)]
pub(super) type FakeSpawn = dyn FnMut(SpawnCall) -> mpsc::Receiver<Result<AgentBackend, BackendError>>;

/// What a test's [`Spawner::Fake`] is asked to start.
#[cfg(test)]
pub(super) enum SpawnCall {
    /// A fresh session, or a resume by hand (`resume`).
    Connect {
        resume: Option<String>,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
    },
    /// One tab of a restore, with the lease already taken for it.
    Resume {
        id: String,
        lease: agent::lease::SessionLease,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
    },
    /// A tab of another agent, fresh or resumed, with the trust plan it was given and, for an ACP
    /// agent, its registration.
    External {
        agent: AgentId,
        plan: agent::TrustPlan,
        mode: SessionModeChoice,
        resume: Option<String>,
        acp: Option<agent::providers::acp::AcpAgentSpec>,
        /// The teardown of the tab's previous backend, which the start waits for (`r` continuing a session).
        after_old: Option<mpsc::Receiver<()>>,
        /// The tool server the session is to run.
        editor_server: Option<agent::EditorServerSpec>,
    },
}

/// Puts the resume of `provider_session_id` into the empty tab `tab`, and selects the tab when `select`. It
/// waits for the trust check like a first message (a resumed session loads the project's configuration only
/// as a new one would); `request_id` is answered once the connect finishes or the question is put off.
pub(super) fn resume_in(
    state: &mut AgentPanelState,
    tab: TabId,
    request_id: &str,
    provider_session_id: String,
    select: bool,
) -> Result<(), String> {
    let agent = state.tabs.get(tab).map_or(AgentId::Claude, |t| t.agent().clone());
    let greeting = BackendGreeting::for_agent_titled(
        state.backend_kind,
        &agent,
        state.project_dir.clone(),
        eitri_core::agent_backend::Titles::Cached(&state.titles),
    );
    // The rename and the title separately: the label puts a rename first, and folding the two into the
    // title lost the rename whenever the record had both.
    let (resumed_title, resumed_name) = greeting
        .resumable
        .iter()
        .find(|r| r.provider_session_id == provider_session_id)
        .map(|r| (r.title.clone(), r.name.clone()))
        .unwrap_or_default();
    let start = DeferredStart::Resume {
        request_id: request_id.to_string(),
        provider_session_id,
        resumed_title,
        resumed_name,
    };
    let deferred = gate_start(state, tab, start);
    if select {
        // A new tab is already selected by `route_resume`; this is for the record.
        state.tabs.select(tab);
    }
    deferred
}

/// Starts a session the gate let through, with the tiers it was given. The one place a session's
/// backend is begun: a first message, a resume by hand and a restore all come here, after the trust
/// check, so none of them can load the project's own configuration unasked.
pub(super) fn start_deferred(
    state: &mut AgentPanelState,
    tab: TabId,
    start: DeferredStart,
    project: ProjectTrust,
) -> Outbox {
    let mut out = Outbox::default();
    if state.tabs.get(tab).is_none() {
        return out;
    }
    // A question the tab may still have had on screen is spent: the tab is starting.
    state.trust.hide(tab);
    let agent = state.tabs.get(tab).expect("checked above").agent().clone();
    if agent.is_external() && !matches!(start, DeferredStart::Restore { .. }) {
        return start_external_tab(state, tab, agent, start, project);
    }
    let (kind, project_dir) = (state.backend_kind, state.project_dir.clone());
    let client_tools = editor_glue::tools_for_start(state, project);
    match start {
        DeferredStart::FirstSend { request_id, first_turn } => {
            let result_rx = match &state.spawner {
                Spawner::Real => spawn_connect(kind, project_dir, None, project, client_tools),
                #[cfg(test)]
                Spawner::Fake(fake) => (fake.borrow_mut())(SpawnCall::Connect {
                    resume: None,
                    project,
                    client_tools,
                }),
            };
            let t = state.tabs.get_mut(tab).expect("checked above");
            t.session.project_tiers = Some(project);
            state.tabs.start(
                tab,
                PendingStart {
                    request_id,
                    result_rx,
                    first_turn: Some(first_turn),
                    resume: None,
                    resumed_title: None,
                    resumed_name: None,
                },
            );
            // The `command_result` is owed once the connect finishes and the first turn is sent
            // (`collect_pending_starts`).
            let view = state.tabs.view_of(tab);
            out.push(Dest::Page(view), tabs_payload_recorded(state, view));
        }
        DeferredStart::Resume {
            request_id,
            provider_session_id,
            resumed_title,
            resumed_name,
        } => {
            let resume = Some(provider_session_id.clone());
            let result_rx = match &state.spawner {
                Spawner::Real => spawn_connect(kind, project_dir, resume, project, client_tools),
                #[cfg(test)]
                Spawner::Fake(fake) => (fake.borrow_mut())(SpawnCall::Connect {
                    resume,
                    project,
                    client_tools,
                }),
            };
            let t = state.tabs.get_mut(tab).expect("checked above");
            t.session.project_tiers = Some(project);
            state.tabs.start(
                tab,
                PendingStart {
                    request_id,
                    result_rx,
                    first_turn: None,
                    resume: Some(provider_session_id),
                    resumed_title,
                    resumed_name,
                },
            );
            // The command_result is owed once the connect finishes.
            shown_payloads(state, tab, &mut out);
        }
        DeferredStart::Restore { go } => {
            let result = match &state.spawner {
                Spawner::Real => {
                    let mut spawn = resume_spawner(state, client_tools);
                    start_restore(state, tab, go, &mut spawn, project)
                }
                #[cfg(test)]
                Spawner::Fake(fake) => {
                    let fake = fake.clone();
                    let mut spawn = move |id: &str, lease, project| {
                        (fake.borrow_mut())(SpawnCall::Resume {
                            id: id.to_string(),
                            lease,
                            project,
                            client_tools: client_tools.clone(),
                        })
                    };
                    start_restore(state, tab, go, &mut spawn, project)
                }
            };
            switch_payloads(state, &mut out);
            out.restored = Some(match result {
                RestoreResult::Started { notice } => notice,
                _ => None,
            });
        }
    }
    out
}

/// What a tab of `agent` takes from the project at `project_dir`, given the answer `project`. At a root that is no
/// boundary what is in the folder is nobody's word, whatever was answered: a rule or a hook there would take a command
/// out of Codex's read-only sandbox with no card. A registered agent's own configuration is out of reach, so the
/// answer changes nothing for it.
pub(super) fn project_trust_at_root(
    agent: &AgentId,
    project: ProjectTrust,
    project_dir: &std::path::Path,
) -> ProjectTrust {
    if *agent == AgentId::Codex && !eitri_core::agent_backend::root_is_a_boundary(project_dir) {
        ProjectTrust::Untrusted
    } else {
        project
    }
}

/// [`start_deferred`] for a tab of another agent: its trust plan is built from the discovery the
/// gate's answer is bound to, and the agent's own connect worker is begun with it. Without such a
/// discovery (the trust check could not run) or with a root no agent configuration can name, the
/// start is dropped and says why. So is an untrusted start of an agent that has no way to leave the
/// project's configuration out, whatever routed it here: such an agent starts only after a `y`.
pub(super) fn start_external_tab(
    state: &mut AgentPanelState,
    tab: TabId,
    agent: AgentId,
    start: DeferredStart,
    project: ProjectTrust,
) -> Outbox {
    // Taken whatever happens to this start, so a wait left for it never holds a later one.
    let wait = state.resume_after.remove(&tab);
    let profile = trust_profile(&state.acp_agents, &agent);
    if project != ProjectTrust::Trusted && profile.requires_explicit_yes() {
        return put_off_start(state, tab, start, &crate::trust_gate::refused_without_yes(&profile));
    }
    let project = project_trust_at_root(&agent, project, &state.project_dir);
    // A registered agent starts with the command its registration names; a tab of a name nothing
    // registers (saved while it was) has none to run.
    let acp = match &agent {
        AgentId::Acp(name) => match acp_spec(&state.acp_agents, name) {
            Some(spec) => Some(spec),
            None => {
                let why = format!("{name} was not started: no agent of that name is registered in init.lua");
                return put_off_start(state, tab, start, &why);
            }
        },
        AgentId::Claude | AgentId::Codex => None,
    };
    let answer = match project {
        ProjectTrust::Trusted => agent::DirTrust::Trusted,
        ProjectTrust::Untrusted | ProjectTrust::NothingFound => agent::DirTrust::Untrusted,
    };
    let plan = match state.trust.bound_discovery(&profile) {
        Some(discovery) => {
            eitri_core::project_trust::trust_plan(discovery, answer, &discovery.root).map_err(|e| e.to_string())
        }
        None => Err(format!(
            "{} was not started: the trust check of this project has not run",
            agent.label()
        )),
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(why) => return put_off_start(state, tab, start, &why),
    };
    let (request_id, resume, resumed_title, resumed_name, first_turn) = match start {
        DeferredStart::FirstSend { request_id, first_turn } => (request_id, None, None, None, Some(first_turn)),
        DeferredStart::Resume {
            request_id,
            provider_session_id,
            resumed_title,
            resumed_name,
        } => (request_id, Some(provider_session_id), resumed_title, resumed_name, None),
        // A restore is started by `start_deferred` itself.
        DeferredStart::Restore { .. } => return Outbox::default(),
    };
    let mode = state.tabs.get(tab).map_or(SessionModeChoice::Auto, |t| t.mode());
    let project_dir = state.project_dir.clone();
    let codex_command = state.codex_command.clone();
    // Only the resume of the session whose old backend it waits for.
    let after_old = wait
        .filter(|(id, _)| resume.as_deref() == Some(id.as_str()))
        .map(|(_, gone)| gone);
    let editor_name = state.trust.bound_discovery(&profile).map_or(
        eitri_core::project_trust::EditorServerName::Unknown,
        eitri_core::project_trust::eitri_server_name,
    );
    let editor_server = editor_glue::server_for_start(state, tab, &agent, project, editor_name);
    let result_rx = match &state.spawner {
        Spawner::Real => spawn_external(
            agent,
            project_dir,
            plan,
            mode,
            resume.clone(),
            codex_command,
            acp,
            state.start_notes.sender(),
            after_old,
            editor_server,
        ),
        #[cfg(test)]
        Spawner::Fake(fake) => (fake.borrow_mut())(SpawnCall::External {
            agent,
            plan,
            mode,
            resume: resume.clone(),
            acp,
            after_old,
            editor_server,
        }),
    };
    let Some(t) = state.tabs.get_mut(tab) else {
        return Outbox::default();
    };
    t.session.project_tiers = Some(project);
    let resuming = resume.is_some();
    state.tabs.start(
        tab,
        PendingStart {
            request_id,
            result_rx,
            first_turn,
            resume,
            resumed_title,
            resumed_name,
        },
    );
    let mut out = Outbox::default();
    if resuming {
        shown_payloads(state, tab, &mut out);
    } else {
        let view = state.tabs.view_of(tab);
        out.push(Dest::Page(view), tabs_payload_recorded(state, view));
    }
    out
}

/// Records that the start of `tab`, owed a `command_result` under `request_id`, was asked from `view`'s
/// page: that page holds the request, wherever the tab is when the start ends. A start owed before it
/// for the same tab is replaced.
pub(super) fn note_start_page(state: &mut AgentPanelState, tab: TabId, request_id: &str, view: ViewId) {
    state.owed.retain(|_, answer| answer.what != Owed::StartPage(tab));
    state.owed.owe(view, request_id, Owed::StartPage(tab));
}

/// Where the `command_result` owed under `request_id` for `tab`'s start goes: the page it was asked
/// from, or the page of the tab's view when none is recorded. `None` when the page that asked has
/// closed: nothing is sent. The record is spent.
pub(super) fn start_answer_dest(state: &mut AgentPanelState, tab: TabId, request_id: &str) -> Option<Dest> {
    let asked = state
        .owed
        .take_where(|answer| answer.what == Owed::StartPage(tab) && answer.request_id == request_id);
    match asked {
        Some(asked) => asked.page.map(Dest::Page),
        None => Some(Dest::Tab(tab)),
    }
}

/// The connect worker, shared by `send_message` on an empty tab and `resume`. Off the GTK main
/// loop: constructing the sidecar backend spawns a real process, does a real gRPC handshake, and on
/// a cold Verdandi checkout runs `npm ci` + `npm run build` -- minutes, in the cold case.
pub(super) fn spawn_connect(
    kind: BackendKind,
    project_dir: PathBuf,
    resume: Option<String>,
    project: ProjectTrust,
    client_tools: Option<agent::ClientToolSet>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    spawn_connect_holding(kind, project_dir, resume, None, project, client_tools)
}

/// What the band says while a Codex binary Eitri has not checked yet goes through its self-check.
pub(super) const CHECKING_CODEX: &str = "checking this Codex\u{2026} (once for this binary, a few seconds)";

/// Where the self-check keeps a verdict, under Eitri's state directory: `codex-check/<key>/verdict.json`,
/// written only for a pass.
pub(super) const CODEX_CHECK_DIR: &str = "codex-check";

/// Whether starting `program` will run the self-check first: no passing verdict is kept for this binary
/// under `state_home`. Asked only to say so in the band; the start decides for itself. A binary whose
/// key cannot be read, or one too old to be checked, is refused by the start before any check runs, so
/// it says nothing here. Runs a `--version` of the binary: worker threads only.
pub(super) fn codex_check_pending(program: &Path, state_home: Option<&Path>) -> bool {
    let Some(state_home) = state_home else { return false };
    let Ok(key) = agent::providers::codex::BinaryKey::of(program) else {
        return false;
    };
    if key
        .version_numbers()
        .is_some_and(|found| found < agent::providers::codex::MIN_CODEX)
    {
        return false;
    }
    !state_home
        .join(CODEX_CHECK_DIR)
        .join(key.id())
        .join("verdict.json")
        .is_file()
}

/// The notes connect workers send while they work, drained by the tick.
pub(super) struct StartNotes {
    pub(super) tx: mpsc::Sender<String>,
    pub(super) rx: mpsc::Receiver<String>,
}

impl Default for StartNotes {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        StartNotes { tx, rx }
    }
}

impl StartNotes {
    pub(super) fn sender(&self) -> mpsc::Sender<String> {
        self.tx.clone()
    }

    /// Every note sent since the last call, as `notice` payloads for the band. Never blocks.
    pub(super) fn drain(&self) -> Vec<String> {
        self.rx
            .try_iter()
            .map(|note| eitri_core::agent_bridge::serialize_notice_for_js(&note))
            .collect()
    }
}

/// How long a session continued in its own tab (`r`) waits for the tab's old backend to let go of the lease.
pub(super) const OLD_SESSION_LET_GO: std::time::Duration = std::time::Duration::from_secs(10);

/// The connect worker of another agent's tab: locates its binary (Codex) or takes its registered
/// command (`acp`, an ACP agent) and starts (or resumes) its session with `plan`, off the GTK main loop,
/// since the agent's own check and handshake take seconds. With `after_old`, it first waits (at most
/// [`OLD_SESSION_LET_GO`]) for the tab's old backend to be dropped.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_external(
    agent: AgentId,
    project_dir: PathBuf,
    plan: agent::TrustPlan,
    mode: SessionModeChoice,
    resume: Option<String>,
    codex_command: Option<String>,
    acp: Option<agent::providers::acp::AcpAgentSpec>,
    notes: mpsc::Sender<String>,
    after_old: Option<mpsc::Receiver<()>>,
    editor_server: Option<agent::EditorServerSpec>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    use eitri_core::agent_backend::ExternalLaunch;
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The session's previous backend holds its lease until it is dropped; the resume would find it held.
        // Bounded: a teardown that hangs leaves the resume to fail on the lease and say so.
        if let Some(after_old) = after_old {
            let _ = after_old.recv_timeout(OLD_SESSION_LET_GO);
        }
        let launch = match &agent {
            AgentId::Codex => {
                // A relative home, or one inside the project, is refused before anything runs, the version
                // probe of the binary included.
                let located = agent::providers::codex::refuse_codex_home(&project_dir)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        agent::providers::codex::locate_codex(codex_command.as_deref()).map_err(|e| e.to_string())
                    })
                    .map_err(|e| BackendError {
                        message: format!("Codex could not start: {e}"),
                        benign: false,
                    });
                // The start is about to run the self-check: the tab would otherwise sit in "starting"
                // for seconds with nothing said.
                if let Ok(program) = &located {
                    let state_home = eitri_core::state_paths::state_subdir(
                        std::env::var_os("XDG_STATE_HOME").as_deref(),
                        std::env::var_os("HOME").as_deref(),
                        "",
                    );
                    if codex_check_pending(program, state_home.as_deref()) {
                        let _ = notes.send(CHECKING_CODEX.to_string());
                    }
                }
                located.map(ExternalLaunch::Codex)
            }
            AgentId::Acp(name) => acp
                .ok_or_else(|| BackendError {
                    message: format!("{name} was not started: no agent of that name is registered in init.lua"),
                    benign: false,
                })
                .map(ExternalLaunch::Acp),
            AgentId::Claude => Err(BackendError {
                message: "a Claude tab is not started as another agent's".to_string(),
                benign: false,
            }),
        };
        let result = launch.and_then(|launch| {
            AgentBackend::start_external(
                agent,
                launch,
                &project_dir,
                plan,
                mode,
                resume.as_deref(),
                editor_server,
            )
        });
        // As in `spawn_connect_holding`: a receiver dropped mid-connect makes this drop the cleanup.
        let _ = result_tx.send(result);
    });
    result_rx
}

/// [`spawn_connect`] for a resume whose session lease is already held: the worker hands it to the
/// resume, which keeps it for as long as the session lives.
pub(super) fn spawn_connect_holding(
    kind: BackendKind,
    project_dir: PathBuf,
    resume: Option<String>,
    lease: Option<agent::lease::SessionLease>,
    project: ProjectTrust,
    client_tools: Option<agent::ClientToolSet>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = AgentBackend::start_holding(kind, &project_dir, resume.as_deref(), lease, project, client_tools);
        // The receiver is gone only if the tab or the panel was torn down mid-connect; dropping
        // the backend here is then the cleanup (`Retiring` normally holds the receiver instead).
        let _ = result_tx.send(result);
    });
    result_rx
}

/// `agent-ui/web/src/problems.ts`'s own `SIDECAR_STOPPED_MARKER`, matched the same way (a plain
/// substring check) and pinned to the same source: `stream_ended_early_reason`'s opening sentence
/// in `agent/src/providers/claude_sidecar/watch.rs` -- see
/// `the_sidecar_stopped_marker_is_pinned_to_watch_rs` below. A `reason` carrying this means the
/// provider's event stream ended under a session that really was open, which is not what
/// `never_opened_message` below guesses for every other reason.
pub(super) const SIDECAR_STOPPED_MARKER: &str = "the connection to the provider ended before this session did";

/// The text `report_sessions_that_never_opened` shows for a `reason`. Split out so it can be
/// tested without a live tab.
///
/// **Most `reason`s here mean the session never opened at all**, so the generic guess ("it most
/// likely no longer exists") is right for them. **A resumed sidecar session is the exception**: it
/// reports itself open only at its first new turn (`AgentConversation` never re-plays
/// `SessionReady` on attach), so a sidecar that stops before that first turn lands here too even
/// though the conversation and its transcript are completely fine -- the guess is simply wrong for
/// it. `agent-ui/web/src/problems.ts`'s `classifySidecarStopped`/`failureEvidence` read this
/// reason correctly on the panel side, so this wrapper gives a `reason` naming a stopped sidecar
/// its own wording with no guess appended instead of folding it into the generic case.
pub(super) fn never_opened_message(reason: &str) -> String {
    if reason.contains(SIDECAR_STOPPED_MARKER) {
        format!("the agent sidecar stopped ({reason})")
    } else {
        format!(
            "the session ended before it started ({reason}). If you were continuing a previous \
             conversation, it most likely no longer exists -- start a new session instead."
        )
    }
}

/// A session that reached a terminal state without ever opening never worked, and must not be
/// left on screen as an empty conversation the user cannot act on. Checked for every tab.
///
/// `AgentConversation::resume` catches the common case synchronously, within its own short window.
/// This is the backstop for everything slower than that window and for fresh sessions, which have
/// no equivalent check: either way the tab becomes `Failed` with the provider's own reason, where `r` starts it over. It is never turned into a fresh session automatically. The
/// `error{tab}` envelope goes out only if that tab is on screen, to the page that shows it; the tab
/// keeps the reason for when it is shown, and no other tab is touched.
///
/// **`reason` used to be a fixed, generic string on the legacy backend** ("provider process
/// exited unexpectedly") for every non-zero exit, regardless of cause -- and that generic text
/// actively misdirected a real investigation (2026-09-18, work, production binary): the child
/// died before opening because a `PATH`-resolved multi-account launcher refused the gate-bearing
/// `--settings` flag outright, and the launcher's own one-line explanation had already gone past
/// on stderr with nowhere for this banner to recover it from. `agent::session`'s translation of
/// `AgentEvent::ProcessExited` now folds the child's own retained stderr tail into
/// `SessionUnavailable.reason` itself (see `agent/src/session.rs`), so `reason` below is already
/// the specific one when the provider produced any stderr at all before dying. See
/// `never_opened_message` for the one exception to the guess it otherwise wraps `reason` in.
pub(super) fn report_sessions_that_never_opened(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    let failed: Vec<(TabId, String)> = state
        .tabs
        .tabs()
        .iter()
        .filter(|t| !t.session.reported_start_failure)
        .filter_map(|t| {
            t.live()
                .and_then(AgentBackend::terminated_before_opening)
                .map(|r| (t.id, r))
        })
        .collect();
    for (tab, reason) in failed {
        let message = never_opened_message(&reason);
        eprintln!("[agent_panel] tab {}: {message}", tab.0);
        let shown_in = state.shown_in(tab);
        let shown = shown_in.is_some_and(|view| screen.shows(view));
        if state.tabs.get(tab).is_none() {
            continue;
        }
        let queue_before = queue_shown(state, tab);
        // Its cards go with it (the tick reports the change), and so does any message it never read.
        let dead = state
            .tabs
            .fail_session(tab, message.clone(), FailedHow::StartNeverOpened, shown);
        if let Some(retired) = dead {
            state.retiring.backend(retired);
        }
        if let Some(view) = shown_in {
            out.send(view, serialize_error_for_js(tab, &message));
        }
        if queue_shown(state, tab) != queue_before {
            send_queue(state, tab, out);
        }
    }
}

/// Every connect that finished since the last tick (`TabSet::collect_starts`), answered. `try_recv`
/// only, inside the set: this runs on the GTK main loop 30 times a second and must never block it --
/// which is the entire reason construction was moved off this thread in the first place.
///
/// What the page showing a tab is owed goes to that page; a command's answer to the page it was sent
/// from (`start_answer_dest`), which need not show the tab any more.
pub(super) fn collect_pending_starts(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    let collected = state.tabs.collect_starts();
    for result in collected {
        let part_of_a_restore = account_for_restore(state, &result);
        match result {
            StartCollected::Installed {
                tab,
                request_id,
                first_turn,
            } => {
                // A tab a restore started: nobody asked for this connect by name, so there is no
                // `command_result` owed -- the restore reports once, when its last resume is back.
                let restored = part_of_a_restore != RestoreStart::NotPartOfOne;
                // The page has been showing this tab as starting; give it the real projection at
                // once rather than making it wait for the first event.
                match state.shown_in(tab) {
                    Some(view) if view.is_base() || state.ready(view) => view_payloads(state, view, out),
                    _ => {}
                }
                if !restored {
                    match first_turn {
                        Some(turn) => send_first_turn(state, screen, tab, &request_id, turn, out),
                        None => send_start_answer(
                            state,
                            tab,
                            &request_id,
                            serialize_command_result_for_js(&request_id, Ok(())),
                            out,
                        ),
                    }
                }
            }
            StartCollected::Failed { tab, request_id, error } => {
                eprintln!(
                    "[agent_panel] tab {}: backend failed to start: {}",
                    tab.0, error.message
                );
                // A tab a restore started is reported with the rest of the restore, and was tidied
                // away by `account_for_restore`, rather than left on screen as a dead tab each.
                if let RestoreStart::Failed { was_active } = part_of_a_restore {
                    if was_active {
                        send_switch(state, screen, out);
                    }
                    continue;
                }
                send_start_answer(
                    state,
                    tab,
                    &request_id,
                    serialize_command_result_for_js(&request_id, Err(&error.message)),
                    out,
                );
                // Only if that tab is on screen; it keeps the reason for later.
                if let Some(view) = state.shown_in(tab) {
                    out.send(view, serialize_error_for_js(tab, &error.message));
                }
            }
        }
    }
    // A start that ended another way (its tab closed, or was reset into a new one) owes nothing. A first
    // message held for the presence is still owed to the page it came from, which may no longer show the
    // tab.
    let tabs = &state.tabs;
    state.owed.retain(|_, answer| match answer.what {
        Owed::StartPage(tab) => {
            tabs.send_is_held(tab)
                || tabs
                    .get(tab)
                    .is_some_and(|t| matches!(t.backend(), TabBackend::Starting(_) | TabBackend::AwaitingTrust(_)))
        }
        _ => true,
    });
    finish_restore(state, out);
}

/// Owes `payload`, the answer owed under `request_id` when `tab`'s start ended, to the page that asked
/// for the start, unless that page has closed.
pub(super) fn send_start_answer(
    state: &mut AgentPanelState,
    tab: TabId,
    request_id: &str,
    payload: String,
    out: &mut Outbox,
) {
    let view = start_answer_dest(state, tab, request_id).map(|asked_from| dest_view(state, asked_from));
    if let Some(view) = view {
        out.send(view, payload);
    }
}

/// Owes `payload` to the page of `tab`'s view as it is now: a command's answer, or a notice about the
/// tab, wherever it is in that view.
pub(super) fn send_to_tabs_page(state: &AgentPanelState, tab: TabId, payload: String, out: &mut Outbox) {
    out.send(state.tabs.view_of(tab), payload);
}

/// The first turn of a session started by it, sent once its backend is installed. Its
/// `command_result` is the one the `send_message` that started the session was owed.
pub(super) fn send_first_turn(
    state: &mut AgentPanelState,
    screen: &Screen,
    tab: TabId,
    request_id: &str,
    turn: FirstTurn,
    out: &mut Outbox,
) {
    // Through the tab set, which puts the tab in the running set before anything goes out; in a linked
    // window the message may wait there for the presence, and the tick reports it then.
    if let Some(sent) = state.tabs.send_first_turn(tab, request_id, turn) {
        apply_sent(state, screen, sent, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    type Connected = Rc<RefCell<Vec<(Option<String>, ProjectTrust, Option<agent::ClientToolSet>)>>>;

    /// A panel whose spawner records each fresh start or resume by hand it is asked for, with the tools it was given.
    fn recording_panel() -> (Rc<RefCell<AgentPanelState>>, Connected) {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let connected: Connected = Rc::default();
        let record = connected.clone();
        state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
            match call {
                SpawnCall::Connect {
                    resume,
                    project,
                    client_tools,
                } => record.borrow_mut().push((resume, project, client_tools)),
                _ => panic!("only a connect is expected here"),
            }
            mpsc::channel().1
        })));
        (state, connected)
    }

    fn what_is_on(project: ProjectTrust) -> agent::ClientToolSet {
        eitri_core::editor_tools::catalog::client_tool_set(
            &eitri_core::editor_tools::switches::Switches::on_by_default(),
            project,
        )
        .expect("the catalogue declares tools")
    }

    fn resume_of(session: &str) -> DeferredStart {
        DeferredStart::Resume {
            request_id: "r1".into(),
            provider_session_id: session.into(),
            resumed_title: None,
            resumed_name: None,
        }
    }

    #[test]
    fn a_first_send_declares_the_tools() {
        let (state, connected) = recording_panel();
        let tab = state.borrow().tabs.active();
        start_deferred(
            &mut state.borrow_mut(),
            tab,
            first_send("r1", "hi"),
            ProjectTrust::Trusted,
        );
        let connected = connected.borrow();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].0, None);
        assert_eq!(connected[0].2, Some(what_is_on(ProjectTrust::Trusted)));
        assert_eq!(connected[0].2.as_ref().unwrap().server_name, "eitri");
    }

    #[test]
    fn a_resume_by_hand_declares_them() {
        let (state, connected) = recording_panel();
        let tab = state.borrow().tabs.active();
        start_deferred(&mut state.borrow_mut(), tab, resume_of("s-1"), ProjectTrust::Trusted);
        let connected = connected.borrow();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].0.as_deref(), Some("s-1"));
        assert_eq!(connected[0].2, Some(what_is_on(ProjectTrust::Trusted)));
    }

    /// A session of an untrusted project is given the rows that may be in one, and the same code path gives them.
    #[test]
    fn an_n_start_declares_what_an_n_start_may_have() {
        use eitri_core::editor_tools::{catalog, switches::Switches};
        let (state, connected) = recording_panel();
        let tab = state.borrow().tabs.active();
        start_deferred(
            &mut state.borrow_mut(),
            tab,
            first_send("r1", "hi"),
            ProjectTrust::Untrusted,
        );
        let second = state.borrow_mut().tabs.open(AgentId::Claude);
        start_deferred(
            &mut state.borrow_mut(),
            second,
            resume_of("s-2"),
            ProjectTrust::Untrusted,
        );
        let expected: Vec<String> = catalog::declared(&Switches::on_by_default(), ProjectTrust::Untrusted)
            .iter()
            .map(|row| row.name.to_owned())
            .collect();
        assert!(!expected.is_empty());
        let connected = connected.borrow();
        assert_eq!(connected.len(), 2);
        for (_, project, tools) in connected.iter() {
            assert_eq!(*project, ProjectTrust::Untrusted);
            let names: Vec<String> = tools.as_ref().unwrap().tools.iter().map(|t| t.name.clone()).collect();
            assert_eq!(names, expected);
        }
    }

    #[test]
    fn tools_off_declares_nothing() {
        use eitri_core::editor_tools::switches::Switches;
        let (state, connected) = recording_panel();
        state.borrow_mut().editor_switches = Switches::parse(Some("off"), &[]).unwrap();
        let tab = state.borrow().tabs.active();
        start_deferred(
            &mut state.borrow_mut(),
            tab,
            first_send("r1", "hi"),
            ProjectTrust::Trusted,
        );
        let second = state.borrow_mut().tabs.open(AgentId::Claude);
        start_deferred(&mut state.borrow_mut(), second, resume_of("s-2"), ProjectTrust::Trusted);
        let connected = connected.borrow();
        assert_eq!(connected.len(), 2);
        assert!(connected.iter().all(|(_, _, tools)| tools.is_none()));
    }

    /// Pins `SIDECAR_STOPPED_MARKER` against the Rust that actually produces it, the same way
    /// `agent-ui/web/src/problems.test.ts`'s own "matches watch.rs's own wording" test pins the
    /// frontend's copy: if `stream_ended_early_reason`'s opening sentence ever drifts, this fails
    /// loudly instead of `never_opened_message` silently going back to guessing on every stopped
    /// sidecar. The marker sits entirely on `stream_ended_early_reason`'s first source line, before
    /// its `\`-continuation, so no line-join normalisation is needed here (contrast the vitest
    /// test, which normalises defensively).
    #[test]
    fn the_sidecar_stopped_marker_is_pinned_to_watch_rs() {
        let watch_rs = include_str!("../../agent/src/providers/claude_sidecar/watch.rs");
        assert!(
            watch_rs.contains(SIDECAR_STOPPED_MARKER),
            "watch.rs's stream_ended_early_reason no longer contains {SIDECAR_STOPPED_MARKER:?}"
        );
    }

    /// A `reason` naming a stopped sidecar must not carry the "most likely no longer
    /// exists" guess, which is wrong for it -- the conversation and its transcript are fine.
    #[test]
    fn a_stopped_sidecar_reason_gets_no_never_opened_guess() {
        let reason = format!(
            "{SIDECAR_STOPPED_MARKER}, so anything after this point never arrived and the reply \
             above may be incomplete (stream closed)"
        );
        let message = never_opened_message(&reason);
        assert!(message.contains("the agent sidecar stopped"), "{message}");
        assert!(message.contains(&reason), "{message}");
        assert!(!message.contains("most likely no longer exists"), "{message}");
        assert!(!message.contains("start a new session instead"), "{message}");
    }

    /// The unwrapped counterpart: every other `reason` keeps the generic guess unchanged, so a
    /// session that really never opened still tells the reader to start over.
    #[test]
    fn a_generic_reason_keeps_the_never_opened_guess() {
        let reason = "provider process exited unexpectedly";
        let message = never_opened_message(reason);
        assert!(message.contains(reason), "{message}");
        assert!(message.contains("most likely no longer exists"), "{message}");
        assert!(message.contains("start a new session instead"), "{message}");
    }

    /// Outside `start_deferred` (and the connect workers it calls), nothing in the panel begins a
    /// session: no other code calls the spawners or `start_restore`, sets a tab connecting, or
    /// starts a backend. Another agent's tab is started by `start_external_tab`, which only
    /// `start_deferred` calls, through `spawn_external`, the one worker that calls
    /// `AgentBackend::start_external`. Read as text, formatting-insensitive.
    #[test]
    fn every_session_start_goes_through_the_trust_gate() {
        let shell = eitri_core::source_scan::rust_sources(&["shell/src"]);
        only_start_deferred_starts_a_session(&panel_sources(), &shell);
    }

    /// The smallest panel that starts its sessions where it should, for the scan's own test.
    const STARTS_ONLY_IN_THE_GATE: &str = "\
        fn start_deferred(tab: T) { spawn_connect(a); resume_spawner(b); start_restore(c); start_external_tab(d); \
            tabs.start(tab, PendingStart { x }); }\n\
        fn start_external_tab() { trust_plan(); spawn_external(); }\n\
        fn spawn_external() { AgentBackend::start_external(x); }\n\
        fn spawn_connect() {}\n\
        fn resume_spawner() {}\n\
        fn spawn_connect_holding() {}\n";

    #[test]
    fn a_session_started_in_a_second_file_of_the_panel_fails_the_scan() {
        let clean = fixture(&[
            ("panel/src/a.rs", STARTS_ONLY_IN_THE_GATE),
            ("panel/src/b.rs", "fn other() {}"),
        ]);
        assert_eq!(scan_failure(|| only_start_deferred_starts_a_session(&clean, &[])), None);
        let read = fixture(&[
            ("panel/src/a.rs", STARTS_ONLY_IN_THE_GATE),
            ("panel/src/b.rs", "fn restore_later() { spawn_connect(x); }"),
        ]);
        let why = scan_failure(|| only_start_deferred_starts_a_session(&read, &[])).expect("the second file is read");
        assert!(why.contains("spawn_connect is called outside start_deferred"), "{why}");
    }

    /// Panics unless the panel's code (`panel`, read as a whole) begins a session only in `start_deferred`
    /// and the workers it calls, and no file of `others` begins one at all.
    fn only_start_deferred_starts_a_session(
        panel: &[eitri_core::source_scan::Source],
        others: &[eitri_core::source_scan::Source],
    ) {
        use eitri_core::source_scan as scan;
        const SPAWNERS: &[&str] = &[
            "spawn_connect",
            "spawn_connect_holding",
            "resume_spawner",
            "start_restore",
            "spawn_external",
            "start_external_tab",
            "start_external",
            "start_external_with",
        ];
        let code = production_code(panel);
        let gate = scan::functions(&code, "start_deferred");
        assert_eq!(gate.len(), 1, "start_deferred not found");
        for spawner in ["spawn_connect", "resume_spawner", "start_restore", "start_external_tab"] {
            assert!(
                scan::calls(gate[0], spawner) > 0,
                "start_deferred no longer calls {spawner}; the scan would prove nothing"
            );
        }
        // A tab is set connecting only by `TabSet::start`, and only with a connect built here.
        let flat_gate: String = gate[0].split_whitespace().collect();
        assert!(flat_gate.contains("tabs.start(tab,PendingStart{"));
        let external = scan::functions(&code, "start_external_tab");
        assert_eq!(external.len(), 1, "start_external_tab not found");
        assert!(
            scan::calls(external[0], "spawn_external") > 0,
            "start_external_tab no longer calls spawn_external; the scan would prove nothing"
        );
        assert!(
            scan::calls(external[0], "trust_plan") > 0,
            "an external start no longer builds its trust plan"
        );
        let worker = scan::functions(&code, "spawn_external");
        assert_eq!(worker.len(), 1, "spawn_external not found");
        let flat_worker: String = worker[0].split_whitespace().collect();
        assert!(
            flat_worker.contains("AgentBackend::start_external("),
            "spawn_external no longer starts the backend; the scan would prove nothing"
        );
        // Of the two, only `start_deferred` may call `start_external_tab`, and only it may call
        // `spawn_external`.
        let without_gate = scan::without(&code, &[gate[0]]);
        assert_eq!(
            scan::calls(&without_gate, "start_external_tab"),
            0,
            "start_external_tab is called outside start_deferred"
        );
        let without_external = scan::without(&code, &[external[0]]);
        assert_eq!(
            scan::calls(&without_external, "spawn_external"),
            0,
            "spawn_external is called outside start_external_tab"
        );

        let mut cut: Vec<&str> = gate;
        cut.extend(external);
        cut.extend(worker);
        for worker in ["spawn_connect", "resume_spawner", "spawn_connect_holding"] {
            let found = scan::functions(&code, worker);
            assert_eq!(found.len(), 1, "{worker} not found");
            cut.extend(found);
        }
        let rest = scan::without(&code, &cut);
        for name in SPAWNERS {
            assert_eq!(scan::calls(&rest, name), 0, "{name} is called outside start_deferred");
        }
        let flat: String = rest.split_whitespace().collect();
        assert!(
            !flat.contains("PendingStart{"),
            "a tab is set connecting outside start_deferred"
        );
        assert!(
            !flat.contains("AgentBackend::start"),
            "a backend is started outside the connect worker"
        );

        // No file outside the panel starts a session either.
        for other in others {
            let code = scan::code_only(&other.text);
            let rest = scan::without(&code, &scan::modules(&code, "tests"));
            for name in SPAWNERS {
                assert_eq!(scan::calls(&rest, name), 0, "{} calls {name}", other.path);
            }
            let flat: String = rest.split_whitespace().collect();
            assert!(!flat.contains("PendingStart{"), "{} sets a tab connecting", other.path);
            assert!(!flat.contains("AgentBackend::start"), "{} starts a backend", other.path);
        }
    }

    /// A stand-in `codex` that prints `version` for `--version`, in a fresh scratch directory.
    fn fake_codex(label: &str, version: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = agent::state_dirs::test_workspace_dir(label);
        let program = dir.join("codex");
        std::fs::write(&program, format!("#!/bin/sh\necho 'codex-cli {version}'\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, program)
    }

    /// The band says the self-check is running exactly when the start is about to run it: a binary with no
    /// passing verdict kept. A kept verdict, a binary too old to check, or no state directory say nothing.
    #[test]
    fn a_first_codex_start_says_it_is_checking_this_codex() {
        let (dir, program) = fake_codex("codex-check-note", "0.161.0");
        let state_home = dir.join("state");
        assert!(codex_check_pending(&program, Some(&state_home)));
        assert!(!codex_check_pending(&program, None));
        let key = agent::providers::codex::BinaryKey::of(&program).unwrap();
        let verdict = state_home.join(CODEX_CHECK_DIR).join(key.id()).join("verdict.json");
        std::fs::create_dir_all(verdict.parent().unwrap()).unwrap();
        std::fs::write(&verdict, b"{}").unwrap();
        assert!(
            !codex_check_pending(&program, Some(&state_home)),
            "checked once is checked"
        );

        let (old_dir, old) = fake_codex("codex-check-note-old", "0.100.0");
        assert!(!codex_check_pending(&old, Some(&old_dir.join("state"))));

        let notes = StartNotes::default();
        notes.sender().send(CHECKING_CODEX.to_string()).unwrap();
        let sent = notes.drain();
        assert_eq!(sent.len(), 1);
        assert_eq!(json_of(&sent[0])["kind"], "notice");
        assert_eq!(
            json_of(&sent[0])["text"],
            "checking this Codex\u{2026} (once for this binary, a few seconds)"
        );
        assert!(notes.drain().is_empty());
    }

    /// The band's guess reads the self-check's own verdict file: the two names must stay the same.
    #[test]
    fn the_check_note_reads_where_the_self_check_writes() {
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/src/providers/codex/self_check.rs"),
        )
        .unwrap();
        assert!(source.contains(&format!("const CHECK_DIR: &str = \"{CODEX_CHECK_DIR}\";")));
        assert!(source.contains("dir.join(\"verdict.json\")"));
    }

    /// A first message sent in the base page, its tab broken out to view 3 while it waits and then
    /// starting there: the start's failure is answered in the base page, which holds the request and
    /// the text, and page 3 (which shows the tab now) is told the tab's error.
    #[test]
    fn a_start_is_answered_in_the_page_it_was_asked_from_after_its_tab_moved() {
        let TrustViews {
            handle,
            base,
            fx: _fx,
            hold: _hold,
            ..
        } = trust_views("views-start-moves");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                first.0
            ),
        );
        poll_trust_until(&handle, "the question in the base page", || {
            !prompts(&base.sent.borrow()).is_empty()
        });
        assert_eq!(handle.break_out(view(3)), Ok(first));
        poll_trust_until(&handle, "the question in page 3", || {
            !prompts(&third.sent.borrow()).is_empty()
        });
        let prompt = prompts(&third.sent.borrow()).pop().unwrap();
        assert_eq!(prompt.tab, first);
        clear(&[&base, &third]);
        trust_answer_from(&handle, view(3), "a1", &prompt, true);
        assert_eq!(result_for(&third.sent.borrow(), "a1"), Some(Ok(())));
        // The fake connect's channel is closed: the start fails on the tick that collects it.
        tick_handle_until(&handle, "the start's answer", || {
            result_for(&base.sent.borrow(), "s1").is_some()
        });
        assert!(matches!(result_for(&base.sent.borrow(), "s1"), Some(Err(_))));
        assert_eq!(result_for(&third.sent.borrow(), "s1"), None);
        assert!(sent_about(&third, "error", first), "{:?}", third.sent.borrow());
        assert!(handle.state.borrow().owed.is_empty());
    }

    /// A first message sent in the base page, its tab broken out to view 3 while it waits for the trust
    /// answer, then closed there: the cancellation goes to the base page, which holds the request and
    /// reports it with the message's text, not to page 3, which never sent it.
    #[test]
    fn a_closed_tabs_start_is_refused_in_the_page_it_was_asked_from_after_its_tab_moved() {
        let TrustViews {
            handle,
            base,
            fx: _fx,
            hold: _hold,
            ..
        } = trust_views("views-start-closed");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                first.0
            ),
        );
        poll_trust_until(&handle, "the question in the base page", || {
            !prompts(&base.sent.borrow()).is_empty()
        });
        assert_eq!(handle.break_out(view(3)), Ok(first));
        clear(&[&base, &third]);
        assert_eq!(handle.close_view_tabs(view(3)), Ok(1));
        assert_eq!(
            result_for(&base.sent.borrow(), "s1"),
            Some(Err(TAB_CLOSED_MESSAGE.to_string()))
        );
        assert_eq!(result_for(&third.sent.borrow(), "s1"), None);
        assert!(handle.state.borrow().owed.is_empty());
    }

    /// A terminal handoff asked from the base page, its tab broken out to view 2 before the close
    /// finished: page 2, which shows the tab, is sent the card (or the failed close's error), and the
    /// base page, which holds the request and shows the tab as closing until it is answered, gets the
    /// answer.
    #[test]
    fn a_handoff_is_answered_in_the_page_it_was_asked_from_after_its_tab_moved() {
        agent::state_dirs::redirect_state_to_a_test_root();
        for failed in [false, true] {
            let (handle, base, second) = handle_with_two_pages();
            let moved = handle.state.borrow_mut().tabs.open(AgentId::Claude);
            let (closed_tx, closed_rx) = mpsc::channel();
            let (_dropped_tx, dropped_rx) = mpsc::channel();
            handle
                .state
                .borrow_mut()
                .owed
                .owe(ViewId::BASE, "h1", Owed::Handoff(moved));
            handle.state.borrow_mut().tabs.get_mut(moved).unwrap().pending_handoff = Some(PendingHandoff {
                request_id: "h1".into(),
                command: a_command(),
                closed_rx,
                dropped_rx,
            });
            assert_eq!(handle.break_out(view(2)), Ok(moved));
            clear(&[&base, &second]);
            if failed {
                drop(closed_tx);
            } else {
                closed_tx.send(()).unwrap();
            }
            tick_handle_until(&handle, "the handoff's answer", || {
                result_for(&base.sent.borrow(), "h1").is_some()
            });
            let answer = result_for(&base.sent.borrow(), "h1").unwrap();
            assert_eq!(answer.is_ok(), !failed, "{answer:?}");
            assert_eq!(result_for(&second.sent.borrow(), "h1"), None);
            let card = if failed { "error" } else { "handoff" };
            assert!(sent_about(&second, card, moved), "{:?}", second.sent.borrow());
            assert!(!sent_about(&base, card, moved), "{:?}", base.sent.borrow());
            assert!(handle.state.borrow().tabs.get(moved).unwrap().pending_handoff.is_none());
            assert!(handle.state.borrow().owed.is_empty(), "the answer is spent");
        }
    }
}
