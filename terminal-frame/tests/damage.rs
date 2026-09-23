//! THE FIVE WAYS `Term::damage()` UNDER-REPORTS, one test each.
//!
//! Every test here has the same shape, and it is the shape that makes it
//! failable:
//!
//!   1. drive a real `Term` to the state in question;
//!   2. project the change with `Compensation::NONE` -- raw damage, exactly as
//!      upstream reports it -- and assert the assembled screen is WRONG;
//!   3. project the same change with the production compensation and assert it
//!      is RIGHT.
//!
//! Step 2 is the load-bearing half. A test that only shows the compensated path
//! working cannot distinguish "the compensation fixed it" from "there was
//! nothing to fix", and the compensation would then be free to rot.

mod common;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{TermDamage, TermMode};

use common::reference::compare as compare_to_term;
use common::Harness;
use terminal_frame::project::{Compensation, SpanPolicy};
use terminal_frame::{FrameAssembler, FrameKind, Projector, TerminalFrame};

/// Drive `setup`, publish a baseline, drive `change`, publish one delta under
/// `compensation`, and hand back (the delta, the assembled screen, the truth).
///
/// The baseline is taken with `next()`, not `full()`: a fresh `Term` starts
/// with `TermDamageState::full = true` and NOTHING resets it except
/// `reset_damage()`. Taking the baseline with `full()` would leave that flag
/// standing and the "delta" under test would silently be a Full frame -- which
/// is exactly how the first draft of this file managed to pass while measuring
/// nothing.
fn one_step(
    cols: usize,
    rows: usize,
    setup: &[u8],
    change: &[u8],
    compensation: Compensation,
) -> (TerminalFrame, FrameAssembler, Harness) {
    let mut harness = Harness::new(cols, rows);
    harness.feed(setup);

    let mut projector = Projector::with_compensation(compensation);
    let mut assembler = FrameAssembler::new();
    let base = projector.next(&mut harness.term);
    assert_eq!(base.kind, FrameKind::Full, "a fresh Term must publish Full first");
    assembler.apply(&base).unwrap();

    harness.feed(change);
    let frame = projector.next(&mut harness.term);
    frame.check().expect("well formed");
    assembler.apply(&frame).unwrap();

    (frame, assembler, harness)
}

/// One line of the terminal as text, for assertions that read better as text.
fn line_of(harness: &Harness, line: usize) -> String {
    harness.screen_text().lines().nth(line).unwrap_or("").to_string()
}

fn spans(frame: &TerminalFrame) -> Vec<(i32, u16, u16)> {
    frame
        .rows_changed
        .iter()
        .map(|row| (row.line, row.left, row.right))
        .collect()
}

/// The narrow span policy alone, so the "is one column enough?" question can be
/// asked directly. Production uses `SpanPolicy::FullLine`; these tests exist to
/// show WHY, by measuring what the cheaper policy misses.
fn widen(left: u16, right: u16) -> Compensation {
    Compensation {
        spans: SpanPolicy::Widened { left, right },
        previous_line_last_column: false,
        below_the_fold: false,
    }
}

// ===========================================================================
// UNDER-REPORT 1: combining marks. `Term::input` takes the zero-width branch
// and `return`s without a single damage call.
// ===========================================================================

#[test]
fn combining_mark_on_a_plain_char_is_one_column_left_of_the_cursor() {
    // "a" then U+0301 COMBINING ACUTE ACCENT. The mark rides on column 0; the
    // cursor -- and therefore all the damage there is -- is at column 1.
    let (raw, _, _) = one_step(20, 5, b"a", "\u{301}".as_bytes(), Compensation::NONE);
    assert_eq!(
        spans(&raw),
        vec![(0, 1, 1)],
        "raw damage should be the cursor column alone; the mark at column 0 is not in it"
    );

    let (_, assembled, truth) = one_step(20, 5, b"a", "\u{301}".as_bytes(), widen(0, 0));
    assert!(
        compare_to_term(&assembled, &truth.term).is_some(),
        "with no widening the combining mark must be LOST; if it is not, this test proves nothing"
    );

    let (_, assembled, truth) = one_step(20, 5, b"a", "\u{301}".as_bytes(), Compensation::default());
    assert_eq!(compare_to_term(&assembled, &truth.term), None);
    assert_eq!(line_of(&truth, 0).trim_end(), "a\u{301}");
}

