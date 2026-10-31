//! Discovery of a project's own Claude configuration, on real files in a temporary directory.
//!
//! Every file a test inspects is made here, under a scratch directory with a fake home inside it;
//! no test points discovery at an existing project or at the real home. `git` runs only to make
//! fixtures, with no user or system configuration.
//!
//! Claude's discovery also looks at every directory above the top level up to `/`, so a stray
//! `CLAUDE.md`, `CLAUDE.local.md`, `.mcp.json`, `.claude/CLAUDE.md` or `.claude/rules/` in a directory
//! above the temp dir (`/tmp`, `/`) shows up in every fixture here and turns tests that expect an exact
//! list red: remove it, it is loaded by every trusted Claude session below it too.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use agent::DirTrust;
use eitri_core::project_trust::{
    codex_fallback_names, discover, eitri_server_name, eitri_server_taken, git_top_level, read_user_codex_config,
    trust_plan, Discovery, EditorServerName, EntryKind, Finding, Limits, Profile, ProfileEnv, ServerDefinition,
};

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
        let outer = std::env::temp_dir().join(format!(
            "eitri-trust-it-{label}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        #[cfg(target_os = "macos")]
        let _ = label;
        std::fs::create_dir_all(&outer).unwrap();
        let base = outer.canonicalize().unwrap();
        let home = base.join("home");
        std::fs::create_dir(&home).unwrap();
        Fixture { outer, base, home }
    }

    fn discover_as(&self, root: &Path, profile: &Profile, env: &ProfileEnv) -> Discovery {
        discover(root, Some(&self.home), &Limits::default(), profile, env)
    }

    /// Codex's environment with a `CODEX_HOME` outside the fake home and no alternate names.
    fn codex_env(&self) -> ProfileEnv {
        ProfileEnv {
            codex_home: Some(self.base.join("codex-home")),
            fallback_names: Vec::new(),
            user_mcp_servers: None,
            user_mcp_names: None,
            user_config_unreadable: None,
        }
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

    /// Discovery on another thread, which must finish within two seconds.
    fn discover_promptly(&self, root: &Path) -> Discovery {
        let (tx, rx) = mpsc::channel();
        let root = root.to_path_buf();
        let home = self.home.clone();
        std::thread::spawn(move || {
            let _ = tx.send(discover(
                &root,
                Some(&home),
                &Limits::default(),
                &Profile::Claude,
                &ProfileEnv::default(),
            ));
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
    assert!(first.findings.contains(&Finding::OtherFile {
        dir: PathBuf::from(".claude/hooks"),
        name: PathBuf::from("run.sh"),
        len: 18,
        frontmatter: Vec::new(),
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

/// The files of `found` the prompt names by themselves: (directory, name, size, frontmatter keys).
fn other_files(found: &Discovery) -> Vec<(String, String, u64, Vec<String>)> {
    let mut out: Vec<_> = found
        .findings
        .iter()
        .filter_map(|f| match f {
            Finding::OtherFile {
                dir,
                name,
                len,
                frontmatter,
            } => Some((
                dir.display().to_string(),
                name.display().to_string(),
                *len,
                frontmatter.clone(),
            )),
            _ => None,
        })
        .collect();
    out.sort();
    out
}

/// Every tree read whole names each of its other files with its size, and a Markdown file whose
/// frontmatter sets hooks, tools, a permission mode or MCP servers says so.
#[test]
fn other_files_are_named_in_every_tree() {
    let fx = Fixture::new("other-named");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    let tools = b"---\nallowed-tools: Read\n---\nbody\n";
    write(&repo.join(".claude/settings.json"), b"{}");
    write(&repo.join(".claude/skills/s/SKILL.md"), tools);
    write(&repo.join(".claude/commands/c.md"), b"plain\n");
    write(
        &repo.join(".claude/agents/a.md"),
        b"---\npermissionMode: plan\nmcpServers: {}\n---\n",
    );
    write(&repo.join(".codex/prompts/p.md"), tools);
    write(&repo.join(".agents/skills/k/SKILL.md"), b"---\nhooks: {}\n---\n");
    write(&repo.join(".gem/x.md"), tools);
    let s = |v: &[&str]| v.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    assert_eq!(
        other_files(&fx.discover(&repo)),
        vec![
            (
                ".claude/agents".into(),
                "a.md".into(),
                44,
                s(&["permissionMode", "mcpServers"])
            ),
            (".claude/commands".into(), "c.md".into(), 6, vec![]),
            (".claude/skills/s".into(), "SKILL.md".into(), 33, s(&["allowed-tools"])),
        ]
    );
    assert_eq!(
        other_files(&fx.discover_as(&repo, &Profile::Codex, &fx.codex_env())),
        vec![
            (".agents/skills/k".into(), "SKILL.md".into(), 18, s(&["hooks"])),
            (".codex/prompts".into(), "p.md".into(), 33, s(&["allowed-tools"])),
        ]
    );
    let gem = Profile::Acp {
        name: "gem".into(),
        files: vec![".gem/".into()],
    };
    assert_eq!(
        other_files(&fx.discover_as(&repo, &gem, &ProfileEnv::default())),
        vec![(".gem".into(), "x.md".into(), 33, s(&["allowed-tools"]))]
    );
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
    // A hook script that is a link is fingerprinted by its target as written, and the CLI reads what
    // it leads to, which no fingerprint sees: the answer lasts one start.
    assert!(!found.fully_hashed());

    // The target's bytes are never read.
    std::fs::write(elsewhere.join("run"), b"changed\n").unwrap();
    assert_eq!(fx.discover(&repo).fingerprint, found.fingerprint);

    // Retargeting the link is a change.
    std::fs::remove_file(repo.join(".claude/hooks/run")).unwrap();
    symlink(elsewhere.join("other"), repo.join(".claude/hooks/run")).unwrap();
    assert_ne!(fx.discover(&repo).fingerprint, found.fingerprint);

    // A link that names a place inside the top level is not outside, even when nothing is there.
    std::fs::remove_file(repo.join(".claude/hooks/run")).unwrap();
    symlink("absent.sh", repo.join(".claude/hooks/run")).unwrap();
    let inside = fx.discover(&repo);
    assert!(inside.entries.iter().any(|e| e.path == Path::new(".claude/hooks/run")
        && e.kind
            == EntryKind::Symlink {
                target: PathBuf::from("absent.sh"),
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

/// Everything under `.claude/` is loaded by the CLI, skills and hook scripts included, and the CLI
/// follows a link anywhere there: what a link out of the root leads to can change under an unchanged
/// fingerprint, so such a link at any depth below `.claude/` makes the answer last one start, never a
/// record.
#[test]
fn a_link_out_of_the_root_anywhere_below_dot_claude_is_not_fully_hashed() {
    let fx = Fixture::new("below-claude-link");
    write(&fx.home.join("scripts/hooks/run.sh"), b"#!/bin/sh\n");
    write(&fx.home.join("x.md"), b"x\n");
    write(&fx.home.join("a.md"), b"a\n");
    write(&fx.home.join("skill/SKILL.md"), b"s\n");
    for (at, target) in [
        (".claude/hooks", "../../scripts/hooks"),
        (".claude/commands/x.md", "../../../x.md"),
        (".claude/agents/deep/er/a.md", "../../../../../a.md"),
        (".claude/skills/s", "../../../skill"),
    ] {
        let repo = fx.home.join(format!("repo-{}", at.replace(['/', '.'], "_")));
        fx.repo(&repo, false);
        write(&repo.join(".claude/settings.json"), b"{}");
        std::fs::create_dir_all(repo.join(at).parent().unwrap()).unwrap();
        symlink(target, repo.join(at)).unwrap();
        assert_link_not_followed(&fx.discover(&repo), at, target, true);
    }
}

/// `at` stayed a link nobody followed: named by its target as written, unchecked, nothing below it, and
/// said to lead `outside` the top level or not.
fn assert_link_not_followed(found: &Discovery, at: &str, target: &str, outside: bool) {
    assert!(!found.fully_hashed(), "{at}");
    let reason = format!("a link to {target}");
    assert!(
        found.uncheckable().contains(&(Path::new(at), reason.as_str())),
        "{at}: {:?}",
        found.uncheckable()
    );
    assert!(
        found.findings.contains(&Finding::Symlink {
            path: PathBuf::from(at),
            target: PathBuf::from(target),
            outside,
        }),
        "{at}: {:?}",
        found.findings
    );
    assert!(
        found.entries.iter().any(|entry| entry.path == Path::new(at)
            && entry.kind
                == EntryKind::Symlink {
                    target: PathBuf::from(target),
                    outside,
                }),
        "{at}: {:?}",
        found.entries
    );
    let below = found
        .entries
        .iter()
        .find(|entry| entry.path.starts_with(at) && entry.path != Path::new(at));
    assert!(below.is_none(), "{at}: {below:?}");
}

/// A repository with a `.claude/` in it, under the fake home.
fn linked_repo(fx: &Fixture, name: &str) -> PathBuf {
    let repo = fx.home.join(name);
    fx.repo(&repo, false);
    write(&repo.join(".claude/settings.json"), b"{}");
    repo
}

fn link(target: impl AsRef<Path>, at: PathBuf) {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    symlink(target, at).unwrap();
}

/// The length of the file reached through the link at `at`.
fn reached_len(found: &Discovery, at: &str) -> Option<u64> {
    found.entries.iter().find_map(|entry| match &entry.kind {
        EntryKind::LinkedFile { len, .. } if entry.path == Path::new(at) => Some(*len),
        _ => None,
    })
}

#[test]
fn a_directory_linked_inside_the_root_is_walked_like_a_real_one() {
    let fx = Fixture::new("link-dir");
    let repo = linked_repo(&fx, "repo");
    write(
        &repo.join(".codex/skills/vm/SKILL.md"),
        b"---\nallowed-tools: Bash\n---\nskill\n",
    );
    write(&repo.join(".codex/skills/vm/refs/notes.md"), b"notes\n");
    link("../../.codex/skills/vm", repo.join(".claude/skills/vm"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    // The prompt names the link with its target, and each file where the CLI reads it.
    assert!(found.findings.contains(&Finding::Symlink {
        path: PathBuf::from(".claude/skills/vm"),
        target: PathBuf::from("../../.codex/skills/vm"),
        outside: false,
    }));
    assert!(found.findings.iter().any(|f| matches!(
        f,
        Finding::OtherFile { dir, name, frontmatter, .. }
            if dir == Path::new(".claude/skills/vm")
                && name == Path::new("SKILL.md")
                && *frontmatter == ["allowed-tools"]
    )));
    let notes = found
        .entries
        .iter()
        .find(|entry| entry.path == Path::new(".claude/skills/vm/refs/notes.md"));
    assert!(
        matches!(notes, Some(e) if matches!(e.kind, EntryKind::File { .. })),
        "{notes:?}"
    );
    // What the link reaches is in the fingerprint.
    write(&repo.join(".codex/skills/vm/refs/notes.md"), b"changed\n");
    let edited = fx.discover(&repo);
    assert_ne!(edited.fingerprint, found.fingerprint);
    // So is the link's target as written: the same content somewhere else is another configuration.
    std::fs::rename(repo.join(".codex/skills/vm"), repo.join(".codex/skills/moved")).unwrap();
    std::fs::remove_file(repo.join(".claude/skills/vm")).unwrap();
    link("../../.codex/skills/moved", repo.join(".claude/skills/vm"));
    let moved = fx.discover(&repo);
    assert!(moved.fully_hashed(), "{:?}", moved.uncheckable());
    assert_ne!(moved.fingerprint, edited.fingerprint);
}

#[test]
fn a_file_linked_inside_the_root_is_hashed_with_its_target() {
    let fx = Fixture::new("link-file");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("docs/x.md"), b"same\n");
    write(&repo.join("docs/y.md"), b"same\n");
    link("../../docs/x.md", repo.join(".claude/commands/x.md"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert_eq!(reached_len(&found, ".claude/commands/x.md"), Some(5));
    assert!(found.findings.iter().any(|f| matches!(
        f,
        Finding::OtherFile { dir, name, len: 5, .. }
            if dir == Path::new(".claude/commands") && name == Path::new("x.md")
    )));
    std::fs::remove_file(repo.join(".claude/commands/x.md")).unwrap();
    link("../../docs/y.md", repo.join(".claude/commands/x.md"));
    let retargeted = fx.discover(&repo);
    assert_ne!(retargeted.fingerprint, found.fingerprint, "same bytes, another target");
    write(&repo.join("docs/y.md"), b"other\n");
    assert_ne!(fx.discover(&repo).fingerprint, retargeted.fingerprint);
}

#[test]
fn a_settings_file_linked_inside_the_root_is_read_as_settings() {
    let fx = Fixture::new("link-settings");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join("config/claude.json"), HOOK_SETTINGS);
    link("../config/claude.json", repo.join(".claude/settings.json"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert!(found
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Hook { file, .. } if file == Path::new(".claude/settings.json"))));
}

#[test]
fn a_link_that_climbs_out_of_the_root_and_back_is_not_followed() {
    let fx = Fixture::new("link-out-and-back");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("skill/SKILL.md"), b"s\n");
    write(&repo.join("x.md"), b"x\n");
    link("../../../repo/skill", repo.join(".claude/skills/s"));
    link("../../repo/x.md", repo.join(".claude/x.md"));
    let found = fx.discover(&repo);
    assert_link_not_followed(&found, ".claude/skills/s", "../../../repo/skill", true);
    assert_link_not_followed(&found, ".claude/x.md", "../../repo/x.md", true);
}

#[test]
fn an_absolute_target_is_not_followed_even_inside_the_root() {
    let fx = Fixture::new("link-absolute");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("x.md"), b"x\n");
    let target = repo.join("x.md");
    link(&target, repo.join(".claude/x.md"));
    assert_link_not_followed(&fx.discover(&repo), ".claude/x.md", target.to_str().unwrap(), false);
}

#[test]
fn a_chain_of_links_is_followed_only_while_it_stays_inside() {
    let fx = Fixture::new("link-chain");
    let repo = linked_repo(&fx, "repo");
    // Inside all the way.
    write(&repo.join("docs/end.md"), b"end\n");
    link("../docs/middle.md", repo.join(".claude/inside.md"));
    link("end.md", repo.join("docs/middle.md"));
    // The second link leaves.
    write(&fx.home.join("secret.md"), b"elsewhere\n");
    link("../docs/out.md", repo.join(".claude/leaves.md"));
    link("../../secret.md", repo.join("docs/out.md"));
    let found = fx.discover(&repo);
    assert_eq!(reached_len(&found, ".claude/inside.md"), Some(4));
    assert_link_not_followed(&found, ".claude/leaves.md", "../docs/out.md", true);
    assert_eq!(found.uncheckable().len(), 1, "{:?}", found.uncheckable());
}

/// Every link on the way is shown and in the fingerprint: the same bytes behind a retargeted link in the
/// middle of a chain are another configuration, as behind the first one.
#[test]
fn every_link_of_a_chain_is_named_and_in_the_fingerprint() {
    let fx = Fixture::new("link-chain-middle");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("docs/a.md"), b"same\n");
    write(&repo.join("docs/b.md"), b"same\n");
    link("../../alias", repo.join(".claude/commands/x.md"));
    link("../../alias", repo.join(".claude/commands/y.md"));
    link("docs/a.md", repo.join("alias"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    let middle = Finding::Symlink {
        path: PathBuf::from("alias"),
        target: PathBuf::from("docs/a.md"),
        outside: false,
    };
    let rows = found.findings.iter().filter(|finding| **finding == middle).count();
    assert_eq!(
        rows, 1,
        "one row for a link two others pass through: {:?}",
        found.findings
    );
    assert!(found.entries.iter().any(|entry| entry.path == Path::new("alias")
        && entry.kind
            == EntryKind::Symlink {
                target: PathBuf::from("docs/a.md"),
                outside: false,
            }));

    std::fs::remove_file(repo.join("alias")).unwrap();
    link("docs/b.md", repo.join("alias"));
    assert_ne!(fx.discover(&repo).fingerprint, found.fingerprint);

    // The same through a directory.
    let repo = linked_repo(&fx, "dirs");
    write(&repo.join("one/f.md"), b"same\n");
    write(&repo.join("two/f.md"), b"same\n");
    link("../alias", repo.join(".claude/d"));
    link("one", repo.join("alias"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    std::fs::remove_file(repo.join("alias")).unwrap();
    link("two", repo.join("alias"));
    assert_ne!(fx.discover(&repo).fingerprint, found.fingerprint);
}

/// A link walked in its own right that another link also passes through is one entry and one row, whichever
/// the walk meets first, and what it reaches stays named at its own path.
#[test]
fn a_walked_link_another_link_passes_through_is_named_once() {
    let fx = Fixture::new("link-walked-chain");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("docs/f.md"), b"f\n");
    write(&repo.join("dir/g.md"), b"g\n");
    // Met first by the walk, then on the other link's way.
    link("../docs/f.md", repo.join(".claude/a.md"));
    link("a.md", repo.join(".claude/z.md"));
    // On the other link's way first, then by the walk.
    link("c.md", repo.join(".claude/b.md"));
    link("../docs/f.md", repo.join(".claude/c.md"));
    // The same with links to a directory.
    link("e", repo.join(".claude/d"));
    link("../dir", repo.join(".claude/e"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    for at in [".claude/a.md", ".claude/c.md", ".claude/e"] {
        let entries = found.entries.iter().filter(|e| e.path == Path::new(at)).count();
        assert_eq!(entries, 1, "{at}: {:?}", found.entries);
        let rows = found
            .findings
            .iter()
            .filter(|f| matches!(f, Finding::Symlink { path, .. } if path == Path::new(at)))
            .count();
        assert_eq!(rows, 1, "{at}: {:?}", found.findings);
    }
    assert_eq!(reached_len(&found, ".claude/a.md"), Some(2));
    assert_eq!(reached_len(&found, ".claude/c.md"), Some(2));
}

/// A chain that leaves is said to, though the first link's own text stays inside.
#[test]
fn a_chain_that_leaves_is_outside_though_its_first_link_is_not() {
    let fx = Fixture::new("link-chain-outside");
    let repo = linked_repo(&fx, "repo");
    write(&fx.home.join("secret.md"), b"elsewhere\n");
    link("../alias", repo.join(".claude/x.md"));
    link("../secret.md", repo.join("alias"));
    link("../absolute", repo.join(".claude/y.md"));
    link(fx.home.join("secret.md"), repo.join("absolute"));
    let found = fx.discover(&repo);
    assert_link_not_followed(&found, ".claude/x.md", "../alias", true);
    assert_link_not_followed(&found, ".claude/y.md", "../absolute", true);
}

/// The note that a link leads out of the project is judged from where the link really is, not from where
/// the agent reads it through another link.
#[test]
fn a_link_in_a_reached_directory_is_outside_by_where_it_really_is() {
    let fx = Fixture::new("link-outside-note");
    let repo = linked_repo(&fx, "repo");
    write(&fx.home.join("secret.md"), b"elsewhere\n");
    write(&repo.join("a/f.md"), b"f\n");
    link("../../secret.md", repo.join("a/out.md"));
    link("../a", repo.join(".claude/s"));
    let found = fx.discover(&repo);
    assert!(
        found.findings.contains(&Finding::Symlink {
            path: PathBuf::from(".claude/s/out.md"),
            target: PathBuf::from("../../secret.md"),
            outside: true,
        }),
        "{:?}",
        found.findings
    );
    assert!(!found.fully_hashed());

    // One the walk could not follow: from where it really is it stays inside, however deep the agent
    // reads it from.
    let repo = linked_repo(&fx, "repo-deep");
    write(&repo.join("a/b/c/f.md"), b"f\n");
    link("../../../gone.md", repo.join("a/b/c/dangling.md"));
    link("../a/b/c", repo.join(".claude/s"));
    assert_link_not_followed(&fx.discover(&repo), ".claude/s/dangling.md", "../../../gone.md", false);
}

/// A top level no repository vouches for may be a shared directory the project does not own: links are
/// followed only inside the root itself then.
#[test]
fn an_unverified_top_level_does_not_widen_where_a_link_may_lead() {
    let fx = Fixture::new("link-unverified");
    let shared = fx.home.join("shared");
    mkfifo(&shared.join(".git"));
    write(&shared.join("skill/SKILL.md"), b"s\n");
    let root = shared.join("project");
    write(&root.join(".claude/settings.json"), b"{}");
    link("../../skill", root.join(".claude/s"));
    link("../skill/SKILL.md", root.join("CLAUDE.md"));
    let found = fx.discover(&root);
    assert_eq!(found.top, shared);
    assert_link_not_followed(&found, "project/.claude/s", "../../skill", false);
    assert_link_not_followed(&found, "project/CLAUDE.md", "../skill/SKILL.md", false);

    // With nothing above it in question, the root is its own boundary.
    let root = fx.home.join("plain");
    write(&root.join(".claude/settings.json"), b"{}");
    write(&root.join("x.md"), b"x\n");
    link("../x.md", root.join(".claude/x.md"));
    let found = fx.discover(&root);
    assert_eq!(found.top, root);
    assert_eq!(reached_len(&found, ".claude/x.md"), Some(2));
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
}

/// `.claude/` directories on the way from the root up to the top level follow links from where each is.
#[test]
fn links_are_followed_in_every_walked_dot_claude() {
    let fx = Fixture::new("link-walked");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("docs/top.md"), b"top\n");
    write(&repo.join("sub/own.md"), b"own one\n");
    write(&repo.join("own.md"), b"the wrong one\n");
    link("../docs/top.md", repo.join(".claude/top.md"));
    link("../own.md", repo.join("sub/.claude/own.md"));
    std::fs::create_dir_all(repo.join("sub/deeper")).unwrap();
    let found = fx.discover(&repo.join("sub/deeper"));
    assert_eq!(found.top, repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert_eq!(reached_len(&found, ".claude/top.md"), Some(4));
    assert_eq!(reached_len(&found, "sub/.claude/own.md"), Some(8));
}

/// Above the top level nothing is followed: only what is read there is checked at all.
#[test]
fn a_link_in_rules_above_the_top_level_is_not_followed() {
    let fx = Fixture::new("link-above");
    let parent = fx.home.join("parent");
    let repo = parent.join("repo");
    fx.repo(&repo, false);
    write(&parent.join("r.md"), b"r\n");
    link("../../r.md", parent.join(".claude/rules/r.md"));
    assert_link_not_followed(&fx.discover(&repo), "../.claude/rules/r.md", "../../r.md", true);
}

/// A slash at the end of a target changes nothing for a directory.
#[test]
fn a_target_ending_in_a_slash_reaches_its_directory() {
    let fx = Fixture::new("link-slash");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("real/f.md"), b"f\n");
    link("../real/", repo.join(".claude/d"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert!(found
        .entries
        .iter()
        .any(|entry| entry.path == Path::new(".claude/d/f.md")));
}

/// One target cannot make the walk take steps without limit, whatever it spells.
#[test]
fn a_target_of_too_many_steps_is_not_followed() {
    let fx = Fixture::new("link-steps");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("d/f.md"), b"f\n");
    let long = format!("../{}d/f.md", "d/../".repeat(200));
    link(&long, repo.join(".claude/long.md"));
    assert_link_not_followed(&fx.discover(&repo), ".claude/long.md", &long, true);
}

/// `..` after a link is the parent of where the link led, as the kernel takes it, not of the link.
#[test]
fn dot_dot_after_a_link_is_taken_where_the_link_led() {
    let fx = Fixture::new("link-physical");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("near/f.md"), b"the wrong one\n");
    write(&repo.join("far/deep/g.md"), b"g\n");
    write(&repo.join("far/f.md"), b"right\n");
    link("../far/deep", repo.join("near/l"));
    link("../near/l/../f.md", repo.join(".claude/x.md"));
    let found = fx.discover(&repo);
    assert_eq!(reached_len(&found, ".claude/x.md"), Some(6));
}

#[test]
fn a_loop_of_links_is_named_and_not_walked() {
    let fx = Fixture::new("link-loop");
    for (name, links) in [
        ("itself", vec![(".claude/l", "l")]),
        ("two", vec![(".claude/l", "m"), (".claude/m", "l")]),
        ("parent", vec![(".claude/skills/l", "..")]),
        ("top", vec![(".claude/l", "..")]),
        ("own-dir", vec![(".claude/skills/l", ".")]),
    ] {
        let repo = linked_repo(&fx, name);
        for (at, target) in &links {
            link(target, repo.join(at));
        }
        let found = fx.discover_promptly(&repo);
        assert_eq!(found.over_budget, None, "{name}: a loop is not walked into the budget");
        let (at, target) = links[0];
        assert_link_not_followed(&found, at, target, false);
    }
    // A cycle through two directories the walk enters by links.
    let repo = linked_repo(&fx, "cycle");
    write(&repo.join("p/f.md"), b"p\n");
    write(&repo.join("q/g.md"), b"q\n");
    link("../p", repo.join(".claude/p"));
    link("../q", repo.join("p/q"));
    link("../p", repo.join("q/back"));
    let found = fx.discover_promptly(&repo);
    assert_eq!(found.over_budget, None);
    assert_link_not_followed(&found, ".claude/p/q/back", "../p", false);
}

#[test]
fn a_linked_target_over_the_limits_is_not_fully_hashed() {
    let fx = Fixture::new("link-big");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("docs/big.md"), &vec![b'x'; (4 << 20) + 1]);
    link("../../docs/big.md", repo.join(".claude/commands/big.md"));
    let found = fx.discover(&repo);
    assert!(!found.fully_hashed());
    assert!(found
        .uncheckable()
        .contains(&(Path::new(".claude/commands/big.md"), "over 4 MiB")));
    assert_eq!(
        found.over_budget.as_deref(),
        Some(".claude/commands/big.md is over 4 MiB")
    );
    // The link is still shown with its target.
    assert!(found.findings.iter().any(|f| matches!(
        f,
        Finding::Symlink { path, outside: false, .. } if path == Path::new(".claude/commands/big.md")
    )));

    // What a link reaches counts against every other budget too.
    let repo = linked_repo(&fx, "budgets");
    write(&repo.join("deep/a/b/c/f.md"), b"12345678\n");
    link("../deep", repo.join(".claude/l"));
    let run = |limits: Limits| discover(&repo, Some(&fx.home), &limits, &Profile::Claude, &ProfileEnv::default());
    let shallow = run(Limits {
        max_depth: 2,
        ..Limits::default()
    });
    assert_eq!(
        shallow.over_budget.as_deref(),
        Some("deeper than 2 directories under .claude/")
    );
    let few = run(Limits {
        max_entries: 3,
        ..Limits::default()
    });
    assert_eq!(few.over_budget.as_deref(), Some("more than 3 entries of configuration"));
    let small = run(Limits {
        max_total_bytes: 4,
        ..Limits::default()
    });
    assert!(!small.fully_hashed());
    assert!(small
        .over_budget
        .as_deref()
        .is_some_and(|reason| reason.contains("more than 4 bytes of configuration")));

    // The links on the way take room too, so the budget is said to be spent, however the chain ends.
    for (name, end) in [("to-file", "two.md"), ("to-dir", "empty")] {
        let repo = fx.home.join(format!("chain-{name}"));
        fx.repo(&repo, false);
        write(&repo.join(".claude/a.md"), b"a\n");
        write(&repo.join("two.md"), b"t\n");
        std::fs::create_dir(repo.join("empty")).unwrap();
        link(end, repo.join("one"));
        link("../one", repo.join(".claude/l"));
        let limits = Limits {
            max_entries: 2,
            ..Limits::default()
        };
        let found = discover(&repo, Some(&fx.home), &limits, &Profile::Claude, &ProfileEnv::default());
        assert_eq!(
            found.over_budget.as_deref(),
            Some("more than 2 entries of configuration"),
            "{name}"
        );
        assert!(found.entries.len() <= 2, "{name}: {:?}", found.entries);
    }
    let repo = linked_repo(&fx, "chain-memory");
    let root = repo.join("sub");
    write(&root.join(".claude/a.md"), b"a\n");
    write(&root.join(".claude/b.md"), b"b\n");
    write(&repo.join("AGENTS.md"), b"m\n");
    link("AGENTS.md", repo.join("alias.md"));
    link("alias.md", repo.join("CLAUDE.md"));
    let limits = Limits {
        max_entries: 3,
        ..Limits::default()
    };
    let found = discover(&root, Some(&fx.home), &limits, &Profile::Claude, &ProfileEnv::default());
    assert_eq!(
        found.over_budget.as_deref(),
        Some("more than 3 entries of configuration")
    );
    assert!(found.entries.len() <= 3, "{:?}", found.entries);

    // A link at the limit is walked no deeper than a directory there would be.
    let repo = linked_repo(&fx, "at-the-limit");
    write(&repo.join("docs/f.md"), b"f\n");
    link("../../docs", repo.join(".claude/a/l"));
    let limits = Limits {
        max_depth: 1,
        ..Limits::default()
    };
    let found = discover(&repo, Some(&fx.home), &limits, &Profile::Claude, &ProfileEnv::default());
    assert_eq!(
        found.over_budget.as_deref(),
        Some("deeper than 1 directories under .claude/")
    );
    assert!(!found
        .entries
        .iter()
        .any(|entry| entry.path == Path::new(".claude/a/l/f.md")));
}

#[test]
fn a_link_to_a_pipe_or_to_nothing_is_not_followed() {
    let fx = Fixture::new("link-fifo");
    let repo = linked_repo(&fx, "repo");
    mkfifo(&repo.join("docs/pipe"));
    link("../docs/pipe", repo.join(".claude/pipe"));
    link("../docs/nothing", repo.join(".claude/dangling"));
    link("../docs/pipe/below", repo.join(".claude/through"));
    let found = fx.discover_promptly(&repo);
    assert_link_not_followed(&found, ".claude/pipe", "../docs/pipe", false);
    assert_link_not_followed(&found, ".claude/dangling", "../docs/nothing", false);
    assert_link_not_followed(&found, ".claude/through", "../docs/pipe/below", false);
}

/// A link in a directory reached through another link leads from where it really is.
#[test]
fn a_link_in_a_reached_directory_resolves_from_where_it_really_is() {
    let fx = Fixture::new("link-nested");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join(".codex/skills/shared.md"), b"shared\n");
    write(&repo.join(".claude/skills/shared.md"), b"not this one\n");
    write(&repo.join(".codex/skills/vm/SKILL.md"), b"s\n");
    link("../shared.md", repo.join(".codex/skills/vm/ref.md"));
    link("../../.codex/skills/vm", repo.join(".claude/skills/vm"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert_eq!(reached_len(&found, ".claude/skills/vm/ref.md"), Some(7));

    // And one that leaves, however deep, is a gap as anywhere else.
    write(&fx.home.join("secret.md"), b"elsewhere\n");
    link("../../../../secret.md", repo.join(".codex/skills/vm/out.md"));
    let found = fx.discover(&repo);
    assert_link_not_followed(&found, ".claude/skills/vm/out.md", "../../../../secret.md", true);
}

/// With the user's home inside the top level a link could reach the account's own files, and with no
/// known home that cannot be ruled out: no link is followed then.
#[test]
fn no_link_is_followed_when_the_top_level_may_hold_the_home() {
    let fx = Fixture::new("link-home");
    // The home is the repository.
    fx.repo(&fx.home, false);
    let root = fx.home.join("sub");
    write(&fx.home.join("account.json"), b"{}");
    write(&root.join(".claude/settings.json"), b"{}");
    link("../../account.json", root.join(".claude/x.json"));
    link("../account.json", fx.home.join(".claude/settings.local.json"));
    link("account.json", fx.home.join("CLAUDE.md"));
    assert!(root.join(".claude/x.json").exists(), "the link leads somewhere");
    let found = fx.discover(&root);
    assert_eq!(found.top, fx.home);
    assert_link_not_followed(&found, "sub/.claude/x.json", "../../account.json", false);
    assert_link_not_followed(&found, ".claude/settings.local.json", "../account.json", false);
    assert_link_not_followed(&found, "CLAUDE.md", "account.json", false);

    // The home is below the repository.
    let fx = Fixture::new("link-below-home");
    fx.repo(&fx.base, false);
    let root = fx.base.join("sub");
    write(&fx.base.join("home/account.json"), b"{}");
    write(&root.join(".claude/settings.json"), b"{}");
    link("../../home/account.json", root.join(".claude/x.json"));
    link("../home/account.json", root.join("CLAUDE.md"));
    let found = fx.discover(&root);
    assert_eq!(found.top, fx.base);
    assert_link_not_followed(&found, "sub/.claude/x.json", "../../home/account.json", false);
    assert_link_not_followed(&found, "sub/CLAUDE.md", "../home/account.json", false);

    // No home is known.
    let fx = Fixture::new("link-no-home");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("x.md"), b"x\n");
    link("../x.md", repo.join(".claude/x.md"));
    let found = discover(
        &repo,
        None,
        &Limits::default(),
        &Profile::Claude,
        &ProfileEnv::default(),
    );
    assert_link_not_followed(&found, ".claude/x.md", "../x.md", false);
}

#[test]
fn a_link_named_worktrees_stays_one_the_walk_does_not_follow() {
    let fx = Fixture::new("link-worktrees");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("checkouts/a/f.md"), b"f\n");
    link("../checkouts", repo.join(".claude/worktrees"));
    assert_link_not_followed(&fx.discover(&repo), ".claude/worktrees", "../checkouts", false);
}

/// A link anywhere else stays as it was: only a `.claude/` of the walk is read through its links.
#[test]
fn links_outside_a_walked_dot_claude_are_not_followed() {
    let fx = Fixture::new("link-elsewhere");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join("real.json"), b"{}");
    write(&repo.join("realdir/settings.json"), b"{}");
    link("real.json", repo.join(".mcp.json"));
    let found = fx.discover(&repo);
    assert_link_not_followed(&found, ".mcp.json", "real.json", false);

    let repo = fx.home.join("repo2");
    fx.repo(&repo, false);
    write(&repo.join("realdir/settings.json"), b"{}");
    link("realdir", repo.join(".claude"));
    assert_link_not_followed(&fx.discover(&repo), ".claude", "realdir", false);

    let repo = fx.home.join("repo3");
    fx.repo(&repo, false);
    write(&repo.join(".agents/skills/real/SKILL.md"), b"s\n");
    link("real", repo.join(".agents/skills/l"));
    write(&repo.join("real.md"), b"memory\n");
    link("real.md", repo.join("AGENTS.md"));
    let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    assert_link_not_followed(&found, ".agents/skills/l", "real", false);
    assert_link_not_followed(&found, "AGENTS.md", "real.md", false);
}

/// `CLAUDE.md -> AGENTS.md` is read through like a link below `.claude/`: the bytes are memory at the
/// link's own path, and the target as written is part of what was trusted.
#[test]
fn a_memory_file_linked_inside_the_root_is_hashed_with_its_target() {
    let fx = Fixture::new("link-memory");
    let repo = linked_repo(&fx, "repo");
    write(&repo.join("AGENTS.md"), b"same\n");
    write(&repo.join("OTHER.md"), b"same\n");
    write(&repo.join("docs/local.md"), b"local\n");
    link("AGENTS.md", repo.join("CLAUDE.md"));
    link("docs/local.md", repo.join("CLAUDE.local.md"));
    let found = fx.discover(&repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert!(found.findings.contains(&Finding::Memory {
        path: PathBuf::from("CLAUDE.md"),
    }));
    assert!(found.findings.contains(&Finding::Symlink {
        path: PathBuf::from("CLAUDE.md"),
        target: PathBuf::from("AGENTS.md"),
        outside: false,
    }));
    assert_eq!(reached_len(&found, "CLAUDE.md"), Some(5));
    assert_eq!(reached_len(&found, "CLAUDE.local.md"), Some(6));

    let before = found.fingerprint;
    write(&repo.join("AGENTS.md"), b"edit\n");
    assert_ne!(fx.discover(&repo).fingerprint, before);
    write(&repo.join("AGENTS.md"), b"same\n");
    assert_eq!(fx.discover(&repo).fingerprint, before);
    // The same bytes behind another target are another configuration.
    std::fs::remove_file(repo.join("CLAUDE.md")).unwrap();
    link("OTHER.md", repo.join("CLAUDE.md"));
    assert_ne!(fx.discover(&repo).fingerprint, before);
}

/// Each directory from the root up to the top level follows its own memory link from where it is.
#[test]
fn a_memory_file_links_from_its_own_walked_directory() {
    let fx = Fixture::new("link-memory-walked");
    let repo = linked_repo(&fx, "repo");
    let root = repo.join("sub");
    write(&repo.join("AGENTS.md"), b"top\n");
    write(&root.join("notes.md"), b"sub note\n");
    link("../AGENTS.md", root.join("CLAUDE.md"));
    link("sub/notes.md", repo.join("CLAUDE.md"));
    let found = fx.discover(&root);
    assert_eq!(found.top, repo);
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert_eq!(reached_len(&found, "sub/CLAUDE.md"), Some(4));
    assert_eq!(reached_len(&found, "CLAUDE.md"), Some(9));
}

/// A memory link that does not arrive at a regular file inside the top level by a way that stays there
/// is a gap, named where the CLI reads it.
#[test]
fn a_memory_file_linked_any_other_way_is_not_followed() {
    let fx = Fixture::new("link-memory-other");
    write(&fx.home.join("outside.md"), b"o\n");
    let cases: [(&str, &str, bool); 6] = [
        ("out", "../outside.md", true),
        ("back", "../repo-back/AGENTS.md", true),
        ("dir", "docs", false),
        ("absent", "gone.md", false),
        ("pipe", "pipe", false),
        ("loop", "CLAUDE.md", false),
    ];
    for (name, target, outside) in cases {
        let repo = linked_repo(&fx, &format!("repo-{name}"));
        write(&repo.join("AGENTS.md"), b"a\n");
        write(&repo.join("docs/x.md"), b"x\n");
        mkfifo(&repo.join("pipe"));
        link(target, repo.join("CLAUDE.md"));
        assert_link_not_followed(&fx.discover(&repo), "CLAUDE.md", target, outside);
    }

    // Absolute, even naming a place inside.
    let repo = linked_repo(&fx, "repo-abs");
    write(&repo.join("AGENTS.md"), b"a\n");
    link(repo.join("AGENTS.md"), repo.join("CLAUDE.md"));
    let target = repo.join("AGENTS.md");
    assert_link_not_followed(&fx.discover(&repo), "CLAUDE.md", target.to_str().unwrap(), false);

    // A chain that leaves through a later link.
    let repo = linked_repo(&fx, "repo-chain");
    link("../outside.md", repo.join("near.md"));
    link("near.md", repo.join("CLAUDE.md"));
    assert_link_not_followed(&fx.discover(&repo), "CLAUDE.md", "near.md", true);
}

/// Above the top level the CLI reads memory too, but no link is followed there.
#[test]
fn a_memory_file_linked_above_the_top_level_is_not_followed() {
    let fx = Fixture::new("link-memory-above");
    let outer = fx.home.join("outer");
    write(&outer.join("real.md"), b"r\n");
    link("real.md", outer.join("CLAUDE.md"));
    let repo = outer.join("repo");
    fx.repo(&repo, false);
    assert_link_not_followed(&fx.discover(&repo), "../CLAUDE.md", "real.md", true);
}

/// The reverse of a skill linked from `.claude/`: on a Codex tab a skill linked from `.codex/skills/` is
/// walked as Codex reads it, with `.codex/skills/` roles, and everything behind the link is hashed.
#[test]
fn a_codex_skill_linked_inside_the_root_is_walked_like_a_real_one() {
    let fx = Fixture::new("link-codex-skill");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".codex/config.toml"), b"model = \"m\"\n");
    write(
        &repo.join(".claude/skills/vm/SKILL.md"),
        b"---\nallowed-tools: Bash\n---\nbody\n",
    );
    write(&repo.join(".claude/skills/vm/refs/notes.md"), b"notes\n");
    write(&repo.join("docs/skill.md"), b"doc\n");
    write(&repo.join("docs/same.md"), b"doc\n");
    link("../../.claude/skills/vm", repo.join(".codex/skills/vm"));
    link("../../../docs/skill.md", repo.join(".codex/skills/x/SKILL.md"));
    link("../../../.claude/skills/vm/refs", repo.join(".codex/skills/x/refs"));
    let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    assert!(found.fully_hashed(), "{:?}", found.uncheckable());
    assert!(found.findings.contains(&Finding::Skill {
        path: PathBuf::from(".codex/skills/vm"),
        name: "vm".to_string(),
    }));
    assert!(found.findings.contains(&Finding::Symlink {
        path: PathBuf::from(".codex/skills/vm"),
        target: PathBuf::from("../../.claude/skills/vm"),
        outside: false,
    }));
    assert!(found.findings.iter().any(|finding| matches!(
        finding,
        Finding::OtherFile { dir, name, frontmatter, .. }
            if dir == Path::new(".codex/skills/vm")
                && name == Path::new("SKILL.md")
                && frontmatter.contains(&"allowed-tools".to_string())
    )));
    assert!(found
        .entries
        .iter()
        .any(|entry| entry.path == Path::new(".codex/skills/x/refs/notes.md")));
    assert_eq!(reached_len(&found, ".codex/skills/x/SKILL.md"), Some(4));

    let before = found.fingerprint;
    let codex = |fx: &Fixture| fx.discover_as(&repo, &Profile::Codex, &fx.codex_env()).fingerprint;
    write(&repo.join(".claude/skills/vm/refs/notes.md"), b"Notes\n");
    assert_ne!(codex(&fx), before);
    write(&repo.join(".claude/skills/vm/refs/notes.md"), b"notes\n");
    assert_eq!(codex(&fx), before);
    // The same bytes behind another target are another configuration.
    std::fs::remove_file(repo.join(".codex/skills/x/SKILL.md")).unwrap();
    link("../../../docs/same.md", repo.join(".codex/skills/x/SKILL.md"));
    assert_ne!(codex(&fx), before);
}

/// On a Codex tab only a link inside `.codex/skills/` is read through, and only by a way that stays
/// inside the top level.
#[test]
fn a_codex_skill_linked_any_other_way_is_not_followed() {
    let fx = Fixture::new("link-codex-other");
    write(&fx.home.join("secret.md"), b"s\n");
    write(&fx.home.join("outside/x/SKILL.md"), b"o\n");
    let codex_repo = |name: &str| {
        let repo = fx.home.join(name);
        fx.repo(&repo, false);
        write(&repo.join(".codex/config.toml"), b"model = \"m\"\n");
        write(&repo.join(".claude/skills/x/SKILL.md"), b"s\n");
        repo
    };
    let discover = |repo: &Path| fx.discover_as(repo, &Profile::Codex, &fx.codex_env());

    let repo = codex_repo("repo-out");
    link("../../../outside/x", repo.join(".codex/skills/x"));
    assert_link_not_followed(&discover(&repo), ".codex/skills/x", "../../../outside/x", true);

    let repo = codex_repo("repo-abs");
    let target = repo.join(".claude/skills/x");
    link(&target, repo.join(".codex/skills/x"));
    assert_link_not_followed(&discover(&repo), ".codex/skills/x", target.to_str().unwrap(), false);

    // The directory itself in place of `.codex/skills`, and a skill tree that is not `.codex/skills/`.
    let repo = codex_repo("repo-dir");
    link("../.claude/skills", repo.join(".codex/skills"));
    assert_link_not_followed(&discover(&repo), ".codex/skills", "../.claude/skills", false);
    let repo = codex_repo("repo-agents");
    link("../../.claude/skills/x", repo.join(".agents/skills/x"));
    assert_link_not_followed(&discover(&repo), ".agents/skills/x", "../../.claude/skills/x", false);

    // Back up over the link itself.
    let repo = codex_repo("repo-back");
    link("..", repo.join(".codex/skills/l"));
    assert_link_not_followed(&discover(&repo), ".codex/skills/l", "..", false);

    // A link in what was reached leaves from where it really is.
    let repo = codex_repo("repo-leaves");
    link("../../../../secret.md", repo.join(".claude/skills/x/out.md"));
    link("../../.claude/skills/x", repo.join(".codex/skills/x"));
    assert_link_not_followed(
        &discover(&repo),
        ".codex/skills/x/out.md",
        "../../../../secret.md",
        true,
    );

    // The top level may hold the home.
    let fx = Fixture::new("link-codex-home");
    fx.repo(&fx.home, false);
    let root = fx.home.join("sub");
    write(&root.join(".codex/config.toml"), b"model = \"m\"\n");
    write(&fx.home.join("account/SKILL.md"), b"a\n");
    link("../../../account", root.join(".codex/skills/a"));
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    assert_eq!(found.top, fx.home);
    assert_link_not_followed(&found, "sub/.codex/skills/a", "../../../account", false);
}

/// A linked worktree's main checkout is above the top level, so a skill linked there is not read through
/// and is judged from where it really is.
#[test]
fn a_codex_skill_link_in_the_main_checkout_is_not_followed() {
    let fx = Fixture::new("link-codex-main");
    let main = fx.home.join("main");
    fx.repo(&main, true);
    write(&main.join(".claude/skills/x/SKILL.md"), b"s\n");
    link("../../.claude/skills/x", main.join(".codex/skills/x"));
    let wt = fx.home.join("wt");
    fx.git(&main, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
    let found = fx.discover_as(&wt, &Profile::Codex, &fx.codex_env());
    assert_link_not_followed(&found, "../main/.codex/skills/x", "../../.claude/skills/x", true);
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
        Some(
            "more than 64 directories above the root without a repository; more than 64 directories above the \
             top level"
        )
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
    // Like any link below `.claude/`, it leads somewhere the fingerprint cannot see: one start only.
    assert!(!found.fully_hashed());
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

/// A repository holding every kind of thing the Claude profile reads (settings with a hook, local
/// settings, `.mcp.json`, memory files at two levels, a link, a nested file, Claude Code's worktree
/// directory) and the files only Codex reads, opened in `src`. Its Claude fingerprint is pinned below
/// as computed before discovery gained profiles, so an existing record keeps matching.
fn profile_fixture(fx: &Fixture) -> PathBuf {
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".claude/settings.json"), HOOK_SETTINGS);
    write(
        &repo.join(".claude/settings.local.json"),
        br#"{"permissions":{"allow":["Bash(ls)"]}}"#,
    );
    write(&repo.join(".claude/agents/reviewer.md"), b"agent\n");
    write(&repo.join(".claude/worktrees/wt/x"), b"x");
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

#[test]
fn claude_fingerprint_unchanged_byte_for_byte() {
    let fx = Fixture::new("claude-pin");
    let root = profile_fixture(&fx);
    let found = fx.discover(&root);
    assert_eq!(
        found.fingerprint.hex(),
        "ba47e9a4d5cbf5ca032f353e8bb1737fd39b760374dbe8f83fcc8d8ee6411a9a"
    );
}

fn has(discovery: &Discovery, path: &str) -> bool {
    discovery.entries.iter().any(|e| e.path == Path::new(path))
}

#[test]
fn codex_profile_reads_dot_codex_agents_skills_and_plugins() {
    let fx = Fixture::new("codex-reads");
    let root = profile_fixture(&fx);
    let repo = fx.home.join("repo");
    write(&repo.join(".codex/hooks.json"), br#"{"hooks":{}}"#);
    write(&repo.join(".codex/prompts/p.md"), b"prompt\n");
    write(&repo.join(".agents/plugins/p/plugin.json"), b"{}");
    write(&repo.join(".agents/other/x.md"), b"not Codex's\n");
    write(&root.join("AGENTS.override.md"), b"override\n");
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    for wanted in [
        "AGENTS.md",
        "src/AGENTS.override.md",
        ".codex/config.toml",
        ".codex/hooks.json",
        ".codex/prompts",
        ".codex/prompts/p.md",
        ".agents/skills/s",
        ".agents/skills/s/SKILL.md",
        ".agents/plugins/p",
        ".agents/plugins/p/plugin.json",
    ] {
        assert!(has(&found, wanted), "{wanted} missing from {:?}", paths(&found));
    }
    for unwanted in [
        ".claude/settings.json",
        ".mcp.json",
        "CLAUDE.md",
        "src/CLAUDE.local.md",
        ".agents/other/x.md",
    ] {
        assert!(!has(&found, unwanted), "{unwanted} read for Codex");
    }
    assert!(found.findings.contains(&Finding::Memory {
        path: PathBuf::from("AGENTS.md")
    }));
    assert!(found.findings.contains(&Finding::Memory {
        path: PathBuf::from("src/AGENTS.override.md")
    }));
    assert!(found.findings.contains(&Finding::Skill {
        path: PathBuf::from(".agents/skills/s"),
        name: "s".into()
    }));
    assert!(found.fully_hashed(), "{:?}", found.findings);
    assert_eq!(found.profile, Profile::Codex);

    // The Claude profile of the same tree reads none of it, and the two fingerprints differ.
    let claude = fx.discover(&root);
    assert!(!has(&claude, ".codex/config.toml") && !has(&claude, "AGENTS.md"));
    assert_ne!(claude.fingerprint, found.fingerprint);

    // A change to any Codex file changes the Codex fingerprint only.
    write(&repo.join(".agents/plugins/p/plugin.json"), b"{\"x\":1}");
    assert_ne!(
        fx.discover_as(&root, &Profile::Codex, &fx.codex_env()).fingerprint,
        found.fingerprint
    );
    assert_eq!(fx.discover(&root).fingerprint, claude.fingerprint);
}

#[test]
fn codex_profile_skips_home_dot_codex_only_when_it_is_codex_home() {
    let fx = Fixture::new("codex-home");
    // A home that is itself a repository, so the walk from a project in it reaches it.
    fx.repo(&fx.home, false);
    write(
        &fx.home.join(".codex/config.toml"),
        b"[mcp_servers.mine]\ncommand = \"my-server\"\n",
    );
    write(&fx.home.join(".agents/skills/mine/SKILL.md"), b"the user's own\n");
    write(&fx.home.join(".agents/plugins/marketplace.json"), b"{}");
    let root = fx.home.join("proj");
    write(&root.join(".agents/skills/theirs/SKILL.md"), b"the project's\n");

    let as_codex_home = ProfileEnv {
        codex_home: Some(fx.home.join(".codex")),
        fallback_names: Vec::new(),
        user_mcp_servers: None,
        user_mcp_names: None,
        user_config_unreadable: None,
    };
    let found = fx.discover_as(&root, &Profile::Codex, &as_codex_home);
    assert_eq!(found.top, fx.home);
    assert!(!has(&found, ".codex/config.toml"), "{:?}", paths(&found));
    assert!(found.findings.iter().any(
        |f| matches!(f, Finding::NotRead { path, reason } if path == Path::new(".codex") && reason.contains("CODEX_HOME"))
    ));
    assert!(!found.findings.iter().any(|f| matches!(f, Finding::McpServer { .. })));
    assert!(has(&found, "proj/.agents/skills/theirs/SKILL.md"));
    assert!(!has(&found, ".agents/skills/mine/SKILL.md"));
    assert!(!has(&found, ".agents/plugins/marketplace.json"));

    // With CODEX_HOME elsewhere, `$HOME/.codex` is a project layer like any other.
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    assert!(has(&found, ".codex/config.toml"), "{:?}", paths(&found));
    assert!(found
        .findings
        .iter()
        .any(|f| matches!(f, Finding::McpServer { name, .. } if name == "mine")));
    assert!(!has(&found, ".agents/skills/mine/SKILL.md"));

    // A `.codex/` that is CODEX_HOME and holds nothing else is nothing to trust.
    let only = fx.base.join("only");
    fx.repo(&only, false);
    let env = ProfileEnv {
        codex_home: Some(only.join(".codex")),
        fallback_names: Vec::new(),
        user_mcp_servers: None,
        user_mcp_names: None,
        user_config_unreadable: None,
    };
    write(&only.join(".codex/config.toml"), b"model = \"x\"\n");
    let lone = discover(&only, None, &Limits::default(), &Profile::Codex, &env);
    assert!(lone.nothing_to_trust(), "{:?}", lone.entries);
}

#[test]
fn a_linked_worktree_with_a_symlinked_git_directory_is_uncheckable_only_for_codex() {
    let fx = Fixture::new("codex-wt-link");
    let main = fx.home.join("main");
    fx.repo(&main, true);
    write(&main.join(".codex/hooks.json"), b"{}");
    let wt = fx.home.join("wt");
    fx.git(&main, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
    let gitfile = wt.join(".git");
    let contents = std::fs::read_to_string(&gitfile).unwrap();
    let gitdir = contents.strip_prefix("gitdir: ").unwrap().trim();
    std::fs::remove_file(&gitfile).unwrap();
    symlink(gitdir, &gitfile).unwrap();
    write(&wt.join(".codex/config.toml"), b"model = \"w\"\n");
    write(&wt.join("CLAUDE.md"), b"project instructions\n");

    let found = fx.discover_as(&wt, &Profile::Codex, &fx.codex_env());
    assert!(!found.fully_hashed());
    assert!(found.uncheckable().iter().any(|(path, reason)| {
        *path == Path::new(".git") && *reason == "a .git here could not be checked, so the main checkout whose .codex/ hooks Codex reads for a linked worktree cannot be found"
    }), "{:?}", found.uncheckable());
    let claude = fx.discover(&wt);
    assert!(!claude.nothing_to_trust());
    assert!(claude.fully_hashed(), "{:?}", claude.uncheckable());
}

#[test]
fn a_linked_worktree_with_symlinked_objects_is_uncheckable_only_for_codex() {
    let fx = Fixture::new("codex-wt-objects-link");
    let main = fx.home.join("main");
    fx.repo(&main, true);
    write(&main.join(".codex/hooks.json"), b"{}");
    let wt = fx.home.join("wt");
    fx.git(&main, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
    write(&wt.join(".codex/config.toml"), b"model = \"w\"\n");
    write(&wt.join("CLAUDE.md"), b"project instructions\n");
    let claude_before = fx.discover(&wt);
    let objects = main.join(".git/objects");
    let moved = main.join(".git/saved-objects");
    std::fs::rename(&objects, &moved).unwrap();
    symlink(&moved, &objects).unwrap();
    assert_eq!(
        fx.git(&wt, &["rev-parse", "--show-toplevel"]).trim(),
        wt.to_str().unwrap()
    );

    let found = fx.discover_as(&wt, &Profile::Codex, &fx.codex_env());
    assert!(!found.fully_hashed(), "{:?}", found.findings);
    let claude = fx.discover(&wt);
    assert!(claude.fully_hashed(), "{:?}", claude.uncheckable());
    assert_eq!(claude.fingerprint, claude_before.fingerprint);
    assert_eq!(claude.entries, claude_before.entries);
}

#[test]
fn codex_profile_reads_main_checkout_hooks_for_a_linked_worktree() {
    let fx = Fixture::new("codex-wt");
    let main = fx.home.join("main");
    fx.repo(&main, true);
    write(
        &main.join(".codex/hooks.json"),
        br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"main-hook.sh"}]}]}}"#,
    );
    write(&main.join("sub/.codex/config.toml"), b"model = \"m\"\n");
    let wt = fx.home.join("wt");
    fx.git(&main, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]);
    std::fs::create_dir_all(wt.join("sub")).unwrap();
    write(&wt.join(".codex/config.toml"), b"model = \"w\"\n");

    let found = fx.discover_as(&wt.join("sub"), &Profile::Codex, &fx.codex_env());
    assert_eq!(found.top, wt);
    for wanted in [
        ".codex/config.toml",
        "../main/.codex/hooks.json",
        "../main/sub/.codex/config.toml",
    ] {
        assert!(has(&found, wanted), "{wanted} missing from {:?}", paths(&found));
    }
    assert!(found.findings.contains(&Finding::Hook {
        file: PathBuf::from("../main/.codex/hooks.json"),
        event: "PreToolUse".into(),
        matcher: None,
        command: "main-hook.sh".into(),
    }));
    let before = found.fingerprint;
    write(
        &main.join(".codex/hooks.json"),
        br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"other.sh"}]}]}}"#,
    );
    assert_ne!(
        fx.discover_as(&wt.join("sub"), &Profile::Codex, &fx.codex_env())
            .fingerprint,
        before
    );

    // In the main checkout itself the files are read once, where they are.
    let in_main = fx.discover_as(&main, &Profile::Codex, &fx.codex_env());
    assert_eq!(paths(&in_main), vec![PathBuf::from(".codex/hooks.json")]);

    // A repository made with a separate git directory: Codex still takes the worktree's hooks from
    // the main checkout, which this walk cannot place, so the answer lasts one start.
    let repo = fx.home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    fx.git(
        &repo,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            repo.join(".git-data").to_str().unwrap(),
        ],
    );
    std::fs::write(repo.join("README.md"), b"readme\n").unwrap();
    fx.git(&repo, &["add", "README.md"]);
    fx.git(&repo, &["commit", "-q", "-m", "first"]);
    write(
        &repo.join(".codex/hooks.json"),
        br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"main-hook.sh"}]}]}}"#,
    );
    let separate = fx.home.join("separate-wt");
    fx.git(
        &repo,
        &["worktree", "add", "-q", "-b", "sep", separate.to_str().unwrap()],
    );
    write(&separate.join(".codex/config.toml"), b"model = \"w\"\n");
    let found = fx.discover_as(&separate, &Profile::Codex, &fx.codex_env());
    assert_eq!(found.top, separate);
    assert!(!found.fully_hashed(), "{:?}", found.findings);
    assert!(
        found
            .uncheckable()
            .iter()
            .any(|(path, reason)| *path == Path::new("../repo/.git-data") && reason.contains("main checkout")),
        "{:?}",
        found.uncheckable()
    );
    // Claude reads nothing from a main checkout, so the same worktree is fully hashed for it.
    let as_claude = fx.discover_as(&separate, &Profile::Claude, &ProfileEnv::default());
    assert!(as_claude.fully_hashed(), "{:?}", as_claude.findings);
}

