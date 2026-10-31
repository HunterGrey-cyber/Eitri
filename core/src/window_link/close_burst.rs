//! Whether a module window's close was the user's can only be told afterwards. A compositor's close reads the same for
//! one window and for every window of a logout, in every compositor, so nothing that reaches a window says which it was.
//! What differs is what comes next: whatever closes every window closes window 1 too, within moments, and window 1 is
//! the window the user keeps while closing the others.
//!
//! So window 1 holds on to what it keeps for a closed window (its tabs, its place) until [`CLOSE_BURST`] has passed since
//! the last close it heard. If window 1 was asked to close in that time, every window of the burst stays for the next
//! start; a reopened window is a smaller loss than a closed tab's contents. Otherwise those windows are forgotten, as a
//! close is. Closes that follow one another each restart the wait, so a slow logout is one burst however many windows it
//! closes, and two windows closed by hand with window 1 left alone are still both closed.

use std::time::{Duration, Instant};

use super::names::WindowName;

/// How long a close stays open to what comes after it. Well above the time a window takes to say `bye` once it has
/// heard the compositor's close, and short enough that a user rarely closes window 1 inside it by choice.
pub const CLOSE_BURST: Duration = Duration::from_secs(2);

/// The closes window 1 has heard and not yet acted on.
#[derive(Debug, Default)]
pub struct CloseBurst {
    /// The windows closed in this burst, in the order they went.
    closed: Vec<WindowName>,
    /// Window 1 was asked to close in this burst.
    group_asked: bool,
    last: Option<Instant>,
}

impl CloseBurst {
    /// A module window said it was closed. A burst that ended before this was heard has no part in it, and may not have
    /// been settled yet: a timer and a link's end can come due together, in either order. What it leaves to forget is
    /// answered, as [`CloseBurst::settle`] answers it.
    pub fn window_closed(&mut self, name: WindowName, now: Instant) -> Vec<WindowName> {
        let ended = self.settle(now);
        self.last = Some(now);
        if !self.closed.contains(&name) {
            self.closed.push(name);
        }
        ended
    }

    /// Window 1 was asked to close, whatever it answers: its question may stay up after every window has gone. Answers
    /// what a burst that ended before this leaves to forget, as [`CloseBurst::window_closed`] does.
    pub fn group_asked(&mut self, now: Instant) -> Vec<WindowName> {
        let ended = self.settle(now);
        self.last = Some(now);
        self.group_asked = true;
        ended
    }

    fn is_over(&self, now: Instant) -> bool {
        self.last.is_some_and(|last| now.duration_since(last) >= CLOSE_BURST)
    }

    /// How long until [`CloseBurst::settle`] has an answer; `None` with no burst running.
    pub fn due_in(&self, now: Instant) -> Option<Duration> {
        let last = self.last?;
        Some(CLOSE_BURST.saturating_sub(now.duration_since(last)))
    }

    /// The burst's end, once it has come: the windows to forget, none when window 1 was asked to close.
    pub fn settle(&mut self, now: Instant) -> Vec<WindowName> {
        if !self.is_over(now) {
            return Vec::new();
        }
        let burst = std::mem::take(self);
        if burst.group_asked {
            Vec::new()
        } else {
            burst.closed
        }
    }

    /// A window opens under `name`: a new window of that name starts empty, so one still waiting here is forgotten now,
    /// and true says so. A burst window 1 was asked in keeps it, and the new window takes up its tabs.
    pub fn reopened(&mut self, name: &WindowName) -> bool {
        let Some(at) = self.closed.iter().position(|closed| closed == name) else {
            return false;
        };
        self.closed.remove(at);
        !self.group_asked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(n: u8) -> WindowName {
        WindowName::Chat(n)
    }

    fn past(burst: &CloseBurst, now: Instant) -> Instant {
        now + burst.due_in(now).expect("a burst is running")
    }

    #[test]
    fn a_close_alone_takes_effect_once_the_burst_has_passed() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.window_closed(chat(1), now);

        assert_eq!(burst.settle(now), Vec::<WindowName>::new(), "too early to tell");
        assert_eq!(burst.settle(now + CLOSE_BURST / 2), Vec::<WindowName>::new());
        assert_eq!(burst.settle(past(&burst, now)), vec![chat(1)]);
        assert_eq!(burst.due_in(now), None, "the burst is over");
    }

