//! `RawViewport` -- the ordinal anchor, exercised through the type the renderer
//! will actually hold.
//!
//! `tests/resize_probe.rs` is the GATE: it measured, on a real `Term`, that
//! logical-line identity survives reflow while absolute lines and
//! indexes-from-top do not, and it ended in Category A (exact anchor
//! preservation, anchor = monotonic logical-line ordinal). This file is the
//! other half -- the implementation of that verdict, held to the same numbers.
//!
//! Where the gate resolved an anchor by SEARCHING for its text, `RawViewport`
//! resolves it arithmetically (`ordinal - oldest_ordinal`) and keeps the
//! alignment up to date incrementally. Searching is not a mechanism a product
//! can use: text is not unique, and a search is what lets a viewport land on a
//! different line that happens to read the same. So these tests assert the
//! arithmetic lands on the same content the gate's search did.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use terminal_frame::viewport::{logical_spans, AnchorResolution, RawViewport};

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

fn fill(t: &mut Term<VoidListener>, p: &mut Processor, from: usize, to: usize) {
    for i in from..=to {
        p.advance(t, format!("L{i:04}\r\n").as_bytes());
    }
}

fn resize(t: &mut Term<VoidListener>, lines: usize, cols: usize) {
    t.resize(Dims { lines, cols });
}

/// The joined text of the logical line at `index`, trailing blanks trimmed --
/// the same notion the gate compared on.
fn logical_text(t: &Term<VoidListener>, index: usize) -> String {
    let spans = logical_spans(t);
    let span = spans[index];
    let grid = t.grid();
    let mut s = String::new();
    for row in 0..span.rows {
        let l = Line(span.start + row as i32).grid_clamp(grid, Boundary::Grid);
        let r = &grid[l];
        for c in 0..grid.columns() {
            s.push(r[Column(c)].c);
        }
    }
    s.trim_end().to_owned()
}

/// What the viewport currently resolves to, as text. `None` == reported gone.
fn anchored_text(v: &RawViewport, t: &Term<VoidListener>) -> Option<String> {
    match v.resolve(t) {
        AnchorResolution::Visible { index } => Some(logical_text(t, index)),
        AnchorResolution::Gone => None,
    }
}

// ---------------------------------------------------------------------------

#[test]
fn an_anchor_survives_narrowing_and_widening_exactly() {
    // The gate's headline measurement: index 14 -> 14 -> 14, counts 41 -> 41 -> 41.
    // Here the same corpus is pinned through the real type instead of searched for.
    let mut t = term(10, 40, 1000);
    let mut p: Processor = Processor::new();
    fill(&mut t, &mut p, 1, 40);

    let spans = logical_spans(&t);
    let pinned_line = spans[14].start;
    let expected = logical_text(&t, 14);
    assert_eq!(expected, "L0015", "corpus sanity: index 14 is L0015");

    let mut v = RawViewport::new();
    v.pin(&t, pinned_line);
    assert_eq!(v.resolve(&t), AnchorResolution::Visible { index: 14 });

    // Narrow: every logical line re-folds across more rows. Absolute grid lines
    // stop identifying content here -- that is asserted in the gate.
    resize(&mut t, 10, 20);
    v.observe(&t);
    assert_eq!(
        anchored_text(&v, &t).as_deref(),
        Some("L0015"),
        "narrowing must not move the anchored content"
    );

    // ...and back out again.
    resize(&mut t, 10, 40);
    v.observe(&t);
    assert_eq!(
        anchored_text(&v, &t).as_deref(),
        Some("L0015"),
        "widening must not move the anchored content"
    );
    assert_eq!(v.resolve(&t), AnchorResolution::Visible { index: 14 });
}

