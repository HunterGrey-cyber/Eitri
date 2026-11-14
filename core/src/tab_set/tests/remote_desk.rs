//! A remote tab at the desk: its `!` lines are refused, since they would run in this machine's copy of the
//! project and not on the server, and its sends and interrupts never wait on the provider on this thread.
#![cfg(test)]

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

fn remote_origin() -> crate::remote_project::TabOrigin {
    crate::remote_project::TabOrigin::Remote {
        host: "example-host".into(),
        host_key_sha256: "SHA256:example".into(),
        root: "/srv/project".into(),
    }
}

/// A remote window's one tab, live over a provider standing for an agent on a server whose link is up.
fn remote_tab(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    let dir = workspace(label);
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let provider = Arc::new(RecordingProvider::interruptible());
    let conversation =
        AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
    set.pump(&dir, true);
    (dir, set, tab, provider)
}

/// A link that loses the send reply after the recording provider has taken the message.
struct LostReply(Arc<RecordingProvider>);

impl agent::AgentProvider for LostReply {
    fn capabilities(&self) -> agent::ProviderCapabilities {
        self.0.capabilities()
    }
    fn info(&self) -> agent::ProviderInfo {
        self.0.info()
    }
    fn create_session(&self, request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
        self.0.create_session(request)
    }
    fn resume_session(&self, request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
        self.0.resume_session(request)
    }
    fn send_turn(&self, request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        self.0.send_turn(request)?;
        Err(agent::ProviderError::Transport("send reply lost".into()))
    }
    fn interrupt_turn(&self, request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
        self.0.interrupt_turn(request)
    }
    fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        self.0.resolve_permission(request)
    }
    fn close_session(&self, request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        self.0.close_session(request)
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        self.0.pump()
    }
}

/// The provider's calls, held until `let_go` or, failing that, until a few seconds have passed on another thread:
/// a call that waited on the provider here still comes back, and `released` then says it had to be let go.
struct Held {
    let_go: std::sync::mpsc::Sender<()>,
    released: Arc<AtomicBool>,
}

impl Held {
    fn over(provider: &RecordingProvider) -> Held {
        let release = provider.hold_calls();
        let (let_go, asked) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(AtomicBool::new(false));
        let flag = released.clone();
        std::thread::spawn(move || {
            if asked.recv_timeout(Duration::from_secs(3)).is_err() {
                flag.store(true, Ordering::SeqCst);
            }
            drop(release);
        });
        Held { let_go, released }
    }

    /// Whether the calls had to be let go for the caller to come back.
    fn had_to_let_go(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    fn let_go(self) {
        let _ = self.let_go.send(());
    }
}

fn typed(text: &str) -> FirstTurn {
    FirstTurn {
        wire: text.into(),
        typed: text.into(),
        typed_by_person: true,
        shell_line: false,
        attachments: Vec::new(),
    }
}

/// The turn sends the tick lets go, waiting for the first.
fn sends_answered(set: &mut TabSet, dir: &Path) -> Vec<Sent> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        set.pump(dir, true);
        let sent = set.poll_turn_sends(Instant::now());
        if !sent.is_empty() {
            return sent;
        }
        assert!(Instant::now() < deadline, "the send was never answered");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start_turn(set: &mut TabSet, dir: &Path, tab: TabId, provider: &RecordingProvider, turn: &str) {
    provider.queue(started(turn));
    until("the turn", || {
        set.pump(dir, true);
        set.get(tab).unwrap().turn_running()
    });
}

#[test]
fn a_shell_line_in_a_remote_tab_is_refused() {
    let (_dir, mut set, tab, _provider) = remote_tab("remote-desk-shell-queue");
    let refused = set.queue_shell_line(tab, "!true", 1).unwrap_err();
    assert_eq!(refused, SHELL_LINES_REMOTE_UNAVAILABLE);
    assert!(set.get(tab).unwrap().session.queue.is_empty(), "nothing waits to run");
    assert!(!set.shell_line_runs(tab));
    shut_down_all(&mut set);
}

#[test]
fn a_remote_tabs_first_message_that_is_a_shell_line_runs_nothing() {
    let (_dir, mut set, tab, provider) = remote_tab("remote-desk-shell-first");
    let turn = FirstTurn {
        shell_line: true,
        ..typed("!true")
    };
    let sent = set.send_first_turn(tab, "r1", turn);
    match sent {
        Some(Sent::First {
            outcome: Err(error), ..
        }) => {
            assert_eq!(error.message, SHELL_LINES_REMOTE_UNAVAILABLE)
        }
        other => panic!("refused at once, not {}", other.is_some()),
    }
    assert!(!set.shell_line_runs(tab));
    assert!(provider.turns().is_empty());
    shut_down_all(&mut set);
}

#[test]
fn a_composers_message_to_a_remote_tab_never_waits_on_the_provider() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-prompt");
    let held = Held::over(&provider);
    let sent = set.send_prompt(tab, "p1", ViewId::BASE, typed("hello"));
    assert!(!held.had_to_let_go(), "the send came back while the provider waited");
    assert!(sent.is_none(), "its outcome comes from the tick");
    assert!(set.send_is_held(tab), "the page's command is owed until then");
    assert!(provider.turns().is_empty());
    assert_eq!(
        set.send_prompt(tab, "p2", ViewId::BASE, typed("again"))
            .map(|sent| match sent {
                Sent::Prompt { outcome, .. } => outcome.map_err(|e| e.message),
                _ => Ok(()),
            }),
        Some(Err(SEND_HELD.to_string())),
        "one send of a tab at a time"
    );
    held.let_go();
    match &sends_answered(&mut set, &dir)[..] {
        [Sent::Prompt {
            request_id, outcome, ..
        }] => {
            assert_eq!(request_id, "p1");
            assert!(outcome.is_ok());
        }
        _ => panic!("the composer's send is answered"),
    }
    assert_eq!(provider.turns(), ["hello"]);
    assert!(!set.send_is_held(tab));
    assert!(set.get(tab).unwrap().session.opening_prompt.is_some());
    shut_down_all(&mut set);
}

