//! The shadow repository against real git, in directories under the system's temporary directory.
//!
//! The git this file runs itself (to build test projects and to inspect the shadow) is isolated
//! the same way the shadow's is -- a cleared environment, no global or system config -- so neither
//! the machine's git config nor the poisoned environment one test sets can change a result.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{mpsc, RwLock};
use std::time::Duration;

use eitri_core::turn_review::{
    git, review_dir_for, Limits, Shadow, ShadowError, Snapshot, SnapshotKind, SnapshotLabel, SnapshotOutcome,
};

/// One test changes the process environment that every child inherits; it takes this for writing,
/// every other test for reading.
static ENVIRONMENT: RwLock<()> = RwLock::new(());

fn environment() -> std::sync::RwLockReadGuard<'static, ()> {
    ENVIRONMENT.read().unwrap_or_else(|e| e.into_inner())
}

/// A directory of the test's own under the system's temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        // The system's temporary directory, not the target dir: these tests need user extended
        // attributes (the in-place write), git's fsmonitor and a disk fast enough for the snapshot
        // budget, and a checkout on a network mount has none of the three. A fresh directory of
        // this user's own holds it: a random name made here at 0700, never one already in place in
        // a shared directory, set exactly so whatever the umask, and removed on drop.
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!(
            "eitri-turn_review_shadow-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// A fresh project directory inside the scratch.
    fn project(&self) -> PathBuf {
        let p = self.path("project");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn review_dir(&self, project: &Path) -> PathBuf {
        review_dir_for(Some(self.path("state").as_os_str()), None, project).unwrap()
    }

    fn shadow(&self, project: &Path) -> Shadow {
        Shadow::open_with_excludes(&self.review_dir(project), project, None).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A snapshot or journal worker a test started can still write here for a moment after the
        // test's last line (they are not joined), and a removal racing it leaves part of the tree,
        // or the worker makes it again. So the directory must stay gone for a while, up to 5 s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut absent_since = None;
        while std::time::Instant::now() < deadline {
            if self.0.exists() {
                let _ = std::fs::remove_dir_all(&self.0);
                absent_since = None;
            } else if absent_since.get_or_insert_with(std::time::Instant::now).elapsed()
                > std::time::Duration::from_millis(100)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Test-side git in `dir`, isolated from the machine's config and from the process environment.
/// Automatic maintenance is off: after a `commit` git may start it in the background, and its
/// `objects/maintenance.lock` would then appear in a repository a test watches for changes.
fn tgit_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .current_dir(dir);
    cmd
}

fn tgit(dir: &Path, args: &[&str]) -> Output {
    let out = tgit_cmd(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn tgit_status(dir: &Path, args: &[&str]) -> bool {
    tgit_cmd(dir).args(args).output().unwrap().status.success()
}

fn sgit(shadow: &Shadow, args: &[&str]) -> Output {
    let mut cmd = tgit_cmd(shadow.review_dir());
    cmd.arg(format!("--git-dir={}", shadow.git_dir().display()));
    let out = cmd.args(args).output().unwrap();
    assert!(
        out.status.success(),
        "shadow git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn shadow_has_object(shadow: &Shadow, id: &str) -> bool {
    let mut cmd = tgit_cmd(shadow.review_dir());
    cmd.arg(format!("--git-dir={}", shadow.git_dir().display()));
    cmd.args(["cat-file", "-e", id]).output().unwrap().status.success()
}

fn text(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap().trim_end().to_owned()
}

/// `(mode, path)` of every entry in `commit`'s tree, recursively.
fn tree_entries(shadow: &Shadow, commit: &str) -> BTreeMap<String, String> {
    let out = sgit(shadow, &["ls-tree", "-r", "-z", "--full-tree", commit]);
    out.stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|line| {
            let line = String::from_utf8_lossy(line);
            let (meta, path) = line.split_once('\t').unwrap();
            (path.to_owned(), meta.split(' ').next().unwrap().to_owned())
        })
        .collect()
}

fn label(turn: u32, kind: SnapshotKind, time_ms: u64) -> SnapshotLabel {
    SnapshotLabel {
        turn,
        kind,
        turn_id: format!("turn-{turn}"),
        tab: 1,
        time_ms,
    }
}

fn taken(outcome: SnapshotOutcome) -> Snapshot {
    match outcome {
        SnapshotOutcome::Taken(s) => s,
        SnapshotOutcome::Unavailable(why) => panic!("snapshot unavailable: {why}"),
    }
}

fn snap(shadow: &Shadow, session: &str, turn: u32, kind: SnapshotKind) -> Snapshot {
    taken(shadow.snapshot(session, &label(turn, kind, 1_000 + u64::from(turn)), &Limits::default()))
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

/// Every file under `dir` with its bytes (and every directory, as an empty entry), recursively.
fn tree_bytes(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path.strip_prefix(dir).unwrap().to_path_buf();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                out.insert(rel, None);
                stack.push(path);
            } else {
                out.insert(rel, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    out
}

/// The only git commands run in the user's own environment: two read-only questions.
const USER_READ_ONLY: [&str; 2] = [
    "git config --global --type=path --get core.excludesFile",
    "git rev-parse --is-inside-work-tree --show-prefix --git-common-dir",
];

/// Every git command line a `GIT_TRACE` or trace2 normal-target file records as started.
fn traced_commands(trace: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(trace).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            ["start ", "built-in: ", "run_command: ", "exec: "]
                .iter()
                .find_map(|marker| line.split_once(marker).map(|(_, rest)| rest.trim().to_owned()))
        })
        .map(|command| {
            // macOS's `/usr/bin/git` re-executes the real one, which traces under its full path
            // (`/Applications/Xcode.app/.../git`); compare by program name as Linux's trace shows it.
            match command.split_once(' ') {
                Some((program, rest)) if program.ends_with("/git") => format!("git {rest}"),
                _ => command,
            }
        })
        .collect()
}

const SESSION: &str = "0b9c2d6e-1f2a-4c3b-9d8e-7f6a5b4c3d2e";

#[test]
fn the_projects_git_is_never_written() {
    let _env = environment();
    let scratch = Scratch::new("never-written");
    let project = scratch.project();
    tgit(&project, &["init", "-q"]);
    write(&project.join("a.txt"), b"one\n");
    write(&project.join("ignored.log"), b"log\n");
    write(&project.join(".gitignore"), b"*.log\n");
    write(&project.join(".git/info/exclude"), b"secret\n");
    tgit(&project, &["add", "a.txt", ".gitignore"]);
    tgit(&project, &["commit", "-qm", "first"]);
    write(&project.join("a.txt"), b"stashed\n");
    tgit(&project, &["stash", "-q"]);
    write(&project.join("b.txt"), b"staged\n");
    tgit(&project, &["add", "b.txt"]);
    write(&project.join("a.txt"), b"dirty\n");
    write(&project.join("secret"), b"excluded\n");
    write(&project.join("untracked.txt"), b"new\n");

    let observe = |project: &Path| {
        let bytes = tree_bytes(project);
        (
            bytes,
            text(&tgit(project, &["for-each-ref"])),
            text(&tgit(project, &["stash", "list"])),
            text(&tgit(project, &["count-objects", "-v"])),
        )
    };
    let before = observe(&project);

    let shadow = scratch.shadow(&project);
    let base = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    let end = snap(&shadow, SESSION, 1, SnapshotKind::End);
    assert_eq!(
        shadow.read_blob(&end.commit, Path::new("a.txt")).unwrap().as_deref(),
        Some(&b"dirty\n"[..])
    );
    assert_eq!(
        shadow.read_blob(&base.commit, Path::new("b.txt")).unwrap().as_deref(),
        Some(&b"staged\n"[..])
    );
    assert_eq!(
        shadow.read_blob(&base.commit, Path::new("secret")).unwrap(),
        None,
        "info/exclude is honoured"
    );
    assert_eq!(shadow.read_blob(&base.commit, Path::new("ignored.log")).unwrap(), None);
    assert!(!tree_entries(&shadow, &base.commit)
        .keys()
        .any(|p| p.starts_with(".git/")));

    let after = observe(&project);
    assert!(before.0 == after.0, "the project's files and .git changed");
    assert_eq!(before.1, after.1, "refs");
    assert_eq!(before.2, after.2, "stash");
    assert_eq!(before.3, after.3, "object count");
    assert!(!shadow.review_dir().starts_with(&project));
}

#[test]
fn no_filter_or_fsmonitor_runs() {
    let _env = ENVIRONMENT.write().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("no-filter");
    let home = scratch.path("home");
    std::fs::create_dir_all(&home).unwrap();
    let filter_marker = scratch.path("filter-ran");
    let fsmonitor_marker = scratch.path("fsmonitor-ran");
    let trace2_marker = scratch.path("trace2-written");
    let trace_marker = scratch.path("trace-written");
    let markers = [&filter_marker, &fsmonitor_marker, &trace2_marker, &trace_marker];
    let fsmonitor = scratch.path("fsmonitor.sh");
    script_fixture::write_executable_script(
        &fsmonitor,
        format!("#!/bin/sh\ntouch '{}'\n", fsmonitor_marker.display()).as_bytes(),
        0o755,
    )
    .unwrap();
    let fsmonitor = fsmonitor.display().to_string();
    // Unquoted, so it can sit inside GIT_CONFIG_PARAMETERS' own single quotes.
    let clean = format!("touch {}", filter_marker.display());
    assert!(!scratch.0.to_str().unwrap().contains([' ', '\'']));
    let settings = [
        ("filter.boom.clean", clean.clone()),
        ("core.fsmonitor", fsmonitor.clone()),
    ];
    // The global config also names a trace2 target, which git reads only from the global and
    // system config: written to if that config is read at all.
    write(
        &home.join(".gitconfig"),
        format!(
            "[filter \"boom\"]\n\tclean = {clean}\n[core]\n\tfsmonitor = {fsmonitor}\n[trace2]\n\tnormalTarget = {}\n",
            trace2_marker.display()
        )
        .as_bytes(),
    );
    let mut counted: Vec<(String, String)> = vec![("GIT_CONFIG_COUNT".into(), settings.len().to_string())];
    for (i, (key, value)) in settings.iter().enumerate() {
        counted.push((format!("GIT_CONFIG_KEY_{i}"), key.to_string()));
        counted.push((format!("GIT_CONFIG_VALUE_{i}"), value.clone()));
    }
    let parameters: Vec<String> = settings.iter().map(|(k, v)| format!("'{k}'='{v}'")).collect();
    let parameters = vec![("GIT_CONFIG_PARAMETERS".to_owned(), parameters.join(" "))];
    let global = vec![("HOME".to_owned(), home.display().to_string())];
    // Any inherited GIT_* variable at all: this one makes every git append a trace to the file.
    let traced = vec![("GIT_TRACE".to_owned(), trace_marker.display().to_string())];

    // The poison works on a plain git: each way of configuring it, alone, does what it says.
    let control = scratch.path("control");
    std::fs::create_dir_all(&control).unwrap();
    tgit(&control, &["init", "-q"]);
    write(&control.join(".gitattributes"), b"* filter=boom\n");
    type Way<'a> = (&'a Vec<(String, String)>, &'a [&'a PathBuf]);
    let ways: [Way; 4] = [
        (&global, &[&filter_marker, &fsmonitor_marker, &trace2_marker]),
        (&counted, &[&filter_marker, &fsmonitor_marker]),
        (&parameters, &[&filter_marker, &fsmonitor_marker]),
        (&traced, &[&trace_marker]),
    ];
    for (i, (way, expected)) in ways.iter().enumerate() {
        write(&control.join("f"), format!("{i}\n").as_bytes());
        for args in [&["add", "-A"][..], &["status"]] {
            let mut cmd = Command::new("git");
            cmd.current_dir(&control).args(args).env("HOME", &home);
            if i > 0 {
                cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
            }
            cmd.envs(way.iter().map(|(k, v)| (k, v)));
            cmd.output().unwrap();
        }
        for marker in markers {
            assert_eq!(
                marker.exists(),
                expected.contains(&marker),
                "control {i}: {}",
                marker.display()
            );
            let _ = std::fs::remove_file(marker);
        }
    }
    let poison: Vec<(String, String)> = global
        .into_iter()
        .chain(counted)
        .chain(parameters)
        .chain(traced)
        .collect();

    let project = scratch.project();
    tgit(&project, &["init", "-q"]);
    write(&project.join(".gitattributes"), b"* filter=boom\n");
    write(&project.join("a.txt"), b"one\n");

    let saved: Vec<_> = poison.iter().map(|(k, _)| (k.clone(), std::env::var_os(k))).collect();
    for (k, v) in &poison {
        std::env::set_var(k, v);
    }
    let result = std::panic::catch_unwind(|| {
        let shadow = Shadow::open(&scratch.review_dir(&project), &project).unwrap();
        let first = snap(&shadow, SESSION, 1, SnapshotKind::Base);
        write(&project.join("a.txt"), b"two\n");
        write(&project.join("b.txt"), b"new\n");
        let second = snap(&shadow, SESSION, 1, SnapshotKind::End);
        assert_eq!(
            shadow.read_blob(&first.commit, Path::new("a.txt")).unwrap().as_deref(),
            Some(&b"one\n"[..])
        );
        assert_eq!(
            shadow.read_blob(&second.commit, Path::new("a.txt")).unwrap().as_deref(),
            Some(&b"two\n"[..])
        );
        shadow.trim(100, Duration::from_secs(3600), 2_000, "now").unwrap();
    });
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    result.unwrap();
    assert!(!filter_marker.exists(), "a clean filter ran");
    assert!(!fsmonitor_marker.exists(), "an fsmonitor hook ran");
    // The two traces see the read-only questions asked of the user's own git, which runs with the
    // user's environment and config by design; the shadow's own git must not appear in either.
    for trace in [&trace2_marker, &trace_marker] {
        let commands = traced_commands(trace);
        assert!(
            !commands.is_empty(),
            "{} saw nothing; the poison did not reach the user's git",
            trace.display()
        );
        for command in commands {
            assert!(
                USER_READ_ONLY.iter().any(|allowed| command.starts_with(allowed)),
                "a shadow git saw the poisoned environment or config: {command}"
            );
        }
    }
}

#[test]
fn snapshots_are_byte_exact() {
    let _env = environment();
    let scratch = Scratch::new("byte-exact");
    let project = scratch.project();
    tgit(&project, &["init", "-q"]);
    write(
        &project.join(".gitattributes"),
        b"* text=auto\nutf16.txt text working-tree-encoding=UTF-16\neol.txt eol=crlf\n",
    );
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "h\u{e9}llo\r\nw\u{f6}rld\n".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("crlf.txt", b"one\r\ntwo\r\n".to_vec()),
        ("utf16.txt", utf16),
        ("eol.txt", b"x\r\ny\r\n".to_vec()),
        ("mixed.txt", b"a\nb\r\nc".to_vec()),
        ("binary.dat", vec![0, 1, 2, b'\r', b'\n', 0, 255, 0]),
    ];
    for (name, bytes) in &files {
        write(&project.join(name), bytes);
    }

    // Plain git would convert these on the way in.
    tgit(&project, &["add", "-A"]);
    let converted = tgit(&project, &["show", ":crlf.txt"]).stdout;
    assert_ne!(converted, files[0].1, "the control should convert line endings");

    let shadow = scratch.shadow(&project);
    let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    for (name, bytes) in &files {
        std::fs::remove_file(project.join(name)).unwrap();
        let restored = shadow.read_blob(&s.commit, Path::new(name)).unwrap().unwrap();
        assert_eq!(&restored, bytes, "{name}");
    }
}

#[test]
fn snapshot_commits_have_no_parent() {
    let _env = environment();
    let scratch = Scratch::new("no-parent");
    let project = scratch.project();
    write(&project.join("f"), b"1\n");
    let shadow = scratch.shadow(&project);
    let first = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    write(&project.join("f"), b"2\n");
    let second = snap(&shadow, SESSION, 1, SnapshotKind::End);
    for s in [&first, &second] {
        let body = text(&sgit(&shadow, &["cat-file", "-p", &s.commit]));
        assert!(!body.lines().any(|l| l.starts_with("parent ")), "{body}");
        assert!(body.contains("eitri <eitri@localhost>"), "{body}");
        let message = text(&sgit(&shadow, &["log", "-1", "--format=%B", &s.commit]));
        let parsed = SnapshotLabel::parse(&message).unwrap();
        assert_eq!(parsed.turn, 1);
    }
    assert_eq!(first.reference, format!("refs/eitri/{SESSION}/1/base"));
    assert_eq!(text(&sgit(&shadow, &["rev-parse", &second.reference])), second.commit);
}

#[test]
fn trim_frees_objects() {
    let _env = environment();
    let scratch = Scratch::new("trim");
    let project = scratch.project();
    let shadow = scratch.shadow(&project);
    let mut blobs = Vec::new();
    for turn in 1..=105u32 {
        write(&project.join("f"), format!("content of turn {turn}\n").as_bytes());
        let s = taken(shadow.snapshot(
            SESSION,
            &label(turn, SnapshotKind::End, u64::from(turn)),
            &Limits::default(),
        ));
        blobs.push((
            s.reference.clone(),
            text(&sgit(&shadow, &["rev-parse", &format!("{}:f", s.commit)])),
        ));
    }
    shadow.trim(100, Duration::from_secs(3600), 200, "now").unwrap();
    for (i, (reference, blob)) in blobs.iter().enumerate() {
        let present = tgit_status(
            shadow.review_dir(),
            &[
                &format!("--git-dir={}", shadow.git_dir().display()),
                "rev-parse",
                "--verify",
                "-q",
                reference,
            ],
        );
        if i < 5 {
            assert!(!present, "{reference} should be trimmed");
            assert!(!shadow_has_object(&shadow, blob), "the blob of {reference} survived gc");
        } else {
            assert!(present, "{reference} should be kept");
            assert!(shadow_has_object(&shadow, blob));
        }
    }

    // By age: at time 1_000 with a 950 ms window, only turns 50.. are young enough.
    shadow.trim(1_000, Duration::from_millis(950), 1_000, "now").unwrap();
    let refs = shadow.snapshot_refs().unwrap();
    assert_eq!(refs.len(), 56);
    assert!(refs.iter().all(|(_, l)| l.as_ref().unwrap().time_ms >= 50));
}

#[test]
fn trim_drops_the_index_of_a_session_with_nothing_left() {
    let _env = environment();
    let scratch = Scratch::new("trim-index");
    let project = scratch.project();
    write(&project.join("f"), b"old\n");
    let shadow = scratch.shadow(&project);
    let old = "old-session";
    taken(shadow.snapshot(old, &label(1, SnapshotKind::End, 1), &Limits::default()));
    write(&project.join("f"), b"new\n");
    taken(shadow.snapshot(SESSION, &label(1, SnapshotKind::End, 2), &Limits::default()));
    assert!(shadow.index_path(old).unwrap().exists());
    shadow.trim(1, Duration::from_secs(3600), 3, "now").unwrap();
    assert!(!shadow.index_path(old).unwrap().exists());
    assert!(shadow.index_path(SESSION).unwrap().exists());
    // The old session can still snapshot afterwards, from scratch.
    let again = taken(shadow.snapshot(old, &label(2, SnapshotKind::Base, 4), &Limits::default()));
    assert_eq!(
        shadow.read_blob(&again.commit, Path::new("f")).unwrap().as_deref(),
        Some(&b"new\n"[..])
    );
}

#[test]
fn trim_keeps_a_running_turn_until_it_ages_out() {
    let _env = environment();
    let scratch = Scratch::new("trim-running");
    let project = scratch.project();
    write(&project.join("f"), b"running\n");
    let shadow = scratch.shadow(&project);
    let base = taken(shadow.snapshot("running", &label(1, SnapshotKind::Base, 1), &Limits::default()));
    for n in 1..=3 {
        write(&project.join("f"), format!("finished {n}\n").as_bytes());
        for kind in [SnapshotKind::Base, SnapshotKind::End] {
            taken(shadow.snapshot(SESSION, &label(n, kind, 10 + u64::from(n)), &Limits::default()));
        }
    }
    shadow.trim(2, Duration::from_secs(3600), 20, "now").unwrap();
    assert!(shadow
        .snapshot_refs()
        .unwrap()
        .iter()
        .any(|(name, _)| name == &base.reference));
    assert!(shadow_has_object(&shadow, &base.commit));
    shadow.trim(2, Duration::from_millis(5), 20, "now").unwrap();
    assert!(!shadow
        .snapshot_refs()
        .unwrap()
        .iter()
        .any(|(name, _)| name == &base.reference));
}

#[test]
fn trim_counts_a_base_only_turn_when_its_session_has_a_later_turn() {
    let _env = environment();
    let scratch = Scratch::new("trim-unfinished");
    let project = scratch.project();
    write(&project.join("f"), b"content\n");
    let shadow = scratch.shadow(&project);
    let old = taken(shadow.snapshot("a", &label(1, SnapshotKind::Base, 1), &Limits::default()));
    for kind in [SnapshotKind::Base, SnapshotKind::End] {
        taken(shadow.snapshot("a", &label(2, kind, 2), &Limits::default()));
    }
    let running = taken(shadow.snapshot("running", &label(1, SnapshotKind::Base, 3), &Limits::default()));
    for turn in 1..=3 {
        taken(shadow.snapshot(
            "b",
            &label(turn, SnapshotKind::End, 10 + u64::from(turn)),
            &Limits::default(),
        ));
    }
    shadow.trim(2, Duration::from_secs(3600), 20, "now").unwrap();
    let refs = shadow.snapshot_refs().unwrap();
    assert!(!refs.iter().any(|(name, _)| name == &old.reference));
    assert!(refs.iter().any(|(name, _)| name == &running.reference));
    assert_eq!(refs.len(), 3);
}

#[test]
fn the_file_limit_leaves_out_deleted_paths_in_the_session_index() {
    let _env = environment();
    let scratch = Scratch::new("file-count-deleted");
    let project = scratch.project();
    let shadow = scratch.shadow(&project);
    let limits = Limits {
        max_files: 3,
        ..Limits::default()
    };
    for name in ["a", "b", "c"] {
        write(&project.join(name), b"content\n");
    }
    taken(shadow.snapshot(SESSION, &label(1, SnapshotKind::Base, 1), &limits));
    std::fs::remove_file(project.join("b")).unwrap();
    write(&project.join("d"), b"new\n");
    let end = taken(shadow.snapshot(SESSION, &label(1, SnapshotKind::End, 2), &limits));
    assert_eq!(
        tree_entries(&shadow, &end.commit).keys().cloned().collect::<Vec<_>>(),
        ["a", "c", "d"]
    );
}

#[test]
fn the_file_limit_counts_unchanged_files_in_the_session_index() {
    let _env = environment();
    let scratch = Scratch::new("file-count");
    let project = scratch.project();
    let shadow = scratch.shadow(&project);
    let limits = Limits {
        max_files: 3,
        ..Limits::default()
    };
    for n in 0..3 {
        write(&project.join(format!("f{n}")), b"x\n");
    }
    taken(shadow.snapshot(SESSION, &label(1, SnapshotKind::Base, 1), &limits));
    write(&project.join("fourth"), b"x\n");
    match shadow.snapshot(SESSION, &label(1, SnapshotKind::End, 2), &limits) {
        SnapshotOutcome::Unavailable(why) => assert!(why.contains("too many files"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn trim_waits_for_a_running_snapshot() {
    let _env = environment();
    let scratch = Scratch::new("trim-waits");
    let project = scratch.project();
    write(&project.join("f"), b"x\n");
    let shadow = scratch.shadow(&project);
    snap(&shadow, SESSION, 1, SnapshotKind::Base);

    let running = shadow.lock_shared().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let trimmer = shadow.clone();
    let (waiting, contended) = mpsc::channel();
    let lock_path = scratch.review_dir(&project).join("lock");
    let worker = std::thread::spawn(move || {
        eitri_core::turn_review::shadow::observe_lock_wait_for_tests(
            move || waiting.send(()).unwrap(),
            move || {
                use std::os::fd::AsRawFd;
                let probe = std::fs::File::open(lock_path).unwrap();
                // An independent reader must conflict with the exclusive lock just acquired.
                // SAFETY: the file owns its descriptor for the duration of the nonblocking call.
                let result = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
                let error = std::io::Error::last_os_error();
                assert_eq!(result, -1, "trim returned from flock without exclusive ownership");
                assert_eq!(error.raw_os_error(), Some(libc::EWOULDBLOCK));
            },
        );
        let result = trimmer.trim(0, Duration::from_millis(500), 2_000, "now");
        done_tx.send(()).unwrap();
        result
    });
    contended
        .recv_timeout(Duration::from_secs(60))
        .expect("trim never reached the held store lock");
    // Other readers still get the lock while trim waits for it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    drop(shadow.lock_shared_until(deadline).unwrap());
    drop(running);
    done_rx.recv_timeout(Duration::from_secs(30)).expect("trim never ran");
    worker.join().unwrap().unwrap();
    assert!(shadow.snapshot_refs().unwrap().is_empty());
}

/// Holds a real `git gc` at its very start. A FIFO stands in for the git directory's `gc.pid`,
/// which gc opens for reading (to see whether another gc is running) before it does any work, and
/// which nothing else reads. The open blocks until a writer comes, so [`entered`](Self::entered)
/// knows gc is there; the read that follows blocks until the writer closes, so gc stays there until
/// [`release`](Self::release).
struct GcGate {
    path: PathBuf,
    writer: Option<std::fs::File>,
}

impl GcGate {
    fn new(git_dir: &Path) -> GcGate {
        use std::os::unix::ffi::OsStrExt;
        let path = git_dir.join("gc.pid");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            made,
            0,
            "mkfifo {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        GcGate { path, writer: None }
    }

    /// Waits until a gc has the FIFO open, and keeps it held there. A write-only open that does not
    /// block fails with `ENXIO` for as long as no reader has the FIFO open.
    fn entered(&mut self, what: &str) {
        use std::os::unix::fs::OpenOptionsExt;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path)
            {
                Ok(writer) => {
                    self.writer = Some(writer);
                    return;
                }
                Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {}
                Err(e) => panic!("the gc gate {}: {e}", self.path.display()),
            }
            assert!(std::time::Instant::now() < deadline, "{what} never reached git gc");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Lets the held gc go on: it reads an empty pid file, so no other gc is running.
    fn release(&mut self) {
        self.writer = None;
    }
}

impl Drop for GcGate {
    /// Lets a gc go on whatever became of the test, since git runs in a process group of its own and
    /// would otherwise outlive the test binary, blocked. One held in its read sees the end of the
    /// file once the writer closes. One still blocked in its open (the test gave up waiting for it,
    /// or failed before it waited) is let through by an open for reading and writing, which never
    /// blocks on a FIFO and counts as the writer that open waits for. The FIFO is then removed, so a
    /// gc that comes later finds no pid file instead of blocking with nobody left to let it go.
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        self.writer = None;
        let waker = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.path);
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.file_type().is_fifo()) {
            let _ = std::fs::remove_file(&self.path);
        }
        drop(waker);
    }
}

#[test]
fn a_snapshot_never_waits_for_a_running_collection() {
    let _env = environment();
    let scratch = Scratch::new("gc-concurrent");
    let project = scratch.project();
    let shadow = scratch.shadow(&project);
    for turn in 1..=3u32 {
        write(&project.join("f"), format!("content of turn {turn}\n").as_bytes());
        taken(shadow.snapshot(
            SESSION,
            &label(turn, SnapshotKind::End, u64::from(turn)),
            &Limits::default(),
        ));
    }

    let mut gate = GcGate::new(shadow.git_dir());
    let (done_tx, done_rx) = mpsc::channel();
    let trimmer = shadow.clone();
    let collection = std::thread::spawn(move || {
        // Only turn 3 is kept. `now` lets the collection remove every object no ref reaches, however
        // new, so nothing but the order of its steps keeps a snapshot taken meanwhile whole.
        let result = trimmer.trim(1, Duration::from_secs(3600), 10, "now");
        // The test may have failed and gone already.
        let _ = done_tx.send(());
        result
    });
    gate.entered("the trim");

    // Turn 1's content again: its objects lost their last ref a moment ago, and the snapshot that
    // brings them back must not lose them to the collection running beside it.
    write(&project.join("f"), b"content of turn 1\n");
    let limits = Limits {
        timeout: Duration::from_secs(20),
        ..Limits::default()
    };
    let during = shadow.snapshot(SESSION, &label(4, SnapshotKind::End, 4), &limits);
    assert!(
        done_rx.try_recv().is_err(),
        "the collection finished while the gate still held it"
    );
    let during = match during {
        SnapshotOutcome::Taken(snapshot) => snapshot,
        SnapshotOutcome::Unavailable(why) => panic!("the snapshot waited for the running gc: {why}"),
    };
    gate.release();
    collection.join().unwrap().unwrap();

    assert_eq!(
        shadow.read_blob(&during.commit, Path::new("f")).unwrap().as_deref(),
        Some(&b"content of turn 1\n"[..])
    );
    sgit(&shadow, &["fsck", "--connectivity-only", "--no-dangling"]);
    let names: Vec<String> = shadow
        .snapshot_refs()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        vec![
            format!("refs/eitri/{SESSION}/3/end"),
            format!("refs/eitri/{SESSION}/4/end"),
        ]
    );
}

/// What retention's step between its two deleting steps relies on git for: `repack -A -d` (what
/// its `gc --no-prune` runs) writes an unreachable object of an old pack out loose with that pack's
/// time, not the time of the repack, and the prune that follows removes it by that time. So a blob
/// whose old pack was only touched after the repack started comes out looking old, and only a ref
/// made before the prune keeps it.
#[test]
fn a_repack_writes_an_unreachable_packed_blob_out_loose_with_its_packs_time() {
    use std::os::unix::fs::MetadataExt;

    let _env = environment();
    let scratch = Scratch::new("loosened-old");
    let project = scratch.project();
    let shadow = scratch.shadow(&project);
    let old = shadow.store_blob(b"in an old pack, reached by nothing\n").unwrap();
    let recent = shadow.store_blob(b"loose, recent, reached by nothing\n").unwrap();
    sgit(&shadow, &["update-ref", "refs/test/old", &old]);
    sgit(&shadow, &["repack", "-a", "-d"]);
    sgit(&shadow, &["prune-packed"]);
    sgit(&shadow, &["update-ref", "-d", "refs/test/old"]);
    let objects = shadow.git_dir().join("objects");
    let loose = |id: &str| objects.join(&id[..2]).join(&id[2..]);
    assert!(!loose(&old).exists(), "the blob is in a pack only");
    assert!(
        loose(&recent).exists(),
        "an unreachable loose blob is left loose by a repack"
    );

    let pack_dir = objects.join("pack");
    backdate(&pack_dir, Duration::from_secs(2 * 3600));
    let pack_time = std::fs::read_dir(&pack_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension() == Some(OsStr::new("pack")))
        .map(|p| std::fs::metadata(p).unwrap().mtime())
        .expect("one pack");
    sgit(&shadow, &["repack", "-A", "-d"]);
    assert_eq!(
        std::fs::metadata(loose(&old)).map(|m| m.mtime()).ok(),
        Some(pack_time),
        "the repack wrote the unreachable blob out loose with its pack's time"
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    shadow
        .trim(100, Duration::from_secs(30 * 24 * 3600), now_ms, "1.hour.ago")
        .unwrap();
    assert!(
        !shadow_has_object(&shadow, &old),
        "the prune removed the blob written out with its pack's old time"
    );
    assert!(
        shadow_has_object(&shadow, &recent),
        "the prune keeps an unreachable blob newer than its expiry"
    );
}

#[test]
fn a_file_ignored_later_stops_being_copied() {
    let _env = environment();
    let scratch = Scratch::new("ignored-later");
    let project = scratch.project();
    tgit(&project, &["init", "-q"]);
    write(&project.join(".env"), b"TOKEN=one\n");
    write(&project.join("kept"), b"k\n");
    let shadow = scratch.shadow(&project);
    let first = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    assert!(tree_entries(&shadow, &first.commit).contains_key(".env"));

    write(&project.join(".gitignore"), b".env\n");
    let changed = b"TOKEN=two\n";
    write(&project.join(".env"), changed);
    let second = snap(&shadow, SESSION, 1, SnapshotKind::End);
    let entries = tree_entries(&shadow, &second.commit);
    assert!(!entries.contains_key(".env"), "{entries:?}");
    assert!(entries.contains_key("kept"));

    let mut hash = tgit_cmd(&project);
    hash.args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut child = hash.spawn().unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), changed).unwrap();
    let id = text(&child.wait_with_output().unwrap());
    assert!(
        !shadow_has_object(&shadow, &id),
        "the ignored file's new content was copied"
    );
}

#[test]
fn limits_fail_visibly() {
    let _env = environment();
    let scratch = Scratch::new("limits");
    let project = scratch.project();
    write(&project.join("small"), b"s\n");
    write(&project.join("grows"), b"g\n");
    let shadow = scratch.shadow(&project);
    let first = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    assert!(tree_entries(&shadow, &first.commit).contains_key("grows"));

    let big = vec![b'x'; 9 * 1024 * 1024];
    write(&project.join("big.bin"), &big);
    write(&project.join("grows"), &big);
    let second = snap(&shadow, SESSION, 1, SnapshotKind::End);
    let mut skipped = second.skipped_large.clone();
    skipped.sort();
    assert_eq!(skipped, vec![PathBuf::from("big.bin"), PathBuf::from("grows")]);
    let entries = tree_entries(&shadow, &second.commit);
    assert!(
        !entries.contains_key("big.bin") && !entries.contains_key("grows"),
        "{entries:?}"
    );
    assert!(entries.contains_key("small"));

    let few = scratch.path("few");
    std::fs::create_dir_all(&few).unwrap();
    for i in 0..4 {
        write(&few.join(format!("f{i}")), b"x");
    }
    let few_shadow = scratch.shadow(&few);
    let limits = Limits {
        max_files: 3,
        ..Limits::default()
    };
    match few_shadow.snapshot(SESSION, &label(1, SnapshotKind::Base, 1), &limits) {
        SnapshotOutcome::Unavailable(why) => assert!(why.contains("too many files"), "{why}"),
        other => panic!("{other:?}"),
    }
    let limits = Limits {
        timeout: Duration::ZERO,
        ..Limits::default()
    };
    match few_shadow.snapshot(SESSION, &label(1, SnapshotKind::Base, 1), &limits) {
        SnapshotOutcome::Unavailable(why) => assert!(why.contains("too long"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(
        few_shadow.snapshot_refs().unwrap().is_empty(),
        "a failed snapshot left a ref"
    );
}

/// The project's `info/exclude` is the project's to shape: a FIFO in its place fails the snapshot at
/// once instead of holding it (and the store's lock) for good, and a link to an endless file fails it
/// instead of reading until memory runs out.
#[test]
fn a_project_exclude_that_is_no_regular_file_fails_the_snapshot_at_once() {
    let _env = environment();
    for (name, make) in [("fifo", "fifo"), ("dev-zero", "link")] {
        let scratch = Scratch::new(&format!("exclude-{name}"));
        let project = scratch.project();
        tgit(&project, &["init", "-q"]);
        write(&project.join("a.txt"), b"one\n");
        let exclude = project.join(".git/info/exclude");
        let _ = std::fs::remove_file(&exclude);
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        if make == "fifo" {
            let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(exclude.as_os_str())).unwrap();
            // SAFETY: a valid NUL-terminated path for the call.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        } else {
            std::os::unix::fs::symlink("/dev/zero", &exclude).unwrap();
        }
        let shadow = scratch.shadow(&project);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = shadow.snapshot(SESSION, &label(1, SnapshotKind::Base, 1), &Limits::default());
            let _ = tx.send(matches!(outcome, SnapshotOutcome::Unavailable(_)));
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(30)),
            Ok(true),
            "{name}: the snapshot must fail, and at once"
        );
    }
}

#[test]
fn a_non_git_project_and_a_linked_worktree() {
    let _env = environment();
    let scratch = Scratch::new("worktree");
    let plain = scratch.path("plain");
    write(&plain.join("dir/file"), b"plain\n");
    let shadow = scratch.shadow(&plain);
    let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    assert_eq!(
        shadow.read_blob(&s.commit, Path::new("dir/file")).unwrap().as_deref(),
        Some(&b"plain\n"[..])
    );
    assert!(!shadow.git_dir().join("info/exclude").exists());

    let main = scratch.path("main");
    std::fs::create_dir_all(&main).unwrap();
    tgit(&main, &["init", "-q"]);
    write(&main.join("tracked"), b"t\n");
    tgit(&main, &["add", "tracked"]);
    tgit(&main, &["commit", "-qm", "c"]);
    write(&main.join(".git/info/exclude"), b"secret.txt\n");
    let linked = scratch.path("linked");
    tgit(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    assert!(linked.join(".git").is_file());
    write(&linked.join("secret.txt"), b"s\n");
    write(&linked.join("other.txt"), b"o\n");
    let shadow = scratch.shadow(&linked);
    let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    let entries = tree_entries(&shadow, &s.commit);
    assert!(
        entries.contains_key("other.txt") && entries.contains_key("tracked"),
        "{entries:?}"
    );
    assert!(
        !entries.contains_key("secret.txt"),
        "the common dir's info/exclude was not honoured"
    );
    assert!(!entries.contains_key(".git"));
}

/// A project that is a directory below a repository's top level, as a project inside a monorepo
/// is: the snapshot is taken of the project alone, the repository above it is never written, and
/// that repository's `info/exclude` is not taken (it is copied only for a project that is the top
/// of its work tree). The scratch directories are not inside any repository, so without this the
/// case would go untested.
#[test]
fn a_project_below_a_repositorys_top_level() {
    let _env = environment();
    let scratch = Scratch::new("below-top");
    let repo = scratch.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    tgit(&repo, &["init", "-q"]);
    write(&repo.join("top.txt"), b"top\n");
    write(&repo.join("sub/a.txt"), b"one\n");
    write(&repo.join("sub/.gitignore"), b"*.tmp\n");
    tgit(&repo, &["add", "top.txt", "sub/a.txt", "sub/.gitignore"]);
    tgit(&repo, &["commit", "-qm", "first"]);
    write(&repo.join(".git/info/exclude"), b"secret\n");
    let project = repo.join("sub");
    write(&project.join("a.txt"), b"dirty\n");
    write(&project.join("y.tmp"), b"ignored here\n");
    write(&project.join("new.txt"), b"new\n");

    let observe = || {
        (
            tree_bytes(&repo.join(".git")),
            text(&tgit(&repo, &["for-each-ref"])),
            text(&tgit(&repo, &["count-objects", "-v"])),
        )
    };
    let before = observe();
    let shadow = scratch.shadow(&project);
    let base = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    let end = snap(&shadow, SESSION, 1, SnapshotKind::End);
    let entries = tree_entries(&shadow, &end.commit);
    assert!(
        entries.contains_key("a.txt") && entries.contains_key("new.txt") && entries.contains_key(".gitignore"),
        "{entries:?}"
    );
    assert!(
        !entries.contains_key("top.txt"),
        "only the project is snapshotted: {entries:?}"
    );
    assert!(
        !entries.contains_key("sub/a.txt"),
        "paths are the project's own: {entries:?}"
    );
    assert!(
        !entries.contains_key("y.tmp"),
        "the project's own .gitignore is honoured: {entries:?}"
    );
    assert_eq!(
        shadow.read_blob(&base.commit, Path::new("a.txt")).unwrap().as_deref(),
        Some(&b"dirty\n"[..])
    );
    assert!(
        !shadow.git_dir().join("info/exclude").exists(),
        "a project below the top does not take the repository's info/exclude"
    );
    let after = observe();
    let changed: Vec<_> = before
        .0
        .keys()
        .chain(after.0.keys())
        .filter(|path| before.0.get(*path) != after.0.get(*path))
        .collect();
    assert!(changed.is_empty(), "the repository's .git changed: {changed:?}");
    assert_eq!(before.1, after.1, "refs");
    assert_eq!(before.2, after.2, "object count");
}

#[test]
fn a_nested_repository_is_one_entry() {
    let _env = environment();
    let scratch = Scratch::new("nested");
    let project = scratch.project();
    write(&project.join("top"), b"t\n");
    let nested = project.join("vendor/lib");
    write(&nested.join("inner.rs"), b"inner\n");
    tgit(&nested, &["init", "-q"]);
    tgit(&nested, &["add", "inner.rs"]);
    tgit(&nested, &["commit", "-qm", "c"]);
    let unborn = project.join("scratchpad");
    write(&unborn.join("note"), b"n\n");
    tgit(&unborn, &["init", "-q"]);

    let shadow = scratch.shadow(&project);
    let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    let entries = tree_entries(&shadow, &s.commit);
    assert_eq!(
        entries.get("vendor/lib").map(String::as_str),
        Some("160000"),
        "{entries:?}"
    );
    assert!(
        !entries
            .keys()
            .any(|p| p.starts_with("vendor/lib/") || p.starts_with("scratchpad")),
        "{entries:?}"
    );
    assert!(entries.contains_key("top"));
}

#[test]
fn the_global_excludes_file_is_honoured() {
    let _env = environment();
    let scratch = Scratch::new("global-excludes");
    let project = scratch.project();
    write(&project.join("keep.rs"), b"k\n");
    write(&project.join("drop.swp"), b"d\n");
    let excludes = scratch.path("global-ignore");
    write(&excludes, b"*.swp\n");
    let shadow = Shadow::open_with_excludes(&scratch.review_dir(&project), &project, Some(excludes)).unwrap();
    let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    let entries = tree_entries(&shadow, &s.commit);
    assert!(
        entries.contains_key("keep.rs") && !entries.contains_key("drop.swp"),
        "{entries:?}"
    );

    let home = scratch.path("home");
    write(&home.join(".gitconfig"), b"[core]\n\texcludesFile = ~/my-ignore\n");
    assert_eq!(
        git::resolve_excludes_file(Some(home.as_os_str()), None),
        Some(home.join("my-ignore"))
    );
    let bare_home = scratch.path("bare-home");
    std::fs::create_dir_all(&bare_home).unwrap();
    assert_eq!(
        git::resolve_excludes_file(Some(bare_home.as_os_str()), None),
        Some(bare_home.join(".config/git/ignore"))
    );
    let xdg = scratch.path("xdg");
    assert_eq!(
        git::resolve_excludes_file(Some(bare_home.as_os_str()), Some(xdg.as_os_str())),
        Some(xdg.join("git/ignore"))
    );
}

#[test]
fn the_store_is_private_and_outside_the_project() {
    let _env = environment();
    let scratch = Scratch::new("private");
    let project = scratch.project();
    write(&project.join("f"), b"x\n");
    let shadow = scratch.shadow(&project);
    snap(&shadow, SESSION, 1, SnapshotKind::Base);
    shadow.trim(100, Duration::from_secs(3600), 2_000, "now").unwrap();
    assert!(shadow.review_dir().starts_with(scratch.path("state/eitri/review")));
    let key = shadow.review_dir().file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(key.len(), 16);
    assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));

    let mut stack = vec![shadow.review_dir().to_path_buf()];
    let mut seen = 0;
    while let Some(dir) = stack.pop() {
        let mode = std::fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                seen += 1;
                assert_eq!(
                    meta.permissions().mode() & 0o077,
                    0,
                    "{} is open to others",
                    path.display()
                );
            }
        }
    }
    assert!(seen > 5);
    assert_eq!(
        std::fs::read(shadow.git_dir().join("info/attributes")).unwrap(),
        b"* -text -filter -ident -working-tree-encoding\n"
    );
}

