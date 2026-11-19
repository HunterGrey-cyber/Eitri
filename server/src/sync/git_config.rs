//! `GitConfig`: the allow-listed entries of a repository's own `.git/config`, for the laptop to write into its copy.
//!
//! Only the repository's file is read (no includes, no global, system or command-line config), the allow-list is
//! applied before answering, and the laptop applies it again. A root whose history cannot be mirrored as one plain
//! `.git` directory is refused with the reason; the laptop then runs no git session and forwards every git command.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use eitri_core::remote_sync::git_replica::{config_digest, parse_config_list, sanitize};
use eitri_server_proto::{GitConfigEntry, GitConfigResponse};
use tokio::io::AsyncReadExt;

use super::super::login_env::Environment;
use super::super::sidecars::{canonical_root, open_root};

/// The most `git config --list` output read; a repository's own config is a few kilobytes.
const MAX_LISTING: usize = 1 << 20;
/// The most of git's error output kept for a refusal.
const MAX_STDERR: usize = 16 << 10;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads the repository config with the daemon's captured login environment, omitting every Git override.
pub async fn answer_with(root: &str, environment: Environment) -> GitConfigResponse {
    let owned = root.to_owned();
    let checked = tokio::task::spawn_blocking(move || mirrorable_config(&owned)).await;
    let (git_dir, config) = match checked {
        Ok(Ok(paths)) => paths,
        Ok(Err(refusal)) => return refused(refusal),
        Err(error) => return refused(format!("checking {root} failed: {error}")),
    };
    // Resolve this directory directly so Git cannot fall back to a repository above the root.
    let mut resolve = git_command(&environment);
    resolve.args(["rev-parse", "--resolve-git-dir"]).arg(&git_dir);
    match run_git(&mut resolve, &git_dir).await {
        Ok(resolved) if resolved.strip_suffix(b"\n") == Some(git_dir.as_os_str().as_bytes()) => {}
        Ok(_) => return refused(format!("{root}/.git resolves to another directory; {FORWARDED}")),
        Err(error) => {
            return refused(format!(
                "{root}/.git is not a usable git directory ({error}); {FORWARDED}"
            ))
        }
    }
    let Some(config) = config else {
        return entries(Vec::new());
    };
    let listed = match run_git(config_command(&environment, &config).args(["--list", "-z"]), &config).await {
        Ok(listed) => listed,
        Err(refusal) => return refused(refusal),
    };
    match declares_partial_clone(&listed, &config, &environment).await {
        Ok(true) => refused(format!(
            "{root}/.git/config contains partial clone settings, so the copy may be missing objects; {FORWARDED}"
        )),
        Ok(false) => entries(sanitize(&parse_config_list(&listed))),
        Err(refusal) => refused(refusal),
    }
}

async fn declares_partial_clone(listed: &[u8], config: &Path, environment: &Environment) -> Result<bool, String> {
    // The allow-list drops these settings, and a promisor remote's subsection name can contain non-UTF-8 bytes.
    let mut has_promisor = false;
    for record in listed.split(|&byte| byte == 0) {
        let key = record.split(|&byte| byte == b'\n').next().unwrap_or_default();
        if key == b"extensions.partialclone" || remote_setting(key) == Some(b"partialclonefilter") {
            return Ok(true);
        }
        has_promisor |= remote_setting(key) == Some(b"promisor");
    }
    if !has_promisor {
        return Ok(false);
    }
    // Git normalizes implicit booleans and numeric spellings. Any true entry registers a promisor remote.
    let mut command = config_command(environment, config);
    command.args(["--type=bool", "-z", "--get-regexp", "^remote[.](.*[.])?promisor$"]);
    let values = run_git(&mut command, config).await?;
    Ok(values.split(|&byte| byte == 0).any(|record| {
        record
            .strip_suffix(b"\ntrue")
            .is_some_and(|key| remote_setting(key) == Some(b"promisor"))
    }))
}

fn remote_setting(key: &[u8]) -> Option<&[u8]> {
    let key = key.strip_prefix(b"remote.")?;
    // Git ignores promisor remotes whose name starts with a slash.
    if key.starts_with(b"/") {
        return None;
    }
    key.rsplit(|&byte| byte == b'.').next()
}

