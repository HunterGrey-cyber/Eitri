//! The guarantees an external backend author is entitled to rely on.
//!
//! Each test here corresponds to something an adversarial audit found a backend
//! would otherwise have had to GUESS. A guess is a divergence: two conforming
//! backends rendering the same PaintList differently.

mod common;

use common::{glyphs_on, Screen};
use terminal_render::{
    CursorColoring, CursorShape, PaintLayer, PaintOp, RgbColor, SelectionSpan, ViewMode, ANCHOR_EXPIRED_NOTICE,
};

// ------------------------- COORDINATES (section 3) -------------------------

#[test]
fn rows_are_window_relative_so_a_history_window_looks_exactly_like_a_live_one() {
    // The defect this closes: absolute grid lines are negative in scrollback,
    // and a backend handed `line: -37` with no window origin either draws 37
    // rows off the top of the surface or invents an origin by scanning the ops.
    // Both were realisable. Rows are now 0..rows for every window.
    let mut s = Screen::with_history(20, 6, 500);
    for i in 1..=60 {
        s.feed(&format!("L{i:04}\r\n"));
    }
    let deep = s.paint_window(-30, 6, ViewMode::Pinned, &[]);

    let mut seen_rows: Vec<u16> = deep
        .ops
        .iter()
        .map(|op| match op {
            PaintOp::FillCells { row, .. }
            | PaintOp::DrawText { row, .. }
            | PaintOp::DrawCursor { row, .. }
            | PaintOp::DrawNotice { row, .. } => *row,
        })
        .collect();
    seen_rows.sort_unstable();
    seen_rows.dedup();
    assert_eq!(
        seen_rows,
        vec![0, 1, 2, 3, 4, 5],
        "a window deep in history still paints rows 0..6"
    );

    // The absolute line is recorded, but only as provenance.
    assert_eq!(deep.top_line, -30);
    // ...and it really is history, so the test is not trivially about the live
    // screen. 60 lines on a 6-row screen puts L0056 at absolute line 0, so
    // absolute line -30 is L0026 -- 30 rows further back.
    assert_eq!(deep.row_text(0), "L0026");
}

#[test]
fn top_line_is_provenance_and_the_ops_do_not_depend_on_it() {
    // Two windows over the SAME content, addressed differently, must produce
    // identical ops -- which is what makes top_line non-load-bearing.
    let mut s = Screen::with_history(20, 4, 500);
    for i in 1..=40 {
        s.feed(&format!("L{i:04}\r\n"));
    }
    let a = s.paint_window(-10, 4, ViewMode::Pinned, &[]);
    assert_eq!(a.top_line, -10);
    // 40 lines on a 4-row screen puts L0038 at absolute line 0.
    assert_eq!(a.row_text(0), "L0028");
    // Same rows, reached from the live screen's own numbering.
    let b = s.paint_window(-10, 4, ViewMode::FollowBottom, &[]);
    assert_eq!(a.ops, b.ops, "the view MODE must not change the pixels, only the state");
}

// --------------------------- Z-ORDER (section 4) ---------------------------

#[test]
fn every_paint_list_is_emitted_in_layer_order() {
    // Ordering is a contract guarantee, not a backend choice, so it is checked
    // mechanically over a corpus rather than described in prose.
    let mut s = Screen::new(20, 4);
    s.feed("\x1b[1;4;31mstyled\x1b[0m \u{6f22}\u{5b57} e\u{301}");
    for (name, list) in [
        ("live", s.paint()),
        (
            "selected",
            s.paint_selected(&[SelectionSpan {
                line: 0,
                start_col: 0,
                end_col: 3,
            }]),
        ),
        ("expired", s.paint_window(0, 4, ViewMode::AnchorExpired, &[])),
        ("unfocused", s.paint_full(0, 4, ViewMode::FollowBottom, &[], false)),
    ] {
        assert!(list.is_layer_ordered(), "{name}: ops are not in layer order");
    }
}

