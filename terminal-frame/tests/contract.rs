//! THE CONTRACT, pinned against the upstream it is a projection of.
//!
//! Two kinds of test here:
//!
//! * **drift alarms** -- the contract restates things `alacritty_terminal`
//!   knows (the palette length, the set of cell flags, the cell layout). Each
//!   is asserted against upstream so a version bump is a red test rather than a
//!   silent re-interpretation.
//! * **known gaps** -- the things this crate deliberately does not carry. A gap
//!   that is only written down in prose is indistinguishable from a bug; each
//!   one here is asserted, so "we decided not to" stays distinguishable from
//!   "we forgot".

mod common;

use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::COUNT as UPSTREAM_PALETTE_COUNT;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use common::Harness;
use terminal_frame::frame::{PALETTE_BACKGROUND, PALETTE_CURSOR, PALETTE_FOREGROUND};
use terminal_frame::{CellFlags, FrameCell, FrameColor, FrameCursorShape, FrameKind, Projector, Rgb, PALETTE_LEN};

// ===========================================================================
// Drift alarms
// ===========================================================================

#[test]
fn the_palette_length_matches_upstream() {
    assert_eq!(PALETTE_LEN, UPSTREAM_PALETTE_COUNT);
}

#[test]
fn the_named_palette_constants_match_upstream_discriminants() {
    // The whole `FrameColor::Palette` design rests on this: `NamedColor`'s
    // discriminants ARE palette indices, which is why `Named` and `Indexed`
    // can collapse into one index space.
    assert_eq!(PALETTE_FOREGROUND as usize, NamedColor::Foreground as usize);
    assert_eq!(PALETTE_BACKGROUND as usize, NamedColor::Background as usize);
    assert_eq!(PALETTE_CURSOR as usize, NamedColor::Cursor as usize);
    assert_eq!(NamedColor::Black as usize, 0);
    assert_eq!(NamedColor::BrightWhite as usize, 15);
    assert_eq!(NamedColor::DimForeground as usize, PALETTE_LEN - 1);
}

#[test]
fn the_flag_mapping_covers_every_upstream_bit() {
    // `project_flags` is written out flag by flag rather than bit-cast, so this
    // is the test that keeps it exhaustive. `Flags::all()` includes the three
    // composite aliases (BOLD_ITALIC, DIM_BOLD, ALL_UNDERLINES), but each is a
    // union of singles and contributes no new bit.
    let mut covered = 0u16;
    for (upstream, ours) in flag_pairs() {
        covered |= upstream.bits();
        // Each flag round-trips to its own bit and nothing else.
        let cell = Cell {
            flags: upstream,
            ..Cell::default()
        };
        let projected = project(&cell).flags;
        assert_eq!(projected, ours, "{upstream:?} projected to {projected:?}");
    }
    assert_eq!(
        covered,
        Flags::all().bits(),
        "the mapping misses bits {:#b}",
        Flags::all().bits() & !covered
    );
    assert_eq!(covered.count_ones(), 15, "upstream uses 15 of the 16 bits");
}

#[test]
fn the_flag_bit_positions_still_agree_with_upstream() {
    // Not relied on -- the mapping is explicit -- but if this ever fails,
    // somebody renumbered a bit and every serialised frame in flight changes
    // meaning. Better to know.
    for (upstream, ours) in flag_pairs() {
        assert_eq!(
            upstream.bits(),
            ours.bits(),
            "{upstream:?} is {:#b} upstream and {:#b} here",
            upstream.bits(),
            ours.bits()
        );
    }
}

