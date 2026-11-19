#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use daemon::sync::{self, activity, files, listing};
use eitri_server_proto::*;
use sha2::{Digest, Sha256};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// A project tree and a state directory of the test's own, removed when it ends.
struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "sync-files-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        agent::private_fs::create_private_dir_all(&base, &base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        std::fs::create_dir(base.join("project")).unwrap();
        std::fs::create_dir(base.join("state")).unwrap();
        Self { base }
    }

    fn project(&self) -> PathBuf {
        self.base.join("project")
    }

    fn state(&self) -> PathBuf {
        self.base.join("state")
    }

    fn root(&self) -> sync::Root {
        sync::root(self.project().to_str().unwrap()).unwrap()
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.project().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.state().join("audit.log")).unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn nothing_ignored(_: &str, _: bool) -> bool {
    false
}

fn later() -> std::time::Instant {
    std::time::Instant::now() + Duration::from_secs(60)
}

fn listed(root: &sync::Root, ignored: listing::Ignored, budget: u64) -> Vec<listing::Listed> {
    let mut seen = Vec::new();
    listing::list_tree(&root.dir, ignored, budget, &mut |step| {
        seen.push(step);
        true
    })
    .unwrap();
    seen
}

fn read(root: &sync::Root, rel: &str, limit: u64) -> Vec<ReadFileChunk> {
    let mut chunks = Vec::new();
    let last = files::read_file(root, rel, limit, &mut |chunk| {
        chunks.push(chunk);
        true
    });
    chunks.extend(last);
    chunks
}

fn replace(scratch: &Scratch, rel: &str, expected: &str, data: &[u8]) -> ReplaceFileIfResponse {
    files::replace_if(
        &scratch.state(),
        &scratch.root(),
        &ReplaceFileIfRequest {
            root: scratch.project().to_str().unwrap().into(),
            rel: rel.into(),
            expected_sha256: expected.into(),
            data: data.to_vec(),
        },
    )
}

fn remove(scratch: &Scratch, rel: &str, expected: &str) -> RemoveFileIfResponse {
    files::remove_if(
        &scratch.state(),
        &scratch.root(),
        &RemoveFileIfRequest {
            root: scratch.project().to_str().unwrap().into(),
            rel: rel.into(),
            expected_sha256: expected.into(),
        },
    )
}

fn wait(scratch: &Scratch, rel: &str, sha256: &str, timeout_ms: u64) -> WaitFileHashResponse {
    files::wait_hash(
        &scratch.root(),
        &WaitFileHashRequest {
            root: scratch.project().to_str().unwrap().into(),
            rel: rel.into(),
            sha256: sha256.into(),
            timeout_ms,
        },
    )
}

#[test]
fn list_tree_skips_ignored_and_never_follows_symlinks() {
    let scratch = Scratch::new("list");
    scratch.write("a.txt", b"alpha");
    scratch.write("dir/b.txt", b"beta!");
    scratch.write("ignored/x", b"hidden");
    scratch.write(".git/config", b"[core]");
    scratch.write("nested/.git/HEAD", b"ref");
    scratch.write("nested/c.txt", b"c");
    std::fs::create_dir(scratch.base.join("outside")).unwrap();
    std::fs::write(scratch.base.join("outside/secret"), b"secret").unwrap();
    std::os::unix::fs::symlink("../outside", scratch.project().join("link")).unwrap();
    std::os::unix::fs::symlink("dir", scratch.project().join("linkdir")).unwrap();
    let ignore = |rel: &str, is_dir: bool| rel == "ignored" && is_dir;
    let entries: Vec<(String, &str, u64)> = listed(&scratch.root(), &ignore, 100)
        .into_iter()
        .map(|step| match step {
            listing::Listed::Entry(visit) => (visit.rel, visit.kind.wire(), visit.size),
            listing::Listed::Truncated => panic!("not truncated"),
        })
        .collect();
    assert_eq!(
        entries,
        vec![
            ("a.txt".to_owned(), "file", 5),
            ("dir".to_owned(), "directory", 0),
            ("dir/b.txt".to_owned(), "file", 5),
            ("link".to_owned(), "symlink", "../outside".len() as u64),
            ("linkdir".to_owned(), "symlink", 3),
            ("nested".to_owned(), "directory", 0),
            ("nested/c.txt".to_owned(), "file", 1),
        ]
    );
    // The request's patterns reach the walk through the predicate the handler builds.
    let from_patterns = listing::ignore_predicate(&["dir".to_owned(), "ignored".to_owned()]);
    let rels: Vec<String> = listed(&scratch.root(), &*from_patterns, 100)
        .into_iter()
        .filter_map(|step| match step {
            listing::Listed::Entry(visit) => Some(visit.rel),
            listing::Listed::Truncated => None,
        })
        .collect();
    assert_eq!(rels, ["a.txt", "link", "linkdir", "nested", "nested/c.txt"]);
}