#[test]
fn a_linked_codex_read_directory_cannot_be_checked() {
    let fx = Fixture::new("codex-linked-dirs");
    let outside = fx.base.join("outside");
    write(
        &outside.join("rules/allow.rules"),
        b"prefix_rule(pattern = [\"rm\"], decision = \"allow\")\n",
    );
    write(&outside.join("skills/s/SKILL.md"), b"skill\n");
    for dir in ["rules", "skills"] {
        let repo = fx.home.join(format!("repo-{dir}"));
        fx.repo(&repo, false);
        write(&repo.join(".codex/config.toml"), b"model = \"m\"\n");
        symlink(outside.join(dir), repo.join(".codex").join(dir)).unwrap();
        let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
        assert!(!found.fully_hashed(), "{dir}: {:?}", found.findings);
        let link = PathBuf::from(".codex").join(dir);
        assert!(
            found.uncheckable().iter().any(|(path, _)| *path == link),
            "{dir}: {:?}",
            found.uncheckable()
        );
    }
    // A link elsewhere in `.codex/` is hashed by its target, as before.
    let repo = fx.home.join("repo-other");
    fx.repo(&repo, false);
    write(&repo.join(".codex/config.toml"), b"model = \"m\"\n");
    symlink(outside.join("rules"), repo.join(".codex/notes")).unwrap();
    let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    assert!(found.fully_hashed(), "{:?}", found.findings);
}

