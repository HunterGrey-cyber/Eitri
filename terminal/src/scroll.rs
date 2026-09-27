//! Scrollback: a read-only pan over history, over `terminal-frame::viewport::RawViewport` -- the
//! anchor-by-ordinal model `terminal-frame`'s own module docs argue for, not by offset. Nothing here
//! reimplements that argument; this module is only the piece `RawViewport` leaves to its caller: what
//! a host gesture (`Shift+PageUp`, a wheel notch, `Shift+End`) means as a movement, and how far that
//! movement clamps against the terminal's own retained history.
//!
//! Spec `docs/superpowers/specs/2026-09-23-bottom-terminal-design.md` §"Phase 3: reading back";
//! owner ruling R6 (`docs/superpowers/plans/2026-09-26-wave4.md`).

use alacritty_terminal::grid::Dimensions;
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
    /// (Task 9, spec §6.2, tmux `page-up`/`page-down`).
    Pages(i32),
    /// Move by half a screen (`rows / 2` lines, at least one) -- copy mode's `Ctrl+u`/`Ctrl+d`
    /// (Task 9, spec §6.2, tmux `halfpage-up`/`halfpage-down`). No other caller sends this yet.
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
}

impl ScrollView {
    pub fn new() -> Self {
        Self {
            viewport: RawViewport::new(),
        }
    }

    /// Applies one gesture: pins on the first move up, clamps at the oldest retained line, and
    /// unpins once the window would reach or pass the live bottom. A no-op in the alternate screen
    /// (`max_scrollback == 0` -- alacritty keeps no history there); Task 9 turns the wheel into
    /// arrow keys for a program that wants it instead.
    pub fn scroll<T>(&mut self, term: &Term<T>, req: ScrollRequest) {
        if req == ScrollRequest::Bottom {
            self.viewport.unpin();
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
        let current_top = self.viewport.top_line(term).unwrap_or(0);
        let max = i32::try_from(max).unwrap_or(i32::MAX);
        let target = current_top.saturating_sub(delta).clamp(-max, 0);
        if target >= 0 {
            self.viewport.unpin();
        } else {
            self.viewport.pin(term, target);
        }
    }

    /// Re-aligns the held anchor against the grid's current contents. Call once per [`crate::screen::Screen::feed`]
    /// -- output and resize both shift what history retains, and an un-pinned view has nothing to
    /// align (`RawViewport::observe` is then a no-op).
    pub fn observe<T>(&mut self, term: &Term<T>) {
        self.viewport.observe(term);
    }

    /// Where the window is now: the absolute top line [`terminal_frame::viewport::project_window`]
    /// needs, and which [`ViewMode`] `terminal-render` should draw it with. `AnchorExpired`'s top
    /// line is `0` -- the live bottom -- because that mode shows the live screen with a notice over
    /// it, never a stale position.
    pub fn window<T>(&self, term: &Term<T>) -> (i32, ViewMode) {
        if !self.viewport.is_pinned() {
            return (0, ViewMode::FollowBottom);
        }
        match self.viewport.top_line(term) {
            Some(top) => (top, ViewMode::Pinned),
            None => (0, ViewMode::AnchorExpired),
        }
    }

    /// tmux copy-mode's `[N/M]` (owner ruling R6): `N` lines are hidden below the window's live
    /// bottom, of `M` retained in history. `None` while following the bottom, or once the pinned
    /// anchor is gone -- `terminal-render`'s own `AnchorExpired` notice already says so, and this is
    /// not a second announcement of the same fact.
    pub fn indicator<T>(&self, term: &Term<T>) -> Option<(usize, usize)> {
        if !self.viewport.is_pinned() {
            return None;
        }
        let top = self.viewport.top_line(term)?;
        Some(((-top) as usize, max_scrollback(term)))
    }

    /// Whether the view is anywhere but the live bottom.
    pub fn scrolled(&self) -> bool {
        self.viewport.is_pinned()
    }
}
