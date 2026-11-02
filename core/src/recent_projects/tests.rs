use super::*;

use std::collections::HashSet;
use std::ffi::{CString, OsString};
use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use crate::state_paths::project_key;
use crate::test_scratch_dir::ScratchDir;

struct Fixture {
    dir: ScratchDir,
    sources: Sources,
}

impl Fixture {
    fn new() -> Self {
        let dir = ScratchDir::new("recent", "records");
        let sources = Sources::from_env(Some(dir.as_os_str()), None, dir.join("control"));
        fs::create_dir_all(sources.layout.as_ref().unwrap()).unwrap();
        fs::create_dir_all(sources.tabs.as_ref().unwrap()).unwrap();
        Self { dir, sources }
    }

    fn root(&self, name: impl AsRef<OsStr>) -> PathBuf {
        self.dir.join(name.as_ref())
    }

    fn record(&self, root: &Path, suffix: &str, bytes: &[u8], seconds: u64) -> PathBuf {
        let dir = if suffix == ".json" {
            self.sources.tabs.as_ref().unwrap()
        } else {
            self.sources.layout.as_ref().unwrap()
        };
        let path = dir.join(format!("{}{suffix}", project_key(root)));
        agent::private_fs::write_private(&path, bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
        path
    }

    fn stamp(&self, root: &Path, seconds: u64) -> PathBuf {
        self.record(root, ".pair.lock", root.as_os_str().as_bytes(), seconds)
    }

    fn json(&self, root: &Path, suffix: &str, seconds: u64) -> PathBuf {
        self.record(
            root,
            suffix,
            serde_json::json!({"project_root": root.to_string_lossy()})
                .to_string()
                .as_bytes(),
            seconds,
        )
    }
}

#[derive(Default)]
struct FakeProbe {
    gone: HashSet<PathBuf>,
    open: HashSet<PathBuf>,
}

impl Probe for FakeProbe {
    fn exists(&self, path: &Path) -> bool {
        !self.gone.contains(path)
    }

    fn window_holds(&self, root: &Path) -> bool {
        self.open.contains(root)
    }
}

/// A flag of its own for each call: tests run in parallel and a flag another test holds would turn
/// this test's facts into Unknown.
fn busy() -> &'static AtomicBool {
    const SLOTS: usize = 512;
    static POOL: [AtomicBool; SLOTS] = [const { AtomicBool::new(false) }; SLOTS];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::Relaxed);
    POOL.get(index).expect("the test flag pool is used up")
}

fn rows(f: &Fixture, probe: FakeProbe) -> Vec<Row> {
    read_with(&f.sources, Box::new(probe), Duration::from_secs(5), busy())
}

fn roots(f: &Fixture) -> Vec<PathBuf> {
    candidates(&f.sources)
        .into_iter()
        .map(|candidate| candidate.root)
        .collect()
}

#[test]
fn a_stamped_project_takes_the_locks_mtime_alone() {
    let f = Fixture::new();
    let a = f.root("a");
    let b = f.root("b");
    f.stamp(&a, 10);
    f.json(&a, ".v2.json", 30);
    f.stamp(&b, 20);
    let found = candidates(&f.sources);
    assert_eq!(found.iter().map(|c| &c.root).collect::<Vec<_>>(), vec![&b, &a]);
    assert_eq!(found[1].used, SystemTime::UNIX_EPOCH + Duration::from_secs(10));
}

#[test]
fn an_empty_lock_takes_the_newest_mtime_of_its_group() {
    let f = Fixture::new();
    let a = f.root("a");
    f.record(&a, ".pair.lock", b"", 10);
    f.json(&a, ".v2.json", 20);
    f.json(&a, ".json", 40);
    f.record(&a, ".cm.json", b"{}", 30);
    let found = candidates(&f.sources);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].root, a);
    assert_eq!(found[0].used, SystemTime::UNIX_EPOCH + Duration::from_secs(40));
}

#[test]
fn the_locks_root_beats_the_jsons() {
    let f = Fixture::new();
    let a = f.root("a");
    f.stamp(&a, 10);
    f.record(&a, ".v2.json", b"{\"project_root\":\"/wrong\"}", 20);
    f.record(&a, ".json", b"broken", 30);
    assert_eq!(roots(&f), vec![a]);
}

#[test]
fn a_cm_file_gives_a_time_and_no_root() {
    let f = Fixture::new();
    let a = f.root("a");
    f.json(&a, ".json", 10);
    f.record(&a, ".cm.json", b"{}", 20);
    f.record(&f.root("cm-only"), ".cm.json", b"{\"project_root\":\"/wrong\"}", 30);
    let found = candidates(&f.sources);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].root, a);
    assert_eq!(found[0].used, SystemTime::UNIX_EPOCH + Duration::from_secs(20));
}