#[test]
fn a_link_a_codex_skill_tree_does_not_read_through_cannot_be_checked() {
    let fx = Fixture::new("codex-linked-skills");
    // Codex follows links in a project's skill directories and reads what they lead to. Where the
    // fingerprint does not see that, each such link makes the answer last one start.
    let cases: [(&str, &str, &str); 4] = [
        (".agents/skills/review", "../../shared/review", "shared/review/SKILL.md"),
        (".agents/skills/y/SKILL.md", "../../../docs/skill.md", "docs/skill.md"),
        (
            ".codex/skills/x/SKILL.md",
            "../../../../held/skill.md",
            "../held/skill.md",
        ),
        (".codex/skills/x/refs", "../../../../held/refs", "../held/refs/a.md"),
    ];
    for (i, (link, target, held)) in cases.iter().enumerate() {
        let repo = fx.home.join(format!("repo-{i}"));
        fx.repo(&repo, false);
        write(&repo.join(held), b"text\n");
        write(&repo.join(".codex/config.toml"), b"model = \"m\"\n");
        write(&repo.join(".codex/skills/x/other.md"), b"x\n");
        write(&repo.join(".agents/skills/y/other.md"), b"y\n");
        symlink(target, repo.join(link)).unwrap();
        let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
        assert!(!found.fully_hashed(), "{link}: {:?}", found.findings);
        assert!(
            found.uncheckable().iter().any(|(path, _)| *path == Path::new(link)),
            "{link}: {:?}",
            found.uncheckable()
        );
    }
}

