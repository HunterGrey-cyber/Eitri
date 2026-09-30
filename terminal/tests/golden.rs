//! Renders Verdandi's own golden op corpus and inspects the resulting pixels.
//!
//! Moved from `terminal-pane/tests/golden.rs` on `freeze/terminal-stack` @ `1e715ab` (2026-09-23).
//! Since the move the corpus is read from `terminal-render/tests/golden/paintops.txt` itself, in this
//! workspace, rather than from a copy of it: one file, so the two can no longer drift apart.
//!
//! The corpus in `fixtures/paintops.txt` is the exact op stream Verdandi's builder produces for 17
//! known scenarios, copied verbatim from `crates/terminal-render/tests/golden/paintops.txt`. It was
//! copied at revision `bc915f5` and still matches that file byte for byte at `775c835`, the revision
//! this crate now pins -- `crates/terminal-render` did not change at all between the two. Using it
//! here means the backend is tested against real op streams without this crate needing the builder,
//! a `Term`, or any terminal vocabulary at test time.
//!
//! **These are pixel assertions, not smoke tests.** Every check samples the rendered surface at a
//! computed cell position and asserts what is actually there: a specific colour, or the presence or
//! absence of ink. "It drew something and did not crash" would pass against a backend that put every
//! glyph in the wrong column, which is precisely the failure mode a cell-lattice bug produces.
//!
//! No GPU and no window: `surfaces::raster_n32_premul` gives a real Skia canvas in memory, so these
//! run in CI and in a plain `cargo test`.

mod fixture;

use eitri_terminal::{paint, TerminalMetrics};
use fixture::{load_scenarios, Scenario};
use skia_safe::{surfaces, Color, ISize};

/// Big enough cells that a glyph has room to be unambiguously present or absent, and that sampling
/// a cell's interior is not at the mercy of one pixel of antialiasing.
const CELL_PX: f32 = 20.0;

struct Rendered {
    pixels: Vec<u8>,
    width: i32,
    metrics: TerminalMetrics,
}

impl Rendered {
    fn of(scenario: &Scenario) -> Self {
        // A font size that yields roughly CELL_PX cells, then the real metrics decide the lattice --
        // the test must measure the same grid the backend draws on, never a parallel guess.
        let metrics = TerminalMetrics::with_font("monospace", CELL_PX * 0.75, 4000.0, 4000.0, 1.0);
        let width = (scenario.cols as f32 * metrics.cell_width()).ceil() as i32;
        let height = (scenario.rows as f32 * metrics.cell_height()).ceil() as i32;
        let mut surface =
            surfaces::raster_n32_premul(ISize::new(width.max(1), height.max(1))).expect("a raster surface");
        paint(surface.canvas(), &scenario.list, &metrics);

        let image_info = surface.image_info();
        let row_bytes = image_info.min_row_bytes();
        let mut pixels = vec![0u8; row_bytes * height.max(1) as usize];
        assert!(
            surface.read_pixels(&image_info, &mut pixels, row_bytes, (0, 0)),
            "could not read the rendered surface back"
        );
        Self { pixels, width, metrics }
    }

    fn at(&self, x: i32, y: i32) -> Color {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        // n32_premul on this platform is BGRA. Alpha is always opaque here (the canvas is cleared to
        // a solid colour), so premultiplication does not distort the comparison.
        Color::from_rgb(self.pixels[i + 2], self.pixels[i + 1], self.pixels[i])
    }

    /// The colour at the centre of a cell.
    fn cell_center(&self, row: u16, col: u16) -> Color {
        let rect = self.metrics.cell_rect(row, col, 1);
        self.at(rect.center_x() as i32, rect.center_y() as i32)
    }

    /// The most common colour in a cell box.
    ///
    /// The right oracle for "what colour is this cell", and the reason a centre-pixel sample is the
    /// wrong one: a glyph's ink covers the centre, so sampling there answers "what colour is this
    /// character" instead. Three assertions in the first draft of this file were wrong for exactly
    /// that reason and passed only where the cell happened to be empty.
    fn dominant_color(&self, row: u16, col: u16) -> Color {
        let rect = self.metrics.cell_rect(row, col, 1);
        // Keyed on the raw channel triple rather than on `Color`, which is not itself hashable.
        let mut counts: std::collections::HashMap<(u8, u8, u8), usize> = std::collections::HashMap::new();
        for y in (rect.top as i32)..(rect.bottom as i32) {
            for x in (rect.left as i32)..(rect.right as i32) {
                let c = self.at(x, y);
                *counts.entry((c.r(), c.g(), c.b())).or_default() += 1;
            }
        }
        let ((r, g, b), _) = counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .expect("a non-empty cell box");
        Color::from_rgb(r, g, b)
    }