#[test]
fn wider_names_are_skipped() {
    let f = Fixture::new();
    for suffix in [
        "-w2.v2.json",
        "-u.v2.json",
        ".v2.json.partial",
        "-w2.pair.lock",
        "-u.json",
    ] {
        let root = f.root(suffix);
        f.json(&root, suffix, 10);
    }
    let root = f.root("short");
    agent::private_fs::write_private(
        &f.sources
            .layout
            .as_ref()
            .unwrap()
            .join(format!("{}.v2.json", &project_key(&root)[..15])),
        serde_json::json!({"project_root":root}).to_string().as_bytes(),
    )
    .unwrap();
    for suffix in ["-w2.json", "-u.json", ".json.partial"] {
        let root = f.root(suffix);
        agent::private_fs::write_private(
            &f.sources
                .tabs
                .as_ref()
                .unwrap()
                .join(format!("{}{suffix}", project_key(&root))),
            serde_json::json!({"project_root": root}).to_string().as_bytes(),
        )
        .unwrap();
    }
    assert!(roots(&f).is_empty());
}

#[test]
fn only_the_newest_two_hundred_groups_supply_candidates() {
    let f = Fixture::new();
    for i in 0..205 {
        f.stamp(&f.root(format!("project-{i}")), 1 + i);
    }
    f.json(&f.root("project-0"), ".v2.json", 1000);
    let found = candidates(&f.sources);
    assert_eq!(found.len(), 200);
    assert_eq!(found.first().unwrap().root, f.root("project-204"));
    assert_eq!(found.last().unwrap().root, f.root("project-5"));
    f.record(&f.root("project-204"), ".pair.lock", b"torn", 205);
    let found = candidates(&f.sources);
    assert_eq!(found.len(), 199);
    assert_eq!(found.last().unwrap().root, f.root("project-5"));
}

#[test]
fn json_precedence_does_not_hide_a_broken_preferred_record() {
    let f = Fixture::new();
    let root = f.root("project");
    let tabs = f.json(&root, ".json", 10);
    assert_eq!(roots(&f), vec![root.clone()]);
    let layout = f.json(&root, ".v2.json", 20);
    fs::write(&tabs, b"broken").unwrap();
    assert_eq!(roots(&f), vec![root.clone()]);
    fs::write(&layout, b"broken").unwrap();
    f.json(&root, ".json", 30);
    assert!(roots(&f).is_empty());
    let lock = f.record(&root, ".pair.lock", b"broken", 40);
    f.json(&root, ".v2.json", 50);
    assert!(roots(&f).is_empty());
    fs::write(&lock, b"").unwrap();
    assert_eq!(roots(&f), vec![root]);
}

#[test]
fn state_sources_use_only_absolute_environment_paths() {
    let f = Fixture::new();
    let home = f.root("home");
    let state = f.root("state-home");
    let sources = Sources::from_env(Some(state.as_os_str()), Some(home.as_os_str()), f.dir.join("control"));
    assert_eq!(sources.layout, Some(state.join("eitri/layout")));
    assert_eq!(sources.tabs, Some(state.join("eitri/tabs")));
    assert_eq!(sources.home, Some(home.clone()));
    for state in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
        let sources = Sources::from_env(state, Some(home.as_os_str()), PathBuf::new());
        assert_eq!(sources.layout, Some(home.join(".local/state/eitri/layout")));
        assert_eq!(sources.tabs, Some(home.join(".local/state/eitri/tabs")));
        let sources = Sources::from_env(state, Some(OsStr::new("relative")), PathBuf::new());
        assert!(sources.layout.is_none());
        assert!(sources.tabs.is_none());
        assert!(sources.home.is_none());
    }
}

