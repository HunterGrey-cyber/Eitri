//! Red-team tests: a server writes what it likes into its repository and its tree, the replica copies `.git` as the
//! git session's ignores allow and the tree as the tree session's do, and Eitri writes the laptop's config. After
//! that, no git run on the laptop -- through `local_git`, or plain git by absolute path as a plugin would run it --
//! may run a program the server planted.
//!
//! Every scenario first shows that its payload is live: plain git on the server runs it. Without that, a payload git
//! never ran (a wrong hook name, a git that ignores the setting) would pass for a defence.
//!
//! The replica is made by a model of Mutagen's rules (a pattern without `/` matches a name at any depth, one with a
//! `/` matches from the session root, a trailing `/` matches directories only; symlink mode `ignore` copies no link,
//! `portable` copies a relative link whose text stays inside the root); `red_team_holds_on_a_real_mutagen_replica`
//! runs the same scenarios through Mutagen itself.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use super::{
    git_session_plan, parse_config_list, write_laptop_config, GIT_SESSION_IGNORES, GIT_SESSION_MODE, TREE_VCS_IGNORES,
};
use crate::remote_sync::local_git::{with_flags_last, LocalGit};
use crate::remote_sync::SyncKey;

pub(super) struct Scratch(PathBuf);

impl Scratch {
    pub(super) fn new(tag: &str) -> Self {
        Self::under(&std::env::temp_dir(), tag)
    }

