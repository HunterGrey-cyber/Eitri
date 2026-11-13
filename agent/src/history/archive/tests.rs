use super::*;
use serde_json::json;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("eitri-archive-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn id() -> ArchiveId {
    ArchiveId::new("conversation", "session").unwrap()
}
fn turn(n: u64, text: &str) -> ArchivedTurn {
    ArchivedTurn {
        turn: n,
        records: vec![ArchivedRecord::UserPrompt(UserPromptRecord {
            seq: n * 10,
            text: text.into(),
            delivered: false,
            attachments: Vec::new(),
        })],
    }
}
fn read(dir: &Path, id: &ArchiveId, before: u64, turns: usize, chars: usize) -> Option<Page> {
    read_page(dir, id, before, turns, chars, &AtomicBool::new(false)).unwrap()
}

#[test]
fn round_trip_every_kind_and_nested_tool_children() {
    let scratch = Scratch::new();
    let id = id();
    let records = vec![
        json!({"kind":"user_prompt","record":{"seq":1,"text":"prompt"}}),
        json!({"kind":"transcript","record":{"seq":2,"text":"reply"}}),
        json!({"kind":"thinking","record":{"seq":3,"turn_id":"t","text":"thought"}}),
        json!({"kind":"tool_call","record":{"seq":4,"turn_id":"t","tool_use_id":"call","name":"Task",
            "input":{"prompt":"work"},"result":{"content":"done","is_error":false},
            "children":{"transcript":[{"seq":1,"text":"child reply"}],"thinking":[{"seq":2,"turn_id":"child","text":"child thought"}],
                "tool_calls":[{"seq":3,"turn_id":"child","tool_use_id":"child-call","name":"Read","input":{"path":"f"},"result":null}],"assistant_message_open":false,"thinking_open":false}}}),
        json!({"kind":"turn_start","record":{"seq":5,"turn_id":"t","origin":"cli","cause":null}}),
        json!({"kind":"turn_ending","record":{"seq":6,"turn_id":"t","kind":"failed","detail":{}}}),
        json!({"kind":"notice","record":{"seq":7,"kind":"informational","level":"notice","content":"notice"}}),
        json!({"kind":"question","record":{"seq":8,"interaction_id":"ask","questions":[{"question":"which?"}],"state":{"kind":"waiting"}}}),
    ].into_iter().map(|value| serde_json::from_value(value).unwrap()).collect();
    let turn = ArchivedTurn { turn: 42, records };
    ArchiveWriter::open(&scratch.0, &id, true)
        .unwrap()
        .append(&[turn.clone()])
        .unwrap();
    let page = read(&scratch.0, &id, 43, 200, PAGE_MAX_CHARS).unwrap();
    assert_eq!(page.turns, vec![turn]);
    assert_eq!(page.from_turn, 42);
    let path = id.path(&scratch.0.join("conversation"), 0);
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 8);
}

#[test]
fn rotation_retains_four_segments_and_newest_contiguous_turns() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    // Sparse, newline-filled padding exercises the actual rotation threshold without huge records.
    for n in 0..6 {
        writer.append(&[turn(n, "kept")]).unwrap();
        writer.file.set_len(SEGMENT_BYTES).unwrap();
        writer.bytes = SEGMENT_BYTES;
        // Make the sparse tail a complete malformed line, which the reader skips.
        writer.file.write_all(b"\n").unwrap();
    }
    let files = segments(&writer.dir, &id).unwrap();
    assert_eq!(files.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![2, 3, 4, 5]);
    let page = read(&scratch.0, &id, 6, 100, PAGE_MAX_CHARS).unwrap();
    assert_eq!(page.turns.iter().map(|t| t.turn).collect::<Vec<_>>(), vec![3, 4, 5]);
}