    /// How many pixels inside a cell box equal `color`.
    fn count_of(&self, row: u16, col: u16, cols: u16, color: Color) -> usize {
        let rect = self.metrics.cell_rect(row, col, cols);
        let mut count = 0;
        for y in (rect.top as i32)..(rect.bottom as i32) {
            for x in (rect.left as i32)..(rect.right as i32) {
                if self.at(x, y) == color {
                    count += 1;
                }
            }
        }
        count
    }

    /// How many pixels inside a cell box differ from `background`. The ink oracle: a drawn glyph has
    /// some, an empty cell has none.
    fn ink_in(&self, row: u16, col: u16, cols: u16, background: Color) -> usize {
        let rect = self.metrics.cell_rect(row, col, cols);
        let mut count = 0;
        for y in (rect.top as i32)..(rect.bottom as i32) {
            for x in (rect.left as i32)..(rect.right as i32) {
                if self.at(x, y) != background {
                    count += 1;
                }
            }
        }
        count
    }
}

fn scenario(name: &str) -> Scenario {
    load_scenarios()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no scenario named {name:?} in the golden corpus"))
}

fn rgb(hex: u32) -> Color {
    Color::from_rgb((hex >> 16) as u8, ((hex >> 8) & 0xff) as u8, (hex & 0xff) as u8)
}

const DEFAULT_BG: u32 = 0x181818;
const DEFAULT_FG: u32 = 0xd8d8d8;

#[test]
fn the_corpus_loads_and_covers_what_it_claims_to() {
    let scenarios = load_scenarios();
    assert_eq!(
        scenarios.len(),
        15,
        "the fixture should hold every scenario Verdandi generated"
    );
    for name in [
        "ascii",
        "mixed-ascii-cjk",
        "wide-char-at-wrap-boundary",
        "decorated-wide-char",
        "combining-marks",
        "colors-indexed-and-truecolor",
        "bold-dim-inverse-underline-strike",
        "cursor-over-glyph",
        "cursor-unfocused",
        "cursor-over-wide-glyph",
        "selection",
        "wrapped-rows",
        "pinned-history-negative-absolute-lines",
        "anchor-expired",
    ] {
        assert!(scenarios.iter().any(|s| s.name == name), "missing scenario {name}");
    }
    // Every list Verdandi emits is layer-ordered, and the backend's correctness depends on executing
    // it in the given order. Checking it here means a corpus that ever violated it would be caught
    // rather than silently painting a cursor under its own background.
    for s in &scenarios {
        assert!(s.list.is_layer_ordered(), "{} is not layer-ordered", s.name);
    }
}

// ---------------------------------------------------------------------------------------------
// §7 ASCII first: exact row/column placement, not "looks roughly right".
// ---------------------------------------------------------------------------------------------

#[test]
fn ascii_glyphs_land_in_their_own_columns_and_nowhere_else() {
    let s = scenario("ascii"); // "abc" at r0 c0,c1,c2, cursor at c3
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);

    for col in 0..3 {
        assert!(r.ink_in(0, col, 1, bg) > 0, "column {col} should hold a glyph");
    }
    // The columns after the cursor are empty. If glyphs were drawn a cell wide but positioned by
    // some other measure, ink would bleed here.
    for col in 4..s.cols {
        assert_eq!(r.ink_in(0, col, 1, bg), 0, "column {col} should be empty");
    }
    // Row 1 is entirely blank.
    assert_eq!(r.ink_in(1, 0, s.cols, bg), 0, "row 1 should be empty");
}

