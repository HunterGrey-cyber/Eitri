use eitri_core::agent_bridge::TabStateWire;
use eitri_core::agents::{derive, Class, EndedHow, State, TabInputs};

fn live() -> TabInputs {
    TabInputs {
        wire: TabStateWire::Live,
        cards: 0,
        editor_cards: 0,
        questions: 0,
        unread: false,
        turn_running: false,
        cli_unseen_busy: false,
        background_calls: 0,
        stopped_resumable: false,
        failed_after_start: false,
        backend_failed: false,
        closed: false,
    }
}

#[test]
fn each_state_from_its_inputs() {
    let cases = [
        (TabInputs { cards: 1, ..live() }, State::Card),
        (
            TabInputs {
                editor_cards: 1,
                ..live()
            },
            State::Card,
        ),
        (TabInputs { questions: 1, ..live() }, State::Question),
        (
            TabInputs {
                wire: TabStateWire::AwaitingTrust,
                ..live()
            },
            State::Trust,
        ),
        (
            TabInputs {
                backend_failed: true,
                ..live()
            },
            State::Failed,
        ),
        (TabInputs { closed: true, ..live() }, State::Ended),
        (
            TabInputs {
                turn_running: true,
                ..live()
            },
            State::Running,
        ),
        (
            TabInputs {
                background_calls: 1,
                ..live()
            },
            State::Background,
        ),
        (
            TabInputs {
                wire: TabStateWire::Starting,
                ..live()
            },
            State::Starting,
        ),
        (TabInputs { unread: true, ..live() }, State::Done),
        (live(), State::Idle),
        (
            TabInputs {
                wire: TabStateWire::NotStarted,
                ..live()
            },
            State::New,
        ),
    ];
    for (inputs, expected) in cases {
        assert_eq!(derive(&inputs).0, expected);
    }
    assert_eq!(
        derive(&TabInputs {
            wire: TabStateWire::Failed,
            ..live()
        }),
        (State::Failed, Some(EndedHow::StartFailed))
    );
    assert_eq!(
        derive(&TabInputs {
            wire: TabStateWire::Ended,
            ..live()
        }),
        (State::Failed, Some(EndedHow::Unavailable))
    );
}

#[test]
fn precedence_pairs() {
    for (inputs, expected) in [
        (
            TabInputs {
                cards: 1,
                questions: 1,
                ..live()
            },
            State::Card,
        ),
        (
            TabInputs {
                questions: 1,
                wire: TabStateWire::AwaitingTrust,
                ..live()
            },
            State::Question,
        ),
        (
            TabInputs {
                backend_failed: true,
                turn_running: true,
                ..live()
            },
            State::Failed,
        ),
        (
            TabInputs {
                closed: true,
                turn_running: true,
                ..live()
            },
            State::Ended,
        ),
        (
            TabInputs {
                turn_running: true,
                background_calls: 2,
                ..live()
            },
            State::Running,
        ),
        (
            TabInputs {
                background_calls: 2,
                wire: TabStateWire::Starting,
                ..live()
            },
            State::Background,
        ),
        (
            TabInputs {
                unread: true,
                turn_running: true,
                ..live()
            },
            State::Running,
        ),
        (
            TabInputs {
                unread: true,
                wire: TabStateWire::NotStarted,
                ..live()
            },
            State::New,
        ),
        (
            TabInputs {
                unread: true,
                wire: TabStateWire::Starting,
                ..live()
            },
            State::Starting,
        ),
    ] {
        assert_eq!(derive(&inputs).0, expected);
    }
}

#[test]
fn background_only_reads_background_not_idle() {
    assert_eq!(
        derive(&TabInputs {
            background_calls: 1,
            ..live()
        })
        .0,
        State::Background
    );
}

#[test]
fn a_cli_started_turn_reads_running() {
    assert_eq!(
        derive(&TabInputs {
            cli_unseen_busy: true,
            ..live()
        })
        .0,
        State::Running
    );
}

#[test]
fn a_dead_session_with_a_card_left_reads_ended_with_no_needs() {
    let inputs = TabInputs {
        closed: true,
        cards: 0,
        wire: TabStateWire::Ended,
        ..live()
    };
    assert_eq!(derive(&inputs), (State::Ended, Some(EndedHow::Closed)));
    assert_eq!(inputs.cards + inputs.editor_cards + inputs.questions, 0);
}

#[test]
fn a_resumable_stop_reads_ended_stopped() {
    assert_eq!(
        derive(&TabInputs {
            stopped_resumable: true,
            wire: TabStateWire::Ended,
            ..live()
        }),
        (State::Ended, Some(EndedHow::Stopped))
    );
}

#[test]
fn a_failure_after_start_reads_failed_tripped() {
    assert_eq!(
        derive(&TabInputs {
            failed_after_start: true,
            backend_failed: true,
            ..live()
        }),
        (State::Failed, Some(EndedHow::Tripped))
    );
}

#[test]
fn background_calls_with_an_empty_roster_still_read_background() {
    let background = eitri_core::agents::Background {
        calls: 2,
        ..Default::default()
    };
    assert_eq!(
        (
            background.shells,
            background.monitors,
            background.agents,
            background.other
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        derive(&TabInputs {
            background_calls: background.calls,
            ..live()
        })
        .0,
        State::Background
    );
}

#[test]
fn every_state_has_its_class_and_the_class_set_is_four() {
    let mut classes = std::collections::BTreeSet::new();
    for (state, class) in [
        (State::Card, Class::Needs),
        (State::Question, Class::Needs),
        (State::Trust, Class::Needs),
        (State::Failed, Class::Ended),
        (State::Ended, Class::Ended),
        (State::Running, Class::Working),
        (State::Background, Class::Working),
        (State::Starting, Class::Working),
        (State::Done, Class::Quiet),
        (State::Idle, Class::Quiet),
        (State::New, Class::Quiet),
    ] {
        assert_eq!(state.class(), class);
        assert_eq!(State::parse(state.as_str()), Some(state));
        classes.insert(serde_json::to_string(&class).unwrap());
    }
    assert_eq!(classes.len(), 4);
    assert_eq!(State::parse("paused"), None);
}
