//! `prefix u` and `prefix B`: the round over the agents that wait for the person, from a key.
//!
//! The round is one shared file every window reads (`crate::agents::shared`); its order is computed from the
//! view each time. A key here decides nothing on disk: it asks the host for [`HostOp::AgentsScan`], which reads the
//! view and the round once on a worker, decides the step with [`on_next`] or [`on_back`], writes the round and
//! focuses the stop, then answers [`HostEvent::Agents`] with what it came to. The decisions are pure functions of
//! the view, so every window reaches the same one.
//!
//! The automatic advance after an answer is the same step taken by the process that owns the round's current
//! row, once that row stopped waiting while that window had the keys ([`after_answer`]).
//!
//! A round that ends with the person still in it (every stop visited, or nothing left waiting) keeps its file with
//! no current stop, so `prefix B` still goes back to where it started; a round idle for [`ROUND_MAX_IDLE_MS`] is over
//! for both keys and for the automatic advance.

use super::chrome;
use super::host::{Batch, HostOp};
use super::toasts::ToastKind;
use super::window::{WinId, Window};
use crate::agents::shared::{advance, should_advance, start_at, Round, RoundPoint};
use crate::agents::{Class, View, ROUND_MAX_IDLE_MS};
use crate::keymap::Action;
use crate::layout::ModuleId;
use std::time::Duration;

/// Which round key ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundKey {
    /// `prefix u`: start the round, or go to its next stop.
    Next,
    /// `prefix B`: leave the round and go back to where it started.
    Back,
}

/// What `prefix u` (or an answer that moves the round on) does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStep {
    /// Focus the row `row_id` with its card first, after writing `round`.
    Go { row_id: String, round: Round },
    /// Every stop that waits was visited: the round ends.
    Ended,
    /// Nothing waits for the person: the round, if one ran, ends.
    NothingWaits,
}

/// What becomes of the round file after a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundFile {
    /// Left as it is.
    Keep,
    /// Replaced by this round.
    Write(Round),
    /// Removed.
    Remove,
}

impl NextStep {
    /// The round file after this step, given `read`, the file as it was read: a stop writes its round; a round that
    /// ends keeps its start with no current stop, so `prefix B` can still go back there; an idle one goes.
    pub fn round_file(&self, read: Option<&Round>, now_ms: u64) -> RoundFile {
        match (self, read) {
            (NextStep::Go { round, .. }, _) => RoundFile::Write(round.clone()),
            (NextStep::Ended | NextStep::NothingWaits, None) => RoundFile::Keep,
            (NextStep::Ended | NextStep::NothingWaits, Some(r)) if idle(r, now_ms) => RoundFile::Remove,
            (NextStep::Ended | NextStep::NothingWaits, Some(r)) if r.current.is_none() => RoundFile::Keep,
            (NextStep::Ended | NextStep::NothingWaits, Some(r)) => RoundFile::Write(Round {
                current: None,
                at_ms: now_ms,
                ..r.clone()
            }),
        }
    }
}

/// Whether `round` stood still for [`ROUND_MAX_IDLE_MS`] since its last move: then it is over.
fn idle(round: &Round, now_ms: u64) -> bool {
    now_ms.saturating_sub(round.at_ms) >= ROUND_MAX_IDLE_MS
}

/// What a round key came to, for the status line. Carried back by [`HostEvent::Agents`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// A stop, or the round's start, came forward.
    Went,
    /// `prefix B` goes back to this window's module where the round started, with no tab: the controller focuses it.
    BackTo(ModuleId),
    NothingWaits,
    Ended,
    /// `prefix B` with no round running: nothing happens.
    NoRound,
    /// `prefix B` to a start point that is gone.
    StartGone,
    /// This window runs without the agents layer.
    Off,
    /// The stop did not come forward, and why.
    Refused(String),
}

/// The status line's words for each outcome that says something.
pub const NOTHING_WAITS_TEXT: &str = "no agent waits for you";
pub const ENDED_TEXT: &str = "the round is over: every waiting agent was visited";
pub const START_GONE_TEXT: &str = "where the round started is gone";
pub const OFF_TEXT: &str = "agents are off in this window";

/// How long the status line shows a round key's words.
pub const STATUS_FOR: Duration = Duration::from_secs(2);

