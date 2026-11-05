//! The selection and the round every face of every window shares: two small private files beside the row files,
//! the order the round walks in, and which process moves it on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use eitri_core::agents::shared::{
    advance, round_order, should_advance, start_at, Round, RoundPoint, Selection, SharedStore, ROUND_FILE,
    SELECTION_FILE,
};
use eitri_core::agents::wire::change_json;
use eitri_core::agents::*;

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ash").join(format!(
            "{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn control(&self) -> PathBuf {
        self.0.join("eitri")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn project() -> ProjectRef {
    ProjectRef {
        key: "9b2c4e7a10f3d856".into(),
        name: "eitri".into(),
    }
}

fn window(instance: &str) -> WindowRef {
    WindowRef {
        instance: instance.into(),
        name: "1".into(),
        focused: true,
    }
}

/// A quiet row whose key belongs to its writer, so rows of two writers never share a key.
fn quiet(instance: &str, number: u16) -> Row {
    Row {
        id: format!("{instance}.{number}"),
        key: format!("k-{}{number:08x}", &instance[..8]),
        agent: "claude".into(),
        project: project(),
        window: window(instance),
        number,
        label: format!("tab {number}"),
        state: "idle".into(),
        class: Class::Quiet,
        mode: "manual".into(),
        ..Default::default()
    }
}

/// The same row with a card that has waited since `since`.
fn card(instance: &str, number: u16, since: u64) -> Row {
    Row {
        state: "card".into(),
        class: Class::Needs,
        needs: Needs {
            cards: 1,
            questions: 0,
            trust: false,
        },
        waiting_since: Some(since),
        ..quiet(instance, number)
    }
}

/// The same row waiting on the workspace trust prompt since `since`.
fn trust(instance: &str, number: u16, since: u64) -> Row {
    Row {
        state: "trust".into(),
        needs: Needs {
            cards: 0,
            questions: 0,
            trust: true,
        },
        ..card(instance, number, since)
    }
}

fn view(rows: Vec<Row>) -> View {
    View {
        rows,
        ..View::default()
    }
}

fn here(instance: &str) -> RoundPoint {
    RoundPoint {
        instance: instance.into(),
        module: "agent".into(),
        tab: Some(1),
    }
}

const A: &str = "a3f09c1d2e4b5f60";
const B: &str = "b7e21f0c3d4a5968";

#[test]
fn a_round_started_at_a_key_puts_it_first() {
    let oldest = card(A, 1, 100);
    let middle = card(B, 2, 200);
    let newest = card(A, 3, 300);
    let v = view(vec![newest.clone(), oldest.clone(), middle.clone(), quiet(B, 4)]);

    assert_eq!(
        round_order(&v, None),
        vec![oldest.key.clone(), middle.key.clone(), newest.key.clone()],
        "the rows that need the person, oldest wait first; a quiet row is no stop"
    );

    let r = start_at(&v, here(A), Some(&newest.key), 1_000).expect("a waiting row starts a round");
    assert_eq!(r.current.as_deref(), Some(newest.key.as_str()));
    assert_eq!(r.visited, vec![newest.key.clone()]);
    assert_eq!(r.from, here(A));
    assert_eq!((r.started_at_ms, r.at_ms), (1_000, 1_000));
    assert_eq!(
        round_order(&v, Some(&r)),
        vec![oldest.key.clone(), middle.key.clone(), newest.key.clone()],
        "a visited stop goes last"
    );
    let next = advance(&v, &r, 2_000).expect("two stops are left");
    assert_eq!(
        next.current.as_deref(),
        Some(oldest.key.as_str()),
        "then the oldest wait"
    );
    assert_eq!(next.visited, vec![newest.key.clone(), oldest.key.clone()]);
    assert_eq!((next.started_at_ms, next.at_ms), (1_000, 2_000));
    assert_eq!(next.from, here(A), "the start point stays for going back");

    let r = start_at(&v, here(B), None, 1_000).expect("a waiting row starts a round");
    assert_eq!(
        r.current.as_deref(),
        Some(oldest.key.as_str()),
        "with no key, the oldest wait"
    );

    assert_eq!(
        start_at(&v, here(A), Some(&quiet(B, 4).key), 1_000),
        None,
        "a row that waits for nothing is no stop to start at"
    );
    assert_eq!(start_at(&v, here(A), Some("k-nowhere"), 1_000), None);
}

