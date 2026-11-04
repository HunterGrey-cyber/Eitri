//! THE ENCODINGS. A byte count for a lossy encoder is not a measurement.
//!
//! Every frame every other corpus produces is round-tripped through both
//! encodings and compared for exact equality. The bandwidth table in the README
//! is only worth reading because of this file.

mod common;

use common::{Harness, Rng};
use terminal_frame::encode::{
    decode_naive, decode_rle, encode_naive, encode_rle, header_len, DecodeError, NAIVE_CELL_BYTES, UNIFORM_MIN_RUN,
};
use terminal_frame::{Projector, Rgb, TerminalFrame};

/// The same corpus shape the differential uses, kept independent of it so a
/// change there cannot quietly shrink the round-trip coverage here.
fn frames(seed: u64, steps: usize, cols: usize, rows: usize) -> Vec<TerminalFrame> {
    const OPS: &[&str] = &[
        "hello",
        "a",
        "\u{6f22}\u{5b57}",
        "e\u{301}",
        "\u{6f22}\u{301}",
        "\u{1f600}",
        "\t",
        "\r\n",
        "\x1b[1m",
        "\x1b[0m",
        "\x1b[31;44m",
        "\x1b[38;2;9;8;7m",
        "\x1b[48;5;99m",
        "\x1b[4:3m",
        "\x1b[58;2;1;2;3m",
        "\x1b[59m",
        "\x1b[K",
        "\x1b[J",
        "\x1b[3@",
        "\x1b[2P",
        "\x1b[5X",
        "\x1b[2L",
        "\x1b[1M",
        "\x1b[?25l",
        "\x1b[?25h",
        "\x1b[4 q",
        "\x1b[?1049h",
        "\x1b[?1049l",
        "\x1b]4;3;#abcdef\x07",
        "\x1b]104;3\x07",
        "\x1b]11;#001122\x07",
        "\x1b]111\x07",
        "\x1b[4h",
        "\x1b[4l",
        "\x1b#8",
    ];
    let mut rng = Rng::new(seed);
    let mut harness = Harness::new(cols, rows);
    let mut projector = Projector::new();
    let mut out = Vec::with_capacity(steps + 1);
    out.push(projector.full(&harness.term));
    for _ in 0..steps {
        let ops = 1 + rng.below(5);
        let mut chunk = Vec::new();
        for _ in 0..ops {
            if rng.below(6) == 0 {
                chunk.extend_from_slice(format!("\x1b[{};{}H", 1 + rng.below(rows), 1 + rng.below(cols)).as_bytes());
            } else {
                chunk.extend_from_slice(rng.pick(OPS).as_bytes());
            }
        }
        harness.feed(&chunk);
        out.push(projector.next(&mut harness.term));
    }
    out
}

fn corpus() -> Vec<TerminalFrame> {
    let mut out = Vec::new();
    for seed in 1..=16u64 {
        out.extend(frames(seed, 120, 24, 8));
    }
    for seed in 101..=104u64 {
        out.extend(frames(seed, 60, 120, 40));
    }
    out
}

#[test]
fn both_encodings_round_trip_every_frame_in_the_corpus() {
    let frames = corpus();
    assert!(frames.len() > 2000, "corpus shrank to {} frames", frames.len());
    let mut cells = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        frame
            .check()
            .unwrap_or_else(|e| panic!("frame {index} malformed: {e:?}"));
        cells += frame.cell_count();

        let naive = encode_naive(frame);
        let back = decode_naive(&naive).unwrap_or_else(|e| panic!("frame {index} naive: {e:?}"));
        assert_eq!(&back, frame, "naive round-trip differs at frame {index}");

        let rle = encode_rle(frame);
        let back = decode_rle(&rle).unwrap_or_else(|e| panic!("frame {index} rle: {e:?}"));
        assert_eq!(&back, frame, "rle round-trip differs at frame {index}");
    }
    assert!(cells > 500_000, "only {cells} cells round-tripped");
    eprintln!(
        "round-tripped {} frames / {cells} cells through both encodings",
        frames.len()
    );
}

