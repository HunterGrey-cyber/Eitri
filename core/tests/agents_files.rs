//! A process's agent rows on disk: the publishing half writes one private file per process, the reading half
//! merges every process's file into one view.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use eitri_core::agents::*;

/// A directory of the test's own under the target dir, removed when dropped. The control directory is made
/// inside it, so its parent is this user's and not writable by anyone else.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("af").join(format!(
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

fn row(instance: &str, number: u16) -> Row {
    Row {
        id: format!("{instance}.{number}"),
        key: format!("k-{number:016x}"),
        agent: "claude".into(),
        project: project(),
        window: window(instance),
        number,
        label: format!("tab {number}"),
        label_from: LabelFrom::New,
        state: "idle".into(),
        class: Class::Quiet,
        mode: "manual".into(),
        ..Default::default()
    }
}

mod publish {
    use super::*;
    use eitri_core::agents::publish::{agents_dir, new_instance, Publisher, PublisherHooks};
    use eitri_core::agents::wire::parse_process_file;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::time::Duration;

    /// Long enough that only a writer that never publishes runs out of it; nothing here measures speed.
    const PATIENCE: Duration = Duration::from_secs(60);

    fn real_clock() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// A publisher whose every publish is also sent down a channel. With `hold` the writer waits, inside its
    /// first publish (the empty file it writes at start), until the returned sender is used or dropped, so
    /// whatever the test offers meanwhile is queued for it at once.
    fn observed(
        control: &Path,
        instance: &str,
        clock: fn() -> u64,
        hold: bool,
    ) -> (Publisher, Receiver<ProcessFile>, Option<Sender<()>>) {
        let (tx, rx) = channel();
        let (release, held) = channel::<()>();
        let mut held = hold.then_some(held);
        let hooks = PublisherHooks {
            clock,
            on_publish: Some(Box::new(move |file: &ProcessFile| {
                let _ = tx.send(file.clone());
                if let Some(held) = held.take() {
                    let _ = held.recv();
                }
            })),
        };
        let publisher = Publisher::start_with(control, instance, project(), window(instance), hooks).unwrap();
        (publisher, rx, hold.then_some(release))
    }

    fn next(rx: &Receiver<ProcessFile>) -> ProcessFile {
        rx.recv_timeout(PATIENCE).expect("the writer published nothing")
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn on_disk(control: &Path, instance: &str) -> ProcessFile {
        let bytes = std::fs::read(agents_dir(control).join(format!("{instance}.json"))).unwrap();
        parse_process_file(&bytes).unwrap()
    }

    /// Field 22 of `/proc/self/stat`, counted after the parenthesised command name, which may hold spaces.
    /// Where there is no `/proc` (macOS) the writer has no start time to give and writes 0, which a reader takes as
    /// "the lock decides".
    fn own_start_time() -> u64 {
        if cfg!(not(target_os = "linux")) {
            return 0;
        }
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let after = &stat[stat.rfind(')').unwrap() + 1..];
        after.split_whitespace().nth(19).unwrap().parse().unwrap()
    }

    #[test]
    fn new_instance_is_sixteen_lowercase_hex_and_never_repeats() {
        let one = new_instance();
        let other = new_instance();
        for name in [&one, &other] {
            assert_eq!(name.len(), 16, "{name}");
            assert!(
                name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{name}"
            );
        }
        assert_ne!(one, other);
    }

    #[test]
    fn the_directory_is_0700_and_the_file_0600() {
        let scratch = Scratch::new("modes");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, _) = observed(&control, &instance, real_clock, false);
        let first = next(&rx);
        let dir = agents_dir(&control);
        assert_eq!(dir, control.join("agents"));
        assert_eq!(mode(&dir), 0o700, "the agents directory");
        assert_eq!(mode(&dir.join(format!("{instance}.json"))), 0o600, "the process file");
        assert_eq!(mode(&dir.join(format!("{instance}.lock"))), 0o600, "the lock file");

        let file = on_disk(&control, &instance);
        assert_eq!(file, first, "the file on disk is what was published");
        assert_eq!(file.v, SCHEMA_V);
        assert_eq!(file.instance, instance);
        assert_eq!(file.pid, std::process::id());
        assert_eq!(
            file.pid_start,
            own_start_time(),
            "the start time a reader checks against pid reuse"
        );
        assert_eq!(file.project, project());
        assert_eq!(file.window, window(&instance));
        assert!(file.rows.is_empty() && file.departed.is_empty() && !file.truncated);
        publisher.stop();
    }

    #[test]
    fn an_instance_that_is_not_sixteen_lowercase_hex_is_refused() {
        let scratch = Scratch::new("bad-instance");
        let control = scratch.control();
        for bad in [
            "../../etc/passwd",
            "A3F09C1D2E4B5F60",
            "a3f09c1d2e4b5f6",
            "a3f09c1d2e4b5f600",
            "",
        ] {
            assert!(
                Publisher::start(&control, bad, project(), window(bad)).is_err(),
                "{bad:?} must not name a file"
            );
        }
    }