#[test]
fn a_queue_flush_in_a_remote_tab_never_waits_on_the_provider() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-flush");
    set.queue_message(tab, "one", "ctx\n\none".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
    let held = Held::over(&provider);
    let flush = set.flush_queue(tab);
    assert!(!held.had_to_let_go(), "the flush came back while the provider waited");
    assert!(flush.is_none(), "its outcome comes from the tick");
    held.let_go();
    match &sends_answered(&mut set, &dir)[..] {
        [Sent::Flushed { tab: flushed, flush }] => {
            assert_eq!(*flushed, tab);
            assert_eq!(flush.typed, "one");
            assert!(flush.outcome.is_ok());
        }
        _ => panic!("the flush is answered"),
    }
    assert_eq!(provider.turns(), ["ctx\n\none"]);
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down_all(&mut set);
}

#[test]
fn a_refused_remote_flush_puts_its_messages_back_in_the_queue() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-flush-refused");
    set.queue_message(tab, "one", "one".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
    set.queue_message(tab, "two", "two".into(), 2, true, agent::PromptOrigin::Typed)
        .unwrap();
    provider.refuse_sends(true);
    assert!(set.flush_queue(tab).is_none());
    match &sends_answered(&mut set, &dir)[..] {
        [Sent::Flushed { flush, .. }] => assert!(flush.outcome.is_err()),
        _ => panic!("the refusal is told"),
    }
    let tab_now = set.get(tab).unwrap();
    let texts: Vec<&str> = tab_now.session.queue.iter().map(|q| q.text.as_str()).collect();
    assert_eq!(texts, ["one", "two"], "the messages wait, in order");
    assert!(tab_now.session.queue_error.is_some());
    shut_down_all(&mut set);
}

#[test]
fn send_now_in_a_remote_tab_interrupts_without_waiting_on_the_provider() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-send-now");
    start_turn(&mut set, &dir, tab, &provider, "t1");
    let held = Held::over(&provider);
    let interrupted = set.send_now(tab, "two", "two".into(), 2);
    assert!(!held.had_to_let_go(), "send-now came back while the provider waited");
    let number = match interrupted {
        Ok(SendNow::InterruptOut(number)) => number,
        _ => panic!("the interrupt is out"),
    };
    assert_eq!(provider.interrupts(), 0, "the provider has not answered yet");
    assert!(set.poll_interrupts().is_empty());
    assert!(set.needs_polling(), "the tick reads its answer");
    held.let_go();
    let mut done = Vec::new();
    until("the interrupt answered", || {
        done.extend(set.poll_interrupts());
        !done.is_empty()
    });
    assert_eq!(done.len(), 1);
    assert_eq!((done[0].number, done[0].tab), (number, tab));
    assert!(done[0].outcome.is_ok());
    assert_eq!(provider.interrupts(), 1);
    assert_eq!(set.queue_texts(tab), ["two"], "the message waits for the turn's end");
    shut_down_all(&mut set);
}

