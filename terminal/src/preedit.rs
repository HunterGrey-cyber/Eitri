//! An input method's unfinished composition, laid out on the terminal's own lattice (bottom-terminal
//! phase 2).
//!
//! While the owner types pinyin, fcitx5/rime holds a *preedit* -- `ni hao` -- that is not yet text
//! the shell should see. The host draws it over the finished frame, at the cell where typing lands
//! ([`CursorCell`]), underlined, and puts the input method's candidate window at its caret. Nothing
//! of it reaches the PTY: only a commit does, and that is the host's business, not this module's.
//!
//! **Why here and not in the host:** the layout is cell arithmetic -- how many columns a character
//! takes, where a composition that would run off the right edge starts instead -- and the answer
//! must use the same width rule the terminal uses for what it prints, or a committed `你好` would
//! land in different cells from the preedit that showed it. It is GTK-free and tested without a
//! display. The host only executes the ops ([`crate::paint_ops`]) and converts the caret to its own
//! coordinates.
//!
//! **What it draws, as foot does:** the preedit in the theme's foreground on the theme's
//! background, every cell underlined, and a beam at the input method's caret. It starts at the
//! cursor; a composition that would not fit moves left until it ends at the right edge -- one cell
//! short of it when the caret is at its end, so that caret has a cell of its own -- and one wider
//! than the whole row is cut there, never a wide character straddling the edge. Its background
//! fill covers the terminal's own cursor cell, so there is one caret on screen, not two -- also
//! when that cell is the one an end caret was given past the text. The caret is wherever
//! the input method puts it, which for the owner's rime is the composition's FIRST cell (its
//! `PreeditCursorPositionAtBeginning`, on by default on Linux). The input method's own attributes
//! (GTK hands them over as pango attributes; rime marks the segment being converted `HighLight`)
//! are not drawn: every cell is underlined alike.

use terminal_render::{CursorShape, GlyphStyle, PaintOp, UnderlineKind};
use unicode_width::UnicodeWidthChar;

use crate::screen::{CursorCell, TerminalColors};

/// A preedit laid out over a frame.
#[derive(Debug, Clone, PartialEq)]
pub struct PreeditLayout {
    /// To paint after the frame, in this order: one background fill, one `DrawText` per character
    /// cell, then the caret. Layer-ordered, like a `PaintList`'s own ops.
    pub ops: Vec<PaintOp>,
    /// The cell the input method's caret is in: where its candidate window belongs.
    pub caret_row: u16,
    pub caret_col: u16,
}

/// One character cell of the preedit: a character with any zero-width marks that follow it, and the
/// columns it takes (1, or 2 for a wide character).
struct Cell {
    text: String,
    cols: u16,
}

