//! A guard against a `WebView`-crash loop (2026-09-27):
//! `webkit6::WebView::connect_web_process_terminated` fires whenever a hosted page's own render
//! process dies (a real WebKit crash, or the host running out of memory), and the right response
//! is to reload the page -- both the agent panel and a Lua panel already have a working "reload
//! the document, keep everything else" recovery path (`eitri_panel::agent_panel::AgentPanelHandle::reload_document`,
//! and `shell::lua::panel`'s revive-on-next-show `load_uri`), just never wired to this signal.
//! Reloading unconditionally would turn a page that cannot even finish loading (a
//! driver bug, a genuinely out-of-memory host) into a tight crash/reload loop that pins a CPU core
//! forever and never leaves the user anything to read.
//!
//! This module is the pure decision -- how many crashes in how long counts as "a loop", and what
//! to do once that line is crossed -- kept apart from the GTK/WebKit call sites so it is
//! unit-tested without a display, the same split `shell::webkit_zoom` uses for its own WebKit-
//! adjacent arithmetic.
//!
//! **`WebProcessTerminationReason::TerminatedByApi` is never a crash to guard against.** It is
//! WebKit's own name for a *deliberate* `WebView::terminate_web_process()` call -- `shell::main`'s
//! `kill_pane` calls it on a Lua panel's `WebView` when the user explicitly hides that module
//! (`prefix x`), which already has its own recovery (`load_uri` runs again when the module is
//! shown), and must not race a second, automatic reload against that deliberate kill. Both call
//! sites check the reason before ever asking a guard.

use std::time::{Duration, Instant};

/// More than this many crashes inside [`CRASH_LOOP_WINDOW`] is treated as a loop, not bad luck.
/// Low enough that a page which cannot even finish loading gives up within well under a minute
/// rather than spinning forever; high enough that a real, rare WebKit crash (the kind this guard
/// exists to let self-heal) is not mistaken for one.
pub(crate) const MAX_CRASHES_PER_WINDOW: usize = 3;

/// The rolling window [`MAX_CRASHES_PER_WINDOW`] is measured over.
pub(crate) const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(60);

/// What the caller should do about one `web-process-terminated` signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashResponse {
    /// Reload the page: still within the allowed rate.
    Reload,
    /// The crash rate just crossed the limit -- stop reloading and show the caller's own visible
    /// message instead, this one time.
    GiveUp,
    /// Already gave up on an earlier crash from this same guard; do nothing at all, not even
    /// another message -- a page that keeps dying under its own static error text (out of memory
    /// system-wide, say) must not turn *that* into a loop either.
    AlreadyGivenUp,
}

/// One guard per `WebView` -- construct it alongside the view and keep it for that view's whole
/// lifetime. A fresh `WebView` (a killed-and-revived Lua panel gets a new `WebView` object) gets a
/// fresh guard; there is no cross-view state.
pub struct WebViewCrashGuard {
    max: usize,
    window: Duration,
    /// Crash timestamps still inside `window`, oldest first.
    crashes: Vec<Instant>,
    given_up: bool,
}

impl WebViewCrashGuard {
    pub(crate) fn new(max: usize, window: Duration) -> Self {
        WebViewCrashGuard {
            max,
            window,
            crashes: Vec::new(),
            given_up: false,
        }
    }

    /// [`MAX_CRASHES_PER_WINDOW`] crashes per [`CRASH_LOOP_WINDOW`] -- what both call sites use.
    pub fn with_defaults() -> Self {
        Self::new(MAX_CRASHES_PER_WINDOW, CRASH_LOOP_WINDOW)
    }

    /// Records a crash at `now` and returns what to do about it. `now` is a parameter, rather than
    /// this reading the clock itself, so the rolling window is exercised in tests without sleeping.
    pub(crate) fn on_crash_at(&mut self, now: Instant) -> CrashResponse {
        if self.given_up {
            return CrashResponse::AlreadyGivenUp;
        }
        self.crashes.retain(|t| now.saturating_duration_since(*t) < self.window);
        self.crashes.push(now);
        if self.crashes.len() > self.max {
            self.given_up = true;
            CrashResponse::GiveUp
        } else {
            CrashResponse::Reload
        }
    }

    /// [`on_crash_at`](Self::on_crash_at) against the real clock -- what the GTK call sites use.
    pub fn on_crash(&mut self) -> CrashResponse {
        self.on_crash_at(Instant::now())
    }

    /// Re-arms the guard after a successful MANUAL recovery -- the user's own `\u{21bb}`/`prefix r`
    /// (`AgentPanelHandle::reload_document_by_hand`), or a Lua panel's `prefix x` then reopening it
    /// by its own key (`main.rs`'s revive-on-next-show path). Clears `given_up` and the whole crash
    /// history.
    ///
    /// **The bug this closes:** without this, once
    /// `given_up` is set it is permanent (see `after_giving_up_every_further_crash_is_ignored_even_much_later`
    /// below) -- so a page that crashed into a loop, was then successfully recovered by hand, and
    /// LATER suffers one more unrelated crash gets `CrashResponse::AlreadyGivenUp` for that later
    /// crash: no reload, and critically no message either, which is worse than the very first crash
    /// (that one at least explained itself). Both call sites reset a guard exactly where they
    /// perform the manual recovery they are telling the user to try, never from inside the
    /// AUTOMATIC path (`on_crash`/`on_crash_at`'s own `Reload` branch), which must keep counting
    /// toward the very loop it exists to detect.
    pub fn reset(&mut self) {
        self.crashes.clear();
        self.given_up = false;
    }
}

