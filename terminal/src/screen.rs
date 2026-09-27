//! Bytes in, a `PaintList` out, and the answers the terminal owes the program.
//!
//! The frozen `terminal-pane/src/live.rs` (`freeze/terminal-stack` @ `1e715ab`) with three changes,
//! each measured by the spike (`.superpowers/terminal/spike-report.md`):
//! - **feed and render are split.** The frozen driver built a whole `PaintList` on every PTY read,
//!   which ran 30-100x slower than the source under a noisy stream (15 MB of `yes`: 44 s against
//!   1.5 s). [`Screen::feed`] only parses and marks dirty; the session renders at most once a frame.
//! - **a real listener.** Replies go back to the program ([`Screen::take_replies`]).
//! - **scrollback exists.** `Config::scrolling_history` is 10,000 lines (alacritty's default); the
//!   frozen comment calling it "deliberately zero" was wrong, since `Term` never read history from
//!   `total_lines`. Nothing shows it yet -- the view is always `FollowBottom` until phase 3.
//!
//! **A synchronized update that never ends is the session's to end.** `terminal-sync` publishes
//! nothing between `ESC[?2026h` and `ESC[?2026l`, and ships no deadline on purpose: its
//! `SyncBarrier::abort_sync` doc puts that policy in the engine's event loop, where alacritty's own
//! 150 ms lives (`vte-0.15.0` `SYNC_UPDATE_TIMEOUT`). [`Screen::open_update`] says an update is
//! open and which one; the session aborts it with [`Screen::abort_sync`] once it has been open too
//! long (review 2026-09-23, finding 2). A RIS (`ESC c`) does NOT end an open update -- the barrier
//! counts it as one more dispatch -- so after a killed nvim, a `reset` is not what brings the
//! screen back; the deadline is.
//!
//! **`render` never projects the live `Term` while an update is open (review 2026-09-23, finding
//! 1; redesigned in the 2026-09-23 re-review, same finding number, because the first fix's own
//! design was itself an Important finding of that re-review).** The first fix snapshotted
//! `self.frame` inside every `publish` closure -- on every `FrameComplete`, every end-of-read
//! `Wakeup`, and every `SyncAborted` -- which paid a full O(rows x cols) [`Projector::full`] on
//! every PTY read that touched anything, whether or not a synchronized update was ever in play:
//! measured 2.6-7.6x slower on a plain flood with no BSU/ESU anywhere, and paying the cost while
//! hidden, which the session's render clock (Task 5) is meant to rule out.
//!
//! The fix keeps the guarantee but ties the cost to what actually needs it. Two fixed points:
//! - **Outside an open update, the live `Term` IS the last publication.** `SyncBarrier::end_sync`
//!   and `abort_sync` both clear `in_sync` unconditionally, so `open_update()` reporting `None`
//!   means `Term` is at a real, complete, presentable state -- there is nothing left to snapshot
//!   for, and [`Screen::render`] projects the live `Term` directly, right there, at most once per
//!   call (bounded by however often the caller calls it -- the render clock, not the PTY's).
//! - **A synchronized update cannot mutate `Term` before its own `BSU` is dispatched.** So the one
//!   moment a snapshot is structurally necessary is the instant `in_sync` flips false -> true:
//!   [`Screen::feed`] steps `terminal-sync::SyncDriver::feed` one byte at a time (which is what it
//!   already does internally -- see its own "WHY BYTE AT A TIME" doc -- so this adds no real
//!   iteration cost, only moves the loop boundary) and takes `self.frame = self.projector.full(term)`
//!   exactly at that transition, before any byte of the new update's content can hide behind it.
//!   Everything published *after* that -- `FrameComplete`, `Wakeup`, `SyncAborted` -- needs no
//!   snapshot of its own: either it closes the update, in which case the next `render()` call
//!   reads the live `Term` per the point above, or it is itself a transition into a *further*
//!   nested/new update, in which case it is captured by that transition instead.
//!
//! `render_never_shows_a_frame_torn_by_a_later_update_in_the_same_read` (the re-review's own
//! repro, `BSU CUP ED "frame\r\nplus1" ESU BSU CUP ED "HEL"` in one `feed()` call, never rendered
//! in between) still passes under this design: the second `BSU`'s false->true transition captures
//! `self.frame` at exactly the moment `Term` shows "frame"/"plus1" and nothing of "HEL" yet.
//! `resize` no longer snapshots at all -- it used to, conditionally, but `render`'s own check now
//! subsumes that, and subsumes a bug the re-review found in the old conditional: resizing *inside*
//! an update that goes on to publish nothing (an ESU or abort with no dispatch in between, so
//! `SyncBarrier`'s own `dirty` flag never latches) used to leave `render` reporting the *old* grid
//! size forever, because nothing about that resize ever triggered `feed`'s or `abort_sync`'s old
//! per-publish snapshot. `Term::resize` unconditionally clears `in_sync` on the barrier by ending
//! up at `open_update() == None` the moment the update actually ends (`end_sync`/`abort_sync` flip
//! `in_sync` regardless of `dirty`), so the very next `render()` call reads the live, correctly
//! resized `Term` -- no special-casing needed in `resize` at all (re-review finding 2, minor).
//! This is still `Projector::full`'s own documented safety property doing the work: it "does
//! **not** touch damage, so it is safe to call for a newly attached consumer at any moment" -- an
//! extra full snapshot, wherever one is taken, is not the delta-stream-stealing hazard
//! `Projector::next` guards against, since `Screen` never calls `next`.

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::Rgb;
use terminal_frame::frame::{ColorOverride, Rgb as FrameRgb, PALETTE_LEN};
use terminal_frame::viewport::{max_scrollback, project_window};
use terminal_frame::{Projector, TerminalFrame};
use terminal_input::NormalizedInput;
use terminal_render::color::{BACKGROUND, BRIGHT_FOREGROUND, CURSOR, DIM_FOREGROUND, FOREGROUND};
use terminal_render::{
    build_paint_list, CursorColoring, PaintList, Palette, RenderInput, RgbColor, SelectionSpan, ViewMode,
};
use terminal_sync::SyncDriver;

use crate::listener::{HostEvents, Listener, Reply};
use crate::paint::indicator_ops;
use crate::pty::PtySize;
use crate::scroll::{ScrollRequest, ScrollView};
use crate::select::{self, SelectCommand};

/// The colours phase 1 takes from the window's theme (spec §Theming): so a light colorscheme does
/// not get a black box under it. The sixteen ANSI colours stay xterm's until phase 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalColors {
    pub background: RgbColor,
    pub foreground: RgbColor,
    /// A fixed cursor colour, or `None` for none: the cursor then takes the colours of the cell it
    /// covers, swapped, as foot and alacritty do when none is configured (GUI pass 2026-09-23,
    /// defect 3 -- the theme's foreground as a fixed colour vanished on a cell a program painted
    /// light). A program's own OSC 12 wins over either.
    pub cursor: Option<RgbColor>,
}

impl Default for TerminalColors {
    /// `Palette::xterm_default`'s own three, so an unthemed screen is exactly what it always was.
    fn default() -> Self {
        let xterm = Palette::xterm_default();
        TerminalColors {
            background: xterm.get(BACKGROUND),
            foreground: xterm.get(FOREGROUND),
            cursor: Some(xterm.get(CURSOR)),
        }
    }
}

impl TerminalColors {
    /// xterm's table with these three put in, plus the two slots derived from them. With no fixed
    /// cursor colour, the `CURSOR` slot holds the foreground: it is what an OSC 12 query is
    /// answered with, and the cursor's own colour comes from the cell ([`Self::cursor_coloring`]).
    /// `BRIGHT_FOREGROUND` is xterm's white by default -- bold default-coloured text on a LIGHT
    /// background would be white on white -- so it follows the foreground, as alacritty's own does
    /// when `bright_foreground` is unset. `DIM_FOREGROUND` is two thirds of the way from the
    /// background to the foreground, so dim text recedes on a light theme as on a dark one.
    /// Alacritty's own rule (the foreground times 2/3) darkens, which on a light background makes
    /// SGR 2 text *stronger* than normal text -- the exit notice is drawn dim.
    pub fn palette(&self) -> Palette {
        let fg = self.foreground;
        let bg = self.background;
        let mix = |f: u8, b: u8| ((u16::from(f) * 2 + u16::from(b)) / 3) as u8;
        let dim = RgbColor::new(mix(fg.r, bg.r), mix(fg.g, bg.g), mix(fg.b, bg.b));
        let set = |index: u16, c: RgbColor| ColorOverride {
            index,
            color: Some(FrameRgb { r: c.r, g: c.g, b: c.b }),
        };
        let mut palette = Palette::xterm_default();
        palette.apply_overrides(&[
            set(BACKGROUND, self.background),
            set(FOREGROUND, fg),
            set(CURSOR, self.cursor.unwrap_or(fg)),
            set(BRIGHT_FOREGROUND, fg),
            set(DIM_FOREGROUND, dim),
        ]);
        palette
    }

