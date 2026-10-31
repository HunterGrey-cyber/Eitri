//! The termination watchdog's bounds and decision, independent of signal delivery.

use std::time::Duration;

/// How long the window has to answer the first signal before the watchdog ends the process. A dispatch on a
/// responsive UI thread takes milliseconds; this is far above that and still short for someone waiting.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(5);

/// How long, from the first signal, a close the window has started may run before the watchdog ends the process.
/// It is longer than every bound that close may wait out, added as if they ran one after another, plus 5 s:
/// - the agent panel's `SESSION_CLOSE_BACKSTOP` (18 s), the session teardown whose drop is what kills a sidecar
///   that does not go;
/// - its `SAVED_TABS_BACKSTOP` (5 s; `CONNECT_CLOSE_BACKSTOP` and `HANDOFF_CLOSE_BACKSTOP`, 3 s each, are shorter);
/// - nvim's `NVIM_QUIT_PATIENCE` (5 s), after which `NeovideEditorPane::shutdown` kills it;
/// - a plugin task group's stop, `crate::plugin::tasks::GRACE` (2 s);
/// - the plugin pages' hold in `plugin_panels::shutdown` (that grace and 1 s more, 3 s).
///
/// That is 33 s; 38 s with the margin, rounded up. A shorter bound would end a healthy close before the session
/// teardown escalates, and leave the very `claude` and sidecar processes that teardown exists to end. Raising any
/// of those bounds means raising this one.
pub const CLOSE_WITHIN: Duration = Duration::from_secs(40);

/// How far ahead of the watchdog's `_exit` the SIGKILL of a window told to close lands: the `_exit` takes the thread
/// that would send it.
pub const KILL_AHEAD_OF_EXIT: Duration = Duration::from_secs(1);

/// How long a window told to close has between its SIGTERM and its SIGKILL, when `grace` passes before that SIGTERM
/// and the signal that started this process's own close came `since_signal` ago (zero if none has). The watchdog ends
/// this process [`CLOSE_WITHIN`] after a signal, and a window that does not answer its own SIGTERM (one that is
/// stopped) outlives whoever was to kill it; a signal that comes later only moves that end further off.
///
/// A window that hears the close late is still given more than its close can take, because a signal has to be answered
/// within [`ANSWER_WITHIN`]: what is left then is above every bound that close may wait out, added up.
pub fn told_within(grace: Duration, since_signal: Duration) -> Duration {
    CLOSE_WITHIN.saturating_sub(since_signal + grace + KILL_AHEAD_OF_EXIT)
}

/// What the watchdog knows when it decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    /// Termination signals seen so far.
    pub seen: u32,
    pub first: i32,
    pub last: i32,
    /// The window's listener has run.
    pub answered: bool,
    /// The window has started its close.
    pub closing: bool,
    /// Since the first signal.
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Wait,
    /// `_exit(128 + signal)`, with the reason written to stderr first.
    Exit {
        signal: i32,
        why: &'static str,
    },
}

