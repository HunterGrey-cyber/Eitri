//! The grid's web host resize throttle decides allocations without querying widgets.
//! Resizing a WebView reflows its page and can consume more than one CPU core during a divider sweep.
//!
//! An overlay using `halign = Start` gives the WebView its natural width, which remembers its
//! widest allocation. After a zoom, maximize or hidden neighbour makes the WebView wider than
//! its slot, an overlay-based debounce can therefore resize it on every frame again.
//!
//! A visible host whose target size changes keeps its last allocated size at the new position,
//! clipped to the target rectangle. It receives the target size after [`QUIET`] with no further
//! change. A newly appearing host receives its target immediately. Moves that keep the size
//! are never held. No input or stored field here is a natural size.
//!
//! A held size is capped to the target in both dimensions. GTK hit-tests the real allocation
//! (`gtk_widget_do_pick` and `gtk_widget_contains`), so a paint clip alone cannot keep an oversized
//! host from taking a sibling's clicks. Unzooming can shrink the target from the whole window
//! in one step, making this cap necessary even without a preceding series of smaller targets.
//! The cap leaves an already smaller held size unchanged. A continuously shrinking target can
//! force a resize and page reflow on every pass. This module does not track which side of a
//! split holds the host, so it cannot distinguish safe overflow from overflow into a sibling.

use std::collections::HashMap;
use std::time::Duration;

use neovibe_core::layout::{ModuleId, Rect};

/// A web host receives its target size this long after the last size change.
pub(crate) const QUIET: Duration = Duration::from_millis(70);

/// What to give one web host this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WebAllocation {
    /// The allocation: the target's position, and either the target's size or the settled one.
    pub(crate) rect: Rect,
    /// `true` when `rect` is not the target yet: the grid must clip the host to its target and
    /// settle once it is quiet.
    pub(crate) held: bool,
    /// `true` when this pass holds the host for a target it was not held for last pass: the
    /// quiet period starts again. A pass that holds it for the same target -- the grid allocated
    /// again for some other reason (a `queue_resize` anywhere under it) -- is not a change, and
    /// restarting the timer on it would let a stream of such passes keep the host at its old size
    /// indefinitely.
    pub(crate) restart_quiet: bool,
}

#[derive(Debug, Default)]
pub(crate) struct WebThrottle {
    /// The size each web host was last really given.
    settled: HashMap<ModuleId, (i32, i32)>,
    /// The target each held host was held for in the pass that last held it.
    held_for: HashMap<ModuleId, Rect>,
}

impl WebThrottle {
    /// What to allocate web host `id`, whose rectangle this pass is `target`. `was_on_screen`: it
    /// was allocated in the previous pass too.
    pub(crate) fn allocate(&mut self, id: &ModuleId, target: Rect, was_on_screen: bool) -> WebAllocation {
        match self.settled.get(id) {
            Some(&(w, h)) if was_on_screen && (w, h) != (target.w, target.h) => WebAllocation {
                // Never bigger than the target (sw-layout-2): the held size is kept only up to what
                // the target itself allows, so the real GTK allocation this pass -- what GTK hit-tests
                // clicks against -- can never reach past the target and over whatever has settled
                // into the space beyond it.
                rect: Rect {
                    w: w.min(target.w),
                    h: h.min(target.h),
                    ..target
                },
                held: true,
                restart_quiet: self.held_for.insert(id.clone(), target) != Some(target),
            },
            _ => {
                self.held_for.remove(id);
                self.settled.insert(id.clone(), (target.w, target.h));
                WebAllocation {
                    rect: target,
                    held: false,
                    restart_quiet: false,
                }
            }
        }
    }

