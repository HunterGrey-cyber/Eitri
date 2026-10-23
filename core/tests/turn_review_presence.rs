//! Presence locks against real files, and a real second process for the death of a window.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use eitri_core::turn_review::{
    busy_elsewhere, Blocked, Busy, Partner, Presence, PresenceGuard, PresenceHolder, Shadow,
};

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_presence")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    /// A review directory shaped like the real one, `<state>/eitri/review/<key>`.
    fn review_dir(&self) -> PathBuf {
        let dir = self.0.join("state/eitri/review/0123456789abcdef");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn another_windows_file_is_busy() {
    let scratch = Scratch::new("other-window");
    let dir = scratch.review_dir();
    let _other = Presence::open(&dir, 4242).unwrap();
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 })
    );
    assert!(Busy::OtherWindow { pid: 4242 }
        .reason()
        .contains("another Eitri window"));
}

#[test]
fn a_running_turn_is_busy_and_wins() {
    let scratch = Scratch::new("running");
    let dir = scratch.review_dir();
    let mut other = Presence::open(&dir, 4242).unwrap();
    other.turn_started(3).unwrap();
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::TurnRunning { pid: 4242 })
    );
    other.turn_ended(3);
    assert!(!dir.join("running/4242.3").exists(), "the ended turn's file is removed");
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 })
    );
}

#[test]
fn own_files_are_skipped_by_name() {
    let scratch = Scratch::new("own");
    let dir = scratch.review_dir();
    let mut me = Presence::open(&dir, 4242).unwrap();
    me.turn_started(1).unwrap();
    assert_eq!(busy_elsewhere(&dir, 4242).unwrap(), None);
    assert!(
        dir.join("windows/4242").exists(),
        "probing one's own files removes nothing"
    );
    assert!(dir.join("running/4242.1").exists());
}

#[test]
fn a_directory_nobody_made_is_nobody() {
    let scratch = Scratch::new("none");
    let dir = scratch.review_dir();
    assert_eq!(busy_elsewhere(&dir, 100).unwrap(), None);
}