#[test]
fn list_tree_reports_truncation() {
    let scratch = Scratch::new("truncate");
    for name in ["1", "2", "3", "4", "5"] {
        scratch.write(name, name.as_bytes());
    }
    let short = listed(&scratch.root(), &nothing_ignored, 3);
    assert_eq!(short.len(), 4);
    assert!(short[..3].iter().all(|step| matches!(step, listing::Listed::Entry(_))));
    assert_eq!(short[3], listing::Listed::Truncated);
    let whole = listed(&scratch.root(), &nothing_ignored, 5);
    assert_eq!(whole.len(), 5);
    assert!(!whole.contains(&listing::Listed::Truncated));
    // Path sizes stop at their budget the same way, and say so.
    let rels = ["1".to_owned(), "2".to_owned(), "3".to_owned()];
    let (sizes, truncated) = listing::path_sizes(&scratch.root().dir, &rels, &nothing_ignored, 2, later()).unwrap();
    assert!(truncated);
    assert_eq!(sizes.len(), 2);
}

#[test]
fn path_sizes_sum_a_directory_without_following_links_or_ignored_names() {
    let scratch = Scratch::new("sizes");
    scratch.write("big/one", &[0; 1000]);
    scratch.write("big/sub/two", &[0; 24]);
    scratch.write("big/skip/three", &[0; 5000]);
    std::fs::create_dir(scratch.base.join("outside")).unwrap();
    std::fs::write(scratch.base.join("outside/huge"), [0; 9000]).unwrap();
    std::os::unix::fs::symlink("../../outside", scratch.project().join("big/out")).unwrap();
    let ignore = |rel: &str, _: bool| rel == "big/skip";
    let rels = ["big".to_owned(), "big/one".to_owned(), "missing/file".to_owned()];
    let (sizes, truncated) = listing::path_sizes(&scratch.root().dir, &rels, &ignore, 100, later()).unwrap();
    assert!(!truncated);
    let link = "../../outside".len() as u64;
    assert_eq!(
        sizes,
        vec![
            ("big".to_owned(), 1000 + 24 + link),
            ("big/one".to_owned(), 1000),
            ("missing/file".to_owned(), 0),
        ]
    );
}

#[test]
fn read_streams_up_to_the_limit_and_refuses_past_it() {
    let scratch = Scratch::new("read");
    let bytes: Vec<u8> = (0..200 * 1024u32).map(|i| (i * 7 % 251) as u8).collect();
    scratch.write("data.bin", &bytes);
    let chunks = read(&scratch.root(), "data.bin", bytes.len() as u64);
    let (last, data) = chunks.split_last().unwrap();
    assert!(data
        .iter()
        .all(|chunk| chunk.data.len() <= sync::CHUNK_BYTES && chunk.refusal.is_empty()));
    assert_eq!(
        data.iter().flat_map(|chunk| chunk.data.clone()).collect::<Vec<u8>>(),
        bytes
    );
    assert_eq!((last.kind.as_str(), last.sha256.clone()), ("file", sha(&bytes)));
    assert!(last.refusal.is_empty() && last.data.is_empty());

    let refused = read(&scratch.root(), "data.bin", bytes.len() as u64 - 1);
    assert_eq!(refused.len(), 1, "nothing is sent before a refusal by size");
    assert!(
        refused[0].refusal.contains(&format!("{}-byte limit", bytes.len() - 1)),
        "{:?}",
        refused[0]
    );
    let too_much = read(&scratch.root(), "data.bin", sync::MAX_READ_BYTES + 1);
    assert!(
        too_much[0].refusal.contains(&sync::MAX_READ_BYTES.to_string()),
        "{:?}",
        too_much[0]
    );

    let missing = read(&scratch.root(), "no/such/file", 10);
    assert_eq!((missing.len(), missing[0].kind.as_str()), (1, "missing"));
    std::fs::create_dir(scratch.project().join("folder")).unwrap();
    assert_eq!(read(&scratch.root(), "folder", 10)[0].kind, "directory");
}