fn refused(refusal: String) -> GitConfigResponse {
    GitConfigResponse {
        entries: Vec::new(),
        digest: String::new(),
        refusal,
    }
}

fn entries(kept: Vec<(String, String)>) -> GitConfigResponse {
    GitConfigResponse {
        digest: config_digest(&kept),
        entries: kept
            .into_iter()
            .map(|(key, value)| GitConfigEntry { key, value })
            .collect(),
        refusal: String::new(),
    }
}

const FORWARDED: &str = "git commands there run on the server";

/// The plain `.git` directory and its optional config, or the reason it cannot be mirrored.
fn mirrorable_config(root: &str) -> Result<(PathBuf, Option<PathBuf>), String> {
    // Absolute, normal form, an existing directory this account owns: the checks every root-taking call makes.
    let held = open_root(root).map_err(|status| status.message().to_owned())?;
    let canonical = PathBuf::from(canonical_root(&held).map_err(|status| status.message().to_owned())?);
    let git_dir = canonical.join(".git");
    let kind = match std::fs::symlink_metadata(&git_dir) {
        Ok(meta) => meta.file_type(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Git looks upwards from a directory without `.git`: a root below a repository's top level shares that
            // repository, whose `.git` is not under the root.
            return Err(
                match canonical.ancestors().skip(1).find(|dir| has_entry(&dir.join(".git"))) {
                    Some(top) => format!(
                        "{root} is inside the repository at {}, below its top level; {FORWARDED}",
                        top.display()
                    ),
                    None => format!("{root} is not a git repository"),
                },
            );
        }
        Err(error) => return Err(format!("cannot read {}: {error}", git_dir.display())),
    };
    if kind.is_file() {
        return Err(format!(
            "{root} is a linked worktree or a submodule checkout (its .git is a file); {FORWARDED}"
        ));
    }
    if !kind.is_dir() {
        return Err(format!("{root}/.git is not a directory; {FORWARDED}"));
    }
    if has_entry(&git_dir.join("commondir")) {
        return Err(format!(
            "{root}/.git takes its config and history from another directory (commondir); {FORWARDED}"
        ));
    }
    if has_entry(&git_dir.join("objects/info/alternates")) {
        return Err(format!(
            "{root}/.git borrows objects from another repository (objects/info/alternates), so a copy of it would \
             be incomplete; {FORWARDED}"
        ));
    }
    // A replica needs its own HEAD, objects and refs. Sync excludes symbolic links.
    if !std::fs::symlink_metadata(git_dir.join("HEAD")).is_ok_and(|meta| meta.is_file())
        || !std::fs::symlink_metadata(git_dir.join("objects")).is_ok_and(|meta| meta.is_dir())
        || !std::fs::symlink_metadata(git_dir.join("refs")).is_ok_and(|meta| meta.is_dir())
    {
        return Err(format!(
            "{root}/.git requires a regular HEAD file and objects and refs directories to be mirrored; {FORWARDED}"
        ));
    }
    let config = git_dir.join("config");
    match std::fs::symlink_metadata(&config) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "{root}/.git/config is a symbolic link, so its entries would come from another file; {FORWARDED}"
        )),
        Ok(meta) if meta.is_file() => Ok((git_dir, Some(config))),
        Ok(_) => Err(format!("{root}/.git/config is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((git_dir, None)),
        Err(error) => Err(format!("cannot read {}: {error}", config.display())),
    }
}

