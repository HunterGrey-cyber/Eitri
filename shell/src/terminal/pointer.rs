//! Pointer geometry for the bottom terminal (bottom-terminal phase 3b, Task 8): turning a widget-
//! relative pixel and a click count into what [`eitri_terminal::SelectCommand`] needs. GTK-free
//! (no `gtk4`/`gdk` type anywhere in this file), so it is tested without a display, the same as
//! `crate::wheel_zoom`.
//!
//! Task 9 (mouse reporting) reuses [`cell_at`] for the same reason: a program's own mouse mode
//! needs the identical pixel -> cell mapping this selection code does.

use eitri_terminal::mouse::{Button, MouseInput, MouseKind, MouseModes, MouseMods};
use eitri_terminal::SelectKind;

/// One pixel axis (x or y) turned into a clamped cell index, and whether the point falls in that
/// cell's right (or bottom) half. A point at or before `0.0` is the left/top half of cell `0`; one
/// at or past the last cell's own right/bottom edge is the right/bottom half of the last cell --
/// never an index past the grid, and never a half-flag read off a position that was actually
/// clamped away.
fn axis_cell(pos: f64, cell: f64, count: u16) -> (u16, bool) {
    let last = count.saturating_sub(1);
    if count == 0 || cell <= 0.0 || !pos.is_finite() {
        return (0, false);
    }
    if pos <= 0.0 {
        return (0, false);
    }
    let idx_f = (pos / cell).floor();
    if idx_f >= f64::from(count) {
        return (last, true);
    }
    let idx = idx_f as u16;
    let frac = pos / cell - idx_f;
    (idx.min(last), frac >= 0.5)
}

/// A widget-relative pixel `(x, y)` -> `(absolute grid line, column, right half)`. `top_line` is
/// [`eitri_terminal::Screen::top_line`] -- the same absolute-line space
/// [`eitri_terminal::SelectCommand`] and `eitri_terminal::scroll::ScrollView` already use, so a
/// drag made while scrolled back names the same cells the render it was made against showed.
/// Clamped to the grid on every edge: a point outside the pane (a drag that leaves the widget while
/// the button is still down, which GTK's `GestureDrag` keeps delivering) never produces a line or
/// column outside `0..rows`/`0..cols`.
pub(crate) fn cell_at(
    x: f64,
    y: f64,
    cell_w: f64,
    cell_h: f64,
    cols: u16,
    rows: u16,
    top_line: i32,
) -> (i32, u16, bool) {
    let (col, right_half) = axis_cell(x, cell_w, cols);
    let (row, _) = axis_cell(y, cell_h, rows);
    (top_line.saturating_add(i32::from(row)), col, right_half)
}

/// What a click starts, from GTK's own click count (`GestureClick`/`GestureDrag`'s `n_press`):
/// alacritty's and foot's own counting -- one press a plain selection, two a word, three or more a
/// whole line.
pub(crate) fn kind_for(n_press: i32) -> SelectKind {
    match n_press {
        n if n <= 1 => SelectKind::Simple,
        2 => SelectKind::Word,
        _ => SelectKind::Line,
    }
}

/// Where a gesture belongs (bottom-terminal phase 3c, Task 9): the program (a report) or this
/// pane's own handling (a selection, or the scrollback a plain wheel would otherwise reach).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerRoute {
    Program,
    Local,
}

/// A press, release or drag motion: `Shift` always selects locally (foot's, alacritty's and kitty's
/// own convention, kept even while a mouse mode is on), otherwise the program gets it iff its own
/// mode asks to be told (`modes.report`, any of 1000/1002/1003).
pub(crate) fn route(shift: bool, modes: MouseModes) -> PointerRoute {
    if shift {
        PointerRoute::Local
    } else if modes.report {
        PointerRoute::Program
    } else {
        PointerRoute::Local
    }
}

/// Where a wheel notch belongs: the program (a report), this pane's own arrow-key alternate scroll
/// (`less`/`man` in the alternate screen, no mouse mode of its own), or Task 7's scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WheelRoute {
    Report,
    Arrows,
    Scrollback,
}