#[test]
fn read_reports_a_symlink_without_following_it() {
    let scratch = Scratch::new("read-link");
    std::fs::create_dir(scratch.base.join("outside")).unwrap();
    std::fs::write(scratch.base.join("outside/secret"), b"secret").unwrap();
    std::os::unix::fs::symlink("../outside/secret", scratch.project().join("link")).unwrap();
    let chunks = read(&scratch.root(), "link", 1024);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, "symlink");
    assert_eq!(chunks[0].data, b"../outside/secret");
    assert!(chunks[0].sha256.is_empty() && chunks[0].refusal.is_empty());
}

#[test]
fn compare_and_replace_keeps_the_mode_and_leaves_nothing_else_behind() {
    use eitri_core::turn_review::write::{compare_and_replace, ProjectDir, WriteError};
    let scratch = Scratch::new("compare");
    scratch.write("tool.sh", b"#!/bin/sh\necho old\n");
    let path = scratch.project().join("tool.sh");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
    let before = std::fs::metadata(&path).unwrap();
    let project = ProjectDir::open(&scratch.project()).unwrap();
    let target = project.target(Path::new("tool.sh"), false).unwrap();
    let expected: [u8; 32] = Sha256::digest(b"#!/bin/sh\necho old\n").into();
    compare_and_replace(&target, Some(expected), b"#!/bin/sh\necho new\n").unwrap();
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\necho new\n");
    assert_eq!(after.permissions().mode() & 0o7777, 0o750);
    assert_eq!(after.gid(), before.gid());
    assert_ne!(
        after.ino(),
        before.ino(),
        "replaced by a rename, never rewritten in place"
    );
    // The old hash no longer matches: the second call writes nothing.
    let target = project.target(Path::new("tool.sh"), false).unwrap();
    assert_eq!(
        compare_and_replace(&target, Some(expected), b"other"),
        Err(WriteError::Changed)
    );
    // A new file, where nothing was expected, gets the bits a synchronized new file has.
    let target = project.target(Path::new("fresh/new.txt"), true).unwrap();
    compare_and_replace(&target, None, b"fresh").unwrap();
    let fresh = scratch.project().join("fresh/new.txt");
    assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o7777, 0o600);
    // Nothing else was written: no temporary is left beside the files.
    let mut names: Vec<String> = std::fs::read_dir(scratch.project())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["fresh", "tool.sh"]);
    assert_eq!(std::fs::read_dir(scratch.project().join("fresh")).unwrap().count(), 1);
}

#[test]
fn replace_if_refuses_a_changed_file_and_returns_its_hash() {
    let scratch = Scratch::new("replace");
    scratch.write("notes.md", b"server side\n");
    let wrong = sha(b"laptop read this\n");
    let refused = replace(&scratch, "notes.md", &wrong, b"laptop\n");
    assert!(!refused.replaced);
    assert_eq!(
        refused.refusal,
        format!(
            "notes.md changed on the server since it was read (now: {})",
            sha(b"server side\n")
        )
    );
    assert_eq!(
        std::fs::read(scratch.project().join("notes.md")).unwrap(),
        b"server side\n"
    );
    // "Must be missing" against a file that exists is the same refusal.
    let appeared = replace(&scratch, "notes.md", "", b"laptop\n");
    assert!(
        appeared.refusal.ends_with(&format!("(now: {})", sha(b"server side\n"))),
        "{appeared:?}"
    );
    let done = replace(&scratch, "notes.md", &sha(b"server side\n"), b"laptop\n");
    assert!(done.replaced && done.refusal.is_empty(), "{done:?}");
    assert_eq!(std::fs::read(scratch.project().join("notes.md")).unwrap(), b"laptop\n");
    let created = replace(&scratch, "new/dir/file.txt", "", b"made\n");
    assert!(created.replaced, "{created:?}");
    let too_big = replace(&scratch, "big", "", &vec![0; sync::MAX_REPLACE_BYTES + 1]);
    assert!(
        too_big.refusal.contains(&sync::MAX_REPLACE_BYTES.to_string()),
        "{too_big:?}"
    );
    let audit = scratch.audit();
    assert!(audit.contains("replace root="), "{audit}");
    assert!(audit.contains("rel=notes.md done"), "{audit}");
    assert!(audit.contains("rel=notes.md refused"), "{audit}");
}

