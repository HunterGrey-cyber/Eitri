//! A grid's cells as the bytes that draw them again in a fresh `Term` of the same size: each cell's character and
//! zero-width characters, its attributes, colours, underline colour and hyperlink, and each line's wrap. A wrapped
//! line is printed across the edge, so a later resize reflows the same lines.

use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

const ATTRS: Flags = Flags::from_bits_truncate(
    Flags::BOLD.bits()
        | Flags::DIM.bits()
        | Flags::ITALIC.bits()
        | Flags::ALL_UNDERLINES.bits()
        | Flags::INVERSE.bits()
        | Flags::HIDDEN.bits()
        | Flags::STRIKEOUT.bits(),
);

fn is_spacer(cell: &Cell) -> bool {
    cell.flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

fn color(c: Color, base: u16, default: u16, out: &mut Vec<String>) {
    match c {
        Color::Named(named) => {
            let n = named as usize;
            if n < 8 {
                out.push((base + n as u16).to_string());
            } else if n < 16 {
                out.push((base + 60 + n as u16 - 8).to_string());
            } else {
                // Only the default colours reach a cell through SGR; the others are drawing-time colours.
                out.push(default.to_string());
            }
        }
        Color::Indexed(i) => out.push(format!("{};5;{i}", base + 8)),
        Color::Spec(rgb) => out.push(format!("{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b)),
    }
}

/// The whole SGR, from a reset, that sets the pen to `cell`'s attributes.
pub fn sgr_for(cell: &Cell) -> String {
    let mut p = vec!["0".to_string()];
    let f = cell.flags;
    if f.contains(Flags::BOLD) {
        p.push("1".into());
    }
    if f.contains(Flags::DIM) {
        p.push("2".into());
    }
    if f.contains(Flags::ITALIC) {
        p.push("3".into());
    }
    if f.contains(Flags::UNDERLINE) {
        p.push("4".into());
    }
    if f.contains(Flags::DOUBLE_UNDERLINE) {
        p.push("4:2".into());
    }
    if f.contains(Flags::UNDERCURL) {
        p.push("4:3".into());
    }
    if f.contains(Flags::DOTTED_UNDERLINE) {
        p.push("4:4".into());
    }
    if f.contains(Flags::DASHED_UNDERLINE) {
        p.push("4:5".into());
    }
    if f.contains(Flags::INVERSE) {
        p.push("7".into());
    }
    if f.contains(Flags::HIDDEN) {
        p.push("8".into());
    }
    if f.contains(Flags::STRIKEOUT) {
        p.push("9".into());
    }
    if cell.fg != Color::Named(NamedColor::Foreground) {
        color(cell.fg, 30, 39, &mut p);
    }
    if cell.bg != Color::Named(NamedColor::Background) {
        color(cell.bg, 40, 49, &mut p);
    }
    match cell.underline_color() {
        Some(Color::Spec(rgb)) => p.push(format!("58;2;{};{};{}", rgb.r, rgb.g, rgb.b)),
        Some(Color::Indexed(i)) => p.push(format!("58;5;{i}")),
        Some(Color::Named(named)) if (named as usize) < 16 => p.push(format!("58;5;{}", named as usize)),
        _ => {}
    }
    format!("\x1b[{}m", p.join(";"))
}

#[derive(Clone, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

fn pen_of(cell: &Cell) -> Pen {
    Pen {
        fg: cell.fg,
        bg: cell.bg,
        flags: cell.flags & ATTRS,
        underline: cell.underline_color(),
    }
}

struct Encoder {
    out: Vec<u8>,
    pen: Option<Pen>,
    link: Option<(String, String)>,
    columns: usize,
}

impl Encoder {
    fn new(columns: usize) -> Self {
        let mut e = Encoder {
            out: Vec::new(),
            pen: None,
            link: None,
            columns,
        };
        e.reset();
        e
    }

    /// Default attributes and no link: what a new line made by a scroll is filled with.
    fn reset(&mut self) {
        self.out.extend_from_slice(b"\x1b[0m");
        self.pen = Some(pen_of(&Cell::default()));
        if self.link.take().is_some() {
            self.out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
    }

    fn set_pen(&mut self, cell: &Cell) {
        let pen = pen_of(cell);
        if self.pen.as_ref() != Some(&pen) {
            self.out.extend_from_slice(sgr_for(cell).as_bytes());
            self.pen = Some(pen);
        }
        let link = cell.hyperlink().map(|h| (h.id().to_owned(), h.uri().to_owned()));
        if link != self.link {
            match &link {
                Some((id, uri)) => self
                    .out
                    .extend_from_slice(format!("\x1b]8;id={id};{uri}\x1b\\").as_bytes()),
                None => self.out.extend_from_slice(b"\x1b]8;;\x1b\\"),
            }
            self.link = link;
        }
    }

    fn print(&mut self, cell: &Cell, col: usize) {
        self.set_pen(cell);
        if cell.c == '\t' {
            // HT writes `\t` into its start cell only when that cell is blank: draw the blank, go back, tab, and
            // return to the next column (a tab in the last column stays where HT leaves it).
            self.out.push(b' ');
            self.out.extend_from_slice(format!("\x1b[{}G\t", col + 1).as_bytes());
            if col + 1 < self.columns {
                self.out.extend_from_slice(format!("\x1b[{}G", col + 2).as_bytes());
            }
            return;
        }
        let mut buf = [0u8; 4];
        self.out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
        for c in cell.zerowidth().unwrap_or(&[]) {
            self.out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }

    /// One row. `full` prints every column (a wrapped row, or the row a wrap printed into, whose new line took the
    /// pen of the character that wrapped); otherwise trailing default cells are left out.
    fn row(&mut self, row: &Row<Cell>, full: bool) {
        let blank = Cell::default();
        let end = if full {
            self.columns
        } else {
            (0..self.columns)
                .rev()
                .find(|&c| row[Column(c)] != blank)
                .map_or(0, |c| c + 1)
        };
        for col in 0..end {
            let cell = &row[Column(col)];
            if !is_spacer(cell) {
                self.print(cell, col);
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.reset();
        self.out
    }
}

fn wrapped(row: &Row<Cell>, columns: usize) -> bool {
    row[Column(columns - 1)].flags.contains(Flags::WRAPLINE)
}

/// History and screen, top to bottom, for a fresh `Term` of the same size with its cursor at the origin.
pub fn encode_primary(grid: &Grid<Cell>) -> Vec<u8> {
    let columns = grid.columns();
    let mut e = Encoder::new(columns);
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32 - 1;
    let mut after_wrap = false;
    for i in top..=bottom {
        let row = &grid[Line(i)];
        let wraps = wrapped(row, columns);
        e.row(row, wraps || after_wrap);
        if !wraps && i != bottom {
            e.reset();
            e.out.extend_from_slice(b"\r\n");
        }
        after_wrap = wraps;
    }
    e.finish()
}

/// The screen's rows, each placed by `CUP` (so nothing scrolls), for a `Term` that has just entered the alternate
/// screen.
pub fn encode_alternate(grid: &Grid<Cell>) -> Vec<u8> {
    let columns = grid.columns();
    let mut e = Encoder::new(columns);
    let mut after_wrap = false;
    for i in 0..grid.screen_lines() {
        let row = &grid[Line(i as i32)];
        let wraps = wrapped(row, columns);
        if !after_wrap {
            e.reset();
            e.out.extend_from_slice(format!("\x1b[{};1H", i + 1).as_bytes());
        }
        e.row(row, wraps || after_wrap);
        after_wrap = wraps;
    }
    e.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::term_config;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Processor;
    use alacritty_terminal::Term;
    use terminal_sync::NeverBuffer;

    struct Size(usize, usize);
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    fn term(cols: usize, rows: usize) -> Term<VoidListener> {
        Term::new(term_config(), &Size(cols, rows), VoidListener)
    }

    fn feed(t: &mut Term<VoidListener>, bytes: &[u8]) {
        let mut p = Processor::<NeverBuffer>::new();
        p.advance(t, bytes);
    }

    /// What a cell shows: an empty extra storage counts as none (alacritty sometimes keeps one after clearing a
    /// wide character), which no byte stream can reproduce and nothing draws differently.
    fn shown(
        cell: &Cell,
    ) -> (
        char,
        Color,
        Color,
        Flags,
        Vec<char>,
        Option<Color>,
        Option<(String, String)>,
    ) {
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

    fn assert_same(a: &Grid<Cell>, b: &Grid<Cell>) {
        assert_eq!(a.history_size(), b.history_size(), "history");
        for i in -(a.history_size() as i32)..a.screen_lines() as i32 {
            for c in 0..a.columns() {
                assert_eq!(
                    shown(&a[Line(i)][Column(c)]),
                    shown(&b[Line(i)][Column(c)]),
                    "line {i} column {c}"
                );
            }
        }
    }

    fn round_trip(cols: usize, rows: usize, bytes: &[u8]) {
        let mut src = term(cols, rows);
        feed(&mut src, bytes);
        let mut dst = term(cols, rows);
        feed(&mut dst, &encode_primary(src.grid()));
        assert_same(src.grid(), dst.grid());
    }

    #[test]
    fn coloured_scrollback_comes_back_cell_for_cell() {
        let mut bytes = Vec::new();
        for i in 0..300 {
            bytes.extend_from_slice(
                format!(
                    "\x1b[{};{}m{i:04}\x1b[0m \x1b[38;5;{}mindexed\x1b[0m \x1b[38;2;10;20;{}mtrue\x1b[48;5;4m bg \x1b[0m\x1b[4:3;58;2;255;0;0mcurl\x1b[0m\r\n",
                    31 + i % 7,
                    if i % 2 == 0 { 1 } else { 3 },
                    i % 256,
                    i % 256
                )
                .as_bytes(),
            );
        }
        round_trip(60, 10, &bytes);
    }

    #[test]
    fn wrapped_lines_wide_characters_and_a_wide_character_at_the_edge_come_back() {
        let mut bytes = b"0123456789012345678\xe4\xbd\xa0tail\r\n".to_vec();
        bytes.extend_from_slice("一二三四五六七八九十一二三四五六七八九十".as_bytes());
        bytes.extend_from_slice(b"\r\n\x1b[44m                         \x1b[0m\r\nlast");
        round_trip(20, 6, &bytes);
    }

    #[test]
    fn combining_marks_links_tabs_and_blank_rows_come_back() {
        let bytes = "e\u{301}\u{302} x\r\n\x1b]8;id=a1;https://example.com/a\x1b\\link\x1b]8;;\x1b\\ \x1b]8;;https://example.com/b\x1b\\anon\x1b]8;;\x1b\\\r\na\tb\tc\r\n\x1b[9;3Hlow\x1b[1;1H";
        round_trip(30, 10, bytes.as_bytes());
    }

    #[test]
    fn the_alternate_screen_comes_back_cell_for_cell() {
        let mut src = term(40, 8);
        feed(
            &mut src,
            b"primary\r\n\x1b[?1049h\x1b[2;3H\x1b[1;32mbox\x1b[0m\x1b[8;1Hbottom row text that wraps past the edge",
        );
        let mut dst = term(40, 8);
        feed(&mut dst, b"\x1b[?1049h");
        feed(&mut dst, &encode_alternate(src.grid()));
        for i in 0..8 {
            for c in 0..40 {
                assert_eq!(
                    shown(&src.grid()[Line(i)][Column(c)]),
                    shown(&dst.grid()[Line(i)][Column(c)]),
                    "{i},{c}"
                );
            }
        }
    }

    #[test]
    fn sgr_for_sets_exactly_the_cells_attributes() {
        let mut t = term(10, 2);
        feed(&mut t, b"\x1b[1;2;3;4:2;7;8;9;91;48;2;1;2;3;58;5;200mX");
        let cell = t.grid()[Line(0)][Column(0)].clone();
        let mut u = term(10, 2);
        feed(&mut u, format!("{}X", sgr_for(&cell)).as_bytes());
        assert_eq!(shown(&u.grid()[Line(0)][Column(0)]), shown(&cell));
        assert_eq!(sgr_for(&Cell::default()), "\x1b[0m");
    }
}
