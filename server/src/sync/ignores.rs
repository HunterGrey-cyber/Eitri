//! Bounded server-side collection of Git's ignore sources.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use eitri_core::remote_sync::ignores::{
    ancestor_source, literal, scope_repository, translate, IgnoreSource, Matcher, Origin, Translated, NON_GIT_DEFAULTS,
};
use eitri_server_proto::{check_sync_rel, SyncIgnoresResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::{login_env, sidecars};

const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const MAX_IGNORE: usize = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);
pub const NON_GIT_WARNING: &str =
    "not a git repository: only the default exclusions apply, and secrets such as .env are not excluded";

type Snapshot = Option<Result<login_env::Environment, String>>;

/// Uses the same captured login snapshot as the daemon's children, with no channel environment.
pub struct SyncIgnores {
    environment: tokio::sync::watch::Sender<Snapshot>,
}

impl Default for SyncIgnores {
    fn default() -> Self {
        let (environment, _) = tokio::sync::watch::channel(None);
        Self { environment }
    }
}

impl SyncIgnores {
    pub fn set_environment(&self, captured: &std::io::Result<login_env::Captured>) {
        self.environment.send_replace(Some(
            captured
                .as_ref()
                .map(|c| {
                    let mut env = c.env.clone();
                    login_env::absolute_path(&mut env);
                    env
                })
                .map_err(ToString::to_string),
        ));
    }

    pub async fn response(&self, root: &str) -> SyncIgnoresResponse {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        // Leave time to reap a child stopped at the work deadline.
        let result = tokio::time::timeout_at(deadline + Duration::from_secs(1), async {
            let mut receiver = self.environment.subscribe();
            let env = receiver
                .wait_for(|env| env.is_some())
                .await
                .map_err(|e| e.to_string())?
                .as_ref()
                .unwrap()
                .clone()?;
            collect(root, &env, deadline).await
        })
        .await
        .unwrap_or_else(|_| Err("ignore discovery exceeded its 10 s deadline".into()));
        match result {
            Ok(response) => response,
            Err(refusal) => SyncIgnoresResponse {
                refusal,
                ..Default::default()
            },
        }
    }
}

struct Git<'a> {
    root: &'a Path,
    env: &'a login_env::Environment,
    deadline: tokio::time::Instant,
}

impl Git<'_> {
    async fn run(&self, args: &[&str]) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
        self.run_input(args, None).await
    }

    async fn run_input(&self, args: &[&str], input: Option<Vec<u8>>) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
        let path = self.env.get(OsStr::new("PATH")).ok_or("login PATH is unavailable")?;
        let home = self.env.get(OsStr::new("HOME")).ok_or("login HOME is unavailable")?;
        let mut child = tokio::process::Command::new("git")
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(self.root)
            .env_clear()
            .env("PATH", path)
            .env("HOME", home)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot run git: {e}"))?;
        async fn read(mut reader: impl tokio::io::AsyncRead + Unpin, budget: &AtomicUsize) -> Result<Vec<u8>, String> {
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let count = reader.read(&mut buffer).await.map_err(|e| e.to_string())?;
                if count == 0 {
                    return Ok(bytes);
                }
                if budget.fetch_add(count, Ordering::Relaxed).saturating_add(count) > MAX_OUTPUT {
                    return Err("git output exceeds 16 MiB".into());
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
        }
        let budget = AtomicUsize::new(0);
        let stdin = child.stdin.take();
        let result = tokio::time::timeout_at(self.deadline, async {
            tokio::try_join!(
                read(child.stdout.take().unwrap(), &budget),
                read(child.stderr.take().unwrap(), &budget),
                async { child.wait().await.map_err(|e| e.to_string()) },
                async {
                    if let (Some(mut stdin), Some(input)) = (stdin, input) {
                        stdin.write_all(&input).await.map_err(|e| e.to_string())?;
                    }
                    Ok(())
                }
            )
        })
        .await;
        match result {
            Ok(Ok((stdout, stderr, status, ()))) => Ok((status.code().unwrap_or(-1), stdout, stderr)),
            error => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Err(match error {
                    Ok(Err(error)) => error,
                    _ => "ignore discovery exceeded its 10 s deadline".into(),
                })
            }
        }
    }

    async fn required(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let (ok, bytes, error) = self.run(args).await?;
        if ok == 0 {
            Ok(bytes)
        } else {
            Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&error).trim()
            ))
        }
    }
}

