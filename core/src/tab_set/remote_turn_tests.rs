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
        authority: None,
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
        let deadline = Instant::now() + Duration::from_secs(60);
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
        let deadline = Instant::now() + Duration::from_secs(60);
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
    let deadline = Instant::now() + Duration::from_secs(60);
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

fn stamped_phone(dir: &Path) -> (PathBuf, RemoteAsker) {
    let store = dir.join("devices.json");
    agent::private_fs::write_private(
        &store,
        serde_json::json!({"v": 1, "devices": [{
            "id": "d1", "name": "Pixel 8", "token_sha256": crate::remote_devices::token_hash_hex("tok"),
            "grant": "watch+steer"
        }]})
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    let (_, _, stamp) = crate::remote_devices::authorize_act(&store, "d1", "tok")
        .unwrap()
        .unwrap();
    (
        store,
        RemoteAsker {
            authority: Some(stamp),
            ..asker()
        },
    )
}

#[test]
fn only_held_non_refused_stamps_keep_fast_polling() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir("remote-polling");
    let (_, phone) = stamped_phone(&dir);
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let now = Instant::now();
    let mut item = Queued::held("phone", "phone".into(), 1);
    item.from = QueuedFrom::Remote {
        asker: phone,
        accepted_mode: SessionModeChoice::Auto,
    };
    set.get_mut(tab).unwrap().session.queue.push(item);
    assert!(set.needs_polling());
    assert_eq!(set.remote_poll_deadline(now), None);

    set.get_mut(tab).unwrap().session.queue[0].refused = true;
    assert!(!set.needs_polling());
    assert_eq!(set.remote_poll_deadline(now), Some(now + Duration::from_secs(1)));

    let item = &mut set.get_mut(tab).unwrap().session.queue[0];
    item.refused = false;
    item.state = QueuedState::Steered {
        client_id: "c1".into(),
        sent_at_ms: 2,
    };
    assert!(!set.needs_polling());
    assert_eq!(set.remote_poll_deadline(now), Some(now + Duration::from_secs(1)));

    set.get_mut(tab).unwrap().session.queue[0].from = QueuedFrom::Desk;
    assert!(!set.needs_polling());
    assert_eq!(set.remote_poll_deadline(now), None);
}

#[test]
fn withdrawn_phone_messages_never_leave_the_queue() {
    for revoke in [true, false] {
        for steer in [true, false] {
            let mut f = Fixture::new("withdraw-held", SessionModeChoice::Auto);
            f.start("t1");
            let (store, phone) = stamped_phone(&f.dir);
            f.set.queue_remote(f.tab, "secret one", phone.clone(), 1).unwrap();
            f.set.queue_remote(f.tab, "secret two", phone, 2).unwrap();
            let other = RemoteAsker {
                device_id: "d2".into(),
                ..asker()
            };
            f.set.queue_remote(f.tab, "other phone", other, 3).unwrap();
            f.set
                .queue_message(f.tab, "desk", "desk".into(), 4, true, PromptOrigin::Typed)
                .unwrap();
            crate::remote_devices::update(&store, |s| {
                if revoke {
                    s.remove("d1");
                } else {
                    s.get_mut("d1").unwrap().grant = crate::remote_devices::Grant::Watch;
                }
            })
            .unwrap();
            if steer {
                f.set.steer_held(f.tab, 5, Some("t1"));
            } else {
                f.end("t1");
                f.set.flush_queue(f.tab);
            }
            let tab = f.set.get(f.tab).unwrap();
            assert!(tab
                .session
                .queue
                .iter()
                .all(|q| q.text != "secret one" && q.text != "secret two"));
            assert!(f.provider.turns().iter().all(|text| !text.contains("secret")));
            if steer {
                assert_eq!(
                    f.provider.steers().iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
                    vec!["other phone", "desk"]
                );
            }
            if !steer {
                assert_eq!(f.provider.turns(), vec!["other phone"]);
                assert_eq!(
                    tab.session.queue.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(),
                    vec!["desk"]
                );
            }
        }
    }
}

#[test]
fn moved_phone_messages_keep_their_withdrawal_stamp() {
    for missing_counter in [false, true] {
        let mut f = Fixture::new("withdraw-moved", SessionModeChoice::Auto);
        f.start("t1");
        let (store, phone) = stamped_phone(&f.dir);
        f.set.queue_remote(f.tab, "secret", phone, 1).unwrap();
        let wire = serde_json::to_string(&f.set.get(f.tab).unwrap().session.queue).unwrap();
        if missing_counter {
            std::fs::remove_file(
                store.with_extension(format!("authority-{}", crate::remote_devices::token_hash_hex("d1"))),
            )
            .unwrap();
        }
        f.set.get_mut(f.tab).unwrap().session.queue = serde_json::from_str(&wire).unwrap();
        if !missing_counter {
            crate::remote_devices::update(&store, |s| s.remove("d1")).unwrap();
        }
        f.end("t1");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !f.set.get(f.tab).unwrap().session.queue.is_empty() {
            f.set.flush_queue(f.tab);
            assert!(f.provider.turns().is_empty(), "a moved withdrawn message ran");
            assert!(Instant::now() < deadline, "the moved authority never loaded");
            std::thread::yield_now();
        }
        assert!(f.provider.turns().is_empty());
    }
}

