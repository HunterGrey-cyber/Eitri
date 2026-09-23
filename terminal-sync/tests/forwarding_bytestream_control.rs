//! NEGATIVE CONTROL for the *forwarding* axis: a reasonable BYTE-STREAM-DRIVEN forwarding test.
//!
//! This is the shape of test the previous prototype relied on, written in good faith: push a
//! representative corpus of escape sequences through the driver into a real `Term` and assert the
//! resulting screen and cursor. It passes on the correct `SyncSpy`.
//!
//! It is kept ONLY so the mutation sweep can be run against it too, quantifying how much of the
//! mutation space a byte-stream test cannot reach:
//!
//! ```sh
//! python3 scripts/mutation_sweep.py --test "cargo test --offline --test forwarding_bytestream_control"
//! ```
//!
//! Do not "improve" it. Its weakness is the measurement.

mod common;

use common::{new_term, snapshot};
use terminal_sync::SyncDriver;

/// A corpus that exercises the primitives a TUI actually uses: absolute and relative cursor
/// moves, save/restore, insert/delete lines and characters, erase, scroll, tabs, SGR, line
/// feeds, reverse index, charset selection, window title, alternate screen.
const CORPUS: &[&[u8]] = &[
    b"\x1b[2J\x1b[H",
    b"\x1b]0;a title\x07",
    b"\x1b[1;1Hline-one",
    b"\r\n",
    b"line-two",
    b"\x1b[3;1Hline-three",
    b"\x1b[s", // save cursor
    b"\x1b[10;5H",
    b"\x1b[u", // restore cursor
    b"\x1b[1m\x1b[31mbold-red\x1b[0m",
    b"\x1b[2;3H\x1b[4@", // insert blanks
    b"\x1b[2;3H\x1b[2P", // delete chars
    b"\x1b[2;3H\x1b[3X", // erase chars
    b"\x1b[2;1H\x1b[L",  // insert blank line
    b"\x1b[4;1H\x1b[M",  // delete line
    b"\x1b[S",           // scroll up
    b"\x1b[T",           // scroll down
    b"\x1b[5;1Htabbed\tcolumn",
    b"\x1b[6;1HbackspaceX\x08!",
    b"\x1b[7;1Hfeed\n",
    b"\x1bM",              // reverse index
    b"\x1b[8;1H\x1b[K",    // clear line
    b"\x1b[?25l\x1b[?25h", // hide/show cursor
    b"\x1b(0lqk\x1b(B",    // charset select
    b"\x1b[H\x1b[?2026hsync-a\x1b[Hsync-final\x1b[?2026l",
];

#[test]
fn byte_stream_forwarding() {
    let mut term = new_term();
    let mut driver = SyncDriver::new();
    let mut publications = 0usize;
    for chunk in CORPUS {
        driver.feed(&mut term, chunk, |_, _| publications += 1);
    }

    let rendered = snapshot(&term);
    assert_eq!(
        rendered,
        // Golden: recorded from the correct SyncSpy, not hand-derived.
        "sync-final\n\nli   e-two\n\ntabbed\t column\nbackspace!\nfeed\n\u{250c}\u{2500}\u{2510}",
        "byte-stream forwarding produced the wrong screen"
    );
    assert!(publications > 0);
}
