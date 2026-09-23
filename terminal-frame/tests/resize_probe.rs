//! PHASE 3 RESIZE GATE -- the experiment that decides the semantic rule.
//!
//! Question: what does a pinned historical Raw View mean when terminal geometry changes?
//! The answer must be exactly one of
//!   A. exact anchor preservation,
//!   B. deterministic re-anchor,
//!   C. explicit return to bottom,
//! and what is forbidden is a resize that silently shows a different historical region.
//!
//! Two candidate anchors are tested against real reflow:
//!   * ABSOLUTE GRID LINE -- what the pre-resize work used. Cheap, and the open question was
//!     whether it survives resize at all.
//!   * LOGICAL LINE -- a maximal run of rows joined by WRAPLINE, i.e. one "line the application
//!     printed" regardless of how it is currently wrapped. Reflow re-wraps, so this is the
//!     candidate for surviving a column change.
//!
//! Upstream's own behaviour is the strongest hint and is asserted here too: `Term::resize` sets
//! `selection = None` when the column count changes, but merely ROTATES the selection by the line
//! delta when only lines change. Alacritty itself treats a column resize as invalidating
//! positional state and a line resize as remappable.
//!
//! ==========================================================================================
//! VERDICT: CATEGORY A -- exact anchor preservation, with the anchor being a MONOTONIC LOGICAL
//! LINE ORDINAL. Measured, not chosen:
//!
//!   line-only resize      content moves by a computable delta; the arithmetic mapping holds.
//!   column resize         absolute grid lines STOP identifying content (asserted), but the
//!                         logical-line sequence is preserved exactly:
//!                           index 14 -> 14 -> 14, counts 41 -> 41 -> 41
//!                         across narrow-then-widen. Reflow re-wraps; it does not reorder,
//!                         split or merge what the application printed.
//!
//! THE ANCHOR MUST BE AN ORDINAL, NOT AN INDEX. Under saturation, narrowing makes each logical
//! line occupy more rows, so the oldest are evicted and every index-from-top shifts:
//!           index_before=38  index_after=4   counts 70 -> 36
//! while the anchored CONTENT survived. So the viewport holds an ordinal the writer assigns once
//! and never reuses, and resolves it as `ordinal - oldest_retained_ordinal`. An index into the
//! current buffer is exactly the representation that drifts.
//!
//! THE ONE BOUNDED FALLBACK. If the anchored ordinal has been evicted -- by continued output or
//! by a narrowing that shrank capacity -- there IS no correct content to show. The rule is to
//! report the anchor gone, explicitly, and let the client decide (return to bottom being the
//! obvious choice). What is forbidden, and what an offset-based viewport does by construction,
//! is landing on whatever now occupies the old position and presenting it as the pinned view.
//! ==========================================================================================

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

