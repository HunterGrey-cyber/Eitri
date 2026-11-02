//! A chat tab handed to another window by resume, the panel's half: the tab let go in the window that held it, and
//! taken up in the window it goes to. It serves the chat windows of compositor mode, which never link to each other, so
//! no live agent crosses: one process drops the session and the next resumes it, as a launch brings a saved tab back.
//!
//! **Let go.** The tab closes as `prefix &` closes one, which keeps its record, and the answer waits for its backend's
//! teardown: the session's lease goes with the drop, and nothing may resume the session while this process holds it. A
//! tab with work in progress stays: a turn that started since the ask, a message about to go out, or background work,
//! all of which would end with the backend.
//!
//! **Taken up.** A tab given here goes through the restore's own steps for one tab: its plan (the record is there and
//! no other window holds the lease), the trust gate, and the lease taken before the resume begins. A restore runs one at
//! a time, so given tabs wait their turn in order. A window that gave up on a tab can still be holding its lease: the
//! link of a window that is closing ends before its backends do, and a tab given back then is waited for, as long as
//! such a close can take. The answer comes once the tab has settled, so a window that dies while it connects leaves the
//! tab with whoever can still give it back.
//!
//! Every answer is a [`TabAnswer`], told to the window through its chat-move hook.

use super::*;
use eitri_core::saved_tabs::{SavedTab, SavedTabs};
use eitri_core::tab_set::MoveRefusedWhy;
use eitri_core::wm::feedback::A_TURN_STARTED_TEXT;
use eitri_core::wm::TabHandOver;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Why a given tab did not come up here, when the restore that tried has said the details in its toast.
const DID_NOT_COME_BACK: &str = "its session did not come back in that window";

/// Why a tab stays while its background work runs.
const BACKGROUND_RUNS: &str = "its background work is still running";

/// Why a tab stays while its agent works on something the panel cannot follow to its end.
const AGENT_WORKS: &str = "its agent is still working";

/// Why a tab stays while it waits for the trust question.
const WAITS_FOR_TRUST: &str = "it is waiting for the trust question";

/// How long a given tab waits for its session's lease: past the longest a closing window can keep a backend, after
/// which its process has ended and the lease with it.
const LEASE_WAIT: Duration = Duration::from_secs(SESSION_CLOSE_BACKSTOP.as_secs() + 2);

/// What the panel tells the window about a hand-over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabAnswer {
    /// The tab is closed here and its session let go; `tab` is what its next holder opens.
    Released { session: String, tab: SavedTab },
    /// The tab stays here.
    Kept { session: String, why: String },
    /// The tab given here has settled in this window.
    Adopted { session: String },
    /// The tab given here is not in this window.
    NotAdopted { session: String, why: String },
}

/// A tab given to this window that has not begun yet.
struct Given {
    tab: SavedTab,
    /// When its lease stops being waited for.
    lease_wait_ends: Instant,
}

struct Adopting {
    tab: TabId,
    session: String,
    /// Made for this tab, so it goes again when the tab does not come up.
    opened: bool,
}

/// The panel's hand-overs in flight.
#[derive(Default)]
pub(super) struct HandOvers {
    /// Tabs closed for a hand-over whose backends have not let go yet.
    releasing: BTreeMap<TabId, SavedTab>,
    /// Tabs given to this window and not begun yet, oldest first.
    given: VecDeque<Given>,
    adopting: Option<Adopting>,
}

impl HandOvers {
    /// Whether a teardown or a start is awaited: the tick then runs on its steady cadence.
    pub(super) fn is_waiting(&self) -> bool {
        !self.releasing.is_empty() || !self.given.is_empty() || self.adopting.is_some()
    }
}

fn answer(out: &mut Outbox, answer: TabAnswer) {
    out.effect(Effect::ChatMove(ChatMoveEvent::HandOver(answer)));
}

/// The session a tab holds, or one whose resume failed and which it keeps for `r`.
fn session_of(tab: &Tab) -> Option<String> {
    tab.provider_session_id().or_else(|| tab.failed_resume.clone())
}