#[test]
fn each_bad_record_leaves_the_row_out() {
    for suffix in [".pair.lock", ".v2.json", ".json"] {
        for kind in ["mismatch", "link", "fifo", "over-cap", "torn"] {
            let f = Fixture::new();
            let root = if kind == "over-cap" && suffix == ".pair.lock" {
                f.root(OsString::from_vec(vec![b'x'; 4 * 1024 * 1024 + 1]))
            } else {
                f.root("project")
            };
            let path = f.json(&root, suffix, 10);
            match kind {
                "mismatch" => fs::write(&path, b"{\"project_root\":\"/wrong\"}").unwrap(),
                "link" => {
                    fs::remove_file(&path).unwrap();
                    let target = f.dir.join("target");
                    let bytes = if suffix == ".pair.lock" {
                        root.as_os_str().as_bytes().to_vec()
                    } else {
                        serde_json::json!({"project_root": root}).to_string().into_bytes()
                    };
                    agent::private_fs::write_private(&target, &bytes).unwrap();
                    symlink(target, &path).unwrap();
                }
                "fifo" => {
                    fs::remove_file(&path).unwrap();
                    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: name is a terminated path with no interior NUL.
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                "over-cap" if suffix == ".pair.lock" => fs::write(&path, root.as_os_str().as_bytes()).unwrap(),
                "over-cap" => {
                    File::options()
                        .append(true)
                        .open(&path)
                        .unwrap()
                        .write_all(&vec![b' '; 4 * 1024 * 1024 + 1])
                        .unwrap();
                }
                "torn" => fs::write(&path, &root.as_os_str().as_bytes()[..root.as_os_str().len() - 1]).unwrap(),
                _ => unreachable!(),
            }
            assert!(roots(&f).is_empty(), "a {kind} {suffix} record supplies no root");
        }
    }
}

#[test]
fn a_non_utf8_root_survives_from_the_lock_and_is_dropped_from_json() {
    let f = Fixture::new();
    let root = f.root(OsString::from_vec(b"project-\xff".to_vec()));
    let lock = f.stamp(&root, 10);
    f.json(&root, ".v2.json", 20);
    f.json(&root, ".json", 30);
    assert_eq!(roots(&f), vec![root]);
    fs::remove_file(lock).unwrap();
    assert!(roots(&f).is_empty());
}

#[test]
fn a_name_with_a_newline_and_markup_is_one_row() {
    let f = Fixture::new();
    let root = f.root("中文 <b>app\nnext");
    f.stamp(&root, 10);
    let found = rows(&f, FakeProbe::default());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, root);
    assert_eq!(found[0].name, "中文 <b>app?next");
    assert!(found[0].shown.contains("中文 <b>app?next"));
    assert!(!found[0].shown.chars().any(char::is_control));
}

#[test]
fn home_and_root_sort_last() {
    let mut f = Fixture::new();
    let home = f.root("home");
    fs::create_dir(&home).unwrap();
    f.sources.home = Some(home.clone());
    f.stamp(&home, 40);
    f.stamp(Path::new("/"), 30);
    let project = f.root("project");
    f.stamp(&project, 10);
    assert_eq!(roots(&f), vec![project, home, PathBuf::from("/")]);
}

#[test]
fn home_sinks_when_home_is_a_symlink() {
    let mut f = Fixture::new();
    let home = f.root("real-home");
    let link = f.root("home-link");
    fs::create_dir(&home).unwrap();
    let home = home.canonicalize().unwrap();
    symlink(&home, &link).unwrap();
    f.sources.home = Some(link);
    f.stamp(&home, 40);
    let project = f.root("project");
    f.stamp(&project, 10);
    assert_eq!(roots(&f), vec![project, home]);
}

#[test]
fn a_symlinked_home_still_shows_as_a_tilde() {
    let mut f = Fixture::new();
    let home = f.root("real-home");
    let link = f.root("home-link");
    fs::create_dir(&home).unwrap();
    let home = home.canonicalize().unwrap();
    symlink(&home, &link).unwrap();
    f.sources.home = Some(link);
    f.stamp(&home, 40);
    let project = home.join("project");
    f.stamp(&project, 10);
    let shown: Vec<_> = rows(&f, FakeProbe::default())
        .into_iter()
        .map(|row| row.shown)
        .collect();
    assert_eq!(shown, vec!["~/project", "~"]);
}

#[test]
fn the_cut_comes_after_the_facts() {
    let f = Fixture::new();
    let mut probe = FakeProbe::default();
    for i in 0..40 {
        let root = f.root(format!("gone-{i}"));
        f.stamp(&root, 100 + i);
        probe.gone.insert(root);
    }
    for i in 0..35 {
        f.stamp(&f.root(format!("present-{i}")), 1 + i);
    }
    let found = rows(&f, probe);
    assert_eq!(found.len(), 35);
    assert!(found[..30].iter().all(|row| row.fact == Fact::Present));
    assert!(found[30..].iter().all(|row| row.fact == Fact::Gone));
    assert_eq!(found[0].path, f.root("present-34"));
    assert_eq!(found[29].path, f.root("present-5"));
    assert_eq!(found[30].path, f.root("gone-39"));
}

#[test]
fn no_state_directory_is_no_rows() {
    let sources = Sources::from_env(None, None, PathBuf::new());
    assert!(sources.layout.is_none());
    assert!(sources.tabs.is_none());
    assert!(sources.home.is_none());
    assert!(read(&sources).is_empty());
    let f = Fixture::new();
    fs::remove_dir_all(f.sources.layout.as_ref().unwrap()).unwrap();
    fs::remove_dir_all(f.sources.tabs.as_ref().unwrap()).unwrap();
    assert!(read(&f.sources).is_empty());
}

struct HeldProbe {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl Probe for HeldProbe {
    fn exists(&self, _: &Path) -> bool {
        self.entered.send(()).unwrap();
        let _ = self.release.recv();
        true
    }