/// **Shift+wheel never sends arrows**: alacritty's own `scroll_terminal` requires `!shift` for
/// alternate scroll (Review Focus 6), so a `Shift`-held wheel always reaches the pane's own
/// scrollback, matching the plain-selection rule above.
pub(crate) fn wheel_route(shift: bool, modes: MouseModes) -> WheelRoute {
    match route(shift, modes) {
        PointerRoute::Program => WheelRoute::Report,
        PointerRoute::Local if !shift && modes.alt_screen && modes.alternate_scroll => WheelRoute::Arrows,
        PointerRoute::Local => WheelRoute::Scrollback,
    }
}

/// Turns a raw gesture into the `MouseInput` a program's own mouse mode asks for (bottom-terminal
/// phase 3c): which button (if any) is held through a drag or release, and whether a motion is new
/// enough to report -- alacritty dedupes motion by CELL, not by pixel, so a slow drag inside one
/// cell reports nothing until the cursor actually crosses into the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ReportTracker {
    held: Option<Button>,
    last: Option<(i32, u16)>,
}

impl ReportTracker {
    /// A press at `cell` (absolute line, column): remembers the button and the cell, for
    /// [`Self::motion`]'s own dedupe and [`Self::release`]'s own button. `None`: `cell` is a
    /// scrollback line (alacritty: "Assure the mouse point is not in the scrollback") -- the button
    /// and cell are still remembered, so a release or a later motion on the program's own screen
    /// still knows which button this gesture is holding.
    pub(crate) fn press(&mut self, button: Button, cell: (i32, u16)) -> Option<MouseInput> {
        self.held = Some(button);
        self.last = Some(cell);
        mouse_input(MouseKind::Press(button), cell)
    }

    /// The button let go: `None` if nothing was held (a release with no matching press -- a
    /// gesture that started before the mode turned on, or a stray event) or `cell` is scrollback.
    pub(crate) fn release(&mut self, cell: (i32, u16)) -> Option<MouseInput> {
        let button = self.held.take()?;
        self.last = None;
        mouse_input(MouseKind::Release(button), cell)
    }

    /// `Some` only when `cell` differs from the last cell reported (a press, a release or a prior
    /// motion) -- alacritty's own dedupe -- and is not scrollback. The held button, if any, rides
    /// along; `encode_mouse` is the one thing that decides whether the live mode (1002 vs 1003)
    /// actually wants it.
    pub(crate) fn motion(&mut self, cell: (i32, u16)) -> Option<MouseInput> {
        if self.last == Some(cell) {
            return None;
        }
        self.last = Some(cell);
        mouse_input(MouseKind::Motion(self.held), cell)
    }

    /// Whether a button is currently tracked as held (a press this tracker actually reported).
    /// **Task 8's fix (codex p1 #4) reads this, not [`route`], to decide whether a release should
    /// report:** xterm's own convention is that a release follows the press it matches, regardless
    /// of the Shift state sampled at release. `route` re-derives its answer from *current* Shift
    /// with no memory of press time, so `pane.rs::connect_drag_end` used to gate its release report
    /// on `route(shift, modes) == Program` -- correct for the common case, but a release with Shift
    /// held only at that moment made `route` say `Local`, so `release` was never called and `held`
    /// stayed stranded (the next ordinary motion then reported a phantom drag). `connect_drag_end`
    /// now checks `is_held()` before calling and releases unconditionally, the same way
    /// `connect_cancel` already did (its own doc: "safe to call unconditionally").
    pub(crate) fn is_held(&self) -> bool {
        self.held.is_some()
    }
}