#[test]
fn combining_mark_on_a_wide_char_is_two_columns_left_so_widening_by_one_is_not_enough() {
    // THE MEASUREMENT THAT SET `Compensation::widen_left` TO 2.
    //
    // `Term::input`'s zero-width branch steps back one column, and then a
    // SECOND time when that column holds a `WIDE_CHAR_SPACER`. After a
    // fullwidth glyph the cursor is two columns right of the cell the mark
    // lands on, so the brief's "widen by one column each side" rule is one
    // column short on the left.
    let setup = "\u{6f22}".as_bytes(); // U+6F22, East Asian Wide
    let mark = "\u{301}".as_bytes();

    let (raw, _, _) = one_step(20, 5, setup, mark, Compensation::NONE);
    assert_eq!(spans(&raw), vec![(0, 2, 2)], "raw damage is the cursor column alone");

    let (_, assembled, truth) = one_step(20, 5, setup, mark, widen(1, 1));
    let missed = compare_to_term(&assembled, &truth.term);
    assert!(
        missed.is_some(),
        "widening by ONE column must still miss this; if it does not, widen_left could drop to 1"
    );
    eprintln!("widen_left=1 leaves: {}", missed.unwrap());

    let (_, assembled, truth) = one_step(20, 5, setup, mark, widen(2, 1));
    assert_eq!(
        compare_to_term(&assembled, &truth.term),
        None,
        "widening by TWO columns is what actually covers a combining mark on a fullwidth char"
    );

    // And the mark really is where this test says it is.
    let mut harness = Harness::new(20, 5);
    harness.feed(setup);
    harness.feed(mark);
    let cell = &harness.term.grid()[Line(0)][Column(0)];
    assert!(cell.flags.contains(Flags::WIDE_CHAR));
    assert_eq!(cell.zerowidth(), Some(&['\u{301}'][..]), "the mark rides on column 0");
    assert_eq!(
        harness.term.grid().cursor.point.column,
        Column(2),
        "the cursor is at column 2"
    );
}

// ===========================================================================
// UNDER-REPORT 2: the cross-line LEADING_WIDE_CHAR_SPACER clear.
// ===========================================================================

#[test]
fn clearing_a_leading_wide_char_spacer_damages_nothing_on_the_line_above() {
    // Build the spacer: 9 columns of text on a 10-column terminal, then a
    // fullwidth glyph that does not fit. `Term::input` writes a
    // LEADING_WIDE_CHAR_SPACER into (0, 9), wraps, and puts the glyph at
    // (1, 0)-(1, 1).
    let setup = {
        let mut bytes = b"123456789".to_vec();
        bytes.extend_from_slice("\u{6f22}".as_bytes());
        bytes
    };
    let mut probe = Harness::new(10, 5);
    probe.feed(&setup);
    assert!(
        probe.term.grid()[Line(0)][Column(9)]
            .flags
            .contains(Flags::LEADING_WIDE_CHAR_SPACER),
        "setup failed: no LEADING_WIDE_CHAR_SPACER at (0, 9)"
    );

    // Overwrite the wide glyph's first half. `write_at_cursor` reaches back to
    // `grid[point.line - 1][last_column]` to clear the leading spacer -- and
    // damages only the cursor.
    let change = b"\x1b[2;1Hx";

    let (raw, _, _) = one_step(10, 5, &setup, change, Compensation::NONE);
    assert!(
        !spans(&raw).iter().any(|(line, _, right)| *line == 0 && *right >= 9),
        "raw damage unexpectedly covered (0, 9); spans were {:?}",
        spans(&raw)
    );

    let no_previous_line = Compensation {
        previous_line_last_column: false,
        ..Compensation::default()
    };
    let (_, assembled, truth) = one_step(10, 5, &setup, change, no_previous_line);
    let missed = compare_to_term(&assembled, &truth.term);
    assert!(
        missed.is_some(),
        "without the previous-line rule the stale LEADING_WIDE_CHAR_SPACER must survive"
    );
    eprintln!("previous_line_last_column=false leaves: {}", missed.unwrap());

    let (frame, assembled, truth) = one_step(10, 5, &setup, change, Compensation::default());
    assert_eq!(compare_to_term(&assembled, &truth.term), None);
    assert!(
        spans(&frame).iter().any(|(line, _, right)| *line == 0 && *right == 9),
        "the compensation must put (0, 9) back into the frame; spans were {:?}",
        spans(&frame)
    );
}