#[test]
fn files_are_private_and_a_leftover_is_swept_not_reported() {
    let scratch = Scratch::new("modes");
    let dir = scratch.review_dir();
    let me = Presence::open(&dir, 4242).unwrap();
    for sub in ["running", "windows"] {
        let mode = std::fs::metadata(dir.join(sub)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{sub}");
    }
    let mode = std::fs::metadata(dir.join("windows/4242"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    drop(me);
    assert!(!dir.join("windows/4242").exists(), "a clean exit removes its own file");

    // A file nobody holds, with a name that is no pid, and a symlink, are not windows.
    std::fs::write(dir.join("windows/7"), b"").unwrap();
    std::fs::write(dir.join("windows/notes.txt"), b"").unwrap();
    let foreign = scratch.0.join("foreign");
    std::fs::write(&foreign, b"keep").unwrap();
    std::os::unix::fs::symlink(&foreign, dir.join("windows/8")).unwrap();
    assert_eq!(busy_elsewhere(&dir, 100).unwrap(), None);
    assert!(!dir.join("windows/7").exists(), "a leftover is removed");
    assert!(
        dir.join("windows/notes.txt").exists(),
        "a name that is no pid is left alone"
    );
    assert!(
        std::fs::symlink_metadata(dir.join("windows/8"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "a symlink is skipped, never followed or removed"
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), b"keep");
}

/// Puts a socket node at `path`, whatever its length. Linux: `mknod` makes it without binding. Elsewhere
/// (macOS) `mknod` of a socket needs root, so one is bound under a short path in `/tmp` and renamed into
/// place: a rename keeps the node, and the socket path cap applies only to the bind.
fn socket_node_at(path: &std::path::Path) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let socket = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `socket` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mknod(socket.as_ptr(), libc::S_IFSOCK | 0o600, 0) }, 0);
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let short = std::path::PathBuf::from(format!("/tmp/eitri-sock-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&short).unwrap();
        let bound = short.join("s");
        let listener = std::os::unix::net::UnixListener::bind(&bound).unwrap();
        std::fs::rename(&bound, path).unwrap();
        drop(listener);
        std::fs::remove_dir(&short).unwrap();
    }
}

/// A directory or a socket at a presence path is no lock file and is skipped like a symlink: the
/// probe opens for writing, which a directory refuses with `EISDIR` and a socket with each OS's own
/// errno (`ENXIO` on Linux, `EOPNOTSUPP` on macOS), and neither may block every revert.
#[test]
fn a_directory_or_a_socket_at_a_presence_path_is_skipped() {
    let scratch = Scratch::new("not-a-file");
    let dir = scratch.review_dir();
    drop(Presence::open(&dir, 4242).unwrap());
    std::fs::create_dir(dir.join("windows/9")).unwrap();
    std::fs::create_dir(dir.join("running/9.1")).unwrap();
    socket_node_at(&dir.join("windows/10"));
    assert_eq!(busy_elsewhere(&dir, 100).unwrap(), None);
    assert!(dir.join("windows/9").is_dir(), "a directory is left alone");
    assert!(dir.join("running/9.1").is_dir());
    assert!(dir.join("windows/10").exists(), "a socket is left alone");
}

/// A window started under a umask that takes the owner's write bit away is still seen by another
/// window, and its files are still this user's 0600 ones. The probe opens another window's file for
/// writing (an exclusive lock on NFS needs that), so a file created 0400 would refuse every revert.
/// The umask is process-wide, so it is set in a child of its own rather than in this test runner.
#[test]
fn a_window_under_a_restrictive_umask_is_still_seen() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("umask");
    let dir = scratch.review_dir();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "presence_child", "--ignored", "--test-threads=1"])
        .env("EITRI_PRESENCE_CHILD_DIR", &dir)
        .env("EITRI_PRESENCE_CHILD_UMASK", "277")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    let seen = loop {
        match busy_elsewhere(&dir, 100) {
            Ok(Some(Busy::TurnRunning { pid: held })) if held == pid => break Ok(()),
            Ok(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(other) => break Err(format!("the child's turn was never seen: {other:?}")),
            Err(e) => break Err(format!("the other windows could not be listed: {e}")),
        }
    };
    let modes = ["windows", "running"].map(|sub| {
        let file = if sub == "windows" {
            dir.join(format!("windows/{pid}"))
        } else {
            dir.join(format!("running/{pid}.5"))
        };
        std::fs::metadata(&file).map(|m| m.permissions().mode() & 0o777)
    });
    // Killed by the pid captured at the spawn, before anything can fail.
    child.kill().unwrap();
    child.wait().unwrap();
    seen.unwrap();
    for mode in modes {
        assert_eq!(mode.unwrap(), 0o600);
    }
}

/// What the child half of `a_killed_window_releases_and_is_swept` and
/// `a_window_under_a_restrictive_umask_is_still_seen` runs: it holds a window and a running turn
/// until killed, under the umask `EITRI_PRESENCE_CHILD_UMASK` names (octal) when it names one. A
/// bare run with nothing to hold does nothing.
#[test]
#[ignore = "run as a child process by a_killed_window_releases_and_is_swept and a_window_under_a_restrictive_umask_is_still_seen"]
fn presence_child() {
    let Some(dir) = std::env::var_os("EITRI_PRESENCE_CHILD_DIR") else {
        return;
    };
    if let Some(mask) = std::env::var_os("EITRI_PRESENCE_CHILD_UMASK") {
        let mask = libc::mode_t::from_str_radix(mask.to_str().unwrap(), 8).unwrap();
        // SAFETY: `umask` has no preconditions; this process is a child of its own, so no other
        // test's files are created under it.
        unsafe { libc::umask(mask) };
    }
    let mut presence = Presence::open(Path::new(&dir), std::process::id()).unwrap();
    presence.turn_started(5).unwrap();
    // Gives up on its own if its parent never kills it.
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn a_killed_window_releases_and_is_swept() {
    let scratch = Scratch::new("killed");
    let dir = scratch.review_dir();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "presence_child", "--ignored", "--test-threads=1"])
        .env("EITRI_PRESENCE_CHILD_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let own = 100;
    until("the child to hold its turn", || {
        busy_elsewhere(&dir, own).unwrap() == Some(Busy::TurnRunning { pid })
    });
    // Killed by the pid captured at the spawn.
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(busy_elsewhere(&dir, own).unwrap(), None, "a dead window blocks nobody");
    assert!(!dir.join(format!("windows/{pid}")).exists(), "its window file is swept");
    assert!(!dir.join(format!("running/{pid}.5")).exists(), "and its turn file");
}

