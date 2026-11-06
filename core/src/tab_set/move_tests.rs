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
pub(super) fn freeze_settled(set: &mut TabSet, tab: TabId) -> Result<FrozenTab, MoveRefusedWhy> {
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
    take_up(frozen, dest, provider, view)
}

/// What a freeze gave, through JSON as the link carries it, taken up in `dest`'s `view` over `provider`.
pub(super) fn take_up(
    frozen: FrozenTab,
    dest: &mut TabSet,
    provider: Arc<dyn agent::AgentProvider + Send + Sync>,
    view: ViewId,
) -> TabId {
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
         running {}\nledger {:?}\ncreates {:?}\nseen {:?}\nclock {:?} {:?} {}\nhost {:?}\nhuman {:?}\nuser {:?}\ngates {:?}",
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
        s.last_began_turn,
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

fn partner_of(pid: u32) -> Partner {
    Partner { pid, ..partner() }
}

#[test]
fn a_window_stays_linked_until_its_last_partner_goes() {
    let mut set = set();
    set.add_partner(partner_of(100));
    set.add_partner(partner_of(200));
    assert!(set.is_linked());
    assert!(set.remove_partner(100, false).is_empty());
    assert!(set.is_linked(), "the other link is still up");
    assert!(set.remove_partner(200, false).is_empty());
    assert!(!set.is_linked());
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
                    typed_by_person: true,
                    wire: "hi".into(),
                    typed: "hi".into(),
                    shell_line: false,
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
        set.queue_message(tab, "x", "x".into(), 1, true, agent::PromptOrigin::Typed),
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
    set.queue_message(tab, "after", "after".into(), 2, true, agent::PromptOrigin::Typed)
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
    set.queue_message(tab, "hello", "hello".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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
    set.queue_message(
        tab,
        "after the turn",
        "after the turn".into(),
        1,
        true,
        agent::PromptOrigin::Typed,
    )
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
    set.queue_message(tab, "fix it", "fix it".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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
    set.queue_message(tab, "explain", "explain".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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
    set.queue_message(tab, "go on", "go on".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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
            typed_by_person: true,
            wire: "first".into(),
            typed: "first".into(),
            shell_line: false,
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
        set.queue_message(tab, text, text.into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
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

    set.queue_message(tab, "wait", "wait".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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

    set.queue_message(tab, "acked", "acked".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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
    set.queue_message(tab, "unsent", "unsent".into(), 2, true, agent::PromptOrigin::Typed)
        .unwrap();
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
    set.queue_message(tab, "now", "now".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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
    set.queue_message(tab, "held", "held".into(), 2, true, agent::PromptOrigin::Typed)
        .unwrap();
    assert!(set.flush_queue(tab).is_none(), "linked, it waits");
    let sent = set.set_partner(None);
    assert!(matches!(&sent[..], [Sent::Flushed { flush, .. }] if flush.outcome.is_ok()));
    assert_eq!(provider.turns(), vec!["now".to_string(), "held".to_string()]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A card that arrives while a composer's message waits for the presence holds that message when the
/// presence lets it go, as it holds one typed while the card waits: nothing goes into a session stopped
/// on the user.
#[test]
fn a_prompt_released_by_the_presence_waits_behind_a_card_that_arrived_meanwhile() {
    let scratch = scratch("move-held-card");
    let dir = scratch.join("project");
    let mut set = set();
    // The presence thread never answers: the prompt waits until the link ends.
    let (holder, _parked) = PresenceHolder::start_parked(scratch.join("state/eitri/review/0123456789abcdef"), 4250);
    set.hold_presence(holder);
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue(opened(&dir));
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().live().unwrap().projection().status == agent::ProjectionStatus::Running
    });
    let _ = set.set_partner(Some(partner()));
    assert!(set
        .send_prompt(tab, "req-1", ViewId::BASE, typed("and the docs"))
        .is_none());
    assert!(set.send_is_held(tab), "linked, it waits for the presence");

    provider.queue(cli_prompt("card-1", "tool-1"));
    pump_until(&mut set, &dir, "the card", |s| s.card_waits(tab));
    let sent = set.set_partner(None);
    assert!(
        matches!(&sent[..], [Sent::Prompt { outcome: Ok(()), .. }]),
        "the page is told it was taken"
    );
    assert!(
        provider.turns().is_empty(),
        "nothing reached the agent while the card waits"
    );
    assert!(!set.send_is_held(tab) && !set.starting.contains(&tab.0));
    assert_eq!(queued_texts(&set, tab), [("and the docs".to_string(), false)]);
    assert!(set.get(tab).unwrap().session.queue[0].behind_card);

    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission("card-1", PermissionDecision::Allow)
        .expect("the card is answered");
    provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "card-1".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    pump_until(&mut set, &dir, "the card's answer", |s| !s.card_waits(tab));
    let released = set.release_after_card();
    assert_eq!(released.len(), 1, "it goes when the card is answered");
    assert_eq!(provider.turns(), ["and the docs"]);
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

fn phone() -> RemoteAsker {
    RemoteAsker {
        device_id: "d1".into(),
        device: "Pixel 8".into(),
        command_id: "s1".into(),
        conn: Some(7),
    }
}

/// A linked window whose presence never answers, with a live Claude tab in Auto: a send waits for the presence
/// until the link ends.
fn linked_with_a_waiting_presence(label: &str, pid: u32) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    let scratch = scratch(label);
    let dir = scratch.join("project");
    let mut set = set();
    let (holder, _parked) = PresenceHolder::start_parked(scratch.join("state/eitri/review/0123456789abcdef"), pid);
    set.hold_presence(holder);
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue(opened(&dir));
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().live().unwrap().projection().status == agent::ProjectionStatus::Running
    });
    set.get_mut(tab).unwrap().set_mode_for_test(SessionModeChoice::Auto);
    let _ = set.set_partner(Some(partner()));
    (scratch, set, tab, provider)
}

/// A phone's send let go by the presence after the desk widened the tab's mode is refused, as the queue refuses
/// one: nothing reaches the agent, and the phone is told why.
#[test]
fn a_phone_send_released_by_the_presence_is_refused_once_the_mode_widened() {
    let (scratch, mut set, tab, provider) = linked_with_a_waiting_presence("move-remote-widened", 4252);
    set.send_remote(tab, "phone words", phone()).unwrap();
    assert!(set.send_is_held(tab), "linked, it waits for the presence");
    set.get_mut(tab).unwrap().set_mode_for_test(SessionModeChoice::Bypass);
    let _ = set.set_partner(None);
    assert!(!set.remote_send_waits(tab), "no call was made");
    assert!(provider.turns().is_empty(), "nothing reached the agent in Bypass");
    let refused = set.take_remote_refused();
    assert_eq!(refused.len(), 1);
    assert_eq!(
        (refused[0].accepted_mode, refused[0].mode),
        (SessionModeChoice::Auto, SessionModeChoice::Bypass)
    );
    assert!(!set.send_is_held(tab) && !set.starting.contains(&tab.0));
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A card that arrives while a phone's send waits for the presence holds the send back: the phone is told it did
/// not go, and nothing goes into a session stopped on the user.
#[test]
fn a_phone_send_released_by_the_presence_is_not_sent_behind_a_card_that_arrived_meanwhile() {
    let (scratch, mut set, tab, provider) = linked_with_a_waiting_presence("move-remote-card", 4253);
    let dir = scratch.join("project");
    set.send_remote(tab, "phone words", phone()).unwrap();
    assert!(set.send_is_held(tab));
    provider.queue(cli_prompt("card-1", "tool-1"));
    pump_until(&mut set, &dir, "the card", |s| s.card_waits(tab));
    let _ = set.set_partner(None);
    assert!(!set.remote_send_waits(tab), "no call was made");
    let told = set.poll_remote_sends();
    assert!(
        matches!(
            &told[..],
            [RemoteSent {
                outcome: Err(_),
                same_session: true,
                ..
            }]
        ),
        "the phone is told: {told:?}"
    );
    assert!(
        provider.turns().is_empty(),
        "nothing reached the agent while the card waits"
    );
    assert!(set.queue_texts(tab).is_empty(), "never put in the desk's queue");
    assert!(!set.send_is_held(tab) && !set.starting.contains(&tab.0));
    shut_down(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A phone's send still waiting for the presence when its tab closes is answered: it never went.
#[test]
fn a_phone_send_held_when_its_tab_closes_is_answered() {
    let (scratch, mut set, tab, provider) = linked_with_a_waiting_presence("move-remote-closed", 4254);
    set.send_remote(tab, "phone words", phone()).unwrap();
    assert!(set.send_is_held(tab));
    let removed = set.remove(tab).expect("the tab closes");
    if let Some(retired) = removed.retired {
        retired.into_backend().shutdown();
    }
    let told = set.poll_remote_sends();
    assert_eq!(told.len(), 1, "the phone is told: {told:?}");
    assert_eq!(told[0].tab, tab);
    assert_eq!(told[0].asker, phone());
    assert!(told[0].outcome.is_err());
    assert!(provider.turns().is_empty());
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
    set.queue_message(tab, "held", "held".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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

#[test]
fn a_turn_that_ran_between_two_looks_is_still_announced() {
    use crate::window_link::EdgeKind;
    let dir = workspace("move-untracked-brief");
    let mut set = set();
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue(opened(&dir));
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().provider_session_id().is_some()
    });
    let _ = set.set_partner(Some(partner()));
    assert!(set.take_untracked_turn_edges(10).is_empty());

    // The ingestion thread folds the whole turn with no tick in between, so no look sees it running.
    provider.queue(started("t0"));
    until("the turn", || set.get(tab).unwrap().turn_running());
    provider.queue(completed("t0"));
    until("its end", || !set.get(tab).unwrap().turn_running());
    set.pump(&dir, true);
    let edges = set.take_untracked_turn_edges(20);
    assert_eq!(
        edges
            .iter()
            .map(|edge| (edge.session.as_str(), edge.edge, edge.at_ns))
            .collect::<Vec<_>>(),
        [("sess-move", EdgeKind::Started, 20), ("sess-move", EdgeKind::Ended, 20)],
        "it started and ended here, in that order"
    );
    assert!(set.take_untracked_turn_edges(30).is_empty(), "each edge once");

    // A turn a look saw running is announced by the look, and only by it, wherever its start was delivered.
    provider.queue(started("t1"));
    until("the next turn", || set.get(tab).unwrap().turn_running());
    let seen = set.take_untracked_turn_edges(crate::turn_review::monotonic_ns());
    assert!(
        matches!(&seen[..], [edge] if edge.edge == EdgeKind::Started),
        "{seen:?}"
    );
    set.pump(&dir, true);
    assert!(set.take_untracked_turn_edges(40).is_empty(), "the look told it already");
    provider.queue(completed("t1"));
    pump_until(&mut set, &dir, "its end", |s| !s.get(tab).unwrap().turn_running());
    let ended = set.take_untracked_turn_edges(50);
    assert!(
        matches!(&ended[..], [edge] if edge.edge == EdgeKind::Ended),
        "{ended:?}"
    );
    shut_down(&mut set);
}

fn typed(text: &str) -> FirstTurn {
    FirstTurn {
        typed_by_person: true,
        wire: text.into(),
        typed: text.into(),
        shell_line: false,
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
    set.queue_message(tab, "queued", "queued".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
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
    assert!(
        prompt_went_through(second),
        "the refusal is not an error: the message waits to steer into the first one's turn"
    );
    assert_eq!(queued_texts(&set, tab), [("two".to_string(), false)]);
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

/// A live tab whose session has opened over `provider`.
fn opened_over(label: &str, provider: RecordingProvider) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    let dir = workspace(label);
    let mut set = set();
    let provider = Arc::new(provider);
    let tab = set.active();
    install(&mut set, tab, provider.clone(), &dir);
    provider.queue(opened(&dir));
    pump_until(&mut set, &dir, "the session", |s| {
        s.get(tab).unwrap().live().unwrap().projection().status == agent::ProjectionStatus::Running
    });
    (dir, set, tab, provider)
}

fn user_prompts(set: &TabSet, tab: TabId) -> Vec<String> {
    set.get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .user_prompts
        .iter()
        .map(|p| p.text.clone())
        .collect()
}

fn queued_texts(set: &TabSet, tab: TabId) -> Vec<(String, bool)> {
    set.get(tab)
        .unwrap()
        .session
        .queue
        .iter()
        .map(|q| (q.text.clone(), !q.is_held()))
        .collect()
}

fn prompt_went_through(sent: Option<Sent>) -> bool {
    matches!(sent, Some(Sent::Prompt { outcome: Ok(()), .. }))
}

#[test]
fn close_asks_while_claude_code_works_unseen() {
    let (dir, mut set, tab, provider) = opened_tab("guards-close-unseen");
    assert!(!set.close_needs_confirm(tab), "an idle tab closes without a question");
    assert!(!set.close_facts(tab).unwrap().unseen_work);

    provider.queue(AgentDomainEvent::CliWorkUnseen);
    pump_until(&mut set, &dir, "the unseen work", |s| {
        s.get(tab).unwrap().cli_unseen_busy()
    });
    assert!(!set.get(tab).unwrap().turn_running(), "no turn is shown running");
    assert!(set.close_needs_confirm(tab));
    assert_eq!(set.running_count(), 0);
    assert_eq!(
        set.running_or_cli_busy_count(),
        1,
        "a window's summary counts the turn Claude Code started by itself"
    );
    let facts = set.close_facts(tab).unwrap();
    assert!(facts.unseen_work);
    assert!(
        crate::tabs::close_prompt(&facts)
            .iter()
            .any(|line| line == "Claude Code is working where this panel cannot show it"),
        "{:?}",
        crate::tabs::close_prompt(&facts)
    );

    provider.queue(AgentDomainEvent::TurnCompleted {
        turn_id: "cli-turn".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    pump_until(&mut set, &dir, "the end of the unseen work", |s| {
        !s.get(tab).unwrap().cli_unseen_busy()
    });
    assert!(!set.close_needs_confirm(tab));
    assert_eq!(set.running_or_cli_busy_count(), 0);
    shut_down(&mut set);
}

#[test]
fn a_typed_message_waits_behind_a_pending_card() {
    let (dir, mut set, tab, provider) = opened_tab("guards-card-wait");
    provider.queue(cli_prompt("card-1", "tool-1"));
    pump_until(&mut set, &dir, "the card", |s| {
        !s.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .is_empty()
    });

    let sent = set.send_prompt(tab, "req-1", ViewId::BASE, typed("and the docs"));
    assert!(prompt_went_through(sent), "the page is told it was taken");
    assert!(
        provider.turns().is_empty(),
        "nothing reached the agent while the card waits"
    );
    assert!(!set.starting.contains(&tab.0), "and no start was begun");
    assert_eq!(queued_texts(&set, tab), [("and the docs".to_string(), false)]);
    assert!(set.release_after_card().is_empty(), "it stays while the card waits");

    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission("card-1", PermissionDecision::Allow)
        .expect("the card is answered");
    provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "card-1".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    pump_until(&mut set, &dir, "the card's answer", |s| {
        s.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .is_empty()
    });
    let released = set.release_after_card();
    assert_eq!(released.len(), 1, "it goes when the card is answered");
    assert!(released[0].1.outcome.is_ok());
    assert_eq!(provider.turns(), ["and the docs"]);
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down(&mut set);
}

/// A background agent's messages keep the note of unseen work up for as long as the agent runs, which
/// can outlast any turn: a message held behind a card goes when the card is answered all the same.
#[test]
fn a_message_behind_a_card_goes_when_answered_while_claude_code_works_unseen() {
    let (dir, mut set, tab, provider) = opened_tab("guards-card-unseen");
    provider.queue(AgentDomainEvent::CliWorkUnseen);
    provider.queue(cli_prompt("card-1", "tool-1"));
    pump_until(&mut set, &dir, "the card and the unseen work", |s| {
        s.card_waits(tab) && s.get(tab).unwrap().cli_unseen_busy()
    });
    assert!(prompt_went_through(set.send_prompt(
        tab,
        "req-1",
        ViewId::BASE,
        typed("and the docs")
    )));
    assert!(provider.turns().is_empty(), "held while the card waits");

    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission("card-1", PermissionDecision::Allow)
        .expect("the card is answered");
    provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "card-1".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    pump_until(&mut set, &dir, "the card's answer", |s| !s.card_waits(tab));
    assert!(set.get(tab).unwrap().cli_unseen_busy(), "the background work goes on");
    let released = set.release_after_card();
    assert_eq!(released.len(), 1, "it goes when the card is answered");
    assert!(released[0].1.outcome.is_ok());
    assert_eq!(provider.turns(), ["and the docs"]);
    shut_down(&mut set);
}

/// A session that ends sends no completion for what Claude Code was doing out of sight: its end is the
/// end of that work, so a dead tab closes without the question and the terminal can take it over.
#[test]
fn a_session_that_ends_ends_the_unseen_work_it_was_doing() {
    let (dir, mut set, tab, provider) = opened_tab("guards-unseen-session-end");
    provider.queue(AgentDomainEvent::CliWorkUnseen);
    pump_until(&mut set, &dir, "the unseen work", |s| {
        s.get(tab).unwrap().cli_unseen_busy()
    });
    provider.queue(AgentDomainEvent::SessionUnavailable {
        reason: "the sidecar exited".into(),
    });
    pump_until(&mut set, &dir, "the session's loss", |s| {
        !s.get(tab).unwrap().cli_unseen_busy()
    });
    assert!(!set.close_facts(tab).unwrap().unseen_work);
    shut_down(&mut set);
}

#[test]
fn a_card_holds_no_send_of_an_external_agents_tab() {
    let (dir, mut set, tab, provider) = opened_tab("guards-card-external");
    provider.queue(cli_prompt("card-1", "tool-1"));
    pump_until(&mut set, &dir, "the card", |s| s.card_waits(tab));
    set.get_mut(tab).unwrap().agent = AgentId::Codex;
    assert!(!set.card_waits(tab), "Codex and ACP tabs send as they did");
    shut_down(&mut set);
}

#[test]
fn a_refused_send_becomes_a_held_steer() {
    let (dir, mut set, tab, provider) = opened_over("guards-held-steer", RecordingProvider::steering());
    provider.refuse_sends(true);

    let sent = set.send_prompt(tab, "req-1", ViewId::BASE, typed("and the docs"));
    assert!(
        prompt_went_through(sent),
        "a refusal for a busy agent is not an error row"
    );
    assert!(
        user_prompts(&set, tab).is_empty(),
        "the prompt it recorded is taken back"
    );
    assert_eq!(queued_texts(&set, tab), [("and the docs".to_string(), false)]);
    assert!(!set.starting.contains(&tab.0), "the send did not start a turn");
    assert!(provider.steers().is_empty(), "no turn shows yet, so it waits");
    assert!(set.steer_waiting(2).is_empty());

    provider.refuse_sends(false);
    provider.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    pump_until(&mut set, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    // The turn can show in the projection between two pumps; the steer is the pump's, whichever one sees it.
    pump_until(&mut set, &dir, "the held steer", |_| !provider.steers().is_empty());
    assert!(
        set.steer_waiting(3).is_empty(),
        "the pump already attempted the held steer"
    );
    let steers = provider.steers();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0].expected_turn_id, "turn-1");
    assert_eq!(steers[0].as_typed, "and the docs");
    assert_eq!(queued_texts(&set, tab), [("and the docs".to_string(), true)]);
    assert!(set.steer_waiting(4).is_empty(), "one attempt only");
    shut_down(&mut set);
}

#[test]
fn a_refused_send_waits_for_the_turns_end_when_the_agent_takes_no_steer() {
    let (dir, mut set, tab, provider) = opened_tab("guards-held-no-steer");
    provider.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    pump_until(&mut set, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    provider.refuse_sends(true);

    // The projection already shows the turn: the local guard refuses, and the steer is tried at once.
    assert!(prompt_went_through(set.send_prompt(
        tab,
        "req-1",
        ViewId::BASE,
        typed("and the docs")
    )));
    assert_eq!(
        queued_texts(&set, tab),
        [("and the docs".to_string(), false)],
        "held for the end"
    );
    assert!(
        set.get(tab).unwrap().session.queue_note.is_some(),
        "with the reason under it"
    );
    assert!(user_prompts(&set, tab).is_empty());
    shut_down(&mut set);
}

/// What Claude Code works on where the panel cannot see it ends without any turn of the panel's ending:
/// the message a refused send held for that work goes out then.
#[test]
fn a_refused_send_goes_out_when_the_unseen_work_ends() {
    let (dir, mut set, tab, provider) = opened_tab("guards-held-unseen-end");
    provider.queue(AgentDomainEvent::CliWorkUnseen);
    pump_until(&mut set, &dir, "the unseen work", |s| {
        s.get(tab).unwrap().cli_unseen_busy()
    });
    provider.refuse_sends(true);
    assert!(prompt_went_through(set.send_prompt(
        tab,
        "req-1",
        ViewId::BASE,
        typed("and the docs")
    )));
    provider.refuse_sends(false);
    assert_eq!(queued_texts(&set, tab), [("and the docs".to_string(), false)]);
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert_eq!(
        provider.turns(),
        ["and the docs"],
        "nothing sends it again while the work goes on"
    );

    provider.queue(AgentDomainEvent::TurnCompleted {
        turn_id: "cli-turn".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    let mut ended = false;
    until("the end of the unseen work is reported as a turn's end", || {
        ended |= set.pump(&dir, true).turn_ended.contains(&tab);
        ended
    });
    assert!(!set.get(tab).unwrap().cli_unseen_busy());
    let flush = set.flush_queue(tab).expect("the held message goes out");
    assert!(flush.outcome.is_ok());
    assert_eq!(
        provider.turns(),
        ["and the docs", "and the docs"],
        "the refused send, then the one the end of the work let go"
    );
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down(&mut set);
}

#[test]
fn a_refused_send_leaves_one_row_after_its_delivery() {
    let (dir, mut set, tab, provider) = opened_over("guards-held-steer-row", RecordingProvider::steering());
    provider.refuse_sends(true);
    assert!(prompt_went_through(set.send_prompt(
        tab,
        "req-1",
        ViewId::BASE,
        typed("and the docs")
    )));
    provider.refuse_sends(false);
    provider.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    pump_until(&mut set, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    pump_until(&mut set, &dir, "the held steer", |_| !provider.steers().is_empty());
    assert!(
        set.steer_waiting(2).is_empty(),
        "the pump already attempted the held steer"
    );
    assert!(user_prompts(&set, tab).is_empty(), "no row before the agent read it");

    provider.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: provider.steers()[0].client_message_id.clone(),
        turn_id: "turn-1".into(),
        text: "and the docs".into(),
    });
    pump_until(&mut set, &dir, "the delivery", |s| {
        s.get(tab).unwrap().session.queue.is_empty()
    });
    assert_eq!(
        user_prompts(&set, tab),
        ["and the docs"],
        "drawn once, where it was read"
    );
    shut_down(&mut set);
}

#[test]
fn a_live_move_preserves_the_origin_of_each_held_message() {
    let (dir, mut source, tab, provider) = opened_over("move-queue-origins", RecordingProvider::steering());
    provider.queue(started("turn-1"));
    pump_until(&mut source, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    for (text, origin) in [
        ("typed", agent::PromptOrigin::Typed),
        ("prepared", agent::PromptOrigin::Composed),
    ] {
        source.queue_message(tab, text, text.into(), 1, true, origin).unwrap();
    }
    let mut destination = set();
    let adopted = moved(&mut source, tab, &mut destination, provider.clone(), ViewId::BASE);
    assert!(destination.steer_held(adopted, 2, Some("turn-1")));
    let stamps = provider.steer_stamps();
    assert_eq!(
        stamps.iter().map(|s| s.typed_by_person).collect::<Vec<_>>(),
        [true, false]
    );
    shut_down(&mut source);
    shut_down(&mut destination);
}

#[test]
fn a_live_move_keeps_the_opening_prompt_for_a_later_cli_cycle() {
    let (dir, mut source, tab, provider) = opened_over("move-opening-origin", RecordingProvider::steering());
    assert!(prompt_went_through(source.send_prompt(
        tab,
        "first",
        ViewId::BASE,
        typed("keep this prompt")
    )));
    provider.queue(started("turn-1"));
    pump_until(&mut source, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    let id = provider.turn_stamps()[0].client_message_id.clone().unwrap();
    let mut destination = set();
    let adopted = moved(&mut source, tab, &mut destination, provider.clone(), ViewId::BASE);
    provider.queue(AgentDomainEvent::CliCycleStarted {
        turn_id: "turn-1".into(),
        cause: None,
    });
    pump_until(&mut destination, &dir, "the waiting prompt", |s| {
        !s.get(adopted).unwrap().session.queue.is_empty()
    });
    let message = &destination.get(adopted).unwrap().session.queue[0];
    assert!(message.typed_by_person);
    assert_eq!(message.text, "keep this prompt");
    assert!(matches!(&message.state, QueuedState::Steered { client_id, .. } if client_id == &id));
    assert!(user_prompts(&destination, adopted).is_empty());
    provider.queue_all(vec![
        AgentDomainEvent::UserMessageDropped {
            client_message_id: id,
            reason: "Claude cancelled it".into(),
        },
        completed("turn-1"),
    ]);
    pump_until(&mut destination, &dir, "the turn's end", |s| {
        !s.get(adopted).unwrap().turn_running()
    });
    assert!(destination.flush_queue(adopted).unwrap().outcome.is_ok());
    assert!(provider.turn_stamps()[1].typed_by_person);
    shut_down(&mut source);
    shut_down(&mut destination);
}

#[test]
fn an_arrivals_presence_is_held_under_the_id_its_tab_then_takes() {
    let scratch = scratch("move-arrival-presence");
    let dir = scratch.join("project");
    let review_dir = scratch.join("state/eitri/review/0123456789abcdef");
    let mut source = set();
    let (provider, tab) = live_tab(&mut source, &dir);
    provider.queue_all(vec![opened(&dir), started("t1")]);
    pump_until(&mut source, &dir, "the running turn", |s| {
        s.get(tab).is_some_and(|t| t.turn_running())
    });

    let mut dest = set();
    let pid = 4250;
    holding_presence(&mut dest, &review_dir, pid);
    let answer = dest
        .hold_arrival("sess-move", 1)
        .expect("a window holding its presence answers");
    let held = answer
        .recv_timeout(Duration::from_secs(10))
        .expect("the presence thread answers");
    assert_eq!(held, crate::turn_review::Held::Yes);
    let files = || -> Vec<String> {
        std::fs::read_dir(review_dir.join("running"))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let held_files = files();
    assert_eq!(held_files.len(), 1, "one file for the arriving turn: {held_files:?}");
    assert!(
        dest.presence_guard().unwrap().check() == Err(Blocked::TurnRunningHere),
        "the window's own reverts wait for the arriving turn"
    );

    let arrived = moved(&mut source, tab, &mut dest, provider.clone(), ViewId::BASE);
    assert_eq!(
        held_files,
        vec![format!("{pid}.{}", arrived.0)],
        "the tab takes the id its arrival was held under"
    );
    for _ in 0..5 {
        dest.pump(&dir, true);
    }
    assert_eq!(files(), held_files, "the file stays across the adoption");

    // An arrival given up lets its file go.
    let mut other = set();
    let review_other = scratch.join("state/eitri/review/fedcba9876543210");
    holding_presence(&mut other, &review_other, pid + 1);
    let answer = other.hold_arrival("sess-gone", 2).unwrap();
    assert_eq!(
        answer.recv_timeout(Duration::from_secs(10)).unwrap(),
        crate::turn_review::Held::Yes
    );
    assert!(
        other.hold_arrival("sess-gone", 2).is_some(),
        "the same move is held once"
    );
    other.drop_arrival(2);
    until("the given-up arrival's file goes", || {
        std::fs::read_dir(review_other.join("running"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    });
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn two_arrivals_of_one_session_id_are_held_and_given_up_apart() {
    let scratch = scratch("move-arrivals-apart");
    let review = scratch.join("review");
    let mut dest = set();
    holding_presence(&mut dest, &review, 4251);
    for mv in [1, 2] {
        assert_eq!(
            dest.hold_arrival("s1", mv)
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            crate::turn_review::Held::Yes
        );
    }
    let second = dest.arrivals[&2].1;
    dest.drop_arrival(1);
    assert_eq!(
        dest.hold_arrival("flush", 3)
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap(),
        crate::turn_review::Held::Yes
    );
    assert!(review.join(format!("running/4251.{}", second.0)).is_file());
    assert_eq!(std::fs::read_dir(review.join("running")).unwrap().count(), 2);
    drop(dest);
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn adopting_one_arrival_keeps_the_hold_of_another_with_the_same_session() {
    let scratch = scratch("move-adopt-keeps-arrival");
    let dir = scratch.join("project");
    let review = scratch.join("review");
    let mut source = set();
    let (provider, tab) = live_tab(&mut source, &dir);
    provider.queue_all(vec![opened(&dir)]);
    pump_until(&mut source, &dir, "the idle session", |s| {
        s.get(tab)
            .and_then(|t| t.live())
            .and_then(|live| live.provider_session_id())
            .is_some()
    });
    assert!(!source.get(tab).unwrap().turn_running());

    let mut dest = set();
    let guard = holding_presence(&mut dest, &review, 4252);
    for mv in [1, 2] {
        assert_eq!(
            dest.hold_arrival("sess-move", mv)
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            crate::turn_review::Held::Yes
        );
    }
    let second = dest.arrivals[&2].1;
    let arrived = moved(&mut source, tab, &mut dest, provider, ViewId::BASE);
    assert!(!dest.get(arrived).unwrap().turn_running());
    dest.drop_arrival(1);
    assert_eq!(
        dest.hold_arrival("flush", 3)
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap(),
        crate::turn_review::Held::Yes
    );
    assert!(review.join(format!("running/4252.{}", second.0)).is_file());
    assert_eq!(std::fs::read_dir(review.join("running")).unwrap().count(), 2);
    assert!(refuses_as_running(&guard));
    dest.drop_arrival(3);
    assert!(refuses_as_running(&guard), "the other arrival still blocks a revert");
    drop(dest);
    drop(source);
    let _ = std::fs::remove_dir_all(scratch);
}

/// A Codex or ACP tab's session lease crosses with it and, once the window that took the tab up commits the move,
/// names that window's process, as a Claude tab's does: the rewrite belongs to the conversation, whichever agent
/// drives it, so a window that later asks who holds the session finds the holder and not the window the tab left.
#[test]
fn a_moved_codex_or_acp_chats_lease_names_the_destination() {
    for (label, agent, name) in [
        ("lease-codex", AgentId::Codex, "codex"),
        ("lease-acp", AgentId::Acp("gem".into()), "acp:gem"),
    ] {
        let scratch = scratch(label);
        let dir = scratch.join("project");
        let mut source = set();
        let provider = Arc::new(ExternalRecordingProvider::named(name, true));
        let backend = AgentBackend::start_external_with(
            agent,
            provider.clone(),
            &dir,
            empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .expect("a scripted external start succeeds");
        let tab = source.active();
        let t = source.get_mut(tab).unwrap();
        t.agent = backend.agent();
        t.backend = TabBackend::Live(backend);
        // The start opens the session itself, as the agent's own answer to it does.
        pump_until(&mut source, &dir, "the session opened", |s| {
            s.get(tab).and_then(Tab::provider_session_id).is_some()
        });

        let frozen = freeze_settled(&mut source, tab).expect("the tab freezes");
        let descriptor = source
            .get(tab)
            .and_then(Tab::live)
            .expect("a frozen tab is still held")
            .lease_descriptor()
            .expect("its session holds a lease")
            .unwrap();
        let record: TabHandoff = serde_json::from_str(&serde_json::to_string(&frozen.tab).unwrap()).unwrap();
        let chat: FrozenChat =
            serde_json::from_str(&serde_json::to_string(&frozen.chat.expect("a live tab's chat")).unwrap()).unwrap();
        let cwd = chat.conversation.canonical_cwd.to_string_lossy().into_owned();
        let session = chat.conversation.provider_session_id.clone().expect("the session's id");
        let lease =
            agent::lease::SessionLease::from_moved(descriptor, &chat.conversation.provider_name, &cwd, &session)
                .expect("the descriptor is the session's own lease");
        let path = lease.path().to_path_buf();
        // As the file reads while the process of the window the tab left owns it.
        std::fs::write(
            &path,
            r#"{"owner_pid":4194001,"instance_id":"first","started_at":"then"}"#,
        )
        .unwrap();
        let owner = || -> serde_json::Value {
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap()["owner_pid"]
                .clone()
        };

        let taken = Arc::new(ExternalRecordingProvider::named(name, true));
        let backend = AgentBackend::adopt_moved(
            chat.core,
            chat.conversation,
            chat.backend,
            agent::MovedProvider {
                agent: taken.clone(),
                external: Some(taken),
                lease: Some(lease),
            },
        )
        .expect("the destination takes the conversation up");
        let mut dest = set();
        let arrived = dest.adopt_moved(record, backend, ViewId::BASE);
        assert_eq!(
            owner(),
            serde_json::json!(4194001),
            "{name}: nothing is rewritten before the commit"
        );

        dest.get_mut(arrived)
            .and_then(Tab::live_mut)
            .expect("the arrival is live")
            .commit_moved()
            .expect("the lease is rewritten");
        assert_eq!(
            owner(),
            serde_json::json!(std::process::id()),
            "{name}: the lease names the window that holds the tab now"
        );
        let _ = source.hand_over(tab);
        let _ = std::fs::remove_dir_all(&scratch);
    }
}

/// The turn ids a tab set reported as begun since the last call.
fn began(set: &mut TabSet) -> Vec<String> {
    set.take_client_news()
        .into_iter()
        .filter_map(|(_, news)| match news {
            ClientCallNews::TurnBegan { turn_id } => Some(turn_id),
            _ => None,
        })
        .collect()
}

/// A window with no editor whose tabs were given `diagnostics`, for the broker's answers that need none.
struct WithoutAnEditor {
    root: PathBuf,
}

impl crate::editor_tools::broker::BrokerEnv for WithoutAnEditor {
    fn way(&self) -> Option<&dyn crate::editor_tools::way::EditorWay> {
        None
    }

    fn facts(&self, _tab: TabId) -> Option<crate::editor_tools::broker::TabFacts> {
        Some(crate::editor_tools::broker::TabFacts {
            mode: agent::PermissionMode::Manual,
            trust: agent::setting_sources::ProjectTrust::Trusted,
            session: "session".to_owned(),
            process: "process".to_owned(),
            revision: 1,
            declared: std::rc::Rc::new(BTreeSet::from(["diagnostics".to_owned()])),
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn home(&self) -> Option<&Path> {
        None
    }

    fn context(&self) -> Option<crate::editor_context::EditorContext> {
        None
    }
}

/// The clock crosses a move, and so does the note of the turn it names as begun: the destination's first pump finds
/// them equal and reports nothing, so no baseline is taken mid-turn that would hide what appeared before the move.
#[test]
fn a_moved_tab_mid_turn_takes_no_baseline() {
    use crate::editor_tools::broker::{Broker, BrokerOut, Caller};
    let dir = workspace("move-no-baseline");
    let mut source = set();
    let (provider, tab) = live_tab(&mut source, &dir);
    provider.queue_all(vec![opened(&dir), started("t1")]);
    let mut told = Vec::new();
    until("the source to report the turn", || {
        source.pump(&dir, true);
        told.extend(began(&mut source));
        !told.is_empty()
    });
    assert_eq!(told, ["t1"], "the source reports the turn it saw begin");

    let mut dest = set();
    let arrived = moved(&mut source, tab, &mut dest, provider.clone(), ViewId::BASE);
    dest.pump(&dir, true);
    dest.pump(&dir, true);
    dest.state_payloads(arrived);
    dest.pump(&dir, true);
    assert!(began(&mut dest).is_empty(), "the turn did not begin again");
    let session = &dest.get(arrived).unwrap().session;
    assert_eq!(session.last_began_turn.as_deref(), Some("t1"));
    assert!(session.turn_clock.is_some(), "the clock crossed too");

    let env = WithoutAnEditor { root: dir.clone() };
    let mut broker = Broker::new();
    let now = Instant::now();
    let caller = Caller::Sidecar { call_id: "c1".into() };
    let mut out = broker.submit(arrived, caller, None, "diagnostics", r#"{"since":"turn"}"#, &env, now);
    out.extend(broker.tick(&env, now));
    let [BrokerOut::Answer { result, .. }] = &out[..] else {
        panic!("one answer: {out:?}");
    };
    assert!(result.is_error);
    assert_eq!(result.text, "no baseline for this turn");

    // The next turn there is the destination's to report.
    provider.queue_all(vec![completed("t1"), started("t2")]);
    pump_until(&mut dest, &dir, "the next turn", |s| {
        s.get(arrived).unwrap().session.last_began_turn.as_deref() == Some("t2")
    });
    assert_eq!(began(&mut dest), ["t2"]);
    let _ = source.hand_over(tab);
}

/// A freeze's last pump can find a call. Nothing of a moving tab may start, and a refused move still owes it.
#[test]
fn a_frozen_tab_keeps_its_client_news_until_the_move_is_called_off() {
    let dir = workspace("move-client-news");
    let mut set = set();
    let (provider, tab) = live_tab(&mut set, &dir);
    provider.queue_all(vec![opened(&dir), started("t1")]);
    pump_until(&mut set, &dir, "the turn", |s| s.get(tab).unwrap().turn_running());
    provider.queue(AgentDomainEvent::ClientToolCall {
        call_id: "c1".into(),
        tool_use_id: None,
        tool: "open_file".into(),
        arguments_json: "{}".into(),
        sequence: 1,
    });
    until("the call to fold", || {
        !set.get(tab).unwrap().live().unwrap().open_client_calls().is_empty()
    });
    let _frozen = freeze_settled(&mut set, tab).expect("the tab freezes");
    assert!(set.take_client_news().is_empty());

    set.unfreeze(tab);
    // The turn's beginning was found by the same pumps; this is about the call.
    let news: Vec<_> = set
        .take_client_news()
        .into_iter()
        .filter(|(_, news)| !matches!(news, ClientCallNews::TurnBegan { .. }))
        .collect();
    assert!(
        matches!(&news[..], [(id, ClientCallNews::Opened(call))] if *id == tab && call.call_id == "c1"),
        "{news:?}"
    );
}

/// The line is for a page of the window that saw the report, and stays there: the window the tab lands
/// in reads the same report from the moved projection and says it once on its own page.
#[test]
fn the_window_a_tab_lands_in_says_a_foreign_server_on_its_own_page() {
    let dir = workspace("move-foreign-server");
    let provider = Arc::new(RecordingProvider::client_tooling());
    let tools = agent::ClientToolSet {
        server_name: "eitri".into(),
        timeout_ms: 900_000,
        tools: vec![agent::ClientToolDecl {
            name: "open_file".into(),
            description: "opens a file".into(),
            input_schema_json: r#"{"type":"object"}"#.into(),
            always_load: true,
        }],
    };
    let conversation = AgentConversation::create_with_tools(
        provider.clone(),
        &dir,
        agent::setting_sources::ProjectTrust::Untrusted,
        Some(tools),
    )
    .unwrap();
    let mut source = set();
    let tab = source.active();
    source.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
    provider.queue_all(vec![
        opened(&dir),
        AgentDomainEvent::SessionServers {
            servers: vec![agent::InitServer {
                name: "eitri".into(),
                status: "connected".into(),
                source: "project".into(),
            }],
        },
    ]);
    until("the line in the window it leaves", || {
        source.pump(&dir, true);
        !source.take_editor_notes().is_empty()
    });

    let mut dest = set();
    let arrived = moved(&mut source, tab, &mut dest, provider.clone(), ViewId::BASE);
    dest.pump(&dir, true);
    let said: Vec<TabId> = dest.take_editor_notes().into_iter().map(|(tab, _)| tab).collect();
    assert_eq!(said, [arrived]);
    dest.pump(&dir, true);
    assert!(dest.take_editor_notes().is_empty(), "once in this window too");
    let _ = source.hand_over(tab);
}

// ---- the row key and the wait stamps --------------------------------------------------------------------

fn row_context() -> RowContext {
    RowContext {
        instance: "0123456789abcdef".into(),
        project: crate::agents::ProjectRef {
            key: "fedcba9876543210".into(),
            name: "eitri".into(),
        },
        window: crate::agents::WindowRef {
            instance: "0123456789abcdef".into(),
            name: "1".into(),
            focused: true,
        },
    }
}

fn is_key_shaped(key: &str) -> bool {
    RowKey::parse(key).is_some_and(|parsed| parsed.as_str() == key)
}

/// The record a session's resume is matched with, as a project's records list it.
fn record_of(session: &str) -> agent::ResumableSession {
    agent::ResumableSession {
        provider: "claude".into(),
        provider_session_id: session.into(),
        created_at: "1".into(),
        updated_at: "2".into(),
        title: None,
        name: None,
    }
}

/// `saved` brought back in a fresh window as a launch or a break's target brings it back: planned against its
/// record, started into the window's empty tab. Returns the window and the tab that took the session.
fn restored(saved: &SavedTabs, session: &str) -> (TabSet, TabId) {
    let plan = crate::tab_restore::plan(saved, &[record_of(session)], &[], |_, _| false);
    assert_eq!(plan.items.len(), 1, "the saved tab is planned");
    let mut target = set();
    let RestoreStep::Go(go) = target.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing in bypass, nothing to ask")
    };
    let from = target.active();
    target.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    let tab = target
        .tab_with_session(&AgentId::Claude, session)
        .expect("the session came back in a tab");
    (target, tab)
}

/// A live tab whose session id is known, so it is listed among the saved tabs.
fn live_tab_with_session(set: &mut TabSet, dir: &Path) -> (Arc<RecordingProvider>, TabId) {
    let (provider, tab) = live_tab(set, dir);
    provider.queue_all(vec![opened(dir)]);
    pump_until(set, dir, "the session id", |s| {
        s.get(tab).unwrap().provider_session_id().is_some()
    });
    (provider, tab)
}

fn end_all(set: &mut TabSet) {
    for removed in set.take_all() {
        if let Some(retired) = removed.retired {
            retired.into_backend().shutdown();
        }
    }
}

#[test]
fn the_row_key_is_set_before_the_first_send() {
    let dir = workspace("row-key-first-send");
    let mut source = set();
    let tab = source.active();
    let before = source.get(tab).unwrap().row_key().clone();
    assert!(is_key_shaped(before.as_str()), "{before}");
    let (_provider, live) = live_tab_with_session(&mut source, &dir);
    assert_eq!(live, tab);
    assert_eq!(
        source.get(tab).unwrap().row_key(),
        &before,
        "the session id appearing does not rename the tab"
    );
    let other = source.open(AgentId::Claude);
    assert_ne!(
        source.get(other).unwrap().row_key(),
        &before,
        "every tab has a key of its own"
    );
    end_all(&mut source);
}

#[test]
fn a_tab_keeps_its_row_key_through_adopt_moved() {
    let dir = workspace("row-key-adopt-moved");
    let mut source = set();
    let (provider, tab) = live_tab_with_session(&mut source, &dir);
    let key = source.get(tab).unwrap().row_key().clone();
    let mut dest = set();
    let arrived = moved(&mut source, tab, &mut dest, provider.clone(), ViewId::BASE);
    assert_eq!(dest.get(arrived).unwrap().row_key(), &key);
    let row = dest
        .agent_rows(&row_context(), &|_| true, 100)
        .into_iter()
        .find(|row| row.number == dest.get(arrived).unwrap().number)
        .expect("the arrived tab's row");
    assert_eq!(row.key, key.as_str(), "the row names the tab by its key");
    let _ = source.hand_over(tab);
    end_all(&mut dest);
}

#[test]
fn an_unstarted_tab_keeps_its_row_key_through_adopt_moved_unstarted() {
    let mut source = set();
    let tab = source.active();
    let key = source.get(tab).unwrap().row_key().clone();
    let frozen = freeze_settled(&mut source, tab).expect("an empty tab moves");
    let record: TabHandoff = serde_json::from_str(&serde_json::to_string(&frozen.tab).unwrap()).unwrap();
    let mut dest = set();
    let arrived = dest.adopt_moved_unstarted(record, ViewId::BASE).unwrap();
    assert_eq!(dest.get(arrived).unwrap().row_key(), &key);
    assert_ne!(
        dest.tabs()
            .iter()
            .find(|t| t.id != arrived)
            .map(|t| t.row_key().clone()),
        Some(key),
        "the destination's own tabs keep their own keys"
    );
}

#[test]
fn wait_stamps_move_with_the_tab() {
    let mut source = set();
    let tab = source.active();
    source.get_mut(tab).unwrap().attention.resync(["p-1".to_string()]);
    let before = source.agent_rows(&row_context(), &|_| true, 100).remove(0);
    assert_eq!(before.waiting_since, Some(100));
    let frozen = freeze_settled(&mut source, tab).expect("an empty tab moves");
    let record: TabHandoff = serde_json::from_str(&serde_json::to_string(&frozen.tab).unwrap()).unwrap();
    let mut dest = set();
    let arrived = dest.adopt_moved_unstarted(record, ViewId::BASE).unwrap();
    let number = dest.get(arrived).unwrap().number;
    let after = dest
        .agent_rows(&row_context(), &|_| true, 5_000)
        .into_iter()
        .find(|row| row.number == number)
        .expect("the arrived tab's row");
    assert_eq!(after.state, before.state);
    assert_eq!(
        after.waiting_since,
        Some(100),
        "the card has waited since it was first seen in the window it left"
    );
    assert_eq!(
        after.since, before.since,
        "the state did not change, so neither did its start"
    );
}

#[test]
fn a_handoff_without_the_new_fields_parses() {
    let mut source = set();
    let tab = source.active();
    let key = source.get(tab).unwrap().row_key().clone();
    source.get_mut(tab).unwrap().replace_draft("from an older peer".into());
    let frozen = freeze_settled(&mut source, tab).expect("an empty tab moves");
    let mut value = serde_json::to_value(&frozen.tab).unwrap();
    let fields = value.as_object_mut().unwrap();
    fields.remove("row_key");
    fields.remove("agent_watch");
    let record: TabHandoff = serde_json::from_value(value).expect("an older peer's record is read");
    let mut dest = set();
    let arrived = dest.adopt_moved_unstarted(record, ViewId::BASE).unwrap();
    let t = dest.get(arrived).unwrap();
    assert_eq!(t.draft(), "from an older peer");
    assert!(is_key_shaped(t.row_key().as_str()), "a new key: {}", t.row_key());
    assert_ne!(t.row_key(), &key);
    let _ = source.hand_over(tab);

    // A key that is no key's shape is read as none, never as a refusal of the record.
    let next = source.active();
    let frozen = freeze_settled(&mut source, next).expect("the next empty tab moves");
    let mut value = serde_json::to_value(&frozen.tab).unwrap();
    value["row_key"] = serde_json::json!("k-NOT-A-KEY");
    let record: TabHandoff = serde_json::from_value(value).expect("a malformed key does not refuse the record");
    let arrived = dest.adopt_moved_unstarted(record, ViewId::BASE).unwrap();
    assert!(is_key_shaped(dest.get(arrived).unwrap().row_key().as_str()));
}

#[test]
fn tab_break_carries_the_key_through_the_saved_record() {
    let dir = workspace("row-key-tab-break");
    let mut source = set();
    let (_provider, tab) = live_tab_with_session(&mut source, &dir);
    let key = source.get(tab).unwrap().row_key().clone();
    // What the source sends with `tab_release` and the target reads from `tabs_given`.
    let snapshot = source.saved_snapshot(PROJECT_ID).expect("the window is not closing");
    let record = snapshot
        .tabs
        .iter()
        .find(|t| t.provider_session_id == "sess-move")
        .expect("the tab is listed");
    assert_eq!(record.row_key.as_deref(), Some(key.as_str()));
    let crossed = SavedTab::from_wire(&record.to_wire()).expect("the record crosses the link");
    let given = SavedTabs::from_parts(vec![SavedTab { view: None, ..crossed }], 0, Vec::new(), None);
    let (mut target, arrived) = restored(&given, "sess-move");
    assert_eq!(target.get(arrived).unwrap().row_key(), &key);
    end_all(&mut target);
    end_all(&mut source);
}

#[test]
fn a_restored_tab_keeps_its_key() {
    let dir = workspace("row-key-restart");
    let mut source = set();
    let (_provider, tab) = live_tab_with_session(&mut source, &dir);
    let key = source.get(tab).unwrap().row_key().clone();
    let snapshot = source.saved_snapshot(PROJECT_ID).expect("the window is not closing");
    let root = Path::new("/home/user/project");
    let read_back = crate::saved_tabs::decode(&crate::saved_tabs::encode(&snapshot, root), root).unwrap();
    let (mut target, arrived) = restored(&read_back, "sess-move");
    assert_eq!(target.get(arrived).unwrap().row_key(), &key);
    end_all(&mut target);
    end_all(&mut source);
}

#[test]
fn a_record_without_a_key_gets_a_new_one() {
    for saved_key in [
        None,
        Some("not-a-key".to_string()),
        Some("k-0123456789ABCDEF".to_string()),
    ] {
        let tab = SavedTab {
            agent: AgentId::Claude,
            conversation_id: PROJECT_ID.into(),
            provider_session_id: "sess-old".into(),
            name: None,
            mode: SessionModeChoice::Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
            row_key: saved_key.clone(),
        };
        let saved = SavedTabs::from_parts(vec![tab], 0, Vec::new(), None);
        let (mut target, arrived) = restored(&saved, "sess-old");
        let key = target.get(arrived).unwrap().row_key().clone();
        assert!(is_key_shaped(key.as_str()), "{saved_key:?}: {key}");
        assert_ne!(Some(key.as_str()), saved_key.as_deref(), "{saved_key:?}");
        end_all(&mut target);
    }
}

const PROJECT_ID: &str = "conv-0123456789abcdef";

/// A given tab whose resume is refused leaves the window's empty tab empty again, and the key it brought goes back
/// with the record: the empty tab must not keep it, or the source, taking the tab up again, would publish a second
/// row under the same name.
#[test]
fn a_failed_restore_into_the_empty_tab_does_not_keep_the_records_key() {
    let key = RowKey::new();
    let tab = SavedTab {
        agent: AgentId::Claude,
        conversation_id: PROJECT_ID.into(),
        provider_session_id: "sess-refused".into(),
        name: None,
        mode: SessionModeChoice::Auto,
        view: None,
        window: None,
        turn_running: false,
        draft: None,
        row_key: Some(key.as_str().to_string()),
    };
    let saved = SavedTabs::from_parts(vec![tab], 0, Vec::new(), None);
    let plan = crate::tab_restore::plan(&saved, &[record_of("sess-refused")], &[], |_, _| false);
    let mut target = set();
    let from = target.active();
    let RestoreStep::Go(go) = target.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing in bypass, nothing to ask")
    };
    let (tx, rx) = mpsc::channel();
    let mut rx = Some(rx);
    let mut run = target.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(rx.take().unwrap())));
    assert_eq!(
        target.get(from).unwrap().row_key(),
        &key,
        "the tab taking the session takes its key"
    );
    tx.send(Err(BackendError {
        message: "no such session".into(),
        benign: false,
    }))
    .unwrap();
    let mut discarded = false;
    for collected in target.collect_starts() {
        if let StartCollected::Failed { tab, error, .. } = collected {
            run.note_failed(tab, &error.message);
            target.discard_failed_restore(tab, run.back_to(tab));
            discarded = true;
        }
    }
    assert!(discarded, "the refusal was collected");
    let empty = target.get(from).expect("the tab the restore began in stays");
    assert!(matches!(empty.backend, TabBackend::NotStarted), "an empty tab again");
    assert!(is_key_shaped(empty.row_key().as_str()), "{}", empty.row_key());
    assert_ne!(empty.row_key(), &key, "the record's key went back with the record");
}
