//! Sections 8 (cursor), 9 (selection), 10 (colour/style) and 11 (wrapping).

mod common;

use alacritty_terminal::grid::Dimensions as _;
use common::{bg_at, glyph_fg, glyphs_on, Screen};
use terminal_render::color::{Palette, BRIGHT_FOREGROUND, DIM_BLACK, DIM_FOREGROUND, FOREGROUND};
use terminal_render::{CursorShape, RgbColor as Rgb};
use terminal_render::{PaintOp, SelectionSpan, ViewMode};

fn pal() -> Palette {
    Palette::xterm_default()
}

// =============================== SECTION 8 ================================

#[test]
fn the_cursor_comes_from_terminal_state_not_from_the_last_glyph() {
    // Section 8's explicit prohibition. The cursor is moved AWAY from the text
    // with an absolute position, so a renderer deriving it from the last glyph
    // painted would put it at the end of "hello" instead of where the terminal
    // says it is.
    let mut s = Screen::new(20, 4);
    s.feed("hello\x1b[3;10H");
    let list = s.paint();
    match list.cursor() {
        Some(PaintOp::DrawCursor { row, col, .. }) => {
            assert_eq!(
                (*row, *col),
                (2, 9),
                "row 3 column 10, one-based on the wire; top_line is 0 so row == line here"
            );
        }
        other => panic!("expected a cursor op, got {other:?}"),
    }
}

#[test]
fn a_hidden_cursor_paints_nothing() {
    let mut s = Screen::new(20, 4);
    s.feed("abc\x1b[?25l");
    assert!(
        s.paint().cursor().is_none(),
        "DECTCEM off must suppress the cursor op entirely"
    );
    // ...and comes back, so the test is not passing because the cursor never appears.
    s.feed("\x1b[?25h");
    assert!(s.paint().cursor().is_some(), "DECTCEM on must restore it");
}

#[test]
fn a_cursor_on_a_wide_char_covers_both_of_its_cells() {
    let mut s = Screen::new(20, 4);
    s.feed("\u{6f22}\u{5b57}\x1b[1;1H");
    match s.paint().cursor() {
        Some(PaintOp::DrawCursor { col, cols, .. }) => {
            assert_eq!(
                (*col, *cols),
                (0, 2),
                "a block cursor over a CJK glyph is two cells wide"
            );
        }
        other => panic!("expected a cursor, got {other:?}"),
    }
}

#[test]
fn the_cursor_shape_survives_being_hidden_and_shown() {
    // terminal-frame keeps shape and visibility separate, unlike upstream's
    // RenderableCursor which folds "hidden" into the shape and destroys it. The
    // renderer must preserve that distinction.
    let mut s = Screen::new(20, 4);
    s.feed("\x1b[5 q"); // blinking bar
    let before = match s.paint().cursor() {
        Some(PaintOp::DrawCursor { shape, .. }) => *shape,
        _ => panic!("cursor expected"),
    };
    assert_eq!(before, CursorShape::Beam);
    s.feed("\x1b[?25l\x1b[?25h");
    match s.paint().cursor() {
        Some(PaintOp::DrawCursor { shape, .. }) => {
            assert_eq!(*shape, before, "the chosen shape must come back unchanged")
        }
        _ => panic!("cursor expected"),
    }
}

#[test]
fn a_cursor_outside_the_rendered_window_is_not_painted() {
    // A pinned historical view does not have the cursor in it. Painting it
    // anyway would put a cursor on unrelated history.
    let mut s = Screen::with_history(20, 4, 200);
    for i in 0..40 {
        s.feed(&format!("L{i:03}\r\n"));
    }
    let far_back = s.paint_window(-30, 4, ViewMode::Pinned, &[]);
    assert!(
        far_back.cursor().is_none(),
        "the live cursor is not inside this historical window"
    );
    assert!(s.paint().cursor().is_some(), "but it is present at the live bottom");
}

// =============================== SECTION 9 ================================

