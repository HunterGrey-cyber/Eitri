//! THE IGNORANCE TEST.
//!
//! This file imports `terminal_render` and NOTHING ELSE -- no `terminal_frame`,
//! no `alacritty_terminal`, no `vte`. It builds a `PaintList` by hand and
//! executes it into a text surface, exactly the way a Skia backend would execute
//! it onto a canvas.
//!
//! If the contract ever leaks a terminal type into its public API, or requires a
//! terminal concept to execute, THIS FILE STOPS COMPILING. That is the whole
//! point of it: the guarantee "a backend author needs no terminal vocabulary" is
//! otherwise only a claim in a doc comment.

use terminal_render::{CursorShape, CursorText, GlyphStyle, PaintLayer, PaintList, PaintOp, RgbColor, UnderlineKind};

/// A backend. It knows about rectangles, text and colours. It has never heard of
/// a terminal.
struct TextSurface {
    cols: u16,
    rows: u16,
    cells: Vec<String>,
    fills: Vec<RgbColor>,
}

impl TextSurface {
    fn new(list: &PaintList) -> Self {
        let n = list.cols as usize * list.rows as usize;
        Self {
            cols: list.cols,
            rows: list.rows,
            // Clear the surface first, as the contract instructs.
            cells: vec![String::new(); n],
            fills: vec![list.surface_background; n],
        }
    }

    fn idx(&self, row: u16, col: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    fn execute(list: &PaintList) -> Self {
        let mut s = Self::new(list);
        for op in &list.ops {
            match op {
                PaintOp::FillCells { row, col, cols, color } => {
                    for c in *col..col + cols {
                        let i = s.idx(*row, c);
                        s.fills[i] = *color;
                    }
                }
                PaintOp::DrawText { row, col, text, .. } => {
                    let i = s.idx(*row, *col);
                    s.cells[i] = text.clone();
                }
                PaintOp::DrawCursor {
                    row,
                    col,
                    cols,
                    color,
                    text_under,
                    ..
                } => {
                    for c in *col..col + cols {
                        let i = s.idx(*row, c);
                        s.fills[i] = *color;
                    }
                    if let Some(t) = text_under {
                        let i = s.idx(*row, *col);
                        s.cells[i] = t.text.clone();
                    }
                }
                PaintOp::DrawNotice {
                    row,
                    col,
                    cols,
                    text,
                    background,
                    ..
                } => {
                    for c in *col..col + cols {
                        let i = s.idx(*row, c);
                        s.fills[i] = *background;
                        s.cells[i] = String::new();
                    }
                    let i = s.idx(*row, *col);
                    s.cells[i] = text.clone();
                }
            }
        }
        s
    }

    fn row_text(&self, row: u16) -> String {
        (0..self.cols).map(|c| self.cells[self.idx(row, c)].as_str()).collect()
    }
}

fn fill(row: u16, col: u16, cols: u16, color: RgbColor) -> PaintOp {
    PaintOp::FillCells { row, col, cols, color }
}
fn text(row: u16, col: u16, cols: u16, t: &str, color: RgbColor) -> PaintOp {
    PaintOp::DrawText {
        row,
        col,
        cols,
        text: t.to_owned(),
        color,
        style: GlyphStyle {
            underline_color: color,
            ..GlyphStyle::default()
        },
    }
}

#[test]
fn a_backend_with_no_terminal_knowledge_can_execute_the_contract() {
    let bg = RgbColor::new(0x18, 0x18, 0x18);
    let fg = RgbColor::new(0xd8, 0xd8, 0xd8);
    let list = PaintList {
        cols: 6,
        rows: 2,
        surface_background: bg,
        top_line: -42,
        ops: vec![
            fill(0, 0, 6, bg),
            fill(1, 0, 6, bg),
            text(0, 0, 1, "a", fg),
            // A full-width glyph: ONE op, TWO columns, and no op for column 2.
            text(0, 1, 2, "\u{6f22}", fg),
            // A base character with a combining mark: one op, one column.
            text(0, 3, 1, "e\u{301}", fg),
            PaintOp::DrawCursor {
                row: 1,
                col: 0,
                cols: 1,
                shape: CursorShape::Block,
                color: fg,
                text_under: Some(CursorText {
                    text: "z".into(),
                    color: bg,
                    style: GlyphStyle::default(),
                }),
                blinking: false,
            },
        ],
    };

    let surface = TextSurface::execute(&list);
    assert_eq!(surface.row_text(0), "a\u{6f22}e\u{301}");
    assert_eq!(surface.row_text(1), "z", "the cursor drew the character it covers");
    assert_eq!(surface.rows, 2);
    // Column 2 was never written: the wide glyph owns it, and the contract never
    // emitted anything for it.
    assert_eq!(surface.cells[surface.idx(0, 2)], "");
}

#[test]
fn every_coordinate_is_non_negative_and_inside_the_window() {
    // The property that lets a backend index a flat buffer without bounds logic.
    let list = PaintList {
        cols: 4,
        rows: 2,
        surface_background: RgbColor::default(),
        top_line: -9999,
        ops: vec![
            fill(0, 0, 4, RgbColor::default()),
            text(1, 3, 1, "x", RgbColor::default()),
        ],
    };
    for op in &list.ops {
        let (row, col, cols) = match op {
            PaintOp::FillCells { row, col, cols, .. }
            | PaintOp::DrawText { row, col, cols, .. }
            | PaintOp::DrawCursor { row, col, cols, .. }
            | PaintOp::DrawNotice { row, col, cols, .. } => (*row, *col, *cols),
        };
        assert!(row < list.rows, "row {row} outside {} rows", list.rows);
        assert!(col + cols <= list.cols, "col {col}+{cols} outside {} cols", list.cols);
    }
    // top_line is provenance and may be any value, including deep history. It is
    // NOT used to place anything.
    assert_eq!(list.top_line, -9999);
}

#[test]
fn layers_are_ordered_and_comparable() {
    assert!(PaintLayer::CellBackground < PaintLayer::Text);
    assert!(PaintLayer::Text < PaintLayer::Cursor);
    assert!(PaintLayer::Cursor < PaintLayer::Overlay);

    let c = RgbColor::default();
    let ordered = PaintList {
        cols: 1,
        rows: 1,
        surface_background: c,
        top_line: 0,
        ops: vec![fill(0, 0, 1, c), text(0, 0, 1, "a", c)],
    };
    assert!(ordered.is_layer_ordered());

    let inverted = PaintList {
        ops: vec![text(0, 0, 1, "a", c), fill(0, 0, 1, c)],
        ..ordered
    };
    assert!(
        !inverted.is_layer_ordered(),
        "the check must be able to fail, or it proves nothing"
    );
}

#[test]
fn underline_colour_is_always_concrete() {
    // Not an Option. A backend draws the underline with this and never has to
    // decide what "no colour" would have meant.
    let s = GlyphStyle {
        underline: UnderlineKind::Single,
        underline_color: RgbColor::new(1, 2, 3),
        ..GlyphStyle::default()
    };
    assert_eq!(s.underline_color, RgbColor::new(1, 2, 3));
}