#[test]
fn remove_if_refuses_a_directory_a_symlink_and_a_changed_file() {
    let scratch = Scratch::new("remove");
    std::fs::create_dir(scratch.project().join("folder")).unwrap();
    std::os::unix::fs::symlink("target", scratch.project().join("link")).unwrap();
    scratch.write("file", b"current\n");
    let any = sha(b"anything");
    for (rel, now) in [
        ("folder", "directory".to_owned()),
        ("link", "symlink".to_owned()),
        ("file", sha(b"current\n")),
        ("gone", "missing".to_owned()),
    ] {
        let refused = remove(&scratch, rel, &any);
        assert!(!refused.removed);
        assert_eq!(
            refused.refusal,
            format!("{rel} changed on the server since it was read (now: {now})")
        );
    }
    assert!(scratch.project().join("folder").is_dir());
    assert!(scratch.project().join("link").symlink_metadata().is_ok());
    assert!(remove(&scratch, "file", "").refusal.contains("lowercase hex"));
    let removed = remove(&scratch, "file", &sha(b"current\n"));
    assert!(removed.removed, "{removed:?}");
    assert!(!scratch.project().join("file").exists());
    assert!(scratch.audit().contains("remove root="));
    assert!(scratch.audit().contains("rel=file done"));
}

#[test]
fn paths_through_a_symlinked_dir_are_refused() {
    let scratch = Scratch::new("through-link");
    scratch.write("real/file.txt", b"inside\n");
    std::fs::create_dir(scratch.base.join("outside")).unwrap();
    std::fs::write(scratch.base.join("outside/file.txt"), b"outside\n").unwrap();
    std::os::unix::fs::symlink("real", scratch.project().join("inner")).unwrap();
    std::os::unix::fs::symlink("../outside", scratch.project().join("away")).unwrap();
    for rel in ["inner/file.txt", "away/file.txt"] {
        let link = rel.split('/').next().unwrap();
        let why = format!("{rel} is not inside the project: {link} is a link or not a directory");
        assert_eq!(read(&scratch.root(), rel, 100)[0].refusal, why);
        let hash = sha(b"inside\n");
        assert_eq!(replace(&scratch, rel, &hash, b"x").refusal, why);
        assert_eq!(remove(&scratch, rel, &hash).refusal, why);
        assert_eq!(wait(&scratch, rel, &hash, 100).refusal, why);
        let rels = [rel.to_owned()];
        assert_eq!(
            listing::path_sizes(&scratch.root().dir, &rels, &nothing_ignored, 10, later()).unwrap_err(),
            why
        );
    }
    assert_eq!(
        std::fs::read(scratch.project().join("real/file.txt")).unwrap(),
        b"inside\n"
    );
    assert_eq!(
        std::fs::read(scratch.base.join("outside/file.txt")).unwrap(),
        b"outside\n"
    );
}

