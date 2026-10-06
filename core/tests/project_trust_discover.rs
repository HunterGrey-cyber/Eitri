//! Discovery of a project's own Claude configuration, on real files in a temporary directory.
//!
//! Every file a test inspects is made here, under a scratch directory with a fake home inside it;
//! no test points discovery at an existing project or at the real home. `git` runs only to make
//! fixtures, with no user or system configuration.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use eitri_core::project_trust::{discover, git_top_level, Discovery, EntryKind, Finding, Limits};

/// A scratch directory (canonical) holding a fake home; removed on drop.
struct Fixture {
    outer: PathBuf,
    base: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        // macOS's `TMPDIR` (canonicalized, `/private/var/...`) is ~57 bytes and a socket in the tree may
        // not pass 103, so there the directory is named by a short id alone.
        #[cfg(target_os = "macos")]
        let outer = std::env::temp_dir().join(&uuid::Uuid::new_v4().simple().to_string()[..12]);
        #[cfg(not(target_os = "macos"))]
        let outer = std::env::temp_dir().join(format!("eitri-trust-it-{label}-{}", uuid::Uuid::new_v4()));
        #[cfg(target_os = "macos")]
        let _ = label;
        std::fs::create_dir_all(&outer).unwrap();
        let base = outer.canonicalize().unwrap();
        let home = base.join("home");
        std::fs::create_dir(&home).unwrap();
        Fixture { outer, base, home }
    }

    fn discover(&self, root: &Path) -> Discovery {
        discover(root, Some(&self.home), &Limits::default())
    }

    /// Discovery on another thread, which must finish within two seconds.
    fn discover_promptly(&self, root: &Path) -> Discovery {
        let (tx, rx) = mpsc::channel();
        let root = root.to_path_buf();
        let home = self.home.clone();
        std::thread::spawn(move || {
            let _ = tx.send(discover(&root, Some(&home), &Limits::default()));
        });
        rx.recv_timeout(Duration::from_secs(2)).expect("discovery blocked")
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_CEILING_DIRECTORIES")
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "protocol.file.allow=always",
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
        String::from_utf8(out.stdout).unwrap().trim_end().to_string()
    }

    /// A repository at `dir` (created), with one commit when `commit`.
    fn repo(&self, dir: &Path, commit: bool) {
        std::fs::create_dir_all(dir).unwrap();
        self.git(dir, &["init", "-q"]);
        if commit {
            std::fs::write(dir.join("README.md"), b"readme\n").unwrap();
            self.git(dir, &["add", "README.md"]);
            self.git(dir, &["commit", "-q", "-m", "first"]);
        }
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
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path alive for the call.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

fn paths(discovery: &Discovery) -> Vec<PathBuf> {
    discovery.entries.iter().map(|e| e.path.clone()).collect()
}

fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// An inotify watch for opens and reads of one file, so a test can show discovery never opened it.
#[cfg(target_os = "linux")]
struct OpenWatch {
    fd: libc::c_int,
}

#[cfg(target_os = "linux")]
impl OpenWatch {
    fn new(path: &Path) -> OpenWatch {
        // SAFETY: `inotify_init1` has no pointer arguments.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fd` is an inotify descriptor and `c` a valid NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(fd, c.as_ptr(), libc::IN_OPEN | libc::IN_ACCESS) };
        assert!(wd >= 0, "inotify_add_watch {}", path.display());
        OpenWatch { fd }
    }

    /// Whether any open or read was seen since the watch began.
    fn saw_anything(&self) -> bool {
        let mut buf = [0u8; 4096];
        // SAFETY: `fd` is open and `buf` writable for its length.
        let got = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let error = std::io::Error::last_os_error();
            assert_eq!(error.raw_os_error(), Some(libc::EAGAIN), "{error}");
            return false;
        }
        got > 0
    }

    const WORKS: bool = true;
}

#[cfg(target_os = "linux")]
impl Drop for OpenWatch {
    fn drop(&mut self) {
        // SAFETY: `fd` is ours and closed once.
        unsafe { libc::close(self.fd) };
    }
}

/// Without inotify the open itself cannot be watched; the findings are still checked.
#[cfg(not(target_os = "linux"))]
struct OpenWatch;

#[cfg(not(target_os = "linux"))]
impl OpenWatch {
    fn new(_path: &Path) -> OpenWatch {
        OpenWatch
    }

    fn saw_anything(&self) -> bool {
        false
    }

    const WORKS: bool = false;
}

const HOOK_SETTINGS: &[u8] =
    br#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"touch /tmp/marker"}]}]}}"#;

