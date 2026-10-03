//! The trust record, on real files in a temporary directory.
//!
//! Every file a test inspects is made here, under a scratch directory that holds a fake home, a
//! fake state directory and the projects; no test points discovery at an existing project or at
//! the real home. `git` runs only to make fixtures, with no user or system configuration.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use eitri_core::project_trust::{civil_date, discover, Diff, Discovery, Limits, Remember, TrustState, TrustStore};

struct Fixture {
    outer: PathBuf,
    home: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let outer = std::env::temp_dir().join(format!("eitri-trust-store-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outer).unwrap();
        let base = outer.canonicalize().unwrap();
        let home = base.join("home");
        let state = base.join("state");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&state).unwrap();
        Fixture {
            outer: base,
            home,
            state,
        }
    }

    fn dir(&self) -> PathBuf {
        self.state.join("eitri").join("trust")
    }

    fn store(&self) -> TrustStore {
        TrustStore::new(Some(self.state.as_os_str()), Some(self.home.as_os_str()))
    }

    fn discover(&self, root: &Path) -> Discovery {
        discover(root, Some(&self.home), &Limits::default())
    }

    fn root(&self, name: &str) -> PathBuf {
        let root = self.home.join(name);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn record_file(&self, root: &Path) -> PathBuf {
        self.dir()
            .join(format!("{}.json", &agent::conversation_id_for_cwd(root)[..16]))
    }

    fn git(&self, dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.root(name);
        self.git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("README.md"), b"readme\n").unwrap();
        self.git(&dir, &["add", "README.md"]);
        self.git(&dir, &["commit", "-q", "-m", "first"]);
        dir
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.outer);
    }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path alive for the call.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

