//! Callback pacing and the parent cover's state, with all times supplied by the caller.

use std::time::{Duration, Instant};

pub(crate) const STALE_AFTER: Duration = Duration::from_millis(1000);
pub(crate) const DEFAULT_PERIOD: Duration = Duration::from_nanos(16_666_667);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hole {
    Closed,
    Open,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    Render,
    /// Render the first child buffer and uncover it in the same parent commit.
    OpenTransaction,
    NullAttach,
}

pub(crate) struct Pacer {
    mapped: bool,
    hole: Hole,
    wanted: bool,
    opening_deferred: bool,
    pending: Option<(u64, Instant)>,
    suspended: bool,
    animating: bool,
    last_done: Option<Instant>,
    intervals: [Duration; 8],
    interval_count: usize,
    next_interval: usize,
    presented_period: Option<Duration>,
}

impl Pacer {
    pub(crate) fn new() -> Self {
        Self {
            mapped: false,
            hole: Hole::Closed,
            wanted: false,
            opening_deferred: false,
            pending: None,
            suspended: false,
            animating: false,
            last_done: None,
            intervals: [Duration::ZERO; 8],
            interval_count: 0,
            next_interval: 0,
            presented_period: None,
        }
    }

    pub(crate) fn want(&mut self) -> Action {
        self.wanted = true;
        self.render_if_wanted()
    }

    pub(crate) fn committed(&mut self, seq: u64, at: Instant) {
        if self.mapped && self.hole == Hole::Open {
            self.wanted = false;
            self.pending = Some((seq, at));
        }
    }

    pub(crate) fn opened(&mut self, seq: u64, at: Instant) {
        if !self.mapped {
            return;
        }
        // A covered child may receive no callback, so the commit itself opens the hole.
        self.hole = Hole::Open;
        self.opening_deferred = false;
        self.committed(seq, at);
    }

    pub(crate) fn open_deferred(&mut self) {
        if self.mapped && self.hole == Hole::Closed {
            self.opening_deferred = true;
        }
    }

    pub(crate) fn buffer_ready(&mut self) -> Action {
        if self.mapped && self.hole == Hole::Closed && self.opening_deferred {
            self.opening_deferred = false;
            Action::OpenTransaction
        } else {
            Action::None
        }
    }

    pub(crate) fn frame_done(&mut self, seq: u64, at: Instant) -> Action {
        if !self.pending.is_some_and(|(pending_seq, _)| pending_seq == seq) {
            return Action::None;
        }
        self.pending = None;
        if self.animating {
            if let Some(interval) = self.last_done.and_then(|previous| at.checked_duration_since(previous)) {
                if !interval.is_zero() {
                    self.intervals[self.next_interval] = interval;
                    self.next_interval = (self.next_interval + 1) % self.intervals.len();
                    self.interval_count = (self.interval_count + 1).min(self.intervals.len());
                }
            }
            self.last_done = Some(at);
        }
        self.render_if_wanted()
    }

    pub(crate) fn mapped(&mut self) -> Action {
        if self.mapped {
            return Action::None;
        }
        self.mapped = true;
        // Opening always renders, including wants retained while the pane was unmapped.
        Action::OpenTransaction
    }

    pub(crate) fn unmapped(&mut self) -> Action {
        self.mapped = false;
        self.hole = Hole::Closed;
        self.pending = None;
        self.opening_deferred = false;
        self.last_done = None;
        Action::NullAttach
    }

    pub(crate) fn set_suspended(&mut self, on: bool) {
        if self.suspended != on {
            self.last_done = None;
        }
        self.suspended = on;
    }

    pub(crate) fn stale_check(&mut self, now: Instant) -> Action {
        if self.stale_check_due().is_some_and(|due| now >= due) {
            self.pending = None;
            self.last_done = None;
            self.render_if_wanted()
        } else {
            Action::None
        }
    }

    pub(crate) fn stale_check_due(&self) -> Option<Instant> {
        if !self.mapped || self.suspended {
            return None;
        }
        self.pending.and_then(|(_, at)| at.checked_add(STALE_AFTER))
    }

    pub(crate) fn presented(&mut self, refresh_ns: u32) {
        // A zero refresh carries no timing information and cannot replace a known period.
        if refresh_ns != 0 {
            self.presented_period = Some(Duration::from_nanos(u64::from(refresh_ns)));
        }
    }

    pub(crate) fn animating(&mut self, on: bool) {
        if self.animating != on {
            // An idle gap between animations is not a display interval.
            self.last_done = None;
        }
        self.animating = on;
    }

    pub(crate) fn hole(&self) -> Hole {
        self.hole
    }

    pub(crate) fn period(&self) -> Duration {
        if let Some(period) = self.presented_period {
            return period;
        }
        if self.interval_count == 0 {
            return DEFAULT_PERIOD;
        }
        let mut intervals = self.intervals;
        let samples = &mut intervals[..self.interval_count];
        samples.sort_unstable();
        let middle = samples.len() / 2;
        if samples.len() % 2 == 0 {
            samples[middle - 1] + (samples[middle] - samples[middle - 1]) / 2
        } else {
            samples[middle]
        }
    }