#[test]
fn an_interrupt_result_carries_the_session_that_sent_it() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-interrupt-generation");
    start_turn(&mut set, &dir, tab, &provider, "t1");
    let generation = set.get(tab).unwrap().backend_generation();
    let held = Held::over(&provider);
    let number = set.interrupt_spawned(tab).unwrap();
    let (_, replacement) = live(&dir);
    let old = set.get_mut(tab).unwrap().set_backend(TabBackend::Live(replacement));
    assert_ne!(generation, set.get(tab).unwrap().backend_generation());
    held.let_go();
    let mut done = Vec::new();
    until("the old interrupt answered", || {
        done.extend(set.poll_interrupts());
        !done.is_empty()
    });
    assert_eq!((done[0].number, done[0].generation), (number, generation));
    drop(old);
    shut_down_all(&mut set);
}

#[test]
fn a_refused_flush_adjusts_positions_for_original_items_taken_back_meanwhile() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-queue-taken-back");
    for (at, text) in ["refused", "one", "two"].iter().enumerate() {
        set.queue_message(tab, text, (*text).into(), at as u64, true, agent::PromptOrigin::Typed)
            .unwrap();
    }
    set.get_mut(tab).unwrap().session.queue[0].refused = true;
    provider.refuse_sends(true);
    let held = Held::over(&provider);
    assert!(set.flush_queue(tab).is_none());
    assert_eq!(set.take_back_queue(tab), ["refused"]);
    set.queue_message(tab, "refused", "refused".into(), 0, true, agent::PromptOrigin::Typed)
        .unwrap();
    held.let_go();
    sends_answered(&mut set, &dir);
    assert_eq!(set.queue_texts(tab), ["one", "two", "refused"]);
    shut_down_all(&mut set);
}

#[test]
fn a_stale_queue_reply_restores_only_messages_the_old_call_did_not_accept() {
    for (taken, lost_reply) in [(false, false), (true, false), (false, true)] {
        let (dir, mut set, tab, provider) = remote_tab("remote-desk-stale-queue");
        if lost_reply {
            let conversation = AgentConversation::create(
                Arc::new(LostReply(provider.clone())),
                &dir,
                agent::setting_sources::ProjectTrust::Untrusted,
            )
            .unwrap();
            set.get_mut(tab).unwrap().install_live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            });
            set.pump(&dir, true);
        }
        for text in ["old-one", "old-two"] {
            set.queue_message(tab, text, text.into(), 1, true, agent::PromptOrigin::Typed)
                .unwrap();
        }
        provider.refuse_sends(!taken && !lost_reply);
        let held = Held::over(&provider);
        assert!(set.flush_queue(tab).is_none());
        let replacement = Arc::new(RecordingProvider::interruptible());
        let conversation =
            AgentConversation::create(replacement, &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        set.get_mut(tab).unwrap().install_live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        set.queue_message(tab, "new", "new".into(), 2, true, agent::PromptOrigin::Typed)
            .unwrap();
        held.let_go();
        let sent = sends_answered(&mut set, &dir);
        assert!(matches!(&sent[..], [Sent::Flushed { flush, .. }] if flush.outcome.is_err()));
        if taken {
            assert_eq!(set.queue_texts(tab), ["new"]);
            assert!(set.get(tab).unwrap().session.queue_error.is_none());
        } else {
            assert_eq!(set.queue_texts(tab), ["old-one", "old-two", "new"]);
            let error = set.get(tab).unwrap().session.queue_error.as_deref().unwrap();
            if lost_reply {
                assert!(error.contains("may not have reached the agent"));
                assert!(error.contains("send reply lost"));
            } else {
                assert!(error.contains("not sent because the old session was busy"));
                assert!(!error.contains("may not have reached"));
            }
        }
        assert!(set.get(tab).unwrap().session.opening_prompt.is_none());
        shut_down_all(&mut set);
    }
}

#[test]
fn a_refused_remote_flush_keeps_refused_items_in_their_original_positions() {
    for refused in [0, 1] {
        let (dir, mut set, tab, provider) = remote_tab("remote-desk-queue-order");
        for (at, text) in ["one", "two", "three"].iter().enumerate() {
            set.queue_message(tab, text, (*text).into(), at as u64, true, agent::PromptOrigin::Typed)
                .unwrap();
        }
        set.get_mut(tab).unwrap().session.queue[refused].refused = true;
        provider.refuse_sends(true);
        let held = Held::over(&provider);
        assert!(set.flush_queue(tab).is_none());
        set.queue_message(tab, "four", "four".into(), 4, true, agent::PromptOrigin::Typed)
            .unwrap();
        held.let_go();
        sends_answered(&mut set, &dir);
        assert_eq!(set.queue_texts(tab), ["one", "two", "three", "four"]);
        shut_down_all(&mut set);
    }
}

