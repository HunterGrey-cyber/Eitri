use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{DrawingArea, Overlay, Paned};

/// Builds a horizontal split with the editor as its start child and a resize-throttling
/// wrapper around `agent` as its end child. The wrapper comes from
/// `install_webview_resize_throttle`; the agent is not attached directly. Returns the
/// wrapping widget and the `Paned`. `main.rs` passes this divider and any vertical divider
/// to `PaneLayout::new`, so `Ctrl+a` zoom and resize can read and set divider positions
/// directly rather than only reacting to `notify::position`.
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

/// How far one `Ctrl+a h/j/k/l` moves a divider, in editor cells.
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

/// Which layout pieces a zoom of pane `pane` hides. `top_row` is the whole
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
/// there is no divider on that side. As in tmux, which of two side-by-side panes is
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

/// The handle `shell` zooms and resizes the panes through.
///
/// A zoom hides every other pane with `set_visible(false)`: a `GtkPaned` with one visible child
/// gives that child all of its space -- the behaviour `install_webview_resize_throttle`'s doc
/// records as a bug for a drag, and exactly what a zoom wants. Hidden panes stay alive (nvim keeps
/// running, the agent session keeps going), and HINT already skips anything unmapped.
pub(crate) struct PaneLayout {
    /// Editor (start) | panel host (end).
    across: Paned,
    /// `across` (start) over the bottom slot (end). Keeps the optional shape used when layouts
    /// could lack a bottom slot. The bottom terminal now makes this `Some` in practice,
    /// matching `build_vertical_split`.
    down: Option<Paned>,
    zoomed: Cell<Option<usize>>,
    /// Both divider positions from just before the zoom, put back by `unzoom`.
    saved: Cell<(i32, Option<i32>)>,
    /// Whether the bottom slot is shown at all, apart from any zoom. False for the built-in
    /// terminal until `Ctrl+a t`; a Lua bottom panel is shown from the
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

/// Whether hiding the bottom slot must first unzoom. When `zoomed == Some(2)`,
/// `hidden_for_zoom(2, ..)` hides only the top row, so hiding the bottom too would leave
/// nothing visible. When pane 0 or 1 is zoomed, `hidden_for_zoom` already sets
/// `bottom: true`, so hiding the bottom changes nothing visible.
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
    ///, yes for a Lua panel.
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

    /// Shows or hides the bottom slot without unrealizing it. The shell keeps running and
    /// a hidden pane keeps its last size because it reports no allocation while hidden.
    /// Showing uses [`shown_position`]; hiding remembers the divider position.
    ///
    /// If the bottom slot is zoomed, unzoom before hiding it ([`hiding_bottom_must_unzoom`]).
    /// `hidden_for_zoom(2, ..)` hides only the top row, so hiding the bottom too would leave
    /// nothing visible. Handle this here so every caller gets the same behavior.
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
    /// before a pane switch and before a resize, as tmux does.
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

/// Wraps the WebView in an overlay to debounce resize reflows. Reallocating it on every
/// `notify::position` change caused high CPU use during divider drags.
///
/// When `agent` was a direct `GtkPaned` child, hiding it during a drag left only one visible child.
/// That child filled the pane and the handle disappeared until the WebView was shown again. Keeping both
/// pane children visible preserves divider geometry throughout the drag.
///
/// The returned `gtk4::Overlay` is the real end child of `Paned`. Its main child is an empty
/// `gtk4::DrawingArea` sensor that receives the slot's full allocation and emits `resize`.
/// `agent` is an overlay child with `halign` and `valign` set to `Start`, so it is placed
/// manually rather than stretched to the slot's size.
///
/// The sensor's `resize` signal drives `agent.set_size_request`, debounced by `DEBOUNCE_MS`:
/// the size is applied about 70ms after the last resize in a burst. Between updates, the
/// WebView keeps its last size and continues painting without reflowing on every motion. The
/// first resize is applied immediately so startup does not leave it at a stale or zero size.
/// The editor is attached directly as the other `Paned` child and receives intermediate resizes
/// immediately.
///
/// `set_clip_overlay` prevents the WebView from spilling into the editor while the slot shrinks
/// and the WebView still has its previous, larger size.
///
/// Returns the overlay upcast to `Widget`. The caller must use it as `Paned`'s end child;
/// passing `agent` directly would bypass the debounce.
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

    /// Hiding the bottom while `zoomed == Some(2)` must unzoom first.
    /// `hidden_for_zoom(2, true)` hides the top row; hiding the bottom too leaves nothing
    /// visible. Enforce this in `set_bottom_shown` even when the caller has not unzoomed.
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