impl RoundOutcome {
    /// What the status line says, if anything.
    pub fn text(&self) -> Option<String> {
        match self {
            RoundOutcome::Went | RoundOutcome::BackTo(_) | RoundOutcome::NoRound => None,
            RoundOutcome::NothingWaits => Some(NOTHING_WAITS_TEXT.to_owned()),
            RoundOutcome::Ended => Some(ENDED_TEXT.to_owned()),
            RoundOutcome::StartGone => Some(START_GONE_TEXT.to_owned()),
            RoundOutcome::Off => Some(OFF_TEXT.to_owned()),
            RoundOutcome::Refused(why) => Some(format!("that agent did not come forward: {why}")),
        }
    }
}

/// The step `prefix u` takes from `here` (where the keys are) over `view`, with `round` the round file as read.
pub fn on_next(view: &View, round: Option<&Round>, here: &RoundPoint, now_ms: u64) -> NextStep {
    // A round that stood still too long, or one the person left by moving elsewhere by hand, is over: this press
    // starts a fresh one from where the keys are now.
    let running = round.filter(|r| !idle(r, now_ms) && at_current_stop(view, r, here));
    let next = match running {
        Some(r) => advance(view, r, now_ms),
        None => start_at(view, here.clone(), None, now_ms),
    };
    match next {
        Some(round) => go(view, round),
        None if waits(view) => NextStep::Ended,
        None => NextStep::NothingWaits,
    }
}

/// Whether anything in `view` waits for the person.
fn waits(view: &View) -> bool {
    view.rows.iter().any(|r| r.class == Class::Needs && !r.key.is_empty())
}

/// The row a round's stop names: the one with its key that is not mid-move, else any.
fn row_of<'a>(view: &'a View, key: &str) -> Option<&'a crate::agents::Row> {
    let mut rows = view.rows.iter().filter(|r| r.key == key);
    let first = rows.next()?;
    Some(if first.moving {
        rows.find(|r| !r.moving).unwrap_or(first)
    } else {
        first
    })
}

/// The step to `round`'s current stop, or the round's end when its stop names no row (never the case for a round
/// [`start_at`] or [`advance`] made from the same view).
fn go(view: &View, round: Round) -> NextStep {
    match round.current.as_deref().and_then(|key| row_of(view, key)) {
        Some(row) => NextStep::Go {
            row_id: row.id.clone(),
            round,
        },
        None => NextStep::Ended,
    }
}

/// Whether the keys are on the round's current stop: its row's process and tab.
fn at_current_stop(view: &View, round: &Round, here: &RoundPoint) -> bool {
    let Some(row) = round.current.as_deref().and_then(|key| row_of(view, key)) else {
        return false;
    };
    match (stop_of(&row.id), here.tab) {
        (Some((instance, tab)), Some(here_tab)) => instance == here.instance && tab == here_tab,
        _ => false,
    }
}

/// Where `prefix B` goes: the round's start, also after the round ended with the person in it; nowhere when no
/// round runs or the round has stood idle too long.
pub fn on_back(round: Option<&Round>, now_ms: u64) -> Option<RoundPoint> {
    round.filter(|r| !idle(r, now_ms)).map(|r| r.from.clone())
}

/// The step the process `own_instance` takes once one of its rows stopped waiting: `None` unless that row is the
/// round's current stop ([`should_advance`]) and the person answered it: the row left `needs` for work or quiet (a
/// session that failed or ended was not answered) while this window had the keys, in a round not yet idle too long.
/// Else the next stop or the round's end.
pub fn after_answer(own_instance: &str, view: &View, round: &Round, now_ms: u64) -> Option<NextStep> {
    if idle(round, now_ms) || !should_advance(own_instance, view, round) {
        return None;
    }
    let answered = round
        .current
        .as_deref()
        .and_then(|key| {
            view.rows
                .iter()
                .find(|row| row.key == key && row.window.instance == own_instance)
        })
        .is_some_and(|row| row.class != Class::Ended && row.window.focused);
    if !answered {
        return None;
    }
    Some(match advance(view, round, now_ms) {
        Some(next) => go(view, next),
        // With none left the round ends where it is: the person stays in the chat they just answered.
        None => NextStep::Ended,
    })
}

/// A row id's process instance and tab number (`<instance>.<tab>`).
pub fn stop_of(row_id: &str) -> Option<(&str, u64)> {
    let (instance, tab) = row_id.rsplit_once('.')?;
    Some((instance, tab.parse().ok()?)).filter(|(instance, _)| !instance.is_empty())
}