fn flag_pairs() -> Vec<(Flags, CellFlags)> {
    vec![
        (Flags::INVERSE, CellFlags::INVERSE),
        (Flags::BOLD, CellFlags::BOLD),
        (Flags::ITALIC, CellFlags::ITALIC),
        (Flags::UNDERLINE, CellFlags::UNDERLINE),
        (Flags::WRAPLINE, CellFlags::WRAPLINE),
        (Flags::WIDE_CHAR, CellFlags::WIDE_CHAR),
        (Flags::WIDE_CHAR_SPACER, CellFlags::WIDE_CHAR_SPACER),
        (Flags::DIM, CellFlags::DIM),
        (Flags::HIDDEN, CellFlags::HIDDEN),
        (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
        (Flags::LEADING_WIDE_CHAR_SPACER, CellFlags::LEADING_WIDE_CHAR_SPACER),
        (Flags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
        (Flags::UNDERCURL, CellFlags::UNDERCURL),
        (Flags::DOTTED_UNDERLINE, CellFlags::DOTTED_UNDERLINE),
        (Flags::DASHED_UNDERLINE, CellFlags::DASHED_UNDERLINE),
    ]
}

/// Project one cell without going through a whole `Term`.
fn project(cell: &Cell) -> FrameCell {
    let mut harness = Harness::new(4, 1);
    harness.term.grid_mut()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)] = cell.clone();
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    frame
        .rows_changed
        .first()
        .map(|row| row.cells[0].clone())
        .unwrap_or_default()
}

#[test]
fn upstream_cell_is_still_twenty_four_bytes() {
    // Not a requirement, a MEASUREMENT the bandwidth numbers are read against:
    // "24 bytes per cell in memory" is the baseline the wire encodings are
    // compared to.
    assert_eq!(std::mem::size_of::<Cell>(), 24, "upstream Cell size changed");
    eprintln!(
        "sizes: alacritty Cell = {}, FrameCell = {}, FrameColor = {}, CellFlags = {}",
        std::mem::size_of::<Cell>(),
        std::mem::size_of::<FrameCell>(),
        std::mem::size_of::<FrameColor>(),
        std::mem::size_of::<CellFlags>()
    );
}

#[test]
fn the_frame_cell_stays_small_enough_to_ship_a_screen_of() {
    // The `Option<Box<CellExtras>>` design exists for this. If it ever grows,
    // a 120x40 Full frame grows with it, 4800 times over.
    assert!(
        std::mem::size_of::<FrameCell>() <= 24,
        "FrameCell grew to {} bytes; the extras box is no longer doing its job",
        std::mem::size_of::<FrameCell>()
    );
}

#[test]
fn a_fresh_term_has_no_colour_overrides_at_all() {
    // The reason the frame must carry the override table: cells reference
    // palette indices, and NOTHING else records what index 4 currently is.
    let harness = Harness::new(10, 3);
    let colors = harness.term.colors();
    for index in 0..PALETTE_LEN {
        assert!(colors[index].is_none(), "index {index} was already overridden");
    }
    let mut projector = Projector::new();
    assert!(projector.full(&harness.term).color_overrides.is_empty());
}

// ===========================================================================
// Known gaps
// ===========================================================================

#[test]
fn sgr_blink_is_unrecoverable_from_term() {
    // `Attr::BlinkSlow` / `BlinkFast` / `CancelBlink` reach
    // `Term::terminal_attribute` and fall into its `_ => ()` arm. No `Flags`
    // bit, no `TermMode` bit, nothing to project. Documented as a gap in
    // lib.rs; asserted here so it stays a decision.
    let mut harness = Harness::new(20, 3);
    let before = *harness.term.mode();
    harness.feed(b"\x1b[5mblink\x1b[25m");

    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    for row in &frame.rows_changed {
        for cell in &row.cells {
            assert_eq!(
                cell.flags & !CellFlags::WRAPLINE,
                CellFlags::empty(),
                "a blink flag appeared from somewhere: {:?}",
                cell.flags
            );
        }
    }
    assert_eq!(*harness.term.mode(), before, "SGR 5 set a TermMode bit");
}

#[test]
fn osc_8_hyperlinks_are_not_carried() {
    // Deliberate. The terminal DOES record them, so this test is the proof the
    // gap is a decision and not an oversight -- and the check that finds out if
    // somebody adds them without updating lib.rs.
    let mut harness = Harness::new(20, 3);
    harness.feed(b"\x1b]8;;https://example.invalid/\x07link\x1b]8;;\x07");
    let cell = &harness.term.grid()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)];
    assert!(cell.hyperlink().is_some(), "the terminal did not record the hyperlink");

    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    let projected = &frame.rows_changed[0].cells[0];
    assert_eq!(projected.c, 'l');
    assert!(
        projected.extra.is_none(),
        "a hyperlink leaked into the frame; lib.rs's known-gaps list is now wrong"
    );
}