#[test]
fn a_dot_codex_linked_to_codex_home_is_not_read() {
    let fx = Fixture::new("codex-home-link");
    fx.repo(&fx.home, false);
    let dotfiles = fx.base.join("dotfiles/codex");
    write(
        &dotfiles.join("config.toml"),
        b"[mcp_servers.mine]\ncommand = \"my-server\"\n",
    );
    symlink(&dotfiles, fx.home.join(".codex")).unwrap();
    let root = fx.home.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let env = ProfileEnv {
        codex_home: Some(dotfiles.clone()),
        fallback_names: Vec::new(),
        user_mcp_servers: None,
        user_mcp_names: None,
        user_config_unreadable: None,
    };
    let found = fx.discover_as(&root, &Profile::Codex, &env);
    assert_eq!(found.top, fx.home);
    assert!(found.fully_hashed(), "{:?}", found.findings);
    assert!(found.nothing_to_trust(), "{:?}", found.entries);
    assert!(found.findings.iter().any(
        |f| matches!(f, Finding::NotRead { path, reason } if path == Path::new(".codex") && reason.contains("CODEX_HOME"))
    ));
    // A link to anywhere else is one the agent would follow.
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    assert!(!found.fully_hashed(), "{:?}", found.findings);
}

#[test]
fn marketplace_listings_are_fingerprinted_and_named() {
    let fx = Fixture::new("codex-listings");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    let listings = [
        ".agents/plugins/marketplace.json",
        ".agents/plugins/api_marketplace.json",
        ".claude-plugin/marketplace.json",
        ".cursor-plugin/marketplace.json",
    ];
    for listing in listings {
        write(&repo.join(listing), b"{\"plugins\":[]}");
    }
    write(&repo.join(".claude-plugin/plugin.json"), b"{}");
    write(&repo.join("src/.claude-plugin/marketplace.json"), b"{}");
    write(&repo.join("src/.agents/plugins/marketplace.json"), b"{}");
    let root = repo.join("src");
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    for listing in listings {
        assert!(has(&found, listing), "{listing} missing from {:?}", paths(&found));
        assert!(
            found.findings.contains(&Finding::PluginListing {
                path: PathBuf::from(listing)
            }),
            "{listing} not named"
        );
    }
    // Below the top only `.agents/plugins/` is Codex's, read as a plugin directory and not a listing.
    assert!(!has(&found, "src/.claude-plugin/marketplace.json"));
    assert!(!has(&found, ".claude-plugin/plugin.json"));
    assert!(has(&found, "src/.agents/plugins/marketplace.json"));
    assert!(!found.findings.contains(&Finding::PluginListing {
        path: PathBuf::from("src/.agents/plugins/marketplace.json")
    }));
    let before = found.fingerprint;
    write(&repo.join(".cursor-plugin/marketplace.json"), b"{\"plugins\":[1]}");
    assert_ne!(
        fx.discover_as(&root, &Profile::Codex, &fx.codex_env()).fingerprint,
        before
    );
    // A link in a listing's place is something Codex would follow: it cannot be checked.
    std::fs::remove_file(repo.join(".claude-plugin/marketplace.json")).unwrap();
    symlink("/etc/hostname", repo.join(".claude-plugin/marketplace.json")).unwrap();
    assert!(!fx.discover_as(&root, &Profile::Codex, &fx.codex_env()).fully_hashed());
}

