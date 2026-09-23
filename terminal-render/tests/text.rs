//! Section 6 (wide characters) and section 7 (combining marks).
//!
//! Both are cases where "the obvious renderer" is wrong in a way no colour or
//! layout test catches, because the pixels are plausible -- just not the text
//! the terminal holds.

mod common;

use common::{glyphs_on, Screen};

// =============================== SECTION 6 ================================

#[test]
fn ascii_is_one_glyph_per_cell() {
    let mut s = Screen::new(20, 3);
    s.feed("abc");
    assert_eq!(
        glyphs_on(&s.paint(), 0),
        vec![(0, 1, "a".into()), (1, 1, "b".into()), (2, 1, "c".into())]
    );
}

#[test]
fn a_full_width_char_is_one_glyph_over_two_cells_with_no_glyph_on_the_spacer() {
    // THE wide-character rule: one glyph, one draw, two columns of advance. The
    // failure this guards is drawing the WIDE_CHAR_SPACER as its own glyph,
    // which puts a second (usually blank or duplicated) glyph beside every CJK
    // character. Asserting only the text would miss it; asserting the COLUMNS
    // is what catches it.
    let mut s = Screen::new(20, 3);
    s.feed("\u{6f22}\u{5b57}"); // 漢字
    let g = glyphs_on(&s.paint(), 0);
    assert_eq!(
        g,
        vec![(0, 2, "\u{6f22}".into()), (2, 2, "\u{5b57}".into())],
        "two wide glyphs at columns 0 and 2, each two cells wide, and NOTHING at 1 or 3"
    );
}

#[test]
fn mixed_ascii_and_cjk_advance_by_cells_not_by_scalar_count() {
    // The explicit warning in section 6: scalar count is not cell count. Here
    // four scalars occupy six columns, and the ASCII after the CJK must land at
    // column 5 -- not column 3, which is where a scalar-indexed renderer puts it.
    let mut s = Screen::new(20, 3);
    s.feed("a\u{6f22}\u{5b57}b");
    let g = glyphs_on(&s.paint(), 0);
    assert_eq!(
        g,
        vec![
            (0, 1, "a".into()),
            (1, 2, "\u{6f22}".into()),
            (3, 2, "\u{5b57}".into()),
            (5, 1, "b".into()),
        ]
    );
    let scalars = "a\u{6f22}\u{5b57}b".chars().count();
    assert_eq!(scalars, 4, "four scalars...");
    assert_eq!(
        g.iter().map(|(_, cells, _)| cells).sum::<u16>(),
        6,
        "...but six terminal cells"
    );
}

#[test]
fn a_wide_char_that_cannot_fit_wraps_whole_and_leaves_no_glyph_in_the_padding_cell() {
    // A full-width glyph never straddles the right edge. With one column left,
    // alacritty writes a LEADING_WIDE_CHAR_SPACER into it and puts the glyph at
    // the start of the next row. The padding cell must paint background only --
    // rendering it as a glyph produces a stray mark in the last column.
    let mut s = Screen::new(5, 3);
    s.feed("abcd\u{6f22}"); // 4 ASCII then a wide char: only column 4 is free
    let list = s.paint();
    let row0 = glyphs_on(&list, 0);
    assert_eq!(
        row0,
        vec![
            (0, 1, "a".into()),
            (1, 1, "b".into()),
            (2, 1, "c".into()),
            (3, 1, "d".into())
        ],
        "column 4 holds the leading spacer and must produce no glyph"
    );
    assert_eq!(
        glyphs_on(&list, 1),
        vec![(0, 2, "\u{6f22}".into())],
        "the wide glyph moved whole to the next row"
    );
}

#[test]
fn overwriting_half_of_a_wide_char_leaves_no_orphan_glyph() {
    // Writing over one cell of a wide pair must not leave the other half
    // painting the original glyph. alacritty clears both cells; the renderer
    // must follow whatever the grid says rather than caching a previous run.
    let mut s = Screen::new(20, 3);
    s.feed("\u{6f22}\u{5b57}");
    assert_eq!(glyphs_on(&s.paint(), 0).len(), 2);

    // Return to column 1 -- the spacer of the first wide char -- and write ASCII.
    s.feed("\x1b[1;2HX");
    let g = glyphs_on(&s.paint(), 0);
    assert!(
        !g.iter().any(|(_, _, t)| t == "\u{6f22}"),
        "the clobbered wide glyph must be gone entirely, not left half-drawn: {g:?}"
    );
    assert!(
        g.iter().any(|(c, _, t)| *c == 1 && t == "X"),
        "the overwrite lands at column 1: {g:?}"
    );
    for (col, cells, text) in &g {
        assert!(
            col + cells <= 20,
            "glyph {text:?} at {col} claims {cells} cells and runs off the row"
        );
    }
}

