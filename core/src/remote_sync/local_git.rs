//! The one way git runs on this laptop over a remote project's mirror.
//!
//! The mirror's `.git` is a copy of the server's repository, and whoever can write there (the agent, or anyone
//! holding the server account) chooses its bytes. Git can run programs that a repository names: hooks, an
//! fsmonitor, an external diff, attribute filters and textconv drivers. The replica never carries the server's
//! config, hooks, `commondir` or symbolic links, and every git Eitri or its shim runs locally goes through
//! [`local_git`], which also overrides each of those settings on the command line and names the repository with
//! `GIT_DIR`, so git never discovers an embedded repository or a gitfile in the tree. `GIT_DIR` itself follows a
//! gitfile or a symbolic link, so [`local_git`] refuses unless `<root>/.git` is a real directory.
//!
//! The person's global config still applies (their display settings and aliases). The residual risk is a filter or
//! textconv driver defined there and named by a `.gitattributes` in the tree, which the server can write.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `-c` settings every local git gets, before its subcommand.
///
/// `diff.external=` is an empty command: a diff form that may call an external diff must also carry
/// `--no-ext-diff`, or git stops with "external diff died" rather than running anything. Command-line settings
/// override the person's global ones, which is why hooks, fsmonitor and the attributes file are set here even though
/// the laptop's repository config never names them.
pub const SANITIZING_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.untrackedCache=false",
    "diff.external=",
    "core.attributesFile=/dev/null",
    "protocol.allow=never",
    "safe.bareRepository=explicit",
];

/// A local git bound to one real binary, with the environment it inherits.
#[derive(Clone, Debug)]
pub struct LocalGit {
    real_git: PathBuf,
    environment: Option<Vec<(OsString, OsString)>>,
}

impl LocalGit {
    /// A local git that inherits this process's environment (minus every `GIT_*` variable).
    pub fn new(real_git: impl Into<PathBuf>) -> Self {
        Self {
            real_git: real_git.into(),
            environment: None,
        }
    }

    /// A local git that inherits `environment` instead of this process's (minus every `GIT_*` variable).
    pub fn with_environment(real_git: impl Into<PathBuf>, environment: Vec<(OsString, OsString)>) -> Self {
        Self {
            real_git: real_git.into(),
            environment: Some(environment),
        }
    }

    pub fn real_git(&self) -> &Path {
        &self.real_git
    }

    /// [`local_git`] for this binary and environment.
    pub fn command(&self, root: &Path, args: &[OsString]) -> io::Result<Command> {
        match &self.environment {
            Some(environment) => local_git_with_env(&self.real_git, root, args, environment.iter().cloned()),
            None => local_git(&self.real_git, root, args),
        }
    }
}

/// The command for `real_git <args>` on the mirror at `root` (absolute), with this process's environment minus every
/// `GIT_*` variable. `args` is the subcommand and its arguments; a caller that adds its own flags puts them last,
/// before any `--` ([`with_flags_last`]). Refused unless `<root>/.git` is a real directory (not a gitfile, not a
/// symbolic link): git would follow either to a repository elsewhere.
pub fn local_git(real_git: &Path, root: &Path, args: &[OsString]) -> io::Result<Command> {
    local_git_with_env(real_git, root, args, std::env::vars_os())
}

