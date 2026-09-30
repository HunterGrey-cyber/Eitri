//! THE DIFFERENTIAL. The only test that can catch an under-report nobody
//! thought of.
//!
//! After every published frame, two things are compared:
//!
//! * the screen a consumer assembled by applying one Full and then nothing but
//!   deltas, and
//! * a fresh Full projection of the same `Term`, taken by a second, independent
//!   projector.
//!
//! They must be cell-for-cell identical, cursor, modes and palette included. An
//! uncompensated damage under-report leaves a stale cell in the first and not
//! the second, so it shows up here whether or not anyone predicted it.
//!
//! Two corpora: a hand-written torture list of every construct known to break
//! damage, and a seeded pseudo-random stream over a 40-operation vocabulary.
//! The random corpus is the part that is not limited by the author's
//! imagination.
//!
//! NEGATIVE CONTROL. `compensation_is_load_bearing` runs the same corpora with
//! each compensation switched off and asserts the differential BREAKS. A
//! compensation whose absence nothing notices is not a compensation.
//!
//! Three of the six tests in this file (`seeded_random_streams_survive_the_delta_path`,
//! `seeded_random_streams_survive_at_the_canonical_geometry`, `compensation_is_load_bearing`) cost
//! about two minutes in a debug build between them and are `#[ignore]`d; run them with `--ignored`.
//! The other three (the torture-corpus tests and the corpus-size guard) cost well under a second
//! and run by default. `Screen` only ever calls `Projector::full`, so Eitri does not use the
//! delta path at all -- these tests protect `terminal-frame`'s own contract, not anything on the
//! product's critical path (engine review 2026-09-23, minor 4).

mod common;

use common::{run_differential, Harness, Rng};
use terminal_frame::project::Compensation;

// ---------------------------------------------------------------------------
// corpus 1: hand-written torture
// ---------------------------------------------------------------------------

/// One entry per construct that is known to confuse damage.
///
/// Each case carries its OWN geometry. That is not decoration: the
/// leading-wide-char-spacer case only exists when the glyph does not fit in the
/// remaining columns, and running it at a comfortable width silently turns it
/// into a test of nothing. An earlier draft did exactly that and reported the
/// previous-line compensation as unnecessary.
struct Case {
    name: &'static str,
    cols: usize,
    rows: usize,
    chunks: Vec<Vec<u8>>,
}