#[test]
fn dot_git_rels_are_refused() {
    let scratch = Scratch::new("dot-git");
    scratch.write(".git/config", b"[core]\n");
    scratch.write("sub/.git/HEAD", b"ref\n");
    let hash = sha(b"[core]\n");
    for rel in [
        ".git/config",
        "sub/.git/HEAD",
        ".GIT/config",
        "../escape",
        "/etc/passwd",
        "a//b",
        "",
    ] {
        let refused = read(&scratch.root(), rel, 100);
        assert_eq!(refused.len(), 1);
        assert!(!refused[0].refusal.is_empty(), "{rel}");
        assert!(!replace(&scratch, rel, &hash, b"x").refusal.is_empty(), "{rel}");
        assert!(!remove(&scratch, rel, &hash).refusal.is_empty(), "{rel}");
        assert!(!wait(&scratch, rel, &hash, 10).refusal.is_empty(), "{rel}");
        let rels = [rel.to_owned()];
        assert!(
            listing::path_sizes(&scratch.root().dir, &rels, &nothing_ignored, 10, later()).is_err(),
            "{rel}"
        );
    }
    assert_eq!(
        std::fs::read(scratch.project().join(".git/config")).unwrap(),
        b"[core]\n"
    );
}

#[test]
fn wait_sees_a_write_that_landed_before_arming() {
    let scratch = Scratch::new("wait-before");
    scratch.write("saved.txt", b"already here\n");
    let waited = wait(&scratch, "saved.txt", &sha(b"already here\n"), 10_000);
    assert!(waited.matched, "{waited:?}");
    assert_eq!(waited.observed, sha(b"already here\n"));
    // Waiting for a removal that already happened matches the same way.
    let gone = wait(&scratch, "not-there.txt", "", 10_000);
    assert!(gone.matched && gone.observed == "missing", "{gone:?}");
}

/// Runs `change` on another thread once the wait has had time to arm, and waits for `rel` to reach `bytes`.
fn wait_for_change(scratch: &Scratch, rel: &str, bytes: &'static [u8], change: impl FnOnce(PathBuf) + Send + 'static) {
    let project = scratch.project();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        change(project);
    });
    let waited = wait(scratch, rel, &sha(bytes), 10_000);
    writer.join().unwrap();
    assert!(waited.matched, "{waited:?}");
    assert_eq!(waited.observed, sha(bytes));
}

#[test]
fn wait_sees_mutagen_style_rename_into_place() {
    let scratch = Scratch::new("wait-rename");
    scratch.write("dir/file.rs", b"old\n");
    wait_for_change(&scratch, "dir/file.rs", b"new\n", |project| {
        let staged = project.join("dir/.mutagen-temporary-cross-device-rename0");
        std::fs::write(&staged, b"new\n").unwrap();
        std::fs::rename(staged, project.join("dir/file.rs")).unwrap();
    });
}

#[test]
fn wait_sees_a_file_created_in_a_new_directory() {
    let scratch = Scratch::new("wait-new-dir");
    wait_for_change(&scratch, "made/later/file.txt", b"arrived\n", |project| {
        std::fs::create_dir(project.join("made")).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        std::fs::create_dir(project.join("made/later")).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(project.join("made/later/file.txt"), b"arrived\n").unwrap();
    });
}

#[test]
fn wait_times_out_with_the_last_hash() {
    let scratch = Scratch::new("wait-timeout");
    scratch.write("file", b"what is there\n");
    let waited = wait(&scratch, "file", &sha(b"what was hoped for\n"), 200);
    assert!(!waited.matched);
    assert_eq!(waited.observed, sha(b"what is there\n"));
    let missing = wait(&scratch, "absent", &sha(b"x"), 50);
    assert!(!missing.matched && missing.observed == "missing", "{missing:?}");
    assert!(wait(&scratch, "file", &sha(b"x"), sync::MAX_WAIT_MS + 1)
        .refusal
        .contains(&sync::MAX_WAIT_MS.to_string()));
}