    fn under(base: &Path, tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = base.join(format!(
            "eitri-git-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn git_binary() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// An environment with its own empty home, so the machine's global and system git config play no part.
fn isolated_env(home: &Path) -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("HOME".into(), home.into()),
        ("XDG_CONFIG_HOME".into(), home.join(".config").into()),
        ("LANG".into(), "C".into()),
    ]
}

pub(super) fn isolated_local_git(home: &Path) -> LocalGit {
    LocalGit::with_environment(git_binary(), isolated_env(home))
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn write_exec(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn append(path: &Path, text: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else if !exists(&target) {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Hooks git runs during reads, index writes, commits and ref updates.
const HOOKS: &[&str] = &[
    "post-index-change",
    "pre-commit",
    "post-commit",
    "post-checkout",
    "reference-transaction",
    "pre-auto-gc",
    "fsmonitor-watchman",
];

/// One server, one laptop mirror, a payload that appends to a marker when anything runs it.
struct Lab {
    _scratch: Scratch,
    home: PathBuf,
    server: PathBuf,
    laptop: PathBuf,
    marker: PathBuf,
    payload: PathBuf,
    saves: AtomicU32,
}

impl Lab {
    fn new(tag: &str) -> Self {
        let scratch = Scratch::new(tag);
        let base = scratch.path().to_owned();
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        // A detached automatic gc or maintenance run would repack objects while a scenario copies them.
        std::fs::write(
            home.join(".gitconfig"),
            "[gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n",
        )
        .unwrap();
        let marker = base.join("marker");
        let payload = base.join("payload.sh");
        write_exec(
            &payload,
            &format!("#!/bin/sh\necho \"payload $*\" >> '{}'\nexit 1\n", marker.display()),
        );
        let server = base.join("server");
        let laptop = base.join("laptop");
        std::fs::create_dir_all(&laptop).unwrap();
        let lab = Self {
            _scratch: scratch,
            home,
            server,
            laptop,
            marker,
            payload,
            saves: AtomicU32::new(0),
        };
        lab.setup_git(&base, &["init", "-q", "-b", "main", "server"]);
        std::fs::write(lab.server.join("a.txt"), "a\n").unwrap();
        std::fs::write(lab.server.join("b.txt"), "b\n").unwrap();
        lab.setup_git(&lab.server, &["add", "-A"]);
        lab.setup_git(&lab.server, &["commit", "-q", "-m", "initial"]);
        lab
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(git_binary());
        command
            .env_clear()
            .envs(isolated_env(&self.home))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(cwd)
            .args(args);
        command
    }

    /// Plain Git with an identity for commands that create commit objects.
    fn setup_command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = self.command(cwd, args);
        command
            .env("GIT_AUTHOR_NAME", "Someone")
            .env("GIT_AUTHOR_EMAIL", "someone@example.com")
            .env("GIT_COMMITTER_NAME", "Someone")
            .env("GIT_COMMITTER_EMAIL", "someone@example.com");
        command
    }

    /// [`Self::setup_command`], which must succeed.
    fn setup_git(&self, cwd: &Path, args: &[&str]) {
        let output = self.setup_command(cwd, args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Git as a plugin runs it by absolute path: nothing of Eitri's.
    fn plain_git(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }

    /// `local_git` from the laptop root.
    fn local(&self, args: &[&str]) -> Output {
        isolated_local_git(&self.home)
            .command(&self.laptop, &os(args))
            .unwrap()
            .current_dir(&self.laptop)
            .output()
            .unwrap()
    }

    /// The directory holding the server, the laptop and the payload: outside both mirrors.
    fn base(&self) -> PathBuf {
        self.server.parent().unwrap().to_owned()
    }

    fn commit_change(&self, text: &str) {
        std::fs::write(self.server.join("b.txt"), text).unwrap();
        self.setup_git(&self.server, &["commit", "-q", "-a", "-m", text.trim_end()]);
    }

    fn ran(&self) -> bool {
        self.marker.exists()
    }

    fn forget(&self) {
        let _ = std::fs::remove_file(&self.marker);
    }

    fn hook_script(&self) -> String {
        format!("#!/bin/sh\necho \"hook $0\" >> '{}'\n", self.marker.display())
    }

    /// Config naming the payload everywhere git would run it, for a `diff=evil filter=evil` attribute too.
    fn payload_config(&self) -> String {
        let payload = self.payload.display();
        format!(
            "[core]\n\tfsmonitor = {payload}\n\tpager = {payload}\n[diff]\n\texternal = {payload}\n\
             [diff \"evil\"]\n\ttextconv = {payload}\n\tcommand = {payload}\n\
             [filter \"evil\"]\n\tclean = {payload}\n\tsmudge = {payload}\n"
        )
    }

    /// A change on the server, so status, diff and add have work to do.
    fn server_change(&self) {
        std::fs::write(self.server.join("a.txt"), "a changed on the server\n").unwrap();
    }

    /// Plain git on the server runs the payload: the scenario is live.
    fn control(&self, cwd: &Path, settings: &[&str]) {
        self.forget();
        for form in [&["status"][..], &["diff"], &["log", "-p"], &["add", "-A"]] {
            let mut args: Vec<&str> = settings.iter().flat_map(|setting| ["-c", *setting]).collect();
            args.extend_from_slice(form);
            let _ = self.plain_git(cwd, &args);
        }
        assert!(
            self.ran(),
            "plain git in {} never ran the planted payload: the scenario proves nothing",
            cwd.display()
        );
        self.forget();
    }

    /// The tree session, Eitri's laptop config, then the git session; the config written before the replica arrives
    /// must survive it.
    fn mirror(&self, replicator: &Replicator) {
        replicator.tree(&self.server, &self.laptop);
        let listed = self.plain_git(
            &self.server,
            &["config", "--no-includes", "--file", ".git/config", "--list", "-z"],
        );
        assert!(listed.status.success());
        let entries = parse_config_list(&listed.stdout);
        let plan = git_session_plan(&SyncKey::new("SHA256:test", &self.server), &self.laptop, &self.server);
        let git_dir = plan.laptop_git_dir.clone();
        let hooks = super::install_refuse_hooks(&self.home.join("state")).unwrap();
        write_laptop_config(&isolated_local_git(&self.home), &git_dir, &entries, &hooks).unwrap();
        let written = std::fs::read(git_dir.join("config")).unwrap();
        let ignores: Vec<&str> = plan.ignores.iter().map(String::as_str).collect();
        replicator.git(&plan.server_root, &git_dir, &ignores, plan.symlink_mode);
        assert_eq!(
            std::fs::read(git_dir.join("config")).unwrap(),
            written,
            "the replica replaced the laptop's config"
        );
        assert!(!self.ran(), "mirroring ran the payload");
    }

    /// A new save of `a.txt` (its text, which a diff must show) and a rewrite of `b.txt` that changes only its times.
    fn save_on_the_laptop(&self) -> String {
        let text = format!(
            "a changed on the laptop {}\n",
            self.saves.fetch_add(1, Ordering::Relaxed)
        );
        std::fs::write(self.laptop.join("a.txt"), &text).unwrap();
        let b = std::fs::read(self.laptop.join("b.txt")).unwrap();
        std::fs::write(self.laptop.join("b.txt"), b).unwrap();
        text
    }

    /// `local_git` status, diff and log -p, bare and with the flags the shim adds, run from `cwd`.
    fn assert_local_git_runs_nothing(&self, cwd: &Path) {
        let saved = self.save_on_the_laptop();
        let local = isolated_local_git(&self.home);
        let shim = |args: &[&str]| with_flags_last(&os(args), &["--no-ext-diff", "--no-textconv"]);
        let forms = [
            ("status", os(&["status"]), true),
            ("diff", os(&["diff"]), false),
            ("log -p", os(&["log", "-p"]), false),
            ("diff with the shim's flags", shim(&["diff"]), true),
            ("log -p with the shim's flags", shim(&["log", "-p"]), true),
            ("show with the shim's flags", shim(&["show"]), true),
        ];
        for (label, args, must_succeed) in forms {
            let output = local
                .command(&self.laptop, &args)
                .unwrap()
                .current_dir(cwd)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !self.ran(),
                "local_git {label} in {} ran the payload: {stderr}",
                cwd.display()
            );
            // A broken replica would make every form fail before git looked at anything: the forms that must work
            // show the copy is a repository git reads.
            if must_succeed {
                assert!(output.status.success(), "local_git {label} failed: {stderr}");
            }
            if label == "diff with the shim's flags" {
                assert!(String::from_utf8_lossy(&output.stdout).contains(saved.trim_end()));
            }
        }
    }

    /// Plain git by absolute path from `cwd`: status, diff, log -p and an index write.
    fn assert_plain_git_runs_nothing(&self, cwd: &Path) {
        self.save_on_the_laptop();
        for form in [&["status"][..], &["diff"], &["log", "-p"], &["add", "-A"]] {
            let output = self.plain_git(cwd, form);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !self.ran(),
                "plain git {form:?} in {} ran the payload: {stderr}",
                cwd.display()
            );
            if form == ["status"] {
                assert!(output.status.success(), "plain git status failed: {stderr}");
            }
        }
    }

    fn assert_laptop_runs_nothing(&self, cwd: &Path) {
        self.assert_local_git_runs_nothing(cwd);
        self.assert_plain_git_runs_nothing(cwd);
    }
}

/// Makes the laptop's copies, by the model of Mutagen's rules or by Mutagen.
enum Replicator {
    Model,
    Mutagen(Mutagen),
}

impl Replicator {
    /// The tree session: everything but [`TREE_VCS_IGNORES`].
    fn tree(&self, server: &Path, laptop: &Path) {
        match self {
            Self::Model => model_mirror(server, laptop, TREE_VCS_IGNORES, "portable"),
            // The engine's tree session: two-way-safe with the laptop as alpha, VCS directories and `.git` ignored.
            Self::Mutagen(mutagen) => mutagen.once("two-way-safe", laptop, server, TREE_VCS_IGNORES, &["--ignore-vcs"]),
        }
    }

    /// The git session: the server's `.git` onto the laptop's, a one-way replica with `ignores` and Mutagen's
    /// `symlink_mode`.
    fn git(&self, server_git: &Path, laptop_git: &Path, ignores: &[&str], symlink_mode: &str) {
        match self {
            Self::Model => model_mirror(server_git, laptop_git, ignores, symlink_mode),
            Self::Mutagen(mutagen) => {
                let mode = format!("--symlink-mode={symlink_mode}");
                mutagen.once(GIT_SESSION_MODE, server_git, laptop_git, ignores, &[&mode])
            }
        }
    }
}

/// Mutagen's reading of `patterns`, written here apart from the product's `ignores::Matcher` (which also treats
/// `.git` specially, as only the tree session does): the last matching pattern decides, `!` takes back, a pattern
/// without `/` matches the last component at any depth, a leading `/` or an inner `/` anchors at the root, a trailing
/// `/` matches directories only, `*` stays within a component and a `**` component spans any number of them. A path
/// under an ignored directory is ignored, since Mutagen does not scan it.
fn model_ignores(patterns: &[&str], rel: &str, is_dir: bool) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    (1..parts.len()).any(|end| model_direct(patterns, &parts[..end].join("/"), true))
        || model_direct(patterns, rel, is_dir)
}

fn model_direct(patterns: &[&str], path: &str, is_dir: bool) -> bool {
    let mut ignored = false;
    for pattern in patterns {
        let (negated, body) = match pattern.strip_prefix('!') {
            Some(body) => (true, body),
            None => (false, *pattern),
        };
        let (body, dir_only) = match body.strip_suffix('/') {
            Some(body) => (body, true),
            None => (body, false),
        };
        if dir_only && !is_dir {
            continue;
        }
        let hit = match body.strip_prefix('/') {
            Some(anchored) => components_match(anchored, path),
            None if body.contains('/') => components_match(body, path),
            None => glob(body, path.rsplit('/').next().unwrap_or(path)),
        };
        if hit {
            ignored = !negated;
        }
    }
    ignored
}

fn components_match(pattern: &str, path: &str) -> bool {
    fn walk(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| walk(rest, &path[skip..])),
            Some((head, rest)) => path
                .split_first()
                .is_some_and(|(first, tail)| glob(head, first) && walk(rest, tail)),
        }
    }
    walk(
        &pattern.split('/').collect::<Vec<_>>(),
        &path.split('/').collect::<Vec<_>>(),
    )
}

/// `*` matches any run of characters within one component.
fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => text.strip_prefix(head).is_some_and(|rest| {
            (0..=rest.len())
                .filter(|cut| rest.is_char_boundary(*cut))
                .any(|cut| glob(tail, &rest[cut..]))
        }),
    }
}