fn has_entry(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn git_command(environment: &Environment) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .env_clear()
        .envs(
            environment
                .iter()
                .filter(|(name, _)| !name.as_bytes().starts_with(b"GIT_")),
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn config_command(environment: &Environment, config: &Path) -> tokio::process::Command {
    let mut command = git_command(environment);
    command.args(["config", "--no-includes", "--file"]).arg(config);
    command
}

async fn run_git(command: &mut tokio::process::Command, path: &Path) -> Result<Vec<u8>, String> {
    let shown = path.display().to_string();
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run git to read {shown}: {error}"))?;
    let outcome = tokio::time::timeout(GIT_TIMEOUT, collect(&mut child, &shown)).await;
    let refusal = match outcome {
        Ok(Ok(listed)) => return Ok(listed),
        Ok(Err(refusal)) => refusal,
        Err(_) => format!("reading {shown} took longer than {} s", GIT_TIMEOUT.as_secs()),
    };
    // A child still running (output over the cap, or too slow) is killed through its own handle and reaped.
    let _ = child.kill().await;
    Err(refusal)
}

/// Reads at most [`MAX_LISTING`] + 1 bytes of the listing and [`MAX_STDERR`] of its errors, then the exit status.
async fn collect(child: &mut tokio::process::Child, shown: &str) -> Result<Vec<u8>, String> {
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    // Errors are read on their own task, so a listing over the cap is refused at once rather than after git exits.
    let errors = tokio::spawn(async move {
        let mut errors = Vec::new();
        let _ = stderr.take(MAX_STDERR as u64).read_to_end(&mut errors).await;
        errors
    });
    let mut listed = Vec::new();
    stdout
        .take(MAX_LISTING as u64 + 1)
        .read_to_end(&mut listed)
        .await
        .map_err(|error| format!("cannot read git's listing of {shown}: {error}"))?;
    if listed.len() > MAX_LISTING {
        return Err(format!("{shown} lists more than {MAX_LISTING} bytes"));
    }
    let status = child
        .wait()
        .await
        .map_err(|error| format!("cannot wait for git reading {shown}: {error}"))?;
    if !status.success() {
        let errors = errors.await.unwrap_or_default();
        return Err(format!(
            "git could not read {shown}: {}",
            String::from_utf8_lossy(&errors).trim()
        ));
    }
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::process::{Command, Output};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(tag: &str) -> Scratch {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        // Under /tmp rather than TMPDIR, so no repository above it changes what a root is inside.
        let path = PathBuf::from(format!(
            "/tmp/eitri-gitconfig-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("home")).unwrap();
        std::fs::write(
            path.join("home/.gitconfig"),
            "[user]\n\tname = From Global\n\temail = global@example.com\n[remote \"g\"]\n\turl = global\n",
        )
        .unwrap();
        Scratch(path)
    }

    fn environment(home: &Path) -> Environment {
        Environment::from([
            (OsString::from("PATH"), std::env::var_os("PATH").unwrap_or_default()),
            ("HOME".into(), home.into()),
        ])
    }

    fn git(scratch: &Scratch, cwd: &Path, args: &[&str]) {
        let output = git_output(scratch, cwd, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(scratch: &Scratch, cwd: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .env_clear()
            .envs(environment(&scratch.0.join("home")))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Someone")
            .env("GIT_AUTHOR_EMAIL", "someone@example.com")
            .env("GIT_COMMITTER_NAME", "Someone")
            .env("GIT_COMMITTER_EMAIL", "someone@example.com")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
    }

    fn repository(scratch: &Scratch, name: &str) -> PathBuf {
        git(scratch, &scratch.0, &["init", "-q", "-b", "main", name]);
        let root = scratch.0.join(name);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(scratch, &root, &["add", "-A"]);
        git(scratch, &root, &["commit", "-q", "-m", "initial"]);
        root
    }

    async fn ask(scratch: &Scratch, root: &Path) -> GitConfigResponse {
        answer_with(root.to_str().unwrap(), environment(&scratch.0.join("home"))).await
    }

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn helper_reads_only_the_repository_file() {
        let scratch = scratch("only");
        let root = repository(&scratch, "repo");
        std::fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfsmonitor = /tmp/run\n\
             \thooksPath = /tmp/hooks\n\tworktree = /elsewhere\n[include]\n\tpath = included\n\
             [includeIf \"gitdir:/\"]\n\tpath = included\n[remote \"origin\"]\n\turl = git@example.com:p.git\n\
             \tfetch = +refs/heads/*:refs/remotes/origin/*\n\tuploadpack = /tmp/run\n\
             \tfetch = +refs/tags/*:refs/tags/*\n[filter \"x\"]\n\tclean = /tmp/run\n[diff \"x\"]\n\
             \ttextconv = /tmp/run\n[alias]\n\tst = !/tmp/run\n[user]\n\tname = Repo Person\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".git/included"),
            "[user]\n\temail = included@example.com\n[core]\n\tbare = true\n",
        )
        .unwrap();
        let mut env = environment(&scratch.0.join("home"));
        // A daemon started from a channel that set these must not let them add entries or move the file.
        for (name, value) in [
            ("GIT_CONFIG_PARAMETERS", "'user.email'='param@example.com'"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "user.email"),
            ("GIT_CONFIG_VALUE_0", "count@example.com"),
            ("GIT_DIR", "/nonexistent"),
            ("GIT_CONFIG", "/nonexistent"),
        ] {
            env.insert(name.into(), value.into());
        }
        let response = answer_with(root.to_str().unwrap(), env).await;
        assert_eq!(response.refusal, "");
        let got: Vec<_> = response.entries.into_iter().map(|e| (e.key, e.value)).collect();
        let expected = pairs(&[
            ("core.repositoryformatversion", "0"),
            ("core.bare", "false"),
            ("remote.origin.url", "git@example.com:p.git"),
            ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
            ("remote.origin.fetch", "+refs/tags/*:refs/tags/*"),
            ("user.name", "Repo Person"),
        ]);
        assert_eq!(got, expected);
        assert_eq!(response.digest, config_digest(&expected));

        // A repository without a config file has no entries, and that is not a refusal.
        std::fs::remove_file(root.join(".git/config")).unwrap();
        let response = ask(&scratch, &root).await;
        assert_eq!((response.refusal.as_str(), response.entries.len()), ("", 0));
        assert_eq!(response.digest, config_digest(&[]));
    }

    #[tokio::test]
    async fn a_listing_over_the_cap_is_refused() {
        let scratch = scratch("large");
        let root = repository(&scratch, "repo");
        let line = format!("\tname = {}\n", "x".repeat(1000));
        std::fs::write(root.join(".git/config"), format!("[user]\n{}", line.repeat(1200))).unwrap();
        let response = ask(&scratch, &root).await;
        assert!(
            response
                .refusal
                .contains(&format!("lists more than {MAX_LISTING} bytes")),
            "{}",
            response.refusal
        );
        assert!(response.entries.is_empty() && response.digest.is_empty());
    }

    #[tokio::test]
    async fn subdir_with_an_empty_git_directory_is_refused() {
        let scratch = scratch("empty-git");
        let root = repository(&scratch, "repo");
        let subdir = root.join("sub");
        std::fs::create_dir_all(subdir.join(".git")).unwrap();

        let response = ask(&scratch, &subdir).await;
        assert!(response.refusal.contains(FORWARDED), "{response:?}");
        assert!(response.entries.is_empty() && response.digest.is_empty());
    }

    #[tokio::test]
    async fn partial_git_dirs_that_git_skips_are_refused() {
        let scratch = scratch("partial-gitdir");
        let root = repository(&scratch, "repo");
        let subdir = root.join("child");
        let git_dir = subdir.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir(git_dir.join("refs")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "").unwrap();
        for missing in ["HEAD contents", "refs directory"] {
            let discovery = git_output(&scratch, &subdir, &["rev-parse", "--show-prefix"]);
            assert!(discovery.status.success());
            assert_eq!(discovery.stdout, b"child/\n", "{missing}");

            let response = ask(&scratch, &subdir).await;
            assert!(response.refusal.contains(FORWARDED), "{missing}: {response:?}");
            assert!(response.entries.is_empty() && response.digest.is_empty());

            if missing == "HEAD contents" {
                std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
                std::fs::remove_dir(git_dir.join("refs")).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn partial_clone_eligibility_matches_git_promisor_settings() {
        let scratch = scratch("promisor-settings");
        let root = repository(&scratch, "repo");
        let config = root.join(".git/config");
        let original = std::fs::read_to_string(&config).unwrap();
        for (settings, promisor) in [
            ("partialCloneFilter = blob:none", true),
            ("promisor = false\npartialCloneFilter = blob:none", true),
            ("promisor", true),
            ("promisor = true\npromisor = false", true),
            ("promisor = false", false),
            ("promisor = OFF", false),
            ("promisor =", false),
            ("promisor = 0x0k", false),
        ] {
            std::fs::write(
                &config,
                format!("{original}\n[remote \"origin.with.dots\"]\nurl = /srv/project\n{settings}\n"),
            )
            .unwrap();
            let output = git_output(&scratch, &root, &["cat-file", "-p", &"1".repeat(40)]);
            assert!(!output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(stderr.contains("transport 'file' not allowed"), promisor, "{stderr}");

            let response = ask(&scratch, &root).await;
            assert_eq!(response.refusal.is_empty(), !promisor, "{settings}: {response:?}");
            if promisor {
                assert!(response.refusal.contains("partial clone"), "{response:?}");
            }
        }
    }

    #[tokio::test]
    async fn partial_clone_configs_are_refused() {
        let scratch = scratch("partial");
        let root = repository(&scratch, "repo");
        for config in [
            b"[remote \"origin.with.dots\"]\n\tPromisor = true\n\tpartialCloneFilter = blob:none\n".as_slice(),
            b"[extensions]\n\tpartialClone = origin\n",
            b"[remote \"origin-\xff\"]\n\tpromisor\n",
        ] {
            std::fs::write(root.join(".git/config"), config).unwrap();

            let response = ask(&scratch, &root).await;
            assert!(response.refusal.contains("partial clone"), "{response:?}");
            assert!(response.refusal.contains(FORWARDED), "{response:?}");
            assert!(response.entries.is_empty() && response.digest.is_empty());
        }
    }

    #[tokio::test]
    async fn linked_worktree_commondir_and_subdir_roots_are_refused_with_the_reason() {
        let scratch = scratch("refused");
        let root = repository(&scratch, "repo");
        git(&scratch, &root, &["worktree", "add", "-q", "../linked"]);
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        let common = repository(&scratch, "common");
        std::fs::write(common.join(".git/commondir"), "../repo/.git\n").unwrap();
        let borrowed = repository(&scratch, "borrowed");
        std::fs::write(
            borrowed.join(".git/objects/info/alternates"),
            format!("{}/.git/objects\n", root.display()),
        )
        .unwrap();
        let linked_git = repository(&scratch, "linked-git");
        std::fs::rename(linked_git.join(".git"), scratch.0.join("moved.git")).unwrap();
        std::os::unix::fs::symlink(scratch.0.join("moved.git"), linked_git.join(".git")).unwrap();
        let plain = scratch.0.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let linked_config = repository(&scratch, "linked-config");
        std::fs::rename(linked_config.join(".git/config"), scratch.0.join("moved-config")).unwrap();
        std::os::unix::fs::symlink(scratch.0.join("moved-config"), linked_config.join(".git/config")).unwrap();

        let cases = [
            (
                scratch.0.join("linked"),
                "linked worktree or a submodule checkout (its .git is a file)".to_owned(),
            ),
            (
                root.join("sub/deeper"),
                format!("inside the repository at {}, below its top level", root.display()),
            ),
            (common.clone(), "(commondir)".to_owned()),
            (borrowed.clone(), "(objects/info/alternates)".to_owned()),
            (linked_git.clone(), ".git is not a directory".to_owned()),
            (linked_config.clone(), ".git/config is a symbolic link".to_owned()),
        ];
        // The scratch directory is not a repository, but another process may make one of its ancestors one for a
        // moment (temporary directories are shared); either answer names why it cannot be mirrored.
        let response = ask(&scratch, &plain).await;
        assert!(
            response.refusal.contains("is not a git repository") || response.refusal.contains("below its top level"),
            "{}: {:?}",
            plain.display(),
            response.refusal
        );
        for (dir, reason) in cases {
            let response = ask(&scratch, &dir).await;
            assert!(
                response.refusal.contains(&reason),
                "{}: {:?} lacks {reason:?}",
                dir.display(),
                response.refusal
            );
            assert!(
                response.refusal.starts_with(dir.to_str().unwrap()),
                "{}",
                response.refusal
            );
            assert!(response.entries.is_empty() && response.digest.is_empty());
        }
        for (root, reason) in [
            ("relative/root", "root must be absolute"),
            ("/tmp//x", "root must be in normal form"),
        ] {
            let response = answer_with(root, environment(&scratch.0.join("home"))).await;
            assert_eq!(response.refusal, reason);
        }
        // The repository itself is mirrorable.
        assert_eq!(ask(&scratch, &root).await.refusal, "");
    }
}
