//! A terminal's whole visible state as one byte stream for a fresh `Term` of the same size: both screens with their
//! history, the keyboard-mode stacks of each, the palette, the modes, the scroll region, the tab stops, the
//! charsets, the title stack, the cursor style, the saved and the live cursor with a pending wrap, and the last
//! printed character `REP` repeats.
//!
//! Reading the alternate screen costs the source its `Term`: the primary is reachable only through one `swap_alt`
//! from the alternate side, and swapping back from the primary side would clear the alternate grid and overwrite
//! the primary's saved cursor. The source therefore never uses that `Term` again; on an abort it builds a fresh one
//! from the replay, as the destination does.

use std::fmt::Write as _;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Cursor, Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::COUNT;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{CharsetIndex, CursorShape, KeyboardModes, NamedColor, StandardCharset};
use alacritty_terminal::Term;
use serde::{Deserialize, Serialize};
use terminal_sync::SyncDriver;

use super::cells::{encode_alternate, encode_primary, sgr_for};
use super::gate::Gate;
use super::shadow::{Shadowed, Shadows};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenReplay {
    pub columns: u16,
    pub rows: u16,
    /// Everything but the last printed character: fed to a fresh `Term` through `Shadowed`.
    pub bytes: Vec<u8>,
    /// The shadow's last printed character, fed last through a `Gate` swallowing one call.
    pub last_input: Option<char>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayRefused {
    /// A keyboard-mode stack at alacritty's depth limit: replaying its pushes would drop titles in another order
    /// than the source did.
    TooDeep,
}

/// How the replayed `Term` treats a cursor placement at the moment it is made.
#[derive(Clone, Copy)]
struct Placing {
    /// The scroll region's lines (0-based, end exclusive) while origin mode is on: `CUP` counts from its first line
    /// and stops inside it.
    origin: Option<(usize, usize)>,
    insert: bool,
    /// The charset `SI`/`SO` selects once the placement is done.
    active: CharsetIndex,
}

/// What the replayed `Term` has set when the saved cursor is placed, which putting a line's wrap flag back has to
/// step around for a moment.
#[derive(Clone, Copy)]
struct Setting {
    insert: bool,
    line_wrap: bool,
    /// As `DECSTBM` sets it again (1-based top, bottom).
    region: Option<(usize, usize)>,
}

/// How a cursor reaches its line.
#[derive(Clone, Copy)]
enum Route {
    /// `CUP`, counted from this line (origin mode's first line, or the top).
    Cup { top: usize },
    /// `DECRC` to the saved cursor on `from`, then line feeds or reverse indexes, then the column from the left
    /// edge. None of these is held inside the scroll region by origin mode, so this reaches a line outside it.
    FromSaved { from: usize },
}

/// Reads `term` (calling `swap_alt` once when the alternate screen is shown: the source `Term` is not used again)
/// and the shadows into a replay.
pub fn build<L: EventListener>(term: &mut Term<L>, shadows: &Shadows) -> Result<ScreenReplay, ReplayRefused> {
    build_with_history(term, shadows, usize::MAX)
}

/// Builds a replay with only the newest `max_lines` history rows; visible rows are always kept.
/// Like [`build`], this consumes the source `Term`.
pub fn build_with_history<L: EventListener>(
    term: &mut Term<L>,
    shadows: &Shadows,
    max_lines: usize,
) -> Result<ScreenReplay, ReplayRefused> {
    if shadows.too_deep() {
        return Err(ReplayRefused::TooDeep);
    }
    let columns = term.columns();
    let rows = term.screen_lines();
    let mode = *term.mode();
    let alt = mode.contains(TermMode::ALT_SCREEN);
    let style = term.cursor_style();
    let palette: Vec<(usize, [u8; 3])> = (0..COUNT)
        .filter_map(|i| term.colors()[i].map(|rgb| (i, [rgb.r, rgb.g, rgb.b])))
        .collect();

    let origin = mode.contains(TermMode::ORIGIN).then(|| match shadows.scroll_region {
        Some((top, bottom)) => (top.saturating_sub(1).min(rows), bottom.min(rows)),
        None => (0, rows),
    });
    let active = Placing {
        origin,
        insert: mode.contains(TermMode::INSERT),
        active: shadows.active_charset,
    };
    let setting = Setting {
        insert: mode.contains(TermMode::INSERT),
        line_wrap: mode.contains(TermMode::LINE_WRAP),
        region: shadows.scroll_region,
    };

    // The shown screen is read first: for the alternate one, before the swap that exposes the primary.
    let mut alternate = None;
    if alt {
        let grid = term.grid();
        let (saved, live) = cursors(grid, active, setting);
        alternate = Some((encode_alternate(grid), saved, live));
        term.swap_alt();
    }

    if max_lines < term.grid().history_size() {
        term.grid_mut().update_history(max_lines);
    }
    let grid = term.grid();
    let mut out = Vec::new();
    if !alt && !shadows.keyboard_inactive.is_empty() {
        // The alternate screen's stack outlives the screen: entering it again makes that stack active. On a fresh
        // `Term` entering and leaving touches nothing that the rest of the stream does not set again.
        out.extend_from_slice(b"\x1b[?1049h");
        push_keyboard(&mut out, &shadows.keyboard_inactive);
        out.extend_from_slice(b"\x1b[?1049l");
    }
    out.extend_from_slice(&encode_primary(grid));

    let (saved, live) = match alternate {
        Some((cells, saved, live)) => {
            // The primary cursor, as it was when the alternate screen was entered: entering it again saves that
            // cursor as the primary's saved cursor, and leaving restores the primary with it.
            let fresh = Placing {
                origin: None,
                insert: false,
                active: CharsetIndex::G0,
            };
            place(&mut out, grid, &grid.cursor, Route::Cup { top: 0 }, fresh);
            if grid.cursor.input_needs_wrap {
                // Entering saves this cursor over the slot, so the slot may carry it around the rewrap.
                out.extend_from_slice(b"\x1b7");
                rewrap(
                    &mut out,
                    grid,
                    cursor_line(grid, &grid.cursor),
                    Setting {
                        insert: false,
                        line_wrap: true,
                        region: None,
                    },
                );
                out.extend_from_slice(b"\x1b8");
            }
            push_keyboard(&mut out, &shadows.keyboard_inactive);
            out.extend_from_slice(b"\x1b[?1049h");
            // The alternate screen's cursor starts as a copy of the primary's: its charsets, pen and link would
            // otherwise draw the cells below. A plain G0 and no link here; the pen is reset by the cells.
            out.extend_from_slice(b"\x1b(B\x0f\x1b]8;;\x1b\\");
            out.extend_from_slice(&cells);
            push_keyboard(&mut out, &shadows.keyboard_active);
            (saved, live)
        }
        None => {
            push_keyboard(&mut out, &shadows.keyboard_active);
            cursors(grid, active, setting)
        }
    };

    palette_bytes(&mut out, &palette);
    modes(&mut out, mode);
    // The kitty bits last of all modes: `CSI = … u` may have changed them after the last push.
    let _ = write!(Str(&mut out), "\x1b[={};1u", kitty_bits(mode));
    if let Some((top, bottom)) = shadows.scroll_region {
        let _ = write!(Str(&mut out), "\x1b[{top};{bottom}r");
    }
    out.extend_from_slice(b"\x1b[3g");
    for (column, _) in shadows.tab_stops.iter().enumerate().filter(|(_, stop)| **stop) {
        let _ = write!(Str(&mut out), "\x1b[1;{}H\x1bH", column + 1);
    }
    let origin = mode.contains(TermMode::ORIGIN);
    if origin {
        out.extend_from_slice(b"\x1b[?6l");
    }
    out.extend_from_slice(&saved);
    if origin {
        // Turning it back on homes the cursor, which is placed for good further on.
        out.extend_from_slice(b"\x1b[?6h");
    }
    titles(&mut out, shadows);
    match style.shape {
        CursorShape::Block | CursorShape::Underline | CursorShape::Beam => {
            let base = match style.shape {
                CursorShape::Block => 1,
                CursorShape::Underline => 3,
                _ => 5,
            };
            let _ = write!(Str(&mut out), "\x1b[{} q", base + u8::from(!style.blinking));
        }
        // No sequence selects these; they come only from configuration.
        CursorShape::HollowBlock | CursorShape::Hidden => {}
    }
    out.extend_from_slice(&live);

    Ok(ScreenReplay {
        columns: columns as u16,
        rows: rows as u16,
        bytes: out,
        last_input: shadows.last_input,
    })
}

/// Feeds a replay into a fresh `Term` of its size and the shadows that go with it.
pub fn apply<L: EventListener>(
    term: &mut Term<L>,
    shadows: &mut Shadows,
    driver: &mut SyncDriver,
    replay: &ScreenReplay,
) {
    driver.feed(&mut Shadowed::new(term, shadows), &replay.bytes, |_, _| {});
    finish_apply(term, shadows, driver, replay);
}

/// What [`apply`] does once the replay's bytes are fed: the parser left as a fresh one would be, holding only
/// the replay's last printed character. Shared with a caller that feeds the bytes its own way.
pub(crate) fn finish_apply<L: EventListener>(
    term: &mut Term<L>,
    shadows: &mut Shadows,
    driver: &mut SyncDriver,
    replay: &ScreenReplay,
) {
    // The stream printed characters of its own, which the parser and the shadow now hold for `REP`. A fresh parser
    // holds none, as the source's held none when it never printed (a screen filled by `DECALN`, say); the barrier
    // is kept as it is.
    let mut fresh = SyncDriver::new();
    std::mem::swap(fresh.barrier_mut(), driver.barrier_mut());
    *driver = fresh;
    shadows.last_input = None;
    if let Some(c) = replay.last_input {
        // The parser remembers the character for `REP`; the `Term` never sees it, so the grid stays the source's.
        let mut buf = [0u8; 4];
        let mut gate = Gate::new(term, 1);
        driver.feed(
            &mut Shadowed::new(&mut gate, shadows),
            c.encode_utf8(&mut buf).as_bytes(),
            |_, _| {},
        );
    }
}

/// `fmt::Write` over a byte buffer, so sequences can be formatted straight into the stream.
struct Str<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Str<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn push_keyboard(out: &mut Vec<u8>, stack: &[KeyboardModes]) {
    for modes in stack {
        let _ = write!(Str(out), "\x1b[>{}u", modes.bits());
    }
}

fn kitty_bits(mode: TermMode) -> u8 {
    [
        (TermMode::DISAMBIGUATE_ESC_CODES, KeyboardModes::DISAMBIGUATE_ESC_CODES),
        (TermMode::REPORT_EVENT_TYPES, KeyboardModes::REPORT_EVENT_TYPES),
        (TermMode::REPORT_ALTERNATE_KEYS, KeyboardModes::REPORT_ALTERNATE_KEYS),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, KeyboardModes::REPORT_ALL_KEYS_AS_ESC),
        (TermMode::REPORT_ASSOCIATED_TEXT, KeyboardModes::REPORT_ASSOCIATED_TEXT),
    ]
    .iter()
    .filter(|(term, _)| mode.contains(*term))
    .fold(0, |bits, (_, keys)| bits | keys.bits())
}

