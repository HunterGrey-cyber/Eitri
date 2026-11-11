//! The trust record, on real files in a temporary directory.
//!
//! Every file a test inspects is made here, under a scratch directory that holds a fake home, a
//! fake state directory and the projects; no test points discovery at an existing project or at
//! the real home. `git` runs only to make fixtures, with no user or system configuration.

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use eitri_core::project_trust::{
    civil_date, discover, ChangedText, Diff, Discovery, EntryKind, Kept, Limits, NoDiff, Profile, ProfileEnv, Remember,
    TrustState, TrustStore, DISCLOSURE, MAX_KEPT_BYTES, MAX_KEPT_FILE_BYTES,
};

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
        discover(
            root,
            Some(&self.home),
            &Limits::default(),
            &Profile::Claude,
            &ProfileEnv::default(),
        )
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
            "a record from before the full disclosure",
            Box::new(|fx, root, at| {
                let mut record: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_slice(&valid(fx, root)).unwrap();
                assert!(record.remove("disclosure").is_some());
                write(at, serde_json::to_string_pretty(&record).unwrap().as_bytes());
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
    let found = discover(&root, Some(&fx.home), &limits, &Profile::Claude, &ProfileEnv::default());
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
        r#"{{"version":1,"root":"{}","trusted_at":1,"fingerprint":"{}","files":{{}},"disclosure":{}}}"#,
        root.display(),
        found.fingerprint.hex(),
        DISCLOSURE
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
    assert!(store.forget(&root, &Profile::Claude).unwrap());
    assert!(!fx.record_file(&root).exists());
    assert!(fx.record_file(&other).exists());
    assert!(
        !store.forget(&root, &Profile::Claude).unwrap(),
        "nothing left to forget"
    );
    assert!(matches!(store.state(fx.discover(&root)), TrustState::Untrusted { .. }));

    // A link in the record's place is left alone, and so is its target.
    let target = fx.outer.join("target.json");
    write(&target, b"{}");
    symlink(&target, fx.record_file(&root)).unwrap();
    let err = store.forget(&root, &Profile::Claude).unwrap_err();
    assert!(err.to_string().contains("a link"), "{err}");
    assert!(std::fs::symlink_metadata(fx.record_file(&root))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(target.exists());

    // So is anything that is not a regular file.
    std::fs::remove_file(fx.record_file(&root)).unwrap();
    mkfifo(&fx.record_file(&root));
    assert!(store.forget(&root, &Profile::Claude).is_err());
    assert!(fx.record_file(&root).exists());
    assert!(!TrustStore::at(None).forget(&root, &Profile::Claude).unwrap());
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

/// The fingerprint `project_trust_discover.rs` pins for the same tree, computed before discovery had
/// profiles.
const CLAUDE_FINGERPRINT_BEFORE_PROFILES: &str = "ba47e9a4d5cbf5ca032f353e8bb1737fd39b760374dbe8f83fcc8d8ee6411a9a";

/// The tree `profile_fixture` in `project_trust_discover.rs` builds, opened in `src`.
fn profile_fixture(fx: &Fixture) -> PathBuf {
    let repo = fx.repo("repo");
    write(
        &repo.join(".claude/settings.json"),
        br#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"touch /tmp/marker"}]}]}}"#,
    );
    write(
        &repo.join(".claude/settings.local.json"),
        br#"{"permissions":{"allow":["Bash(ls)"]}}"#,
    );
    write(&repo.join(".claude/agents/reviewer.md"), b"agent\n");
    write(&repo.join(".claude/worktrees/wt/x"), b"x");
    // Leads nowhere, so it stays a link nobody could follow.
    std::fs::remove_file(repo.join("README.md")).unwrap();
    symlink("../README.md", repo.join(".claude/link")).unwrap();
    write(
        &repo.join(".mcp.json"),
        br#"{"mcpServers":{"m":{"command":"sh","args":["-c","true"]}}}"#,
    );
    write(&repo.join("CLAUDE.md"), b"memory\n");
    write(&repo.join("src/CLAUDE.local.md"), b"local\n");
    write(&repo.join("AGENTS.md"), b"codex memory\n");
    write(&repo.join(".codex/config.toml"), b"model = \"x\"\n");
    write(&repo.join(".agents/skills/s/SKILL.md"), b"skill\n");
    repo.join("src")
}

fn discover_as(fx: &Fixture, root: &Path, profile: &Profile) -> Discovery {
    let env = ProfileEnv {
        codex_home: Some(fx.outer.join("codex-home")),
        fallback_names: Vec::new(),
        user_mcp_servers: None,
        user_mcp_names: None,
        user_config_unreadable: None,
    };
    discover(root, Some(&fx.home), &Limits::default(), profile, &env)
}

/// A skill linked from `.claude/` to another directory of the same root: every part of it is hashed, so
/// the answer is kept, and a change behind the link asks again and is named where the CLI reads it.
#[test]
fn a_link_inside_the_root_is_remembered_and_what_it_reaches_is_checked() {
    let fx = Fixture::new("link-inside");
    let root = fx.repo("repo");
    write(&root.join(".claude/settings.json"), b"{}");
    write(&root.join(".codex/skills/vm/SKILL.md"), b"skill\n");
    write(&root.join("docs/x.md"), b"same\n");
    write(&root.join("docs/y.md"), b"same\n");
    std::fs::create_dir_all(root.join(".claude/skills")).unwrap();
    std::fs::create_dir_all(root.join(".claude/commands")).unwrap();
    symlink("../../.codex/skills/vm", root.join(".claude/skills/vm")).unwrap();
    symlink("../../docs/x.md", root.join(".claude/commands/x.md")).unwrap();

    let store = fx.store();
    let found = fx.discover(&root);
    match store.state(found.clone()) {
        TrustState::Untrusted { remember, .. } => assert_eq!(remember, Remember::Yes),
        other => panic!("{other:?}"),
    }
    store.record(&found, 1_800_000_000).unwrap();
    assert!(matches!(store.state(fx.discover(&root)), TrustState::Trusted { .. }));

    write(&root.join(".codex/skills/vm/SKILL.md"), b"edited\n");
    match store.state(fx.discover(&root)) {
        TrustState::Changed { diff, remember, .. } => {
            assert_eq!(remember, Remember::Yes);
            assert_eq!(diff.changed, vec![PathBuf::from(".claude/skills/vm/SKILL.md")]);
        }
        other => panic!("{other:?}"),
    }
    write(&root.join(".codex/skills/vm/SKILL.md"), b"skill\n");

    // The same bytes behind another target are another configuration, and named as one.
    std::fs::remove_file(root.join(".claude/commands/x.md")).unwrap();
    symlink("../../docs/y.md", root.join(".claude/commands/x.md")).unwrap();
    match store.state(fx.discover(&root)) {
        TrustState::Changed { diff, .. } => {
            assert_eq!(diff.changed, vec![PathBuf::from(".claude/commands/x.md")]);
        }
        other => panic!("{other:?}"),
    }
}

/// The two links people keep between Claude's and Codex's files: each is hashed through, so the answer
/// is kept, and a change behind it asks again and is named where the agent reads it.
#[test]
fn links_between_claude_and_codex_files_are_remembered_and_checked() {
    let fx = Fixture::new("link-both-ways");
    let root = fx.repo("repo");
    write(&root.join("AGENTS.md"), b"memory\n");
    write(&root.join(".claude/skills/vm/SKILL.md"), b"skill\n");
    write(&root.join(".codex/config.toml"), b"model = \"m\"\n");
    std::fs::create_dir_all(root.join(".codex/skills")).unwrap();
    symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
    symlink("../../.claude/skills/vm", root.join(".codex/skills/vm")).unwrap();

    let store = fx.store();
    let cases = [
        (Profile::Claude, "AGENTS.md", b"edited\n".as_slice(), "CLAUDE.md"),
        (
            Profile::Codex,
            ".claude/skills/vm/SKILL.md",
            b"edited\n".as_slice(),
            ".codex/skills/vm/SKILL.md",
        ),
    ];
    for (profile, edit, edited, named) in cases {
        let found = discover_as(&fx, &root, &profile);
        match store.state(found.clone()) {
            TrustState::Untrusted { remember, .. } => assert_eq!(remember, Remember::Yes, "{profile:?}"),
            other => panic!("{other:?}"),
        }
        store.record(&found, 1_800_000_000).unwrap();
        assert!(matches!(
            store.state(discover_as(&fx, &root, &profile)),
            TrustState::Trusted { .. }
        ));
        let before = std::fs::read(root.join(edit)).unwrap();
        write(&root.join(edit), edited);
        match store.state(discover_as(&fx, &root, &profile)) {
            TrustState::Changed { diff, remember, .. } => {
                assert_eq!(remember, Remember::Yes);
                assert_eq!(diff.changed, vec![PathBuf::from(named)]);
            }
            other => panic!("{other:?}"),
        }
        write(&root.join(edit), &before);
    }
}

/// A change behind a file link is named at that link, also when another link passes through it.
#[test]
fn a_change_behind_a_link_another_link_passes_through_names_both() {
    let fx = Fixture::new("link-through");
    let root = fx.repo("repo");
    write(&root.join("config/claude.json"), b"{}");
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    symlink("../config/claude.json", root.join(".claude/settings.json")).unwrap();
    symlink("settings.json", root.join(".claude/z.json")).unwrap();
    let store = fx.store();
    store.record(&fx.discover(&root), 1_800_000_000).unwrap();
    write(&root.join("config/claude.json"), br#"{"env":{}}"#);
    match store.state(fx.discover(&root)) {
        TrustState::Changed { diff, .. } => assert_eq!(
            diff.changed,
            vec![PathBuf::from(".claude/settings.json"), PathBuf::from(".claude/z.json")]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_record_from_before_the_full_disclosure_is_asked_about_again() {
    let fx = Fixture::new("before-profiles");
    let root = profile_fixture(&fx);
    std::fs::create_dir_all(fx.dir()).unwrap();
    std::fs::set_permissions(fx.state.join("eitri"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(fx.dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = fx.record_file(&root);
    let plant = |record: serde_json::Value| {
        std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    // A record exactly as the store wrote it before profiles existed, and before the prompt showed
    // every value whole: no profile field, no disclosure, the same name, the same fingerprint. The
    // `y` it stands for was given to a prompt that showed less, so the project is asked about again,
    // as an ordinary untrusted question whose answer can be recorded.
    let before = serde_json::json!({
        "version": 1,
        "root": root.to_str().unwrap(),
        "trusted_at": 1_790_000_000u64,
        "fingerprint": CLAUDE_FINGERPRINT_BEFORE_PROFILES,
        "files": {},
    });
    plant(before.clone());
    match fx.store().state(fx.discover(&root)) {
        TrustState::Untrusted { remember, discovery } => {
            // The tree holds a link below `.claude/` that could not be followed, so the answer lasts
            // one start whatever the record says.
            assert!(
                matches!(&remember, Remember::SessionOnly(note) if note.contains(".claude/link")),
                "{remember:?}"
            );
            // The fingerprint itself is unchanged: only the record is too old to count.
            assert_eq!(discovery.fingerprint.hex(), CLAUDE_FINGERPRINT_BEFORE_PROFILES);
        }
        other => panic!("{other:?}"),
    }
    // Without the link everything is hashed; a record of that fingerprint from before the full
    // disclosure is asked about again as an ordinary question whose answer can be recorded.
    std::fs::remove_file(root.parent().unwrap().join(".claude/link")).unwrap();
    let unlinked = fx.discover(&root);
    assert!(unlinked.fully_hashed(), "{:?}", unlinked.uncheckable());
    let mut before = before;
    before["fingerprint"] = serde_json::json!(unlinked.fingerprint.hex());
    plant(before.clone());
    match fx.store().state(fx.discover(&root)) {
        TrustState::Untrusted { remember, .. } => assert_eq!(remember, Remember::Yes),
        other => panic!("{other:?}"),
    }
    // A lower disclosure is no better than none: neither the one that showed every value whole but
    // only counted the other files under `.claude/`, nor any before it.
    assert_eq!(DISCLOSURE, 5);
    for lower_disclosure in [1, 2, 3, 4] {
        let mut lower = before.clone();
        lower["disclosure"] = serde_json::json!(lower_disclosure);
        plant(lower);
        assert!(matches!(
            fx.store().state(fx.discover(&root)),
            TrustState::Untrusted {
                remember: Remember::Yes,
                ..
            }
        ));
    }
    // The same record at the current disclosure is trust.
    let mut current = before;
    current["disclosure"] = serde_json::json!(DISCLOSURE);
    plant(current);
    match fx.store().state(fx.discover(&root)) {
        TrustState::Trusted { since_unix, discovery } => {
            assert_eq!(since_unix, 1_790_000_000);
            assert_eq!(discovery.fingerprint, unlinked.fingerprint);
        }
        other => panic!("{other:?}"),
    }
    // A Claude record written now has the same name, no profile field, the current disclosure, and
    // is trusted.
    std::fs::remove_file(&path).unwrap();
    fx.store().record(&fx.discover(&root), 7).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let written = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).unwrap();
    let keys: Vec<String> = written.keys().cloned().collect();
    assert_eq!(
        keys,
        ["disclosure", "files", "fingerprint", "root", "trusted_at", "version"]
    );
    assert_eq!(written["disclosure"], serde_json::json!(DISCLOSURE));
    assert_eq!(written["version"], serde_json::json!(1));
    assert!(matches!(
        fx.store().state(fx.discover(&root)),
        TrustState::Trusted { .. }
    ));
}

#[test]
fn another_profiles_record_from_before_the_full_disclosure_is_asked_about_again() {
    let fx = Fixture::new("codex-before-disclosure");
    let root = profile_fixture(&fx);
    let store = fx.store();
    let found = discover_as(&fx, &root, &Profile::Codex);
    store.record(&found, 3).unwrap();
    assert!(matches!(store.state(found.clone()), TrustState::Trusted { .. }));
    // The same record with its disclosure taken away, as a build before the field wrote it.
    let path = fx
        .dir()
        .join(format!("{}.json.codex", &agent::conversation_id_for_cwd(&root)[..16]));
    let mut record: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(record.remove("disclosure"), Some(serde_json::json!(DISCLOSURE)));
    assert_eq!(record["profile"], serde_json::json!("codex"));
    std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap()).unwrap();
    assert!(matches!(store.state(found), TrustState::Untrusted { .. }));
}

#[test]
fn profiles_never_alias_records() {
    let fx = Fixture::new("alias");
    let root = profile_fixture(&fx);
    // A link below `.claude/` would make Claude's answer last one start, and this test records it.
    std::fs::remove_file(root.parent().unwrap().join(".claude/link")).unwrap();
    let store = fx.store();
    let gemini = Profile::Acp {
        name: "codex".into(),
        files: vec!["GEMINI.md".into(), ".gemini/".into()],
    };
    write(&root.join("GEMINI.md"), b"gemini\n");
    let profiles = [Profile::Claude, Profile::Codex, gemini.clone()];
    let found: Vec<Discovery> = profiles.iter().map(|p| discover_as(&fx, &root, p)).collect();
    for (i, a) in found.iter().enumerate() {
        for b in &found[i + 1..] {
            assert_ne!(a.fingerprint, b.fingerprint, "{:?} and {:?}", a.profile, b.profile);
        }
    }
    // Trusting for Claude trusts nothing else.
    store.record(&found[0], 1).unwrap();
    assert!(matches!(store.state(found[0].clone()), TrustState::Trusted { .. }));
    for other in &found[1..] {
        assert!(
            matches!(store.state(other.clone()), TrustState::Untrusted { .. }),
            "{:?}",
            other.profile
        );
    }
    // Each profile's record has its own name.
    store.record(&found[1], 2).unwrap();
    store.record(&found[2], 3).unwrap();
    let claude_file = fx.record_file(&root);
    let codex_file = PathBuf::from(format!("{}.codex", claude_file.display()));
    let acp_file = PathBuf::from(format!("{}.acp-codex", claude_file.display()));
    for file in [&claude_file, &codex_file, &acp_file] {
        assert!(file.is_file(), "{} missing", file.display());
    }
    let records = std::fs::read_dir(fx.dir())
        .unwrap()
        .filter(|entry| {
            !entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with("~last-trusted")
        })
        .count();
    assert_eq!(records, 3);
    for found in &found {
        assert!(matches!(store.state(found.clone()), TrustState::Trusted { .. }));
    }
    // A record under another profile's name does not answer for it, whichever way round.
    std::fs::copy(&codex_file, &claude_file).unwrap();
    assert!(!matches!(store.state(found[0].clone()), TrustState::Trusted { .. }));
    store.record(&found[0], 1).unwrap();
    std::fs::copy(&claude_file, &codex_file).unwrap();
    assert!(!matches!(store.state(found[1].clone()), TrustState::Trusted { .. }));
    // Forgetting one profile's record leaves the others.
    assert!(store.forget(&root, &gemini).unwrap());
    assert!(!acp_file.exists());
    assert!(matches!(store.state(found[0].clone()), TrustState::Trusted { .. }));
    assert!(matches!(store.state(found[2].clone()), TrustState::Untrusted { .. }));
    // A registered name never spells a path or another key.
    let odd = Profile::Acp {
        name: "../x y".into(),
        files: Vec::new(),
    };
    assert_eq!(odd.key(), "acp-..%2Fx%20y");
    assert_ne!(Profile::Codex.key(), gemini.key());
}

/// A registered agent starts only where the user said yes for it, so finding nothing is still a
/// question: an empty discovery is untrusted until its empty fingerprint is recorded, and a file
/// that appears afterwards changes it. Claude and Codex keep "nothing to trust".
#[test]
fn an_explicit_yes_profile_asks_about_nothing_found_and_records_it() {
    let fx = Fixture::new("explicit-yes");
    let root = fx.root("empty");
    let store = fx.store();
    let acp = Profile::Acp {
        name: "gem".into(),
        files: vec!["GEM.md".into()],
    };
    assert!(acp.requires_explicit_yes());
    assert!(!Profile::Claude.requires_explicit_yes());
    assert!(!Profile::Codex.requires_explicit_yes());
    for quiet in [Profile::Claude, Profile::Codex] {
        assert!(matches!(
            store.state(discover_as(&fx, &root, &quiet)),
            TrustState::NothingToTrust(_)
        ));
    }
    let empty = discover_as(&fx, &root, &acp);
    assert!(empty.nothing_to_trust());
    assert!(matches!(
        store.state(empty.clone()),
        TrustState::Untrusted {
            remember: Remember::Yes,
            ..
        }
    ));
    store.record(&empty, 5).unwrap();
    assert!(matches!(
        store.state(discover_as(&fx, &root, &acp)),
        TrustState::Trusted { since_unix: 5, .. }
    ));
    write(&root.join("GEM.md"), b"now there is one\n");
    assert!(matches!(
        store.state(discover_as(&fx, &root, &acp)),
        TrustState::Changed { .. }
    ));
}

#[test]
fn profiles_are_named_for_people() {
    assert_eq!(Profile::Claude.label(), "Claude");
    assert_eq!(Profile::Codex.label(), "Codex");
    let acp = Profile::Acp {
        name: "gem".into(),
        files: vec!["GEM.md".into()],
    };
    assert_eq!(acp.label(), "gem");
}

#[test]
fn an_unparsed_settings_file_is_session_only_and_never_recorded() {
    let fx = Fixture::new("unparsed");
    let root = fx.root("p");
    // A lone surrogate escape: serde_json refuses it, another JSON parser may not.
    write(
        &root.join(SETTINGS),
        br#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"x\ud800"}]}]}}"#,
    );
    let store = fx.store();
    let found = fx.discover(&root);
    assert!(!found.fully_hashed());
    match store.state(found.clone()) {
        TrustState::Untrusted {
            remember: Remember::SessionOnly(note),
            ..
        } => assert!(note.contains(SETTINGS), "{note}"),
        other => panic!("{other:?}"),
    }
    assert!(store.record(&found, 1).is_err());
    assert!(!fx.record_file(&root).exists());
}

#[test]
fn an_unparseable_codex_config_is_session_only_and_shows_its_text() {
    let fx = Fixture::new("codex-bad-toml");
    let root = profile_fixture(&fx);
    let repo = root.parent().unwrap().to_path_buf();
    write(
        &repo.join(".codex/config.toml"),
        b"[mcp_servers.marker]\ncommand = \"sh -c 'touch /tmp/n'\"\n[unclosed\n",
    );
    let store = fx.store();
    let found = discover_as(&fx, &root, &Profile::Codex);
    assert!(!found.fully_hashed());
    // Named as unchecked, in words that fit a TOML file, and with the text Codex might still load.
    assert!(found
        .uncheckable()
        .iter()
        .any(|(path, why)| *path == Path::new(".codex/config.toml") && *why == "could not be parsed for this check"));
    assert!(found.findings.iter().any(|f| matches!(f,
        eitri_core::project_trust::Finding::Unparsed { file, reason }
            if file == Path::new(".codex/config.toml") && reason.contains("command = \"sh -c 'touch /tmp/n'\""))));
    match store.state(found.clone()) {
        TrustState::Untrusted {
            remember: Remember::SessionOnly(note),
            ..
        } => assert!(
            note.contains(".codex/config.toml (could not be parsed for this check)"),
            "{note}"
        ),
        other => panic!("{other:?}"),
    }
    let refused = store.record(&found, 1).unwrap_err();
    assert!(
        refused.to_string().starts_with("not recorded: cannot be checked"),
        "{refused}"
    );
    assert!(!fx
        .dir()
        .join(format!("{}.json.codex", &agent::conversation_id_for_cwd(&root)[..16]))
        .exists());
}

/// A registered agent's profile: one file and one directory it reads whole.
fn gem_profile() -> Profile {
    Profile::Acp {
        name: "gem".into(),
        files: vec!["GEM.md".into(), ".gem/".into()],
    }
}

/// Asserts that `root`'s configuration for `profile` can be answered for one start only, names
/// `path` as what cannot be checked, and is never recorded.
fn assert_session_only(fx: &Fixture, root: &Path, profile: &Profile, path: &str) {
    let found = discover_as(fx, root, profile);
    assert!(!found.fully_hashed(), "{:?}", found.findings);
    assert!(
        found.uncheckable().iter().any(|(p, _)| *p == Path::new(path)),
        "{path} not named: {:?}",
        found.uncheckable()
    );
    match fx.store().state(found.clone()) {
        TrustState::Untrusted {
            remember: Remember::SessionOnly(note),
            ..
        } => assert!(note.contains(path), "{note}"),
        other => panic!("{other:?}"),
    }
    assert!(fx.store().record(&found, 1).is_err());
    let acp_file = PathBuf::from(format!("{}.{}", fx.record_file(root).display(), profile.key()));
    assert!(!acp_file.exists());
}

/// Eitri cannot know which files in a registered directory the agent reads, so a link anywhere in
/// it leaves the agent reading what no fingerprint covers: the answer lasts one start.
#[test]
fn a_link_inside_a_registered_directory_is_session_only() {
    let fx = Fixture::new("acp-inner-link");
    let root = fx.root("p");
    write(&root.join("payload.json"), b"{\"a\":1}\n");
    write(&root.join(".gem/plain.txt"), b"plain\n");
    symlink("../payload.json", root.join(".gem/settings.json")).unwrap();
    let profile = gem_profile();
    assert_session_only(&fx, &root, &profile, ".gem/settings.json");

    // Deeper down, and a link in place of a directory, are the same.
    std::fs::remove_file(root.join(".gem/settings.json")).unwrap();
    write(&root.join(".gem/deep/er/x.json"), b"{}\n");
    symlink("../../../payload.json", root.join(".gem/deep/er/y.json")).unwrap();
    assert_session_only(&fx, &root, &profile, ".gem/deep/er/y.json");
    std::fs::remove_file(root.join(".gem/deep/er/y.json")).unwrap();
    std::fs::create_dir(root.join("elsewhere")).unwrap();
    symlink("../../elsewhere", root.join(".gem/deep/linked")).unwrap();
    assert_session_only(&fx, &root, &profile, ".gem/deep/linked");
}

#[test]
fn a_registered_file_that_is_a_link_is_session_only() {
    let fx = Fixture::new("acp-file-link");
    let root = fx.root("p");
    write(&root.join("payload.md"), b"memory\n");
    symlink("payload.md", root.join("GEM.md")).unwrap();
    assert_session_only(&fx, &root, &gem_profile(), "GEM.md");
}

#[test]
fn a_registered_directory_that_is_a_link_is_session_only() {
    let fx = Fixture::new("acp-dir-link");
    let root = fx.root("p");
    write(&root.join("real/settings.json"), b"{}\n");
    symlink("real", root.join(".gem")).unwrap();
    assert_session_only(&fx, &root, &gem_profile(), ".gem");
}

/// Without a link, a registered agent's configuration is recorded, and a change to any file in its
/// directory asks again.
#[test]
fn a_registered_plain_tree_is_recorded() {
    let fx = Fixture::new("acp-plain");
    let root = fx.root("p");
    write(&root.join("GEM.md"), b"memory\n");
    write(&root.join(".gem/settings.json"), b"{\"a\":1}\n");
    write(&root.join(".gem/deep/x.json"), b"{}\n");
    let profile = gem_profile();
    let store = fx.store();
    let found = discover_as(&fx, &root, &profile);
    assert!(found.fully_hashed(), "{:?}", found.findings);
    assert!(matches!(
        store.state(found.clone()),
        TrustState::Untrusted {
            remember: Remember::Yes,
            ..
        }
    ));
    store.record(&found, 9).unwrap();
    assert!(matches!(
        store.state(discover_as(&fx, &root, &profile)),
        TrustState::Trusted { since_unix: 9, .. }
    ));
    write(&root.join(".gem/deep/x.json"), b"{\"b\":2}\n");
    match store.state(discover_as(&fx, &root, &profile)) {
        TrustState::Changed { diff, .. } => assert_eq!(diff.changed, vec![PathBuf::from(".gem/deep/x.json")]),
        other => panic!("{other:?}"),
    }
}

/// A state home reached through a link (`/home -> /var/home`, a linked `~/.local/state`): the
/// record is written and read back at the same real place, so a `y` that said "trusted" is trust
/// at the next start too. The record file itself is still never followed.
#[test]
fn a_store_under_a_linked_directory_reads_back_what_it_records() {
    let fx = Fixture::new("linked-state");
    let root = fx.root("p");
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    let real = fx.outer.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = fx.outer.join("link");
    symlink(&real, &link).unwrap();
    for state in [link.join("state"), link.clone()] {
        let store = TrustStore::new(Some(state.as_os_str()), Some(fx.home.as_os_str()));
        let found = fx.discover(&root);
        assert!(found.fully_hashed());
        store.record(&found, 5).unwrap();
        assert!(
            matches!(
                store.state(fx.discover(&root)),
                TrustState::Trusted { since_unix: 5, .. }
            ),
            "{}",
            state.display()
        );
        let in_real = real
            .join(state.strip_prefix(&link).unwrap())
            .join("eitri/trust")
            .join(format!("{}.json", &agent::conversation_id_for_cwd(&root)[..16]));
        assert!(in_real.is_file(), "{} missing", in_real.display());
        assert!(store.forget(&root, &Profile::Claude).unwrap());
        assert!(!in_real.exists());
        assert!(matches!(store.state(fx.discover(&root)), TrustState::Untrusted { .. }));
    }
}

/// A record that would not read back as trust is an error, never a silent "trusted": here one too
/// large for the reader, from a configuration of many long-named files.
#[test]
fn a_record_that_does_not_read_back_is_an_error() {
    let fx = Fixture::new("no-read-back");
    let root = fx.root("p");
    let dir = root.join(".claude/commands").join("d".repeat(250));
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..1900 {
        std::fs::write(dir.join(format!("{i:04}{}", "n".repeat(240))), b"x").unwrap();
    }
    let found = fx.discover(&root);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    let store = fx.store();
    let refused = store.record(&found, 1).unwrap_err();
    assert!(refused.to_string().contains("could not be read back"), "{refused}");
    assert!(!matches!(store.state(found), TrustState::Trusted { .. }));
}

/// The refusal of an oversized record says what happened: nothing was written, so it must not talk
/// of a record that "could not be read back" as if one had been put on disk.
#[test]
fn an_oversized_record_is_refused_in_words_that_say_nothing_was_written() {
    let fx = Fixture::new("oversized-wording");
    let root = fx.root("p");
    let dir = root.join(".claude/commands").join("d".repeat(250));
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..1900 {
        std::fs::write(dir.join(format!("{i:04}{}", "n".repeat(240))), b"x").unwrap();
    }
    let found = fx.discover(&root);
    let store = fx.store();
    let refused = store.record(&found, 1).unwrap_err().to_string();
    assert!(
        refused.starts_with("not written: a record this large could not be read back"),
        "{refused}"
    );
    assert!(!refused.contains("the trust record"), "{refused}");
}

/// The same configuration as above, asked about: since its record could never be kept, the check says
/// so from the start (a `y` lasts for the window, with the reason), rather than offering a record
/// that fails once the user has answered. A small configuration still offers one.
#[test]
fn a_configuration_whose_record_cannot_be_kept_is_window_only_from_the_start() {
    let fx = Fixture::new("record-too-large");
    let root = fx.root("p");
    let dir = root.join(".claude/commands").join("d".repeat(250));
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..1900 {
        std::fs::write(dir.join(format!("{i:04}{}", "n".repeat(240))), b"x").unwrap();
    }
    let store = fx.store();
    match store.state(fx.discover(&root)) {
        TrustState::Untrusted {
            remember: Remember::WindowOnly(why),
            ..
        } => {
            assert!(why.starts_with("a trust record of it would be "), "{why}");
            assert!(
                why.contains(" files and directories, more than the 1.0 MiB a record may be"),
                "{why}"
            );
        }
        other => panic!("{other:?}"),
    }
    let small = fx.root("small");
    write(&small.join(".claude/settings.json"), &hook("touch /tmp/a"));
    assert!(matches!(
        store.state(fx.discover(&small)),
        TrustState::Untrusted {
            remember: Remember::Yes,
            ..
        }
    ));
}

/// A record the reader would refuse is not written at all: no file is left that could only ever read
/// as untrusted.
#[test]
fn a_record_over_the_cap_is_refused_before_anything_is_written() {
    let fx = Fixture::new("record-not-written");
    let root = fx.root("p");
    let dir = root.join(".claude/commands").join("d".repeat(250));
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..1900 {
        std::fs::write(dir.join(format!("{i:04}{}", "n".repeat(240))), b"x").unwrap();
    }
    let found = fx.discover(&root);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    let refused = fx.store().record(&found, 1).unwrap_err();
    assert!(refused.to_string().starts_with("not written: "), "{refused}");
    assert!(!fx.record_file(&root).exists(), "a record was left behind");
    let leftovers: Vec<_> = std::fs::read_dir(fx.dir())
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

const MEMORY: &str = "CLAUDE.md";

fn copy_file(fx: &Fixture, root: &Path) -> PathBuf {
    let record = fx.record_file(root);
    record.with_file_name(format!(
        "{}~last-trusted",
        record.file_name().unwrap().to_str().unwrap()
    ))
}

/// A root whose `CLAUDE.md` holds `text`, trusted.
fn trusted_memory(fx: &Fixture, name: &str, text: &[u8]) -> PathBuf {
    let root = fx.root(name);
    write(&root.join(MEMORY), text);
    fx.store().record(&fx.discover(&root), 1).unwrap();
    root
}

fn texts_of(fx: &Fixture, root: &Path) -> Vec<ChangedText> {
    match fx.store().state(fx.discover(root)) {
        TrustState::Changed { texts, .. } => texts,
        other => panic!("{other:?}"),
    }
}

fn no_diff(path: &str, why: NoDiff) -> ChangedText {
    ChangedText::NoDiff {
        path: PathBuf::from(path),
        why,
    }
}

#[test]
fn a_changed_instruction_file_comes_with_its_trusted_text() {
    let fx = Fixture::new("text");
    let root = trusted_memory(&fx, "p", b"keep\nold\n");
    write(&root.join(MEMORY), b"keep\nnew\n");
    assert_eq!(
        texts_of(&fx, &root),
        vec![ChangedText::Diff {
            path: PathBuf::from(MEMORY),
            before: "keep\nold\n".into(),
            after: "keep\nnew\n".into(),
        }]
    );
}

#[test]
fn every_instruction_file_is_kept_and_nothing_else() {
    let fx = Fixture::new("which");
    let root = fx.root("p");
    let instructions = [
        "CLAUDE.md",
        "CLAUDE.local.md",
        ".claude/CLAUDE.md",
        ".claude/rules/style.md",
        ".claude/rules/deep/more.md",
    ];
    for file in instructions {
        write(&root.join(file), b"before\n");
    }
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    write(&root.join(".claude/commands/go.md"), b"before\n");
    write(&root.join(".claude/rules/data.txt"), b"before\n");
    let found = fx.discover(&root);
    let mut kept: Vec<_> = found.instructions.keys().map(|p| p.to_str().unwrap()).collect();
    kept.sort();
    let mut want = instructions.to_vec();
    want.sort();
    assert_eq!(kept, want);
    fx.store().record(&found, 1).unwrap();
    for file in instructions
        .iter()
        .chain(&[".claude/commands/go.md", ".claude/rules/data.txt"])
    {
        write(&root.join(file), b"after\n");
    }
    let texts = texts_of(&fx, &root);
    assert_eq!(texts.len(), instructions.len(), "{texts:?}");
    assert!(texts
        .iter()
        .all(|t| matches!(t, ChangedText::Diff { after, .. } if after == "after\n")));
}

/// The text kept is the text hashed, from the one read.
#[test]
fn the_text_kept_is_the_text_the_fingerprint_covers() {
    let fx = Fixture::new("same-read");
    let root = fx.root("p");
    write(&root.join(MEMORY), b"hashed\n");
    let found = fx.discover(&root);
    let Kept::Text(text) = &found.instructions[Path::new(MEMORY)] else {
        panic!("{:?}", found.instructions)
    };
    use sha2::{Digest, Sha256};
    let sha256: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    assert!(found
        .entries
        .iter()
        .any(|entry| entry.path == Path::new(MEMORY) && entry.kind == EntryKind::File { sha256, len: 7 }));
}

#[test]
fn codex_and_a_registered_agent_keep_their_own_files_under_their_own_records() {
    let fx = Fixture::new("profiles");
    let root = fx.root("p");
    write(&root.join("AGENTS.md"), b"codex old\n");
    write(&root.join("GEM.md"), b"gem old\n");
    write(&root.join(MEMORY), b"claude old\n");
    let gem = Profile::Acp {
        name: "gem".into(),
        files: vec!["GEM.md".into()],
    };
    let store = fx.store();
    for profile in [Profile::Claude, Profile::Codex, gem.clone()] {
        store.record(&discover_as(&fx, &root, &profile), 1).unwrap();
    }
    write(&root.join("AGENTS.md"), b"codex new\n");
    write(&root.join("GEM.md"), b"gem new\n");
    let before_of = |profile: &Profile, file: &str| match store.state(discover_as(&fx, &root, profile)) {
        TrustState::Changed { texts, .. } => match &texts[..] {
            [ChangedText::Diff { path, before, .. }] if path == Path::new(file) => before.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(before_of(&Profile::Codex, "AGENTS.md"), "codex old\n");
    assert_eq!(before_of(&gem, "GEM.md"), "gem old\n");

    // Forgetting one agent leaves the other's copy.
    assert!(store.forget(&root, &Profile::Codex).unwrap());
    assert_eq!(before_of(&gem, "GEM.md"), "gem old\n");
    assert!(fx
        .dir()
        .join(format!(
            "{}.acp-gem~last-trusted",
            fx.record_file(&root).file_name().unwrap().to_str().unwrap()
        ))
        .exists());
    assert!(copy_file(&fx, &root).exists(), "Claude's copy stays");
}

/// A registered name can hold almost anything, and its record's file name holds its key: no name may
/// reach the file another agent keeps its text in.
#[test]
fn no_registered_name_is_another_agents_copy() {
    let fx = Fixture::new("names");
    let root = fx.root("p");
    write(&root.join("GEM.md"), b"old\n");
    let agent = |name: &str| Profile::Acp {
        name: name.into(),
        files: vec!["GEM.md".into()],
    };
    let store = fx.store();
    let (gem, beside) = (agent("gem"), agent("gem~last-trusted"));
    let also = agent("gem.last-trusted");
    for profile in [&gem, &beside, &also] {
        store.record(&discover_as(&fx, &root, profile), 1).unwrap();
    }
    for profile in [&gem, &beside, &also] {
        assert!(
            matches!(
                store.state(discover_as(&fx, &root, profile)),
                TrustState::Trusted { .. }
            ),
            "{profile:?}"
        );
    }
    write(&root.join("GEM.md"), b"new\n");
    for profile in [&gem, &beside, &also] {
        match store.state(discover_as(&fx, &root, profile)) {
            TrustState::Changed { texts, .. } => {
                assert!(
                    matches!(&texts[..], [ChangedText::Diff { before, .. }] if before == "old\n"),
                    "{texts:?}"
                )
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn the_copy_is_private_and_moves_with_each_yes() {
    let fx = Fixture::new("private");
    let root = trusted_memory(&fx, "p", b"first\n");
    let copy = copy_file(&fx, &root);
    assert_eq!(std::fs::metadata(&copy).unwrap().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(fx.dir()).unwrap().mode() & 0o777, 0o700);
    assert!(std::fs::read_to_string(&copy).unwrap().contains("first"));

    // An `n` or a question left open moves nothing: the comparison stays with the last `y`.
    write(&root.join(MEMORY), b"second\n");
    write(&root.join(MEMORY), b"third\n");
    assert!(matches!(&texts_of(&fx, &root)[..], [ChangedText::Diff { before, .. }] if before == "first\n"));

    fx.store().record(&fx.discover(&root), 2).unwrap();
    assert!(!std::fs::read_to_string(&copy).unwrap().contains("first"));
    write(&root.join(MEMORY), b"fourth\n");
    assert!(matches!(&texts_of(&fx, &root)[..], [ChangedText::Diff { before, .. }] if before == "third\n"));

    // A `y` to a configuration with no instruction file left leaves no text behind.
    std::fs::remove_file(root.join(MEMORY)).unwrap();
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    fx.store().record(&fx.discover(&root), 3).unwrap();
    assert!(!copy.exists());
}

#[test]
fn an_older_store_reads_as_it_did_with_a_line_for_the_file() {
    let fx = Fixture::new("older");
    let root = trusted_memory(&fx, "p", b"old\n");
    std::fs::remove_file(copy_file(&fx, &root)).unwrap();
    assert!(
        matches!(fx.store().state(fx.discover(&root)), TrustState::Trusted { .. }),
        "no copy is no reason to ask"
    );
    write(&root.join(MEMORY), b"new\n");
    match fx.store().state(fx.discover(&root)) {
        TrustState::Changed {
            diff, remember, texts, ..
        } => {
            assert_eq!(diff.changed, vec![PathBuf::from(MEMORY)]);
            assert_eq!(remember, Remember::Yes);
            assert_eq!(texts, vec![no_diff(MEMORY, NoDiff::NoEarlierCopy)]);
        }
        other => panic!("{other:?}"),
    }
}

/// The copy is read under the record's rules and only counts when its hash is the record's, so
/// nothing in its place can draw a "before" that was not trusted.
#[test]
fn a_copy_that_is_not_the_trusted_text_shows_nothing() {
    let fx = Fixture::new("forged");
    let root = trusted_memory(&fx, "p", b"trusted\n");
    let copy = copy_file(&fx, &root);
    let good = std::fs::read_to_string(&copy).unwrap();
    write(&root.join(MEMORY), b"changed\n");
    let none = vec![no_diff(MEMORY, NoDiff::NoEarlierCopy)];

    let forged = good.replace("trusted", "harmless");
    assert_ne!(forged, good);
    std::fs::write(&copy, &forged).unwrap();
    assert_eq!(texts_of(&fx, &root), none, "another text under the path");

    for bad in [
        "not json".to_string(),
        good.replace("\"version\":1", "\"version\":2"),
        good.replace(root.to_str().unwrap(), "/elsewhere"),
        good.replacen('{', "{\"profile\":\"codex\",", 1),
        format!("{}{good}", " ".repeat(MAX_KEPT_BYTES as usize)),
    ] {
        assert_ne!(bad, good);
        std::fs::write(&copy, &bad).unwrap();
        assert_eq!(texts_of(&fx, &root), none, "{}", &bad[..bad.len().min(60)]);
    }

    std::fs::remove_file(&copy).unwrap();
    let elsewhere = fx.outer.join("planted");
    std::fs::write(&elsewhere, &good).unwrap();
    symlink(&elsewhere, &copy).unwrap();
    assert_eq!(texts_of(&fx, &root), none, "a link is not read");

    std::fs::remove_file(&copy).unwrap();
    mkfifo(&copy);
    assert_eq!(texts_of(&fx, &root), none, "a FIFO is not waited on");
}

/// Nothing about the copy can make a changed configuration read as trusted, or an unchanged one as
/// anything else.
#[test]
fn the_copy_never_decides() {
    let fx = Fixture::new("decides");
    let root = trusted_memory(&fx, "p", b"trusted\n");
    let copy = copy_file(&fx, &root);
    let good = std::fs::read_to_string(&copy).unwrap();
    std::fs::write(&copy, good.replace("trusted", "other")).unwrap();
    assert!(matches!(
        fx.store().state(fx.discover(&root)),
        TrustState::Trusted { .. }
    ));

    write(&root.join(MEMORY), b"other\n");
    assert!(matches!(
        fx.store().state(fx.discover(&root)),
        TrustState::Changed { .. }
    ));
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(fx.record_file(&root)).unwrap()).unwrap();
    assert_eq!(record["disclosure"], DISCLOSURE);
    assert_eq!(
        DISCLOSURE, 5,
        "a diff at a question that is asked anyway asks nothing new"
    );
    assert!(record.get("texts").is_none() && !record.to_string().contains("trusted\\n"));
}

#[test]
fn a_file_past_a_bound_is_marked_and_never_cut() {
    let fx = Fixture::new("bounds");
    let big = vec![b'x'; MAX_KEPT_FILE_BYTES as usize + 1];
    let exactly = vec![b'x'; MAX_KEPT_FILE_BYTES as usize];

    // Over the bound when it was trusted.
    let root = trusted_memory(&fx, "was", &big);
    write(&root.join(MEMORY), b"short\n");
    assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::WasTooLong)]);
    assert!(std::fs::metadata(copy_file(&fx, &root)).unwrap().len() < 4096);

    // Over the bound now.
    let root = trusted_memory(&fx, "now", b"short\n");
    write(&root.join(MEMORY), &big);
    assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::TooLong)]);

    // At the bound it is kept whole.
    let root = trusted_memory(&fx, "at", &exactly);
    write(&root.join(MEMORY), b"short\n");
    assert!(matches!(&texts_of(&fx, &root)[..], [ChangedText::Diff { before, .. }] if before.len() == exactly.len()));

    // The whole copy: files in path order until no more fit; the file on disk stays under the bound.
    let root = fx.root("many");
    let files: Vec<String> = (0..6).map(|i| format!(".claude/rules/f{i}.md")).collect();
    for file in &files {
        write(&root.join(file), &vec![b'y'; 200 << 10]);
    }
    fx.store().record(&fx.discover(&root), 1).unwrap();
    assert!(std::fs::metadata(copy_file(&fx, &root)).unwrap().len() <= MAX_KEPT_BYTES);
    for file in &files {
        write(&root.join(file), b"short\n");
    }
    let texts = texts_of(&fx, &root);
    assert_eq!(texts.len(), 6);
    let shown = texts.iter().filter(|t| matches!(t, ChangedText::Diff { .. })).count();
    assert_eq!(shown, 5, "{}", texts.len());
    assert_eq!(texts[5], no_diff(&files[5], NoDiff::NoRoom));

    // Text that takes more room as JSON is measured as written.
    let root = fx.root("escaped");
    for file in &files {
        write(&root.join(file), &vec![0x01; 100 << 10]);
    }
    fx.store().record(&fx.discover(&root), 1).unwrap();
    assert!(std::fs::metadata(copy_file(&fx, &root)).unwrap().len() <= MAX_KEPT_BYTES);
}

#[test]
fn a_binary_file_has_a_line_and_no_text() {
    let fx = Fixture::new("binary");
    for (name, bytes) in [("nul", &b"a\0b\n"[..]), ("latin", &b"caf\xe9\n"[..])] {
        let root = trusted_memory(&fx, &format!("was-{name}"), bytes);
        write(&root.join(MEMORY), b"text\n");
        assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::WasNotText)]);

        let root = trusted_memory(&fx, &format!("now-{name}"), b"text\n");
        write(&root.join(MEMORY), bytes);
        assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::NotText)]);
    }
}

#[test]
fn a_link_has_a_line_and_no_text() {
    let fx = Fixture::new("link");
    let root = fx.repo("p");
    write(&root.join("AGENTS.md"), b"linked old\n");
    symlink("AGENTS.md", root.join(MEMORY)).unwrap();
    fx.store().record(&fx.discover(&root), 1).unwrap();
    write(&root.join("AGENTS.md"), b"linked new\n");
    assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::Link)]);

    // A file that became a link says so; what it was still reads as what was trusted.
    let root = fx.repo("q");
    write(&root.join(MEMORY), b"plain\n");
    write(&root.join("AGENTS.md"), b"plain\n");
    fx.store().record(&fx.discover(&root), 1).unwrap();
    std::fs::remove_file(root.join(MEMORY)).unwrap();
    symlink("AGENTS.md", root.join(MEMORY)).unwrap();
    assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::Link)]);
}