fn utf8(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| "git returned a non-UTF-8 path".into())
}

fn paths(bytes: Vec<u8>) -> Result<Vec<String>, String> {
    let text = utf8(bytes)?;
    Ok(text.split_terminator('\0').map(str::to_owned).collect())
}

/// Reads at most one byte past the cap, including for a file that grows while being read.
async fn source(
    path: &Path,
    dir: &str,
    origin: Origin,
    sources: &mut Vec<IgnoreSource>,
    problems: &mut Vec<String>,
    forced: &mut Vec<String>,
) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = path.to_owned();
    let label = if dir.is_empty() { "." } else { dir }.to_owned();
    let opened = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                libc::O_NONBLOCK
                    | if origin == Origin::GitIgnore {
                        libc::O_NOFOLLOW
                    } else {
                        0
                    },
            )
            .open(path)
    })
    .await
    .map_err(|e| e.to_string())?;
    let file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            problems.push(format!(
                "{label}: ignore file is a symbolic link; its directory is excluded"
            ));
            forced.push(if dir.is_empty() {
                "/**".into()
            } else {
                format!("/{}/", literal(dir))
            });
            return Ok(());
        }
        Err(e) => return Err(format!("cannot read ignore source: {e}")),
    };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{label}: ignore source is not a regular file"));
    }
    let mut bytes = Vec::new();
    tokio::fs::File::from_std(file)
        .take((MAX_IGNORE + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    let label = if dir.is_empty() { "." } else { dir };
    let reason = if bytes.len() > MAX_IGNORE {
        Some("exceeds 1 MiB")
    } else if std::str::from_utf8(&bytes).is_err() {
        Some("contains a non-UTF-8 line")
    } else {
        None
    };
    if let Some(reason) = reason {
        problems.push(format!("{label}: ignore file {reason}; its directory is excluded"));
        forced.push(if dir.is_empty() {
            "/**".into()
        } else {
            format!("/{}/", literal(dir))
        });
    } else {
        sources.push(IgnoreSource {
            dir: dir.into(),
            text: String::from_utf8(bytes).unwrap(),
            origin,
        });
    }
    Ok(())
}

async fn collect(
    root: &str,
    env: &login_env::Environment,
    deadline: tokio::time::Instant,
) -> Result<SyncIgnoresResponse, String> {
    let held = sidecars::open_root(root).map_err(|e| e.message().to_owned())?;
    let canonical = sidecars::canonical_root(&held).map_err(|e| e.message().to_owned())?;
    let root = Path::new(&canonical);
    let git = Git { root, env, deadline };
    let (ok, top, error) = git.run(&["rev-parse", "--show-toplevel"]).await?;
    if ok != 0 {
        let not_repository = String::from_utf8_lossy(&error).contains("not a git repository (or any ");
        let marker = match tokio::fs::symlink_metadata(root.join(".git")).await {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("cannot inspect repository marker: {e}")),
        };
        // A broken root gitdir is a Git failure, not a reason to use defaults that may expose secrets.
        if not_repository && !marker {
            let sources = [IgnoreSource {
                dir: String::new(),
                text: NON_GIT_DEFAULTS.join("\n"),
                origin: Origin::GitIgnore,
            }];
            return Ok(answer(translate(&sources, &[]), vec![], vec![NON_GIT_WARNING.into()]));
        }
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&error).trim()
        ));
    }
    let top = PathBuf::from(utf8(top)?.trim_end_matches('\n'));
    if top != root && tokio::fs::symlink_metadata(root.join(".git")).await.is_ok() {
        return Err("repository marker does not identify this repository root".into());
    }
    let root_rel = root
        .strip_prefix(&top)
        .map_err(|_| "git returned a repository outside the root")?;
    let mut sources = Vec::new();
    let mut problems = Vec::new();
    let mut forced = Vec::new();
    let (ok, exclude, error) = git
        .run(&["config", "--path", "--null", "--get", "core.excludesFile"])
        .await?;
    let excludes = if ok == 0 {
        PathBuf::from(utf8(exclude)?.trim_end_matches('\0'))
    } else if ok == 1 && error.is_empty() {
        PathBuf::from(env.get(OsStr::new("HOME")).ok_or("login HOME is unavailable")?).join(".config/git/ignore")
    } else {
        return Err(format!("git config failed: {}", String::from_utf8_lossy(&error).trim()));
    };
    let excludes = top.join(excludes);
    if ok == 0 && tokio::fs::symlink_metadata(&excludes).await.is_err() {
        problems.push(format!(
            "configured core.excludesFile {} cannot be read",
            excludes.display()
        ));
    }
    let read_excludes = source(
        &excludes,
        "",
        Origin::ExcludesFile,
        &mut sources,
        &mut problems,
        &mut forced,
    )
    .await;
    if let Err(error) = read_excludes {
        if ok != 0 {
            return Err(error);
        }
        problems.push(format!(
            "configured core.excludesFile {} cannot be read: {error}; the root is excluded",
            excludes.display()
        ));
        forced.push("/**".into());
    }
    if forced.iter().any(|pattern| pattern == "/**") {
        // No ignore-aware Git command may open the unusable configured source.
        let tracked = sync_paths(
            git.required(&["ls-files", "-z", "--cached"]).await?,
            &mut problems,
            &mut forced,
        );
        for path in &tracked {
            problems.push(format!(
                "{path} is tracked but will not sync because the sync root is excluded"
            ));
        }
        // The unusable source prevents determining which tracked paths Git would ignore.
        return Ok(answer(Translated::new(vec!["/**".into()], vec![]), vec![], problems));
    }
    let info =
        PathBuf::from(utf8(git.required(&["rev-parse", "--git-path", "info/exclude"]).await?)?.trim_end_matches('\n'));
    source(
        &root.join(info),
        "",
        Origin::InfoExclude,
        &mut sources,
        &mut problems,
        &mut forced,
    )
    .await?;
    let root_rel_text = root_rel.to_str().ok_or("root is not UTF-8")?;
    let mut ancestor = top.clone();
    while ancestor != root {
        if ancestor != top {
            let (code, _, error) = git
                .run(&[
                    "check-ignore",
                    "--no-index",
                    "-q",
                    "--",
                    ancestor.to_str().ok_or("ancestor is not UTF-8")?,
                ])
                .await?;
            if code > 1 {
                return Err(format!("git check-ignore failed: {}", String::from_utf8_lossy(&error)));
            }
            if code == 0 {
                break;
            }
        }
        let mut loaded = Vec::new();
        source(
            &ancestor.join(".gitignore"),
            "",
            Origin::GitIgnore,
            &mut loaded,
            &mut problems,
            &mut forced,
        )
        .await?;
        let from_source = root
            .strip_prefix(&ancestor)
            .unwrap()
            .to_str()
            .ok_or("ancestor is not UTF-8")?;
        sources.extend(
            loaded
                .into_iter()
                .map(|loaded| ancestor_source(&loaded.text, from_source)),
        );
        let next = root_rel
            .components()
            .nth(ancestor.strip_prefix(&top).unwrap().components().count())
            .ok_or("invalid repository root")?;
        ancestor.push(next);
    }
    if !root_rel_text.is_empty() {
        // Global and info excludes use repository-top coordinates, too.
        for source in &mut sources {
            if source.origin != Origin::GitIgnore {
                let projected = ancestor_source(&source.text, root_rel_text);
                source.text = projected.text;
            }
        }
    }
    let root_excluded = if root_rel_text.is_empty() {
        false
    } else {
        let (code, _, error) = git.run(&["check-ignore", "--no-index", "-q", "--", &canonical]).await?;
        if code > 1 {
            return Err(format!("git check-ignore failed: {}", String::from_utf8_lossy(&error)));
        }
        code == 0
    };
    if root_excluded {
        let tracked = sync_paths(
            git.required(&["ls-files", "-z", "--cached", "-i", "--exclude-standard"])
                .await?,
            &mut problems,
            &mut forced,
        );
        check_tracked_count(tracked.len())?;
        let all_tracked = sync_paths(
            git.required(&["ls-files", "-z", "--cached"]).await?,
            &mut problems,
            &mut forced,
        );
        problems.extend(
            all_tracked
                .iter()
                .map(|path| format!("{path} is tracked but will not sync because the sync root is excluded")),
        );
        return Ok(answer(Translated::new(vec!["/**".into()], vec![]), tracked, problems));
    }
    let mut tracked = Vec::new();
    let mut nested = std::collections::BTreeSet::new();
    let mut directories = vec![String::new()];
    let mut directory_bytes = 0usize;
    // Visit parents before Git can read their sources while checking the next depth.
    // Filesystem discovery also finds special .gitignore files that ls-files omits.
    while !directories.is_empty() {
        directories.sort();
        let dirs: Vec<_> = directories.iter().filter(|dir| !dir.is_empty()).cloned().collect();
        let ignored_dirs: std::collections::HashSet<_> = if dirs.is_empty() {
            Default::default()
        } else {
            let input = format!("{}\0", dirs.join("\0")).into_bytes();
            let (code, output, error) = git
                .run_input(&["check-ignore", "--no-index", "-z", "--stdin"], Some(input))
                .await?;
            if code > 1 {
                return Err(format!("git check-ignore failed: {}", String::from_utf8_lossy(&error)));
            }
            paths(output)?.into_iter().collect()
        };
        let mut next = Vec::new();
        for dir in directories {
            if ignored_dirs.contains(&dir) {
                continue;
            }
            let directory = root.join(&dir);
            if !dir.is_empty() && tokio::fs::symlink_metadata(directory.join(".git")).await.is_ok() {
                nested.insert(dir);
                continue;
            }
            source(
                &directory.join(".gitignore"),
                &dir,
                Origin::GitIgnore,
                &mut sources,
                &mut problems,
                &mut forced,
            )
            .await?;
            let mut entries = tokio::fs::read_dir(&directory).await.map_err(|e| e.to_string())?;
            while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
                if entry.file_name() == ".git" {
                    continue;
                }
                let Some(rel) = sync_name(&entry.file_name(), &dir, &mut problems, &mut forced) else {
                    continue;
                };
                if !entry.file_type().await.map_err(|e| e.to_string())?.is_dir() {
                    continue;
                }
                directory_bytes = directory_bytes.saturating_add(rel.len() + 1);
                if directory_bytes > MAX_OUTPUT {
                    return Err("ignore discovery directory names exceed 16 MiB".into());
                }
                next.push(rel);
            }
        }
        directories = next;
    }
    tracked.extend(sync_paths(
        git.required(&["ls-files", "-z", "--cached", "-i", "--exclude-standard"])
            .await?,
        &mut problems,
        &mut forced,
    ));
    check_tracked_count(tracked.len())?;
    for path in &tracked {
        check_sync_rel(path)?;
    }
    for entry in git.required(&["ls-files", "-z", "--stage"]).await?.split(|b| *b == 0) {
        if entry.starts_with(b"160000 ") {
            let (_, name) = entry.split_at(
                entry
                    .iter()
                    .position(|b| *b == b'\t')
                    .ok_or("invalid git stage entry")?
                    + 1,
            );
            use std::os::unix::ffi::OsStrExt;
            if let Some(dir) = sync_name(OsStr::from_bytes(name), "", &mut problems, &mut forced) {
                nested.insert(dir);
            }
        }
    }
    let own_tracked = tracked.clone();
    let mut nested_rules = Vec::new();
    for dir in nested {
        check_sync_rel(&dir)?;
        let nested_root = root.join(&dir);
        if !tokio::fs::symlink_metadata(&nested_root)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
            || tokio::fs::symlink_metadata(nested_root.join(".git")).await.is_err()
        {
            // An uninitialised gitlink has no repository to discover or contents to sync.
            continue;
        }
        // Recursive repositories share the work deadline and each command's output bounds.
        match Box::pin(collect(
            nested_root.to_str().ok_or("nested repository is not UTF-8")?,
            env,
            deadline,
        ))
        .await
        {
            Ok(answer) if !answer.problems.iter().any(|p| p == NON_GIT_WARNING) => {
                nested_rules.push((dir.clone(), answer.patterns));
                problems.extend(answer.problems.into_iter().map(|p| format!("{dir}: {p}")));
                tracked.extend(answer.tracked_ignored.into_iter().map(|p| format!("{dir}/{p}")));
                check_tracked_count(tracked.len())?;
            }
            result => {
                let reason = result.err().unwrap_or_else(|| "not a usable repository".into());
                if tokio::time::Instant::now() >= deadline || reason.contains("tracked-but-ignored paths exceed") {
                    return Err(reason);
                }
                problems.push(format!("{dir}: nested repository excluded: {reason}"));
                forced.push(format!("/{}/", literal(&dir)));
            }
        }
    }
    let mut translated = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || translate(&sources, &own_tracked)),
    )
    .await
    .map_err(|_| "ignore discovery exceeded its 10 s deadline".to_owned())?
    .map_err(|e| e.to_string())?;
    for (dir, patterns) in nested_rules {
        if let Err(problem) = scope_repository(&mut translated.patterns, &dir, &patterns) {
            problems.push(problem);
        }
    }
    let forced_matcher = Matcher::new(&forced);
    for path in &tracked {
        if forced_matcher.ignored(path, false) {
            let dir = path
                .match_indices('/')
                .map(|(i, _)| &path[..i])
                .find(|dir| forced_matcher.ignored(dir, true))
                .unwrap_or("the sync root");
            problems.push(format!("{path} is tracked but will not sync because {dir} is excluded"));
        }
    }
    translated.patterns.extend(forced);
    translated = Translated::new(translated.patterns, translated.problems);
    use std::os::unix::fs::MetadataExt;
    let before = held.metadata().map_err(|e| e.to_string())?;
    let after = tokio::fs::metadata(root).await.map_err(|e| e.to_string())?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || sidecars::canonical_root(&held).map_err(|e| e.message().to_owned())? != canonical
    {
        return Err("the project folder changed during ignore discovery".into());
    }
    Ok(answer(translated, tracked, problems))
}