#[test]
fn torn_tail_is_truncated_and_next_append_is_a_separate_line() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer.append(&[turn(0, "first")]).unwrap();
    let good_len = writer.bytes;
    writer.file.write_all(b"{\"v\":1,\"turn\":").unwrap();
    drop(writer);
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    assert_eq!(writer.bytes, good_len);
    writer.append(&[turn(1, "second")]).unwrap();
    assert_eq!(
        read(&scratch.0, &id, 2, 20, 100).unwrap().turns,
        vec![turn(0, "first"), turn(1, "second")]
    );
    let mut other = id.clone();
    other.generation = "0123456789abcdef".into();
    let mut empty = ArchiveWriter::open(&scratch.0, &other, true).unwrap();
    empty.file.write_all(b"no newline").unwrap();
    drop(empty);
    assert_eq!(ArchiveWriter::open(&scratch.0, &other, true).unwrap().bytes, 0);
}

#[test]
fn oversized_text_is_cut_at_utf8_head_and_turn_survives() {
    let scratch = Scratch::new();
    let id = id();
    let huge = "界".repeat(super::ARCHIVE_MAX_LINE_BYTES);
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer.append(&[turn(0, &huge)]).unwrap();
    let bytes = std::fs::read(id.path(&writer.dir, 0)).unwrap();
    assert!(bytes.len() <= super::ARCHIVE_MAX_LINE_BYTES + 1);
    let line: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(line["cut"], true);
    let text = line["record"]["text"].as_str().unwrap();
    assert!(text.starts_with("界界"));
    assert!(text.ends_with("[cut]"));
    assert_eq!(read(&scratch.0, &id, 1, 10, 1).unwrap().turns.len(), 1);
}

#[test]
fn gaps_and_page_caps_keep_whole_newest_turns() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer
        .append(&[turn(0, "old"), turn(2, "界界界"), turn(3, "four"), turn(4, "five")])
        .unwrap();
    let page = read(&scratch.0, &id, 5, 100, 100).unwrap();
    assert_eq!(page.turns.iter().map(|t| t.turn).collect::<Vec<_>>(), vec![2, 3, 4]);
    assert_eq!(read(&scratch.0, &id, 5, 2, 100).unwrap().from_turn, 3);
    assert_eq!(read(&scratch.0, &id, 5, 100, 8).unwrap().from_turn, 3);
    assert_eq!(read(&scratch.0, &id, 4, 100, 7).unwrap().from_turn, 2);
    assert_eq!(read(&scratch.0, &id, 5, 0, 0).unwrap().turns, vec![turn(4, "five")]);
    assert!(read(&scratch.0, &id, 2, 10, 100).is_none());
    assert!(read(&scratch.0, &id, 0, 10, 100).is_none());
}

#[test]
fn retries_keep_last_record_and_skip_malformed_and_unknown_version_lines() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer
        .append(&[turn(0, "old"), turn(1, "next"), turn(0, "replacement")])
        .unwrap();
    writer
        .file
        .write_all(
            b"not json\n{\"v\":2,\"turn\":0,\"kind\":\"user_prompt\",\"record\":{\"seq\":0,\"text\":\"wrong\"}}\n",
        )
        .unwrap();
    let page = read(&scratch.0, &id, 2, 10, 100).unwrap();
    assert_eq!(page.turns, vec![turn(0, "replacement"), turn(1, "next")]);
    // A replacement that shrinks must leave room for the older turn.
    writer.append(&[turn(1, "x")]).unwrap();
    assert_eq!(read(&scratch.0, &id, 2, 10, 12).unwrap().turns.len(), 2);
}

#[test]
fn opening_deletes_only_this_sessions_other_generations() {
    let scratch = Scratch::new();
    let id = id();
    let writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    let dir = writer.dir.clone();
    drop(writer);
    let names = [
        "session.json",
        "session.evicted-garbage.jsonl",
        "session.evicted-0123456789abcdef.no.jsonl",
        "other.evicted-0123456789abcdef.0.jsonl",
    ];
    for name in names {
        std::fs::write(dir.join(name), b"untouched").unwrap();
    }
    let old = id.path(&dir, 0);
    let mut new = id.clone();
    new.generation = "0123456789abcdef".into();
    ArchiveWriter::open(&scratch.0, &new, true).unwrap();
    assert!(!old.exists());
    for name in names {
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"untouched");
    }
}