// ===========================================================================
// UNDER-REPORT 3: the spacer-half overwrite reaches both ways.
// ===========================================================================

#[test]
fn overwriting_the_spacer_half_clears_the_wide_char_one_column_left() {
    // Fullwidth glyph at (0,0)-(0,1); then write over the SPACER at (0,1).
    // `write_at_cursor` calls `clear_wide()` on (0,0) -- no damage.
    let setup = "\u{6f22}".as_bytes();
    let change = b"\x1b[1;2Hx";

    let mut probe = Harness::new(20, 5);
    probe.feed(setup);
    assert!(probe.term.grid()[Line(0)][Column(1)]
        .flags
        .contains(Flags::WIDE_CHAR_SPACER));

    let (raw, _, _) = one_step(20, 5, setup, change, Compensation::NONE);
    assert_eq!(
        spans(&raw),
        vec![(0, 1, 2)],
        "raw damage covers the cursor's old and new columns, not column 0"
    );

    let (_, assembled, truth) = one_step(20, 5, setup, change, widen(0, 0));
    assert!(
        compare_to_term(&assembled, &truth.term).is_some(),
        "without widening the cleared wide char at column 0 must be stale"
    );

    let (_, assembled, truth) = one_step(20, 5, setup, change, Compensation::default());
    assert_eq!(compare_to_term(&assembled, &truth.term), None);
    assert_eq!(line_of(&truth, 0).trim_end(), " x", "the glyph really was cleared");
}

#[test]
fn overwriting_the_wide_half_clears_the_spacer_to_its_right_but_the_cursor_already_covers_it() {
    // The mirror image, and a FINDING recorded rather than papered over.
    //
    // Writing over the WIDE_CHAR at (0,0) removes the spacer flag at (0,1)
    // with no damage call -- but `input()` then advances the cursor to column
    // 1, and `Term::damage()`'s unconditional `damage_cursor()` covers exactly
    // that column. The right-hand reach of `write_at_cursor` can never exceed
    // the cursor's own next position, so a RIGHT-widening is not load-bearing
    // for this under-report at all. It is recorded because the obvious
    // symmetric rule ("widen one column each side") is right for the wrong
    // reason on this side.
    let setup = "\u{6f22}".as_bytes();
    let change = b"\x1b[1;1Hx";

    let (raw, assembled, truth) = one_step(20, 5, setup, change, Compensation::NONE);
    assert_eq!(
        spans(&raw),
        vec![(0, 0, 2)],
        "raw damage spans the cursor's old column (2) and its new one (1)"
    );
    assert_eq!(
        compare_to_term(&assembled, &truth.term),
        None,
        "if raw damage stops covering this, a right-widening becomes load-bearing and this \
         finding should be revisited"
    );

    let (_, assembled, truth) = one_step(20, 5, setup, change, Compensation::default());
    assert_eq!(compare_to_term(&assembled, &truth.term), None);
}

// ===========================================================================
// UNDER-REPORT 6: everything written to the right of where the cursor ended.
// Found by tests/delta_equals_full.rs, not by reading the source.
// ===========================================================================