#[test]
fn a_file_below_a_linked_directory_has_a_line_and_no_text() {
    let fx = Fixture::new("linked-dir");
    let root = fx.repo("p");
    write(&root.join("shared/style.md"), b"old\n");
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    symlink("../shared", root.join(".claude/rules")).unwrap();
    let found = fx.discover(&root);
    assert_eq!(found.instructions[Path::new(".claude/rules/style.md")], Kept::Link);
    fx.store().record(&found, 1).unwrap();
    write(&root.join("shared/style.md"), b"new\n");
    assert_eq!(
        texts_of(&fx, &root),
        vec![no_diff(".claude/rules/style.md", NoDiff::Link)]
    );
}

/// The record's keys are lossy, so `a\xff.md` and a file really named `a\u{fffd}.md` share one. The
/// question must not draw one file's text under the other's change.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses file names that are not UTF-8; this test needs one"
)]
fn a_file_name_that_is_not_utf8_never_lends_its_key_to_a_diff() {
    let fx = Fixture::new("lossy");
    let root = fx.root("p");
    let plain = ".claude/rules/a\u{fffd}.md";
    let raw = root.join(OsStr::from_bytes(b".claude/rules/a\xff.md"));
    write(&root.join(plain), b"safe\n");
    write(&raw, b"safe\n");
    fx.store().record(&fx.discover(&root), 1).unwrap();
    write(&raw, b"changed instructions\n");
    assert_eq!(texts_of(&fx, &root), vec![no_diff(plain, NoDiff::NotText)]);
}