#[test]
fn cancellation_discards_a_page_and_invalid_ids_never_create_files() {
    let scratch = Scratch::new();
    let id = id();
    ArchiveWriter::open(&scratch.0, &id, true)
        .unwrap()
        .append(&[turn(0, "one")])
        .unwrap();
    assert!(read_page(&scratch.0, &id, 1, 10, 100, &AtomicBool::new(true))
        .unwrap()
        .is_none());
    assert!(ArchiveId::new("../escape", "sid").is_err());
    assert!(ArchiveId::new("c", "../escape").is_err());
    let mut bad = id.clone();
    bad.generation = "../escape".into();
    assert!(ArchiveWriter::open(&scratch.0, &bad, true).is_err());
    assert!(read_page(&scratch.0, &bad, 1, 10, 100, &AtomicBool::new(false)).is_err());
}

#[test]
fn write_errors_are_returned_for_the_caller_to_defer_eviction() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer.append(&[turn(0, "kept")]).unwrap();
    let before = std::fs::read(id.path(&writer.dir, 0)).unwrap();
    writer.write_fault = Some((2, false));
    let err = writer
        .append(&[turn(1, "failed one"), turn(2, "failed two")])
        .unwrap_err();
    assert_eq!(err.to_string(), "injected archive write failure");
    assert_eq!(std::fs::read(id.path(&writer.dir, 0)).unwrap(), before);
    assert!(read(&scratch.0, &id, 2, 10, 100).is_none());
    writer.append(&[turn(1, "retry")]).unwrap();
    assert_eq!(
        read(&scratch.0, &id, 2, 10, 100).unwrap().turns,
        vec![turn(0, "kept"), turn(1, "retry")]
    );
}

#[test]
fn cancellation_after_the_first_line_discards_partial_results() {
    let scratch = Scratch::new();
    let id = id();
    ArchiveWriter::open(&scratch.0, &id, true)
        .unwrap()
        .append(&[turn(0, "one"), turn(1, "two")])
        .unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    AFTER_READ_LINE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || signal.store(true, Ordering::Release))));
    assert!(read_page(&scratch.0, &id, 2, 10, 100, &cancel).unwrap().is_none());
    assert!(cancel.load(Ordering::Acquire));
}

#[test]
fn oversized_thinking_reply_and_tool_payloads_remain_readable() {
    let scratch = Scratch::new();
    let id = id();
    let huge = "x".repeat(super::ARCHIVE_MAX_LINE_BYTES + 1);
    let records = vec![
        ArchivedRecord::Thinking(ThinkingBlock {
            seq: 1,
            turn_id: "t".into(),
            text: huge.clone(),
        }),
        ArchivedRecord::Transcript(TranscriptMessage {
            seq: 2,
            text: huge.clone(),
        }),
        serde_json::from_value(
            json!({"kind":"tool_call","record":{"seq":3,"turn_id":"t","tool_use_id":"call","name":"Read",
            "input":{"text":huge},"result":{"content":"y".repeat(super::ARCHIVE_MAX_LINE_BYTES+1),"is_error":false}}}),
        )
        .unwrap(),
    ];
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer.append(&[ArchivedTurn { turn: 0, records }]).unwrap();
    let page = read(&scratch.0, &id, 1, 10, PAGE_MAX_CHARS).unwrap();
    assert_eq!(page.turns[0].records.len(), 3);
    for bytes in PrivateLines::open(
        &id.path(&writer.dir, 0),
        super::ARCHIVE_MAX_LINE_BYTES,
        &AtomicBool::new(false),
    )
    .unwrap()
    {
        let value: serde_json::Value = serde_json::from_slice(&bytes.unwrap()).unwrap();
        assert_eq!(value["cut"], true);
        match value["kind"].as_str().unwrap() {
            "tool_call" => {
                assert!(value["record"]["input"].as_str().unwrap().ends_with("[cut]"));
                assert!(value["record"]["result"]["content"]
                    .as_str()
                    .unwrap()
                    .ends_with("[cut]"));
            }
            _ => assert!(value["record"]["text"].as_str().unwrap().ends_with("[cut]")),
        }
    }
}