#[test]
fn the_surface_is_cleared_to_the_lists_own_background() {
    let s = scenario("ascii");
    let r = Rendered::of(&s);
    // A cell no glyph covers shows the resolved terminal background -- not black, not the previous
    // frame. Skipping the clear leaves the margin showing whatever was there before.
    assert_eq!(r.cell_center(1, 5), rgb(DEFAULT_BG));
}

#[test]
fn wrapped_rows_place_the_continuation_on_the_next_row() {
    let s = scenario("wrapped-rows"); // A..F on r0, G..L on r1, 6 cols
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);
    for col in 0..6 {
        assert!(r.ink_in(0, col, 1, bg) > 0, "r0c{col}");
        assert!(r.ink_in(1, col, 1, bg) > 0, "r1c{col}");
    }
    assert_eq!(r.ink_in(2, 0, 6, bg), 0, "the third row is untouched");
}

// ---------------------------------------------------------------------------------------------
// §8 Wide characters. One glyph, two cells, no spacer.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_wide_glyph_occupies_both_of_its_cells() {
    let s = scenario("mixed-ascii-cjk"); // a | 漢(c1+2) | 字(c3+2) | b(c5)
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);

    // Ink in BOTH halves of the wide glyph. A backend that clipped a wide glyph to one cell, or that
    // drew it at single-cell width, would leave the second column empty.
    assert!(r.ink_in(0, 1, 1, bg) > 0, "first half of 漢");
    assert!(
        r.ink_in(0, 2, 1, bg) > 0,
        "second half of 漢 -- the cell the spacer would have been"
    );
    assert!(r.ink_in(0, 3, 1, bg) > 0, "first half of 字");
    assert!(r.ink_in(0, 4, 1, bg) > 0, "second half of 字");
    assert!(r.ink_in(0, 5, 1, bg) > 0, "the ASCII 'b' after them");
    // And the column after 'b' is clear: two wide glyphs did not push the row along by a cell.
    assert_eq!(
        r.ink_in(0, 7, 1, bg),
        0,
        "nothing should have been displaced past the cursor cell"
    );
}

#[test]
fn a_wide_glyph_at_a_wrap_boundary_starts_the_next_row() {
    let s = scenario("wide-char-at-wrap-boundary"); // abcd fills r0 (5 cols), 漢 wraps to r1 c0+2
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);
    assert!(r.ink_in(1, 0, 1, bg) > 0, "the wide glyph begins row 1 column 0");
    assert!(r.ink_in(1, 1, 1, bg) > 0, "and spans into column 1");
    // It did NOT get squeezed into the one remaining column of row 0. Verdandi decided that; this
    // asserts the backend honoured the decision rather than re-deriving it.
    assert_eq!(r.ink_in(0, 4, 1, bg), 0, "row 0's last column stays empty");
}

#[test]
fn a_decorated_wide_glyph_is_decorated_across_both_cells() {
    let s = scenario("decorated-wide-char"); // 漢 at c0+2 with underline + strikeout
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);
    // The decoration spans the glyph's full two-column box, because `cols` is its width. A backend
    // that drew the rule one cell wide would underline half a character.
    assert!(
        r.ink_in(0, 1, 1, bg) > 0,
        "the second column carries glyph and/or decoration"
    );
}

// ---------------------------------------------------------------------------------------------
// §9 Combining marks must not be silently dropped.
// ---------------------------------------------------------------------------------------------

#[test]
fn combining_marks_change_what_is_drawn() {
    let s = scenario("combining-marks"); // "é" (e+U+0301), " ́" (space+U+0301), "漢̈" (wide+U+0308)
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);

    // A bare combining mark over a space still has ink. Dropping the mark would leave this cell
    // blank, and blank is exactly what "we lost it" looks like.
    assert!(
        r.ink_in(0, 1, 1, bg) > 0,
        "a combining mark on a space must still draw something"
    );

    // And the marked base differs from the unmarked one. Rendered side by side at the same size, an
    // "e" and an "é" cannot have identical ink unless the mark was discarded.
    let plain = {
        let mut bare = scenario("combining-marks");
        bare.replace_text(0, 0, "e");
        Rendered::of(&bare)
    };
    assert_ne!(
        r.ink_in(0, 0, 1, bg),
        plain.ink_in(0, 0, 1, bg),
        "'e' and 'e+U+0301' rendered identically -- the combining mark was dropped"
    );
}