    /// How the paint list colours the cursor: the fixed colour if there is one, else the cell's.
    pub fn cursor_coloring(&self) -> CursorColoring {
        match self.cursor {
            Some(_) => CursorColoring::Palette,
            None => CursorColoring::CellInverse,
        }
    }
}

/// Where the cursor is on the frame [`Screen::render`] last produced: window cells, the same space
/// as every `PaintOp`. Reported whether or not the program hid the cursor (`ESC[?25l`), because it
/// is still where the next character lands -- which is where the host draws an input method's
/// preedit and places its candidate window (bottom-terminal phase 2). The `PaintList` itself carries
/// the cursor only while it is visible, which is `terminal-render`'s contract and is not changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorCell {
    pub row: u16,
    pub col: u16,
    /// DECTCEM: `false` while the program has hidden the cursor.
    pub visible: bool,
}

/// The `Term` configuration every neovibe terminal runs with. A function, so a test can pin it.
///
/// - `osc52: OnlyCopy` -- a program may SET the clipboard (nvim's `"+y`), never READ it (spec §4.6).
///   `Term` enforces this itself, before any event reaches the listener.
/// - `kitty_keyboard: true` -- the owner's foot speaks it, `terminal-input` encodes it (its
///   `real_app` suite drives nvim with it on), and a program that asks for it should get it.
/// - `scrolling_history: 10_000` -- alacritty's default, kept.
pub fn term_config() -> Config {
    Config {
        osc52: Osc52::OnlyCopy,
        kitty_keyboard: true,
        scrolling_history: 10_000,
        ..Config::default()
    }
}

struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// The one place a [`PtySize`] becomes a [`GridSize`] (review 2026-09-23, finding 5): `new` and
/// `resize` used to repeat both the `.max(1)` (already redundant with [`PtySize::clamped`], which
/// every caller here applies first) and the field-by-field construction.
fn grid_size(size: PtySize) -> GridSize {
    GridSize {
        cols: usize::from(size.cols),
        rows: usize::from(size.rows),
    }
}

/// Clamps a cell dimension so `cells as u32 * cell_px as u32` fits in `u16`, matching what
/// [`crate::pty::PtySize::to_winsize`] already does for `TIOCSWINSZ`. `CSI 14 t`'s own formatter
/// (`alacritty_terminal::Term::text_area_size_pixels`) multiplies `num_lines * cell_height` and
/// `num_cols * cell_width` in plain `u16` -- which panics past 65535 in a debug build and wraps
/// silently in a release one -- and this crate is the only side of that call with a chance to keep
/// the product under the cap before handing it over (spec §5.2; engine review 2026-09-23, minor 5).
/// No real window reaches it, so this is a floor against a pathological one, not a common case.
fn saturate_cell_px(cells: u16, cell_px: u16) -> u16 {
    let cells = u32::from(cells.max(1));
    ((u32::from(u16::MAX) / cells).min(u32::from(cell_px))) as u16
}

/// One terminal's emulator state. Owned by exactly one thread (the session's); nothing here blocks.
pub struct Screen {
    term: Term<Listener>,
    listener: Listener,
    sync: SyncDriver,
    projector: Projector,
    palette: Palette,
    /// From the same [`TerminalColors`] as `palette`: whether the host gave a fixed cursor colour.
    cursor_coloring: CursorColoring,
    size: PtySize,
    dirty: bool,
    /// The frame taken at the last transition into a synchronized update (or the empty grid at
    /// construction). Read by [`Self::render`] only while an update is open AND the view is
    /// following the live bottom -- outside one, `render` projects the live `term` directly instead,
    /// since it is then guaranteed quiescent (re-review 2026-09-23, finding 1).
    frame: TerminalFrame,
    /// How many updates `abort_sync` has forced closed. `SyncBarrier` has no counter of its own
    /// for this (only `esu_count`, bumped by a real ESU); added to `esu_count` in [`Self::open_update`]
    /// so the ordinal changes exactly when an update ENDS, by either path (review 2026-09-23,
    /// finding 2).
    abort_count: u64,
    /// Scrollback (Task 7): how far back the window is, over `term`'s own retained history.
    scroll: ScrollView,
    /// The last window `render` computed while pinned or anchor-expired and no update was open --
    /// [`Self::render`]'s own analogue of `frame` for a scrolled-back view. Read instead of a fresh
    /// `project_window` while an update is open, for the same reason `frame` exists: some of a
    /// pinned window's rows can still be the live screen's own (a pin near the bottom), which a
    /// synchronized update is free to mutate mid-draw.
    pinned_frame: Option<TerminalFrame>,
    /// Set by [`Self::select`] on `SelectCommand::Finish`, and taken by [`Self::take_events`] into
    /// `HostEvents::selection` (Task 8) -- the one selection event is not a `Term` event at all
    /// (nothing in `alacritty_terminal::event::Event` reports "a selection finished"), so it cannot
    /// arrive through [`Listener`] the way title/bell/clipboard do, and needs this second slot
    /// merged in at the one place a caller reads events out.
    pending_selection: Option<String>,
    /// Copy mode (Task 9, spec §6.2): read-only, entered by `prefix [`/`prefix PageUp`. Distinct
    /// from `self.scroll.scrolled()` -- entering shows the `[N/M]` indicator at once, `[0/M]` at the
    /// live bottom, where nothing is pinned yet and the plain scrollback keys (Task 7) show nothing
    /// until the first scroll actually moves the window. See [`Self::copy_mode_indicator`].
    copy_mode: bool,
}

impl Screen {
    pub fn new(size: PtySize, colors: TerminalColors) -> Self {
        let size = size.clamped();
        let listener = Listener::default();
        let term = Term::new(term_config(), &grid_size(size), listener.clone());
        let mut projector = Projector::new();
        let frame = projector.full(&term);
        Screen {
            term,
            listener,
            sync: SyncDriver::new(),
            projector,
            palette: colors.palette(),
            cursor_coloring: colors.cursor_coloring(),
            size,
            dirty: true,
            frame,
            abort_count: 0,
            scroll: ScrollView::new(),
            pinned_frame: None,
            pending_selection: None,
            copy_mode: false,
        }
    }

