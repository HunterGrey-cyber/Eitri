//! THE GATE. Exhaustive proof that `SyncSpy` forwards every `vte::ansi::Handler` method.
//!
//! WHY THIS TEST IS SHAPED THE WAY IT IS.
//!
//! The previous prototype's forwarding test was BYTE-STREAM driven: it pushed escape sequences
//! through the parser and checked the terminal reacted. That can only ever cover the methods
//! some escape sequence happens to trigger. An adversarial sweep deleted each of the 71 forwards
//! one at a time: 26 died, **42 survived green** -- including `scroll_up`, `scroll_down`,
//! `insert_blank_lines`, `delete_lines`, `erase_chars`, `delete_chars`, `insert_blank`,
//! `clear_line`, `backspace`, `newline`, `reverse_index` and `save_cursor_position`, i.e. the
//! most load-bearing TUI primitives there are.
//!
//! So this test is DIRECT, not byte-stream driven: for every method of the trait it calls that
//! method on a `SyncSpy` wrapping a recording inner handler and asserts the recorder saw exactly
//! that call with exactly those arguments. It is exhaustive BY CONSTRUCTION, independent of
//! which escapes vte emits.
//!
//! The method list is regenerated from vte's own `pub trait Handler` declaration by `build.rs`
//! on every build (see that file for why a build script and not a checked-in list). If vte grows
//! a method, this test grows a case; if `SyncSpy` lacks the forward, the case fails.

use terminal_sync::{SyncBarrier, SyncSpy, ZeroWidthTarget, MAX_ZERO_WIDTH_MARKS_PER_CELL};

include!(concat!(env!("OUT_DIR"), "/generated_handler_methods.rs"));

/// Sanity: the generator actually found the trait, and found all of it.
#[test]
fn generated_method_list_is_plausible() {
    assert_eq!(VTE_VERSION, "0.15.0", "vte version changed; re-verify the whole gate");
    assert_eq!(
        HANDLER_METHODS.len(),
        71,
        "vte::ansi::Handler method count changed (source: {VTE_HANDLER_SOURCE}). \
         This is the drift alarm -- re-read the trait and extend SyncSpy."
    );
    let mut sorted = HANDLER_METHODS.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), HANDLER_METHODS.len(), "duplicate method names parsed");
}

