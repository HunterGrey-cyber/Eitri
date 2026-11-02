//! Restoring the last session's tabs: the offer, the bypass question, the run that resumes each saved
//! session, and the saved-tabs file kept in step with the tabs.

use super::*;

/// Whether the launch dashboard may offer to bring the last window's tabs back at all: the policy is
/// to offer, there is something saved, no tab here has started a session and no restore is running.
pub(super) fn restore_is_on_offer(state: &AgentPanelState) -> bool {
    state.restore_policy == RestorePolicy::Offer
        && state.restore_source.is_some()
        && state.restore_run.is_none()
        && state.tabs.is_pristine()
}

/// Whether the last window's tabs are settled ([`AgentPanelHandle::restore_settled`]). `agent.restore
/// = "auto"` keeps its source until the base page's first `ready` uses it up.
pub(super) fn restore_settled(state: &AgentPanelState) -> bool {
    let on_offer = restore_is_on_offer(state) || launch_restore_waits(state);
    !on_offer && !state.tabs.restore_question_open() && !restore_waits_for_trust(state) && state.restore_run.is_none()
}

/// Whether `agent.restore = "auto"` has its go still to come: the source waits for the base page's first `ready`, or
/// the launch is held.
pub(super) fn launch_restore_waits(state: &AgentPanelState) -> bool {
    state.restore_policy == RestorePolicy::Auto
        && ((state.restore_source.is_some() && !state.ready(ViewId::BASE))
            || state.launch_restore_held
            || state.launch_restore_owed)
}

/// What the dashboard says it can bring back: the saved tabs that still can be, `None` when that is
/// none (a key that restores nothing is never offered).
pub(super) fn restore_offer(state: &AgentPanelState) -> Option<RestoreOffer> {
    if !restore_is_on_offer(state) {
        return None;
    }
    restore_plan(state).offer()
}

/// Which of the saved tabs can come back, judged against the records and leases as they are now.
pub(super) fn restore_plan(state: &AgentPanelState) -> eitri_core::tab_restore::RestorePlan {
    plan_of(state, &state.restore_source.clone().unwrap_or_default())
}

/// Which of `saved` can come back, judged against the records and leases as they are now.
pub(super) fn plan_of(state: &AgentPanelState, saved: &SavedTabs) -> eitri_core::tab_restore::RestorePlan {
    let canonical = state.canonical_project_dir.as_str();
    // Read here rather than taken from a greeting: the greeting lists the active tab's agent's
    // records, and the saved Claude tabs are matched with Claude's whichever agent that tab runs. The
    // other agents' records are read beside them, each saved tab being matched only with a record its
    // own agent wrote.
    let mut records = BackendGreeting::claude_sessions_titled(
        state.backend_kind,
        &state.project_dir,
        eitri_core::agent_backend::Titles::Cached(&state.titles),
    );
    if saved.has_other_agents() {
        records.extend(eitri_core::agent_backend::external_sessions(&state.project_dir));
    }
    let mut plan = eitri_core::tab_restore::plan(saved, &records, &state.tabs.open_sessions(), |agent, id| {
        agent::lease::SessionLease::is_held(&agent.record_name(), canonical, id).unwrap_or(false)
    });
    // A saved tab of an ACP agent `init.lua` no longer registers has no command to run: it is skipped
    // and said to be, and the next save of the open tabs leaves it out of the file.
    plan.skip_unregistered(|agent| match agent {
        AgentId::Acp(name) => state.acp_agents.iter().any(|a| a.name == *name),
        AgentId::Claude | AgentId::Codex => true,
    });
    plan
}

/// Where a restore was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestoreAsk {
    /// `s` on the launch dashboard: a saved bypass tab is asked about first.
    Dashboard,
    /// `agent.restore = "auto"`: nobody pressed a key, so nothing can be asked.
    Launch,
}

/// What asking for, or answering, a restore did.
pub(super) enum RestoreResult {
    /// Why nothing was started.
    Refused(String),
    /// A saved tab was in bypass; the page shows the question and answers it.
    Question(RestorePrompt),
    /// Every saved tab that could be has been started or skipped. `notice` is what the band says.
    Started { notice: Option<String> },
    /// Cleared to start, and waiting for the trust question: `start_deferred` starts it.
    Deferred,
}

/// Begins one resume: given a session and the lease already taken for it, the receiver its result
/// will arrive on.
pub(super) type SpawnResume<'a> = &'a mut dyn FnMut(
    &str,
    agent::lease::SessionLease,
    ProjectTrust,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>>;

/// A restore's resumes, as the real workers start them, each with `client_tools`. Only
/// [`start_deferred`] calls it.
pub(super) fn resume_spawner(
    state: &AgentPanelState,
    client_tools: Option<agent::ClientToolSet>,
) -> impl FnMut(&str, agent::lease::SessionLease, ProjectTrust) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (kind, project_dir) = (state.backend_kind, state.project_dir.clone());
    move |id, lease, project| {
        spawn_connect_holding(
            kind,
            project_dir.clone(),
            Some(id.to_string()),
            Some(lease),
            project,
            client_tools.clone(),
        )
    }
}

/// Asks for the last window's tabs back, into `from` (the empty tab the key was pressed in) and new
/// tabs. Nothing is started unless this window has started nothing yet; what is cleared waits for the
/// trust question before anything is leased or started.
pub(super) fn ask_for_restore(state: &mut AgentPanelState, from: TabId, ask: RestoreAsk) -> RestoreResult {
    // Another agent's saved tab does not need the sidecar; a window with only Claude tabs saved does.
    let saved_external = state.restore_source.as_ref().is_some_and(SavedTabs::has_other_agents);
    if state.backend_kind != BackendKind::Sidecar && !saved_external {
        return RestoreResult::Refused("only the sidecar backend can resume sessions".to_string());
    }
    if state.restore_run.is_some() {
        return RestoreResult::Refused("the last session's tabs are already being restored".to_string());
    }
    if restore_waits_for_trust(state) {
        return RestoreResult::Refused("the last session's tabs are waiting for the trust question".to_string());
    }
    if !state.tabs.is_pristine() {
        return RestoreResult::Refused(
            "the last session's tabs can be restored only before a session has started".to_string(),
        );
    }
    if state.restore_source.is_none() {
        return RestoreResult::Refused("there are no saved tabs to restore".to_string());
    }
    let plan = restore_plan(state);
    // A launch has one go at it, whatever comes of it: a reload of the page must not try again.
    if ask == RestoreAsk::Launch {
        state.restore_source = None;
    }
    let policy = if state.bypass_by_config {
        BypassPolicy::Keep
    } else if ask == RestoreAsk::Launch {
        BypassPolicy::Downgrade
    } else {
        BypassPolicy::Ask
    };
    match state.tabs.begin_restore(plan, policy) {
        RestoreStep::Confirm(prompt) => RestoreResult::Question(prompt),
        RestoreStep::Go(go) => gate_restore(state, from, go),
    }
}

/// Whether a cleared restore is holding a tab while the trust question is asked.
pub(super) fn restore_waits_for_trust(state: &AgentPanelState) -> bool {
    state.tabs.tabs().iter().any(|t| {
        matches!(
            t.backend(),
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { .. },
                ..
            })
        )
    })
}

/// `y` or `n` to the question [`ask_for_restore`] raised.
pub(super) fn answer_restore_question(state: &mut AgentPanelState, nonce: u64, keep_bypass: bool) -> RestoreResult {
    match state.tabs.answer_restore(nonce, keep_bypass) {
        Err(why) => RestoreResult::Refused(why),
        Ok(go) => {
            let from = state.tabs.active();
            gate_restore(state, from, go)
        }
    }
}

/// A restore the bypass question (if any) has cleared waits in `from` for the trust question. It holds
/// no lease while it waits: those are taken when it starts, so a question left unanswered blocks no
/// other window.
pub(super) fn gate_restore(state: &mut AgentPanelState, from: TabId, mut go: RestoreGo) -> RestoreResult {
    // Still held by the window (a dashboard restore): put off while the window saves nothing else, it stays on offer.
    go.reoffer = state.restore_source.is_some();
    match gate_start(state, from, DeferredStart::Restore { go }) {
        Ok(()) => RestoreResult::Deferred,
        Err(why) => RestoreResult::Refused(why),
    }
}

/// Takes every lease first and only then starts the resumes, so that nothing -- a record pruned to
/// make room for another session, another window -- can claim a saved session between the decision
/// and its own resume. A session whose lease cannot be taken is skipped with the reason. Every resumed
/// tab is given `project`: they all share the window's root, so one answer covers them.
pub(super) fn start_restore(
    state: &mut AgentPanelState,
    from: TabId,
    go: RestoreGo,
    spawn: SpawnResume<'_>,
    project: ProjectTrust,
) -> RestoreResult {
    let canonical = state.canonical_project_dir.clone();
    // Only Claude tabs are started here, on this answer. Another agent's tab comes back holding its
    // resume and waits for its own agent's trust answer, which starts it as a resume by hand; its
    // lease is taken when it starts, so none is taken here.
    let mut leases: std::collections::HashMap<String, Result<agent::lease::SessionLease, String>> = go
        .sessions()
        .into_iter()
        .filter(|(agent, _)| *agent == AgentId::Claude)
        .map(|(_, id)| {
            let lease = match agent::lease::SessionLease::try_acquire("claude", &canonical, &id) {
                Ok(lease) => Ok(lease),
                Err(agent::lease::LeaseError::AlreadyHeld) => Err(eitri_core::tab_restore::HELD_ELSEWHERE.to_string()),
                Err(other) => Err(format!("its session lease could not be taken ({other})")),
            };
            (id, lease)
        })
        .collect();
    let (downgraded, asked) = (go.downgraded, go.asked);
    state.restore_cut_note = eitri_core::tab_restore::cut_turns_note(go.cut_turns());
    let run = state.tabs.start_restore(from, go, |agent, id| {
        if agent.is_external() {
            return Ok(RestoreConnect::AwaitTrust);
        }
        match leases.remove(id) {
            Some(Ok(lease)) => Ok(RestoreConnect::Started(spawn(id, lease, project))),
            Some(Err(reason)) => Err(reason),
            None => Err("it was listed twice".to_string()),
        }
    });
    // Each waiting tab is checked for its own agent now; the tick asks the question once the tab is
    // on screen, or starts it at once when the window already has that agent's answer.
    for &tab in run.awaiting_trust() {
        let profile = waiting_profile(state, tab);
        state.trust.wait_for(tab, &profile);
        state.trust.begin_check(&profile);
    }
    // The tabs this restore just set connecting are the only starting ones without their tiers:
    // every other start names them as it begins.
    for tab in state.tabs.tabs().iter().map(|t| t.id).collect::<Vec<_>>() {
        if let Some(t) = state.tabs.get_mut(tab) {
            if matches!(t.backend(), TabBackend::Starting(_)) && t.session.project_tiers.is_none() {
                t.session.project_tiers = Some(project);
            }
        }
    }
    state.restore_source = None;
    state.restore_run = Some(run);
    let notice = (downgraded > 0 && !asked).then(|| {
        let tabs = if downgraded == 1 { "tab" } else { "tabs" };
        format!("{downgraded} bypass {tabs} came back in auto; Shift+Tab then y switches")
    });
    RestoreResult::Started { notice }
}