#[test]
fn the_naive_body_really_is_a_fixed_record_per_cell() {
    // The claim the naive number rests on. Any frame whose cells carry no
    // extras must encode to exactly header + per-row head + 15 bytes per cell.
    let mut harness = Harness::new(40, 4);
    harness.feed(b"\x1b[1;1Hplain ascii, no combining marks");
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    assert!(frame
        .rows_changed
        .iter()
        .all(|r| r.cells.iter().all(|c| c.extra.is_none())));

    // Per row: zigzag line + left + right, all < 128 here, so one byte each.
    let head: usize = frame.rows_changed.len() * 3;
    let expected = header_len(&frame) + head + frame.cell_count() * NAIVE_CELL_BYTES;
    assert_eq!(encode_naive(&frame).len(), expected);
}

#[test]
fn the_rle_encoding_is_smaller_than_the_naive_one_on_a_real_screen() {
    // Not a tautology: the RLE pays a per-segment header, so a maximally
    // heterogeneous row could in principle cost more. This checks the case the
    // bandwidth argument actually depends on.
    // A FULL 120x40 screen: every column of every row carries text, with an SGR
    // change every 20 columns, which is what a colourful TUI actually looks
    // like. A half-empty screen would flatter the RLE.
    let mut harness = Harness::canonical();
    for line in 0..40 {
        harness.feed_str(&format!("\x1b[{};1H", line + 1));
        for block in 0..6 {
            harness.feed_str(&format!(
                "\x1b[3{};4{}mrow {line:02} block {block} xy",
                (line + block) % 8,
                (line * 3 + block) % 8
            ));
        }
    }
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    let naive = encode_naive(&frame).len();
    let rle = encode_rle(&frame).len();
    eprintln!(
        "120x40 full screen, {} cells: naive {naive} B, rle {rle} B ({:.1}x)",
        frame.cell_count(),
        naive as f64 / rle as f64
    );
    assert!(
        rle * 2 < naive,
        "rle {rle} is not meaningfully smaller than naive {naive}"
    );
}

#[test]
fn the_rle_encoding_collapses_a_blank_screen_to_almost_nothing() {
    let harness = Harness::canonical();
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    assert!(frame.rows_changed.is_empty(), "a blank screen has no non-default rows");
    let rle = encode_rle(&frame).len();
    assert!(rle < 32, "a blank full frame cost {rle} bytes");
    eprintln!("blank 120x40 full frame: {rle} B");
}

#[test]
fn the_uniform_threshold_is_the_measured_minimum() {
    // MEASURED, not argued. Encode the whole round-trip corpus at every
    // candidate threshold and take the argmin. The break-even point is not a
    // constant -- it depends on whether the run is at the end of a row or
    // sandwiched between literals -- so the only defensible way to pick one is
    // to try them all on real frames.
    use terminal_frame::encode::encode_rle_with_threshold;

    let frames = corpus();
    let mut totals = Vec::new();
    for threshold in 1..=12usize {
        let total: usize = frames
            .iter()
            .map(|frame| encode_rle_with_threshold(frame, threshold).len())
            .sum();
        totals.push((threshold, total));
    }
    eprintln!("corpus bytes by uniform threshold: {totals:?}");

    let (best, best_bytes) = *totals.iter().min_by_key(|(_, bytes)| *bytes).unwrap();
    let shipped = totals.iter().find(|(t, _)| *t == UNIFORM_MIN_RUN).unwrap().1;
    eprintln!(
        "shipped threshold {UNIFORM_MIN_RUN} costs {shipped} B; the minimum is {best} at \
         {best_bytes} B"
    );
    assert_eq!(
        UNIFORM_MIN_RUN, best,
        "UNIFORM_MIN_RUN is {UNIFORM_MIN_RUN} but {best} encodes this corpus smaller \
         ({best_bytes} B vs {shipped} B)"
    );

    // The sweep has to be able to distinguish thresholds at all, or the argmin
    // above is meaningless.
    let distinct: std::collections::BTreeSet<usize> = totals.iter().map(|(_, bytes)| *bytes).collect();
    assert!(
        distinct.len() >= 5,
        "the threshold barely changes the size ({distinct:?}); this test cannot choose anything"
    );

    // And at the shipped threshold the parameterised encoder IS `encode_rle`.
    for frame in frames.iter().take(200) {
        assert_eq!(encode_rle_with_threshold(frame, UNIFORM_MIN_RUN), encode_rle(frame));
    }
}

