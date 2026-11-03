//! The trust question: a start put off, and who may answer it.
#![cfg(test)]

use super::*;

fn pending_connect() -> PendingStart {
    PendingStart {
        request_id: "r-pending".into(),
        result_rx: mpsc::channel().1,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    }
}

/// A set whose active tab waits for the trust question, already asked: `(set, tab, nonce)`.
fn asked(start: DeferredStart) -> (TabSet, TabId, u64) {
    let mut set = set();
    let tab = set.active();
    set.defer_start(tab, start).unwrap();
    let nonce = set.ask_trust(tab).unwrap();
    (set, tab, nonce)
}

#[test]
fn defer_start_needs_an_empty_tab() {
    let dir = workspace("trust-defer-needs-empty");
    let mut set = set();
    let starting = set.active();
    set.get_mut(starting).unwrap().backend = TabBackend::Starting(pending_connect());
    assert!(set.defer_start(starting, first_send("r1", "hi")).is_err(), "Starting");
    let running = set.open(AgentId::Claude);
    give_session(&mut set, running, &dir, "sess-live");
    assert!(set.defer_start(running, first_send("r2", "hi")).is_err(), "Live");
    let empty = set.open(AgentId::Claude);
    set.defer_start(empty, first_send("r3", "hi")).unwrap();
    assert!(
        set.defer_start(empty, first_send("r4", "again")).is_err(),
        "AwaitingTrust"
    );
    assert!(
        set.defer_start(TabId(999), first_send("r5", "hi")).is_err(),
        "no such tab"
    );
    assert!(
        matches!(set.get(starting).unwrap().backend, TabBackend::Starting(_)),
        "a refused deferral changes nothing"
    );
    shut_down_all(&mut set);
}

#[test]
fn a_waiting_tab_is_shown_as_waiting_for_trust_not_as_starting() {
    let (set, tab, _) = asked(first_send("r1", "hi"));
    assert_eq!(set.get(tab).unwrap().wire_state(), TabStateWire::AwaitingTrust);
    assert_eq!(TabStateWire::AwaitingTrust.as_str(), "awaiting_trust");
    assert!(
        set.close_needs_confirm(tab),
        "closing it would lose the message it holds"
    );
}

#[test]
fn ask_trust_rotates_the_nonce() {
    let (mut set, tab, first) = asked(first_send("r1", "hi"));
    let second = set.ask_trust(tab).unwrap();
    assert_ne!(first, second);
    assert_ne!(first, 0, "0 is never a valid nonce");
    assert_eq!(awaiting(&set, tab).asking, Some(second));
    assert!(set.ask_trust(TabId(999)).is_err());
    let empty = set.open(AgentId::Claude);
    assert!(set.ask_trust(empty).is_err(), "a tab with nothing to ask about");
}

#[test]
fn an_old_nonce_is_refused() {
    let (mut set, tab, first) = asked(first_send("r1", "hi"));
    let second = set.ask_trust(tab).unwrap();
    assert!(set.check_trust_answer(tab, first, ViewId::BASE).is_err());
    assert!(set.mark_recording(tab, first, ViewId::BASE).is_err());
    assert!(set.take_answered(tab, first, ViewId::BASE).is_err());
    assert!(set.cancel_trust(tab, first, ViewId::BASE).is_err());
    assert!(
        awaiting(&set, tab).asking == Some(second),
        "refusals leave the question as it was"
    );
    assert!(set.check_trust_answer(tab, second, ViewId::BASE).is_ok());
    assert!(set.check_trust_answer(tab, 0, ViewId::BASE).is_err());
}

#[test]
fn an_answer_for_a_tab_not_on_screen_is_refused() {
    let (mut set, first, _) = asked(first_send("r1", "hi"));
    let other = set.open(AgentId::Claude);
    assert_eq!(set.active(), other);
    let nonce = set.ask_trust(first).unwrap();
    let why = set.check_trust_answer(first, nonce, ViewId::BASE).unwrap_err();
    assert!(why.contains("not the one on screen"), "{why}");
    assert!(set.mark_recording(first, nonce, ViewId::BASE).is_err());
    assert!(set.take_answered(first, nonce, ViewId::BASE).is_err());
    assert!(set.cancel_trust(first, nonce, ViewId::BASE).is_err());
    assert!(matches!(set.get(first).unwrap().backend, TabBackend::AwaitingTrust(_)));
    assert!(
        set.check_trust_answer(other, nonce, ViewId::BASE).is_err(),
        "and not for a tab that waits for nothing"
    );
    set.select(first);
    let nonce = set.ask_trust(first).unwrap();
    assert!(
        set.check_trust_answer(first, nonce, ViewId::BASE).is_ok(),
        "on screen again, asked again"
    );
}

