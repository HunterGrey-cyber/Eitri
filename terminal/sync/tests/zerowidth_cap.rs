//! A per-cell combining-mark cap, end to end through a real `Term` and the byte-at-a-time
//! `SyncDriver`/`vte::ansi::Processor`: one base character followed by far more than the cap's
//! worth of a real zero-width combining mark must leave a BOUNDED `Vec` on the `Term` itself,
//! not the unbounded one `Term::input` (alacritty_terminal 0.26.0) would accumulate on its own.
//!
//! `tests/forwarding.rs::zero_width_input_is_capped_by_what_the_target_cell_already_holds` pins
//! the cap at the `SyncSpy` level over a one-cell stand-in; this file drives the real `Term`
//! through the real `SyncDriver`, which is where the cell the next mark lands on is actually
//! decided.

mod common;

use alacritty_terminal::index::{Column, Line};
use common::new_term;
use terminal_sync::{SyncDriver, MAX_ZERO_WIDTH_MARKS_PER_CELL};

#[test]
fn a_huge_run_of_combining_marks_is_capped_on_the_real_term() {
    let mut term = new_term();
    let mut driver = SyncDriver::new();

    // "a" then far more than the cap's worth of U+0301 COMBINING ACUTE ACCENT (2 UTF-8 bytes,
    // width 0). 10,000 is already 625x the cap: enough to test the bound without
    // measuring how large an unbounded run can grow.
    let mut stream = b"a".to_vec();
    for _ in 0..10_000 {
        stream.extend_from_slice("\u{0301}".as_bytes());
    }

    driver.feed(&mut term, &stream, |_, _| {});

    let cell = &term.grid()[Line(0)][Column(0)];
    assert_eq!(cell.c, 'a', "the base character itself is untouched by the cap");
    let stored = cell.zerowidth().map(<[char]>::len).unwrap_or(0);
    assert!(
        stored <= MAX_ZERO_WIDTH_MARKS_PER_CELL as usize,
        "Term accumulated {stored} zero-width marks on one cell -- the input-boundary cap did not hold"
    );
    assert!(stored > 0, "a bounded run is not the same as dropping every mark");
}

/// Feed the real byte-at-a-time `SyncDriver`/`vte::ansi::Processor` one base character,
/// then batches of `MAX_ZERO_WIDTH_MARKS_PER_CELL` U+0301 marks and one BEL byte (0x07).
/// `Term::bell` in alacritty_terminal 0.26.0 only fires an event; it never moves the cursor
/// or writes a cell. A per-run counter reset on BEL therefore lets every batch add a fresh
/// allowance to cell (0, 0), at about 33 bytes per 16 marks, barely above two bytes per mark
/// without a cap. With 10,000 batches that bypass produces 160,000 marks on one cell;
/// a cap based on the cell's contents must keep the count far below that.
#[test]
fn zerowidth_run_bypass_via_bell_is_capped_on_the_real_term() {
    let mut term = new_term();
    let mut driver = SyncDriver::new();

    let mut stream = b"a".to_vec();
    for _ in 0..10_000 {
        for _ in 0..MAX_ZERO_WIDTH_MARKS_PER_CELL {
            stream.extend_from_slice("\u{0301}".as_bytes());
        }
        stream.push(0x07); // BEL
    }

    driver.feed(&mut term, &stream, |_, _| {});

    let cell = &term.grid()[Line(0)][Column(0)];
    assert_eq!(cell.c, 'a', "BEL never moves the cursor off the base character's cell");
    let stored = cell.zerowidth().map(<[char]>::len).unwrap_or(0);
    assert!(
        stored <= MAX_ZERO_WIDTH_MARKS_PER_CELL as usize,
        "Term accumulated {stored} zero-width marks on one cell across 10,000 BEL-interleaved \
         batches -- a dispatch that cannot move the cursor renewed the cell's allowance"
    );
}

/// The most zero-width marks any one cell of the visible grid holds.
fn most_marks_on_any_cell(term: &alacritty_terminal::Term<alacritty_terminal::event::VoidListener>) -> usize {
    use common::{COLUMNS, SCREEN_LINES};
    let grid = term.grid();
    let mut most = 0;
    for line in 0..SCREEN_LINES {
        for col in 0..COLUMNS {
            let n = grid[Line(line as i32)][Column(col)]
                .zerowidth()
                .map_or(0, <[char]>::len);
            most = most.max(n);
        }
    }
    most
}