/// Leaves an answered signal to the close while it runs within its bounds.
pub fn decide(w: Watch) -> Outcome {
    if w.seen == 0 {
        return Outcome::Wait;
    }
    if w.seen >= 2 && (!w.answered || w.closing) {
        return Outcome::Exit {
            signal: w.last,
            why: "a second termination signal",
        };
    }
    if !w.answered && w.elapsed >= ANSWER_WITHIN {
        return Outcome::Exit {
            signal: w.first,
            why: "the window did not answer the termination signal within 5 s",
        };
    }
    if w.answered && w.closing && w.elapsed >= CLOSE_WITHIN {
        return Outcome::Exit {
            signal: w.first,
            why: "the window's close did not end the process in time",
        };
    }
    Outcome::Wait
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn watch(seen: u32, answered: bool, closing: bool, elapsed_ms: u64) -> Watch {
        Watch {
            seen,
            first: libc::SIGTERM,
            last: if seen >= 2 { libc::SIGINT } else { libc::SIGTERM },
            answered,
            closing,
            elapsed: Duration::from_millis(elapsed_ms),
        }
    }

    fn ms(d: Duration) -> u64 {
        d.as_millis() as u64
    }

    #[test]
    fn one_signal_answered_in_time_is_left_to_the_close() {
        assert_eq!(decide(watch(0, false, false, 60_000)), Outcome::Wait);
        assert_eq!(decide(watch(1, false, false, 4_999)), Outcome::Wait);
        // Answered: the close runs within its own bounds (nvim's end allows 5 s, the agent's teardown more).
        assert_eq!(decide(watch(1, true, true, 30_000)), Outcome::Wait);
        assert_eq!(decide(watch(1, true, false, 60_000)), Outcome::Wait);
    }

    #[test]
    fn a_signal_answered_is_not_ended_by_the_answer_bound_whatever_the_clock_reads() {
        for elapsed_ms in [0, 4_999, 5_000, 5_001, 60_000, u64::MAX / 2] {
            assert_eq!(
                decide(watch(1, true, false, elapsed_ms)),
                Outcome::Wait,
                "{elapsed_ms} ms"
            );
        }
        for elapsed_ms in [0, 4_999, 5_000, 5_001, ms(CLOSE_WITHIN) - 1] {
            assert_eq!(
                decide(watch(1, true, true, elapsed_ms)),
                Outcome::Wait,
                "{elapsed_ms} ms"
            );
        }
    }

    #[test]
    fn a_close_that_outlasts_its_bounds_ends_the_process() {
        assert_eq!(decide(watch(1, true, true, ms(CLOSE_WITHIN) - 1)), Outcome::Wait);
        for elapsed_ms in [ms(CLOSE_WITHIN), ms(CLOSE_WITHIN) + 1, u64::MAX / 2] {
            let Outcome::Exit { signal, .. } = decide(watch(1, true, true, elapsed_ms)) else {
                panic!("a close still running after {elapsed_ms} ms was not ended");
            };
            assert_eq!(signal, libc::SIGTERM, "the first signal's own number");
        }
    }

    /// Every bound a window's close may wait out, added as if they ran one after another.
    fn every_close_bound() -> Duration {
        let session = Duration::from_secs(18);
        let saved_tabs = Duration::from_secs(5);
        let nvim = Duration::from_secs(5);
        let tasks = crate::plugin::tasks::GRACE;
        let pages = crate::plugin::tasks::GRACE + Duration::from_secs(1);
        session + saved_tabs + nvim + tasks + pages
    }

    #[test]
    fn the_close_bound_covers_every_bound_the_close_waits_out() {
        let margin = Duration::from_secs(5);
        assert!(CLOSE_WITHIN >= every_close_bound() + margin);
    }

    const GRACE: Duration = Duration::from_secs(2);

    #[test]
    fn a_told_window_is_killed_before_the_watchdog_ends_this_process() {
        for since_signal in [Duration::ZERO, Duration::from_secs(1), ANSWER_WITHIN] {
            let kill_at = since_signal + GRACE + told_within(GRACE, since_signal);
            assert!(
                kill_at + KILL_AHEAD_OF_EXIT <= CLOSE_WITHIN,
                "a close {since_signal:?} after its signal kills at {kill_at:?}"
            );
        }
    }

    #[test]
    fn a_told_window_has_longer_than_its_close_can_take() {
        for since_signal in [Duration::ZERO, Duration::from_secs(1), ANSWER_WITHIN] {
            assert!(
                GRACE + told_within(GRACE, since_signal) >= every_close_bound(),
                "told {since_signal:?} after the signal"
            );
        }
    }

    #[test]
    fn a_close_with_no_time_left_kills_at_once() {
        assert_eq!(told_within(GRACE, CLOSE_WITHIN + GRACE), Duration::ZERO);
    }

    #[test]
    fn an_unanswered_signal_ends_the_process_after_the_bound_with_its_own_number() {
        let Outcome::Exit { signal, .. } = decide(watch(1, false, false, 5_000)) else {
            panic!("not ended at the bound");
        };
        assert_eq!(signal, libc::SIGTERM);
    }

    #[test]
    fn a_second_signal_forces_unless_the_first_was_answered_and_no_close_began() {
        for (answered, closing) in [(false, false), (true, true), (false, true)] {
            let Outcome::Exit { signal, .. } = decide(watch(2, answered, closing, 10)) else {
                panic!("a second signal did not force (answered {answered}, closing {closing})");
            };
            assert_eq!(signal, libc::SIGINT, "the second signal's own number");
        }
        assert_eq!(decide(watch(2, true, false, 10)), Outcome::Wait);
    }
}