/// A minimal, self-contained "this panel's web process kept crashing" document: no script, no
/// external resource, no themed injection pulled from state that might itself be part of whatever
/// crashed the real page. `recovery` names the affected caller's own way back to a live page by
/// hand -- the agent panel's `\u{21bb}`/`prefix r`, or a Lua panel's `prefix x` then its own key --
/// since the two differ and a message naming the wrong one would not help.
pub fn crash_message_html(recovery: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head><body style=\"margin:0;padding:24px;\
font-family:sans-serif;background:#1e1e1e;color:#d4d4d4;\"><p>This panel's web process kept crashing, \
so Eitri stopped trying to reload it automatically.</p><p>{recovery}</p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Instant {
        // `Instant` has no public constructor from an arbitrary point; anchor every test's
        // timeline to one real `now()` and offset from there.
        Instant::now() + Duration::from_secs(secs)
    }

    #[test]
    fn crashes_within_the_budget_are_reloaded() {
        let mut guard = WebViewCrashGuard::new(3, Duration::from_secs(60));
        assert_eq!(guard.on_crash_at(t(0)), CrashResponse::Reload);
        assert_eq!(guard.on_crash_at(t(1)), CrashResponse::Reload);
        assert_eq!(guard.on_crash_at(t(2)), CrashResponse::Reload);
    }

    #[test]
    fn the_crash_that_crosses_the_limit_gives_up_once() {
        let mut guard = WebViewCrashGuard::new(3, Duration::from_secs(60));
        assert_eq!(guard.on_crash_at(t(0)), CrashResponse::Reload);
        assert_eq!(guard.on_crash_at(t(1)), CrashResponse::Reload);
        assert_eq!(guard.on_crash_at(t(2)), CrashResponse::Reload);
        // The 4th crash inside the 60s window crosses `max == 3`.
        assert_eq!(guard.on_crash_at(t(3)), CrashResponse::GiveUp);
    }

    #[test]
    fn after_giving_up_every_further_crash_is_ignored_even_much_later() {
        let mut guard = WebViewCrashGuard::new(1, Duration::from_secs(60));
        assert_eq!(guard.on_crash_at(t(0)), CrashResponse::Reload);
        assert_eq!(guard.on_crash_at(t(1)), CrashResponse::GiveUp);
        assert_eq!(guard.on_crash_at(t(2)), CrashResponse::AlreadyGivenUp);
        // Even outside the original window: giving up is a terminal state for this guard, not a
        // rate the window ever lets it earn back.
        assert_eq!(guard.on_crash_at(t(600)), CrashResponse::AlreadyGivenUp);
    }

    #[test]
    fn a_crash_older_than_the_window_rolls_off_and_does_not_count_toward_the_limit() {
        let mut guard = WebViewCrashGuard::new(1, Duration::from_secs(10));
        assert_eq!(guard.on_crash_at(t(0)), CrashResponse::Reload);
        // 11s later the first crash is outside the 10s window, so this is the only one counted --
        // still within budget, not a loop.
        assert_eq!(guard.on_crash_at(t(11)), CrashResponse::Reload);
    }

    #[test]
    fn exactly_at_the_limit_still_reloads_only_the_next_one_gives_up() {
        let mut guard = WebViewCrashGuard::new(2, Duration::from_secs(60));
        assert_eq!(guard.on_crash_at(t(0)), CrashResponse::Reload);
        assert_eq!(
            guard.on_crash_at(t(1)),
            CrashResponse::Reload,
            "the 2nd crash is still within max=2"
        );
        assert_eq!(
            guard.on_crash_at(t(2)),
            CrashResponse::GiveUp,
            "the 3rd crash exceeds max=2"
        );
    }

    #[test]
    fn with_defaults_uses_the_published_constants() {
        let mut guard = WebViewCrashGuard::with_defaults();
        for i in 0..MAX_CRASHES_PER_WINDOW {
            assert_eq!(guard.on_crash_at(t(i as u64)), CrashResponse::Reload);
        }
        assert_eq!(
            guard.on_crash_at(t(MAX_CRASHES_PER_WINDOW as u64)),
            CrashResponse::GiveUp
        );
    }

    #[test]
    fn a_reset_guard_forgets_given_up_and_its_crash_history() {
        let mut guard = WebViewCrashGuard::new(1, Duration::from_secs(60));
        assert_eq!(guard.on_crash_at(t(0)), CrashResponse::Reload);
        assert_eq!(guard.on_crash_at(t(1)), CrashResponse::GiveUp);
        // Without a reset, every further crash -- however much later -- is `AlreadyGivenUp` (see
        // the test above). A manual recovery must re-arm it.
        guard.reset();
        assert_eq!(
            guard.on_crash_at(t(2)),
            CrashResponse::Reload,
            "a reset guard catches a later, unrelated crash fresh, not AlreadyGivenUp"
        );
    }

    #[test]
    fn a_reset_guard_also_forgets_crashes_that_had_not_yet_crossed_the_limit() {
        let mut guard = WebViewCrashGuard::new(2, Duration::from_secs(60));
        assert_eq!(guard.on_crash_at(t(0)), CrashResponse::Reload);
        guard.reset();
        // The pre-reset crash must not count toward this guard's budget any more: two more crashes
        // starting fresh both stay within max=2, rather than the 2nd tripping GiveUp as it would
        // if the reset had not cleared `crashes`.
        assert_eq!(guard.on_crash_at(t(1)), CrashResponse::Reload);
        assert_eq!(guard.on_crash_at(t(2)), CrashResponse::Reload);
    }

    #[test]
    fn crash_message_html_carries_the_given_recovery_text_and_no_script() {
        let html = crash_message_html("prefix r reloads it.");
        assert!(html.contains("prefix r reloads it."));
        assert!(
            !html.contains("<script"),
            "the give-up page must never run script: {html}"
        );
    }
}
