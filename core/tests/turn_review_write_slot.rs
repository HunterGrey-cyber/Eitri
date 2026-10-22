//! The project's write slot across two real processes: this test binary runs again as the second
//! one. The child tests are `#[ignore]`d and do nothing unless the parent set their directory, so
//! `-- --ignored` over this target is harmless.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use eitri_core::turn_review::{Shadow, SlotBusy, SlotHold, WriteSlot};

const RACE_DIR: &str = "EITRI_WRITE_SLOT_RACE_DIR";
const RACE_ROLE: &str = "EITRI_WRITE_SLOT_RACE_ROLE";
const UMASK_DIR: &str = "EITRI_WRITE_SLOT_UMASK_DIR";
/// Rounds in which both sides try at once.
const RACE_ROUNDS: u32 = 200;
/// Rounds in which one side holds the slot first and the other tries against it.
const TURN_ROUNDS: u32 = 100;

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_write_slot")
            .join(format!("{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn review_dir_in(base: &Path) -> PathBuf {
    base.join("state/eitri/review/0123456789abcdef")
}

fn mark(base: &Path, what: &str, role: &str, round: u32) {
    std::fs::write(base.join(format!("{what}-{role}-{round}")), b"").unwrap();
}

fn wait_for(base: &Path, what: &str, role: &str, round: u32) {
    let path = base.join(format!("{what}-{role}-{round}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// `a` always reverts; `b` takes a revert's hold or, when `read`, a send's read of the disk.
fn take(slot: &WriteSlot, role: &str, read: bool) -> Result<SlotHold, SlotBusy> {
    if role == "b" && read {
        slot.try_shared()
    } else {
        slot.try_exclusive()
    }
}

/// Runs this test binary again as the other process, at `test` with `env` set.
fn spawn_child(test: &str, env: &[(&str, &Path)], role: Option<&str>) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--ignored", "--nocapture", "--test-threads=1"])
        .stdout(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(role) = role {
        command.env(RACE_ROLE, role);
    }
    command.spawn().unwrap()
}

fn wait_child(child: &mut std::process::Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the other process did not finish in 60 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One side of the race, in two parts.
///
/// First both sides meet at a barrier and try at once; a side that gets in claims `inside` with
/// `O_EXCL` for a moment, so two sides in at once fail that claim and leave a `both-` file. Both
/// meet again before the next round, so neither still holds the slot when it starts. Which side
/// wins is up to the scheduler.
///
/// Then the sides take turns to lead: the leader takes the slot and says so, the other tries
/// against it (and must be refused with the note for the leader's kind of hold), then the leader
/// lets go and the other must get in. That shows each side refused and each side admitted, every
/// time, whatever the scheduler does.
///
/// Writes how many first-part rounds it got in, then every way the second part went wrong.
fn race(base: &Path, role: &str) {
    let slot = WriteSlot::open(&review_dir_in(base)).unwrap();
    let other = if role == "a" { "b" } else { "a" };

    let mut took = 0u32;
    for round in 0..RACE_ROUNDS {
        mark(base, "ready", role, round);
        wait_for(base, "ready", other, round);
        match take(&slot, role, round % 2 == 1) {
            Ok(hold) => {
                took += 1;
                let inside = base.join("inside");
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&inside) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => mark(base, "both", role, round),
                    Err(e) => panic!("the claim could not be made: {e}"),
                }
                std::thread::sleep(Duration::from_millis(1));
                let _ = std::fs::remove_file(&inside);
                drop(hold);
            }
            Err(SlotBusy::Unavailable(why)) => panic!("the slot could not be taken: {why}"),
            Err(_) => {}
        }
        mark(base, "done", role, round);
        wait_for(base, "done", other, round);
    }

    let mut wrong = Vec::new();
    for round in 0..TURN_ROUNDS {
        let leader = if round % 2 == 0 { "a" } else { "b" };
        // Over four rounds `b` leads and follows once with each kind of hold.
        let read = (round / 2) % 2 == 1;
        if role == leader {
            let hold = take(&slot, role, read);
            if let Err(e) = &hold {
                wrong.push(format!("round {round}: the leader was refused a free slot: {e}"));
            }
            mark(base, "held", role, round);
            wait_for(base, "tried", other, round);
            drop(hold);
            mark(base, "released", role, round);
            wait_for(base, "after", other, round);
        } else {
            wait_for(base, "held", leader, round);
            let expected = if leader == "b" && read {
                SlotBusy::Reading
            } else {
                SlotBusy::Writing
            };
            match take(&slot, role, read) {
                Ok(_) => wrong.push(format!("round {round}: got in while the other side held the slot")),
                Err(SlotBusy::Unavailable(why)) => panic!("the slot could not be taken: {why}"),
                Err(busy) if busy != expected => {
                    wrong.push(format!("round {round}: refused with {busy:?}, not {expected:?}"))
                }
                Err(_) => {}
            }
            mark(base, "tried", role, round);
            wait_for(base, "released", leader, round);
            if let Err(e) = take(&slot, role, read) {
                wrong.push(format!("round {round}: refused once the other side let go: {e}"));
            }
            mark(base, "after", role, round);
        }
    }
    std::fs::write(
        base.join(format!("result-{role}")),
        format!("{took}\n{}", wrong.join("\n")),
    )
    .unwrap();
}

/// The child side of the two-process race. Does nothing unless the parent set its directory.
#[test]
#[ignore = "run by the two-process race"]
fn write_slot_race_child() {
    let (Some(dir), Ok(role)) = (std::env::var_os(RACE_DIR), std::env::var(RACE_ROLE)) else {
        return;
    };
    race(Path::new(&dir), &role);
}

#[test]
fn a_revert_in_one_process_and_a_revert_or_a_disk_reading_send_in_another_never_both_proceed() {
    let scratch = Scratch::new("race");
    let base = scratch.0.clone();
    std::fs::create_dir_all(review_dir_in(&base)).unwrap();
    let mut child = spawn_child("write_slot_race_child", &[(RACE_DIR, &base)], Some("b"));
    race(&base, "a");
    assert!(wait_child(&mut child).success(), "the other process failed");

    let both: Vec<String> = std::fs::read_dir(&base)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("both-"))
        .collect();
    assert!(both.is_empty(), "both processes were in the slot at once: {both:?}");

    let result = |role: &str| -> (u32, Vec<String>) {
        let text = std::fs::read_to_string(base.join(format!("result-{role}"))).unwrap();
        let mut lines = text.lines();
        let took = lines.next().unwrap().parse().unwrap();
        (took, lines.map(str::to_owned).collect())
    };
    let ((a_took, a_wrong), (b_took, b_wrong)) = (result("a"), result("b"));
    // The two sides are never in at once (no `both-` file), and a side is refused only by a hold of the other's, so
    // at least one gets in every round. Both may get in one after the other when one side is descheduled until the
    // other has let go; that is a legitimate take, not an overlap.
    assert!(
        a_took + b_took >= RACE_ROUNDS,
        "someone got in every round: a {a_took}, b {b_took}"
    );
    assert!(a_wrong.is_empty(), "a: {a_wrong:?}");
    assert!(b_wrong.is_empty(), "b: {b_wrong:?}");
}

/// The child that takes the slot under a umask that takes the owner's write bit away. Does
/// nothing unless the parent set its directory.
#[test]
#[ignore = "run by the umask test"]
fn write_slot_umask_child() {
    let Some(dir) = std::env::var_os(UMASK_DIR) else {
        return;
    };
    // SAFETY: `umask` takes and returns plain integers; this process runs only this test.
    unsafe { libc::umask(0o277) };
    let dir = review_dir_in(Path::new(&dir));
    let slot = WriteSlot::open(&dir).unwrap();
    drop(slot.try_exclusive().expect("the first take"));
    let mode = std::fs::metadata(dir.join("write.lock")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the slot file is 0600 whatever the umask");
    drop(slot.try_exclusive().expect("a later take"));
    drop(slot.try_shared().expect("a later read"));
    drop(
        WriteSlot::open(&dir)
            .unwrap()
            .try_exclusive()
            .expect("another window's take"),
    );
}

#[test]
fn a_restrictive_umask_does_not_lock_the_slot_for_good() {
    let scratch = Scratch::new("umask");
    let mut child = spawn_child("write_slot_umask_child", &[(UMASK_DIR, &scratch.0)], None);
    assert!(wait_child(&mut child).success(), "the umask child failed");
}

#[test]
fn trimming_the_store_never_removes_a_held_write_slot() {
    let scratch = Scratch::new("trim");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("a.txt"), "a\n").unwrap();
    let dir = review_dir_in(&scratch.0);
    let shadow = Shadow::open_with_excludes(&dir, &project, None).unwrap();
    let mine = WriteSlot::open(&dir).unwrap();
    let writing = mine.try_exclusive().unwrap();
    let inode = std::fs::metadata(dir.join("write.lock")).unwrap().ino();
    // A git index lock a killed run left beside it still goes.
    std::fs::write(dir.join("index-killed.lock"), b"").unwrap();

    // The collection clears stale lock files under the store's lock, which a revert holding the
    // write slot does not take.
    shadow.trim(0, Duration::ZERO, u64::MAX / 2, "now").unwrap();

    assert!(!dir.join("index-killed.lock").exists(), "the sweep ran");
    assert_eq!(
        std::fs::symlink_metadata(dir.join("write.lock")).map(|m| m.ino()).ok(),
        Some(inode),
        "the held slot file is still the one at its name"
    );
    let other = WriteSlot::open(&dir).unwrap();
    assert_eq!(other.try_exclusive().unwrap_err(), SlotBusy::Writing);
    assert_eq!(other.try_shared().unwrap_err(), SlotBusy::Writing);
    drop(writing);
    // A hold just let go can stay held for a moment: a child another test thread is starting (git, for a
    // snapshot) holds a copy of every descriptor of this process until it execs.
    let deadline = Instant::now() + Duration::from_secs(5);
    let free = loop {
        match other.try_exclusive() {
            Err(SlotBusy::Writing | SlotBusy::Reading) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            taken => break taken,
        }
    };
    drop(free.expect("free once let go"));
}
