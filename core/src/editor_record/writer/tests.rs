use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::editor_record::{Leaf, Node, Tab, INPUT_FILE_NAME};
use crate::test_scratch_dir::ScratchDir;
use crate::window_link::pair::Role;

const T1: &str = "0123456789abcdef";
const T2: &str = "fedcba9876543210";
const LONG: Duration = Duration::from_secs(20);

fn root() -> PathBuf {
    PathBuf::from("/home/someone/project")
}

/// A record of one window showing `name`.
fn rec(name: &str) -> Record {
    Record {
        root: root().as_os_str().as_bytes().to_vec(),
        tab: Some(1),
        tabs: vec![Tab {
            cols: 200,
            lines: 48,
            tree: Node::Leaf(Leaf {
                file: format!("/home/someone/project/{name}").into_bytes(),
                lnum: 1,
                col: 0,
                want: 0,
                top: 1,
                left: 0,
                w: 200,
                h: 47,
                cur: true,
            }),
        }],
        bufs: Vec::new(),
    }
}

fn file_in(dir: &ScratchDir) -> RecordFile {
    RecordFile::for_window(Some(dir.as_os_str()), None, &root(), Role::Main).unwrap()
}

/// What the record file holds now, parsed; `None` when there is none.
fn on_disk(file: &RecordFile) -> Option<Record> {
    let text = std::fs::read_to_string(file.path()).ok()?;
    Some(crate::editor_record::parse(&text, &root()).unwrap())
}

fn written(answers: &[WriterAnswer]) -> Vec<u64> {
    answers
        .iter()
        .filter_map(|a| match a {
            WriterAnswer::Written { seq } => Some(*seq),
            _ => None,
        })
        .collect()
}

fn handoffs(answers: &[WriterAnswer]) -> Vec<(String, Option<Record>)> {
    answers
        .iter()
        .filter_map(|a| match a {
            WriterAnswer::Handoff { token, record } => Some((token.clone(), record.clone())),
            _ => None,
        })
        .collect()
}

/// A handoff file as nvim leaves it, in `dir`.
fn put_handoff(dir: &Path, token: &str, record: &Record, same: bool) -> PathBuf {
    let path = dir.join(format!("reopen-out-{token}.json"));
    std::fs::write(&path, format!("{{\"same\":{same},\"record\":{}}}", record.to_json())).unwrap();
    path
}

fn channel(dir: &ScratchDir) -> PathBuf {
    let channel = dir.join("channel");
    std::fs::create_dir_all(&channel).unwrap();
    channel
}

#[derive(Default)]
struct CountWaker(AtomicUsize);

