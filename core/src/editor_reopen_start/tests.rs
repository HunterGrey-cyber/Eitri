use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::editor_record::{Leaf, Node, Record, Tab};
use crate::test_scratch_dir::ScratchDir;

pub(crate) fn root() -> PathBuf {
    PathBuf::from("/home/someone/project")
}

/// A record of one window on `file`, for `root()`.
pub(crate) fn one_file(file: &str) -> Record {
    Record {
        root: root().as_os_str().as_bytes().to_vec(),
        tab: Some(1),
        tabs: vec![Tab {
            cols: 200,
            lines: 48,
            tree: Node::Leaf(Leaf {
                file: file.as_bytes().to_vec(),
                lnum: 3,
                col: 0,
                want: 0,
                top: 1,
                left: 0,
                w: 200,
                h: 48,
                cur: true,
            }),
        }],
        bufs: vec![],
    }
}

/// A state home and a channel directory under one scratch directory.
pub(crate) struct Place {
    pub(crate) dir: ScratchDir,
}

impl Place {
    pub(crate) fn new(label: &str) -> Place {
        let dir = ScratchDir::new("nv-reopen-start", label);
        let channel = dir.join("channel");
        std::fs::create_dir(&channel).unwrap();
        std::fs::set_permissions(&channel, std::fs::Permissions::from_mode(0o700)).unwrap();
        Place { dir }
    }

    pub(crate) fn channel(&self) -> PathBuf {
        self.dir.join("channel")
    }

    pub(crate) fn file(&self) -> RecordFile {
        RecordFile::for_window(Some(self.dir.as_os_str()), None, &root(), Role::Main).unwrap()
    }

    pub(crate) fn start(&self) -> ReopenStart {
        ReopenStart {
            enabled: true,
            state_home: Some(self.dir.as_os_str().to_owned()),
            home: None,
            role: Role::Main,
            channel_dir: Some(self.channel()),
            root: root(),
            prefixes: vec![self.channel().join(""), PathBuf::from("/state/eitri/")],
            started_with_files: false,
        }
    }

    pub(crate) fn input(&self) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(self.channel().join(crate::editor_reopen::REOPEN_IN)).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }
}

fn b64(path: &Path) -> String {
    crate::editor_record::encode_name(path.as_os_str().as_bytes())
}

#[test]
fn prepare_start_writes_the_input_with_a_fresh_token_and_seeds_the_writer() {
    let place = Place::new("fresh");
    let record = one_file("/home/someone/project/src/main.rs");
    place.file().write(&record).unwrap();
    let start = place.start();

    let prepared = prepare_start(&start);
    let token = prepared.token.clone().expect("a token");
    assert_eq!(token.len(), 16);
    assert!(!prepared.skip);
    assert_eq!(prepared.toast, None, "a record that reads back says nothing");
    let writer = prepared.writer.clone().expect("a writer");
    assert_eq!(
        writer.newest(),
        Some(record.clone()),
        "the writer starts from the record read"
    );

    let input = place.input().expect("nvim's input is on disk before nvim starts");
    assert_eq!(input["token"], token.as_str());
    assert_eq!(input["skip"], false);
    assert_eq!(input["root"], b64(&root()).as_str());
    assert_eq!(
        input["prefixes"],
        serde_json::json!([b64(&start.prefixes[0]), b64(&start.prefixes[1])])
    );
    let written = crate::editor_record::parse(&input["record"].to_string(), &root()).unwrap();
    assert_eq!(written, record, "the input's record is the one read, re-serialised");
    let mode = std::fs::metadata(place.channel().join(crate::editor_reopen::REOPEN_IN))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // Each start's nvim gets its own token.
    let again = prepare_start(&start);
    assert_ne!(again.token, prepared.token);
    assert_eq!(place.input().unwrap()["token"], again.token.unwrap().as_str());
    assert!(writer.flush(Duration::from_secs(5)));
}

#[test]
fn prepare_start_writes_nothing_when_off_clean_second_or_with_files() {
    let place = Place::new("off");
    // A file that would be set aside if read, and a marker that would be read: both must stay as they are.
    let file = place.file();
    crate::state_paths::create_state_dir(file.path().parent().unwrap()).unwrap();
    agent::private_fs::write_private(file.path(), b"{\"v\":1, not a record").unwrap();
    file.set_marker("0123456789abcdef").unwrap();
    let base = place.start();
    for (why, start) in [
        (
            "off or --clean",
            ReopenStart {
                enabled: false,
                ..base.clone()
            },
        ),
        (
            "window 2",
            ReopenStart {
                role: Role::Second,
                ..base.clone()
            },
        ),
        (
            "started with files",
            ReopenStart {
                started_with_files: true,
                ..base.clone()
            },
        ),
        (
            "no pane-switch channel",
            ReopenStart {
                channel_dir: None,
                ..base.clone()
            },
        ),
    ] {
        let prepared = prepare_start(&start);
        assert_eq!(prepared.token, None, "{why}");
        assert!(prepared.writer.is_none(), "{why}");
        assert_eq!(prepared.toast, None, "{why}");
        assert!(place.input().is_none(), "{why}: no input written");
        assert_eq!(
            std::fs::read(file.path()).unwrap(),
            b"{\"v\":1, not a record",
            "{why}: the record is not read"
        );
        assert_eq!(file.marker().as_deref(), Some("0123456789abcdef"), "{why}");
    }
    assert!(prepare_start(&base).token.is_some(), "the same start, on, does reopen");
}