#[test]
fn bad_inputs_are_refused() {
    let _env = environment();
    let scratch = Scratch::new("bad-inputs");
    let project = scratch.project();
    write(&project.join("f"), b"x\n");
    let shadow = scratch.shadow(&project);
    let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    match shadow.snapshot("../escape", &label(1, SnapshotKind::Base, 1), &Limits::default()) {
        SnapshotOutcome::Unavailable(_) => {}
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        shadow.read_blob("--output=x", Path::new("f")),
        Err(ShadowError::NoSuchCommit(_))
    ));
    assert!(matches!(
        shadow.read_blob(&"0".repeat(40), Path::new("f")),
        Err(ShadowError::NoSuchCommit(_))
    ));
    assert!(matches!(
        shadow.read_blob(&s.commit, Path::new("/etc/passwd")),
        Err(ShadowError::InvalidPath(_))
    ));
    assert_eq!(shadow.read_blob(&s.commit, Path::new("missing")).unwrap(), None);
}

#[test]
fn a_timed_out_git_is_stopped_with_its_children() {
    let _env = environment();
    let scratch = Scratch::new("timeout");
    let exec = scratch.path("exec");
    let pid_file = scratch.path("grandchild.pid");
    let helper = exec.join("git-eitri-test-hang");
    std::fs::create_dir_all(&exec).unwrap();
    script_fixture::write_executable_script(
        &helper,
        format!("#!/bin/sh\nsleep 60 &\necho $! > '{}'\nwait\n", pid_file.display()).as_bytes(),
        0o755,
    )
    .unwrap();
    let mut cmd = git::user_read_only(&scratch.0);
    cmd.arg(format!("--exec-path={}", exec.display()))
        .arg("eitri-test-hang");
    let base = std::time::Instant::now();
    let clock_reads = std::cell::Cell::new(0);
    let timeout = Duration::from_secs(1);
    let result = git::run_with_clock_for_tests(cmd, timeout, &|| {
        let reads = clock_reads.get();
        assert!(reads < 10000, "the git deadline loop did not finish");
        clock_reads.set(reads + 1);
        if reads == 0 {
            return base;
        }
        let guard = std::time::Instant::now() + Duration::from_secs(60);
        while !std::fs::read_to_string(&pid_file).is_ok_and(|text| text.trim().parse::<u32>().is_ok()) {
            assert!(std::time::Instant::now() < guard, "git never started its held helper");
            std::thread::yield_now();
        }
        base + timeout + Duration::from_millis(reads)
    });
    assert!(matches!(result, Err(git::GitError::TimedOut)), "{result:?}");
    let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_owned();
    #[cfg(not(target_os = "macos"))]
    let gone = {
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        (0..200).any(|_| {
            let alive = std::fs::read_to_string(proc_dir.join("stat"))
                .map(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
                .unwrap_or(false);
            if alive {
                std::thread::sleep(Duration::from_millis(10));
            }
            !alive
        })
    };
    // macOS has no /proc: `ps` prints nothing (and fails) for a pid that no longer exists, and a
    // zombie, killed and not yet reaped, counts as gone like on Linux.
    #[cfg(target_os = "macos")]
    let gone = (0..200).any(|_| {
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid])
            .output()
            .expect("ps runs");
        let stat = String::from_utf8_lossy(&out.stdout);
        let alive = !stat.trim().is_empty() && !stat.trim().starts_with('Z');
        if alive {
            std::thread::sleep(Duration::from_millis(10));
        }
        !alive
    });
    assert!(gone, "the timed-out git's child {pid} is still running");
}

