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
//!
//! **A held size never reaches over a sibling (sw-layout-2, 2026-09-27 Codex sweep).** GTK
//! hit-tests a widget by its own real allocation (`gtk_widget_do_pick`), never by
//! `snapshot_children`'s paint clip, so a held size bigger than the target -- a host that was just
//! unzoomed, settled at the whole window, is the clearest case -- used to reach past its target over
//! the sibling that settled into the space and take a click that visibly lands on that sibling. On
//! each axis where the held size is bigger than the target, the overflow is pointed past the edge of
//! the grid the target touches, where there is no sibling: past the far edge when the target touches
//! it (the target's own position, as before), else past the near edge (the held size ends where the
//! target ends). The grid sets `overflow: hidden`, which makes GTK's pick return nothing for a point
//! outside the grid, so an allocation reaching past the grid's edge can take no click from the top
//! bar or anything else outside it. Only a host with a sibling on both sides of an axis is clamped to
//! its target there, trading a WebView resize per pass for correctness.
//!
//! The first version of this fix clamped every held size to its target, which made every pass of
//! every shrinking gesture a WebView resize -- 100 widths for a 100-pass sweep of the agent on the
//! window's right, against one before, and P7/S3 measured over a full core for a WebView that follows
//! every frame (whole-branch review).

use std::collections::HashMap;
use std::time::Duration;

