#![cfg(target_os = "linux")]
#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use daemon::sync::modes::{self, Keeper, Keepers, Options};
use eitri_server_proto::*;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
const BINARY: &str = env!("CARGO_BIN_EXE_eitri-server");
const HOST: &str = "example-host";
const STAGING: &str = ".mutagen-temporary-staging-sync_test-beta";

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// A directory of the test's own (the root's parent, where Mutagen would stage), holding the project, a directory
/// outside it and a state directory; removed when the test ends.
struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "m6-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        for dir in ["project", "outside", "state"] {
            std::fs::create_dir(base.join(dir)).unwrap();
        }
        std::fs::set_permissions(base.join("state"), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { base }
    }

    fn project(&self) -> PathBuf {
        self.base.join("project")
    }

    fn root(&self) -> String {
        self.project().to_str().unwrap().to_owned()
    }

    fn state(&self) -> PathBuf {
        self.base.join("state")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.project().join(rel)
    }

    /// A file the way a person or the agent leaves it on the server.
    fn file(&self, rel: &str, bytes: &[u8], mode: u32) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        chmod(&path, mode);
    }

    fn keeper(&self, umask: u32) -> Keeper {
        self.keeper_with(umask, Box::new(|_: &str, _: bool| false), None)
    }

    fn keeper_with(&self, umask: u32, ignored: modes::Ignore, watch_limit: Option<usize>) -> Keeper {
        self.keeper_named(Some(STAGING), umask, ignored, watch_limit)
    }

    fn keeper_named(
        &self,
        staging: Option<&str>,
        umask: u32,
        ignored: modes::Ignore,
        watch_limit: Option<usize>,
    ) -> Keeper {
        let dir = modes::sync_dir(&self.state(), &self.root());
        std::fs::create_dir_all(&dir).unwrap();
        // A keeper writes its record only while its registration is there.
        std::fs::write(dir.join("watch"), b"{}").unwrap();
        let keeper = Keeper::start(
            &self.root(),
            staging.map(str::to_owned),
            ignored,
            options(umask, watch_limit),
            Some(dir),
        )
        .unwrap();
        keeper.settle();
        keeper
    }

    fn staging(&self) -> PathBuf {
        self.base.join(STAGING)
    }

    /// Stages a file as Mutagen does beside the root: a `storage<n>` file in the staging directory, given its mode
    /// there and renamed into a `<2 hex>` subdirectory under its digest. Returns the staged path.
    fn stage(&self, bytes: &[u8], mode: u32) -> PathBuf {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let staging = self.staging();
        let _ = std::fs::create_dir(&staging);
        let storage = staging.join(format!("storage{serial}"));
        std::fs::write(&storage, bytes).unwrap();
        chmod(&storage, mode);
        let sub = staging.join(format!("{:02x}", serial % 256));
        let _ = std::fs::create_dir(&sub);
        let staged = sub.join(format!("{serial:064x}"));
        std::fs::rename(&storage, &staged).unwrap();
        staged
    }

    /// Removes the staging directory, as Mutagen does after a cycle.
    fn end_cycle(&self) {
        let _ = std::fs::remove_dir_all(self.staging());
    }

    /// A Mutagen write of `rel`: staged, then (once the keeper has seen the staging, as it has during the time Mutagen
    /// takes between staging a batch and applying it) renamed into place.
    fn mutagen_write(&self, keeper: &Keeper, rel: &str, bytes: &[u8], mode: u32) {
        let staged = self.stage(bytes, mode);
        keeper.settle();
        std::fs::rename(staged, self.path(rel)).unwrap();
        self.end_cycle();
        keeper.settle();
    }

    /// [`Self::mutagen_write`] for a keeper reached through a registry.
    fn mutagen_write_via(&self, settle: impl Fn(), rel: &str, bytes: &[u8], mode: u32) {
        let staged = self.stage(bytes, mode);
        settle();
        std::fs::rename(staged, self.path(rel)).unwrap();
        self.end_cycle();
        settle();
    }

    /// A Mutagen write when the root is on another file system than its staging: during a cycle, a copy made in the
    /// target directory and renamed into place.
    fn cross_device_write(&self, keeper: &Keeper, rel: &str, bytes: &[u8], mode: u32) {
        // Mutagen stages the file first; the rename out of staging then fails across devices and it copies instead.
        let _staged = self.stage(bytes, 0o600);
        let target = self.path(rel);
        let copy = target
            .parent()
            .unwrap()
            .join(format!("{}4262986106793742329", modes::CROSS_DEVICE_PREFIX));
        std::fs::write(&copy, bytes).unwrap();
        chmod(&copy, mode);
        std::fs::rename(&copy, &target).unwrap();
        self.end_cycle();
        keeper.settle();
    }

    fn modes_file(&self) -> std::collections::BTreeMap<String, u32> {
        modes::read_modes_file(&modes::sync_dir(&self.state(), &self.root()).join("modes")).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.base, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn options(umask: u32, watch_limit: Option<usize>) -> Options {
    Options {
        umask,
        host: HOST.to_owned(),
        watch_limit,
        watches: Default::default(),
    }
}

/// Makes the daemon with run key `key` look running to the keepers: its `info` names this (live) process.
fn running_daemon(state: &Path, key: &str) {
    let dir = state.join("run").join(key);
    agent::private_fs::create_private_dir_all(&dir, state).unwrap();
    agent::private_fs::write_private(
        &dir.join("info"),
        format!("{{\"pid\":{}}}", std::process::id()).as_bytes(),
    )
    .unwrap();
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn a_mutagen_rename_over_0644_restores_0644() {
    let scratch = Scratch::new("r644");
    scratch.file("src/a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    assert_eq!(keeper.recorded("src/a.txt"), Some(0o644));
    scratch.mutagen_write(&keeper, "src/a.txt", b"one, saved on the laptop", 0o600);
    assert_eq!(
        std::fs::read(scratch.path("src/a.txt")).unwrap(),
        b"one, saved on the laptop"
    );
    assert_eq!(mode(&scratch.path("src/a.txt")), 0o644);
    assert_eq!(keeper.recorded("src/a.txt"), Some(0o644));
    assert_eq!(keeper.status().restored, 1);
    assert_eq!(keeper.status().unpaired, 0);
    assert_eq!(scratch.modes_file().get("src/a.txt"), Some(&0o644));
    assert!(keeper.warnings().is_empty(), "{:?}", keeper.warnings());
}

/// Mutagen makes its staging directory when a cycle starts and places a file the moment it is staged, with no pause a
/// watch added after the directory appears could use. The keeper keeps the named staging directory and its `<2 hex>`
/// subdirectories ready and watched between cycles, so such a write is still paired, cycle after cycle.
#[test]
fn a_mutagen_write_with_no_pause_between_staging_and_placing_is_restored() {
    let scratch = Scratch::new("nopause");
    scratch.file("a.txt", b"one", 0o644);
    scratch.file("b.txt", b"two", 0o640);
    let keeper = scratch.keeper(0o022);
    for (rel, want, text) in [
        ("a.txt", 0o644, &b"saved once"[..]),
        ("b.txt", 0o640, b"saved twice"),
        ("a.txt", 0o644, b"saved again"),
    ] {
        let staged = scratch.stage(text, 0o600);
        std::fs::rename(staged, scratch.path(rel)).unwrap();
        scratch.end_cycle();
        keeper.settle();
        assert_eq!(mode(&scratch.path(rel)), want, "{rel}");
    }
    let status = keeper.status();
    assert_eq!((status.restored, status.unpaired, status.unpaired_in_sync), (3, 0, 0));
    assert!(keeper.warnings().is_empty(), "{:?}", keeper.warnings());
}

#[test]
fn the_named_staging_directory_is_kept_ready_and_removed_when_the_keeper_stops() {
    let scratch = Scratch::new("ready");
    let keeper = scratch.keeper(0o022);
    assert_eq!(mode(&scratch.staging()), 0o700);
    assert!(scratch.staging().join("00").is_dir() && scratch.staging().join("ff").is_dir());
    // Mutagen removes it when a cycle ends; it is made again for the next one.
    scratch.end_cycle();
    keeper.settle();
    assert!(scratch.staging().join("c7").is_dir());
    drop(keeper);
    assert!(!scratch.staging().exists());
    // One Mutagen is using when the keeper stops is left to Mutagen.
    let keeper = scratch.keeper(0o022);
    let _staged = scratch.stage(b"in flight", 0o600);
    keeper.settle();
    drop(keeper);
    for byte in 0..=u8::MAX {
        assert!(scratch.staging().join(format!("{byte:02x}")).is_dir(), "{byte:02x}");
    }
    scratch.end_cycle();
    // Without a staging name nothing is made beside the root.
    let keeper = scratch.keeper_named(None, 0o022, Box::new(|_: &str, _: bool| false), None);
    let beside: Vec<_> = std::fs::read_dir(&scratch.base)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(modes::STAGING_PREFIX))
        .collect();
    assert!(beside.is_empty(), "{beside:?}");
    drop(keeper);
}

/// The keeper holds its root open, so removing the root sends the root's own watch nothing; the parent's event for the
/// root's name stops it, and a daemon's next look starts a keeper on the folder now at that path.
#[test]
fn a_root_removed_and_made_again_is_kept_afresh() {
    let scratch = Scratch::new("remade");
    scratch.file("a.txt", b"one", 0o644);
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    keepers.set(&scratch.root(), Vec::new(), STAGING, true).unwrap();
    keepers.with_keeper(&scratch.root(), Keeper::settle).unwrap();
    std::fs::remove_dir_all(scratch.project()).unwrap();
    std::fs::create_dir(scratch.project()).unwrap();
    wait_for(|| {
        keepers
            .with_keeper(&scratch.root(), |keeper| !keeper.running())
            .unwrap_or(true)
    });
    let lines = keepers.hello_warnings();
    assert!(
        lines
            .iter()
            .any(|line| line.text.contains("the root was moved or removed")),
        "{lines:?}"
    );
    scratch.file("b.txt", b"two", 0o640);
    keepers.resume();
    let settle = || keepers.with_keeper(&scratch.root(), Keeper::settle).unwrap();
    settle();
    assert_eq!(
        keepers.with_keeper(&scratch.root(), |keeper| keeper.recorded("b.txt")),
        Some(Some(0o640))
    );
    scratch.mutagen_write_via(settle, "b.txt", b"two, saved", 0o600);
    assert_eq!(mode(&scratch.path("b.txt")), 0o640);
}

/// A re-created session stages under a new name: the running keeper takes it in place (no new walk, so its counted
/// line keeps its epoch and sequence), the directory kept ready for the old name goes, and a write with no pause
/// through the new name is paired.
#[test]
fn a_new_staging_name_is_taken_by_the_running_keeper() {
    let scratch = Scratch::new("rename");
    scratch.file("a.txt", b"one", 0o644);
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    keepers.set(&scratch.root(), Vec::new(), STAGING, true).unwrap();
    let settle = || keepers.with_keeper(&scratch.root(), Keeper::settle).unwrap();
    settle();
    // A counted line, so a restarted keeper (which counts from nothing under a new epoch) would show.
    let _cycle = scratch.stage(b"cycle", 0o600);
    settle();
    let outside = scratch.base.join("outside/x");
    std::fs::write(&outside, b"x").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("landed.txt")).unwrap();
    settle();
    scratch.end_cycle();
    settle();
    let before = keepers.hello_warnings();
    assert_eq!(before.len(), 1, "{before:?}");
    const OTHER: &str = ".mutagen-temporary-staging-sync_other-beta";
    keepers.set(&scratch.root(), Vec::new(), OTHER, true).unwrap();
    settle();
    let after = keepers.hello_warnings();
    assert_eq!(
        (after[0].epoch, after[0].sequence),
        (before[0].epoch, before[0].sequence)
    );
    assert!(!scratch.staging().exists());
    let other = scratch.base.join(OTHER);
    assert!(other.join("c7").is_dir());
    std::fs::write(other.join("storage1"), b"two").unwrap();
    chmod(&other.join("storage1"), 0o600);
    std::fs::rename(other.join("storage1"), other.join("c7/c7aa")).unwrap();
    std::fs::rename(other.join("c7/c7aa"), scratch.path("a.txt")).unwrap();
    std::fs::remove_dir_all(&other).unwrap();
    settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o644);
}