#[test]
fn text_written_to_the_right_of_the_final_cursor_column_is_undamaged() {
    // The shrinker's four escapes, verbatim.
    //
    //   ESC[4;5r    scroll region = rows 4..5
    //   ESC[7;21H   cursor to the BOTTOM screen line, column 21
    //   ESC[3B      cursor down -- clamped, stays on the bottom line
    //   tab<TAB>here
    //
    // `input()` and `put_tab()` write with NO damage call. The line wraps, so
    // `wrapline()` runs -- and takes the `linefeed()` path, which SKIPS the
    // pre-move `damage_cursor()`. `linefeed()` itself does nothing here:
    // `cursor.line + 1 == scroll_region.end` is false (8 != 5) and
    // `cursor.line < bottommost_line()` is false (7 == 7). The cursor snaps to
    // column 0 of the SAME line and everything written to its right is outside
    // every damaged span.
    let change = b"\x1b[4;5r\x1b[7;21H\x1b[3Btab\there";

    let (raw, assembled, truth) = one_step(24, 8, b"", change, Compensation::NONE);
    eprintln!("shrinker case, raw spans: {:?}", spans(&raw));
    let missed = compare_to_term(&assembled, &truth.term);
    assert!(
        missed.is_some(),
        "the under-report is gone; SpanPolicy::FullLine may be unnecessary"
    );
    eprintln!("shrinker case, raw damage leaves: {}", missed.unwrap());

    let (_, assembled, truth) = one_step(24, 8, b"", change, Compensation::default());
    assert_eq!(
        compare_to_term(&assembled, &truth.term),
        None,
        "SpanPolicy::FullLine must close it -- that is the only reason it is the policy"
    );
}

#[test]
fn the_undamaged_gap_is_as_wide_as_the_line_so_no_fixed_widening_bounds_it() {
    // The same mechanism, started further left so the gap is 20 columns wide.
    // This is the argument for `SpanPolicy::FullLine` over any `Widened{..}`:
    // the number of columns that go missing is a property of where the text
    // started, not a constant.
    let change = b"\x1b[4;5r\x1b[8;4Hxxxxxxxxxxxxxxxxxxxxxxxxx";

    for pad in [1u16, 2, 3, 5, 8, 13] {
        let (_, assembled, truth) = one_step(24, 8, b"", change, widen(pad, pad));
        let missed = compare_to_term(&assembled, &truth.term);
        assert!(
            missed.is_some(),
            "widening by {pad} columns each side covered a 20-column gap; the FullLine \
             argument would then be wrong"
        );
        if pad == 13 {
            eprintln!("even widen(13, 13) leaves: {}", missed.unwrap());
        }
    }

    let (frame, assembled, truth) = one_step(24, 8, b"", change, Compensation::default());
    assert_eq!(compare_to_term(&assembled, &truth.term), None);
    assert!(
        frame
            .rows_changed
            .iter()
            .any(|row| row.line == 7 && row.left == 0 && row.right == 23),
        "FullLine must carry line 7 whole; spans were {:?}",
        spans(&frame)
    );
}

#[test]
fn put_tab_writes_characters_into_cells_it_is_not_obvious_it_touches() {
    // Pinned because "a tab is a cursor move" is the natural assumption and it
    // is wrong: `put_tab` substitutes the mapped '\t' into every blank cell it
    // starts a hop from, and does it with no damage call. On its own that is
    // covered (the cursor ends to the RIGHT of everything it wrote); it becomes
    // visible only in combination with under-report 6, which is why the
    // shrinker's minimal case contains a tab.
    let mut harness = Harness::new(40, 4);
    harness.feed(b"\t\t\t");
    let written: Vec<usize> = {
        let grid = harness.term.grid();
        (0..40).filter(|col| grid[Line(0)][Column(*col)].c == '\t').collect()
    };
    assert_eq!(
        written,
        vec![0, 8, 16],
        "put_tab should have written a tab character at every stop it hopped from"
    );
    assert_eq!(harness.term.grid().cursor.point.column, Column(24));

    // AND A SECOND FINDING, measured while writing the first: CHT (`CSI 3 I`)
    // reaches a DIFFERENT function and writes nothing at all, even though it
    // lands the cursor in exactly the same place. "Move by tabs" and "print
    // tabs" are not the same operation in this terminal.
    let mut cht = Harness::new(40, 4);
    cht.feed(b"\x1b[3I");
    let written: Vec<usize> = {
        let grid = cht.term.grid();
        (0..40).filter(|col| grid[Line(0)][Column(*col)].c == '\t').collect()
    };
    assert!(written.is_empty(), "CHT wrote tab characters at {written:?}");
    assert_eq!(
        cht.term.grid().cursor.point.column,
        Column(24),
        "but the cursor lands identically"
    );
}

// ===========================================================================
// UNDER-REPORT 4: Term::grid_mut(). Undetectable from here; the escape hatch is
// Projector::force_full.
// ===========================================================================