/// `symlink_mode` is Mutagen's: `ignore` copies no link, `portable` copies a relative link that stays inside the
/// root by its text.
fn model_mirror(alpha: &Path, beta: &Path, ignores: &[&str], symlink_mode: &str) {
    assert!(matches!(symlink_mode, "ignore" | "portable"), "{symlink_mode}");
    std::fs::create_dir_all(beta).unwrap();
    model_walk(alpha, beta, Path::new(""), ignores, symlink_mode);
}

fn model_walk(alpha: &Path, beta: &Path, rel: &Path, ignores: &[&str], symlink_mode: &str) {
    let mut entries: Vec<_> = std::fs::read_dir(alpha.join(rel))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    for name in entries {
        let child = rel.join(&name);
        let source = alpha.join(&child);
        let meta = std::fs::symlink_metadata(&source).unwrap();
        if model_ignores(ignores, child.to_str().unwrap(), meta.is_dir()) {
            continue;
        }
        let target = beta.join(&child);
        if meta.is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            model_walk(alpha, beta, &child, ignores, symlink_mode);
        } else if meta.file_type().is_symlink() {
            let link = std::fs::read_link(&source).unwrap();
            if symlink_mode == "portable" && link_stays_inside(&child, &link) {
                std::os::unix::fs::symlink(&link, &target).unwrap();
            }
        } else if meta.is_file() {
            std::fs::copy(&source, &target).unwrap();
        }
    }
}