use neovibe_core::layout::{ModuleId, Rect, Size};

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
    /// What to allocate web host `id`, whose rectangle this pass is `target` inside a grid of size
    /// `bounds`. `was_on_screen`: it was allocated in the previous pass too.
    pub(crate) fn allocate(&mut self, id: &ModuleId, target: Rect, was_on_screen: bool, bounds: Size) -> WebAllocation {
        match self.settled.get(id) {
            Some(&(w, h)) if was_on_screen && (w, h) != (target.w, target.h) => {
                let (x, w) = held_span(target.x, target.w, w, bounds.w);
                let (y, h) = held_span(target.y, target.h, h, bounds.h);
                WebAllocation {
                    rect: Rect { x, y, w, h },
                    held: true,
                    restart_quiet: self.held_for.insert(id.clone(), target) != Some(target),
                }
            }
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

/// One axis of a held allocation, as `(start, length)`: the target's span is `start..start + len`,
/// the settled length `held`, the grid's length on this axis `bound`. See the module doc's
/// sw-layout-2 paragraph: an overflow goes past a grid edge the target touches, never over a
/// sibling.
fn held_span(start: i32, len: i32, held: i32, bound: i32) -> (i32, i32) {
    if held <= len || start + len >= bound {
        // Nothing overflows, or it overflows past the grid's far edge: the target's own position.
        (start, held)
    } else if start <= 0 {
        // Past the grid's near edge: the held length ends where the target ends.
        (start + len - held, held)
    } else {
        // A sibling on both sides: never past the target.
        (start, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn rect(x: i32, w: i32) -> Rect {
        Rect { x, y: 0, w, h: 700 }
    }

    /// The grid these tests' rectangles fill: `rect(803, 797)` ends at its right edge.
    const WINDOW: Size = Size { w: 1600, h: 700 };

    #[test]
    fn the_first_allocation_and_a_reappearing_host_get_their_target_at_once() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        assert_eq!(
            t.allocate(&agent, rect(803, 797), false, WINDOW),
            WebAllocation {
                rect: rect(803, 797),
                held: false,
                restart_quiet: false,
            }
        );
        // Hidden, then shown at another size: not a resize anyone watched, so no hold.
        assert_eq!(t.allocate(&agent, rect(0, 1600), false, WINDOW).rect, rect(0, 1600));
    }

    #[test]
    fn a_size_change_keeps_the_settled_size_at_the_new_position_until_quiet() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        t.allocate(&agent, rect(803, 797), false, WINDOW);
        let held = t.allocate(&agent, rect(700, 900), true, WINDOW);
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
            t.allocate(&agent, rect(700, 900), true, WINDOW),
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
        t.allocate(&agent, rect(803, 797), false, WINDOW);
        assert!(!t.allocate(&agent, rect(400, 797), true, WINDOW).held);
    }

    /// The S3 regression, as a test of the rule rather than of GTK: after the host has once been
    /// wider (its neighbour hidden, then shown again), a divider sweep still gives it ONE size for
    /// the whole sweep. P7's wrapper followed the WebView's natural width, which remembers the
    /// widest allocation (1600 here), and resized it on every frame of the sweep.
    #[test]
    fn the_throttle_never_follows_a_width_the_host_once_had() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        t.allocate(&agent, rect(803, 797), false, WINDOW);
        // The editor is hidden: the agent takes the whole width once things are quiet.
        t.allocate(&agent, rect(0, 1600), true, WINDOW);
        t.settle();
        assert_eq!(t.allocate(&agent, rect(0, 1600), true, WINDOW).rect.w, 1600);
        // The editor comes back.
        t.allocate(&agent, rect(803, 797), true, WINDOW);
        t.settle();
        assert_eq!(t.allocate(&agent, rect(803, 797), true, WINDOW).rect.w, 797);
        // A sweep of 100 divider moves with no quiet in between.
        let widths: BTreeSet<i32> = (0..100)
            .map(|i| t.allocate(&agent, rect(803 - i * 6, 797 + i * 6), true, WINDOW).rect.w)
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
        t.allocate(&agent, rect(803, 797), false, WINDOW);
        assert!(
            t.allocate(&agent, rect(700, 900), true, WINDOW).restart_quiet,
            "the drag's first step"
        );
        let again = t.allocate(&agent, rect(700, 900), true, WINDOW);
        assert!(again.held, "still held");
        assert!(!again.restart_quiet, "the same target again is not a change");
        assert!(
            t.allocate(&agent, rect(650, 950), true, WINDOW).restart_quiet,
            "the next step is"
        );
        // Back to the settled size: not held, and a later hold for a target seen before still counts.
        assert!(!t.allocate(&agent, rect(803, 797), true, WINDOW).held);
        assert!(t.allocate(&agent, rect(650, 950), true, WINDOW).restart_quiet);
        t.settle();
        assert!(
            !t.allocate(&agent, rect(650, 950), true, WINDOW).restart_quiet,
            "settled: not held at all"
        );
    }

    #[test]
    fn hosts_are_held_independently() {
        let mut t = WebThrottle::default();
        let (agent, notes) = (ModuleId::agent(), ModuleId::lua("notes"));
        t.allocate(&agent, rect(0, 500), false, WINDOW);
        t.allocate(&notes, rect(501, 500), false, WINDOW);
        assert!(t.allocate(&agent, rect(0, 600), true, WINDOW).held);
        assert!(
            !t.allocate(&notes, rect(601, 500), true, WINDOW).held,
            "notes only moved"
        );
    }
    /// The whole-branch review's probe (sw-layout-2): the agent settled at 797 on the right of
    /// `Row(editor | agent)` in a 1280-wide window, then a 100-pass divider sweep SHRINKING it. Its
    /// held size overflows past the window's right edge, where there is no sibling to cover, so the
    /// hold must stand for the whole sweep -- P7/S3's throttle. Clamping every held size to its
    /// target made every pass of every shrink a WebView resize (100 widths, not 1).
    #[test]
    fn a_host_shrinking_toward_the_window_edge_is_still_held_at_one_size() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        let window = Size { w: 1280, h: 700 };
        t.allocate(&agent, rect(483, 797), false, window);
        let passes: Vec<WebAllocation> = (1..=100)
            .map(|i| t.allocate(&agent, rect(483 + i * 6, 797 - i * 6), true, window))
            .collect();
        let widths: BTreeSet<i32> = passes.iter().map(|a| a.rect.w).collect();
        assert_eq!(widths, BTreeSet::from([797]));
        for (i, a) in (1..=100).zip(&passes) {
            assert_eq!(
                a.rect.x,
                483 + i * 6,
                "at the target's own position; the overflow is off the window"
            );
        }
    }

    /// The same sweep with the agent on the LEFT of `Row(agent | editor)`: its far edge faces the
    /// editor, so the held size ends where the target ends and overflows past the window's left
    /// edge instead -- still one size for the whole sweep, and never a pixel over the editor. This
    /// is sw-layout-2's own shape (a host shrinking with its sibling beyond its far edge).
    #[test]
    fn a_host_shrinking_away_from_the_window_edge_is_held_without_covering_its_sibling() {
        let mut t = WebThrottle::default();
        let agent = ModuleId::agent();
        let window = Size { w: 1280, h: 700 };
        t.allocate(&agent, rect(0, 797), false, window);
        let mut widths = BTreeSet::new();
        for i in 1..=100 {
            let target = rect(0, 797 - i * 6);
            let a = t.allocate(&agent, target, true, window);
            assert!(a.held);
            assert_eq!(
                a.rect.x + a.rect.w,
                target.x + target.w,
                "pass {i}: ends where the target ends"
            );
            widths.insert(a.rect.w);
        }
        assert_eq!(widths, BTreeSet::from([797]));
    }

    /// A host with a sibling on both sides of the axis (a middle column) has nowhere to overflow:
    /// it is clamped to its target there, never reaching either neighbour.
    #[test]
    fn a_host_between_two_siblings_is_clamped_to_its_target() {
        let mut t = WebThrottle::default();
        let panel = ModuleId::lua("notes");
        let window = Size { w: 1280, h: 700 };
        t.allocate(&panel, rect(400, 480), false, window);
        let target = rect(420, 400);
        let a = t.allocate(&panel, target, true, window);
        assert!(a.held);
        assert_eq!((a.rect.x, a.rect.w), (420, 400));
        // And vertically: a shrink whose target touches neither the grid's top nor its bottom.
        let tall = Rect {
            x: 0,
            y: 100,
            w: 400,
            h: 400,
        };
        t.allocate(&panel, tall, false, window);
        let shorter = Rect {
            x: 0,
            y: 150,
            w: 400,
            h: 300,
        };
        let a = t.allocate(&panel, shorter, true, window);
        assert_eq!((a.rect.y, a.rect.h), (150, 300));
    }
}
