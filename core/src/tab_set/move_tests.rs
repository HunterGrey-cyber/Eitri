//! A tab moving between two tab sets, and the wait of every turn start, without a display. Both tab sets
//! live in this process and drive one provider double: the source's frozen conversation folds nothing
//! more, and the destination's takes up from there.

use super::*;
use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider, RecordingProvider};
use crate::turn_review::{Blocked, Partner, PresenceGuard, PresenceHolder, ReviewOptions, TurnReview};
use agent::{AgentConversation, AgentDomainEvent, PermissionDecision};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    dir
}

/// A directory under the build's own target directory, for a test's review and presence files.
fn scratch(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let exe = std::env::current_exe().unwrap();
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("tab-move-tests")
        .join(format!(
            "{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
    std::fs::create_dir_all(dir.join("project")).unwrap();
    std::fs::write(dir.join("project/main.rs"), "fn main() {}\n").unwrap();
    dir
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn set() -> TabSet {
    TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
}

/// `TabSet::freeze_for_move`, asked again while its conversation folds a batch, as a window asks again on
/// its next tick (the freeze itself never waits for the ingest thread).
fn freeze_settled(set: &mut TabSet, tab: TabId) -> Result<FrozenTab, MoveRefusedWhy> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match set.freeze_for_move(tab) {
            Err(MoveRefusedWhy::Folding) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            other => return other,
        }
    }
}

fn opened(dir: &Path) -> AgentDomainEvent {
    AgentDomainEvent::SessionOpened {
        session_id: "fake-session".into(),
        provider_session_id: "sess-move".into(),
        model: "m".into(),
        cwd: dir.to_string_lossy().into_owned(),
    }
}

fn started(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnStarted { turn_id: id.into() }
}

fn completed(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: id.into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

/// A `Write` into `.git/`, which a human answers in auto.
fn card(id: &str, tool_use_id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: Some(tool_use_id.into()),
        tool_name: "Write".into(),
        input: probe_input(),
        provider_prompt: None,
    }
}

fn probe_input() -> serde_json::Value {
    serde_json::json!({ "file_path": ".git/probe", "content": "x" })
}

/// The CLI's own question about a call the gate already let through.
fn cli_prompt(id: &str, tool_use_id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: Some(tool_use_id.into()),
        tool_name: "Write".into(),
        input: probe_input(),
        provider_prompt: Some(agent::ProviderPrompt {
            reason: Some("Claude requested permissions to edit .git/probe which is a sensitive file.".into()),
            description: Some(".git/probe".into()),
            ..Default::default()
        }),
    }
}