#[test]
fn every_threshold_produces_a_decodable_stream() {
    // The threshold decides where the encoder splits; the wire format records
    // the split. If that were not true, the sweep above would be comparing
    // incompatible formats.
    use terminal_frame::encode::encode_rle_with_threshold;
    let frames = frames(11, 60, 24, 8);
    for threshold in [1usize, 2, 4, 7, 12, 1000] {
        for frame in &frames {
            let bytes = encode_rle_with_threshold(frame, threshold);
            assert_eq!(
                &decode_rle(&bytes).unwrap(),
                frame,
                "threshold {threshold} did not round-trip"
            );
        }
    }
}

#[test]
fn a_truncated_buffer_is_an_error_not_a_panic() {
    let frames = frames(7, 40, 24, 8);
    let frame = frames.iter().max_by_key(|f| f.cell_count()).unwrap();
    for (name, bytes) in [("naive", encode_naive(frame)), ("rle", encode_rle(frame))] {
        for cut in [0usize, 1, 3, 7, 11, bytes.len() / 3, bytes.len() / 2, bytes.len() - 1] {
            let result = if name == "naive" {
                decode_naive(&bytes[..cut])
            } else {
                decode_rle(&bytes[..cut])
            };
            assert!(result.is_err(), "{name} decoded a buffer truncated to {cut} bytes");
        }
    }
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut harness = Harness::new(10, 2);
    harness.feed(b"hi");
    let mut projector = Projector::new();
    let frame = projector.full(&harness.term);
    let mut bytes = encode_rle(&frame);
    bytes.push(0);
    assert_eq!(decode_rle(&bytes), Err(DecodeError::Trailing(1)));
}

#[test]
fn a_bad_version_byte_is_rejected() {
    let mut harness = Harness::new(10, 2);
    harness.feed(b"hi");
    let mut projector = Projector::new();
    let mut bytes = encode_naive(&projector.full(&harness.term));
    bytes[0] = 99;
    assert_eq!(decode_naive(&bytes), Err(DecodeError::BadVersion(99)));
}

#[test]
fn every_cell_attribute_survives_both_encodings() {
    // The round-trip corpus is random; this one is exhaustive over the parts a
    // random corpus reaches unevenly.
    use terminal_frame::{CellExtras, CellFlags, FrameCell, FrameColor, Rgb};

    let mut cells = Vec::new();
    for bits in 0..(1u16 << 15) {
        let Some(flags) = CellFlags::from_bits(bits) else {
            continue;
        };
        cells.push(FrameCell {
            c: char::from_u32(0x20 + (bits as u32 % 0x2000)).unwrap_or('x'),
            fg: if bits % 3 == 0 {
                FrameColor::Rgb(Rgb::new(bits as u8, 7, 9))
            } else {
                FrameColor::Palette(bits % 269)
            },
            bg: FrameColor::Palette((bits as u32 % 269) as u16),
            flags,
            extra: if bits % 5 == 0 {
                Some(Box::new(CellExtras {
                    zerowidth: vec!['\u{301}', '\u{308}'],
                    underline_color: Some(FrameColor::Rgb(Rgb::new(1, 2, 3))),
                }))
            } else {
                None
            },
        });
    }
    assert_eq!(cells.len(), 1 << 15, "every flag combination should be represented");

    use terminal_frame::{FrameKind, RowUpdate, TerminalFrame};
    let frame = TerminalFrame {
        generation: 42,
        kind: FrameKind::Delta,
        cols: cells.len() as u16,
        rows: 1,
        cursor: Default::default(),
        focused: true,
        modes: Default::default(),
        color_overrides: (0..269)
            .map(|index| terminal_frame::ColorOverride {
                index,
                color: if index % 2 == 0 {
                    Some(Rgb::new(index as u8, 1, 2))
                } else {
                    None
                },
            })
            .collect(),
        rows_changed: vec![RowUpdate {
            line: -7,
            left: 0,
            right: cells.len() as u16 - 1,
            cells,
        }],
    };
    assert_eq!(decode_naive(&encode_naive(&frame)).unwrap(), frame);
    assert_eq!(decode_rle(&encode_rle(&frame)).unwrap(), frame);
}

