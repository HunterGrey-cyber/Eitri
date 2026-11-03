//! The keys a window started to leave them elsewhere is given by the compositor when it maps.
//!
//! A window whose placement role is [`PlacementRole::RestoredSecond`] -- window 2 started again at a launch, a hub's
//! chat or page window opened at the hub's start -- is meant to leave the keys in window 1. sway gives every window
//! it maps the keys all the same (Hyprland and niri do too by default), and the placement hands them back to window 1
//! only once the window is on an output and the compositor has run the focus command: milliseconds on an idle machine,
//! longer on a loaded one. A key typed in window 1 in that gap reaches this window instead: a chat page, where a key
//! can type into the composer or run a page key. So such a window holds its window stage's key presses from its start
//! until the keys are its own, and drops what it holds.
//!
//! The hold ends at the first of: the window losing the keys after it had them (they went back), a click in the
//! window (the person reached for it), the other window giving it the keys (`focus{}` over the link), and
//! [`BACKSTOP`] after the keys first reached it or its placement was asked, whichever came first. The backstop is
//! for a compositor that leaves the keys here (a rule that focuses the window, niri, which is not asked to move
//! them): there a person who wants this window is not refused its keys for more than a moment.

use std::time::Duration;

use super::window::{PlacementRole, Window};
use super::TimerToken;

/// How long a hold lasts at most after the keys reached the window or its placement was asked.
pub(crate) const BACKSTOP: Duration = Duration::from_secs(1);

/// Whether the window holds the keys it is given, and what the hold has seen.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum StartKeys {
    /// Keys go where the window sends them.
    #[default]
    Free,
    Held {
        /// The backstop, once armed.
        backstop: Option<TimerToken>,
        /// Whether the window has had the keys since the hold began.
        was_active: bool,
        /// How many presses the hold dropped.
        dropped: u32,
    },
}

/// The hold a start with `role` begins with.
pub(super) fn for_role(role: PlacementRole) -> StartKeys {
    match role {
        PlacementRole::RestoredSecond { .. } => StartKeys::Held {
            backstop: None,
            was_active: false,
            dropped: 0,
        },
        PlacementRole::Main | PlacementRole::NewSecond | PlacementRole::NewModule => StartKeys::Free,
    }
}

/// A key press reached the window's stage: `true` when the hold takes it, which drops it.
pub(super) fn holds(w: &mut Window) -> bool {
    let StartKeys::Held { dropped, .. } = &mut w.start_keys else {
        return false;
    };
    *dropped += 1;
    if *dropped == 1 {
        w.say("[focus] a key came before the keys went back to the window that keeps them; it is dropped".into());
    }
    true
}

/// The window became active or stopped being.
pub(super) fn on_active(w: &mut Window, active: bool) {
    let StartKeys::Held { was_active, .. } = &mut w.start_keys else {
        return;
    };
    if active {
        *was_active = true;
        arm_backstop(w);
    } else if *was_active {
        release(w, "they went back");
    }
}

/// The placement that hands the keys back was asked of the host.
pub(super) fn placement_asked(w: &mut Window) {
    arm_backstop(w);
}

/// The person or the other window chose this window: the keys are its own.
pub(super) fn chosen(w: &mut Window, why: &str) {
    release(w, why);
}

/// `token` fired: `true` when it was this window's backstop, which ends the hold.
pub(super) fn timer_fired(w: &mut Window, token: TimerToken) -> bool {
    if !matches!(w.start_keys, StartKeys::Held { backstop: Some(armed), .. } if armed == token) {
        return false;
    }
    if let StartKeys::Held { backstop, .. } = &mut w.start_keys {
        *backstop = None;
    }
    release(w, "the backstop passed");
    true
}

fn arm_backstop(w: &mut Window) {
    if matches!(w.start_keys, StartKeys::Held { backstop: None, .. }) {
        let token = w.arm_after(BACKSTOP);
        if let StartKeys::Held { backstop, .. } = &mut w.start_keys {
            *backstop = Some(token);
        }
    }
}

fn release(w: &mut Window, why: &str) {
    let StartKeys::Held { backstop, dropped, .. } = std::mem::take(&mut w.start_keys) else {
        return;
    };
    if let Some(token) = backstop {
        w.cancel_timer(token);
    }
    w.say(format!(
        "[focus] the window takes its keys ({why}); {dropped} key(s) dropped before"
    ));
}

#[cfg(test)]
mod tests;