/// Why `tab` cannot be let go of right now, its own turn apart. Each of these ends with the backend or the tab, and
/// none is reported by a change of the window's running work, so the asker is told rather than left waiting.
fn stays(state: &AgentPanelState, tab: TabId) -> Option<String> {
    if let Some(why) = state.tabs.move_blocker(tab) {
        return Some(why.to_string());
    }
    let t = state.tabs.get(tab)?;
    if t.background_calls() > 0 {
        return Some(BACKGROUND_RUNS.to_string());
    }
    if t.cli_unseen_busy() {
        return Some(AGENT_WORKS.to_string());
    }
    matches!(t.backend(), TabBackend::AwaitingTrust(_)).then(|| WAITS_FOR_TRUST.to_string())
}

/// What a hand-over of `tab` would take, or why it cannot go. A turn that runs is no refusal: the asker waits for its
/// end, and only then is the rest asked. The tab's agent and session are what cross: no agent stays alive across a
/// hand-over.
pub(super) fn facts(state: &AgentPanelState, tab: TabId) -> Result<TabHandOver, String> {
    if state.shutting_down {
        return Err("the window is closing".into());
    }
    let t = state.tabs.get(tab).ok_or_else(|| MoveRefusedWhy::NoTab.to_string())?;
    let running = t.turn_running();
    if !running {
        if let Some(why) = stays(state, tab) {
            return Err(why);
        }
    }
    Ok(TabHandOver {
        agent: t.agent().clone(),
        session: session_of(t),
        running,
    })
}

/// The tab holding `agent`'s `session`, as the file would list it, in no view: it goes to another window's own.
fn saved_tab(state: &AgentPanelState, agent: &AgentId, session: &str) -> Option<SavedTab> {
    let saved = state.tabs.saved_snapshot(&state.conversation_id)?;
    let of_session = |tab: &SavedTab| tab.agent == *agent && tab.provider_session_id == session;
    let tab = saved.tabs.into_iter().find(of_session)?;
    Some(SavedTab { view: None, ..tab })
}

/// Window 1 asks this window to let the tab holding `agent`'s `session` go. Answered here when it stays, and by
/// [`poll`] once its backend has let the session go.
pub(super) fn release(state: &mut AgentPanelState, agent: &AgentId, session: &str, out: &mut Outbox) {
    let kept = match let_go(state, agent, session, out) {
        Ok(()) => {
            poll(state, out);
            return;
        }
        Err(why) => why,
    };
    println!("[hand_over] a tab stays here: {kept}");
    answer(
        out,
        TabAnswer::Kept {
            session: session.to_string(),
            why: kept,
        },
    );
}

fn let_go(state: &mut AgentPanelState, agent: &AgentId, session: &str, out: &mut Outbox) -> Result<(), String> {
    if state.shutting_down {
        return Err("the window is closing".into());
    }
    let tab = state
        .tabs
        .tabs()
        .iter()
        .find(|t| t.agent() == agent && session_of(t).as_deref() == Some(session));
    let Some(tab) = tab else {
        return Err(MoveRefusedWhy::NoTab.to_string());
    };
    if tab.turn_running() {
        return Err(A_TURN_STARTED_TEXT.to_string());
    }
    let tab = tab.id;
    if let Some(why) = stays(state, tab) {
        return Err(why);
    }
    let saved = saved_tab(state, agent, session).ok_or_else(|| MoveRefusedWhy::NoTab.to_string())?;
    close_tab(state, tab, out).map_err(str::to_string)?;
    state.hand_overs.releasing.insert(tab, saved);
    Ok(())
}

/// A tab another window let go is given to this one: taken up as soon as no restore runs here.
pub(super) fn adopt(state: &mut AgentPanelState, tab: SavedTab, out: &mut Outbox) {
    let lease_wait_ends = Instant::now() + LEASE_WAIT;
    state.hand_overs.given.push_back(Given { tab, lease_wait_ends });
    poll(state, out);
}

/// Whether `given`'s session is still held by another process that may let go of it soon. A tab this window has open
/// holds the lease itself, and that goes as it always did.
fn lease_awaited(state: &AgentPanelState, given: &Given) -> bool {
    let (agent, session) = (&given.tab.agent, &given.tab.provider_session_id);
    Instant::now() < given.lease_wait_ends
        && state.tabs.tab_with_session(agent, session).is_none()
        && agent::lease::SessionLease::is_held(&agent.record_name(), &state.canonical_project_dir, session)
            .unwrap_or(false)
}

