use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{DrawingArea, Overlay, Paned};

/// Horizontal split, carried over from `shell_chrome::build_content_area`'s `GtkPaned` setup, with
/// the two placeholder panes replaced by the real editor/agent widgets. `agent` is deliberately
/// *not* attached to `paned` directly -- see `install_webview_resize_throttle`'s doc for why -- so
/// the wrapper widget that helper returns becomes the actual end child instead. Returns the
/// wrapping widget plus the `Paned` itself: `main.rs` hands that `Paned` (and `build_vertical_split`'s,
/// when there is one) to `PaneLayout::new`, which is what drives `Ctrl+a`'s zoom and resize (spec
/// 2026-09-19-window-modes-design.md §3.4-3.5) by reading and setting its divider position directly,
/// instead of only reacting to `notify::position` from outside.
pub(crate) fn build_content_area(editor: &gtk4::Widget, agent: &gtk4::Widget) -> (gtk4::Widget, Paned) {
    let paned = Paned::new(gtk4::Orientation::Horizontal);
    paned.add_css_class("content-area");
    paned.set_vexpand(true);
    paned.set_hexpand(true);
    paned.set_wide_handle(true);

    let agent_host = install_webview_resize_throttle(agent);

    paned.set_start_child(Some(editor));
    paned.set_end_child(Some(&agent_host));
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_position(760);

    let widget = paned.clone().upcast();
    (widget, paned)
}

/// How far one `Ctrl+a h/j/k/l` moves a divider, in editor cells (owner's tmux: `resize-pane -L 5`).
pub(crate) const RESIZE_CELLS: i32 = 5;

/// A cell's size, in logical px, when the editor cannot say (not ready yet, or a Lua plugin took
/// the main slot).
pub(crate) const FALLBACK_CELL: (f64, f64) = (8.0, 16.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Down,
    Up,
    Right,
}

/// The two dividers: between editor and panel, and above the bottom slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Split {
    Across,
    Down,
}

/// Which layout pieces a zoom of pane `pane` hides (spec §3.4). `top_row` is the whole
/// editor-and-panel row above the bottom slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Hidden {
    pub(crate) main: bool,
    pub(crate) side: bool,
    pub(crate) top_row: bool,
    pub(crate) bottom: bool,
}

/// `None` for a pane index that does not exist.
pub(crate) fn hidden_for_zoom(pane: usize, has_bottom: bool) -> Option<Hidden> {
    match pane {
        0 => Some(Hidden {
            side: true,
            bottom: has_bottom,
            ..Hidden::default()
        }),
        1 => Some(Hidden {
            main: true,
            bottom: has_bottom,
            ..Hidden::default()
        }),
        2 if has_bottom => Some(Hidden {
            top_row: true,
            ..Hidden::default()
        }),
        _ => None,
    }
}

/// Which divider `direction` moves and which way (`-1` left/up, `+1` right/down), or `None` when
/// there is no divider on that side (spec §3.5). As in tmux, which of two side-by-side panes is
/// current does not matter: they share one divider. The bottom slot spans the whole width, so it
/// has no left/right divider.
pub(crate) fn resize_target(direction: Direction, focused_pane: usize, has_bottom: bool) -> Option<(Split, i32)> {
    match direction {
        Direction::Left | Direction::Right if focused_pane == 2 => None,
        Direction::Left => Some((Split::Across, -1)),
        Direction::Right => Some((Split::Across, 1)),
        Direction::Up | Direction::Down if !has_bottom => None,
        Direction::Up => Some((Split::Down, -1)),
        Direction::Down => Some((Split::Down, 1)),
    }
}

/// The handle `shell` zooms and resizes the panes through (spec §3.4, §3.5).
///
/// A zoom hides every other pane with `set_visible(false)`: a `GtkPaned` with one visible child
/// gives that child all of its space -- the behaviour `install_webview_resize_throttle`'s doc
/// records as a bug for a drag, and exactly what a zoom wants. Hidden panes stay alive (nvim keeps
/// running, the agent session keeps going), and HINT already skips anything unmapped.
pub(crate) struct PaneLayout {
    /// Editor (start) | panel host (end).
    across: Paned,
    /// `across` (start) over the bottom slot (end). Kept `Option` for the shape it had before
    /// something always claimed the bottom slot; since the bottom terminal (2026-09-23) this is
    /// always `Some` in practice too, matching `build_vertical_split`'s own doc (window review
    /// 2026-09-23, M4).
    down: Option<Paned>,
    zoomed: Cell<Option<usize>>,
    /// Both divider positions from just before the zoom, put back by `unzoom`.
    saved: Cell<(i32, Option<i32>)>,
    /// Whether the bottom slot is shown at all, apart from any zoom. False for the built-in
    /// terminal until `Ctrl+a t` (bottom-terminal spec §4.5); a Lua bottom panel is shown from the
    /// start, as before.
    bottom_shown: Cell<bool>,
    /// The divider as the bottom slot was last hidden with, put back when it is shown again. `None`
    /// until it has been shown once: the first show sizes it from the window instead.
    bottom_position: Cell<Option<i32>>,
}

