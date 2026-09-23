//! THE READ-ONLY SECOND VIEW, and the reason its indexing is clamped.

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Line};

use common::Harness;
use terminal_frame::{max_scrollback, project_scrollback, project_window, FrameAssembler, FrameKind, Projector};

fn fill(harness: &mut Harness, lines: usize, text: &str) {
    for line in 0..lines {
        harness.feed_str(&format!("{text}{line}\r\n"));
    }
}

#[test]
fn unclamped_indexing_returns_erased_scrollback() {
    // THE FOOTGUN, demonstrated.
    //
    // `Storage::compute_index` guards its bounds with `debug_assert!`. In a
    // release build an out-of-range `Line` does not panic -- the ring-buffer
    // arithmetic wraps into the PHYSICAL backing store and hands back a row
    // that is no longer logically part of the grid.
    let mut harness = Harness::with_history(20, 5, 200);
    fill(&mut harness, 60, "SECRET-");
    assert!(
        harness.term.grid().history_size() > 0,
        "setup: nothing went into history"
    );

    // The terminal is told to erase every saved line.
    harness.feed(b"\x1b[3J");
    assert_eq!(
        harness.term.grid().history_size(),
        0,
        "CSI 3 J should have cleared the scrollback"
    );

    // An UNCLAMPED read of a line that is now outside the grid.
    let unclamped = catch_unwind(AssertUnwindSafe(|| {
        let grid = harness.term.grid();
        let mut row = String::new();
        for col in 0..20 {
            row.push(grid[Line(-1)][Column(col)].c);
        }
        row
    }));

    match &unclamped {
        // Debug build: the debug_assert fires. That is the SAFE outcome, and it
        // is exactly why the release behaviour is worth a test.
        Err(_) => eprintln!(
            "unclamped read panicked (debug_assertions are on); the release path is the \
             dangerous one"
        ),
        Ok(row) => {
            eprintln!("unclamped read of the ERASED history returned {row:?}");
            assert!(
                row.trim_end().starts_with("SECRET-"),
                "the unclamped read returned {row:?}; if the ring no longer holds erased content \
                 this test has stopped demonstrating anything"
            );
        }
    }

    // The CLAMPED read -- what `viewport` does -- lands on the top of the grid.
    let clamped = {
        let grid = harness.term.grid();
        let line = Line(-1).grid_clamp(&*grid, Boundary::Grid);
        assert_eq!(line, Line(0), "with no history, topmost_line is 0");
        let mut row = String::new();
        for col in 0..20 {
            row.push(grid[line][Column(col)].c);
        }
        row
    };
    assert!(
        !clamped.trim_end().starts_with("SECRET-") || clamped.contains("SECRET-5"),
        "clamped read returned {clamped:?}"
    );

    // And the crate's own path never reaches the erased row either.
    let frame = project_scrollback(&harness.term, 999);
    for row in &frame.rows_changed {
        assert!(
            row.line >= 0,
            "project_scrollback produced line {} with no history",
            row.line
        );
    }
}

#[test]
fn every_projected_line_is_inside_the_grid() {
    // The claim that makes ONE clamp, at the window origin, sufficient: every
    // derived index `top + row` stays inside `[topmost_line, bottommost_line]`.
    let mut harness = Harness::with_history(20, 6, 50);
    fill(&mut harness, 40, "line");
    let grid_top = harness.term.grid().topmost_line();
    let grid_bottom = harness.term.grid().bottommost_line();
    assert!(grid_top < Line(0), "setup: no history to scroll into");

    for scrollback in [0usize, 1, 5, 33, 34, 44, 45, 100, 10_000, usize::MAX / 2, usize::MAX] {
        let frame = project_scrollback(&harness.term, scrollback);
        assert_eq!(frame.rows_changed.len(), 6);
        for row in &frame.rows_changed {
            assert!(
                Line(row.line) >= grid_top && Line(row.line) <= grid_bottom,
                "scrollback {scrollback} produced line {} outside [{grid_top}, {grid_bottom}]",
                row.line
            );
        }
        // Lines are contiguous and strictly increasing: the window is a window.
        for pair in frame.rows_changed.windows(2) {
            assert_eq!(pair[1].line, pair[0].line + 1);
        }
        frame.check().expect("a scrollback frame is still a well-formed frame");
    }
}