/// A neighbouring root's staging directory kept ready is no cycle here: a rename from elsewhere is counted as
/// outside a cycle, and its empty subdirectories are not watched.
#[test]
fn a_neighbours_ready_staging_directory_is_no_cycle() {
    let scratch = Scratch::new("neighbour");
    scratch.file("a.txt", b"one", 0o644);
    std::fs::create_dir(scratch.base.join("q")).unwrap();
    let neighbour = Keeper::start(
        scratch.base.join("q").to_str().unwrap(),
        Some(STAGING.to_owned()),
        Box::new(|_: &str, _: bool| false),
        options(0o022, None),
        None,
    )
    .unwrap();
    neighbour.settle();
    assert!(scratch.staging().join("00").is_dir());
    let keeper = scratch.keeper_named(None, 0o022, Box::new(|_: &str, _: bool| false), None);
    let outside = scratch.base.join("outside/x");
    std::fs::write(&outside, b"x").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("a.txt")).unwrap();
    keeper.settle();
    let status = keeper.status();
    assert_eq!((status.unpaired, status.unpaired_in_sync), (1, 0), "{status:?}");
    drop(keeper);
    drop(neighbour);
    assert!(!scratch.staging().exists());
}

/// Changed patterns start a new keeper, whose first walk takes the modes it finds; the registration answers only
/// after that walk, so a sync write right after it (the laptop creates the session once the patterns are registered)
/// is judged, not adopted.
#[test]
fn a_pattern_change_answers_after_the_new_keepers_walk() {
    let scratch = Scratch::new("repattern");
    for n in 0..3000 {
        scratch.file(&format!("bulk/{:02}/f{n}", n % 50), b"x", 0o644);
    }
    scratch.file("a.txt", b"one", 0o644);
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    keepers.set(&scratch.root(), Vec::new(), STAGING, true).unwrap();
    keepers.with_keeper(&scratch.root(), Keeper::settle).unwrap();
    keepers
        .set(&scratch.root(), vec!["/target/".to_owned()], STAGING, true)
        .unwrap();
    let staged = scratch.stage(b"saved", 0o600);
    std::fs::rename(staged, scratch.path("a.txt")).unwrap();
    scratch.end_cycle();
    keepers.with_keeper(&scratch.root(), Keeper::settle).unwrap();
    assert_eq!(mode(&scratch.path("a.txt")), 0o644);
}

/// A registration waiting for its keeper to settle holds no lock the daemon's other calls take: the idle check and
/// Hello answer meanwhile.
#[test]
fn a_registration_waiting_for_its_keeper_blocks_no_other_call() {
    let scratch = Scratch::new("waitlock");
    scratch.file("a.txt", b"one", 0o644);
    let root = scratch.root();
    let keepers = Arc::new(
        Keepers::new(&scratch.state(), options(0o022, None), "run-a").with_register_settle(Duration::from_secs(120)),
    );
    keepers.set(&root, Vec::new(), STAGING, true).unwrap();
    let asked = keepers.with_keeper(&root, Keeper::settles_asked).unwrap();
    let paused = keepers.with_keeper(&root, Keeper::paused).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let waiting = {
        let (keepers, done, root) = (Arc::clone(&keepers), Arc::clone(&done), root.clone());
        std::thread::spawn(move || {
            let answer = keepers.set(&root, Vec::new(), STAGING, true);
            done.store(true, Ordering::SeqCst);
            answer
        })
    };
    while keepers.with_keeper(&root, Keeper::settles_asked).unwrap() == asked {
        std::thread::yield_now();
    }
    // Waiting under the locks, these would answer only after the registration's whole wait.
    let (answered, answers) = std::sync::mpsc::channel();
    {
        let keepers = Arc::clone(&keepers);
        std::thread::spawn(move || {
            let holds = keepers.holds();
            let _ = keepers.hello_warnings();
            let _ = answered.send(holds);
        });
    }
    assert_eq!(answers.recv_timeout(Duration::from_secs(20)), Ok(true));
    assert!(!done.load(Ordering::SeqCst), "the registration still waits");
    drop(paused);
    assert_eq!(waiting.join().unwrap(), Ok(true));
}