/// Whether the bottom slot is visible: shown, and not hidden by a zoom. A zoom ending must not
/// reveal a terminal nobody asked for -- `unzoom` used to show every child unconditionally.
pub(crate) fn bottom_visible(bottom_shown: bool, zoom: Hidden) -> bool {
    bottom_shown && !zoom.bottom
}

/// Whether `set_bottom_shown(shown)` must unzoom before it hides: the bottom slot is itself the
/// zoomed pane (`zoomed == Some(2)`), so `hidden_for_zoom(2, ..)` hides only the top row -- hiding
/// the bottom on top of that would leave nothing visible at all. Hiding while a DIFFERENT pane is
/// zoomed is not this hole: `hidden_for_zoom` already sets `bottom: true` for panes 0 and 1, so the
/// bottom slot is hidden by the zoom already and `set_bottom_shown`'s own hide changes nothing
/// visible (review 2026-09-23, M3).
pub(crate) fn hiding_bottom_must_unzoom(shown: bool, zoomed: Option<usize>) -> bool {
    !shown && zoomed == Some(2)
}

/// Where the divider goes when the bottom slot is shown: back where it was last hidden, or -- the
/// first time -- a third of `height` (the vertical `GtkPaned`'s, logical px) for the bottom. The
/// divider's build-time position is a constant 480px, which in a maximized window would give the
/// terminal most of the screen on its first appearance. `None`: nothing to go on yet (no height).
pub(crate) fn shown_position(saved: Option<i32>, height: i32) -> Option<i32> {
    saved.or_else(|| (height > 0).then(|| height * 2 / 3))
}

impl PaneLayout {
    /// `bottom_shown` is whether a bottom slot starts out shown: not for the built-in terminal
    /// (bottom-terminal spec §4.5), yes for a Lua panel.
    pub(crate) fn new(across: Paned, down: Option<Paned>, bottom_shown: bool) -> Rc<Self> {
        let layout = Rc::new(PaneLayout {
            across,
            down,
            zoomed: Cell::new(None),
            saved: Cell::new((0, None)),
            bottom_shown: Cell::new(bottom_shown),
            bottom_position: Cell::new(None),
        });
        layout.set_hidden(Hidden::default());
        layout
    }

    /// There is a bottom slot and it is shown -- what zoom and resize treat as "has a bottom".
    pub(crate) fn bottom_shown(&self) -> bool {
        self.down.is_some() && self.bottom_shown.get()
    }

    /// Shows or hides the bottom slot. Hiding never unrealizes it: the terminal's shell keeps
    /// running, and a hidden pane keeps its last size (it reports no allocation while hidden).
    /// Showing puts the divider where [`shown_position`] says; hiding remembers where it was.
    ///
    /// Hiding while the bottom slot is itself the zoomed pane unzooms first ([`hiding_bottom_must_unzoom`]):
    /// `hidden_for_zoom(2, ..)` only hides the top row, so hiding the bottom on top of that zoom
    /// would leave nothing visible at all -- only `ToggleAction::steps`'s leading `Unzoom`
    /// prevented this before (review 2026-09-23, M3); a second caller (phase 5's `Ctrl+a x`) would
    /// have hit it.
    pub(crate) fn set_bottom_shown(&self, shown: bool) {
        if hiding_bottom_must_unzoom(shown, self.zoomed.get()) {
            self.unzoom();
        }
        if let (Some(down), true) = (&self.down, shown != self.bottom_shown.get()) {
            if shown {
                if let Some(position) = shown_position(self.bottom_position.get(), down.height()) {
                    down.set_position(position);
                }
            } else {
                self.bottom_position.set(Some(down.position()));
            }
        }
        self.bottom_shown.set(shown);
        let zoom = self
            .zoomed
            .get()
            .and_then(|pane| hidden_for_zoom(pane, true))
            .unwrap_or_default();
        self.set_hidden(zoom);
    }

    fn set_hidden(&self, hidden: Hidden) {
        let show = |child: Option<gtk4::Widget>, hide: bool| {
            if let Some(child) = child {
                child.set_visible(!hide);
            }
        };
        show(self.across.start_child(), hidden.main);
        show(self.across.end_child(), hidden.side);
        if let Some(down) = &self.down {
            show(down.start_child(), hidden.top_row);
            show(down.end_child(), !bottom_visible(self.bottom_shown.get(), hidden));
        }
    }