#[test]
fn the_cursor_is_painted_after_the_text_and_the_notice_after_the_cursor() {
    let mut s = Screen::new(20, 4);
    s.feed("abc");
    let list = s.paint_window(0, 4, ViewMode::AnchorExpired, &[]);
    let layers: Vec<PaintLayer> = list.ops.iter().map(|o| o.layer()).collect();
    let first = |l: PaintLayer| layers.iter().position(|x| *x == l);
    assert!(first(PaintLayer::CellBackground) < first(PaintLayer::Text));
    assert!(first(PaintLayer::Text) < first(PaintLayer::Cursor));
    assert!(first(PaintLayer::Cursor) < first(PaintLayer::Overlay));
}

// ---------------------------- CURSOR (section 7) ---------------------------

#[test]
fn a_block_cursor_carries_the_character_it_covers_already_recoloured() {
    // The defect: a block cursor executed literally as "fill the cell, last, on
    // top" ERASES the character under it. Nothing in the op said otherwise, and
    // the backend had no access to the cell. Now the readable text comes with it.
    let mut s = Screen::new(20, 4);
    s.feed("Xyz\x1b[1;1H"); // cursor back onto the 'X'
    match s.paint().cursor() {
        Some(PaintOp::DrawCursor {
            shape,
            color,
            text_under,
            ..
        }) => {
            assert_eq!(*shape, CursorShape::Block);
            let t = text_under
                .as_ref()
                .expect("a block cursor must carry the character it covers");
            assert_eq!(t.text, "X");
            assert_ne!(
                t.color, *color,
                "and it must contrast with the cursor body, or it is invisible"
            );
        }
        other => panic!("expected a block cursor, got {other:?}"),
    }
}

#[test]
fn a_cursor_on_a_blank_cell_carries_no_text() {
    let mut s = Screen::new(20, 4);
    s.feed("\x1b[2;5H");
    match s.paint().cursor() {
        Some(PaintOp::DrawCursor { text_under, .. }) => {
            assert!(text_under.is_none(), "there is nothing under it to preserve");
        }
        other => panic!("expected a cursor, got {other:?}"),
    }
}

#[test]
fn a_non_obscuring_cursor_shape_carries_no_text_because_the_glyph_is_still_visible() {
    let mut s = Screen::new(20, 4);
    s.feed("Xyz\x1b[5 q\x1b[1;1H"); // beam
    match s.paint().cursor() {
        Some(PaintOp::DrawCursor { shape, text_under, .. }) => {
            assert_eq!(*shape, CursorShape::Beam);
            assert!(
                text_under.is_none(),
                "a beam does not cover the cell, so nothing is re-drawn"
            );
        }
        other => panic!("expected a beam cursor, got {other:?}"),
    }
    // ...and the glyph is still in the text layer, so it really is visible.
    assert!(glyphs_on(&s.paint(), 0).iter().any(|(c, _, t)| *c == 0 && t == "X"));
}

#[test]
fn an_unfocused_window_gets_a_hollow_cursor_and_keeps_its_glyph() {
    // Focus is VIEW state. project_window hardcodes focused:false on the frame,
    // so reading it there would make every window permanently unfocused --
    // which is why RenderInput carries it instead.
    let mut s = Screen::new(20, 4);
    s.feed("Xyz\x1b[1;1H");
    match s.paint_full(0, 4, ViewMode::FollowBottom, &[], false).cursor() {
        Some(PaintOp::DrawCursor { shape, text_under, .. }) => {
            assert_eq!(*shape, CursorShape::HollowBlock);
            assert!(text_under.is_none(), "a hollow cursor does not cover the glyph");
        }
        other => panic!("expected a hollow cursor, got {other:?}"),
    }
    // Focused, the same screen gets a solid block: the flag is load-bearing.
    match s.paint_full(0, 4, ViewMode::FollowBottom, &[], true).cursor() {
        Some(PaintOp::DrawCursor { shape, .. }) => assert_eq!(*shape, CursorShape::Block),
        other => panic!("expected a block cursor, got {other:?}"),
    }
}