    #[test]
    fn a_reader_never_sees_a_half_written_file() {
        const PUBLISHES: u16 = 2_000;
        let scratch = Scratch::new("half");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, _) = observed(&control, &instance, real_clock, false);
        next(&rx);
        let path = agents_dir(&control).join(format!("{instance}.json"));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let done = done.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                // Bounded by count as well, so a writer that never finishes cannot keep this loop alive.
                while !done.load(Ordering::Acquire) && reads < 50_000_000 {
                    let bytes = std::fs::read(&path).expect("the name always holds a whole file");
                    if let Err(e) = parse_process_file(&bytes) {
                        panic!("read {reads} did not parse: {e}");
                    }
                    reads += 1;
                }
                reads
            })
        };
        for i in 0..PUBLISHES {
            publisher.offer_now(vec![row(&instance, 1 + i % 50)]);
        }
        for i in 0..PUBLISHES {
            let expected = vec![row(&instance, 1 + i % 50)];
            loop {
                let file = next(&rx);
                if file.rows == expected {
                    break;
                }
                // The only other publish is the same rows again, once a departure has aged out of the file.
                assert!(
                    i > 0 && file.rows == vec![row(&instance, 1 + (i - 1) % 50)],
                    "publish {i}: {file:?}"
                );
            }
        }
        done.store(true, Ordering::Release);
        let reads = reader.join().expect("every read parsed");
        assert!(reads > 0);
        publisher.stop();
    }

    #[test]
    fn coalesced_offers_publish_the_last_rows() {
        let scratch = Scratch::new("coalesce");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, release) = observed(&control, &instance, real_clock, true);
        assert!(next(&rx).rows.is_empty(), "the file a process starts with");
        publisher.offer(vec![row(&instance, 1)]);
        publisher.offer(vec![row(&instance, 1), row(&instance, 2)]);
        let last = vec![row(&instance, 1), row(&instance, 2), row(&instance, 3)];
        publisher.offer(last.clone());
        drop(release);
        assert_eq!(
            next(&rx).rows,
            last,
            "offers made within the window are published once, as the last one"
        );

        // Nothing else was published in between: the next publish is the one asked for now.
        let marker = vec![row(&instance, 9)];
        publisher.offer_now(marker.clone());
        assert_eq!(next(&rx).rows, marker);
        assert_eq!(on_disk(&control, &instance).rows, marker);

        // The same rows again change nothing, so nothing is written.
        publisher.offer(marker.clone());
        publisher.offer_now(vec![row(&instance, 10)]);
        assert_eq!(next(&rx).rows, vec![row(&instance, 10)]);
        publisher.stop();
    }

    #[test]
    fn offer_now_is_not_coalesced_away() {
        let scratch = Scratch::new("now");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, release) = observed(&control, &instance, real_clock, true);
        next(&rx);
        let mut frozen = row(&instance, 1);
        frozen.moving = true;
        publisher.offer_now(vec![frozen.clone()]);
        publisher.offer(vec![row(&instance, 2)]);
        drop(release);
        assert_eq!(
            next(&rx).rows,
            vec![frozen],
            "a frozen tab is published before anything after it"
        );
        assert_eq!(next(&rx).rows, vec![row(&instance, 2)]);
        publisher.stop();
    }

    #[test]
    fn stop_removes_the_file_and_the_lock() {
        let scratch = Scratch::new("stop");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, _) = observed(&control, &instance, real_clock, false);
        next(&rx);
        publisher.offer(vec![row(&instance, 1)]);
        next(&rx);
        publisher.stop();
        let left: Vec<_> = std::fs::read_dir(agents_dir(&control))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "a clean exit leaves nothing behind: {left:?}");
    }

    #[test]
    fn stop_leaves_a_file_it_did_not_write() {
        let scratch = Scratch::new("stop-other");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, _) = observed(&control, &instance, real_clock, false);
        next(&rx);
        let dir = agents_dir(&control);
        let name = dir.join(format!("{instance}.json"));
        let other = dir.join("replacement");
        std::fs::write(&other, b"{}").unwrap();
        std::fs::rename(&other, &name).unwrap();
        publisher.stop();
        assert_eq!(
            std::fs::read(&name).unwrap(),
            b"{}",
            "only the file this process published is removed"
        );
        assert!(!dir.join(format!("{instance}.lock")).exists());
    }

    static NOW: AtomicU64 = AtomicU64::new(1_793_865_600_000);

    fn fake_clock() -> u64 {
        NOW.load(Ordering::SeqCst)
    }

    #[test]
    fn a_departed_row_names_its_why_and_ages_out() {
        let scratch = Scratch::new("departed");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, _) = observed(&control, &instance, fake_clock, false);
        next(&rx);
        let (a, b, c, d) = (
            row(&instance, 1),
            row(&instance, 2),
            row(&instance, 3),
            row(&instance, 4),
        );
        publisher.offer_now(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(next(&rx).departed, vec![]);

        let left_at = fake_clock();
        publisher.departing(b.key.clone(), GoneWhy::Moved);
        publisher.offer_now(vec![a.clone()]);
        let file = next(&rx);
        assert_eq!(file.rows, vec![a.clone()]);
        let gone = |r: &Row, why| Departed {
            id: r.id.clone(),
            key: r.key.clone(),
            why,
            at: left_at,
        };
        assert_eq!(file.departed, vec![gone(&b, GoneWhy::Moved), gone(&c, GoneWhy::Closed)]);

        // Kept until DEPARTED_KEEP_MS have passed since it left.
        NOW.fetch_add(DEPARTED_KEEP_MS - 1, Ordering::SeqCst);
        publisher.offer_now(vec![a.clone(), d.clone()]);
        let file = next(&rx);
        assert_eq!(file.departed, vec![gone(&b, GoneWhy::Moved), gone(&c, GoneWhy::Closed)]);

        // Then they leave the file by themselves, with no offer to prompt it.
        NOW.fetch_add(1, Ordering::SeqCst);
        let file = next(&rx);
        assert_eq!(file.rows, vec![a.clone(), d.clone()]);
        assert_eq!(file.departed, vec![]);

        publisher.offer_now(vec![a.clone()]);
        let file = next(&rx);
        assert_eq!(
            file.departed,
            vec![Departed {
                id: d.id.clone(),
                key: d.key.clone(),
                why: GoneWhy::Closed,
                at: fake_clock(),
            }],
            "a row with no mark was closed"
        );
        publisher.stop();
    }

    #[test]
    fn seventy_rows_publish_sixty_four_and_truncated() {
        let scratch = Scratch::new("seventy");
        let control = scratch.control();
        let instance = new_instance();
        let (publisher, rx, _) = observed(&control, &instance, real_clock, false);
        next(&rx);
        // Offered out of order: the cut keeps the lowest tab numbers.
        let all: Vec<Row> = (1..=70).rev().map(|n| row(&instance, n)).collect();
        publisher.offer_now(all.clone());
        let file = next(&rx);
        assert_eq!(file.rows.len(), MAX_ROWS_PER_FILE);
        assert_eq!(
            file.rows.iter().map(|r| r.number).collect::<Vec<_>>(),
            (1..=MAX_ROWS_PER_FILE as u16).collect::<Vec<_>>()
        );
        assert!(file.truncated);
        assert_eq!(on_disk(&control, &instance), file);
        let bytes = std::fs::read(agents_dir(&control).join(format!("{instance}.json"))).unwrap();
        assert!(bytes.len() <= MAX_FILE_BYTES);

        // A row that was cut off is not reported as departed when it goes; a shown one is.
        let fewer: Vec<Row> = all.into_iter().filter(|r| r.number != 70 && r.number != 2).collect();
        publisher.offer_now(fewer);
        let file = next(&rx);
        assert_eq!(
            file.departed.iter().map(|d| d.key.clone()).collect::<Vec<_>>(),
            vec![row(&instance, 2).key]
        );
        assert_eq!(file.rows.len(), MAX_ROWS_PER_FILE);
        assert_eq!(file.rows.last().unwrap().number, 65);
        assert!(file.truncated);

        publisher.offer_now((1..=MAX_ROWS_PER_FILE as u16).map(|n| row(&instance, n)).collect());
        let file = next(&rx);
        assert_eq!(file.rows.len(), MAX_ROWS_PER_FILE);
        assert!(!file.truncated, "exactly the limit is not cut");
        publisher.stop();
    }

    #[test]
    fn an_existing_symlink_at_the_dir_is_refused() {
        let scratch = Scratch::new("symlink");
        let control = scratch.control();
        std::fs::create_dir(&control).unwrap();
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700)).unwrap();
        let elsewhere = scratch.0.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, agents_dir(&control)).unwrap();

        let instance = new_instance();
        let refused = Publisher::start(&control, &instance, project(), window(&instance));
        assert!(
            refused.is_err(),
            "a link where the agents directory goes is never written through"
        );
        assert!(std::fs::symlink_metadata(agents_dir(&control))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing was made where the link leads"
        );
        assert_eq!(mode(&elsewhere), 0o755, "the link's target was not changed");
    }
}

/// The reading half runs on Linux only: it watches the directory with inotify and each writer with a pidfd.
#[cfg(target_os = "linux")]
mod read {
    use super::*;
    use agent::private_fs::{publish_private_file, PrivateLock};
    use eitri_core::agents::liveness::{self, clean_dead, Alive};
    use eitri_core::agents::publish::{agents_dir, new_instance, process_start_time, Publisher, PublisherHooks};
    use eitri_core::agents::read::{scan_once, scan_windows, Batch, Reader, ReaderEvent, ReaderHooks};
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::io::{BufRead, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, TryRecvError};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Long enough that only a reader that never answers runs out of it; nothing here measures speed.
    const PATIENCE: Duration = Duration::from_secs(60);

