//! Scrollback: a read-only pan over history, over `terminal-frame::viewport::RawViewport` -- the
//! anchor-by-ordinal model `terminal-frame`'s own module docs argue for, not by offset. Nothing here
//! reimplements that argument; this module is only the piece `RawViewport` leaves to its caller: what
//! a host gesture (`Shift+PageUp`, a wheel notch, `Shift+End`) means as a movement, and how far that
//! movement clamps against the terminal's own retained history.
//!
//! The gestures follow tmux copy mode, foot and alacritty, the tools a terminal user already has in
//! their hands: the `[N/M]` indicator, three lines a wheel notch, and the page keys.
//!
//! `RawViewport` holds a logical line, and a line that wraps fills several rows; the window's top can
//! be any of them. So the view keeps, beside the anchor, how far into that line the top is, in
//! cells: a move then goes exactly as many rows as asked, also through a line longer than the
//! screen, and a column resize that re-folds the line keeps the same text at the top.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;

use terminal_frame::viewport::{max_scrollback, RawViewport};
use terminal_render::ViewMode;

/// One scroll gesture from the host. Positive is UP, into history -- the same sign
/// `RawViewport::pin`'s absolute grid lines use once negated (line 0 is the live top, more negative
/// is further back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRequest {
    /// Move the window this many grid lines.
    Lines(i32),
    /// Move by whole screens (`rows` lines each) -- `Shift+PageUp`/`Shift+PageDown`, matching
    /// alacritty's own `ScrollPageUp`/`ScrollPageDown` (`screen_lines`) and foot's
    /// `scrollback-up-page`/`scrollback-down-page`; also copy mode's own `PageUp`/`PageDown`
    /// (tmux `page-up`/`page-down`).
    Pages(i32),
    /// Move by half a screen (`rows / 2` lines, at least one) -- copy mode's `Ctrl+u`/`Ctrl+d`
    /// (tmux `halfpage-up`/`halfpage-down`). No other caller sends this yet.
    HalfPages(i32),
    /// Return to the live bottom and end the pin -- `Shift+End`, foot's `scrollback-end`, and any
    /// typed key ([`crate::screen::Screen::note_input`]).
    Bottom,
}

/// The scrolled-back view over one terminal's history: how far back the window is, and what
/// [`terminal_render::ViewMode`] to render it with. Holds no borrow on `Term` -- every method takes
/// one, the same shape as `RawViewport` itself.
#[derive(Debug, Default)]
pub struct ScrollView {
    viewport: RawViewport,
    /// How far into the anchored logical line the window's top is: the rows above it within that
    /// line, times the width they had. `0` with no pin.
    into: usize,
    /// The anchored line's rows and the width, as of the last pin or observation: a line at the
    /// head of a full history loses rows from its top as output evicts them, and `into` counts from
    /// the row that was its top.
    anchor_rows: usize,
    anchor_cols: usize,
}

impl ScrollView {
    pub fn new() -> Self {
        Self {
            viewport: RawViewport::new(),
            into: 0,
            anchor_rows: 0,
            anchor_cols: 0,
        }
    }

    /// Records the anchored line's rows at the current width, for the next [`Self::observe`].
    fn note_anchor<T>(&mut self, term: &Term<T>) {
        if let Some(start) = self.viewport.top_line(term) {
            self.anchor_rows = rows_of_line(term, start);
            self.anchor_cols = term.columns();
        }
    }

    /// The window's absolute top line while pinned: the anchored line's first row plus the rows
    /// `into` names at the current width, never past the line's last row or the live top.
    fn top<T>(&self, term: &Term<T>) -> Option<i32> {
        let start = self.viewport.top_line(term)?;
        let cols = term.columns().max(1);
        let last_row = rows_of_line(term, start) - 1;
        let into = i32::try_from((self.into / cols).min(last_row)).unwrap_or(i32::MAX);
        Some(start.saturating_add(into).min(0))
    }