/// A registration whose keeper has not settled within the wait says so: the laptop then waits before it creates the
/// session the registration is for.
#[test]
fn a_registration_says_when_its_keeper_has_not_settled() {
    let scratch = Scratch::new("unsettled");
    scratch.file("a.txt", b"one", 0o644);
    let root = scratch.root();
    let keepers =
        Keepers::new(&scratch.state(), options(0o022, None), "run-a").with_register_settle(Duration::from_millis(100));
    keepers.set(&root, Vec::new(), STAGING, true).unwrap();
    let paused = keepers.with_keeper(&root, Keeper::paused).unwrap();
    assert_eq!(keepers.set(&root, Vec::new(), STAGING, true), Ok(false));
    drop(paused);
    assert_eq!(keepers.set(&root, Vec::new(), STAGING, true), Ok(true));
}

/// While another daemon still holds a root's keeper, nothing keeps it under this registration yet: not settled.
#[test]
fn a_root_another_daemon_still_keeps_is_not_settled_here() {
    let scratch = Scratch::new("handover");
    scratch.file("a.txt", b"one", 0o644);
    let root = scratch.root();
    running_daemon(&scratch.state(), "run-old");
    let old = Keepers::new(&scratch.state(), options(0o022, None), "run-old");
    old.set(&root, Vec::new(), STAGING, true).unwrap();
    running_daemon(&scratch.state(), "run-new");
    let new = Keepers::new(&scratch.state(), options(0o022, None), "run-new")
        .with_register_settle(Duration::from_millis(300));
    assert_eq!(new.set(&root, Vec::new(), STAGING, true), Ok(false));
    // The old daemon lets the root go at its next tick, while a registration waits: that registration takes it.
    let new = Arc::new(
        Keepers::new(&scratch.state(), options(0o022, None), "run-new").with_register_settle(Duration::from_secs(30)),
    );
    let waiting = {
        let (new, root) = (Arc::clone(&new), root.clone());
        std::thread::spawn(move || new.set(&root, Vec::new(), STAGING, true))
    };
    std::thread::sleep(Duration::from_millis(300));
    old.resume();
    assert!(!old.keeps(&root));
    assert_eq!(waiting.join().unwrap(), Ok(true));
    assert!(new.keeps(&root));
}

/// A registration waiting for another daemon to let the root go stops when the root is turned off meanwhile, or the
/// daemon stops its keepers: nothing is kept for it afterwards.
#[test]
fn a_registration_waiting_for_the_root_keeps_nothing_after_off_or_stop() {
    for stop in [false, true] {
        let scratch = Scratch::new("waitoff");
        scratch.file("a.txt", b"one", 0o644);
        let root = scratch.root();
        running_daemon(&scratch.state(), "run-old");
        let old = Keepers::new(&scratch.state(), options(0o022, None), "run-old");
        old.set(&root, Vec::new(), STAGING, true).unwrap();
        running_daemon(&scratch.state(), "run-new");
        let new = Arc::new(
            Keepers::new(&scratch.state(), options(0o022, None), "run-new")
                .with_register_settle(Duration::from_secs(30)),
        );
        let waiting = {
            let (new, root) = (Arc::clone(&new), root.clone());
            std::thread::spawn(move || new.set(&root, Vec::new(), STAGING, true))
        };
        std::thread::sleep(Duration::from_millis(300));
        if stop {
            new.stop_all();
        } else {
            new.set(&root, Vec::new(), "", false).unwrap();
        }
        // The old daemon lets the root go at its next tick (no registration now names it, or the new one runs).
        old.resume();
        assert!(!old.keeps(&root));
        assert_eq!(waiting.join().unwrap(), Ok(false), "stop {stop}");
        assert!(!new.keeps(&root), "stop {stop}");
    }
}

/// A daemon that stopped its keepers starts none at a later tick either.
#[test]
fn a_tick_after_stop_all_starts_no_keeper() {
    let scratch = Scratch::new("stoptick");
    scratch.file("a.txt", b"one", 0o644);
    let root = scratch.root();
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    keepers.set(&root, Vec::new(), STAGING, true).unwrap();
    keepers.stop_all();
    keepers.resume();
    assert!(!keepers.keeps(&root));
}

/// A keeper followed every staging directory while it had no name; once named, a sibling's staging directory it
/// followed then is not this root's: its file renamed here is not restored.
#[test]
fn a_sibling_followed_before_the_name_came_never_pairs() {
    let scratch = Scratch::new("namedlate");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper_named(None, 0o022, Box::new(|_: &str, _: bool| false), None);
    let sibling = scratch.base.join(".mutagen-temporary-staging-sync_other-beta");
    std::fs::create_dir_all(sibling.join("ab")).unwrap();
    let storage = sibling.join("storage1");
    std::fs::write(&storage, b"other").unwrap();
    chmod(&storage, 0o600);
    std::fs::rename(&storage, sibling.join("ab/f")).unwrap();
    keeper.settle();
    keeper.set_staging(Some(STAGING.to_owned())).unwrap();
    keeper.settle();
    std::fs::rename(sibling.join("ab/f"), scratch.path("a.txt")).unwrap();
    keeper.settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    std::fs::remove_dir_all(&sibling).unwrap();
    keeper.settle();
}

/// A registration that names no staging directory (the laptop does not know the session's yet) keeps the one the
/// root has, so the keeper never goes back to following every staging directory beside it.
#[test]
fn a_registration_with_no_staging_name_keeps_the_roots_name() {
    let scratch = Scratch::new("noname");
    scratch.file("a.txt", b"one", 0o644);
    let root = scratch.root();
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    keepers.set(&root, Vec::new(), STAGING, true).unwrap();
    keepers.with_keeper(&root, Keeper::settle).unwrap();
    assert!(scratch.staging().join("00").is_dir());
    keepers.set(&root, vec!["/target/".to_owned()], "", true).unwrap();
    keepers.with_keeper(&root, Keeper::settle).unwrap();
    assert!(
        scratch.staging().join("00").is_dir(),
        "a new keeper keeps the root's name"
    );
    keepers.set(&root, vec!["/target/".to_owned()], "", true).unwrap();
    keepers.with_keeper(&root, Keeper::settle).unwrap();
    assert!(
        scratch.staging().join("00").is_dir(),
        "the running keeper keeps its name"
    );
}

/// A neighbour's directory kept ready, first seen while the neighbour was still making its subdirectories, is no cycle
/// once it has all 256 of them.
#[test]
fn a_neighbours_ready_directory_seen_half_made_is_no_cycle() {
    let scratch = Scratch::new("halfmade");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper_named(None, 0o022, Box::new(|_: &str, _: bool| false), None);
    let ready = scratch.base.join(".mutagen-temporary-staging-sync_next-beta");
    {
        let _held = keeper.hold();
        std::fs::create_dir(&ready).unwrap();
        for n in 0..40 {
            std::fs::create_dir(ready.join(format!("{n:02x}"))).unwrap();
        }
    }
    keeper.settle();
    for n in 40..256 {
        std::fs::create_dir(ready.join(format!("{n:02x}"))).unwrap();
    }
    keeper.settle();
    let outside = scratch.base.join("outside/x");
    std::fs::write(&outside, b"x").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("a.txt")).unwrap();
    keeper.settle();
    let status = keeper.status();
    assert_eq!((status.unpaired, status.unpaired_in_sync), (1, 0), "{status:?}");
    std::fs::remove_dir_all(&ready).unwrap();
    keeper.settle();
}

/// The keeper removes the directory it kept ready only while it is as it made it: a file Mutagen staged straight into
/// a subdirectory (which leaves the top directory's change time as it was) keeps every subdirectory.
#[test]
fn a_ready_directory_with_a_file_in_a_subdirectory_is_left_when_the_keeper_stops() {
    let scratch = Scratch::new("leftready");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    std::fs::write(scratch.staging().join("ab/f"), b"staged").unwrap();
    keeper.settle();
    drop(keeper);
    assert!(scratch.staging().join("ab/f").is_file());
    assert!(scratch.staging().join("00").is_dir());
    scratch.end_cycle();
}

/// A staging directory Mutagen made and already used (an empty `<2 hex>` left after a one-file cycle outran the watch)
/// is a cycle; one found empty or holding exactly the 256 empty subdirectories of a directory kept ready is not.
#[test]
fn a_used_staging_directory_found_late_is_a_cycle() {
    let scratch = Scratch::new("late");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper_named(None, 0o022, Box::new(|_: &str, _: bool| false), None);
    let staging = scratch.base.join(".mutagen-temporary-staging-sync_late-beta");
    {
        let _held = keeper.hold();
        std::fs::create_dir_all(staging.join("3f")).unwrap();
    }
    keeper.settle();
    let outside = scratch.base.join("outside/x");
    std::fs::write(&outside, b"x").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("a.txt")).unwrap();
    keeper.settle();
    let status = keeper.status();
    assert_eq!((status.unpaired, status.unpaired_in_sync), (1, 1), "{status:?}");
    std::fs::remove_dir_all(&staging).unwrap();
    keeper.settle();
}