fn palette_bytes(out: &mut Vec<u8>, palette: &[(usize, [u8; 3])]) {
    for &(index, [r, g, b]) in palette {
        let colour = format!("rgb:{r:02x}/{g:02x}/{b:02x}");
        let _ = match index {
            i if i == NamedColor::Foreground as usize => write!(Str(out), "\x1b]10;{colour}\x1b\\"),
            i if i == NamedColor::Background as usize => write!(Str(out), "\x1b]11;{colour}\x1b\\"),
            i if i == NamedColor::Cursor as usize => write!(Str(out), "\x1b]12;{colour}\x1b\\"),
            i => write!(Str(out), "\x1b]4;{i};{colour}\x1b\\"),
        };
    }
}

fn modes(out: &mut Vec<u8>, mode: TermMode) {
    // Each setting written whichever way it stands, so a fresh `Term`'s defaults do not matter.
    let private = [
        (TermMode::APP_CURSOR, 1),
        (TermMode::ORIGIN, 6),
        (TermMode::LINE_WRAP, 7),
        (TermMode::SHOW_CURSOR, 25),
        (TermMode::FOCUS_IN_OUT, 1004),
        (TermMode::ALTERNATE_SCROLL, 1007),
        (TermMode::URGENCY_HINTS, 1042),
        (TermMode::BRACKETED_PASTE, 2004),
    ];
    for (bit, number) in private {
        let on = if mode.contains(bit) { 'h' } else { 'l' };
        let _ = write!(Str(out), "\x1b[?{number}{on}");
    }
    // Mouse modes and mouse encodings each exclude the others of their kind: only the one that is on is set.
    let exclusive = [
        (TermMode::MOUSE_REPORT_CLICK, 1000),
        (TermMode::MOUSE_DRAG, 1002),
        (TermMode::MOUSE_MOTION, 1003),
        (TermMode::UTF8_MOUSE, 1005),
        (TermMode::SGR_MOUSE, 1006),
    ];
    for (bit, number) in exclusive {
        if mode.contains(bit) {
            let _ = write!(Str(out), "\x1b[?{number}h");
        }
    }
    for (bit, number) in [(TermMode::INSERT, 4), (TermMode::LINE_FEED_NEW_LINE, 20)] {
        let on = if mode.contains(bit) { 'h' } else { 'l' };
        let _ = write!(Str(out), "\x1b[{number}{on}");
    }
    out.extend_from_slice(if mode.contains(TermMode::APP_KEYPAD) {
        b"\x1b="
    } else {
        b"\x1b>"
    });
}