#[test]
fn a_card_arriving_mid_round_joins_in_its_place() {
    let first = card(A, 1, 100);
    let later = card(B, 2, 300);
    let r = start_at(&view(vec![first.clone(), later.clone()]), here(A), None, 1_000).unwrap();
    assert_eq!(r.current.as_deref(), Some(first.key.as_str()));

    // A row whose wait began between the two (a window that opened during the round) joins ahead of the later.
    let between = card(A, 3, 200);
    let newest = card(B, 4, 900);
    let v = view(vec![newest.clone(), later.clone(), between.clone(), first.clone()]);
    assert_eq!(
        round_order(&v, Some(&r)),
        vec![
            between.key.clone(),
            later.key.clone(),
            newest.key.clone(),
            first.key.clone()
        ]
    );
    let r = advance(&v, &r, 1_100).unwrap();
    assert_eq!(r.current.as_deref(), Some(between.key.as_str()));
    let r = advance(&v, &r, 1_200).unwrap();
    assert_eq!(r.current.as_deref(), Some(later.key.as_str()));
    let r = advance(&v, &r, 1_300).unwrap();
    assert_eq!(r.current.as_deref(), Some(newest.key.as_str()));
    assert_eq!(advance(&v, &r, 1_400), None, "every stop has been visited");
}

#[test]
fn trust_rows_of_one_window_are_one_stop() {
    let one = trust(A, 1, 100);
    let two = trust(A, 2, 150);
    let other_window = trust(B, 3, 120);
    let a_card = card(A, 4, 200);
    let v = view(vec![one.clone(), two.clone(), other_window.clone(), a_card.clone()]);
    assert_eq!(
        round_order(&v, None),
        vec![one.key.clone(), other_window.key.clone(), a_card.key.clone()],
        "one answer settles a window's trust prompts, so they are one stop, at the oldest's place"
    );
    let r = start_at(&v, here(B), Some(&two.key), 1_000).expect("a trust row starts the round");
    assert_eq!(r.current.as_deref(), Some(two.key.as_str()));
    let r = advance(&v, &r, 1_100).unwrap();
    assert_eq!(
        r.current.as_deref(),
        Some(other_window.key.as_str()),
        "the stop the started row belongs to is visited"
    );
}

#[test]
fn a_visited_trust_stop_stays_visited_when_its_first_member_leaves() {
    let one = trust(A, 1, 100);
    let two = trust(A, 2, 150);
    let other_window = trust(B, 3, 120);
    let a_card = card(A, 4, 200);
    let v = view(vec![one.clone(), two.clone(), other_window.clone(), a_card.clone()]);
    let r = start_at(&v, here(B), None, 1_000).expect("the oldest trust row starts the round");
    assert_eq!(r.current.as_deref(), Some(one.key.as_str()));
    let r = advance(&v, &r, 1_100).unwrap();
    assert_eq!(r.current.as_deref(), Some(other_window.key.as_str()));

    // The window's first trust row closes; the other still waits on the same answer.
    let without_one = view(vec![two.clone(), other_window.clone(), a_card.clone()]);
    assert_eq!(
        round_order(&without_one, Some(&r)),
        vec![a_card.key.clone(), other_window.key.clone(), two.key.clone()],
        "the window's trust stop was visited, whichever of its rows is left"
    );
    let r = advance(&without_one, &r, 1_200).unwrap();
    assert_eq!(
        r.current.as_deref(),
        Some(a_card.key.as_str()),
        "the round does not come back to a trust stop it has visited"
    );
    assert_eq!(advance(&without_one, &r, 1_300), None, "every stop has been visited");
}

#[test]
fn a_round_with_none_left_ends() {
    assert_eq!(
        start_at(&view(vec![quiet(A, 1)]), here(A), None, 1_000),
        None,
        "nothing waits"
    );
    let only = card(A, 1, 100);
    let r = start_at(&view(vec![only.clone()]), here(A), None, 1_000).unwrap();
    assert_eq!(advance(&view(vec![only.clone()]), &r, 1_100), None);
    assert_eq!(
        advance(&view(vec![quiet(A, 1)]), &r, 1_100),
        None,
        "the one stop was answered"
    );
    assert_eq!(advance(&view(Vec::new()), &r, 1_100), None, "the one stop's tab closed");
}

