//! Publication-barrier semantics on synthetic byte streams.

mod common;

use common::{new_term, snapshot, Splitter, BSU, ESU};
use terminal_sync::{PublishReason, SyncDriver};

/// Run `reads` through a driver over a real `Term`, collecting every publication.
fn run(reads: &[&[u8]]) -> (Vec<(PublishReason, String)>, String, usize) {
    let mut term = new_term();
    let mut driver = SyncDriver::new();
    let mut published = Vec::new();
    for read in reads {
        driver.feed(&mut term, read, |t, why| published.push((why, snapshot(t))));
    }
    let sync_bytes = driver.sync_bytes_count();
    let final_state = snapshot(&term);
    (published, final_state, sync_bytes)
}

/// `ESC[H` home, then text: each write overwrites the previous one, so every intermediate state
/// is observably different from the final one.
fn frame(texts: &[&str]) -> Vec<u8> {
    let mut v = BSU.to_vec();
    for t in texts {
        v.extend_from_slice(b"\x1b[H");
        v.extend_from_slice(t.as_bytes());
    }
    v.extend_from_slice(ESU);
    v
}

// -------------------------------------------------------------------------------------------
// Baseline: no synchronized updates at all.
// -------------------------------------------------------------------------------------------

#[test]
fn a_plain_read_publishes_exactly_once_at_the_wakeup() {
    let (published, final_state, _) = run(&[b"hello"]);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].0, PublishReason::Wakeup);
    assert_eq!(published[0].1, final_state);
    assert_eq!(final_state, "hello");
}

#[test]
fn a_read_that_dispatches_nothing_publishes_nothing() {
    // A lone ESC is a partial escape: the parser consumes it and dispatches no Handler method.
    let (published, _, _) = run(&[b"\x1b"]);
    assert!(
        published.is_empty(),
        "publication must be gated on dispatch, not on bytes arriving"
    );
}

// -------------------------------------------------------------------------------------------
// PROOF 2: a burst of intermediate mutations publishes EXACTLY ONE snapshot = final state.
// -------------------------------------------------------------------------------------------

#[test]
fn burst_between_bsu_and_esu_publishes_exactly_one_final_snapshot() {
    let f = frame(&["one       ", "two       ", "three     "]);
    let (published, final_state, sync_bytes) = run(&[&f]);

    assert_eq!(
        published.len(),
        1,
        "expected one consolidated snapshot, got {published:?}"
    );
    assert_eq!(published[0].0, PublishReason::FrameComplete);
    assert_eq!(published[0].1, final_state);
    assert_eq!(final_state, "three");
    assert!(!published[0].1.contains("one"), "an intermediate frame leaked");
    assert!(!published[0].1.contains("two"), "an intermediate frame leaked");
    assert_eq!(sync_bytes, 0, "NeverBuffer must never populate vte's raw sync buffer");
}

#[test]
fn same_burst_split_one_byte_per_read_still_publishes_exactly_once() {
    // Each byte is its OWN read, i.e. its own wakeup. This is the shape that broke the
    // prototype: gating publication on chunk arrival published a snapshot per read.
    let f = frame(&["one       ", "two       ", "three     "]);
    let reads: Vec<&[u8]> = f.chunks(1).collect();
    let (published, final_state, _) = run(&reads);

    assert_eq!(
        published.len(),
        1,
        "expected one consolidated snapshot, got {published:?}"
    );
    assert_eq!(published[0].0, PublishReason::FrameComplete);
    assert_eq!(published[0].1, final_state);
    assert_eq!(final_state, "three");
}

/// THE REGRESSION. An 8-byte BSU arriving one byte per read produced SEVEN spurious snapshots
/// when publication was gated on "bytes arrived". Not one Handler method is dispatched by the
/// first seven bytes, so a dispatch-gated barrier publishes nothing at all.
#[test]
fn an_eight_byte_bsu_delivered_byte_at_a_time_publishes_nothing() {
    let reads: Vec<&[u8]> = BSU.chunks(1).collect();
    assert_eq!(reads.len(), 8);
    let (published, _, _) = run(&reads);
    assert!(published.is_empty(), "expected 0 publications, got {published:?}");
}

#[test]
fn nondeterministic_chunking_is_equivalent_to_one_big_read() {
    let f = frame(&["alpha     ", "beta      ", "gamma     "]);
    let (baseline, baseline_final, _) = run(&[&f]);
    assert_eq!(baseline.len(), 1);

    for seed in 1..=64u64 {
        for max in [1usize, 2, 3, 5, 8, 13, 64] {
            let mut splitter = Splitter::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let reads = splitter.split(&f, max);
            let (published, final_state, sync_bytes) = run(&reads);
            assert_eq!(
                published.len(),
                1,
                "seed {seed} max {max}: chunking changed the publication count: {published:?}"
            );
            assert_eq!(published[0].1, baseline_final, "seed {seed} max {max}");
            assert_eq!(final_state, baseline_final, "seed {seed} max {max}");
            assert_eq!(sync_bytes, 0, "seed {seed} max {max}");
        }
    }
}

// -------------------------------------------------------------------------------------------
// PROOF 4: ESU(n) followed by BSU(n+1) inside ONE read must still publish frame n.
// -------------------------------------------------------------------------------------------