#[test]
fn routing_away_invalidates_the_prompt() {
    for how in ["select", "drop_bypass_prompt", "cycle_mode", "cycle_default_mode"] {
        let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
        match how {
            "select" => {
                let other = set.open(AgentId::Claude);
                assert_ne!(other, tab);
                set.select(tab);
            }
            "drop_bypass_prompt" => set.drop_bypass_prompt(),
            "cycle_mode" => {
                let _ = set.cycle_mode(tab, ViewId::BASE);
            }
            _ => {
                let _ = set.cycle_default_mode(ViewId::BASE);
            }
        }
        assert!(
            set.check_trust_answer(tab, nonce, ViewId::BASE).is_err(),
            "{how}: the old nonce is refused"
        );
        assert!(set.take_answered(tab, nonce, ViewId::BASE).is_err(), "{how}");
        let waiting = awaiting(&set, tab);
        assert_eq!(waiting.asking, None, "{how}");
        assert!(!waiting.recording, "{how}");
        let fresh = set.ask_trust(tab).unwrap();
        assert!(
            set.check_trust_answer(tab, fresh, ViewId::BASE).is_ok(),
            "{how}: showing it again asks again"
        );
    }
}

#[test]
fn check_trust_answer_consumes_nothing() {
    let (set, tab, nonce) = asked(first_send("r1", "hi"));
    assert!(set.check_trust_answer(tab, nonce + 1, ViewId::BASE).is_err());
    assert!(set.check_trust_answer(tab, nonce, ViewId::BASE).is_ok());
    assert!(
        set.check_trust_answer(tab, nonce, ViewId::BASE).is_ok(),
        "a second look still passes"
    );
    let waiting = awaiting(&set, tab);
    assert_eq!(waiting.asking, Some(nonce));
    assert!(!waiting.recording);
    assert!(matches!(waiting.start, DeferredStart::FirstSend { .. }));
}

#[test]
fn an_accepted_answer_hands_the_start_over_once() {
    let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
    let start = set.take_answered(tab, nonce, ViewId::BASE).unwrap();
    assert_eq!(start.request_id(), Some("r1"));
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
    assert!(
        set.take_answered(tab, nonce, ViewId::BASE).is_err(),
        "the answer is spent"
    );

    let (mut set, tab, nonce) = asked(resume_of("r2", "sess-9"));
    let start = set.cancel_trust(tab, nonce, ViewId::BASE).unwrap();
    assert!(matches!(start, DeferredStart::Resume { ref provider_session_id, .. } if provider_session_id == "sess-9"));
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
    assert!(set.awaiting_trust().is_empty());
}

#[test]
fn a_recording_tab_waits_for_its_record() {
    let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
    set.mark_recording(tab, nonce, ViewId::BASE).unwrap();
    let waiting = awaiting(&set, tab);
    assert!(waiting.recording);
    assert_eq!(waiting.asking, None);
    assert!(
        set.check_trust_answer(tab, nonce, ViewId::BASE).is_err(),
        "the nonce is spent"
    );
    assert!(set.take_answered(tab, nonce, ViewId::BASE).is_err());
    assert!(set.cancel_trust(tab, nonce, ViewId::BASE).is_err());
    assert!(
        set.mark_recording(tab, nonce, ViewId::BASE).is_err(),
        "no second record for one answer"
    );
    assert!(
        set.take_deferred(tab).is_none(),
        "the record's result decides, not the no-ask route"
    );
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::AwaitingTrust(_)));

    // The user went to another tab meanwhile: the record's result still finds its start.
    let other = set.open(AgentId::Claude);
    assert_eq!(set.active(), other);
    let start = set.take_recorded(tab).expect("the start is still held");
    assert_eq!(start.request_id(), Some("r1"));
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
    assert!(set.take_recorded(tab).is_none(), "handed over once");

    // A record that failed asks the tab again, which ends the wait.
    let (mut set, tab, nonce) = asked(first_send("r2", "hi"));
    set.mark_recording(tab, nonce, ViewId::BASE).unwrap();
    let again = set.ask_trust(tab).unwrap();
    assert_ne!(again, nonce);
    let waiting = awaiting(&set, tab);
    assert!(!waiting.recording);
    assert_eq!(waiting.asking, Some(again));
    assert!(set.take_recorded(tab).is_none(), "nothing is being recorded any more");
}