    /// `Ctrl+a m`/`z`: zoom `pane`, or restore if anything is zoomed.
    pub(crate) fn toggle_zoom(&self, pane: usize) {
        if self.unzoom() {
            return;
        }
        let Some(hidden) = hidden_for_zoom(pane, self.bottom_shown()) else {
            return;
        };
        self.saved
            .set((self.across.position(), self.down.as_ref().map(|d| d.position())));
        self.set_hidden(hidden);
        self.zoomed.set(Some(pane));
        println!("[layout] zoomed pane {pane}");
    }

    /// Shows everything again and puts both dividers back. `false` if nothing was zoomed. Called
    /// before a pane switch and before a resize, as tmux does (spec §3.4).
    pub(crate) fn unzoom(&self) -> bool {
        if self.zoomed.take().is_none() {
            return false;
        }
        self.set_hidden(Hidden::default());
        let (across, down) = self.saved.get();
        self.across.set_position(across);
        if let (Some(paned), Some(position)) = (&self.down, down) {
            paned.set_position(position);
        }
        println!("[layout] unzoomed");
        true
    }

    /// `Ctrl+a h/j/k/l`: move the divider on that side by `RESIZE_CELLS` cells of `cell` (logical
    /// px). `GtkPaned` clamps the position to its children's minimum sizes itself.
    pub(crate) fn resize(&self, direction: Direction, focused_pane: usize, cell: (f64, f64)) {
        // Decided BEFORE unzooming: a direction with no divider on that side is a no-op, and a
        // no-op must not silently undo a zoom. tmux's `resize-pane` unzooms because it resizes;
        // at the edge of its own grid it does nothing at all.
        let Some((split, sign)) = resize_target(direction, focused_pane, self.bottom_shown()) else {
            return;
        };
        self.unzoom();
        let (paned, px) = match split {
            Split::Across => (&self.across, cell.0),
            Split::Down => match &self.down {
                Some(down) => (down, cell.1),
                None => return,
            },
        };
        let delta = (f64::from(sign * RESIZE_CELLS) * px).round() as i32;
        paned.set_position(paned.position() + delta);
    }
}