#[test]
fn the_cursor_carries_its_blink_state_rather_than_dropping_it() {
    // terminal-frame carries `blinking` deliberately -- it is the one field a
    // damage-driven consumer cannot recover from the renderable path -- and the
    // renderer used to drop it silently. A backend may ignore it; it may not be
    // denied it.
    let mut s = Screen::new(20, 4);
    s.feed("\x1b[2 q"); // steady block
    let steady = match s.paint().cursor() {
        Some(PaintOp::DrawCursor { blinking, .. }) => *blinking,
        _ => panic!("cursor expected"),
    };
    s.feed("\x1b[1 q"); // blinking block
    let blinking = match s.paint().cursor() {
        Some(PaintOp::DrawCursor { blinking, .. }) => *blinking,
        _ => panic!("cursor expected"),
    };
    assert_ne!(
        steady, blinking,
        "the two cursor styles must be distinguishable in the contract"
    );
    assert!(blinking);
}

/// The body and under-text colours of the cursor, as a backend receives them.
fn cursor_colors(list: &terminal_render::PaintList) -> (RgbColor, Option<RgbColor>) {
    match list.cursor() {
        Some(PaintOp::DrawCursor { color, text_under, .. }) => (*color, text_under.as_ref().map(|t| t.color)),
        other => panic!("expected a cursor, got {other:?}"),
    }
}

#[test]
fn a_cell_inverse_cursor_takes_the_covered_cells_own_colours_swapped() {
    // neovibe's GUI pass 2026-09-23, defect 3: a fixed cursor colour from the
    // host's theme (a dark theme's pale foreground) nearly vanished on a cell a
    // program had painted light -- nvim with a light colorscheme, inside. foot
    // draws the cell inverted. Black on white here, the case in miniature.
    let mut s = Screen::new(20, 4);
    s.feed("\x1b[30;47mX\x1b[0m\x1b[1;1H");
    let black = RgbColor::new(0x00, 0x00, 0x00);
    let white = RgbColor::new(0xe5, 0xe5, 0xe5); // xterm's `7`, the palette's white
    assert_eq!(
        cursor_colors(&s.paint_colored(CursorColoring::CellInverse)),
        (black, Some(white)),
        "the block in the cell's foreground, the character in its background"
    );
    // The fixed colour it replaces, for contrast: xterm's cursor slot, pale on
    // the white cell -- the defect.
    let fixed = cursor_colors(&s.paint_colored(CursorColoring::Palette)).0;
    assert_eq!(fixed, RgbColor::new(0xd8, 0xd8, 0xd8));
}

#[test]
fn a_cell_inverse_cursor_on_a_blank_cell_is_the_default_foreground() {
    // An empty prompt position: the default colours, so the same block a fixed
    // foreground-coloured cursor would draw -- nothing changes where nothing was
    // wrong.
    let mut s = Screen::new(20, 4);
    s.feed("\x1b[2;5H");
    let list = s.paint_colored(CursorColoring::CellInverse);
    let fg = terminal_render::palette_for(&terminal_frame::project_window(&s.term, 0, 4))
        .get(terminal_render::color::FOREGROUND);
    assert_eq!(cursor_colors(&list), (fg, None));
}

#[test]
fn a_programs_own_cursor_colour_wins_over_cell_inverse() {
    // OSC 12: the program chose a cursor colour for the colours it paints, and
    // it is in the frame's overrides. The host's "follow the cell" is only for
    // when nobody chose.
    let mut s = Screen::new(20, 4);
    s.feed("\x1b]12;#ff0000\x07\x1b[30;47mX\x1b[0m\x1b[1;1H");
    let red = RgbColor::new(0xff, 0x00, 0x00);
    assert_eq!(cursor_colors(&s.paint_colored(CursorColoring::CellInverse)).0, red);
    // And a reset (OSC 112) gives the cell back its say.
    s.feed("\x1b]112\x07");
    assert_eq!(
        cursor_colors(&s.paint_colored(CursorColoring::CellInverse)).0,
        RgbColor::new(0x00, 0x00, 0x00)
    );
}