/// `kind` at `cell` (absolute line, column), turned into the `MouseInput` a report names. `None`:
/// `cell`'s line is scrollback (negative -- alacritty: "Assure the mouse point is not in the
/// scrollback"). Used by [`ReportTracker`]'s own three methods, and directly by a wheel report
/// (`pane.rs::report_wheel`), which needs no dedupe or held-button state of its own.
pub(crate) fn mouse_input(kind: MouseKind, (line, col): (i32, u16)) -> Option<MouseInput> {
    let line = u16::try_from(line).ok()?;
    Some(MouseInput {
        kind,
        line,
        col,
        mods: MouseMods::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CW: f64 = 9.0;
    const CH: f64 = 18.0;
    const COLS: u16 = 20;
    const ROWS: u16 = 5;

    #[test]
    fn the_top_left_corner_is_cell_zero_zero_left_half() {
        assert_eq!(cell_at(0.0, 0.0, CW, CH, COLS, ROWS, 0), (0, 0, false));
    }

    #[test]
    fn a_negative_position_clamps_to_the_left_top_edge() {
        assert_eq!(cell_at(-50.0, -50.0, CW, CH, COLS, ROWS, 0), (0, 0, false));
    }

    #[test]
    fn a_point_past_the_last_column_clamps_there_as_its_right_half() {
        let past_right = f64::from(COLS) * CW + 40.0;
        let (line, col, right_half) = cell_at(past_right, 0.0, CW, CH, COLS, ROWS, 0);
        assert_eq!((line, col), (0, COLS - 1));
        assert!(
            right_half,
            "past the grid's own right edge is the right half of the last cell"
        );
    }

    #[test]
    fn a_point_past_the_last_row_clamps_to_the_bottom_row() {
        let past_bottom = f64::from(ROWS) * CH + 40.0;
        let (line, col, _) = cell_at(0.0, past_bottom, CW, CH, COLS, ROWS, 0);
        assert_eq!((line, col), (i32::from(ROWS) - 1, 0));
    }

    #[test]
    fn the_right_half_flag_flips_exactly_at_the_cells_midpoint() {
        let col = 3.0;
        let just_left = col * CW + CW * 0.5 - 0.01;
        let at_or_past_half = col * CW + CW * 0.5;
        assert_eq!(cell_at(just_left, 0.0, CW, CH, COLS, ROWS, 0), (0, 3, false));
        assert_eq!(cell_at(at_or_past_half, 0.0, CW, CH, COLS, ROWS, 0), (0, 3, true));
    }

    #[test]
    fn top_line_is_added_to_the_row_for_a_scrolled_back_window() {
        // Row 2 of a window pinned 10 lines into history is absolute line -10 + 2 = -8.
        let (line, _, _) = cell_at(0.0, 2.0 * CH + 1.0, CW, CH, COLS, ROWS, -10);
        assert_eq!(line, -8);
    }

    #[test]
    fn click_counting_matches_alacrittys_own() {
        assert_eq!(kind_for(1), SelectKind::Simple);
        assert_eq!(kind_for(2), SelectKind::Word);
        assert_eq!(kind_for(3), SelectKind::Line);
        assert_eq!(kind_for(4), SelectKind::Line);
    }

    fn modes(report: bool, alt_screen: bool, alternate_scroll: bool) -> MouseModes {
        MouseModes {
            report,
            alt_screen,
            alternate_scroll,
        }
    }

    #[test]
    fn shift_always_selects_locally_report_on_or_off() {
        assert_eq!(route(true, modes(true, false, false)), PointerRoute::Local);
        assert_eq!(route(true, modes(false, false, false)), PointerRoute::Local);
    }

    #[test]
    fn no_report_is_local_report_is_the_program() {
        assert_eq!(route(false, modes(false, false, false)), PointerRoute::Local);
        assert_eq!(route(false, modes(true, false, false)), PointerRoute::Program);
    }

    #[test]
    fn wheel_route_reports_when_the_program_asks() {
        assert_eq!(wheel_route(false, modes(true, true, true)), WheelRoute::Report);
    }

    #[test]
    fn wheel_route_is_arrows_only_in_the_alternate_screen_with_no_shift() {
        assert_eq!(wheel_route(false, modes(false, true, true)), WheelRoute::Arrows);
        assert_eq!(
            wheel_route(true, modes(false, true, true)),
            WheelRoute::Scrollback,
            "Shift+wheel never sends arrows"
        );
        assert_eq!(
            wheel_route(false, modes(false, true, false)),
            WheelRoute::Scrollback,
            "alternate_scroll (1007) is off"
        );
        assert_eq!(
            wheel_route(false, modes(false, false, true)),
            WheelRoute::Scrollback,
            "not the alternate screen"
        );
    }

    #[test]
    fn report_tracker_dedupes_motion_by_cell_and_carries_the_held_button() {
        let mut tracker = ReportTracker::default();
        let press = tracker.press(Button::Left, (2, 4)).unwrap();
        assert_eq!(press.kind, MouseKind::Press(Button::Left));
        assert_eq!((press.line, press.col), (2, 4));
        assert_eq!(tracker.motion((2, 4)), None, "same cell as the press: no report");
        let moved = tracker.motion((2, 5)).unwrap();
        assert_eq!(moved.kind, MouseKind::Motion(Some(Button::Left)));
        assert_eq!((moved.line, moved.col), (2, 5));
        let release = tracker.release((2, 5)).unwrap();
        assert_eq!(release.kind, MouseKind::Release(Button::Left));
    }

    #[test]
    fn report_tracker_release_with_no_press_is_none() {
        let mut tracker = ReportTracker::default();
        assert_eq!(tracker.release((0, 0)), None);
    }

    #[test]
    fn report_tracker_motion_with_no_button_carries_none() {
        let mut tracker = ReportTracker::default();
        let moved = tracker.motion((1, 1)).unwrap();
        assert_eq!(moved.kind, MouseKind::Motion(None));
    }

    #[test]
    fn a_scrollback_cell_never_produces_a_report() {
        let mut tracker = ReportTracker::default();
        assert_eq!(tracker.press(Button::Left, (-3, 0)), None, "a history line");
        // The button is still remembered even though the press itself was not reported.
        let moved = tracker.motion((0, 0));
        assert_eq!(moved.unwrap().kind, MouseKind::Motion(Some(Button::Left)));
    }

    #[test]
    fn is_held_reports_whether_a_button_is_tracked() {
        let mut tracker = ReportTracker::default();
        assert!(!tracker.is_held());
        tracker.press(Button::Left, (0, 0));
        assert!(tracker.is_held());
        tracker.release((0, 0));
        assert!(!tracker.is_held());
    }

    /// Reproduces Task 8's finding (codex p1 #4): a press with no Shift routes to the program and
    /// the tracker starts holding the button; if Shift is held only at release, `route()` alone
    /// says `Local` for that moment. Gating the release report on `route()` there (the old
    /// `pane.rs::connect_drag_end`, `if route(shift, modes) == Program { report_release(..) }`)
    /// skips `ReportTracker::release` entirely and strands `held` -- so an ordinary hover motion
    /// right after reports a phantom left-button drag. The fix reads `is_held()` instead and calls
    /// release unconditionally once it knows a button is tracked (xterm's own convention: the
    /// release follows the press, not the Shift state sampled at release). This test pins that
    /// corrected decision at the `ReportTracker`/`route()` level, which is what `pane.rs` calls
    /// through a GTK gesture this crate cannot unit-test without a display.
    #[test]
    fn a_release_reports_even_when_shift_is_held_only_at_release() {
        let mut tracker = ReportTracker::default();
        let live_modes = modes(true, false, false); // the program has asked for reports (DECSET 1002)

        // Press with no Shift: routed to the program, the tracker starts holding the button --
        // mirrors `pane.rs::connect_drag_begin` calling `report_press`.
        assert_eq!(route(false, live_modes), PointerRoute::Program);
        let press = tracker.press(Button::Left, (2, 4)).unwrap();
        assert_eq!(press.kind, MouseKind::Press(Button::Left));
        assert!(tracker.is_held());

        // At release, Shift is held: `route()` alone says Local -- the old, buggy gate would have
        // skipped the release report here.
        assert_eq!(route(true, live_modes), PointerRoute::Local);

        // The fix: decide from `is_held()`, not `route()`, and release unconditionally once a
        // button really is tracked.
        assert!(
            tracker.is_held(),
            "a real press was reported, so the release must be too"
        );
        let release = tracker.release((2, 4)).unwrap();
        assert_eq!(release.kind, MouseKind::Release(Button::Left));
        assert!(
            !tracker.is_held(),
            "the tracker must heal: nothing left held after the release"
        );

        // An ordinary hover motion with no button held must report Motion(None), never a phantom
        // Motion(Some(Left)) drag -- the symptom the finding named ("ordinary pointer movement sends
        // left-button drag reports").
        let moved = tracker.motion((3, 4)).unwrap();
        assert_eq!(moved.kind, MouseKind::Motion(None));
    }
}