/// Lays `text` out at `anchor` on a `cols` x `rows` grid. `caret` is the input method's caret, in
/// characters from the start of `text` (GTK's `preedit_string` cursor), clamped to the text.
/// `None` when there is nothing to draw: an empty preedit, one of only control characters, or a
/// grid without a cell.
pub fn layout_preedit(
    text: &str,
    caret: usize,
    anchor: CursorCell,
    cols: u16,
    rows: u16,
    colors: &TerminalColors,
) -> Option<PreeditLayout> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let mut cells: Vec<Cell> = Vec::new();
    let mut caret_offset: u16 = 0;
    for (index, ch) in text.chars().enumerate() {
        let width = match ch.width() {
            // A control character has no cell; an input method has no business sending one.
            None => continue,
            Some(width) => width as u16,
        };
        if index < caret {
            caret_offset = caret_offset.saturating_add(width);
        }
        match (width, cells.last_mut()) {
            // A combining mark joins the character before it; one with nothing before it is dropped.
            (0, Some(last)) => last.text.push(ch),
            (0, None) => {}
            (width, _) => cells.push(Cell {
                text: ch.to_string(),
                cols: width,
            }),
        }
    }
    let total: u16 = cells.iter().fold(0, |sum, cell| sum.saturating_add(cell.cols));
    if total == 0 {
        return None;
    }
    let shown = total.min(cols);
    // A caret at the very end sits in the cell just after the text, so a composition shifted left
    // from the right edge leaves that cell free rather than drawing the caret over its own last
    // character (whole-branch review 2026-09-24, engine minor 3). Only a composition as wide as the
    // row has no such cell: its end caret shares the last column.
    let caret_at_end = caret_offset >= total;
    let needed = if caret_at_end { total.saturating_add(1) } else { total };
    let start = anchor.col.min(cols.saturating_sub(needed));
    let row = anchor.row.min(rows - 1);
    let underlined = GlyphStyle {
        underline: UnderlineKind::Single,
        underline_color: colors.foreground,
        ..GlyphStyle::default()
    };

    // The fill is what hides the terminal's own cursor, so it reaches the cursor's cell even when
    // the text does not: an end caret shifted in from the last column leaves that column past the
    // text, and without this the frame's block cursor stayed drawn there, with the beam inside it
    // (re-review 2026-09-24, N1). `start <= anchor_col` always, and `anchor_col < cols`, so the fill
    // never leaves the row.
    let anchor_col = anchor.col.min(cols - 1);
    let fill_cols = shown.max(anchor_col - start + 1);
    let mut ops = vec![PaintOp::FillCells {
        row,
        col: start,
        cols: fill_cols,
        color: colors.background,
    }];
    let mut offset: u16 = 0;
    for cell in cells {
        if offset + cell.cols > shown {
            break;
        }
        ops.push(PaintOp::DrawText {
            row,
            col: start + offset,
            cols: cell.cols,
            text: cell.text,
            color: colors.foreground,
            style: underlined,
        });
        offset += cell.cols;
    }
    // Clamped to the text, and to the row for a composition wider than it (cut at the edge).
    let caret_col = (start + caret_offset.min(shown)).min(cols - 1);
    ops.push(PaintOp::DrawCursor {
        row,
        col: caret_col,
        cols: 1,
        shape: CursorShape::Beam,
        color: colors.foreground,
        text_under: None,
        blinking: false,
    });
    Some(PreeditLayout {
        ops,
        caret_row: row,
        caret_col,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_render::RgbColor;

    const BG: RgbColor = RgbColor::new(0xfa, 0xf4, 0xed);
    const FG: RgbColor = RgbColor::new(0x57, 0x52, 0x79);

    fn colors() -> TerminalColors {
        TerminalColors {
            background: BG,
            foreground: FG,
            cursor: None,
        }
    }

    fn at(row: u16, col: u16) -> CursorCell {
        CursorCell {
            row,
            col,
            visible: true,
        }
    }

    /// `(col, cols, text)` of every `DrawText`, in order.
    fn texts(layout: &PreeditLayout) -> Vec<(u16, u16, String)> {
        layout
            .ops
            .iter()
            .filter_map(|op| match op {
                PaintOp::DrawText { col, cols, text, .. } => Some((*col, *cols, text.clone())),
                _ => None,
            })
            .collect()
    }

    fn fill(layout: &PreeditLayout) -> (u16, u16, u16) {
        match layout.ops.first() {
            Some(PaintOp::FillCells { row, col, cols, color }) => {
                assert_eq!(*color, BG, "the preedit sits on the theme's background");
                (*row, *col, *cols)
            }
            other => panic!("the first op must be the fill, got {other:?}"),
        }
    }

    /// rime's own segmentation of `nihao` (the P4 pass saw `ni hao`), at the cursor, every cell
    /// underlined in the foreground, a beam caret after it, and the ops in layer order.
    #[test]
    fn a_preedit_sits_at_the_cursor_underlined_with_a_caret() {
        let layout = layout_preedit("ni hao", 6, at(2, 4), 20, 5, &colors()).unwrap();
        assert_eq!(fill(&layout), (2, 4, 6));
        let drawn = texts(&layout);
        assert_eq!(drawn.len(), 6);
        assert_eq!(drawn[0], (4, 1, "n".to_string()));
        assert_eq!(drawn[5], (9, 1, "o".to_string()));
        for op in &layout.ops {
            if let PaintOp::DrawText { row, color, style, .. } = op {
                assert_eq!(*row, 2);
                assert_eq!(*color, FG);
                assert_eq!(style.underline, UnderlineKind::Single);
                assert_eq!(style.underline_color, FG);
            }
        }
        assert_eq!((layout.caret_row, layout.caret_col), (2, 10));
        assert!(matches!(
            layout.ops.last(),
            Some(PaintOp::DrawCursor {
                row: 2,
                col: 10,
                shape: CursorShape::Beam,
                ..
            })
        ));
        assert!(layout.ops.windows(2).all(|w| w[0].layer() <= w[1].layer()));
    }

    /// The owner's rime puts its caret at the START of the composition, not the end: fcitx5-rime
    /// 5.1.16's `PreeditCursorPositionAtBeginning` defaults to true on Linux (`rimeengine.h:88-94`,
    /// "this pins candidate window while typing"), and `rimestate.cpp:387` sets the cursor to 0. So
    /// the beam is on the composition's first cell and the candidate window goes there -- which, for
    /// a composition moved left from the right edge, is where it now starts, not the cursor's column.
    #[test]
    fn rimes_caret_at_the_start_puts_the_beam_on_the_first_cell() {
        let layout = layout_preedit("ni hao", 0, at(2, 4), 20, 5, &colors()).unwrap();
        assert_eq!(fill(&layout), (2, 4, 6));
        assert_eq!((layout.caret_row, layout.caret_col), (2, 4));
        assert!(matches!(
            layout.ops.last(),
            Some(PaintOp::DrawCursor {
                row: 2,
                col: 4,
                shape: CursorShape::Beam,
                ..
            })
        ));
        let moved = layout_preedit("ni hao", 0, at(1, 18), 20, 5, &colors()).unwrap();
        assert_eq!(fill(&moved), (1, 14, 6));
        assert_eq!(
            moved.caret_col, 14,
            "the moved composition's first cell, not the cursor's 18"
        );
    }

    /// A wide character takes two cells, as it does when the shell prints it, and the caret counts
    /// cells, not characters.
    #[test]
    fn wide_characters_take_two_cells_and_the_caret_counts_cells() {
        let layout = layout_preedit("\u{4f60}\u{597d}", 1, at(0, 3), 20, 5, &colors()).unwrap();
        assert_eq!(fill(&layout), (0, 3, 4));
        assert_eq!(
            texts(&layout),
            vec![(3, 2, "\u{4f60}".to_string()), (5, 2, "\u{597d}".to_string())]
        );
        assert_eq!(layout.caret_col, 5, "after one wide character: two cells in");
    }

    /// Near the right edge a composition moves left until it ends at the edge (foot does the same),
    /// rather than running off the row. With the caret at its END, one cell further: the beam sits
    /// in the last column after the text, as it does anywhere else, and not on the composition's
    /// own last character (whole-branch review 2026-09-24, engine minor 3: `ni hao` used to be
    /// drawn at 14..19 with its end caret in 19, between `a` and `o`).
    #[test]
    fn a_preedit_that_would_run_off_the_right_edge_moves_left() {
        let layout = layout_preedit("ni hao", 6, at(1, 18), 20, 5, &colors()).unwrap();
        assert_eq!(fill(&layout), (1, 13, 6));
        let drawn = texts(&layout);
        assert_eq!(drawn[0].0, 13);
        assert_eq!(drawn[5], (18, 1, "o".to_string()));
        assert_eq!(layout.caret_col, 19, "just after the text, in the last column");
        assert!(
            drawn.iter().all(|(col, cols, _)| layout.caret_col >= col + cols),
            "an end caret is on no character"
        );
        let mid = layout_preedit("ni hao", 3, at(1, 18), 20, 5, &colors()).unwrap();
        assert_eq!(fill(&mid), (1, 14, 6), "a caret inside it needs no extra cell");
        assert_eq!(mid.caret_col, 17);
        let row_wide = layout_preedit("abcde", 5, at(0, 3), 5, 3, &colors()).unwrap();
        assert_eq!(fill(&row_wide), (0, 0, 5), "exactly as wide as the row: nowhere to go");
        assert_eq!(row_wide.caret_col, 4);
    }

    /// The fill is the only thing that hides the terminal's own block cursor, so it must reach the
    /// cursor's cell wherever the caret is. The case that broke (re-review 2026-09-24, N1): the
    /// cursor in the LAST column and the caret at the end, so the composition shifts one cell
    /// further left than the cursor and its text ends one column short of it -- the fill used to
    /// stop with the text, at 13..18, and the frame's block cursor stayed drawn in 19 with the beam
    /// inside it. Swept over every anchor column (and past the grid, a frame and a resize crossing)
    /// and every kind of caret, the fill covers the cursor's cell, stays on the row, and holds every
    /// character it draws.
    #[test]
    fn the_fill_covers_the_terminals_cursor_cell_wherever_the_caret_is() {
        let end = layout_preedit("ni hao", 6, at(1, 19), 20, 5, &colors()).unwrap();
        assert_eq!(fill(&end), (1, 13, 7), "the text's six cells and the cursor's own");
        assert_eq!(end.caret_col, 19);
        assert_eq!(
            texts(&end).last(),
            Some(&(18, 1, "o".to_string())),
            "the text itself still ends one column short, leaving the caret its own cell"
        );
        for text in [
            "ni hao",
            "\u{4f60}\u{597d}",
            "abcdefghijklmnopqrst",
            "abcdefghijklmnopqrstuvwxyz",
        ] {
            let chars = text.chars().count();
            for caret in [0, 1, chars - 1, chars, 99] {
                for col in 0..23 {
                    let layout = layout_preedit(text, caret, at(1, col), 20, 5, &colors()).unwrap();
                    let (_, start, cols) = fill(&layout);
                    let cursor = col.min(19);
                    let case = format!("{text:?}, caret {caret}, cursor column {col}");
                    assert!(start <= cursor && cursor < start + cols, "{case}: fill {start}+{cols}");
                    assert!(start + cols <= 20, "{case}: the fill leaves the row");
                    for (drawn_col, drawn_cols, _) in texts(&layout) {
                        assert!(
                            start <= drawn_col && drawn_col + drawn_cols <= start + cols,
                            "{case}: a character outside the fill"
                        );
                    }
                }
            }
        }
    }

    /// Wider than the whole row: it starts at column 0 and is cut at the edge, and a wide character
    /// that would straddle the edge is not drawn at all.
    #[test]
    fn a_preedit_wider_than_the_row_is_cut_and_never_straddles_the_edge() {
        let layout = layout_preedit("\u{4f60}\u{597d}\u{5417}", 3, at(0, 2), 5, 3, &colors()).unwrap();
        assert_eq!(fill(&layout), (0, 0, 5));
        assert_eq!(
            texts(&layout),
            vec![(0, 2, "\u{4f60}".to_string()), (2, 2, "\u{597d}".to_string())]
        );
        assert_eq!(layout.caret_col, 4);
    }

    #[test]
    fn a_combining_mark_joins_the_character_before_it() {
        let layout = layout_preedit("e\u{301}x", 3, at(0, 0), 20, 5, &colors()).unwrap();
        assert_eq!(
            texts(&layout),
            vec![(0, 1, "e\u{301}".to_string()), (1, 1, "x".to_string())]
        );
        assert_eq!(layout.caret_col, 2);
    }

    /// GTK's caret is clamped to the text, and a cursor row past the grid (a frame and a resize
    /// crossing) is clamped to its last row.
    #[test]
    fn the_caret_and_the_row_are_clamped() {
        let layout = layout_preedit("ab", 99, at(9, 0), 20, 5, &colors()).unwrap();
        assert_eq!((layout.caret_row, layout.caret_col), (4, 2));
        let layout = layout_preedit("ab", 0, at(0, 0), 20, 5, &colors()).unwrap();
        assert_eq!(layout.caret_col, 0, "a caret at the start is before the first cell");
    }

    /// Whole-branch review 2026-09-24 (engine minor 5): "the same width rule the terminal uses"
    /// holds because alacritty's `Term::input` and this module both ask `unicode-width`'s
    /// `UnicodeWidthChar::width`, one version in the lockfile. Nothing else ties them, so this
    /// compares the two directly: the cells a composition takes against how far the terminal's own
    /// cursor moves for the same text, on the characters where width rules part -- CJK, a combining
    /// mark, an emoji with VS16, a regional-indicator pair, and the East Asian ambiguous `°` and `…`
    /// (checked red against a layout that uses `width_cjk`).
    #[test]
    fn a_composition_takes_the_cells_the_terminal_gives_the_same_text() {
        use crate::pty::PtySize;
        use crate::screen::Screen;
        let size = PtySize {
            cols: 40,
            rows: 3,
            cell_width_px: 9,
            cell_height_px: 18,
        };
        for text in [
            "\u{4f60}\u{597d}",
            "e\u{301}x",
            "\u{2764}\u{fe0f}",
            "\u{1f1e8}\u{1f1f3}",
            "\u{b0}\u{2026}",
            "a\u{4f60}\u{301}b\u{1f600}",
        ] {
            let mut screen = Screen::new(size, colors());
            screen.feed(text.as_bytes());
            screen.render(true);
            let printed = screen.cursor_cell().col;
            let layout = layout_preedit(text, 0, at(0, 0), size.cols, size.rows, &colors()).unwrap();
            assert_eq!(fill(&layout).2, printed, "{text:?}");
        }
    }

    #[test]
    fn nothing_to_draw_lays_out_nothing() {
        assert_eq!(layout_preedit("", 0, at(0, 0), 20, 5, &colors()), None);
        assert_eq!(layout_preedit("\u{7}\u{1b}", 0, at(0, 0), 20, 5, &colors()), None);
        assert_eq!(layout_preedit("\u{301}", 0, at(0, 0), 20, 5, &colors()), None);
        assert_eq!(layout_preedit("ab", 0, at(0, 0), 0, 5, &colors()), None);
        assert_eq!(layout_preedit("ab", 0, at(0, 0), 20, 0, &colors()), None);
    }
}