/// Once every resume of a restore has returned, its one message: to the log and to the window's toast.
pub(super) fn finish_restore(state: &mut AgentPanelState, out: &mut Outbox) {
    let AgentPanelState { restore_run, tabs, .. } = &mut *state;
    // A tab the user closed while it was still connecting will never report: it is not waited for.
    if let Some(run) = restore_run.as_mut() {
        run.forget_closed(|tab| tabs.get(tab).is_some());
    }
    let outcome = state
        .restore_run
        .as_ref()
        .and_then(eitri_core::tab_restore::RestoreRun::outcome);
    let Some(outcome) = outcome else { return };
    state.restore_run = None;
    let mut message = outcome.message();
    if let Some(note) = state.restore_cut_note.take() {
        message = format!("{message}; {note}");
    }
    eprintln!("[restore] {message}");
    out.effect(Effect::Toast(message));
}

/// What a [`RestoreResult`] owes the base page, the only one that offers or asks about a restore;
/// `Err` is a refusal for the caller to report.
pub(super) fn deliver_restore(
    state: &mut AgentPanelState,
    screen: &Screen,
    result: RestoreResult,
    out: &mut Outbox,
) -> Result<(), String> {
    match result {
        RestoreResult::Refused(why) => Err(why),
        RestoreResult::Question(prompt) => {
            out.send(ViewId::BASE, serialize_confirm_restore_for_js(&prompt));
            Ok(())
        }
        RestoreResult::Started { notice } => {
            send_switch(state, screen, out);
            // A restored tab that came back into its extra view is shown there.
            send_extra_views(state, screen, out);
            if let Some(notice) = notice {
                out.send(ViewId::BASE, eitri_core::agent_bridge::serialize_notice_for_js(&notice));
            }
            finish_restore(state, out);
            Ok(())
        }
        // The tab shows as starting while the question is asked; the tick's trust events go on.
        RestoreResult::Deferred => {
            send_tabs(state, out);
            Ok(())
        }
    }
}

/// [`send_view`] for every live extra page.
pub(super) fn send_extra_views(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    for view in live_views(state) {
        if !view.is_base() {
            send_view(state, screen, view, out);
        }
    }
}

/// `agent.restore = "auto"`: on the base page's first `ready`, bring the last window's tabs back
/// with no key pressed. Used up on its first go, so a reload of the page does not do it again.
pub(super) fn restore_at_launch(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    let Some(result) = launch_restore(state) else {
        return;
    };
    if let Err(why) = deliver_restore(state, screen, result, out) {
        eprintln!("[restore] not restored: {why}");
    }
}

/// The state half of [`restore_at_launch`]: `None` when the launch restores nothing, or is held
/// ([`AgentPanelHandle::hold_launch_restore`]) and is owed until the hold is let go.
pub(super) fn launch_restore(state: &mut AgentPanelState) -> Option<RestoreResult> {
    if state.launch_restore_held {
        state.launch_restore_owed = true;
        return None;
    }
    state.launch_restore_owed = false;
    if state.restore_policy != RestorePolicy::Auto || state.restore_source.is_none() {
        return None;
    }
    let from = state.tabs.active();
    Some(ask_for_restore(state, from, RestoreAsk::Launch))
}

/// The launch restore the base page's `ready` owed while it was held, once the hold is let go: run on the
/// tick after.
pub(super) fn restore_once_released(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    if state.launch_restore_owed && !state.launch_restore_held && state.ready(ViewId::BASE) {
        restore_at_launch(state, screen, out);
    }
}

/// Tabs window 1 kept for this chat window, given at each link: they wait for the base page's first `ready` as a
/// launch's saved tabs do, or take their go at once when it has been. A window that has started something has its
/// tabs, as a launch restore finds them, and leaves these. So does a window that has told window 1 a set of its own and
/// closed it since: empty again it looks new, but what it is given dates from before that close.
pub(super) fn take_given_tabs(state: &mut AgentPanelState, screen: &Screen, tabs: SavedTabs, out: &mut Outbox) {
    if state.restore_policy == RestorePolicy::Off || tabs.is_empty() {
        return;
    }
    if !state.tabs.is_pristine() || state.restore_run.is_some() || state.group_tabs.told_its_own() {
        println!(
            "[restore] {} saved tab(s) given; this window has started, so they stay out",
            tabs.tabs.len()
        );
        return;
    }
    state.restore_source = Some(tabs);
    if state.ready(ViewId::BASE) {
        restore_at_launch(state, screen, out);
    }
}

/// The tabs a restore still holds that the user has said nothing about. A source a user session passed by is out:
/// the window's next write leaves those tabs out, as `eitri` mode's does.
pub(super) fn unopened_tabs(state: &AgentPanelState) -> Option<SavedTabs> {
    let waiting = restore_is_on_offer(state) || launch_restore_waits(state) || state.group_tabs.holds_unopened;
    state.restore_source.clone().filter(|_| waiting)
}

/// See [`AgentPanelHandle::tabs_to_keep`].
pub(super) fn tabs_to_keep(state: &AgentPanelState) -> Option<SavedTabs> {
    let now = state.tabs.saved_snapshot(&state.conversation_id);
    let tabs = match (now, unopened_tabs(state)) {
        (Some(now), Some(unopened)) => now.followed_by(unopened),
        (now, unopened) => now.or(unopened)?,
    };
    (state.group_tabs.told || !tabs.is_empty()).then_some(tabs)
}

/// Keeps the saved-tabs file in step with the tabs: once per tick, cheap when nothing changed. A change is
/// handed to the writer thread (`TabWriter`), or told to window 1 by a window of a group; nothing here touches the
/// disk.
pub(super) fn remember_tabs(state_ref: &mut AgentPanelState) {
    // A dashboard restore still waiting is listed from the first write that would otherwise leave it out.
    if state_ref.tabs.settle_waiting_restore() {
        state_ref.restore_source = None;
    }
    let snapshot = state_ref.tabs.saved_snapshot(&state_ref.conversation_id);
    if state_ref.restore_policy != RestorePolicy::Off {
        let AgentPanelState {
            group_tabs,
            restore_source,
            ..
        } = &mut *state_ref;
        group_tabs.observe(snapshot.as_ref(), restore_source.as_ref());
    }
    state_ref.tab_memory.observe(snapshot);
}

/// Where a chat window of a group tells its tabs.
type TabReportHook = Rc<dyn Fn(&SavedTabs)>;

/// This window's part in a group's one tabs file, whichever side of it this window is on.
#[derive(Default)]
pub(super) struct GroupTabs {
    /// A chat window: where its tabs are told, and the set last told. It starts from "no tabs", as the writer does, so
    /// a window that has done nothing says nothing and what window 1 keeps for it stands.
    report: Option<(TabReportHook, SavedTabs)>,
    /// A chat window has told window 1 a set, so what is its to save is known there from this window, not from the file.
    told: bool,
    /// A chat window: a tab handed over took a restore's place on the empty tab, so the tabs of that restore are neither
    /// open here nor on offer, and only what it reports keeps them.
    holds_unopened: bool,
    /// Window 1: the file's last write, which its close waits for as it waits for a window's own.
    pub(super) last_write: Option<mpsc::Receiver<()>>,
}

impl GroupTabs {
    pub(super) fn report_to(&mut self, hook: TabReportHook) {
        self.report = Some((hook, SavedTabs::default()));
    }

    pub(super) fn told_its_own(&self) -> bool {
        self.told
    }

    /// A tab handed over is about to take the empty tab the window's restore would have used.
    pub(super) fn hold_unopened(&mut self) {
        self.holds_unopened = true;
    }

    /// `unopened`: the tabs a restore still holds, which this window reports beside its own once a hand-over has taken
    /// their place.
    fn observe(&mut self, now: Option<&SavedTabs>, unopened: Option<&SavedTabs>) {
        let (Some(now), Some((hook, last))) = (now, self.report.as_mut()) else {
            return;
        };
        let now = match unopened.filter(|_| self.holds_unopened) {
            Some(unopened) => now.clone().followed_by(unopened.clone()),
            None => now.clone(),
        };
        if now != *last {
            hook(&now);
            *last = now;
            self.told = true;
        }
    }
}

/// How one collected connect relates to the restore in progress, if there is one.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RestoreStart {
    /// An ordinary connect (a resume by hand, a first message): handled as it always was.
    NotPartOfOne,
    Installed,
    /// A resume the restore started was refused. The tab is already tidied away; `was_active` says
    /// the page was showing it, so it must be told which tab it is showing now, or that the tab it
    /// shows took the refused tab's draft, which its composer must show before its next mirror.
    Failed {
        was_active: bool,
    },
}