#[test]
fn project_activity_reports_a_running_turn_and_unknowns() {
    use activity::{judge, Facts, SessionFacts, Tasks, Turn};
    let sidecar = |id: &str, root: &str, handshake: bool| SidecarInfo {
        sidecar_id: id.into(),
        root: root.into(),
        alive: true,
        handshake,
        ..Default::default()
    };
    let session = |sidecar: &str, turn: Turn| SessionFacts {
        sidecar: sidecar.into(),
        closed: false,
        ended: None,
        current: true,
        turn,
        tasks: Tasks::Known(0),
    };
    let root = "/srv/project";
    let sidecars = vec![
        sidecar("here", root, true),
        sidecar("parent", "/srv", true),
        sidecar("elsewhere", "/srv/project-two", true),
    ];
    let facts = |sessions: Vec<SessionFacts>| Facts {
        adopted: true,
        sessions,
        unobserved: Vec::new(),
    };
    let quiet = judge(root, &facts(vec![session("here", Turn::Idle)]), &sidecars, 0);
    assert_eq!((quiet.turns_running, quiet.unknown), (0, false));
    let running = facts(vec![
        session("here", Turn::Running),
        session("parent", Turn::Running),
        session("elsewhere", Turn::Running),
        SessionFacts {
            closed: true,
            ..session("here", Turn::Running)
        },
        SessionFacts {
            ended: Some(tonic::Code::NotFound),
            ..session("here", Turn::Running)
        },
    ]);
    let busy = judge(root, &running, &sidecars, 0);
    assert_eq!((busy.turns_running, busy.unknown), (2, false), "{busy:?}");
    // Each thing that cannot be judged makes the answer unknown.
    assert!(judge(root, &facts(vec![session("here", Turn::Unknown)]), &sidecars, 0).unknown);
    let ended = SessionFacts {
        ended: Some(tonic::Code::Unavailable),
        ..session("here", Turn::Idle)
    };
    assert!(judge(root, &facts(vec![ended]), &sidecars, 0).unknown);
    assert!(judge(root, &facts(Vec::new()), &[sidecar("here", root, false)], 0).unknown);
    let mut unobserved = facts(Vec::new());
    unobserved.unobserved.push("here".into());
    assert!(judge(root, &unobserved, &sidecars, 0).unknown);
    let mut adopting = facts(Vec::new());
    adopting.adopted = false;
    assert!(judge(root, &adopting, &sidecars, 0).unknown);
    assert!(judge(root, &facts(Vec::new()), &sidecars, 1).unknown);
    // A watch that is reconnecting or still taking in what it missed says nothing about now.
    let behind = SessionFacts {
        current: false,
        ..session("here", Turn::Idle)
    };
    assert!(judge(root, &facts(vec![behind]), &sidecars, 0).unknown);
    // Background tasks work outside any turn; an unknown roster is unknown.
    let tasks = SessionFacts {
        tasks: Tasks::Known(2),
        ..session("here", Turn::Idle)
    };
    let working = judge(root, &facts(vec![tasks]), &sidecars, 0);
    assert_eq!(
        (working.turns_running, working.tasks_running, working.unknown),
        (0, 2, false)
    );
    let roster = SessionFacts {
        tasks: Tasks::Unknown,
        ..session("here", Turn::Idle)
    };
    assert!(judge(root, &facts(vec![roster]), &sidecars, 0).unknown);
    // A sidecar elsewhere, or one that is no longer alive, changes nothing.
    let dead = SidecarInfo {
        alive: false,
        ..sidecar("here", root, false)
    };
    let calm = judge(
        root,
        &facts(vec![session("elsewhere", Turn::Unknown)]),
        &[sidecar("elsewhere", "/srv/project-two", false), dead],
        0,
    );
    assert_eq!((calm.turns_running, calm.unknown), (0, false));
}

#[test]
fn other_installations_daemons_with_sidecars_at_the_root_are_counted() {
    let scratch = Scratch::new("others");
    let state = scratch.state();
    let own = state.join("run/own");
    let other = state.join("run/other");
    for run in [&own, &other] {
        agent::private_fs::create_private_dir_all(run, &state).unwrap();
    }
    let root = scratch.project().to_str().unwrap().to_owned();
    assert_eq!(activity::other_daemons_at(&state, &own, &root), 0);
    // This test's own process stands in for a sidecar the other daemon runs.
    let pid = std::process::id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start_time: u64 = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let record = |root: &str| {
        serde_json::json!({
            "pid": pid,
            "start_time": start_time,
            "socket": "/nonexistent",
            "root": root,
        })
        .to_string()
    };
    let file = other.join("sc-0123456789abcdef.pid");
    agent::private_fs::write_private(&file, record("/somewhere/else").as_bytes()).unwrap();
    assert_eq!(activity::other_daemons_at(&state, &own, &root), 0);
    agent::private_fs::write_private(&file, record(&root).as_bytes()).unwrap();
    assert_eq!(activity::other_daemons_at(&state, &own, &root), 1);
    // The same record in this daemon's own run directory is this daemon's, not another's.
    std::fs::rename(&file, own.join("sc-0123456789abcdef.pid")).unwrap();
    assert_eq!(activity::other_daemons_at(&state, &own, &root), 0);
}

