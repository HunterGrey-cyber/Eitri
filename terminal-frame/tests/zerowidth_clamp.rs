//! sw-terminal-2's SECOND, independent guard. `Term::input` (alacritty_terminal 0.26.0)
//! accumulates a zero-width combining mark's `Vec` on a cell with no cap of its own --
//! `terminal_sync::SyncSpy::input` caps what reaches a `Term` driven through it, but this
//! crate's own `project_cell` must bound what it COPIES into a `FrameCell` regardless of how the
//! `Term` it is handed got that way (a raw byte stream here, `Term::grid_mut()` elsewhere, per
//! this crate's own module doc). This test drives `Term` directly through the real
//! `vte::ansi::Processor` -- `terminal-frame` does not depend on `terminal-sync` at all -- so the
//! `Term` really does hold the whole, uncapped run before the projector ever sees it.

mod common;

use alacritty_terminal::index::{Column, Line};
use common::Harness;
use terminal_frame::{Projector, MAX_ZERO_WIDTH_MARKS_PER_CELL};

#[test]
fn a_term_holding_an_unbounded_zerowidth_run_is_projected_bounded() {
    let mut h = Harness::new(10, 3);
    h.feed_str("a");
    // Far more than any reasonable cap: nothing at this layer limits it.
    h.feed_str(&"\u{0301}".repeat(10_000));

    let stored = h.term.grid()[Line(0)][Column(0)]
        .zerowidth()
        .map(|z| z.len())
        .unwrap_or(0);
    assert_eq!(
        stored, 10_000,
        "sanity: Term itself really did accumulate the whole run with nothing here capping it"
    );

    let mut projector = Projector::new();
    let frame = projector.full(&h.term);
    let row0 = frame
        .rows_changed
        .iter()
        .find(|r| r.line == 0)
        .expect("row 0 is non-default and must be reported");
    let cell = &row0.cells[0];
    assert_eq!(cell.c, 'a');
    assert!(
        cell.zerowidth().len() <= MAX_ZERO_WIDTH_MARKS_PER_CELL,
        "the projector copied {} zero-width marks into one FrameCell -- unbounded",
        cell.zerowidth().len()
    );
    assert!(
        !cell.zerowidth().is_empty(),
        "a bounded run is not the same as dropping every mark"
    );
}