#[test]
fn a_turn_end_while_the_remote_send_is_out_is_retried_after_its_reply() {
    for taken in [false, true] {
        let (dir, mut set, tab, provider) = remote_tab("remote-desk-flush-owed");
        provider.refuse_sends(!taken);
        set.queue_message(tab, "/cost", "/cost".into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
        set.queue_message(tab, "next", "next".into(), 2, true, agent::PromptOrigin::Typed)
            .unwrap();
        let held = Held::over(&provider);
        assert!(set.flush_queue(tab).is_none());
        start_turn(&mut set, &dir, tab, &provider, "t1");
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        until("the turn ended", || !set.pump(&dir, true).turn_ended.is_empty());
        assert!(set.flush_queue(tab).is_none(), "the reply is still held");
        held.let_go();
        sends_answered(&mut set, &dir);
        assert!(
            set.pump(&dir, true).turn_ended.contains(&tab),
            "the deferred turn-end flush is owed after either reply"
        );
        shut_down_all(&mut set);
    }
}

#[test]
fn a_busy_remote_send_waits_for_the_open_turn_without_owing_a_new_turn() {
    for composer in [false, true] {
        let (dir, mut set, tab, provider) = remote_tab("remote-desk-busy-waits");
        provider.refuse_sends(true);
        if composer {
            assert!(set.send_prompt(tab, "p1", ViewId::BASE, typed("waiting")).is_none());
        } else {
            set.queue_message(tab, "waiting", "waiting".into(), 1, true, agent::PromptOrigin::Typed)
                .unwrap();
            assert!(set.flush_queue(tab).is_none());
        }
        sends_answered(&mut set, &dir);
        assert_eq!(set.queue_texts(tab), ["waiting"]);
        assert!(!set.pump(&dir, true).turn_ended.contains(&tab));
        assert_eq!(provider.turns(), ["waiting"]);
        if composer {
            assert!(set.get(tab).unwrap().session.queue[0].steer_when_running);
        }
        shut_down_all(&mut set);
    }
}

#[test]
fn a_stale_reply_preserves_a_flush_deferred_while_the_send_was_out() {
    for taken in [false, true] {
        let (dir, mut set, tab, provider) = remote_tab("remote-desk-stale-owed");
        set.queue_message(tab, "/cost", "/cost".into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
        provider.refuse_sends(!taken);
        let held = Held::over(&provider);
        assert!(set.flush_queue(tab).is_none());
        let (replacement_provider, replacement) = live(&dir);
        let old = set.get_mut(tab).unwrap().set_backend(TabBackend::Live(replacement));
        set.queue_message(tab, "next", "next".into(), 2, true, agent::PromptOrigin::Typed)
            .unwrap();
        start_turn(&mut set, &dir, tab, &replacement_provider, "replacement-turn");
        replacement_provider.queue(completed("replacement-turn", agent::TurnOutcome::Completed));
        until("the replacement turn ended", || {
            !set.pump(&dir, true).turn_ended.is_empty()
        });
        assert!(set.flush_queue(tab).is_none(), "the old session's reply is still held");
        held.let_go();
        sends_answered(&mut set, &dir);
        assert!(set.pump(&dir, true).turn_ended.contains(&tab));
        drop(old);
        shut_down_all(&mut set);
    }
}

#[test]
fn a_remote_send_reply_without_a_deferred_flush_does_not_start_the_remaining_queue() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-no-deferred-flush");
    for text in ["/cost", "next"] {
        set.queue_message(tab, text, text.into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
    }
    assert!(set.flush_queue(tab).is_none());
    sends_answered(&mut set, &dir);
    assert_eq!(set.queue_texts(tab), ["next"]);
    assert!(!set.pump(&dir, true).turn_ended.contains(&tab));
    assert_eq!(provider.turns(), ["/cost"]);
    shut_down_all(&mut set);
}

#[test]
fn a_busy_reply_does_not_retry_a_deferred_flush_without_a_new_turn_end() {
    let (dir, mut set, tab, provider) = remote_tab("remote-desk-busy-deferred");
    start_turn(&mut set, &dir, tab, &provider, "previous");
    provider.queue(completed("previous", agent::TurnOutcome::Completed));
    until("the previous turn ended", || {
        !set.pump(&dir, true).turn_ended.is_empty()
    });
    provider.refuse_sends(true);
    set.queue_message(tab, "waiting", "waiting".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
    let held = Held::over(&provider);
    assert!(set.flush_queue(tab).is_none());
    assert!(set.flush_queue(tab).is_none(), "a manual flush is deferred too");
    held.let_go();
    sends_answered(&mut set, &dir);
    assert_eq!(set.queue_texts(tab), ["waiting"]);
    assert!(!set.pump(&dir, true).turn_ended.contains(&tab));
    assert_eq!(provider.turns(), ["waiting"]);
    shut_down_all(&mut set);
}