fn torture() -> Vec<Case> {
    fn c(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }
    let raw: Vec<(&'static str, usize, usize, Vec<Vec<u8>>)> = vec![
        (
            "plain text",
            20,
            6,
            c(&["hello world", " and more", "\r\n", "second line"]),
        ),
        (
            "combining mark on a narrow char",
            20,
            6,
            c(&["a", "\u{301}", "b", "\u{308}"]),
        ),
        (
            "combining mark on a wide char",
            20,
            6,
            c(&["\u{6f22}", "\u{301}", "\u{5b57}", "\u{308}"]),
        ),
        (
            "stacked combining marks",
            20,
            6,
            c(&["e", "\u{301}", "\u{308}", "\u{327}"]),
        ),
        (
            "leading wide char spacer at the right margin",
            10,
            6,
            c(&["123456789", "\u{6f22}", "\x1b[2;1Hx", "\x1b[1;10Hy"]),
        ),
        (
            "overwrite a wide char's first half",
            20,
            6,
            c(&["\u{6f22}\u{5b57}", "\x1b[1;1Hx"]),
        ),
        (
            "overwrite a wide char's spacer half",
            20,
            6,
            c(&["\u{6f22}\u{5b57}", "\x1b[1;2Hx"]),
        ),
        (
            "overwrite a wide char at the right margin",
            10,
            6,
            c(&["12345678", "\u{6f22}", "\x1b[1;10Hx"]),
        ),
        (
            "wide chars then erase in line",
            20,
            6,
            c(&["\u{6f22}\u{5b57}\u{6f22}", "\x1b[1;3H\x1b[K"]),
        ),
        (
            "insert mode",
            20,
            6,
            c(&["abcdef", "\x1b[4h", "\x1b[1;3HXY", "\x1b[4l", "\x1b[1;1HZ"]),
        ),
        (
            "insert blank / delete chars",
            20,
            6,
            c(&["abcdefghij", "\x1b[1;3H\x1b[3@", "\x1b[2P"]),
        ),
        ("erase chars", 20, 6, c(&["abcdefghij", "\x1b[1;4H\x1b[3X"])),
        (
            "scroll region",
            20,
            6,
            c(&["\x1b[2;4r", "\x1b[2;1H", "one\r\ntwo\r\nthree\r\nfour\r\nfive"]),
        ),
        ("reverse index", 20, 6, c(&["a\r\nb\r\nc", "\x1b[1;1H", "\x1bM\x1bM"])),
        (
            "alt screen",
            20,
            6,
            c(&["primary", "\x1b[?1049h", "alternate", "\x1b[?1049l"]),
        ),
        (
            "cursor save / restore",
            20,
            6,
            c(&["\x1b7abc", "\x1b[3;3Hxyz", "\x1b8Q"]),
        ),
        (
            "sgr attributes",
            40,
            6,
            c(&["\x1b[1;3;4;7mbold\x1b[0m plain", "\x1b[38;2;1;2;3mrgb\x1b[0m"]),
        ),
        (
            "underline colours",
            40,
            6,
            c(&["\x1b[4:3m\x1b[58;2;10;20;30mwavy\x1b[59m\x1b[0m done"]),
        ),
        (
            "indexed colours",
            40,
            6,
            c(&["\x1b[38;5;196mred\x1b[48;5;21mblue\x1b[0m"]),
        ),
        (
            "osc 4 palette override",
            20,
            6,
            c(&["\x1b]4;1;#ff0000\x07text", "\x1b]104;1\x07more"]),
        ),
        (
            "osc 10/11 fg and bg",
            20,
            6,
            c(&["\x1b]10;#112233\x07\x1b]11;#445566\x07x", "\x1b]110\x07y"]),
        ),
        (
            "osc 12 cursor colour",
            20,
            6,
            c(&["\x1b]12;#00ff00\x07x", "\x1b]112\x07y"]),
        ),
        (
            "cursor shape and visibility",
            20,
            6,
            c(&["\x1b[?25l", "\x1b[4 q", "\x1b[?25h", "\x1b[2 q"]),
        ),
        ("decaln", 20, 6, c(&["\x1b#8", "\x1b[1;1Hx"])),
        ("tabs", 20, 6, c(&["\tA\tB\tC", "\r\n\x1b[3g\tD"])),
        (
            "line wrap off and on",
            20,
            6,
            c(&["\x1b[?7l", &"x".repeat(30), "\x1b[?7h", &"y".repeat(30)]),
        ),
        (
            "clear screen variants",
            20,
            6,
            c(&["abc\r\ndef\r\nghi", "\x1b[2;2H\x1b[1J", "\x1b[0J"]),
        ),
        ("newline flood", 20, 6, c(&["\r\n".repeat(12).as_str(), "bottom"])),
        ("backspace and overwrite", 20, 6, c(&["abcdef", "\x08\x08\x08XYZ"])),
        (
            "origin mode",
            20,
            6,
            c(&["\x1b[2;4r\x1b[?6h", "\x1b[1;1Hin-region", "\x1b[?6l"]),
        ),
        // The four escapes the shrinker produced for under-report 6, kept as a
        // named regression rather than left to the random corpus to rediscover.
        (
            "wrap with a dead linefeed",
            24,
            8,
            c(&["\x1b[4;5r\x1b[7;21H\x1b[3Btab\there"]),
        ),
        // A combining mark on a wide char, then the cursor LEAVES that line in
        // the same read. cursor_line_in_full cannot reach it; only the
        // horizontal widening can.
        (
            "combining mark then the cursor leaves the line",
            20,
            6,
            c(&["\x1b[1;1H\u{6f22}\u{301}\x1b[3;1H"]),
        ),
        (
            "combining mark on a narrow char then the cursor leaves",
            20,
            6,
            c(&["\x1b[1;1Ha\u{301}\x1b[3;1H"]),
        ),
        (
            "spacer-half overwrite then the cursor leaves",
            20,
            6,
            c(&["\u{6f22}", "\x1b[1;2Hx\x1b[4;1H"]),
        ),
    ];
    raw.into_iter()
        .map(|(name, cols, rows, chunks)| Case {
            name,
            cols,
            rows,
            chunks,
        })
        .collect()
}

/// Every torture chunk, for the one case that runs them all against a single
/// canonical-geometry terminal.
fn torture_chunks() -> Vec<Vec<u8>> {
    torture().into_iter().flat_map(|case| case.chunks).collect()
}

// Fast (0.01s): not part of the ~2 minute debug cost the sibling seeded-random/compensation
// tests below carry, so this one runs by default (engine review 2026-09-23, minor 4).
#[test]
fn every_torture_case_survives_the_delta_path() {
    for case in torture() {
        let mut harness = Harness::new(case.cols, case.rows);
        if let Some(divergence) = run_differential(&mut harness, &case.chunks, Compensation::default()) {
            panic!(
                "{}: diverged at step {}: {}",
                case.name, divergence.step, divergence.detail
            );
        }
    }
}