fn osc_title(out: &mut Vec<u8>, title: &str) {
    let _ = write!(Str(out), "\x1b]2;{title}\x1b\\");
}

/// The title stack, then the title. A title is only ever set to some text by a sequence, so an entry with no title
/// can be pushed only while the `Term` has none: before the first title, which is the only place such entries
/// arise.
fn titles(out: &mut Vec<u8>, shadows: &Shadows) {
    let untitled = shadows.title_stack.iter().take_while(|e| e.is_none()).count();
    for (i, entry) in shadows.title_stack.iter().enumerate() {
        match entry {
            Some(title) => osc_title(out, title),
            // A no-title entry above a titled one: the nearest a sequence can come is an empty title.
            None if i >= untitled => osc_title(out, ""),
            None => {}
        }
        out.extend_from_slice(b"\x1b[22t");
    }
    if let Some(title) = &shadows.title {
        osc_title(out, title);
    }
}

fn charset_bytes(out: &mut Vec<u8>, cursor: &Cursor<Cell>) {
    for (index, intro) in [
        (CharsetIndex::G0, '('),
        (CharsetIndex::G1, ')'),
        (CharsetIndex::G2, '*'),
        (CharsetIndex::G3, '+'),
    ] {
        let set = match cursor.charsets[index] {
            StandardCharset::Ascii => 'B',
            StandardCharset::SpecialCharacterAndLineDrawing => '0',
        };
        let _ = write!(Str(out), "\x1b{intro}{set}");
    }
}