#[test]
fn the_anchor_holds_content_while_the_index_from_top_shifts_under_saturation() {
    // THE test that forces an ordinal. The gate measured index 38 -> 4 with the
    // logical-line count going 70 -> 36 as narrowing evicted from a saturated
    // history. An index-based viewport shows the wrong line here; an offset-based
    // one shows the wrong line AND cannot tell.
    let mut t = term(10, 40, 60);
    let mut p: Processor = Processor::new();
    // Long lines, so narrowing genuinely multiplies the rows each one needs and
    // the saturated history is forced to give ground.
    for i in 1..=70 {
        p.advance(&mut t, format!("L{i:04}{}\r\n", "x".repeat(60)).as_bytes());
    }

    let spans = logical_spans(&t);
    let index_before = 38.min(spans.len() - 1);
    let pinned_line = spans[index_before].start;
    let expected = logical_text(&t, index_before);
    let count_before = spans.len();

    let mut v = RawViewport::new();
    v.pin(&t, pinned_line);

    resize(&mut t, 10, 20);
    v.observe(&t);

    let count_after = logical_spans(&t).len();
    let index_after = match v.resolve(&t) {
        AnchorResolution::Visible { index } => index,
        AnchorResolution::Gone => panic!("the anchored line was still retained; it must not be reported gone"),
    };

    println!(
        "ANCHOR saturated-narrow: index_before={index_before} index_after={index_after} \
         counts before={count_before} after={count_after}"
    );
    assert!(
        count_after < count_before,
        "precondition: narrowing a saturated history must evict, or this test proves nothing \
         (before={count_before} after={count_after})"
    );
    assert_ne!(
        index_after, index_before,
        "precondition: the index-from-top must actually shift, or an index-based viewport would \
         have passed too"
    );
    assert_eq!(
        anchored_text(&v, &t).as_deref(),
        Some(expected.as_str()),
        "the ordinal must still name the same content after the index shifted"
    );
}

#[test]
fn an_evicted_anchor_is_reported_gone_never_resolved_to_a_neighbour() {
    // The one bounded fallback. Output keeps coming until the pinned line is
    // pushed out of history; there is then no correct content, and the rule is to
    // say so rather than hand back whatever now occupies that position.
    let mut t = term(10, 40, 30);
    let mut p: Processor = Processor::new();
    fill(&mut t, &mut p, 1, 20);

    let spans = logical_spans(&t);
    let pinned_line = spans[0].start;
    let expected = logical_text(&t, 0);

    let mut v = RawViewport::new();
    v.pin(&t, pinned_line);
    assert_eq!(anchored_text(&v, &t).as_deref(), Some(expected.as_str()));

    // Push it out, observing along the way as a real wakeup loop would.
    for i in 21..=90 {
        p.advance(&mut t, format!("L{i:04}\r\n").as_bytes());
        v.observe(&t);
    }

    assert_eq!(
        v.resolve(&t),
        AnchorResolution::Gone,
        "an evicted anchor must be reported gone"
    );
    assert_eq!(anchored_text(&v, &t), None);
    // And the content genuinely is gone -- so this is not a false negative.
    assert!(
        !(0..logical_spans(&t).len()).any(|i| logical_text(&t, i) == expected),
        "precondition: the pinned line must really have been evicted"
    );
}

#[test]
fn a_history_that_turns_over_entirely_between_observations_loses_the_view_it_does_not_move_it() {
    // The alignment's failure mode, pinned deliberately. If everything retained
    // at the last observation is gone, there is no evidence of where the anchor
    // went. Failing to GONE is safe; silently re-aligning would put the user on
    // unrelated content while the view still claimed to be pinned.
    let mut t = term(10, 40, 30);
    let mut p: Processor = Processor::new();
    fill(&mut t, &mut p, 1, 20);

    let mut v = RawViewport::new();
    v.pin(&t, logical_spans(&t)[5].start);

    // No observe() inside the loop: one huge batch, far more than the history holds.
    fill(&mut t, &mut p, 21, 400);
    v.observe(&t);

    assert_eq!(
        v.resolve(&t),
        AnchorResolution::Gone,
        "an unalignable observation must lose the view, not re-anchor it somewhere plausible"
    );
}

