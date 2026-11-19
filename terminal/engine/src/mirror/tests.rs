use super::*;

fn part(total: u64, replay: &[u8], last: bool) -> SnapshotPartWire {
    SnapshotPartWire {
        offset: 7,
        columns: 80,
        rows: 24,
        total_bytes: total,
        replay: replay.to_vec(),
        last,
        last_input: None,
        tail_kind: WireTail::None,
        tail: Vec::new(),
        tail_calls: 0,
        history_at_cap: false,
        history_trimmed: 0,
        output_while_detached: false,
        input_written: 0,
    }
}

#[test]
fn parts_add_up_to_one_snapshot() {
    let mut a = SnapshotAssembler::new();
    assert_eq!(a.part(part(6, b"abc", false)), Ok(None));
    assert!(a.in_progress());
    let snapshot = a.part(part(6, b"def", true)).unwrap().unwrap();
    assert_eq!(snapshot.replay.bytes, b"abcdef");
    assert_eq!(snapshot.offset, 7);
    assert_eq!((snapshot.replay.columns, snapshot.replay.rows), (80, 24));
    assert_eq!(snapshot.tail, Tail::ReadAhead(Vec::new()));
    assert!(!a.in_progress());
}

#[test]
fn frames_out_of_bounds_refuse_without_panic() {
    let cases: Vec<(&str, Vec<SnapshotPartWire>, FrameRefused)> = vec![
        (
            "zero columns",
            vec![SnapshotPartWire {
                columns: 0,
                ..part(0, b"", true)
            }],
            FrameRefused::SizeOutOfRange,
        ),
        (
            "65,535 columns",
            vec![SnapshotPartWire {
                columns: 65_535,
                ..part(0, b"", true)
            }],
            FrameRefused::SizeOutOfRange,
        ),
        (
            "501 rows",
            vec![SnapshotPartWire {
                rows: 501,
                ..part(0, b"", true)
            }],
            FrameRefused::SizeOutOfRange,
        ),
        (
            "33 MiB announced",
            vec![part(33 << 20, b"x", false)],
            FrameRefused::SnapshotTooLarge,
        ),
        (
            "a part of 1 MiB + 1",
            vec![part(SNAPSHOT_MAX as u64, &vec![0; SNAPSHOT_PART_MAX + 1], false)],
            FrameRefused::PartTooLarge,
        ),
        (
            "more than announced",
            vec![part(2, b"abc", true)],
            FrameRefused::SnapshotTooLarge,
        ),
        (
            "less than announced",
            vec![part(4, b"abc", true)],
            FrameRefused::SnapshotIncomplete,
        ),
        (
            "a part from another snapshot",
            vec![
                part(6, b"abc", false),
                SnapshotPartWire {
                    offset: 8,
                    ..part(6, b"def", true)
                },
            ],
            FrameRefused::PartOutOfOrder,
        ),
        (
            "a 65 KiB raw tail",
            vec![SnapshotPartWire {
                tail_kind: WireTail::Raw,
                tail: vec![b'x'; RAW_TAIL_MAX + 1],
                ..part(0, b"", true)
            }],
            FrameRefused::TailTooLong,
        ),
        (
            "more calls than tail bytes",
            vec![SnapshotPartWire {
                tail_kind: WireTail::Raw,
                tail: b"\x1b]".to_vec(),
                tail_calls: 3,
                ..part(0, b"", true)
            }],
            FrameRefused::TailCallsImpossible,
        ),
        (
            "bytes in an empty tail",
            vec![SnapshotPartWire {
                tail: b"x".to_vec(),
                ..part(0, b"", true)
            }],
            FrameRefused::TailCallsImpossible,
        ),
        (
            "a surrogate as the last character",
            vec![SnapshotPartWire {
                last_input: Some(0xD800),
                ..part(0, b"", true)
            }],
            FrameRefused::BadLastInput,
        ),
    ];
    for (what, parts, refused) in cases {
        let mut a = SnapshotAssembler::new();
        let mut result = Ok(None);
        for p in parts {
            result = a.part(p);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(refused), "{what}");
        assert!(!a.in_progress(), "{what}: an error empties the assembler");
    }
}

#[test]
fn an_announced_length_never_sizes_the_buffer() {
    // 32 MiB announced, parts never completing: only what arrived is held.
    let mut a = SnapshotAssembler::new();
    for _ in 0..3 {
        assert_eq!(a.part(part(SNAPSHOT_MAX as u64, &[0; 1024], false)), Ok(None));
    }
    assert!(a.bytes.capacity() < 64 * 1024, "capacity {}", a.bytes.capacity());
}

#[test]
fn a_raw_tail_and_a_read_ahead_tail_come_through() {
    let mut a = SnapshotAssembler::new();
    let raw = a
        .part(SnapshotPartWire {
            tail_kind: WireTail::Raw,
            tail: b"\x1b]2;ti".to_vec(),
            tail_calls: 0,
            last_input: Some('x' as u32),
            ..part(0, b"", true)
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        raw.tail,
        Tail::Raw {
            bytes: b"\x1b]2;ti".to_vec(),
            calls: 0
        }
    );
    assert_eq!(raw.replay.last_input, Some('x'));
    let ahead = a
        .part(SnapshotPartWire {
            tail_kind: WireTail::ReadAhead,
            tail: b"abc".to_vec(),
            ..part(0, b"", true)
        })
        .unwrap()
        .unwrap();
    assert_eq!(ahead.tail, Tail::ReadAhead(b"abc".to_vec()));
}

#[test]
fn last_input_accepts_del_and_c1_and_refuses_c0() {
    for c in [0x7f, 0x85, 0x9b] {
        let snapshot = SnapshotAssembler::new()
            .part(SnapshotPartWire {
                last_input: Some(c),
                ..part(0, b"", true)
            })
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.replay.last_input, char::from_u32(c));
    }
    for c in [0x1b, 0x07] {
        let mut a = SnapshotAssembler::new();
        let refused = a.part(SnapshotPartWire {
            last_input: Some(c),
            ..part(0, b"", true)
        });
        assert_eq!(refused, Err(FrameRefused::BadLastInput), "{c:#x}");
    }
}

#[test]
fn a_mirror_screen_queues_no_answer_live_or_replayed() {
    let size = PtySize {
        cols: 80,
        rows: 24,
        cell_width_px: 8,
        cell_height_px: 16,
    };
    let queries: &[u8] = b"\x1b[c\x1b[6n\x1b]11;?\x07\x1b]4;1;?\x07\x1b[18t\x1b[14t\x1b[?u\x1b[>c";
    // The same bytes on a window's screen do produce answers: the check below is not vacuous.
    let mut window = Screen::new(size, TerminalColors::default());
    window.feed(queries);
    assert!(!window.take_replies().is_empty());

    let mut live = Screen::mirror(size, TerminalColors::default());
    live.feed(queries);
    assert!(live.take_replies().is_empty());

    let replay = ScreenReplay {
        columns: 80,
        rows: 24,
        bytes: queries.to_vec(),
        last_input: None,
    };
    let mut replayed = Screen::replayed_untrusted(&replay, size, TerminalColors::default()).unwrap();
    assert!(replayed.take_replies().is_empty());
}