#[test]
fn a_mutagen_rename_over_0640_restores_0640() {
    let scratch = Scratch::new("r640");
    scratch.file("a.txt", b"one", 0o640);
    let keeper = scratch.keeper(0o022);
    scratch.mutagen_write(&keeper, "a.txt", b"two", 0o600);
    assert_eq!(mode(&scratch.path("a.txt")), 0o640);
    assert_eq!(keeper.recorded("a.txt"), Some(0o640));
}

#[test]
fn a_new_mutagen_file_gets_the_umask_mode() {
    for (umask, expected) in [(0o022, 0o644), (0o027, 0o640), (0o077, 0o600)] {
        let scratch = Scratch::new("new");
        let keeper = scratch.keeper(umask);
        scratch.mutagen_write(&keeper, "new.txt", b"new", 0o600);
        assert_eq!(mode(&scratch.path("new.txt")), expected, "umask {umask:o}");
        assert_eq!(keeper.recorded("new.txt"), Some(expected));
    }
}

#[test]
fn a_new_executable_from_mutagen_keeps_its_x_bits_under_the_umask() {
    for (umask, expected) in [(0o022, 0o755), (0o027, 0o750), (0o077, 0o700)] {
        let scratch = Scratch::new("newx");
        let keeper = scratch.keeper(umask);
        scratch.mutagen_write(&keeper, "tool.sh", b"#!/bin/sh\n", 0o700);
        assert_eq!(mode(&scratch.path("tool.sh")), expected, "umask {umask:o}");
    }
}

#[test]
fn an_agent_chmod_becomes_the_record() {
    let scratch = Scratch::new("chmod");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    chmod(&scratch.path("a.txt"), 0o600);
    keeper.settle();
    assert_eq!(keeper.recorded("a.txt"), Some(0o600));
    // The narrower mode the agent chose is never widened back to the older one.
    scratch.mutagen_write(&keeper, "a.txt", b"two", 0o600);
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    chmod(&scratch.path("a.txt"), 0o640);
    keeper.settle();
    assert_eq!(keeper.recorded("a.txt"), Some(0o640));
    scratch.mutagen_write(&keeper, "a.txt", b"three", 0o600);
    assert_eq!(mode(&scratch.path("a.txt")), 0o640);
    // The agent's own new file is recorded as it made it.
    scratch.file("made.txt", b"x", 0o600);
    keeper.settle();
    assert_eq!(keeper.recorded("made.txt"), Some(0o600));
}

#[test]
fn a_rename_from_outside_the_staging_dir_is_not_restored() {
    let scratch = Scratch::new("intree");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    // An editor's or the agent's write through a temporary file inside the tree.
    scratch.file(".a.txt.swp", b"secret", 0o600);
    std::fs::rename(scratch.path(".a.txt.swp"), scratch.path("a.txt")).unwrap();
    keeper.settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    assert_eq!(keeper.recorded("a.txt"), Some(0o600));
    assert_eq!(keeper.recorded(".a.txt.swp"), None);
    // The same while Mutagen is staging beside the root: a name that is not Mutagen's is still somebody else's.
    let staged = scratch.stage(b"unused", 0o600);
    keeper.settle();
    scratch.file("tmp.b", b"b", 0o600);
    scratch.file("b.txt", b"old", 0o644);
    keeper.settle();
    std::fs::rename(scratch.path("tmp.b"), scratch.path("b.txt")).unwrap();
    keeper.settle();
    assert_eq!(mode(&scratch.path("b.txt")), 0o600);
    drop(staged);
    scratch.end_cycle();
    keeper.settle();
    assert_eq!(keeper.status().restored, 0);
}

#[test]
fn an_unpaired_rename_is_left_alone_and_counted() {
    let scratch = Scratch::new("unpaired");
    scratch.file("a.txt", b"one", 0o644);
    scratch.file("b.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    let outside = scratch.base.join("outside/x");
    std::fs::write(&outside, b"from elsewhere").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("a.txt")).unwrap();
    keeper.settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    assert_eq!(keeper.recorded("a.txt"), Some(0o600));
    let status = keeper.status();
    assert_eq!((status.unpaired, status.unpaired_in_sync, status.restored), (1, 0, 0));
    assert!(keeper.warnings().is_empty());
    // During a sync cycle such a rename may have been Mutagen's: still left alone, and the person is told.
    let _staged = scratch.stage(b"unused", 0o600);
    keeper.settle();
    std::fs::write(&outside, b"again").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("b.txt")).unwrap();
    keeper.settle();
    scratch.end_cycle();
    keeper.settle();
    assert_eq!(mode(&scratch.path("b.txt")), 0o600);
    let status = keeper.status();
    assert_eq!((status.unpaired, status.unpaired_in_sync, status.restored), (2, 1, 0));
    let warnings = keeper.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].sequence, 1);
    assert_eq!(
        (warnings[0].host.as_str(), warnings[0].root.clone()),
        (HOST, scratch.root())
    );
    assert_eq!(warnings[0].paths, vec!["b.txt".to_owned()]);
    assert!(
        warnings[0].text.starts_with(&format!(
            "file modes on {HOST}: 1 sync writes into {} were left as Mutagen wrote them",
            scratch.root()
        )),
        "{warnings:?}"
    );
    assert!(warnings[0].text.ends_with(": b.txt"), "{warnings:?}");
    // Asking again gives the same line: delivering it changes nothing.
    assert_eq!(keeper.warnings(), warnings);
    // A restarted keeper counts from 1 again, under a new epoch.
    drop(keeper);
    let keeper = scratch.keeper(0o022);
    let _staged = scratch.stage(b"unused", 0o600);
    keeper.settle();
    std::fs::write(&outside, b"once more").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("a.txt")).unwrap();
    keeper.settle();
    scratch.end_cycle();
    keeper.settle();
    let restarted = keeper.warnings();
    assert_eq!(restarted[0].sequence, 1);
    assert_ne!(restarted[0].epoch, warnings[0].epoch);
    assert!(warnings[0].epoch != 0 && restarted[0].epoch != 0);
}

#[test]
fn counted_lines_repeat_with_growing_sequences_and_name_the_last_twenty() {
    let scratch = Scratch::new("ring");
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    keepers.set(&scratch.root(), Vec::new(), STAGING, true).unwrap();
    let settle = || keepers.with_keeper(&scratch.root(), Keeper::settle).unwrap();
    settle();
    // A cycle: Mutagen stages in the staging directory the keeper keeps ready.
    let _cycle = scratch.stage(b"cycle", 0o600);
    settle();
    let outside = scratch.base.join("outside/x");
    let land = |rel: &str| {
        std::fs::write(&outside, b"x").unwrap();
        chmod(&outside, 0o600);
        std::fs::rename(&outside, scratch.path(rel)).unwrap();
    };
    for n in 0..25 {
        land(&format!("f{n:02}"));
    }
    settle();
    // Two Hellos (two windows, or one lost on a dropped connection and asked again) carry the same line.
    let first = keepers.hello_warnings();
    assert_eq!(keepers.hello_warnings(), first);
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].sequence, 25);
    let named: Vec<String> = (5..25).map(|n| format!("f{n:02}")).collect();
    assert_eq!(first[0].paths, named);
    assert!(first[0].text.contains("; the last: f05, "), "{first:?}");
    land("f25");
    settle();
    let later = keepers.hello_warnings();
    assert_eq!(later[0].sequence, 26);
    assert_eq!(later[0].paths.len(), modes::MAX_NAMED);
    assert_eq!(later[0].paths.last().map(String::as_str), Some("f25"));
    scratch.end_cycle();
}