#[test]
fn page_character_ceiling_and_tool_payload_cost_are_enforced() {
    let scratch = Scratch::new();
    let id = id();
    let text = "x".repeat(PAGE_MAX_CHARS / 2 + 1);
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer.append(&[turn(0, &text), turn(1, &text)]).unwrap();
    assert_eq!(read(&scratch.0, &id, 2, 200, usize::MAX).unwrap().from_turn, 1);
    let record: ArchivedRecord = serde_json::from_value(json!({"kind":"tool_call","record":{
        "seq":1,"turn_id":"t","tool_use_id":"c","name":"Read","input":"界界",
        "result":{"content":"reply","is_error":false}}}))
    .unwrap();
    assert_eq!(record_chars(&record), 4 + 2 + 5);
}

#[test]
fn rotation_keeps_a_line_whole_before_the_segment_boundary() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer.file.set_len(SEGMENT_BYTES - 10).unwrap();
    writer.bytes = SEGMENT_BYTES - 10;
    writer.append(&[turn(0, "whole")]).unwrap();
    assert_eq!(writer.k, 1);
    assert_eq!(id.path(&writer.dir, 0).metadata().unwrap().len(), SEGMENT_BYTES - 10);
    assert!(writer.bytes < SEGMENT_BYTES);
    assert_eq!(read(&scratch.0, &id, 1, 10, 100).unwrap().turns, vec![turn(0, "whole")]);
}

#[test]
fn an_older_turn_over_budget_is_measured_without_retaining_its_payload() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    writer
        .append(&[turn(0, &"x".repeat(PAGE_MAX_CHARS)), turn(1, "new")])
        .unwrap();
    assert_eq!(read(&scratch.0, &id, 2, 10, 10).unwrap().turns, vec![turn(1, "new")]);
    assert_eq!(RETAINED_RECORDS.with(|count| count.get()), 1);
}

#[test]
fn a_many_turn_page_opens_each_segment_at_most_twice() {
    let scratch = Scratch::new();
    let id = id();
    let writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    let dir = writer.dir.clone();
    drop(writer);
    let mut expected = Vec::new();
    for k in 0..4 {
        let mut file = append_private(&id.path(&dir, k)).unwrap();
        for n in k * 60..(k + 1) * 60 {
            let mut turn = turn(n, &format!("turn {n}"));
            // Turn identity separates otherwise identical record keys.
            let ArchivedRecord::UserPrompt(record) = &mut turn.records[0] else {
                unreachable!();
            };
            record.seq = 0;
            let mut line = encode(n, &turn.records[0]).unwrap();
            line.push(b'\n');
            file.write_all(&line).unwrap();
            if (20..220).contains(&n) {
                expected.push(turn);
            }
        }
    }
    let page = read(&scratch.0, &id, 220, 200, PAGE_MAX_CHARS).unwrap();
    assert_eq!(page.from_turn, 20);
    assert_eq!(page.turns, expected);
    SEGMENT_OPENS.with(|counts| {
        let counts = counts.borrow();
        assert_eq!(counts.len(), 4);
        for (path, count) in counts.iter() {
            assert!(*count <= 2, "{} opened {count} times", path.display());
        }
    });
}

#[test]
fn rotation_does_not_serve_a_turn_whose_first_segment_was_deleted() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    let mut large = turn(0, "short prompt");
    for seq in 1..=64 {
        large.records.push(ArchivedRecord::Transcript(TranscriptMessage {
            seq,
            text: "x".repeat(1 << 20),
        }));
    }
    writer.append(&[large]).unwrap();
    let files = segments(&writer.dir, &id).unwrap();
    assert_eq!(files.len(), 4);
    assert_eq!(files[0].0, 1);
    assert!(read(&scratch.0, &id, 1, 10, PAGE_MAX_CHARS).is_none());
    writer.append(&[turn(1, "whole"), turn(2, "newest")]).unwrap();
    assert_eq!(
        read(&scratch.0, &id, 3, 10, PAGE_MAX_CHARS).unwrap().turns,
        vec![turn(1, "whole"), turn(2, "newest")]
    );
}