/// EVERY Handler method must reach the inner handler, with its arguments intact.
///
/// A forward that was never written falls through to the trait's silent no-op default, so the
/// recorder's log is empty and this fails. A forward that goes to the wrong method, or passes
/// the wrong / swapped arguments, produces a different log line and this fails too (same-typed
/// parameters are given index-dependent values precisely so a swap is visible).
#[test]
fn every_handler_method_forwards_to_the_inner_handler() {
    let mut failures = Vec::new();
    for (idx, name) in HANDLER_METHODS.iter().enumerate() {
        let mut inner = RecordingHandler::default();
        let mut barrier = SyncBarrier::new();
        let expected = {
            let mut spy = SyncSpy::new(&mut inner, &mut barrier);
            call_handler_method(&mut spy, idx)
        };
        if inner.log != vec![expected.clone()] {
            failures.push(format!(
                "  {name}: expected inner handler to observe [{expected}], observed {:?}",
                inner.log
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} Handler forwards are missing or wrong:\n{}",
        failures.len(),
        HANDLER_METHODS.len(),
        failures.join("\n")
    );
}

/// EVERY Handler method must also tell the barrier a dispatch happened.
///
/// Publication is gated on dispatch, not on byte arrival. A method that forwards correctly but
/// forgets to signal the barrier makes its own output silently unpublishable -- the same class
/// of silent defect, one layer over.
#[test]
fn every_handler_method_signals_the_barrier() {
    let mut failures = Vec::new();
    for (idx, name) in HANDLER_METHODS.iter().enumerate() {
        let mut inner = RecordingHandler::default();
        let mut barrier = SyncBarrier::new();
        {
            let mut spy = SyncSpy::new(&mut inner, &mut barrier);
            let _ = call_handler_method(&mut spy, idx);
        }
        if barrier.dispatch_count() != 1 {
            failures.push(format!(
                "  {name}: barrier saw {} dispatches, expected exactly 1",
                barrier.dispatch_count()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} Handler methods do not signal the barrier:\n{}",
        failures.len(),
        HANDLER_METHODS.len(),
        failures.join("\n")
    );
}

/// Non-2026 private modes must NOT be mistaken for BSU/ESU.
#[test]
fn ordinary_private_modes_do_not_drive_the_barrier() {
    use terminal_sync::vte::ansi::{Handler as _, NamedPrivateMode, PrivateMode};

    let mut inner = RecordingHandler::default();
    let mut barrier = SyncBarrier::new();
    {
        let mut spy = SyncSpy::new(&mut inner, &mut barrier);
        spy.set_private_mode(PrivateMode::Named(NamedPrivateMode::ShowCursor));
        spy.unset_private_mode(PrivateMode::Named(NamedPrivateMode::BracketedPaste));
        spy.set_private_mode(PrivateMode::Unknown(2025));
        spy.unset_private_mode(PrivateMode::Unknown(2027));
    }
    assert_eq!(barrier.bsu_count(), 0);
    assert_eq!(barrier.esu_count(), 0);
    assert!(!barrier.in_sync());
    assert_eq!(inner.log.len(), 4, "all four must still forward");
}

/// `RecordingHandler` has no cells, so no cell is ever full: every zero-width `input` reaches it,
/// which is what the exhaustive forwarding tests above need.
impl ZeroWidthTarget for RecordingHandler {
    fn zerowidth_at_input_target(&self) -> usize {
        0
    }
}

/// A handler with a single cell: a character with a width starts a new cell, a zero-width one
/// piles onto it. It reports the pile to `SyncSpy` the way `Term` reports its target cell's.
#[derive(Default)]
struct OneCell {
    marks: usize,
    received: Vec<char>,
}

impl terminal_sync::vte::ansi::Handler for OneCell {
    fn input(&mut self, c: char) {
        if c == '\u{0301}' {
            self.marks += 1;
        } else {
            self.marks = 0;
        }
        self.received.push(c);
    }
}

impl ZeroWidthTarget for OneCell {
    fn zerowidth_at_input_target(&self) -> usize {
        self.marks
    }
}

/// sw-terminal-2. `Term::input` (alacritty_terminal 0.26.0) pushes every zero-width character
/// onto a cell's `Vec` with no cap of its own. `SyncSpy::input` forwards a zero-width character
/// only while the handler reports fewer than `MAX_ZERO_WIDTH_MARKS_PER_CELL` on the cell it would
/// land on; one it drops is not a dispatch. The count comes from the handler, so a new cell gets a
/// full allowance and nothing else can renew an old one (`tests/zerowidth_cap.rs` drives the real
/// `Term` with every separator that renewed the dispatch-counting versions of this cap).
#[test]
fn zero_width_input_is_capped_by_what_the_target_cell_already_holds() {
    use terminal_sync::vte::ansi::Handler as _;

    let cap = MAX_ZERO_WIDTH_MARKS_PER_CELL as usize;
    let mut inner = OneCell::default();
    let mut barrier = SyncBarrier::new();
    {
        let mut spy = SyncSpy::new(&mut inner, &mut barrier);
        spy.input('a');
        for _ in 0..1_000 {
            spy.input('\u{0301}');
        }
    }
    assert_eq!(
        inner.received.len(),
        1 + cap,
        "the base character plus exactly the cap's worth of marks"
    );
    assert_eq!(
        barrier.dispatch_count(),
        1 + cap as u64,
        "a dropped mark changes nothing, so it is not a dispatch"
    );

    // A bell between marks is still forwarded; the marks around it are still dropped.
    {
        let mut spy = SyncSpy::new(&mut inner, &mut barrier);
        spy.bell();
        spy.input('\u{0301}');
    }
    assert_eq!(
        inner.received.len(),
        1 + cap,
        "a full cell stays full whatever is dispatched between marks"
    );

    // A character that starts a new cell gets that cell a full allowance of its own.
    {
        let mut spy = SyncSpy::new(&mut inner, &mut barrier);
        spy.input('b');
        for _ in 0..1_000 {
            spy.input('\u{0301}');
        }
    }
    assert_eq!(inner.received.len(), 2 + 2 * cap);

    // A character with no width at all (DEL) is not a zero-width mark: forwarded, for `Term` to
    // ignore, never capped.
    {
        let mut spy = SyncSpy::new(&mut inner, &mut barrier);
        spy.input('\u{7f}');
    }
    assert_eq!(inner.received.last(), Some(&'\u{7f}'));
}

/// The two intercepted methods must do BOTH things: drive the barrier AND still forward.
#[test]
fn sync_update_mode_is_intercepted_and_still_forwarded() {
    use terminal_sync::vte::ansi::{Handler as _, NamedPrivateMode, PrivateMode};

    let sync = PrivateMode::Named(NamedPrivateMode::SyncUpdate);
    let mut inner = RecordingHandler::default();
    let mut barrier = SyncBarrier::new();
    {
        let mut spy = SyncSpy::new(&mut inner, &mut barrier);
        spy.set_private_mode(sync);
    }
    assert!(barrier.in_sync(), "BSU must open the barrier");
    assert_eq!(barrier.bsu_count(), 1);

    {
        let mut spy = SyncSpy::new(&mut inner, &mut barrier);
        spy.unset_private_mode(sync);
    }
    assert!(!barrier.in_sync(), "ESU must close the barrier");
    assert_eq!(barrier.esu_count(), 1);

    // ... and alacritty_terminal still has to see them: it ignores 2026 in set/unset
    // (term/mod.rs:1992, :2041) but reports it as recognised-but-Reset from DECRQM (:2084),
    // so swallowing the escape would diverge from the Term the renderer reads.
    assert_eq!(
        inner.log,
        vec![
            "set_private_mode(Named(SyncUpdate))".to_string(),
            "unset_private_mode(Named(SyncUpdate))".to_string(),
        ],
        "the intercepted methods must ALSO forward"
    );
}