    const CHILD_CONTROL: &str = "EITRI_AGENTS_TEST_CHILD_CONTROL";
    const CHILD_INSTANCE: &str = "EITRI_AGENTS_TEST_CHILD_INSTANCE";
    const CHILD_ROWS: &str = "EITRI_AGENTS_TEST_CHILD_ROWS";
    const CHILD_READY: &str = "agents-writer-child-ready";

    /// A row whose key belongs to its writer, so rows of several writers never share a key unless a test says so.
    fn keyed(instance: &str, number: u16) -> Row {
        let mut r = row(instance, number);
        r.key = format!("k-{}{number:08x}", &instance[..8]);
        r
    }

    /// `row` as the writer `to` holds it once it has adopted the tab: same key, its own id, window and number.
    fn adopted(row: &Row, to: &str, number: u16) -> Row {
        let mut r = row.clone();
        r.id = format!("{to}.{number}");
        r.window = window(to);
        r.number = number;
        r.moving = false;
        r
    }

    fn own_start() -> u64 {
        process_start_time(std::process::id()).expect("this process has a start time")
    }

    /// What a face holds after applying every change in order, by row id.
    #[derive(Default)]
    struct Face {
        rows: BTreeMap<String, Row>,
        projects: Vec<ProjectRef>,
    }

    impl Face {
        fn apply(&mut self, change: &Change) {
            match change {
                Change::Reset { view } => {
                    self.rows = view.rows.iter().map(|r| (r.id.clone(), r.clone())).collect();
                    self.projects = view.projects.clone();
                }
                Change::Row { row } => {
                    self.rows.insert(row.id.clone(), row.clone());
                }
                Change::Gone { id, .. } => {
                    self.rows.remove(id);
                }
                // The shared selection and round are not rows; their own tests follow them.
                Change::Selection { .. } | Change::Round { .. } => {}
            }
        }

        fn has(&self, id: &str) -> bool {
            self.rows.contains_key(id)
        }

        fn shown(&self) -> Vec<Row> {
            self.rows.values().cloned().collect()
        }

        fn keys_are_unique(&self) -> bool {
            let mut keys: Vec<&str> = self.rows.values().map(|r| r.key.as_str()).collect();
            let all = keys.len();
            keys.sort_unstable();
            keys.dedup();
            keys.len() == all
        }
    }

    struct Watcher {
        reader: Option<Reader>,
        events: Receiver<ReaderEvent>,
        batches: Receiver<Batch>,
        log: Arc<Mutex<Vec<String>>>,
        face: Face,
        first: View,
    }