/// Gives `tab` a live Claude conversation over `provider`.
fn install(set: &mut TabSet, tab: TabId, provider: Arc<dyn agent::AgentProvider + Send + Sync>, dir: &Path) {
    let conversation =
        AgentConversation::create(provider, dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
}

fn live_tab(set: &mut TabSet, dir: &Path) -> (Arc<RecordingProvider>, TabId) {
    let provider = Arc::new(RecordingProvider::default());
    let tab = set.active();
    install(set, tab, provider.clone(), dir);
    (provider, tab)
}

fn pump_until(set: &mut TabSet, dir: &Path, what: &str, mut done: impl FnMut(&TabSet) -> bool) {
    until(what, || {
        set.pump(dir, true);
        done(set)
    });
}

/// Freezes `tab`, carries its records through JSON as the link carries them, and takes them up in
/// `dest`'s `view` over `provider`.
fn moved(
    source: &mut TabSet,
    tab: TabId,
    dest: &mut TabSet,
    provider: Arc<dyn agent::AgentProvider + Send + Sync>,
    view: ViewId,
) -> TabId {
    let frozen = freeze_settled(source, tab).expect("the tab freezes");
    let record: TabHandoff = serde_json::from_str(&serde_json::to_string(&frozen.tab).unwrap()).unwrap();
    let chat: FrozenChat =
        serde_json::from_str(&serde_json::to_string(&frozen.chat.expect("a live tab's chat")).unwrap()).unwrap();
    let backend = AgentBackend::adopt_moved(
        chat.core,
        chat.conversation,
        chat.backend,
        agent::MovedProvider {
            agent: provider,
            external: None,
            lease: None,
        },
    )
    .expect("the destination takes the conversation up");
    dest.adopt_moved(record, backend, view)
}

/// Everything of a tab a move must keep, as text, the fields' own values side by side.
fn kept(tab: &Tab) -> String {
    let s = &tab.session;
    let offers: BTreeMap<&String, String> = s.rule_offers.iter().map(|(k, r)| (k, r.to_rule_string())).collect();
    format!(
        "name {:?}\nmode {:?}\nagent {:?}\ncards {:?} unread {}\nfailed {} {:?}\nreview {:?}\nnotes {:?}\n\
         title {:?} {}\ntiers {:?}\nstart failure {}\nqueue {:?} {:?} {:?}\ndraft {:?}\noffers {:?}\n\
         running {}\nledger {:?}\ncreates {:?}\nseen {:?}\nclock {:?} {}\nhost {:?}\nhuman {:?}\nuser {:?}\ngates {:?}",
        tab.name,
        tab.mode,
        tab.agent,
        tab.attention.card_ids(),
        tab.attention.attention().unread,
        tab.failed_after_start,
        tab.failed_resume,
        tab.review_draft,
        tab.notes,
        s.title,
        s.title_from_record,
        s.project_tiers,
        s.reported_start_failure,
        s.queue,
        s.queue_error,
        s.queue_note,
        s.draft,
        offers,
        s.was_running,
        s.note_ledger,
        s.creates_file_unsent,
        s.rule_offers_seen,
        s.turn_clock,
        s.turn_times_unsent,
        s.host_answered,
        s.human_allowed,
        s.user_answered,
        tab.gate_log,
    )
}

/// A provider whose every turn first asks `probe` what the window looks like, and records the answer:
/// what holds at the moment the first byte of a turn reaches the agent.
struct Probed<P> {
    inner: Arc<P>,
    probe: Mutex<Option<Box<dyn Fn() -> bool + Send>>>,
    seen: Mutex<Vec<bool>>,
}

impl<P> Probed<P> {
    fn new(inner: P) -> Arc<Self> {
        Arc::new(Probed {
            inner: Arc::new(inner),
            probe: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn probe_with(&self, probe: impl Fn() -> bool + Send + 'static) {
        *self.probe.lock().unwrap() = Some(Box::new(probe));
    }

    fn look(&self) {
        let seen = self.probe.lock().unwrap().as_ref().is_some_and(|probe| probe());
        self.seen.lock().unwrap().push(seen);
    }

    fn seen(&self) -> Vec<bool> {
        self.seen.lock().unwrap().clone()
    }
}

macro_rules! delegate_provider {
    ($inner:ty, $send:item) => {
        impl agent::AgentProvider for Probed<$inner> {
            fn capabilities(&self) -> agent::ProviderCapabilities {
                agent::AgentProvider::capabilities(&*self.inner)
            }
            fn info(&self) -> agent::ProviderInfo {
                agent::AgentProvider::info(&*self.inner)
            }
            fn create_session(&self, request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
                agent::AgentProvider::create_session(&*self.inner, request)
            }
            fn resume_session(&self, request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
                agent::AgentProvider::resume_session(&*self.inner, request)
            }
            $send
            fn interrupt_turn(&self, request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
                agent::AgentProvider::interrupt_turn(&*self.inner, request)
            }
            fn resolve_permission(
                &self,
                request: agent::ResolvePermissionRequest,
            ) -> Result<(), agent::ProviderError> {
                agent::AgentProvider::resolve_permission(&*self.inner, request)
            }
            fn close_session(&self, request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
                agent::AgentProvider::close_session(&*self.inner, request)
            }
            fn pump(&self) -> Vec<AgentDomainEvent> {
                agent::AgentProvider::pump(&*self.inner)
            }
        }
    };
}

delegate_provider!(
    RecordingProvider,
    fn send_turn(&self, request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        self.look();
        agent::AgentProvider::send_turn(&*self.inner, request)
    }
);

delegate_provider!(
    ExternalRecordingProvider,
    fn send_turn(&self, request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        agent::AgentProvider::send_turn(&*self.inner, request)
    }
);

impl agent::ExternalProvider for Probed<ExternalRecordingProvider> {
    fn name(&self) -> &'static str {
        agent::ExternalProvider::name(&*self.inner)
    }
    fn resumes(&self) -> bool {
        agent::ExternalProvider::resumes(&*self.inner)
    }
    fn start(&self, req: agent::ExternalStart) -> Result<String, agent::ProviderError> {
        agent::ExternalProvider::start(&*self.inner, req)
    }
    fn send_turn_with(
        &self,
        request: agent::SendTurnRequest,
        policy: agent::TurnPolicy,
    ) -> Result<String, agent::ProviderError> {
        self.look();
        agent::ExternalProvider::send_turn_with(&*self.inner, request, policy)
    }
}

/// A window holding its presence in `review`, its thread running, under pid `pid`.
fn holding_presence(set: &mut TabSet, review: &Path, pid: u32) -> PresenceGuard {
    set.hold_presence(PresenceHolder::start(review.to_path_buf(), pid));
    let guard = set.presence_guard().expect("held");
    until("the presence lock", || {
        !matches!(guard.check(), Err(Blocked::NotHeld(_)))
    });
    guard
}

/// The window's own revert refuses as it does while a turn of this window runs.
fn refuses_as_running(guard: &PresenceGuard) -> bool {
    guard.check() == Err(Blocked::TurnRunningHere)
}

fn partner() -> Partner {
    Partner {
        pid: 100,
        start: 0,
        counts_as_other: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn adopt_moved_keeps_every_handed_over_field_through_the_first_tick_and_delivery() {
    let dir = workspace("move-keeps-fields");
    let mut source = set();
    let (provider, tab) = live_tab(&mut source, &dir);
    provider.queue_all(vec![
        opened(&dir),
        started("t1"),
        card("p-host", "tu-host"),
        card("p-user", "tu-user"),
        AgentDomainEvent::PermissionRequested {
            permission_id: "p-left".into(),
            tool_use_id: Some("tu-left".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "cargo test --all" }),
            provider_prompt: None,
        },
    ]);
    pump_until(&mut source, &dir, "three cards and the rule offered on one", |s| {
        let t = s.get(tab).unwrap();
        t.attention.card_ids().len() == 3 && !t.session.rule_offers.is_empty()
    });
    {
        let t = source.get_mut(tab).unwrap();
        t.name = Some("named".into());
        t.session.host_answered.insert("p-host".into());
        t.session.user_answered.insert("p-user".into());
        t.session.human_allowed.record("tu-approved", "Write", &probe_input());
        t.session.note_ledger.record(
            "tu-noted".into(),
            NoteCandidate::PromptNote("Claude Code asked — allowed in bypass".into()),
        );
        t.session.queue.push(Queued::held("next", "next".into(), 1));
        t.session.creates_file_unsent.insert("tu-new".into());
        t.replace_draft("half typed".into());
    }
    // What the window it leaves derives from those settles on its next tick, as it would before a move.
    source.pump(&dir, true);
    let before = kept(source.get(tab).unwrap());

    let mut dest = set();
    let arrived = moved(&mut source, tab, &mut dest, provider.clone(), ViewId::BASE);
    assert_eq!(
        dest.active(),
        arrived,
        "a tab moved into the base view is the one it shows"
    );
    // The first tick, the first delivery, and the tick after it.
    dest.pump(&dir, true);
    let payloads = dest.state_payloads(arrived);
    assert!(
        payloads.iter().any(|p| p.contains("\"snapshot\"")),
        "the first delivery is a snapshot: {payloads:?}"
    );
    dest.pump(&dir, true);

    let t = dest.get(arrived).unwrap();
    assert!(
        !t.session.human_allowed.is_empty(),
        "an approval of the running turn is kept"
    );
    assert!(!t.session.host_answered.is_empty() && !t.session.user_answered.is_empty());
    assert!(!t.session.note_ledger.is_empty(), "a note waiting for its call is kept");
    assert_eq!(
        kept(t),
        before,
        "every handed-over field is as it was in the window it left"
    );
    let _ = source.hand_over(tab);
}

#[test]
fn a_card_approved_before_the_move_answers_the_clis_own_prompt_after_it() {
    let dir = workspace("move-approved-card");
    let mut source = set();
    let (provider, tab) = live_tab(&mut source, &dir);
    provider.queue_all(vec![opened(&dir), started("t1"), card("p-gate", "tu-1")]);
    pump_until(&mut source, &dir, "the gate's card", |s| {
        s.get(tab).unwrap().attention.card_ids() == ["p-gate"]
    });
    source
        .answer_card(tab, ViewId::BASE, "p-gate", PermissionDecision::Allow)
        .expect("the card is answered");
    assert!(source.get(tab).unwrap().session.human_allowed.contains("tu-1"));

    let mut dest = set();
    let arrived = moved(&mut source, tab, &mut dest, provider.clone(), ViewId::BASE);
    dest.state_payloads(arrived);
    provider.queue(cli_prompt("p-cli", "tu-1"));
    pump_until(&mut dest, &dir, "the CLI's prompt answered", |_| {
        provider.decisions().iter().any(|(id, _)| id == "p-cli")
    });
    assert!(
        provider
            .decisions()
            .iter()
            .any(|(id, decision)| id == "p-cli" && *decision == PermissionDecision::Allow),
        "the approval given before the move answers the CLI's own prompt for the same call"
    );
    let t = dest.get(arrived).unwrap();
    assert!(
        !t.attention.card_ids().contains(&"p-cli".to_string()),
        "and no second card is drawn for it"
    );
    assert!(
        t.session.human_allowed.is_empty(),
        "the approval answered that one prompt"
    );
    let _ = source.hand_over(tab);
}

#[test]
fn hand_over_takes_no_end_snapshot_counts_no_retiring_and_drops_the_running_file() {
    let scratch = scratch("move-hand-over");
    let dir = scratch.join("project");
    let review_dir = scratch.join("state/eitri/review/0123456789abcdef");
    let jobs: Arc<Mutex<Vec<crate::turn_review::JobEvent>>> = Arc::default();
    let mut source = set();
    let hook_jobs = jobs.clone();
    source
        .install_turn_review(TurnReview::with_options(
            Some(scratch.join("state/review")),
            &dir,
            ReviewOptions {
                excludes: Some(None),
                on_job: Some(Arc::new(move |event| hook_jobs.lock().unwrap().push(event.clone()))),
                ..Default::default()
            },
        ))
        .unwrap();
    let pid = 4242;
    holding_presence(&mut source, &review_dir, pid);
    let (provider, tab) = live_tab(&mut source, &dir);
    provider.queue_all(vec![opened(&dir), started("t1")]);
    let running_file = review_dir.join(format!("running/{pid}.{}", tab.0));
    pump_until(&mut source, &dir, "the turn's base and its running file", |s| {
        running_file.exists()
            && s.turn_review()
                .unwrap()
                .turns("sess-move")
                .first()
                .is_some_and(|r| matches!(r.base, crate::turn_review::Snap::Taken { .. }))
    });

    let _frozen = freeze_settled(&mut source, tab).expect("the tab freezes");
    assert!(running_file.exists(), "a frozen tab's turn still holds its file");
    let left = source.hand_over(tab).expect("the tab leaves");
    assert!(source.get(tab).is_none());
    assert!(
        source.retiring.is_empty(),
        "nothing counts the handed-over tab as retiring"
    );
    until("the running file goes", || !running_file.exists());
    for _ in 0..5 {
        source.pump(&dir, true);
    }
    let ends: Vec<_> = jobs
        .lock()
        .unwrap()
        .iter()
        .filter(|event| {
            matches!(
                event,
                crate::turn_review::JobEvent::Starting {
                    kind: crate::turn_review::SnapshotKind::End,
                    ..
                }
            )
        })
        .cloned()
        .collect();
    assert!(ends.is_empty(), "no end snapshot is taken of the moved turn: {ends:?}");
    drop(left);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_base_view_left_without_tabs_by_a_move_gets_its_empty_tab() {
    let mut source = set();
    let only = source.active();
    source.get_mut(only).unwrap().replace_draft("going over".into());
    let frozen = freeze_settled(&mut source, only).expect("an empty tab moves too");
    assert!(frozen.chat.is_none() && !frozen.tab.has_session());
    let _ = source.hand_over(only).expect("the tab leaves");
    assert_eq!(source.tabs().len(), 1, "the base view gets an empty tab");
    let fresh = source.active_tab();
    assert_ne!(fresh.id, only);
    assert_eq!(fresh.number, 1);
    assert!(matches!(fresh.backend, TabBackend::NotStarted));

    let mut dest = set();
    let arrived = dest
        .adopt_moved_unstarted(frozen.tab, ViewId::BASE)
        .expect("an empty tab is taken up");
    assert_eq!(dest.get(arrived).unwrap().draft(), "going over");
    assert!(matches!(dest.get(arrived).unwrap().backend, TabBackend::NotStarted));
}

#[test]
fn a_moved_tab_waiting_for_the_trust_question_is_asked_again_where_it_lands() {
    let mut source = set();
    let tab = source.active();
    source
        .defer_start(
            tab,
            DeferredStart::FirstSend {
                request_id: "r1".into(),
                first_turn: FirstTurn {
                    wire: "hi".into(),
                    typed: "hi".into(),
                },
            },
        )
        .unwrap();
    source.ask_trust(tab).unwrap();
    let frozen = freeze_settled(&mut source, tab).expect("a tab waiting for the question moves");
    let mut dest = set();
    let arrived = dest.adopt_moved_unstarted(frozen.tab, ViewId::BASE).unwrap();
    match &dest.get(arrived).unwrap().backend {
        TabBackend::AwaitingTrust(waiting) => {
            assert_eq!(waiting.asking, None, "no question of the other page is current here");
            assert!(!waiting.recording);
            assert!(matches!(&waiting.start, DeferredStart::FirstSend { first_turn, .. } if first_turn.typed == "hi"));
        }
        _ => panic!("the start still waits for the question"),
    }
    assert_eq!(dest.awaiting_trust(), vec![arrived]);
}

#[test]
fn a_frozen_tab_takes_no_input_answers_no_card_and_comes_back_on_unfreeze() {
    let dir = workspace("move-frozen-input");
    let mut set = set();
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue_all(vec![opened(&dir), started("t1"), card("p1", "tu-1")]);
    pump_until(&mut set, &dir, "the card", |s| {
        s.get(tab).unwrap().attention.card_ids() == ["p1"]
    });
    let _frozen = freeze_settled(&mut set, tab).expect("the tab freezes");
    assert!(set.is_moving(tab));
    assert_eq!(
        set.queue_message(tab, "x", "x".into(), 1, true),
        Err(MOVING.to_string())
    );
    assert_eq!(
        set.answer_card(tab, ViewId::BASE, "p1", PermissionDecision::Allow)
            .unwrap_err()
            .message,
        MOVING
    );
    assert!(provider.decisions().is_empty(), "no card of a moving tab is answered");
    assert!(matches!(set.cycle_mode(tab, ViewId::BASE), Err(why) if why == MOVING));
    assert_eq!(set.freeze_for_move(tab).unwrap_err(), MoveRefusedWhy::AlreadyMoving);

    // Folded while frozen, delivered once the move is called off.
    provider.queue(completed("t1"));
    for _ in 0..5 {
        set.pump(&dir, true);
    }
    assert!(
        set.get(tab).unwrap().session.was_running,
        "a frozen tab delivers nothing"
    );
    set.unfreeze(tab);
    assert!(!set.is_moving(tab));
    let mut ended = Vec::new();
    until("the turn's end after the unfreeze", || {
        ended.extend(set.pump(&dir, true).turn_ended);
        ended.contains(&tab)
    });
    set.queue_message(tab, "after", "after".into(), 2, true)
        .expect("input again");
}

#[test]
fn what_cannot_move_is_refused_and_left_as_it_was() {
    let dir = workspace("move-refused");
    let mut set = set();
    let (_provider, tab) = live_tab(&mut set, &dir);
    let gate = set.begin_turn_send(tab);
    assert_eq!(set.freeze_for_move(tab).unwrap_err(), MoveRefusedWhy::Starting);
    set.end_start(gate.tab());
    assert_eq!(set.freeze_for_move(TabId(999)).unwrap_err(), MoveRefusedWhy::NoTab);

    let mut steered = set.get(tab).unwrap().session.queue.clone();
    steered.push(Queued {
        state: QueuedState::Steered {
            client_id: "eitri-1-1".into(),
            sent_at_ms: 1,
        },
        ..Queued::held("in the turn", "in the turn".into(), 1)
    });
    set.get_mut(tab).unwrap().session.queue = steered;
    assert_eq!(set.freeze_for_move(tab).unwrap_err(), MoveRefusedWhy::Steered);
    set.get_mut(tab).unwrap().session.queue.clear();

    let (_tx, rx) = mpsc::channel();
    let starting = set.open(AgentId::Claude);
    set.start(
        starting,
        PendingStart {
            request_id: "r".into(),
            result_rx: rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        },
    );
    assert_eq!(set.freeze_for_move(starting).unwrap_err(), MoveRefusedWhy::Starting);
    assert!(!set.is_moving(tab) && !set.is_moving(starting));
}

#[test]
fn the_last_pump_answers_a_folded_bypass_card_before_the_record_is_taken() {
    let dir = workspace("move-last-pump");
    let mut source = set();
    let (provider, tab) = live_tab(&mut source, &dir);
    source.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
    provider.queue_all(vec![opened(&dir), started("t1")]);
    pump_until(&mut source, &dir, "the turn", |s| {
        s.get(tab).unwrap().session.was_running
    });
    // Folded, and not yet pumped: the record must not be taken with it unanswered.
    provider.queue(card("p-bypass", "tu-b"));
    until("the card's fold", || {
        source
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("p-bypass")
    });
    assert!(provider.decisions().is_empty());

    let mut dest = set();
    let arrived = moved(&mut source, tab, &mut dest, provider.clone(), ViewId::BASE);
    assert_eq!(
        provider.decisions(),
        vec![("p-bypass".to_string(), PermissionDecision::Allow)],
        "answered once, by the window it left"
    );
    let t = dest.get(arrived).unwrap();
    assert!(
        t.session.host_answered.contains("p-bypass"),
        "and known there as answered"
    );
    dest.pump(&dir, true);
    assert!(
        !dest
            .get(arrived)
            .unwrap()
            .attention
            .card_ids()
            .contains(&"p-bypass".to_string()),
        "so it is no card where it lands"
    );
    assert_eq!(provider.decisions().len(), 1, "and it is not answered a second time");
    let _ = source.hand_over(tab);
}

#[test]
fn a_tab_about_to_send_is_in_the_running_set_as_starting() {
    let scratch = scratch("move-starting-set");
    let dir = scratch.join("project");
    let review_dir = scratch.join("state/eitri/review/0123456789abcdef");
    let mut set = set();
    let guard = holding_presence(&mut set, &review_dir, 4243);
    let (_provider, tab) = live_tab(&mut set, &dir);
    assert_eq!(guard.check(), Ok(()), "an idle window may revert");
    let gate = set.begin_turn_send(tab);
    assert!(set.starting.contains(&tab.0));
    assert!(refuses_as_running(&guard), "a starting tab counts as a running turn");
    let running_file = review_dir.join(format!("running/4243.{}", tab.0));
    until("the starting tab's running file", || running_file.exists());
    set.end_start(gate.tab());
    assert_eq!(guard.check(), Ok(()));
    until("the file goes with the start", || !running_file.exists());
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

fn shut_down(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// A window with its presence held in a fresh directory, and a live Claude tab over a probed provider
/// whose probe asks whether this window's revert refuses as for a running turn.
fn probed_claude(label: &str) -> (PathBuf, TabSet, TabId, Arc<Probed<RecordingProvider>>) {
    let scratch = scratch(label);
    let dir = scratch.join("project");
    let mut set = set();
    let guard = holding_presence(&mut set, &scratch.join("state/eitri/review/0123456789abcdef"), 4244);
    let provider = Probed::new(RecordingProvider::default());
    provider.probe_with(move || refuses_as_running(&guard));
    let tab = set.active();
    install(&mut set, tab, provider.clone(), &dir);
    provider.inner.queue(opened(&dir));
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().live().unwrap().projection().status == agent::ProjectionStatus::Running
    });
    (scratch, set, tab, provider)
}

#[test]
fn a_typed_prompt_waits_as_starting() {
    let (scratch, mut set, tab, provider) = probed_claude("move-typed");
    set.queue_message(tab, "hello", "hello".into(), 1, true).unwrap();
    let flush = set.flush_queue(tab).expect("an idle tab sends at once");
    assert!(flush.outcome.is_ok());
    assert_eq!(
        provider.seen(),
        vec![true],
        "the tab was running before the prompt reached the agent"
    );
    assert!(set.starting.contains(&tab.0), "and stays so until its turn starts");
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_queue_flush_at_a_turns_end_waits_as_starting() {
    let (scratch, mut set, tab, provider) = probed_claude("move-flush");
    let dir = scratch.join("project");
    provider.inner.queue(started("t1"));
    pump_until(&mut set, &dir, "the turn", |s| s.get(tab).unwrap().session.was_running);
    set.queue_message(tab, "after the turn", "after the turn".into(), 1, true)
        .unwrap();
    assert!(set.flush_queue(tab).is_none(), "it waits for the turn's end");
    provider.inner.queue(completed("t1"));
    let mut ended = Vec::new();
    until("the turn's end", || {
        ended.extend(set.pump(&dir, true).turn_ended);
        ended.contains(&tab)
    });
    let flush = set.flush_queue(tab).expect("the queue goes at the turn's end");
    assert!(flush.outcome.is_ok());
    assert_eq!(provider.seen(), vec![true]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_review_message_waits_as_starting() {
    let (scratch, mut set, tab, provider) = probed_claude("move-review");
    let flush = set
        .send_review(tab, "the review", 1)
        .unwrap()
        .expect("an idle tab sends the review at once");
    assert!(flush.outcome.is_ok());
    assert_eq!(provider.seen(), vec![true]);
    assert_eq!(provider.inner.turns(), vec!["the review".to_string()]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A window with its presence held, and a live tab of the external agent `agent` over a probed provider
/// named `name`, resumed from `resume` when given.
fn probed_external(
    label: &str,
    agent: AgentId,
    name: &'static str,
    resume: Option<&str>,
) -> (PathBuf, TabSet, TabId, Arc<Probed<ExternalRecordingProvider>>) {
    let scratch = scratch(label);
    let dir = scratch.join("project");
    let mut set = set();
    let guard = holding_presence(&mut set, &scratch.join("state/eitri/review/0123456789abcdef"), 4245);
    let provider = Probed::new(ExternalRecordingProvider::named(name, true));
    provider.probe_with(move || refuses_as_running(&guard));
    let backend = AgentBackend::start_external_with(
        agent,
        provider.clone(),
        &dir,
        empty_trust_plan(),
        SessionModeChoice::Auto,
        resume,
    )
    .expect("a scripted external start succeeds");
    let tab = set.active();
    let t = set.get_mut(tab).unwrap();
    t.agent = backend.agent();
    t.backend = TabBackend::Live(backend);
    (scratch, set, tab, provider)
}

#[test]
fn a_codex_turn_start_waits_as_starting() {
    let (scratch, mut set, tab, provider) = probed_external("move-codex", AgentId::Codex, "codex", None);
    set.queue_message(tab, "fix it", "fix it".into(), 1, true).unwrap();
    assert!(set.flush_queue(tab).expect("sent at once").outcome.is_ok());
    assert_eq!(
        provider.seen(),
        vec![true],
        "the turn/start reached Codex with the tab already running"
    );
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn an_acp_session_prompt_waits_as_starting() {
    let (scratch, mut set, tab, provider) = probed_external("move-acp", AgentId::Acp("gem".into()), "acp:gem", None);
    set.queue_message(tab, "explain", "explain".into(), 1, true).unwrap();
    assert!(set.flush_queue(tab).expect("sent at once").outcome.is_ok());
    assert_eq!(
        provider.seen(),
        vec![true],
        "the session/prompt reached the agent with the tab already running"
    );
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_resumed_sessions_first_prompt_waits_as_starting() {
    let (scratch, mut set, tab, provider) =
        probed_external("move-resumed", AgentId::Codex, "codex", Some("thread-resumed"));
    set.queue_message(tab, "go on", "go on".into(), 1, true).unwrap();
    assert!(set.flush_queue(tab).expect("sent at once").outcome.is_ok());
    assert_eq!(provider.seen(), vec![true]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);

    // A new session's first message goes the same way.
    let (scratch, mut set, tab, provider) = probed_claude("move-first-turn");
    let sent = set.send_first_turn(
        tab,
        "req-1",
        FirstTurn {
            wire: "first".into(),
            typed: "first".into(),
        },
    );
    assert!(matches!(sent, Some(Sent::First { ref request_id, outcome: Ok(()), .. }) if request_id == "req-1"));
    assert_eq!(provider.seen(), vec![true]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn starting_ends_on_turn_started_a_send_failure_and_a_lifecycle_event() {
    let dir = workspace("move-starting-ends");
    let mut set = set();
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue(opened(&dir));
    // Folded before any send: a `SessionOpened` folded after one would end its start too.
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().live().unwrap().projection().status == agent::ProjectionStatus::Running
    });
    let send = |set: &mut TabSet, text: &str| {
        set.queue_message(tab, text, text.into(), 1, true).unwrap();
        set.flush_queue(tab).expect("an idle tab sends")
    };

    // Its own `TurnStarted`.
    assert!(send(&mut set, "one").outcome.is_ok());
    assert!(set.starting.contains(&tab.0));
    provider.queue(started("t1"));
    pump_until(&mut set, &dir, "the start", |s| !s.starting.contains(&tab.0));
    assert!(
        set.get(tab).unwrap().turn_running(),
        "from here on the running turn holds it"
    );
    provider.queue(completed("t1"));
    pump_until(&mut set, &dir, "the end", |s| !s.get(tab).unwrap().turn_running());

    // A send that fails.
    provider.refuse_sends(true);
    assert!(send(&mut set, "two").outcome.is_err());
    assert!(!set.starting.contains(&tab.0), "a refused send starts nothing");
    provider.refuse_sends(false);
    set.get_mut(tab).unwrap().session.queue.clear();

    // A turn ending folded after it, with no start of its own.
    assert!(send(&mut set, "three").outcome.is_ok());
    provider.queue(completed("t-other"));
    pump_until(&mut set, &dir, "the ending", |s| !s.starting.contains(&tab.0));

    // A lifecycle event folded after it.
    assert!(send(&mut set, "four").outcome.is_ok());
    assert!(set.starting.contains(&tab.0));
    provider.queue(AgentDomainEvent::SessionUnavailable { reason: "gone".into() });
    pump_until(&mut set, &dir, "the session's loss", |s| !s.starting.contains(&tab.0));
}

#[test]
fn in_a_linked_window_the_prompt_waits_for_the_presence_confirmation() {
    let scratch = scratch("move-linked");
    let dir = scratch.join("project");
    let review_dir = scratch.join("state/eitri/review/0123456789abcdef");
    let pid = 4246;
    let mut set = set();
    let (holder, release) = PresenceHolder::start_parked(review_dir.clone(), pid);
    set.hold_presence(holder);
    assert!(set.set_partner(Some(partner())).is_empty());
    let provider = Probed::new(RecordingProvider::default());
    let tab = set.active();
    let running_file = review_dir.join(format!("running/{pid}.{}", tab.0));
    let seen_file = running_file.clone();
    provider.probe_with(move || seen_file.exists());
    install(&mut set, tab, provider.clone(), &dir);
    set.set_ui_wake(Arc::new(|| {}));
    assert!(!set.needs_polling(), "the backend announces its folds");

    set.queue_message(tab, "wait", "wait".into(), 1, true).unwrap();
    assert!(set.flush_queue(tab).is_none(), "held for the presence");
    assert!(set.send_is_held(tab) && set.starting.contains(&tab.0));
    assert!(
        set.needs_polling(),
        "the presence's confirmation raises no wake, so the tick keeps looking for it"
    );
    assert!(set.poll_turn_sends(Instant::now()).is_empty());
    assert!(provider.inner.turns().is_empty(), "nothing reached the agent");

    release.send(()).unwrap();
    let mut sent = Vec::new();
    until("the confirmed send", || {
        sent.extend(set.poll_turn_sends(Instant::now()));
        !sent.is_empty()
    });
    assert!(matches!(&sent[..], [Sent::Flushed { flush, .. }] if flush.outcome.is_ok()));
    assert_eq!(
        provider.seen(),
        vec![true],
        "the turn's file was held before its prompt went out"
    );
    assert_eq!(provider.inner.turns(), vec!["wait".to_string()]);
    assert!(!set.send_is_held(tab));
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_linked_window_whose_presence_cannot_confirm_sends_only_after_the_partners_ack() {
    let scratch = scratch("move-linked-ack");
    let dir = scratch.join("project");
    let review_dir = scratch.join("state/eitri/review/0123456789abcdef");
    let mut set = set();
    let (holder, _parked) = PresenceHolder::start_parked(review_dir.clone(), 4247);
    set.hold_presence(holder);
    let _ = set.set_partner(Some(partner()));
    let (provider, tab) = live_tab(&mut set, &dir);

    set.queue_message(tab, "acked", "acked".into(), 1, true).unwrap();
    assert!(set.flush_queue(tab).is_none());
    let later = Instant::now() + crate::turn_review::CONFIRM_WAIT;
    assert!(set.poll_turn_sends(later).is_empty());
    assert!(
        set.take_presence_false_owed(),
        "the partner is told this window counts as another"
    );
    assert!(!set.take_presence_false_owed(), "once");
    let sent = set.on_presence_ack(later + Duration::from_millis(10));
    assert!(matches!(&sent[..], [Sent::Flushed { flush, .. }] if flush.outcome.is_ok()));
    assert_eq!(provider.turns(), vec!["acked".to_string()]);

    // With no acknowledgement in time, the prompt is not sent and waits with the note.
    provider.queue(started("t1"));
    pump_until(&mut set, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    provider.queue(completed("t1"));
    pump_until(&mut set, &dir, "the end", |s| !s.get(tab).unwrap().turn_running());
    set.queue_message(tab, "unsent", "unsent".into(), 2, true).unwrap();
    assert!(set.flush_queue(tab).is_none());
    let late = Instant::now() + crate::turn_review::CONFIRM_WAIT * 3;
    let _ = set.poll_turn_sends(Instant::now() + crate::turn_review::CONFIRM_WAIT);
    let sent = set.poll_turn_sends(late);
    assert!(matches!(&sent[..], [Sent::NotSent { note, request_id: None, .. }] if note.starts_with("not sent:")));
    let t = set.get(tab).unwrap();
    assert_eq!(t.session.queue[0].text, "unsent", "it waits at the head of the queue");
    assert!(t
        .session
        .queue_error
        .as_deref()
        .is_some_and(|e| e.starts_with("not sent:")));
    assert!(!set.starting.contains(&tab.0));
    assert_eq!(provider.turns(), vec!["acked".to_string()]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn an_unlinked_window_sends_at_once_as_today() {
    let scratch = scratch("move-unlinked");
    let dir = scratch.join("project");
    let mut set = set();
    // The presence thread has not even started: alone, nothing waits for it.
    let (holder, _parked) = PresenceHolder::start_parked(scratch.join("state/eitri/review/0123456789abcdef"), 4248);
    set.hold_presence(holder);
    let (provider, tab) = live_tab(&mut set, &dir);
    set.queue_message(tab, "now", "now".into(), 1, true).unwrap();
    let flush = set.flush_queue(tab).expect("sent at once");
    assert!(flush.outcome.is_ok());
    assert_eq!(provider.turns(), vec!["now".to_string()]);
    assert!(!set.send_is_held(tab));

    // A link that ends lets a held prompt go.
    let _ = set.set_partner(Some(partner()));
    provider.queue(started("t1"));
    pump_until(&mut set, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    provider.queue(completed("t1"));
    pump_until(&mut set, &dir, "the end", |s| !s.get(tab).unwrap().turn_running());
    set.queue_message(tab, "held", "held".into(), 2, true).unwrap();
    assert!(set.flush_queue(tab).is_none(), "linked, it waits");
    let sent = set.set_partner(None);
    assert!(matches!(&sent[..], [Sent::Flushed { flush, .. }] if flush.outcome.is_ok()));
    assert_eq!(provider.turns(), vec!["now".to_string(), "held".to_string()]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_prompt_held_when_this_window_closes_its_link_stays_unsent_in_its_queue() {
    let scratch = scratch("move-closing");
    let dir = scratch.join("project");
    let mut set = set();
    // The presence thread never answers: the prompt waits for it.
    let (holder, _parked) = PresenceHolder::start_parked(scratch.join("state/eitri/review/0123456789abcdef"), 4249);
    set.hold_presence(holder);
    let (provider, tab) = live_tab(&mut set, &dir);
    let _ = set.set_partner(Some(partner()));
    set.queue_message(tab, "held", "held".into(), 1, true).unwrap();
    assert!(set.flush_queue(tab).is_none(), "linked, it waits");

    let sent = set.close_link();
    assert!(
        matches!(&sent[..], [Sent::NotSent { note, .. }] if note == crate::turn_review::CLOSING_NOTE),
        "one prompt, not sent, with the closing note ({} decided)",
        sent.len()
    );
    assert!(provider.turns().is_empty(), "nothing went to the agent");
    assert!(!set.is_linked());
    assert!(
        !set.starting.contains(&tab.0),
        "the tab no longer counts as starting a turn"
    );
    let t = set.get(tab).unwrap();
    assert_eq!(
        t.session.queue[0].text, "held",
        "the prompt waits at the head of the queue"
    );
    assert_eq!(t.session.queue_error.as_deref(), Some(crate::turn_review::CLOSING_NOTE));
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_linked_window_without_turn_review_still_announces_its_turns() {
    use crate::window_link::EdgeKind;
    let dir = workspace("move-untracked-edges");
    let mut set = set();
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue(opened(&dir));
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().provider_session_id().is_some()
    });
    provider.queue(started("t0"));
    pump_until(&mut set, &dir, "a turn", |s| s.get(tab).unwrap().turn_running());
    assert!(
        set.take_untracked_turn_edges(5).is_empty(),
        "an unlinked window announces nothing"
    );

    // Linked while that turn runs: it is named as running, and not announced again as a new start.
    let _ = set.set_partner(Some(partner()));
    let running = set.untracked_running_turn_edges();
    assert!(
        matches!(&running[..], [edge] if edge.session == "sess-move" && edge.edge == EdgeKind::Started),
        "{running:?}"
    );
    assert!(set.take_untracked_turn_edges(10).is_empty());
    provider.queue(completed("t0"));
    pump_until(&mut set, &dir, "its end", |s| !s.get(tab).unwrap().turn_running());
    let ended = set.take_untracked_turn_edges(20);
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(
        (ended[0].session.as_str(), ended[0].edge, ended[0].at_ns),
        ("sess-move", EdgeKind::Ended, 20)
    );

    provider.queue(started("t1"));
    pump_until(&mut set, &dir, "the next turn", |s| s.get(tab).unwrap().turn_running());
    let started_edges = set.take_untracked_turn_edges(crate::turn_review::monotonic_ns());
    assert!(
        matches!(&started_edges[..], [edge] if edge.edge == EdgeKind::Started && edge.at_ns > 20),
        "{started_edges:?}"
    );
    assert!(set.take_untracked_turn_edges(40).is_empty(), "each edge once");
    let _ = set.set_partner(None);
    assert!(
        set.take_untracked_turn_edges(50).is_empty(),
        "nothing after the link ends"
    );
    shut_down(&mut set);
}

fn typed(text: &str) -> FirstTurn {
    FirstTurn {
        wire: text.into(),
        typed: text.into(),
    }
}

fn text_delta(turn: &str, text: &str) -> AgentDomainEvent {
    AgentDomainEvent::ContentDelta {
        turn_id: turn.into(),
        kind: agent::ContentKind::Text,
        text: text.into(),
    }
}

/// A live tab whose session has opened, over a recording provider.
fn opened_tab(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    let dir = workspace(label);
    let mut set = set();
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue(opened(&dir));
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().live().unwrap().projection().status == agent::ProjectionStatus::Running
    });
    (dir, set, tab, provider)
}

#[test]
fn a_composers_prompt_waits_as_starting() {
    let (scratch, mut set, tab, provider) = probed_claude("move-composer");
    let sent = set.send_prompt(tab, "req-1", ViewId::BASE, typed("hello"));
    assert!(
        matches!(sent, Some(Sent::Prompt { ref request_id, outcome: Ok(()), .. }) if request_id == "req-1"),
        "an idle tab sends the composer's message at once"
    );
    assert_eq!(
        provider.seen(),
        vec![true],
        "the tab was running before the message reached the agent"
    );
    assert!(set.starting.contains(&tab.0), "and stays so until its turn starts");
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A close of a tab frozen for a move takes nothing out: its session is the other window's to take up, and
/// retiring it here would shut that session down under the move.
#[test]
fn a_frozen_tab_is_not_closed_under_its_move() {
    let (_dir, mut set, tab, _provider) = opened_tab("move-frozen-close");
    let _frozen = freeze_settled(&mut set, tab).expect("an idle tab freezes");
    assert!(set.remove(tab).is_none(), "a moving tab is not taken out");
    assert!(set.is_moving(tab), "it is still moving");
    assert!(set.get(tab).unwrap().live().is_some(), "its session stays for the move");
    set.unfreeze(tab);
    let removed = set.remove(tab).expect("a move called off leaves the tab closable");
    if let Some(retired) = removed.retired {
        retired.into_backend().shutdown();
    }
    shut_down(&mut set);
}

#[test]
fn a_frozen_tab_starts_no_turn_and_no_session() {
    let (_dir, mut set, tab, provider) = opened_tab("move-frozen-sends");
    let _frozen = freeze_settled(&mut set, tab).expect("an idle tab freezes");
    let refused = |sent: Option<Sent>| match sent {
        Some(Sent::Prompt { outcome: Err(e), .. }) | Some(Sent::First { outcome: Err(e), .. }) => {
            e.benign && e.message == MOVING
        }
        _ => false,
    };
    assert!(refused(set.send_prompt(tab, "r1", ViewId::BASE, typed("mid-move"))));
    assert!(refused(set.send_first_turn(tab, "r2", typed("mid-move"))));
    assert!(provider.turns().is_empty(), "nothing reached the agent of a moving tab");
    assert!(!set.starting.contains(&tab.0), "and no start was left behind");
    assert!(
        set.take_for_handoff(tab).is_none(),
        "a moving session is not handed to a terminal"
    );
    assert!(set.get(tab).unwrap().live().is_some(), "the session stays for the move");

    // A tab waiting for the trust question answers it only where it lands.
    let waiting = set.open(AgentId::Claude);
    set.defer_start(
        waiting,
        DeferredStart::FirstSend {
            request_id: "r3".into(),
            first_turn: typed("hi"),
        },
    )
    .unwrap();
    set.select(waiting);
    let nonce = set.ask_trust(waiting).unwrap();
    assert_eq!(set.check_trust_answer(waiting, nonce, ViewId::BASE), Ok(()));
    let _frozen = freeze_settled(&mut set, waiting).expect("a waiting tab freezes");
    assert_eq!(
        set.check_trust_answer(waiting, nonce, ViewId::BASE),
        Err(MOVING.to_string())
    );
    assert_eq!(
        set.mark_recording(waiting, nonce, ViewId::BASE),
        Err(MOVING.to_string())
    );
    assert!(
        set.take_deferred(waiting).is_none(),
        "no start of it is handed over here"
    );
    assert!(matches!(
        set.get(waiting).unwrap().backend,
        TabBackend::AwaitingTrust(AwaitingTrust { recording: false, .. })
    ));

    // An empty tab puts no start off.
    let empty = set.open(AgentId::Claude);
    let _frozen = freeze_settled(&mut set, empty).expect("an empty tab freezes");
    assert_eq!(
        set.defer_start(
            empty,
            DeferredStart::FirstSend {
                request_id: "r4".into(),
                first_turn: typed("hi"),
            },
        ),
        Err(MOVING.to_string())
    );
    assert!(matches!(set.get(empty).unwrap().backend, TabBackend::NotStarted));
    shut_down(&mut set);
}

#[test]
fn a_second_message_while_one_is_held_is_refused_and_the_first_still_goes() {
    let scratch = scratch("move-two-prompts");
    let dir = scratch.join("project");
    let review_dir = scratch.join("state/eitri/review/0123456789abcdef");
    let mut set = set();
    let (holder, release) = PresenceHolder::start_parked(review_dir, 4249);
    set.hold_presence(holder);
    assert!(set.set_partner(Some(partner())).is_empty());
    let (provider, tab) = live_tab(&mut set, &dir);

    assert!(
        set.send_prompt(tab, "r1", ViewId::BASE, typed("first")).is_none(),
        "held for the presence"
    );
    let second = set.send_prompt(tab, "r2", ViewId::BASE, typed("second"));
    assert!(
        matches!(&second, Some(Sent::Prompt { request_id, outcome: Err(e), .. })
            if request_id == "r2" && e.benign && e.message == SEND_HELD),
        "a second message does not take the held one's place"
    );
    queue_and_flush_wait(&mut set, tab);

    release.send(()).unwrap();
    let mut sent = Vec::new();
    until("the held message", || {
        sent.extend(set.poll_turn_sends(Instant::now()));
        !sent.is_empty()
    });
    assert!(
        matches!(&sent[..], [Sent::Prompt { request_id, outcome: Ok(()), .. }] if request_id == "r1"),
        "the first message goes out, answered under its own request"
    );
    assert_eq!(provider.turns(), vec!["first".to_string()]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A message queued while another of the tab is held or starting waits for that turn's end.
fn queue_and_flush_wait(set: &mut TabSet, tab: TabId) {
    set.queue_message(tab, "queued", "queued".into(), 1, true).unwrap();
    assert!(
        set.flush_queue(tab).is_none(),
        "the queue waits for the turn already on its way"
    );
    set.get_mut(tab).unwrap().session.queue.clear();
}

#[test]
fn a_refused_second_message_leaves_the_first_ones_start() {
    let (_dir, mut set, tab, provider) = opened_tab("move-second-refused");
    let first = set.send_prompt(tab, "r1", ViewId::BASE, typed("one"));
    assert!(matches!(first, Some(Sent::Prompt { outcome: Ok(()), .. })));
    assert!(set.starting.contains(&tab.0));
    // Its `TurnStarted` is not folded yet: the agent refuses a second turn.
    provider.refuse_sends(true);
    let second = set.send_prompt(tab, "r2", ViewId::BASE, typed("two"));
    assert!(matches!(second, Some(Sent::Prompt { outcome: Err(_), .. })));
    assert!(
        set.starting.contains(&tab.0),
        "the first message's turn is still starting, and still counted as running"
    );
    queue_and_flush_wait(&mut set, tab);
    provider.refuse_sends(false);
    shut_down(&mut set);
}

#[test]
fn a_turn_that_started_and_completed_within_a_resync_ends_its_start() {
    let (dir, mut set, tab, provider) = opened_tab("move-resync-completed");
    let sent = set.send_prompt(tab, "r1", ViewId::BASE, typed("quick"));
    assert!(matches!(sent, Some(Sent::Prompt { outcome: Ok(()), .. })));
    assert!(set.starting.contains(&tab.0));
    // The whole turn, more events than the UI queue holds, folded before the next pump.
    let mut batch = vec![started("t1")];
    batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
    batch.push(completed("t1"));
    provider.queue_all(batch);
    until("the turn's end folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .last_turn_end_seq
            .is_some()
    });
    let out = set.pump(&dir, true);
    let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
    assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
    assert!(
        !set.starting.contains(&tab.0),
        "a turn that completed among the dropped events ends the start"
    );
    shut_down(&mut set);
}

#[test]
fn a_move_given_up_while_waiting_for_the_session_id_waits_afresh_later() {
    let mut set = set();
    let tab = set.active();
    let t0 = Instant::now();
    assert_eq!(set.unbound_wait(tab, t0), t0);
    let within = t0 + Duration::from_millis(1500);
    assert_eq!(
        set.unbound_wait(tab, within),
        t0,
        "one attempt asking again waits from its start"
    );
    let given_up = within + Duration::from_secs(300);
    assert_eq!(
        set.unbound_wait(tab, given_up),
        given_up,
        "an attempt long after the last refusal is a new one"
    );
}