/// Each tick and after each step: a backend that let go answers its release, a tab that settled answers its adoption,
/// and the next given tab begins.
pub(super) fn poll(state: &mut AgentPanelState, out: &mut Outbox) {
    let teardown_of = |state: &AgentPanelState, tab: TabId| {
        let waits = |(of, _): &(Option<TabId>, mpsc::Receiver<()>)| *of == Some(tab);
        state.retiring.teardowns.iter().any(waits)
    };
    let let_go: Vec<TabId> = state
        .hand_overs
        .releasing
        .keys()
        .filter(|tab| !teardown_of(state, **tab))
        .copied()
        .collect();
    for tab in let_go {
        if let Some(saved) = state.hand_overs.releasing.remove(&tab) {
            println!("[hand_over] tab {}: released", tab.0);
            let session = saved.provider_session_id.clone();
            answer(out, TabAnswer::Released { session, tab: saved });
        }
    }
    settle(state, out);
    while state.hand_overs.adopting.is_none() && !restore_runs(state) {
        match state.hand_overs.given.front() {
            Some(given) if !lease_awaited(state, given) => {}
            _ => return,
        }
        if let Some(given) = state.hand_overs.given.pop_front() {
            begin(state, given.tab, out);
        }
    }
}

/// Whether a restore holds the window: another may not start until it has ended, one tab's included. One still to come
/// at the base page's first `ready` counts, since a tab started first would turn it away.
fn restore_runs(state: &AgentPanelState) -> bool {
    state.restore_run.is_some()
        || restore_waits_for_trust(state)
        || state.tabs.restore_question_open()
        || launch_restore_waits(state)
}

/// The restore's steps for one tab, the tab on screen when it is empty and a new one otherwise. Nobody pressed a key
/// here, so nothing is asked of a bypass tab: as at launch, it comes back in auto unless the configuration says bypass.
fn begin(state: &mut AgentPanelState, saved: SavedTab, out: &mut Outbox) {
    let (agent, session) = (saved.agent.clone(), saved.provider_session_id.clone());
    let before_any_start = state.tabs.is_pristine();
    if let Err(why) = started(state, saved, out) {
        println!("[hand_over] a given tab is not taken up: {why}");
        answer(out, TabAnswer::NotAdopted { session, why });
        return;
    }
    if before_any_start {
        state.group_tabs.hold_unopened();
    }
    if state.hand_overs.adopting.is_none() {
        println!("[hand_over] a given tab of {} is open here already", agent.label());
        answer(out, TabAnswer::Adopted { session });
    }
}

fn started(state: &mut AgentPanelState, saved: SavedTab, out: &mut Outbox) -> Result<(), String> {
    if state.shutting_down {
        return Err("the window is closing".into());
    }
    let (agent, session) = (saved.agent.clone(), saved.provider_session_id.clone());
    if state.backend_kind != BackendKind::Sidecar && !agent.is_external() {
        return Err("only the sidecar backend can resume sessions".to_string());
    }
    let from = state.tabs.active();
    if let Some(open) = state.tabs.tab_with_session(&agent, &session) {
        state.tabs.select(open);
        switch_payloads(state, out);
        return Ok(());
    }
    let saved = if agent.is_external() {
        SavedTabs::from_parts(Vec::new(), 0, vec![saved], Some(0))
    } else {
        SavedTabs::from_parts(vec![saved], 0, Vec::new(), None)
    };
    let plan = plan_of(state, &saved);
    if plan.items.is_empty() {
        let why = plan.skipped.first().map(|skipped| skipped.reason.clone());
        return Err(why.unwrap_or_else(|| DID_NOT_COME_BACK.to_string()));
    }
    let policy = if state.bypass_by_config {
        BypassPolicy::Keep
    } else {
        BypassPolicy::Downgrade
    };
    let RestoreStep::Go(go) = state.tabs.begin_restore(plan, policy) else {
        return Err(DID_NOT_COME_BACK.to_string());
    };
    let ResumeRoute::StartIn(tab) = state.tabs.route_resume(from, &agent, &session, false) else {
        return Err(DID_NOT_COME_BACK.to_string());
    };
    gate_start(state, tab, DeferredStart::Restore { go })?;
    state.hand_overs.adopting = Some(Adopting {
        tab,
        session,
        opened: tab != from,
    });
    switch_payloads(state, out);
    Ok(())
}

