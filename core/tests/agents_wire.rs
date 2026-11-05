use eitri_core::agents::wire::{change_json, cut, parse_process_file, process_file_json, view_json, WireError};
use eitri_core::agents::*;

fn row() -> Row {
    Row {
        id: "a3f09c1d2e4b5f60.7".into(),
        key: "k-5c1e0b9a7d3f2e18".into(),
        agent: "claude".into(),
        project: ProjectRef {
            key: "9b2c4e7a10f3d856".into(),
            name: "eitri".into(),
        },
        window: WindowRef {
            instance: "a3f09c1d2e4b5f60".into(),
            name: "1".into(),
            focused: true,
        },
        number: 3,
        label: "close question".into(),
        label_from: LabelFrom::Title,
        shown: false,
        state: "card".into(),
        class: Class::Needs,
        since: 1793865600123,
        needs: Needs {
            cards: 1,
            questions: 0,
            trust: false,
        },
        waiting_since: Some(1793865600123),
        unread: false,
        tool: Some("Bash".into()),
        background: Background::default(),
        queued: 0,
        mode: "manual".into(),
        ended_how: None,
        moving: false,
    }
}
fn view() -> View {
    View {
        at: 1793865600200,
        enabled: eitri_core::agent_prefs::AGENTS_ENABLED_DEFAULT,
        projects: vec![row().project],
        rows: vec![row()],
        ..Default::default()
    }
}
fn file() -> ProcessFile {
    ProcessFile {
        v: SCHEMA_V,
        instance: row().window.instance,
        pid: 123,
        pid_start: 456,
        project: row().project,
        window: row().window,
        rows: vec![row()],
        departed: vec![Departed {
            id: "a3f09c1d2e4b5f60.6".into(),
            key: "k-0000000000000001".into(),
            why: GoneWhy::Moved,
            at: 1793865600100,
        }],
        truncated: false,
    }
}

#[test]
fn fixtures_match_byte_for_byte() {
    for (actual, expected) in [
        (
            serde_json::to_string(&row()).unwrap(),
            include_str!("../src/agents/fixtures/row.json"),
        ),
        (
            view_json(&view(), SCHEMA_V),
            include_str!("../src/agents/fixtures/view.json"),
        ),
        (
            change_json(&Change::Row { row: row() }, SCHEMA_V),
            include_str!("../src/agents/fixtures/change_row.json"),
        ),
        (
            change_json(
                &Change::Gone {
                    id: row().id,
                    key: row().key,
                    why: GoneWhy::Closed,
                },
                SCHEMA_V,
            ),
            include_str!("../src/agents/fixtures/change_gone.json"),
        ),
        (
            change_json(&Change::Reset { view: view() }, SCHEMA_V),
            include_str!("../src/agents/fixtures/change_reset.json"),
        ),
        (
            String::from_utf8(process_file_json(&file())).unwrap(),
            include_str!("../src/agents/fixtures/process_file.json"),
        ),
    ] {
        assert_eq!(actual, expected);
        assert!(!actual.contains('\n'));
    }
    assert_eq!(parse_process_file(&process_file_json(&file())).unwrap(), file());
}

#[test]
fn field_order_is_pinned() {
    let text = serde_json::to_string(&row()).unwrap();
    let keys = [
        "id",
        "key",
        "agent",
        "project",
        "window",
        "number",
        "label",
        "label_from",
        "shown",
        "state",
        "class",
        "since",
        "needs",
        "waiting_since",
        "unread",
        "tool",
        "background",
        "queued",
        "mode",
        "ended_how",
        "moving",
    ];
    let mut tail = text.as_str();
    let mut at = 0;
    for key in keys {
        let marker = format!("\"{key}\":");
        let index = tail
            .find(&marker)
            .unwrap_or_else(|| panic!("key {key} missing or out of order in {text}"));
        at += index;
        assert!(text[at..].starts_with(&marker), "key {key} out of order");
        tail = &tail[index + marker.len()..];
        at += marker.len();
    }
}