#[test]
fn fallback_names_read_one_key_and_follow_no_link() {
    let fx = Fixture::new("codex-fallback");
    let codex_home = fx.base.join("codex-home");
    write(
        &codex_home.join("config.toml"),
        br#"model = "secret-model"
project_doc_fallback_filenames = ["TEAM.md", "TEAM.md", 7, "docs/AI.md"]

[mcp_servers.x]
command = "y"
"#,
    );
    assert_eq!(
        codex_fallback_names(&codex_home),
        vec!["TEAM.md".to_string(), "docs/AI.md".to_string()]
    );

    // The file through a link, a home through a link, a FIFO, bad TOML, a missing key: no names.
    let linked = fx.base.join("linked-home");
    std::fs::create_dir_all(&linked).unwrap();
    symlink(codex_home.join("config.toml"), linked.join("config.toml")).unwrap();
    assert!(codex_fallback_names(&linked).is_empty());
    let via_link = fx.base.join("home-link");
    symlink(&codex_home, &via_link).unwrap();
    assert!(codex_fallback_names(&via_link).is_empty());
    let fifo_home = fx.base.join("fifo-home");
    mkfifo(&fifo_home.join("config.toml"));
    let (tx, rx) = mpsc::channel();
    let probe = fifo_home.clone();
    std::thread::spawn(move || {
        let _ = tx.send(codex_fallback_names(&probe));
    });
    assert!(rx
        .recv_timeout(Duration::from_secs(2))
        .expect("blocked on a FIFO")
        .is_empty());
    let bad = fx.base.join("bad-home");
    write(&bad.join("config.toml"), b"project_doc_fallback_filenames = [");
    assert!(codex_fallback_names(&bad).is_empty());
    let none = fx.base.join("none-home");
    write(&none.join("config.toml"), b"model = \"x\"\n");
    assert!(codex_fallback_names(&none).is_empty());
    assert!(codex_fallback_names(&fx.base.join("missing")).is_empty());

    // Discovery reads a plain alternate name in every walked directory, and cannot check one that is
    // not a plain name, so trust in that configuration lasts one start.
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join("TEAM.md"), b"team\n");
    write(&repo.join("src/TEAM.md"), b"src team\n");
    let env = ProfileEnv {
        codex_home: Some(codex_home.clone()),
        fallback_names: vec!["TEAM.md".into()],
        user_mcp_servers: None,
        user_mcp_names: None,
        user_config_unreadable: None,
    };
    let found = fx.discover_as(&repo.join("src"), &Profile::Codex, &env);
    assert!(
        has(&found, "TEAM.md") && has(&found, "src/TEAM.md"),
        "{:?}",
        paths(&found)
    );
    assert!(found.findings.contains(&Finding::Memory {
        path: PathBuf::from("src/TEAM.md")
    }));
    assert!(found.fully_hashed());
    let env = ProfileEnv {
        codex_home: Some(codex_home),
        fallback_names: codex_fallback_names(&fx.base.join("codex-home")),
        user_mcp_servers: None,
        user_mcp_names: None,
        user_config_unreadable: None,
    };
    let found = fx.discover_as(&repo.join("src"), &Profile::Codex, &env);
    assert!(!found.fully_hashed());
    assert!(found
        .uncheckable()
        .iter()
        .any(|(path, _)| *path == Path::new("docs/AI.md")));
}