#[test]
fn a_root_whose_registrant_is_gone_is_kept_by_another_daemon() {
    let scratch = Scratch::new("orphan");
    scratch.file("a.txt", b"one", 0o644);
    let root = scratch.root();
    running_daemon(&scratch.state(), "run-old");
    let old = Keepers::new(&scratch.state(), options(0o022, None), "run-old");
    let other = Keepers::new(&scratch.state(), options(0o022, None), "run-other");
    let third = Keepers::new(&scratch.state(), options(0o022, None), "run-third");
    old.set(&root, Vec::new(), STAGING, true).unwrap();
    other.resume();
    assert!(old.keeps(&root) && !other.keeps(&root));
    // The registering daemon goes (its keeper and its `info` with it): another daemon takes the root up.
    old.stop_all();
    std::fs::remove_file(scratch.state().join("run/run-old/info")).unwrap();
    other.resume();
    assert!(other.keeps(&root) && other.holds());
    third.resume();
    assert!(!third.keeps(&root));
    // A daemon that cannot take it passes the keeper's lines on; with none, it says nothing while another daemon holds
    // the keeper lock (its keeper may still be walking), and says nobody keeps the root once nobody does.
    other.with_keeper(&root, Keeper::settle).unwrap();
    std::fs::remove_file(modes::sync_dir(&scratch.state(), &root).join("status")).unwrap();
    assert!(third.hello_warnings().is_empty());
    other.stop_all();
    let lines = third.hello_warnings();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        lines[0].text,
        format!("file modes on {HOST} are not kept for {root}: the daemon that registered it is not running")
    );
    // Both daemons come back (new processes, so new keepers): the other one takes the root while the registering daemon
    // is away, and leaves it to it once it runs again.
    let other = Keepers::new(&scratch.state(), options(0o022, None), "run-other");
    other.resume();
    assert!(other.keeps(&root));
    running_daemon(&scratch.state(), "run-old");
    other.resume();
    assert!(!other.keeps(&root));
    let old = Keepers::new(&scratch.state(), options(0o022, None), "run-old");
    old.resume();
    assert!(old.keeps(&root));
}

#[test]
fn keepers_share_one_watch_budget() {
    let one = Scratch::new("budget1");
    let two = Scratch::new("budget2");
    one.file("a/x", b"x", 0o644);
    two.file("b/x", b"x", 0o644);
    two.file("c/x", b"x", 0o644);
    let shared = options(0o022, Some(4));
    let start = |scratch: &Scratch| {
        let dir = modes::sync_dir(&scratch.state(), &scratch.root());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("watch"), b"{}").unwrap();
        let keeper = Keeper::start(
            &scratch.root(),
            Some(STAGING.to_owned()),
            Box::new(|_: &str, _: bool| false),
            shared.clone(),
            Some(dir),
        )
        .unwrap();
        keeper.settle();
        keeper
    };
    let first = start(&one);
    assert_eq!(shared.watches.load(Ordering::SeqCst), 2);
    let second = start(&two);
    assert_eq!(shared.watches.load(Ordering::SeqCst), 4);
    assert!(second.status().partial.is_some(), "{:?}", second.status());
    assert!(first.status().partial.is_none());
    // A keeper that stops gives its watches back.
    drop(first);
    assert_eq!(shared.watches.load(Ordering::SeqCst), 2);
    drop(second);
    assert_eq!(shared.watches.load(Ordering::SeqCst), 0);
}

#[test]
fn executability_follows_the_laptop_and_the_record_keeps_the_rest() {
    let scratch = Scratch::new("xbits");
    scratch.file("run.sh", b"#!/bin/sh\n", 0o755);
    scratch.file("notes.txt", b"n", 0o640);
    let keeper = scratch.keeper(0o022);
    // A save of an executable: the record comes back whole.
    scratch.mutagen_write(&keeper, "run.sh", b"#!/bin/sh\necho\n", 0o700);
    assert_eq!(mode(&scratch.path("run.sh")), 0o755);
    // The laptop dropped the executable bit: the record's read and write bits, nothing executable.
    scratch.mutagen_write(&keeper, "run.sh", b"#!/bin/sh\necho 2\n", 0o600);
    assert_eq!(mode(&scratch.path("run.sh")), 0o644);
    // The laptop made a file executable: the record's read and write bits and owner-execute, nothing more.
    scratch.mutagen_write(&keeper, "notes.txt", b"#!/bin/sh\n", 0o700);
    assert_eq!(mode(&scratch.path("notes.txt")), 0o740);
    assert_eq!(modes::wanted(Some(0o755), 0o700, 0o022), 0o755);
    assert_eq!(modes::wanted(Some(0o755), 0o600, 0o022), 0o644);
    assert_eq!(modes::wanted(Some(0o640), 0o700, 0o022), 0o740);
    assert_eq!(modes::wanted(None, 0o700, 0o022), 0o755);
}