fn answer(translated: Translated, tracked_ignored: Vec<String>, mut problems: Vec<String>) -> SyncIgnoresResponse {
    problems.extend(translated.problems);
    // Lines that say a whole directory is left out matter most; a shortened list keeps them first.
    let (mut excluded, rest): (Vec<String>, Vec<String>) =
        problems.into_iter().partition(|line| line.contains("excluded"));
    excluded.extend(rest);
    let mut problems = excluded;
    if problems.len() > 50 {
        let remaining = problems.len() - 50;
        problems.truncate(50);
        problems.push(format!("and {remaining} more"));
    }
    SyncIgnoresResponse {
        patterns: translated.patterns,
        tracked_ignored,
        digest: translated.digest,
        problems,
        refusal: String::new(),
    }
}

fn check_tracked_count(count: usize) -> Result<(), String> {
    if count > 10_000 {
        Err(format!("{count} tracked-but-ignored paths exceed the limit of 10000"))
    } else {
        Ok(())
    }
}

fn sync_name(name: &OsStr, dir: &str, problems: &mut Vec<String>, forced: &mut Vec<String>) -> Option<String> {
    let name = match name.to_str() {
        Some(name) => name,
        None => {
            problems.push(format!(
                "{} is not synced: its name is not UTF-8",
                Path::new(dir).join(name).as_os_str().as_encoded_bytes().escape_ascii()
            ));
            return None;
        }
    };
    let rel = if dir.is_empty() {
        name.into()
    } else {
        format!("{dir}/{name}")
    };
    if check_sync_rel(&rel).is_err() {
        problems.push(format!(
            "{} is not synced: its name contains control characters or unsupported path components",
            rel.escape_debug()
        ));
        // A control character cannot appear in an ignore pattern; `?` stands for it and matches a little more.
        let pattern: String = literal(&rel)
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        forced.push(format!("/{pattern}"));
        return None;
    }
    Some(rel)
}