    fn window_holds(&self, _: &Path) -> bool {
        false
    }
}

fn held() -> (HeldProbe, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    (
        HeldProbe {
            entered: entered_tx,
            release: release_rx,
        },
        entered_rx,
        release_tx,
    )
}

fn wait_idle(flag: &AtomicBool) {
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while flag.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < until,
            "the released worker must relinquish its slot"
        );
        std::thread::yield_now();
    }
}

#[test]
fn a_blocked_stat_reads_unknown_at_the_deadline() {
    let f = Fixture::new();
    let root = f.root("project");
    f.stamp(&root, 10);
    let (probe, entered, release) = held();
    let flag = busy();
    let (answer_tx, answer_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let found = read_with(&f.sources, Box::new(probe), Duration::from_millis(20), flag);
        answer_tx.send(found).unwrap();
        f
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let found = answer_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, root);
    assert_eq!(found[0].fact, Fact::Unknown);
    assert!(flag.load(Ordering::Acquire));
    release.send(()).unwrap();
    wait_idle(flag);
    let _fixture = thread.join().unwrap();
}

struct BlockAt {
    root: PathBuf,
    held: HeldProbe,
}

impl Probe for BlockAt {
    fn exists(&self, path: &Path) -> bool {
        path != self.root || self.held.exists(path)
    }

    fn window_holds(&self, _: &Path) -> bool {
        true
    }
}

#[test]
fn completed_facts_survive_a_later_blocked_stat() {
    let f = Fixture::new();
    let roots: Vec<_> = (0..3).map(|i| f.root(format!("project-{i}"))).collect();
    for (i, root) in roots.iter().enumerate() {
        f.stamp(root, 10 - i as u64);
    }
    let (held, entered, release) = held();
    let flag = busy();
    let probe = BlockAt {
        root: roots[1].clone(),
        held,
    };
    let found = read_with(&f.sources, Box::new(probe), Duration::from_millis(250), flag);
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        found.iter().map(|row| &row.path).collect::<Vec<_>>(),
        roots.iter().collect::<Vec<_>>()
    );
    assert_eq!(
        found.iter().map(|row| row.fact).collect::<Vec<_>>(),
        vec![Fact::Open, Fact::Unknown, Fact::Unknown]
    );
    release.send(()).unwrap();
    wait_idle(flag);
}

struct CountProbe(&'static AtomicBool);

impl Probe for CountProbe {
    fn exists(&self, _: &Path) -> bool {
        self.0.store(true, Ordering::Release);
        true
    }

    fn window_holds(&self, _: &Path) -> bool {
        false
    }
}

#[test]
fn no_second_fact_worker_while_one_is_stuck() {
    let f = Fixture::new();
    f.stamp(&f.root("project"), 10);
    let (probe, entered, release) = held();
    let flag = busy();
    let found = read_with(&f.sources, Box::new(probe), Duration::from_millis(20), flag);
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(found[0].fact, Fact::Unknown);
    let called = busy();
    let found = read_with(
        &f.sources,
        Box::new(CountProbe(called)),
        Duration::from_millis(20),
        flag,
    );
    assert_eq!(found[0].fact, Fact::Unknown);
    assert!(!called.load(Ordering::Acquire));
    release.send(()).unwrap();
    wait_idle(flag);
    let found = read_with(&f.sources, Box::new(CountProbe(called)), Duration::from_secs(5), flag);
    assert_eq!(found[0].fact, Fact::Present);
    assert!(called.load(Ordering::Acquire));
}

#[test]
fn facts_never_reorder_present_rows() {
    let f = Fixture::new();
    let roots: Vec<_> = (0..4).map(|i| f.root(format!("project-{i}"))).collect();
    for (i, root) in roots.iter().enumerate() {
        f.stamp(root, 10 - i as u64);
    }
    let probe = FakeProbe {
        gone: [roots[1].clone()].into(),
        open: [roots[3].clone()].into(),
    };
    let found = rows(&f, probe);
    assert_eq!(
        found.iter().map(|r| &r.path).collect::<Vec<_>>(),
        vec![&roots[0], &roots[2], &roots[3], &roots[1]]
    );
    assert_eq!(found[2].fact, Fact::Open);
}

#[test]
fn newest_present_skips_gone_and_unknown() {
    let make = |name, fact| Row::new(PathBuf::from(name), None, "0 m".into(), fact);
    let mut found = vec![make("/gone", Fact::Gone), make("/unknown", Fact::Unknown)];
    assert!(newest_present(&found).is_none());
    found.push(make("/open", Fact::Open));
    found.push(make("/present", Fact::Present));
    assert_eq!(newest_present(&found).unwrap().path, Path::new("/open"));
    found[2].fact = Fact::Gone;
    assert_eq!(newest_present(&found).unwrap().path, Path::new("/present"));
}
