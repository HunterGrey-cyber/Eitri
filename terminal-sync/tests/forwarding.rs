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

use terminal_sync::{SyncBarrier, SyncSpy};

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