// ===========================================================================
// The contract's own invariants
// ===========================================================================

#[test]
fn colour_projection_collapses_named_and_indexed_into_one_index_space() {
    use terminal_frame::project::project_color_for_tests as project_color;
    assert_eq!(project_color(Color::Named(NamedColor::Red)), FrameColor::Palette(1));
    assert_eq!(project_color(Color::Indexed(1)), FrameColor::Palette(1));
    assert_eq!(
        project_color(Color::Named(NamedColor::Foreground)),
        FrameColor::Palette(PALETTE_FOREGROUND)
    );
    assert_eq!(project_color(Color::Indexed(231)), FrameColor::Palette(231));
    assert_eq!(
        project_color(Color::Spec(alacritty_terminal::vte::ansi::Rgb { r: 1, g: 2, b: 3 })),
        FrameColor::Rgb(Rgb::new(1, 2, 3))
    );
}

#[test]
fn cursor_visibility_is_separate_from_cursor_shape() {
    // `RenderableCursor::new` folds DECTCEM into `CursorShape::Hidden`, losing
    // the shape the application chose. The contract keeps them apart.
    let mut harness = Harness::new(20, 3);
    harness.feed(b"\x1b[4 q"); // steady underline
    let mut projector = Projector::new();
    let visible = projector.full(&harness.term).cursor;
    assert_eq!(visible.shape, FrameCursorShape::Underline);
    assert!(visible.visible);

    harness.feed(b"\x1b[?25l");
    let hidden = projector.full(&harness.term).cursor;
    assert!(!hidden.visible, "DECTCEM off should clear `visible`");
    assert_eq!(
        hidden.shape,
        FrameCursorShape::Underline,
        "the shape must survive being hidden -- that is the whole point of the split"
    );
}

#[test]
fn cursor_blinking_comes_from_cursor_style_not_from_the_renderable_cursor() {
    let mut harness = Harness::new(20, 3);
    let mut projector = Projector::new();
    harness.feed(b"\x1b[2 q"); // steady block
    assert!(!projector.full(&harness.term).cursor.blinking);
    harness.feed(b"\x1b[1 q"); // blinking block
    let frame = projector.full(&harness.term);
    assert!(frame.cursor.blinking, "CSI 1 SP q should set blinking");
    assert_eq!(frame.cursor.shape, FrameCursorShape::Block);
}

#[test]
fn a_cursor_on_a_wide_char_spacer_reports_the_wide_char() {
    let mut harness = Harness::new(20, 3);
    harness.feed("\u{6f22}".as_bytes());
    harness.feed(b"\x1b[1;2H"); // onto the spacer
    let mut projector = Projector::new();
    let cursor = projector.full(&harness.term).cursor;
    assert_eq!(
        (cursor.line, cursor.col),
        (0, 0),
        "the cursor belongs to the glyph, not its spacer"
    );
}

#[test]
fn focus_is_client_parked_state_the_crate_never_reads_from_term() {
    let mut harness = Harness::new(10, 3);
    harness.term.is_focused = true;
    let mut projector = Projector::new();
    assert!(
        !projector.full(&harness.term).focused,
        "the projector read Term::is_focused; it is the CLIENT that owns focus"
    );
    projector.set_focused(true);
    assert!(projector.full(&harness.term).focused);
}

#[test]
fn a_full_frame_omits_rows_that_are_entirely_default() {
    let mut harness = Harness::canonical();
    harness.feed(b"only one line");
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    assert_eq!(frame.kind, FrameKind::Full);
    assert_eq!(frame.rows_changed.len(), 1, "39 default rows should not be on the wire");
    assert_eq!(
        frame.rows_changed[0].right, 12,
        "trailing default cells should be trimmed"
    );
    assert_eq!(frame.rows_changed[0].cells.len(), 13);
    frame.check().unwrap();
}