#[test]
fn nothing_on_the_path_is_nothing_to_trust() {
    let fx = Fixture::new("nothing");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join("src/main.rs"), b"fn main() {}\n");
    write(&repo.join("README.md"), b"hi\n");
    let found = fx.discover(&repo);
    assert!(found.entries.is_empty(), "{:?}", found.entries);
    assert!(found.nothing_to_trust());
    assert!(found.fully_hashed());
    assert_eq!(found.top, repo);
    assert_eq!(found.walked, vec![repo.clone()]);
}

#[test]
fn opening_src_finds_the_repo_top_level_and_its_local_settings() {
    let fx = Fixture::new("src");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".claude/settings.local.json"), HOOK_SETTINGS);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let found = fx.discover(&repo.join("src"));
    assert_eq!(found.top, repo);
    assert_eq!(found.walked, vec![repo.join("src"), repo.clone()]);
    assert_eq!(paths(&found), vec![PathBuf::from(".claude/settings.local.json")]);
    assert!(found.findings.contains(&Finding::Hook {
        file: PathBuf::from(".claude/settings.local.json"),
        event: "SessionStart".into(),
        matcher: None,
        command: "touch /tmp/marker".into(),
    }));
    assert!(found.fully_hashed());
}

#[test]
fn git_top_level_matches_git() {
    let fx = Fixture::new("matches");
    // A plain repository, opened in a subdirectory.
    let plain = fx.home.join("plain");
    fx.repo(&plain, false);
    std::fs::create_dir_all(plain.join("a/b")).unwrap();
    let root = plain.join("a/b");
    let want = PathBuf::from(fx.git(&root, &["rev-parse", "--show-toplevel"]));
    assert_eq!(git_top_level(&root, Some(&fx.home), 64).top, Some(want));

    // A linked worktree: a gitfile, and a `commondir` in the git directory it names.
    let main = fx.home.join("main");
    fx.repo(&main, true);
    let wt = fx.home.join("wt");
    fx.git(&main, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
    assert!(wt.join(".git").is_file());
    std::fs::create_dir_all(wt.join("inner")).unwrap();
    let root = wt.join("inner");
    let want = PathBuf::from(fx.git(&root, &["rev-parse", "--show-toplevel"]));
    assert_eq!(want, wt);
    let walk = git_top_level(&root, Some(&fx.home), 64);
    assert_eq!(walk.top, Some(want));
    assert!(walk.findings.is_empty(), "{:?}", walk.findings);

    // A submodule: a gitfile naming `../.git/modules/<name>` relative to itself.
    let sub_src = fx.home.join("sub-src");
    fx.repo(&sub_src, true);
    let superproject = fx.home.join("super");
    fx.repo(&superproject, true);
    fx.git(
        &superproject,
        &["submodule", "add", "-q", sub_src.to_str().unwrap(), "sub"],
    );
    let root = superproject.join("sub");
    assert!(root.join(".git").is_file());
    let want = PathBuf::from(fx.git(&root, &["rev-parse", "--show-toplevel"]));
    assert_eq!(want, root);
    let walk = git_top_level(&root, Some(&fx.home), 64);
    assert_eq!(walk.top, Some(want));
    assert!(walk.findings.is_empty(), "{:?}", walk.findings);
}

#[test]
fn the_walk_stops_at_the_git_top_level() {
    let fx = Fixture::new("stops-top");
    let outer = fx.home.join("outer");
    write(&outer.join(".claude/settings.json"), HOOK_SETTINGS);
    let repo = outer.join("repo");
    fx.repo(&repo, false);
    let found = fx.discover(&repo);
    assert_eq!(found.top, repo);
    assert!(found.entries.is_empty(), "{:?}", found.entries);
}

#[test]
fn a_git_dir_git_would_skip_does_not_end_the_walk() {
    let fx = Fixture::new("skip");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".claude/settings.json"), HOOK_SETTINGS);
    std::fs::create_dir_all(repo.join("src/.git")).unwrap();
    let found = fx.discover(&repo.join("src"));
    assert_eq!(found.top, repo);
    assert_eq!(paths(&found), vec![PathBuf::from(".claude/settings.json")]);
    assert!(
        found
            .findings
            .iter()
            .any(|f| matches!(f, Finding::GitUnverified { path, .. } if path == Path::new("src/.git/HEAD"))),
        "{:?}",
        found.findings
    );
}