    /// Parses one chunk of output. Marks the screen dirty when the chunk reached a publication
    /// point (`terminal-sync`'s rule: never mid-`DECSET 2026` update).
    ///
    /// Steps `terminal-sync` one byte at a time (matching what it already does internally -- see
    /// its own "WHY BYTE AT A TIME" doc -- so this adds no real iteration cost) so it can catch the
    /// exact byte where an update opens (`in_sync` false -> true) and snapshot `self.frame` right
    /// there, before any of that update's own content can mutate `term`. That is the only point a
    /// snapshot is structurally required (module doc, finding 1); every other publication in this
    /// call needs none, because `Self::render` reprojects the live `term` itself once no update is
    /// open. A `feed()` call may carry more than one such transition (`BSU ... ESU BSU ...`); each
    /// one replaces `self.frame`, so what is left after the call is the state right before the
    /// *last* update that opened in this read, never that update's own half-drawn content.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut published = false;
        for &byte in bytes {
            let was_in_sync = self.sync.barrier().in_sync();
            self.sync
                .feed(&mut self.term, &[byte], |_term, _reason| published = true);
            if !was_in_sync && self.sync.barrier().in_sync() {
                self.frame = self.projector.full(&self.term);
            }
        }
        self.dirty |= published;
        // Output can evict or reflow what a pinned view holds; re-align once per call, the same
        // cadence `RawViewport::observe`'s own doc asks for ("once per wakeup while pinned"). A
        // no-op while following the bottom (Task 7 design).
        self.scroll.observe(&self.term);
    }

    /// Whether anything visible changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// A grid below one cell is stored as one cell, the size `TIOCSWINSZ` gets too, so a
    /// `CSI 18 t` never answers `0x0`.
    ///
    /// Takes no snapshot itself -- `Term::resize` is geometry, not a parser dispatch, so it cannot
    /// be the transition [`Self::feed`] watches for, and `Self::render` already reprojects the
    /// live `term` (new size included) the next time it is called with no update open. That covers
    /// this even when an update IS open at resize time and goes on to publish nothing of its own
    /// (an ESU/abort with no dispatch in between never latches `SyncBarrier`'s `dirty` flag): both
    /// still clear `in_sync` unconditionally, so `open_update()` still reports `None` once it ends,
    /// and the next `render()` picks up the resize regardless (re-review 2026-09-23, finding 2).
    pub fn resize(&mut self, size: PtySize) {
        let size = size.clamped();
        self.size = size;
        self.term.resize(grid_size(size));
        self.dirty = true;
    }

    /// Something outside the byte stream changed what the frame shows (focus).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// The synchronized update (`ESC[?2026h`) that is holding publication back, if one is open: an
    /// ordinal that changes exactly when an update ENDS (by ESU or by [`Self::abort_sync`]), so a
    /// caller can tell a new update from one that has stayed open. **Not** a count of BSUs seen
    /// (review 2026-09-23, finding 2): `SyncBarrier::begin_sync` is idempotent for a nested BSU
    /// inside an already-open update, so counting BSUs gave a *different* ordinal for the same
    /// still-open update on every re-arm -- `esu_count` (bumped only by a real ESU) plus this
    /// screen's own count of forced aborts does not.
    pub fn open_update(&self) -> Option<u64> {
        let barrier = self.sync.barrier();
        barrier.in_sync().then(|| barrier.esu_count() + self.abort_count)
    }

    /// Ends an open synchronized update as if its `ESC[?2026l` had arrived, and marks the screen
    /// dirty if anything was drawn inside it. A no-op when none is open. Takes no snapshot of its
    /// own, for the same reason `resize` no longer does: it clears `in_sync` unconditionally, so
    /// the next `Self::render` call reprojects the live `term` itself once this returns.
    pub fn abort_sync(&mut self) {
        let was_open = self.sync.barrier().in_sync();
        let mut published = false;
        self.sync.abort_sync(&mut self.term, |_term, _reason| published = true);
        if was_open {
            self.abort_count += 1;
        }
        self.dirty |= published;
    }

    pub fn set_colors(&mut self, colors: TerminalColors) {
        self.palette = colors.palette();
        self.cursor_coloring = colors.cursor_coloring();
        self.dirty = true;
    }

    /// What `terminal_input::encode` needs: the modes the program has negotiated.
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Every answer owed to the program since the last call, in the order it asked, as bytes to
    /// write to the PTY.
    pub fn take_replies(&mut self) -> Vec<u8> {
        let replies = std::mem::take(&mut self.listener.0.lock().expect("listener mutex").replies);
        let mut out = Vec::new();
        for reply in replies {
            match reply {
                Reply::Bytes(text) => out.extend_from_slice(text.as_bytes()),
                Reply::Color(index, format) if index < PALETTE_LEN => {
                    let c = self.term.colors()[index].unwrap_or_else(|| {
                        let p = self.palette.get(index as u16);
                        Rgb { r: p.r, g: p.g, b: p.b }
                    });
                    out.extend_from_slice(format(c).as_bytes());
                }
                Reply::Color(..) => {}
                Reply::TextAreaSize(format) => {
                    let size = WindowSize {
                        num_lines: self.size.rows,
                        num_cols: self.size.cols,
                        cell_width: saturate_cell_px(self.size.cols, self.size.cell_width_px),
                        cell_height: saturate_cell_px(self.size.rows, self.size.cell_height_px),
                    };
                    out.extend_from_slice(format(size).as_bytes());
                }
            }
        }
        out
    }

    /// Title, bell, clipboard and a finished selection since the last call. The first three come
    /// from `Term`'s own event stream (via [`Listener`]); a finished selection does not (module
    /// doc, `pending_selection`) and is merged in here, the one place a caller reads events out.
    pub fn take_events(&mut self) -> HostEvents {
        let mut events = std::mem::take(&mut self.listener.0.lock().expect("listener mutex").events);
        if self.pending_selection.is_some() {
            events.selection = self.pending_selection.take();
        }
        events
    }

    /// Where the cursor is on the frame the last [`Self::render`] produced, visible or not. Always
    /// inside the grid: a frame's cursor line is an absolute grid line, which in the live view this
    /// screen always renders is the window row itself, and it is clamped rather than trusted.
    pub fn cursor_cell(&self) -> CursorCell {
        let cursor = &self.frame.cursor;
        let last_row = self.frame.rows.saturating_sub(1);
        let last_col = self.frame.cols.saturating_sub(1);
        CursorCell {
            row: u16::try_from(cursor.line.max(0)).unwrap_or(u16::MAX).min(last_row),
            col: cursor.col.min(last_col),
            visible: cursor.visible,
        }
    }

    /// The whole visible screen. `focused` picks a solid or a hollow cursor -- neovibe's rule that a
    /// solid cursor means the keys go here (`shell/src/pane_focus.rs`).
    ///
    /// Outside an open synchronized update, `term` is always quiescent (`open_update() == None`
    /// means `terminal-sync` is not mid-tear), so this reprojects it fresh, at most once per call
    /// -- bounded by however often the caller renders, not by the PTY's own read rate. While an
    /// update is open, that would show the update's own half-drawn content, so this reads
    /// `self.frame` instead: the snapshot `Self::feed` took at the exact byte the update opened,
    /// before any of its content could mutate `term` (re-review 2026-09-23, finding 1). `self.frame`
    /// is kept up to date every such call regardless of scroll position -- it is what
    /// [`Self::cursor_cell`] reports from, and typing always lands on the live screen, never on a
    /// scrolled-back one.
    ///
    /// Scrollback (Task 7): while pinned or anchor-expired, the window painted is
    /// `terminal_frame::viewport::project_window` at `Self::top_line`, cached in `self.pinned_frame`
    /// the same way `self.frame` is -- some of a near-bottom pin's rows are still the live screen's
    /// own, which an open update is free to mutate mid-draw, so reading a fresh projection then would
    /// show the same torn content this crate's whole snapshot design exists to rule out. **Exception
    /// (fix round 1):** the very first render of a pin still recomputes even mid-update, because
    /// `self.pinned_frame` starting `None` has no quiescent snapshot to fall back to, and `self.frame`
    /// is not a substitute -- its rows are numbered `0..rows-1` (screen-local), never the negative
    /// absolute lines a pinned `top_line` needs, so falling back to it drew blank or misplaced rows
    /// instead of a torn-but-correctly-positioned one. The `[N/M]` indicator (owner ruling R6) is
    /// appended after everything `build_paint_list` drew, only while `ScrollView::indicator` has one
    /// to show.
    pub fn render(&mut self, focused: bool) -> PaintList {
        let (top_line, mode) = self.scroll.window(&self.term);
        let update_open = self.open_update().is_some();
        if !update_open {
            self.frame = self.projector.full(&self.term);
        }
        let frame: &TerminalFrame = match mode {
            ViewMode::FollowBottom => &self.frame,
            ViewMode::Pinned | ViewMode::AnchorExpired => {
                // The `unwrap_or(&self.frame)` fallback below only makes sense once
                // `self.pinned_frame` has been populated at least once: `self.frame`'s own
                // `RowUpdate::line` values are screen-local (`0..rows-1`, `full_rows`), never
                // absolute grid lines the way `project_window`'s are, so falling back to it while
                // `top_line` is negative feeds `build_paint_list` two different line numberings at
                // once (fix round 1, `docs/superpowers/plans/2026-09-26-wave4.md` review). The very
                // first render of a pin is therefore recomputed unconditionally even mid-update --
                // a possibly torn frame for that one call is still strictly better than the wrong
                // rows entirely, and every later call while the update stays open reuses the
                // now-populated snapshot exactly as before.
                if !update_open || self.pinned_frame.is_none() {
                    self.pinned_frame = Some(project_window(&self.term, top_line, self.term.screen_lines()));
                }
                self.pinned_frame.as_ref().unwrap_or(&self.frame)
            }
        };
        let mut palette = self.palette.clone();
        palette.apply_overrides(&frame.color_overrides);
        let selection = self.selection_spans(top_line, frame.rows);
        let mut list = build_paint_list(&RenderInput {
            frame,
            window_top_line: top_line,
            mode,
            selection: &selection,
            focused,
            palette: &palette,
            cursor_color: self.cursor_coloring,
        });
        if let Some((above, history)) = self.copy_mode_indicator() {
            list.ops.extend(indicator_ops(above, history, list.cols, &palette));
        }
        list
    }

    /// [`Self::render`]'s own `[N/M]` reading (owner ruling R6; spec §6.2 extends it to copy mode).
    /// Outside copy mode this is exactly [`ScrollView::indicator`] (Task 7's plain scrollback keys):
    /// `None` until the view is actually pinned. Copy mode additionally reports `Some((0, M))` while
    /// unpinned -- tmux's own copy mode is visible from the moment it is entered, before any key has
    /// moved the window, unlike Task 7's keys, which show nothing until the first scroll pins one.
    fn copy_mode_indicator(&self) -> Option<(usize, usize)> {
        match self.scroll.indicator(&self.term) {
            some @ Some(_) => some,
            None if self.copy_mode => Some((0, max_scrollback(&self.term))),
            None => None,
        }
    }

    /// `prefix [`/`prefix PageUp` opens copy mode; `q`/`Esc`/a shell exit or restart leaves it
    /// (Task 9, spec §6.2). Entering neither pins nor unpins by itself: at the live bottom the view
    /// keeps following new output exactly as it always has (module doc, Task 7) -- only
    /// [`Self::copy_mode_indicator`] treats the toggle specially, so `[0/M]` is visible at once.
    /// Leaving always snaps back to the bottom, whether or not the view had been scrolled up while
    /// it was open, and marks the screen dirty either way -- the indicator is part of what a frame
    /// shows.
    pub fn set_copy_mode(&mut self, active: bool) {
        self.copy_mode = active;
        if active {
            self.mark_dirty();
        } else {
            self.scroll(ScrollRequest::Bottom);
        }
    }

    /// Applies one host scroll gesture ([`ScrollRequest`]): `Shift+PageUp`/`Shift+PageDown`, a wheel
    /// notch, `Shift+Home`/`Shift+End`. Always marks the screen dirty -- the window position is part
    /// of what a frame shows, the same as any other visible change.
    pub fn scroll(&mut self, req: ScrollRequest) {
        self.scroll.scroll(&self.term, req);
        self.dirty = true;
    }

    /// Applies one host selection gesture (Task 8): starts, extends, finishes or clears
    /// `term.selection`, the model `alacritty_terminal::selection` already owns
    /// ([`crate::select`]'s own module doc). Always marks the screen dirty -- the highlight is part
    /// of what a frame shows. On `Finish`, the text (if any was selected) is kept in
    /// `pending_selection` for [`Self::take_events`] to publish as `HostEvents::selection`, and
    /// also returned directly for a caller that wants it without waiting on that round trip.
    pub fn select(&mut self, cmd: SelectCommand) -> Option<String> {
        let text = select::apply(&mut self.term, cmd);
        if matches!(cmd, SelectCommand::Finish) && text.is_some() {
            self.pending_selection = text.clone();
        }
        self.dirty = true;
        text
    }

    /// `term.selection`, converted into per-line spans clamped to the window `Self::render` is
    /// about to draw -- what `RenderInput::selection` needs. Absolute grid lines throughout (module
    /// doc): a span's `line` can be negative while scrolled back, the same space `Self::top_line`
    /// reports. `None` selection, or one that resolves to nothing (`Selection::to_range` is `None`
    /// for an empty drag), yields no spans.
    fn selection_spans(&self, top: i32, rows: u16) -> Vec<SelectionSpan> {
        let Some(range) = self.term.selection.as_ref().and_then(|s| s.to_range(&self.term)) else {
            return Vec::new();
        };
        let cols = self.term.columns();
        let last_col = u16::try_from(cols.saturating_sub(1)).unwrap_or(u16::MAX);
        let window_end = top.saturating_add(i32::from(rows));
        let mut spans = Vec::new();
        let mut line = range.start.line.0;
        while line <= range.end.line.0 {
            if line >= top && line < window_end {
                let start_col = if line == range.start.line.0 {
                    u16::try_from(range.start.column.0).unwrap_or(last_col).min(last_col)
                } else {
                    0
                };
                let end_col = if line == range.end.line.0 {
                    u16::try_from(range.end.column.0).unwrap_or(last_col).min(last_col)
                } else {
                    last_col
                };
                spans.push(SelectionSpan {
                    line,
                    start_col,
                    end_col,
                });
            }
            line += 1;
        }
        spans
    }

    /// Whether the view is scrolled back at all -- pinned or anchor-expired, never following the
    /// live bottom.
    pub fn scrolled(&self) -> bool {
        self.scroll.scrolled()
    }

    /// The window's absolute top line, as [`Self::render`] last computed it: `0` while following the
    /// bottom. What Task 8's pointer -> cell conversion needs to turn a click row into a grid line.
    pub fn top_line(&self) -> i32 {
        self.scroll.window(&self.term).0
    }

    /// A key or paste about to reach the program ([`crate::session::SessionCommand::Input`]):
    /// typing -- never a bare modifier press or release -- snaps a scrolled-back view to the live
    /// bottom, the same as alacritty's own `on_terminal_input_start` (`terminal_input::is_modifier_key`'s
    /// own doc: "a bare modifier press must NOT count as terminal input"), and clears the selection
    /// (Task 8, owner ruling R6: "typing ... snaps to the bottom and clears the selection", the
    /// same `on_terminal_input_start` rule alacritty applies to both at once). A paste always
    /// counts, having no single key to be a bare modifier.
    pub fn note_input(&mut self, input: &NormalizedInput) {
        let bare_modifier =
            matches!(input, NormalizedInput::Key { event, .. } if terminal_input::is_modifier_key(event));
        if !bare_modifier {
            self.scroll(ScrollRequest::Bottom);
            if self.term.selection.take().is_some() {
                self.dirty = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::SelectKind;

    const SIZE: PtySize = PtySize {
        cols: 20,
        rows: 5,
        cell_width_px: 9,
        cell_height_px: 18,
    };

    fn themed() -> TerminalColors {
        TerminalColors {
            background: RgbColor::new(0x12, 0x34, 0x56),
            foreground: RgbColor::new(0xee, 0xdd, 0xcc),
            cursor: Some(RgbColor::new(0xee, 0xdd, 0xcc)),
        }
    }

    #[test]
    fn a_cursor_position_query_is_answered() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"ab\x1b[6n");
        assert_eq!(screen.take_replies(), b"\x1b[1;3R");
        assert_eq!(screen.take_replies(), b"", "answered once");
    }

    #[test]
    fn a_background_query_is_answered_from_the_theme_and_then_from_the_programs_own_colour() {
        let mut screen = Screen::new(SIZE, themed());
        screen.feed(b"\x1b]11;?\x07");
        assert_eq!(screen.take_replies(), b"\x1b]11;rgb:1212/3434/5656\x07");
        screen.feed(b"\x1b]11;#010203\x07\x1b]11;?\x07");
        assert_eq!(screen.take_replies(), b"\x1b]11;rgb:0101/0202/0303\x07");
    }

    #[test]
    fn osc52_may_copy_and_may_not_paste() {
        // Review 2026-09-23, finding 3: `assert_eq!(term_config().osc52, ...)` is the load-bearing
        // check. `Term` itself (`Osc52::OnlyCopy`) refuses a read before it ever reaches an
        // `Event`, so the two `screen.feed`/`take_replies` lines below observe an empty reply
        // either way and cannot distinguish "Term refused it" from "the listener would have
        // dropped it too" (`Event::ClipboardLoad(..) => {}` in listener.rs is unconditional).
        // They still exercise the real client path end to end and are kept for that.
        assert_eq!(term_config().osc52, Osc52::OnlyCopy);
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"\x1b]52;c;?\x07");
        assert_eq!(screen.take_replies(), b"", "a read of the clipboard is never answered");
        screen.feed(b"\x1b]52;p;?\x07\x1b]52;s;?\x07");
        assert_eq!(screen.take_replies(), b"", "nor a read of the selection");
        screen.feed(b"\x1b]52;c;aGk=\x07");
        assert_eq!(screen.take_events().clipboard.as_deref(), Some("hi"));
    }

    /// Review 2026-09-23, finding 3 (the other half): `Term` really is the layer that refuses a
    /// read, not merely `Screen`'s listener happening to agree. Under `Osc52::CopyPaste` (the
    /// negative control -- never neovibe's own config), alacritty's `clipboard_load` (verified
    /// against `alacritty_terminal-0.26.0/src/term/mod.rs`) DOES emit `Event::ClipboardLoad`; only
    /// our listener's own, separate, unconditional no-op then drops it. So the refusal is real and
    /// double-layered, as the module doc claims -- neither layer alone is a coincidence.
    #[test]
    fn a_clipboard_read_is_refused_by_term_itself_under_only_copy() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct RecordingListener(Arc<Mutex<Vec<alacritty_terminal::event::Event>>>);

        impl alacritty_terminal::event::EventListener for RecordingListener {
            fn send_event(&self, event: alacritty_terminal::event::Event) {
                self.0.lock().unwrap().push(event);
            }
        }

        let record = |osc52: Osc52, read: &[u8]| {
            let listener = RecordingListener::default();
            let grid = GridSize { cols: 20, rows: 5 };
            let config = Config { osc52, ..term_config() };
            let mut term = Term::new(config, &grid, listener.clone());
            let mut sync = SyncDriver::new();
            sync.feed(&mut term, read, |_, _| {});
            // The `let` binding (not a bare tail expression) matters: it ends the `MutexGuard`'s
            // scope here, before `listener` itself is dropped at the end of this closure body.
            let received = listener.0.lock().unwrap().len();
            received
        };

        // `c` is the clipboard; `p` and `s` are the selection phase 2 puts on the primary
        // (whole-branch review 2026-09-24, engine minor 5): `Term::clipboard_load` checks the
        // policy before it looks at the target, and this pins that for the selection too.
        for read in [&b"\x1b]52;c;?\x07"[..], b"\x1b]52;p;?\x07", b"\x1b]52;s;?\x07"] {
            let name = String::from_utf8_lossy(read).escape_debug().to_string();
            assert_eq!(
                record(Osc52::OnlyCopy, read),
                0,
                "{name}: Term itself never emits an event to refuse"
            );
            assert_eq!(
                record(Osc52::CopyPaste, read),
                1,
                "{name}: under a config neovibe never uses, Term does emit ClipboardLoad -- \
                 proving OnlyCopy's silence above is Term's own refusal, not an artifact of this test"
            );
        }
    }

    /// Bottom-terminal phase 2: OSC 52 names its target, and `Term` reports `c` as the clipboard and
    /// both `p` and `s` as the selection (`alacritty_terminal-0.26.0` `clipboard_store`). nvim's own
    /// OSC 52 provider copies `"*` with `s`. Before phase 2 the listener dropped the target, so a
    /// `"*y` inside the terminal overwrote the CLIPBOARD the owner pastes with `Ctrl+V`.
    #[test]
    fn osc52_to_the_selection_is_kept_apart_from_the_clipboard() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"\x1b]52;p;aGk=\x07");
        let events = screen.take_events();
        assert_eq!(events.primary.as_deref(), Some("hi"));
        assert_eq!(events.clipboard, None, "the selection is not the clipboard");
        screen.feed(b"\x1b]52;s;Ynll\x07\x1b]52;c;Y2xpcA==\x07");
        let events = screen.take_events();
        assert_eq!(events.primary.as_deref(), Some("bye"), "`s` is the selection too");
        assert_eq!(events.clipboard.as_deref(), Some("clip"));
    }

    /// Bottom-terminal phase 2: where typing lands, which is where the input method's preedit is
    /// drawn and its candidate window placed. The `PaintList` carries the cursor only while it is
    /// visible (`terminal-render`'s contract); this is the same cell, reported either way, and
    /// always the cell of the frame `render` just produced.
    #[test]
    fn the_cursor_cell_is_where_typing_lands_visible_or_not() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"ab\r\ncd");
        screen.render(true);
        assert_eq!(
            screen.cursor_cell(),
            CursorCell {
                row: 1,
                col: 2,
                visible: true
            }
        );
        screen.feed(b"\x1b[?25lx");
        let hidden = screen.render(true);
        assert!(hidden.cursor().is_none(), "a hidden cursor is not painted");
        assert_eq!(
            screen.cursor_cell(),
            CursorCell {
                row: 1,
                col: 3,
                visible: false
            },
            "but it is still where the next character goes"
        );
        screen.feed("\u{4f60}".as_bytes());
        screen.render(true);
        assert_eq!(screen.cursor_cell().col, 5, "a wide character moves it two cells");
        screen.feed(b"\x1b[5;20H");
        screen.render(true);
        assert_eq!((screen.cursor_cell().row, screen.cursor_cell().col), (4, 19));
    }

    /// Whole-branch review 2026-09-24 (engine minor 2): "the cell of the frame `render` just
    /// produced" matters exactly while a synchronized update is open, when that frame is the
    /// snapshot taken as the update opened. The session renders then (a focus change, a resize, a
    /// title inside nvim's `DECSET 2026` redraw), and a cursor read off the live `Term` would put the
    /// preedit and the candidate window at the half-drawn update's cursor. Checked red against a
    /// `cursor_cell` that reads `self.term`.
    #[test]
    fn inside_an_open_update_the_cursor_cell_is_the_snapshots_not_the_half_drawn_ones() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"ab");
        screen.render(true);
        screen.feed(b"\x1b[?2026h\x1b[3;7Hxyz");
        assert!(screen.open_update().is_some());
        screen.render(true);
        let cell = screen.cursor_cell();
        assert_eq!(
            (cell.row, cell.col),
            (0, 2),
            "the snapshot's cursor, not the update's (2, 9)"
        );
        screen.feed(b"\x1b[?2026l");
        assert!(screen.open_update().is_none());
        screen.render(true);
        let cell = screen.cursor_cell();
        assert_eq!((cell.row, cell.col), (2, 9), "the update ended: now its cursor");
    }

    #[test]
    fn title_and_bell_are_events() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"\x1b]2;one\x07\x1b]2;two\x07\x07");
        let events = screen.take_events();
        assert_eq!(events.title, Some(Some("two".to_string())), "latest title wins");
        assert!(events.bell);
        assert_eq!(screen.take_events(), HostEvents::default());
    }

    #[test]
    fn the_theme_decides_background_foreground_cursor_and_bold_default_text() {
        let palette = themed().palette();
        assert_eq!(palette.get(BACKGROUND), RgbColor::new(0x12, 0x34, 0x56));
        assert_eq!(palette.get(FOREGROUND), RgbColor::new(0xee, 0xdd, 0xcc));
        assert_eq!(palette.get(CURSOR), RgbColor::new(0xee, 0xdd, 0xcc));
        assert_eq!(
            palette.get(BRIGHT_FOREGROUND),
            RgbColor::new(0xee, 0xdd, 0xcc),
            "not xterm's white"
        );
        assert_eq!(
            palette.get(DIM_FOREGROUND),
            RgbColor::new(0xa4, 0xa4, 0xa4),
            "2/3 of the way to the foreground"
        );
        let mut screen = Screen::new(SIZE, themed());
        assert_eq!(screen.render(true).surface_background, RgbColor::new(0x12, 0x34, 0x56));
    }

    /// GUI pass 2026-09-23, defect 3, at the `Screen`: with no fixed cursor colour the cursor takes
    /// the covered cell's colours swapped; a fixed one (and `set_colors` switching between the two)
    /// is honoured; and an OSC 12 query still gets an answer -- the foreground.
    #[test]
    fn with_no_fixed_cursor_colour_the_cursor_follows_the_cell_and_a_query_gets_the_foreground() {
        let cursor_color = |screen: &mut Screen| match screen.render(true).cursor() {
            Some(terminal_render::PaintOp::DrawCursor { color, .. }) => *color,
            other => panic!("expected a cursor, got {other:?}"),
        };
        let follow = TerminalColors {
            cursor: None,
            ..themed()
        };
        let mut screen = Screen::new(SIZE, follow);
        // A cell the program painted itself (black on xterm white), and the cursor put back on it.
        screen.feed(b"\x1b[30;47mX\x1b[0m\x1b[1;1H");
        assert_eq!(cursor_color(&mut screen), RgbColor::new(0, 0, 0), "the cell's own ink");
        screen.set_colors(themed());
        assert_eq!(
            cursor_color(&mut screen),
            RgbColor::new(0xee, 0xdd, 0xcc),
            "a fixed colour is fixed"
        );
        screen.set_colors(follow);
        assert_eq!(cursor_color(&mut screen), RgbColor::new(0, 0, 0));
        screen.feed(b"\x1b]12;?\x07");
        assert_eq!(screen.take_replies(), b"\x1b]12;rgb:eeee/dddd/cccc\x07");
    }

    #[test]
    fn nothing_is_dirty_inside_an_open_synchronized_update() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        assert!(screen.take_dirty(), "a new screen has a first frame to show");
        screen.feed(b"\x1b[?2026hheld");
        assert!(!screen.take_dirty());
        screen.feed(b"\x1b[?2026l");
        assert!(screen.take_dirty());
    }

    #[test]
    fn dim_text_recedes_toward_the_background_on_light_and_dark() {
        let dawn = TerminalColors {
            background: RgbColor::new(0xfa, 0xf4, 0xed),
            foreground: RgbColor::new(0x57, 0x52, 0x79),
            cursor: None,
        };
        for colors in [dawn, themed()] {
            let dim = colors.palette().get(DIM_FOREGROUND);
            let (fg, bg) = (colors.foreground, colors.background);
            for (d, f, b) in [(dim.r, fg.r, bg.r), (dim.g, fg.g, bg.g), (dim.b, fg.b, bg.b)] {
                assert!(
                    (f.min(b)..=f.max(b)).contains(&d),
                    "{dim:?} not between {fg:?} and {bg:?}"
                );
                assert!(
                    d.abs_diff(b) < f.abs_diff(b) || f == b,
                    "{dim:?} is no closer to {bg:?} than {fg:?} is"
                );
            }
        }
    }

    /// `cat` of a binary file sends every byte value; the screen survives it, and what comes after
    /// a reset is PUBLISHED -- through `take_dirty`, the way the session sees it, not a direct
    /// `render` (Review Focus 4). The garbage ends inside an unterminated synchronized update, the
    /// case a killed nvim leaves: a RIS does not end it, and aborting it (the session's deadline)
    /// does.
    #[test]
    fn binary_garbage_neither_panics_nor_wedges_the_screen() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        let mut x: u32 = 0x2545_f491;
        let mut garbage: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        garbage.extend_from_slice(b"\x1b[?2026h");
        screen.feed(&garbage);
        let _ = screen.take_replies();
        assert!(screen.open_update().is_some(), "the garbage ends inside an open update");
        screen.take_dirty();
        screen.feed(b"\x1bc\x1b[2J\x1b[Hok");
        assert!(
            !screen.take_dirty(),
            "RIS does not end the update: nothing is published yet"
        );
        screen.abort_sync();
        assert!(
            screen.take_dirty(),
            "aborting the update publishes what was drawn inside it"
        );
        assert_eq!(screen.open_update(), None);
        let first = screen
            .render(true)
            .ops
            .iter()
            .any(|op| matches!(op, terminal_render::PaintOp::DrawText { row: 0, col: 0, text, .. } if text == "o"));
        assert!(first, "after RIS, 'ok' is drawn at the top left");
    }

    #[test]
    fn an_open_update_is_named_and_a_new_one_gets_a_new_name() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        assert_eq!(screen.open_update(), None);
        screen.feed(b"\x1b[?2026ha");
        let first = screen.open_update().expect("open");
        screen.feed(b"b");
        assert_eq!(screen.open_update(), Some(first), "the same update, still open");
        screen.feed(b"\x1b[?2026l\x1b[?2026hc");
        let second = screen.open_update().expect("open again");
        assert_ne!(second, first, "frame after frame: each BSU is a new update");
        screen.feed(b"\x1b[?2026l");
        assert_eq!(screen.open_update(), None);
        screen.abort_sync();
        assert_eq!(screen.open_update(), None, "aborting with nothing open is a no-op");
    }

    /// Review 2026-09-23, finding 2. A nested BSU inside an already-open update (`SyncBarrier`
    /// treats it as idempotent, matching vte's own "just reset the timeout" handling) must not
    /// perturb the ordinal: it is still the same update, still open, still unpublished.
    #[test]
    fn a_nested_bsu_does_not_rename_the_still_open_update() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"\x1b[?2026ha");
        let first = screen.open_update().expect("open");
        screen.feed(b"\x1b[?2026hb"); // a second BSU with no ESU in between: still one update.
        assert_eq!(screen.open_update(), Some(first), "a re-armed BSU is not a new update");
        screen.feed(b"\x1b[?2026hc"); // and a third, for good measure.
        assert_eq!(screen.open_update(), Some(first));
        screen.feed(b"\x1b[?2026l");
        assert_eq!(screen.open_update(), None, "the ESU that actually ends it still works");
    }

    /// Review 2026-09-23, finding 2 (the other half). Aborting an open update must retire its
    /// ordinal exactly like an ESU would, so the update that opens next gets a genuinely new name
    /// -- not one that collides with the aborted one's.
    #[test]
    fn aborting_an_update_retires_its_ordinal_like_an_esu_would() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"\x1b[?2026ha");
        let first = screen.open_update().expect("open");
        screen.abort_sync();
        assert_eq!(screen.open_update(), None);
        screen.feed(b"\x1b[?2026hb");
        let second = screen.open_update().expect("open again");
        assert_ne!(second, first, "the aborted update's name must not be reused");
    }

    /// A 0x0 resize is stored as one cell. `CSI 18 t` is answered from the `Term`'s own grid, which
    /// alacritty clamps anyway; `CSI 14 t` (the text area in pixels) is answered from the stored
    /// size, and said `4;0;0` before the clamp.
    #[test]
    fn a_zero_resize_is_reported_as_one_cell() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.resize(PtySize {
            cols: 0,
            rows: 0,
            cell_width_px: 9,
            cell_height_px: 18,
        });
        screen.feed(b"\x1b[18t\x1b[14t");
        assert_eq!(screen.take_replies(), b"\x1b[8;1;1t\x1b[4;18;9t");
    }

    /// `CSI 14 t`'s formatter (`alacritty_terminal::Term::text_area_size_pixels`) multiplies
    /// `num_lines * cell_height` and `num_cols * cell_width` in plain `u16` -- a debug build panics
    /// past 65535, a release one wraps silently. `saturate_cell_px` must keep the product under the
    /// cap (engine review 2026-09-23, minor 5; spec §5.2 says pixel sizes are saturated, and before
    /// this fix only `PtySize::to_winsize` actually did).
    #[test]
    fn a_wide_high_res_terminal_does_not_overflow_csi_14_t() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.resize(PtySize {
            cols: 2000,
            rows: 2000,
            cell_width_px: 100,
            cell_height_px: 100,
        });
        screen.feed(b"\x1b[14t");
        // 2000 * 100 = 200,000 would overflow. Clamped to the largest cell that still keeps the
        // product at or under 65535: 65535 / 2000 = 32, so 2000 * 32 = 64000.
        assert_eq!(screen.take_replies(), b"\x1b[4;64000;64000t");
    }

    /// Review 2026-09-23, finding 1. One read carries a complete, published frame ("frame"/
    /// "plus1") followed immediately by a second, still-open update that has only written "HEL"
    /// so far. `render()` must keep showing the last PUBLISHED frame, never the live `Term`'s
    /// current, torn content -- that is the whole point of the publication barrier.
    #[test]
    fn render_never_shows_a_frame_torn_by_a_later_update_in_the_same_read() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.take_dirty();
        screen.feed(b"\x1b[?2026h\x1b[1;1H\x1b[2Jframe\r\nplus1\x1b[?2026l\x1b[?2026h\x1b[1;1H\x1b[2JHEL");
        assert!(screen.take_dirty(), "the first update published inside this one read");
        assert!(
            screen.open_update().is_some(),
            "a second update is still open, unpublished"
        );
        let frame = screen.render(true);
        assert_eq!(
            frame.row_text(0),
            "frame",
            "must show the published frame, not the torn one"
        );
        assert_eq!(frame.row_text(1), "plus1");
    }

    /// The same hazard from a second call site: a caller renders once (correctly, mid-update,
    /// showing the last publish), the update ends, and a LATER `render()` call must pick up the
    /// newly published content -- caching must not go stale forever.
    #[test]
    fn render_catches_up_once_the_pending_update_publishes() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.take_dirty();
        screen.feed(b"\x1b[?2026h\x1b[1;1H\x1b[2Jfirst");
        assert!(!screen.take_dirty(), "still open, nothing published yet");
        assert_eq!(screen.render(true).row_text(0), "", "nothing published yet to show");
        screen.feed(b"\x1b[?2026l");
        assert!(screen.take_dirty());
        assert_eq!(screen.render(true).row_text(0), "first");
    }

    /// Re-review 2026-09-23 (fix round 1), finding 2 (minor). A resize while an update is open,
    /// that update then ending with nothing dispatched inside it (so `SyncBarrier`'s own `dirty`
    /// flag never latches, and neither `feed`'s transition-capture nor `abort_sync` ever sees
    /// anything to react to), used to leave `render` reporting the OLD grid size forever -- because
    /// `Term::resize` is not a parser dispatch, so nothing about it ever set `frame_ready`.
    /// `render`'s own unconditional "no update open -> reproject the live `term`" check fixes this
    /// with no special-casing in `resize` at all: both `end_sync` and `abort_sync` clear `in_sync`
    /// regardless of `dirty`, so `open_update()` reports `None` the moment the update ends either
    /// way, and the very next `render()` call reads the live (correctly resized) `term`.
    #[test]
    fn a_resize_inside_an_update_that_publishes_nothing_still_reaches_render() {
        for end_with_esu in [true, false] {
            let mut screen = Screen::new(SIZE, TerminalColors::default());
            screen.feed(b"hello");
            screen.feed(b"\x1b[?2026h");
            assert!(screen.open_update().is_some(), "the update is open");
            screen.resize(PtySize {
                cols: 40,
                rows: 10,
                cell_width_px: 9,
                cell_height_px: 18,
            });
            if end_with_esu {
                screen.feed(b"\x1b[?2026l");
            } else {
                screen.abort_sync();
            }
            assert_eq!(screen.open_update(), None, "end_with_esu={end_with_esu}");
            assert!(screen.take_dirty(), "a resize always marks the screen dirty");
            let frame = screen.render(true);
            assert_eq!(
                (frame.cols, frame.rows),
                (40, 10),
                "end_with_esu={end_with_esu}: must reflect the resize, not the stale 20x5 grid"
            );
        }
    }

    /// Re-review 2026-09-23 (fix round 1), finding 1 (important). The round-1 fix snapshotted a
    /// full O(rows x cols) projection inside every `publish` closure -- every `FrameComplete`,
    /// every end-of-read `Wakeup` -- so a PLAIN flood with no synchronized update anywhere paid one
    /// projection per PTY read, measured 2.6-7.6x slower than before that fix. This feeds 50,000
    /// small (64-byte) chunks of plain text -- no BSU/ESU at all -- mimicking the review's own
    /// measurement of a fast PTY reader (16-17 bytes per real read); under the round-1 code this
    /// reliably takes several seconds even in a debug build (50,000 full-screen projections), and
    /// under the fix it does zero projections during `feed` at all, only per-byte bookkeeping.
    /// `#[ignore]`d for the same reason Task 1's `delta_equals_full` is: a timing-sensitive,
    /// multi-second-when-red test in every debug workspace run costs more than it buys. Run with
    /// `cargo test -p neovibe-terminal --lib -- --ignored`.
    #[test]
    #[ignore = "timing-sensitive; run with --ignored to confirm no per-read projection cost"]
    fn feeding_a_plain_flood_pays_no_per_read_projection_cost() {
        let mut screen = Screen::new(
            PtySize {
                cols: 200,
                rows: 50,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            TerminalColors::default(),
        );
        let chunk: Vec<u8> = b"the quick brown fox jumps over the lazy dog!!\r\n"
            .iter()
            .cycle()
            .take(64)
            .copied()
            .collect();
        let start = std::time::Instant::now();
        for _ in 0..50_000 {
            screen.feed(&chunk);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "feeding 50,000 plain 64-byte chunks (3.2 MB, no synchronized update at all) took \
             {elapsed:?}; a per-read full-screen projection (the round-1 regression this guards \
             against) would make this far slower"
        );
    }

    #[test]
    fn kitty_keyboard_is_on() {
        assert!(term_config().kitty_keyboard);
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"\x1b[>1u");
        assert!(screen.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
    }

    // ---- scrollback (Task 7) ----

    /// Feeds `n` numbered lines, one `feed()` call per line -- matching how the module doc above
    /// describes the shape of a real, slow PTY reader. Used only by the small setup below; the
    /// eviction test (`f`) feeds thousands of lines in one call instead, because `ScrollView::observe`
    /// (via `RawViewport::observe`) is `O(history)` while pinned, and thousands of separate one-line
    /// `feed()` calls each paying that cost would make the test itself minutes slow for no assertion
    /// it needs -- a real PTY read is one chunk of many lines, not one `feed()` per line.
    fn feed_numbered_lines(screen: &mut Screen, range: std::ops::Range<i32>) {
        for i in range {
            screen.feed(format!("{i}\r\n").as_bytes());
        }
    }

    const SCROLL_SIZE: PtySize = PtySize {
        cols: 20,
        rows: 5,
        cell_width_px: 9,
        cell_height_px: 18,
    };

    /// (a) Scrolling up 3 lines moves the window's top row 3 lines further into history, and the
    /// indicator reports 3 lines hidden below the bottom, of the whole retained history.
    #[test]
    fn scrolling_up_moves_the_window_and_the_indicator_says_how_far() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        assert_eq!(
            screen.render(true).row_text(0),
            "46",
            "before scrolling: the live top row"
        );
        screen.scroll(ScrollRequest::Lines(3));
        let frame = screen.render(true);
        assert_eq!(
            frame.row_text(0),
            "43",
            "3 lines further back than the live top row (46)"
        );
        assert_eq!(screen.scroll.indicator(&screen.term), Some((3, 46)));
        assert!(screen.scrolled());
    }

    /// (b) Review Focus 4: output arriving while scrolled back must not move the pinned view -- the
    /// anchor holds the same content -- but the indicator's `N` grows by however many new lines
    /// arrived below it.
    #[test]
    fn output_while_pinned_does_not_move_the_view_only_the_indicator_grows() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.scroll(ScrollRequest::Lines(3));
        assert_eq!(screen.render(true).row_text(0), "43");
        feed_numbered_lines(&mut screen, 50..60);
        let frame = screen.render(true);
        assert_eq!(frame.row_text(0), "43", "the anchor held: same content at the top");
        assert_eq!(
            screen.scroll.indicator(&screen.term),
            Some((13, 56)),
            "10 more lines arrived below the still-pinned window"
        );
    }

    /// Fix round 1 review finding: the first scroll into `Pinned` while a synchronized update is
    /// already open used to leave `self.pinned_frame` at `None`, and `render`'s
    /// `unwrap_or(&self.frame)` fell back to the live-screen snapshot -- whose `RowUpdate::line`
    /// values are screen-local (`0..rows-1`, see `terminal_frame::project::full_rows`), not absolute
    /// grid lines like `project_window`'s. `build_paint_list`'s `to_row` then subtracted a negative
    /// `window_top_line` from those screen-local numbers, so the window's top rows drew nothing and
    /// the rest showed the live screen's own rows shifted into the wrong window rows. `render` must
    /// still produce a correct pinned window the very first time it is asked for one, even mid-update.
    #[test]
    fn scrolling_for_the_first_time_during_an_open_update_still_renders_the_pinned_window() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.feed(b"\x1b[?2026h"); // BSU, deliberately never closed in this test
        assert!(
            screen.open_update().is_some(),
            "the update must still be open at render time"
        );
        screen.scroll(ScrollRequest::Lines(3));
        let frame = screen.render(true);
        assert_eq!(
            frame.row_text(0),
            "43",
            "the pinned window's top row, not blank or the live screen's own top row"
        );
        assert_eq!(frame.row_text(4), "47", "the pinned window's bottom row");
    }

    /// (c) Typing (never a bare modifier press) snaps a scrolled-back view to the bottom; a bare
    /// `Shift` press does not (alacritty's own `on_terminal_input_start` rule, reproduced by
    /// `terminal_input::is_modifier_key`).
    #[test]
    fn typing_snaps_to_the_bottom_but_a_bare_modifier_does_not() {
        use terminal_input::keys::{Key, KeyEvent, ModifiersState, NamedKey};
        use terminal_input::NormalizedInput;

        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.scroll(ScrollRequest::Lines(3));
        assert!(screen.scrolled());
        let letter = NormalizedInput::Key {
            event: KeyEvent::press(Key::Character("a".into())),
            mods: ModifiersState::empty(),
        };
        screen.note_input(&letter);
        assert!(!screen.scrolled(), "a real key snaps back to the bottom");

        screen.scroll(ScrollRequest::Lines(3));
        assert!(screen.scrolled());
        let shift = NormalizedInput::Key {
            event: KeyEvent::press(Key::Named(NamedKey::Shift)),
            mods: ModifiersState::empty(),
        };
        screen.note_input(&shift);
        assert!(
            screen.scrolled(),
            "a bare modifier press must NOT count as terminal input"
        );

        let paste = NormalizedInput::Paste {
            text: "x".to_string(),
            bracketed: false,
        };
        screen.note_input(&paste);
        assert!(
            !screen.scrolled(),
            "a paste always counts, having no single key to be a modifier"
        );
    }

    /// (d) `Pages(n)` moves a whole screen (`rows` lines) at a time; scrolling past the oldest
    /// retained line clamps there rather than going further; `Bottom` always unpins.
    #[test]
    fn pages_move_a_screen_at_a_time_clamp_at_the_oldest_line_and_bottom_always_unpins() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.scroll(ScrollRequest::Pages(1));
        assert_eq!(screen.top_line(), -5, "one page is SCROLL_SIZE.rows (5) lines");
        screen.scroll(ScrollRequest::Pages(100));
        assert_eq!(
            screen.top_line(),
            -46,
            "clamped at the oldest retained line (history is 46)"
        );
        assert_eq!(screen.scroll.indicator(&screen.term), Some((46, 46)));
        screen.scroll(ScrollRequest::Bottom);
        assert!(!screen.scrolled());
        assert_eq!(screen.top_line(), 0);
    }

    /// (d2) `HalfPages(n)` moves half a screen (`rows / 2` lines, at least one) at a time -- Task 9's
    /// own `Ctrl+u`/`Ctrl+d` in copy mode.
    #[test]
    fn half_pages_move_half_a_screen_at_a_time() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.scroll(ScrollRequest::HalfPages(1));
        assert_eq!(screen.top_line(), -2, "half of SCROLL_SIZE.rows (5) is 2, rounded down");
        screen.scroll(ScrollRequest::HalfPages(-1));
        assert!(!screen.scrolled(), "back down by the same half screen unpins");
    }

    /// (e) The alternate screen keeps no history (`max_scrollback == 0`): every scroll request is a
    /// no-op there. Task 9 turns the wheel into arrow keys for a program running in it instead.
    #[test]
    fn scrolling_in_the_alternate_screen_is_a_no_op() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.feed(b"\x1b[?1049h");
        screen.scroll(ScrollRequest::Lines(3));
        assert!(!screen.scrolled());
        assert_eq!(screen.top_line(), 0);
    }

    /// (f) An anchor scrolled past its own eviction is reported `AnchorExpired`, not resolved to
    /// whatever now occupies its old position. Fed in ONE `feed()` call, not one per line -- see
    /// `feed_numbered_lines`'s own doc: `ScrollView::observe` costs `O(history)` while pinned, and
    /// thousands of separate calls would multiply that thousands of times over for no reason.
    #[test]
    fn a_pinned_anchor_scrolled_past_eviction_is_reported_expired_not_silently_moved() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.scroll(ScrollRequest::Lines(45));
        assert_eq!(screen.top_line(), -45, "pinned close to the oldest retained line");
        let mut flood = String::new();
        for i in 50..11_000 {
            flood.push_str(&format!("{i}\r\n"));
        }
        screen.feed(flood.as_bytes());
        let frame = screen.render(true);
        assert!(
            frame.has_expiry_notice(),
            "the evicted anchor must say so, not show something else"
        );
        assert_eq!(
            screen.scroll.indicator(&screen.term),
            None,
            "the notice is the one announcement"
        );
    }

    // ---- copy mode (Task 9) ----

    /// Entering copy mode shows the indicator at once, `[0/M]` at the live bottom where
    /// `ScrollView::indicator` alone reports nothing because nothing is pinned there; scrolling
    /// inside copy mode counts exactly as Task 7's plain scrollback keys already do; and leaving
    /// always snaps back to the bottom and removes the indicator, whether or not the view had been
    /// scrolled up in between.
    #[test]
    fn copy_mode_shows_0_of_m_at_the_bottom_and_leaving_removes_it() {
        fn indicator_text(frame: &PaintList) -> Option<&str> {
            frame.ops.iter().find_map(|op| match op {
                terminal_render::PaintOp::DrawNotice { text, .. } => Some(text.as_str()),
                _ => None,
            })
        }

        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        assert_eq!(
            indicator_text(&screen.render(true)),
            None,
            "not in copy mode: nothing drawn"
        );

        screen.set_copy_mode(true);
        assert_eq!(
            indicator_text(&screen.render(true)),
            Some("[0/46]"),
            "at the bottom, entering copy mode shows [0/M] at once"
        );
        assert!(!screen.scrolled(), "entering alone does not pin the view");

        screen.scroll(ScrollRequest::Lines(3));
        assert_eq!(
            indicator_text(&screen.render(true)),
            Some("[3/46]"),
            "scrolling inside copy mode counts the same way Task 7's own keys do"
        );

        screen.set_copy_mode(false);
        assert_eq!(indicator_text(&screen.render(true)), None, "leaving removes it");
        assert!(!screen.scrolled(), "leaving snaps back to the bottom");
    }

    // ---- selection (Task 8) ----

    /// (a) The half-cell rule (alacritty's own): a plain drag started on the LEFT half of its first
    /// cell and released on the RIGHT half of its last includes both ends. `Start`'s `right_half`
    /// stays `false` (the left half of col 1), and `Finish` reads out the inclusive range.
    #[test]
    fn a_drag_selects_the_half_cell_inclusive_range() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"hello world");
        screen.select(SelectCommand::Start {
            line: 0,
            col: 1,
            right_half: false,
            kind: SelectKind::Simple,
        });
        screen.select(SelectCommand::Extend {
            line: 0,
            col: 3,
            right_half: true,
        });
        assert_eq!(screen.select(SelectCommand::Finish).as_deref(), Some("ell"));
    }

    /// (b) A word click (`Semantic`) expands to the nearest `semantic_escape_chars` boundary --
    /// alacritty's default keeps a space as one, so a click anywhere in "world" reads out the whole
    /// word with no `Extend` needed. A line click (`Lines`) reads out the whole line with no
    /// trailing padding from the grid's unused columns -- `Term::line_to_string`'s own
    /// `line_length()` bound, not this crate's doing.
    #[test]
    fn word_and_line_selection_expand_to_their_own_boundary() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"hello world");
        screen.select(SelectCommand::Start {
            line: 0,
            col: 7,
            right_half: false,
            kind: SelectKind::Word,
        });
        assert_eq!(screen.select(SelectCommand::Finish).as_deref(), Some("world"));

        screen.select(SelectCommand::Start {
            line: 0,
            col: 0,
            right_half: false,
            kind: SelectKind::Line,
        });
        // `Term::selection_to_string` appends its own trailing `\n` for a `Lines` selection
        // (alacritty-0.26.0 `term/mod.rs`) -- real alacritty behaviour, not this crate's choice.
        assert_eq!(screen.select(SelectCommand::Finish).as_deref(), Some("hello world\n"));
    }

    /// (c) A word selection started on the SECOND cell of a wide character (`你`'s own spacer
    /// column) still reads out the whole word, not half of it -- alacritty's semantic search walks
    /// grid cells, not selection-side bookkeeping, so a wide char's spacer is not a special case
    /// here.
    #[test]
    fn a_word_selection_on_a_wide_chars_second_cell_reads_the_whole_word() {
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed("你好 ab".as_bytes());
        screen.select(SelectCommand::Start {
            line: 0,
            col: 1,
            right_half: false,
            kind: SelectKind::Word,
        });
        assert_eq!(screen.select(SelectCommand::Finish).as_deref(), Some("你好"));
    }

    /// (d) `Term::line_to_string` only appends `\n` when the row it just read is NOT flagged
    /// `WRAPLINE` -- so a selection crossing an auto-wrapped row boundary joins with no `\n`, while
    /// one crossing two really separate lines (`\r\n`) keeps exactly one.
    #[test]
    fn a_selection_joins_a_wrapped_line_without_a_newline_but_keeps_one_between_real_lines() {
        let mut wrapped = Screen::new(SIZE, TerminalColors::default());
        // 25 letters, no CR/LF: at 20 columns this auto-wraps after "abcdefghijklmnopqrst",
        // continuing "uvwxy" on the next row with WRAPLINE set on the first row's last cell.
        wrapped.feed(b"abcdefghijklmnopqrstuvwxy");
        wrapped.select(SelectCommand::Start {
            line: 0,
            col: 0,
            right_half: false,
            kind: SelectKind::Simple,
        });
        wrapped.select(SelectCommand::Extend {
            line: 1,
            col: 4,
            right_half: true,
        });
        assert_eq!(
            wrapped.select(SelectCommand::Finish).as_deref(),
            Some("abcdefghijklmnopqrstuvwxy"),
            "an auto-wrapped row joins with no newline"
        );

        let mut real_lines = Screen::new(SIZE, TerminalColors::default());
        real_lines.feed(b"ab\r\ncd");
        real_lines.select(SelectCommand::Start {
            line: 0,
            col: 0,
            right_half: false,
            kind: SelectKind::Simple,
        });
        real_lines.select(SelectCommand::Extend {
            line: 1,
            col: 1,
            right_half: true,
        });
        assert_eq!(
            real_lines.select(SelectCommand::Finish).as_deref(),
            Some("ab\ncd"),
            "two really separate lines keep exactly one newline between them"
        );
    }

    /// (e) Review Focus 4 (second half): typing (never a bare modifier) clears the selection, the
    /// same `on_terminal_input_start` rule that snaps a scrolled-back view to the bottom
    /// (`note_input`'s own doc).
    #[test]
    fn typing_clears_the_selection_but_a_bare_modifier_does_not() {
        use terminal_input::keys::{Key, KeyEvent, ModifiersState, NamedKey};
        use terminal_input::NormalizedInput;

        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"hello world");
        screen.select(SelectCommand::Start {
            line: 0,
            col: 0,
            right_half: false,
            kind: SelectKind::Simple,
        });
        assert!(screen.term.selection.is_some());

        let shift = NormalizedInput::Key {
            event: KeyEvent::press(Key::Named(NamedKey::Shift)),
            mods: ModifiersState::empty(),
        };
        screen.note_input(&shift);
        assert!(
            screen.term.selection.is_some(),
            "a bare modifier press must not clear the selection"
        );

        let letter = NormalizedInput::Key {
            event: KeyEvent::press(Key::Character("a".into())),
            mods: ModifiersState::empty(),
        };
        screen.note_input(&letter);
        assert!(screen.term.selection.is_none(), "a real key clears the selection");
    }

    /// (f) `Self::selection_spans` -- what `render` hands `RenderInput::selection` -- carries
    /// exactly the selected cells, including while scrolled back: Task 7's `Lines(3)` pins the
    /// window 3 lines into history, and a selection made on the window's own top row (now the
    /// absolute line the pin reports, negative) reports that same negative line, not `0`.
    #[test]
    fn selection_spans_cover_exactly_the_selected_cells_including_while_scrolled_back() {
        let mut screen = Screen::new(SCROLL_SIZE, TerminalColors::default());
        feed_numbered_lines(&mut screen, 0..50);
        screen.scroll(ScrollRequest::Lines(3));
        let top = screen.top_line();
        assert!(top < 0, "pinned into history");
        screen.select(SelectCommand::Start {
            line: top,
            col: 0,
            right_half: false,
            kind: SelectKind::Simple,
        });
        screen.select(SelectCommand::Extend {
            line: top,
            col: 1,
            right_half: true,
        });
        assert_eq!(
            screen.selection_spans(top, 5),
            vec![SelectionSpan {
                line: top,
                start_col: 0,
                end_col: 1,
            }]
        );
    }
}