#[test]
fn should_advance_needs_the_owner_and_a_row_that_left_needs() {
    let waiting = card(A, 1, 100);
    let r = start_at(&view(vec![waiting.clone(), card(B, 2, 200)]), here(B), None, 1_000).unwrap();
    let still = view(vec![waiting.clone(), card(B, 2, 200)]);
    assert!(!should_advance(A, &still, &r), "the current row still waits");
    let answered = view(vec![quiet(A, 1), card(B, 2, 200)]);
    assert!(should_advance(A, &answered, &r));
    assert!(
        !should_advance(B, &answered, &r),
        "only the window that owns the current row moves the round"
    );
    assert!(
        !should_advance(A, &view(vec![card(B, 2, 200)]), &r),
        "a row no window shows has no owner to move the round"
    );
    let ended = Round { current: None, ..r };
    assert!(!should_advance(A, &answered, &ended));
}

#[test]
fn the_change_fixtures_match_byte_for_byte() {
    let selection = Selection {
        key: "k-5c1e0b9a7d3f2e18".into(),
        by: A.into(),
        at_ms: 1793865600100,
    };
    let round = Round {
        from: RoundPoint {
            instance: A.into(),
            module: "agent".into(),
            tab: Some(7),
        },
        current: Some("k-5c1e0b9a7d3f2e18".into()),
        visited: vec!["k-5c1e0b9a7d3f2e18".into()],
        started_at_ms: 1793865500000,
        at_ms: 1793865600100,
    };
    let cases = [
        (
            Change::Selection {
                selection: Some(selection),
            },
            include_str!("../src/agents/fixtures/change_selection.json"),
        ),
        (
            Change::Round { round: Some(round) },
            include_str!("../src/agents/fixtures/change_round.json"),
        ),
    ];
    for (change, expected) in cases {
        let text = change_json(&change, SCHEMA_V);
        assert_eq!(text, expected);
        assert!(!text.contains('\n'));
        assert_eq!(serde_json::from_str::<Change>(&text).unwrap(), change);
    }
    assert_eq!(
        change_json(&Change::Round { round: None }, SCHEMA_V),
        r#"{"v":1,"type":"round","round":null}"#
    );
    assert_eq!(
        change_json(&Change::Selection { selection: None }, SCHEMA_V),
        r#"{"v":1,"type":"selection","selection":null}"#
    );
}

/// The reading half runs on Linux only, as the reader does.
#[cfg(target_os = "linux")]
mod files {
    use super::*;
    use agent::private_fs::publish_private_file;
    use eitri_core::agents::publish::{agents_dir, new_instance, Publisher};
    use eitri_core::agents::read::{scan_once, Reader, ReaderEvent, ReaderHooks};
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Long enough that only a reader that never answers runs out of it; nothing here measures speed.
    const PATIENCE: Duration = Duration::from_secs(60);

    /// What one process's face holds after applying every change in order.
    #[derive(Default)]
    struct Face {
        rows: BTreeMap<String, Row>,
        selection: Option<Selection>,
        round: Option<Round>,
    }

    impl Face {
        fn apply(&mut self, change: &Change) {
            match change {
                Change::Reset { view } => {
                    self.rows = view.rows.iter().map(|r| (r.id.clone(), r.clone())).collect();
                    self.selection = view.selection.clone();
                    self.round = view.round.clone();
                }
                Change::Row { row } => {
                    self.rows.insert(row.id.clone(), row.clone());
                }
                Change::Gone { id, .. } => {
                    self.rows.remove(id);
                }
                Change::Selection { selection } => self.selection = selection.clone(),
                Change::Round { round } => self.round = round.clone(),
            }
        }

        fn view(&self) -> View {
            View {
                rows: self.rows.values().cloned().collect(),
                selection: self.selection.clone(),
                round: self.round.clone(),
                ..View::default()
            }
        }
    }

    /// A watching reader standing for one process's faces.
    struct Watcher {
        reader: Option<Reader>,
        events: Receiver<ReaderEvent>,
        log: Arc<Mutex<Vec<String>>>,
        face: Face,
        first: View,
    }

    fn watch(control: &Path) -> Watcher {
        let (tx, events) = channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let hooks = ReaderHooks {
            log: {
                let log = log.clone();
                Box::new(move |line: &str| log.lock().unwrap().push(line.to_owned()))
            },
            ..ReaderHooks::default()
        };
        let reader = Reader::start_with(
            control,
            SCHEMA_V,
            move |e| {
                let _ = tx.send(e);
            },
            hooks,
        )
        .expect("the reader starts");
        let mut w = Watcher {
            reader: Some(reader),
            events,
            log,
            face: Face::default(),
            first: View::default(),
        };
        match w.next() {
            Change::Reset { view } => w.first = view,
            other => panic!("a reader starts with the whole view, not {other:?}"),
        }
        w
    }

