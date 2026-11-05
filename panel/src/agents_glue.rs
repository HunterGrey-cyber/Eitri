//! A window's agent rows, from the panel's tick to the process's rows file.
//!
//! Each tick recomputes every tab's row from the facts the panel already draws and hands the rows on only when
//! they differ from the last ones handed on: a streamed token changes no row, so a busy turn costs an offer per
//! state change at most. Handing rows on only queues them (the publisher writes on its own thread), so this runs
//! on the window's thread. A row that starts moving goes at once, ahead of the coalescing, so its `moving` is on
//! disk before the tab leaves: a freeze outside the tick (a hidden tab's, or one its page answered) has its row
//! offered as moving before the window hears of the freeze, since hearing it starts the hand-over. A tab that
//! leaves for another window is marked `moved` before the offer that no longer holds it.
//!
//! A row of this window that stops waiting (its card, question or trust prompt answered here) may be the stop the
//! shared round is at, and only this process can move the round on from it. The glue cannot tell from its rows
//! alone (the round is a file, read off this thread), so it hands its rows to the round hook once, at the offer
//! where a row left class `needs`; the hook reads the round on a worker and takes the next step when it should.

use eitri_core::agents::{Class, GoneWhy, Publisher, Row};
use eitri_core::tab_set::{RowContext, TabSet};
use eitri_core::tabs::TabId;

/// Where the rows go: the process's [`Publisher`], or a test's recorder. Every call only queues. `Send`, so a closing
/// window can let its publisher (whose drop waits for the last write) go on a thread of its own.
pub trait RowSink: Send {
    /// The rows, published once they have been the last offered for the publisher's coalescing window.
    fn offer(&self, rows: Vec<Row>);
    /// The rows, published before any later offer.
    fn offer_now(&self, rows: Vec<Row>);
    /// Why the row with `key` is about to leave.
    fn departing(&self, key: String, why: GoneWhy);
}

impl RowSink for Publisher {
    fn offer(&self, rows: Vec<Row>) {
        Publisher::offer(self, rows);
    }

    fn offer_now(&self, rows: Vec<Row>) {
        Publisher::offer_now(self, rows);
    }

    fn departing(&self, key: String, why: GoneWhy) {
        Publisher::departing(self, key, why);
    }
}

/// How a set of rows is handed on, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offer {
    /// Nothing changed.
    None,
    /// Through the publisher's coalescing.
    Coalesced,
    /// At once: a row started moving.
    Now,
}