/// A snapshot killed for its time can leave git's lock files behind (the session's index lock, a
/// ref's lock); the next snapshot of that session must still be taken. Each round uses a fresh
/// session, so its `add` hashes every file and a short timeout is likely to land inside it.
#[test]
fn a_killed_snapshot_does_not_block_the_next() {
    let _env = environment();
    let scratch = Scratch::new("killed");
    let project = scratch.project();
    let body = vec![b'k'; 32 * 1024];
    for i in 0..3000 {
        let mut bytes = body.clone();
        bytes.extend_from_slice(format!("{i}\n").as_bytes());
        write(&project.join(format!("d{}/f{i}", i % 30)), &bytes);
    }
    let shadow = scratch.shadow(&project);
    let mut left_a_lock = 0;
    let mut timed_out = 0;
    for (round, ms) in [2u64, 5, 10, 20, 40, 80, 160, 320, 640].into_iter().enumerate() {
        let session = format!("killed-{round}");
        let limits = Limits {
            timeout: Duration::from_millis(ms),
            ..Limits::default()
        };
        if let SnapshotOutcome::Unavailable(_) = shadow.snapshot(&session, &label(1, SnapshotKind::Base, 1), &limits) {
            timed_out += 1;
            let mut index_lock = shadow.index_path(&session).unwrap().into_os_string();
            index_lock.push(".lock");
            if Path::new(&index_lock).exists() {
                left_a_lock += 1;
            }
        }
        let next = taken(shadow.snapshot(&session, &label(1, SnapshotKind::Base, 2), &Limits::default()));
        assert!(tree_entries(&shadow, &next.commit).contains_key("d0/f0"));
    }
    assert!(timed_out > 0, "no round timed out; the test proves nothing");
    assert!(
        left_a_lock > 0,
        "no killed round left an index lock; the test proves nothing"
    );
    shadow.trim(100, Duration::from_secs(3600), 2_000, "now").unwrap();
}

