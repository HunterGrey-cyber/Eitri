//! P4-A2 (2026-09-27 Codex audit, verdicts P4): narrowing a pane while a scrollback anchor is
//! pinned into saturated history can silently substitute a different retained line for the
//! evicted one, with no expiry notice -- `Screen::resize` resized `Term` but never called
//! `ScrollView::observe`, so the anchor's `oldest_ordinal` alignment was left stale until the next
//! `feed()`. `RawViewport::resolve`'s own doc comment already named this exact risk but only
//! guarded the crash case (`index` out of the *current* bounds), not the wrong-content case (a
//! stale `index` that still happens to be in range after a resize reflows the grid).
//!
//! Adapted from Codex's saved probe (`/scratch/v1-audit-probes/neovibe-p4-audit/terminal_resize.rs`,
//! written against `8b13773`) -- unchanged in substance, just placed under this crate's own
//! `tests/` the way its other integration tests are.

use neovibe_terminal::{PtySize, Screen, ScrollRequest, TerminalColors};
use terminal_render::PaintOp;

#[test]
fn narrowing_saturated_history_never_silently_substitutes_anchor() {
    let size = PtySize {
        cols: 40,
        rows: 5,
        cell_width_px: 9,
        cell_height_px: 18,
    };
    let mut screen = Screen::new(size, TerminalColors::default());
    // Enough lines to overrun the default 10,000-line scrollback (module doc, `screen.rs`) and
    // evict the line the anchor below pins to.
    let text: String = (0..10020)
        .map(|i| format!("L{i:05} 123456789012345678901234567890\r\n"))
        .collect();
    screen.feed(text.as_bytes());
    screen.scroll(ScrollRequest::Lines(9000));
    let before = screen.render(true).row_text(0);

    screen.resize(PtySize { cols: 20, ..size });
    let after = screen.render(true);
    let notice_after_resize = after
        .ops
        .iter()
        .any(|op| matches!(op, PaintOp::DrawNotice { text, .. } if text.contains("no longer available")));

    // Control: an empty `feed()` does call `observe` (module doc, `Screen::feed`'s own comment),
    // so it proves the anchor really was evicted by the resize, independent of whether `resize`
    // itself reported that eviction.
    screen.feed(b"");
    let observed = screen.render(true);
    let notice_after_observe = observed
        .ops
        .iter()
        .any(|op| matches!(op, PaintOp::DrawNotice { text, .. } if text.contains("no longer available")));

    println!(
        "before={before:?}; after={:?}; notice_after_resize={notice_after_resize}",
        after.row_text(0)
    );
    assert!(
        notice_after_observe,
        "precondition: narrowing actually evicted the anchor"
    );
    assert!(
        notice_after_resize,
        "resize must report the eviction itself, not substitute another retained line"
    );
}
