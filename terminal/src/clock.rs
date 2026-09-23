//! When to render: a leading edge, then at most one render per frame interval.
//!
//! The first change after an idle period renders at once, so a keystroke's echo waits for nothing.
//! Further changes inside the interval fold into one render at its end. No change means no render
//! at all -- the idle cost is zero, which is the P11 lesson (an unconditional redraw once cost this
//! project 60x idle CPU). Pure, so the rule is tested without a thread or a clock.

use std::time::{Duration, Instant};

/// One frame at 60 Hz.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone)]
pub struct RenderClock {
    interval: Duration,
    last: Option<Instant>,
    due: Option<Instant>,
}

impl RenderClock {
    pub fn new(interval: Duration) -> Self {
        RenderClock {
            interval,
            last: None,
            due: None,
        }
    }

    /// Something visible changed at `now`. `true`: render now. `false`: a render is scheduled
    /// (see [`RenderClock::deadline`]).
    pub fn changed(&mut self, now: Instant) -> bool {
        if self.due.is_some() {
            return false;
        }
        match self.last {
            Some(last) if now < last + self.interval => {
                self.due = Some(last + self.interval);
                false
            }
            _ => true,
        }
    }

    /// A render happened at `now`.
    pub fn rendered(&mut self, now: Instant) {
        self.last = Some(now);
        self.due = None;
    }

    /// When a scheduled render is due, if one is.
    pub fn deadline(&self) -> Option<Instant> {
        self.due
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.due.is_some_and(|due| now >= due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_first_change_after_idle_renders_at_once() {
        let t0 = Instant::now();
        let mut clock = RenderClock::new(FRAME_INTERVAL);
        assert!(clock.changed(t0), "nothing rendered yet: render now");
        clock.rendered(t0);
        assert!(clock.changed(t0 + 40 * MS), "idle for longer than a frame: render now");
    }

    #[test]
    fn changes_inside_a_frame_fold_into_one_render_at_its_end() {
        let t0 = Instant::now();
        let mut clock = RenderClock::new(FRAME_INTERVAL);
        assert!(clock.changed(t0));
        clock.rendered(t0);
        assert!(!clock.changed(t0 + 2 * MS));
        assert!(!clock.changed(t0 + 9 * MS), "already scheduled");
        assert_eq!(clock.deadline(), Some(t0 + FRAME_INTERVAL));
        assert!(!clock.is_due(t0 + 15 * MS));
        assert!(clock.is_due(t0 + 16 * MS));
        clock.rendered(t0 + 16 * MS);
        assert_eq!(clock.deadline(), None, "nothing more changed: nothing more to do");
    }

    #[test]
    fn no_change_schedules_nothing() {
        let mut clock = RenderClock::new(FRAME_INTERVAL);
        clock.rendered(Instant::now());
        assert_eq!(clock.deadline(), None);
        assert!(!clock.is_due(Instant::now() + Duration::from_secs(10)));
    }
}
