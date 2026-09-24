//! Bytes in, a `PaintList` out, and replies back to the program.
//!
//! [`Screen::feed`] parses bytes and marks the screen dirty. The session renders at most once
//! per frame, avoiding a full projection on every PTY read. [`Screen::take_replies`] returns
//! terminal replies. `Config::scrolling_history` retains 10,000 lines.
//!
//! `terminal-sync` withholds publication between `ESC[?2026h` and `ESC[?2026l`.
//! The session uses [`Screen::open_update`] to track the open update and
//! [`Screen::abort_sync`] to end it after a deadline. RIS (`ESC c`) is another dispatch and
//! does not end the update, so the deadline restores publication if a program never closes it.
//!
//! While an update is open, `render` uses the snapshot from just before its content began.
//! `feed` steps the sync driver one byte at a time and snapshots when `in_sync` changes from
//! false to true. This captures the last complete state before any new update content mutates it.
//! A second update in the same read captures the completed state left by the first update.
//!
//! Outside an update, the live `Term` is complete, so `render` projects it directly.
//! `end_sync` and `abort_sync` clear `in_sync` even when nothing was dispatched, allowing the
//! next render to reflect a resize without a separate resize snapshot.
//! `Projector::full` leaves damage untouched; these snapshots do not consume a delta stream.

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

/// The colours phase 1 takes from the window's theme: so a light colorscheme does
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
/// - `osc52: OnlyCopy` -- a program may SET the clipboard (nvim's `"+y`), never READ it.
///   `Term` enforces this itself, before any event reaches the listener.
/// - `kitty_keyboard: true` -- foot speaks it, `terminal-input` encodes it (its
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

/// Convert [`PtySize`] to [`GridSize`] in one place for construction and resize. Callers
/// already apply [`PtySize::clamped`], so no further minimum-size clamp is needed here.
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
/// the product under the cap before handing it over.
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
    /// The frame captured just as a synchronized update opens, or the initial empty grid.
    /// [`Self::render`] reads it while an update is open to avoid partially drawn content.
    /// Otherwise it projects the quiescent live `term`.
    frame: TerminalFrame,
    /// Count updates forced closed by `abort_sync`. Add this to `SyncBarrier`'s `esu_count` in
    /// [`Self::open_update`] so the ordinal changes whenever an update ends, whether by ESU or
    /// abort.
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

    /// Parse an output chunk and mark the screen dirty when publication is allowed, never
    /// midway through a `DECSET 2026` update.
    ///
    /// Feed `terminal-sync` one byte at a time to capture `self.frame` exactly when `in_sync`
    /// changes from false to true, before the update can mutate `term`. No other publication
    /// needs a snapshot because `Self::render` projects the live `term` whenever no update is
    /// open. A chunk can contain several `BSU ... ESU BSU ...` transitions; each opening
    /// replaces the snapshot, leaving the state just before the last still-open update.
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

    /// Clamp each grid dimension to at least one cell, matching `TIOCSWINSZ`, so `CSI 18 t`
    /// never reports `0x0`.
    ///
    /// Resizing changes geometry without a parser dispatch, so it takes no snapshot here.
    /// `Self::render` projects the resized live `term` once no update is open. Even an update
    /// with no dispatched content clears `in_sync` when it ends or is aborted, regardless of
    /// `SyncBarrier`'s `dirty` flag, so the next render sees the new size.
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

    /// Identify the synchronized update (`ESC[?2026h`) currently holding publication back. The
    /// ordinal changes when an update ends by ESU or [`Self::abort_sync`], letting callers
    /// distinguish a new update from one still open. Nested BSUs are idempotent and must not
    /// change it. Use `esu_count` plus forced aborts rather than counting BSUs.
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

    /// Render the visible screen. `focused` selects a solid cursor when keys go here and a
    /// hollow cursor otherwise.
    ///
    /// With no synchronized update open, project the quiescent live `term` once per render call
    /// rather than once per PTY read. During an update, use `self.frame`, captured by
    /// `Self::feed` at the byte that opened the update before its content could mutate `term`.
    /// This keeps partially drawn content off the screen.
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
        // Assert `term_config().osc52` directly to verify that `Term` refuses clipboard reads
        // under `Osc52::OnlyCopy`. An empty reply alone cannot distinguish that refusal from
        // the listener's unconditional `Event::ClipboardLoad(..) => {}`. The `screen.feed` and
        // `take_replies` checks also exercise the client path end to end.
        assert_eq!(term_config().osc52, Osc52::OnlyCopy);
        let mut screen = Screen::new(SIZE, TerminalColors::default());
        screen.feed(b"\x1b]52;c;?\x07");
        assert_eq!(screen.take_replies(), b"", "a read of the clipboard is never answered");
        screen.feed(b"\x1b]52;p;?\x07\x1b]52;s;?\x07");
        assert_eq!(screen.take_replies(), b"", "nor a read of the selection");
        screen.feed(b"\x1b]52;c;aGk=\x07");
        assert_eq!(screen.take_events().clipboard.as_deref(), Some("hi"));
    }

    /// Check that `Term` itself refuses clipboard reads under `Osc52::OnlyCopy`. With
    /// `Osc52::CopyPaste` as a control, `clipboard_load` emits `Event::ClipboardLoad`, which
    /// the listener separately discards unconditionally. This verifies both layers of
    /// clipboard-read protection.
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
        //: `Term::clipboard_load` checks the
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
    /// `"*y` inside the terminal overwrote the CLIPBOARD used for `Ctrl+V`.
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

    /// While a synchronized update is open, the cursor must come from the snapshot that `render`
    /// uses. Reading the live `Term` instead would place preedit and the input method's candidate
    /// window at the cursor of a partially drawn update.
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
    /// `render`. The garbage ends inside an unterminated synchronized update, the
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

    /// A nested BSU is idempotent in `SyncBarrier` and only resets the parser timeout. It must
    /// preserve the ordinal because the same update remains open and unpublished.
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

    /// Aborting an open update retires its ordinal just as ESU does, so the next update gets a
    /// different ordinal from the aborted one.
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

    /// `CSI 14 t` uses `alacritty_terminal::Term::text_area_size_pixels`, which multiplies
    /// `num_lines * cell_height` and `num_cols * cell_width` in `u16`. Products above 65535
    /// panic in debug builds and wrap in release builds. `saturate_cell_px` must keep both
    /// products at or below that limit.
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

    /// One read contains a published frame ("frame" and "plus1") followed by a still-open
    /// update that has written only "HEL". `render()` must show the last published frame while
    /// the second update remains incomplete.
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

    /// A resize inside an update must reach `render()` even if the update dispatches no
    /// content. `Term::resize` does not set the publication barrier's dirty flag. Both ESU and
    /// abort clear `in_sync` regardless of that flag, so the next render projects the live
    /// `term` with the new grid size.
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

    /// Feeding plain output must not pay for a full screen projection on each PTY read.
    /// This feeds 50,000 small 64-byte chunks without BSU or ESU. Projection belongs in `render`;
    /// `feed` should do only parsing and bookkeeping for this stream.
    /// The test is ignored because it is timing-sensitive and can take seconds on a regression.
    /// Run with `cargo test --lib -- --ignored` from this crate's directory.
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