#[test]
fn structural_check_rejects_the_four_ways_a_frame_can_be_malformed() {
    use terminal_frame::frame::{FrameError, RowUpdate};
    let mut harness = Harness::new(10, 3);
    harness.feed(b"abc");
    let mut projector = Projector::new();
    let good = projector.full(&harness.term);
    good.check().unwrap();

    let mut wrong_width = good.clone();
    wrong_width.rows_changed[0].cells.pop();
    assert!(matches!(wrong_width.check(), Err(FrameError::RowWidthMismatch { .. })));

    let mut inverted = good.clone();
    inverted.rows_changed[0].left = 5;
    inverted.rows_changed[0].right = 2;
    assert!(matches!(inverted.check(), Err(FrameError::RowSpanInverted { .. })));

    let mut past_end = good.clone();
    past_end.rows_changed[0].right = 99;
    assert!(matches!(past_end.check(), Err(FrameError::RowSpanOutOfBounds { .. })));

    let mut duplicate = good.clone();
    let row = duplicate.rows_changed[0].clone();
    duplicate.rows_changed.push(row);
    assert!(matches!(duplicate.check(), Err(FrameError::DuplicateLine { .. })));

    let mut bad_colour = good.clone();
    bad_colour.rows_changed.push(RowUpdate {
        line: 1,
        left: 0,
        right: 0,
        cells: vec![FrameCell {
            fg: FrameColor::Palette(999),
            ..FrameCell::default()
        }],
    });
    assert!(matches!(
        bad_colour.check(),
        Err(FrameError::ColorIndexOutOfRange { index: 999 })
    ));
}

#[test]
fn a_delta_out_of_order_is_refused_rather_than_applied() {
    use terminal_frame::{ApplyError, FrameAssembler};
    let mut harness = Harness::new(10, 3);
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();

    harness.feed(b"a");
    let first = projector.next(&mut harness.term);
    harness.feed(b"b");
    let second = projector.next(&mut harness.term);

    // Skip `first`.
    assert_eq!(
        assembler.apply(&second),
        Err(ApplyError::GenerationGap {
            expected: first.generation,
            got: second.generation
        }),
        "a gap must be refused; applying it would leave a screen nobody can explain"
    );
}

#[test]
fn only_a_full_frame_may_change_the_geometry() {
    use terminal_frame::{ApplyError, FrameAssembler};
    let mut harness = Harness::new(10, 3);
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();

    harness.feed(b"x");
    let mut delta = projector.next(&mut harness.term);
    delta.cols = 20;
    assert_eq!(
        assembler.apply(&delta),
        Err(ApplyError::DeltaResized {
            from: (10, 3),
            to: (20, 3)
        })
    );
}

#[test]
fn a_resize_is_expressed_in_cells_and_reaches_the_frame() {
    use alacritty_terminal::term::test::TermSize;
    let mut harness = Harness::new(20, 5);
    harness.feed(b"hello");
    let mut projector = Projector::new();
    assert_eq!(
        (projector.full(&harness.term).cols, projector.full(&harness.term).rows),
        (20, 5)
    );

    // The renderer's job, from ONE decision: Term first...
    harness.term.resize(TermSize::new(40, 8));
    let frame = projector.full(&harness.term);
    assert_eq!((frame.cols, frame.rows), (40, 8));
    // ...and the PTY winsize second. The frame carries the cells so a consumer
    // can DETECT a mismatch; it never drives one.
}

#[test]
fn generations_are_monotonic_across_full_and_delta() {
    let mut harness = Harness::new(10, 3);
    let mut projector = Projector::new();
    let mut last = 0;
    for step in 0..25 {
        harness.feed_str(&format!("{step}"));
        let frame = projector.next(&mut harness.term);
        assert_eq!(frame.generation, last + 1, "generation jumped");
        last = frame.generation;
    }
    let stats = projector.stats();
    assert_eq!(stats.frames(), 25);
    assert!(stats.delta_frames > 0 && stats.full_frames > 0, "stats: {stats:?}");
}

// ===========================================================================
// The public surface, pinned directly.
//
// Everything below is reachable by a consumer and therefore mutable in a way a
// consumer would see. The differential compares an assembled screen against the
// Term through an INDEPENDENT reference (tests/common/reference.rs), which
// covers the cell/cursor/mode/palette accessors; these cover the rest.
// ===========================================================================