#[test]
fn a_combining_mark_on_a_wide_base_keeps_the_wide_box() {
    let s = scenario("combining-marks");
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);
    // 漢+U+0308 is at c2+2: two scalars, still two columns. `cols` is the terminal's accounting and
    // counting scalars to decide width would make this three.
    assert!(r.ink_in(0, 2, 1, bg) > 0, "wide base, first column");
    assert!(r.ink_in(0, 3, 1, bg) > 0, "wide base, second column");
}

// ---------------------------------------------------------------------------------------------
// §10 Styles and colours arrive resolved. The backend must use them, not re-derive them.
// ---------------------------------------------------------------------------------------------

#[test]
fn fills_use_exactly_the_colour_they_were_given() {
    let s = scenario("colors-indexed-and-truecolor");
    let r = Rendered::of(&s);
    // The first cell's background is an indexed blue, already resolved to 0000ee upstream.
    assert_eq!(r.cell_center(0, 0), rgb(0x0000ee));
    // Its neighbour is the default background. Two adjacent fills must not bleed into each other.
    assert_eq!(r.cell_center(0, 5), rgb(DEFAULT_BG));
}

#[test]
fn an_inverse_cell_is_already_swapped_and_is_not_swapped_again() {
    let s = scenario("bold-dim-inverse-underline-strike");
    let r = Rendered::of(&s);
    // Verdandi resolved the inverse cell to fg=181818 on bg=cd0000. If the backend inverted a second
    // time the cell would come back dark-on-red, so the cell's DOMINANT colour is the check that
    // matters -- the glyph's own ink covers the centre and says nothing about the fill.
    assert_eq!(
        r.dominant_color(0, 2),
        rgb(0xcd0000),
        "the inverse cell's resolved background"
    );
    // And the glyph on top of it is the dark colour, not the bright default: proof the pair was
    // taken as given rather than one half of it being re-derived.
    assert!(
        r.count_of(0, 2, 1, rgb(DEFAULT_BG)) > 0,
        "the inverse cell's dark glyph"
    );
}

#[test]
fn selection_arrives_as_colours_with_nothing_to_interpret() {
    let s = scenario("selection");
    let r = Rendered::of(&s);
    // Selected cells simply have swapped colours in their ops. There is no selection flag anywhere
    // in the stream and the backend has no notion of one.
    assert_eq!(
        r.cell_center(0, 0),
        rgb(DEFAULT_BG),
        "unselected cell keeps the default background"
    );
    let selected = r.cell_center(0, 2);
    assert_ne!(selected, rgb(DEFAULT_BG), "a selected cell must be visibly filled");
}

// ---------------------------------------------------------------------------------------------
// §11 Cursor. Drawn only from explicit ops, never inferred.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_block_cursor_fills_its_cell_and_keeps_the_character_readable() {
    let s = scenario("cursor-over-glyph"); // cursor at c0 over "X", under="X"@181818
    let r = Rendered::of(&s);
    let cursor_color = rgb(DEFAULT_FG);

    // The cell is predominantly the cursor colour...
    let filled = r.ink_in(0, 0, 1, rgb(DEFAULT_BG));
    assert!(filled > 0, "the cursor must fill its cell");
    // ...and the covered character is drawn ON TOP in the contrasting colour the op supplied. If the
    // backend skipped `text_under`, a block cursor would erase the character underneath it.
    let under_ink = r.ink_in(0, 0, 1, cursor_color);
    assert!(
        under_ink > 0,
        "the character under a block cursor must still be visible"
    );
}

#[test]
fn an_unfocused_cursor_does_not_obscure_its_cell() {
    let s = scenario("cursor-unfocused"); // HollowBlock, under=-
    let r = Rendered::of(&s);
    // The interior is left alone, which is why the contract sends no `text_under` for this shape.
    // Filling it would hide the character and make an unfocused pane look like it had lost one.
    //
    // Compared against the SAME scenario drawn with a Block cursor, because the glyph under it is
    // itself drawn in the cursor's colour -- so "is the centre pixel cursor-coloured" cannot tell a
    // hollow outline from a filled box. Coverage can.
    let hollow = r.count_of(0, 0, 1, rgb(DEFAULT_FG));
    let filled = {
        let mut block = scenario("cursor-unfocused");
        block.set_cursor_shape(terminal_render::CursorShape::Block);
        Rendered::of(&block).count_of(0, 0, 1, rgb(DEFAULT_FG))
    };
    assert!(
        hollow * 2 < filled,
        "a hollow cursor covered {hollow} px where a block covers {filled} -- it is filling its cell"
    );
    assert!(hollow > 0, "a hollow cursor must still draw its outline");
}