/// A project's own `.codex/config.toml` can name alternates for `AGENTS.md` too, and a trusted session
/// then loads them as instructions: discovery fingerprints each name it gives in every walked directory,
/// as it does the user's, and a name that is not one plain name makes the answer last one start.
#[test]
fn a_project_config_s_alternate_names_are_fingerprinted() {
    let fx = Fixture::new("project-fallback");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    let root = repo.join("src");
    write(&repo.join("TEAM.md"), b"team\n");
    write(&root.join("RULES.md"), b"rules\n");
    write(&repo.join("NOTES.md"), b"not named\n");
    // Named by the root's configuration and by the top's: both apply in every walked directory.
    write(
        &root.join(".codex/config.toml"),
        b"project_doc_fallback_filenames = [\"RULES.md\"]\n",
    );
    write(
        &repo.join(".codex/config.toml"),
        b"project_doc_fallback_filenames = [\"TEAM.md\", \"AGENTS.md\"]\n",
    );
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    assert!(
        has(&found, "TEAM.md") && has(&found, "src/RULES.md"),
        "{:?}",
        paths(&found)
    );
    assert!(!has(&found, "NOTES.md"), "{:?}", paths(&found));
    for path in ["TEAM.md", "src/RULES.md"] {
        assert!(
            found.findings.contains(&Finding::Memory {
                path: PathBuf::from(path)
            }),
            "{path}: {:?}",
            found.findings
        );
    }
    assert!(found.fully_hashed());

    // The named file is part of the fingerprint: changing it asks again.
    write(&repo.join("TEAM.md"), b"team, changed\n");
    let changed = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    assert_ne!(changed.fingerprint, found.fingerprint);

    // A name that reaches outside the directory cannot be checked.
    write(
        &repo.join(".codex/config.toml"),
        b"project_doc_fallback_filenames = [\"../outside.md\"]\n",
    );
    let refused = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    assert!(!refused.fully_hashed());

    // The Claude profile reads none of it.
    let claude = fx.discover(&root);
    assert!(
        !has(&claude, "TEAM.md") && !has(&claude, "src/RULES.md"),
        "{:?}",
        paths(&claude)
    );
}

const CODEX_CONFIG: &[u8] = br#"model = "o3"
sandbox_mode = "danger-full-access"
approval_policy = "never"

[features]
exec_permission_approvals = true
web_search = true

[mcp_servers.marker]
command = "sh"
args = ["-c", "touch /tmp/n"]
default_tools_approval_mode = "approve"

[mcp_servers.marker.tools.write]
approval_mode = "prompt"

[mcp_servers.remote]
url = "https://example.com/mcp"
"#;

#[test]
fn toml_findings_name_mcp_servers_and_approval_modes() {
    let fx = Fixture::new("codex-toml");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".codex/config.toml"), CODEX_CONFIG);
    let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    let file = PathBuf::from(".codex/config.toml");
    let expected = [
        Finding::McpServer {
            file: file.clone(),
            name: "marker".into(),
            // The approval keys are shown beside the command as well: a tool's table can hold more
            // than its `approval_mode`, and every field of a server is part of what it is.
            command_line:
                r#"sh -c 'touch /tmp/n' {"default_tools_approval_mode":"approve","tools":{"write":{"approval_mode":"prompt"}}}"#
                    .into(),
            approval_mode: Some("approve; write: prompt".into()),
        },
        Finding::McpServer {
            file: file.clone(),
            name: "remote".into(),
            command_line: "http https://example.com/mcp".into(),
            approval_mode: None,
        },
        Finding::Overridden {
            file: file.clone(),
            key: "sandbox_mode".into(),
            value: "\"danger-full-access\"".into(),
        },
        Finding::Overridden {
            file: file.clone(),
            key: "approval_policy".into(),
            value: "\"never\"".into(),
        },
        Finding::Overridden {
            file: file.clone(),
            key: "features.exec_permission_approvals".into(),
            value: "true".into(),
        },
        Finding::OtherSetting {
            file: file.clone(),
            key: "features.web_search".into(),
            value: "true".into(),
        },
        Finding::OtherSetting {
            file: file.clone(),
            key: "model".into(),
            value: "\"o3\"".into(),
        },
    ];
    for finding in &expected {
        assert!(
            found.findings.contains(finding),
            "{finding:?} missing from {:?}",
            found.findings
        );
    }
    let plan = trust_plan(&found, DirTrust::Untrusted, &repo).unwrap();
    assert_eq!(plan.project_only, vec!["marker".to_string(), "remote".to_string()]);

    write(&repo.join(".codex/config.toml"), b"[mcp_servers\n");
    let broken = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    assert!(broken
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Unparsed { file: f, .. } if *f == file)));
}