/// Feeds `prefix`, then 10,000 batches of (`MAX_ZERO_WIDTH_MARKS_PER_CELL` x U+0301 +
/// `separator`), and returns the most marks any one cell ended up holding. 10,000 batches is
/// 160,000 marks on one cell if `separator` renews the cell's allowance.
fn marks_after_batches_separated_by(prefix: &[u8], separator: &[u8]) -> usize {
    let mut term = new_term();
    let mut driver = SyncDriver::new();
    let mut stream = prefix.to_vec();
    for _ in 0..10_000 {
        for _ in 0..MAX_ZERO_WIDTH_MARKS_PER_CELL {
            stream.extend_from_slice("\u{0301}".as_bytes());
        }
        stream.extend_from_slice(separator);
    }
    driver.feed(&mut term, &stream, |_, _| {});
    most_marks_on_any_cell(&term)
}

/// Counting potentially cursor-moving dispatches can renew the allowance without moving
/// `Term`'s cursor. Each separator here can let batches accumulate 160,000 marks on cell (0, 0):
/// - DEL 0x7F and U+0085, which byte-at-a-time feeding lets vte print, reach `input`, but
///   `Term::input` returns without touching anything when `width()` is `None`;
/// - `ESC[?25h` shows the cursor, a non-2026 private mode that does not move it;
/// - `ESC[1;2H` moves the cursor to where it already is.
/// The cap must inspect marks already on the destination cell so dispatches cannot renew it.
#[test]
fn no_separator_renews_a_cells_allowance_on_the_real_term() {
    let cap = MAX_ZERO_WIDTH_MARKS_PER_CELL as usize;
    let mut failures = Vec::new();
    for (name, separator) in [
        ("DEL 0x7F", &b"\x7f"[..]),
        ("U+0085", "\u{0085}".as_bytes()),
        ("ESC[?25h", &b"\x1b[?25h"[..]),
        ("ESC[1;2H (goto where the cursor already is)", &b"\x1b[1;2H"[..]),
        ("BEL", &b"\x07"[..]),
    ] {
        let most = marks_after_batches_separated_by(b"a", separator);
        if most > cap {
            failures.push(format!("  {name}: a cell holds {most} zero-width marks"));
        }
    }
    assert!(
        failures.is_empty(),
        "after 10,000 separated batches (cap {cap}):\n{}",
        failures.join("\n")
    );
}

/// With DECAWM off, a wide character at the last column sets `input_needs_wrap` and returns
/// without writing. Repeating a wide character followed by marks must not renew the cap:
/// every batch's marks still land on the same last-column cell.
#[test]
fn a_wide_char_that_cannot_be_written_does_not_renew_the_allowance() {
    let cap = MAX_ZERO_WIDTH_MARKS_PER_CELL as usize;
    let most = marks_after_batches_separated_by(b"\x1b[?7l\x1b[1;40H", "\u{4e2d}".as_bytes());
    assert!(most <= cap, "a cell holds {most} zero-width marks (cap {cap})");
}

/// The cap is per cell, not per stream: a capped cell's neighbour still gets its own full
/// allowance, and a wide character's marks land on its first half (Term's own rule, which
/// `ZeroWidthTarget for Term` mirrors), capped there too.
#[test]
fn each_cell_gets_its_own_allowance_on_the_real_term() {
    let cap = MAX_ZERO_WIDTH_MARKS_PER_CELL as usize;
    let mut term = new_term();
    let mut driver = SyncDriver::new();
    let mut stream = Vec::new();
    for base in ["a", "b", "\u{4e2d}"] {
        stream.extend_from_slice(base.as_bytes());
        for _ in 0..1_000 {
            stream.extend_from_slice("\u{0301}".as_bytes());
        }
    }
    driver.feed(&mut term, &stream, |_, _| {});

    let marks = |col: usize| term.grid()[Line(0)][Column(col)].zerowidth().map_or(0, <[char]>::len);
    assert_eq!(marks(0), cap, "'a'");
    assert_eq!(marks(1), cap, "'b' is a new cell with its own allowance");
    assert_eq!(marks(2), cap, "the wide character's first half");
    assert_eq!(marks(3), 0, "never its spacer");
}