    /// Applies one gesture: pins on the first move up, clamps at the oldest retained line, and
    /// unpins once the window would reach or pass the live bottom. A no-op in the alternate screen
    /// (`max_scrollback == 0` -- alacritty keeps no history there); the host turns the wheel into
    /// arrow keys for a program that wants it instead.
    pub fn scroll<T>(&mut self, term: &Term<T>, req: ScrollRequest) {
        if req == ScrollRequest::Bottom {
            self.viewport.unpin();
            self.into = 0;
            return;
        }
        let max = max_scrollback(term);
        if max == 0 {
            return;
        }
        let rows = i32::try_from(term.screen_lines()).unwrap_or(i32::MAX);
        let delta = match req {
            ScrollRequest::Lines(n) => n,
            ScrollRequest::Pages(n) => n.saturating_mul(rows),
            ScrollRequest::HalfPages(n) => n.saturating_mul((rows / 2).max(1)),
            ScrollRequest::Bottom => unreachable!("handled above"),
        };
        // `unwrap_or(0)`: a request that arrives after the anchor was declared Gone starts a fresh
        // pin from the live bottom rather than being stuck -- the same recovery `ScrollRequest::Bottom`
        // gives explicitly, reached here for a plain `Lines`/`Pages` gesture too.
        let current_top = self.top(term).unwrap_or(0);
        let max = i32::try_from(max).unwrap_or(i32::MAX);
        let target = current_top.saturating_sub(delta).clamp(-max, 0);
        if target >= 0 {
            self.viewport.unpin();
            self.into = 0;
        } else {
            self.viewport.pin(term, target);
            let start = self.viewport.top_line(term).unwrap_or(target);
            self.into = usize::try_from(target - start).unwrap_or(0) * term.columns();
            self.note_anchor(term);
        }
    }

    /// Re-aligns the held anchor against the grid's current contents. Call once per [`crate::screen::Screen::feed`]
    /// -- output and resize both shift what history retains, and an un-pinned view has nothing to
    /// align (`RawViewport::observe` is then a no-op).
    ///
    /// A full history evicts rows, not lines: the anchored line, once it is the oldest retained, can
    /// lose rows from its top while it keeps its anchor. `into` then drops the cells those rows held,
    /// so the same text stays at the window's top. At a changed width the row count changed by
    /// re-folding as well, and the two cannot be told apart, so only an unchanged width adjusts.
    pub fn observe<T>(&mut self, term: &Term<T>) {
        if !self.viewport.is_pinned() {
            return;
        }
        let (rows_before, cols_before) = (self.anchor_rows, self.anchor_cols);
        self.viewport.observe(term);
        let Some(start) = self.viewport.top_line(term) else {
            return;
        };
        let rows = rows_of_line(term, start);
        let cols = term.columns();
        let oldest = -i32::try_from(term.grid().history_size()).unwrap_or(i32::MAX);
        if start == oldest && cols == cols_before && rows < rows_before {
            self.into = self.into.saturating_sub((rows_before - rows) * cols);
        }
        self.anchor_rows = rows;
        self.anchor_cols = cols;
    }

    /// Where the window is now: the absolute top line [`terminal_frame::viewport::project_window`]
    /// needs, and which [`ViewMode`] `terminal-render` should draw it with. `AnchorExpired`'s top
    /// line is `0` -- the live bottom -- because that mode shows the live screen with a notice over
    /// it, never a stale position.
    pub fn window<T>(&self, term: &Term<T>) -> (i32, ViewMode) {
        if !self.viewport.is_pinned() {
            return (0, ViewMode::FollowBottom);
        }
        match self.top(term) {
            Some(top) => (top, ViewMode::Pinned),
            None => (0, ViewMode::AnchorExpired),
        }
    }

    /// tmux copy-mode's `[N/M]`: `N` lines are hidden below the window's live
    /// bottom, of `M` retained in history. `None` while following the bottom, or once the pinned
    /// anchor is gone -- `terminal-render`'s own `AnchorExpired` notice already says so, and this is
    /// not a second announcement of the same fact.
    pub fn indicator<T>(&self, term: &Term<T>) -> Option<(usize, usize)> {
        if !self.viewport.is_pinned() {
            return None;
        }
        let top = self.top(term)?;
        Some(((-top) as usize, max_scrollback(term)))
    }

    /// Whether the view is anywhere but the live bottom.
    pub fn scrolled(&self) -> bool {
        self.viewport.is_pinned()
    }
}

/// How many rows the logical line starting at absolute `start` fills: alacritty marks a row that
/// continues into the next on its last cell (the flag its own reflow reads), and the live bottom row
/// ends any line still open there.
fn rows_of_line<T>(term: &Term<T>, start: i32) -> usize {
    let grid = term.grid();
    let Some(last) = grid.columns().checked_sub(1) else {
        return 1;
    };
    let bottom = i32::try_from(grid.screen_lines()).unwrap_or(i32::MAX) - 1;
    let mut line = start;
    while line < bottom
        && grid[Line(line).grid_clamp(grid, Boundary::Grid)][Column(last)]
            .flags
            .contains(Flags::WRAPLINE)
    {
        line += 1;
    }
    usize::try_from(line - start).unwrap_or(0) + 1
}