#[test]
fn grid_mut_is_invisible_to_damage_and_force_full_is_the_only_answer() {
    let mut harness = Harness::new(20, 5);
    harness.feed(b"hello");

    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();

    // The out-of-band mutation. No parser, no damage bookkeeping.
    harness.term.grid_mut()[Line(2)][Column(3)].c = 'Z';

    let frame = projector.next(&mut harness.term);
    assembler.apply(&frame).unwrap();
    assert!(
        compare_to_term(&assembler, &harness.term).is_some(),
        "grid_mut() was somehow visible to damage; if upstream started tracking it, \
         Projector::force_full's reason for existing has changed"
    );

    // The documented remedy.
    projector.force_full();
    let frame = projector.next(&mut harness.term);
    assert_eq!(frame.kind, FrameKind::Full);
    assembler.apply(&frame).unwrap();
    assert_eq!(compare_to_term(&assembler, &harness.term), None);
}

// ===========================================================================
// UNDER-REPORT 5: in-place writes below the fold while display_offset != 0.
// ===========================================================================

/// The scrolled-back scenario, run end to end under one compensation policy.
fn below_the_fold_run(compensation: Compensation) -> (Vec<(i32, u16, u16)>, FrameAssembler, Harness) {
    use alacritty_terminal::grid::Scroll;

    let mut harness = Harness::with_history(20, 5, 100);
    for line in 0..30 {
        harness.feed_str(&format!("line{line}\r\n"));
    }

    let mut projector = Projector::with_compensation(compensation);
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();

    // Only a TEST may do this. The engine never calls scroll_display; that is
    // the architectural rule this compensation is insurance against.
    harness.term.scroll_display(Scroll::Delta(3));
    assert_eq!(harness.term.grid().display_offset(), 3);
    // scroll_display marks full damage, so drain that frame first.
    let drained = projector.next(&mut harness.term);
    assert_eq!(
        drained.kind,
        FrameKind::Full,
        "scroll_display should have forced a full frame"
    );
    assembler.apply(&drained).unwrap();

    // Now write in place on the bottom screen line -- below the fold.
    harness.feed(b"\x1b[5;1HZZZZ");
    let frame = projector.next(&mut harness.term);
    assert_eq!(
        frame.kind,
        FrameKind::Delta,
        "the write itself should not force a full frame"
    );
    let reported = spans(&frame);
    assembler.apply(&frame).unwrap();

    assert!(
        line_of(&harness, 4).starts_with("ZZZZ"),
        "setup failed: the write never landed"
    );
    (reported, assembler, harness)
}

#[test]
fn writes_below_the_fold_are_truncated_away_while_scrolled_back() {
    let (raw_spans, assembled, truth) = below_the_fold_run(Compensation::NONE);
    assert!(
        !raw_spans.iter().any(|(line, _, _)| *line == 4),
        "raw damage unexpectedly reported line 4; spans were {raw_spans:?}"
    );
    let missed = compare_to_term(&assembled, &truth.term);
    assert!(
        missed.is_some(),
        "TermDamageIterator::new truncates the bottom display_offset lines; without the \
         compensation that write MUST be lost"
    );
    eprintln!("below_the_fold=false leaves: {}", missed.unwrap());

    let (reported, assembled, truth) = below_the_fold_run(Compensation::default());
    assert!(
        reported
            .iter()
            .any(|(line, left, right)| *line == 4 && *left == 0 && *right == 19),
        "the compensation must re-damage the whole of line 4; spans were {reported:?}"
    );
    assert_eq!(compare_to_term(&assembled, &truth.term), None);
}

#[test]
fn display_offset_stays_zero_when_scroll_display_is_never_called() {
    // The architectural claim under-report 5's compensation is insurance
    // against. `Grid::display_offset` is raised only by `Grid::scroll_display`
    // and by `scroll_up` when it is ALREADY non-zero -- so a terminal that is
    // never scrolled by the user cannot drift into the truncating case.
    let mut harness = Harness::with_history(40, 10, 500);
    for line in 0..2000 {
        harness.feed_str(&format!("scrolling line {line}\r\n"));
        assert_eq!(
            harness.term.grid().display_offset(),
            0,
            "display_offset became non-zero without a scroll_display call, at line {line}"
        );
    }
    assert!(
        harness.term.grid().history_size() > 0,
        "the history never filled; test is vacuous"
    );
}