/// [`local_git`] inheriting `inherited` rather than this process's environment.
pub fn local_git_with_env(
    real_git: &Path,
    root: &Path,
    args: &[OsString],
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> io::Result<Command> {
    let git_dir = root.join(".git");
    let kind = std::fs::symlink_metadata(&git_dir)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", git_dir.display())))?
        .file_type();
    if !kind.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory, so git there would read another repository",
            git_dir.display()
        )));
    }
    let mut command = Command::new(real_git);
    command.env_clear();
    // Every GIT_* variable can move the repository, add configuration (GIT_CONFIG_PARAMETERS, GIT_CONFIG_COUNT) or
    // name programs, so none is inherited; the ones git needs here are set below.
    for (name, value) in inherited {
        if !name.as_bytes().starts_with(b"GIT_") {
            command.env(name, value);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_DIR", &git_dir)
        .env("GIT_WORK_TREE", root);
    if let Some(parent) = root.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    for setting in SANITIZING_CONFIG {
        command.arg("-c").arg(setting);
    }
    command.args(args);
    Ok(command)
}

/// `args` with `flags` inserted before the first `--`, or appended when there is none: flags placed last win over a
/// caller's own, and a `--` would turn them into paths.
pub fn with_flags_last(args: &[OsString], flags: &[&str]) -> Vec<OsString> {
    let split = args
        .iter()
        .position(|arg| arg.as_os_str() == OsStr::new("--"))
        .unwrap_or(args.len());
    let mut out = Vec::with_capacity(args.len() + flags.len());
    out.extend_from_slice(&args[..split]);
    out.extend(flags.iter().map(OsString::from));
    out.extend_from_slice(&args[split..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn envs(command: &Command) -> BTreeMap<OsString, Option<OsString>> {
        command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect()
    }

    /// A mirror root with a real `.git` directory, removed on drop.
    struct Mirror(PathBuf);

    impl Mirror {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let base = std::env::temp_dir().join(format!(
                "eitri-local-git-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("project/.git")).unwrap();
            Self(base)
        }

        fn root(&self) -> PathBuf {
            self.0.join("project")
        }
    }

    impl Drop for Mirror {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn local_git_sets_the_sanitizing_flags_and_env() {
        let mirror = Mirror::new();
        let root = mirror.root();
        let command = local_git_with_env(
            Path::new("/usr/bin/git"),
            &root,
            &os(&["log", "-p", "--", "a.txt"]),
            [
                ("PATH".into(), "/usr/bin".into()),
                ("HOME".into(), "/home/someone".into()),
            ],
        )
        .unwrap();
        assert_eq!(command.get_program(), "/usr/bin/git");
        let args: Vec<_> = command.get_args().map(OsStr::to_owned).collect();
        let mut expected = Vec::new();
        for setting in [
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "core.untrackedCache=false",
            "diff.external=",
            "core.attributesFile=/dev/null",
            "protocol.allow=never",
            "safe.bareRepository=explicit",
        ] {
            expected.push(OsString::from("-c"));
            expected.push(OsString::from(setting));
        }
        expected.extend(os(&["log", "-p", "--", "a.txt"]));
        assert_eq!(args, expected);
        let env = envs(&command);
        let get = |name: &str| env.get(OsStr::new(name)).cloned().flatten();
        assert_eq!(get("GIT_OPTIONAL_LOCKS").unwrap(), "0");
        assert_eq!(get("GIT_CONFIG_NOSYSTEM").unwrap(), "1");
        assert_eq!(get("GIT_ATTR_NOSYSTEM").unwrap(), "1");
        assert_eq!(get("GIT_DIR").unwrap(), root.join(".git"));
        assert_eq!(get("GIT_WORK_TREE").unwrap(), root);
        assert_eq!(get("GIT_CEILING_DIRECTORIES").unwrap(), mirror.0);
        assert_eq!(get("PATH").unwrap(), "/usr/bin");
        assert_eq!(get("HOME").unwrap(), "/home/someone");
    }

    #[test]
    fn local_git_drops_inherited_git_variables() {
        let mirror = Mirror::new();
        let root = mirror.root();
        let inherited: Vec<(OsString, OsString)> = [
            ("GIT_DIR", "/elsewhere/.git"),
            ("GIT_WORK_TREE", "/elsewhere"),
            ("GIT_CONFIG_PARAMETERS", "'core.fsmonitor'='/tmp/run'"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.hooksPath"),
            ("GIT_CONFIG_VALUE_0", "/tmp/hooks"),
            ("GIT_CONFIG_GLOBAL", "/tmp/global"),
            ("GIT_EXTERNAL_DIFF", "/tmp/run"),
            ("GIT_PAGER", "/tmp/run"),
            ("GIT_COMMON_DIR", "/tmp/common"),
            ("GIT_INDEX_FILE", "/tmp/index"),
            ("GIT_OBJECT_DIRECTORY", "/tmp/objects"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/tmp/objects"),
            ("GIT_TRACE", "1"),
            ("LANG", "C.UTF-8"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();
        let command = local_git_with_env(Path::new("git"), &root, &os(&["status"]), inherited).unwrap();
        let env = envs(&command);
        let names: Vec<_> = env.keys().map(|name| name.to_string_lossy().into_owned()).collect();
        assert_eq!(
            names,
            [
                "GIT_ATTR_NOSYSTEM",
                "GIT_CEILING_DIRECTORIES",
                "GIT_CONFIG_NOSYSTEM",
                "GIT_DIR",
                "GIT_OPTIONAL_LOCKS",
                "GIT_WORK_TREE",
                "LANG",
            ]
        );
        assert_eq!(env[OsStr::new("GIT_DIR")].as_deref().unwrap(), root.join(".git"));
        assert_eq!(env[OsStr::new("GIT_WORK_TREE")].as_deref().unwrap(), root);
    }

    #[test]
    fn local_git_value_holds_through_the_struct() {
        let mirror = Mirror::new();
        let root = mirror.root();
        let git = LocalGit::with_environment("/usr/bin/git", vec![("GIT_DIR".into(), "/x".into())]);
        let command = git.command(&root, &os(&["status"])).unwrap();
        assert_eq!(git.real_git(), Path::new("/usr/bin/git"));
        assert_eq!(
            envs(&command)[OsStr::new("GIT_DIR")].as_deref().unwrap(),
            root.join(".git")
        );
        assert_eq!(command.get_args().last().unwrap(), "status");
    }

    #[test]
    fn local_git_refuses_unless_dot_git_is_a_real_directory() {
        let mirror = Mirror::new();
        let root = mirror.root();
        let git = LocalGit::with_environment("/usr/bin/git", Vec::new());
        let status = os(&["status"]);
        let refused = |git: &LocalGit| git.command(&root, &status).err().map(|error| error.to_string());
        assert_eq!(refused(&git), None);
        std::fs::remove_dir(root.join(".git")).unwrap();
        assert!(refused(&git).is_some(), "a missing .git");
        // A gitfile and a link both lead git to another repository.
        std::fs::write(root.join(".git"), "gitdir: ../elsewhere\n").unwrap();
        assert!(refused(&git).unwrap().contains("is not a directory"), "a gitfile");
        std::fs::remove_file(root.join(".git")).unwrap();
        std::fs::create_dir(mirror.0.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink("../elsewhere", root.join(".git")).unwrap();
        assert!(
            refused(&git).unwrap().contains("is not a directory"),
            "a link to a directory"
        );
    }

    #[test]
    fn flags_go_last_before_the_separator() {
        assert_eq!(
            with_flags_last(&os(&["diff", "--stat", "--", "a"]), &["--no-ext-diff", "--no-textconv"]),
            os(&["diff", "--stat", "--no-ext-diff", "--no-textconv", "--", "a"])
        );
        assert_eq!(
            with_flags_last(&os(&["log", "-p"]), &["--no-ext-diff"]),
            os(&["log", "-p", "--no-ext-diff"])
        );
        assert_eq!(
            with_flags_last(&os(&["diff", "--", "--", "b"]), &["--no-textconv"]),
            os(&["diff", "--no-textconv", "--", "--", "b"])
        );
    }
}