#[test]
fn esu_then_bsu_in_the_same_read_still_publishes_frame_n() {
    // Read 1 opens frame n. Read 2 carries the tail of frame n, its ESU, the BSU of frame n+1
    // and the beginning of frame n+1 -- all in one chunk. The prototype closed and reopened the
    // barrier inside a single wakeup, so the end-of-read decision saw in_sync() == true and
    // frame n was NEVER PUBLISHED.
    let mut read1 = BSU.to_vec();
    read1.extend_from_slice(b"\x1b[Hframe-n-partial\x1b[K");

    let mut read2 = b"\x1b[Hframe-n-final\x1b[K".to_vec();
    read2.extend_from_slice(ESU);
    read2.extend_from_slice(BSU);
    read2.extend_from_slice(b"\x1b[Hframe-n+1-part\x1b[K");

    let mut read3 = b"\x1b[Hframe-n+1-final\x1b[K".to_vec();
    read3.extend_from_slice(ESU);

    let (published, final_state, _) = run(&[&read1, &read2, &read3]);

    assert_eq!(
        published.len(),
        2,
        "frame n was dropped -- expected one publication per completed frame, got {published:?}"
    );
    assert_eq!(published[0].0, PublishReason::FrameComplete);
    assert_eq!(published[0].1, "frame-n-final", "frame n published torn or dropped");
    assert_eq!(published[1].0, PublishReason::FrameComplete);
    assert_eq!(published[1].1, "frame-n+1-final");
    assert_eq!(final_state, "frame-n+1-final");
}

#[test]
fn esu_then_bsu_publication_is_not_torn_by_the_next_frames_bytes() {
    // Same read, but frame n+1's bytes that follow the BSU would make the snapshot WRONG if the
    // publication were deferred to the end of the read. The publication happens at the ESU byte.
    let mut read = BSU.to_vec();
    read.extend_from_slice(b"\x1b[HNNNN");
    read.extend_from_slice(ESU);
    read.extend_from_slice(BSU);
    read.extend_from_slice(b"\x1b[HXXXX");

    let (published, _, _) = run(&[&read]);
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(
        published[0].1, "NNNN",
        "publication landed after frame n+1 had already mutated Term"
    );
}

// -------------------------------------------------------------------------------------------
// The locked semantic model: Term is NOT frozen during an update.
// -------------------------------------------------------------------------------------------

#[test]
fn term_keeps_parsing_and_mutating_inside_the_update() {
    let mut term = new_term();
    let mut driver = SyncDriver::new();
    let mut published = Vec::new();

    driver.feed(&mut term, BSU, |t, why| published.push((why, snapshot(t))));
    driver.feed(&mut term, b"\x1b[Hpartial", |t, why| published.push((why, snapshot(t))));

    assert!(published.is_empty(), "no intermediate snapshot may be published");
    assert!(driver.barrier().in_sync());
    assert_eq!(
        driver.sync_bytes_count(),
        0,
        "no raw PTY bytes may be buffered outside Term"
    );
    assert_eq!(
        snapshot(&term),
        "partial",
        "Term must keep parsing and mutating during a synchronized update"
    );

    driver.feed(&mut term, ESU, |t, why| published.push((why, snapshot(t))));
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].1, "partial");
}

#[test]
fn an_empty_synchronized_update_publishes_nothing() {
    let mut bytes = BSU.to_vec();
    bytes.extend_from_slice(ESU);
    let (published, _, _) = run(&[&bytes]);
    assert!(
        published.is_empty(),
        "BSU/ESU with no mutation must not publish: {published:?}"
    );
}

#[test]
fn nested_bsu_is_idempotent_and_the_first_esu_closes_the_frame() {
    let mut bytes = BSU.to_vec();
    bytes.extend_from_slice(BSU);
    bytes.extend_from_slice(b"\x1b[Hnested");
    bytes.extend_from_slice(ESU);
    let (published, _, _) = run(&[&bytes]);
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(published[0].1, "nested");
}

#[test]
fn a_stray_esu_without_bsu_does_not_invent_a_frame() {
    let mut bytes = b"\x1b[Hplain".to_vec();
    bytes.extend_from_slice(ESU);
    let (published, _, _) = run(&[&bytes]);
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(
        published[0].0,
        PublishReason::Wakeup,
        "a stray ESU is not a frame boundary"
    );
}

// -------------------------------------------------------------------------------------------
// Unterminated BSU: documented policy, not an invented timeout.
// -------------------------------------------------------------------------------------------

#[test]
fn an_unterminated_bsu_suspends_publication_but_never_freezes_term() {
    let mut term = new_term();
    let mut driver = SyncDriver::new();
    let mut published = Vec::new();

    driver.feed(&mut term, BSU, |t, why| published.push((why, snapshot(t))));
    for i in 0..50 {
        let bytes = format!("\x1b[Hstill-alive-{i:02}");
        driver.feed(&mut term, bytes.as_bytes(), |t, why| published.push((why, snapshot(t))));
    }

    assert!(
        published.is_empty(),
        "publication stays suspended -- there is no default deadline"
    );
    assert_eq!(
        driver.sync_bytes_count(),
        0,
        "and nothing accumulates in a hidden buffer"
    );
    assert_eq!(
        snapshot(&term),
        "still-alive-49",
        "Term is not frozen, only publication is"
    );

    // The engine's event loop -- not the parser -- decides if and when to give up.
    driver.abort_sync(&mut term, |t, why| published.push((why, snapshot(t))));
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].0, PublishReason::SyncAborted);
    assert_eq!(published[0].1, "still-alive-49");
    assert!(!driver.barrier().in_sync());
}