#[test]
fn no_two_glyphs_ever_claim_the_same_column() {
    // The invariant behind every case above, checked over a mixed corpus: cell
    // occupancy must partition. A double-drawn spacer or a mis-sized wide glyph
    // shows up here as an overlap even if the individual assertions were written
    // to the wrong expectation.
    let mut s = Screen::new(24, 4);
    s.feed("ab\u{6f22}cd\u{5b57}\u{ff21}z");
    let list = s.paint();
    for line in 0..4u16 {
        let mut claimed = vec![false; 24];
        for (col, cells, text) in glyphs_on(&list, line) {
            for c in col..col + cells {
                assert!(
                    !claimed[c as usize],
                    "line {line}: column {c} claimed twice; {text:?} overlaps an earlier glyph"
                );
                claimed[c as usize] = true;
            }
        }
    }
}

// =============================== SECTION 7 ================================

#[test]
fn a_combining_mark_rides_on_the_glyph_it_modifies() {
    // The frame carries combining marks in CellExtras.zerowidth. Dropping them
    // renders "e" where the terminal holds "e" + U+0301 -- silent text
    // corruption, and the reason section 7 demands this be proven rather than
    // assumed.
    let mut s = Screen::new(20, 3);
    s.feed("e\u{301}x");
    let g = glyphs_on(&s.paint(), 0);
    assert_eq!(
        g,
        vec![(0, 1, "e\u{301}".into()), (1, 1, "x".into())],
        "the mark must be part of the glyph run's text, and must NOT consume a column"
    );
}

#[test]
fn stacked_combining_marks_are_all_carried_in_order() {
    let mut s = Screen::new(20, 3);
    s.feed("e\u{301}\u{308}\u{327}");
    assert_eq!(glyphs_on(&s.paint(), 0), vec![(0, 1, "e\u{301}\u{308}\u{327}".into())]);
}

#[test]
fn a_combining_mark_on_a_wide_char_keeps_both_the_mark_and_the_two_cell_width() {
    // The two hard cases at once. The mark is stored two columns back (past the
    // WIDE_CHAR_SPACER), so a renderer that looks one column left loses it.
    let mut s = Screen::new(20, 3);
    s.feed("\u{6f22}\u{301}");
    assert_eq!(
        glyphs_on(&s.paint(), 0),
        vec![(0, 2, "\u{6f22}\u{301}".into())],
        "one glyph, two cells, mark attached"
    );
}

#[test]
fn a_space_carrying_a_combining_mark_is_still_painted() {
    // The blank-cell skip is an optimisation, and this is where a naive version
    // of it loses data: the cell's char is ' ', but the cell is not empty.
    let mut s = Screen::new(20, 3);
    s.feed(" \u{301}");
    let g = glyphs_on(&s.paint(), 0);
    assert_eq!(
        g,
        vec![(0, 1, " \u{301}".into())],
        "a marked space must not be skipped as blank"
    );
}

#[test]
fn an_unmarked_space_is_skipped() {
    // The other half, so the skip is proven to still apply where it should --
    // otherwise the test above would pass on a renderer that paints every blank
    // cell in the grid.
    let mut s = Screen::new(20, 3);
    s.feed("a b");
    assert_eq!(
        glyphs_on(&s.paint(), 0),
        vec![(0, 1, "a".into()), (2, 1, "b".into())],
        "column 1 is an undecorated blank and produces no glyph op"
    );
}

#[test]
fn a_decorated_wide_char_paints_no_second_run_on_its_spacer() {
    // The case that makes the spacer check load-bearing rather than redundant.
    //
    // A WIDE_CHAR_SPACER's char is ' ', so the ordinary blank-cell skip already
    // drops an UNDECORATED one -- which means the spacer rule looks dead until
    // SGR enters the picture. Measured: after `SGR 4;9` on a fullwidth char the
    // grid holds
    //     col 0: '漢' WIDE_CHAR | UNDERLINE | STRIKEOUT
    //     col 1: ' '  WIDE_CHAR_SPACER | UNDERLINE | STRIKEOUT
    // so the spacer is NOT blank by the decoration test, and without the spacer
    // rule it paints a second underlined, struck-through run right beside the
    // glyph -- a visible double-drawn decoration.
    //
    // Found by mutation: disabling the spacer check left every other test in
    // this file green.
    let mut s = Screen::new(20, 3);
    s.feed("\x1b[4;9m\u{6f22}");
    let g = glyphs_on(&s.paint(), 0);
    assert_eq!(
        g,
        vec![(0, 2, "\u{6f22}".into())],
        "exactly one run: the decorated spacer must not become a second glyph"
    );
}