#[test]
fn the_assembler_reports_the_producers_generation_and_its_own_count() {
    use terminal_frame::FrameAssembler;
    let mut harness = Harness::new(10, 3);
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assert_eq!((assembler.generation(), assembler.applied()), (0, 0));

    for step in 1..=7u64 {
        harness.feed_str("x");
        let frame = projector.next(&mut harness.term);
        assembler.apply(&frame).unwrap();
        assert_eq!(assembler.generation(), frame.generation, "step {step}");
        assert_eq!(assembler.generation(), step);
        assert_eq!(assembler.applied(), step);
    }

    // A refused frame must not advance either counter.
    harness.feed_str("y");
    let a = projector.next(&mut harness.term);
    harness.feed_str("z");
    let b = projector.next(&mut harness.term);
    assert!(assembler.apply(&b).is_err());
    assert_eq!((assembler.generation(), assembler.applied()), (7, 7));
    assembler.apply(&a).unwrap();
    assert_eq!((assembler.generation(), assembler.applied()), (a.generation, 8));
}

#[test]
fn the_assembler_carries_focus_through_from_the_frame() {
    use terminal_frame::FrameAssembler;
    let mut harness = Harness::new(10, 3);
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();
    assert!(!assembler.focused());

    projector.set_focused(true);
    harness.feed_str("x");
    assembler.apply(&projector.next(&mut harness.term)).unwrap();
    assert!(assembler.focused(), "focus did not reach the assembler");

    projector.set_focused(false);
    harness.feed_str("y");
    assembler.apply(&projector.next(&mut harness.term)).unwrap();
    assert!(!assembler.focused(), "focus did not clear");
}

#[test]
fn assembler_screen_text_matches_the_terminal() {
    use terminal_frame::FrameAssembler;
    let mut harness = Harness::new(20, 4);
    harness.feed_str("first\r\nse\u{301}cond\r\n\u{6f22}\u{5b57} wide");
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();
    assert_eq!(assembler.screen_text(), harness.screen_text());
    assert!(assembler.screen_text().contains("se\u{301}cond"));
    assert!(assembler.screen_text().contains('\u{6f22}'));
}

#[test]
fn cell_lookups_outside_the_screen_return_none() {
    use terminal_frame::FrameAssembler;
    let mut harness = Harness::new(10, 3);
    harness.feed_str("abc");
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();

    assert!(assembler.cell(0, 0).is_some());
    assert!(assembler.cell(2, 9).is_some());
    for (line, col) in [(-1, 0), (3, 0), (0, 10), (-1, 10), (3, 10), (i32::MIN, 0)] {
        assert!(
            assembler.cell(line, col).is_none(),
            "({line},{col}) is outside a 10x3 screen but returned a cell"
        );
    }
    // The row-major index really is row-major: (1, 0) is not (0, 1).
    harness.feed_str("\r\nZ");
    assembler.apply(&projector.next(&mut harness.term)).unwrap();
    assert_eq!(assembler.cell(1, 0).unwrap().c, 'Z');
    assert_eq!(assembler.cell(0, 1).unwrap().c, 'b');
}

#[test]
fn frame_cell_accessors_report_the_extras() {
    use terminal_frame::{CellExtras, FrameCell, FrameColor, Rgb};
    let plain = FrameCell::default();
    assert_eq!(plain.zerowidth(), &[] as &[char]);
    assert_eq!(plain.underline_color(), None);

    let rich = FrameCell {
        extra: Some(Box::new(CellExtras {
            zerowidth: vec!['\u{301}', '\u{308}'],
            underline_color: Some(FrameColor::Rgb(Rgb::new(4, 5, 6))),
        })),
        ..FrameCell::default()
    };
    assert_eq!(rich.zerowidth(), &['\u{301}', '\u{308}']);
    assert_eq!(rich.underline_color(), Some(FrameColor::Rgb(Rgb::new(4, 5, 6))));
}