    fn watch(control: &Path, tune: impl FnOnce(&mut ReaderHooks)) -> Watcher {
        let (tx, events) = channel();
        let (batch_tx, batches) = channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut hooks = ReaderHooks::default();
        {
            let log = log.clone();
            hooks.log = Box::new(move |line: &str| log.lock().unwrap().push(line.to_owned()));
        }
        hooks.on_batch = Some(Box::new(move |b: &Batch| {
            let _ = batch_tx.send(b.clone());
        }));
        tune(&mut hooks);
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
            batches,
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
                    assert!(
                        self.face.keys_are_unique(),
                        "a key is shown twice after {change:?}: {:?}",
                        self.face.rows.keys().collect::<Vec<_>>()
                    );
                    change
                }
                ReaderEvent::Error(e) => panic!("the reader failed: {e}"),
            }
        }

        /// The changes up to the first one after which `done` holds for the face.
        fn until(&mut self, done: impl Fn(&Face) -> bool) -> Vec<Change> {
            let mut seen = Vec::new();
            while !done(&self.face) {
                seen.push(self.next());
            }
            seen
        }

        fn batch_until(&self, done: impl Fn(&Batch) -> bool) -> Batch {
            loop {
                let b = self
                    .batches
                    .recv_timeout(PATIENCE)
                    .expect("the reader finished no batch");
                if done(&b) {
                    return b;
                }
            }
        }

        fn logged(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn nothing_pending(&self) -> bool {
            matches!(self.events.try_recv(), Err(TryRecvError::Empty))
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            if let Some(reader) = self.reader.take() {
                reader.stop();
            }
        }
    }

    /// A writer in a process of its own, which the test can kill. It is this test binary run again for the one
    /// test below, which publishes the rows it is given and then waits to be killed.
    struct ChildWriter {
        pid: u32,
        process: Option<std::process::Child>,
        instance: String,
    }

    fn spawn_writer(control: &Path, instance: &str, rows: &[Row]) -> ChildWriter {
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "read::a_spawned_writer_child_publishes_until_killed",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_CONTROL, control)
            .env(CHILD_INSTANCE, instance)
            .env(CHILD_ROWS, serde_json::to_string(rows).unwrap())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the child writer starts");
        let pid = process.id();
        let out = process.stdout.take().unwrap();
        let mut child = ChildWriter {
            pid,
            process: Some(process),
            instance: instance.to_owned(),
        };
        // Read on a thread of its own, so a child that never gets ready fails the test instead of hanging it. The
        // harness may print the line after its own `test <name> ... `, so the marker is looked for within a line.
        let (ready_tx, ready) = channel();
        std::thread::spawn(move || {
            let ready_seen = std::io::BufReader::new(out)
                .lines()
                .map_while(Result::ok)
                .any(|line| line.contains(CHILD_READY));
            let _ = ready_tx.send(ready_seen);
        });
        if ready.recv_timeout(PATIENCE) != Ok(true) {
            child.kill();
            panic!("the child writer did not publish");
        }
        child
    }

    impl ChildWriter {
        /// SIGKILL to the pid recorded at spawn, then reaped, so the pid is never signalled again.
        fn kill(&mut self) {
            if let Some(mut process) = self.process.take() {
                // SAFETY: `kill` takes no pointers; the pid is this test's own child, not yet reaped.
                unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
                let _ = process.wait();
            }
        }

        /// SIGKILL, then waits for the exit without reaping it: the pid stays this test's child, ended but not yet
        /// collected, until `kill` reaps it.
        fn kill_unreaped(&mut self) {
            // SAFETY: `kill` takes no pointers; the pid is this test's own child, not yet reaped.
            unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
            // SAFETY: an all-zero `siginfo_t` is a valid value for the kernel to overwrite.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is valid for writes and outlives the call.
            let waited = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            assert_eq!(waited, 0, "{}", std::io::Error::last_os_error());
        }
    }

    impl Drop for ChildWriter {
        fn drop(&mut self) {
            self.kill();
        }
    }

    #[test]
    fn a_spawned_writer_child_publishes_until_killed() {
        let (Some(control), Ok(instance), Ok(rows)) = (
            std::env::var_os(CHILD_CONTROL),
            std::env::var(CHILD_INSTANCE),
            std::env::var(CHILD_ROWS),
        ) else {
            // Run as an ordinary test: there is no writer to be.
            return;
        };
        let rows: Vec<Row> = serde_json::from_str(&rows).unwrap();
        let want = rows.len();
        let (tx, rx) = channel();
        let hooks = PublisherHooks {
            on_publish: Some(Box::new(move |f: &ProcessFile| {
                let _ = tx.send(f.rows.len());
            })),
            ..PublisherHooks::default()
        };
        let publisher =
            Publisher::start_with(Path::new(&control), &instance, project(), window(&instance), hooks).unwrap();
        publisher.offer_now(rows);
        while rx.recv().unwrap() != want {}
        println!("\n{CHILD_READY}");
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }

    fn file_path(control: &Path, name: &str) -> PathBuf {
        agents_dir(control).join(name)
    }

    /// `bytes` put at `name` in the agents directory by a rename, as a writer would, so a reader never meets it
    /// half-written.
    fn put(control: &Path, name: &str, bytes: &[u8]) {
        let dir = std::fs::File::open(agents_dir(control)).unwrap();
        publish_private_file(&dir, OsStr::new(name), bytes).unwrap();
    }

    /// A file a live writer could have written: this process's pid and start time.
    fn live_file(instance: &str, rows: Vec<Row>) -> ProcessFile {
        ProcessFile {
            v: SCHEMA_V,
            instance: instance.into(),
            pid: std::process::id(),
            pid_start: own_start(),
            project: project(),
            window: window(instance),
            rows,
            departed: Vec::new(),
            truncated: false,
        }
    }

    fn held_lock(control: &Path, instance: &str) -> PrivateLock {
        let lock = PrivateLock::open(&file_path(control, &format!("{instance}.lock"))).unwrap();
        assert!(lock.try_lock().unwrap());
        lock
    }

    fn gone(row: &Row, why: GoneWhy) -> Change {
        Change::Gone {
            id: row.id.clone(),
            key: row.key.clone(),
            why,
        }
    }

    fn row_change(row: &Row) -> Change {
        Change::Row { row: row.clone() }
    }

    fn assert_cleaned(control: &Path, instance: &str) {
        for name in [format!("{instance}.json"), format!("{instance}.lock")] {
            assert!(
                std::fs::symlink_metadata(file_path(control, &name)).is_err(),
                "{name} is left behind"
            );
        }
    }

    #[test]
    fn a_writer_killed_by_pid_yields_gone_window_with_no_file_event() {
        let scratch = Scratch::new("kill");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let instance = new_instance();
        let rows = vec![keyed(&instance, 1), keyed(&instance, 2)];
        let mut child = spawn_writer(&control, &instance, &rows);
        w.until(|f| f.rows.len() == 2);

        child.kill();
        let changes = w.until(|f| f.rows.is_empty());
        assert_eq!(
            changes,
            vec![gone(&rows[0], GoneWhy::Window), gone(&rows[1], GoneWhy::Window)]
        );
        let batch = w.batch_until(|b| b.buried.contains(&child.instance));
        assert!(
            batch.read.is_empty(),
            "the end was seen through the writer's pid, with no file read: {batch:?}"
        );
        assert_cleaned(&control, &instance);
    }

    #[test]
    fn the_lock_poll_finds_a_dead_writer() {
        let scratch = Scratch::new("lockpoll");
        let control = scratch.control();
        let mut w = watch(&control, |hooks| hooks.pidfd = false);
        let instance = new_instance();
        let rows = vec![keyed(&instance, 1)];
        let mut child = spawn_writer(&control, &instance, &rows);
        w.until(|f| f.rows.len() == 1);

        child.kill();
        let changes = w.until(|f| f.rows.is_empty());
        assert_eq!(changes, vec![gone(&rows[0], GoneWhy::Window)]);
        let batch = w.batch_until(|b| b.buried.contains(&instance));
        assert!(batch.read.is_empty(), "found by its lock, with no file read: {batch:?}");
        assert_cleaned(&control, &instance);
    }

    #[test]
    fn a_reused_pid_with_another_start_time_is_dead() {
        let scratch = Scratch::new("reuse");
        let control = scratch.control();
        let own = std::process::id();
        // The reader makes the directory; the first reader also reads the file put there before it.
        drop(watch(&control, |_| {}));

        // Locks here are either held for the whole test or never taken: a child another test forks shares an open
        // lock until its exec, so a lock let go here could still read as held for a moment.
        let alive = new_instance();
        let alive_lock = file_path(&control, &format!("{alive}.lock"));
        let _held = held_lock(&control, &alive);
        assert!(matches!(
            liveness::check(own, own_start(), &alive_lock),
            Alive::Yes(Some(_))
        ));
        assert!(matches!(
            liveness::check_with(own, own_start(), &alive_lock, false),
            Alive::Yes(None)
        ));
        assert!(
            matches!(liveness::check(own, own_start() + 1, &alive_lock), Alive::Yes(None)),
            "a pid that cannot be matched, with its lock held, is watched through the lock"
        );

        let before = new_instance();
        let lock = file_path(&control, &format!("{before}.lock"));
        drop(PrivateLock::open(&lock).unwrap());
        assert!(matches!(liveness::check(own, own_start() + 1, &lock), Alive::No));
        assert!(matches!(
            liveness::check_with(own, own_start() + 1, &lock, false),
            Alive::No
        ));

        let mut file = live_file(&before, vec![keyed(&before, 1)]);
        file.pid_start += 1;
        put(&control, &format!("{before}.json"), &serde_json::to_vec(&file).unwrap());
        let mut w = watch(&control, |_| {});
        assert!(
            w.first.rows.is_empty(),
            "a writer whose pid now names another process shows nothing"
        );
        assert_cleaned(&control, &before);

        let after = new_instance();
        let mut file = live_file(&after, vec![keyed(&after, 1)]);
        file.pid_start += 1;
        put(&control, &format!("{after}.json"), &serde_json::to_vec(&file).unwrap());
        w.batch_until(|b| b.buried.contains(&after));
        let barrier = new_instance();
        let publisher = Publisher::start(&control, &barrier, project(), window(&barrier)).unwrap();
        publisher.offer_now(vec![keyed(&barrier, 1)]);
        let changes = w.until(|f| f.has(&keyed(&barrier, 1).id));
        assert!(changes.iter().all(|c| !matches!(c, Change::Gone { .. })), "{changes:?}");
        assert_eq!(w.face.shown(), vec![keyed(&barrier, 1)]);
        assert_cleaned(&control, &after);
        publisher.stop();
    }

    #[test]
    fn two_readers_clean_one_dead_writer_once() {
        let scratch = Scratch::new("tr");
        let control = scratch.control();
        let mut one = watch(&control, |_| {});
        // Pause the second reader between metadata and the read, so it finds the file missing
        // after the first reader cleans it, before it can process the writer's death.
        type Gate = (std::sync::mpsc::Sender<()>, Receiver<()>);
        static GATE: Mutex<Option<Gate>> = Mutex::new(None);
        fn pause_read(_: &Path, uid: u32) -> u32 {
            let gate = GATE.lock().unwrap().take();
            if let Some((inside, resume)) = gate {
                inside.send(()).unwrap();
                resume.recv_timeout(PATIENCE).expect("the read is released");
            }
            uid
        }
        let mut other = watch(&control, |h| h.owner = pause_read);
        let instance = new_instance();
        let socket = agent::socket_path::in_dir(&control, &format!("ag-{instance}.sock")).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let rows = vec![keyed(&instance, 1)];
        let mut child = spawn_writer(&control, &instance, &rows);
        one.until(|f| f.rows.len() == 1);
        other.until(|f| f.rows.len() == 1);

        let (inside_tx, inside) = channel();
        let (resume, resume_rx) = channel();
        *GATE.lock().unwrap() = Some((inside_tx, resume_rx));
        let bytes = std::fs::read(file_path(&control, &format!("{instance}.json"))).unwrap();
        put(&control, &format!("{instance}.json"), &bytes);
        inside
            .recv_timeout(PATIENCE)
            .expect("the other reader is inside its read");
        child.kill();
        assert_eq!(one.until(|f| f.rows.is_empty()), vec![gone(&rows[0], GoneWhy::Window)]);
        let cleaned_first = one.batch_until(|b| b.cleaned.contains(&instance));
        resume.send(()).unwrap();
        assert_eq!(
            other.until(|f| f.rows.is_empty()),
            vec![gone(&rows[0], GoneWhy::Window)]
        );
        // Joining makes every completed batch available, including the batch after the last change.
        for w in [&mut one, &mut other] {
            w.reader.take().unwrap().stop();
        }
        let other_batches: Vec<_> = other.batches.try_iter().collect();
        assert!(other_batches.iter().all(|b| !b.buried.contains(&instance)));
        let cleaned = cleaned_first.cleaned.iter().filter(|i| *i == &instance).count()
            + one
                .batches
                .try_iter()
                .chain(other_batches)
                .filter(|b| b.cleaned.contains(&instance))
                .count();
        assert_eq!(cleaned, 1, "exactly one reader removes a dead writer's files");
        assert_cleaned(&control, &instance);
        assert!(
            std::fs::symlink_metadata(&socket).is_err(),
            "the dead writer's socket goes with its files"
        );
        drop(listener);

        let dir = std::fs::File::open(agents_dir(&control)).unwrap();
        let control_handle = std::fs::File::open(&control).unwrap();
        assert!(
            !clean_dead(&dir, &control_handle, &instance).unwrap(),
            "nothing is left to clean"
        );

        // A writer whose lock is held is never cleaned, whatever said it was dead; one whose lock nobody took is.
        let live = new_instance();
        let _held = held_lock(&control, &live);
        put(&control, &format!("{live}.json"), b"{}");
        assert!(!clean_dead(&dir, &control_handle, &live).unwrap());
        assert!(file_path(&control, &format!("{live}.json")).exists());
        let ended = new_instance();
        drop(PrivateLock::open(&file_path(&control, &format!("{ended}.lock"))).unwrap());
        put(&control, &format!("{ended}.json"), b"{}");
        assert!(clean_dead(&dir, &control_handle, &ended).unwrap());
        assert_cleaned(&control, &ended);
        assert!(clean_dead(&dir, &control_handle, "../escape").is_err());
    }

    /// Files named by the test as another user's: the owner a reader sees for them is not this user.
    fn foreign_owner(path: &Path, uid: u32) -> u32 {
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if name.starts_with("ffff") {
            uid.wrapping_add(1)
        } else {
            uid
        }
    }

    #[test]
    fn ignored_files_log_once() {
        let scratch = Scratch::new("ignored");
        let control = scratch.control();
        drop(watch(&control, |_| {}));
        let link = format!("{}.json", new_instance());
        let foreign = format!("ffff{}.json", &new_instance()[..12]);
        let bad_name = "not-an-instance.json".to_owned();
        let big = format!("{}.json", new_instance());
        let newer = format!("{}.json", new_instance());
        let target = scratch.0.join("target.json");
        let stem = |name: &str| name.trim_end_matches(".json").to_owned();

        let write_all = |round: usize| {
            let valid = |name: &str| serde_json::to_vec(&live_file(&stem(name), vec![])).unwrap();
            std::fs::write(&target, valid(&link)).unwrap();
            let tmp = file_path(&control, &format!(".link-{round}"));
            std::os::unix::fs::symlink(&target, &tmp).unwrap();
            std::fs::rename(&tmp, file_path(&control, &link)).unwrap();
            put(&control, &foreign, &valid(&foreign));
            put(&control, &bad_name, &valid("a3f09c1d2e4b5f60.json"));
            let mut padded = live_file(&stem(&big), vec![]);
            padded.project.name = " ".repeat(70 * 1024);
            put(&control, &big, &serde_json::to_vec(&padded).unwrap());
            put(
                &control,
                &newer,
                format!(r#"{{"v":2,"rows":"another shape","round":{round}}}"#).as_bytes(),
            );
        };
        write_all(0);
        let mut w = watch(&control, |hooks| hooks.owner = foreign_owner);
        assert!(w.first.rows.is_empty());
        write_all(1);
        write_all(2);
        let barrier = new_instance();
        let publisher = Publisher::start(&control, &barrier, project(), window(&barrier)).unwrap();
        publisher.offer_now(vec![keyed(&barrier, 1)]);
        let changes = w.until(|f| f.has(&keyed(&barrier, 1).id));
        assert!(changes.iter().all(|c| !matches!(c, Change::Gone { .. })), "{changes:?}");
        assert_eq!(w.face.shown(), vec![keyed(&barrier, 1)]);

        let log = w.logged();
        for name in [&link, &foreign, &bad_name, &big, &newer] {
            let lines: Vec<_> = log.iter().filter(|l| l.contains(name.as_str())).collect();
            assert_eq!(lines.len(), 1, "{name}: {log:#?}");
        }
        for name in [&link, &foreign, &bad_name, &big, &newer] {
            assert!(
                std::fs::symlink_metadata(file_path(&control, name)).is_ok(),
                "a file the reader ignores is not its to remove: {name}"
            );
        }
        publisher.stop();
    }

    #[test]
    fn a_file_with_unknown_fields_and_an_unknown_state_is_read_by_class() {
        let scratch = Scratch::new("unknown");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let instance = new_instance();
        let _lock = held_lock(&control, &instance);
        let mut value = serde_json::to_value(live_file(&instance, vec![keyed(&instance, 1)])).unwrap();
        value["a_later_field"] = serde_json::json!({"anything": [1, 2]});
        let r = &mut value["rows"][0];
        r["state"] = "paused".into();
        r["class"] = "working".into();
        r["label_from"] = "telepathy".into();
        r["ended_how"] = "evaporated".into();
        r["a_later_row_field"] = 7.into();
        put(
            &control,
            &format!("{instance}.json"),
            &serde_json::to_vec(&value).unwrap(),
        );

        let id = keyed(&instance, 1).id;
        w.until(|f| f.has(&id));
        let shown = &w.face.rows[&id];
        assert_eq!(shown.state, "paused");
        assert_eq!(
            shown.class,
            Class::Working,
            "an unknown state keeps the class it was written with"
        );
        assert_eq!(State::parse(&shown.state), None);
        assert_eq!(shown.label_from, LabelFrom::New);
        assert_eq!(shown.ended_how, None);
        assert_eq!(scan_once(&control, SCHEMA_V).unwrap().rows, vec![shown.clone()]);
        assert!(
            w.logged().is_empty(),
            "a file of this schema is read, not logged: {:?}",
            w.logged()
        );
    }

    #[test]
    fn a_newer_major_version_is_skipped_once() {
        let scratch = Scratch::new("newer");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let instance = new_instance();
        let name = format!("{instance}.json");
        let _lock = held_lock(&control, &instance);
        for round in 0..3 {
            put(
                &control,
                &name,
                format!(r#"{{"v":{},"rows":{round}}}"#, SCHEMA_V + 1).as_bytes(),
            );
            // Renames that arrive together are one read of the last file, so each version is read before the
            // next one is put.
            w.batch_until(|b| b.read.contains(&instance));
        }
        let ignored = |log: &[String]| log.iter().filter(|l| l.contains(&name)).count();
        assert_eq!(ignored(&w.logged()), 1, "{:#?}", w.logged());
        // Read by the same reader once it is a file of this schema again.
        put(
            &control,
            &name,
            &serde_json::to_vec(&live_file(&instance, vec![keyed(&instance, 1)])).unwrap(),
        );
        let changes = w.until(|f| f.has(&keyed(&instance, 1).id));
        assert!(changes.iter().all(|c| !matches!(c, Change::Gone { .. })), "{changes:?}");
        assert_eq!(w.face.shown(), vec![keyed(&instance, 1)]);
        let log = w.logged();
        let lines: Vec<_> = log.iter().filter(|l| l.contains(&name)).collect();
        assert_eq!(lines.len(), 1, "{log:#?}");
        assert!(lines[0].contains(&(SCHEMA_V + 1).to_string()), "{}", lines[0]);
    }

    #[test]
    fn target_first_move_shows_one_row() {
        let scratch = Scratch::new("target-first");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let (from, to) = (new_instance(), new_instance());
        let source = Publisher::start(&control, &from, project(), window(&from)).unwrap();
        let target = Publisher::start(&control, &to, project(), window(&to)).unwrap();
        let tab = keyed(&from, 1);
        source.offer_now(vec![tab.clone()]);
        w.until(|f| f.has(&tab.id));
        let mut frozen = tab.clone();
        frozen.moving = true;
        source.offer_now(vec![frozen.clone()]);
        assert_eq!(w.next(), row_change(&frozen));

        let landed = adopted(&tab, &to, 4);
        target.offer_now(vec![landed.clone()]);
        assert_eq!(
            w.until(|f| f.has(&landed.id)),
            vec![gone(&frozen, GoneWhy::Moved), row_change(&landed)],
            "the frozen row gives way to the adopted one in one step"
        );

        // The source catching up changes nothing a face sees.
        source.departing(tab.key.clone(), GoneWhy::Moved);
        source.offer_now(vec![]);
        let marker = keyed(&to, 9);
        target.offer_now(vec![landed.clone(), marker.clone()]);
        assert_eq!(w.until(|f| f.has(&marker.id)), vec![row_change(&marker)]);
        source.stop();
        target.stop();
    }

    #[test]
    fn source_first_move_keeps_the_slot_and_says_moved() {
        let scratch = Scratch::new("source-first");
        let control = scratch.control();
        assert_eq!(ReaderHooks::default().move_hold, Duration::from_millis(MOVE_HOLD_MS));
        // Held for as long as the test takes, so the order below does not depend on how fast it runs.
        let mut w = watch(&control, |hooks| hooks.move_hold = PATIENCE * 10);
        let (from, to) = (new_instance(), new_instance());
        let source = Publisher::start(&control, &from, project(), window(&from)).unwrap();
        let target = Publisher::start(&control, &to, project(), window(&to)).unwrap();
        let (tab, closing) = (keyed(&from, 1), keyed(&from, 2));
        source.offer_now(vec![tab.clone(), closing.clone()]);
        w.until(|f| f.has(&tab.id) && f.has(&closing.id));

        source.departing(tab.key.clone(), GoneWhy::Moved);
        source.offer_now(vec![]);
        assert_eq!(
            w.next(),
            gone(&closing, GoneWhy::Closed),
            "a row that left unmarked was closed"
        );
        w.batch_until(|b| b.held.contains(&tab.id));
        assert!(w.nothing_pending(), "a row that left for another window is held");
        assert!(w.face.has(&tab.id), "the face keeps the slot");

        let landed = adopted(&tab, &to, 2);
        target.offer_now(vec![landed.clone()]);
        assert_eq!(
            w.until(|f| f.has(&landed.id)),
            vec![gone(&tab, GoneWhy::Moved), row_change(&landed)]
        );
        source.stop();
        target.stop();

        // With the usual hold, a move nobody adopts goes out as moved when the hold ends.
        let scratch = Scratch::new("source-only");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let source = Publisher::start(&control, &from, project(), window(&from)).unwrap();
        source.offer_now(vec![tab.clone()]);
        w.until(|f| f.has(&tab.id));
        source.departing(tab.key.clone(), GoneWhy::Moved);
        source.offer_now(vec![]);
        assert_eq!(w.until(|f| f.rows.is_empty()), vec![gone(&tab, GoneWhy::Moved)]);
        source.stop();
    }

    #[test]
    fn a_writer_killed_mid_move_leaves_one_row_per_key() {
        let scratch = Scratch::new("mid-move");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let (from, to) = (new_instance(), new_instance());
        let frozen = |n| {
            let mut r = keyed(&from, n);
            r.moving = true;
            r
        };
        let (adopted_one, plain, unadopted) = (frozen(1), keyed(&from, 2), frozen(3));
        let mut child = spawn_writer(
            &control,
            &from,
            &[adopted_one.clone(), plain.clone(), unadopted.clone()],
        );
        let target = Publisher::start(&control, &to, project(), window(&to)).unwrap();
        w.until(|f| f.rows.len() == 3);

        let first = adopted(&adopted_one, &to, 1);
        target.offer_now(vec![first.clone()]);
        assert_eq!(
            w.until(|f| f.has(&first.id)),
            vec![gone(&adopted_one, GoneWhy::Moved), row_change(&first)]
        );

        child.kill();
        assert_eq!(
            w.until(|f| f.rows.len() == 1),
            vec![gone(&plain, GoneWhy::Window), gone(&unadopted, GoneWhy::Window)],
            "a killed window's rows go, the adopted one stays"
        );
        assert!(w.face.has(&first.id));

        let late = adopted(&unadopted, &to, 2);
        target.offer_now(vec![first.clone(), late.clone()]);
        assert_eq!(w.until(|f| f.has(&late.id)), vec![row_change(&late)]);
        assert_cleaned(&control, &from);
        target.stop();
    }

    #[test]
    fn twenty_writers_and_a_truncated_file() {
        let scratch = Scratch::new("twenty");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let (published_tx, published) = channel::<String>();
        let writers: Vec<(String, Publisher)> = (0..20)
            .map(|i| {
                let instance = new_instance();
                let tx = published_tx.clone();
                let name = instance.clone();
                let hooks = PublisherHooks {
                    on_publish: Some(Box::new(move |f: &ProcessFile| {
                        if !f.rows.is_empty() {
                            let _ = tx.send(name.clone());
                        }
                    })),
                    ..PublisherHooks::default()
                };
                let publisher =
                    Publisher::start_with(&control, &instance, project(), window(&instance), hooks).unwrap();
                let count = if i == 0 { 70 } else { 2 };
                publisher.offer_now((1..=count).map(|n| keyed(&instance, n)).collect());
                (instance, publisher)
            })
            .collect();
        for _ in 0..writers.len() {
            published.recv_timeout(PATIENCE).expect("every writer publishes");
        }
        let expected = MAX_ROWS_PER_FILE + 19 * 2;
        w.until(|f| f.rows.len() == expected);
        let big = &writers[0].0;
        let mut numbers: Vec<u16> = w
            .face
            .rows
            .values()
            .filter(|r| r.window.instance == *big)
            .map(|r| r.number)
            .collect();
        numbers.sort_unstable();
        assert_eq!(numbers, (1..=MAX_ROWS_PER_FILE as u16).collect::<Vec<_>>());
        for (instance, _) in &writers[1..] {
            assert_eq!(
                w.face.rows.values().filter(|r| r.window.instance == *instance).count(),
                2,
                "every window is shown"
            );
        }

        // Every write so far has been made. A newcomer's row, once shown, means the reader has caught up.
        let newcomer = new_instance();
        let late = Publisher::start(&control, &newcomer, project(), window(&newcomer)).unwrap();
        late.offer_now(vec![keyed(&newcomer, 1)]);
        w.until(|f| f.has(&keyed(&newcomer, 1).id));
        w.batch_until(|b| b.read.contains(&newcomer) && b.changed > 0);

        let (one, publisher) = &writers[7];
        publisher.offer_now(vec![keyed(one, 1), keyed(one, 2), keyed(one, 3)]);
        assert_eq!(w.until(|f| f.has(&keyed(one, 3).id)), vec![row_change(&keyed(one, 3))]);
        let mut read = Vec::new();
        loop {
            let b = w.batch_until(|_| true);
            read.extend(b.read.iter().cloned());
            if b.changed > 0 {
                break;
            }
        }
        read.retain(|name| *name != newcomer);
        assert_eq!(
            read,
            vec![one.clone()],
            "one publish reads one file, not every window's"
        );

        let notes: Vec<_> = w.logged().into_iter().filter(|l| l.contains(big.as_str())).collect();
        assert_eq!(notes.len(), 1, "a truncated window is noted once: {notes:?}");
        for (_, publisher) in writers {
            publisher.stop();
        }
        late.stop();
    }

    #[test]
    fn an_overflow_yields_reset() {
        let scratch = Scratch::new("overflow");
        let control = scratch.control();
        let overflow = Arc::new(AtomicBool::new(false));
        let mut w = watch(&control, |hooks| hooks.overflow = Some(overflow.clone()));
        let instance = new_instance();
        let publisher = Publisher::start(&control, &instance, project(), window(&instance)).unwrap();
        publisher.offer_now(vec![keyed(&instance, 1)]);
        w.until(|f| f.has(&keyed(&instance, 1).id));

        overflow.store(true, Ordering::SeqCst);
        publisher.offer_now(vec![keyed(&instance, 1), keyed(&instance, 2)]);
        let first = w.next();
        assert!(
            matches!(first, Change::Reset { .. }),
            "events lost are answered with the whole view, not {first:?}"
        );
        w.until(|f| f.has(&keyed(&instance, 2).id));
        assert!(!overflow.load(Ordering::SeqCst), "the overflow was taken");
        publisher.stop();
    }

    #[test]
    fn scan_once_matches_the_watched_view() {
        let scratch = Scratch::new("scan");
        let control = scratch.control();
        assert_eq!(
            scan_once(&control, SCHEMA_V).unwrap().rows,
            vec![],
            "no directory yet: an empty view"
        );
        assert!(!control.exists(), "a one-shot read makes nothing");

        let mut w = watch(&control, |_| {});
        let other_project = ProjectRef {
            key: "0123456789abcdef".into(),
            name: "another".into(),
        };
        let a = new_instance();
        let b = new_instance();
        let empty = new_instance();
        let pa = Publisher::start(&control, &a, project(), window(&a)).unwrap();
        let pb = Publisher::start(&control, &b, project(), window(&b)).unwrap();
        let pe = Publisher::start(&control, &empty, other_project.clone(), window(&empty)).unwrap();
        pa.offer_now(vec![keyed(&a, 2), keyed(&a, 1)]);
        let mut frozen = keyed(&b, 1);
        frozen.moving = true;
        pb.offer_now(vec![frozen, keyed(&b, 3)]);
        // The window with no rows writes its empty file on its own thread: wait for its project too.
        w.until(|f| f.rows.len() == 4 && f.projects.len() == 2);

        let scanned = scan_once(&control, SCHEMA_V).unwrap();
        let mut watched: Vec<Row> = w.face.rows.values().cloned().collect();
        let mut rows = scanned.rows.clone();
        watched.sort_by(|x, y| x.id.cmp(&y.id));
        rows.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(rows, watched);

        let order = |r: &Row| {
            (
                r.project.name.clone(),
                r.project.key.clone(),
                r.window.name.clone(),
                r.window.instance.clone(),
                r.number,
            )
        };
        assert!(
            scanned.rows.windows(2).all(|p| order(&p[0]) <= order(&p[1])),
            "rows come in a stable order"
        );
        assert_eq!(
            scanned.projects,
            vec![other_project, project()],
            "every running window's project, by name, rows or not"
        );
        let fresh = watch(&control, |_| {});
        assert_eq!(fresh.first.rows, scanned.rows, "a new reader starts from the same view");
        assert_eq!(fresh.first.projects, scanned.projects);
        drop(fresh);
        pa.stop();
        pb.stop();
        pe.stop();
    }

    #[test]
    fn scan_windows_names_every_running_window_with_or_without_rows() {
        let scratch = Scratch::new("scan-windows");
        let control = scratch.control();
        assert!(scan_windows(&control, SCHEMA_V).unwrap().is_empty(), "no directory yet");
        assert!(!control.exists(), "a one-shot read makes nothing");

        let mut w = watch(&control, |_| {});
        let chat = new_instance();
        let hub = new_instance();
        let hub_window = WindowRef {
            instance: hub.clone(),
            name: "1".into(),
            focused: false,
        };
        let pc = Publisher::start(&control, &chat, project(), window(&chat)).unwrap();
        let ph = Publisher::start(&control, &hub, project(), hub_window.clone()).unwrap();
        pc.offer_now(vec![keyed(&chat, 1)]);
        w.until(|f| f.rows.len() == 1);
        let mut found = Vec::new();
        for _ in 0..200 {
            found = scan_windows(&control, SCHEMA_V).unwrap();
            if found.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        found.sort_by(|a, b| a.1.instance.cmp(&b.1.instance));
        let mut expected = vec![(project(), window(&chat)), (project(), hub_window)];
        expected.sort_by(|a, b| a.1.instance.cmp(&b.1.instance));
        assert_eq!(found, expected, "the window that holds no tab is there too");
        drop(w);
        pc.stop();
        ph.stop();
    }

    /// A child keeps the probe's open file description after the probing thread closes its copy.
    struct InheritedProbe {
        release: Option<OwnedFd>,
        pid: libc::pid_t,
    }

    impl Drop for InheritedProbe {
        fn drop(&mut self) {
            drop(self.release.take());
            // SAFETY: the pid remains our unreaped child. Closing the pipe wakes it; its poll
            // also has a hang guard, so collecting it cannot leave a child behind.
            unsafe {
                while libc::waitpid(self.pid, std::ptr::null_mut(), 0) < 0 {
                    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        break;
                    }
                }
            }
        }
    }

    fn inherit_probe(control: &Path, instance: &str) -> InheritedProbe {
        let held = held_lock(control, instance);
        std::thread::spawn(move || {
            let mut pipe = [-1; 2];
            // SAFETY: the array has room for both descriptors. After fork the child only uses
            // async-signal-safe syscalls and _exit, never Rust allocation or inherited locks.
            let pid = unsafe {
                assert_eq!(libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC), 0);
                let pid = libc::fork();
                if pid == 0 {
                    libc::close(pipe[1]);
                    let mut fd = libc::pollfd {
                        fd: pipe[0],
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    libc::poll(&mut fd, 1, 10_000);
                    libc::_exit(0);
                }
                libc::close(pipe[0]);
                pid
            };
            assert!(pid > 0, "fork: {}", std::io::Error::last_os_error());
            drop(held);
            // SAFETY: the successful pipe call made this descriptor, owned only here.
            InheritedProbe {
                release: Some(unsafe { OwnedFd::from_raw_fd(pipe[1]) }),
                pid,
            }
        })
        .join()
        .unwrap()
    }

    fn scan_until_dead(control: &Path, mut retained: impl FnMut()) {
        let deadline = std::time::Instant::now() + PATIENCE;
        while !scan_once(control, SCHEMA_V).unwrap().rows.is_empty() {
            retained();
            assert!(
                std::time::Instant::now() < deadline,
                "the dead writer's row never disappeared"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_killed_writer_not_yet_reaped_is_dead() {
        let scratch = Scratch::new("unreaped");
        let control = scratch.control();
        let instance = new_instance();
        let rows = vec![keyed(&instance, 1)];
        let mut child = spawn_writer(&control, &instance, &rows);
        let ids = |view: View| view.rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(scan_once(&control, SCHEMA_V).unwrap()), vec![rows[0].id.clone()]);
        let start = process_start_time(child.pid).expect("the child has a start time");

        child.kill_unreaped();
        let lock = file_path(&control, &format!("{instance}.lock"));
        assert!(
            matches!(liveness::check(child.pid, start, &lock), Alive::No),
            "a process that has exited is dead before its parent collects it"
        );
        let mut inherited = Some(inherit_probe(&control, &instance));
        let mut retries = 0;
        scan_until_dead(&control, || {
            retries += 1;
            drop(inherited.take());
        });
        assert!(
            retries > 0,
            "the forked child retained the probe lock for the first scan"
        );
        assert_cleaned(&control, &instance);
    }

    #[test]
    fn a_lock_that_cannot_be_opened_keeps_its_writer() {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            // Permission bits do not stop root from opening the lock.
            return;
        }
        let scratch = Scratch::new("unopenable");
        let control = scratch.control();
        drop(watch(&control, |_| {}));
        let own = std::process::id();
        let instance = new_instance();
        let lock = file_path(&control, &format!("{instance}.lock"));
        drop(PrivateLock::open(&lock).unwrap());
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            matches!(
                liveness::check_with(own, own_start() + 1, &lock, false),
                Alive::Yes(None)
            ),
            "only a lock that was taken says its writer is gone"
        );
        assert!(matches!(liveness::check(own, own_start() + 1, &lock), Alive::Yes(None)));

        let missing = file_path(&control, &format!("{}.lock", new_instance()));
        assert!(matches!(
            liveness::check_with(own, own_start() + 1, &missing, false),
            Alive::No
        ));
    }

    #[test]
    fn a_lock_left_by_a_writer_that_never_published_is_removed() {
        let scratch = Scratch::new("orphan");
        let control = scratch.control();
        drop(watch(&control, |_| {}));
        let made = |instance: &str, modified: std::time::SystemTime| {
            let path = file_path(&control, &format!("{instance}.lock"));
            drop(PrivateLock::open(&path).unwrap());
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(modified).unwrap();
            path
        };
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        // Dated ahead of now, so it is younger than any age a reader waits for however slowly this runs.
        let just_made = std::time::SystemTime::now() + Duration::from_secs(3600);

        let ended = new_instance();
        let ended_lock = made(&ended, long_ago);
        let socket = agent::socket_path::in_dir(&control, &format!("ag-{ended}.sock")).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let starting = new_instance();
        let starting_lock = made(&starting, just_made);
        let live = new_instance();
        let _held = held_lock(&control, &live);
        let live_lock = made(&live, long_ago);

        assert!(scan_once(&control, SCHEMA_V).unwrap().rows.is_empty());
        assert!(
            std::fs::symlink_metadata(&ended_lock).is_err(),
            "an old lock nobody holds, with no file beside it, is removed"
        );
        assert!(std::fs::symlink_metadata(&socket).is_err(), "its socket goes with it");
        assert!(
            starting_lock.exists(),
            "a lock made a moment ago may be a writer about to take it"
        );
        assert!(live_lock.exists(), "a held lock stays");
        drop(listener);

        let later = new_instance();
        let later_lock = made(&later, long_ago);
        drop(watch(&control, |_| {}));
        assert!(
            std::fs::symlink_metadata(&later_lock).is_err(),
            "a watching reader removes one when it reads the directory whole"
        );
        assert!(starting_lock.exists() && live_lock.exists());
    }

    #[test]
    fn a_project_that_comes_or_goes_reaches_a_watching_face() {
        let scratch = Scratch::new("projects");
        let control = scratch.control();
        let mut w = watch(&control, |_| {});
        let a = new_instance();
        let pa = Publisher::start(&control, &a, project(), window(&a)).unwrap();
        pa.offer_now(vec![keyed(&a, 1)]);
        w.until(|f| f.has(&keyed(&a, 1).id));
        assert_eq!(w.face.projects, vec![project()]);

        let other = ProjectRef {
            key: "0123456789abcdef".into(),
            name: "another".into(),
        };
        let e = new_instance();
        let pe = Publisher::start(&control, &e, other.clone(), window(&e)).unwrap();
        w.until(|f| f.projects.len() == 2);
        assert_eq!(
            w.face.projects,
            vec![other.clone(), project()],
            "a window with no tab brings its project"
        );
        assert_eq!(w.face.shown(), vec![keyed(&a, 1)]);

        pe.stop();
        w.until(|f| f.projects.len() == 1);
        assert_eq!(w.face.projects, vec![project()]);
        assert_eq!(w.face.shown(), vec![keyed(&a, 1)]);

        pa.stop();
        let changes = w.until(|f| f.projects.is_empty());
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!(
            changes[0],
            gone(&keyed(&a, 1), GoneWhy::Window),
            "the rows leave with their reason before the whole view"
        );
        assert!(
            matches!(&changes[1], Change::Reset { view } if view.rows.is_empty() && view.projects.is_empty()),
            "{:?}",
            changes[1]
        );
    }

    #[test]
    fn an_extra_descriptor_that_hangs_up_ends_the_reader() {
        let scratch = Scratch::new("extra");
        let control = scratch.control();
        let w = watch(&control, |_| {});
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors `pipe2` writes.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors were just made and nothing else owns them.
        let (read_end, write_end) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        w.reader.as_ref().unwrap().add_fd(write_end);
        drop(read_end);
        loop {
            match w.events.recv_timeout(PATIENCE) {
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("the reader kept running after its output went"),
                Ok(_) => {}
            }
        }
    }
}