#[test]
fn cut_children_are_reported_and_last_cut_flag_wins_per_record_key() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    let tool: ArchivedRecord = serde_json::from_value(json!({"kind":"tool_call","record":{
        "seq":1,"turn_id":"t","tool_use_id":"call","name":"Task","input":null,
        "children":{"transcript":[{"seq":2,"text":"x".repeat(super::ARCHIVE_MAX_LINE_BYTES)}],"thinking":[],"tool_calls":[],"assistant_message_open":false,"thinking_open":false}
    }})).unwrap();
    writer
        .append(&[ArchivedTurn {
            turn: 0,
            records: vec![tool],
        }])
        .unwrap();
    let page = read(&scratch.0, &id, 1, 10, 100).unwrap();
    assert_eq!(page.cut, vec![(0, "tool_call", 1)]);
    let ArchivedRecord::ToolCall(mut tool) = page.turns[0].records[0].clone() else {
        panic!()
    };
    assert!(tool.children.is_none());
    writer
        .append(&[ArchivedTurn {
            turn: 0,
            records: vec![ArchivedRecord::ToolCall(tool.clone())],
        }])
        .unwrap();
    assert!(read(&scratch.0, &id, 1, 10, 100).unwrap().cut.is_empty());
    tool.input = json!("x".repeat(super::ARCHIVE_MAX_LINE_BYTES));
    writer
        .append(&[
            ArchivedTurn {
                turn: 0,
                records: vec![ArchivedRecord::ToolCall(tool)],
            },
            turn(1, "uncut"),
        ])
        .unwrap();
    assert_eq!(
        read(&scratch.0, &id, 2, 10, PAGE_MAX_CHARS).unwrap().cut,
        vec![(0, "tool_call", 1)]
    );
    assert!(read(&scratch.0, &id, 2, 1, 100).unwrap().cut.is_empty());
}

#[test]
fn failed_chunk_after_multiple_rotations_restores_start_and_keeps_old_segments() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    for n in 0..4 {
        writer.append(&[turn(n, "kept")]).unwrap();
        writer.file.set_len(SEGMENT_BYTES - 10).unwrap();
        writer.bytes = SEGMENT_BYTES - 10;
    }
    let saved_k = writer.k;
    let saved_bytes = writer.bytes;
    let old_files = segments(&writer.dir, &id).unwrap();
    let old_lengths: Vec<_> = old_files.iter().map(|(_, p)| p.metadata().unwrap().len()).collect();
    let records = (0..20)
        .map(|seq| {
            ArchivedRecord::Transcript(TranscriptMessage {
                seq,
                text: "x".repeat(1 << 20),
            })
        })
        .collect();
    writer.write_fault = Some((18, true));
    let err = writer.append(&[ArchivedTurn { turn: 4, records }]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WriteZero);
    assert_eq!((writer.k, writer.bytes), (saved_k, saved_bytes));
    assert_eq!(segments(&writer.dir, &id).unwrap(), old_files);
    for ((_, path), len) in old_files.iter().zip(old_lengths) {
        assert_eq!(path.metadata().unwrap().len(), len);
    }
    assert!(read(&scratch.0, &id, 5, 10, PAGE_MAX_CHARS).is_none());
    writer.append(&[turn(4, "retry")]).unwrap();
    assert_eq!(read(&scratch.0, &id, 5, 1, 100).unwrap().turns, vec![turn(4, "retry")]);
}