#[test]
fn hooks_json_and_config_toml_hooks_both_found() {
    let fx = Fixture::new("codex-hooks");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(
        &repo.join(".codex/config.toml"),
        br#"[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "toml-hook.sh"

[hooks.state.some-hook]
trusted_hash = "abc"
"#,
    );
    write(
        &repo.join(".codex/hooks.json"),
        br#"{"description":"d","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"json-hook.sh"}]}]}}"#,
    );
    let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    assert!(found.findings.contains(&Finding::Hook {
        file: PathBuf::from(".codex/config.toml"),
        event: "PreToolUse".into(),
        matcher: Some("Bash".into()),
        command: "toml-hook.sh".into(),
    }));
    assert!(found.findings.contains(&Finding::Hook {
        file: PathBuf::from(".codex/hooks.json"),
        event: "SessionStart".into(),
        matcher: None,
        command: "json-hook.sh".into(),
    }));
    assert!(found
        .findings
        .iter()
        .any(|f| matches!(f, Finding::OtherSetting { key, .. } if key == "hooks.state")));
    assert!(!found
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Hook { event, .. } if event == "state")));
}

#[test]
fn rule_file_calls_shown_verbatim() {
    let fx = Fixture::new("codex-rules");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    let rules = "# prefix_rule(pattern = [\"commented\"])\nprefix_rule(\n    pattern = [\"touch\", \"/outside\"],\n    decision = \"allow\",\n)\nprefix_rule(pattern=[\"git\", \"push\"], decision=\"prompt\")\n";
    write(&repo.join(".codex/rules/default.rules"), rules.as_bytes());
    write(&repo.join(".codex/rules/notes.txt"), b"prefix_rule(pattern=[\"x\"])\n");
    write(
        &repo.join(".codex/rules/deeper/more.rules"),
        b"prefix_rule(pattern=[\"y\"])\n",
    );
    // Rules a scan for calls cannot see: one reached through another name, and one a function makes.
    let aliased = "r = prefix_rule\nr(pattern = [\"rm\"], decision = \"allow\")\n";
    write(&repo.join(".codex/rules/aliased.rules"), aliased.as_bytes());
    let wrapped = "def allow(cmd):\n    prefix_rule(pattern = [cmd], decision = \"allow\")\n\nallow(\"rm\")\n";
    write(&repo.join(".codex/rules/wrapped.rules"), wrapped.as_bytes());
    write(&repo.join(".codex/rules/empty.rules"), b"# nothing yet\n\n");
    let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    let rule_files: Vec<&Finding> = found
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::RuleFile { .. }))
        .collect();
    assert_eq!(
        rule_files,
        vec![
            &Finding::RuleFile {
                path: PathBuf::from(".codex/rules/aliased.rules"),
                calls: vec![],
                text: Some(aliased.to_string()),
            },
            &Finding::RuleFile {
                path: PathBuf::from(".codex/rules/default.rules"),
                calls: vec![
                    "prefix_rule(\n    pattern = [\"touch\", \"/outside\"],\n    decision = \"allow\",\n)".into(),
                    "prefix_rule(pattern=[\"git\", \"push\"], decision=\"prompt\")".into(),
                ],
                text: None,
            },
            &Finding::RuleFile {
                path: PathBuf::from(".codex/rules/empty.rules"),
                calls: vec![],
                text: None,
            },
            &Finding::RuleFile {
                path: PathBuf::from(".codex/rules/wrapped.rules"),
                calls: vec!["prefix_rule(pattern = [cmd], decision = \"allow\")".into()],
                text: Some(wrapped.to_string()),
            },
        ]
    );
    // The other files are still hashed.
    assert!(has(&found, ".codex/rules/notes.txt") && has(&found, ".codex/rules/deeper/more.rules"));
}

/// The user's own MCP servers are read from their `config.toml` by name only, leaving out one
/// switched off; a file that is not there defines none, and one that cannot be read leaves them
/// unknown.
#[test]
fn the_user_config_names_its_mcp_servers() {
    let fx = Fixture::new("codex-user-servers");
    let file = fx.base.join("codex-home/config.toml");
    write(
        &file,
        br#"model = "secret-model"
project_doc_fallback_filenames = ["TEAM.md"]

[mcp_servers.context7]
command = "npx"
args = ["secret-arg"]

[mcp_servers.off]
command = "x"
enabled = false

[mcp_servers.alpha]
url = "https://example.com/mcp"
"#,
    );
    let read = read_user_codex_config(&file);
    assert_eq!(read.fallback_names, vec!["TEAM.md".to_string()]);
    assert_eq!(
        read.mcp_server_names(),
        Some(vec!["alpha".to_string(), "context7".to_string()])
    );

    let none = fx.base.join("none/config.toml");
    write(&none, b"model = \"x\"\n");
    assert_eq!(read_user_codex_config(&none).mcp_server_names(), Some(Vec::new()));
    assert_eq!(
        read_user_codex_config(&fx.base.join("missing/config.toml")).mcp_server_names(),
        Some(Vec::new())
    );

    // A link (not followed here), bad TOML, a FIFO, a key that is not a table: unknown.
    let linked = fx.base.join("linked/config.toml");
    std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
    symlink(&file, &linked).unwrap();
    assert_eq!(read_user_codex_config(&linked).mcp_servers, None);
    let bad = fx.base.join("bad/config.toml");
    write(&bad, b"[mcp_servers\n");
    assert_eq!(read_user_codex_config(&bad).mcp_servers, None);
    // A file there that cannot be read says so, naming itself and not its text; one not there does not.
    let (file, why) = read_user_codex_config(&bad).unreadable.expect("a gap");
    assert_eq!(file, bad);
    assert!(!why.contains("mcp_servers"), "{why}");
    assert!(read_user_codex_config(&linked).unreadable.is_some());
    assert_eq!(read_user_codex_config(&none).unreadable, None);
    assert_eq!(
        read_user_codex_config(&fx.base.join("missing/config.toml")).unreadable,
        None
    );
    let odd = fx.base.join("odd/config.toml");
    write(&odd, b"mcp_servers = 3\n");
    assert_eq!(read_user_codex_config(&odd).mcp_servers, None);
    let fifo = fx.base.join("fifo-home");
    mkfifo(&fifo.join("config.toml"));
    let (tx, rx) = mpsc::channel();
    let probe = fifo.join("config.toml");
    std::thread::spawn(move || {
        let _ = tx.send(read_user_codex_config(&probe));
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2))
            .expect("blocked on a FIFO")
            .mcp_servers,
        None
    );
}

/// The MCP servers of a user `config.toml` holding `text`, as the trust worker reads them.
fn user_servers(fx: &Fixture, name: &str, text: &[u8]) -> Option<std::collections::BTreeMap<String, ServerDefinition>> {
    let file = fx.base.join(format!("user-{name}/config.toml"));
    write(&file, text);
    read_user_codex_config(&file).mcp_servers
}

/// Both configurations reach the check of the editor's server name through a discovery.
#[test]
fn a_discovery_says_whether_the_editor_servers_name_is_taken() {
    let fx = Fixture::new("eitri-name-taken");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    let env = |names: &[&str]| ProfileEnv {
        user_mcp_names: Some(names.iter().map(|name| name.to_string()).collect()),
        ..fx.codex_env()
    };
    let taken = |names: &[&str]| eitri_server_taken(&fx.discover_as(&repo, &Profile::Codex, &env(names)));
    assert!(!taken(&["mine"]));
    assert!(taken(&["mine", "eitri"]));
    write(
        &repo.join(".codex/config.toml"),
        "[mcp_servers.eitri]\ncommand = \"x\"\n".as_bytes(),
    );
    assert!(taken(&["mine"]));
    let found = fx.discover_as(&repo, &Profile::Codex, &env(&["mine"]));
    assert_eq!(eitri_server_name(&found), EditorServerName::Defined);
}

/// A project `.codex/config.toml` discovery could not read may define the name as well as one it read:
/// Codex follows a link and has a parser of its own, so what is unknown here counts as taken.
#[test]
fn a_project_configuration_that_went_unread_leaves_the_name_taken() {
    let fx = Fixture::new("eitri-name-unread");
    let env = ProfileEnv {
        user_mcp_names: Some(Default::default()),
        ..fx.codex_env()
    };
    let elsewhere = fx.base.join("elsewhere/config.toml");
    let definition = "[mcp_servers.eitri]\ncommand = \"x\"\n";
    write(&elsewhere, definition.as_bytes());
    let fresh = |name: &str| {
        let repo = fx.home.join(name);
        fx.repo(&repo, false);
        repo
    };
    let taken = |root: &Path| eitri_server_taken(&fx.discover_as(root, &Profile::Codex, &env));

    let linked_file = fresh("linked-file");
    std::fs::create_dir_all(linked_file.join(".codex")).unwrap();
    symlink(&elsewhere, linked_file.join(".codex/config.toml")).unwrap();
    assert!(
        taken(&linked_file),
        "a link to a configuration whose servers were never read"
    );

    let linked_dir = fresh("linked-dir");
    symlink(elsewhere.parent().unwrap(), linked_dir.join(".codex")).unwrap();
    assert!(taken(&linked_dir), "a link in place of the whole directory");

    let above_root = fresh("above-root");
    let root = above_root.join("sub");
    std::fs::create_dir_all(above_root.join(".codex")).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    symlink(&elsewhere, above_root.join(".codex/config.toml")).unwrap();
    assert!(taken(&root), "a link in a directory above the root, on the walk");

    let bad_toml = fresh("bad-toml");
    write(&bad_toml.join(".codex/config.toml"), b"not toml [");
    assert!(taken(&bad_toml), "a file this parser refuses and Codex's may not");
    assert_eq!(
        eitri_server_name(&fx.discover_as(&bad_toml, &Profile::Codex, &env)),
        EditorServerName::Unknown,
        "nothing says the name is there"
    );

    let too_large = fresh("too-large");
    write(&too_large.join(".codex/config.toml"), definition.as_bytes());
    let small = Limits {
        max_file_bytes: 8,
        ..Limits::default()
    };
    let found = discover(&too_large, Some(&fx.home), &small, &Profile::Codex, &env);
    assert!(eitri_server_taken(&found), "a file over the budget is never read");

    let crowded = fresh("crowded");
    write(&crowded.join("AGENTS.md"), b"memory");
    write(&crowded.join(".codex/config.toml"), definition.as_bytes());
    let one_entry = Limits {
        max_entries: 1,
        ..Limits::default()
    };
    let found = discover(&crowded, Some(&fx.home), &one_entry, &Profile::Codex, &env);
    assert!(
        eitri_server_taken(&found),
        "a walk out of entries never looked at the file"
    );
}

/// Only what could hold a server definition counts: a link to a memory file is common and defines none.
#[test]
fn a_gap_elsewhere_leaves_the_name_free() {
    let fx = Fixture::new("eitri-name-free");
    let env = ProfileEnv {
        user_mcp_names: Some(Default::default()),
        ..fx.codex_env()
    };
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join("memory.md"), b"memory");
    symlink("memory.md", repo.join("AGENTS.md")).unwrap();
    write(
        &repo.join(".codex/config.toml"),
        b"[mcp_servers.mine]\ncommand = \"x\"\n",
    );
    symlink("elsewhere.json", repo.join(".codex/hooks.json")).unwrap();
    let found = fx.discover_as(&repo, &Profile::Codex, &env);
    assert!(!found.fully_hashed(), "the links are gaps in what was hashed");
    assert!(!eitri_server_taken(&found));
}

/// A server the project defines exactly as the user's own configuration does is no sign that the
/// project's configuration loaded, so the plan does not watch for it; when the user's servers are
/// not known, every project server is watched for.
#[test]
fn trust_plan_leaves_out_servers_the_user_also_runs() {
    let fx = Fixture::new("plan-user-servers");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".codex/config.toml"), CODEX_CONFIG);
    let env = |user| ProfileEnv {
        user_mcp_servers: user,
        user_mcp_names: None,
        ..fx.codex_env()
    };

    // The same table, written in another order and layout, is the same definition.
    let same = user_servers(
        &fx,
        "same",
        br#"[mcp_servers.mine]
command = "mine"

[mcp_servers.marker]
default_tools_approval_mode = "approve"
args = [ "-c", "touch /tmp/n" ]
command = "sh"
tools = { write = { approval_mode = "prompt" } }
"#,
    );
    let found = fx.discover_as(&repo, &Profile::Codex, &env(same));
    let plan = trust_plan(&found, DirTrust::Untrusted, &repo).unwrap();
    assert_eq!(plan.project_only, vec!["remote".to_string()]);

    let unknown = fx.discover_as(&repo, &Profile::Codex, &env(None));
    assert_eq!(
        unknown.fingerprint, found.fingerprint,
        "the user's servers are not the project's"
    );
    let plan = trust_plan(&unknown, DirTrust::Untrusted, &repo).unwrap();
    assert_eq!(plan.project_only, vec!["marker".to_string(), "remote".to_string()]);
}