/// Every lock file a killed git can leave in the store, put there by hand: none of them may stop a
/// snapshot, a collection or opening the store.
#[test]
fn stale_lock_files_do_not_wedge_the_store() {
    let _env = environment();
    let scratch = Scratch::new("stale-locks");
    let project = scratch.project();
    write(&project.join("f"), b"x\n");
    let shadow = scratch.shadow(&project);
    snap(&shadow, SESSION, 1, SnapshotKind::Base);
    let git_dir = shadow.git_dir().to_path_buf();
    let mut index_lock = shadow.index_path(SESSION).unwrap().into_os_string();
    index_lock.push(".lock");
    let stale = [
        PathBuf::from(index_lock),
        git_dir.join(format!("refs/eitri/{SESSION}/1/end.lock")),
        git_dir.join(format!("refs/eitri/{SESSION}/1/base.lock")),
        git_dir.join("packed-refs.lock"),
    ];
    for lock in &stale {
        write(lock, b"");
    }
    write(&project.join("f"), b"y\n");
    let end = snap(&shadow, SESSION, 1, SnapshotKind::End);
    assert_eq!(
        shadow.read_blob(&end.commit, Path::new("f")).unwrap().as_deref(),
        Some(&b"y\n"[..])
    );
    for lock in &stale[1..] {
        write(lock, b"");
    }
    shadow.trim(0, Duration::from_secs(3600), 2_000, "now").unwrap();
    assert!(shadow.snapshot_refs().unwrap().is_empty());
    for lock in &stale {
        assert!(!lock.exists(), "{} is still there", lock.display());
    }

    // A store whose setup was killed halfway (no attributes yet, a config lock left) opens.
    std::fs::remove_file(git_dir.join("info/attributes")).unwrap();
    write(&git_dir.join("config.lock"), b"");
    let reopened = scratch.shadow(&project);
    snap(&reopened, SESSION, 2, SnapshotKind::Base);
}