/// The offer `rows` need after `last`: none when equal, at once when a row is moving that was not, else
/// coalesced.
fn offer_for(last: Option<&[Row]>, rows: &[Row]) -> Offer {
    if last == Some(rows) {
        return Offer::None;
    }
    let was_moving = |key: &str| last.is_some_and(|last| last.iter().any(|r| r.key == key && r.moving));
    if rows.iter().any(|r| r.moving && !was_moving(&r.key)) {
        Offer::Now
    } else {
        Offer::Coalesced
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Told this window's rows when one of them stopped waiting. It only queues: the round is read and moved on
/// elsewhere. `Send`, as the glue is.
pub type RoundHook = Box<dyn Fn(Vec<Row>) + Send>;

/// Whether a row of `last` that waited for the person waits no more in `rows`, still held here.
fn left_needs(last: Option<&[Row]>, rows: &[Row]) -> bool {
    let Some(last) = last else {
        return false;
    };
    rows.iter()
        .any(|row| row.class != Class::Needs && last.iter().any(|was| was.key == row.key && was.class == Class::Needs))
}

/// One window's rows: what identifies them, where they go and what was last handed on.
pub struct AgentsGlue {
    context: RowContext,
    sink: Box<dyn RowSink>,
    last: Option<Vec<Row>>,
    clock: fn() -> u64,
    round: Option<RoundHook>,
}

impl AgentsGlue {
    /// Rows named by `context` (this process's instance, its project and window) handed to `sink`.
    pub fn new(context: RowContext, sink: Box<dyn RowSink>) -> AgentsGlue {
        AgentsGlue::with_clock(context, sink, wall_clock_ms)
    }

    /// [`AgentsGlue::new`] with the clock rows are stamped by.
    pub fn with_clock(context: RowContext, sink: Box<dyn RowSink>, clock: fn() -> u64) -> AgentsGlue {
        AgentsGlue {
            context,
            sink,
            last: None,
            clock,
            round: None,
        }
    }

    /// Where the rows go when one of them stopped waiting, for the shared round to move on.
    pub fn set_round_hook(&mut self, hook: RoundHook) {
        self.round = Some(hook);
    }

    /// Whether the window has the keyboard, for every row's `window.focused`.
    pub fn set_focused(&mut self, focused: bool) {
        self.context.window.focused = focused;
    }

    /// Recomputes the rows of `tabs` (`shown` says which tab is on screen in a view) and hands them on if they
    /// changed since the last offer.
    pub fn tick(&mut self, tabs: &mut TabSet, shown: &dyn Fn(TabId) -> bool) {
        let rows = tabs.agent_rows(&self.context, shown, (self.clock)());
        self.take(rows);
    }

    /// Hands `rows` on if they differ from the last ones, and to the round hook when one stopped waiting.
    fn take(&mut self, rows: Vec<Row>) {
        match offer_for(self.last.as_deref(), &rows) {
            Offer::None => return,
            Offer::Coalesced => self.sink.offer(rows.clone()),
            Offer::Now => self.sink.offer_now(rows.clone()),
        }
        if let Some(round) = self.round.as_ref().filter(|_| left_needs(self.last.as_deref(), &rows)) {
            round(rows.clone());
        }
        self.last = Some(rows);
    }

    /// The rows last handed on, with every tab of `tabs` that froze for a move since marked moving, handed on at
    /// once when that changed one: a freeze made outside the tick is on its way to disk before the move goes on.
    /// Nothing is recomputed (the next tick does that), so this is cheap enough to run after every panel step.
    pub fn offer_moving(&mut self, tabs: &TabSet) {
        let Some(last) = self.last.as_mut() else {
            return;
        };
        let mut changed = false;
        for tab in tabs.tabs().iter().filter(|tab| tabs.is_moving(tab.id)) {
            let key = tab.row_key();
            if let Some(row) = last.iter_mut().find(|row| row.key == key.as_str() && !row.moving) {
                row.moving = true;
                changed = true;
            }
        }
        if changed {
            self.sink.offer_now(last.clone());
        }
    }

    /// The row keys `tabs` holds now, read before a step that may hand tabs to another window.
    pub fn keys(tabs: &TabSet) -> Vec<String> {
        tabs.tabs()
            .iter()
            .map(|tab| tab.row_key().as_str().to_owned())
            .collect()
    }

    /// After a hand-over: every key of `before` that `tabs` no longer holds left for another window, and is
    /// marked so before the next offer drops its row.
    pub fn handed_over(&self, before: &[String], tabs: &TabSet) {
        let now = AgentsGlue::keys(tabs);
        for key in before.iter().filter(|key| !now.contains(key)) {
            self.sink.departing(key.clone(), GoneWhy::Moved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::agent_backend::{AgentId, BackendKind};
    use eitri_core::agent_bridge::SessionModeChoice;
    use eitri_core::agents::{ProjectRef, WindowRef};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, PartialEq)]
    enum Seen {
        Offer(Vec<String>),
        /// Each row's key, and whether it is moving.
        OfferNow(Vec<(String, bool)>),
        Departing(String, GoneWhy),
    }

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Seen>>>);

    impl Recorder {
        fn seen(&self) -> std::sync::MutexGuard<'_, Vec<Seen>> {
            self.0.lock().unwrap()
        }
    }

    fn keys_of(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.key.clone()).collect()
    }

    impl RowSink for Recorder {
        fn offer(&self, rows: Vec<Row>) {
            self.seen().push(Seen::Offer(keys_of(&rows)));
        }
        fn offer_now(&self, rows: Vec<Row>) {
            self.seen()
                .push(Seen::OfferNow(rows.iter().map(|r| (r.key.clone(), r.moving)).collect()));
        }
        fn departing(&self, key: String, why: GoneWhy) {
            self.seen().push(Seen::Departing(key, why));
        }
    }

    fn context() -> RowContext {
        RowContext {
            instance: "0123456789abcdef".into(),
            project: ProjectRef {
                key: "fedcba9876543210".into(),
                name: "eitri".into(),
            },
            window: WindowRef {
                instance: "0123456789abcdef".into(),
                name: "1".into(),
                focused: false,
            },
        }
    }

    thread_local! {
        static NOW: std::cell::Cell<u64> = const { std::cell::Cell::new(1_000) };
    }

    fn test_clock() -> u64 {
        NOW.with(std::cell::Cell::get)
    }

    fn glue() -> (AgentsGlue, Recorder) {
        let recorder = Recorder::default();
        (
            AgentsGlue::with_clock(context(), Box::new(recorder.clone()), test_clock),
            recorder,
        )
    }

    fn tabs() -> TabSet {
        TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
    }

    #[test]
    fn rows_are_offered_only_when_they_change() {
        let (mut glue, seen) = glue();
        let mut tabs = tabs();
        glue.tick(&mut tabs, &|_| false);
        NOW.with(|now| now.set(5_000));
        glue.tick(&mut tabs, &|_| false);
        let offers = seen.seen().clone();
        assert_eq!(
            offers.len(),
            1,
            "the same facts a moment later are the same rows: {offers:?}"
        );
        glue.tick(&mut tabs, &|_| true);
        assert_eq!(seen.seen().len(), 2, "a tab coming on screen changes its row");
        glue.set_focused(true);
        glue.tick(&mut tabs, &|_| true);
        assert_eq!(seen.seen().len(), 3, "the window taking the keys changes every row");
        assert!(seen.seen().iter().all(|s| matches!(s, Seen::Offer(_))));
    }

    #[test]
    fn a_hand_over_marks_the_key_departing_moved() {
        let (mut glue, seen) = glue();
        let mut tabs = tabs();
        let staying = tabs.active_tab().row_key().as_str().to_owned();
        let second = tabs.open(AgentId::Claude);
        glue.tick(&mut tabs, &|_| false);
        let leaving = tabs.get(second).unwrap().row_key().as_str().to_owned();
        assert_ne!(leaving, staying);

        let before = AgentsGlue::keys(&tabs);
        assert!(tabs.hand_over(second).is_some());
        glue.handed_over(&before, &tabs);
        glue.tick(&mut tabs, &|_| false);

        assert_eq!(
            seen.seen()[1..],
            [Seen::Departing(leaving, GoneWhy::Moved), Seen::Offer(vec![staying])],
            "the mark comes before the offer that drops the row"
        );

        // A step that hands nothing over marks nothing.
        let before = AgentsGlue::keys(&tabs);
        glue.handed_over(&before, &tabs);
        assert_eq!(seen.seen().len(), 3);
    }

    #[test]
    fn a_tab_frozen_between_ticks_is_offered_as_moving_at_once() {
        let (mut glue, seen) = glue();
        let mut tabs = tabs();
        let staying = tabs.active_tab().row_key().as_str().to_owned();
        let second = tabs.open(AgentId::Claude);
        let leaving = tabs.get(second).unwrap().row_key().as_str().to_owned();
        glue.offer_moving(&tabs);
        assert!(
            seen.seen().is_empty(),
            "nothing was handed on yet, so there is nothing to mark"
        );
        glue.tick(&mut tabs, &|_| false);

        glue.offer_moving(&tabs);
        assert_eq!(seen.seen().len(), 1, "no tab is moving: nothing more goes");
        assert!(tabs.freeze_for_move(second).is_ok());
        glue.offer_moving(&tabs);
        assert_eq!(
            seen.seen()[1..],
            [Seen::OfferNow(vec![(staying.clone(), false), (leaving.clone(), true)])]
        );
        glue.offer_moving(&tabs);
        assert_eq!(seen.seen().len(), 2, "a row already offered as moving goes once");
        glue.tick(&mut tabs, &|_| false);
        let at_once = seen.seen().iter().filter(|s| matches!(s, Seen::OfferNow(_))).count();
        assert_eq!(
            at_once, 1,
            "the next tick does not hand the moving row on at once again"
        );
    }

    #[test]
    fn answering_the_current_card_advances_once() {
        let (mut glue, _) = glue();
        let asked = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        {
            let asked = asked.clone();
            glue.set_round_hook(Box::new(move |rows| asked.lock().unwrap().push(keys_of(&rows))));
        }
        let with = |class: Class, focused: bool| {
            let mut card = row("k-card", false);
            card.class = class;
            card.window.focused = focused;
            vec![card, row("k-other", false)]
        };
        glue.take(with(Class::Needs, false));
        glue.take(with(Class::Needs, true));
        assert!(asked.lock().unwrap().is_empty(), "nothing stopped waiting yet");
        glue.take(with(Class::Working, true));
        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "the answer is the moment the round may move on"
        );
        glue.take(with(Class::Working, true));
        glue.take(with(Class::Working, false));
        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "two more ticks after the row left needs: still one advance"
        );
        assert_eq!(
            asked.lock().unwrap()[0],
            ["k-card", "k-other"],
            "the hook gets this window's rows"
        );
        glue.take(vec![row("k-other", false)]);
        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "a row that left the window did not stop waiting here"
        );
    }

    fn row(key: &str, moving: bool) -> Row {
        Row {
            key: key.into(),
            moving,
            ..Row::default()
        }
    }

    #[test]
    fn a_row_that_starts_moving_is_offered_at_once() {
        let calm = [row("k-a", false), row("k-b", false)];
        let moving = [row("k-a", true), row("k-b", false)];
        assert_eq!(offer_for(None, &calm), Offer::Coalesced);
        assert_eq!(offer_for(Some(&calm), &calm), Offer::None);
        assert_eq!(offer_for(Some(&calm), &moving), Offer::Now);
        assert_eq!(offer_for(None, &moving), Offer::Now);
        let moving_and_renamed = [row("k-a", true), row("k-c", false)];
        assert_eq!(
            offer_for(Some(&moving), &moving_and_renamed),
            Offer::Coalesced,
            "a row already moving goes through the coalescing"
        );
    }
}