#[test]
fn without_a_repository_only_the_root_is_read() {
    let fx = Fixture::new("norepo");
    let parent = fx.home.join("p");
    write(&parent.join(".claude/settings.json"), HOOK_SETTINGS);
    let root = parent.join("q");
    write(&root.join(".mcp.json"), br#"{"mcpServers":{"m":{"command":"srv"}}}"#);
    let found = fx.discover(&root);
    assert_eq!(found.top, root);
    assert_eq!(found.walked, vec![root.clone()]);
    assert_eq!(paths(&found), vec![PathBuf::from(".mcp.json")]);
    assert!(found
        .findings
        .iter()
        .any(|f| matches!(f, Finding::McpServer { name, command_line, .. }
        if name == "m" && command_line == "srv")));
}

/// A fake home that is itself a repository (dotfiles), with the user tier's own files in it.
fn dotfiles_home(fx: &Fixture) -> PathBuf {
    fx.git(&fx.home, &["init", "-q"]);
    write(&fx.home.join(".claude/projects/x.jsonl"), b"{}\n");
    write(&fx.home.join(".claude/settings.json"), HOOK_SETTINGS);
    write(&fx.home.join(".claude/settings.local.json"), HOOK_SETTINGS);
    // Above the home, so never looked at.
    write(&fx.base.join(".claude/settings.json"), HOOK_SETTINGS);
    let root = fx.home.join("p");
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn the_walk_stops_at_home() {
    let fx = Fixture::new("home-stop");
    let root = dotfiles_home(&fx);
    let found = fx.discover(&root);
    assert_eq!(found.top, fx.home);
    assert_eq!(found.walked, vec![root, fx.home.clone()]);
    assert!(paths(&found).iter().all(|p| !p.starts_with("..")));

    // Not a repository: the root alone, and still nothing above the home.
    let fx = Fixture::new("home-stop-norepo");
    write(&fx.base.join(".git/HEAD"), b"ref: refs/heads/main\n");
    std::fs::create_dir_all(fx.base.join(".git/objects")).unwrap();
    std::fs::create_dir_all(fx.base.join(".git/refs")).unwrap();
    let root = fx.home.join("p");
    std::fs::create_dir_all(&root).unwrap();
    let found = fx.discover(&root);
    assert_eq!(found.top, root);
}

#[test]
fn home_dot_claude_contributes_only_settings_local() {
    let fx = Fixture::new("home-local");
    let root = dotfiles_home(&fx);
    let found = fx.discover(&root);
    assert_eq!(paths(&found), vec![PathBuf::from(".claude/settings.local.json")]);
}

/// A repository with a settings file and a hook script under `.claude/`.
fn configured_repo(fx: &Fixture) -> PathBuf {
    let repo = fx.home.join("repo");
    fx.repo(&repo, true);
    write(&repo.join(".claude/settings.json"), HOOK_SETTINGS);
    write(&repo.join(".claude/hooks/run.sh"), b"#!/bin/sh\necho hi\n");
    write(&repo.join("src/main.rs"), b"fn main() {}\n");
    repo
}

#[test]
fn every_byte_of_a_settings_file_counts() {
    let fx = Fixture::new("bytes");
    let repo = configured_repo(&fx);
    let before = fx.discover(&repo).fingerprint;
    assert_eq!(fx.discover(&repo).fingerprint, before, "the fingerprint is stable");
    let mut bytes = std::fs::read(repo.join(".claude/settings.json")).unwrap();
    let last = bytes.len() - 3;
    bytes[last] ^= 0x01;
    std::fs::write(repo.join(".claude/settings.json"), &bytes).unwrap();
    assert_ne!(fx.discover(&repo).fingerprint, before);
}

#[test]
fn a_file_under_dot_claude_counts() {
    let fx = Fixture::new("under");
    let repo = configured_repo(&fx);
    let first = fx.discover(&repo);
    assert!(paths(&first).contains(&PathBuf::from(".claude/hooks")));
    assert!(paths(&first).contains(&PathBuf::from(".claude/hooks/run.sh")));
    assert!(first.findings.contains(&Finding::OtherFiles {
        dir: PathBuf::from(".claude/hooks"),
        count: 1
    }));

    std::fs::write(repo.join(".claude/hooks/run.sh"), b"#!/bin/sh\necho bye\n").unwrap();
    let edited = fx.discover(&repo).fingerprint;
    assert_ne!(edited, first.fingerprint);

    std::fs::rename(repo.join(".claude/hooks/run.sh"), repo.join(".claude/hooks/go.sh")).unwrap();
    let renamed = fx.discover(&repo).fingerprint;
    assert_ne!(renamed, edited);

    write(&repo.join(".claude/agents/a.md"), b"agent\n");
    assert_ne!(fx.discover(&repo).fingerprint, renamed);
}

#[test]
fn a_symlink_is_reported_by_target_and_never_followed() {
    let fx = Fixture::new("symlink");
    let repo = configured_repo(&fx);
    let elsewhere = fx.base.join("elsewhere");
    write(&elsewhere.join("run"), b"one\n");
    write(&elsewhere.join("other"), b"two\n");
    symlink(elsewhere.join("run"), repo.join(".claude/hooks/run")).unwrap();
    let found = fx.discover(&repo);
    let entry = found
        .entries
        .iter()
        .find(|e| e.path == Path::new(".claude/hooks/run"))
        .unwrap();
    assert_eq!(
        entry.kind,
        EntryKind::Symlink {
            target: elsewhere.join("run"),
            outside: true
        }
    );
    assert!(found.findings.contains(&Finding::Symlink {
        path: PathBuf::from(".claude/hooks/run"),
        target: elsewhere.join("run"),
        outside: true,
    }));
    // A hook script that is a link is fingerprinted by its target, not unhashed.
    assert!(found.fully_hashed());

    // The target's bytes are never read.
    std::fs::write(elsewhere.join("run"), b"changed\n").unwrap();
    assert_eq!(fx.discover(&repo).fingerprint, found.fingerprint);

    // Retargeting the link is a change.
    std::fs::remove_file(repo.join(".claude/hooks/run")).unwrap();
    symlink(elsewhere.join("other"), repo.join(".claude/hooks/run")).unwrap();
    assert_ne!(fx.discover(&repo).fingerprint, found.fingerprint);

    // A link that stays inside the top level is not outside.
    std::fs::remove_file(repo.join(".claude/hooks/run")).unwrap();
    symlink("run.sh", repo.join(".claude/hooks/run")).unwrap();
    let inside = fx.discover(&repo);
    assert!(inside.entries.iter().any(|e| e.path == Path::new(".claude/hooks/run")
        && e.kind
            == EntryKind::Symlink {
                target: PathBuf::from("run.sh"),
                outside: false
            }));

    // `.claude` itself a link: one entry, nothing below it.
    let other = fx.home.join("other");
    fx.repo(&other, false);
    let real = fx.base.join("real-claude");
    write(&real.join("settings.json"), HOOK_SETTINGS);
    symlink(&real, other.join(".claude")).unwrap();
    let found = fx.discover(&other);
    assert_eq!(found.entries.len(), 1, "{:?}", found.entries);
    assert_eq!(
        found.entries[0].kind,
        EntryKind::Symlink {
            target: real.clone(),
            outside: true
        }
    );
    assert!(!found.findings.iter().any(|f| matches!(f, Finding::Hook { .. })));
}

#[test]
fn a_symlinked_configuration_file_is_not_fully_hashed() {
    let fx = Fixture::new("cfg-link");
    for name in [
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".mcp.json",
        "CLAUDE.md",
        "CLAUDE.local.md",
    ] {
        let repo = fx.home.join(format!("repo-{}", name.replace(['/', '.'], "_")));
        fx.repo(&repo, false);
        let target = fx.base.join(format!("target-{}", name.replace(['/', '.'], "_")));
        write(&target, b"{}");
        std::fs::create_dir_all(repo.join(name).parent().unwrap()).unwrap();
        symlink(&target, repo.join(name)).unwrap();
        let found = fx.discover(&repo);
        assert!(!found.fully_hashed(), "{name}");
        let reason = format!("a link to {}", target.display());
        assert!(
            found.uncheckable().contains(&(Path::new(name), reason.as_str())),
            "{name}: {:?}",
            found.uncheckable()
        );
    }
}

#[test]
fn a_change_outside_the_configuration_keeps_the_fingerprint() {
    let fx = Fixture::new("outside");
    let repo = configured_repo(&fx);
    let before = fx.discover(&repo).fingerprint;
    std::fs::write(repo.join("src/main.rs"), b"fn main() { println!(); }\n").unwrap();
    std::fs::write(repo.join("README.md"), b"other\n").unwrap();
    std::fs::write(repo.join(".git/HEAD"), b"ref: refs/heads/other\n").unwrap();
    let after = fx.discover(&repo);
    assert_eq!(after.top, repo);
    assert_eq!(after.fingerprint, before);
}

#[test]
fn a_fifo_or_device_never_blocks() {
    let fx = Fixture::new("fifo");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    mkfifo(&repo.join(".claude/settings.json"));
    let found = fx.discover_promptly(&repo);
    let entry = &found.entries[0];
    assert_eq!(entry.path, Path::new(".claude/settings.json"));
    assert_eq!(
        entry.kind,
        EntryKind::Unreadable {
            reason: "a FIFO".into()
        }
    );
    assert!(!found.fully_hashed());

    // Only root can make a device node; the number is Linux's /dev/null.
    #[cfg(target_os = "linux")]
    if is_root() {
        let dev = repo.join(".claude/dev");
        let c = CString::new(dev.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path; 1:3 is /dev/null's device number.
        assert_eq!(
            unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | 0o600, libc::makedev(1, 3)) },
            0
        );
        let found = fx.discover_promptly(&repo);
        assert!(found.uncheckable().contains(&(Path::new(".claude/dev"), "a device")));
    }
}

#[test]
fn a_fifo_at_git_commondir_never_blocks() {
    let fx = Fixture::new("commondir-fifo");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    mkfifo(&repo.join(".git/commondir"));
    let found = fx.discover_promptly(&repo);
    assert!(
        found.findings.contains(&Finding::GitUnverified {
            path: PathBuf::from(".git/commondir"),
            reason: "a FIFO, not read".into(),
            target: None,
        }),
        "{:?}",
        found.findings
    );
    assert_eq!(git_top_level(&repo, Some(&fx.home), 64).top, None);
    // Opened below it, the walk still reaches the repository it could not check: git might take it,
    // and the local tier beside it would then load.
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let found = fx.discover_promptly(&repo.join("src"));
    assert_eq!(found.top, repo);
    assert_eq!(found.walked, vec![repo.join("src"), repo.clone()]);
}

#[test]
fn a_dot_git_link_git_accepts_still_reaches_its_local_settings() {
    let fx = Fixture::new("dotgit-link");
    let real = fx.home.join("real");
    fx.repo(&real, true);
    let repo = fx.home.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    symlink(real.join(".git"), repo.join(".git")).unwrap();
    write(&repo.join(".claude/settings.local.json"), HOOK_SETTINGS);
    write(&repo.join("src/.claude/settings.json"), br#"{"env":{"A":"1"}}"#);
    let src = repo.join("src");
    // git itself follows the link and takes the directory holding it as the top level.
    assert_eq!(PathBuf::from(fx.git(&src, &["rev-parse", "--show-toplevel"])), repo);

    let found = fx.discover(&src);
    assert_eq!(found.top, repo);
    assert_eq!(found.walked, vec![src.clone(), repo.clone()]);
    assert_eq!(
        paths(&found),
        vec![
            PathBuf::from(".claude/settings.local.json"),
            PathBuf::from("src/.claude/settings.json"),
        ]
    );
    assert!(found.findings.contains(&Finding::Hook {
        file: PathBuf::from(".claude/settings.local.json"),
        event: "SessionStart".into(),
        matcher: None,
        command: "touch /tmp/marker".into(),
    }));
    assert!(found.findings.contains(&Finding::GitUnverified {
        path: PathBuf::from(".git"),
        reason: "a symlink, not followed".into(),
        target: Some(real.join(".git")),
    }));
    // Every file the CLI could load is hashed, wherever git puts the top level.
    assert!(found.fully_hashed());

    // The hook git's top level holds is part of the fingerprint.
    let before = found.fingerprint;
    write(
        &repo.join(".claude/settings.local.json"),
        br#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"touch /tmp/other"}]}]}}"#,
    );
    assert_ne!(fx.discover(&src).fingerprint, before);
}

/// Gives a directory back its owner's permissions when the test ends, so the fixture can be removed.
struct RestoreMode(PathBuf);

impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

#[test]
fn a_search_only_ancestor_does_not_hide_the_configuration() {
    if is_root() {
        // Permissions do not hold root back; nothing to check.
        return;
    }
    let fx = Fixture::new("search-only");
    let locked = fx.home.join("locked");
    let repo = locked.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".claude/settings.json"), HOOK_SETTINGS);
    let _restore = RestoreMode(locked.clone());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o100)).unwrap();
    let found = fx.discover_promptly(&repo);
    assert_eq!(found.top, repo);
    assert_eq!(paths(&found), vec![PathBuf::from(".claude/settings.json")]);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
}