fn link_bytes(out: &mut Vec<u8>, cell: &Cell) {
    match cell.hyperlink() {
        Some(link) => {
            let _ = write!(Str(out), "\x1b]8;id={};{}\x1b\\", link.id(), link.uri());
        }
        None => out.extend_from_slice(b"\x1b]8;;\x1b\\"),
    }
}

/// The line a cursor stands on, held to the screen.
fn cursor_line(grid: &Grid<Cell>, cursor: &Cursor<Cell>) -> usize {
    (cursor.point.line.0.max(0) as usize).min(grid.screen_lines().saturating_sub(1))
}

/// Where `line` lies against the scroll region `(start, end)`: above it, inside it, or below it.
fn zone(line: usize, (start, end): (usize, usize)) -> std::cmp::Ordering {
    if line < start {
        std::cmp::Ordering::Less
    } else if line >= end {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

/// The saved cursor's bytes (placed, then `DECSC`, then its line's wrap flag put back) and the live cursor's.
fn cursors(grid: &Grid<Cell>, active: Placing, setting: Setting) -> (Vec<u8>, Vec<u8>) {
    let saved_cursor = &grid.saved_cursor;
    let saved_line = cursor_line(grid, saved_cursor);
    // The saved cursor is placed before the live charsets are chosen, and with origin mode off for that moment:
    // a saved cursor may lie outside the scroll region (the default one, at the top left, often does), where no
    // origin-relative `CUP` reaches.
    let mut saved = Vec::new();
    place(
        &mut saved,
        grid,
        saved_cursor,
        Route::Cup { top: 0 },
        Placing {
            origin: None,
            active: CharsetIndex::G0,
            ..active
        },
    );
    saved.extend_from_slice(b"\x1b7");
    if saved_cursor.input_needs_wrap {
        rewrap(&mut saved, grid, saved_line, setting);
    }

    let mut live = Vec::new();
    if grid.cursor == *saved_cursor {
        // The same cursor: restoring it is exact, wherever it stands and whatever its cell holds.
        live.extend_from_slice(b"\x1b8");
        live.push(shift(active.active));
        return (saved, live);
    }
    let line = cursor_line(grid, &grid.cursor);
    let route = match active.origin {
        // Origin mode holds every `CUP` inside the region; a cursor outside it (put there by a `DECRC`) is reached
        // from the saved cursor when both lie on the same side of the region, where no line feed or reverse index
        // on the way scrolls. Otherwise the nearest line inside the region is the best a placement can do.
        Some(region) if zone(line, region).is_ne() && zone(line, region) == zone(saved_line, region) => {
            Route::FromSaved { from: saved_line }
        }
        Some((start, _)) => Route::Cup { top: start },
        None => Route::Cup { top: 0 },
    };
    place(&mut live, grid, &grid.cursor, route, active);
    (saved, live)
}

fn shift(active: CharsetIndex) -> u8 {
    if active == CharsetIndex::G1 {
        0x0e
    } else {
        0x0f
    }
}

/// Moves the cursor to `line`, `column` along `route`.
fn go(out: &mut Vec<u8>, route: Route, line: usize, column: usize) {
    match route {
        Route::Cup { top } => {
            let _ = write!(Str(out), "\x1b[{};{}H", line - top.min(line) + 1, column + 1);
        }
        Route::FromSaved { from } => {
            out.extend_from_slice(b"\x1b8");
            for _ in from..line {
                out.push(b'\n');
            }
            for _ in line..from {
                out.extend_from_slice(b"\x1bM");
            }
            out.push(b'\r');
            if column > 0 {
                let _ = write!(Str(out), "\x1b[{column}C");
            }
        }
    }
}

/// Prints a cell again exactly as it is, in its own pen and link, with an ASCII G0 for that one print. A cell
/// that is the second half of a wide character is printed by printing that character.
fn reprint(out: &mut Vec<u8>, grid: &Grid<Cell>, line: usize, column: usize, route: Route) {
    let mut at = column;
    if at > 0
        && grid[Line(line as i32)][Column(at)]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
    {
        at -= 1;
    }
    let cell = &grid[Line(line as i32)][Column(at)];
    go(out, route, line, at);
    out.extend_from_slice(b"\x1b(B\x0f");
    out.extend_from_slice(sgr_for(cell).as_bytes());
    link_bytes(out, cell);
    let mut buf = [0u8; 4];
    // A tab's cell holds `\t`, which would be executed rather than printed: its blank is printed here.
    let c = if cell.c == '\t' { ' ' } else { cell.c };
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    for mark in cell.zerowidth().unwrap_or(&[]) {
        out.extend_from_slice(mark.encode_utf8(&mut buf).as_bytes());
    }
}

/// Puts a cursor where it stands, with its charsets and its pen. A cursor waiting to wrap is placed on the cell
/// its last character went to, and that cell is printed again exactly as it is (insert mode off and an ASCII G0
/// for that one print): the next character wraps, as it would have in the source. The print clears the cell's
/// wrap flag, which `rewrap` puts back for the saved cursor; a live cursor waiting to wrap on a line already
/// joined to the next (only a scroll under it, or a restore, makes one) loses that flag until its next character
/// wraps and sets it again.
fn place(out: &mut Vec<u8>, grid: &Grid<Cell>, cursor: &Cursor<Cell>, route: Route, placing: Placing) {
    let line = cursor_line(grid, cursor);
    let column = cursor.point.column.0.min(grid.columns().saturating_sub(1));
    if cursor.input_needs_wrap {
        if placing.insert {
            out.extend_from_slice(b"\x1b[4l");
        }
        reprint(out, grid, line, column, route);
        if placing.insert {
            out.extend_from_slice(b"\x1b[4h");
        }
    } else {
        go(out, route, line, column);
    }
    charset_bytes(out, cursor);
    out.push(shift(placing.active));
    out.extend_from_slice(sgr_for(&cursor.template).as_bytes());
    link_bytes(out, &cursor.template);
}

/// Puts `line`'s wrap flag back after a reprint of its last cell cleared it: that cell is printed again and one
/// more character wraps from it, which sets the flag; the character it leaves at the start of the next line is
/// then overwritten with that cell's own content. Origin mode must be off. Nothing scrolls: a region ending at
/// this line is lifted for the moment, and a line at the bottom of the screen, which can only wrap by scrolling,
/// keeps its flag cleared.
fn rewrap(out: &mut Vec<u8>, grid: &Grid<Cell>, line: usize, setting: Setting) {
    let columns = grid.columns();
    if columns < 2
        || line + 1 >= grid.screen_lines()
        || !grid[Line(line as i32)][Column(columns - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    {
        return;
    }
    let lift = setting.region.filter(|&(_, bottom)| bottom == line + 1);
    if lift.is_some() {
        out.extend_from_slice(b"\x1b[r");
    }
    if setting.insert {
        out.extend_from_slice(b"\x1b[4l");
    }
    if !setting.line_wrap {
        out.extend_from_slice(b"\x1b[?7h");
    }
    let top = Route::Cup { top: 0 };
    reprint(out, grid, line, columns - 1, top);
    out.push(b'X');
    let next = &grid[Line(line as i32 + 1)][Column(0)];
    reprint(out, grid, line + 1, 0, top);
    if next.c == '\t' {
        // A tab writes `\t` into the blank cell it starts from.
        let _ = write!(Str(out), "\x1b[{};1H\t", line + 2);
    }
    if !setting.line_wrap {
        out.extend_from_slice(b"\x1b[?7l");
    }
    if setting.insert {
        out.extend_from_slice(b"\x1b[4h");
    }
    if let Some((top, bottom)) = lift {
        let _ = write!(Str(out), "\x1b[{top};{bottom}r");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::term_config;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;

    fn feed(term: &mut Term<VoidListener>, shadows: &mut Shadows, driver: &mut SyncDriver, bytes: &[u8]) {
        driver.feed(&mut Shadowed::new(term, shadows), bytes, |_, _| {});
    }

    #[test]
    fn reading_the_alternate_screen_swaps_once_and_never_back() {
        let mut term = Term::new(term_config(), &TermSize::new(40, 10), VoidListener);
        let mut shadows = Shadows::new(40, true);
        let mut driver = SyncDriver::new();
        let mut primary = Vec::new();
        for i in 0..30 {
            primary.extend_from_slice(format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8).as_bytes());
        }
        primary.extend_from_slice(b"\x1b[1;32m$ ");
        feed(&mut term, &mut shadows, &mut driver, &primary);
        let cursor_before = term.grid().cursor.clone();
        let rows_before: Vec<Vec<Cell>> = (term.grid().topmost_line().0..=term.grid().bottommost_line().0)
            .map(|l| (0..40).map(|c| term.grid()[Line(l)][Column(c)].clone()).collect())
            .collect();
        feed(
            &mut term,
            &mut shadows,
            &mut driver,
            b"\x1b[?1049h\x1b[H\x1b[2Jvim\x1b[5;5H\x1b7\x1b[8;8H",
        );

        let replay = build(&mut term, &shadows).unwrap();
        assert!(!replay.bytes.is_empty());
        assert!(
            !term.mode().contains(TermMode::ALT_SCREEN),
            "the source now shows its primary screen"
        );
        let grid = term.grid();
        assert_eq!(
            grid.saved_cursor, cursor_before,
            "entering the alternate screen saved this cursor"
        );
        assert_eq!(grid.cursor, cursor_before, "the primary's own cursor is untouched");
        let rows_after: Vec<Vec<Cell>> = (grid.topmost_line().0..=grid.bottommost_line().0)
            .map(|l| (0..40).map(|c| grid[Line(l)][Column(c)].clone()).collect())
            .collect();
        assert_eq!(
            rows_before, rows_after,
            "the primary screen and its history are untouched"
        );
    }

    #[test]
    fn a_replay_survives_serialisation() {
        let mut term = Term::new(term_config(), &TermSize::new(20, 4), VoidListener);
        let mut shadows = Shadows::new(20, true);
        let mut driver = SyncDriver::new();
        feed(&mut term, &mut shadows, &mut driver, "x漢".as_bytes());
        let replay = build(&mut term, &shadows).unwrap();
        let back: ScreenReplay = serde_json::from_slice(&serde_json::to_vec(&replay).unwrap()).unwrap();
        assert_eq!(back, replay);
        assert_eq!(back.last_input, Some('漢'));
    }
}