#[test]
fn a_project_path_with_a_newline_keeps_its_excludes() {
    let _env = environment();
    let scratch = Scratch::new("newline");
    let project = scratch.path("pro\nject");
    std::fs::create_dir_all(&project).unwrap();
    tgit(&project, &["init", "-q"]);
    write(&project.join(".git/info/exclude"), b"secret\n");
    write(&project.join("secret"), b"s\n");
    write(&project.join("other"), b"o\n");
    let shadow = scratch.shadow(&project);
    let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
    let entries = tree_entries(&shadow, &s.commit);
    assert!(entries.contains_key("other"), "{entries:?}");
    assert!(
        !entries.contains_key("secret"),
        "the project's info/exclude was not honoured"
    );
}

/// Eitri started from a git hook or a shell that exported `GIT_DIR` must still read the excludes
/// of the project it was opened on, not of the repository the variable names.
#[test]
fn an_inherited_git_dir_does_not_choose_the_excludes() {
    let _env = ENVIRONMENT.write().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("git-dir");
    let elsewhere = scratch.path("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    tgit(&elsewhere, &["init", "-q"]);
    write(&elsewhere.join(".git/info/exclude"), b"other\n");
    let project = scratch.project();
    tgit(&project, &["init", "-q"]);
    write(&project.join(".git/info/exclude"), b"secret\n");
    write(&project.join("secret"), b"s\n");
    write(&project.join("other"), b"o\n");

    let names = ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"];
    let saved: Vec<_> = names.iter().map(|k| (*k, std::env::var_os(k))).collect();
    std::env::set_var("GIT_DIR", elsewhere.join(".git"));
    std::env::set_var("GIT_COMMON_DIR", elsewhere.join(".git"));
    std::env::remove_var("GIT_WORK_TREE");
    let result = std::panic::catch_unwind(|| {
        let shadow = scratch.shadow(&project);
        let s = snap(&shadow, SESSION, 1, SnapshotKind::Base);
        tree_entries(&shadow, &s.commit)
    });
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    let entries = result.unwrap();
    assert!(entries.contains_key("other"), "{entries:?}");
    assert!(!entries.contains_key("secret"), "{entries:?}");
}

#[test]
fn an_older_builds_open_state_dirs_are_tightened() {
    let _env = environment();
    let scratch = Scratch::new("open-dirs");
    let project = scratch.project();
    let eitri = scratch.path("state/eitri");
    std::fs::create_dir_all(eitri.join("review")).unwrap();
    for dir in [&eitri, &eitri.join("review")] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let state = scratch.path("state");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    let shadow = scratch.shadow(&project);
    for dir in [&eitri, &eitri.join("review"), &shadow.review_dir().to_path_buf()] {
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
    // Nothing above Eitri's own directory is changed.
    assert_eq!(std::fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o755);
}

/// The only process this module starts is git, and only `git.rs` builds it; nothing goes through a
/// shell. Whitespace is removed before matching, so how a call is wrapped across lines does not
/// matter.
#[test]
fn only_git_rs_builds_git_commands() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/turn_review");
    let mut sources = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension() == Some(OsStr::new("rs")) {
                sources.push(path);
            }
        }
    }
    // `git::user_read_only` runs git with the user's own environment and config, so it is kept to
    // the two read-only questions it exists for: in git.rs its definition, the excludes-file
    // question and two unit tests; in shadow.rs the common-dir question. Any new caller must be
    // argued for here.
    let user_read_only_sites: BTreeMap<&str, usize> = [("git.rs", 4), ("shadow.rs", 1)].into_iter().collect();
    let mut scanned = 0;
    for path in sources {
        scanned += 1;
        let source: String = std::fs::read_to_string(&path)
            .unwrap()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let relative = path.strip_prefix(&dir).unwrap().to_str().unwrap().to_owned();
        let name = relative.as_str();
        assert_eq!(
            source.matches("user_read_only(").count(),
            user_read_only_sites.get(name).copied().unwrap_or(0),
            "{name}: git::user_read_only is called somewhere new"
        );
        for shell in [
            "Command::new(\"sh\"",
            "Command::new(\"/bin/sh\"",
            "Command::new(\"bash\"",
            "\"sh\",\"-c\"",
            "\"-c\",\"sh\"",
        ] {
            assert!(!source.contains(shell), "{name} runs a shell ({shell})");
        }
        if name != "git.rs" {
            assert!(
                !source.contains("Command::new("),
                "{name} starts a process; only git.rs may"
            );
            assert!(
                !source.contains("process::Command::new"),
                "{name} starts a process; only git.rs may"
            );
        }
    }
    assert!(scanned >= 3);
}