#[test]
fn row_update_width_and_frame_cell_count_agree_with_the_spans() {
    let mut harness = Harness::new(40, 4);
    harness.feed_str("some text here");
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    let mut total = 0;
    for row in &frame.rows_changed {
        assert_eq!(row.width(), row.cells.len());
        assert_eq!(row.width(), row.right as usize - row.left as usize + 1);
        total += row.cells.len();
    }
    assert_eq!(frame.cell_count(), total);
    assert_eq!(frame.cell_count(), 14, "the trimmed Full should carry exactly the text");
}

#[test]
fn the_encoding_dispatchers_agree_with_the_direct_functions() {
    use terminal_frame::encode::{decode_naive, decode_rle, encode_naive, encode_rle};
    use terminal_frame::{decode, encode, Encoding};
    let mut harness = Harness::new(30, 4);
    harness.feed_str("\x1b[31mred\x1b[0m and \u{6f22}\u{301}");
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);

    assert_eq!(encode(&frame, Encoding::Naive), encode_naive(&frame));
    assert_eq!(encode(&frame, Encoding::Rle), encode_rle(&frame));
    assert_eq!(
        decode(&encode(&frame, Encoding::Naive), Encoding::Naive).unwrap(),
        decode_naive(&encode_naive(&frame)).unwrap()
    );
    assert_eq!(
        decode(&encode(&frame, Encoding::Rle), Encoding::Rle).unwrap(),
        decode_rle(&encode_rle(&frame)).unwrap()
    );
    // Decoding with the wrong encoding must fail rather than silently produce a
    // plausible-looking screen.
    assert!(decode(&encode(&frame, Encoding::Rle), Encoding::Naive).is_err());
    assert!(decode(&encode(&frame, Encoding::Naive), Encoding::Rle).is_err());
}

#[test]
fn palette_index_is_the_named_colour_discriminant() {
    use terminal_frame::project::palette_index;
    assert_eq!(palette_index(NamedColor::Black), 0);
    assert_eq!(palette_index(NamedColor::BrightWhite), 15);
    assert_eq!(palette_index(NamedColor::Foreground), PALETTE_FOREGROUND);
    assert_eq!(palette_index(NamedColor::Background), PALETTE_BACKGROUND);
    assert_eq!(palette_index(NamedColor::Cursor), PALETTE_CURSOR);
    assert_eq!(palette_index(NamedColor::DimForeground), PALETTE_LEN as u16 - 1);
}

#[test]
fn frame_stats_count_what_was_published() {
    let mut harness = Harness::new(20, 4);
    let mut projector = Projector::new();
    assert_eq!(projector.stats(), Default::default());

    let first = projector.next(&mut harness.term); // Full: fresh Term
    assert_eq!(first.kind, FrameKind::Full);
    assert_eq!(projector.stats().full_frames, 1);
    assert_eq!(projector.stats().delta_frames, 0);

    harness.feed_str("abc");
    let delta = projector.next(&mut harness.term);
    assert_eq!(delta.kind, FrameKind::Delta);
    let stats = projector.stats();
    assert_eq!((stats.full_frames, stats.delta_frames, stats.frames()), (1, 1, 2));
    assert_eq!(stats.rows_emitted, delta.rows_changed.len() as u64);
    assert_eq!(stats.cells_emitted, delta.cell_count() as u64);

    projector.force_full();
    let forced = projector.next(&mut harness.term);
    assert_eq!(forced.kind, FrameKind::Full);
    assert_eq!(projector.stats().full_frames, 2);
    assert_eq!(projector.stats().frames(), 3);

    // force_full is one-shot.
    harness.feed_str("d");
    assert_eq!(projector.next(&mut harness.term).kind, FrameKind::Delta);
}