#[test]
fn a_link_that_became_a_file_says_it_was_one() {
    let fx = Fixture::new("was-link");
    let root = fx.repo("p");
    write(&root.join("AGENTS.md"), b"linked\n");
    symlink("AGENTS.md", root.join(MEMORY)).unwrap();
    fx.store().record(&fx.discover(&root), 1).unwrap();
    std::fs::remove_file(root.join(MEMORY)).unwrap();
    write(&root.join(MEMORY), b"a file now\n");
    assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::WasLink)]);
}

/// Each reason in a person's words, the two about the past saying so.
#[test]
fn every_plain_line_says_what_is_true_of_the_file() {
    let lines = [
        (NoDiff::NoEarlierCopy, "no earlier copy to compare with"),
        (NoDiff::TooLong, "over the 256 KiB kept of a file, so no diff"),
        (NoDiff::WasTooLong, "was over the 256 KiB kept of a file, so no diff"),
        (NoDiff::NotText, "not text, so no diff"),
        (NoDiff::WasNotText, "was not text, so no diff"),
        (NoDiff::Link, "a link, so no diff"),
        (NoDiff::WasLink, "was a link, so no diff"),
        (
            NoDiff::NoRoom,
            "did not fit in the 1 MiB kept for a project, so no diff",
        ),
    ];
    for (why, line) in lines {
        assert_eq!(why.line(), line);
    }
}