fn hook(command: &str) -> Vec<u8> {
    format!(r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#).into_bytes()
}

const SETTINGS: &str = ".claude/settings.json";

/// A project with one hook, recorded as trusted.
fn trusted(fx: &Fixture, name: &str) -> PathBuf {
    let root = fx.root(name);
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    write(&root.join(".claude/agents/other.md"), b"an agent\n");
    let found = fx.discover(&root);
    assert!(found.fully_hashed());
    fx.store().record(&found, 1_790_000_000).unwrap();
    root
}

#[test]
fn record_then_state_is_trusted_and_quiet() {
    let fx = Fixture::new("quiet");
    let root = fx.root("p");
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    let store = fx.store();
    let found = fx.discover(&root);
    match store.state(found.clone()) {
        TrustState::Untrusted { remember, .. } => assert_eq!(remember, Remember::Yes),
        other => panic!("{other:?}"),
    }
    store.record(&found, 1_790_000_000).unwrap();
    match store.state(fx.discover(&root)) {
        TrustState::Trusted { since_unix, discovery } => {
            assert_eq!(since_unix, 1_790_000_000);
            assert_eq!(discovery.fingerprint, found.fingerprint);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_changed_fingerprint_asks_again_with_the_files_that_changed() {
    let fx = Fixture::new("changed");
    let root = trusted(&fx, "p");
    write(&root.join(SETTINGS), &hook("touch /tmp/b"));
    write(&root.join(".claude/agents/a.md"), b"new agent\n");
    match fx.store().state(fx.discover(&root)) {
        TrustState::Changed { diff, remember, .. } => {
            assert_eq!(diff.changed, vec![PathBuf::from(SETTINGS)]);
            assert_eq!(diff.added, vec![PathBuf::from(".claude/agents/a.md")]);
            assert!(diff.removed.is_empty(), "{diff:?}");
            assert_eq!(remember, Remember::Yes);
        }
        other => panic!("{other:?}"),
    }
    std::fs::remove_file(root.join(".claude/agents/other.md")).unwrap();
    std::fs::remove_file(root.join(".claude/agents/a.md")).unwrap();
    std::fs::write(root.join(SETTINGS), hook("touch /tmp/a")).unwrap();
    match fx.store().state(fx.discover(&root)) {
        TrustState::Changed { diff, .. } => {
            assert_eq!(
                diff,
                Diff {
                    added: vec![],
                    removed: vec![PathBuf::from(".claude/agents/other.md")],
                    changed: vec![]
                }
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_change_outside_the_configuration_stays_trusted() {
    let fx = Fixture::new("outside");
    let root = trusted(&fx, "p");
    write(&root.join("src/main.rs"), b"fn main() {}\n");
    write(&root.join("notes.txt"), b"hello\n");
    assert!(matches!(
        fx.store().state(fx.discover(&root)),
        TrustState::Trusted { .. }
    ));
}

#[test]
fn a_parent_record_never_trusts_a_child() {
    let fx = Fixture::new("parent");
    let repo = fx.repo("repo");
    write(&repo.join(".claude/settings.local.json"), &hook("touch /tmp/a"));
    let src = repo.join("src");
    std::fs::create_dir(&src).unwrap();
    let store = fx.store();
    store.record(&fx.discover(&repo), 1).unwrap();
    assert!(matches!(store.state(fx.discover(&repo)), TrustState::Trusted { .. }));
    let from_src = fx.discover(&src);
    assert!(
        !from_src.entries.is_empty(),
        "opening src still reaches the top level's local settings"
    );
    assert!(matches!(store.state(from_src), TrustState::Untrusted { .. }));
}

#[test]
fn a_child_record_never_trusts_a_parent() {
    let fx = Fixture::new("child");
    let repo = fx.repo("repo");
    write(&repo.join(".claude/settings.local.json"), &hook("touch /tmp/a"));
    let src = repo.join("src");
    std::fs::create_dir(&src).unwrap();
    let store = fx.store();
    store.record(&fx.discover(&src), 1).unwrap();
    assert!(matches!(store.state(fx.discover(&src)), TrustState::Trusted { .. }));
    assert!(matches!(store.state(fx.discover(&repo)), TrustState::Untrusted { .. }));
}

#[test]
fn the_claude_json_trust_flag_is_never_consulted() {
    let fx = Fixture::new("flag");
    let root = fx.root("p");
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    let flag = format!(
        r#"{{"projects":{{"{}":{{"hasTrustDialogAccepted":true}}}}}}"#,
        root.display()
    );
    write(&fx.home.join(".claude.json"), flag.as_bytes());
    write(&fx.home.join(".claude-work/.claude.json"), flag.as_bytes());
    match fx.store().state(fx.discover(&root)) {
        TrustState::Untrusted { .. } => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn project_trust_never_names_claude_json() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/project_trust");
    let needle = ".claude.json";
    let mut scanned = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains(needle),
                "{} names the account's config file",
                path.display()
            );
            scanned.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    for needed in ["nofollow.rs", "git_top.rs", "discover.rs", "store.rs"] {
        assert!(
            scanned.iter().any(|name| name == needed),
            "{needed} was not scanned: {scanned:?}"
        );
    }
}

#[test]
fn an_unusable_record_reads_as_untrusted_and_is_left_alone() {
    // A record that is valid for `root`, to copy or link to.
    fn valid(fx: &Fixture, root: &Path) -> Vec<u8> {
        let scratch = Fixture::new("valid-source");
        let store = scratch.store();
        store.record(&fx.discover(root), 7).unwrap();
        std::fs::read(scratch.record_file(root)).unwrap()
    }
    type Plant = Box<dyn Fn(&Fixture, &Path, &Path)>;
    let cases: Vec<(&str, Plant)> = vec![
        ("corrupt JSON", Box::new(|_, _, at| write(at, b"{not json"))),
        (
            "version 2",
            Box::new(|fx, root, at| {
                let text = String::from_utf8(valid(fx, root))
                    .unwrap()
                    .replace("\"version\": 1", "\"version\": 2");
                write(at, text.as_bytes());
            }),
        ),
        (
            "another root",
            Box::new(|fx, root, at| {
                let text = String::from_utf8(valid(fx, root))
                    .unwrap()
                    .replace(root.to_str().unwrap(), "/somewhere/else");
                write(at, text.as_bytes());
            }),
        ),
        (
            "a symlink to a valid record",
            Box::new(|fx, root, at| {
                let target = fx.outer.join("elsewhere.json");
                write(&target, &valid(fx, root));
                std::fs::create_dir_all(at.parent().unwrap()).unwrap();
                symlink(&target, at).unwrap();
            }),
        ),
        (
            "a FIFO",
            Box::new(|_, _, at| {
                std::fs::create_dir_all(at.parent().unwrap()).unwrap();
                mkfifo(at);
            }),
        ),
    ];
    for (label, plant) in cases {
        let fx = Fixture::new("unusable");
        let root = fx.root("p");
        write(&root.join(SETTINGS), &hook("touch /tmp/a"));
        let at = fx.record_file(&root);
        plant(&fx, &root, &at);
        let before = std::fs::symlink_metadata(&at).unwrap();
        let link_before = std::fs::read_link(&at).ok();
        let bytes_before = before.is_file().then(|| std::fs::read(&at).unwrap());

        let store = fx.store();
        let found = fx.discover(&root);
        match store.state(found.clone()) {
            TrustState::Untrusted { remember, .. } => assert_eq!(remember, Remember::Yes, "{label}"),
            other => panic!("{label}: {other:?}"),
        }
        let after = std::fs::symlink_metadata(&at).unwrap();
        assert_eq!(before.ino(), after.ino(), "{label}: the file was touched");
        assert_eq!(link_before, std::fs::read_link(&at).ok(), "{label}");
        assert_eq!(
            bytes_before,
            after.is_file().then(|| std::fs::read(&at).unwrap()),
            "{label}"
        );
        assert!(!at.with_extension("json.unusable").exists(), "{label}: renamed aside");

        // `record` replaces it atomically, the link itself and not its target.
        let link_target_bytes = link_before.as_ref().map(|target| std::fs::read(target).unwrap());
        store.record(&found, 9).unwrap();
        let replaced = std::fs::symlink_metadata(&at).unwrap();
        assert!(replaced.is_file(), "{label}: {:?}", replaced.file_type());
        assert!(
            matches!(store.state(fx.discover(&root)), TrustState::Trusted { .. }),
            "{label}"
        );
        if let Some(target) = link_before {
            assert_eq!(
                std::fs::read(&target).unwrap(),
                link_target_bytes.unwrap(),
                "{label}: the target was written"
            );
        }
    }
}

#[test]
fn files_and_dirs_are_private() {
    let fx = Fixture::new("private");
    let root = fx.root("p");
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    // SAFETY: `umask` has no pointer arguments; the previous value is restored below.
    let old = unsafe { libc::umask(0) };
    let result = fx.store().record(&fx.discover(&root), 1);
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    result.unwrap();
    let dir_mode = std::fs::metadata(fx.dir()).unwrap().permissions().mode() & 0o777;
    let file_mode = std::fs::metadata(fx.record_file(&root)).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(file_mode, 0o600);
    let leftovers: Vec<_> = std::fs::read_dir(fx.dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
}

#[test]
fn over_budget_is_session_only() {
    let fx = Fixture::new("budget");
    let root = fx.root("p");
    for i in 0..6 {
        write(&root.join(format!(".claude/agents/a{i}.md")), b"agent\n");
    }
    let limits = Limits {
        max_entries: 3,
        ..Limits::default()
    };
    let found = discover(&root, Some(&fx.home), &limits);
    assert!(found.over_budget.is_some());
    let store = fx.store();
    match store.state(found.clone()) {
        TrustState::Untrusted {
            remember: Remember::SessionOnly(note),
            ..
        } => {
            assert!(note.contains(found.over_budget.as_deref().unwrap()), "{note}");
        }
        other => panic!("{other:?}"),
    }
    assert!(store.record(&found, 1).is_err());
    assert!(!fx.record_file(&root).exists());
}

#[test]
fn no_state_home_is_window_only() {
    let fx = Fixture::new("nostate");
    let root = fx.root("p");
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    let store = TrustStore::new(None, None);
    let found = fx.discover(&root);
    match store.state(found.clone()) {
        TrustState::Untrusted {
            remember: Remember::WindowOnly(note),
            ..
        } => {
            assert!(note.contains("no usable state directory"), "{note}");
        }
        other => panic!("{other:?}"),
    }
    assert!(store.record(&found, 1).is_err());
    let relative = TrustStore::new(Some("relative/state".as_ref()), Some("relative/home".as_ref()));
    assert!(matches!(
        relative.state(found),
        TrustState::Untrusted {
            remember: Remember::WindowOnly(_),
            ..
        }
    ));
}

/// A settings file of `len` bytes: the hook, padded with spaces before its closing brace.
fn padded_hook(len: usize) -> Vec<u8> {
    let mut body = hook("touch /tmp/a");
    body.pop();
    body.resize(len - 1, b' ');
    body.push(b'}');
    body
}

/// A project whose only configuration is a 5 MiB settings file that holds a hook.
fn five_mib_settings(fx: &Fixture, name: &str) -> (PathBuf, Vec<u8>) {
    let root = fx.root(name);
    let body = padded_hook(5 << 20);
    write(&root.join(SETTINGS), &body);
    (root, body)
}

#[test]
fn an_uncheckable_discovery_is_session_only_and_never_recorded() {
    let fx = Fixture::new("uncheckable");
    let (root, body) = five_mib_settings(&fx, "p");
    let store = fx.store();
    let found = fx.discover(&root);
    assert!(!found.fully_hashed());
    match store.state(found.clone()) {
        TrustState::Untrusted {
            remember: Remember::SessionOnly(note),
            ..
        } => {
            assert!(note.contains(SETTINGS), "{note}");
        }
        other => panic!("{other:?}"),
    }
    assert!(store.record(&found, 1).is_err());
    assert!(!fx.record_file(&root).exists());
    assert!(!fx.dir().exists() || std::fs::read_dir(fx.dir()).unwrap().next().is_none());

    // A record made by hand with this very fingerprint still is not trust.
    let text = format!(
        r#"{{"version":1,"root":"{}","trusted_at":1,"fingerprint":"{}","files":{{}}}}"#,
        root.display(),
        found.fingerprint.hex()
    );
    write(&fx.record_file(&root), text.as_bytes());
    assert!(matches!(
        store.state(fx.discover(&root)),
        TrustState::Untrusted {
            remember: Remember::SessionOnly(_),
            ..
        }
    ));

    // Rewriting the hook in place at the same size, with the mtime put back, changes nothing the
    // fingerprint can see, and the answer is still a one-start answer.
    let path = root.join(SETTINGS);
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let rewritten: Vec<u8> = String::from_utf8(body)
        .unwrap()
        .replace("/tmp/a", "/tmp/b")
        .into_bytes();
    std::fs::write(&path, rewritten).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert!(matches!(
        store.state(fx.discover(&root)),
        TrustState::Untrusted {
            remember: Remember::SessionOnly(_),
            ..
        }
    ));
}

#[test]
fn a_trusted_file_grown_past_the_limit_is_changed_and_session_only() {
    let fx = Fixture::new("grown");
    let root = fx.root("p");
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    let store = fx.store();
    store.record(&fx.discover(&root), 1).unwrap();
    write(&root.join(SETTINGS), &padded_hook(5 << 20));
    match store.state(fx.discover(&root)) {
        TrustState::Changed {
            diff,
            remember: Remember::SessionOnly(note),
            ..
        } => {
            assert_eq!(diff.changed, vec![PathBuf::from(SETTINGS)]);
            assert!(note.contains(SETTINGS), "{note}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn forget_removes_only_the_own_record() {
    let fx = Fixture::new("forget");
    let root = trusted(&fx, "p");
    let store = fx.store();
    let other = trusted(&fx, "q");
    assert!(store.forget(&root).unwrap());
    assert!(!fx.record_file(&root).exists());
    assert!(fx.record_file(&other).exists());
    assert!(!store.forget(&root).unwrap(), "nothing left to forget");
    assert!(matches!(store.state(fx.discover(&root)), TrustState::Untrusted { .. }));

    // A link in the record's place is left alone, and so is its target.
    let target = fx.outer.join("target.json");
    write(&target, b"{}");
    symlink(&target, fx.record_file(&root)).unwrap();
    let err = store.forget(&root).unwrap_err();
    assert!(err.to_string().contains("a link"), "{err}");
    assert!(std::fs::symlink_metadata(fx.record_file(&root))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(target.exists());

    // So is anything that is not a regular file.
    std::fs::remove_file(fx.record_file(&root)).unwrap();
    mkfifo(&fx.record_file(&root));
    assert!(store.forget(&root).is_err());
    assert!(fx.record_file(&root).exists());
    assert!(!TrustStore::at(None).forget(&root).unwrap());
}

#[test]
fn a_state_dir_that_is_a_file_fails_the_record() {
    let fx = Fixture::new("dirfile");
    let root = fx.root("p");
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    write(&fx.dir(), b"i am a file\n");
    let store = fx.store();
    let found = fx.discover(&root);
    assert!(store.record(&found, 1).is_err());
    assert!(fx.dir().is_file());
    assert_eq!(std::fs::read(fx.dir()).unwrap(), b"i am a file\n");
    assert!(matches!(store.state(found), TrustState::Untrusted { .. }));
}

#[test]
fn nothing_to_trust_is_nothing_to_trust() {
    let fx = Fixture::new("nothing");
    let root = fx.root("p");
    write(&root.join("src/main.rs"), b"fn main() {}\n");
    assert!(matches!(
        fx.store().state(fx.discover(&root)),
        TrustState::NothingToTrust(_)
    ));
}

#[test]
fn civil_date_examples() {
    assert_eq!(civil_date(0), "1970-01-01");
    assert_eq!(civil_date(86_399), "1970-01-01");
    assert_eq!(civil_date(86_400), "1970-01-02");
    assert_eq!(civil_date(1_790_000_000), "2026-09-21");
    // 2024-02-29 00:00:00 UTC, a leap day, and the day after it.
    assert_eq!(civil_date(1_709_164_800), "2024-02-29");
    assert_eq!(civil_date(1_709_164_800 + 86_400), "2024-03-01");
    // 2100 is not a leap year.
    assert_eq!(civil_date(4_107_542_400), "2100-03-01");
}
