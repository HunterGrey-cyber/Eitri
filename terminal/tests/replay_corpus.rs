//! A terminal's screen carried to a fresh `Term` by a `ScreenReplay` must be the same terminal: the same projected
//! frame (every line of history and screen, the cursor, the modes the frame carries, the palette overrides), the
//! same `TermMode`, the same shadows, and what the frame leaves out read from both `Term`s directly -- hyperlinks,
//! zero-width characters, each line's wrap flag, the palette, both cursors with their charsets, pen and pending
//! wrap. Where the state shows only in what comes next (a `REP`, a wrap, a resize, leaving the alternate screen),
//! the moved terminal and an unmoved twin are fed the same bytes and compared again.

use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Cursor, Dimensions};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::COUNT;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{CharsetIndex, Color, CursorStyle, StandardCharset};
use eitri_terminal::replay::screen_replay::{apply, build, ReplayRefused, ScreenReplay};
use eitri_terminal::replay::shadow::{Shadowed, Shadows, KEYBOARD_STACK_MAX};
use eitri_terminal::screen::term_config;
use terminal_frame::viewport::project_window;
use terminal_frame::TerminalFrame;
use terminal_sync::SyncDriver;

struct Side {
    term: Term<VoidListener>,
    shadows: Shadows,
    driver: SyncDriver,
}

impl Side {
    fn new(cols: usize, rows: usize) -> Self {
        Side {
            term: Term::new(term_config(), &TermSize::new(cols, rows), VoidListener),
            shadows: Shadows::new(cols, true),
            driver: SyncDriver::new(),
        }
    }

    fn fed(cols: usize, rows: usize, bytes: &[u8]) -> Self {
        let mut side = Side::new(cols, rows);
        side.feed(bytes);
        side
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.driver
            .feed(&mut Shadowed::new(&mut self.term, &mut self.shadows), bytes, |_, _| {});
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(TermSize::new(cols, rows));
        self.shadows.resize(cols);
    }

    fn cell(&self, line: i32, col: usize) -> &Cell {
        &self.term.grid()[Line(line)][Column(col)]
    }