/// P7 quick-win: `WebKitWebProcess` was measured pegging ~108% CPU (over one full core) during a
/// resize sweep, because every single `notify::position` change on the paned reallocates -- and
/// so reflows -- the WebView in real time. There's no public GtkPaned/WebKitGTK API to throttle
/// that reflow rate directly.
///
/// **2026-09-06 rewrite.** The first version of this fix called `agent.set_visible(false)` for the
/// duration of a drag burst and `set_visible(true)` ~70ms after the last position change (a hidden
/// widget isn't allocated/painted, so WebKit doesn't reflow it). That traded the CPU problem for a
/// real, separate visual bug: with only one of `Paned`'s two children visible, `GtkPaned` gives
/// that lone child the *entire* paned width and draws no handle at all -- this is standard
/// `GtkPaned` behavior for a single-visible-child paned, not something this crate's code caused.
/// Any real drag (mouse or synthesized) produces `notify::position` events well under 70ms apart,
/// so the WebView stayed hidden -- and the editor pane filled the whole window, divider and
/// WebView gone -- for the entire held-and-moving duration of a drag, snapping back only once
/// motion paused or the button was released. (`Paned::position()` itself tracked the real cursor
/// throughout the hidden period, which is exactly why it always self-corrected instead of getting
/// stuck.) See `MANUAL_VERIFICATION.md` and `docs/canonical/neovibe_feasibility_status.md`'s P7 section for
/// the full repro/root-cause writeup.
///
/// This version never touches `agent`'s visibility, and never lets `agent` itself be a real,
/// currently-allocated `Paned` child at all -- so `Paned`'s own two-visible-children width math
/// can't collapse, regardless of how fast position changes arrive:
/// - `agent` becomes a manually-placed *overlay child* of a `gtk4::Overlay` (`halign`/`valign`
///   left at `Start`, so `Overlay` does not auto-stretch it to the overlay's own size -- see
///   `gtk_overlay_add_overlay()`'s docs).
/// - An empty `gtk4::DrawingArea` "sensor" (drawn to only in the sense of never having a draw
///   func set at all -- it exists purely for its cheap, standard `resize` signal) is the
///   `Overlay`'s *main* child, and it's the `Overlay` -- not `agent` -- that `build_content_area`
///   gives to `Paned` as the real end child. `Overlay` always gives its main child the overlay's
///   full, real-time allocation no matter what any overlay child is doing, so from `Paned`'s point
///   of view its end child is an ordinary, always-correctly-sized visible widget throughout any
///   drag or plain window resize.
/// - The sensor's `resize` signal -- emitted synchronously by GTK every time `Paned` (via
///   `Overlay`) gives it a new size -- is what actually drives `agent`'s size, via
///   `set_size_request`, debounced by the same `DEBOUNCE_MS` as before: at most once per burst,
///   ~70ms after the last resize in it. In between, `agent` simply keeps its last real size --
///   still visible, still painting, just not asked to reflow in real time -- which is what
///   preserves the original CPU fix. The very first resize (app startup) is applied immediately
///   rather than debounced, so the WebView is never briefly stuck at a stale/zero size before the
///   first drag ever happens. This throttle is what makes the WebView pane different from the
///   editor pane: `editor` is `paned`'s *other* child, attached directly with no such debounce, so
///   `NeovideEditorPane` (in the separate, frozen `neovide-editor` crate) receives every
///   intermediate resize during a drag immediately and in real time, with no equivalent sensor or
///   debounce logic anywhere in `shell` standing in front of it.
/// - `set_clip_overlay` guards the one edge case this design creates: mid-*shrink*-drag, `agent`
///   can briefly still be sized larger than the shrinking `Overlay` slot until the debounce catches
///   up; without clipping, that would visibly spill over into the editor pane.
///
/// Returns the `Overlay` (upcast to `Widget`) the caller must use as `Paned`'s end child -- handing
/// `agent` itself to `Paned` would bypass this whole mechanism and reintroduce the original bug.
pub(crate) fn install_webview_resize_throttle(agent: &gtk4::Widget) -> gtk4::Widget {
    const DEBOUNCE_MS: u32 = 70;

    agent.set_halign(gtk4::Align::Start);
    agent.set_valign(gtk4::Align::Start);

    let sensor = DrawingArea::new();
    sensor.set_hexpand(true);
    sensor.set_vexpand(true);

    let overlay = Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.set_child(Some(&sensor));
    overlay.add_overlay(agent);
    overlay.set_clip_overlay(agent, true);

    let agent = agent.clone();
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let primed: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    sensor.connect_resize(move |_sensor, width, height| {
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        if !primed.get() {
            // First-ever layout: size it now, not debounced, so the WebView doesn't sit at a
            // stale/zero size before the user has dragged (or resized) anything.
            primed.set(true);
            agent.set_size_request(width, height);
            return;
        }
        let agent2 = agent.clone();
        let pending2 = pending.clone();
        let id = glib::source::timeout_add_local(Duration::from_millis(DEBOUNCE_MS as u64), move || {
            agent2.set_size_request(width, height);
            *pending2.borrow_mut() = None;
            glib::ControlFlow::Break
        });
        *pending.borrow_mut() = Some(id);
    });

    overlay.upcast()
}