#[derive(Clone, Copy, Debug)]
struct Dims {
    lines: usize,
    cols: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn term(lines: usize, cols: usize, history: usize) -> Term<VoidListener> {
    let config = Config {
        scrolling_history: history,
        ..Config::default()
    };
    Term::new(config, &Dims { lines, cols }, VoidListener)
}

fn feed(term: &mut Term<VoidListener>, parser: &mut Processor, bytes: &[u8]) {
    parser.advance(term, bytes);
}

/// Text of one grid row, trailing blanks trimmed. Always clamped -- an unclamped read in release
/// can return content the terminal was told to erase.
fn row_text(term: &Term<VoidListener>, line: i32) -> String {
    let grid = term.grid();
    let l = Line(line).grid_clamp(grid, Boundary::Grid);
    let row = &grid[l];
    let mut s = String::new();
    for c in 0..grid.columns() {
        s.push(row[Column(c)].c);
    }
    s.trim_end().to_owned()
}

/// Does this row continue into the next one? Alacritty marks the CONTINUING row, not the
/// continuation, so a logical line is `row` plus every following row until one lacks WRAPLINE.
fn wraps(term: &Term<VoidListener>, line: i32) -> bool {
    let grid = term.grid();
    let l = Line(line).grid_clamp(grid, Boundary::Grid);
    let row = &grid[l];
    (0..grid.columns()).any(|c| row[Column(c)].flags.contains(Flags::WRAPLINE))
}

/// Every logical line currently in the grid, oldest first, as joined text.
///
/// This is the candidate anchor space: if reflow preserves logical lines and their order, an index
/// into this sequence is stable across a column resize even though absolute grid lines are not.
fn logical_lines(term: &Term<VoidListener>) -> Vec<String> {
    let grid = term.grid();
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32 - 1;
    let mut out = Vec::new();
    let mut current = String::new();
    let mut open = false;
    for line in top..=bottom {
        let text = row_text(term, line);
        if wraps(term, line) {
            // A wrapped row is full-width: take it verbatim, not trimmed, or the join loses spaces.
            let grid = term.grid();
            let l = Line(line).grid_clamp(grid, Boundary::Grid);
            let row = &grid[l];
            let mut raw = String::new();
            for c in 0..grid.columns() {
                raw.push(row[Column(c)].c);
            }
            current.push_str(&raw);
            open = true;
        } else {
            current.push_str(&text);
            out.push(std::mem::take(&mut current));
            open = false;
        }
    }
    if open {
        out.push(current);
    }
    out
}

fn fill(term: &mut Term<VoidListener>, parser: &mut Processor, from: usize, to: usize) {
    for i in from..=to {
        feed(term, parser, format!("L{i:04}\r\n").as_bytes());
    }
}

// ---------------------------------------------------------------------------------------------

#[test]
fn baseline_absolute_line_identifies_content_before_any_resize() {
    let mut t = term(10, 40, 1000);
    let mut p = Processor::new();
    fill(&mut t, &mut p, 1, 60);

    // Some line well back in history.
    let target = -30;
    let text = row_text(&t, target);
    assert!(
        text.starts_with('L'),
        "precondition: history holds identifiable lines, got {text:?}"
    );
    assert_eq!(
        t.grid().display_offset(),
        0,
        "the authoritative viewport never moves for browsing"
    );
}

#[test]
fn lines_only_resize_moves_content_by_a_computable_delta_not_by_reflow() {
    let mut t = term(10, 40, 1000);
    let mut p = Processor::new();
    fill(&mut t, &mut p, 1, 60);

    let before_at_minus_30 = row_text(&t, -30);
    let before_history = t.grid().history_size();

    // Lines only. Columns unchanged, so nothing re-wraps.
    t.resize(Dims { lines: 20, cols: 40 });

    let after_history = t.grid().history_size();
    let delta = before_history as i32 - after_history as i32;

    // The SAME text is still findable, shifted by exactly that delta. This is what makes a
    // line-only resize category A: the mapping is arithmetic, not a search.
    let after = row_text(&t, -30 + delta);
    assert_eq!(
        after,
        before_at_minus_30,
        "line-only resize must move history by a computable delta ({delta}); \
         abs {} -> {}",
        -30,
        -30 + delta
    );
    assert_eq!(t.grid().display_offset(), 0);
}

#[test]
fn column_resize_reflows_so_absolute_lines_stop_identifying_content() {
    let mut t = term(10, 40, 1000);
    let mut p = Processor::new();
    // Lines long enough that narrowing MUST re-wrap them.
    for i in 1..=40 {
        feed(&mut t, &mut p, format!("L{i:04}-{}\r\n", "x".repeat(30)).as_bytes());
    }

    let before = row_text(&t, -20);
    t.resize(Dims { lines: 10, cols: 20 }); // narrower -> reflow
    let after = row_text(&t, -20);

    assert_ne!(
        after, before,
        "if these were equal the corpus never actually reflowed and this gate proves nothing"
    );
}

#[test]
fn logical_line_identity_survives_narrowing_and_widening() {
    let mut t = term(10, 40, 1000);
    let mut p = Processor::new();
    for i in 1..=40 {
        feed(&mut t, &mut p, format!("L{i:04}-{}\r\n", "x".repeat(30)).as_bytes());
    }

    let before = logical_lines(&t);
    let anchor_text = before.iter().find(|l| l.starts_with("L0015")).cloned();
    assert!(
        anchor_text.is_some(),
        "precondition: the anchor line must be in history"
    );
    let anchor_index = before.iter().position(|l| l.starts_with("L0015")).unwrap();

    t.resize(Dims { lines: 10, cols: 20 }); // narrow
    let narrowed = logical_lines(&t);
    let narrowed_index = narrowed.iter().position(|l| l.starts_with("L0015"));

    t.resize(Dims { lines: 10, cols: 60 }); // wide again
    let widened = logical_lines(&t);
    let widened_index = widened.iter().position(|l| l.starts_with("L0015"));

    // What this actually establishes is printed, not assumed: whether the logical-line SEQUENCE is
    // preserved, and whether the INDEX into it is stable.
    eprintln!(
        "RESIZE GATE logical-line anchor: before_index={anchor_index} narrowed_index={narrowed_index:?} \
         widened_index={widened_index:?} counts before={} narrowed={} widened={}",
        before.len(),
        narrowed.len(),
        widened.len()
    );

    assert!(
        narrowed_index.is_some(),
        "the logical line must still EXIST after narrowing"
    );
    assert!(widened_index.is_some(), "and after widening again");
}

#[test]
fn upstream_itself_discards_selection_on_a_column_resize_but_rotates_it_on_a_line_resize() {
    // Not our code, but it is the precedent that decides the rule: alacritty treats a column
    // change as invalidating positional state and a line change as remappable.
    use alacritty_terminal::index::{Point, Side};
    use alacritty_terminal::selection::{Selection, SelectionType};

    let mut t = term(10, 40, 1000);
    let mut p = Processor::new();
    fill(&mut t, &mut p, 1, 40);

    t.selection = Some(Selection::new(
        SelectionType::Simple,
        Point::new(Line(-5), Column(0)),
        Side::Left,
    ));
    t.resize(Dims { lines: 12, cols: 40 }); // lines only
    assert!(
        t.selection.is_some(),
        "a line-only resize keeps the selection (rotated)"
    );

    t.selection = Some(Selection::new(
        SelectionType::Simple,
        Point::new(Line(-5), Column(0)),
        Side::Left,
    ));
    t.resize(Dims { lines: 12, cols: 30 }); // columns change
    assert!(
        t.selection.is_none(),
        "a column resize discards it -- upstream's own verdict"
    );
}

#[test]
fn history_saturation_plus_resize_is_where_a_naive_offset_drifts() {
    // The combination the plan calls out: once history saturates, history_size() stops growing, so
    // anything tracking movement by its delta goes blind -- and then a resize moves content again.
    let mut t = term(10, 40, 50); // deliberately small history
    let mut p = Processor::new();
    fill(&mut t, &mut p, 1, 200); // far past saturation

    assert_eq!(t.grid().history_size(), 50, "precondition: history is saturated");
    let pinned_abs = -25;
    let pinned_text = row_text(&t, pinned_abs);

    fill(&mut t, &mut p, 201, 220); // more output while saturated
    let after_output = row_text(&t, pinned_abs);
    assert_ne!(
        after_output, pinned_text,
        "a raw absolute offset drifts under continued output once history is saturated"
    );

    t.resize(Dims { lines: 20, cols: 40 });
    let after_resize = row_text(&t, pinned_abs);
    eprintln!(
        "RESIZE GATE saturation: pinned={pinned_text:?} after_output={after_output:?} after_resize={after_resize:?}"
    );
}

#[test]
fn saturated_history_plus_narrowing_shifts_index_from_top_but_not_content_identity() {
    // The case the first logical-line test did NOT exercise: with history saturated, narrowing
    // turns each logical line into MORE rows, so the top of history is evicted and every
    // index-from-top shifts. Content identity is what survives; an ordinal counted from the top
    // does not.
    let mut t = term(10, 40, 60); // small history so narrowing evicts
    let mut p = Processor::new();
    for i in 1..=80 {
        feed(&mut t, &mut p, format!("L{i:04}-{}\r\n", "x".repeat(30)).as_bytes());
    }
    assert_eq!(t.grid().history_size(), 60, "precondition: saturated");

    let before = logical_lines(&t);
    let anchor = before
        .iter()
        .find(|l| l.contains("L0050"))
        .cloned()
        .expect("anchor present");
    let index_before = before.iter().position(|l| l.contains("L0050")).unwrap();

    t.resize(Dims { lines: 10, cols: 20 }); // narrow: each logical line needs more rows
    let after = logical_lines(&t);
    let index_after = after.iter().position(|l| l.contains("L0050"));

    eprintln!(
        "RESIZE GATE saturated-narrow: index_before={index_before} index_after={index_after:?} \
         counts before={} after={} anchor_survives={}",
        before.len(),
        after.len(),
        index_after.is_some()
    );

    if let Some(i) = index_after {
        // Content identity holds even where the ordinal moved.
        assert!(after[i].contains("L0050"), "the located line must be the anchored one");
    }
    // Either way this test's job is to REPORT, not to assert a preference: the printed numbers are
    // what decides whether the rule needs an eviction branch.
    let _ = anchor;
}

#[test]
fn an_anchor_that_falls_out_of_history_is_detectably_gone_not_silently_replaced() {
    // The case that forces an explicit fallback: if the pinned logical line is evicted, there is
    // no correct content to show. The rule must report that, never substitute a neighbour.
    let mut t = term(10, 40, 30);
    let mut p = Processor::new();
    fill(&mut t, &mut p, 1, 60);

    let anchored = "L0005";
    assert!(
        !logical_lines(&t).iter().any(|l| l.contains(anchored)),
        "precondition: L0005 must already be evicted past a 30-line history"
    );

    // A viewport holding that anchor can PROVE it is gone by searching, rather than landing on
    // whatever now occupies the old offset.
    let found = logical_lines(&t).iter().position(|l| l.contains(anchored));
    assert_eq!(
        found, None,
        "an evicted anchor must be reported missing, not resolved to a neighbour"
    );
}