fn sync_paths(bytes: Vec<u8>, problems: &mut Vec<String>, forced: &mut Vec<String>) -> Vec<String> {
    use std::os::unix::ffi::OsStrExt;
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|path| sync_name(OsStr::from_bytes(path), "", problems, forced))
        .collect()
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn a_name_with_a_newline_is_excluded_by_a_pattern_without_one() {
        let (mut problems, mut forced) = (Vec::new(), Vec::new());
        assert_eq!(
            sync_name(OsStr::new("bad\nname"), "dir", &mut problems, &mut forced),
            None
        );
        assert_eq!(forced, vec!["/dir/bad?name".to_owned()]);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn a_shortened_problem_list_keeps_excluded_directories_first() {
        let translated = Translated {
            patterns: Vec::new(),
            problems: vec!["deep: ignore pattern cannot be represented safely; its directory is excluded".to_owned()],
            digest: String::new(),
        };
        let many = (0..60).map(|i| format!("name {i} is not synced")).collect();
        let answer = answer(translated, Vec::new(), many);
        assert!(
            answer.problems[0].contains("its directory is excluded"),
            "{:?}",
            answer.problems
        );
        assert_eq!(answer.problems.len(), 51);
        assert_eq!(answer.problems[50], "and 11 more");
    }
}