#[test]
fn the_compensation_presets_are_what_they_say_they_are() {
    use terminal_frame::project::SpanPolicy;
    use terminal_frame::Compensation;
    assert_eq!(Compensation::default(), Compensation::DEFAULT);
    assert_eq!(Compensation::DEFAULT.spans, SpanPolicy::FullLine);
    assert!(Compensation::DEFAULT.previous_line_last_column);
    assert!(Compensation::DEFAULT.below_the_fold);

    assert_eq!(Compensation::NONE.spans, SpanPolicy::Widened { left: 0, right: 0 });
    assert!(!Compensation::NONE.previous_line_last_column);
    assert!(!Compensation::NONE.below_the_fold);

    assert_eq!(Compensation::WIDEN_ONE.spans, SpanPolicy::Widened { left: 1, right: 1 });
    assert_eq!(Compensation::WIDEN_TWO.spans, SpanPolicy::Widened { left: 2, right: 1 });

    let projector = Projector::with_compensation(Compensation::WIDEN_ONE);
    assert_eq!(projector.compensation(), Compensation::WIDEN_ONE);
    assert_eq!(Projector::new().compensation(), Compensation::DEFAULT);
}

#[test]
fn a_full_line_delta_really_does_carry_the_whole_line() {
    // The production span policy, observed rather than assumed.
    let mut harness = Harness::canonical();
    harness.feed_str("\x1b[5;30Hx");
    let mut projector = Projector::new();
    let _ = projector.next(&mut harness.term);
    harness.feed_str("y");
    let frame = projector.next(&mut harness.term);
    assert_eq!(frame.kind, FrameKind::Delta);
    let row = frame
        .rows_changed
        .iter()
        .find(|row| row.line == 4)
        .expect("line 4 must be present");
    assert_eq!((row.left, row.right), (0, 119), "FullLine must carry columns 0..=119");
    assert_eq!(row.cells.len(), 120);
}

#[test]
fn the_whole_screen_slice_is_row_major_and_agrees_with_cell() {
    use terminal_frame::FrameAssembler;
    let mut harness = Harness::new(7, 5);
    harness.feed_str("ab\r\ncd\r\nef");
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();

    assert_eq!(assembler.cells().len(), 7 * 5, "the slice must be the whole screen");
    for line in 0..5i32 {
        for col in 0..7u16 {
            assert_eq!(
                &assembler.cells()[line as usize * 7 + col as usize],
                assembler.cell(line, col).unwrap(),
                "cells() and cell() disagree at ({line},{col})"
            );
        }
    }
    let text: String = assembler.cells().iter().map(|cell| cell.c).collect();
    assert_eq!(&text[0..2], "ab");
    assert_eq!(&text[7..9], "cd");
    assert_eq!(&text[14..16], "ef");
}

#[test]
fn a_row_update_outside_the_screen_is_refused_rather_than_panicking() {
    use terminal_frame::{ApplyError, FrameAssembler, RowUpdate};
    let mut harness = Harness::new(10, 4);
    harness.feed_str("x");
    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    let good = projector.next(&mut harness.term);
    assembler.apply(&good).unwrap();

    for line in [-1i32, 4, 5, i32::MAX, i32::MIN] {
        let mut bad = good.clone();
        bad.generation += 1;
        bad.rows_changed = vec![RowUpdate {
            line,
            left: 0,
            right: 0,
            cells: vec![FrameCell::default()],
        }];
        assert_eq!(
            assembler.apply(&bad),
            Err(ApplyError::LineOutOfBounds { line, rows: 4 }),
            "line {line} should have been refused"
        );
    }
    // THE POINT: every refusal left the screen exactly as it was. `apply` is
    // atomic -- all checks before any mutation -- because a Full that is
    // rejected half-way has already blanked the screen, and the consumer then
    // holds neither the old picture nor the new one.
    assert_eq!(assembler.cell(0, 0).unwrap().c, 'x');
    assert_eq!(assembler.cells().len(), 40);
    assert_eq!(assembler.applied(), 1);
    assert_eq!(assembler.generation(), good.generation);

    // A rejected DELTA is equally inert.
    harness.feed_str("y");
    let next = projector.next(&mut harness.term);
    let before = assembler.clone();
    let mut bad = next.clone();
    bad.rows_changed = vec![RowUpdate {
        line: 99,
        left: 0,
        right: 0,
        cells: vec![FrameCell::default()],
    }];
    assert!(assembler.apply(&bad).is_err());
    assert_eq!(assembler, before, "a refused delta changed the assembler");
    assembler.apply(&next).unwrap();
    assert_ne!(assembler, before, "the good delta should have changed something");
}
