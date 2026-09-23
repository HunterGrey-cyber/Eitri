//! GOLDEN DIGESTS.
//!
//! The differential sweep compares the production encoder against an independent
//! reference, but BOTH call the same vendored `build_sequence`. A change inside
//! `build_sequence` moves both sides equally and the differential stays green. So
//! `build_sequence` needs its own anchor, and that is this file.
//!
//! `tests/golden/encoder.digest` holds one FNV-1a-64 digest per `TermMode`
//! combination over the whole sweep. Any change to any produced byte, anywhere in
//! the crate, changes at least one line.
//!
//! WHAT THIS FILE DOES AND DOES NOT PROVE
//! --------------------------------------
//! It does NOT prove correctness: the digests were generated from this crate, so
//! they pin current behaviour, including any bug current behaviour has.
//! Correctness is `tests/differential.rs` (upstream fidelity) plus
//! `tests/known_vectors.rs` (hand-written expected bytes). This file's job is
//! narrower and it is a real job: make every behavioural change VISIBLE, so that
//! a mutation sweep has something to kill.
//!
//! Regenerate ONLY after a deliberate, reviewed behaviour change:
//!     cargo test --release --test golden -- --ignored regenerate
//! and then re-read the diff line by line.

mod support;

use std::path::PathBuf;

use support::goldenwalk::golden_lines;
use support::sweep;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/encoder.digest")
}

const GOLDEN: &str = include_str!("golden/encoder.digest");

#[test]
fn encoder_output_matches_the_golden_digests() {
    let want: Vec<&str> = GOLDEN
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let got = golden_lines();

    assert_eq!(
        got.len(),
        want.len(),
        "golden file has {} rows, sweep produced {} -- the sweep space changed; regenerate \
         deliberately",
        want.len(),
        got.len()
    );

    let mut mismatches = Vec::new();
    for (g, w) in got.iter().zip(&want) {
        if g != w {
            mismatches.push(format!("  golden: {w}\n  actual: {g}"));
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of {} golden rows changed:\n{}",
        mismatches.len(),
        want.len(),
        mismatches.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// The golden file must not be trivially satisfiable. If every row were the FNV
/// offset basis (what you get by hashing nothing) the test above would pass under
/// a total no-op encoder, and if a mode flag never changed a digest the mode axis
/// would be decoration.
#[test]
fn golden_rows_are_not_vacuous() {
    let empty = support::digest::Fnv::default().hex();
    let rows: Vec<&str> = GOLDEN
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(rows.len(), 257, "expected 256 mode rows plus a paste row");
    for row in &rows {
        let hex = row.rsplit(' ').next().unwrap();
        assert_ne!(hex, empty, "row {row:?} is the digest of nothing");
    }

    // Rows are NOT expected to be all-distinct. Most `TermMode` flags are inert
    // outside a particular regime -- `REPORT_ALTERNATE_KEYS` and
    // `REPORT_ASSOCIATED_TEXT` do nothing at all unless `kitty_seq` is on, and
    // `VI` collapses all 128 of its combinations onto one empty-output row. What
    // MUST hold is that every flag matters somewhere.
    let digests: std::collections::HashMap<&str, &str> = rows
        .iter()
        .filter(|r| !r.starts_with("__"))
        .map(|r| {
            let (label, hex) = r.rsplit_once(' ').unwrap();
            (label, hex)
        })
        .collect();

    let labels: Vec<String> = sweep::modes().into_iter().map(|(l, _)| l).collect();
    for (i, (flag_name, _)) in sweep::MODE_FLAGS.iter().enumerate() {
        let mut changed = 0usize;
        for bits in 0u32..256 {
            if bits & (1 << i) != 0 {
                continue;
            }
            let a = &labels[bits as usize];
            let b = &labels[(bits | (1 << i)) as usize];
            if digests[a.as_str()] != digests[b.as_str()] {
                changed += 1;
            }
        }
        assert!(
            changed > 0,
            "toggling {flag_name} never changed the encoder output in any of 128 mode pairs --              that flag is not wired up"
        );
        eprintln!("  {flag_name:<24} changes output in {changed:>3}/128 mode pairs");
    }

    let distinct: std::collections::HashSet<&str> = digests.values().copied().collect();
    eprintln!("  distinct digests: {} of {} mode rows", distinct.len(), digests.len());
    assert!(distinct.len() >= 40, "only {} distinct digests", distinct.len());
}

#[test]
#[ignore = "regenerates the golden file; run deliberately and review the diff"]
fn regenerate() {
    let mut body = String::new();
    body.push_str("# terminal-input encoder golden digests.\n");
    body.push_str("# One FNV-1a-64 digest per TermMode combination over the full sweep in\n");
    body.push_str("# tests/support/sweep.rs, plus one row for the paste encoder.\n");
    body.push_str("# Regenerate with: cargo test --release --test golden -- --ignored regenerate\n");
    for line in golden_lines() {
        body.push_str(&line);
        body.push('\n');
    }
    std::fs::write(golden_path(), body).unwrap();
    eprintln!("wrote {}", golden_path().display());
}