    /// The grid has been quiet for [`QUIET`]: every host gets its target on the next pass.
    pub(crate) fn settle(&mut self) {
        self.settled.clear();
        self.held_for.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn rect(x: i32, w: i32) -> Rect {
        Rect { x, y: 0, w, h: 700 }
    }

    #[test]
    fn the_first_allocation_and_a_reappearing_host_get_their_target_at_once() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        assert_eq!(
            t.allocate(&agent, rect(803, 797), false),
            WebAllocation {
                rect: rect(803, 797),
                held: false,
                restart_quiet: false,
            }
        );
        // Hidden, then shown at another size: not a resize anyone watched, so no hold.
        assert_eq!(t.allocate(&agent, rect(0, 1600), false).rect, rect(0, 1600));
    }

    #[test]
    fn a_size_change_keeps_the_settled_size_at_the_new_position_until_quiet() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        t.allocate(&agent, rect(803, 797), false);
        let held = t.allocate(&agent, rect(700, 900), true);
        assert_eq!(
            held,
            WebAllocation {
                rect: Rect {
                    x: 700,
                    y: 0,
                    w: 797,
                    h: 700
                },
                held: true,
                restart_quiet: true,
            }
        );
        t.settle();
        assert_eq!(
            t.allocate(&agent, rect(700, 900), true),
            WebAllocation {
                rect: rect(700, 900),
                held: false,
                restart_quiet: false,
            }
        );
    }

    #[test]
    fn a_move_that_keeps_the_size_is_never_held() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        t.allocate(&agent, rect(803, 797), false);
        assert!(!t.allocate(&agent, rect(400, 797), true).held);
    }

    /// After a host has been wider because its neighbour was hidden, a divider sweep still gives
    /// it one size throughout the sweep. Following the WebView's natural width would instead resize
    /// it on every frame because that width remembers the widest allocation, 1600 pixels here.
    #[test]
    fn the_throttle_never_follows_a_width_the_host_once_had() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        t.allocate(&agent, rect(803, 797), false);
        // The editor is hidden: the agent takes the whole width once things are quiet.
        t.allocate(&agent, rect(0, 1600), true);
        t.settle();
        assert_eq!(t.allocate(&agent, rect(0, 1600), true).rect.w, 1600);
        // The editor comes back.
        t.allocate(&agent, rect(803, 797), true);
        t.settle();
        assert_eq!(t.allocate(&agent, rect(803, 797), true).rect.w, 797);
        // A sweep of 100 divider moves with no quiet in between.
        let widths: BTreeSet<i32> = (0..100)
            .map(|i| t.allocate(&agent, rect(803 - i * 6, 797 + i * 6), true).rect.w)
            .collect();
        assert_eq!(widths, BTreeSet::from([797]));
    }

    /// The module doc's rule, "the target once the grid has seen [`QUIET`] with no further change":
    /// a pass that holds a host for the target it was already held for does not restart the quiet
    /// period. The first version restarted it on every pass that held anything.
    #[test]
    fn only_a_new_target_restarts_the_quiet_period() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        t.allocate(&agent, rect(803, 797), false);
        assert!(
            t.allocate(&agent, rect(700, 900), true).restart_quiet,
            "the drag's first step"
        );
        let again = t.allocate(&agent, rect(700, 900), true);
        assert!(again.held, "still held");
        assert!(!again.restart_quiet, "the same target again is not a change");
        assert!(
            t.allocate(&agent, rect(650, 950), true).restart_quiet,
            "the next step is"
        );
        // Back to the settled size: not held, and a later hold for a target seen before still counts.
        assert!(!t.allocate(&agent, rect(803, 797), true).held);
        assert!(t.allocate(&agent, rect(650, 950), true).restart_quiet);
        t.settle();
        assert!(
            !t.allocate(&agent, rect(650, 950), true).restart_quiet,
            "settled: not held at all"
        );
    }

    #[test]
    fn hosts_are_held_independently() {
        let mut t = WebThrottle::default();
        let (agent, notes) = (ModuleId::agent(), ModuleId::lua("notes"));
        t.allocate(&agent, rect(0, 500), false);
        t.allocate(&notes, rect(501, 500), false);
        assert!(t.allocate(&agent, rect(0, 600), true).held);
        assert!(!t.allocate(&notes, rect(601, 500), true).held, "notes only moved");
    }
}