/// The variable naming the directory [`measure_a_large_collection`] builds its store in.
const MEASURE_DIR: &str = "EITRI_GC_MEASURE_DIR";

/// Removes a directory when dropped, a failed run's too.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A small fast generator, so the fixture is the same on every run without a crate for it.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn line(&mut self) -> String {
        format!("let value_{:016x} = compute({:016x});\n", self.next(), self.next())
    }
}

/// Sets every file under `dir` to have last changed `age` ago, as a store whose objects were written
/// that long before looks to `git prune`. git's object files are read-only to their owner, who may
/// still set their times.
fn backdate(dir: &Path, age: Duration) {
    let when = std::time::SystemTime::now() - age;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else if entry.file_type().unwrap().is_file() {
                let file = std::fs::File::open(entry.path()).unwrap();
                file.set_times(std::fs::FileTimes::new().set_modified(when).set_accessed(when))
                    .unwrap();
            }
        }
    }
}

/// One line of `git count-objects -v`, as `key=value` pairs.
fn object_counts(shadow: &Shadow) -> String {
    text(&sgit(shadow, &["count-objects", "-v"]))
        .lines()
        .map(|line| line.replace(": ", "="))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs `trim` on `shadow` while two probes watch: one takes the store lock shared again and again
/// and keeps the longest it had to wait (what any snapshot, read or write would wait), the other
/// takes snapshots of the project one after another and keeps how long each took. Prints what it saw.
fn measured_trim(shadow: &Shadow, what: &str, keep: usize, now_ms: u64) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    let limits = Limits {
        timeout: Duration::from_secs(1800),
        ..Limits::default()
    };
    // The probe's own session: one turn whose base it takes again and again (the ref is replaced
    // each time), warmed first so its snapshots during the trim hash nothing new. Dated after every
    // real turn, it is kept, and takes one of the `keep` places.
    let probe_label = SnapshotLabel {
        turn: 1,
        kind: SnapshotKind::Base,
        turn_id: "probe".into(),
        tab: 9,
        time_ms: u64::MAX / 4,
    };
    taken(shadow.snapshot("probe", &probe_label, &limits));
    let before = object_counts(shadow);

    let stop = Arc::new(AtomicBool::new(false));
    let lock_probe = {
        let (shadow, stop) = (shadow.clone(), stop.clone());
        std::thread::spawn(move || {
            let (mut longest, mut tries) = (Duration::ZERO, 0u32);
            while !stop.load(Ordering::SeqCst) {
                let at = Instant::now();
                let held = shadow.lock_shared().unwrap();
                longest = longest.max(at.elapsed());
                tries += 1;
                drop(held);
                std::thread::sleep(Duration::from_millis(1));
            }
            (longest, tries)
        })
    };
    let snapshot_probe = {
        let (shadow, stop, limits) = (shadow.clone(), stop.clone(), limits.clone());
        std::thread::spawn(move || {
            let mut took = Vec::new();
            let mut unavailable = 0u32;
            while !stop.load(Ordering::SeqCst) {
                let at = Instant::now();
                if let SnapshotOutcome::Unavailable(why) = shadow.snapshot("probe", &probe_label, &limits) {
                    eprintln!("measure: a probe snapshot was unavailable: {why}");
                    unavailable += 1;
                }
                took.push(at.elapsed());
            }
            (took, unavailable)
        })
    };

    let at = Instant::now();
    shadow
        .trim(keep, Duration::from_secs(30 * 24 * 3600), now_ms, "1.hour.ago")
        .unwrap();
    let trim_took = at.elapsed();
    stop.store(true, Ordering::SeqCst);
    let (longest_wait, tries) = lock_probe.join().unwrap();
    let (mut took, unavailable) = snapshot_probe.join().unwrap();
    took.sort();
    let after = object_counts(shadow);
    println!("measure: {what}: trim took {trim_took:?}");
    println!("measure: {what}: objects before: {before}");
    println!("measure: {what}: objects after:  {after}");
    println!("measure: {what}: longest wait for the store lock (shared), over {tries} takes: {longest_wait:?}");
    println!(
        "measure: {what}: {} snapshots taken during the trim ({unavailable} unavailable): fastest {:?}, median {:?}, slowest {:?}",
        took.len(),
        took.first().copied().unwrap_or_default(),
        took.get(took.len() / 2).copied().unwrap_or_default(),
        took.last().copied().unwrap_or_default(),
    );
}