#[test]
fn scrolling_back_actually_shows_older_content() {
    // A clamp that returned the same window for every offset would pass the
    // bounds test above. This is the control for that.
    let mut harness = Harness::with_history(20, 4, 50);
    fill(&mut harness, 30, "row");

    let live = project_scrollback(&harness.term, 0);
    let back = project_scrollback(&harness.term, 10);
    assert_ne!(live.rows_changed[0].line, back.rows_changed[0].line);
    assert_eq!(back.rows_changed[0].line, -10);

    let text = |frame: &terminal_frame::TerminalFrame, row: usize| -> String {
        frame.rows_changed[row]
            .cells
            .iter()
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    eprintln!("live top row   = {:?}", text(&live, 0));
    eprintln!("10 back top row = {:?}", text(&back, 0));
    assert_eq!(text(&live, 0), "row27");
    assert_eq!(text(&back, 0), "row17");
}

#[test]
fn scrollback_zero_matches_the_live_full_frame() {
    let mut harness = Harness::with_history(20, 4, 50);
    fill(&mut harness, 10, "x");
    harness.feed(b"tail");

    let scrollback = project_scrollback(&harness.term, 0);
    assert_eq!(scrollback.kind, FrameKind::Full);
    assert_eq!(
        scrollback.generation, 0,
        "a scrollback frame is not in the live sequence"
    );

    let mut projector = Projector::new();
    let live = projector.full(&harness.term);

    // The live Full trims default rows and trailing default cells; the
    // scrollback view does not (it is a window, and its consumer draws every
    // row). Compare through the assembler, which is where that difference is
    // defined away.
    let mut from_live = FrameAssembler::new();
    from_live.apply(&live).unwrap();
    let mut from_scrollback = FrameAssembler::new();
    from_scrollback.apply(&scrollback).unwrap();
    assert_eq!(from_live.cells(), from_scrollback.cells());
    assert_eq!(from_live.cursor(), from_scrollback.cursor());
}

#[test]
fn the_authoritative_term_is_not_moved_by_a_scrollback_projection() {
    // The architectural invariant this module exists to serve.
    let mut harness = Harness::with_history(20, 4, 50);
    fill(&mut harness, 30, "row");
    let before = (
        harness.term.grid().display_offset(),
        harness.term.grid().cursor.point,
        harness.term.grid().history_size(),
    );

    let mut projector = Projector::new();
    let before_damage = projector.next(&mut harness.term).kind;

    for scrollback in [0, 3, 17, 999] {
        let _ = project_scrollback(&harness.term, scrollback);
    }

    let after = (
        harness.term.grid().display_offset(),
        harness.term.grid().cursor.point,
        harness.term.grid().history_size(),
    );
    assert_eq!(before, after, "project_scrollback moved the authoritative terminal");
    assert_eq!(before_damage, FrameKind::Full);

    // And it did not consume damage either: the next frame is still a delta of
    // exactly what happened after it, not a Full caused by the viewport.
    harness.feed(b"z");
    let frame = projector.next(&mut harness.term);
    assert_eq!(
        frame.kind,
        FrameKind::Delta,
        "the scrollback view disturbed the damage state; it must not touch damage at all"
    );
}

#[test]
fn max_scrollback_is_the_history_size() {
    let mut harness = Harness::with_history(20, 4, 50);
    assert_eq!(max_scrollback(&harness.term), 0);
    fill(&mut harness, 30, "row");
    assert_eq!(max_scrollback(&harness.term), harness.term.grid().history_size());
    assert!(max_scrollback(&harness.term) >= 20, "history did not accumulate");

    // Scrolling exactly to the limit lands on the topmost line.
    let frame = project_scrollback(&harness.term, max_scrollback(&harness.term));
    assert_eq!(Line(frame.rows_changed[0].line), harness.term.grid().topmost_line());
}

#[test]
fn project_window_never_reads_past_the_bottom_of_the_grid() {
    // The bound that `project_scrollback` never needed. Its window is always
    // exactly `screen_lines`, which `total_lines >= screen_lines` guarantees
    // fits. `project_window` takes `rows` from the caller, so an over-long
    // window would walk past `bottommost_line` -- where Grid's bounds checks are
    // `debug_assert!` only, so a RELEASE build wraps into the ring's physical
    // store and returns rows that are not logically in the grid.
    //
    // Asserted on the frame's own report: every projected line must be inside
    // [topmost, bottommost], and the frame must say how many rows it really
    // carries rather than the number that was asked for.
    let mut harness = Harness::new(20, 6);
    harness.feed_str("one\r\ntwo\r\nthree\r\n");
    let term = &harness.term;
    let grid = term.grid();
    let (topmost, bottommost) = (grid.topmost_line().0, grid.bottommost_line().0);

    for rows in [1usize, 6, 7, 50, 10_000] {
        let frame = project_window(term, topmost, rows);
        assert!(
            frame
                .rows_changed
                .iter()
                .all(|r| r.line >= topmost && r.line <= bottommost),
            "rows={rows}: projected a line outside the grid"
        );
        assert_eq!(
            frame.rows_changed.len(),
            frame.rows as usize,
            "rows={rows}: the frame's row count must match the rows it carries"
        );
        assert!(
            frame.rows_changed.len() <= (bottommost - topmost + 1) as usize,
            "rows={rows}: more rows than the grid has"
        );
    }
}

#[test]
fn project_window_at_the_live_bottom_matches_the_offset_projection() {
    // The two spellings must agree where they overlap, or the renderer path and
    // the legacy offset path would disagree about what "the screen" is.
    let mut harness = Harness::new(20, 6);
    harness.feed_str("alpha\r\nbeta\r\ngamma\r\n");
    let a = project_scrollback(&harness.term, 0);
    let b = project_window(&harness.term, 0, harness.term.screen_lines());
    assert_eq!(a.rows_changed, b.rows_changed);
    assert_eq!(a.rows, b.rows);
}