#[test]
fn failed_rollback_preserves_original_error_and_repairs_before_next_append() {
    for rotate in [false, true] {
        let scratch = Scratch::new();
        let id = id();
        let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
        writer.append(&[turn(0, "kept")]).unwrap();
        if rotate {
            writer.file.set_len(SEGMENT_BYTES - 10).unwrap();
            writer.bytes = SEGMENT_BYTES - 10;
        }
        writer.write_fault = Some((2, true));
        writer.fail_rollback = true;
        let err = writer
            .append(&[turn(1, "failed one"), turn(2, "failed two")])
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        writer.append(&[turn(3, "after repair")]).unwrap();
        let bytes = std::fs::read(id.path(&writer.dir, writer.k)).unwrap();
        let lines: Vec<Line> = bytes
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).expect("complete separate JSON line"))
            .collect();
        assert_eq!(lines.last().unwrap().turn, 3);
        assert!(read(&scratch.0, &id, 3, 10, 100).is_none());
        assert_eq!(
            read(&scratch.0, &id, 4, 10, 100).unwrap().turns,
            vec![turn(3, "after repair")]
        );
    }
}

#[test]
fn metadata_heavy_record_uses_minimal_cut_instead_of_failing_forever() {
    let scratch = Scratch::new();
    let id = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &id, true).unwrap();
    let huge_key = "k".repeat(super::ARCHIVE_MAX_LINE_BYTES + 1);
    let record: ArchivedRecord = serde_json::from_value(json!({"kind":"tool_call","record":{
        "seq":1,"turn_id":"t","tool_use_id":"call","name":"Read","input":null,
        "changed_input":{"fields":{huge_key:"value"}}
    }}))
    .unwrap();
    writer
        .append(&[ArchivedTurn {
            turn: 0,
            records: vec![record],
        }])
        .unwrap();
    let page = read(&scratch.0, &id, 1, 10, 100).unwrap();
    assert_eq!(page.cut, vec![(0, "tool_call", 1)]);
    let ArchivedRecord::ToolCall(tool) = &page.turns[0].records[0] else {
        panic!()
    };
    assert_eq!(
        (&tool.turn_id, &tool.tool_use_id, tool.seq),
        (&"t".to_string(), &"call".to_string(), 1)
    );
    assert!(tool.changed_input.is_none());
    assert!(tool.name.is_empty());
    assert!(tool.input.is_null());
    assert!(tool.result.is_none());
    assert!(writer.bytes < super::ARCHIVE_MAX_LINE_BYTES as u64);
}

#[test]
fn opening_without_the_session_lease_preserves_other_generations() {
    let scratch = Scratch::new();
    let old = id();
    ArchiveWriter::open(&scratch.0, &old, true)
        .unwrap()
        .append(&[turn(0, "original")])
        .unwrap();
    let new = id();
    ArchiveWriter::open(&scratch.0, &new, false)
        .unwrap()
        .append(&[turn(1, "second")])
        .unwrap();
    assert_eq!(
        read(&scratch.0, &old, 1, 10, usize::MAX).unwrap().turns,
        vec![turn(0, "original")]
    );
    assert!(new.path(&scratch.0.join("conversation"), 0).exists());
}

#[test]
fn vanished_segments_are_harmless_during_open_trim_and_rollback() {
    let scratch = Scratch::new();
    let old = id();
    ArchiveWriter::open(&scratch.0, &old, true).unwrap();
    BEFORE_REMOVE.with(|hook| *hook.borrow_mut() = Some(Box::new(|path| std::fs::remove_file(path).unwrap())));
    let new = id();
    let mut writer = ArchiveWriter::open(&scratch.0, &new, true).unwrap();
    for k in 1..6 {
        append_private(&new.path(&writer.dir, k)).unwrap();
    }
    BEFORE_REMOVE.with(|hook| *hook.borrow_mut() = Some(Box::new(|path| std::fs::remove_file(path).unwrap())));
    writer.k = 5;
    writer.file = append_private(&new.path(&writer.dir, 5)).unwrap();
    writer.trim(None).unwrap();
    BEFORE_REMOVE.with(|hook| *hook.borrow_mut() = Some(Box::new(|path| std::fs::remove_file(path).unwrap())));
    writer.rollback((2, 0)).unwrap();
    assert_eq!(segments(&writer.dir, &new).unwrap().len(), 1);
}
