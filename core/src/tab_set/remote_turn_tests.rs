//! A phone's words in a tab's turns: which mode they may go out in after they were taken, and whose they stay
//! once the agent hands them back to the queue.
#![cfg(test)]

use super::*;
use crate::test_providers::RecordingProvider;
use std::time::{Duration, Instant};

fn asker() -> RemoteAsker {
    RemoteAsker {
        device_id: "d1".into(),
        device: "Pixel 8".into(),
        command_id: "s1".into(),
        conn: Some(7),
    }
}

struct Fixture {
    dir: PathBuf,
    set: TabSet,
    tab: TabId,
    provider: Arc<RecordingProvider>,
}

impl Fixture {
    /// A live Claude tab in `mode` over a provider that takes messages mid-turn.
    fn new(label: &str, mode: SessionModeChoice) -> Self {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let provider = Arc::new(RecordingProvider::steering());
        let mut set = TabSet::new(BackendKind::Sidecar, mode);
        let tab = set.active();
        install(&mut set, tab, provider.clone(), &dir);
        set.get_mut(tab).unwrap().set_mode_for_test(mode);
        Self {
            dir,
            set,
            tab,
            provider,
        }
    }

    fn wait(&mut self, what: &str, mut done: impl FnMut(&mut TabSet) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.set.pump(&self.dir, true);
            if done(&mut self.set) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: the tab did not reach the expected state"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn start(&mut self, turn: &str) {
        self.provider
            .queue(AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        let tab = self.tab;
        self.wait("the turn", |set| set.get(tab).unwrap().turn_running());
    }

    fn end(&mut self, turn: &str) {
        self.provider.queue(AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        let tab = self.tab;
        self.wait("the turn's end", |set| !set.get(tab).unwrap().turn_running());
    }

    fn set_mode(&mut self, mode: SessionModeChoice) {
        self.set.get_mut(self.tab).unwrap().set_mode_for_test(mode);
    }

    /// The phone's sends answered by now, waiting for the first.
    fn sends_answered(&mut self) -> Vec<RemoteSent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done = self.set.poll_remote_sends();
            if !done.is_empty() {
                return done;
            }
            assert!(Instant::now() < deadline, "the phone's send was never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The agent's own cycle runs before it reads the opening prompt, which goes back to the queue as a message
    /// sent into that turn; its client id.
    fn cycle_back(&mut self, turn: &str) -> String {
        let id = self
            .provider
            .turn_stamps()
            .last()
            .unwrap()
            .client_message_id
            .clone()
            .unwrap();
        self.provider.queue(AgentDomainEvent::CliCycleStarted {
            turn_id: turn.into(),
            cause: None,
        });
        let tab = self.tab;
        self.wait("the prompt back in the queue", |set| {
            set.get(tab).unwrap().session.queue.iter().any(|item| !item.is_held())
        });
        id
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for removed in self.set.take_all() {
            if let Some(retired) = removed.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

fn install(set: &mut TabSet, tab: TabId, provider: Arc<RecordingProvider>, dir: &Path) -> TabBackend {
    let conversation =
        agent::AgentConversation::create(provider, dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    set.get_mut(tab)
        .unwrap()
        .set_backend(TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        }))
}

#[test]
fn a_phone_message_put_into_a_turn_is_not_sent_again_once_the_mode_widened() {
    use SessionModeChoice::{Auto, Bypass, Manual};
    for (from, to) in [(Manual, Auto), (Auto, Bypass)] {
        let mut f = Fixture::new(&format!("remote-steered-widened-{from:?}"), from);
        f.start("turn-1");
        f.set.queue_remote(f.tab, "phone words", asker(), 1).unwrap();
        f.set.steer_held(f.tab, 1, Some("turn-1"));
        assert_eq!(f.provider.steers().len(), 1, "it went into the turn: {from:?}");
        f.set_mode(to);
        // The turn ends with no word on whether the agent read it.
        f.end("turn-1");
        assert!(f.set.flush_queue(f.tab).is_none(), "{from:?} -> {to:?}");
        assert!(
            f.provider.turns().is_empty(),
            "the phone's words were not sent as a turn in {to:?}: {:?}",
            f.provider.turns()
        );
        let refused = f.set.take_remote_refused();
        assert_eq!(refused.len(), 1, "{from:?} -> {to:?}");
        assert_eq!((refused[0].accepted_mode, refused[0].mode), (from, to));
        assert!(f.set.queue_texts(f.tab).is_empty());
        assert!(!f.set.starting.contains(&f.tab.0), "no start is left behind");
    }
}

#[test]
fn a_phone_send_handed_back_to_the_queue_stays_the_phones() {
    let mut f = Fixture::new("remote-send-cycled", SessionModeChoice::Auto);
    f.set.send_remote(f.tab, "phone words", asker()).unwrap();
    let sent = f.sends_answered();
    assert!(matches!(&sent[..], [RemoteSent { outcome: Ok(()), .. }]), "{sent:?}");
    f.start("turn-1");
    let id = f.cycle_back("turn-1");
    let item = f.set.get(f.tab).unwrap().session.queue[0].clone();
    assert_eq!(item.text, "phone words");
    assert_eq!(
        item.from,
        QueuedFrom::Remote {
            asker: asker(),
            accepted_mode: SessionModeChoice::Auto
        },
        "the queue line is the phone's, with the mode it was taken in"
    );
    // The agent drops it; the tab goes to Bypass before the queue can send it again.
    f.provider.queue(AgentDomainEvent::UserMessageDropped {
        client_message_id: id,
        reason: "Claude cancelled it".into(),
    });
    let tab = f.tab;
    f.wait("held again", |set| set.get(tab).unwrap().session.queue[0].is_held());
    f.set_mode(SessionModeChoice::Bypass);
    f.end("turn-1");
    assert!(f.set.flush_queue(f.tab).is_none());
    assert_eq!(f.provider.turns(), ["phone words"], "sent once, never in Bypass");
    assert_eq!(f.set.take_remote_refused().len(), 1);
}

#[test]
fn a_phone_message_goes_in_a_turn_of_its_own_and_stays_the_phones() {
    let mut f = Fixture::new("remote-queue-own-turn", SessionModeChoice::Manual);
    f.start("turn-0");
    f.set.queue_remote(f.tab, "phone words", asker(), 1).unwrap();
    f.set
        .queue_message(f.tab, "mine", "mine".into(), 2, true, PromptOrigin::Typed)
        .unwrap();
    f.end("turn-0");
    let flush = f.set.flush_queue(f.tab).expect("the queue goes");
    assert_eq!(flush.typed, "phone words", "never one turn with the desk's words");
    assert!(flush.outcome.is_ok());
    assert_eq!(f.provider.turns(), ["phone words"]);
    assert_eq!(f.set.queue_texts(f.tab), ["mine"]);
    f.start("turn-1");
    f.cycle_back("turn-1");
    let queue = &f.set.get(f.tab).unwrap().session.queue;
    let back = queue.iter().find(|item| !item.is_held()).unwrap();
    assert_eq!(back.text, "phone words");
    assert!(
        matches!(&back.from, QueuedFrom::Remote { asker, accepted_mode: SessionModeChoice::Manual } if asker.device_id == "d1"),
        "{:?}",
        back.from
    );
}

#[test]
fn a_phone_send_answered_after_its_session_was_replaced_changes_nothing_in_the_tab() {
    let mut f = Fixture::new("remote-send-replaced", SessionModeChoice::Auto);
    let release = f.provider.hold_calls();
    f.set.send_remote(f.tab, "phone words", asker()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.provider.calls_waiting() == 0 {
        assert!(Instant::now() < deadline, "the call never reached the provider");
        std::thread::sleep(Duration::from_millis(2));
    }
    let fresh = Arc::new(RecordingProvider::steering());
    let old = install(&mut f.set, f.tab, fresh.clone(), &f.dir);
    drop(release);
    let sent = f.sends_answered();
    assert!(
        matches!(&sent[..], [RemoteSent { outcome: Ok(()), .. }]),
        "the old session took it: {sent:?}"
    );
    let tab = f.set.get(f.tab).unwrap();
    assert!(tab.session.opening_prompt.is_none(), "the new session was sent nothing");
    assert!(!f.set.starting.contains(&f.tab.0), "the old session's start is over");
    assert!(fresh.turns().is_empty());
    if let TabBackend::Live(mut old) = old {
        old.shutdown();
    }
}

#[test]
fn every_phone_send_is_counted_as_a_call_out() {
    let mut f = Fixture::new("remote-send-counted", SessionModeChoice::Auto);
    let release = f.provider.hold_calls();
    assert_eq!(f.set.remote_sends_out(), 0);
    f.set.send_remote(f.tab, "phone words", asker()).unwrap();
    assert_eq!(f.set.remote_sends_out(), 1);
    drop(release);
    f.sends_answered();
    assert_eq!(f.set.remote_sends_out(), 0);
}