/// The lines follow the paths, whichever list each file came from.
#[test]
fn the_texts_are_in_path_order() {
    let fx = Fixture::new("order");
    let root = fx.root("p");
    write(&root.join(".claude/CLAUDE.md"), b"old\n");
    fx.store().record(&fx.discover(&root), 1).unwrap();
    write(&root.join(".claude/CLAUDE.md"), b"new\n");
    write(&root.join("CLAUDE.md"), b"added\n");
    write(&root.join("CLAUDE.local.md"), b"added\n");
    let texts = texts_of(&fx, &root);
    let paths: Vec<_> = texts
        .iter()
        .map(|text| match text {
            ChangedText::Diff { path, .. } | ChangedText::NoDiff { path, .. } => path.to_str().unwrap(),
        })
        .collect();
    assert_eq!(paths, [".claude/CLAUDE.md", "CLAUDE.local.md", "CLAUDE.md"]);
}

/// A name that is not UTF-8, alone, reaches the question: `diff` holds its lossy key.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses file names that are not UTF-8; this test needs one"
)]
fn a_file_name_that_is_not_utf8_has_a_line() {
    let fx = Fixture::new("lossy-alone");
    let root = fx.root("p");
    let raw = root.join(OsStr::from_bytes(b".claude/rules/a\xff.md"));
    write(&raw, b"old\n");
    fx.store().record(&fx.discover(&root), 1).unwrap();
    write(&raw, b"new\n");
    assert_eq!(
        texts_of(&fx, &root),
        vec![no_diff(".claude/rules/a\u{fffd}.md", NoDiff::NotText)]
    );
}