#[test]
fn writes_go_to_the_held_root_even_when_a_directory_above_it_is_swapped_for_a_link() {
    let scratch = Scratch::new("swapped-ancestor");
    let above = scratch.base.join("above");
    std::fs::create_dir_all(above.join("project")).unwrap();
    std::fs::create_dir_all(scratch.base.join("elsewhere/project")).unwrap();
    let root = sync::root(above.join("project").to_str().unwrap()).unwrap();
    // After the root was checked, the directory above it moves away and a link to another tree takes its name.
    std::fs::rename(&above, scratch.base.join("moved")).unwrap();
    std::os::unix::fs::symlink(scratch.base.join("elsewhere"), &above).unwrap();
    let request = ReplaceFileIfRequest {
        root: root.canonical.clone(),
        rel: "new.txt".into(),
        expected_sha256: String::new(),
        data: b"written\n".to_vec(),
    };
    let done = files::replace_if(&scratch.state(), &root, &request);
    assert!(done.replaced, "{done:?}");
    assert!(
        !scratch.base.join("elsewhere/project/new.txt").exists(),
        "the write followed the swapped path"
    );
    assert_eq!(
        std::fs::read(scratch.base.join("moved/project/new.txt")).unwrap(),
        b"written\n"
    );
}

#[test]
fn directories_made_for_a_new_file_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("private-dirs");
    // A permissive umask must not widen them.
    let previous = unsafe { libc::umask(0o022) };
    let done = replace(&scratch, "made/deeper/file.txt", "", b"x");
    unsafe { libc::umask(previous) };
    assert!(done.replaced, "{done:?}");
    for dir in ["made", "made/deeper"] {
        let mode = std::fs::metadata(scratch.project().join(dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700, "{dir}");
    }
}

#[test]
fn path_sizes_charge_every_requested_path_even_a_missing_one() {
    let scratch = Scratch::new("charge-missing");
    let rels: Vec<String> = (0..5).map(|n| format!("missing-{n}")).collect();
    let (sizes, truncated) = listing::path_sizes(&scratch.root().dir, &rels, &nothing_ignored, 2, later()).unwrap();
    assert!(truncated);
    assert_eq!(sizes.len(), 2);
}

#[test]
fn a_directory_is_never_read_past_what_the_walk_may_still_read() {
    use std::os::fd::AsRawFd;
    let scratch = Scratch::new("cap");
    for n in 0..20 {
        scratch.write(&format!("f{n}"), b"");
    }
    let root = scratch.root();
    assert!(listing::names_in(root.dir.as_raw_fd(), 10).unwrap().is_none());
    assert_eq!(listing::names_in(root.dir.as_raw_fd(), 20).unwrap().unwrap().len(), 20);
    // A listing whose read allowance runs out says it is truncated.
    let mut allowance = 5;
    let ended = listing::walk(&root.dir, &nothing_ignored, &mut allowance, &mut |_| listing::Flow::Go).unwrap();
    assert_eq!(ended, listing::Flow::Exhausted);
}

#[test]
fn hashing_a_file_is_bounded_by_its_size_and_the_deadline() {
    let scratch = Scratch::new("bounded-hash");
    let huge = std::fs::File::create(scratch.project().join("huge")).unwrap();
    huge.set_len(sync::MAX_READ_BYTES + 1).unwrap();
    scratch.write("small", b"small");
    let root = scratch.root();
    assert_eq!(files::fact(&root, "huge", later()).unwrap(), files::Fact::TooLarge);
    assert_eq!(
        files::fact(&root, "small", std::time::Instant::now()).unwrap(),
        files::Fact::Unfinished
    );
    let waited = wait(&scratch, "huge", &sha(b"x"), 100);
    assert_eq!((waited.matched, waited.observed.as_str()), (false, "too-large"));
}