#[test]
fn a_cross_device_rename_is_restored() {
    let scratch = Scratch::new("xdev");
    scratch.file("lib/a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    scratch.cross_device_write(&keeper, "lib/a.txt", b"two", 0o600);
    assert_eq!(mode(&scratch.path("lib/a.txt")), 0o644);
    scratch.cross_device_write(&keeper, "lib/new.txt", b"new", 0o600);
    assert_eq!(mode(&scratch.path("lib/new.txt")), 0o644);
    assert!(keeper
        .hold()
        .keys()
        .all(|rel| !rel.contains(modes::CROSS_DEVICE_PREFIX)));
    assert_eq!(keeper.status().restored, 2);
}

#[test]
fn a_cross_device_name_reached_by_a_rename_is_not_mutagens() {
    let scratch = Scratch::new("xname");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    // A cycle: Mutagen stages in the staging directory the keeper keeps ready.
    let _cycle = scratch.stage(b"cycle", 0o600);
    keeper.settle();
    // A file from elsewhere given Mutagen's temporary name, then renamed over a file: nobody's copy.
    let outside = scratch.base.join("outside/secret");
    std::fs::write(&outside, b"secret").unwrap();
    chmod(&outside, 0o600);
    let copy = scratch.path(&format!("{}1", modes::CROSS_DEVICE_PREFIX));
    std::fs::rename(&outside, &copy).unwrap();
    keeper.settle();
    std::fs::rename(&copy, scratch.path("a.txt")).unwrap();
    keeper.settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    // Nor is one made outside a sync cycle.
    scratch.end_cycle();
    keeper.settle();
    scratch.file(&format!("{}2", modes::CROSS_DEVICE_PREFIX), b"x", 0o600);
    std::fs::rename(
        scratch.path(&format!("{}2", modes::CROSS_DEVICE_PREFIX)),
        scratch.path("b.txt"),
    )
    .unwrap();
    keeper.settle();
    assert_eq!(mode(&scratch.path("b.txt")), 0o600);
    assert_eq!(keeper.status().restored, 0);
}

#[test]
fn a_write_after_mutagens_cancels_the_restore() {
    use std::io::Write;
    let scratch = Scratch::new("write");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    let staged = scratch.stage(b"two", 0o600);
    keeper.settle();
    {
        let _held = keeper.hold();
        std::fs::rename(&staged, scratch.path("a.txt")).unwrap();
        // The agent writes the file before the keeper gets to it.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(scratch.path("a.txt"))
            .unwrap();
        file.write_all(b" and more").unwrap();
    }
    keeper.settle();
    scratch.end_cycle();
    keeper.settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    assert_eq!(keeper.recorded("a.txt"), Some(0o600));
    let status = keeper.status();
    assert_eq!((status.restored, status.unpaired_in_sync), (0, 1), "{status:?}");
    assert_eq!(keeper.warnings()[0].paths, vec!["a.txt".to_owned()]);
}

#[test]
fn an_identity_match_never_widens_a_mode_set_since() {
    let scratch = Scratch::new("guard");
    let keeper = scratch.keeper(0o022);
    let first = scratch.stage(b"one", 0o600);
    let second = scratch.stage(b"two", 0o600);
    keeper.settle();
    {
        let _held = keeper.hold();
        std::fs::create_dir(scratch.path("newdir")).unwrap();
        std::fs::rename(&first, scratch.path("newdir/chosen.txt")).unwrap();
        std::fs::rename(&second, scratch.path("newdir/linked.txt")).unwrap();
        // Somebody narrows one to a mode of their own, and links the other elsewhere, before the keeper looks.
        chmod(&scratch.path("newdir/chosen.txt"), 0o640);
        std::fs::hard_link(scratch.path("newdir/linked.txt"), scratch.base.join("outside/link")).unwrap();
    }
    keeper.settle();
    scratch.end_cycle();
    keeper.settle();
    assert_eq!(mode(&scratch.path("newdir/chosen.txt")), 0o640);
    assert_eq!(mode(&scratch.path("newdir/linked.txt")), 0o600);
    assert_eq!(keeper.status().restored, 0);
}

#[test]
fn a_directory_renamed_after_a_chmod_keeps_the_chmod() {
    let scratch = Scratch::new("dirmove");
    scratch.file("a/x.txt", b"x", 0o644);
    let keeper = scratch.keeper(0o022);
    {
        let _held = keeper.hold();
        chmod(&scratch.path("a/x.txt"), 0o600);
        std::fs::rename(scratch.path("a"), scratch.path("b")).unwrap();
    }
    keeper.settle();
    assert_eq!(keeper.recorded("b/x.txt"), Some(0o600));
    assert_eq!(keeper.recorded("a/x.txt"), None);
    scratch.mutagen_write(&keeper, "b/x.txt", b"y", 0o600);
    assert_eq!(mode(&scratch.path("b/x.txt")), 0o600);
    assert_eq!(keeper.status().restored, 0);
}

#[test]
fn a_sibling_roots_staging_is_not_this_roots() {
    let scratch = Scratch::new("sibling");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    // Another root beside this one syncs: its staging directory sits in the same parent.
    let sibling = scratch.base.join(".mutagen-temporary-staging-sync_other-beta");
    std::fs::create_dir_all(sibling.join("ab")).unwrap();
    std::fs::write(sibling.join("ab/f"), b"other").unwrap();
    chmod(&sibling.join("ab/f"), 0o600);
    keeper.settle();
    // Its staged file leaves for its own root, and a rename lands here meanwhile: neither is this root's sync.
    std::fs::rename(sibling.join("ab/f"), scratch.base.join("outside/f")).unwrap();
    std::fs::write(sibling.join("ab/g"), b"other").unwrap();
    chmod(&sibling.join("ab/g"), 0o600);
    std::fs::rename(sibling.join("ab/g"), scratch.path("a.txt")).unwrap();
    std::fs::remove_dir_all(&sibling).unwrap();
    keeper.settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    let status = keeper.status();
    assert_eq!(
        (status.unpaired, status.unpaired_in_sync, status.restored),
        (1, 0, 0),
        "{status:?}"
    );
    assert!(keeper.warnings().is_empty(), "{:?}", keeper.warnings());
}

#[test]
fn a_staging_directory_made_while_the_keeper_was_busy_is_scanned_on_add() {
    let scratch = Scratch::new("scan");
    scratch.file("a.txt", b"one", 0o644);
    let keeper = scratch.keeper(0o022);
    let staged = {
        let _held = keeper.hold();
        // The staging directory and its subdirectory both exist before the keeper looks at the first event.
        scratch.stage(b"two", 0o600)
    };
    keeper.settle();
    std::fs::rename(staged, scratch.path("a.txt")).unwrap();
    scratch.end_cycle();
    keeper.settle();
    assert_eq!(mode(&scratch.path("a.txt")), 0o644);
}

#[test]
fn a_staged_file_landing_in_a_new_directory_is_restored() {
    let scratch = Scratch::new("newdir");
    let keeper = scratch.keeper(0o022);
    let staged = scratch.stage(b"inner", 0o600);
    keeper.settle();
    {
        // Mutagen makes the directory and renames into it before the keeper can watch it.
        let _held = keeper.hold();
        std::fs::create_dir(scratch.path("newdir")).unwrap();
        std::fs::rename(&staged, scratch.path("newdir/inner.txt")).unwrap();
        // Files somebody else makes there meanwhile are theirs. One with the mode Mutagen writes may also have been
        // a Mutagen write that outran every watch: it is left alone too, and counted.
        scratch.file("newdir/own.txt", b"own", 0o600);
        scratch.file("newdir/shared.txt", b"shared", 0o644);
    }
    keeper.settle();
    scratch.end_cycle();
    keeper.settle();
    assert_eq!(mode(&scratch.path("newdir/inner.txt")), 0o644);
    assert_eq!(mode(&scratch.path("newdir/own.txt")), 0o600);
    assert_eq!(mode(&scratch.path("newdir/shared.txt")), 0o644);
    assert_eq!(keeper.recorded("newdir/own.txt"), Some(0o600));
    let status = keeper.status();
    assert_eq!(
        (status.unpaired, status.unpaired_in_sync, status.restored),
        (1, 1, 1),
        "{status:?}"
    );
}

#[test]
fn ignored_paths_are_not_walked() {
    let scratch = Scratch::new("ignored");
    scratch.file("secret.txt", b"s", 0o644);
    scratch.file("build/out.o", b"o", 0o644);
    scratch.file("src/main.rs", b"m", 0o644);
    scratch.file(".git/config", b"c", 0o644);
    let ignored: modes::Ignore = Box::new(|rel: &str, _| rel == "secret.txt" || rel == "build");
    let keeper = scratch.keeper_with(0o022, ignored, None);
    let records = scratch.modes_file();
    assert_eq!(records.keys().collect::<Vec<_>>(), vec!["src/main.rs"]);
    // Nothing is watched or restored there either.
    scratch.mutagen_write(&keeper, "secret.txt", b"t", 0o600);
    assert_eq!(mode(&scratch.path("secret.txt")), 0o600);
    scratch.file("build/more.o", b"o", 0o600);
    keeper.settle();
    assert_eq!(keeper.recorded("secret.txt"), None);
    assert_eq!(keeper.recorded("build/more.o"), None);
    assert_eq!(keeper.status().restored, 0);
}

#[test]
fn watch_limit_raises_a_warning() {
    let scratch = Scratch::new("limit");
    scratch.file("a/one.txt", b"1", 0o644);
    scratch.file("b/two.txt", b"2", 0o644);
    let keeper = scratch.keeper_with(0o022, Box::new(|_: &str, _: bool| false), Some(2));
    let warnings = keeper.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].text.starts_with(&format!(
            "file modes on {HOST} are kept only for part of {}: the keepers' watch budget (2 directories, half of \
             fs.inotify.max_user_watches) is reached",
            scratch.root()
        )),
        "{warnings:?}"
    );
    assert!(keeper.running());
}

#[test]
fn a_target_pattern_keeps_target_unwatched() {
    // Through the registration, so the keeper uses whatever matcher `ignore_for` gives.
    let scratch = Scratch::new("target");
    scratch.file("target/debug/out", b"o", 0o644);
    scratch.file("src/lib.rs", b"l", 0o644);
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    keepers
        .set(&scratch.root(), vec!["/target/".to_owned()], STAGING, true)
        .unwrap();
    let keeper_settle = || keepers.with_keeper(&scratch.root(), Keeper::settle).unwrap();
    keeper_settle();
    scratch.file("target/debug/more", b"m", 0o600);
    scratch.file("src/new.rs", b"n", 0o600);
    keeper_settle();
    let recorded = |rel: &str| keepers.with_keeper(&scratch.root(), |k| k.recorded(rel)).unwrap();
    assert_eq!(recorded("src/lib.rs"), Some(0o644));
    assert_eq!(recorded("src/new.rs"), Some(0o600));
    assert_eq!(recorded("target/debug/out"), None);
    assert_eq!(recorded("target/debug/more"), None);
}

#[test]
fn an_oversized_registration_is_refused() {
    let scratch = Scratch::new("big");
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    let patterns: Vec<String> = (0..3000).map(|n| format!("{n:04}{}", "x".repeat(4000))).collect();
    let refusal = keepers.set(&scratch.root(), patterns, STAGING, true).unwrap_err();
    assert!(refusal.contains("exceeds"), "{refusal}");
    assert!(!modes::sync_dir(&scratch.state(), &scratch.root())
        .join("watch")
        .exists());
    assert!(keepers
        .set(&scratch.root(), Vec::new(), "not-a-staging-name", true)
        .is_err());
}

#[test]
fn no_paired_cookie_means_no_keeper_and_a_warning() {
    let scratch = Scratch::new("parent");
    scratch.file("a.txt", b"one", 0o644);
    let keepers = Keepers::new(&scratch.state(), options(0o022, None), "run-a");
    // Without a writable parent Mutagen cannot stage beside the root, so its writes cannot be paired.
    chmod(&scratch.base, 0o500);
    let refusal = keepers.set(&scratch.root(), Vec::new(), STAGING, true).unwrap_err();
    chmod(&scratch.base, 0o700);
    let expected = format!("file modes on {HOST} are not kept for {}: ", scratch.root());
    assert!(refusal.starts_with(&expected), "{refusal}");
    assert!(refusal.contains("is not writable"), "{refusal}");
    let warnings = keepers.hello_warnings();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        (
            warnings[0].host.as_str(),
            warnings[0].root.clone(),
            warnings[0].text.clone()
        ),
        (HOST, scratch.root(), refusal)
    );
    assert!(!keepers.holds());
    assert!(!keepers.keeps(&scratch.root()));
}

