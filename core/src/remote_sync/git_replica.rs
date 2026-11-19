//! The `.git` replica: the git session's description, and the laptop's own `.git/config`.
//!
//! The laptop's `.git` is a one-way copy of the server's, so git features (signs, blame, branch names, pickers) read
//! history locally. Whoever can write the server's `.git` chooses its bytes, so the copy leaves out every file through
//! which git would run a program or read configuration, attributes or objects from elsewhere: config files, hooks,
//! `commondir`, `gitdir`, attributes, alternates, locks and fsmonitor state wherever their names appear; the state
//! of an operation in progress (a rebase's todo list can hold `exec` lines); the gitdirs of submodules and linked
//! worktrees, which carry their own config; and every symbolic link, since a link inside `.git` can lead a path the
//! ignores exclude, or a file git reads, to anywhere on the laptop. The laptop's `.git/config` is written here, by
//! Eitri, from an allow-list of the server's entries; it is never taken from the replica, and it is written before
//! the git session is created.

use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use super::local_git::LocalGit;
use super::SyncKey;

/// What the tree session ignores so that no repository metadata crosses with the files, at any depth. Mutagen's
/// `--ignore-vcs` leaves out `.git` directories but syncs a `.git` file (a submodule's or linked worktree's gitfile),
/// and a gitfile written on the server could point a laptop git at a config the server wrote into the tree. An
/// unanchored `.git` matches both kinds. The tree session's engine adds these to its ignores.
pub const TREE_VCS_IGNORES: &[&str] = &[".git"];

/// The git session's mode: alpha (the server) is the source, and the laptop's copy is made to match it. Laptop files
/// that are not ignored and not on the server are deleted; ignored laptop files are left alone.
pub const GIT_SESSION_MODE: &str = "one-way-replica";

/// The git session's ignores, relative to `.git`. A single name without `/` matches that name at any depth; a
/// pattern starting with `/` matches only at `.git`'s top; a trailing `/` matches directories only. None of these
/// has a trailing `/` except `fsmonitor--daemon/`, so an entry of any kind under one of these names stays out.
///
/// The session also copies no symbolic links ([`GIT_SESSION_SYMLINK_MODE`]), so no link can stand in for a directory
/// these names would exclude or point a file git reads (`shallow`, `objects/info`) outside the mirror.
pub const GIT_SESSION_IGNORES: &[&str] = &[
    "config",
    "config.worktree",
    "hooks",
    "commondir",
    "gitdir",
    "attributes",
    "alternates",
    "*.lock",
    "fsmonitor--daemon/",
    "fsmonitor--daemon.ipc",
    "/modules",
    "/worktrees",
    "/rebase-merge",
    "/rebase-apply",
    "/sequencer",
];

/// The git session's Mutagen symlink mode: a repository's `.git` holds no symbolic links, so none is copied.
pub const GIT_SESSION_SYMLINK_MODE: &str = "ignore";

/// The git session, described for the sync engine: a one-way replica from the server's `.git` onto the laptop's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSessionPlan {
    /// The Mutagen session name.
    pub name: String,
    /// The server's `.git`: the source (alpha).
    pub server_root: PathBuf,
    /// The laptop's `.git`: the copy (beta).
    pub laptop_git_dir: PathBuf,
    pub ignores: Vec<String>,
    /// Mutagen's `--symlink-mode`: [`GIT_SESSION_SYMLINK_MODE`].
    pub symlink_mode: &'static str,
}

pub fn git_session_plan(key: &SyncKey, laptop_root: &Path, server_root: &Path) -> GitSessionPlan {
    GitSessionPlan {
        name: format!("eitri-{key}-git"),
        server_root: server_root.join(".git"),
        laptop_git_dir: laptop_root.join(".git"),
        ignores: GIT_SESSION_IGNORES
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect(),
        symlink_mode: GIT_SESSION_SYMLINK_MODE,
    }
}