#[test]
fn a_holder_whose_file_was_unlinked_recreates_it() {
    let scratch = Scratch::new("unlinked");
    let dir = scratch.review_dir();
    let mut me = Presence::open(&dir, 4242).unwrap();
    me.turn_started(2).unwrap();
    std::fs::remove_file(dir.join("windows/4242")).unwrap();
    std::fs::remove_file(dir.join("running/4242.2")).unwrap();
    assert_eq!(busy_elsewhere(&dir, 100).unwrap(), None, "unlinked, so nobody sees it");
    me.refresh().unwrap();
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::TurnRunning { pid: 4242 }),
        "the turn's file is back"
    );
    me.turn_ended(2);
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 }),
        "and the window's"
    );
}

/// `windows` made a symlink to a directory of the user's own elsewhere.
fn symlinked_windows(scratch: &Scratch, dir: &Path) -> PathBuf {
    let target = scratch.0.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, dir.join("windows")).unwrap();
    target
}

#[test]
fn presence_dirs_are_private_and_never_follow_a_symlink() {
    let scratch = Scratch::new("symlink");
    let dir = scratch.review_dir();
    let target = symlinked_windows(&scratch, &dir);
    assert!(Presence::open(&dir, 4242).is_err());
    assert_eq!(
        std::fs::read_dir(&target).unwrap().count(),
        0,
        "nothing was created in the target"
    );
    // Probing does not read through it either.
    assert!(busy_elsewhere(&dir, 100).is_err());
}

#[test]
fn a_holder_that_could_not_open_refuses_every_check() {
    let scratch = Scratch::new("refuses");
    let dir = scratch.review_dir();
    symlinked_windows(&scratch, &dir);
    let holder = PresenceHolder::start(dir, 4242);
    let guard = holder.guard();
    until("the open to fail", || match guard.check() {
        Err(Blocked::NotHeld(why)) => !why.contains("not been taken yet"),
        other => panic!("unexpected {other:?}"),
    });
    assert!(guard
        .check()
        .unwrap_err()
        .to_string()
        .contains("presence lock is not held"));
}

#[test]
fn a_holder_that_holds_lets_a_lone_window_through_and_blocks_a_second() {
    let scratch = Scratch::new("holder");
    let dir = scratch.review_dir();
    let holder = PresenceHolder::start(dir.clone(), 4242);
    let guard = holder.guard();
    until("the window file", || guard.check().is_ok());
    let _other = Presence::open(&dir, 5000).unwrap();
    assert_eq!(guard.check(), Err(Blocked::Busy(Busy::OtherWindow { pid: 5000 })));
    // Another window sees this one.
    assert_eq!(
        busy_elsewhere(&dir, 5000).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 })
    );
}

#[test]
fn trim_leaves_presence_and_journal_alone() {
    let scratch = Scratch::new("trim");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("a.txt"), "a\n").unwrap();
    let dir = scratch.review_dir();
    let shadow = Shadow::open_with_excludes(&dir, &project, None).unwrap();
    let mut me = Presence::open(&dir, 4242).unwrap();
    me.turn_started(1).unwrap();
    std::fs::create_dir_all(dir.join("journal")).unwrap();
    std::fs::write(dir.join("journal/entry"), b"pending").unwrap();
    // An index lock a killed run left is swept by the same pass; none of the presence files is a
    // `.lock`, and none of the directories is entered.
    std::fs::write(dir.join("index-killed.lock"), b"").unwrap();

    shadow.trim(0, Duration::ZERO, u64::MAX / 2, "now").unwrap();

    assert!(!dir.join("index-killed.lock").exists(), "the pass ran");
    assert!(dir.join("windows/4242").exists());
    assert!(dir.join("running/4242.1").exists());
    assert_eq!(std::fs::read(dir.join("journal/entry")).unwrap(), b"pending");
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::TurnRunning { pid: 4242 })
    );
}

// ---- a linked pair -------------------------------------------------------------------------------