/// Whether a relative link at `rel` resolves inside the root (Mutagen's portable mode copies no other link).
fn link_stays_inside(rel: &Path, link: &Path) -> bool {
    let mut depth = rel.components().count() as i64 - 1;
    for component in link.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// A private Mutagen daemon for the real-replica run, stopped on drop.
struct Mutagen {
    binary: PathBuf,
    data: PathBuf,
    next: AtomicU32,
    _scratch: Scratch,
}

impl Mutagen {
    fn start(binary: PathBuf) -> Self {
        // The daemon's socket lives in its data directory; a short base keeps the path under the socket limit.
        let scratch = Scratch::under(Path::new("/tmp"), "mutagen");
        let mutagen = Self {
            binary,
            data: scratch.path().join("m"),
            next: AtomicU32::new(0),
            _scratch: scratch,
        };
        mutagen.run(&["daemon", "start"]);
        mutagen
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env("MUTAGEN_DATA_DIRECTORY", &self.data)
            .env("MUTAGEN_DISABLE_AUTOSTART", "1")
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "mutagen {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Creates a session, waits for a full cycle and terminates it.
    fn once(&self, mode: &str, alpha: &Path, beta: &Path, ignores: &[&str], extra: &[&str]) {
        let name = format!("redteam{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut args = vec![
            "sync",
            "create",
            "--no-global-configuration",
            "--name",
            &name,
            "--mode",
            mode,
        ];
        args.extend_from_slice(extra);
        for pattern in ignores {
            args.extend_from_slice(&["--ignore", pattern]);
        }
        let alpha = alpha.to_str().unwrap();
        let beta = beta.to_str().unwrap();
        args.extend_from_slice(&[alpha, beta]);
        self.run(&args);
        self.run(&["sync", "flush", &name]);
        self.run(&["sync", "terminate", &name]);
    }
}

impl Drop for Mutagen {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

// The scenarios, each run by a named test with the model and by the Mutagen test.

fn hooks_scenario(replicator: &Replicator) {
    for variant in ["directory", "link", "hooks path in the tree"] {
        let lab = Lab::new("hooks");
        let git_dir = lab.server.join(".git");
        let script = lab.hook_script();
        match variant {
            "directory" => {
                for hook in HOOKS {
                    write_exec(&git_dir.join("hooks").join(hook), &script);
                }
            }
            "link" => {
                std::fs::remove_dir_all(git_dir.join("hooks")).unwrap();
                for hook in HOOKS {
                    write_exec(&git_dir.join("evilhooks").join(hook), &script);
                }
                std::os::unix::fs::symlink("evilhooks", git_dir.join("hooks")).unwrap();
            }
            _ => {
                for hook in HOOKS {
                    write_exec(&lab.server.join(".githooks").join(hook), &script);
                }
                append(&git_dir.join("config"), "[core]\n\thooksPath = .githooks\n");
            }
        }
        lab.server_change();
        lab.control(&lab.server, &[]);
        lab.mirror(replicator);
        assert!(
            !exists(&lab.laptop.join(".git/hooks")),
            "{variant}: hooks reached the laptop"
        );
        lab.assert_laptop_runs_nothing(&lab.laptop);
    }
}

fn commondir_scenario(replicator: &Replicator) {
    let lab = Lab::new("commondir");
    let git_dir = lab.server.join(".git");
    let common = git_dir.join("evilcommon");
    copy_dir(&git_dir.join("objects"), &common.join("objects"));
    copy_dir(&git_dir.join("refs"), &common.join("refs"));
    std::fs::write(
        common.join("config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n{}",
            lab.payload_config()
        ),
    )
    .unwrap();
    std::fs::write(git_dir.join("commondir"), "evilcommon\n").unwrap();
    lab.server_change();
    lab.control(&lab.server, &[]);
    // The control's `add` stored its blob in the common directory; the copy reads `.git/objects`, so the blob goes
    // there too and the laptop's index names only objects it has.
    copy_dir(&common.join("objects"), &git_dir.join("objects"));
    lab.mirror(replicator);
    assert!(!exists(&lab.laptop.join(".git/commondir")));
    assert!(!exists(&lab.laptop.join(".git/evilcommon/config")));
    lab.assert_laptop_runs_nothing(&lab.laptop);
}

fn submodule_scenario(replicator: &Replicator) {
    let lab = Lab::new("submodule");
    let sub = lab.server.join("sub");
    lab.setup_git(&lab.server, &["init", "-q", "-b", "main", "sub"]);
    std::fs::write(sub.join("s.txt"), "s\n").unwrap();
    lab.setup_git(&sub, &["add", "-A"]);
    lab.setup_git(&sub, &["commit", "-q", "-m", "sub"]);
    let modules = lab.server.join(".git/modules");
    std::fs::create_dir_all(&modules).unwrap();
    std::fs::rename(sub.join(".git"), modules.join("sub")).unwrap();
    std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
    lab.setup_git(&lab.server, &["add", "sub"]);
    lab.setup_git(&lab.server, &["commit", "-q", "-m", "add sub"]);
    append(&modules.join("sub/config"), &lab.payload_config());
    std::fs::write(sub.join(".gitattributes"), "* filter=evil diff=evil\n").unwrap();
    std::fs::write(sub.join("s.txt"), "s changed on the server\n").unwrap();
    lab.server_change();
    lab.control(&lab.server, &[]);
    lab.mirror(replicator);
    assert!(!exists(&lab.laptop.join(".git/modules")));
    assert!(!exists(&lab.laptop.join("sub/.git")));
    lab.assert_laptop_runs_nothing(&lab.laptop);
    lab.assert_laptop_runs_nothing(&lab.laptop.join("sub"));
}

fn embedded_bare_scenario(replicator: &Replicator) {
    let lab = Lab::new("embedded");
    std::fs::create_dir_all(lab.server.join("vendor")).unwrap();
    let server_path = lab.server.to_str().unwrap().to_owned();
    lab.setup_git(&lab.server, &["clone", "-q", "--bare", &server_path, "vendor/evil"]);
    let evil = lab.server.join("vendor/evil");
    append(&evil.join("config"), &lab.payload_config());
    std::fs::create_dir_all(evil.join("info")).unwrap();
    std::fs::write(evil.join("info/attributes"), "* filter=evil diff=evil\n").unwrap();
    lab.server_change();
    lab.control(&evil, &["safe.bareRepository=all"]);
    lab.mirror(replicator);
    // The embedded repository is in the tree, so it is on the laptop, and plain git started inside it (where a
    // person's git allows discovering bare repositories) reads it: the reason local_git pins the repository.
    let laptop_evil = lab.laptop.join("vendor/evil");
    lab.control(&laptop_evil, &["safe.bareRepository=all"]);
    lab.assert_local_git_runs_nothing(&laptop_evil);
    lab.assert_laptop_runs_nothing(&lab.laptop);
}

/// Branches and tags named like the excluded names are refs, and arrive; the excluded names stay out everywhere else,
/// and what a server plants below `refs/` is never read as configuration by the laptop's git.
fn ref_names_scenario(replicator: &Replicator) {
    let lab = Lab::new("ref-names");
    let names = [
        "feature/config",
        "hooks",
        "fix/attributes",
        "gitdir",
        "commondir",
        "x/alternates",
        "config.worktree",
    ];
    for name in names {
        lab.setup_git(&lab.server, &["branch", name]);
    }
    lab.setup_git(&lab.server, &["tag", "config"]);
    let git_dir = lab.server.join(".git");
    // A bare repository planted where a branch directory would be, with config and attributes that name the payload.
    let server_path = lab.server.to_str().unwrap().to_owned();
    lab.setup_git(&lab.base(), &["clone", "-q", "--bare", &server_path, "planted"]);
    let planted = git_dir.join("refs/heads/evil");
    std::fs::rename(lab.base().join("planted"), &planted).unwrap();
    append(&planted.join("config"), &lab.payload_config());
    std::fs::create_dir_all(planted.join("info")).unwrap();
    std::fs::write(planted.join("info/attributes"), "* filter=evil diff=evil\n").unwrap();
    std::fs::write(
        git_dir.join("refs/heads/main.lock"),
        "0000000000000000000000000000000000000000\n",
    )
    .unwrap();
    std::fs::create_dir_all(git_dir.join("evil/deep")).unwrap();
    append(&git_dir.join("evil/deep/config"), &lab.payload_config());
    // A whole worktree with its repository planted below refs: plain git started in it would read its config, even
    // with bare discovery off.
    lab.setup_git(&lab.base(), &["init", "-q", "-b", "main", "plantwt"]);
    let plant = lab.base().join("plantwt");
    std::fs::write(plant.join("w.txt"), "w\n").unwrap();
    lab.setup_git(&plant, &["add", "-A"]);
    lab.setup_git(&plant, &["commit", "-q", "-m", "w"]);
    append(&plant.join(".git/config"), &lab.payload_config());
    let planted_tree = git_dir.join("refs/heads/wt");
    std::fs::rename(&plant, &planted_tree).unwrap();
    lab.server_change();
    lab.control(&planted, &["safe.bareRepository=all"]);
    lab.control(&planted_tree, &["safe.bareRepository=explicit"]);
    lab.mirror(replicator);
    let laptop_git = lab.laptop.join(".git");
    for name in names {
        let rev = |at: &Path| {
            String::from_utf8(
                lab.plain_git(at, &["rev-parse", "--verify", &format!("refs/heads/{name}")])
                    .stdout,
            )
            .unwrap()
        };
        assert!(!rev(&lab.server).is_empty());
        assert_eq!(rev(&lab.laptop), rev(&lab.server), "branch {name} was not mirrored");
        let local = lab.local(&["rev-parse", "--verify", &format!("refs/heads/{name}")]);
        assert!(local.status.success(), "local_git does not see branch {name}");
        assert!(
            exists(&laptop_git.join("logs/refs/heads").join(name)),
            "the reflog of {name} was not mirrored"
        );
    }
    assert!(lab
        .local(&["rev-parse", "--verify", "refs/tags/config"])
        .status
        .success());
    // Still out: the names where git reads them, and wherever else they appear outside refs.
    for out in ["hooks", "evil/deep/config", "refs/heads/main.lock"] {
        assert!(!exists(&laptop_git.join(out)), "{out} reached the laptop");
    }
    assert_ne!(
        std::fs::read(laptop_git.join("config")).unwrap(),
        std::fs::read(git_dir.join("config")).unwrap(),
        "the server's config reached the laptop"
    );
    // Below refs only what can be a ref arrives: no `.git` and no `HEAD`, so git discovers no repository there.
    for out in ["refs/heads/wt/.git", "refs/heads/evil/HEAD"] {
        assert!(!exists(&laptop_git.join(out)), "{out} reached the laptop");
    }
    lab.assert_laptop_runs_nothing(&lab.laptop);
    for planted in ["refs/heads/evil", "refs/heads/wt"] {
        let inside = laptop_git.join(planted);
        lab.assert_local_git_runs_nothing(&inside);
        // Plain git, as a shell prompt or a file manager runs it there, with bare discovery allowed (git's default).
        lab.save_on_the_laptop();
        for form in [&["status"][..], &["diff"], &["log", "-p"], &["add", "-A"]] {
            let mut args = vec!["-c", "safe.bareRepository=all"];
            args.extend_from_slice(form);
            let output = lab.plain_git(&inside, &args);
            assert!(
                !lab.ran(),
                "plain git {form:?} in {planted} ran the payload: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

fn tree_attributes_scenario(replicator: &Replicator) {
    let lab = Lab::new("attributes");
    std::fs::write(lab.server.join(".gitattributes"), "* filter=evil diff=evil\n").unwrap();
    lab.setup_git(&lab.server, &["add", "-A"]);
    lab.setup_git(&lab.server, &["commit", "-q", "-m", "attributes"]);
    let git_dir = lab.server.join(".git");
    append(&git_dir.join("config"), &lab.payload_config());
    std::fs::create_dir_all(git_dir.join("info")).unwrap();
    std::fs::write(git_dir.join("info/attributes"), "* filter=evil diff=evil\n").unwrap();
    lab.server_change();
    lab.control(&lab.server, &[]);
    lab.mirror(replicator);
    assert!(!exists(&lab.laptop.join(".git/info/attributes")));
    assert!(
        exists(&lab.laptop.join(".gitattributes")),
        "the tree's attributes arrive"
    );
    lab.assert_laptop_runs_nothing(&lab.laptop);
}

fn rebase_exec_scenario(replicator: &Replicator) {
    let lab = Lab::new("rebase");
    lab.commit_change("b second\n");
    // An interactive rebase stopped at `break`, with an `exec` of the payload next in its todo list.
    let editor = lab.base().join("todo-editor.sh");
    write_exec(
        &editor,
        &format!(
            "#!/bin/sh\nprintf 'break\\nexec {}\\n' > \"$1\"\n",
            lab.payload.display()
        ),
    );
    let started = lab
        .setup_command(&lab.server, &["rebase", "-i", "HEAD~1"])
        .env("GIT_SEQUENCE_EDITOR", &editor)
        .output()
        .unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    assert!(exists(&lab.server.join(".git/rebase-merge/git-rebase-todo")));
    assert!(!lab.ran());
    lab.mirror(replicator);
    for state in ["rebase-merge", "rebase-apply", "sequencer"] {
        assert!(
            !exists(&lab.laptop.join(".git").join(state)),
            "{state} reached the laptop"
        );
    }
    // Live: continuing the rebase on the server runs the payload.
    let _ = lab
        .setup_command(&lab.server, &["rebase", "--continue"])
        .output()
        .unwrap();
    assert!(
        lab.ran(),
        "plain git rebase --continue on the server never ran the payload"
    );
    lab.forget();
    let _ = lab
        .setup_command(&lab.laptop, &["rebase", "--continue"])
        .output()
        .unwrap();
    assert!(!lab.ran(), "plain git rebase --continue on the laptop ran the payload");
    let status = lab.plain_git(&lab.laptop, &["status"]);
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    lab.assert_laptop_runs_nothing(&lab.laptop);
}

fn linked_alternates_scenario(replicator: &Replicator) {
    let lab = Lab::new("alternates");
    // A repository outside the mirror, whose objects git on the laptop must never read.
    lab.setup_git(&lab.base(), &["init", "-q", "-b", "main", "outside"]);
    let outside = lab.base().join("outside");
    std::fs::write(outside.join("secret.txt"), "outside the mirror\n").unwrap();
    lab.setup_git(&outside, &["add", "-A"]);
    lab.setup_git(&outside, &["commit", "-q", "-m", "outside"]);
    let secret = String::from_utf8(lab.plain_git(&outside, &["rev-parse", "HEAD"]).stdout).unwrap();
    let secret = secret.trim();
    // `objects/info` becomes a link to a directory whose `alternates` names those objects.
    let git_dir = lab.server.join(".git");
    std::fs::create_dir_all(git_dir.join("evilinfo")).unwrap();
    std::fs::write(
        git_dir.join("evilinfo/alternates"),
        format!("{}\n", outside.join(".git/objects").display()),
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(git_dir.join("objects/info"));
    std::os::unix::fs::symlink("../evilinfo", git_dir.join("objects/info")).unwrap();
    // Live: the server's git reads the outside object through the link.
    assert!(lab.plain_git(&lab.server, &["cat-file", "-e", secret]).status.success());
    lab.mirror(replicator);
    assert!(
        !exists(&lab.laptop.join(".git/objects/info")),
        "the link reached the laptop"
    );
    assert!(!exists(&lab.laptop.join(".git/evilinfo/alternates")));
    assert!(!lab.plain_git(&lab.laptop, &["cat-file", "-e", secret]).status.success());
    assert!(!lab.local(&["cat-file", "-e", secret]).status.success());
    assert!(lab.local(&["log", "--oneline"]).status.success());
    lab.assert_laptop_runs_nothing(&lab.laptop);
}

fn linked_shallow_scenario(replicator: &Replicator) {
    let lab = Lab::new("shallow");
    lab.commit_change("b second\n");
    lab.commit_change("b third\n");
    let second = String::from_utf8(lab.plain_git(&lab.server, &["rev-parse", "HEAD~1"]).stdout).unwrap();
    // A file outside the mirror, reached through links inside `.git`: with `a -> .`, the kernel reads
    // `a/a/../../x` as the mirror's parent while its text stays inside `.git`.
    std::fs::write(lab.base().join("outside-shallow"), &second).unwrap();
    let git_dir = lab.server.join(".git");
    std::os::unix::fs::symlink(".", git_dir.join("a")).unwrap();
    std::os::unix::fs::symlink("a/a/../../outside-shallow", git_dir.join("shallow")).unwrap();
    let count = |output: Output| String::from_utf8_lossy(&output.stdout).lines().count();
    // Live: the server's git reads the outside file as its shallow boundary.
    assert_eq!(count(lab.plain_git(&lab.server, &["log", "--oneline"])), 2);
    lab.mirror(replicator);
    assert!(!exists(&lab.laptop.join(".git/shallow")), "the link reached the laptop");
    assert!(!exists(&lab.laptop.join(".git/a")));
    assert_eq!(count(lab.plain_git(&lab.laptop, &["log", "--oneline"])), 3);
    assert_eq!(count(lab.local(&["log", "--oneline"])), 3);
    lab.assert_laptop_runs_nothing(&lab.laptop);
}

fn hostile_global_config_scenario(replicator: &Replicator) {
    let lab = Lab::new("global");
    // Files the server writes into the tree, which a global setting could name.
    for hook in HOOKS {
        write_exec(&lab.server.join(".githooks").join(hook), &lab.hook_script());
    }
    std::fs::write(lab.server.join(".evilattrs"), "* filter=evil diff=evil\n").unwrap();
    lab.mirror(replicator);
    // The person's own global config: a relative hooks path, an fsmonitor, an attributes file the tree supplies and
    // the drivers it names.
    let payload = lab.payload.display();
    append(
        &lab.home.join(".gitconfig"),
        &format!(
            "[core]\n\thooksPath = .githooks\n\tfsmonitor = {payload}\n\tattributesFile = {}\n\
             [diff]\n\texternal = {payload}\n[diff \"evil\"]\n\ttextconv = {payload}\n\tcommand = {payload}\n\
             [filter \"evil\"]\n\tclean = {payload}\n\tsmudge = {payload}\n",
            lab.laptop.join(".evilattrs").display()
        ),
    );
    // Live: plain git on the laptop runs it.
    lab.save_on_the_laptop();
    lab.control(&lab.laptop, &[]);
    lab.assert_local_git_runs_nothing(&lab.laptop);
    // An index write, where git would run post-index-change.
    lab.save_on_the_laptop();
    let _ = lab.local(&["update-index", "--refresh"]);
    let _ = lab.local(&["add", "-A"]);
    assert!(!lab.ran(), "local_git's index write ran the payload");
}

fn gitfile_scenario(replicator: &Replicator) {
    // The tree session's ignores reach `.git` at any depth, whatever its kind.
    assert!(TREE_VCS_IGNORES.contains(&".git"));
    assert!(model_ignores(TREE_VCS_IGNORES, "deep/er/.git", false));
    let lab = Lab::new("gitfile");
    let server_path = lab.server.to_str().unwrap().to_owned();
    lab.setup_git(&lab.server, &["clone", "-q", "--bare", &server_path, "evilgit"]);
    let evil = lab.server.join("evilgit");
    append(
        &evil.join("config"),
        &format!("[core]\n\tbare = false\n{}", lab.payload_config()),
    );
    std::fs::create_dir_all(evil.join("info")).unwrap();
    std::fs::write(evil.join("info/attributes"), "* filter=evil diff=evil\n").unwrap();
    let nested = lab.server.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("a.txt"), "nested\n").unwrap();
    std::fs::write(nested.join(".git"), "gitdir: ../evilgit\n").unwrap();
    lab.control(&nested, &[]);
    lab.mirror(replicator);
    assert!(
        !exists(&lab.laptop.join("nested/.git")),
        "a gitfile written on the server reached the laptop"
    );
    assert!(exists(&lab.laptop.join("nested/a.txt")), "the rest of the tree arrives");
    lab.assert_laptop_runs_nothing(&lab.laptop.join("nested"));
}

/// A commit made on the copy would be undone by the next replica cycle, and git by full path (a plugin, a terminal
/// without the shim) does not go through Eitri: the laptop config's own hooks directory refuses the commit before it
/// writes anything, and `protocol.allow=never` stops a fetch or push from it.
#[test]
fn a_plain_commit_fetch_or_push_on_the_copy_is_refused() {
    let lab = Lab::new("refuse");
    lab.mirror(&Replicator::Model);
    let head = lab.plain_git(&lab.laptop, &["rev-parse", "HEAD"]);
    assert!(head.status.success());
    std::fs::write(lab.laptop.join("a.txt"), "changed on the laptop\n").unwrap();
    let commit = lab
        .setup_command(&lab.laptop, &["commit", "-q", "-a", "-m", "on the copy"])
        .output()
        .unwrap();
    assert!(!commit.status.success(), "a commit on the copy went through");
    let said = String::from_utf8_lossy(&commit.stderr);
    assert!(said.contains("read-only copy of the server's history"), "{said}");
    assert!(said.contains("Commit on the server"), "{said}");
    assert_eq!(lab.plain_git(&lab.laptop, &["rev-parse", "HEAD"]).stdout, head.stdout);
    let server = lab.server.to_str().unwrap();
    let fetch = lab.plain_git(&lab.laptop, &["fetch", "-q", server, "main"]);
    assert!(!fetch.status.success(), "a fetch from the copy ran");
    let push = lab.plain_git(&lab.laptop, &["push", "-q", server, "HEAD:refs/heads/from-the-copy"]);
    assert!(!push.status.success(), "a push from the copy ran");
    assert!(!lab
        .plain_git(
            &lab.server,
            &["rev-parse", "--verify", "-q", "refs/heads/from-the-copy"]
        )
        .status
        .success());
    // The hook is laptop-local: nothing under `.git` holds it.
    assert!(!exists(&lab.laptop.join(".git/hooks")));
    assert!(!lab.ran());
}

/// Check every ref and HEAD's symbolic target, including after a failed branch switch.
fn assert_ref_update_refused(lab: &Lab, args: &[&str]) {
    let snapshot = || {
        let refs = lab.plain_git(&lab.laptop, &["show-ref", "--head"]);
        let head = lab.plain_git(&lab.laptop, &["symbolic-ref", "HEAD"]);
        assert!(refs.status.success() && head.status.success());
        (refs.stdout, head.stdout)
    };
    let before = snapshot();
    let output = lab.setup_command(&lab.laptop, args).output().unwrap();
    assert!(!output.status.success(), "git {args:?} succeeded");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let messages: Vec<_> = stderr.lines().filter(|line| line.starts_with("eitri:")).collect();
    assert_eq!(messages.len(), 1, "git {args:?}: {stderr}");
    assert!(messages[0].contains("on the server"), "{stderr}");
    assert_eq!(snapshot(), before, "git {args:?} changed refs");
}

#[test]
fn plain_stash_is_refused_before_resetting_the_laptop_file() {
    for existing_stash in [false, true] {
        let lab = Lab::new("stash");
        if existing_stash {
            lab.server_change();
            lab.setup_git(&lab.server, &["stash"]);
        }
        lab.mirror(&Replicator::Model);
        let text = lab.save_on_the_laptop();
        assert_ref_update_refused(&lab, &["stash"]);
        assert_eq!(std::fs::read_to_string(lab.laptop.join("a.txt")).unwrap(), text);
    }
}

#[test]
fn plain_ref_updates_on_the_copy_are_refused() {
    let lab = Lab::new("refs");
    lab.setup_git(&lab.server, &["branch", "topic"]);
    lab.commit_change("second commit");
    lab.mirror(&Replicator::Model);
    for args in [
        &["branch", "-f", "topic", "HEAD"][..],
        &["update-ref", "refs/heads/x", "HEAD"],
        &["tag", "t"],
        &["checkout", "-b", "y"],
        &["commit", "--no-verify", "--allow-empty", "-m", "on the copy"],
    ] {
        assert_ref_update_refused(&lab, args);
    }
}

#[test]
fn local_git_reads_and_updates_refs_with_the_hooks_installed() {
    let lab = Lab::new("local-refs");
    lab.mirror(&Replicator::Model);
    let head = lab.local(&["rev-parse", "HEAD"]);
    assert!(head.status.success());
    let args = ["update-ref", "refs/heads/x", "HEAD"];
    assert_ref_update_refused(&lab, &args);
    let update = lab.local(&args);
    assert!(update.status.success(), "{}", String::from_utf8_lossy(&update.stderr));
    let created = lab.local(&["rev-parse", "--verify", "refs/heads/x"]);
    assert!(created.status.success());
    assert_eq!(created.stdout, head.stdout);
}

#[test]
fn a_replica_hook_never_runs() {
    hooks_scenario(&Replicator::Model);
}

#[test]
fn a_planted_commondir_config_never_runs() {
    commondir_scenario(&Replicator::Model);
}

#[test]
fn a_submodule_gitdir_config_filter_never_runs() {
    submodule_scenario(&Replicator::Model);
}

#[test]
fn an_embedded_bare_repo_config_is_not_read() {
    embedded_bare_scenario(&Replicator::Model);
}

#[test]
fn branches_named_like_excluded_files_are_mirrored_and_nothing_planted_under_refs_runs() {
    ref_names_scenario(&Replicator::Model);
}

#[test]
fn a_tree_gitattributes_filter_from_repo_config_never_runs() {
    tree_attributes_scenario(&Replicator::Model);
}

#[test]
fn a_server_gitfile_in_the_tree_never_reaches_the_laptop() {
    gitfile_scenario(&Replicator::Model);
}

#[test]
fn a_replicated_rebase_exec_never_runs() {
    rebase_exec_scenario(&Replicator::Model);
}

#[test]
fn a_linked_objects_info_never_reads_objects_outside_the_mirror() {
    linked_alternates_scenario(&Replicator::Model);
}

#[test]
fn a_linked_shallow_never_reads_a_file_outside_the_mirror() {
    linked_shallow_scenario(&Replicator::Model);
}

#[test]
fn a_hostile_global_config_runs_nothing_through_local_git() {
    hostile_global_config_scenario(&Replicator::Model);
}

#[test]
fn the_model_reads_patterns_as_mutagen_does() {
    assert!(model_ignores(GIT_SESSION_IGNORES, "config", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "evil/deep/config", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "hooks", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "index.lock", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "refs/heads/main.lock", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "modules", true));
    assert!(model_ignores(GIT_SESSION_IGNORES, "modules", false));
    assert!(!model_ignores(GIT_SESSION_IGNORES, "evil/modules", true));
    assert!(model_ignores(GIT_SESSION_IGNORES, "info/attributes", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "evilinfo/attributes", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "objects/info/alternates", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "evilinfo/alternates", false));
    assert!(model_ignores(GIT_SESSION_IGNORES, "rebase-merge", true));
    assert!(model_ignores(GIT_SESSION_IGNORES, "sequencer", false));
    assert!(!model_ignores(GIT_SESSION_IGNORES, "refs/heads/sequencer", false));
    // Below refs/ and logs/refs/ the names are branch and tag names, taken back; locks stay out there too.
    for rel in [
        "refs/heads/feature/config",
        "refs/heads/hooks",
        "refs/tags/attributes",
        "refs/remotes/origin/gitdir",
        "logs/refs/heads/feature/config",
        "refs/heads/x/commondir",
    ] {
        assert!(!model_ignores(GIT_SESSION_IGNORES, rel, false), "{rel}");
    }
    assert!(!model_ignores(GIT_SESSION_IGNORES, "refs/heads/hooks", true));
    assert!(!model_ignores(GIT_SESSION_IGNORES, "refs/heads/hooks/x", false));
    assert!(model_ignores(
        GIT_SESSION_IGNORES,
        "refs/heads/feature/config.lock",
        false
    ));
    assert!(model_ignores(GIT_SESSION_IGNORES, "logs/refs/heads/x.lock", false));
    // No repository can be discovered below refs: no `.git` (no ref component starts with `.`) and no `HEAD`.
    for rel in [
        "refs/heads/wt/.git",
        "refs/heads/wt/.git/config",
        "refs/heads/wt/.hidden",
        "refs/heads/evil/HEAD",
        "refs/HEAD",
        "logs/refs/heads/x/HEAD",
        "logs/refs/heads/x/.git",
    ] {
        assert!(model_ignores(GIT_SESSION_IGNORES, rel, false), "{rel}");
    }
    // Everywhere else, as before: at the top, at any depth outside refs, and in the excluded gitdirs.
    for rel in [
        "config",
        "config.worktree",
        "hooks",
        "commondir",
        "gitdir",
        "objects/info/alternates",
        "info/attributes",
        "logs/config",
        "objects/hooks",
        "refsx/config",
        "modules/sub/config",
        "worktrees/w/gitdir",
        "worktrees/w/refs/heads/x",
    ] {
        assert!(model_ignores(GIT_SESSION_IGNORES, rel, false), "{rel}");
    }
    assert!(!model_ignores(GIT_SESSION_IGNORES, "objects/pack/pack-1.pack", false));
    assert!(!model_ignores(GIT_SESSION_IGNORES, "HEAD", false));
    assert!(
        !model_ignores(&["hooks/"], "hooks", false),
        "a directory-only pattern lets a link through"
    );
    assert!(link_stays_inside(Path::new("hooks"), Path::new("evilhooks")));
    assert!(!link_stays_inside(Path::new("hooks"), Path::new("../tree/hooks")));
    assert!(!link_stays_inside(Path::new("hooks"), Path::new("/abs")));
}

#[test]
#[ignore = "needs Mutagen: EITRI_TEST_MUTAGEN=<path to the mutagen binary>"]
fn red_team_holds_on_a_real_mutagen_replica() {
    let binary = std::env::var_os("EITRI_TEST_MUTAGEN").expect("EITRI_TEST_MUTAGEN names the mutagen binary");
    let replicator = Replicator::Mutagen(Mutagen::start(PathBuf::from(binary)));
    hooks_scenario(&replicator);
    commondir_scenario(&replicator);
    submodule_scenario(&replicator);
    embedded_bare_scenario(&replicator);
    tree_attributes_scenario(&replicator);
    ref_names_scenario(&replicator);
    gitfile_scenario(&replicator);
    rebase_exec_scenario(&replicator);
    linked_alternates_scenario(&replicator);
    linked_shallow_scenario(&replicator);
    hostile_global_config_scenario(&replicator);

    // Why `hooks` carries no trailing slash: with `hooks/`, Mutagen copies a link named hooks.
    let lab = Lab::new("hooks-pattern");
    let git_dir = lab.server.join(".git");
    std::fs::remove_dir_all(git_dir.join("hooks")).unwrap();
    write_exec(&git_dir.join("evilhooks/post-index-change"), &lab.hook_script());
    std::os::unix::fs::symlink("evilhooks", git_dir.join("hooks")).unwrap();
    let with_slash: Vec<&str> = GIT_SESSION_IGNORES
        .iter()
        .map(|pattern| if *pattern == "hooks" { "hooks/" } else { pattern })
        .collect();
    replicator.git(&git_dir, &lab.laptop.join(".git"), &with_slash, "portable");
    assert!(
        std::fs::symlink_metadata(lab.laptop.join(".git/hooks"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "Mutagen no longer copies a link past a directory-only pattern"
    );
}