    pub(crate) fn callback_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn render_if_wanted(&mut self) -> Action {
        if self.wanted && self.mapped && self.hole == Hole::Open && self.pending.is_none() {
            self.wanted = false;
            Action::Render
        } else {
            Action::None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Action, Hole, Pacer, DEFAULT_PERIOD, STALE_AFTER};

    fn open_pacer(t0: Instant) -> Pacer {
        let mut pacer = Pacer::new();
        assert_eq!(pacer.mapped(), Action::OpenTransaction);
        pacer.opened(0, t0);
        assert_eq!(pacer.frame_done(0, t0), Action::None);
        pacer
    }

    #[test]
    fn an_idle_want_renders_at_once() {
        let mut pacer = open_pacer(Instant::now());
        assert_eq!(pacer.want(), Action::Render);
    }

    #[test]
    fn a_want_while_a_callback_is_pending_waits_for_done() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        assert_eq!(pacer.want(), Action::Render);
        pacer.committed(1, t0);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.frame_done(1, t0 + DEFAULT_PERIOD), Action::Render);
        pacer.committed(2, t0 + DEFAULT_PERIOD);
        assert_eq!(pacer.frame_done(2, t0 + 2 * DEFAULT_PERIOD), Action::None);
    }

    #[test]
    fn done_with_nothing_wanted_renders_nothing() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        pacer.committed(1, t0);
        assert_eq!(pacer.frame_done(1, t0 + DEFAULT_PERIOD), Action::None);
        assert!(!pacer.callback_pending());
    }

    #[test]
    fn an_unmapped_pane_never_renders_and_keeps_its_want_for_the_map() {
        let mut pacer = Pacer::new();
        assert_eq!(pacer.hole(), Hole::Closed);
        assert!(!pacer.callback_pending());
        assert_eq!(pacer.unmapped(), Action::NullAttach);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.mapped(), Action::OpenTransaction);
    }

    #[test]
    fn the_hole_opens_with_no_frame_done_delivered() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new();
        assert_eq!(pacer.mapped(), Action::OpenTransaction);
        assert_eq!(pacer.hole(), Hole::Closed);
        pacer.opened(3, t0);
        assert_eq!(pacer.hole(), Hole::Open);
        assert!(pacer.callback_pending());
        assert_eq!(pacer.unmapped(), Action::NullAttach);
        assert_eq!(pacer.hole(), Hole::Closed);
        assert!(!pacer.callback_pending());
    }

    #[test]
    fn an_opening_without_a_buffer_waits_for_one() {
        let mut pacer = Pacer::new();
        assert_eq!(pacer.mapped(), Action::OpenTransaction);
        pacer.open_deferred();
        assert_eq!(pacer.hole(), Hole::Closed);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.buffer_ready(), Action::OpenTransaction);
        assert_eq!(pacer.buffer_ready(), Action::None);
    }

    #[test]
    fn a_want_kept_while_unmapped_is_rendered_in_the_opening_transaction() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        pacer.committed(1, t0);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.unmapped(), Action::NullAttach);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.mapped(), Action::OpenTransaction);
        assert_eq!(pacer.want(), Action::None);
        pacer.opened(2, t0 + DEFAULT_PERIOD);
        assert_eq!(pacer.hole(), Hole::Open);
        assert!(pacer.callback_pending());
        // The opening's render consumed the retained wants; no extra render follows its callback.
        assert_eq!(pacer.frame_done(2, t0 + 2 * DEFAULT_PERIOD), Action::None);
    }

    #[test]
    fn a_stale_callback_is_dropped_after_a_second_when_visible() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        pacer.committed(1, t0);
        assert_eq!(pacer.want(), Action::None);
        assert_eq!(pacer.stale_check_due(), Some(t0 + STALE_AFTER));
        assert_eq!(pacer.stale_check(t0 + Duration::from_millis(999)), Action::None);
        assert!(pacer.callback_pending());
        assert_eq!(pacer.stale_check(t0 + Duration::from_millis(1000)), Action::Render);
        assert!(!pacer.callback_pending());
        assert_eq!(pacer.stale_check_due(), None);
        assert_eq!(pacer.stale_check(t0 + 2 * STALE_AFTER), Action::None);
    }

    #[test]
    fn a_stale_callback_is_kept_while_suspended() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        pacer.committed(1, t0);
        assert_eq!(pacer.want(), Action::None);
        pacer.set_suspended(true);
        assert_eq!(pacer.stale_check_due(), None);
        assert_eq!(pacer.stale_check(t0 + 2 * STALE_AFTER), Action::None);
        assert!(pacer.callback_pending());
        pacer.set_suspended(false);
        assert_eq!(pacer.stale_check_due(), Some(t0 + STALE_AFTER));
        assert_eq!(pacer.stale_check(t0 + 2 * STALE_AFTER), Action::Render);
    }

    #[test]
    fn the_period_prefers_presented_then_measured_then_default() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        assert_eq!(pacer.period(), DEFAULT_PERIOD);
        pacer.presented(0);
        assert_eq!(pacer.period(), DEFAULT_PERIOD);
        pacer.animating(true);
        let measured = Duration::from_micros(6060);
        for seq in 1..=9 {
            pacer.committed(seq, t0 + measured * (seq as u32 - 1));
            pacer.frame_done(seq, t0 + measured * seq as u32);
        }
        assert_eq!(pacer.period(), measured);
        pacer.presented(0);
        assert_eq!(pacer.period(), measured);
        pacer.presented(6_944_444);
        assert_eq!(pacer.period(), Duration::from_nanos(6_944_444));
        pacer.presented(0);
        assert_eq!(pacer.period(), Duration::from_nanos(6_944_444));
    }

    #[test]
    fn done_intervals_outside_animation_are_not_measured() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        for seq in 1..=9 {
            pacer.committed(seq, t0);
            pacer.frame_done(seq, t0 + Duration::from_millis(seq * 7));
        }
        assert_eq!(pacer.period(), DEFAULT_PERIOD);
    }

    #[test]
    fn a_late_callback_cannot_complete_a_newer_commit() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        pacer.committed(1, t0);
        pacer.want();
        assert_eq!(pacer.stale_check(t0 + STALE_AFTER), Action::Render);
        pacer.committed(2, t0 + STALE_AFTER);
        pacer.want();
        assert_eq!(pacer.frame_done(1, t0 + STALE_AFTER), Action::None);
        assert!(pacer.callback_pending());
        assert_eq!(pacer.stale_check_due(), Some(t0 + 2 * STALE_AFTER));
        assert_eq!(pacer.frame_done(2, t0 + STALE_AFTER + DEFAULT_PERIOD), Action::Render);
        assert_eq!(pacer.frame_done(2, t0 + STALE_AFTER + DEFAULT_PERIOD), Action::None);
    }

    #[test]
    fn a_stale_callback_with_nothing_wanted_does_not_render() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        pacer.committed(1, t0);
        assert_eq!(pacer.stale_check(t0 + STALE_AFTER), Action::None);
        assert!(!pacer.callback_pending());
        assert_eq!(pacer.stale_check_due(), None);
        assert_eq!(pacer.want(), Action::Render);
    }

    #[test]
    fn unmapping_cancels_a_deferred_opening_and_late_events() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new();
        pacer.mapped();
        pacer.open_deferred();
        pacer.unmapped();
        assert_eq!(pacer.buffer_ready(), Action::None);
        pacer.opened(1, t0);
        pacer.committed(2, t0);
        assert_eq!(pacer.frame_done(1, t0), Action::None);
        assert_eq!(pacer.hole(), Hole::Closed);
        assert!(!pacer.callback_pending());
        assert_eq!(pacer.stale_check_due(), None);
        assert_eq!(pacer.mapped(), Action::OpenTransaction);
    }

    #[test]
    fn an_opening_can_be_deferred_again_until_a_buffer_is_available() {
        let mut pacer = Pacer::new();
        pacer.mapped();
        assert_eq!(pacer.buffer_ready(), Action::None);
        for _ in 0..2 {
            pacer.open_deferred();
            assert_eq!(pacer.buffer_ready(), Action::OpenTransaction);
            assert_eq!(pacer.hole(), Hole::Closed);
            assert!(!pacer.callback_pending());
        }
        pacer.opened(1, Instant::now());
        assert_eq!(pacer.buffer_ready(), Action::None);
        assert_eq!(pacer.mapped(), Action::None);
    }

    #[test]
    fn the_measured_period_is_the_median_of_only_the_last_eight_intervals() {
        let mut at = Instant::now();
        let mut pacer = open_pacer(at);
        pacer.animating(true);
        pacer.committed(1, at);
        pacer.frame_done(1, at);
        for (index, millis) in [100, 8, 1, 7, 2, 6, 3, 5, 4].into_iter().enumerate() {
            pacer.committed(index as u64 + 2, at);
            at += Duration::from_millis(millis);
            pacer.frame_done(index as u64 + 2, at);
        }
        assert_eq!(pacer.period(), Duration::from_micros(4500));
    }

    #[test]
    fn an_idle_gap_between_animations_is_not_a_display_interval() {
        let t0 = Instant::now();
        let mut pacer = open_pacer(t0);
        pacer.animating(true);
        pacer.committed(1, t0);
        pacer.frame_done(1, t0);
        pacer.animating(false);
        pacer.committed(2, t0);
        pacer.frame_done(2, t0 + Duration::from_secs(1));
        pacer.animating(true);
        pacer.committed(3, t0 + Duration::from_secs(2));
        pacer.frame_done(3, t0 + Duration::from_secs(2));
        assert_eq!(pacer.period(), DEFAULT_PERIOD);
        pacer.animating(true);
        pacer.committed(4, t0 + Duration::from_secs(2));
        pacer.frame_done(4, t0 + Duration::from_secs(2) + Duration::from_millis(6));
        assert_eq!(pacer.period(), Duration::from_millis(6));
    }
}