#[test]
fn a_gone_anchor_stays_gone_even_if_matching_text_reappears() {
    // Fingerprints are content hashes, so identical text recurs constantly in a
    // real terminal (prompts, repeated build output). Once the anchor is gone,
    // re-finding its text is finding a DIFFERENT line -- resolving to it would be
    // exactly the silent substitution the gate forbids.
    let mut t = term(10, 40, 30);
    let mut p: Processor = Processor::new();
    p.advance(&mut t, b"UNIQUE-MARKER\r\n");
    fill(&mut t, &mut p, 1, 20);

    let mut v = RawViewport::new();
    let marker_index = (0..logical_spans(&t).len())
        .find(|&i| logical_text(&t, i) == "UNIQUE-MARKER")
        .expect("the marker must be present before eviction");
    v.pin(&t, logical_spans(&t)[marker_index].start);

    for i in 21..=90 {
        p.advance(&mut t, format!("L{i:04}\r\n").as_bytes());
        v.observe(&t);
    }
    assert_eq!(
        v.resolve(&t),
        AnchorResolution::Gone,
        "precondition: the marker was evicted"
    );

    // The very same text is printed again. It is a new line, and the anchor must
    // not latch onto it.
    p.advance(&mut t, b"UNIQUE-MARKER\r\n");
    v.observe(&t);
    assert_eq!(
        v.resolve(&t),
        AnchorResolution::Gone,
        "a gone anchor must not resurrect onto a later line with the same text"
    );
}

#[test]
fn observing_while_unpinned_is_a_no_op_so_following_the_bottom_costs_nothing() {
    // The cost argument for scoping ordinals to a pinning session: the common
    // case must not walk the history at all. Asserted behaviourally -- an
    // unpinned viewport has no anchor to report before or after observation.
    let mut t = term(10, 40, 1000);
    let mut p: Processor = Processor::new();
    fill(&mut t, &mut p, 1, 200);

    let mut v = RawViewport::new();
    assert!(!v.is_pinned());
    v.observe(&t);
    assert!(!v.is_pinned());
    assert_eq!(
        v.resolve(&t),
        AnchorResolution::Gone,
        "an unpinned viewport anchors nothing"
    );
    assert_eq!(v.top_line(&t), None);
}

#[test]
fn pinning_mid_way_through_a_wrapped_line_pins_the_whole_logical_line() {
    // A window top can land on a continuation row. That row has no ordinal of its
    // own -- only the logical line does -- so the pin must resolve to the line
    // that CONTAINS it, not to the next one starting at or after it.
    let mut t = term(10, 40, 1000);
    let mut p: Processor = Processor::new();
    p.advance(&mut t, b"short-before\r\n");
    p.advance(&mut t, format!("WRAPPED{}\r\n", "y".repeat(90)).as_bytes());
    p.advance(&mut t, b"short-after\r\n");

    let spans = logical_spans(&t);
    let wrapped = (0..spans.len())
        .find(|&i| logical_text(&t, i).starts_with("WRAPPED"))
        .expect("the wrapped line must be present");
    assert!(
        spans[wrapped].rows >= 3,
        "precondition: the line must actually wrap across rows"
    );

    // Pin using the SECOND row of that logical line -- a continuation.
    let mut v = RawViewport::new();
    v.pin(&t, spans[wrapped].start + 1);
    assert_eq!(
        v.resolve(&t),
        AnchorResolution::Visible { index: wrapped },
        "pinning a continuation row must anchor its logical line"
    );
    assert_eq!(
        v.top_line(&t),
        Some(spans[wrapped].start),
        "and the view starts at the line's first row"
    );
}

#[test]
fn top_line_follows_the_anchored_content_across_a_reflow() {
    // What the renderer actually consumes. The grid line the anchor sits on MUST
    // change under reflow -- if it did not, absolute lines would be stable and
    // this whole type would be unnecessary.
    let mut t = term(10, 40, 1000);
    let mut p: Processor = Processor::new();
    for i in 1..=40 {
        p.advance(&mut t, format!("L{i:04}{}\r\n", "z".repeat(50)).as_bytes());
    }

    let mut v = RawViewport::new();
    let spans = logical_spans(&t);
    v.pin(&t, spans[10].start);
    let expected = logical_text(&t, 10);
    let line_before = v.top_line(&t).expect("anchored");

    resize(&mut t, 10, 20);
    v.observe(&t);
    let line_after = v.top_line(&t).expect("still anchored");

    assert_ne!(
        line_before, line_after,
        "precondition: reflow must move the absolute grid line, or nothing is being tested"
    );
    assert_eq!(
        anchored_text(&v, &t).as_deref(),
        Some(expected.as_str()),
        "top_line must track the content, not the old coordinate"
    );
}

