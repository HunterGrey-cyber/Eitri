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
use terminal_frame::{Projector, TerminalFrame};
use terminal_render::color::{BACKGROUND, BRIGHT_FOREGROUND, CURSOR, DIM_FOREGROUND, FOREGROUND};
use terminal_render::{build_paint_list, CursorColoring, PaintList, Palette, RenderInput, RgbColor, ViewMode};
use terminal_sync::SyncDriver;

use crate::listener::{HostEvents, Listener, Reply};
use crate::pty::PtySize;

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
    /// construction). Read by [`Self::render`] only while an update is open -- outside one,
    /// `render` projects the live `term` directly instead, since it is then guaranteed quiescent
    /// (re-review 2026-09-23, finding 1).
    frame: TerminalFrame,
    /// How many updates `abort_sync` has forced closed. `SyncBarrier` has no counter of its own
    /// for this (only `esu_count`, bumped by a real ESU); added to `esu_count` in [`Self::open_update`]
    /// so the ordinal changes exactly when an update ENDS, by either path (review 2026-09-23,
    /// finding 2).
    abort_count: u64,
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

    /// Title, bell and clipboard since the last call.
    pub fn take_events(&mut self) -> HostEvents {
        std::mem::take(&mut self.listener.0.lock().expect("listener mutex").events)
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
    /// before any of its content could mutate `term` (re-review 2026-09-23, finding 1).
    pub fn render(&mut self, focused: bool) -> PaintList {
        if self.open_update().is_none() {
            self.frame = self.projector.full(&self.term);
        }
        let mut palette = self.palette.clone();
        palette.apply_overrides(&self.frame.color_overrides);
        build_paint_list(&RenderInput {
            frame: &self.frame,
            window_top_line: 0,
            mode: ViewMode::FollowBottom,
            selection: &[],
            focused,
            palette: &palette,
            cursor_color: self.cursor_coloring,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