/// A project that defines a server under the name of one of the user's own, but otherwise, starts its
/// own server under that name: the plan watches for it, so an untrusted session that starts it stops.
#[test]
fn trust_plan_watches_a_project_server_that_redefines_one_of_the_users() {
    let fx = Fixture::new("plan-shadowed-server");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    write(&repo.join(".codex/config.toml"), CODEX_CONFIG);
    let env = |user| ProfileEnv {
        user_mcp_servers: user,
        user_mcp_names: None,
        ..fx.codex_env()
    };
    for (what, text) in [
        (
            "another command",
            &br#"[mcp_servers.marker]
command = "my-marker"
"#[..],
        ),
        (
            "one more field",
            br#"[mcp_servers.marker]
command = "sh"
args = ["-c", "touch /tmp/n"]
default_tools_approval_mode = "approve"
tools = { write = { approval_mode = "prompt" } }
cwd = "/x"
"#,
        ),
        (
            "an approval mode of its own",
            br#"[mcp_servers.marker]
command = "sh"
args = ["-c", "touch /tmp/n"]
default_tools_approval_mode = "prompt"
tools = { write = { approval_mode = "prompt" } }
"#,
        ),
    ] {
        let found = fx.discover_as(&repo, &Profile::Codex, &env(user_servers(&fx, "other", text)));
        let plan = trust_plan(&found, DirTrust::Untrusted, &repo).unwrap();
        assert_eq!(
            plan.project_only,
            vec!["marker".to_string(), "remote".to_string()],
            "{what}"
        );
    }

    // A walked directory above the root that defines the name otherwise counts too, even when the
    // root's own definition is the user's.
    let root = repo.join("sub");
    std::fs::create_dir_all(&root).unwrap();
    write(
        &root.join(".codex/config.toml"),
        b"[mcp_servers.marker]\ncommand = \"my-marker\"\n",
    );
    let user = user_servers(&fx, "mine", b"[mcp_servers.marker]\ncommand = \"my-marker\"\n");
    let found = fx.discover_as(&root, &Profile::Codex, &env(user));
    let plan = trust_plan(&found, DirTrust::Untrusted, &root).unwrap();
    assert_eq!(plan.project_only, vec!["marker".to_string(), "remote".to_string()]);
}

#[test]
fn trust_plan_untrusted_lists_every_ancestor() {
    let fx = Fixture::new("plan-untrusted");
    let root = profile_fixture(&fx);
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    let plan = trust_plan(&found, DirTrust::Untrusted, &root).unwrap();
    let want: Vec<(PathBuf, DirTrust)> = root
        .ancestors()
        .map(|dir| (dir.to_path_buf(), DirTrust::Untrusted))
        .collect();
    assert_eq!(plan.dirs, want);
    assert_eq!(plan.dirs.first().map(|(d, _)| d.clone()), Some(root.clone()));
    assert_eq!(plan.dirs.last().map(|(d, _)| d.clone()), Some(PathBuf::from("/")));
    assert!(plan.skills_off);
    assert_eq!(plan.fingerprint, found.fingerprint.hex());

    // A root with nothing to trust gets the same plan.
    let bare = fx.home.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let nothing = fx.discover_as(&bare, &Profile::Codex, &fx.codex_env());
    assert!(nothing.nothing_to_trust());
    let plan = trust_plan(&nothing, DirTrust::Untrusted, &bare).unwrap();
    assert!(plan.dirs.iter().all(|(_, level)| *level == DirTrust::Untrusted));
    assert_eq!(plan.dirs.len(), bare.ancestors().count());
}

#[test]
fn trust_plan_trusted_trusts_walked_untrusts_above() {
    let fx = Fixture::new("plan-trusted");
    let root = profile_fixture(&fx);
    let repo = fx.home.join("repo");
    let found = fx.discover_as(&root, &Profile::Codex, &fx.codex_env());
    assert_eq!(found.walked, vec![root.clone(), repo.clone()]);
    let plan = trust_plan(&found, DirTrust::Trusted, &root).unwrap();
    let mut want = vec![(root.clone(), DirTrust::Trusted), (repo.clone(), DirTrust::Trusted)];
    want.extend(
        repo.ancestors()
            .skip(1)
            .map(|dir| (dir.to_path_buf(), DirTrust::Untrusted)),
    );
    assert_eq!(plan.dirs, want);
    assert!(!plan.skills_off);
    assert_eq!(plan.project_only, Vec::<String>::new());

    // A root that is not UTF-8 cannot be named exactly, so there is no plan for it.
    let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/nonexistent/\xff"));
    assert!(trust_plan(&found, DirTrust::Trusted, &odd).is_err());
    assert!(trust_plan(&found, DirTrust::Untrusted, &odd).is_err());
}

/// Memory and servers above the git top level: what a trusted CLI loads from there (measured by
/// `workspace_trust_memory_real_cli`) and nothing else.
fn above_the_top(outer: &Path) {
    write(&outer.join("CLAUDE.md"), b"above\n");
    write(&outer.join("CLAUDE.local.md"), b"above, local\n");
    write(
        &outer.join(".mcp.json"),
        br#"{"mcpServers":{"above":{"command":"srv"}}}"#,
    );
    write(&outer.join(".claude/CLAUDE.md"), b"above, in .claude\n");
    write(&outer.join(".claude/rules/a.md"), b"rule a\n");
    write(&outer.join(".claude/rules/deeper/b.md"), b"rule b\n");
    // Loaded only from the working directory or the top level, never from above it.
    write(&outer.join(".claude/settings.json"), HOOK_SETTINGS);
    write(&outer.join(".claude/settings.local.json"), HOOK_SETTINGS);
    write(&outer.join(".claude/skills/s/SKILL.md"), b"---\nname: s\n---\n");
    write(&outer.join(".claude/agents/a.md"), b"---\nname: a\n---\n");
    write(&outer.join(".claude/commands/c.md"), b"do it\n");
}

const ABOVE: [&str; 7] = [
    "../.claude/CLAUDE.md",
    "../.claude/rules/a.md",
    "../.claude/rules/deeper",
    "../.claude/rules/deeper/b.md",
    "../.mcp.json",
    "../CLAUDE.local.md",
    "../CLAUDE.md",
];

#[test]
fn memory_and_servers_above_the_top_level_are_read() {
    let fx = Fixture::new("above");
    let outer = fx.home.join("outer");
    above_the_top(&outer);
    let repo = outer.join("repo");
    fx.repo(&repo, false);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    for root in [repo.clone(), repo.join("src")] {
        let found = fx.discover(&root);
        assert_eq!(found.top, repo);
        assert_eq!(paths(&found), ABOVE.map(PathBuf::from).to_vec());
        assert!(found.fully_hashed());
        assert!(!found.nothing_to_trust());
        assert!(found
            .findings
            .iter()
            .any(|f| matches!(f, Finding::McpServer { name, command_line, .. }
            if name == "above" && command_line == "srv")));
        // The walk for the git top level is unchanged.
        assert_eq!(found.walked.last(), Some(&repo));
    }

    // Each of them is in the fingerprint.
    let before = fx.discover(&repo).fingerprint;
    write(&outer.join(".claude/rules/deeper/b.md"), b"rule b, changed\n");
    let after = fx.discover(&repo).fingerprint;
    assert_ne!(before, after);
    write(&outer.join("CLAUDE.md"), b"above, changed\n");
    assert_ne!(after, fx.discover(&repo).fingerprint);
}

#[test]
fn memory_above_a_root_without_a_repository_is_read() {
    let fx = Fixture::new("above-norepo");
    let outer = fx.home.join("outer");
    above_the_top(&outer);
    let root = outer.join("q");
    std::fs::create_dir_all(&root).unwrap();
    let found = fx.discover(&root);
    assert_eq!(found.top, root);
    assert_eq!(paths(&found), ABOVE.map(PathBuf::from).to_vec());
}

#[test]
fn memory_further_up_is_read_and_home_dot_claude_is_left_out() {
    let fx = Fixture::new("above-home");
    // At the home: its memory files and server are read, its `.claude/` is the user tier's own.
    write(&fx.home.join("CLAUDE.md"), b"home memory\n");
    write(
        &fx.home.join(".mcp.json"),
        br#"{"mcpServers":{"home":{"command":"h"}}}"#,
    );
    write(&fx.home.join(".claude/CLAUDE.md"), b"user memory\n");
    write(&fx.home.join(".claude/rules/r.md"), b"user rule\n");
    // Above the home, and two levels above the top.
    write(&fx.base.join("CLAUDE.local.md"), b"higher\n");
    write(&fx.base.join(".claude/rules/r.md"), b"higher rule\n");
    let repo = fx.home.join("a/repo");
    fx.repo(&repo, false);
    let found = fx.discover(&repo);
    assert_eq!(found.top, repo);
    assert_eq!(
        paths(&found),
        [
            "../../../.claude/rules/r.md",
            "../../../CLAUDE.local.md",
            "../../.mcp.json",
            "../../CLAUDE.md",
        ]
        .map(PathBuf::from)
        .to_vec()
    );
}

#[test]
fn a_link_above_the_top_level_is_reported_and_not_followed() {
    let fx = Fixture::new("above-link");
    let elsewhere = fx.base.join("elsewhere");
    write(&elsewhere.join("CLAUDE.md"), b"followed?\n");
    write(&elsewhere.join("rules/r.md"), b"followed?\n");
    let outer = fx.home.join("outer");
    std::fs::create_dir_all(&outer).unwrap();
    symlink(elsewhere.join("CLAUDE.md"), outer.join("CLAUDE.md")).unwrap();
    symlink(&elsewhere, outer.join(".claude")).unwrap();
    let repo = outer.join("repo");
    fx.repo(&repo, false);
    let watch = OpenWatch::new(&elsewhere.join("CLAUDE.md"));
    let found = fx.discover(&repo);
    assert!(
        !watch.saw_anything() || !OpenWatch::WORKS,
        "a link above the top was followed"
    );
    assert_eq!(
        paths(&found),
        ["../.claude", "../CLAUDE.md"].map(PathBuf::from).to_vec()
    );
    assert!(
        found
            .entries
            .iter()
            .all(|e| matches!(e.kind, EntryKind::Symlink { .. })),
        "{:?}",
        found.entries
    );
    // The CLI would follow either link to content the fingerprint cannot see.
    assert!(!found.fully_hashed());
}

#[test]
fn too_many_directories_above_the_top_level_are_over_budget() {
    let fx = Fixture::new("above-budget");
    let repo = fx.home.join("repo");
    fx.repo(&repo, false);
    let limits = Limits {
        max_walk_up: 1,
        ..Limits::default()
    };
    let found = discover(&repo, Some(&fx.home), &limits, &Profile::Claude, &ProfileEnv::default());
    assert!(!found.fully_hashed());
    assert!(
        found
            .findings
            .iter()
            .any(|f| matches!(f, Finding::OverBudget { reason } if reason.contains("above the top level"))),
        "{:?}",
        found.findings
    );
}

#[test]
fn codex_reads_nothing_above_the_top_level() {
    let fx = Fixture::new("above-codex");
    let outer = fx.home.join("outer");
    above_the_top(&outer);
    write(&outer.join("AGENTS.md"), b"above\n");
    let repo = outer.join("repo");
    fx.repo(&repo, false);
    let found = fx.discover_as(&repo, &Profile::Codex, &fx.codex_env());
    assert!(
        paths(&found).iter().all(|p| !p.starts_with("..")),
        "{:?}",
        paths(&found)
    );
}