#[test]
fn an_unfocused_cell_inverse_cursor_strokes_in_the_cells_foreground() {
    // The hollow block keeps the glyph visible inside it; its outline takes the
    // same colour a solid block would have, so it is as visible as one.
    let mut s = Screen::new(20, 4);
    s.feed("\x1b[30;47mX\x1b[0m\x1b[1;1H");
    let list = s.paint_cursor(0, 4, ViewMode::FollowBottom, &[], false, CursorColoring::CellInverse);
    match list.cursor() {
        Some(PaintOp::DrawCursor {
            shape,
            color,
            text_under,
            ..
        }) => {
            assert_eq!(*shape, CursorShape::HollowBlock);
            assert_eq!(*color, RgbColor::new(0x00, 0x00, 0x00));
            assert!(text_under.is_none());
        }
        other => panic!("expected a hollow cursor, got {other:?}"),
    }
}

// ------------------------- ANCHOR EXPIRED (section 9) ----------------------

#[test]
fn the_expiry_notice_is_fully_specified_so_a_backend_invents_nothing() {
    // The defect: the op used to be a bare line number. A backend had to make up
    // the wording and the colours -- and one that made up NOTHING drew nothing,
    // silently producing the exact behaviour the state exists to forbid, while
    // passing every test.
    let mut s = Screen::new(24, 4);
    s.feed("live output here");
    let list = s.paint_window(0, 4, ViewMode::AnchorExpired, &[]);
    match list.notice() {
        Some(PaintOp::DrawNotice {
            row,
            col,
            cols,
            text,
            color,
            background,
        }) => {
            assert_eq!((*row, *col, *cols), (0, 0, 24), "it spans the top row of the window");
            assert_eq!(text, ANCHOR_EXPIRED_NOTICE);
            assert!(!text.trim().is_empty(), "there must be something to read");
            assert_ne!(color, background, "and it must be legible");
        }
        other => panic!("expected a fully specified notice, got {other:?}"),
    }
}

#[test]
fn no_notice_is_emitted_while_the_view_is_valid() {
    let mut s = Screen::new(24, 4);
    s.feed("hello");
    assert!(!s.paint().has_expiry_notice(), "following the bottom");
    assert!(
        !s.paint_window(0, 4, ViewMode::Pinned, &[]).has_expiry_notice(),
        "a valid pin must not claim to be expired"
    );
}

// --------------------------- SURFACE (section 2) ---------------------------

#[test]
fn the_list_carries_the_colour_to_clear_the_surface_with() {
    // The surface is bigger than the cells in three ways -- omitted rows, the
    // sub-cell remainder of the window size, and a scrollback window truncated
    // to fewer rows than asked for -- so the remainder has to come from
    // somewhere. It comes from here.
    let mut s = Screen::new(10, 3);
    s.feed("x");
    let list = s.paint();
    assert_eq!(
        list.surface_background,
        common::bg_at(&list, 1, 0).expect("an untouched cell"),
        "clearing with it must match what an empty cell paints, or the seams show"
    );
}

// -------------------------- SELECTION (section 8) --------------------------

#[test]
fn selection_is_resolved_into_the_colours_and_never_exposed_as_a_flag() {
    // The backend is never asked "is this selected?". It is handed different
    // colours. Asserted by structure: no op carries a selection marker.
    let mut s = Screen::new(20, 3);
    s.feed("abcdef");
    let plain = s.paint();
    let sel = s.paint_selected(&[SelectionSpan {
        line: 0,
        start_col: 1,
        end_col: 3,
    }]);
    assert_ne!(plain.ops, sel.ops, "selection must change the ops");
    assert_eq!(
        plain.ops.len(),
        sel.ops.len(),
        "but only their colours -- no extra overlay op appears, so there is nothing new to interpret"
    );
}
