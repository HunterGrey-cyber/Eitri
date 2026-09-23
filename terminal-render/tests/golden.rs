//! THE GOLDEN CORPUS (section 11).
//!
//! A compact, human-reviewable rendering of the PaintOp stream for a fixed set
//! of scenarios. Its purpose is NOT to re-assert the individual rules -- the
//! other test files do that -- but to make semantic drift visible: if a builder
//! change alters what is painted anywhere, the diff shows exactly where, in
//! terms a reviewer can read.
//!
//! It deliberately operates at PaintOp level, not screenshots. A screenshot
//! test would change when a font did.
//!
//! To update after an INTENDED change: run with `UPDATE_GOLDEN=1` and READ THE
//! DIFF. A golden regenerated without reading it is worse than no golden -- it
//! converts a semantic regression into a silently committed new expectation.

mod common;

use common::Screen;
use std::fmt::Write as _;
use terminal_render::{PaintList, PaintOp, RgbColor, SelectionSpan, UnderlineKind, ViewMode};

fn hex(c: RgbColor) -> String {
    format!("{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

fn underline(u: UnderlineKind) -> &'static str {
    match u {
        UnderlineKind::None => "-",
        UnderlineKind::Single => "u",
        UnderlineKind::Double => "U",
        UnderlineKind::Curl => "~",
        UnderlineKind::Dotted => ".",
        UnderlineKind::Dashed => ",",
    }
}

/// One op per line. Backgrounds are folded into runs so a 20-column blank row is
/// one line rather than twenty -- the file stays readable, which is the only
/// reason a golden is worth having.
fn render(list: &PaintList) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "surface {}x{} bg={} top_line={}",
        list.cols,
        list.rows,
        hex(list.surface_background),
        list.top_line
    );

    let mut i = 0;
    while i < list.ops.len() {
        match &list.ops[i] {
            PaintOp::FillCells { row, col, cols, color } => {
                let (mut end_col, mut j) = (col + cols, i + 1);
                while let Some(PaintOp::FillCells {
                    row: r2,
                    col: c2,
                    cols: n2,
                    color: k2,
                }) = list.ops.get(j)
                {
                    if *r2 == *row && *c2 == end_col && k2 == color {
                        end_col += n2;
                        j += 1;
                    } else {
                        break;
                    }
                }
                let _ = writeln!(out, "fill   r{row} c{col}..{end_col} {}", hex(*color));
                i = j;
                continue;
            }
            PaintOp::DrawText {
                row,
                col,
                cols,
                text,
                color,
                style,
            } => {
                let _ = writeln!(
                    out,
                    "text   r{row} c{col}+{cols} {:?} fg={}{}{}{}{} ul={}",
                    text,
                    hex(*color),
                    if style.bold { " bold" } else { "" },
                    if style.italic { " italic" } else { "" },
                    if style.strikeout { " strike" } else { "" },
                    if style.underline == UnderlineKind::None {
                        String::new()
                    } else {
                        format!(" ulc={}", hex(style.underline_color))
                    },
                    underline(style.underline)
                );
            }
            PaintOp::DrawCursor {
                row,
                col,
                cols,
                shape,
                color,
                text_under,
                blinking,
            } => {
                let _ = writeln!(
                    out,
                    "cursor r{row} c{col}+{cols} {shape:?} {}{} under={}",
                    hex(*color),
                    if *blinking { " blink" } else { "" },
                    match text_under {
                        None => "-".to_owned(),
                        Some(t) => format!("{:?}@{}", t.text, hex(t.color)),
                    }
                );
            }
            PaintOp::DrawNotice {
                row,
                col,
                cols,
                text,
                color,
                background,
            } => {
                let _ = writeln!(
                    out,
                    "notice r{row} c{col}+{cols} {:?} fg={} bg={}",
                    text,
                    hex(*color),
                    hex(*background)
                );
            }
        }
        i += 1;
    }
    out
}

fn case(name: &str, list: &PaintList) -> String {
    format!("=== {name} ===\n{}\n", render(list))
}