#[test]
fn a_marker_makes_the_input_skip_and_warns_once() {
    let place = Place::new("marker");
    let record = one_file("/home/someone/project/a.rs");
    let file = place.file();
    file.write(&record).unwrap();
    file.set_marker("0123456789abcdef").unwrap();

    let prepared = prepare_start(&place.start());
    assert!(prepared.skip);
    assert_eq!(
        prepared.toast.as_deref(),
        Some(crate::wm::feedback::reopen_unfinished_text().as_str()),
        "one line, the unfinished restore's"
    );
    let input = place.input().unwrap();
    assert_eq!(input["skip"], true);
    assert_eq!(
        input["record"],
        serde_json::Value::Null,
        "nothing is restored this time"
    );
    assert_eq!(input["token"], prepared.token.clone().unwrap().as_str());
    let writer = prepared.writer.clone().unwrap();
    assert_eq!(
        writer.newest(),
        Some(record),
        "the record stays the newest until another is taken"
    );
    assert!(writer.flush(Duration::from_secs(5)));
    assert_eq!(
        file.marker().as_deref(),
        Some("0123456789abcdef"),
        "the marker stays until a record is written"
    );
}

#[test]
fn a_record_that_does_not_read_is_set_aside_and_said() {
    let place = Place::new("aside");
    let file = place.file();
    crate::state_paths::create_state_dir(file.path().parent().unwrap()).unwrap();
    agent::private_fs::write_private(file.path(), b"{\"v\":1, nope").unwrap();

    let prepared = prepare_start(&place.start());
    assert!(prepared.token.is_some());
    assert!(!prepared.skip);
    assert_eq!(
        prepared.toast.as_deref(),
        Some(crate::wm::feedback::reopen_set_aside_text().as_str())
    );
    assert!(!file.path().exists());
    assert!(file.path().with_extension("json.unusable").exists());
    assert_eq!(place.input().unwrap()["record"], serde_json::Value::Null);
}

#[test]
fn the_close_reads_the_last_record_then_waits_for_the_writer() {
    let place = Place::new("close-record");
    let prepared = prepare_start(&place.start());
    let (writer, token) = (prepared.writer.unwrap(), prepared.token.unwrap());
    let record = one_file("/home/someone/project/z.rs");
    let path = place.channel().join(crate::editor_reopen::handoff_name(&token));
    let json = format!("{{\"same\":false,\"record\":{}}}", record.to_json());
    agent::private_fs::write_private(&path, json.as_bytes()).unwrap();
    assert!(close_record(&writer, &place.channel(), Some(&token), 1));
    assert_eq!(
        std::fs::read_to_string(place.file().path()).unwrap(),
        record.to_json(),
        "on disk before the close goes on to remove the channel directory"
    );
    assert!(!path.exists(), "the handoff is removed once read");
    assert_eq!(
        writer.take_answers(),
        vec![
            crate::editor_record::WriterAnswer::Written { seq: 1 },
            crate::editor_record::WriterAnswer::Handoff {
                token: token.clone(),
                record: Some(record)
            }
        ]
    );
    // Nothing to read: only the wait for the writer.
    assert!(close_record(&writer, &place.channel(), None, 2));
    assert!(writer.take_answers().is_empty());
}

#[test]
fn a_set_aside_record_and_a_marker_say_both() {
    let place = Place::new("aside-and-marker");
    let file = place.file();
    crate::state_paths::create_state_dir(file.path().parent().unwrap()).unwrap();
    agent::private_fs::write_private(file.path(), b"{\"v\":1, nope").unwrap();
    file.set_marker("0123456789abcdef").unwrap();

    let prepared = prepare_start(&place.start());
    assert!(prepared.skip);
    let toast = prepared.toast.expect("a toast");
    assert!(toast.contains(&crate::wm::feedback::reopen_set_aside_text()), "{toast}");
    assert!(
        toast.contains(&crate::wm::feedback::reopen_unfinished_text()),
        "{toast}"
    );
}

#[test]
fn drop_input_removes_the_input_and_a_missing_one_is_fine() {
    let place = Place::new("drop-input");
    let _prepared = prepare_start(&place.start());
    assert!(place.input().is_some());
    drop_input(&place.channel());
    assert!(place.input().is_none());
    drop_input(&place.channel());
    assert!(place.input().is_none());
}