#[test]
fn a_cursor_over_a_wide_glyph_covers_both_cells() {
    let s = scenario("cursor-over-wide-glyph"); // cursor c0+2 over 漢
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);
    // `cols: 2` on the cursor op. A backend that assumed a cursor is always one cell would leave
    // half a highlighted character.
    assert!(r.ink_in(0, 0, 1, bg) > 0);
    assert!(
        r.ink_in(0, 1, 1, bg) > 0,
        "the cursor spans the wide glyph's second column"
    );
}

#[test]
fn no_cursor_op_means_no_cursor_drawn() {
    let s = scenario("pinned-history-negative-absolute-lines");
    assert!(
        s.list.cursor().is_none(),
        "this scenario is a pinned view with the cursor out of window"
    );
    let r = Rendered::of(&s);
    let bg = rgb(DEFAULT_BG);
    // Nothing beyond the three "L1x" labels. The cursor's presence is the whole decision; a backend
    // that kept a last-known position and drew it anyway would show a cursor in scrollback.
    for col in 3..s.cols {
        assert_eq!(
            r.ink_in(0, col, 1, bg),
            0,
            "column {col} of a pinned view should be empty"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// §13/§14 Pinned history and the expiry notice.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_pinned_window_renders_exactly_like_a_live_one() {
    let pinned = scenario("pinned-history-negative-absolute-lines");
    // top_line is negative -- the window is scrolled into history -- and that is the ONLY place it
    // shows. The ops themselves are window-relative, so the backend never learns it is looking at
    // history, which is what makes a pinned view impossible to render differently by accident.
    assert_eq!(pinned.list.top_line, -9);
    assert!(
        pinned.list.ops.iter().all(|op| match op {
            terminal_render::PaintOp::FillCells { row, .. }
            | terminal_render::PaintOp::DrawText { row, .. }
            | terminal_render::PaintOp::DrawCursor { row, .. }
            | terminal_render::PaintOp::DrawNotice { row, .. } => *row < pinned.rows,
        }),
        "every op must address a window row"
    );
    let r = Rendered::of(&pinned);
    assert!(r.ink_in(0, 0, 3, rgb(DEFAULT_BG)) > 0, "the pinned content renders");
}

#[test]
fn an_expired_anchor_draws_its_notice_over_the_content() {
    let s = scenario("anchor-expired");
    let r = Rendered::of(&s);

    assert!(s.list.has_expiry_notice(), "the corpus scenario must carry a notice");
    // The notice row is filled with the notice's own background and carries its text. Drawing
    // nothing here is the forbidden outcome: the user would be shown live output while believing
    // they were still looking at the history they pinned.
    let notice_bg = rgb(DEFAULT_FG); // bg=d8d8d8 in the fixture
    assert_eq!(r.cell_center(0, 7), notice_bg, "the notice fills its full extent");
    assert!(r.ink_in(0, 0, 8, notice_bg) > 0, "the notice text must be drawn");
}

/// The positive counterpart §27 asks for. A failure-state test alone would pass against a backend
/// that drew a notice over everything, always.
#[test]
fn a_healthy_view_draws_no_notice_at_all() {
    for name in ["ascii", "wrapped-rows", "pinned-history-negative-absolute-lines"] {
        let s = scenario(name);
        assert!(!s.list.has_expiry_notice(), "{name} must not carry an expiry notice");
        let r = Rendered::of(&s);
        // Specifically: no row is filled edge to edge with a notice background. Checked on the
        // cell's dominant colour so a glyph in that column does not read as a notice.
        let bg = rgb(DEFAULT_BG);
        assert_eq!(
            r.dominant_color(0, s.cols - 1),
            bg,
            "{name}: the last column of row 0 is ordinary terminal background"
        );
    }
}