#[test]
fn a_commondir_symlink_to_claude_json_is_reported_and_never_opened() {
    let fx = Fixture::new("commondir-link");
    let secret = fx.home.join(".claude.json");
    write(&secret, br#"{"projects":{}}"#);
    // A git directory whose HEAD is the same file under another name: reading it there would open
    // the watched inode.
    std::fs::create_dir_all(fx.home.join("fakegit/objects")).unwrap();
    std::fs::create_dir_all(fx.home.join("fakegit/refs")).unwrap();
    std::fs::hard_link(&secret, fx.home.join("fakegit/HEAD")).unwrap();

    // `.git/commondir` is a link to it.
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    symlink(&secret, repo.join(".git/commondir")).unwrap();
    // `.git` itself is a link to it.
    let repo2 = fx.home.join("repo2");
    std::fs::create_dir_all(&repo2).unwrap();
    symlink(&secret, repo2.join(".git")).unwrap();
    // A gitfile whose path passes through a symlinked directory to the git directory above.
    let link = fx.base.join("lnk");
    symlink(&fx.home, &link).unwrap();
    let repo3 = fx.home.join("repo3");
    let named = link.join("fakegit");
    write(&repo3.join(".git"), format!("gitdir: {}\n", named.display()).as_bytes());

    let watch = OpenWatch::new(&secret);
    let found = fx.discover_promptly(&repo);
    assert!(
        found.findings.contains(&Finding::GitUnverified {
            path: PathBuf::from(".git/commondir"),
            reason: "a symlink, not followed".into(),
            target: Some(secret.clone()),
        }),
        "{:?}",
        found.findings
    );
    let found = fx.discover_promptly(&repo2);
    assert!(
        found.findings.contains(&Finding::GitUnverified {
            path: PathBuf::from(".git"),
            reason: "a symlink, not followed".into(),
            target: Some(secret.clone()),
        }),
        "{:?}",
        found.findings
    );
    let found = fx.discover_promptly(&repo3);
    assert!(
        found
            .findings
            .iter()
            .any(|f| matches!(f, Finding::GitUnverified { path, target, .. }
            if path == Path::new(".git") && target.as_deref() == Some(named.as_path()))),
        "{:?}",
        found.findings
    );
    assert_eq!(git_top_level(&repo3, Some(&fx.home), 64).top, None);
    assert!(!watch.saw_anything(), "discovery opened the file a link named");
    // The watch does see an open through the other name, so its silence above means something.
    std::fs::read(fx.home.join("fakegit/HEAD")).unwrap();
    assert_eq!(watch.saw_anything(), OpenWatch::WORKS);
}

#[test]
fn a_head_symlink_is_read_as_a_link_only() {
    let fx = Fixture::new("head-link");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    std::fs::remove_file(repo.join(".git/HEAD")).unwrap();
    symlink("refs/heads/main", repo.join(".git/HEAD")).unwrap();
    assert_eq!(git_top_level(&repo, Some(&fx.home), 64).top, Some(repo.clone()));

    let secret = fx.home.join(".claude.json");
    write(&secret, b"{}");
    std::fs::remove_file(repo.join(".git/HEAD")).unwrap();
    symlink(&secret, repo.join(".git/HEAD")).unwrap();
    let watch = OpenWatch::new(&secret);
    let walk = git_top_level(&repo, Some(&fx.home), 64);
    assert_eq!(walk.top, None);
    assert!(walk
        .findings
        .iter()
        .any(|f| matches!(f, Finding::GitUnverified { path, target, .. }
        if path == &repo.join(".git/HEAD") && target.as_deref() == Some(secret.as_path()))));
    fx.discover_promptly(&repo);
    assert!(!watch.saw_anything(), "discovery opened the file HEAD links to");
}

#[test]
fn project_trust_touches_the_disk_only_through_nofollow() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/project_trust");
    let forbidden = [
        "File::open",
        "File::create",
        "OpenOptions",
        "fs::",
        "read_to_string",
        "read_dir",
        "metadata(",
        "symlink_metadata",
        "canonicalize",
        "read_link",
        ".exists(",
        ".try_exists(",
        ".is_dir(",
        ".is_file(",
        ".is_symlink(",
        "Command::new",
        "libc::",
        "permission_policy",
    ];
    for name in ["mod.rs", "discover.rs", "findings.rs", "git_top.rs"] {
        let source = std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(source.len() > 200, "{name} is present");
        let squeezed: String = source.chars().filter(|c| !c.is_whitespace()).collect();
        // The in-file tests build their fixtures with the standard library; only what ships counts.
        let shipped = squeezed.split("#[cfg(test)]").next().unwrap_or_default();
        assert!(shipped.len() > 100, "{name} has code before its tests");
        for token in forbidden {
            assert!(!shipped.contains(token), "{name} uses `{token}`; go through nofollow");
        }
    }
}