#[test]
fn two_daemons_keep_one_root_once() {
    let scratch = Scratch::new("two");
    scratch.file("a.txt", b"one", 0o644);
    let root = scratch.root();
    running_daemon(&scratch.state(), "run-old");
    running_daemon(&scratch.state(), "run-new");
    let old = Keepers::new(&scratch.state(), options(0o022, None), "run-old");
    let new = Keepers::new(&scratch.state(), options(0o022, None), "run-new");
    old.set(&root, Vec::new(), STAGING, true).unwrap();
    assert!(old.keeps(&root) && old.holds());
    // A newer installation's daemon registers the root: it waits for the lock, and the old one lets go at its tick.
    new.set(&root, Vec::new(), STAGING, true).unwrap();
    assert!(old.keeps(&root) && !new.keeps(&root));
    old.resume();
    assert!(!old.keeps(&root) && !old.holds());
    new.resume();
    assert!(new.keeps(&root) && !old.keeps(&root));
    old.resume();
    assert!(!old.keeps(&root));
    let settle = |keepers: &Keepers| keepers.with_keeper(&root, Keeper::settle).unwrap();
    settle(&new);
    scratch.mutagen_write_via(|| settle(&new), "a.txt", b"two", 0o600);
    assert_eq!(mode(&scratch.path("a.txt")), 0o644);
    assert_eq!(new.with_keeper(&root, |k| k.status().restored), Some(1));
    // The daemon that does not keep the root passes the keeper's lines on, the counted ones once.
    let outside = scratch.base.join("outside/x");
    // A cycle: Mutagen stages in the staging directory the keeper keeps ready.
    let _cycle = scratch.stage(b"cycle", 0o600);
    settle(&new);
    std::fs::write(&outside, b"x").unwrap();
    chmod(&outside, 0o600);
    std::fs::rename(&outside, scratch.path("b.txt")).unwrap();
    scratch.end_cycle();
    settle(&new);
    let relayed = old.hello_warnings();
    assert_eq!(relayed.len(), 1, "{relayed:?}");
    assert_eq!(relayed[0].paths, vec!["b.txt".to_owned()]);
    assert_eq!(relayed, new.hello_warnings());
    assert_eq!(old.hello_warnings(), relayed);
    // A registration that names no daemon goes to whoever takes the lock, and on to the other when it goes.
    let first = Keepers::new(&scratch.state(), options(0o022, None), "");
    let second = Keepers::new(&scratch.state(), options(0o022, None), "");
    new.stop_all();
    first.set(&root, Vec::new(), STAGING, true).unwrap();
    second.resume();
    assert!(first.keeps(&root) && !second.keeps(&root));
    first.stop_all();
    second.resume();
    assert!(second.keeps(&root));
    settle(&second);
    // Turned off by either, the registration is gone for both, and the stopped keeper writes no record again.
    first.set(&root, Vec::new(), "", false).unwrap();
    second.resume();
    assert!(!second.keeps(&root));
    let dir = modes::sync_dir(&scratch.state(), &root);
    assert!(!dir.join("watch").exists());
    assert!(!dir.join("modes").exists());
}

#[test]
fn queue_overflow_rewalks_and_restores_nothing_meanwhile() {
    let max: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let scratch = Scratch::new("overflow");
    scratch.file("a.txt", b"one", 0o644);
    scratch.file("b.txt", b"one", 0o644);
    scratch.file("flood/0", b"", 0o644);
    scratch.file("flood/1", b"", 0o644);
    let keeper = scratch.keeper(0o022);
    let staged = scratch.stage(b"two", 0o600);
    keeper.settle();
    {
        let _held = keeper.hold();
        std::fs::rename(&staged, scratch.path("a.txt")).unwrap();
        // Two names in turn, so the kernel cannot merge the events.
        for index in 0..max + 64 {
            chmod(
                &scratch.path(&format!("flood/{}", index % 2)),
                if index % 4 < 2 { 0o640 } else { 0o644 },
            );
        }
    }
    keeper.settle();
    let status = keeper.status();
    assert!(status.rewalks >= 1, "{status:?}");
    assert_eq!(status.restored, 0);
    // Whatever happened before the rewalk ended is taken as it is.
    assert_eq!(mode(&scratch.path("a.txt")), 0o600);
    assert_eq!(keeper.recorded("a.txt"), Some(0o600));
    // Afterwards the keeper restores again.
    scratch.end_cycle();
    keeper.settle();
    scratch.mutagen_write(&keeper, "b.txt", b"three", 0o600);
    assert_eq!(mode(&scratch.path("b.txt")), 0o644);
}

// ---- The keeper inside a daemon ----

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A daemon of the test binary, with homes in the scratch directory; killed if the test ends before it exits.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn paths(scratch: &Scratch) -> run_dir::Paths {
    run_dir::Paths::new(
        VERSION,
        &scratch.base.join("d"),
        &scratch.base.join("s"),
        &std::env::temp_dir(),
    )
    .unwrap()
}

fn start_daemon(scratch: &Scratch, idle: u64) -> Daemon {
    let mut command = Command::new(BINARY);
    command
        .args(["daemon", "--idle-seconds", &idle.to_string()])
        .env("HOME", &scratch.base)
        .env("XDG_DATA_HOME", scratch.base.join("d"))
        .env("XDG_STATE_HOME", scratch.base.join("s"))
        .env("XDG_RUNTIME_DIR", scratch.base.join("outside"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("DBUS_SYSTEM_BUS_ADDRESS");
    let mut daemon = Daemon {
        child: command.spawn().unwrap(),
    };
    wait_for(|| {
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited before readiness"
        );
        runtime().block_on(hello(scratch)).is_ok()
    });
    daemon
}

/// Waits for a condition; the bound is a liveness limit, not a speed claim.
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "condition not reached");
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn client(scratch: &Scratch) -> std::io::Result<helper_client::HelperClient<tonic::transport::Channel>> {
    let socket = paths(scratch).socket()?;
    let channel = tonic::transport::Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .map_err(std::io::Error::other)?;
    Ok(helper_client::HelperClient::new(channel))
}

async fn hello(scratch: &Scratch) -> std::io::Result<HelloResponse> {
    let request = async {
        client(scratch)
            .await?
            .hello(HelloRequest {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                client_version: VERSION.to_owned(),
            })
            .await
            .map(tonic::Response::into_inner)
            .map_err(std::io::Error::other)
    };
    tokio::time::timeout(Duration::from_millis(500), request)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "Hello timed out"))?
}

fn watch_modes(scratch: &Scratch, on: bool) -> WatchModesResponse {
    runtime().block_on(async {
        client(scratch)
            .await
            .unwrap()
            .watch_modes(WatchModesRequest {
                root: scratch.root(),
                patterns: Vec::new(),
                on,
                staging_name: STAGING.to_owned(),
            })
            .await
            .unwrap()
            .into_inner()
    })
}

#[test]
fn a_registered_keeper_holds_the_daemon_past_idle() {
    let scratch = Scratch::new("idle");
    scratch.file("a.txt", b"one", 0o644);
    let mut daemon = start_daemon(&scratch, 2);
    let hello = runtime().block_on(hello(&scratch)).unwrap();
    assert!(hello.capabilities.iter().any(|cap| cap == CAP_SYNC_MODES));
    let registered = watch_modes(&scratch, true);
    assert_eq!((registered.refusal.as_str(), registered.host.is_empty()), ("", false));
    // The wait spans the injected idle period; it is a liveness observation, not a speed bound.
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        daemon.child.try_wait().unwrap().is_none(),
        "a keeper must hold the daemon"
    );
    assert_eq!(watch_modes(&scratch, false).refusal, "");
    wait_for(|| daemon.child.try_wait().unwrap().is_some());
    assert!(daemon.child.wait().unwrap().success());
}