    #[test]
    fn a_close_window_1_follows_inside_the_burst_is_kept() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.window_closed(chat(1), now);
        burst.group_asked(now + CLOSE_BURST - Duration::from_millis(1));

        assert_eq!(burst.settle(past(&burst, now + CLOSE_BURST)), Vec::<WindowName>::new());
    }

    #[test]
    fn a_close_that_follows_window_1_being_asked_is_kept() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.group_asked(now);
        burst.window_closed(chat(1), now + Duration::from_millis(150));
        burst.window_closed(chat(2), now + Duration::from_millis(150));

        assert_eq!(burst.settle(now + CLOSE_BURST * 2), Vec::<WindowName>::new());
    }

    #[test]
    fn each_close_restarts_the_wait_so_a_slow_logout_is_one_burst() {
        let now = Instant::now();
        let step = CLOSE_BURST - Duration::from_millis(100);
        let mut burst = CloseBurst::default();
        burst.window_closed(chat(1), now);
        burst.window_closed(chat(2), now + step);
        burst.window_closed(chat(3), now + step * 2);
        assert_eq!(
            burst.settle(now + step * 2),
            Vec::<WindowName>::new(),
            "the last close has only just come"
        );
        burst.group_asked(now + step * 3);

        assert_eq!(burst.settle(now + step * 3 + CLOSE_BURST), Vec::<WindowName>::new());
    }

    #[test]
    fn windows_closed_by_hand_with_window_1_left_alone_are_all_closed() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.window_closed(chat(1), now);
        burst.window_closed(chat(2), now + Duration::from_millis(300));

        assert_eq!(
            burst.settle(now + Duration::from_millis(300) + CLOSE_BURST),
            vec![chat(1), chat(2)]
        );
    }

    #[test]
    fn a_close_after_window_1_answered_stands_on_its_own() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.group_asked(now);
        assert_eq!(burst.settle(now + CLOSE_BURST), Vec::<WindowName>::new());
        // The timer of an answered question may not have run: what it left is no part of a later close.
        burst.group_asked(now + CLOSE_BURST);
        burst.window_closed(chat(1), now + CLOSE_BURST * 2);

        assert_eq!(burst.settle(now + CLOSE_BURST * 3), vec![chat(1)]);
    }

    /// A timer and a link's end can come due together, the link first: the burst that ended is settled, not lost.
    #[test]
    fn a_close_heard_before_the_ended_bursts_timer_ran_leaves_that_burst_settled() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.window_closed(chat(1), now);

        assert_eq!(burst.window_closed(chat(2), now + CLOSE_BURST), vec![chat(1)]);
        assert_eq!(burst.settle(now + CLOSE_BURST * 2), vec![chat(2)]);

        burst.window_closed(chat(3), now + CLOSE_BURST * 3);
        assert_eq!(
            burst.group_asked(now + CLOSE_BURST * 4),
            vec![chat(3)],
            "window 1 was asked too late for it"
        );
    }

    #[test]
    fn a_window_reopened_in_the_burst_is_forgotten_at_once_and_only_once() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.window_closed(chat(1), now);
        burst.window_closed(chat(2), now);

        assert!(
            burst.reopened(&chat(1)),
            "the new window must not take up the closed window's tabs"
        );
        assert!(!burst.reopened(&chat(1)));
        assert!(!burst.reopened(&chat(3)));
        assert_eq!(burst.settle(past(&burst, now)), vec![chat(2)]);
    }

    #[test]
    fn a_window_reopened_while_the_group_was_asked_keeps_what_it_held() {
        let now = Instant::now();
        let mut burst = CloseBurst::default();
        burst.group_asked(now);
        burst.window_closed(chat(1), now);

        assert!(!burst.reopened(&chat(1)));
    }
}