/// The server config keys the laptop's `.git/config` may carry, as git lists them (section and name in lower case;
/// `*` is one nonempty subsection). Nothing here names a program, a path git would read, or another repository.
/// `core.worktree` is absent: the mirror's work tree is its root, and a server value could only point elsewhere.
pub const GIT_CONFIG_ALLOWED: &[&str] = &[
    "remote.*.url",
    "remote.*.fetch",
    "branch.*.remote",
    "branch.*.merge",
    "branch.*.rebase",
    "core.bare",
    "core.repositoryformatversion",
    "extensions.objectformat",
    "extensions.refstorage",
    "user.name",
    "user.email",
];

fn key_is_allowed(key: &str) -> bool {
    // A quote, a backslash or a control character in a name could only matter to a writer that builds the file by
    // hand; git's own writer quotes, but no legitimate key needs them.
    if key.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return false;
    }
    GIT_CONFIG_ALLOWED
        .iter()
        .any(|pattern| match pattern.split_once(".*.") {
            Some((section, name)) => key
                .strip_prefix(section)
                .and_then(|rest| rest.strip_prefix('.'))
                .and_then(|rest| rest.strip_suffix(name))
                .and_then(|rest| rest.strip_suffix('.'))
                .is_some_and(|subsection| !subsection.is_empty()),
            None => key == *pattern,
        })
}

/// The entries whose key is on [`GIT_CONFIG_ALLOWED`], in order. The helper applies it before answering and the
/// laptop applies it again before writing, so a helper that sends more changes nothing.
pub fn sanitize(entries: &[(String, String)]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter(|(key, value)| key_is_allowed(key) && !value.contains('\0'))
        .cloned()
        .collect()
}

/// The entries of `git config --list -z` output, in order. A key listed without a value (a bare boolean such as
/// `[core] bare`) reads as `true`, which is what git takes it to mean. Records that are not UTF-8 are left out.
pub fn parse_config_list(bytes: &[u8]) -> Vec<(String, String)> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let record = std::str::from_utf8(record).ok()?;
            Some(match record.split_once('\n') {
                Some((key, value)) => (key.to_owned(), value.to_owned()),
                None => (record.to_owned(), "true".to_owned()),
            })
        })
        .collect()
}

/// A digest of `entries` in order, so the laptop rewrites its config only when the allowed entries change.
pub fn config_digest(entries: &[(String, String)]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"eitri-git-config\0");
    for (key, value) in entries {
        hash.update(key.as_bytes());
        hash.update(b"\0");
        hash.update(value.as_bytes());
        hash.update(b"\0");
    }
    format!("{:x}", hash.finalize())
}

/// The laptop's own hooks directory, `<state home>/eitri/remote/hooks`: outside every mirror, so no sync reaches it.
pub fn refuse_hooks_dir(state_home: &Path) -> PathBuf {
    state_home.join("eitri/remote/hooks")
}

/// The only hook in [`refuse_hooks_dir`]. A commit made in the laptop's copy would be undone by the next replica
/// cycle (the server's `.git` is the source), so a plain `git commit` there, one run by full path or from a terminal
/// that has no shim, is refused before it writes anything. Eitri's own git never reaches it: forwarded commands run on
/// the server and local reads run with hooks off.
pub const REFUSE_PRE_COMMIT: &str = "#!/bin/sh\n\
echo \"eitri: this .git is a read-only copy of the server's history; a commit made here would be undone by the next \
sync. Commit on the server: from an Eitri window or an nvim with eitri.nvim (git there goes to the server), or in a \
terminal on the server.\" >&2\n\
exit 1\n";