    fn line_text(&self, line: i32) -> String {
        let cols = self.term.columns();
        (0..cols)
            .map(|c| self.cell(line, c).c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}

/// A cell or a cursor's pen as it shows: an empty extra storage counts as none (alacritty keeps one after some
/// resets), which no byte stream can reproduce and nothing draws differently.
type Pen = (
    char,
    Color,
    Color,
    Flags,
    Vec<char>,
    Option<Color>,
    Option<(String, String)>,
);

fn pen(cell: &Cell) -> Pen {
    (
        cell.c,
        cell.fg,
        cell.bg,
        cell.flags,
        cell.zerowidth().unwrap_or(&[]).to_vec(),
        cell.underline_color(),
        cell.hyperlink().map(|h| (h.id().to_owned(), h.uri().to_owned())),
    )
}

#[derive(Debug, PartialEq)]
struct CursorSeen {
    line: i32,
    column: usize,
    waiting_to_wrap: bool,
    charsets: [StandardCharset; 4],
    pen: Pen,
}

fn cursor_seen(cursor: &Cursor<Cell>) -> CursorSeen {
    CursorSeen {
        line: cursor.point.line.0,
        column: cursor.point.column.0,
        waiting_to_wrap: cursor.input_needs_wrap,
        charsets: [CharsetIndex::G0, CharsetIndex::G1, CharsetIndex::G2, CharsetIndex::G3].map(|i| cursor.charsets[i]),
        pen: pen(&cursor.template),
    }
}

/// Everything the equality covers.
struct Seen {
    frame: TerminalFrame,
    mode: TermMode,
    shadows: Shadows,
    links: Vec<Option<(String, String)>>,
    marks: Vec<Vec<char>>,
    wraps: Vec<bool>,
    palette: Vec<Option<(u8, u8, u8)>>,
    cursor: CursorSeen,
    saved: CursorSeen,
    style: CursorStyle,
}

fn seen(side: &Side) -> Seen {
    let term = &side.term;
    let grid = term.grid();
    let cols = term.columns();
    let top = -(grid.history_size() as i32);
    let mut links = Vec::new();
    let mut marks = Vec::new();
    let mut wraps = Vec::new();
    for line in top..grid.screen_lines() as i32 {
        let row = &grid[Line(line)];
        wraps.push(row[Column(cols - 1)].flags.contains(Flags::WRAPLINE));
        for col in 0..cols {
            let cell = &row[Column(col)];
            links.push(cell.hyperlink().map(|h| (h.id().to_owned(), h.uri().to_owned())));
            marks.push(cell.zerowidth().unwrap_or(&[]).to_vec());
        }
    }
    Seen {
        frame: project_window(term, top, grid.total_lines()),
        mode: *term.mode(),
        shadows: side.shadows.clone(),
        links,
        marks,
        wraps,
        palette: (0..COUNT).map(|i| term.colors()[i].map(|c| (c.r, c.g, c.b))).collect(),
        cursor: cursor_seen(&grid.cursor),
        saved: cursor_seen(&grid.saved_cursor),
        style: term.cursor_style(),
    }
}

/// A link opened without an id gets one from a counter global to the process, so two terminals fed the same bytes
/// in one process number such links differently. A replay carries the source's ids exactly (compared exactly
/// against the source); a moved terminal and its unmoved twin are compared with those ids set aside.
fn without_generated_ids(mut s: Seen) -> Seen {
    let fix = |link: &mut Option<(String, String)>| {
        if let Some((id, _)) = link {
            if id.ends_with("_alacritty") {
                *id = "generated".into();
            }
        }
    };
    s.links.iter_mut().for_each(fix);
    fix(&mut s.cursor.pen.6);
    fix(&mut s.saved.pen.6);
    s
}

fn assert_same(a: &Seen, b: &Seen, what: &str) {
    let (fa, fb) = (&a.frame, &b.frame);
    assert_eq!((fa.cols, fa.rows), (fb.cols, fb.rows), "{what}: size and history");
    for (ra, rb) in fa.rows_changed.iter().zip(&fb.rows_changed) {
        assert_eq!(ra.line, rb.line, "{what}: line numbers");
        for (col, (ca, cb)) in ra.cells.iter().zip(&rb.cells).enumerate() {
            assert_eq!(ca, cb, "{what}: line {} column {col}", ra.line);
        }
    }
    assert_eq!(fa.cursor, fb.cursor, "{what}: the frame's cursor");
    assert_eq!(fa.modes, fb.modes, "{what}: the frame's modes");
    assert_eq!(fa.color_overrides, fb.color_overrides, "{what}: the frame's palette");
    assert_eq!(a.mode, b.mode, "{what}: TermMode");
    assert_eq!(a.shadows, b.shadows, "{what}: shadows");
    assert_eq!(a.links, b.links, "{what}: hyperlinks");
    assert_eq!(a.marks, b.marks, "{what}: zero-width characters");
    assert_eq!(a.wraps, b.wraps, "{what}: wrapped lines");
    assert_eq!(a.palette, b.palette, "{what}: palette");
    assert_eq!(a.cursor, b.cursor, "{what}: cursor");
    assert_eq!(a.saved, b.saved, "{what}: saved cursor");
    assert_eq!(a.style, b.style, "{what}: cursor style");
    assert_eq!(fa, fb, "{what}: the whole frame");
}

/// Moves `source` the way a terminal moves: a replay read from it, fed to a fresh `Term`.
fn moved(source: &mut Side) -> (Side, ScreenReplay) {
    let replay = build(&mut source.term, &source.shadows).expect("a replay");
    let mut dst = Side::new(replay.columns as usize, replay.rows as usize);
    dst.shadows.kitty_keyboard = source.shadows.kitty_keyboard;
    apply(&mut dst.term, &mut dst.shadows, &mut dst.driver, &replay);
    (dst, replay)
}

/// The corpus case: `bytes` into a source and an unmoved twin, the source moved, the moved terminal equal to the
/// twin. Returns the twin and the moved terminal for what the case feeds next.
fn case(cols: usize, rows: usize, bytes: &[u8]) -> (Side, Side, ScreenReplay) {
    let twin = Side::fed(cols, rows, bytes);
    let mut source = Side::fed(cols, rows, bytes);
    let before = seen(&source);
    let (dst, replay) = moved(&mut source);
    assert_same(&before, &seen(&dst), "after the move");
    (twin, dst, replay)
}

/// Both fed `bytes`, then compared.
fn both(twin: &mut Side, dst: &mut Side, bytes: &[u8], what: &str) {
    twin.feed(bytes);
    dst.feed(bytes);
    assert_same(
        &without_generated_ids(seen(twin)),
        &without_generated_ids(seen(dst)),
        what,
    );
}

fn mixed_lines(count: usize) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..count {
        let n = format!("{i:05}");
        let line = match i % 9 {
            0 => format!("\x1b[3{}m{n}\x1b[0m plain text after a coloured number", 1 + i % 7),
            1 => format!("\x1b[9{}m{n}\x1b[0m \x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m", i % 8),
            2 => format!(
                "\x1b[38;5;{}m{n}\x1b[0m \x1b[4munderlined\x1b[0m \x1b[7minverse\x1b[0m",
                i % 256
            ),
            3 => format!(
                "\x1b[38;2;{};{};{}m{n}\x1b[0m \x1b[48;2;10;20;30mtrue-colour background\x1b[0m",
                i % 256,
                (i * 7) % 256,
                (i * 13) % 256
            ),
            4 => format!(
                "\x1b[4{}m{n}\x1b[0m \x1b[2mdim\x1b[0m \x1b[9mstruck\x1b[0m \x1b[8mhidden\x1b[0m",
                i % 8
            ),
            5 => format!("{n} 中文 wide characters \x1b[32m漢字\x1b[0m end"),
            6 => format!(
                "\x1b[1;3;4;31;44m{n} every attribute at once\x1b[0m \x1b[4:3;58;5;{}mcurly\x1b[0m",
                i % 256
            ),
            7 => format!("{n} \x1b[44mblue to the edge\x1b[K"),
            _ => {
                let long: String = (0..130).map(|k| char::from(b'a' + (k % 26) as u8)).collect();
                format!("\x1b[35m{n}\x1b[0m {long}")
            }
        };
        out.push_str(&line);
        out.push_str("\x1b[0m\r\n");
    }
    out.push_str("\x1b[1;32mprompt$ ");
    out.into_bytes()
}

#[test]
fn ten_thousand_lines_of_mixed_sgr_come_back() {
    let (twin, dst, _) = case(80, 24, &mixed_lines(10_000));
    assert_eq!(dst.term.grid().history_size(), twin.term.grid().history_size());
    assert!(
        dst.term.grid().history_size() >= 9_000,
        "the history is carried, not just the screen"
    );
}

#[test]
fn vim_on_the_alternate_screen_with_a_split_comes_back_and_quitting_shows_the_primary() {
    let mut bytes = mixed_lines(2_000);
    bytes.extend_from_slice(b"\x1b[>1u");
    // vim: the alternate screen, its own keyboard flags, a vertical split, two status lines, a region for the
    // upper window, a saved cursor and a bar cursor in the left window.
    bytes.extend_from_slice(b"\x1b[22t\x1b]2;vim main.rs\x07\x1b[?1049h\x1b[>5u\x1b[?1h\x1b=\x1b[H\x1b[2J");
    for row in 1..=10 {
        bytes.extend_from_slice(
            format!(
                "\x1b[{row};1H\x1b[33m{row:3}\x1b[0m fn line_{row}() {{}}\x1b[{row};40H\x1b[34m│\x1b[0m right {row}"
            )
            .as_bytes(),
        );
    }
    bytes.extend_from_slice(b"\x1b[11;1H\x1b[1;7m main.rs [+]                           \x1b[0m\x1b[7m other.rs                               \x1b[0m");
    for row in 12..=22 {
        bytes.extend_from_slice(format!("\x1b[{row};1H\x1b[36m~\x1b[0m").as_bytes());
    }
    bytes.extend_from_slice(
        b"\x1b[23;1H\x1b[7m [No Name]                                                                      \x1b[0m",
    );
    bytes.extend_from_slice(b"\x1b[24;1H-- INSERT --\x1b[1;10r\x1b[4;9H\x1b7\x1b[6;12H\x1b[5 q\x1b[1m");
    let (mut twin, mut dst, _) = case(80, 24, &bytes);
    both(&mut twin, &mut dst, b"\x1bD\x1bDtyped", "typing in vim");
    both(&mut twin, &mut dst, b"\x1b8x", "the saved cursor in vim");
    both(
        &mut twin,
        &mut dst,
        b"\x1b[r\x1b[<1u\x1b[?1l\x1b>\x1b[0 q\x1b[?1049l\x1b[23t",
        "vim quit",
    );
    assert!(dst.line_text(23).starts_with("prompt$"), "the shell's prompt is back");
    both(&mut twin, &mut dst, b"ls\r\n", "the shell after vim");
}

#[test]
fn less_with_a_search_highlight() {
    let mut bytes = mixed_lines(300);
    bytes.extend_from_slice(b"\x1b[?1049h\x1b[?1h\x1b=\x1b[H\x1b[2J");
    for row in 1..=23 {
        bytes.extend_from_slice(
            format!("\x1b[{row};1Hline {row} has a \x1b[7mmatch\x1b[27m and \x1b[7mmatch\x1b[27m again").as_bytes(),
        );
    }
    bytes.extend_from_slice(b"\x1b[24;1H\x1b[7mPattern: match (press RETURN)\x1b[27m\x1b[K");
    let (mut twin, mut dst, _) = case(80, 24, &bytes);
    both(
        &mut twin,
        &mut dst,
        b"\x1b[24;1H\x1b[K\x1b[23;1H\n",
        "scrolling in less",
    );
    both(&mut twin, &mut dst, b"\x1b[?1l\x1b>\x1b[?1049l", "less quit");
}

#[test]
fn mouse_mode_with_sgr_encoding_and_bracketed_paste() {
    let bytes = b"text\r\n\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?1h\x1b=\x1b[4h\x1b[20h\x1b[?7l\x1b[?25l\
\x1b[?1007l\x1b[?12h\x1b[3;10r\x1b[?6h\x1b[4;5Hin the region";
    let (mut twin, mut dst, _) = case(40, 12, bytes);
    assert!(dst
        .term
        .mode()
        .contains(TermMode::SGR_MOUSE | TermMode::MOUSE_DRAG | TermMode::BRACKETED_PASTE));
    both(
        &mut twin,
        &mut dst,
        b"\x1b[1;1Hins\nfeed\n\n\n\n\n\n\n\n\n\nlong line past the edge with no wrap at all",
        "origin, insert, newline mode and no wrap after the move",
    );
}

#[test]
fn kitty_flags_on_both_screens_and_the_active_bits_after_a_set() {
    let (mut twin, mut dst, _) = case(40, 10, b"\x1b[>1u\x1b[>3u\x1b[?1049h\x1b[>5u\x1b[=8;2u");
    assert_eq!(dst.shadows.keyboard_inactive.len(), 2);
    both(&mut twin, &mut dst, b"\x1b[<1u", "popped on the alternate screen");
    both(&mut twin, &mut dst, b"\x1b[?1049l", "back on the primary");
    both(&mut twin, &mut dst, b"\x1b[<1u", "popped on the primary");

    // The alternate screen's stack outlives the screen; entering it again brings it back.
    let (mut twin, mut dst, _) = case(40, 10, b"\x1b[>1u\x1b[?1049h\x1b[>7u\x1b[?1049l\x1b[=2;2u");
    assert_eq!(dst.shadows.keyboard_inactive.len(), 1);
    both(
        &mut twin,
        &mut dst,
        b"\x1b[?1049h",
        "the alternate screen entered after the move",
    );
    both(&mut twin, &mut dst, b"\x1b[<1u\x1b[?1049l", "and left");
}

#[test]
fn a_title_stack_tab_stops_a_g1_charset_and_a_decstbm_region() {
    let mut bytes = b"\x1b[22t\x1b]2;shell\x07\x1b[22t\x1b]2;apt\x07".to_vec();
    bytes.extend_from_slice(b"\x1b[3g\x1b[1;5H\x1bH\x1b[1;13H\x1bH\x1b[1;30H\x1bH\x1b[1;1H");
    bytes.extend_from_slice(b"\x1b)0\x0elqk\x0f\r\n\x1b[1;23r");
    for i in 0..30 {
        bytes.extend_from_slice(format!("\x1b[23;1H\nGet:{i} http://deb.example stable/main\t[{i} kB]").as_bytes());
        bytes.extend_from_slice(format!("\x1b7\x1b[24;1H\x1b[42mProgress: [{:3}%]\x1b[0m\x1b8", i * 3).as_bytes());
    }
    bytes.extend_from_slice(b"\x0e");
    let (mut twin, mut dst, _) = case(60, 24, &bytes);
    assert_eq!(dst.shadows.title_stack, vec![None, Some("shell".to_string())]);
    both(
        &mut twin,
        &mut dst,
        b"q\x0f\ta\tb\tc\td",
        "tabs and the charset after the move",
    );
    for i in 0..30 {
        both(
            &mut twin,
            &mut dst,
            format!("\x1b[23;1H\nGet:{i} more").as_bytes(),
            "the region scrolls after the move",
        );
    }
    assert!(
        dst.line_text(23).starts_with("Progress:"),
        "the status line outside the region is not scrolled over"
    );
    both(&mut twin, &mut dst, b"\x1b[23t\x1b[23t", "titles popped");
    // A resize keeps set stops and extends with the default ones, as on an unmoved terminal.
    twin.resize(20, 24);
    dst.resize(20, 24);
    both(&mut twin, &mut dst, b"\r\t.\t.\t.", "after a narrower resize");
    twin.resize(70, 24);
    dst.resize(70, 24);
    both(&mut twin, &mut dst, b"\r\t.\t.\t.\t.\t.\t.\t.", "after a wider resize");
}

#[test]
fn a_region_then_a_resize_replays_no_region() {
    let bytes = b"one\r\ntwo\r\n\x1b[5;10r";
    let mut twin = Side::fed(40, 12, bytes);
    let mut source = Side::fed(40, 12, bytes);
    twin.resize(50, 14);
    source.resize(50, 14);
    let (mut dst, replay) = moved(&mut source);
    assert_eq!(dst.shadows.scroll_region, None);
    assert!(
        !replay.bytes.windows(3).any(|w| w == b"10r"),
        "a region a resize reset is not replayed"
    );
    assert_same(&seen(&twin), &seen(&dst), "after the move");
    let lines: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
    both(&mut twin, &mut dst, lines.as_bytes(), "the whole screen scrolls");
}

#[test]
fn a_ris_replays_empty_stacks_default_tabs_and_no_region() {
    let bytes =
        b"\x1b[>1u\x1b[?1049h\x1b[>3u\x1b[?1049l\x1b]2;t\x07\x1b[22t\x1b[3g\x1b[1;7H\x1bH\x1b[2;9r\x1b)0\x0eabc\x1bc";
    let (mut twin, mut dst, _) = case(40, 12, bytes);
    let mut expected = Shadows::new(40, true);
    expected.last_input = Some('c');
    assert_eq!(dst.shadows, expected);
    both(
        &mut twin,
        &mut dst,
        b"\ta\tb\x1b[?1049h\x1b[?1049l",
        "tabs and screens after the move",
    );
}

#[test]
fn wide_cjk_at_a_line_end() {
    let mut bytes = "0123456789012345678你tail\r\n".as_bytes().to_vec();
    bytes.extend_from_slice("一二三四五六七八九十一二三四五六七八九十\r\n".as_bytes());
    // A last row ending in a wide character: the cursor waits to wrap on the wide character's spacer.
    bytes.extend_from_slice("\x1b[31m012345678901234567\x1b[1;34m漢".as_bytes());
    let (mut twin, mut dst, _) = case(20, 6, &bytes);
    let cursor = dst.term.grid().cursor.point;
    assert!(dst.term.grid().cursor.input_needs_wrap);
    assert_eq!(cursor.column.0, 19, "waiting on the spacer of the last column");
    assert_eq!(dst.cell(cursor.line.0, 18).c, '漢');
    both(&mut twin, &mut dst, b"Z", "the next character wraps");
    assert_eq!(dst.line_text(cursor.line.0 + 1), "Z");
}

#[test]
fn rep_after_an_erase_repeats_the_last_printed_character_onto_a_blank_screen() {
    let (mut twin, mut dst, replay) = case(20, 5, b"X\x1b[2J\x1b[H");
    assert_eq!(replay.last_input, Some('X'));
    both(&mut twin, &mut dst, b"\x1b[3b", "REP after the move");
    assert_eq!(dst.line_text(0), "XXX");
    assert!(
        (1..5).all(|l| dst.line_text(l).is_empty()),
        "nothing else on the screen"
    );
}

#[test]
fn a_cursor_waiting_to_wrap_still_wraps_after_the_replay() {
    let a80 = "A".repeat(80);
    let (mut twin, mut dst, _) = case(80, 24, a80.as_bytes());
    both(&mut twin, &mut dst, b"B", "the next character after the move");
    assert_eq!(dst.cell(0, 79).c, 'A');
    assert_eq!(dst.cell(1, 0).c, 'B', "B starts the next line");

    // Saved in that state, restored after the move.
    let bytes = format!("{a80}\x1b7\x1b[10;10Hmiddle");
    let (mut twin, mut dst, _) = case(80, 24, bytes.as_bytes());
    both(&mut twin, &mut dst, b"\x1b8B", "the restored cursor still wraps");
    assert_eq!(dst.cell(0, 79).c, 'A');
    assert_eq!(dst.cell(1, 0).c, 'B');

    // A line-drawing G1 and insert mode, both restored around the reprint.
    let bytes = format!("\x1b[4h\x1b)0\x0e\x1b[1;35m{}", "q".repeat(80));
    let (mut twin, mut dst, _) = case(80, 24, bytes.as_bytes());
    both(
        &mut twin,
        &mut dst,
        b"qx\x1b[1;1Hq",
        "line drawing and insert mode after the move",
    );
}

#[test]
fn a_palette_change_an_osc8_link_a_combining_mark_and_a_coloured_underline() {
    let bytes = "\x1b]4;1;rgb:12/34/56\x1b\\\x1b]4;200;#abcdef\x07\x1b]10;rgb:aa/bb/cc\x07\x1b]11;#102030\x07\
\x1b]12;rgb:ff/00/ff\x07\x1b[31mred\x1b[0m \x1b]8;id=x;https://example.com/a\x1b\\link\x1b]8;;\x1b\\ \
\x1b]8;;https://example.com/b\x1b\\anon\x1b]8;;\x1b\\ e\u{301}\u{302} \x1b[4:3;58;2;255;0;0mcurl\x1b[0m \
\x1b]8;;https://example.com/open\x1b\\\x1b[4;58;5;3mstill open";
    let (mut twin, mut dst, _) = case(60, 6, bytes.as_bytes());
    both(
        &mut twin,
        &mut dst,
        b" more\x1b[3b",
        "the open link and pen after the move",
    );
}

#[test]
fn wrapped_lines_reflow_after_a_resize_as_the_source_does() {
    let mut bytes = Vec::new();
    for i in 0..12 {
        bytes.extend_from_slice(
            format!(
                "\x1b[3{}m{i:02} a line long enough to wrap twice on thirty columns 中文字\x1b[0m\r\n",
                i % 8
            )
            .as_bytes(),
        );
    }
    bytes.extend_from_slice(b"$ ");
    let (mut twin, mut dst, _) = case(30, 8, &bytes);
    for (cols, rows) in [(17, 8), (50, 12), (31, 5)] {
        twin.resize(cols, rows);
        dst.resize(cols, rows);
        assert_same(&seen(&twin), &seen(&dst), &format!("after a resize to {cols}x{rows}"));
    }
    both(&mut twin, &mut dst, b"echo after", "typing after the resizes");
}

#[test]
fn a_keyboard_stack_at_the_depth_limit_is_refused() {
    let below = "\x1b[>1u".repeat(KEYBOARD_STACK_MAX - 1);
    let (_twin, dst, _) = case(20, 4, below.as_bytes());
    assert_eq!(dst.shadows.keyboard_active.len(), KEYBOARD_STACK_MAX - 1);

    let at = "\x1b[>1u".repeat(KEYBOARD_STACK_MAX);
    let mut source = Side::fed(20, 4, at.as_bytes());
    assert_eq!(
        build(&mut source.term, &source.shadows).err(),
        Some(ReplayRefused::TooDeep)
    );
}

#[test]
fn the_alternate_screen_is_drawn_in_its_own_charset_and_link_not_the_primary_cursors() {
    // The primary cursor holds a line-drawing G0 and an open link when the alternate screen is entered; the
    // alternate side switches both off before printing.
    let bytes = b"\x1b(0\x1b]8;id=p;https://example.com/p\x1b\\\x1b[?1049h\x1b(B\x1b]8;;\x1b\\q x";
    let (mut twin, mut dst, _) = case(20, 5, bytes);
    assert_eq!(dst.cell(0, 0).c, 'q', "a literal q on the alternate screen");
    assert!(dst.cell(0, 0).hyperlink().is_none(), "no link on the alternate screen");
    both(
        &mut twin,
        &mut dst,
        b"\x1b[?1049lq",
        "the primary's charset and link after leaving",
    );
    assert_eq!(dst.cell(0, 0).c, '─', "the primary still draws lines");
}

#[test]
fn a_saved_cursor_waiting_to_wrap_on_a_joined_line_keeps_the_line_joined() {
    let a80 = "A".repeat(80);
    // The character after the save wrapped, joining the first line to the second.
    let bytes = format!("{a80}\x1b7B");
    let (mut twin, mut dst, _) = case(80, 24, bytes.as_bytes());
    assert!(
        dst.cell(0, 79).flags.contains(Flags::WRAPLINE),
        "the first line is still joined"
    );
    both(&mut twin, &mut dst, b"\x1b8C", "the restored cursor wraps again");
    let (mut twin, mut dst, _) = case(80, 24, bytes.as_bytes());
    twin.resize(60, 24);
    dst.resize(60, 24);
    both(
        &mut twin,
        &mut dst,
        b"",
        "a resize reflows the joined line the same way",
    );

    // A scroll region ending at that line, which the wrap would otherwise scroll.
    let bytes = format!("\x1b[5;1H{a80}\x1b7B\x1b[1;5r");
    let (mut twin, mut dst, _) = case(80, 24, bytes.as_bytes());
    assert!(dst.cell(4, 79).flags.contains(Flags::WRAPLINE));
    both(
        &mut twin,
        &mut dst,
        b"\x1b8C\x1b[1;1Hx",
        "inside the region after the move",
    );

    // Line wrapping off and insert mode on when the cursor is placed.
    let bytes = format!("{a80}\x1b7B\x1b[?7l\x1b[4h\x1b[3;3H");
    let (mut twin, mut dst, _) = case(80, 24, bytes.as_bytes());
    both(
        &mut twin,
        &mut dst,
        b"yz\x1b[?7h\x1b8C",
        "wrapping and insert mode after the move",
    );

    // The same cursor restored, then the alternate screen entered over it.
    let bytes = format!("{a80}\x1b7B\x1b8\x1b[?1049hvim");
    let (mut twin, mut dst, _) = case(80, 24, bytes.as_bytes());
    both(
        &mut twin,
        &mut dst,
        b"\x1b[?1049lC",
        "the primary after leaving the alternate screen",
    );
    assert_eq!(dst.cell(1, 0).c, 'C', "the primary cursor wrapped onto the joined line");
}

#[test]
fn a_cursor_restored_outside_the_region_with_origin_mode_on_stays_where_it_was() {
    // The default saved cursor, above a region, restored while origin mode is on.
    let (mut twin, mut dst, _) = case(40, 24, b"\x1b[5;10r\x1b[?6h\x1b8");
    assert_eq!(dst.term.grid().cursor.point.line, Line(0));
    both(&mut twin, &mut dst, b"X\nYZ", "output after the move");
    assert_eq!(dst.cell(0, 0).c, 'X');

    // Moved within the lines above the region after the restore.
    let (mut twin, mut dst, _) = case(40, 24, b"\x1b[5;10r\x1b[?6h\x1b8ab\nc");
    assert_eq!(dst.term.grid().cursor.point.line, Line(1));
    both(&mut twin, &mut dst, b"d\x1bMe", "output after the move");

    // Below the region.
    let (mut twin, mut dst, _) = case(40, 24, b"\x1b[20;7H\x1b7\x1b[5;10r\x1b[?6h\x1b8\n\nxy");
    assert_eq!(dst.term.grid().cursor.point.line, Line(21));
    both(&mut twin, &mut dst, b"z\x1bMw", "output after the move");
}

#[test]
fn a_screen_that_never_printed_leaves_nothing_for_rep() {
    // DECALN fills the screen without printing, so `REP` has no character to repeat.
    let (mut twin, mut dst, replay) = case(20, 5, b"\x1b#8");
    assert_eq!(replay.last_input, None);
    both(&mut twin, &mut dst, b"\x1b[3;3H\x1b[2K\x1b[3b", "REP after the move");
    assert!(dst.line_text(2).is_empty(), "REP printed nothing");
}

/// The freeze budget's main term: reading and feeding a replay of 10,000 lines at 120 columns.
#[test]
#[ignore = "timing: run with --ignored --nocapture"]
fn replay_time_for_ten_thousand_lines() {
    let bytes = mixed_lines(10_000);
    let mut times = Vec::new();
    for _ in 0..5 {
        let mut source = Side::fed(120, 40, &bytes);
        let start = Instant::now();
        let (dst, replay) = moved(&mut source);
        let took = start.elapsed();
        assert!(dst.term.grid().history_size() > 0);
        times.push((took, replay.bytes.len()));
    }
    times.sort();
    let (median, len): (Duration, usize) = times[2];
    println!(
        "replay of 10,000 lines at 120 columns: build + apply median {:.1} ms over 5 runs ({} bytes); all: {:?}",
        median.as_secs_f64() * 1000.0,
        len,
        times.iter().map(|(t, _)| t.as_millis()).collect::<Vec<_>>()
    );
}