#[test]
fn keeper_restarts_from_its_watch_file_after_a_daemon_restart() {
    let scratch = Scratch::new("restart");
    scratch.file("a.txt", b"one", 0o644);
    let state = scratch.base.join("s").join("eitri-server");
    let dir = modes::sync_dir(&state, &scratch.root());
    let mut daemon = start_daemon(&scratch, 60);
    assert_eq!(watch_modes(&scratch, true).refusal, "");
    assert!(dir.join("watch").exists());
    // SAFETY: the pid is this test's own child, not yet reaped.
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGTERM) };
    wait_for(|| daemon.child.try_wait().unwrap().is_some());
    assert!(dir.join("watch").exists());
    let _ = std::fs::remove_file(dir.join("modes"));
    // Nobody asks the new daemon: it takes the root up from the registration alone.
    let _daemon = start_daemon(&scratch, 60);
    wait_for(|| dir.join("modes").exists());
    assert_eq!(
        modes::read_modes_file(&dir.join("modes")).unwrap().get("a.txt"),
        Some(&0o644)
    );
    // No settle reaches a keeper inside a daemon. Each try stages a file, leaves it staged a while (as Mutagen does
    // while a batch arrives), applies it and polls for the restore; a try the keeper missed is left as it is (and
    // counted), and the next try waits longer.
    let restored = (1..=8u64).any(|attempt| {
        chmod(&scratch.path("a.txt"), 0o644);
        let staged = scratch.stage(b"two", 0o600);
        std::thread::sleep(Duration::from_millis(100 * attempt));
        std::fs::rename(&staged, scratch.path("a.txt")).unwrap();
        scratch.end_cycle();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if mode(&scratch.path("a.txt")) == 0o644 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    });
    assert!(restored, "no try was restored");
    // The only line allowed is the count of tries the keeper missed, naming a.txt.
    let hello = runtime().block_on(hello(&scratch)).unwrap();
    assert!(
        hello
            .mode_warnings
            .iter()
            .all(|line| line.paths.iter().all(|path| path == "a.txt")
                && line.text.contains("were left as Mutagen wrote them")),
        "{:?}",
        hello.mode_warnings
    );
    // Mode lines go to the sync actor per root, never into the warnings every agent tab shows.
    assert!(
        hello.warnings.iter().all(|line| !line.contains("file modes")),
        "{:?}",
        hello.warnings
    );
}

// ---- With a real Mutagen ----

/// Mutagen's daemon for this test's private data directory, stopped when the test ends.
struct Mutagen {
    bin: PathBuf,
    data: PathBuf,
}

impl Mutagen {
    fn run(&self, args: &[&str]) -> String {
        let output = Command::new(&self.bin)
            .args(args)
            .env("MUTAGEN_DATA_DIRECTORY", &self.data)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mutagen {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn cycles(&self, name: &str) -> u64 {
        let listed: serde_json::Value =
            serde_json::from_str(&self.run(&["sync", "list", "--template", "{{json .}}", name])).unwrap();
        listed[0]["successfulCycles"].as_u64().unwrap()
    }

    /// The cycle count once it has not moved for two seconds (Mutagen runs a cycle or two after its own writes).
    fn settled_cycles(&self, name: &str) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = self.cycles(name);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let now = self.cycles(name);
            if now == last {
                return now;
            }
            assert!(Instant::now() < deadline, "Mutagen keeps cycling");
            last = now;
        }
    }
}

impl Drop for Mutagen {
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .args(["daemon", "stop"])
            .env("MUTAGEN_DATA_DIRECTORY", &self.data)
            .output();
    }
}

#[test]
#[ignore]
fn real_laptop_save_keeps_server_0644_and_0600() {
    let bin = PathBuf::from(std::env::var_os("EITRI_TEST_MUTAGEN").expect("EITRI_TEST_MUTAGEN names mutagen"));
    assert!(bin.is_absolute());
    let scratch = Scratch::new("real");
    let laptop = scratch.base.join("outside");
    scratch.file("a.txt", b"one\n", 0o644);
    scratch.file("b.txt", b"two\n", 0o600);
    scratch.file("run.sh", b"#!/bin/sh\n", 0o755);
    let mutagen = Mutagen {
        bin,
        data: scratch.base.join("mutagen"),
    };
    let name = "eitri-modes-test";
    mutagen.run(&[
        "sync",
        "create",
        "--name",
        name,
        "--no-global-configuration",
        "--stage-mode-beta=neighboring",
        laptop.to_str().unwrap(),
        &scratch.root(),
    ]);
    mutagen.run(&["sync", "flush", name]);
    assert_eq!(std::fs::read(laptop.join("a.txt")).unwrap(), b"one\n");
    // The keeper knows the session's staging directory by the session's identifier.
    let listed: serde_json::Value =
        serde_json::from_str(&mutagen.run(&["sync", "list", "--template", "{{json .}}", name])).unwrap();
    let staging = format!(
        "{}{}-beta",
        modes::STAGING_PREFIX,
        listed[0]["identifier"].as_str().unwrap()
    );
    let keeper = scratch.keeper_named(Some(&staging), 0o022, Box::new(|_: &str, _: bool| false), None);
    // A save on the laptop of each file, all in one batch.
    mutagen.run(&["sync", "pause", name]);
    std::fs::write(laptop.join("a.txt"), b"one, saved\n").unwrap();
    std::fs::write(laptop.join("b.txt"), b"two, saved\n").unwrap();
    std::fs::write(laptop.join("run.sh"), b"#!/bin/sh\necho saved\n").unwrap();
    std::fs::write(laptop.join("new.txt"), b"new\n").unwrap();
    std::fs::create_dir(laptop.join("newdir")).unwrap();
    std::fs::write(laptop.join("newdir/inner.txt"), b"inner\n").unwrap();
    std::fs::write(laptop.join("zz-large.bin"), vec![0u8; 96 << 20]).unwrap();
    mutagen.run(&["sync", "resume", name]);
    mutagen.run(&["sync", "flush", name]);
    keeper.settle();
    assert_eq!(std::fs::read(scratch.path("a.txt")).unwrap(), b"one, saved\n");
    assert_eq!(std::fs::read(scratch.path("b.txt")).unwrap(), b"two, saved\n");
    let status = keeper.status();
    eprintln!(
        "modes: a {:o} b {:o} run.sh {:o} new {:o} newdir/inner {:o} large {:o}; {status:?}",
        mode(&scratch.path("a.txt")),
        mode(&scratch.path("b.txt")),
        mode(&scratch.path("run.sh")),
        mode(&scratch.path("new.txt")),
        mode(&scratch.path("newdir/inner.txt")),
        mode(&scratch.path("zz-large.bin")),
    );
    assert_eq!(mode(&scratch.path("b.txt")), 0o600);
    // Mutagen places each file the moment it is staged; the staging directory kept ready pairs every rename into a
    // watched directory.
    for (rel, restored) in [
        ("a.txt", 0o644),
        ("run.sh", 0o755),
        ("new.txt", 0o644),
        ("zz-large.bin", 0o644),
    ] {
        assert_eq!(mode(&scratch.path(rel)), restored, "{rel}: {status:?}");
    }
    // A file Mutagen renames into a directory it made in the same cycle lands before that directory can be watched:
    // it is restored when the keeper caught its identity in staging, else left as Mutagen wrote it and counted.
    let inner = mode(&scratch.path("newdir/inner.txt"));
    assert!(inner == 0o644 || inner == 0o600, "newdir/inner.txt: {inner:o}");
    assert_eq!(status.restored + u64::from(inner == 0o600), 5, "{status:?}");
    assert_eq!(status.unpaired_in_sync == 0, inner == 0o644, "{status:?}");
    // The keeper's chmods change no executability, so they start no cycle of their own: the count settles and
    // stays, and the laptop's files keep the modes Mutagen gave them.
    let laptop_modes = (mode(&laptop.join("a.txt")), mode(&laptop.join("run.sh")));
    let server_modes = (mode(&scratch.path("a.txt")), mode(&scratch.path("run.sh")));
    let cycles = mutagen.settled_cycles(name);
    std::thread::sleep(Duration::from_secs(3));
    keeper.settle();
    assert_eq!(mutagen.cycles(name), cycles);
    assert_eq!(
        (mode(&scratch.path("a.txt")), mode(&scratch.path("run.sh"))),
        server_modes
    );
    assert_eq!(
        (mode(&laptop.join("a.txt")), mode(&laptop.join("run.sh"))),
        laptop_modes
    );
    assert_eq!(std::fs::read(laptop.join("a.txt")).unwrap(), b"one, saved\n");
    mutagen.run(&["sync", "terminate", name]);
}
