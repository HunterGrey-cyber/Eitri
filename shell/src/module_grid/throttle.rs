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
//! GTK hit-tests real allocations (`gtk_widget_do_pick`), so a paint clip alone cannot prevent
//! an oversized held host from taking a sibling's clicks. On each oversized axis, the held
//! allocation extends beyond a grid edge touched by the target. If the target touches the far
//! edge, its position is kept; otherwise it is shifted so its far edge matches the target's
//! far edge. The grid's hidden overflow prevents clicks outside the grid from reaching the host.
//! Only a host with siblings on both sides of an axis is capped to its target on that axis.
//! Capping every shrinking allocation would resize and reflow the WebView on every pass,
//! losing the benefit of holding its size.

use std::collections::HashMap;
use std::time::Duration;

use neovibe_core::layout::{ModuleId, Rect, Size};

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

    /// After a host has been wider because its neighbour was hidden, a divider sweep still gives
    /// it one size throughout the sweep. Following the WebView's natural width would instead resize
    /// it on every frame because that width remembers the widest allocation, 1600 pixels here.
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
    /// The agent starts at width 797 on the right of `Row(editor | agent)` in a
    /// 1280-wide window, then shrinks over 100 divider steps. Its held size extends
    /// past the right window edge, where it cannot cover a sibling, so keep that
    /// size for the whole sweep. Clamping to each target width would resize the
    /// WebView on all 100 steps instead of holding one width.
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