/// A real process to stand for the linked partner, so its pid has a start time to match; killed by
/// its own handle when dropped.
struct PartnerProcess(std::process::Child);

impl PartnerProcess {
    fn start() -> PartnerProcess {
        PartnerProcess(
            Command::new("sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn partner(&self) -> (Partner, Arc<AtomicBool>) {
        let counts_as_other = Arc::new(AtomicBool::new(false));
        let start = eitri_core::split::proc_start_time(self.pid()).expect("the partner has a start time");
        (
            Partner {
                pid: self.pid(),
                start,
                counts_as_other: Arc::clone(&counts_as_other),
            },
            counts_as_other,
        )
    }
}

impl Drop for PartnerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// This window's own pid in the pair tests: any number the other files do not use, since a window
/// skips its own files by name.
const ME: u32 = 4100;

#[test]
fn the_partners_windows_file_is_skipped_only_while_linked_and_counted_again_after() {
    let scratch = Scratch::new("partner-skip");
    let dir = scratch.review_dir();
    let partner = PartnerProcess::start();
    let _mine = Presence::open(&dir, ME).unwrap();
    let _theirs = Presence::open(&dir, partner.pid()).unwrap();
    let guard = PresenceGuard::for_tests(&dir, ME, 0);
    let unlinked = Err(Blocked::Busy(Busy::OtherWindow { pid: partner.pid() }));

    assert_eq!(
        guard.check(),
        unlinked,
        "before the link the other window is another window"
    );
    let (linked, _) = partner.partner();
    guard.set_partner(Some(linked));
    assert_eq!(guard.check(), Ok(()), "a linked partner is not another window");
    let worker_copy = guard.clone();
    guard.set_partner(None);
    assert_eq!(guard.check(), unlinked, "once the link ends it counts again");
    assert_eq!(
        worker_copy.check(),
        unlinked,
        "a guard already handed to a worker sees it too"
    );
    assert!(
        dir.join(format!("windows/{}", partner.pid())).exists(),
        "the partner's held file is never swept"
    );
}

#[test]
fn a_partner_whose_pid_names_another_process_now_is_another_window() {
    let scratch = Scratch::new("partner-reused");
    let dir = scratch.review_dir();
    let partner = PartnerProcess::start();
    let _theirs = Presence::open(&dir, partner.pid()).unwrap();
    let guard = PresenceGuard::for_tests(&dir, ME, 0);
    let (mut linked, _) = partner.partner();
    // The same pid, but a process that started at another time: not the window that linked.
    linked.start += 1;
    guard.set_partner(Some(linked));
    assert_eq!(
        guard.check(),
        Err(Blocked::Busy(Busy::OtherWindow { pid: partner.pid() }))
    );
}

#[test]
fn a_partner_that_reported_held_false_counts_as_another_window_until_held_true() {
    let scratch = Scratch::new("partner-held-false");
    let dir = scratch.review_dir();
    let partner = PartnerProcess::start();
    let _theirs = Presence::open(&dir, partner.pid()).unwrap();
    let guard = PresenceGuard::for_tests(&dir, ME, 0);
    let (linked, counts_as_other) = partner.partner();
    guard.set_partner(Some(linked));
    assert_eq!(guard.check(), Ok(()));

    // What the link's reader does on `presence{held: false}`, before it answers.
    counts_as_other.store(true, Ordering::SeqCst);
    assert_eq!(
        guard.check(),
        Err(Blocked::Busy(Busy::OtherWindow { pid: partner.pid() })),
        "a partner that cannot mark its turns is another window"
    );
    // And on `presence{held: true}`.
    counts_as_other.store(false, Ordering::SeqCst);
    assert_eq!(guard.check(), Ok(()));
}

#[test]
fn a_partner_counted_as_another_window_refuses_even_before_its_own_file_exists() {
    let scratch = Scratch::new("partner-no-file");
    let dir = scratch.review_dir();
    let partner = PartnerProcess::start();
    let _mine = Presence::open(&dir, ME).unwrap();
    let guard = PresenceGuard::for_tests(&dir, ME, 0);
    let (linked, counts_as_other) = partner.partner();
    guard.set_partner(Some(linked));
    assert_eq!(
        guard.check(),
        Ok(()),
        "no file and nothing said: the partner is not another window"
    );
    // A partner whose presence thread never got as far as its own file says it cannot mark its turns.
    counts_as_other.store(true, Ordering::SeqCst);
    assert_eq!(
        guard.check(),
        Err(Blocked::Busy(Busy::OtherWindow { pid: partner.pid() })),
        "it is another window though nothing of it is on disk"
    );
    counts_as_other.store(false, Ordering::SeqCst);
    assert_eq!(guard.check(), Ok(()));
}

#[test]
fn the_running_check_is_unchanged_and_sees_a_partner_turn() {
    let scratch = Scratch::new("partner-turn");
    let dir = scratch.review_dir();
    let partner = PartnerProcess::start();
    let mut theirs = Presence::open(&dir, partner.pid()).unwrap();
    let guard = PresenceGuard::for_tests(&dir, ME, 0);
    let (linked, _) = partner.partner();
    guard.set_partner(Some(linked));
    assert_eq!(guard.check(), Ok(()));

    theirs.turn_started(2).unwrap();
    assert_eq!(
        guard.check(),
        Err(Blocked::Busy(Busy::TurnRunning { pid: partner.pid() })),
        "a turn of the partner blocks a revert here"
    );
    theirs.turn_ended(2);
    assert_eq!(guard.check(), Ok(()));

    // This window's own turn still blocks it, linked or not.
    let busy_here = PresenceGuard::for_tests(&dir, ME, 1);
    let (linked, _) = partner.partner();
    busy_here.set_partner(Some(linked));
    assert_eq!(busy_here.check(), Err(Blocked::TurnRunningHere));
}

#[test]
fn an_unrelated_window_still_blocks_the_pair_and_is_blocked() {
    let scratch = Scratch::new("partner-unrelated");
    let dir = scratch.review_dir();
    let partner = PartnerProcess::start();
    let _mine = Presence::open(&dir, ME).unwrap();
    let _theirs = Presence::open(&dir, partner.pid()).unwrap();
    let guard = PresenceGuard::for_tests(&dir, ME, 0);
    let (linked, _) = partner.partner();
    guard.set_partner(Some(linked));
    assert_eq!(guard.check(), Ok(()));

    let unrelated_pid = 4242;
    let unrelated = Presence::open(&dir, unrelated_pid).unwrap();
    assert_eq!(
        guard.check(),
        Err(Blocked::Busy(Busy::OtherWindow { pid: unrelated_pid })),
        "a window outside the pair blocks it"
    );
    let theirs_check = PresenceGuard::for_tests(&dir, unrelated_pid, 0).check();
    assert!(
        matches!(theirs_check, Err(Blocked::Busy(Busy::OtherWindow { pid })) if pid == ME || pid == partner.pid()),
        "the window outside the pair sees the pair: {theirs_check:?}"
    );
    drop(unrelated);
    assert_eq!(guard.check(), Ok(()));
}

#[test]
fn a_linked_windows_prompt_waits_for_its_presence_file() {
    use eitri_core::turn_review::Held;
    let scratch = Scratch::new("confirmed");
    let dir = scratch.review_dir();
    let holder = PresenceHolder::start(dir.clone(), 4242);
    let answer = holder.sync_confirmed(&std::collections::BTreeSet::from([3]));
    // The answer comes once the file is taken, so by then another window sees the turn.
    assert_eq!(answer.recv_timeout(Duration::from_secs(10)), Ok(Held::Yes));
    assert_eq!(
        busy_elsewhere(&dir, 5000).unwrap(),
        Some(Busy::TurnRunning { pid: 4242 })
    );
    // The same set again is answered again: a second prompt waits for its own confirmation.
    let again = holder.sync_confirmed(&std::collections::BTreeSet::from([3]));
    assert_eq!(again.recv_timeout(Duration::from_secs(10)), Ok(Held::Yes));
    // A set without the turn lets its file go.
    let ended = holder.sync_confirmed(&std::collections::BTreeSet::new());
    assert_eq!(ended.recv_timeout(Duration::from_secs(10)), Ok(Held::Yes));
    assert!(!dir.join("running/4242.3").exists());
}

#[test]
fn a_holder_that_could_not_open_answers_no_with_why() {
    use eitri_core::turn_review::Held;
    let scratch = Scratch::new("confirmed-no");
    let dir = scratch.review_dir();
    symlinked_windows(&scratch, &dir);
    let holder = PresenceHolder::start(dir, 4242);
    let answer = holder.sync_confirmed(&std::collections::BTreeSet::from([3]));
    match answer.recv_timeout(Duration::from_secs(10)) {
        Ok(Held::No(why)) => assert!(!why.is_empty()),
        other => panic!("a window whose presence cannot be held must say no: {other:?}"),
    }
}

#[test]
fn with_no_ack_the_prompt_is_not_sent_and_says_why() {
    use eitri_core::turn_review::{Decision, Held, PresenceFallback, CONFIRM_WAIT};
    let start = Instant::now();
    let mut wait = PresenceFallback::start(start);
    assert_eq!(
        wait.on_confirmed(Held::No("the review directory is read-only".into()), start),
        Decision::SendPresenceFalse
    );
    assert_eq!(
        wait.tick(start + CONFIRM_WAIT - Duration::from_millis(1)),
        Decision::Wait
    );
    assert_eq!(
        wait.tick(start + CONFIRM_WAIT),
        Decision::NotSent(
            "not sent: turn review could not mark the turn as running (the review directory is read-only)".into()
        )
    );
    // Decided once: a late acknowledgement sends nothing.
    assert_eq!(wait.on_ack(start + CONFIRM_WAIT * 2), Decision::Wait);

    // A thread that never answers falls back with a reason of its own.
    let mut silent = PresenceFallback::start(start);
    assert_eq!(silent.tick(start + CONFIRM_WAIT), Decision::SendPresenceFalse);
    match silent.tick(start + CONFIRM_WAIT * 2) {
        Decision::NotSent(note) => assert!(
            note.starts_with("not sent: turn review could not mark the turn as running (") && note.ends_with(')'),
            "{note}"
        ),
        other => panic!("no acknowledgement must leave the prompt unsent: {other:?}"),
    }
}

#[test]
fn a_prompt_waiting_when_its_window_closes_the_link_is_not_sent() {
    use eitri_core::turn_review::{Decision, Held, PresenceFallback, CLOSING_NOTE};
    let start = Instant::now();
    let mut confirming = PresenceFallback::start(start);
    assert_eq!(confirming.on_closing(), Decision::NotSent(CLOSING_NOTE.to_string()));
    assert_eq!(
        confirming.on_confirmed(Held::Yes, start),
        Decision::Wait,
        "decided once"
    );
    let mut awaiting = PresenceFallback::start(start);
    assert_eq!(
        awaiting.on_confirmed(Held::No("full".into()), start),
        Decision::SendPresenceFalse
    );
    assert_eq!(awaiting.on_closing(), Decision::NotSent(CLOSING_NOTE.to_string()));
    let mut confirmed = PresenceFallback::start(start);
    assert_eq!(confirmed.on_confirmed(Held::Yes, start), Decision::SendPrompt);
    assert_eq!(
        confirmed.on_closing(),
        Decision::Wait,
        "a prompt already sent is not taken back"
    );
}

#[test]
fn a_link_end_while_waiting_sends_the_prompt() {
    use eitri_core::turn_review::{Decision, Held, PresenceFallback, CONFIRM_WAIT};
    let start = Instant::now();
    // While the thread has not answered.
    let mut confirming = PresenceFallback::start(start);
    assert_eq!(confirming.on_link_end(), Decision::SendPrompt);
    // While the partner's acknowledgement is awaited.
    let mut awaiting = PresenceFallback::start(start);
    assert_eq!(
        awaiting.on_confirmed(Held::No("full".into()), start),
        Decision::SendPresenceFalse
    );
    assert_eq!(awaiting.on_link_end(), Decision::SendPrompt);
    assert_eq!(awaiting.tick(start + CONFIRM_WAIT * 3), Decision::Wait, "decided once");
    // A confirmed prompt is not sent twice by a later link end.
    let mut confirmed = PresenceFallback::start(start);
    assert_eq!(confirmed.on_confirmed(Held::Yes, start), Decision::SendPrompt);
    assert_eq!(confirmed.on_link_end(), Decision::Wait);
}