/// Writes [`REFUSE_PRE_COMMIT`] as `pre-commit` (0700) into [`refuse_hooks_dir`] (0700) and returns the directory. A
/// hook that already has that text and mode is left alone; anything else there is replaced through a rename, so
/// every connection repairs a changed one.
pub fn install_refuse_hooks(state_home: &Path) -> io::Result<PathBuf> {
    use std::io::Write;
    if !state_home.is_absolute() {
        return Err(io::Error::other("the state home is not absolute"));
    }
    let root = state_home.join("eitri");
    agent::private_fs::require_safe_state_parent(&root, "git hooks for remote mirrors")?;
    let dir = refuse_hooks_dir(state_home);
    agent::private_fs::create_private_dir_all(&dir, &root)?;
    let hook = dir.join("pre-commit");
    use std::os::unix::fs::MetadataExt;
    if std::fs::symlink_metadata(&hook).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o7777 == 0o700)
        && agent::private_fs::read_private_nofollow_capped(&hook, REFUSE_PRE_COMMIT.len() as u64)
            .is_ok_and(|text| text == REFUSE_PRE_COMMIT)
    {
        return Ok(dir);
    }
    let temporary = dir.join(format!(".pre-commit-{}", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = agent::private_fs::open_private(&temporary)?;
        file.write_all(REFUSE_PRE_COMMIT.as_bytes())?;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &hook)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map(|()| dir)
}

/// The transports git knows, each refused by name in the laptop's config.
const PROTOCOLS: &[&str] = &["file", "git", "ssh", "http", "https", "ext"];