/// Every scenario section 11 names, in one file.
fn corpus() -> String {
    let mut out = String::new();

    let mut s = Screen::new(12, 2);
    s.feed("abc");
    out.push_str(&case("ascii", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("a\u{6f22}\u{5b57}b");
    out.push_str(&case("mixed-ascii-cjk", &s.paint()));

    let mut s = Screen::new(5, 3);
    s.feed("abcd\u{6f22}");
    out.push_str(&case("wide-char-at-wrap-boundary", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("\x1b[4;9m\u{6f22}");
    out.push_str(&case("decorated-wide-char", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("e\u{301} \u{301}\u{6f22}\u{308}");
    out.push_str(&case("combining-marks", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("\x1b[31;44mA\x1b[0m\x1b[38;5;129mB\x1b[0m\x1b[38;2;17;34;51mC");
    out.push_str(&case("colors-indexed-and-truecolor", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("\x1b[1;31mb\x1b[0m\x1b[2;31md\x1b[0m\x1b[7;31mi\x1b[0m\x1b[4;9ms");
    out.push_str(&case("bold-dim-inverse-underline-strike", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("XY\x1b[1;1H");
    out.push_str(&case("cursor-over-glyph", &s.paint()));
    out.push_str(&case(
        "cursor-unfocused",
        &s.paint_full(0, 2, ViewMode::FollowBottom, &[], false),
    ));

    let mut s = Screen::new(12, 2);
    s.feed("XY\x1b[1 q\x1b[1;1H"); // blinking block
    out.push_str(&case("cursor-blinking", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("\u{6f22}\x1b[1;1H"); // block cursor over a full-width glyph
    out.push_str(&case("cursor-over-wide-glyph", &s.paint()));

    let mut s = Screen::new(12, 2);
    s.feed("abcdef");
    out.push_str(&case(
        "selection",
        &s.paint_selected(&[SelectionSpan {
            line: 0,
            start_col: 1,
            end_col: 3,
        }]),
    ));

    let mut s = Screen::new(6, 3);
    s.feed("ABCDEFGHIJKL");
    out.push_str(&case("wrapped-rows", &s.paint()));

    let mut s = Screen::with_history(8, 3, 100);
    for i in 1..=20 {
        s.feed(&format!("L{i:02}\r\n"));
    }
    out.push_str(&case(
        "pinned-history-negative-absolute-lines",
        &s.paint_window(-9, 3, ViewMode::Pinned, &[]),
    ));
    out.push_str(&case(
        "anchor-expired",
        &s.paint_window(0, 3, ViewMode::AnchorExpired, &[]),
    ));

    out
}

const GOLDEN: &str = "tests/golden/paintops.txt";

#[test]
fn the_paintop_stream_matches_the_golden_corpus() {
    let actual = corpus();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(GOLDEN, &actual).expect("write golden");
        eprintln!("golden updated -- READ THE DIFF before committing it");
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("missing golden {GOLDEN}: {e}. Create it with UPDATE_GOLDEN=1 and review it."));
    if actual != expected {
        // A unified-ish diff, because a 200-line assert_eq! dump is unreadable
        // and an unreadable failure gets regenerated instead of understood.
        let (a, b): (Vec<_>, Vec<_>) = (expected.lines().collect(), actual.lines().collect());
        let mut report = String::from("PaintOp golden drift:\n");
        for (i, (want, got)) in a.iter().zip(b.iter()).enumerate() {
            if want != got {
                let _ = writeln!(report, "  line {}:\n    want {want}\n    got  {got}", i + 1);
            }
        }
        if a.len() != b.len() {
            let _ = writeln!(report, "  line count {} -> {}", a.len(), b.len());
        }
        panic!("{report}\nIf the change is intended, re-run with UPDATE_GOLDEN=1 and read the diff.");
    }
}

#[test]
fn the_golden_corpus_covers_every_op_variant_and_style_axis() {
    // A golden that silently stopped exercising something would keep passing
    // forever. This asserts the corpus still contains what it claims to.
    let text = corpus();
    for needle in [
        "fill ",
        "text ",
        "cursor ",
        "notice ",
        "bold",
        "strike",
        "ul=u",
        "Block",
        "HollowBlock",
        "blink",
        "top_line=-9",
        "under=",
    ] {
        assert!(text.contains(needle), "the corpus no longer exercises {needle:?}");
    }
    // Wide glyphs must be present as two-column runs.
    assert!(text.contains("+2 \"\u{6f22}\""), "no full-width glyph in the corpus");
    // And a combining mark must survive into the golden text. Checked in the
    // ESCAPED form the renderer writes: `{:?}` leaves printable CJK alone but
    // escapes a combining mark, so the file literally contains the characters
    // `e\u{301}`. Asserting the unescaped form silently never matches.
    assert!(text.contains(r"e\u{301}"), "no combining mark in the corpus");
}
