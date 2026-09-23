//! Sections 12 and 13: the pinned historical view, rendered.
//!
//! This is the test that stops the renderer layer from quietly reintroducing a
//! scroll offset. `RawViewport` proves anchor identity at the frame layer; this
//! proves the PAINTED PIXELS follow it, which is a different claim -- a renderer
//! can hold a correct anchor and still paint from a stale row number.

mod common;

use alacritty_terminal::grid::Dimensions as _;
use common::Screen;
use terminal_frame::viewport::{AnchorResolution, RawViewport};
use terminal_render::ViewMode;

/// Paint whatever the viewport currently says is the top of the view.
///
/// The ONLY selection surface: `top_line()`. There is no offset anywhere in this
/// function, which is the structural half of section 12's guarantee.
fn paint_pinned(s: &Screen, v: &RawViewport, rows: usize) -> (Option<i32>, String) {
    match v.top_line(&s.term) {
        Some(top) => {
            let list = s.paint_window(top, rows, ViewMode::Pinned, &[]);
            (Some(top), list.row_text(0))
        }
        None => {
            let list = s.paint_window(0, rows, ViewMode::AnchorExpired, &[]);
            assert!(list.has_expiry_notice(), "an expired anchor must paint its notice");
            (None, String::new())
        }
    }
}

#[test]
fn a_pinned_view_keeps_painting_the_same_content_while_output_continues() {
    // Section 12, promoted from the viewport's own 170-append test into the
    // renderer. Required: the painted content stays put, the physical top line
    // moves, and the live terminal keeps advancing underneath.
    let mut s = Screen::with_history(20, 6, 1000);
    for i in 1..=30 {
        s.feed(&format!("L{i:04}\r\n"));
    }

    let mut v = RawViewport::new();
    let spans = terminal_frame::viewport::logical_spans(&s.term);
    v.pin(&s.term, spans[10].start);

    let (first_line, first_text) = paint_pinned(&s, &v, 6);
    assert_eq!(first_text, "L0011", "corpus sanity: the pinned row paints L0011");
    let first_line = first_line.expect("anchored");

    // The live screen before the run, so we can prove it really advanced. Taken
    // as the whole window rather than one row: the bottom row is the cursor's
    // own line and is blank after every "\r\n", so comparing it alone compares
    // "" with "" forever and proves nothing.
    let live_screen = |s: &Screen| (0..6u16).map(|r| s.paint().row_text(r)).collect::<Vec<_>>();
    let live_before = live_screen(&s);

    for i in 31..=200 {
        s.feed(&format!("L{i:04}\r\n"));
        v.observe(&s.term);
        let (line, text) = paint_pinned(&s, &v, 6);
        assert_eq!(
            text, "L0011",
            "append {i}: the pinned view painted different content -- a scroll offset has crept back in"
        );
        assert!(line.is_some(), "append {i}: the anchor must still resolve");
    }

    let (last_line, _) = paint_pinned(&s, &v, 6);
    let last_line = last_line.expect("still anchored");

    // (1) the physical top line moved a long way...
    assert_ne!(
        last_line, first_line,
        "the anchor's absolute grid line must move; if it did not, this test would pass on an \
         offset-based viewport and prove nothing"
    );
    assert_eq!(last_line, first_line - 170, "by exactly the number of lines appended");

    // (2) ...and the live terminal advanced underneath the whole time.
    let live_after = live_screen(&s);
    assert_ne!(
        live_before, live_after,
        "the live screen must have advanced underneath the pin"
    );
    assert!(
        live_after.iter().any(|r| r.contains("L0200")),
        "and it must have advanced all the way to the newest output: {live_after:?}"
    );
}

#[test]
fn the_painted_window_follows_the_anchor_across_a_reflow() {
    // Same claim, but the thing that moves is the fold rather than the history:
    // the absolute line changes because every logical line above was re-folded.
    let mut s = Screen::with_history(40, 6, 1000);
    for i in 1..=30 {
        s.feed(&format!("L{i:04}{}\r\n", "z".repeat(50)));
    }
    let mut v = RawViewport::new();
    let spans = terminal_frame::viewport::logical_spans(&s.term);
    v.pin(&s.term, spans[10].start);
    let (line_before, text_before) = paint_pinned(&s, &v, 6);

    s.resize(20, 6);
    v.observe(&s.term);
    let (line_after, text_after) = paint_pinned(&s, &v, 6);

    assert_ne!(line_before, line_after, "reflow must move the absolute line");
    assert!(
        text_after.starts_with(&text_before[..5]),
        "but the painted row must still begin with the same logical content \
         (before={text_before:?} after={text_after:?})"
    );
}

#[test]
fn an_expired_anchor_paints_an_explicit_notice_and_never_the_content_at_the_old_line() {
    // Section 13. The forbidden behaviour is rendering whatever now occupies the
    // former position while still claiming to be pinned. Both halves are
    // asserted: the notice appears, AND the old line's current content is not
    // silently presented as the pinned view.
    let mut s = Screen::with_history(20, 6, 30);
    for i in 1..=20 {
        s.feed(&format!("L{i:04}\r\n"));
    }
    let mut v = RawViewport::new();
    let spans = terminal_frame::viewport::logical_spans(&s.term);
    v.pin(&s.term, spans[0].start);
    let (pinned_line, pinned_text) = paint_pinned(&s, &v, 6);
    let pinned_line = pinned_line.expect("anchored");
    assert!(!pinned_text.is_empty());

    // Push the anchored content out of history entirely.
    for i in 21..=200 {
        s.feed(&format!("L{i:04}\r\n"));
        v.observe(&s.term);
    }
    assert_eq!(
        v.resolve(&s.term),
        AnchorResolution::Gone,
        "precondition: the anchor is evicted"
    );

    // The renderer must now be in the explicit expired state.
    let list = s.paint_window(0, 6, ViewMode::AnchorExpired, &[]);
    assert!(list.has_expiry_notice(), "the transition must be visible, not hidden");

    // And whatever sits at the old absolute line today is NOT what a pinned view
    // would have been showing -- which is exactly what an offset model would
    // have painted without noticing.
    let whatever_is_there_now = s
        .paint_window(
            pinned_line.max(-(s.term.grid().history_size() as i32)),
            1,
            ViewMode::FollowBottom,
            &[],
        )
        .row_text(0);
    assert_ne!(
        whatever_is_there_now, pinned_text,
        "the old line now holds different content; presenting it as the pin is the section 13 bug"
    );
}

#[test]
fn a_follow_bottom_view_never_paints_an_expiry_notice() {
    // The negative control for section 13: the notice must be tied to the
    // expired state and not emitted unconditionally, or the assertion above
    // would pass on a renderer that always draws it.
    let mut s = Screen::new(20, 4);
    s.feed("hello");
    assert!(!s.paint().has_expiry_notice());
}