#[test]
fn take_recorded_and_take_deferred_need_the_right_state() {
    let mut set = set();
    let tab = set.active();
    assert!(set.take_deferred(tab).is_none(), "an empty tab holds nothing");
    assert!(set.take_recorded(tab).is_none());
    set.defer_start(tab, first_send("r1", "hi")).unwrap();
    assert!(set.take_recorded(tab).is_none(), "not recording");
    let start = set.take_deferred(tab).expect("the no-ask route needs no question");
    assert_eq!(start.request_id(), Some("r1"));
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
    assert!(set.take_deferred(TabId(999)).is_none());
}

#[test]
fn reset_hands_back_a_deferred_first_send() {
    let (mut set, tab, _) = asked(first_send("r1", "fix the build"));
    set.get_mut(tab).unwrap().replace_draft("and then the tests".into());
    let old = set.reset(tab).expect("a waiting tab starts over");
    assert!(old.is_none(), "nothing was spawned");
    let t = set.get(tab).unwrap();
    assert!(matches!(t.backend, TabBackend::NotStarted));
    assert_eq!(
        t.session.draft, "fix the build\n\nand then the tests",
        "the typed text comes back first"
    );
    assert_eq!(t.session.project_tiers, None);
    assert!(set.awaiting_trust().is_empty());

    // A resume has no text to give back: it is dropped.
    let (mut set, tab, _) = asked(resume_of("r2", "sess-1"));
    assert!(set.reset(tab).unwrap().is_none());
    assert!(set.get(tab).unwrap().session.draft.is_empty());
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
}

#[test]
fn closing_an_awaiting_tab_drops_its_start() {
    let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
    let other = set.open(AgentId::Claude);
    let closed = set.remove(tab).expect("the tab closes");
    match closed.tab.backend {
        TabBackend::AwaitingTrust(waiting) => assert_eq!(waiting.start.request_id(), Some("r1")),
        _ => panic!("the tab was waiting"),
    }
    assert!(set.get(tab).is_none());
    assert!(set.awaiting_trust().is_empty());
    assert!(
        set.check_trust_answer(tab, nonce, ViewId::BASE).is_err(),
        "no answer reaches a closed tab"
    );
    assert_eq!(set.active(), other);
}

#[test]
fn an_awaiting_tab_refuses_a_second_send_and_a_resume() {
    let (mut set, tab, _) = asked(resume_of("r1", "sess-wait"));
    assert!(
        set.defer_start(tab, first_send("r2", "again")).is_err(),
        "a second start"
    );
    assert_eq!(
        set.get(tab).unwrap().provider_session_id().as_deref(),
        Some("sess-wait"),
        "the session it will resume is known"
    );
    let other = set.open(AgentId::Claude);
    // Resuming the same session again switches to the tab that already holds it: no second start.
    assert!(
        matches!(set.route_resume(other, &AgentId::Claude, "sess-wait", false), ResumeRoute::SwitchTo(t) if t == tab)
    );
    // A tab with a message waiting is not "empty": a resume from it opens another tab.
    let (mut set, waiting, _) = asked(first_send("r3", "hi"));
    match set.route_resume(waiting, &AgentId::Claude, "sess-new", false) {
        ResumeRoute::StartIn(target) => assert_ne!(target, waiting),
        _ => panic!("expected a new tab"),
    }
    assert!(matches!(
        set.get(waiting).unwrap().backend,
        TabBackend::AwaitingTrust(_)
    ));
}

#[test]
fn a_deferred_restore_starts_nothing() {
    let mut set = set();
    let from = set.active();
    let plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
        ],
        None,
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing in bypass, nothing to ask")
    };
    assert_eq!(go.session_ids(), ["a", "b"]);
    set.defer_start(from, DeferredStart::Restore { go }).unwrap();
    assert_eq!(set.awaiting_trust(), [from]);
    assert_eq!(set.tabs().len(), 1, "no tab was opened for the sessions");
    assert!(
        set.tabs().iter().all(|t| !matches!(t.backend, TabBackend::Starting(_))),
        "nothing is starting"
    );
    assert!(set.open_sessions().is_empty(), "no lease or session is named yet");
    let nonce = set.ask_trust(from).unwrap();
    let DeferredStart::Restore { go } = set.take_answered(from, nonce, ViewId::BASE).unwrap() else {
        panic!("the restore comes back")
    };
    assert_eq!(
        go.session_ids(),
        ["a", "b"],
        "intact, ready to start when the answer allows it"
    );
}