impl crate::wm::Waker for CountWaker {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn an_older_seq_never_replaces_a_newer() {
    let dir = ScratchDir::new("nv-record-writer", "older");
    let file = file_in(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.offer(5, rec("new.rs"), false);
    assert!(writer.flush(LONG));
    writer.offer(3, rec("old.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), Some(rec("new.rs")));
    assert_eq!(written(&writer.take_answers()), [5]);
}

#[test]
fn an_equal_record_is_not_rewritten() {
    let dir = ScratchDir::new("nv-record-writer", "equal");
    let file = file_in(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.offer(1, rec("a.rs"), false);
    writer.offer(2, rec("a.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(written(&writer.take_answers()), [1]);

    // The record read at start is what the file holds: the same again is not written either.
    let seeded = RecordWriter::start(file.clone(), root(), Some(rec("a.rs")));
    seeded.offer(1, rec("a.rs"), false);
    assert!(seeded.flush(LONG));
    assert_eq!(written(&seeded.take_answers()), Vec::<u64>::new());
}

#[test]
fn same_as_restored_is_skipped_until_the_first_write_then_written() {
    let dir = ScratchDir::new("nv-record-writer", "restored");
    let file = file_in(&dir);
    file.write(&rec("morning.rs")).unwrap();
    let writer = RecordWriter::start(file.clone(), root(), Some(rec("morning.rs")));
    writer.begin_session(T1.to_string(), false);
    writer.offer(1, rec("a.rs"), true);
    assert!(writer.flush(LONG));
    assert_eq!(
        on_disk(&file),
        Some(rec("morning.rs")),
        "a partial restore leaves the fuller record"
    );
    writer.offer(2, rec("b.rs"), false);
    writer.offer(3, rec("a.rs"), true);
    assert!(writer.flush(LONG));
    assert_eq!(written(&writer.take_answers()), [2, 3]);
    assert_eq!(on_disk(&file), Some(rec("a.rs")), "back to A after B is written");

    // A new nvim starts again from "nothing written for it".
    writer.begin_session(T2.to_string(), false);
    writer.offer(4, rec("c.rs"), true);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), Some(rec("a.rs")));
}

#[test]
fn an_empty_handoff_after_closing_is_written() {
    let dir = ScratchDir::new("nv-record-writer", "empty");
    let file = file_in(&dir);
    let channel = channel(&dir);
    file.write(&rec("a.rs")).unwrap();
    let writer = RecordWriter::start(file.clone(), root(), Some(rec("a.rs")));
    writer.begin_session(T1.to_string(), false);
    let path = put_handoff(&channel, T1, &Record::empty(&root()), false);
    writer.read_handoff(7, T1.to_string(), path.clone(), LONG);
    assert!(writer.flush(LONG));
    let answers = writer.take_answers();
    assert_eq!(handoffs(&answers), [(T1.to_string(), Some(Record::empty(&root())))]);
    assert_eq!(written(&answers), [7]);
    assert_eq!(
        on_disk(&file),
        Some(Record::empty(&root())),
        "the person closed everything"
    );
    assert!(!path.exists(), "the handoff is removed once read");

    // A start that restored nothing, quit untouched, leaves the record alone.
    let untouched = ScratchDir::new("nv-record-writer", "untouched");
    let file = file_in(&untouched);
    let channel = self::channel(&untouched);
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.begin_session(T1.to_string(), false);
    let path = put_handoff(&channel, T1, &Record::empty(&root()), true);
    writer.read_handoff(1, T1.to_string(), path, LONG);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), None);
}

#[test]
fn a_handoff_newer_than_a_late_timed_answer_wins() {
    let dir = ScratchDir::new("nv-record-writer", "late");
    let file = file_in(&dir);
    let channel = channel(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.begin_session(T1.to_string(), false);
    let path = put_handoff(&channel, T1, &rec("at-exit.rs"), false);
    writer.read_handoff(8, T1.to_string(), path, LONG);
    assert!(writer.flush(LONG));
    // The capture asked before the exit answers only now.
    writer.offer(7, rec("timed.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), Some(rec("at-exit.rs")));
    assert_eq!(writer.newest(), Some(rec("at-exit.rs")));
}

#[test]
fn a_late_timed_answer_never_replaces_a_skipped_handoff() {
    let dir = ScratchDir::new("nv-record-writer", "skipped");
    let file = file_in(&dir);
    let channel = channel(&dir);
    file.write(&rec("morning.rs")).unwrap();
    let writer = RecordWriter::start(file.clone(), root(), Some(rec("morning.rs")));
    writer.begin_session(T1.to_string(), false);
    // The exit's record is the restore's own, so nothing is written for it ...
    let path = put_handoff(&channel, T1, &rec("restored.rs"), true);
    writer.read_handoff(8, T1.to_string(), path, LONG);
    // ... and a capture asked before it, answering after, is older than what the exit said.
    writer.offer(7, rec("moved-away.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), Some(rec("morning.rs")));
}

#[test]
fn a_handoff_that_never_appears_answers_none_after_its_wait() {
    let dir = ScratchDir::new("nv-record-writer", "never");
    let file = file_in(&dir);
    let channel = channel(&dir);
    let writer = RecordWriter::start(file.clone(), root(), Some(rec("a.rs")));
    let waited = Duration::from_millis(200);
    let asked = Instant::now();
    writer.read_handoff(1, T1.to_string(), channel.join(format!("reopen-out-{T1}.json")), waited);
    assert!(writer.flush(LONG));
    assert!(asked.elapsed() >= waited, "it waits for the file");
    assert_eq!(handoffs(&writer.take_answers()), [(T1.to_string(), None)]);
    assert_eq!(on_disk(&file), None, "the timed record, here none, stands");
}

#[test]
fn a_second_read_of_one_token_waits_for_nothing() {
    let dir = ScratchDir::new("nv-record-writer", "second");
    let file = file_in(&dir);
    let channel = channel(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    let path = channel.join(format!("reopen-out-{T1}.json"));
    writer.read_handoff(1, T1.to_string(), path.clone(), Duration::ZERO);
    assert!(writer.flush(LONG));
    put_handoff(&channel, T1, &rec("a.rs"), false);
    writer.read_handoff(2, T1.to_string(), path.clone(), LONG);
    assert!(writer.flush(LONG));
    assert_eq!(handoffs(&writer.take_answers()), [(T1.to_string(), None)]);
    assert!(path.exists(), "the second call read nothing");
    assert_eq!(on_disk(&file), None);
}

#[test]
fn a_handoff_symlink_is_refused() {
    let dir = ScratchDir::new("nv-record-writer", "symlink");
    let file = file_in(&dir);
    let channel = channel(&dir);
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let target = put_handoff(&elsewhere, T1, &rec("planted.rs"), false);
    let path = channel.join(format!("reopen-out-{T1}.json"));
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.read_handoff(1, T1.to_string(), path.clone(), LONG);
    assert!(writer.flush(LONG));
    assert_eq!(handoffs(&writer.take_answers()), [(T1.to_string(), None)]);
    assert_eq!(on_disk(&file), None);
    assert!(target.exists(), "what the link points at is left alone");
}

#[test]
fn a_failure_on_a_read_only_dir_is_reported_once_and_the_old_file_stays() {
    let dir = ScratchDir::new("nv-record-writer", "failing");
    let file = file_in(&dir);
    file.write(&rec("old.rs")).unwrap();
    let before = std::fs::read(file.path()).unwrap();
    // Every write sets the state directory to 0700 again, so a mode bit cannot keep it from being written; a
    // directory standing at the temporary name fails the write the way a full or read-only disk does.
    let blocked = crate::state_paths::temporary(file.path());
    std::fs::create_dir(&blocked).unwrap();
    let writer = RecordWriter::start(file.clone(), root(), Some(rec("old.rs")));
    writer.offer(1, rec("b.rs"), false);
    writer.offer(2, rec("b.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(
        writer.failures_reported(),
        1,
        "one report however often the same record fails"
    );
    assert_eq!(
        std::fs::read(file.path()).unwrap(),
        before,
        "the record on disk is untouched"
    );
    assert_eq!(
        written(&writer.take_answers()),
        Vec::<u64>::new(),
        "a failure answers nothing"
    );
    writer.offer(3, rec("c.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(
        writer.failures_reported(),
        2,
        "a different record that fails is news again"
    );
    std::fs::remove_dir(&blocked).unwrap();
    writer.offer(4, rec("c.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), Some(rec("c.rs")));
    assert_eq!(written(&writer.take_answers()), [4]);
}

#[test]
fn flush_returns_true_when_the_queue_drains_and_false_at_its_bound() {
    let dir = ScratchDir::new("nv-record-writer", "flush");
    let file = file_in(&dir);
    let channel = channel(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.offer(1, rec("a.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), Some(rec("a.rs")));
    writer.read_handoff(2, T1.to_string(), channel.join(format!("reopen-out-{T1}.json")), LONG);
    assert!(!writer.flush(Duration::from_millis(50)), "a handoff still awaited");
    put_handoff(&channel, T1, &rec("b.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(on_disk(&file), Some(rec("b.rs")));
}

#[test]
fn newest_is_the_seed_until_an_offer_then_the_highest_seq() {
    let dir = ScratchDir::new("nv-record-writer", "newest");
    let file = file_in(&dir);
    assert_eq!(RecordWriter::start(file.clone(), root(), None).newest(), None);
    let writer = RecordWriter::start(file.clone(), root(), Some(rec("seed.rs")));
    assert_eq!(writer.newest(), Some(rec("seed.rs")));
    writer.offer(3, rec("a.rs"), true);
    assert_eq!(writer.newest(), Some(rec("a.rs")), "handed over, written or not");
    writer.offer(2, rec("b.rs"), false);
    assert_eq!(writer.newest(), Some(rec("a.rs")));
    writer.offer(4, rec("c.rs"), false);
    assert_eq!(writer.newest(), Some(rec("c.rs")));
}

#[test]
fn a_write_clears_the_marker_and_a_skip_session_clears_it_only_on_its_first_write() {
    let dir = ScratchDir::new("nv-record-writer", "marker");
    let file = file_in(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.begin_session(T1.to_string(), false);
    writer.set_marker(T1.to_string());
    assert!(writer.flush(LONG));
    assert_eq!(file.marker(), Some(T1.to_string()));
    writer.offer(1, rec("a.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(file.marker(), None);

    // The last try did not finish; this nvim skipped its restore.
    file.set_marker(T1).unwrap();
    writer.begin_session(T2.to_string(), true);
    writer.clear_marker_if(T1.to_string());
    writer.offer(2, rec("a.rs"), false);
    writer.offer(3, rec("restored.rs"), true);
    assert!(writer.flush(LONG));
    assert_eq!(file.marker(), Some(T1.to_string()), "nothing written yet");
    writer.offer(4, rec("b.rs"), false);
    assert!(writer.flush(LONG));
    assert_eq!(file.marker(), None);
    assert_eq!(on_disk(&file), Some(rec("b.rs")));
}

#[test]
fn clear_marker_if_leaves_another_tokens_marker() {
    let dir = ScratchDir::new("nv-record-writer", "other-marker");
    let file = file_in(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    writer.set_marker(T1.to_string());
    writer.clear_marker_if(T2.to_string());
    assert!(writer.flush(LONG));
    assert_eq!(file.marker(), Some(T1.to_string()));
    writer.clear_marker_if(T1.to_string());
    assert!(writer.flush(LONG));
    assert_eq!(file.marker(), None);
}

#[test]
fn write_input_uses_newest_and_answers() {
    let dir = ScratchDir::new("nv-record-writer", "input");
    let file = file_in(&dir);
    let channel = channel(&dir);
    let writer = RecordWriter::start(file.clone(), root(), Some(rec("seed.rs")));
    let waker = Arc::new(CountWaker::default());
    writer.set_waker(waker.clone());
    // Queued behind the handoff, so it holds what the handoff read.
    let path = put_handoff(&channel, T1, &rec("at-exit.rs"), true);
    writer.read_handoff(1, T1.to_string(), path, LONG);
    let prefixes = vec![PathBuf::from("/home/someone/project/target")];
    writer.write_input(channel.clone(), T2.to_string(), prefixes, false);
    assert!(writer.flush(LONG));
    let answers = writer.take_answers();
    assert!(answers.contains(&WriterAnswer::InputWritten { token: T2.to_string() }));
    assert!(
        waker.0.load(Ordering::SeqCst) >= 1,
        "the controller is woken for its answers"
    );
    let input: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(channel.join(INPUT_FILE_NAME)).unwrap()).unwrap();
    assert_eq!(input["token"], T2);
    assert_eq!(input["skip"], false);
    let want: serde_json::Value = serde_json::from_str(&rec("at-exit.rs").to_json()).unwrap();
    assert_eq!(input["record"], want);
}

/// The restarted nvim would read the ended one's input, with a token the window no longer hears and the record it
/// started from: when the new input cannot be written, the old one goes, so the new nvim finds none.
#[test]
fn a_failed_input_write_leaves_no_stale_input() {
    let dir = ScratchDir::new("nv-record-writer", "input-fails");
    let file = file_in(&dir);
    let channel = channel(&dir);
    let writer = RecordWriter::start(file, root(), Some(rec("seed.rs")));
    writer.write_input(channel.clone(), T1.to_string(), Vec::new(), false);
    assert!(writer.flush(LONG));
    assert!(channel.join(INPUT_FILE_NAME).exists());
    // The temporary name is taken by a directory, so the write fails and leaves the old file in place.
    let blocker = channel.join(format!("{INPUT_FILE_NAME}.{}.tmp", std::process::id()));
    std::fs::create_dir(&blocker).unwrap();
    writer.write_input(channel.clone(), T2.to_string(), Vec::new(), false);
    assert!(writer.flush(LONG));
    std::fs::remove_dir(&blocker).unwrap();
    assert!(
        !channel.join(INPUT_FILE_NAME).exists(),
        "the ended nvim's input is not left for the new one"
    );
    assert!(writer
        .take_answers()
        .contains(&WriterAnswer::InputWritten { token: T2.to_string() }));
}

#[test]
fn clones_are_one_writer() {
    let dir = ScratchDir::new("nv-record-writer", "clones");
    let file = file_in(&dir);
    let writer = RecordWriter::start(file.clone(), root(), None);
    let other = RecordWriter::start(file, root(), None);
    assert_eq!(writer.clone(), writer);
    assert_ne!(writer, other);
}