// Fast (0.11s): runs by default (engine review 2026-09-23, minor 4).
#[test]
fn the_whole_torture_corpus_survives_at_the_canonical_geometry() {
    let mut harness = Harness::canonical();
    let chunks = torture_chunks();
    assert_eq!(chunks.len(), 89, "corpus shrank unexpectedly");
    if let Some(divergence) = run_differential(&mut harness, &chunks, Compensation::default()) {
        panic!("diverged at step {}: {}", divergence.step, divergence.detail);
    }
}

// ---------------------------------------------------------------------------
// corpus 2: seeded random
// ---------------------------------------------------------------------------

/// The operation vocabulary. Deliberately heavy on the things that move the
/// cursor without writing and write without moving the cursor, because that is
/// where damage and reality come apart.
fn random_chunk(rng: &mut Rng, cols: usize, rows: usize) -> Vec<u8> {
    const TEXT: &[&str] = &[
        "a",
        "ab",
        "hello",
        "x",
        " ",
        "\u{6f22}",
        "\u{5b57}\u{6f22}",
        "e\u{301}",
        "\u{6f22}\u{301}",
        "\u{1f600}",
        "tab\there",
        "\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}",
    ];
    let row = 1 + rng.below(rows);
    let col = 1 + rng.below(cols);
    let n = 1 + rng.below(8);
    match rng.below(24) {
        0..=5 => rng.pick(TEXT).as_bytes().to_vec(),
        6 => format!("\x1b[{row};{col}H").into_bytes(),
        7 => format!("\x1b[{n}C").into_bytes(),
        8 => format!("\x1b[{n}D").into_bytes(),
        9 => format!("\x1b[{n}A").into_bytes(),
        10 => format!("\x1b[{n}B").into_bytes(),
        11 => format!("\x1b[{}K", rng.below(3)).into_bytes(),
        12 => format!("\x1b[{}J", rng.below(3)).into_bytes(),
        13 => format!("\x1b[{n}@").into_bytes(),
        14 => format!("\x1b[{n}P").into_bytes(),
        15 => format!("\x1b[{n}X").into_bytes(),
        16 => format!("\x1b[{n}L").into_bytes(),
        17 => format!("\x1b[{n}M").into_bytes(),
        18 => b"\r\n".to_vec(),
        19 => format!("\x1b[{};{}m", rng.below(8) + 30, rng.below(8) + 40).into_bytes(),
        20 => format!("\x1b[{}m", rng.pick(&[0, 1, 2, 3, 4, 7, 9, 21, 53])).into_bytes(),
        21 => {
            if rng.below(2) == 0 {
                b"\x1b[4h".to_vec()
            } else {
                b"\x1b[4l".to_vec()
            }
        }
        22 => format!("\x1b]4;{};#{:06x}\x07", rng.below(16), rng.next_u64() & 0xff_ffff).into_bytes(),
        _ => format!(
            "\x1b[{};{}r",
            1 + rng.below(rows / 2),
            rows / 2 + 1 + rng.below(rows / 2)
        )
        .into_bytes(),
    }
}

/// A corpus of READS, not of operations.
///
/// Each chunk concatenates 1..=6 operations, because that is what a PTY read
/// actually contains and because several under-reports are only reachable when
/// a write and a cursor move land in the SAME frame -- publish a frame between
/// them and the cursor damage covers the write for free. A one-operation-per-
/// chunk corpus reports the horizontal widening as unnecessary.
fn random_corpus(seed: u64, steps: usize, cols: usize, rows: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    (0..steps)
        .map(|_| {
            let ops = 1 + rng.below(6);
            let mut chunk = Vec::new();
            for _ in 0..ops {
                chunk.extend_from_slice(&random_chunk(&mut rng, cols, rows));
            }
            chunk
        })
        .collect()
}

#[ignore = "~2 min in debug; run with --ignored"]
#[test]
fn seeded_random_streams_survive_the_delta_path() {
    let (cols, rows) = (24, 8);
    for seed in 1..=256u64 {
        let mut harness = Harness::new(cols, rows);
        let chunks = random_corpus(seed, 600, cols, rows);
        if let Some(divergence) = run_differential(&mut harness, &chunks, Compensation::default()) {
            panic!(
                "seed {seed} diverged at step {}: {}\nchunk was {:?}",
                divergence.step, divergence.detail, chunks[divergence.step]
            );
        }
    }
}