    impl Watcher {
        fn next(&mut self) -> Change {
            match self.events.recv_timeout(PATIENCE).expect("the reader sent nothing") {
                ReaderEvent::Change(change) => {
                    self.face.apply(&change);
                    change
                }
                ReaderEvent::Error(e) => panic!("the reader failed: {e}"),
            }
        }

        fn until(&mut self, done: impl Fn(&Face) -> bool) -> Vec<Change> {
            let mut seen = Vec::new();
            while !done(&self.face) {
                seen.push(self.next());
            }
            seen
        }

        fn logged(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// Waits until the reader has logged a line containing `needle`: an ignored file sends no change, so its
        /// log line is the only sign the reader has read it.
        fn until_logged(&self, needle: &str) {
            let deadline = std::time::Instant::now() + PATIENCE;
            while !self.logged().iter().any(|l| l.contains(needle)) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the reader logged nothing about {needle}: {:#?}",
                    self.logged()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            if let Some(reader) = self.reader.take() {
                reader.stop();
            }
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn put(control: &Path, name: &str, bytes: &[u8]) {
        let dir = std::fs::File::open(agents_dir(control)).unwrap();
        publish_private_file(&dir, OsStr::new(name), bytes).unwrap();
    }

    fn a_round(current: &str) -> Round {
        Round {
            from: here(A),
            current: Some(current.into()),
            visited: vec![current.into()],
            started_at_ms: 1_000,
            at_ms: 1_000,
        }
    }

    #[test]
    fn a_selection_written_reaches_a_reader_as_selection() {
        let scratch = Scratch::new("selection");
        let control = scratch.control();
        let mut w = watch(&control);
        assert_eq!((w.first.selection.clone(), w.first.round.clone()), (None, None));
        let store = SharedStore::open(&control).unwrap();
        assert_eq!(store.read_selection(), None, "nothing selected yet");

        let first = Selection {
            key: "k-5c1e0b9a7d3f2e18".into(),
            by: A.into(),
            at_ms: 1_000,
        };
        store.write_selection(&first).unwrap();
        assert_eq!(store.read_selection(), Some(first.clone()));
        let changes = w.until(|f| f.selection.as_ref() == Some(&first));
        assert_eq!(
            changes,
            vec![Change::Selection {
                selection: Some(first.clone())
            }]
        );

        // Another process moves the cursor: the last writer wins.
        let second = Selection {
            key: "k-0000000000000002".into(),
            by: B.into(),
            at_ms: 2_000,
        };
        SharedStore::open(&control).unwrap().write_selection(&second).unwrap();
        assert_eq!(
            w.until(|f| f.selection.as_ref() == Some(&second)),
            vec![Change::Selection {
                selection: Some(second.clone())
            }]
        );
        assert_eq!(store.read_selection(), Some(second.clone()));
        assert_eq!(scan_once(&control, SCHEMA_V).unwrap().selection, Some(second.clone()));

        // A reader that starts now has both in its first view.
        let round = a_round("k-0000000000000002");
        store.write_round(Some(&round)).unwrap();
        w.until(|f| f.round.as_ref() == Some(&round));
        let late = watch(&control);
        assert_eq!(late.first.selection, Some(second));
        assert_eq!(late.first.round, Some(round.clone()));
        assert_eq!(scan_once(&control, SCHEMA_V).unwrap().round, Some(round));

        // Ending the round removes its file; a reader is told there is none.
        store.write_round(None).unwrap();
        assert!(std::fs::symlink_metadata(agents_dir(&control).join(ROUND_FILE)).is_err());
        assert_eq!(w.until(|f| f.round.is_none()), vec![Change::Round { round: None }]);
        assert_eq!(store.read_round(), None);
        store.write_round(None).unwrap();
        assert!(
            w.logged().is_empty(),
            "the shared files are read, not ignored: {:?}",
            w.logged()
        );
    }

    /// Two processes, each publishing its rows and watching every process's, as two windows would.
    struct Process {
        instance: String,
        publisher: Publisher,
        watcher: Watcher,
        store: SharedStore,
    }

    fn process(control: &Path) -> Process {
        let instance = new_instance();
        let publisher = Publisher::start(control, &instance, project(), window(&instance)).unwrap();
        Process {
            watcher: watch(control),
            store: SharedStore::open(control).unwrap(),
            instance,
            publisher,
        }
    }

    /// Two processes with one waiting row each, the first's waiting longer, and a round started at it that both
    /// have seen.
    fn two_waiting(control: &Path) -> (Process, Process, Round) {
        let mut one = process(control);
        let mut two = process(control);
        let a = card(&one.instance, 1, 100);
        let b = card(&two.instance, 1, 200);
        one.publisher.offer_now(vec![a.clone()]);
        two.publisher.offer_now(vec![b.clone()]);
        for p in [&mut one, &mut two] {
            p.watcher.until(|f| f.rows.len() == 2);
        }
        let r = start_at(&one.watcher.face.view(), here(&one.instance), None, 1_000).unwrap();
        assert_eq!(r.current.as_deref(), Some(a.key.as_str()));
        one.store.write_round(Some(&r)).unwrap();
        for p in [&mut one, &mut two] {
            p.watcher.until(|f| f.round.as_ref() == Some(&r));
        }
        (one, two, r)
    }

    #[test]
    fn only_the_owner_of_the_current_row_advances() {
        let scratch = Scratch::new("owner");
        let control = scratch.control();
        let (mut one, mut two, r) = two_waiting(&control);
        for p in [&one, &two] {
            assert!(
                !should_advance(&p.instance, &p.watcher.face.view(), &r),
                "the current row still waits"
            );
        }

        // The card is answered in the first window's chat.
        let answered = quiet(&one.instance, 1);
        one.publisher.offer_now(vec![answered.clone()]);
        for p in [&mut one, &mut two] {
            p.watcher.until(|f| f.rows.get(&answered.id) == Some(&answered));
        }
        let round = |p: &Process| p.watcher.face.round.clone().unwrap();
        assert!(should_advance(&one.instance, &one.watcher.face.view(), &round(&one)));
        assert!(!should_advance(&two.instance, &two.watcher.face.view(), &round(&two)));
        one.publisher.stop();
        two.publisher.stop();
    }

    #[test]
    fn advance_happens_once_when_the_row_leaves_needs() {
        let scratch = Scratch::new("once");
        let control = scratch.control();
        let (mut one, mut two, r) = two_waiting(&control);
        let answered = quiet(&one.instance, 1);
        one.publisher.offer_now(vec![answered.clone()]);
        for p in [&mut one, &mut two] {
            p.watcher.until(|f| f.rows.get(&answered.id) == Some(&answered));
        }

        // The owner moves the round on and writes it.
        let view = one.watcher.face.view();
        assert!(should_advance(&one.instance, &view, &r));
        let next = advance(&view, &r, 2_000).expect("the second window's card is left");
        let b = card(&two.instance, 1, 200);
        assert_eq!(next.current.as_deref(), Some(b.key.as_str()));
        assert_eq!(next.from, r.from);
        one.store.write_round(Some(&next)).unwrap();

        // Every process sees the new round once, and no process, the owner included, would move it again.
        for p in [&mut one, &mut two] {
            let changes = p.watcher.until(|f| f.round.as_ref() == Some(&next));
            let rounds: Vec<_> = changes.iter().filter(|c| matches!(c, Change::Round { .. })).collect();
            assert_eq!(
                rounds,
                vec![&Change::Round {
                    round: Some(next.clone())
                }]
            );
            let latest = p.store.read_round().unwrap();
            assert_eq!(latest, next);
            assert!(!should_advance(&p.instance, &p.watcher.face.view(), &latest));
        }
        one.publisher.stop();
        two.publisher.stop();
    }

    #[test]
    fn the_files_are_0600_and_symlinks_are_refused() {
        let scratch = Scratch::new("private");
        let control = scratch.control();
        let store = SharedStore::open(&control).unwrap();
        let dir = agents_dir(&control);
        assert_eq!(mode(&dir), 0o700);
        let selection = Selection {
            key: "k-0000000000000001".into(),
            by: A.into(),
            at_ms: 1_000,
        };
        let round = a_round("k-0000000000000001");
        store.write_selection(&selection).unwrap();
        store.write_round(Some(&round)).unwrap();
        assert_eq!(mode(&dir.join(SELECTION_FILE)), 0o600);
        assert_eq!(mode(&dir.join(ROUND_FILE)), 0o600);

        // A link at either name is never followed: not by a read, a watching reader or a write.
        let elsewhere = scratch.0.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let mut w = watch(&control);
        assert_eq!(w.first.round, Some(round.clone()));
        for (name, bytes) in [
            (SELECTION_FILE, serde_json::to_vec(&selection).unwrap()),
            (ROUND_FILE, br#"{"v":1,"current":"k-ffffffffffffffff"}"#.to_vec()),
        ] {
            let target = elsewhere.join(name);
            std::fs::write(&target, &bytes).unwrap();
            let tmp = dir.join(format!(".link-{name}"));
            std::os::unix::fs::symlink(&target, &tmp).unwrap();
            std::fs::rename(&tmp, dir.join(name)).unwrap();
        }
        w.until(|f| f.selection.is_none() && f.round.is_none());
        assert_eq!(store.read_selection(), None);
        assert_eq!(store.read_round(), None);
        let view = scan_once(&control, SCHEMA_V).unwrap();
        assert_eq!((view.selection, view.round), (None, None));
        for name in [SELECTION_FILE, ROUND_FILE] {
            let lines = w.logged().into_iter().filter(|l| l.contains(name)).count();
            assert_eq!(lines, 1, "{name}: {:#?}", w.logged());
        }

        store.write_selection(&selection).unwrap();
        store.write_round(Some(&round)).unwrap();
        for name in [SELECTION_FILE, ROUND_FILE] {
            assert!(
                std::fs::symlink_metadata(dir.join(name)).unwrap().is_file(),
                "{name} replaced the link"
            );
            assert_eq!(mode(&dir.join(name)), 0o600);
        }
        assert_eq!(
            std::fs::read(elsewhere.join(SELECTION_FILE)).unwrap(),
            serde_json::to_vec(&selection).unwrap(),
            "the link's target was not written"
        );
        w.until(|f| f.selection.as_ref() == Some(&selection) && f.round.as_ref() == Some(&round));
        store.write_round(None).unwrap();

        // A round removed through a link at the agents directory would remove something else: refused at open.
        let other = Scratch::new("private-link");
        let linked = other.control();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::set_permissions(&linked, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, agents_dir(&linked)).unwrap();
        assert!(
            SharedStore::open(&linked).is_err(),
            "a link at the agents directory is refused"
        );
    }

    #[test]
    fn a_round_file_of_a_newer_major_is_ignored() {
        let scratch = Scratch::new("newer");
        let control = scratch.control();
        let store = SharedStore::open(&control).unwrap();
        let mut w = watch(&control);
        let newer = format!(r#"{{"v":{},"from":"another shape","current":7}}"#, SCHEMA_V + 1);
        put(&control, ROUND_FILE, newer.as_bytes());
        put(
            &control,
            SELECTION_FILE,
            format!(r#"{{"v":{},"key":["another","shape"]}}"#, SCHEMA_V + 1).as_bytes(),
        );
        assert_eq!(store.read_round(), None);
        assert_eq!(store.read_selection(), None);
        let view = scan_once(&control, SCHEMA_V).unwrap();
        assert_eq!((view.selection, view.round), (None, None));
        // The reader's own read of the newer round, before it is replaced: otherwise both writes can reach it as
        // one, and it would only ever read the replacement.
        w.until_logged(ROUND_FILE);

        // Read again once it is of this schema; the newer file was logged once, not removed.
        let round = a_round("k-0000000000000001");
        put(
            &control,
            ROUND_FILE,
            &serde_json::to_vec(&serde_json::json!({
                "v": SCHEMA_V,
                "from": round.from,
                "current": round.current,
                "visited": round.visited,
                "started_at": round.started_at_ms,
                "at": round.at_ms,
                "a_later_field": true,
            }))
            .unwrap(),
        );
        let changes = w.until(|f| f.round.as_ref() == Some(&round));
        assert!(
            changes
                .iter()
                .all(|c| !matches!(c, Change::Round { round: Some(r) } if *r != round)),
            "{changes:?}"
        );
        assert_eq!(store.read_round(), Some(round));
        let log = w.logged();
        let lines: Vec<_> = log.iter().filter(|l| l.contains(ROUND_FILE)).collect();
        assert_eq!(lines.len(), 1, "{log:#?}");
        assert!(lines[0].contains(&(SCHEMA_V + 1).to_string()), "{}", lines[0]);
    }
}