#[test]
fn selection_inverts_the_cells_it_covers_and_nothing_else() {
    let mut s = Screen::new(20, 3);
    s.feed("abcdef");
    let plain = s.paint();
    let sel = s.paint_selected(&[SelectionSpan {
        line: 0,
        start_col: 1,
        end_col: 3,
    }]);

    // Inside the span: foreground and background have traded places.
    assert_eq!(
        glyph_fg(&sel, 0, 1),
        bg_at(&plain, 0, 1),
        "selected text takes the old background"
    );
    assert_eq!(
        bg_at(&sel, 0, 1),
        glyph_fg(&plain, 0, 1),
        "and the old foreground becomes the fill"
    );
    // Outside it: untouched.
    assert_eq!(glyph_fg(&sel, 0, 4), glyph_fg(&plain, 0, 4));
    assert_eq!(bg_at(&sel, 0, 4), bg_at(&plain, 0, 4));
    // And selection changes no text.
    assert_eq!(glyphs_on(&sel, 0), glyphs_on(&plain, 0));
}

#[test]
fn selection_is_view_state_and_composes_with_an_already_inverse_cell() {
    // A cell that is SGR-inverse and also selected ends up back at its original
    // colours -- two swaps. Applying selection to the RAW pair instead of the
    // resolved one would get this backwards.
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[7mab");
    let sel = s.paint_selected(&[SelectionSpan {
        line: 0,
        start_col: 0,
        end_col: 1,
    }]);
    let p = pal();
    assert_eq!(
        glyph_fg(&sel, 0, 0),
        Some(p.get(FOREGROUND)),
        "inverse then selected returns to the default foreground"
    );
}

// =============================== SECTION 10 ===============================

#[test]
fn default_foreground_and_background() {
    let mut s = Screen::new(20, 3);
    s.feed("a");
    let p = pal();
    assert_eq!(glyph_fg(&s.paint(), 0, 0), Some(p.get(FOREGROUND)));
    assert_eq!(bg_at(&s.paint(), 0, 0), Some(p.get(terminal_render::color::BACKGROUND)));
}

#[test]
fn indexed_and_truecolor_reach_the_paint_list() {
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[38;5;129ma\x1b[0m\x1b[38;2;17;34;51mb");
    let p = pal();
    assert_eq!(
        glyph_fg(&s.paint(), 0, 0),
        Some(p.get(129)),
        "SGR 38;5;129 is palette entry 129"
    );
    assert_eq!(
        glyph_fg(&s.paint(), 0, 1),
        Some(Rgb::new(17, 34, 51)),
        "SGR 38;2 is a direct colour and must not be routed through the palette"
    );
}

#[test]
fn bold_brightens_the_eight_base_colours_but_not_an_indexed_one() {
    // Alacritty's rule, and the classic bug when it is applied too widely: bold
    // turning a 256-colour prompt a different hue.
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[1;31ma\x1b[0m\x1b[1;38;5;129mb");
    let p = pal();
    assert_eq!(
        glyph_fg(&s.paint(), 0, 0),
        Some(p.get(9)),
        "bold red (1) becomes bright red (9)"
    );
    assert_eq!(
        glyph_fg(&s.paint(), 0, 1),
        Some(p.get(129)),
        "bold must NOT shift an indexed colour from the cube"
    );
}

#[test]
fn bold_brightens_the_default_foreground() {
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[1ma");
    assert_eq!(glyph_fg(&s.paint(), 0, 0), Some(pal().get(BRIGHT_FOREGROUND)));
}

#[test]
fn dim_darkens_the_base_colours_and_the_default_foreground() {
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[2;31ma\x1b[0m\x1b[2mb");
    let p = pal();
    assert_eq!(
        glyph_fg(&s.paint(), 0, 0),
        Some(p.get(DIM_BLACK + 1)),
        "dim red is the dim-red slot"
    );
    assert_eq!(glyph_fg(&s.paint(), 0, 1), Some(p.get(DIM_FOREGROUND)));
}

#[test]
fn dim_with_an_explicit_truecolor_leaves_the_colour_alone() {
    // There is no dim slot for a direct colour, and inventing one (multiplying
    // the RGB) is a renderer policy this track has not adopted. The rule is that
    // Palette indices move and Rgb values do not -- asserted so a future change
    // is deliberate.
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[2;38;2;200;100;50ma");
    assert_eq!(glyph_fg(&s.paint(), 0, 0), Some(Rgb::new(200, 100, 50)));
}

#[test]
fn inverse_swaps_the_resolved_pair_after_bold_has_been_applied() {
    // Order matters: bold adjusts the FOREGROUND, then inverse swaps. Swapping
    // first would brighten the background instead of the text, which is the most
    // common way to get this combination wrong.
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[1;7;31ma");
    let p = pal();
    let list = s.paint();
    assert_eq!(
        bg_at(&list, 0, 0),
        Some(p.get(9)),
        "the BRIGHT red bold produced is what ends up in the background after the swap"
    );
    assert_eq!(glyph_fg(&list, 0, 0), Some(p.get(terminal_render::color::BACKGROUND)));
}

#[test]
fn inverse_with_explicit_foreground_and_background() {
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[7;38;5;196;48;5;21ma");
    let p = pal();
    let list = s.paint();
    assert_eq!(glyph_fg(&list, 0, 0), Some(p.get(21)), "fg and bg trade places exactly");
    assert_eq!(bg_at(&list, 0, 0), Some(p.get(196)));
}

#[test]
fn hidden_text_keeps_its_background_and_loses_its_glyph() {
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[48;5;196m\x1b[8mabc");
    let list = s.paint();
    assert_eq!(glyphs_on(&list, 0), vec![], "SGR 8 suppresses the text");
    assert_eq!(bg_at(&list, 0, 0), Some(pal().get(196)), "but the cell is still filled");
}

#[test]
fn underline_styles_are_distinguished_rather_than_collapsed() {
    use terminal_render::UnderlineKind;
    let cases = [
        ("\x1b[4m", UnderlineKind::Single),
        ("\x1b[4:2m", UnderlineKind::Double),
        ("\x1b[4:3m", UnderlineKind::Curl),
        ("\x1b[4:4m", UnderlineKind::Dotted),
        ("\x1b[4:5m", UnderlineKind::Dashed),
    ];
    for (sgr, expected) in cases {
        let mut s = Screen::new(20, 3);
        s.feed(&format!("\x1b[0m{sgr}a"));
        let list = s.paint();
        let style = list.ops.iter().find_map(|op| match op {
            PaintOp::DrawText { col: 0, style, .. } => Some(*style),
            _ => None,
        });
        assert_eq!(style.map(|s| s.underline), Some(expected), "for {sgr:?}");
    }
}

// =============================== SECTION 11 ===============================

#[test]
fn the_renderer_paints_physical_rows_and_never_rejoins_a_wrapped_line() {
    // Section 11: reflow has already happened by the time the renderer sees the
    // frame. Its job is physical rows. A long logical line must appear as
    // several painted rows, and the split must follow the grid exactly.
    let mut s = Screen::new(10, 4);
    s.feed("ABCDEFGHIJKLMNOPQRSTUVWXY");
    let list = s.paint();
    assert_eq!(list.row_text(0), "ABCDEFGHIJ");
    assert_eq!(list.row_text(1), "KLMNOPQRST");
    assert_eq!(list.row_text(2), "UVWXY");
}

#[test]
fn narrowing_and_widening_repaint_from_the_reflowed_grid() {
    // The pixels follow the frame supplied AFTER reflow. The renderer holds no
    // memory of the previous width, so this really asserts that nothing is
    // cached across a resize.
    //
    // Note where the content ENDS UP. Narrowing to 5 columns needs 5 rows for
    // what fitted in 3, and alacritty keeps the cursor anchored near the bottom,
    // so two rows are pushed into history and the logical line now starts at
    // absolute line -2. That is section 1's rule showing up in the renderer's
    // own coordinates: a physical line number is not an identity. The window is
    // therefore taken from the top of the grid rather than from line 0.
    let mut s = Screen::new(10, 6);
    s.feed("ABCDEFGHIJKLMNOPQRSTUVWXY");
    assert_eq!(s.paint().row_text(0), "ABCDEFGHIJ");

    let rows_from_top = |s: &Screen| {
        let hist = s.term.grid().history_size();
        let list = s.paint_window(
            -(hist as i32),
            hist + s.term.screen_lines(),
            ViewMode::FollowBottom,
            &[],
        );
        (0..(hist + s.term.screen_lines()) as u16)
            .map(|r| list.row_text(r))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
    };

    s.resize(5, 6);
    assert_eq!(
        rows_from_top(&s),
        vec!["ABCDE", "FGHIJ", "KLMNO", "PQRST", "UVWXY"],
        "narrowing re-folds into five physical rows, in order"
    );
    assert_eq!(
        s.term.grid().history_size(),
        2,
        "and two of them were pushed above line 0"
    );

    s.resize(25, 6);
    assert_eq!(
        rows_from_top(&s),
        vec!["ABCDEFGHIJKLMNOPQRSTUVWXY"],
        "widening re-joins the row because the GRID did, not because the renderer did"
    );
}