/// Counts a collected connect into the restore that started it. A refused one is also taken off the
/// screen: an empty tab again if the restore began in it, gone if the restore made it.
pub(super) fn account_for_restore(state: &mut AgentPanelState, collected: &StartCollected) -> RestoreStart {
    let AgentPanelState {
        restore_run,
        tabs,
        emptied_views,
        ..
    } = state;
    let Some(run) = restore_run.as_mut() else {
        return RestoreStart::NotPartOfOne;
    };
    match collected {
        StartCollected::Installed { tab, .. } if run.owns(*tab) => {
            run.note_installed(*tab);
            RestoreStart::Installed
        }
        StartCollected::Failed { tab, error, .. } if run.owns(*tab) => {
            let back_to = run.back_to(*tab);
            run.note_failed(*tab, &error.message);
            let was_active = tabs.active() == *tab;
            let view = tabs.view_of(*tab);
            let shown_draft = tabs.get(tabs.active()).map(|t| t.draft().to_string());
            tabs.discard_failed_restore(*tab, back_to);
            let draft_moved = tabs.get(tabs.active()).map(|t| t.draft()) != shown_draft.as_deref();
            // A restored tab that came back into an extra view and is discarded leaves that view.
            if !view.is_base() && tabs.tab_in(view).is_none() {
                emptied_views.push(view);
            }
            // The tab on screen did not come back: the first that did (or is coming) takes its place,
            // rather than leaving an empty dashboard over tabs that restored.
            if was_active {
                if let Some(survivor) = run.first_survivor() {
                    tabs.select(survivor);
                }
            }
            RestoreStart::Failed {
                was_active: was_active || draft_moved,
            }
        }
        _ => RestoreStart::NotPartOfOne,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    /// A project directory with resumable records for `records` and a panel state whose last window
    /// left `saved` open.
    /// Whether a Claude session's lease is free, asked again for up to five seconds while it looks held.
    /// A lease just dropped can look held for a moment while another test thread starts a child process:
    /// the child has a copy of every descriptor of this process until it execs. One never let go is
    /// still held after the five seconds, so this checks "released within five seconds".
    fn lease_comes_free(canonical: &str, session: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while agent::lease::SessionLease::is_held("claude", canonical, session).unwrap() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn the_dashboard_offers_the_saved_tabs_that_can_still_come_back() {
        let (state, _dir) = restore_panel(
            "restore-offer",
            &[
                ("s-a", None, Auto),
                ("s-b", Some("api"), Bypass),
                ("s-gone", None, Auto),
            ],
            &["s-a", "s-b"],
        );
        let s = state.borrow();
        assert!(restore_is_on_offer(&s));
        let offer = restore_offer(&s).expect("two tabs can come back");
        assert_eq!(offer.labels, vec!["title of s-a".to_string(), "api".to_string()]);
        assert_eq!(offer.bypass, 1);
    }

    /// The greeting lists the active tab's agent's sessions, and a switch to a tab running another
    /// agent sends a new one though no session id changed; the restore offer still matches the saved
    /// Claude tabs with Claude's records whichever agent the active tab runs.
    #[test]
    fn hello_follows_the_active_tabs_agent_and_the_offer_keeps_the_claude_tabs() {
        let (state, dir) = restore_panel("hello-agent", &[("s-a", None, Auto)], &["s-a"]);
        agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
            conversation_id: agent::conversation_id_for_cwd(&dir),
            provider: AgentId::Codex.record_name(),
            provider_session_id: "thread-1".to_string(),
            canonical_cwd: dir.to_string_lossy().into_owned(),
            created_at: "1".to_string(),
            updated_at: "3".to_string(),
            provider_advertised_resume: true,
            title: None,
            name: None,
        })
        .unwrap();
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        let last_hello = || {
            let sent = page.sent.borrow();
            let hello = sent.iter().rev().find(|e| kind_of(e) == "hello").expect("a hello");
            let hello: serde_json::Value = serde_json::from_str(hello).unwrap();
            let ids: Vec<String> = hello["resumableSessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["providerSessionId"].as_str().unwrap().to_string())
                .collect();
            (ids, hello["restore"]["labels"].clone())
        };
        let hellos = || page.sent.borrow().iter().filter(|e| kind_of(e) == "hello").count();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert_eq!(
            last_hello(),
            (vec!["s-a".to_string()], serde_json::json!(["title of s-a"]))
        );

        let before = hellos();
        handle.run(send_hello_if_open_sessions_changed);
        assert_eq!(hellos(), before, "nothing changed, nothing is sent");

        let claude_tab = state.borrow().tabs.active();
        state.borrow_mut().tabs.open(AgentId::Codex);
        handle.run(send_hello_if_open_sessions_changed);
        assert_eq!(hellos(), before + 1);
        assert_eq!(
            last_hello(),
            (vec!["thread-1".to_string()], serde_json::json!(["title of s-a"])),
            "the Codex tab offers Codex's sessions, and the saved Claude tab can still come back"
        );

        state.borrow_mut().tabs.select(claude_tab);
        handle.run(send_hello_if_open_sessions_changed);
        assert_eq!(hellos(), before + 2);
        assert_eq!(
            last_hello(),
            (vec!["s-a".to_string()], serde_json::json!(["title of s-a"]))
        );
    }

    #[test]
    fn nothing_is_offered_after_a_session_started_under_another_policy_or_when_nothing_would_come_back() {
        let (state, _dir) = restore_panel("restore-not-offered", &[("s-a", None, Auto)], &["s-a"]);
        assert!(restore_offer(&state.borrow()).is_some());

        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        assert!(
            restore_offer(&state.borrow()).is_none(),
            "auto restores at launch, it does not offer"
        );
        state.borrow_mut().restore_policy = RestorePolicy::Off;
        assert!(restore_offer(&state.borrow()).is_none());
        state.borrow_mut().restore_policy = RestorePolicy::Offer;

        // A tab that has started a session: this is no longer a launch.
        let started_tab = state.borrow().tabs.active();
        let (_tx, rx) = mpsc::channel();
        state
            .borrow_mut()
            .tabs
            .get_mut(started_tab)
            .unwrap()
            .set_backend(TabBackend::Starting(PendingStart {
                request_id: "r".into(),
                result_rx: rx,
                first_turn: None,
                resume: None,
                resumed_title: None,
                resumed_name: None,
            }));
        assert!(restore_offer(&state.borrow()).is_none());
        state
            .borrow_mut()
            .tabs
            .get_mut(started_tab)
            .unwrap()
            .set_backend(TabBackend::NotStarted);
        assert!(restore_offer(&state.borrow()).is_some());

        // Every saved tab skipped: a key that restores nothing is not offered.
        let (nothing, _dir) = restore_panel("restore-nothing", &[("s-x", None, Auto)], &[]);
        assert!(restore_offer(&nothing.borrow()).is_none());
        // And no file, no offer.
        nothing.borrow_mut().restore_source = None;
        assert!(!restore_is_on_offer(&nothing.borrow()));
    }

    #[test]
    fn a_restore_declares_them() {
        let (state, _dir) = restore_panel(
            "restore-tools",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        let mut resumes = FakeResumes::default();
        assert_eq!(started(ask(&state, &mut resumes, RestoreAsk::Dashboard)), None);
        assert_eq!(resumes.asked, ["s-1", "s-2"]);
        let expected = eitri_core::editor_tools::catalog::client_tool_set(
            &eitri_core::editor_tools::switches::Switches::on_by_default(),
            ProjectTrust::NothingFound,
        );
        assert!(expected.is_some());
        assert_eq!(resumes.client_tools, [expected.clone(), expected]);
    }

    /// Every lease is held before the first resume is begun, so nothing can claim a saved session
    /// -- or prune its record -- in between.
    #[test]
    fn every_lease_is_taken_before_the_first_resume_begins() {
        let (state, dir) = restore_panel(
            "restore-leases",
            &[("s-1", None, Auto), ("s-2", None, Auto), ("s-3", None, Auto)],
            &["s-1", "s-2", "s-3"],
        );
        let canonical = dir.to_string_lossy().into_owned();
        let mut resumes = FakeResumes {
            all: vec!["s-1".into(), "s-2".into(), "s-3".into()],
            ..Default::default()
        };
        assert_eq!(started(ask(&state, &mut resumes, RestoreAsk::Dashboard)), None);
        assert_eq!(resumes.asked, ["s-1", "s-2", "s-3"], "in order");
        assert_eq!(resumes.held_when_first_asked, Some(vec![true, true, true]));
        assert_eq!(resumes.leases.len(), 3);
        assert_eq!(
            resumes.trusts,
            [ProjectTrust::NothingFound; 3],
            "no restored session loads the project's own configuration"
        );
        assert!(state.borrow().restore_source.is_none(), "used up");
        let numbers: Vec<Option<String>> = state
            .borrow()
            .tabs
            .tabs()
            .iter()
            .map(eitri_core::tab_set::Tab::provider_session_id)
            .collect();
        assert_eq!(numbers, ["s-1", "s-2", "s-3"].map(|s| Some(s.to_string())));
        drop(resumes.leases);
        assert!(lease_comes_free(&canonical, "s-1"), "dropping the leases lets them go");
    }

    #[test]
    fn a_session_another_window_takes_after_the_offer_is_skipped_and_said() {
        let (state, dir) = restore_panel(
            "restore-late-lease",
            &[("s-1", None, Bypass), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        let canonical = dir.to_string_lossy().into_owned();
        let mut resumes = FakeResumes::default();
        let RestoreResult::Question(prompt) = ask(&state, &mut resumes, RestoreAsk::Dashboard) else {
            panic!("a bypass tab is asked about")
        };
        // Another window opens s-2 while the question is on screen.
        let theirs = agent::lease::SessionLease::try_acquire("claude", &canonical, "s-2").unwrap();
        started(answer(&state, &mut resumes, prompt.nonce, true));
        assert_eq!(resumes.asked, ["s-1"], "only the one that could be leased was begun");

        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        stepped(&state, finish_restore);
        assert_eq!(
            *toasts.borrow(),
            ["Restored 1 of 2 tabs; 1 could not be: title of s-2 (open in another window)"]
        );
        drop(theirs);
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn a_saved_bypass_tab_asks_first_and_each_answer_has_its_own_result() {
        let (state, _dir) = restore_panel(
            "restore-bypass-ask",
            &[("s-1", None, Auto), ("s-2", None, Bypass)],
            &["s-1", "s-2"],
        );
        let mut resumes = FakeResumes::default();
        let RestoreResult::Question(prompt) = ask(&state, &mut resumes, RestoreAsk::Dashboard) else {
            panic!("asked")
        };
        assert_eq!(prompt.lines[0], "Restore 2 tabs (1 in bypass)? y/n");
        assert!(resumes.asked.is_empty(), "nothing begins until the answer");
        assert!(
            state.borrow().restore_source.is_some(),
            "and the offer is still there if it is cancelled"
        );
        assert!(matches!(
            answer(&state, &mut resumes, prompt.nonce + 1, true),
            RestoreResult::Refused(_)
        ));

        assert_eq!(
            started(answer(&state, &mut resumes, prompt.nonce, false)),
            None,
            "an answered n needs no second telling"
        );
        assert_eq!(mode(&state, "s-2"), Auto, "n: the tab comes back in auto");
        assert_eq!(state.borrow().tabs.default_mode(), Auto);

        let (yes, _dir) = restore_panel(
            "restore-bypass-yes",
            &[("s-1", None, Auto), ("s-2", None, Bypass)],
            &["s-1", "s-2"],
        );
        let mut resumes = FakeResumes::default();
        let RestoreResult::Question(prompt) = ask(&yes, &mut resumes, RestoreAsk::Dashboard) else {
            panic!("asked")
        };
        started(answer(&yes, &mut resumes, prompt.nonce, true));
        assert_eq!(mode(&yes, "s-2"), Bypass, "y: as it was");
        assert_eq!(mode(&yes, "s-1"), Auto);
        assert_eq!(
            yes.borrow().tabs.default_mode(),
            Auto,
            "the window's own default never moves"
        );
    }

    #[test]
    fn a_restore_at_launch_asks_nothing_downgrades_bypass_and_says_so() {
        let (state, _dir) = restore_panel(
            "restore-launch",
            &[("s-1", None, Bypass), ("s-2", None, Bypass)],
            &["s-1", "s-2"],
        );
        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        let mut resumes = FakeResumes::default();
        let notice = started(ask(&state, &mut resumes, RestoreAsk::Launch)).expect("the band names it");
        assert_eq!(notice, "2 bypass tabs came back in auto; Shift+Tab then y switches");
        assert_eq!(mode(&state, "s-1"), Auto);
        assert_eq!(mode(&state, "s-2"), Auto);
        assert_eq!(resumes.asked.len(), 2);
    }

    #[test]
    fn a_launch_has_one_go_whatever_comes_of_it() {
        let (state, _dir) = restore_panel("restore-launch-once", &[("s-x", None, Auto)], &[]);
        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Launch));
        assert!(
            state.borrow().restore_source.is_none(),
            "a reload of the page does not try again"
        );
        assert!(resumes.asked.is_empty(), "the record is gone, so nothing was begun");
        // And its message is already due: nothing is left to wait for.
        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        stepped(&state, finish_restore);
        assert_eq!(
            *toasts.borrow(),
            ["Restored 0 of 1 tab; 1 could not be: s-x (its saved record is gone)"]
        );
        stepped(&state, finish_restore);
        assert_eq!(toasts.borrow().len(), 1, "said once");
    }

    /// Window 1 starting window 2 again holds the automatic restore: the saved tabs of a window 2 that does not
    /// come back are given while it is held, and both windows' tabs return in the one restore a window gets.
    #[test]
    fn an_automatic_restore_held_for_window_2_runs_on_the_tick_after_it_is_let_go() {
        let (state, _dir) = restore_panel("restore-held", &[("s-1", None, Auto)], &["s-1"]);
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let handle = AgentPanelHandle::over(state.clone(), None);
        let tick = || {
            handle.run_seen(|s, screen, out| {
                tick_collect(s, screen, out);
            })
        };
        handle.hold_launch_restore();
        // The base page's `ready` while window 2 may still come back.
        assert!(launch_restore(&mut state.borrow_mut()).is_none());
        assert!(
            state.borrow().restore_source.is_some(),
            "nothing is restored while held"
        );
        assert!(!restore_settled(&state.borrow()), "a held restore is not settled");
        tick();
        assert!(
            state.borrow().restore_source.is_some(),
            "a tick while held restores nothing"
        );

        handle.release_launch_restore();
        tick();
        assert!(
            state.borrow().restore_source.is_none(),
            "the launch restore has its go on the tick after the hold is let go"
        );
        tick();
        assert!(!state.borrow().launch_restore_owed, "and only that once");
    }

    #[test]
    fn the_users_own_bypass_default_restores_bypass_tabs_without_asking() {
        let (state, _dir) = restore_panel("restore-config-bypass", &[("s-1", None, Bypass)], &["s-1"]);
        state.borrow_mut().bypass_by_config = true;
        let mut resumes = FakeResumes::default();
        assert_eq!(started(ask(&state, &mut resumes, RestoreAsk::Dashboard)), None);
        assert_eq!(mode(&state, "s-1"), Bypass);
    }

    /// Only the sidecar resumes: a window on the legacy backend neither offers nor attempts a restore,
    /// and says why if asked.
    #[test]
    fn the_legacy_backend_is_never_asked_to_restore() {
        let (state, _dir) = restore_panel("restore-legacy", &[("s-1", None, Auto)], &["s-1"]);
        state.borrow_mut().backend_kind = BackendKind::Legacy;
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask(&state, &mut resumes, RestoreAsk::Launch),
            RestoreResult::Refused(_)
        ));
        assert!(resumes.asked.is_empty());
    }

    #[test]
    fn a_restore_is_refused_when_it_would_be_stale() {
        let (state, _dir) = restore_panel("restore-stale", &[("s-1", None, Auto)], &["s-1"]);
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        assert!(
            matches!(
                ask(&state, &mut resumes, RestoreAsk::Dashboard),
                RestoreResult::Refused(_)
            ),
            "a second press: the tabs are already coming back"
        );
        let (none, _dir) = restore_panel("restore-none", &[], &[]);
        none.borrow_mut().restore_source = None;
        assert!(matches!(
            ask(&none, &mut FakeResumes::default(), RestoreAsk::Dashboard),
            RestoreResult::Refused(_)
        ));
    }

    /// A tab refused after another came up leaves its draft in the tab on screen: the page has that tab's composer
    /// open, so it is told, or its next mirror would erase the text; and the tab on screen stays on screen.
    #[test]
    fn a_late_refusal_puts_its_draft_in_the_composer_on_screen_and_moves_no_tab() {
        let (state, dir) = restore_panel(
            "restore-late-draft",
            &[("s-1", None, Auto), ("s-2", None, Auto), ("s-3", None, Auto)],
            &["s-1", "s-2", "s-3"],
        );
        {
            let mut s = state.borrow_mut();
            let source = s.restore_source.as_mut().unwrap();
            source.active = Some(1);
            source.claude_active = Some(1);
            source.tabs[2].draft = Some("typed for s-3".to_string());
        }
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let on_screen = state.borrow().tabs.active();
        for sender in &resumes.senders[..2] {
            let (_provider, backend) = live_backend(&dir);
            sender.send(Ok(backend)).unwrap();
        }
        let screen = Screen::nothing_shown();
        collect_pending_starts(&mut state.borrow_mut(), &screen, &mut Outbox::default());
        resumes.senders[2]
            .send(Err(BackendError {
                message: "refused".to_string(),
                benign: false,
            }))
            .unwrap();
        let mut out = Outbox::default();
        collect_pending_starts(&mut state.borrow_mut(), &screen, &mut out);
        assert_eq!(state.borrow().tabs.active(), on_screen, "the tab on screen stays");
        let payloads = out.payloads();
        let draft = payloads.iter().find(|p| p.contains("typed for s-3"));
        assert!(draft.is_some(), "the page hears the draft: {:?}", kinds(&payloads));
        out.deliver(&state, &Pages::default());
        assert_eq!(state.borrow().tabs.get(on_screen).unwrap().draft(), "typed for s-3");
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// The glue the tick runs: each result counts into the restore, a refused tab is tidied away,
    /// and one message ends it once the last resume is back.
    #[test]
    fn a_restore_ends_in_one_toast_once_the_last_resume_has_returned() {
        let (state, dir) = restore_panel(
            "restore-toast",
            &[("s-1", None, Auto), ("s-2", None, Auto), ("s-3", None, Auto)],
            &["s-1", "s-2", "s-3"],
        );
        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        stepped(&state, finish_restore);
        assert!(toasts.borrow().is_empty(), "three resumes are still connecting");

        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        resumes.senders[1]
            .send(Err(BackendError {
                message: "could not continue the previous session: no such session".to_string(),
                benign: false,
            }))
            .unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        let mut accounted = Vec::new();
        for result in &collected {
            accounted.push(account_for_restore(&mut state.borrow_mut(), result));
        }
        stepped(&state, finish_restore);
        assert!(
            accounted.contains(&RestoreStart::Installed)
                && accounted.iter().any(|a| matches!(a, RestoreStart::Failed { .. })),
            "{accounted:?}"
        );
        assert!(toasts.borrow().is_empty(), "the third is still connecting");
        assert_eq!(state.borrow().tabs.tabs().len(), 2, "the refused tab is gone");

        let (_provider, backend) = live_backend(&dir);
        resumes.senders[2].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        stepped(&state, finish_restore);
        assert_eq!(
            *toasts.borrow(),
            ["Restored 2 of 3 tabs; 1 could not be: title of s-2 (could not continue the previous session: no such session)"]
        );
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A restored tab the user closes before its resume returns is never going to report; the restore
    /// must still end, and say nothing about it.
    #[test]
    fn closing_a_restored_tab_while_it_connects_does_not_hold_the_restore_open() {
        let (state, dir) = restore_panel(
            "restore-closed",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let second = state.borrow().tabs.tab_with_session(&AgentId::Claude, "s-2").unwrap();
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        stepped(&state, finish_restore);
        assert!(toasts.borrow().is_empty(), "s-2 is still connecting");
        let closed = state.borrow_mut().tabs.remove(second);
        assert!(closed.is_some());
        stepped(&state, finish_restore);
        assert_eq!(*toasts.borrow(), ["Restored 1 tab"]);
        assert!(state.borrow().restore_run.is_none(), "so the restore is over");
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A restore that fails in the tab it began in leaves that tab as it found it: in the mode it had,
    /// not in one only the restored session was cleared for.
    /// The saved active tab is the one that does not come back: the screen goes to a tab that did, not
    /// to an empty dashboard over them.
    #[test]
    fn when_the_tab_on_screen_fails_the_first_that_restored_takes_its_place() {
        let (state, dir) = restore_panel(
            "restore-survivor",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let first = state.borrow().tabs.tab_with_session(&AgentId::Claude, "s-1").unwrap();
        let second = state.borrow().tabs.tab_with_session(&AgentId::Claude, "s-2").unwrap();
        assert_eq!(state.borrow().tabs.active(), first, "the saved active tab is the first");
        resumes.senders[0]
            .send(Err(BackendError {
                message: "refused".to_string(),
                benign: false,
            }))
            .unwrap();
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[1].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        assert_eq!(state.borrow().tabs.active(), second);
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn a_failed_restore_leaves_the_tab_it_began_in_in_its_old_mode() {
        let (state, _dir) = restore_panel("restore-mode-back", &[("s-1", None, Bypass)], &["s-1"]);
        state.borrow_mut().bypass_by_config = true; // the saved bypass tab comes back as saved
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let tab = state.borrow().tabs.active();
        assert_eq!(
            state.borrow().tabs.get(tab).unwrap().mode(),
            Bypass,
            "while it connects"
        );
        resumes.senders[0]
            .send(Err(BackendError {
                message: "refused".to_string(),
                benign: false,
            }))
            .unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        let s = state.borrow();
        assert!(matches!(s.tabs.get(tab).unwrap().backend(), TabBackend::NotStarted));
        assert_eq!(s.tabs.get(tab).unwrap().mode(), Auto, "an empty tab, in auto again");
    }

    #[test]
    fn a_connect_that_is_not_part_of_a_restore_is_left_to_its_own_handling() {
        let (state, _dir) = restore_panel("restore-unrelated", &[("s-1", None, Auto)], &["s-1"]);
        let collected = StartCollected::Failed {
            tab: state.borrow().tabs.active(),
            request_id: "r".to_string(),
            error: BackendError {
                message: "x".to_string(),
                benign: false,
            },
        };
        assert_eq!(
            account_for_restore(&mut state.borrow_mut(), &collected),
            RestoreStart::NotPartOfOne
        );
    }

    /// Once per tick, the tabs are shown to the memory: a session adopted is written, and so is
    /// nothing more until something changes.
    #[test]
    fn the_tick_keeps_the_saved_file_in_step_and_a_closing_window_leaves_it_alone() {
        let (state, dir) = restore_panel("restore-remember", &[], &[]);
        let files = agent::state_dirs::test_workspace_dir("restore-remember-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        remember_tabs(&mut state.borrow_mut());
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        assert!(
            std::fs::read_dir(&files).unwrap().next().is_none(),
            "an untouched launch writes nothing"
        );

        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.open_session("s-live", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        remember_tabs(&mut state.borrow_mut());
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        let loaded = match eitri_core::saved_tabs::load(&files, &dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].provider_session_id, "s-live");
        assert_eq!(loaded.tabs[0].conversation_id, state.borrow().conversation_id);

        let taken = state.borrow_mut().tabs.take_all();
        remember_tabs(&mut state.borrow_mut());
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        assert_eq!(
            eitri_core::saved_tabs::load(&files, &dir),
            eitri_core::saved_tabs::Loaded::Saved(loaded)
        );
        for tab in taken {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// What a report and a given set need of a real launch: the saved tabs of an empty panel, on its first page.
    fn chat_window(label: &str, policy: RestorePolicy, sessions: &[&str]) -> Rc<RefCell<AgentPanelState>> {
        let (state, _dir) = restore_panel(label, &[], sessions);
        state.borrow_mut().restore_policy = policy;
        state.borrow_mut().restore_source = None;
        state
    }

    fn given(sessions: &[&str]) -> SavedTabs {
        let tabs = sessions
            .iter()
            .map(|session| eitri_core::saved_tabs::SavedTab {
                agent: AgentId::Claude,
                conversation_id: "0123456789abcdef".to_string(),
                provider_session_id: session.to_string(),
                name: None,
                mode: Auto,
                view: None,
                window: None,
                turn_running: false,
                draft: None,
            })
            .collect();
        SavedTabs::from_parts(tabs, 0, Vec::new(), None)
    }

    fn take_given(state: &Rc<RefCell<AgentPanelState>>, tabs: SavedTabs) {
        handle_for(state).give_tabs(tabs);
    }

    /// Closing a window answers nothing about tabs on offer, and at a close that keeps it a window that told nothing and
    /// holds nothing must not say "no tabs": window 1 would take that for the window having closed them.
    #[test]
    fn tabs_on_offer_stay_unopened_and_an_empty_window_says_nothing() {
        let state = chat_window("unopened-offer", RestorePolicy::Offer, &["s-1"]);
        let handle = handle_for(&state);
        let sessions = |tabs: Option<SavedTabs>| {
            tabs.map(|tabs| {
                tabs.tabs
                    .iter()
                    .map(|tab| tab.provider_session_id.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(handle.unopened_tabs(), None);
        assert_eq!(handle.tabs_to_keep(), None);
        take_given(&state, given(&["s-1"]));
        assert_eq!(sessions(handle.unopened_tabs()), Some(vec!["s-1".to_string()]));
        assert_eq!(sessions(handle.tabs_to_keep()), Some(vec!["s-1".to_string()]));
    }

    #[test]
    fn tabs_given_before_the_first_ready_are_restored_by_it() {
        let state = chat_window("given-before-ready", RestorePolicy::Auto, &["s-1"]);
        take_given(&state, given(&["s-1"]));
        assert_eq!(
            state.borrow().restore_source.as_ref().map(|s| s.tabs.len()),
            Some(1),
            "they wait for the page, as a launch's saved tabs do"
        );
        assert!(!restore_settled(&state.borrow()));
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let result = launch_restore(&mut state.borrow_mut()).expect("the launch restore has them");
        assert!(matches!(result, RestoreResult::Deferred));
        assert!(state.borrow().restore_source.is_none(), "one go");
    }

    #[test]
    fn tabs_given_after_the_first_ready_are_restored_at_once() {
        let state = chat_window("given-after-ready", RestorePolicy::Auto, &["s-1"]);
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        take_given(&state, given(&["s-1"]));
        assert!(state.borrow().restore_source.is_none(), "a launch restore had them");
        assert!(
            restore_waits_for_trust(&state.borrow()),
            "through the trust gate, as any restore"
        );
    }

    /// The dashboard's line goes with the offer, and with `auto` there never was one: what the tab came back without is
    /// said in the restore's one message, which nothing replaces.
    #[test]
    fn a_restore_says_a_turn_was_cut_off_in_its_one_message() {
        let (state, dir) = restore_panel("restore-cut-turn", &[("s-1", None, Auto)], &["s-1"]);
        state.borrow_mut().restore_source.as_mut().unwrap().tabs[0].turn_running = true;
        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        stepped(&state, finish_restore);
        assert_eq!(
            *toasts.borrow(),
            ["Restored 1 tab; a turn was cut off when this window last ended"]
        );
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn tabs_given_under_offer_are_offered_and_under_off_are_dropped() {
        let offered = chat_window("given-offer", RestorePolicy::Offer, &["s-1"]);
        offered.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        take_given(&offered, given(&["s-1"]));
        assert_eq!(
            restore_offer(&offered.borrow()).map(|offer| offer.labels),
            Some(vec!["title of s-1".to_string()])
        );

        let off = chat_window("given-off", RestorePolicy::Off, &["s-1"]);
        take_given(&off, given(&["s-1"]));
        assert!(off.borrow().restore_source.is_none());
    }

    /// A window that lived through window 1's restart has its tabs, and the file may be behind it: what it closed
    /// meanwhile must not come back.
    #[test]
    fn a_window_that_has_started_keeps_what_it_has_when_tabs_are_given() {
        let (state, dir) = restore_panel("given-started", &[], &["s-1", "s-2"]);
        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        state.borrow_mut().restore_source = None;
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let tab = state.borrow().tabs.active();
        let (_provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        take_given(&state, given(&["s-1", "s-2"]));
        assert!(state.borrow().restore_source.is_none());
        assert_eq!(state.borrow().tabs.tabs().len(), 1);
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A chat window's writer is off and its tabs go over the link: only the sets that differ, nothing before its
    /// first session, and nothing on the disk.
    #[test]
    fn module_windows_never_write_a_tabs_file() {
        let (state, dir) = restore_panel("module-window-tabs", &[], &[]);
        let files = agent::state_dirs::test_workspace_dir("module-window-tabs-files");
        state.borrow_mut().tab_memory = TabWriter::new_as(
            Some(files.clone()),
            dir.clone(),
            eitri_core::window_link::pair::Role::Second,
        );
        let handle = handle_for(&state);
        handle.keep_no_tabs();
        let told = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        handle.report_tabs({
            let told = told.clone();
            move |tabs: &SavedTabs| {
                let sessions = tabs.tabs.iter().map(|t| t.provider_session_id.clone());
                told.borrow_mut().push(sessions.collect());
            }
        });
        remember_tabs(&mut state.borrow_mut());
        assert!(told.borrow().is_empty(), "a window that has done nothing says nothing");

        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.open_session("s-live", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        remember_tabs(&mut state.borrow_mut());
        remember_tabs(&mut state.borrow_mut());
        assert_eq!(*told.borrow(), [vec!["s-live".to_string()]], "once for one change");
        assert_eq!(
            handle.tabs_now().map(|tabs| tabs.tabs.len()),
            Some(1),
            "and it can say them at any moment"
        );
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        assert_eq!(std::fs::read_dir(&files).unwrap().count(), 0, "nothing was written");

        let taken = state.borrow_mut().tabs.take_all();
        remember_tabs(&mut state.borrow_mut());
        assert_eq!(told.borrow().len(), 1, "a closing window's empty set is not a change");
        for tab in taken {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A hub is told no tab files and keeps no tabs: what the last run saved is not offered, not claimed, and not
    /// written over at its close.
    #[test]
    fn a_window_that_keeps_no_tabs_offers_claims_and_writes_none() {
        let (state, dir) = restore_panel("keeps-no-tabs", &[("s-1", None, Auto)], &["s-1"]);
        let files = agent::state_dirs::test_workspace_dir("keeps-no-tabs-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        let handle = handle_for(&state);
        handle.keep_no_tabs();
        assert!(state.borrow().restore_source.is_none());
        assert!(!state.borrow().tab_memory.is_enabled());
        assert!(!restore_is_on_offer(&state.borrow()));
        drop(handle.shutdown());
        assert_eq!(std::fs::read_dir(&files).unwrap().count(), 0, "nothing was written");
    }

    /// `agent.restore = "off"` is the whole feature off: nothing is offered, nothing is remembered.
    #[test]
    fn restore_off_offers_nothing_and_remembers_nothing() {
        let (state, dir) = restore_panel("restore-off", &[("s-1", None, Auto)], &["s-1"]);
        let files = agent::state_dirs::test_workspace_dir("restore-off-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        handle_for(&state).set_restore_policy(RestorePolicy::Off);
        assert!(state.borrow().restore_source.is_none());
        assert!(!state.borrow().tab_memory.is_enabled());
        assert!(!restore_is_on_offer(&state.borrow()));

        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.open_session("s-live", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        remember_tabs(&mut state.borrow_mut());
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        assert!(std::fs::read_dir(&files).unwrap().next().is_none(), "nothing written");
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// At launch nothing of an untrusted root starts and no saved session is leased while the
    /// question waits; an `n` then brings every saved tab back without the project's tiers.
    #[test]
    fn automatic_restore_never_starts_an_untrusted_session() {
        let (state, dir) = restore_panel(
            "trust-restore-launch",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let canonical = dir.to_string_lossy().into_owned();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        let result = launch_restore(&mut state.borrow_mut());
        assert!(matches!(result, Some(RestoreResult::Deferred)));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        assert!(resumes.asked.is_empty(), "nothing is resumed while the question waits");
        for id in ["s-1", "s-2"] {
            let lease = agent::lease::SessionLease::try_acquire("claude", &canonical, id);
            assert!(lease.is_ok(), "{id} is not leased while the question waits");
        }
        // The answer below leases both again, and a lease dropped just above may still be held a moment
        // by a child another test is starting; the restore would then skip it as open elsewhere.
        for id in ["s-1", "s-2"] {
            assert!(lease_comes_free(&canonical, id), "{id}'s probe lease is let go");
        }
        assert!(
            state.borrow().restore_source.is_none(),
            "a launch has its one go, even while it waits"
        );
        assert!(
            matches!(
                ask_for_restore(&mut state.borrow_mut(), prompt.tab, RestoreAsk::Dashboard),
                RestoreResult::Refused(_)
            ),
            "a second restore waits for the first"
        );

        let out = with_resumes(&state, &mut resumes, || answer_prompt(&state, "n1", &prompt, false));
        assert!(out.restored.is_some(), "the restore starts on the answer");
        assert_eq!(resumes.asked, ["s-1", "s-2"]);
        assert_eq!(resumes.trusts, [ProjectTrust::Untrusted; 2]);
        assert!(notices(&out.payloads()).contains(&crate::trust_gate::NOT_LOADED.to_string()));
        for t in state.borrow().tabs.tabs() {
            assert_eq!(t.session.project_tiers, Some(ProjectTrust::Untrusted));
        }
    }

    /// A restore waiting for the trust question keeps its sessions in the saved-tabs file, and so does
    /// one put off with `Escape`: they come back as failed tabs holding their resume ids, names and
    /// modes, so a session started afterwards does not write over them.
    #[test]
    fn a_restore_put_off_by_escape_keeps_its_sessions_in_the_saved_file() {
        let (state, dir) = restore_panel(
            "trust-restore-escape",
            &[("s-1", Some("api"), Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let files = agent::state_dirs::test_workspace_dir("trust-restore-escape-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        let both = vec![("s-1".to_string(), Some("api".to_string())), ("s-2".to_string(), None)];
        remember_tabs_written(&state);
        assert_eq!(saved_file_sessions(&files, &dir), both, "kept while the question waits");

        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads(), "c1"), Some(Ok(())));
        assert_eq!(out.toast.as_deref(), Some(crate::trust_gate::PUT_OFF));
        assert!(resumes.asked.is_empty(), "nothing was resumed");
        // A session started afterwards is written beside them, not over them.
        let (other, _provider) = start_session_in_new_tab(&state, &dir, "s-new");
        remember_tabs_written(&state);
        let mut with_new = both.clone();
        with_new.push(("s-new".to_string(), None));
        assert_eq!(saved_file_sessions(&files, &dir), with_new, "kept after it is put off");
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
        let s = state.borrow();
        let failed: Vec<_> = s
            .tabs
            .tabs()
            .iter()
            .filter(|t| matches!(t.backend(), TabBackend::Failed { reason } if reason == crate::trust_gate::PUT_OFF))
            .map(|t| t.failed_resume.clone())
            .collect();
        assert_eq!(failed, [Some("s-1".to_string()), Some("s-2".to_string())]);
    }

    /// A put-off restore's tab saved in an extra view is kept there, as a failed tab, and that view's
    /// page is shown it; the saved file still names the view.
    #[test]
    fn a_restore_put_off_keeps_a_tab_in_its_saved_view_and_shows_it_there() {
        let (state, dir) = restore_panel(
            "trust-restore-escape-view",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_source.as_mut().unwrap().tabs[0].view = Some(2);
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
            s.page_mut(view(2)).document_ready = true;
            s.tabs.set_layout_views(vec![view(2)]);
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads(), "c1"), Some(Ok(())));
        assert!(resumes.asked.is_empty(), "nothing was resumed");

        let s = state.borrow();
        let kept = s.tabs.tab_in(view(2)).expect("the tab saved in agent#2 is kept there");
        assert_eq!(s.tabs.get(kept).unwrap().failed_resume.as_deref(), Some("s-1"));
        assert!(s.tabs.view_of(s.tabs.active()).is_base());
        let to_view: Vec<serde_json::Value> = out
            .payloads()
            .iter()
            .zip(out.dests())
            .filter(|(_, dest)| dest_view(&s, *dest) == view(2))
            .map(|(payload, _)| json_of(payload))
            .collect();
        assert!(
            to_view.iter().any(|v| v["kind"] == "tabs" && v["active"] == kept.0),
            "its page is shown the kept tab: {to_view:?}"
        );
        let saved = s.tabs.saved_snapshot(&s.conversation_id).unwrap();
        let mut views: Vec<(String, Option<u8>)> = saved
            .tabs
            .iter()
            .map(|t| (t.provider_session_id.clone(), t.view))
            .collect();
        views.sort();
        assert_eq!(views, [("s-1".to_string(), Some(2)), ("s-2".to_string(), None)]);
    }

    /// A restore asked for from the launch dashboard leaves the saved tabs on offer: put off with
    /// `Escape`, its tab is empty again, the offer stands (the same key asks again) and the file is not
    /// touched, as before restores kept their sessions as failed tabs.
    #[test]
    fn a_dashboard_restore_put_off_by_escape_stays_on_offer() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-escape",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-escape-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(
            matches!(result, RestoreResult::Deferred),
            "no bypass tab, so no question"
        );
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        remember_tabs_written(&state);
        assert!(
            std::fs::read_dir(&files).unwrap().next().is_none(),
            "the file is not written while it waits with nothing else to save"
        );
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads(), "c1"), Some(Ok(())));
        remember_tabs_written(&state);
        assert!(std::fs::read_dir(&files).unwrap().next().is_none(), "nor after");
        let s = state.borrow();
        assert!(s.tabs.is_pristine(), "the tab is empty again");
        assert!(restore_is_on_offer(&s), "the same key can ask again");
    }

    /// `remember_tabs`, then waits until the writer thread has written what it was handed, so the test
    /// can read the file.
    fn remember_tabs_written(state: &Rc<RefCell<AgentPanelState>>) {
        remember_tabs(&mut state.borrow_mut());
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
    }

    /// A dashboard restore waits for the trust question while another tab's session opens. That session's
    /// id arrives on the backend's own thread, whose fold wakes the tick; the tick it brings settles the
    /// waiting restore and hands the writer a set with the waiting sessions in it.
    #[test]
    fn a_waiting_restore_is_settled_and_written_by_the_tick_a_fold_wakes() {
        let (state, dir) = restore_panel(
            "trust-restore-settle-wake",
            &[("s-1", Some("api"), Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-settle-wake-files");
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        let from = state.borrow().tabs.active();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let at = tick_until_quiet(&handle, std::time::Instant::now());
        assert!(
            page.sent.borrow().iter().any(|p| kind_of(p) == "trust_prompt"),
            "the restore waits for the question"
        );
        assert!(state.borrow().restore_source.is_some(), "still on offer");

        // Another tab's backend; the tick that finds it hands it the wake.
        let other = state.borrow_mut().tabs.open(AgentId::Claude);
        state.borrow_mut().tabs.select(from);
        let (provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(other)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        let at = tick_until_quiet(&handle, at + AgentPanelHandle::TICK);
        handle.take_wake(at);

        provider.open_session("s-new", &dir);
        let due = wait_for_wake(&handle);
        tick_until_quiet(&handle, due);
        assert!(state.borrow().restore_source.is_none(), "the offer is spent");
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        let ids: Vec<String> = saved_file_sessions(&files, &dir)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["s-new", "s-1", "s-2"], "written with the waiting sessions");
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
    }

    /// The saved file as the window last wrote it: each Claude session with its name.
    fn saved_file_sessions(files: &std::path::Path, dir: &std::path::Path) -> Vec<(String, Option<String>)> {
        match eitri_core::saved_tabs::load(files, dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved
                .tabs
                .into_iter()
                .map(|t| (t.provider_session_id, t.name))
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    /// Starts a session in a new tab of the window, as a typed first message would, and waits for its id.
    /// The tab on screen stays on screen.
    fn start_session_in_new_tab(
        state: &Rc<RefCell<AgentPanelState>>,
        dir: &std::path::Path,
        session: &str,
    ) -> (TabId, std::sync::Arc<RecordingProvider>) {
        let back = state.borrow().tabs.active();
        let tab = state.borrow_mut().tabs.open(AgentId::Claude);
        state.borrow_mut().tabs.select(back);
        let (provider, backend) = live_backend(dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.open_session(session, dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (tab, provider)
    }

    /// A dashboard restore waits for the trust question while another tab starts a session: the file
    /// the window then writes keeps the waiting sessions, also once that other tab is closed again, and
    /// put off with `Escape` they come back as failed tabs instead of being left out of every tab.
    #[test]
    fn a_waiting_dashboard_restore_stays_in_the_file_when_another_tab_starts_a_session() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-other",
            &[("s-1", Some("api"), Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-other-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let _ = with_resumes(&state, &mut resumes, || next_prompt(&state));

        let (other, _provider) = start_session_in_new_tab(&state, &dir, "s-new");
        remember_tabs_written(&state);
        let waiting = vec![("s-1".to_string(), Some("api".to_string())), ("s-2".to_string(), None)];
        let mut all = vec![("s-new".to_string(), None)];
        all.extend(waiting.clone());
        assert_eq!(
            saved_file_sessions(&files, &dir),
            all,
            "written with the waiting sessions"
        );
        assert!(state.borrow().restore_source.is_none(), "the offer is spent");

        // Closing the other tab again does not take the waiting sessions back out of the file.
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
        remember_tabs_written(&state);
        assert_eq!(
            saved_file_sessions(&files, &dir),
            waiting,
            "still listed while they wait"
        );

        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads(), "c1"), Some(Ok(())));
        assert!(resumes.asked.is_empty(), "nothing was resumed");
        remember_tabs_written(&state);
        assert_eq!(saved_file_sessions(&files, &dir), waiting, "kept after it is put off");
        let s = state.borrow();
        let failed: Vec<_> = s
            .tabs
            .tabs()
            .iter()
            .filter(|t| matches!(t.backend(), TabBackend::Failed { reason } if reason == crate::trust_gate::PUT_OFF))
            .map(|t| t.failed_resume.clone())
            .collect();
        assert_eq!(failed, [Some("s-1".to_string()), Some("s-2".to_string())]);
    }

    /// A put-off while another tab's session is still starting (no id yet, so nothing of it is saved)
    /// keeps the sessions as failed tabs: that tab writes the file once its id arrives, and the offer
    /// could not be shown again beside it.
    #[test]
    fn a_dashboard_restore_put_off_beside_a_starting_session_keeps_its_sessions() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-starting",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-starting-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        // Another tab starts a session whose id has not arrived.
        let other = state.borrow_mut().tabs.open(AgentId::Claude);
        state.borrow_mut().tabs.select(from);
        let (provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(other)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        let nonce = state.borrow_mut().tabs.ask_trust(from).unwrap();
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads(), "c1"), Some(Ok(())));
        assert!(state.borrow().restore_source.is_none(), "no longer on offer");
        remember_tabs_written(&state);

        provider.open_session("s-new", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(other).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        remember_tabs_written(&state);
        let ids: Vec<String> = saved_file_sessions(&files, &dir)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["s-1", "s-new", "s-2"]);
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
    }

    /// A put-off that comes before the tick has written anything (the other session started in the same
    /// tick) still keeps the sessions, since the file is about to be written without them.
    #[test]
    fn a_dashboard_restore_put_off_beside_another_session_keeps_its_sessions() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-same-tick",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-same-tick-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let _ = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let (other, _provider) = start_session_in_new_tab(&state, &dir, "s-new");
        // Asked again with no tick between (a tick would write the file first).
        let nonce = state.borrow_mut().tabs.ask_trust(from).unwrap();
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", from, nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads(), "c1"), Some(Ok(())));
        assert!(state.borrow().restore_source.is_none(), "no longer on offer");
        remember_tabs_written(&state);
        let ids: Vec<String> = saved_file_sessions(&files, &dir)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["s-1", "s-new", "s-2"], "the first in the tab it waited in");
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let Some(retired) = closed.retired.take() {
            retired.into_backend().shutdown();
        }
    }

    /// A session of a waiting restore that a failed tab already holds (a resume by hand that failed) is
    /// listed once.
    #[test]
    fn a_waiting_restore_does_not_list_a_session_a_failed_tab_holds() {
        let (state, dir) = restore_panel(
            "trust-restore-no-twice",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let _ = with_resumes(&state, &mut resumes, || next_prompt(&state));
        {
            let mut s = state.borrow_mut();
            let other = s.tabs.open(AgentId::Claude);
            let t = s.tabs.get_mut(other).unwrap();
            t.set_backend(TabBackend::Failed { reason: "x".into() });
            t.failed_resume = Some("s-1".into());
        }
        let saved = state.borrow().tabs.saved_snapshot("conv").unwrap();
        let ids: Vec<_> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(ids.iter().filter(|id| **id == "s-1").count(), 1, "{ids:?}");
        assert!(ids.contains(&"s-2"), "{ids:?}");
    }

    /// A restore whose start is refused outright (here by the panel's own put-off, as an agent that
    /// needs a `y` gets after an `n`) keeps its sessions the same way, saying why on the failed tabs.
    #[test]
    fn a_restore_whose_start_is_refused_keeps_its_sessions_in_the_saved_file() {
        let (state, dir) = restore_panel(
            "trust-restore-refused",
            &[("s-1", None, Auto), ("s-2", Some("two"), Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let why = "not started: for the test";
        {
            let mut s = state.borrow_mut();
            let start = s.tabs.take_deferred(prompt.tab).expect("the restore waits in its tab");
            assert!(matches!(start, DeferredStart::Restore { .. }));
            let out = put_off_start(&mut s, prompt.tab, start, why);
            assert_eq!(out.toast.as_deref(), Some(why));
        }
        let s = state.borrow();
        let saved = s.tabs.saved_snapshot("conv").unwrap();
        let ids: Vec<_> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["s-1", "s-2"]);
        assert_eq!(saved.tabs[1].name.as_deref(), Some("two"));
        for t in s.tabs.tabs() {
            assert!(
                matches!(t.backend(), TabBackend::Failed { reason } if reason == why),
                "{:?}",
                t.id
            );
        }
        assert!(resumes.asked.is_empty());
    }

    /// A panel from [`restore_panel`] whose last window also left a Codex tab (`thread-9`, the one on
    /// screen), with its record, and a spawner that records what it was asked to start.
    #[allow(clippy::type_complexity)]
    fn restore_panel_with_codex(
        label: &str,
    ) -> (
        Rc<RefCell<AgentPanelState>>,
        PathBuf,
        Rc<RefCell<Vec<String>>>,
        Rc<RefCell<Vec<ExternalSpawned>>>,
    ) {
        restore_panel_with_external(label, AgentId::Codex, "thread-9")
    }

    /// [`restore_panel_with_codex`] for a tab of `agent` holding `session`.
    #[allow(clippy::type_complexity)]
    fn restore_panel_with_external(
        label: &str,
        agent: AgentId,
        session: &str,
    ) -> (
        Rc<RefCell<AgentPanelState>>,
        PathBuf,
        Rc<RefCell<Vec<String>>>,
        Rc<RefCell<Vec<ExternalSpawned>>>,
    ) {
        let (state, dir) = restore_panel(label, &[("s-1", None, Auto)], &["s-1"]);
        let conversation_id = agent::conversation_id_for_cwd(&dir);
        agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
            conversation_id: conversation_id.clone(),
            provider: agent.record_name(),
            provider_session_id: session.to_string(),
            canonical_cwd: dir.to_string_lossy().into_owned(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            provider_advertised_resume: true,
            title: Some(format!("title of {session}")),
            name: None,
        })
        .unwrap();
        let resumed = Rc::new(RefCell::new(Vec::new()));
        let external = Rc::new(RefCell::new(Vec::new()));
        {
            let mut s = state.borrow_mut();
            s.page_mut(ViewId::BASE).document_ready = true;
            let saved = s.restore_source.take().unwrap();
            // The other agent's tab is the one that was on screen.
            s.restore_source = Some(SavedTabs::from_parts(
                saved.tabs,
                0,
                vec![eitri_core::saved_tabs::SavedTab {
                    agent,
                    conversation_id,
                    provider_session_id: session.to_string(),
                    name: None,
                    mode: Auto,
                    view: None,
                    window: None,
                    turn_running: false,
                    draft: None,
                }],
                Some(0),
            ));
            let (resumed, external) = (resumed.clone(), external.clone());
            s.spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
                match call {
                    SpawnCall::Resume { id, .. } => resumed.borrow_mut().push(id),
                    SpawnCall::Connect { .. } => panic!("a restore resumes; it never connects afresh"),
                    SpawnCall::External {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp,
                        after_old,
                        editor_server,
                    } => external.borrow_mut().push(ExternalSpawned {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp: acp.map(|spec| (spec.argv, spec.auto_mode)),
                        waits_for_old: after_old.is_some(),
                        editor_server,
                    }),
                }
                mpsc::channel().1
            })));
        }
        (state, dir, resumed, external)
    }

    /// A saved Codex tab comes back after the Claude tabs' answer, holding its resume for Codex's own
    /// answer (here nothing to trust, so given at once), and is resumed as a resume by hand would be.
    #[test]
    fn a_saved_codex_tab_is_restored_through_its_own_trust_answer() {
        let (state, _dir, resumed, external) = restore_panel_with_codex("restore-codex");
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        tick_until(&state, |_| !external.borrow().is_empty());
        assert_eq!(*resumed.borrow(), ["s-1"]);
        let spawned = external.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].agent, AgentId::Codex);
        assert_eq!(spawned[0].resume.as_deref(), Some("thread-9"));
        assert_eq!(spawned[0].mode, Auto);
        assert!(spawned[0].plan.skills_off, "nothing to trust starts untrusted");
        let s = state.borrow();
        let codex = s
            .tabs
            .tab_with_session(&AgentId::Codex, "thread-9")
            .expect("the Codex tab is back");
        assert_eq!(s.tabs.get(codex).unwrap().agent(), &AgentId::Codex);
        assert_eq!(s.tabs.active(), codex, "the saved active tab is on screen");
        let saved = s.tabs.saved_snapshot("conv").unwrap();
        assert_eq!(saved.claude_tabs().count(), 1);
        assert_eq!(saved.other_tabs().count(), 1, "the Codex tab is still saved");
    }

    /// A saved tab of a registered ACP agent comes back holding its resume and waits for that agent's own
    /// question, which it is asked even with nothing found; a `y` resumes it with the registered command,
    /// and an `n` starts nothing.
    #[test]
    fn a_saved_acp_tab_is_restored_only_after_its_own_yes() {
        for yes in [true, false] {
            let label = if yes { "restore-acp-yes" } else { "restore-acp-no" };
            let gem = AgentId::Acp("gem".into());
            let (state, _dir, resumed, external) = restore_panel_with_external(label, gem.clone(), "gem-s");
            state.borrow_mut().acp_agents = vec![gem_registration()];
            let from = state.borrow().tabs.active();
            let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
            assert!(matches!(result, RestoreResult::Deferred));
            let prompt = next_prompt(&state);
            assert_eq!(*resumed.borrow(), ["s-1"], "Claude has nothing to trust and started");
            assert!(external.borrow().is_empty(), "nothing of gem starts while it is asked");
            let waiting = state
                .borrow()
                .tabs
                .tab_with_session(&AgentId::Acp("gem".into()), "gem-s")
                .expect("the gem tab is back");
            assert_eq!(prompt.tab, waiting);
            let saved = state.borrow().tabs.saved_snapshot("conv").unwrap();
            assert_eq!(saved.other_tabs().count(), 1, "kept in the saved file while it waits");
            assert_eq!(saved.other_tabs().next().unwrap().agent, gem);

            answer_prompt(&state, "a1", &prompt, yes);
            if yes {
                tick_until(&state, |_| !external.borrow().is_empty());
                let spawned = external.borrow();
                assert_eq!(spawned.len(), 1);
                assert_eq!(spawned[0].agent, gem);
                assert_eq!(spawned[0].resume.as_deref(), Some("gem-s"));
                assert_eq!(
                    spawned[0].acp,
                    Some((
                        vec![std::ffi::OsString::from("gem-acp"), std::ffi::OsString::from("--stdio")],
                        Some("auto_edit".to_string())
                    ))
                );
            } else {
                for _ in 0..20 {
                    trust_events(&mut state.borrow_mut());
                }
                assert!(external.borrow().is_empty(), "an n starts nothing");
            }
        }
    }

    /// A saved tab of an ACP agent `init.lua` no longer registers is skipped, said to be, and never
    /// asked about; the Claude tabs come back as usual, and the next save leaves it out.
    #[test]
    fn a_saved_tab_of_an_unregistered_acp_agent_is_skipped() {
        let gem = AgentId::Acp("gem".into());
        let (state, _dir, resumed, external) = restore_panel_with_external("restore-acp-gone", gem, "gem-s");
        assert!(state.borrow().acp_agents.is_empty());
        let plan = restore_plan(&state.borrow());
        assert_eq!(plan.items.len(), 1, "only the Claude tab");
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, eitri_core::tab_restore::NOT_REGISTERED);
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        tick_until(&state, |_| !resumed.borrow().is_empty());
        assert_eq!(*resumed.borrow(), ["s-1"]);
        assert!(external.borrow().is_empty());
        let s = state.borrow();
        assert!(
            s.tabs.tab_with_session(&AgentId::Acp("gem".into()), "gem-s").is_none(),
            "no tab is made for it"
        );
        let saved = s.tabs.saved_snapshot("conv").unwrap();
        assert!(!saved.has_other_agents(), "the next save leaves it out");
    }

    /// When the project has something for Codex to load, the restored Codex tab waits for that
    /// question without starting, and stays in the saved file meanwhile; the answer starts it.
    #[test]
    fn a_restored_codex_tab_waiting_for_its_question_stays_saved() {
        let (state, dir, resumed, external) = restore_panel_with_codex("restore-codex-asks");
        std::fs::write(dir.join("AGENTS.md"), b"be careful\n").unwrap();
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        let prompt = next_prompt(&state);
        assert_eq!(*resumed.borrow(), ["s-1"], "Claude has nothing to trust and started");
        assert!(
            external.borrow().is_empty(),
            "nothing of Codex starts while it is asked"
        );
        let codex = state
            .borrow()
            .tabs
            .tab_with_session(&AgentId::Codex, "thread-9")
            .expect("the Codex tab is back");
        assert_eq!(prompt.tab, codex);
        assert!(matches!(
            state.borrow().tabs.get(codex).unwrap().backend(),
            TabBackend::AwaitingTrust(_)
        ));
        let saved = state.borrow().tabs.saved_snapshot("conv").unwrap();
        assert_eq!(saved.other_tabs().count(), 1, "kept in the saved file while it waits");
        assert_eq!(saved.other_tabs().next().unwrap().provider_session_id, "thread-9");

        let out = answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(result_for(&out.payloads(), "n1"), Some(Ok(())));
        let spawned = external.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].resume.as_deref(), Some("thread-9"));
        assert!(spawned[0].plan.skills_off, "an n starts it untrusted");
    }

    /// A restore of Codex tabs alone asks Codex's question, once, and never Claude's.
    #[test]
    fn a_restore_of_codex_tabs_alone_asks_only_codexs_question() {
        let (state, dir, resumed, external) = restore_panel_with_codex("restore-codex-only");
        {
            // Only the Codex tab is saved, and it was the one on screen.
            let mut s = state.borrow_mut();
            let saved = s.restore_source.take().unwrap();
            let codex_only = SavedTabs::from_parts(Vec::new(), 0, saved.other_tabs().cloned().collect(), Some(0));
            s.restore_source = Some(codex_only);
        }
        // Claude would have something to ask about too; it is not asked.
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), b"be careful\n").unwrap();
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let asked: Vec<serde_json::Value> = all_payloads(&seen)
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "trust_prompt")
            .collect();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["agent"], "Codex");
        let prompt = prompts(&all_payloads(&seen)).pop().unwrap();
        let out = answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(result_for(&out.payloads(), "n1"), Some(Ok(())));
        let seen = tick_until(&state, |_| !external.borrow().is_empty());
        assert!(
            prompts(&all_payloads(&seen)).is_empty(),
            "the window's answer covers the tab"
        );
        assert!(resumed.borrow().is_empty());
        assert_eq!(external.borrow()[0].resume.as_deref(), Some("thread-9"));
    }

    /// A saved bypass tab's question is answered first; only then is the trust question asked, and
    /// still nothing has been leased.
    #[test]
    fn the_restore_bypass_question_comes_before_the_trust_question() {
        let (state, dir) = restore_panel("trust-restore-bypass", &[("s-1", None, Bypass)], &["s-1"]);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let RestoreResult::Question(question) = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard)
        else {
            panic!("a bypass tab is asked about first")
        };
        assert!(state.borrow().tabs.awaiting_trust().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(prompts(&trust_events(&mut state.borrow_mut()).payloads()).is_empty());

        let answered = answer_restore_question(&mut state.borrow_mut(), question.nonce, true);
        assert!(matches!(answered, RestoreResult::Deferred));
        let mut resumes = FakeResumes::default();
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        assert_eq!(prompt.tab, from);
        assert!(resumes.asked.is_empty());
        let canonical = dir.to_string_lossy().into_owned();
        assert!(agent::lease::SessionLease::try_acquire("claude", &canonical, "s-1").is_ok());
    }

    /// A restore question's `y` resumes the saved bypass tabs in bypass, so from a chat a float covers
    /// it meets the same gate as a card answer: refused, nothing is started and the question is kept,
    /// so the same `y` goes through once the float is off the chat.
    #[test]
    fn a_bypass_restore_y_from_a_covered_chat_is_refused_and_the_question_stays() {
        const COVERED: &str = "agent is under terminal 2: C-b M-- from terminal 2 stows it";
        let (state, dir) = restore_panel("gate-restore-y", &[("s-1", None, Bypass)], &["s-1"]);
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        // Attaching the page starts its record afresh, so it is ready only from here.
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let RestoreResult::Question(question) = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard)
        else {
            panic!("a bypass tab is asked about first")
        };
        let covered = Rc::new(Cell::new(true));
        let asked = Rc::new(Cell::new(0));
        handle.set_answer_gate(Rc::new({
            let covered = covered.clone();
            let asked = asked.clone();
            move |view: ViewId| {
                assert_eq!(view, ViewId::BASE, "asked with the view of the page that answered");
                asked.set(asked.get() + 1);
                if covered.get() {
                    Err(COVERED.to_string())
                } else {
                    Ok(())
                }
            }
        }));
        let answer = |request: &str| {
            handle.page_message(
                ViewId::BASE,
                &format!(
                    r#"{{"type":"restore_answer","request_id":"{request}","nonce":{},"keep_bypass":true}}"#,
                    question.nonce
                ),
            );
        };

        answer("r1");
        assert_eq!(result_for(&page.sent.borrow(), "r1"), Some(Err(COVERED.to_string())));
        assert_eq!(asked.get(), 1, "the gate is asked once, with the base view");
        assert!(
            state.borrow().tabs.awaiting_trust().is_empty(),
            "nothing waits to start"
        );
        let canonical = dir.to_string_lossy().into_owned();
        assert!(
            agent::lease::SessionLease::try_acquire("claude", &canonical, "s-1").is_ok(),
            "nothing was started"
        );

        covered.set(false);
        answer("r2");
        assert_eq!(
            result_for(&page.sent.borrow(), "r2"),
            Some(Ok(())),
            "the question was kept for the same nonce"
        );
        assert_eq!(asked.get(), 2);
    }

    /// A restore question's `n` resumes nothing in bypass, so it is not asked of the gate.
    #[test]
    fn a_restore_n_is_not_asked_of_the_gate() {
        let (state, _dir) = restore_panel("gate-restore-n", &[("s-1", None, Bypass)], &["s-1"]);
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        // Attaching the page starts its record afresh, so it is ready only from here.
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let RestoreResult::Question(question) = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard)
        else {
            panic!("a bypass tab is asked about first")
        };
        let asked = Rc::new(Cell::new(0));
        handle.set_answer_gate(Rc::new({
            let asked = asked.clone();
            move |_view: ViewId| {
                asked.set(asked.get() + 1);
                Err("covered".to_string())
            }
        }));
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"restore_answer","request_id":"n1","nonce":{},"keep_bypass":false}}"#,
                question.nonce
            ),
        );
        assert_eq!(result_for(&page.sent.borrow(), "n1"), Some(Ok(())));
        assert_eq!(asked.get(), 0, "an `n` is not asked of the gate");
    }

    #[test]
    fn a_restore_that_asks_keeps_an_extra_view_until_its_tab_arrives() {
        let (state, dir) = restore_panel("views-restore", &[("s-1", None, Bypass)], &["s-1"]);
        state.borrow_mut().restore_source.as_mut().unwrap().tabs[0].view = Some(2);
        let base = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(base.clone() as Rc<dyn PageSurface>))),
        );
        let _second = attach_recording_page(&handle, view(2));
        let _third = attach_recording_page(&handle, view(3));
        handle.set_layout_views(vec![view(2), view(3)]);
        let emptied = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        handle.on_view_empty({
            let emptied = emptied.clone();
            move |view| emptied.borrow_mut().push(view)
        });
        handle.page_message(ViewId::BASE, READY);
        handle.tick();
        assert!(!handle.restore_settled(), "on offer");
        assert!(emptied.borrow().is_empty());

        // The saved bypass tab is asked about first.
        let from = state.borrow().tabs.active();
        let asked = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        let RestoreResult::Question(prompt) = asked else {
            panic!("a saved bypass tab is asked about");
        };
        handle.tick();
        assert!(!handle.restore_settled(), "the restore's question is open");
        assert!(emptied.borrow().is_empty());

        // Answered, it waits for the trust check, then resumes into its view.
        let answered = answer_restore_question(&mut state.borrow_mut(), prompt.nonce, false);
        assert!(matches!(answered, RestoreResult::Deferred));
        assert!(!handle.restore_settled(), "waiting for the trust question");
        let mut resumes = FakeResumes::default();
        with_resumes(&state, &mut resumes, || {
            tick_handle_until(&handle, "the restore to start", || state.borrow().restore_run.is_some());
            assert!(emptied.borrow().is_empty());
        });
        assert_eq!(resumes.asked, vec!["s-1".to_string()]);
        let restored = state
            .borrow()
            .tabs
            .tab_in(view(2))
            .expect("the saved tab comes back into its view");
        handle.tick();
        assert!(!handle.restore_settled(), "its resume is still out");
        assert!(emptied.borrow().is_empty());

        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        tick_handle_until(&handle, "the restore to settle", || handle.restore_settled());
        handle.tick();
        assert_eq!(state.borrow().tabs.tab_in(view(2)), Some(restored));
        assert_eq!(
            *emptied.borrow(),
            vec![view(3)],
            "only the view the restore left empty goes, once"
        );
        handle.tick();
        assert_eq!(emptied.borrow().len(), 1);
    }

    /// A restore `y` the window's gate turns down starts nothing and leaves the question open under the
    /// nonce the page was shown, so that same answer goes through once the gate lets it.
    #[test]
    fn a_refused_restore_answer_keeps_the_question_under_its_nonce() {
        let (state, dir) = restore_panel("admission-restore", &[("s-1", None, Bypass)], &["s-1"]);
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        // Attaching the page starts its record afresh, so it is ready only from here.
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let RestoreResult::Question(question) = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard)
        else {
            panic!("a bypass tab is asked about first")
        };
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[1], covered.clone());
        let answer = |request: &str| {
            handle.page_message(
                ViewId::BASE,
                &format!(
                    r#"{{"type":"restore_answer","request_id":"{request}","nonce":{},"keep_bypass":true}}"#,
                    question.nonce
                ),
            );
        };

        answer("r1");
        assert!(
            matches!(result_for(&page.sent.borrow(), "r1"), Some(Err(why)) if why.contains("under terminal 2")),
            "the page is told the window's words"
        );
        assert!(
            state.borrow().tabs.awaiting_trust().is_empty(),
            "nothing waits to start"
        );
        let canonical = dir.to_string_lossy().into_owned();
        assert!(
            agent::lease::SessionLease::try_acquire("claude", &canonical, "s-1").is_ok(),
            "nothing was started"
        );

        covered.set(false);
        answer("r2");
        assert_eq!(
            result_for(&page.sent.borrow(), "r2"),
            Some(Ok(())),
            "the same nonce still answers the question"
        );
        assert_eq!(
            *asked.borrow(),
            vec![ViewId::BASE, ViewId::BASE],
            "asked once per answer"
        );
    }
}