/// How long a collection of a large store takes, and how long snapshots wait while it runs. A
/// measurement, not a check: it asserts no time. It does nothing unless `EITRI_GC_MEASURE_DIR` names
/// a directory to build its store in (about 2 GiB at its largest), which it removes again. Run it
/// alone (`--ignored --exact --nocapture`) and read the `measure:` lines.
///
/// The store: a project of 2,000 small text files in 40 directories and 20 binary files of 4 MiB.
/// Each of 150 turns takes a base snapshot, changes 3 lines in each of 40 small files and rewrites
/// 2 of the large ones with new bytes, and takes an end snapshot. Its objects are then dated two
/// hours back, and a trim keeping 100 turns runs: the 50 oldest turns' objects are what `git prune`
/// removes. Then 20 more turns, dated back the same way, and a second trim: the store as an hourly
/// trim finds it, already packed once.
#[test]
#[ignore = "a measurement: set EITRI_GC_MEASURE_DIR and run it alone"]
fn measure_a_large_collection() {
    use std::time::Instant;
    let Some(base) = std::env::var_os(MEASURE_DIR) else {
        return;
    };
    const SMALL_DIRS: usize = 40;
    const SMALL_PER_DIR: usize = 50;
    const SMALL_LINES: usize = 64;
    const LARGE: usize = 20;
    const LARGE_BYTES: usize = 4 << 20;
    const TURNS: u32 = 150;
    const MORE_TURNS: u32 = 20;
    const SMALL_CHANGED: usize = 40;
    const LARGE_CHANGED: usize = 2;
    const KEEP: usize = 100;

    let root = PathBuf::from(base).join(format!("eitri-gc-measure-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    let _remove = RemoveOnDrop(root.clone());
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let review_dir = review_dir_for(Some(root.join("state").as_os_str()), None, &project).unwrap();
    let shadow = Shadow::open_with_excludes(&review_dir, &project, None).unwrap();

    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    let mut small: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for d in 0..SMALL_DIRS {
        for f in 0..SMALL_PER_DIR {
            let lines: Vec<String> = (0..SMALL_LINES).map(|_| rng.line()).collect();
            let path = project.join(format!("src/m{d:02}/f{f:02}.rs"));
            write(&path, lines.concat().as_bytes());
            small.push((path, lines));
        }
    }
    for l in 0..LARGE {
        write(&project.join(format!("assets/blob{l:02}.bin")), &rng.bytes(LARGE_BYTES));
    }

    let limits = Limits {
        timeout: Duration::from_secs(600),
        ..Limits::default()
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut turns_taken = 0u32;
    let mut take_turns = |count: u32, rng: &mut XorShift| {
        let at = Instant::now();
        for _ in 0..count {
            turns_taken += 1;
            let turn = turns_taken;
            // A turn a second, the newest a moment ago, all well inside the age limit.
            let time_ms = now_ms - u64::from(TURNS + MORE_TURNS - turn) * 1_000;
            taken(shadow.snapshot(SESSION, &label(turn, SnapshotKind::Base, time_ms), &limits));
            for _ in 0..SMALL_CHANGED {
                let (path, lines) = &mut small[rng.below(SMALL_DIRS * SMALL_PER_DIR)];
                for _ in 0..3 {
                    let at = rng.below(SMALL_LINES);
                    lines[at] = rng.line();
                }
                write(path, lines.concat().as_bytes());
            }
            for _ in 0..LARGE_CHANGED {
                let l = rng.below(LARGE);
                write(&project.join(format!("assets/blob{l:02}.bin")), &rng.bytes(LARGE_BYTES));
            }
            taken(shadow.snapshot(SESSION, &label(turn, SnapshotKind::End, time_ms + 500), &limits));
        }
        at.elapsed()
    };

    let built = take_turns(TURNS, &mut rng);
    println!("measure: built {TURNS} turns (two snapshots each) in {built:?}");
    backdate(&shadow.git_dir().join("objects"), Duration::from_secs(2 * 3600));
    measured_trim(&shadow, "first trim (all loose)", KEEP, now_ms);

    let built = take_turns(MORE_TURNS, &mut rng);
    println!("measure: built {MORE_TURNS} more turns in {built:?}");
    backdate(&shadow.git_dir().join("objects"), Duration::from_secs(2 * 3600));
    measured_trim(&shadow, "second trim (packed once)", KEEP, now_ms + 60_000);
    sgit(&shadow, &["fsck", "--connectivity-only", "--no-dangling"]);
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