/// Writes the laptop's `<git_dir>/config` (mode 0600) from `entries`, keeping only what [`sanitize`] keeps, then
/// Eitri's own two entries: `core.hookspath` naming `hooks_dir` (laptop-local, holding only [`REFUSE_PRE_COMMIT`];
/// see [`install_refuse_hooks`]) and `protocol.allow=never`, so a git run on the copy by full path fetches and pushes
/// nothing and commits nothing.
///
/// Each entry goes through `git config --file <temporary> --add`, so git does the quoting; the temporary file is
/// synced and renamed over `config`, so a reader sees the old file or the new one. Its name ends in `.lock`, which the
/// replica ignores, so a replica cycle running meanwhile leaves it alone. `git_dir` is created (0700) when the replica
/// has not made it yet.
pub fn write_laptop_config(
    local_git: &LocalGit,
    git_dir: &Path,
    entries: &[(String, String)],
    hooks_dir: &Path,
) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = git_dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the git directory has no parent"))?;
    match DirBuilder::new().mode(0o700).create(git_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !std::fs::symlink_metadata(git_dir)?.is_dir() {
        return Err(io::Error::other(format!("{} is not a directory", git_dir.display())));
    }
    if !hooks_dir.is_absolute() {
        return Err(io::Error::other("the hooks directory is not absolute"));
    }
    let mut entries: Vec<(OsString, OsString)> = sanitize(entries)
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    entries.push(("core.hookspath".into(), hooks_dir.into()));
    entries.push(("protocol.allow".into(), "never".into()));
    // A person's global `protocol.<name>.allow` beats the general key, so each transport is named too.
    for name in PROTOCOLS {
        entries.push((format!("protocol.{name}.allow").into(), "never".into()));
    }
    let temporary = git_dir.join(format!(
        "eitri-config-{}-{}.lock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&temporary);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let written = fill_and_replace(local_git, root, git_dir, &temporary, &entries);
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

fn fill_and_replace(
    local_git: &LocalGit,
    root: &Path,
    git_dir: &Path,
    temporary: &Path,
    entries: &[(OsString, OsString)],
) -> io::Result<()> {
    for (key, value) in entries {
        let args: Vec<OsString> = vec![
            "config".into(),
            "--file".into(),
            temporary.into(),
            "--add".into(),
            "--".into(),
            key.clone(),
            value.clone(),
        ];
        let output = local_git
            .command(root, &args)?
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "git config could not write {}: {}",
                key.to_string_lossy(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    let file = File::open(temporary)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, git_dir.join("config"))?;
    File::open(git_dir)?.sync_all()
}

#[cfg(test)]
#[path = "git_replica_red_team.rs"]
mod red_team;

/// The git the replica's tests run (the first on `PATH`).
#[cfg(test)]
pub(crate) fn red_team_git_binary() -> PathBuf {
    red_team::git_binary()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn git_session_is_server_alpha_and_ignores_every_config_hook_and_indirection() {
        let key = SyncKey::new("SHA256:example", Path::new("/srv/project"));
        let plan = git_session_plan(&key, Path::new("/home/someone/project"), Path::new("/srv/project"));
        assert_eq!(plan.name, format!("eitri-{key}-git"));
        assert_eq!(plan.server_root, Path::new("/srv/project/.git"));
        assert_eq!(plan.laptop_git_dir, Path::new("/home/someone/project/.git"));
        assert_eq!(GIT_SESSION_MODE, "one-way-replica");
        // Single names, matched wherever they appear in `.git`: no `/` at all.
        for leaf in [
            "config",
            "config.worktree",
            "hooks",
            "commondir",
            "gitdir",
            "attributes",
            "alternates",
            "*.lock",
            "fsmonitor--daemon/",
            "fsmonitor--daemon.ipc",
        ] {
            assert!(plan.ignores.iter().any(|p| p == leaf), "{leaf}");
            assert!(
                !leaf.trim_end_matches('/').contains('/'),
                "{leaf} would match only from the top"
            );
        }
        // At the top only: submodule and linked-worktree gitdirs (their own config) and an operation in progress
        // (a rebase todo list can hold `exec` lines). No trailing slash, so no entry of any kind by these names.
        for anchored in ["/modules", "/worktrees", "/rebase-merge", "/rebase-apply", "/sequencer"] {
            assert!(plan.ignores.iter().any(|p| p == anchored), "{anchored}");
        }
        assert_eq!(plan.ignores.len(), 15);
        assert!(
            !plan.ignores.iter().any(|p| p == "hooks/"),
            "a link named hooks must be ignored too"
        );
        assert_eq!(plan.symlink_mode, "ignore");
        assert_eq!(TREE_VCS_IGNORES, &[".git"]);
    }

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn sanitize_drops_every_key_outside_the_list() {
        let kept = pairs(&[
            ("core.repositoryformatversion", "0"),
            ("core.bare", "false"),
            ("extensions.objectformat", "sha256"),
            ("extensions.refstorage", "reftable"),
            ("remote.origin.url", "git@example.com:someone/project.git"),
            ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
            ("remote.origin.fetch", "+refs/tags/*:refs/tags/*"),
            ("remote.with.dots.url", "https://example.com/x.git"),
            ("branch.main.remote", "origin"),
            ("branch.main.merge", "refs/heads/main"),
            ("branch.feature/x.rebase", "true"),
            ("user.name", "Someone"),
            ("user.email", "someone@example.com"),
        ]);
        let dropped = pairs(&[
            ("core.fsmonitor", "/tmp/run"),
            ("core.hookspath", "/tmp/hooks"),
            ("core.worktree", "/elsewhere"),
            ("include.path", "/tmp/more"),
            ("includeif.gitdir:/x/.path", "/tmp/more"),
            ("filter.x.clean", "/tmp/run"),
            ("filter.x.smudge", "/tmp/run"),
            ("filter.x.process", "/tmp/run"),
            ("diff.x.textconv", "/tmp/run"),
            ("diff.external", "/tmp/run"),
            ("core.sshcommand", "/tmp/run"),
            ("remote.origin.uploadpack", "/tmp/run"),
            ("remote.origin.receivepack", "/tmp/run"),
            ("url.https://evil.example/.insteadof", "https://example.com/"),
            ("core.pager", "/tmp/run"),
            ("alias.st", "!/tmp/run"),
            ("core.editor", "/tmp/run"),
            ("core.askpass", "/tmp/run"),
            ("credential.helper", "/tmp/run"),
            ("extensions.worktreeconfig", "true"),
            ("extensions.partialclone", "origin"),
            ("gpg.program", "/tmp/run"),
            ("remote..url", "x"),
            ("remote.url", "x"),
            ("branch..merge", "x"),
            ("user.signingkey", "x"),
            ("Core.Bare", "true"),
        ]);
        let mut mixed = Vec::new();
        for (index, entry) in dropped.iter().enumerate() {
            mixed.push(entry.clone());
            if let Some(keep) = kept.get(index) {
                mixed.push(keep.clone());
            }
        }
        assert_eq!(sanitize(&mixed), kept);
        assert!(sanitize(&dropped).is_empty());
        assert_eq!(sanitize(&pairs(&[("user.name", "a\0b")])), Vec::new());
    }

    #[test]
    fn hostile_subsection_names_cannot_open_a_section() {
        for key in [
            "remote.a\"]\n[core]\nfsmonitor=x.url",
            "remote.a\"].url",
            "remote.a\\.url",
            "branch.a\nb.merge",
            "branch.a\rb.merge",
            "remote.a\0b.url",
            "remote.a\tb.fetch",
        ] {
            assert!(
                sanitize(&[(key.to_owned(), "v".to_owned())]).is_empty(),
                "{key:?} was kept"
            );
        }
        // Values are written by git, which quotes them; a value cannot open a section.
        let value = "x\"\n[core]\n\tfsmonitor = /tmp/run";
        assert_eq!(
            sanitize(&[("user.name".to_owned(), value.to_owned())]),
            vec![("user.name".to_owned(), value.to_owned())]
        );
    }

    #[test]
    fn config_list_parses_in_order_with_bare_booleans() {
        let listed =
            b"core.bare\0remote.origin.fetch\n+a\0remote.origin.fetch\n+b\0user.name\nline one\nline two\0bad\xff\nx\0";
        assert_eq!(
            parse_config_list(listed),
            pairs(&[
                ("core.bare", "true"),
                ("remote.origin.fetch", "+a"),
                ("remote.origin.fetch", "+b"),
                ("user.name", "line one\nline two"),
            ])
        );
        let a = pairs(&[("remote.o.fetch", "+a"), ("remote.o.fetch", "+b")]);
        let b = pairs(&[("remote.o.fetch", "+b"), ("remote.o.fetch", "+a")]);
        assert_ne!(config_digest(&a), config_digest(&b));
        assert_eq!(config_digest(&a), config_digest(&a.clone()));
    }

    #[test]
    fn laptop_config_is_written_atomically_0600_through_git_config() {
        let scratch = red_team::Scratch::new("config");
        let home = scratch.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let root = scratch.path().join("mirror");
        std::fs::create_dir_all(&root).unwrap();
        let git_dir = root.join(".git");
        let git = red_team::isolated_local_git(&home);
        let entries = pairs(&[
            ("core.repositoryformatversion", "0"),
            ("remote.origin.url", "-x \"quoted\" \\ back ; # semi"),
            ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
            ("remote.origin.fetch", "+refs/tags/*:refs/tags/*"),
            ("branch.a b]c.merge", "refs/heads/a b]c"),
            ("user.name", "  leading spaces\nand a newline"),
            ("core.fsmonitor", "/tmp/run"),
            ("remote.a\"]\n[core]\nfsmonitor=x.url", "v"),
        ]);
        let hooks = install_refuse_hooks(&scratch.path().join("state")).unwrap();
        let eitri = |mut kept: Vec<(String, String)>| {
            kept.push(("core.hookspath".into(), hooks.to_str().unwrap().into()));
            kept.push(("protocol.allow".into(), "never".into()));
            for name in PROTOCOLS {
                kept.push((format!("protocol.{name}.allow"), "never".into()));
            }
            kept
        };
        // No `.git` yet (the replica has not arrived): it is made 0700.
        write_laptop_config(&git, &git_dir, &entries, &hooks).unwrap();
        assert_eq!(std::fs::metadata(&git_dir).unwrap().mode() & 0o777, 0o700);
        let config = git_dir.join("config");
        assert_eq!(std::fs::metadata(&config).unwrap().mode() & 0o777, 0o600);
        let listed = std::process::Command::new(red_team::git_binary())
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["config", "--file"])
            .arg(&config)
            .args(["--list", "-z"])
            .output()
            .unwrap();
        assert!(listed.status.success());
        // git files each key under its section, so Eitri's own two land among the kept ones.
        let (own, kept): (Vec<_>, Vec<_>) = parse_config_list(&listed.stdout)
            .into_iter()
            .partition(|(key, _)| key == "core.hookspath" || key.starts_with("protocol."));
        assert_eq!(kept, sanitize(&entries));
        let mut own = own;
        own.sort();
        let mut expected = eitri(vec![]);
        expected.sort();
        assert_eq!(own, expected);
        // Nothing but the config is left behind in `.git`.
        let names: Vec<_> = std::fs::read_dir(&git_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("config")]);

        // A rewrite replaces the file (a new inode) rather than editing it, and an empty list leaves Eitri's own two.
        let before = std::fs::metadata(&config).unwrap().ino();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_laptop_config(&git, &git_dir, &[], &hooks).unwrap();
        let after = std::fs::metadata(&config).unwrap();
        assert_ne!(after.ino(), before);
        assert_eq!(after.mode() & 0o777, 0o600);
        let own = format!(
            "[core]\n\thookspath = {}\n[protocol]\n\tallow = never\n[protocol \"file\"]\n\tallow = never\n\
             [protocol \"git\"]\n\tallow = never\n[protocol \"ssh\"]\n\tallow = never\n[protocol \"http\"]\n\
             \tallow = never\n[protocol \"https\"]\n\tallow = never\n[protocol \"ext\"]\n\tallow = never\n",
            hooks.display()
        );
        assert_eq!(std::fs::read_to_string(&config).unwrap(), own);

        // A replaced config is not read back: the file is built from the entries alone.
        std::fs::write(&config, "[core]\n\tfsmonitor = /tmp/run\n").unwrap();
        write_laptop_config(&git, &git_dir, &pairs(&[("user.name", "Someone")]), &hooks).unwrap();
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            format!("[user]\n\tname = Someone\n{own}")
        );
        assert!(write_laptop_config(&git, &git_dir, &[], Path::new("relative/hooks")).is_err());
    }

    #[test]
    fn refuse_hooks_hold_only_the_pre_commit_hook_and_are_repaired() {
        let scratch = red_team::Scratch::new("hooks");
        let state = scratch.path().join("state");
        let dir = install_refuse_hooks(&state).unwrap();
        assert_eq!(dir, state.join("eitri/remote/hooks"));
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let hook = dir.join("pre-commit");
        assert_eq!(std::fs::metadata(&hook).unwrap().mode() & 0o7777, 0o700);
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), REFUSE_PRE_COMMIT);
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("pre-commit")]);
        // The hook refuses with its line, whatever it is given.
        // Through sh: exec'ing a file just written can meet ETXTBSY while another test thread's fork still holds it.
        let run = std::process::Command::new("/bin/sh").arg(&hook).output().unwrap();
        assert_eq!(run.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&run.stderr).contains("read-only copy"));
        // An unchanged hook is left alone; a changed one is replaced.
        let ino = std::fs::metadata(&hook).unwrap().ino();
        install_refuse_hooks(&state).unwrap();
        assert_eq!(std::fs::metadata(&hook).unwrap().ino(), ino);
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        install_refuse_hooks(&state).unwrap();
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), REFUSE_PRE_COMMIT);
        assert!(install_refuse_hooks(Path::new("relative")).is_err());
    }
}