// ===========================================================================
// Things upstream already does, pinned so we notice if it stops.
// ===========================================================================

#[test]
fn upstream_forces_full_damage_under_insert_mode() {
    // `Term::damage()` opens with
    //   if self.mode.contains(TermMode::INSERT) { self.mark_fully_damaged(); }
    // so this crate deliberately does NOT carry its own INSERT check -- it
    // would be dead code and an equivalent mutant. If that ever changes,
    // `Projector` needs one and this test is the alarm.
    let mut harness = Harness::new(20, 5);
    harness.feed(b"abcdef\x1b[4h"); // SM 4 = IRM = TermMode::INSERT
    assert!(harness.term.mode().contains(TermMode::INSERT), "IRM was not set");

    let mut projector = Projector::new();
    let mut assembler = FrameAssembler::new();
    assembler.apply(&projector.next(&mut harness.term)).unwrap();

    harness.feed(b"\x1b[1;1HXY");
    let frame = projector.next(&mut harness.term);
    assert_eq!(
        frame.kind,
        FrameKind::Full,
        "upstream stopped forcing full damage under INSERT; Projector now needs its own guard"
    );
    assembler.apply(&frame).unwrap();
    assert_eq!(compare_to_term(&assembler, &harness.term), None);
    assert!(frame.modes.insert, "and the frame reports the mode that made it full");
}

#[test]
fn a_fresh_term_reports_full_before_any_reset() {
    let mut harness = Harness::new(20, 5);
    let full = matches!(harness.term.damage(), TermDamage::Full);
    assert!(full, "TermDamageState::new starts with full = true");
}

#[test]
fn partial_damage_is_essentially_never_empty_because_the_cursor_is_always_damaged() {
    // `Term::damage()` ends with an unconditional `self.damage_cursor()`. So an
    // empty partial does NOT mean "nothing changed" -- there is no such signal,
    // and a consumer that treats an empty partial as "skip this frame" is
    // reasoning about a state that does not occur.
    let mut harness = Harness::new(20, 5);
    harness.term.reset_damage();

    let mut counts = Vec::new();
    for _ in 0..8 {
        // Feed nothing at all between frames.
        let count = match harness.term.damage() {
            TermDamage::Full => usize::MAX,
            TermDamage::Partial(iter) => iter.count(),
        };
        harness.term.reset_damage();
        counts.push(count);
    }
    assert_eq!(
        counts,
        vec![1; 8],
        "every call reported exactly the cursor line, with no input whatsoever"
    );

    // The projector's own counter agrees: no empty deltas on a live terminal.
    let mut harness = Harness::new(20, 5);
    let mut projector = Projector::new();
    let _ = projector.next(&mut harness.term);
    for step in 0..20 {
        harness.feed_str(&format!("{step}"));
        let _ = projector.next(&mut harness.term);
    }
    assert_eq!(projector.stats().empty_deltas, 0);
}

// ===========================================================================
// Damage OVER-reports too. Free, but worth pinning so the numbers make sense.
// ===========================================================================

#[test]
fn cuf_damages_columns_it_never_wrote() {
    // `move_forward` damages [old_column, new_column] whether or not anything
    // was written there. Over-reporting costs bandwidth, never correctness, so
    // nothing is done about it -- but it is half of why a "delta" is rarely as
    // small as it looks like it should be.
    let (raw, _, _) = one_step(40, 5, b"", b"\x1b[30C", Compensation::NONE);
    assert_eq!(
        spans(&raw),
        vec![(0, 0, 30)],
        "CUF should damage every column it passed over, none of which it wrote"
    );

    let mut probe = Harness::new(40, 5);
    probe.feed(b"\x1b[30C");
    assert_eq!(probe.screen_text().trim(), "", "nothing was actually written");
}

#[test]
fn the_cursor_point_is_carried_in_absolute_grid_coordinates() {
    let mut harness = Harness::new(20, 5);
    harness.feed(b"\x1b[3;7H");
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    assert_eq!((frame.cursor.line, frame.cursor.col), (2, 6));
    let point: Point = harness.term.grid().cursor.point;
    assert_eq!(
        (point.line.0, point.column.0 as u16),
        (frame.cursor.line, frame.cursor.col)
    );
}