#[test]
fn the_oldest_retained_logical_line_can_be_a_fragment_whose_text_changes_under_reflow() {
    // The limitation that shaped `align`, pinned so it cannot be quietly
    // designed around later. When eviction cuts a wrapped logical line in half,
    // the surviving rows are still one WRAPLINE run, so they read as a logical
    // line -- but the text is only its TAIL, and where that tail starts depends
    // on the current width. Reflow changes it.
    //
    // This is unfixable from the public API, and not for want of looking:
    // whether the topmost retained row continues an evicted line is precisely
    // what eviction destroyed, and `Grid` keeps no marker for it. Hence the rule
    // that the first entry is never an alignment key.
    let mut t = term(10, 40, 60);
    let mut p: Processor = Processor::new();
    for i in 1..=70 {
        p.advance(&mut t, format!("L{i:04}{}\r\n", "x".repeat(60)).as_bytes());
    }

    let head_before = logical_text(&t, 0);
    // A whole line from deep enough in the buffer to survive the narrowing's own
    // eviction -- the contrast case. Near the top it would simply be evicted, and
    // the test would prove nothing about reflow.
    let whole_before = logical_text(&t, logical_spans(&t).len() - 5);
    resize(&mut t, 10, 20);
    let head_after = logical_text(&t, 0);

    assert_ne!(
        head_before, head_after,
        "the fragment's text must move under reflow -- if this ever stops being true, the first-entry \
         exclusion in align() is dead code and should be removed with evidence, not kept on faith \
         (before={head_before:?} after={head_after:?})"
    );
    // And the contrast that makes it a fragment problem rather than a reflow
    // problem: a WHOLE logical line's text is width-independent.
    let whole_still_present = (0..logical_spans(&t).len()).any(|i| logical_text(&t, i) == whole_before);
    assert!(
        whole_still_present,
        "a whole logical line must survive reflow verbatim; only the partially-evicted head does not"
    );
}

#[test]
fn history_destruction_loses_an_anchor_that_was_in_history() {
    // Plan section 9: when history is EVICTED or DESTROYED, the anchor must be
    // reported invalid honestly -- never silently replaced by whatever now sits
    // at that position. `ESC[3J` (clear_history) and `RIS` (full reset) are the
    // two destruction paths.
    for (name, seq) in [("ESC[3J", "\x1b[3J"), ("RIS", "\x1bc")] {
        let mut t = term(10, 40, 100);
        let mut p: Processor = Processor::new();
        fill(&mut t, &mut p, 1, 60);

        let mut v = RawViewport::new();
        let spans = logical_spans(&t);
        // Index 5 is deep in history, not on the screen -- destruction must take it.
        v.pin(&t, spans[5].start);
        assert!(
            matches!(v.resolve(&t), AnchorResolution::Visible { .. }),
            "{name}: pinned before"
        );

        p.advance(&mut t, seq.as_bytes());
        v.observe(&t);

        assert_eq!(
            v.resolve(&t),
            AnchorResolution::Gone,
            "{name} destroyed the history the anchor lived in; it must be reported gone"
        );
        assert_eq!(v.top_line(&t), None, "{name}: and there is no line to render");
    }
}

#[test]
fn clear_history_keeps_an_anchor_that_was_on_the_screen() {
    // The discriminating half, and the reason this is alignment rather than a
    // blanket "history changed, drop everything" signal. `ESC[3J` destroys
    // SCROLLBACK only: the screen is untouched, so an anchor living there is
    // still valid and dropping it would be a needless loss of the user's place.
    // A viewport that invalidated on the escape sequence alone would fail here.
    let mut t = term(10, 40, 100);
    let mut p: Processor = Processor::new();
    fill(&mut t, &mut p, 1, 60);

    let spans = logical_spans(&t);
    // Second row of the screen: comfortably inside the live area, so clear_history
    // cannot touch it.
    let on_screen = spans.len() - 8;
    let expected = logical_text(&t, on_screen);
    let mut v = RawViewport::new();
    v.pin(&t, spans[on_screen].start);

    p.advance(&mut t, b"\x1b[3J");
    v.observe(&t);

    assert_eq!(
        anchored_text(&v, &t).as_deref(),
        Some(expected.as_str()),
        "an on-screen anchor survives clear_history -- only scrollback was destroyed"
    );
}