/// Puts `bottom` underneath `content`, full width, in a vertical `GtkPaned`.
///
/// Called only when something has claimed the bottom slot. Since the bottom terminal
/// (2026-09-23) something always has: the terminal, which is registered as the built-in bottom
/// panel and hidden until `Ctrl+a t`, or a Lua panel that replaced it. So this `Paned` now always
/// exists; while the terminal is hidden its end child is not visible, and the `Paned` gives
/// `content` the whole height. (Before, a window with nothing in the bottom slot had no `Paned` at
/// all.) The 480px below is only a starting value: the terminal's first show sets the divider from
/// the window's height (`shown_position`).
///
/// `bottom` gets `shrink = false` and a `size_request` floor for a reason a terminal makes
/// concrete: a `GtkPaned` child with shrink enabled can be dragged to zero height, and a terminal
/// dragged to zero reports a 1-row grid to its child process. `resize_start_child(true)` with
/// `resize_end_child(false)` keeps the divider where the user left it when the window is resized,
/// which is what a bottom terminal should do -- growing the window should give the editor the new
/// space, not the terminal.
pub(crate) fn build_vertical_split(content: &gtk4::Widget, bottom: &gtk4::Widget) -> (gtk4::Widget, Paned) {
    const MIN_TERMINAL_HEIGHT: i32 = 80;

    let paned = Paned::new(gtk4::Orientation::Vertical);
    paned.add_css_class("content-area");
    paned.set_vexpand(true);
    paned.set_hexpand(true);
    paned.set_wide_handle(true);

    bottom.set_size_request(-1, MIN_TERMINAL_HEIGHT);

    paned.set_start_child(Some(content));
    paned.set_end_child(Some(bottom));
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(false);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_position(480);

    let widget = paned.clone().upcast();
    (widget, paned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zooming_a_pane_hides_every_other_one() {
        assert_eq!(
            hidden_for_zoom(0, false),
            Some(Hidden {
                side: true,
                ..Hidden::default()
            })
        );
        assert_eq!(
            hidden_for_zoom(1, false),
            Some(Hidden {
                main: true,
                ..Hidden::default()
            })
        );
        assert_eq!(
            hidden_for_zoom(0, true),
            Some(Hidden {
                side: true,
                bottom: true,
                ..Hidden::default()
            })
        );
        assert_eq!(
            hidden_for_zoom(1, true),
            Some(Hidden {
                main: true,
                bottom: true,
                ..Hidden::default()
            })
        );
        assert_eq!(
            hidden_for_zoom(2, true),
            Some(Hidden {
                top_row: true,
                ..Hidden::default()
            })
        );
    }

    #[test]
    fn the_first_show_gives_the_bottom_a_third_and_later_ones_put_the_divider_back() {
        assert_eq!(
            shown_position(None, 1440),
            Some(960),
            "a maximized window: 480px of terminal"
        );
        assert_eq!(shown_position(Some(700), 1440), Some(700), "where the user left it");
        assert_eq!(shown_position(None, 0), None, "not laid out: leave the divider alone");
    }

    /// Only the pure rule: whether `set_hidden` really consults it (instead of `hidden.bottom`) is
    /// GTK code no unit test here reaches, and is on the GUI checklist ("with the terminal hidden,
    /// `Ctrl+a z` twice must NOT reveal it").
    #[test]
    fn a_hidden_bottom_slot_stays_hidden_whatever_the_zoom_does() {
        assert!(
            !bottom_visible(false, Hidden::default()),
            "unzoom must not reveal a hidden terminal"
        );
        assert!(!bottom_visible(false, hidden_for_zoom(0, true).unwrap()));
        assert!(
            !bottom_visible(true, hidden_for_zoom(1, true).unwrap()),
            "zooming a pane above hides it"
        );
        assert!(bottom_visible(true, Hidden::default()));
        assert!(
            bottom_visible(true, hidden_for_zoom(2, true).unwrap()),
            "zooming the terminal itself"
        );
    }

    /// Review 2026-09-23, M3: `set_bottom_shown(false)` while `zoomed == Some(2)` used to compute
    /// `hidden_for_zoom(2, true)` (top row hidden) and then hide the bottom too, leaving nothing
    /// visible. Only `ToggleAction::steps`'s own leading `Unzoom` masked this for `Ctrl+a t`; a
    /// second caller of `set_bottom_shown` (phase 5's `Ctrl+a x`) would have hit it directly.
    #[test]
    fn hiding_the_bottom_while_it_is_the_zoomed_pane_must_unzoom_first() {
        assert!(hiding_bottom_must_unzoom(false, Some(2)));
        assert!(!hiding_bottom_must_unzoom(true, Some(2)), "showing is unaffected");
        assert!(
            !hiding_bottom_must_unzoom(false, Some(0)),
            "a different pane's zoom already hides the bottom via hidden_for_zoom's own bottom: true"
        );
        assert!(
            !hiding_bottom_must_unzoom(false, Some(1)),
            "same as pane 0: hidden_for_zoom(1, true) already sets bottom: true"
        );
        assert!(!hiding_bottom_must_unzoom(false, None), "nothing zoomed");
    }

    #[test]
    fn a_pane_that_does_not_exist_zooms_nothing() {
        assert_eq!(hidden_for_zoom(2, false), None);
        assert_eq!(hidden_for_zoom(3, true), None);
    }

    #[test]
    fn h_and_l_move_the_one_divider_between_editor_and_panel_from_either_side() {
        for pane in [0, 1] {
            assert_eq!(resize_target(Direction::Left, pane, true), Some((Split::Across, -1)));
            assert_eq!(resize_target(Direction::Right, pane, false), Some((Split::Across, 1)));
        }
        assert_eq!(resize_target(Direction::Left, 2, true), None);
        assert_eq!(resize_target(Direction::Right, 2, true), None);
    }

    #[test]
    fn k_and_j_move_the_bottom_divider_and_do_nothing_without_one() {
        assert_eq!(resize_target(Direction::Up, 0, true), Some((Split::Down, -1)));
        assert_eq!(resize_target(Direction::Down, 2, true), Some((Split::Down, 1)));
        assert_eq!(resize_target(Direction::Up, 0, false), None);
        assert_eq!(resize_target(Direction::Down, 1, false), None);
    }
}