#[test]
fn the_budget_is_kept() {
    let fx = Fixture::new("budget");
    let limits = Limits::default();

    let many = fx.home.join("many");
    fx.repo(&many, false);
    for i in 0..=limits.max_entries {
        write(&many.join(format!(".claude/lots/f{i:04}")), b"x");
    }
    let found = fx.discover(&many);
    assert!(found.over_budget.is_some());
    assert!(found.entries.len() <= limits.max_entries);
    assert!(found.findings.iter().any(|f| matches!(f, Finding::OverBudget { .. })));
    assert!(!found.fully_hashed());

    let big = fx.home.join("big");
    fx.repo(&big, false);
    write(&big.join(".claude/settings.json"), &vec![b' '; 5 << 20]);
    let found = fx.discover(&big);
    assert_eq!(
        found.entries[0].kind,
        EntryKind::Unreadable {
            reason: "over 4 MiB".into()
        }
    );
    assert!(found.over_budget.is_some());
    assert!(!found.fully_hashed());

    let mut deep = fx.home.join("deep");
    for i in 0..64 {
        deep.push(format!("d{i}"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    // 65 directories below the home: the root and 64 more before the home is reached.
    assert_eq!(deep.strip_prefix(&fx.home).unwrap().components().count(), 65);
    let found = fx.discover(&deep);
    assert_eq!(
        found.over_budget.as_deref(),
        Some("more than 64 directories above the root without a repository")
    );
    assert!(!found.fully_hashed());
}

#[test]
fn an_uncheckable_entry_is_not_fully_hashed() {
    let fx = Fixture::new("uncheckable");
    let make = |name: &str| {
        let repo = fx.home.join(name);
        fx.repo(&repo, false);
        repo
    };

    let big = make("big");
    write(&big.join(".claude/settings.json"), &vec![b' '; 5 << 20]);
    let fifo = make("fifo");
    mkfifo(&fifo.join(".claude/pipe"));
    let sock = make("sock");
    std::fs::create_dir_all(sock.join(".claude")).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(sock.join(".claude/s")).unwrap();
    let mut cases = vec![
        (big, ".claude/settings.json", "over 4 MiB"),
        (fifo, ".claude/pipe", "a FIFO"),
        (sock, ".claude/s", "a socket"),
    ];
    if !is_root() {
        let locked = make("locked");
        write(&locked.join(".claude/secret"), b"x");
        std::fs::set_permissions(locked.join(".claude/secret"), std::fs::Permissions::from_mode(0o000)).unwrap();
        cases.push((locked, ".claude/secret", "permission denied"));
    }
    for (repo, path, reason) in cases {
        let found = fx.discover_promptly(&repo);
        assert!(!found.fully_hashed(), "{path}");
        assert!(
            found.uncheckable().contains(&(Path::new(path), reason)),
            "{path}: {:?}",
            found.uncheckable()
        );
    }
}

const WORKTREES_REASON: &str = "Claude Code's own worktree checkouts, not read";

/// Strictly below the `.claude/worktrees` entry (`starts_with` alone also matches the entry).
fn below_worktrees(path: &Path) -> bool {
    path.starts_with(".claude/worktrees") && path != Path::new(".claude/worktrees")
}

fn not_read(path: &str) -> Finding {
    Finding::NotRead {
        path: PathBuf::from(path),
        reason: WORKTREES_REASON.to_string(),
    }
}

#[test]
fn claude_code_worktree_checkouts_are_named_and_not_read() {
    let fx = Fixture::new("worktrees");
    let repo = configured_repo(&fx);
    let checkout = repo.join(".claude/worktrees/feature-a");
    // Far more files than the entry budget allows: reading them would make every start ask again.
    for i in 0..3000 {
        write(&checkout.join(format!("src/f{i:04}.rs")), b"fn f() {}\n");
    }
    write(&checkout.join(".claude/settings.json"), HOOK_SETTINGS);
    let found = fx.discover(&repo);
    assert!(found.over_budget.is_none(), "{:?}", found.over_budget);
    assert!(found.fully_hashed());
    assert_eq!(
        found
            .findings
            .iter()
            .filter(|f| **f == not_read(".claude/worktrees"))
            .count(),
        1
    );
    let listed = paths(&found);
    assert!(listed.contains(&PathBuf::from(".claude/worktrees")));
    assert!(!listed.iter().any(|p| below_worktrees(p)), "{listed:?}");
    assert!(listed.contains(&PathBuf::from(".claude/settings.json")));

    write(&checkout.join("src/f0001.rs"), b"fn changed() {}\n");
    write(&checkout.join(".claude/hooks/new.sh"), b"echo\n");
    std::fs::remove_file(checkout.join("src/f0002.rs")).unwrap();
    assert_eq!(fx.discover(&repo).fingerprint, found.fingerprint);

    std::fs::write(repo.join(".claude/settings.json"), b"{}").unwrap();
    assert_ne!(fx.discover(&repo).fingerprint, found.fingerprint);
}

#[test]
fn the_worktrees_directory_appearing_or_going_is_visible_and_nothing_else() {
    let fx = Fixture::new("worktrees-name");
    let repo = configured_repo(&fx);
    let without = fx.discover(&repo);
    assert!(!without.findings.contains(&not_read(".claude/worktrees")));
    write(&repo.join(".claude/worktrees/x/a"), b"a");
    let with = fx.discover(&repo);
    assert_ne!(with.fingerprint, without.fingerprint);
    assert!(with.findings.contains(&not_read(".claude/worktrees")));
    let others = |d: &Discovery| {
        paths(d)
            .into_iter()
            .filter(|p| p != Path::new(".claude/worktrees"))
            .collect::<Vec<_>>()
    };
    assert_eq!(others(&with), others(&without));
    std::fs::remove_dir_all(repo.join(".claude/worktrees")).unwrap();
    assert_eq!(fx.discover(&repo).fingerprint, without.fingerprint);
}

#[test]
fn a_worktrees_link_is_reported_and_not_followed() {
    let fx = Fixture::new("worktrees-link");
    let repo = configured_repo(&fx);
    let elsewhere = fx.base.join("elsewhere");
    write(&elsewhere.join("a/settings.json"), HOOK_SETTINGS);
    symlink(&elsewhere, repo.join(".claude/worktrees")).unwrap();
    let found = fx.discover(&repo);
    let entry = found
        .entries
        .iter()
        .find(|e| e.path == Path::new(".claude/worktrees"))
        .expect("the link is an entry");
    assert!(matches!(&entry.kind, EntryKind::Symlink { target, .. } if *target == elsewhere));
    assert!(found
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Symlink { path, .. } if path == Path::new(".claude/worktrees"))));
    assert!(!found.findings.contains(&not_read(".claude/worktrees")));
    assert!(!paths(&found).iter().any(|p| below_worktrees(p)));
    // The content behind the link is not read, so changing it changes nothing.
    write(&elsewhere.join("a/settings.json"), b"{}");
    assert_eq!(fx.discover(&repo).fingerprint, found.fingerprint);
}

#[test]
fn only_the_worktrees_entry_directly_in_dot_claude_is_skipped() {
    let fx = Fixture::new("worktrees-near");
    let repo = configured_repo(&fx);
    write(&repo.join(".claude/worktreesX/a.md"), b"a");
    write(&repo.join(".claude/agents/worktrees/b.md"), b"b");
    write(&repo.join(".claude/hooks/worktrees/c.sh"), b"c");
    let found = fx.discover(&repo);
    let listed = paths(&found);
    for wanted in [
        ".claude/worktreesX",
        ".claude/worktreesX/a.md",
        ".claude/agents/worktrees",
        ".claude/agents/worktrees/b.md",
        ".claude/hooks/worktrees/c.sh",
    ] {
        assert!(
            listed.contains(&PathBuf::from(wanted)),
            "{wanted} missing from {listed:?}"
        );
    }
    assert!(!found.findings.iter().any(|f| matches!(f, Finding::NotRead { .. })));
    let before = found.fingerprint;
    write(&repo.join(".claude/agents/worktrees/b.md"), b"changed");
    assert_ne!(fx.discover(&repo).fingerprint, before);
}

#[test]
fn a_root_inside_a_worktree_checkout_is_discovered_on_its_own() {
    let fx = Fixture::new("worktrees-root");
    let repo = configured_repo(&fx);
    let checkout = repo.join(".claude/worktrees/wt");
    fx.git(
        &repo,
        &["worktree", "add", "-q", "-b", "wt", checkout.to_str().unwrap()],
    );
    write(&checkout.join(".claude/settings.json"), HOOK_SETTINGS);
    write(&checkout.join("src/lib.rs"), b"\n");
    let found = fx.discover(&checkout.join("src"));
    assert_eq!(found.top, checkout);
    assert_eq!(found.walked, vec![checkout.join("src"), checkout.clone()]);
    assert!(paths(&found).contains(&PathBuf::from(".claude/settings.json")));
    assert!(found.fully_hashed());
    assert!(found.findings.iter().any(|f| matches!(f, Finding::Hook { .. })));
    assert!(!found.findings.iter().any(|f| matches!(f, Finding::NotRead { .. })));
}

#[test]
fn a_repository_with_only_worktree_checkouts_has_nothing_to_trust() {
    let fx = Fixture::new("worktrees-only");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".claude/worktrees/a/src/main.rs"), b"fn main() {}\n");
    let found = fx.discover(&repo);
    assert!(found.nothing_to_trust());
    assert!(found.findings.contains(&not_read(".claude/worktrees")));
    write(&repo.join(".claude/settings.json"), b"{}");
    assert!(!fx.discover(&repo).nothing_to_trust());
}