/// A tab being taken up has settled once its backend runs, or holds its resume for `r` (a question put off, a resume of
/// another agent that failed). One back to empty, or gone, did not come up.
fn settle(state: &mut AgentPanelState, out: &mut Outbox) {
    let Some(adopting) = &state.hand_overs.adopting else {
        return;
    };
    let came_up = match state.tabs.get(adopting.tab).map(Tab::backend) {
        Some(TabBackend::Live(_) | TabBackend::Failed { .. }) => true,
        Some(TabBackend::Starting(_) | TabBackend::AwaitingTrust(_)) => return,
        Some(TabBackend::NotStarted) | None => false,
    };
    let Some(Adopting { tab, session, opened }) = state.hand_overs.adopting.take() else {
        return;
    };
    if came_up {
        println!("[hand_over] tab {}: taken up", tab.0);
        answer(out, TabAnswer::Adopted { session });
        return;
    }
    println!("[hand_over] tab {}: {DID_NOT_COME_BACK}", tab.0);
    if opened && state.tabs.get(tab).is_some() {
        if let Err(why) = close_tab(state, tab, out) {
            eprintln!("[hand_over] tab {}: the tab made for it stays ({why})", tab.0);
        }
    }
    let why = DID_NOT_COME_BACK.to_string();
    answer(out, TabAnswer::NotAdopted { session, why });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use eitri_core::agent_bridge::SessionModeChoice::{Auto, Bypass};

    type Answers = Rc<RefCell<Vec<ChatMoveEvent>>>;

    /// A panel over `records`, with what it tells the window about hand-overs kept.
    fn panel(label: &str, records: &[&str]) -> (Rc<RefCell<AgentPanelState>>, PathBuf, Answers) {
        let (state, dir) = restore_panel(label, &[], records);
        let answers: Answers = Rc::default();
        {
            let mut s = state.borrow_mut();
            s.restore_source = None;
            s.page_mut(ViewId::BASE).document_ready = true;
            let into = answers.clone();
            s.chat_moves.hook = Some(Rc::new(move |event| into.borrow_mut().push(event)));
        }
        (state, dir, answers)
    }

    fn saved(state: &Rc<RefCell<AgentPanelState>>, session: &str) -> SavedTab {
        SavedTab {
            agent: AgentId::Claude,
            conversation_id: state.borrow().conversation_id.clone(),
            provider_session_id: session.to_string(),
            name: None,
            mode: Auto,
            view: None,
            window: None,
            turn_running: false,
        }
    }

    /// Waits for what a tab's conversation folds on its own thread.
    fn folded(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "the conversation never folded it");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The active tab, live on a backend that has opened `session`.
    fn live_tab(
        state: &Rc<RefCell<AgentPanelState>>,
        dir: &Path,
        session: &str,
    ) -> (TabId, std::sync::Arc<eitri_core::test_providers::RecordingProvider>) {
        let (provider, backend) = live_backend(dir);
        let tab = {
            let mut s = state.borrow_mut();
            let tab = s.tabs.active();
            s.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
            tab
        };
        provider.open_session(session, dir);
        folded(|| {
            state
                .borrow()
                .tabs
                .get(tab)
                .and_then(Tab::provider_session_id)
                .is_some()
        });
        (tab, provider)
    }

    /// A turn runs in `tab` from here on.
    fn turn_starts(
        state: &Rc<RefCell<AgentPanelState>>,
        tab: TabId,
        provider: &eitri_core::test_providers::RecordingProvider,
    ) {
        stream(provider, "t1", "working");
        folded(|| state.borrow().tabs.get(tab).is_some_and(Tab::turn_running));
    }

    fn answers_of(answers: &Answers) -> Vec<TabAnswer> {
        answers
            .borrow()
            .iter()
            .filter_map(|event| match event {
                ChatMoveEvent::HandOver(said) => Some(said.clone()),
                _ => None,
            })
            .collect()
    }

    fn released(state: &Rc<RefCell<AgentPanelState>>, session: &str) {
        stepped(state, |s, out| release(s, &AgentId::Claude, session, out));
    }

    fn given(state: &Rc<RefCell<AgentPanelState>>, tab: SavedTab) {
        stepped(state, |s, out| adopt(s, tab, out));
    }

    fn polled(state: &Rc<RefCell<AgentPanelState>>) {
        stepped(state, |s, out| {
            for tab in s.retiring.poll() {
                s.tabs.retire_done(tab);
            }
            poll(s, out);
        });
    }

    fn until(state: &Rc<RefCell<AgentPanelState>>, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            polled(state);
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_release_answers_only_once_the_backend_has_let_the_session_go() {
        let (state, dir, answers) = panel("hand-over-release", &["s-1"]);
        let (tab, provider) = live_tab(&state, &dir, "s-1");
        // A teardown of this tab the test holds open, as a sidecar's reap holds the real one.
        let (done, teardown) = mpsc::channel();
        state.borrow_mut().retiring.teardowns.push((Some(tab), teardown));
        released(&state, "s-1");
        assert!(state.borrow().tabs.get(tab).is_none(), "the tab is closed here");
        polled(&state);
        assert!(
            answers_of(&answers).is_empty(),
            "the backend may still hold the session, so nothing may resume it yet"
        );
        done.send(()).unwrap();
        assert!(until(&state, || !answers_of(&answers).is_empty()));
        let [TabAnswer::Released { session, tab: saved }] = &answers_of(&answers)[..] else {
            panic!("{:?}", answers_of(&answers));
        };
        assert_eq!(session, "s-1");
        assert_eq!(saved.provider_session_id, "s-1");
        assert_eq!(provider.closes(), 1, "the session was closed");
        assert!(!state.borrow().hand_overs.is_waiting());
    }

    #[test]
    fn a_turn_started_since_the_ask_keeps_the_tab() {
        let (state, dir, answers) = panel("hand-over-kept", &["s-1"]);
        let (tab, provider) = live_tab(&state, &dir, "s-1");
        turn_starts(&state, tab, &provider);
        released(&state, "s-1");
        assert_eq!(
            answers_of(&answers),
            [TabAnswer::Kept {
                session: "s-1".into(),
                why: "a turn started".into()
            }]
        );
        assert!(state.borrow().tabs.get(tab).is_some(), "the tab stays");
        shut_tabs(&state);
    }

    #[test]
    fn a_tab_waiting_for_the_trust_question_holds_on_to_its_window() {
        let (state, _dir, _answers) = panel("hand-over-trust", &[]);
        let tab = state.borrow().tabs.active();
        let start = DeferredStart::FirstSend {
            request_id: "r1".into(),
            first_turn: FirstTurn {
                wire: "hello".into(),
                typed: "hello".into(),
                typed_by_person: true,
                shell_line: false,
            },
        };
        state.borrow_mut().tabs.defer_start(tab, start).unwrap();
        assert_eq!(facts(&state.borrow(), tab), Err(WAITS_FOR_TRUST.to_string()));
    }

    /// A tab of `agent`, live on the session `session`.
    fn tab_of(
        state: &Rc<RefCell<AgentPanelState>>,
        dir: &Path,
        agent: AgentId,
        session: &str,
    ) -> (TabId, std::sync::Arc<eitri_core::test_providers::RecordingProvider>) {
        let (provider, backend) = live_backend(dir);
        let tab = {
            let mut s = state.borrow_mut();
            let tab = s.tabs.open(agent);
            s.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
            tab
        };
        provider.open_session(session, dir);
        folded(|| {
            state
                .borrow()
                .tabs
                .get(tab)
                .and_then(Tab::provider_session_id)
                .is_some()
        });
        (tab, provider)
    }

    #[test]
    fn two_agents_with_one_session_id_are_two_tabs() {
        let (state, dir, answers) = panel("hand-over-two-agents", &[]);
        let (first, _claude) = tab_of(&state, &dir, AgentId::Claude, "s1");
        let (second, _codex) = tab_of(&state, &dir, AgentId::Codex, "s1");
        assert_eq!(
            facts(&state.borrow(), second).unwrap().agent,
            AgentId::Codex,
            "the asker can name which it means"
        );
        stepped(&state, |s, out| release(s, &AgentId::Codex, "s1", out));
        assert!(until(&state, || !answers_of(&answers).is_empty()));
        assert!(state.borrow().tabs.get(first).is_some(), "the other agent's tab stays");
        assert!(state.borrow().tabs.get(second).is_none(), "the tab asked for went");
        let [TabAnswer::Released { tab: saved, .. }] = &answers_of(&answers)[..] else {
            panic!("{:?}", answers_of(&answers));
        };
        assert_eq!(saved.agent, AgentId::Codex, "and what is given on is that tab");
        shut_tabs(&state);
    }

    #[test]
    fn a_session_no_tab_here_holds_is_answered_kept() {
        let (state, _dir, answers) = panel("hand-over-no-tab", &[]);
        released(&state, "gone");
        assert!(matches!(&answers_of(&answers)[..], [TabAnswer::Kept { .. }]));
    }

    #[test]
    fn a_running_turn_is_no_refusal_for_the_asker_and_its_session_is_named() {
        let (state, dir, _answers) = panel("hand-over-facts", &["s-1"]);
        let (tab, provider) = live_tab(&state, &dir, "s-1");
        turn_starts(&state, tab, &provider);
        let read = facts(&state.borrow(), tab).unwrap();
        assert!(read.running);
        assert_eq!(read.session.as_deref(), Some("s-1"));
        shut_tabs(&state);
    }

    #[test]
    fn a_given_tab_resumes_in_the_empty_tab_and_answers_once_it_runs() {
        let (state, dir, answers) = panel("hand-over-adopt", &["s-1"]);
        let mut resumes = FakeResumes::default();
        let empty = state.borrow().tabs.active();
        with_resumes(&state, &mut resumes, || {
            given(&state, saved(&state, "s-1"));
            tick_until(&state, |_| restore_ran(&state));
        });
        assert_eq!(resumes.asked, ["s-1"], "the restore's own steps: the lease first");
        assert_eq!(resumes.leases.len(), 1);
        assert!(answers_of(&answers).is_empty(), "it has not come up yet");
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        polled(&state);
        assert_eq!(answers_of(&answers), [TabAnswer::Adopted { session: "s-1".into() }]);
        assert_eq!(state.borrow().tabs.active(), empty, "and it is the tab on screen");
        shut_tabs(&state);
    }

    #[test]
    fn a_given_tab_opens_beside_a_live_one_and_becomes_active() {
        let (state, dir, _answers) = panel("hand-over-beside", &["s-1", "s-2"]);
        let (live, _provider) = live_tab(&state, &dir, "s-1");
        let mut resumes = FakeResumes::default();
        with_resumes(&state, &mut resumes, || {
            given(&state, saved(&state, "s-2"));
            tick_until(&state, |_| state.borrow().tabs.tabs().len() == 2 && restore_ran(&state));
        });
        let s = state.borrow();
        assert_ne!(s.tabs.active(), live);
        let active = s.tabs.get(s.tabs.active()).unwrap();
        assert_eq!(active.provider_session_id().as_deref(), Some("s-2"));
        drop(s);
        shut_tabs(&state);
    }

    #[test]
    fn a_bypass_tab_comes_back_in_auto_unasked() {
        let (state, _dir, _answers) = panel("hand-over-bypass", &["s-1"]);
        let mut resumes = FakeResumes::default();
        let tab = SavedTab {
            mode: Bypass,
            ..saved(&state, "s-1")
        };
        with_resumes(&state, &mut resumes, || {
            given(&state, tab);
            tick_until(&state, |_| restore_ran(&state));
        });
        let s = state.borrow();
        assert_eq!(s.tabs.get(s.tabs.active()).unwrap().mode(), Auto);
        drop(s);
        shut_tabs(&state);
    }

    #[test]
    fn a_given_tab_waits_for_the_lease_of_a_window_still_closing() {
        let (state, dir, answers) = panel("hand-over-lease-wait", &["s-1"]);
        let canonical = dir.to_string_lossy().into_owned();
        let held = agent::lease::SessionLease::try_acquire("claude", &canonical, "s-1").unwrap();
        let mut resumes = FakeResumes::default();
        with_resumes(&state, &mut resumes, || {
            given(&state, saved(&state, "s-1"));
            polled(&state);
            assert!(answers_of(&answers).is_empty(), "the tab is not lost to the wait");
            assert!(!restore_ran(&state), "nothing resumes a session another process holds");
            assert!(state.borrow().hand_overs.is_waiting(), "so the tick keeps coming");
            drop(held);
            // As the tick does while `is_waiting`: a child another test thread is starting holds a copy of the lease's
            // descriptor until it execs, so one poll right after the drop can still find it held.
            tick_until(&state, |_| {
                polled(&state);
                restore_ran(&state)
            });
        });
        assert_eq!(resumes.asked, ["s-1"]);
        shut_tabs(&state);
    }

    #[test]
    fn a_tab_another_window_still_holds_is_not_taken_up() {
        let (state, dir, answers) = panel("hand-over-held", &["s-1"]);
        let canonical = dir.to_string_lossy().into_owned();
        let _held = agent::lease::SessionLease::try_acquire("claude", &canonical, "s-1").unwrap();
        given(&state, saved(&state, "s-1"));
        assert!(answers_of(&answers).is_empty());
        state.borrow_mut().hand_overs.given[0].lease_wait_ends = Instant::now();
        polled(&state);
        assert_eq!(
            answers_of(&answers),
            [TabAnswer::NotAdopted {
                session: "s-1".into(),
                why: eitri_core::tab_restore::HELD_ELSEWHERE.into()
            }]
        );
    }

    #[test]
    fn a_resume_that_fails_answers_not_adopted_and_the_tab_made_for_it_goes() {
        let (state, dir, answers) = panel("hand-over-failed", &["s-1", "s-2"]);
        let (_live, _provider) = live_tab(&state, &dir, "s-1");
        let mut resumes = FakeResumes::default();
        with_resumes(&state, &mut resumes, || {
            given(&state, saved(&state, "s-2"));
            tick_until(&state, |_| restore_ran(&state));
        });
        resumes.senders[0]
            .send(Err(BackendError {
                message: "no such session".to_string(),
                benign: false,
            }))
            .unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        polled(&state);
        assert!(matches!(&answers_of(&answers)[..], [TabAnswer::NotAdopted { .. }]));
        assert_eq!(state.borrow().tabs.tabs().len(), 1, "only the live tab is left");
        shut_tabs(&state);
    }

    #[test]
    fn tabs_given_while_a_restore_runs_wait_their_turn() {
        let (state, _dir, _answers) = panel("hand-over-queue", &["s-1", "s-2"]);
        let mut resumes = FakeResumes::default();
        with_resumes(&state, &mut resumes, || {
            given(&state, saved(&state, "s-1"));
            given(&state, saved(&state, "s-2"));
            tick_until(&state, |_| restore_ran(&state));
        });
        assert_eq!(resumes.asked, ["s-1"], "one at a time");
        assert!(state.borrow().hand_overs.is_waiting());
        shut_tabs(&state);
    }

    /// Each set the window tells window 1, as its sessions.
    fn reports(state: &Rc<RefCell<AgentPanelState>>) -> Rc<RefCell<Vec<Vec<String>>>> {
        let told: Rc<RefCell<Vec<Vec<String>>>> = Rc::default();
        let into = told.clone();
        handle_for(state).report_tabs(move |tabs: &SavedTabs| {
            let sessions = tabs.tabs.iter().map(|t| t.provider_session_id.clone());
            into.borrow_mut().push(sessions.collect());
        });
        told
    }

    /// Window 1 gives the window what it kept for it.
    fn kept(state: &Rc<RefCell<AgentPanelState>>, sessions: &[&str]) {
        let tabs = sessions.iter().map(|session| saved(state, session)).collect();
        handle_for(state).give_tabs(SavedTabs::from_parts(tabs, 0, Vec::new(), None));
    }

    /// A window started again has its own tabs coming, and a tab handed to it follows them: begun first, it would turn
    /// the launch's restore away, and the window's first report would leave them out of the file.
    #[test]
    fn a_tab_handed_to_a_new_window_waits_for_the_tabs_kept_for_it() {
        let (state, _dir, _answers) = panel("hand-over-after-given", &["s-1", "s-2", "s-3"]);
        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = false;
        let told = reports(&state);
        kept(&state, &["s-1", "s-2"]);
        given(&state, saved(&state, "s-3"));
        polled(&state);
        assert!(state.borrow().tabs.is_pristine(), "the page is not ready yet");
        assert!(state.borrow().hand_overs.is_waiting());

        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let result = launch_restore(&mut state.borrow_mut()).expect("the launch restore has them");
        assert!(matches!(result, RestoreResult::Deferred), "it is not turned away");
        polled(&state);
        remember_tabs(&mut state.borrow_mut());
        assert_eq!(*told.borrow(), [["s-1".to_string(), "s-2".to_string()]]);

        let mut resumes = FakeResumes::default();
        with_resumes(&state, &mut resumes, || {
            tick_until(&state, |_| restore_ran(&state));
        });
        assert_eq!(resumes.asked, ["s-1", "s-2"]);
        assert!(state.borrow().hand_overs.is_waiting(), "and its own turn comes after");
        shut_tabs(&state);
    }

    /// A tab handed over takes the offer off the dashboard, and the user has said nothing about the tabs on it: window 1
    /// goes on keeping them.
    #[test]
    fn a_tab_handed_to_a_window_with_an_offer_leaves_the_offered_tabs_in_its_report() {
        let (state, _dir, _answers) = panel("hand-over-offer", &["s-1", "s-2", "s-3"]);
        let told = reports(&state);
        kept(&state, &["s-1", "s-2"]);
        assert!(restore_is_on_offer(&state.borrow()));
        given(&state, saved(&state, "s-3"));
        assert!(!restore_is_on_offer(&state.borrow()));
        remember_tabs(&mut state.borrow_mut());
        assert_eq!(
            *told.borrow(),
            [["s-3".to_string(), "s-1".to_string(), "s-2".to_string()]]
        );
        shut_tabs(&state);
    }

    /// Starting a session of one's own leaves an offer behind, here as in a window with its own file.
    #[test]
    fn a_tab_handed_to_a_window_that_passed_its_offer_by_does_not_bring_it_back() {
        let (state, dir, _answers) = panel("hand-over-offer-passed", &["s-1", "s-2"]);
        let told = reports(&state);
        kept(&state, &["s-1"]);
        live_tab(&state, &dir, "s-own");
        assert_eq!(
            unopened_tabs(&state.borrow()),
            None,
            "passed by, so a close forgets them"
        );
        given(&state, saved(&state, "s-2"));
        remember_tabs(&mut state.borrow_mut());
        assert_eq!(*told.borrow(), [["s-own".to_string(), "s-2".to_string()]]);
        shut_tabs(&state);
    }

    /// A window that outlived window 1 and closed its last tab holds one empty tab, as a new window does. What a relink
    /// gives it dates from before that close, and what the user closed stays closed.
    #[test]
    fn a_window_that_emptied_itself_takes_nothing_given_at_a_relink() {
        let (state, dir, _answers) = panel("hand-over-emptied", &["s-1"]);
        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        let told = reports(&state);
        let (tab, _provider) = live_tab(&state, &dir, "s-1");
        remember_tabs(&mut state.borrow_mut());
        stepped(&state, |s, out| close_tab(s, tab, out)).unwrap();
        assert!(until(&state, || state.borrow().tabs.is_pristine()));
        remember_tabs(&mut state.borrow_mut());
        assert_eq!(*told.borrow(), [vec!["s-1".to_string()], Vec::new()]);

        kept(&state, &["s-1"]);
        assert!(state.borrow().restore_source.is_none());
        assert!(state.borrow().tabs.is_pristine(), "nothing came back");
    }

    /// Whether the restore's start has run: nothing waits for the trust check any more.
    fn restore_ran(state: &Rc<RefCell<AgentPanelState>>) -> bool {
        state.borrow().restore_run.is_some()
    }

    fn shut_tabs(state: &Rc<RefCell<AgentPanelState>>) {
        for tab in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}