#[test]
fn a_new_file_has_a_line_and_a_removed_file_only_its_row() {
    let fx = Fixture::new("added");
    let root = trusted_memory(&fx, "p", b"old\n");
    write(&root.join("CLAUDE.local.md"), b"mine\n");
    std::fs::remove_file(root.join(MEMORY)).unwrap();
    write(&root.join(SETTINGS), &hook("touch /tmp/a"));
    match fx.store().state(fx.discover(&root)) {
        TrustState::Changed { diff, texts, .. } => {
            assert_eq!(diff.removed, vec![PathBuf::from(MEMORY)]);
            assert_eq!(diff.added.len(), 2, "{diff:?}");
            assert_eq!(texts, vec![no_diff("CLAUDE.local.md", NoDiff::NoEarlierCopy)]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unread_file_says_what_its_row_says() {
    let fx = Fixture::new("unread");
    let root = trusted_memory(&fx, "p", b"old\n");
    write(&root.join(MEMORY), &vec![b'x'; 5 << 20]);
    match fx.store().state(fx.discover(&root)) {
        TrustState::Changed { remember, texts, .. } => {
            assert!(matches!(remember, Remember::SessionOnly(_)), "{remember:?}");
            assert!(texts.is_empty(), "{texts:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_copy_that_cannot_be_written_is_not_a_failed_record() {
    let fx = Fixture::new("copy-fails");
    let root = fx.root("p");
    write(&root.join(MEMORY), b"old\n");
    let copy = copy_file(&fx, &root);
    std::fs::create_dir_all(copy.join("in-the-way")).unwrap();
    std::fs::set_permissions(fx.dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    fx.store().record(&fx.discover(&root), 1).unwrap();
    assert!(matches!(
        fx.store().state(fx.discover(&root)),
        TrustState::Trusted { .. }
    ));
    write(&root.join(MEMORY), b"new\n");
    assert_eq!(texts_of(&fx, &root), vec![no_diff(MEMORY, NoDiff::NoEarlierCopy)]);
}

#[test]
fn a_session_only_answer_keeps_no_copy() {
    let fx = Fixture::new("session");
    let root = fx.root("p");
    write(&root.join(MEMORY), b"mine\n");
    write(&root.join(SETTINGS), b"not json");
    let found = fx.discover(&root);
    assert!(!found.fully_hashed());
    assert!(fx.store().record(&found, 1).is_err());
    assert!(!copy_file(&fx, &root).exists());
}

#[test]
fn forget_takes_the_copy_with_the_record() {
    let fx = Fixture::new("forget-copy");
    let root = trusted_memory(&fx, "p", b"old\n");
    let other = trusted_memory(&fx, "q", b"old\n");
    let store = fx.store();
    let copy = copy_file(&fx, &root);
    assert!(copy.exists());
    assert!(store.forget(&root, &Profile::Claude).unwrap());
    assert!(!copy.exists());
    assert!(copy_file(&fx, &other).exists());

    // A copy left with no record goes too, and is no record to report.
    std::fs::write(&copy, b"left over").unwrap();
    assert!(!store.forget(&root, &Profile::Claude).unwrap());
    assert!(!copy.exists());

    // A link in its place is left, and named; the record still goes.
    store.record(&fx.discover(&root), 1).unwrap();
    std::fs::remove_file(&copy).unwrap();
    let target = fx.outer.join("target");
    write(&target, b"keep");
    symlink(&target, &copy).unwrap();
    let err = store.forget(&root, &Profile::Claude).unwrap_err();
    assert!(
        err.to_string().contains("copy of the instruction files") && err.to_string().contains("a link"),
        "{err}"
    );
    assert!(!fx.record_file(&root).exists());
    assert!(target.exists());
    assert!(matches!(store.state(fx.discover(&root)), TrustState::Untrusted { .. }));

    // A record that cannot be removed does not keep the copy.
    std::fs::remove_file(&copy).unwrap();
    std::fs::write(&copy, b"left over").unwrap();
    mkfifo(&fx.record_file(&root));
    assert!(store.forget(&root, &Profile::Claude).is_err());
    assert!(!copy.exists());
}

#[test]
fn a_record_written_before_copies_existed_still_answers() {
    let fx = Fixture::new("old-record");
    let root = fx.root("p");
    write(&root.join(MEMORY), b"old\n");
    let found = fx.discover(&root);
    let store = fx.store();
    store.record(&found, 1).unwrap();
    std::fs::remove_file(copy_file(&fx, &root)).unwrap();
    let text = std::fs::read_to_string(fx.record_file(&root)).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["disclosure", "files", "fingerprint", "root", "trusted_at", "version"].into(),
        "the record's own shape did not move"
    );
    assert!(matches!(store.state(fx.discover(&root)), TrustState::Trusted { .. }));
}