#[test]
fn a_buffer_of_repeated_lines_still_aligns_on_the_true_eviction_count() {
    // Fingerprints are content hashes, so a corpus of identical lines is the
    // degenerate case: many positions hash the same. Alignment survives it
    // because the settled slice must match in FULL, so its LENGTH pins the
    // offset -- an accidental shorter match at a smaller `j` leaves the lengths
    // inconsistent and is rejected.
    let mut t = term(10, 40, 40);
    let mut p: Processor = Processor::new();
    for _ in 0..30 {
        p.advance(&mut t, b"SAME\r\n");
    }
    let spans = logical_spans(&t);
    let anchor_index = spans.len() - 6;
    let mut v = RawViewport::new();
    v.pin(&t, spans[anchor_index].start);

    // Enough distinguishable output to push past the 40-line history and force
    // real eviction, so the index-from-top genuinely moves.
    for i in 1..=30 {
        p.advance(&mut t, format!("UNIQUE{i}\r\n").as_bytes());
        v.observe(&t);
    }

    match v.resolve(&t) {
        AnchorResolution::Visible { index } => {
            assert_eq!(
                logical_text(&t, index),
                "SAME",
                "the anchor must still land on the repeated content it was pinned to"
            );
            assert!(
                index < anchor_index,
                "precondition: eviction must have shifted the index from top \
                 (anchor_index={anchor_index} index={index}) -- without a shift an index-based \
                 viewport would pass this too"
            );
        }
        AnchorResolution::Gone => {
            panic!("the anchored line was not evicted; it must not be reported gone")
        }
    }
}

#[test]
fn an_anchor_stays_put_while_output_arrives_below_it() {
    // Plan section 9, `follow_bottom == false`: new output must NOT make the
    // visible history jump. This is the whole point of a pinned view and it was
    // the case MISSING from this file -- every other output-while-pinned test
    // asserted `Gone`, which a broken alignment also produces, so all three
    // passed for the wrong reason while alignment was in fact failing on every
    // append. It fails if either exclusion in align() is removed.
    let mut t = term(10, 40, 500);
    let mut p: Processor = Processor::new();
    fill(&mut t, &mut p, 1, 30);

    let spans = logical_spans(&t);
    let anchor_index = 10;
    let expected = logical_text(&t, anchor_index);
    assert_eq!(expected, "L0011", "corpus sanity");

    let mut v = RawViewport::new();
    v.pin(&t, spans[anchor_index].start);
    let line_at_pin = v.top_line(&t).expect("pinned");

    // A long run of output, well past one screen, with history to spare so
    // nothing is evicted. The anchored content must not move at all.
    for i in 31..=200 {
        p.advance(&mut t, format!("L{i:04}\r\n").as_bytes());
        v.observe(&t);
        assert_eq!(
            anchored_text(&v, &t).as_deref(),
            Some(expected.as_str()),
            "output at line {i} moved the pinned view off its content"
        );
    }

    // Nothing was evicted, so the ORDINAL and its index are unchanged.
    assert_eq!(v.resolve(&t), AnchorResolution::Visible { index: anchor_index });

    // But the ABSOLUTE GRID LINE moved a long way -- measured -11 -> -181 across
    // these 170 appends. Grid lines are numbered relative to the live screen, so
    // every line of output pushes existing content one further into the negative.
    // That gap IS the drift: a viewport holding the offset it was given at pin
    // time would now be showing content 170 lines away from what the user parked
    // on, with nothing anywhere reporting an error. Asserting the movement keeps
    // the demonstration honest -- if grid lines ever stopped moving here, this
    // test would be proving nothing and should be deleted rather than trusted.
    let line_now = v.top_line(&t).expect("still anchored");
    assert_ne!(
        line_now, line_at_pin,
        "precondition: absolute grid lines must move under output"
    );
    assert_eq!(
        line_now,
        line_at_pin - 170,
        "and they move by exactly the lines appended"
    );
}