#[test]
fn withdrawal_leaves_an_inflight_steer_but_prevents_its_retry() {
    let mut f = Fixture::new("withdraw-steered", SessionModeChoice::Auto);
    f.start("t1");
    let (store, phone) = stamped_phone(&f.dir);
    f.set.queue_remote(f.tab, "already sent", phone, 1).unwrap();
    f.set.steer_held(f.tab, 2, Some("t1"));
    assert_eq!(f.provider.steers().len(), 1);
    crate::remote_devices::update(&store, |s| s.remove("d1")).unwrap();
    f.set.pump(&f.dir, true);
    assert_eq!(f.set.get(f.tab).unwrap().session.queue.len(), 1);
    assert!(f.set.take_remote_withdrawn().is_empty());
    f.end("t1");
    f.set.flush_queue(f.tab);
    assert!(f.provider.turns().is_empty());
    assert_eq!(f.set.take_remote_withdrawn().len(), 1);
}

#[test]
fn moved_authorized_phone_messages_resume_after_counter_loading() {
    let mut f = Fixture::new("withdraw-move-valid", SessionModeChoice::Auto);
    f.start("t1");
    let (_, phone) = stamped_phone(&f.dir);
    f.set.queue_remote(f.tab, "still allowed", phone, 1).unwrap();
    let wire = serde_json::to_string(&f.set.get(f.tab).unwrap().session.queue).unwrap();
    f.set.get_mut(f.tab).unwrap().session.queue = serde_json::from_str(&wire).unwrap();
    f.end("t1");
    let tab = f.tab;
    f.wait("moved counter loaded", |set| {
        set.flush_queue(tab);
        set.queue_texts(tab).is_empty()
    });
    assert_eq!(f.provider.turns(), vec!["still allowed"]);
}

#[test]
fn a_device_queues_at_most_its_cap_of_messages_in_a_tab() {
    let mut f = Fixture::new("remote-queue-cap-items", SessionModeChoice::Manual);
    f.start("turn-1");
    for n in 0..REMOTE_QUEUED_MAX_ITEMS {
        f.set
            .queue_remote(f.tab, &format!("phone words {n}"), asker(), 1)
            .unwrap();
    }
    assert_eq!(
        f.set.queue_remote(f.tab, "one more", asker(), 1),
        Err(REMOTE_QUEUE_FULL.to_string())
    );
    assert_eq!(f.set.queue_texts(f.tab).len(), REMOTE_QUEUED_MAX_ITEMS);
    // Another device's room and the desk's queue are their own.
    let other = RemoteAsker {
        device_id: "d2".into(),
        ..asker()
    };
    f.set.queue_remote(f.tab, "other phone", other, 1).unwrap();
    for n in 0..REMOTE_QUEUED_MAX_ITEMS {
        let text = format!("desk words {n}");
        f.set
            .queue_message(f.tab, &text, text.clone(), 1, true, PromptOrigin::Typed)
            .unwrap();
    }
    assert_eq!(f.set.queue_texts(f.tab).len(), 2 * REMOTE_QUEUED_MAX_ITEMS + 1);
    // Taking its messages back gives the device its room again.
    assert_eq!(f.set.take_back_remote(f.tab, "d1").len(), REMOTE_QUEUED_MAX_ITEMS);
    f.set.queue_remote(f.tab, "again", asker(), 1).unwrap();
}

#[test]
fn a_device_queues_at_most_its_cap_of_text_in_a_tab() {
    let mut f = Fixture::new("remote-queue-cap-bytes", SessionModeChoice::Manual);
    f.start("turn-1");
    let text = "x".repeat(64 * 1024);
    let fit = REMOTE_QUEUED_MAX_BYTES / text.len();
    assert!(
        fit < REMOTE_QUEUED_MAX_ITEMS,
        "the text cap, not the count, is what refuses here"
    );
    for _ in 0..fit {
        f.set.queue_remote(f.tab, &text, asker(), 1).unwrap();
    }
    assert_eq!(
        f.set.queue_remote(f.tab, "y", asker(), 1),
        Err(REMOTE_QUEUE_FULL.to_string())
    );
    assert_eq!(f.set.queue_texts(f.tab).len(), fit);
    let desk = "z".repeat(2 * REMOTE_QUEUED_MAX_BYTES);
    f.set
        .queue_message(f.tab, &desk, desk.clone(), 1, true, PromptOrigin::Typed)
        .unwrap();
}
