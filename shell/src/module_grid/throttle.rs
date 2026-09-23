//! The web host resize throttle, owned by the grid (modules spec §5, S3 change 2). Pure: it decides
//! what size to give a web host this allocation pass, and has no way to ask a widget anything.
//!
//! **Why web hosts are throttled at all (P7).** `WebKitWebProcess` was measured at ~108% CPU -- over
//! one full core -- during a divider sweep, because every size change reflows the page. S3
//! reproduced it inside the grid: 115-126% with the WebView following every frame.
//!
//! **Why this is not P7's wrapper any more.** `layout.rs`'s `install_webview_resize_throttle` put
//! the WebView in a `GtkOverlay` at `halign = Start` over a `DrawingArea` sensor, and resized it by
//! `set_size_request` ~70ms after the sensor stopped moving. `halign = Start` makes the overlay
//! give the WebView its **natural** width, and `WebKitWebView`'s natural width sticks at the widest
//! allocation it has ever had. So once the web host has been wider than its slot -- its neighbour
//! hidden, a zoom, a maximise -- the wrapper follows the overlay every frame again: S3 measured
//! 56.7-57.7% WebKit CPU in a sweep after 50 editor hides, against 0% from a fresh start, and 442
//! WebView resizes in one sweep against 1. `shell` on `main` very likely has the same defect after
//! zooming the agent (inferred from the identical code, not measured; the P1 checklist measures it).
//!
//! **The rule here.** A web host that was on screen last pass and whose target size changed keeps
//! the size it was last really given, at its new position, clipped to its new rectangle, and gets
//! the target once the grid has seen [`QUIET`] with no further change. A host appearing (first
//! allocation, or shown again) gets its target at once. A move that keeps the size is not a
//! resize and is never held. Nothing here -- no input, no field -- is a natural size.

use std::collections::HashMap;
use std::time::Duration;

use neovibe_core::layout::{ModuleId, Rect};

/// P7's debounce: a web host gets its real size this long after the last change.
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
    /// indefinitely (the whole-branch review, Task 6's #4 carried over).
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
                rect: Rect { w, h, ..target },
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

    /// The S3 regression, as a test of the rule rather than of GTK: after the host has once been
    /// wider (its neighbour hidden, then shown again), a divider sweep still gives it ONE size for
    /// the whole sweep. P7's wrapper followed the WebView's natural width, which remembers the
    /// widest allocation (1600 here), and resized it on every frame of the sweep.
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