#[test]
fn a_row_with_an_unknown_field_and_state_parses() {
    let mut value = serde_json::to_value(file()).unwrap();
    value["rows"][0]["state"] = "paused".into();
    value["rows"][0]["class"] = "working".into();
    value["rows"][0]["future"] = true.into();
    let parsed = parse_process_file(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(State::parse(&parsed.rows[0].state), None);
    assert_eq!(parsed.rows[0].class, Class::Working);
    assert!(serde_json::from_str::<Row>("{}").is_ok());
}

#[test]
fn a_newer_major_is_refused_by_number() {
    let bytes = format!("{{\"v\":{},\"future\":true}}", SCHEMA_V + 1);
    assert!(matches!(parse_process_file(bytes.as_bytes()), Err(WireError::NewerMajor(v)) if v == SCHEMA_V + 1));
    assert!(matches!(
        parse_process_file(&vec![b' '; MAX_FILE_BYTES + 1]),
        Err(WireError::TooLarge)
    ));
    assert!(matches!(parse_process_file(b"{"), Err(WireError::Json(_))));
}

#[test]
fn a_foreign_label_is_cut_again_on_read() {
    let mut value = serde_json::to_value(file()).unwrap();
    value["rows"][0]["label"] = format!("\u{202e}{}", "界".repeat(LABEL_MAX)).into();
    let parsed = parse_process_file(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        parsed.rows[0].label,
        format!("\\u{{202e}}{}", "界".repeat(LABEL_MAX - 8))
    );
    assert_eq!(parsed.rows[0].label.chars().count(), LABEL_MAX);
}

#[test]
fn cut_never_splits_an_escape() {
    let prefix = "x".repeat(LABEL_MAX - 3);
    assert_eq!(cut(&format!("{prefix}\\u{{202e}}"), LABEL_MAX), prefix);
    for (raw, escaped) in [("\u{202e}", "\\u{202e}"), ("\u{200b}", "\\u{200b}"), ("\n", "\\u{a}")] {
        for max in 0..escaped.len() {
            assert_eq!(cut(raw, max), "");
        }
        assert_eq!(cut(raw, escaped.len()), escaped);
        assert_eq!(cut(&format!("界{raw}z"), escaped.len() + 1), format!("界{escaped}"));
    }
}

#[test]
fn a_newer_writers_new_words_in_closed_sets_do_not_drop_the_window() {
    let mut value = serde_json::to_value(file()).unwrap();
    value["rows"][0]["ended_how"] = "evicted".into();
    value["rows"][0]["label_from"] = "history".into();
    value["rows"][0]["number"] = 70000.into();
    value["departed"] = serde_json::json!([{"id": "a", "key": "k", "why": "evicted", "at": 3}]);
    let parsed = parse_process_file(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(parsed.rows[0].ended_how, None);
    assert_eq!(parsed.rows[0].label_from, LabelFrom::New);
    assert_eq!(parsed.rows[0].number, 0);
    assert_eq!(parsed.departed[0].why, GoneWhy::Closed);
    assert_eq!(parsed.departed[0].id, "a");
}

#[test]
fn a_known_ended_how_still_reads() {
    let mut value = serde_json::to_value(file()).unwrap();
    value["rows"][0]["ended_how"] = "tripped".into();
    let parsed = parse_process_file(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(parsed.rows[0].ended_how, Some(EndedHow::Tripped));
}

#[test]
fn a_file_past_the_row_limit_keeps_its_first_rows_and_says_truncated() {
    let mut value = serde_json::to_value(file()).unwrap();
    let first = value["rows"][0].clone();
    value["rows"] = serde_json::Value::Array(vec![first; MAX_ROWS_PER_FILE + 5]);
    let parsed = parse_process_file(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(parsed.rows.len(), MAX_ROWS_PER_FILE);
    assert!(parsed.truncated);
}