#[test]
fn the_worst_case_full_frame_sets_the_ceiling_for_the_transport_argument() {
    // The real-PTY measurement gives typical numbers. This gives the CEILING,
    // which is the number a transport decision actually has to survive: a
    // 120x40 screen in which every single cell has its own 24-bit foreground,
    // its own 24-bit background and its own flag combination, so the RLE has
    // nothing whatsoever to run over.
    use terminal_frame::{CellFlags, FrameCell, FrameColor, FrameKind, RowUpdate, TerminalFrame};

    let (cols, rows) = (120u16, 40u16);
    let mut rng = Rng::new(0xfeed);
    let rows_changed: Vec<RowUpdate> = (0..rows as i32)
        .map(|line| {
            let cells = (0..cols)
                .map(|_| {
                    let a = rng.next_u64();
                    FrameCell {
                        c: char::from_u32(0x4e00 + (a % 0x2000) as u32).unwrap(),
                        fg: FrameColor::Rgb(Rgb::new(a as u8, (a >> 8) as u8, (a >> 16) as u8)),
                        bg: FrameColor::Rgb(Rgb::new((a >> 24) as u8, (a >> 32) as u8, (a >> 40) as u8)),
                        flags: CellFlags::from_bits_truncate((a >> 48) as u16 & 0x7fff),
                        extra: None,
                    }
                })
                .collect();
            RowUpdate {
                line,
                left: 0,
                right: cols - 1,
                cells,
            }
        })
        .collect();

    let frame = TerminalFrame {
        generation: 1,
        kind: FrameKind::Full,
        cols,
        rows,
        cursor: Default::default(),
        focused: true,
        modes: Default::default(),
        color_overrides: Vec::new(),
        rows_changed,
    };
    frame.check().unwrap();
    assert_eq!(frame.cell_count(), 4800);

    let naive = encode_naive(&frame).len();
    let rle = encode_rle(&frame).len();
    assert_eq!(
        decode_rle(&encode_rle(&frame)).unwrap(),
        frame,
        "the ceiling must still decode"
    );
    eprintln!(
        "WORST-CASE 120x40 full frame (every cell a unique style): naive {naive} B, rle {rle} B\n\
         at 60 fps that is {:.2} MB/s naive, {:.2} MB/s rle",
        naive as f64 * 60.0 / 1e6,
        rle as f64 * 60.0 / 1e6
    );

    // A best case for contrast: the same geometry, one style, one character.
    let uniform: Vec<RowUpdate> = (0..rows as i32)
        .map(|line| RowUpdate {
            line,
            left: 0,
            right: cols - 1,
            cells: vec![FrameCell::default(); cols as usize],
        })
        .collect();
    let best = TerminalFrame {
        rows_changed: uniform,
        ..frame.clone()
    };
    eprintln!(
        "BEST-CASE 120x40 full frame (all default): rle {} B",
        encode_rle(&best).len()
    );

    // A FINDING, recorded rather than asserted away: on this pathological frame
    // the RLE is about 3.5% LARGER than the naive encoding. Every segment is
    // one cell, so it pays a 2-byte segment header that the fixed record does
    // not, and only claws back one byte on the character (a varint CJK
    // codepoint is 3 bytes against the fixed 4). The RLE's advantage is a
    // property of real screens being repetitive, not a property of the
    // encoding, and a shipping transport should therefore emit
    // `min(naive, rle)` behind a one-byte tag rather than committing to either.
    assert!(
        rle > naive,
        "the RLE stopped losing on its worst case ({rle} vs {naive}); the finding above should \
         be revisited"
    );
    assert!(
        rle as f64 / naive as f64 <= 1.10,
        "the RLE's worst case got worse than 10%: {rle} vs {naive}"
    );
    assert!(rle < 80_000, "worst-case rle frame is {rle} B");
}