/// Runs `agents.next` or `agents.back`: the host reads the view and the round off this thread and answers with
/// [`HostEvent::Agents`].
pub(super) fn run(w: &mut Window, win: WinId, action: &Action, batch: &mut Batch) {
    let step = match action {
        Action::AgentsNext => RoundKey::Next,
        Action::AgentsBack => RoundKey::Back,
        other => {
            debug_assert!(false, "{other:?} is not a round key");
            return;
        }
    };
    // Where the keys are now, for the round to come back to: the module GTK last reported, else the layout's.
    let at = w
        .focus
        .native_module()
        .cloned()
        .or_else(|| w.layout.try_borrow().ok().map(|layout| layout.focus().clone()));
    w.say(format!("[agents] {}: reading every window's agents", action.name()));
    batch.push_effect(HostOp::AgentsScan { win, step, at });
}

/// What a round key came to: the status line's words, if any.
pub(super) fn outcome(w: &mut Window, outcome: &RoundOutcome) {
    w.say(format!("[agents] the round: {outcome:?}"));
    if let Some(text) = outcome.text() {
        chrome::show_toast_for(w, text, ToastKind::Other, STATUS_FOR);
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::agents::{Needs, Row, WindowRef};
    use crate::layout::{Axis, Layout, Node};
    use crate::wm::host::{Decide, HostEvent};
    use crate::wm::verbs::rig::{rig, WIN};

    /// `editor | agent`, the keys on the chat.
    fn two() -> Layout {
        Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::agent(),
        )
        .unwrap()
    }

    const NOW: u64 = 10_000_000;

    fn row(instance: &str, tab: u64, key: &str, class: Class, waiting_since: Option<u64>) -> Row {
        Row {
            id: format!("{instance}.{tab}"),
            key: key.into(),
            window: WindowRef {
                instance: instance.into(),
                name: "1".into(),
                focused: false,
            },
            class,
            since: waiting_since.unwrap_or(1),
            waiting_since,
            needs: Needs {
                cards: usize::from(class == Class::Needs),
                ..Needs::default()
            },
            ..Row::default()
        }
    }

    fn trust(instance: &str, tab: u64, key: &str, waiting_since: u64) -> Row {
        let mut r = row(instance, tab, key, Class::Needs, Some(waiting_since));
        r.needs = Needs {
            trust: true,
            ..Needs::default()
        };
        r
    }

    /// A row of the window with the keys, the way the glue hands it over once its wait was answered there.
    fn answered(instance: &str, tab: u64, key: &str, class: Class) -> Row {
        let mut r = row(instance, tab, key, class, None);
        r.window.focused = true;
        r
    }

    fn view(rows: Vec<Row>) -> View {
        View {
            rows,
            ..View::default()
        }
    }

    fn at(instance: &str, tab: Option<u64>) -> RoundPoint {
        RoundPoint {
            instance: instance.into(),
            module: "agent".into(),
            tab,
        }
    }

    fn go(step: NextStep) -> (String, Round) {
        match step {
            NextStep::Go { row_id, round } => (row_id, round),
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[test]
    fn the_oldest_wait_goes_first_across_processes() {
        let v = view(vec![
            row("aaaa", 1, "k-young", Class::Needs, Some(5_000)),
            row("bbbb", 4, "k-old", Class::Needs, Some(1_000)),
            row("bbbb", 5, "k-busy", Class::Working, None),
        ]);
        let here = at("cccc", None);
        let (row_id, round) = go(on_next(&v, None, &here, NOW));
        assert_eq!(row_id, "bbbb.4", "the row that has waited longest, in another process");
        assert_eq!(round.current.as_deref(), Some("k-old"));
        assert_eq!(round.from, here, "the round remembers where the keys were");
        assert_eq!(round.started_at_ms, NOW);
        assert_eq!(stop_of(&row_id), Some(("bbbb", 4)));
    }

    #[test]
    fn trust_rows_of_one_window_are_one_stop() {
        let v = view(vec![
            trust("aaaa", 1, "k-t1", 1_000),
            trust("aaaa", 2, "k-t2", 2_000),
            row("bbbb", 7, "k-card", Class::Needs, Some(3_000)),
        ]);
        let (first, round) = go(on_next(&v, None, &at("cccc", None), NOW));
        assert_eq!(first, "aaaa.1");
        let (second, round) = go(on_next(&v, Some(&round), &at("aaaa", Some(1)), NOW + 1));
        assert_eq!(
            second, "bbbb.7",
            "the other trust row of that window was settled with the first: no stop of its own"
        );
        assert!(round.visited.iter().any(|k| k == "k-t2"));
    }

    #[test]
    fn a_second_press_takes_the_next() {
        let v = view(vec![
            row("aaaa", 1, "k-a", Class::Needs, Some(1_000)),
            row("bbbb", 2, "k-b", Class::Needs, Some(2_000)),
        ]);
        let from = at("cccc", None);
        let (first, round) = go(on_next(&v, None, &from, NOW));
        assert_eq!(first, "aaaa.1");
        let (second, next) = go(on_next(&v, Some(&round), &at("aaaa", Some(1)), NOW + 5));
        assert_eq!(second, "bbbb.2");
        assert_eq!(next.from, from, "the start is kept");
        assert_eq!(next.started_at_ms, NOW);
        assert_eq!(next.at_ms, NOW + 5);
        assert_eq!(
            on_next(&v, Some(&next), &at("bbbb", Some(2)), NOW + 6),
            NextStep::Ended,
            "both stops visited and still waiting: the round is over"
        );
        let RoundFile::Write(ended) = NextStep::Ended.round_file(Some(&next), NOW + 6) else {
            panic!("an ended round keeps its file");
        };
        assert_eq!(ended.current, None, "with no current stop");
        assert_eq!(
            on_back(Some(&ended), NOW + 7),
            Some(from),
            "prefix B still goes back to the start"
        );
        let (row_id, fresh) = go(on_next(&v, Some(&ended), &at("bbbb", Some(2)), NOW + 8));
        assert_eq!(row_id, "aaaa.1", "the next press starts a fresh round");
        assert_eq!(fresh.from, at("bbbb", Some(2)));
    }

    #[test]
    fn a_round_idle_ten_minutes_ends() {
        let v = view(vec![
            row("aaaa", 1, "k-a", Class::Needs, Some(1_000)),
            row("bbbb", 2, "k-b", Class::Needs, Some(2_000)),
        ]);
        let (_, round) = go(on_next(&v, None, &at("cccc", None), NOW));
        let later = NOW + ROUND_MAX_IDLE_MS;
        let here = at("aaaa", Some(1));
        let (row_id, fresh) = go(on_next(&v, Some(&round), &here, later));
        assert_eq!(row_id, "aaaa.1", "a fresh round starts at the oldest wait again");
        assert_eq!(fresh.from, here, "from where the keys are now");
        assert_eq!(fresh.started_at_ms, later);
        let (row_id, _) = go(on_next(&v, Some(&round), &here, later - 1));
        assert_eq!(row_id, "bbbb.2", "a moment earlier the round still ran");
        assert_eq!(on_back(Some(&round), later), None, "prefix B has no round to leave");
        assert_eq!(on_back(Some(&round), later - 1), Some(round.from.clone()));
        assert_eq!(
            NextStep::NothingWaits.round_file(Some(&round), later),
            RoundFile::Remove,
            "an idle round's file goes"
        );
    }

    #[test]
    fn an_answer_after_the_round_stood_idle_moves_nothing() {
        let v = view(vec![
            answered("aaaa", 1, "k-a", Class::Working),
            row("bbbb", 2, "k-b", Class::Needs, Some(2_000)),
        ]);
        let round = Round {
            from: at("cccc", None),
            current: Some("k-a".into()),
            visited: vec!["k-a".into()],
            started_at_ms: NOW,
            at_ms: NOW,
        };
        assert_eq!(after_answer("aaaa", &v, &round, NOW + ROUND_MAX_IDLE_MS), None);
        assert!(after_answer("aaaa", &v, &round, NOW + ROUND_MAX_IDLE_MS - 1).is_some());
    }

    #[test]
    fn only_an_answer_in_the_window_with_the_keys_moves_the_round_on() {
        let round = Round {
            from: at("cccc", None),
            current: Some("k-a".into()),
            visited: vec!["k-a".into()],
            started_at_ms: NOW,
            at_ms: NOW,
        };
        let waiting = row("bbbb", 2, "k-b", Class::Needs, Some(2_000));
        let mut unfocused = answered("aaaa", 1, "k-a", Class::Working);
        unfocused.window.focused = false;
        assert_eq!(
            after_answer("aaaa", &view(vec![unfocused, waiting.clone()]), &round, NOW + 1),
            None,
            "the keys were elsewhere: nobody answered here"
        );
        let failed = answered("aaaa", 1, "k-a", Class::Ended);
        assert_eq!(
            after_answer("aaaa", &view(vec![failed, waiting.clone()]), &round, NOW + 1),
            None,
            "a session that failed or ended under its card was not answered"
        );
        for class in [Class::Working, Class::Quiet] {
            let done = answered("aaaa", 1, "k-a", class);
            assert!(
                matches!(
                    after_answer("aaaa", &view(vec![done, waiting.clone()]), &round, NOW + 1),
                    Some(NextStep::Go { .. })
                ),
                "{class:?}"
            );
        }
    }

    #[test]
    fn back_after_the_last_answer_returns_to_from() {
        let from = at("cccc", None);
        let round = Round {
            from: from.clone(),
            current: Some("k-a".into()),
            visited: vec!["k-a".into()],
            started_at_ms: NOW,
            at_ms: NOW,
        };
        let v = view(vec![answered("aaaa", 1, "k-a", Class::Working)]);
        let step = after_answer("aaaa", &v, &round, NOW + 1).expect("its row was answered here");
        assert_eq!(step, NextStep::Ended, "no stop left: the person stays where they are");
        let RoundFile::Write(ended) = step.round_file(Some(&round), NOW + 1) else {
            panic!("the round's start is kept");
        };
        assert_eq!(ended.current, None);
        assert_eq!(ended.at_ms, NOW + 1);
        assert_eq!(on_back(Some(&ended), NOW + 2), Some(from));
        assert_eq!(
            step.round_file(Some(&ended), NOW + 2),
            RoundFile::Keep,
            "an ended round stays as it is"
        );
        assert_eq!(after_answer("aaaa", &v, &ended, NOW + 2), None, "and moves on no more");
    }

    #[test]
    fn a_hand_move_ends_the_round() {
        let v = view(vec![
            row("aaaa", 1, "k-a", Class::Needs, Some(1_000)),
            row("bbbb", 2, "k-b", Class::Needs, Some(2_000)),
        ]);
        let (_, round) = go(on_next(&v, None, &at("cccc", None), NOW));
        for elsewhere in [at("aaaa", Some(9)), at("aaaa", None), at("dddd", Some(1))] {
            let (row_id, fresh) = go(on_next(&v, Some(&round), &elsewhere, NOW + 1));
            assert_eq!(
                row_id, "aaaa.1",
                "{elsewhere:?} is not the current stop: a new round starts"
            );
            assert_eq!(fresh.from, elsewhere);
            assert_eq!(fresh.started_at_ms, NOW + 1);
        }
    }

    #[test]
    fn back_returns_to_from() {
        let v = view(vec![row("aaaa", 1, "k-a", Class::Needs, Some(1_000))]);
        let from = RoundPoint {
            instance: "cccc".into(),
            module: "terminal".into(),
            tab: None,
        };
        let (_, round) = go(on_next(&v, None, &from, NOW));
        assert_eq!(on_back(Some(&round), NOW + 1), Some(from));
    }

    #[test]
    fn back_with_no_round_does_nothing() {
        assert_eq!(on_back(None, NOW), None);
        assert_eq!(RoundOutcome::NoRound.text(), None, "nothing on the status line either");
    }

    #[test]
    fn nothing_waiting_says_so() {
        let quiet = view(vec![row("aaaa", 1, "k-a", Class::Working, None)]);
        assert_eq!(on_next(&quiet, None, &at("aaaa", None), NOW), NextStep::NothingWaits);
        let running = Round {
            from: at("cccc", None),
            current: Some("k-a".into()),
            visited: vec!["k-a".into()],
            started_at_ms: NOW - 10,
            at_ms: NOW - 10,
        };
        let step = on_next(&quiet, Some(&running), &at("aaaa", Some(1)), NOW);
        assert_eq!(step, NextStep::NothingWaits);
        let RoundFile::Write(ended) = step.round_file(Some(&running), NOW) else {
            panic!("a running round ends, its start kept for prefix B");
        };
        assert_eq!(ended.current, None, "a running round ends");
        assert_eq!(
            NextStep::NothingWaits.round_file(None, NOW),
            RoundFile::Keep,
            "with no round nothing is written"
        );
        assert_eq!(
            RoundOutcome::NothingWaits.text().as_deref(),
            Some("no agent waits for you")
        );

        let mut r = rig(two());
        r.controller.handle(HostEvent::Agents {
            win: WIN,
            outcome: RoundOutcome::NothingWaits,
        });
        assert_eq!(
            r.controller.chrome(WIN).toast.map(|toast| toast.text).as_deref(),
            Some(NOTHING_WAITS_TEXT),
            "the status line says it"
        );
    }

    #[test]
    fn the_keys_ask_the_host_and_the_answer_moves_on_once() {
        let mut r = rig(two());
        let ops: Vec<HostOp> = r.run(Action::AgentsNext).ops().cloned().collect();
        assert!(
            ops.iter()
                .any(|op| matches!(op, HostOp::AgentsScan { win, step: RoundKey::Next, .. } if *win == WIN)),
            "{ops:?}"
        );
        let ops: Vec<HostOp> = r.run(Action::AgentsBack).ops().cloned().collect();
        assert!(
            ops.iter().any(|op| matches!(
                op,
                HostOp::AgentsScan {
                    step: RoundKey::Back,
                    ..
                }
            )),
            "{ops:?}"
        );

        let v = view(vec![
            answered("aaaa", 1, "k-a", Class::Working),
            row("bbbb", 2, "k-b", Class::Needs, Some(2_000)),
        ]);
        let round = Round {
            from: at("cccc", None),
            current: Some("k-a".into()),
            visited: vec!["k-a".into()],
            started_at_ms: NOW - 10,
            at_ms: NOW - 10,
        };
        assert_eq!(
            after_answer("bbbb", &v, &round, NOW),
            None,
            "only the owner of the current row moves it"
        );
        let (row_id, next) = go(after_answer("aaaa", &v, &round, NOW).expect("its row stopped waiting"));
        assert_eq!(row_id, "bbbb.2");
        assert_eq!(
            after_answer("aaaa", &v, &next, NOW + 1),
            None,
            "the round it wrote names a row that still waits: no second advance"
        );
        let last = view(vec![answered("aaaa", 1, "k-a", Class::Working)]);
        assert_eq!(after_answer("aaaa", &last, &round, NOW), Some(NextStep::Ended));
    }

    #[test]
    fn back_to_a_start_point_no_longer_in_the_layout_says_it_is_gone() {
        let mut r = rig(two());
        r.controller.handle(HostEvent::Agents {
            win: WIN,
            outcome: RoundOutcome::BackTo(ModuleId::terminal()),
        });
        assert_eq!(
            r.controller.chrome(WIN).toast.map(|toast| toast.text).as_deref(),
            Some(START_GONE_TEXT)
        );
        let mut r = rig(two());
        let batch = r.controller.handle(HostEvent::Agents {
            win: WIN,
            outcome: RoundOutcome::BackTo(ModuleId::editor()),
        });
        assert_eq!(
            r.controller.chrome(WIN).toast,
            None,
            "a start still there just takes the keys"
        );
        let ops: Vec<HostOp> = batch.ops().cloned().collect();
        assert!(
            ops.iter().any(|op| matches!(
                op,
                HostOp::Focus { to: crate::wm::host::FocusTo::Module(m), .. } if *m == ModuleId::editor()
            )),
            "the keys go back to it: {ops:?}"
        );
    }

    #[test]
    fn with_the_layer_off_u_and_B_say_agents_are_off() {
        let mut r = rig(two());
        for action in [Action::AgentsNext, Action::AgentsBack] {
            r.run(action);
            r.controller.handle(HostEvent::Agents {
                win: WIN,
                outcome: RoundOutcome::Off,
            });
            assert_eq!(
                r.controller.chrome(WIN).toast.map(|toast| toast.text).as_deref(),
                Some("agents are off in this window")
            );
        }
    }

    #[test]
    fn a_row_id_names_its_process_and_tab() {
        assert_eq!(stop_of("0123456789abcdef.12"), Some(("0123456789abcdef", 12)));
        assert_eq!(stop_of("0123456789abcdef"), None);
        assert_eq!(stop_of("x.y"), None);
    }
}