#[ignore = "~2 min in debug; run with --ignored"]
#[test]
fn seeded_random_streams_survive_at_the_canonical_geometry() {
    let (cols, rows) = (120, 40);
    for seed in 1001..=1064u64 {
        let mut harness = Harness::new(cols, rows);
        let chunks = random_corpus(seed, 400, cols, rows);
        if let Some(divergence) = run_differential(&mut harness, &chunks, Compensation::default()) {
            panic!(
                "seed {seed} diverged at step {}: {}\nchunk was {:?}",
                divergence.step, divergence.detail, chunks[divergence.step]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// THE NEGATIVE CONTROL
// ---------------------------------------------------------------------------

/// Run both corpora under `compensation` and report every divergence.
fn divergences(compensation: Compensation) -> Vec<String> {
    let mut out = Vec::new();
    for case in torture() {
        let mut harness = Harness::new(case.cols, case.rows);
        if let Some(divergence) = run_differential(&mut harness, &case.chunks, compensation) {
            out.push(format!(
                "torture/{}@{}: {}",
                case.name, divergence.step, divergence.detail
            ));
        }
    }
    for seed in 1..=256u64 {
        let mut harness = Harness::new(24, 8);
        let chunks = random_corpus(seed, 600, 24, 8);
        if let Some(divergence) = run_differential(&mut harness, &chunks, compensation) {
            out.push(format!("random/{seed}@{}: {}", divergence.step, divergence.detail));
        }
    }
    out
}

const POLICIES: &[(&str, Compensation)] = &[
    ("production: FullLine + prev + fold", Compensation::DEFAULT),
    (
        "FullLine, no previous-line rule",
        Compensation {
            previous_line_last_column: false,
            ..Compensation::DEFAULT
        },
    ),
    ("Widened{2,1} + prev + fold", Compensation::WIDEN_TWO),
    (
        "Widened{1,1} + prev + fold  (the brief's rule)",
        Compensation::WIDEN_ONE,
    ),
    ("nothing at all", Compensation::NONE),
];

/// THE MEASUREMENT that chose `SpanPolicy::FullLine`, with exact counts so a
/// mutation of any knob changes a number this test reads.
#[ignore = "~2 min in debug; run with --ignored"]
#[test]
fn compensation_is_load_bearing() {
    let mut counts = Vec::new();
    for (name, compensation) in POLICIES {
        let broken = divergences(*compensation);
        eprintln!("{:<46} {:>3} corpora diverge", name, broken.len());
        if let Some(first) = broken.first() {
            eprintln!("{:<46}     first: {first}", "");
        }
        counts.push((*name, broken.len()));
    }

    let count = |name: &str| counts.iter().find(|(n, _)| *n == name).unwrap().1;

    assert_eq!(
        count("production: FullLine + prev + fold"),
        0,
        "the production policy must reproduce the terminal exactly on every corpus"
    );
    assert_eq!(
        count("nothing at all"),
        219,
        "raw, uncompensated damage must break -- if it stopped breaking, either the corpora \
         stopped exercising the under-reports or upstream started reporting them, and either \
         way this crate is lying about why the compensations exist"
    );
    assert_eq!(
        count("FullLine, no previous-line rule"),
        21,
        "the previous-line-last-column rule must still be reachable; it is the one under-report \
         no span policy can cover, because that cell sits on a line with no damage at all"
    );
    assert_eq!(
        count("Widened{1,1} + prev + fold  (the brief's rule)"),
        125,
        "widening by one column each side must still be insufficient"
    );
    assert_eq!(
        count("Widened{2,1} + prev + fold"),
        124,
        "widening by two on the left must still be insufficient -- that is the whole argument \
         for SpanPolicy::FullLine"
    );
}

/// The corpora are the argument. If they silently shrink, every divergence
/// count above becomes a smaller claim than it reads as.
// Fast (0.00s): runs by default (engine review 2026-09-23, minor 4).
#[test]
fn the_corpora_are_the_size_the_readme_says_they_are() {
    let cases = torture();
    assert_eq!(cases.len(), 34, "the torture corpus changed size");

    // Distinct geometries, because several cases only exist at a width where
    // the construct under test is reachable at all.
    let widths: std::collections::BTreeSet<usize> = cases.iter().map(|case| case.cols).collect();
    assert!(
        widths.len() >= 3,
        "every torture case runs at the same width: {widths:?}"
    );

    let chunks: usize = cases.iter().map(|case| case.chunks.len()).sum();
    assert_eq!(chunks, 89, "the torture corpus changed size");

    // The random corpus: 256 seeds x 600 reads, each read 1..=6 operations.
    let corpus = random_corpus(1, 600, 24, 8);
    assert_eq!(corpus.len(), 600);
    let bytes: usize = corpus.iter().map(|chunk| chunk.len()).sum();
    assert!(bytes > 4_000, "the random corpus is only {bytes} bytes per seed");
    let multi = corpus.iter().filter(|chunk| chunk.len() > 8).count();
    assert!(
        multi > 200,
        "only {multi} of 600 reads carry more than one operation; a one-operation-per-read \
         corpus cannot reach the under-reports that need a write and a cursor move in the SAME \
         frame"
    );

    // 34 torture + 256 seeds is what every count in `compensation_is_load_bearing`
    // is out of.
    assert_eq!(cases.len() + 256, 290);
}
