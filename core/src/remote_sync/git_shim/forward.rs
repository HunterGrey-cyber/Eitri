use super::{classify::Invocation, read_disk_status, ShimEnv};
use crate::remote_project::RemoteProject;
use crate::remote_sync::actor::project_dir;
use crate::remote_sync::engine::{FlushOutcome, Problem, SessionStatus, Side, SideCounts, SyncState};
use crate::remote_sync::flush::{omitted_problems, problem_detail, tree_excluded};
use crate::remote_sync::ignores::Matcher;
use crate::remote_sync::recreate::read_created_patterns;
use crate::remote_sync::status_schema::StatusFile;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Conflicts {
    pub state: SyncState,
    pub successful_cycles: u64,
    pub paths: Vec<String>,
    pub excluded: u64,
    pub problems: Vec<Problem>,
    pub excluded_scan_problems: SideCounts,
    pub excluded_transition_problems: SideCounts,
}

impl From<SessionStatus> for Conflicts {
    fn from(status: SessionStatus) -> Self {
        Self {
            excluded_scan_problems: status.excluded_scan_problems,
            excluded_transition_problems: status.excluded_transition_problems,
            problems: status.problems,
            state: status.state,
            successful_cycles: status.successful_cycles,
            paths: status.conflicts.into_iter().map(|c| c.path).collect(),
            excluded: status.excluded_conflicts,
        }
    }
}

/// The blocking boundaries of a shim invocation. Production opens the engine and connection lazily.
pub trait Services {
    fn connect(&mut self, _project: &RemoteProject) -> Result<(), String> {
        Ok(())
    }
    fn flush(&mut self, session: &str, cap: Duration) -> FlushOutcome;
    fn conflicts(&mut self, session: &str) -> Result<Conflicts, String>;
    fn exec(
        &mut self,
        project: &RemoteProject,
        cwd: &Path,
        args: &[OsString],
        stdin: Box<dyn Read + Send>,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<i32, String>;
    fn status(&mut self, env: &ShimEnv, project: &RemoteProject) -> Option<StatusFile> {
        read_disk_status(env, project)
    }
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn pause(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub(super) fn forward(
    env: &ShimEnv,
    project: &RemoteProject,
    invocation: &Invocation,
    whole_tree: bool,
    paths: &[PathBuf],
    services: &mut dyn Services,
    stdin: Box<dyn Read + Send>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let cwd = &invocation.cwd;
    let unknown = || format!("sync state for {} is unknown; nothing was run", project.root.display());
    if !env.enabled {
        return refuse(stderr, 1, &crate::remote_project::remote_dev_off("git forwarding"));
    }
    if let Err(error) = services.connect(project) {
        return refuse(stderr, 1, &error);
    }
    if services.status(env, project).is_none() {
        return refuse(stderr, 1, &unknown());
    }
    let record = match project.sync_record() {
        Ok(Some(record)) => record,
        _ => return refuse(stderr, 1, &unknown()),
    };
    let Some(tree) = record.tree_session.as_deref() else {
        return refuse(stderr, 1, &unknown());
    };
    if let Some(code) = flush(services, tree, env.flush_cap, &project.host, stderr, true) {
        return code;
    }
    // The engine's named listing observes the flush; the actor's status file may still describe an older poll.
    let conflicts = match services.conflicts(tree) {
        Ok(conflicts) => conflicts,
        Err(error) => return refuse(stderr, 1, &format!("{} ({error})", unknown())),
    };
    // Mutagen can clear conflicts on reconnect, and a scanning status does not say whether a cycle has completed.
    let non_current = match conflicts.state {
        SyncState::Watching | SyncState::Staging | SyncState::Applying if conflicts.successful_cycles == 0 => {
            Some("still completing its first sync cycle")
        }
        SyncState::Watching | SyncState::Staging | SyncState::Applying => None,
        SyncState::Disconnected(_) => Some("disconnected"),
        SyncState::Connecting => Some("connecting"),
        SyncState::Halted(_) => Some("halted"),
        SyncState::Paused => Some("paused"),
        SyncState::Other(_) => Some("in an unknown state"),
        SyncState::Scanning => Some("scanning"),
    };
    if let Some(state) = non_current {
        return refuse(
            stderr,
            1,
            &format!("sync with {} is {state}; nothing was run", project.host),
        );
    }
    // `mv` operands and output files are literal names: Git never reads them as pathspecs, so a `[` or a leading `:`
    // in one is part of the name. For a pathspec, the literal name is checked as well as the name Git selects.
    let literal_from = paths.len() - super::classify::literal_path_count(&invocation.args, paths.len());
    let named: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let literal = super::classify::normalize(&cwd.join(path));
            let selected = if index >= literal_from {
                Some((literal.clone(), false))
            } else {
                invocation.pathspec_name(path, &project.root)
            };
            (literal, selected)
        })
        .collect();
    let affected_path = |entry: &str| {
        let affected = project.root.join(entry);
        let overlaps = |path: &Path, icase| {
            !path.starts_with(&project.root)
                || path_has_prefix(&affected, path, icase)
                || path_has_prefix(path, &affected, icase)
        };
        whole_tree
            || conflicts.excluded > 0
            || named.iter().any(|(literal, selected)| {
                overlaps(literal, false) || selected.as_ref().is_none_or(|(name, icase)| overlaps(name, *icase))
            })
    };
    if omitted_problems(
        &conflicts.excluded_scan_problems,
        &conflicts.excluded_transition_problems,
    ) {
        let (scan, transition) = (
            &conflicts.excluded_scan_problems,
            &conflicts.excluded_transition_problems,
        );
        let side = match (scan.laptop + transition.laptop, scan.server + transition.server) {
            (_, 0) => Some(Side::Laptop),
            (0, _) => Some(Side::Server),
            _ => None,
        };
        return refuse(
            stderr,
            1,
            &format!(
                "more sync problems than Mutagen lists; check :EitriSync. To proceed, {}",
                super::super::problem_actions(side, &project.host)
            ),
        );
    }
    if let Some(problem) = conflicts.problems.iter().find(|p| affected_path(&p.path)) {
        return refuse(
            stderr,
            1,
            &format!(
                "{}; check :EitriSync. To proceed, {}",
                problem_detail(problem),
                super::super::problem_actions(Some(problem.side), &project.host)
            ),
        );
    }
    let affected: Vec<_> = conflicts
        .paths
        .iter()
        .filter(|entry| affected_path(entry))
        .map(String::as_str)
        .collect();
    if !affected.is_empty() || conflicts.excluded > 0 {
        let mut details = affected.join(", ");
        if conflicts.excluded > 0 {
            if !details.is_empty() {
                details.push_str("; ");
            }
            details.push_str("more conflicts than Mutagen lists");
        }
        return refuse(
            stderr,
            1,
            &format!(
                "these files differ between this laptop and {}: {}; settle them with :EitriSync",
                project.host, details
            ),
        );
    }
    if !named.is_empty()
        && !matches!(
            super::classify::classify_invocation(&invocation.args, true),
            super::classify::Route::Local { .. }
        )
    {
        let dir = project_dir(&env.state_home, project);
        let Some(patterns) = read_created_patterns(&dir) else {
            return refuse(stderr, 1, "sync exclusions could not be read; nothing was run");
        };
        let matcher = Matcher::new(&patterns);
        let leading_revision = super::classify::leading_revision_candidate(&invocation.args);
        for (index, (operand, (literal, selected))) in paths.iter().zip(&named).enumerate() {
            if operand.to_str().is_none() {
                return refuse(stderr, 1, "remote git argument is not UTF-8; nothing was run");
            }
            // The first word of `checkout` or `reset` without `--` is a revision when it names one and no path on the
            // laptop has that name: an ignore pattern such as `*~` or `dist/` must not refuse `reset HEAD~` or
            // `checkout dist`. A word that is both stays checked as a path (Git refuses it as ambiguous).
            let could_be_revision = leading_revision && index == 0;
            if could_be_revision
                && std::fs::symlink_metadata(literal).is_err()
                && names_commit(env, project, cwd, operand)
            {
                continue;
            }
            let run_on_host = if could_be_revision {
                format!(
                    "if {} is a revision, add `--` after the revision; otherwise run it in a terminal on {}",
                    operand.display(),
                    project.host
                )
            } else {
                format!("run it in a terminal on {}", project.host)
            };
            let Some((name, icase)) = selected else {
                return refuse(
                    stderr,
                    1,
                    &format!(
                        "cannot check sync exclusions for pathspec {}; {run_on_host}",
                        operand.display()
                    ),
                );
            };
            for (path, fold_case) in [(literal, false), (name, *icase)] {
                let Ok(rel) = path.strip_prefix(&project.root) else {
                    return refuse(
                        stderr,
                        1,
                        &format!("{} is outside the sync root; {run_on_host}", path.display()),
                    );
                };
                // A server-only directory may have no entry on the laptop.
                let directory = fold_case || std::fs::symlink_metadata(path).map_or(true, |meta| meta.is_dir());
                let possible_spelling = fold_case
                    && (matcher.possibly_ignored_icase(&rel.to_string_lossy())
                        || tree_excluded(None, Path::new(&rel.to_string_lossy().to_ascii_lowercase()), true));
                if tree_excluded(Some(&matcher), rel, directory) || possible_spelling {
                    return refuse(
                        stderr,
                        1,
                        &format!(
                            "{} is excluded from sync; git on {} uses the server's copy; {run_on_host}",
                            rel.display(),
                            project.host
                        ),
                    );
                }
                if literal == name && !icase {
                    break;
                }
            }
        }
    }
    let args = invocation.forwarded_args();
    let stdin: Box<dyn Read + Send> = if super::reads_stdin(&args, false) {
        stdin
    } else {
        Box::new(std::io::empty())
    };
    let result = services.exec(project, cwd, &args, stdin, stdout, stderr);
    // A failed post-command flush is visible even when the server returned success. Both sessions are tried.
    let _ = flush(services, tree, env.flush_cap, &project.host, stderr, false);
    let _ = record
        .git_session
        .as_deref()
        .and_then(|git| flush(services, git, env.flush_cap, &project.host, stderr, false));
    match result {
        Ok(code) => code,
        Err(error) => refuse(stderr, if error.contains("nothing was run") { 1 } else { 128 }, &error),
    }
}

/// Whether `word` names a commit on the laptop's mirror: `git rev-parse --verify --quiet --end-of-options
/// <word>^{commit}` through the sanitized local git (no hooks, no repository config), given at most two seconds. A
/// mirror without history, or one that cannot run git, names nothing.
fn names_commit(env: &ShimEnv, project: &RemoteProject, cwd: &Path, word: &Path) -> bool {
    use std::process::Stdio;
    let mut spec = word.as_os_str().to_owned();
    spec.push("^{commit}");
    let args: Vec<OsString> = vec![
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        "--end-of-options".into(),
        spec,
    ];
    let Ok(mut child) = crate::remote_sync::local_git::LocalGit::new(&env.real_git)
        .command(&project.root, &args)
        .and_then(|mut command| {
            command
                .current_dir(cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        })
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn path_has_prefix(path: &Path, prefix: &Path, icase: bool) -> bool {
    if !icase {
        return path.starts_with(prefix);
    }
    let mut components = path.components();
    prefix.components().all(|expected| {
        components.next().is_some_and(|actual| {
            actual
                .as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(expected.as_os_str().as_encoded_bytes())
        })
    })
}

fn flush(
    services: &mut dyn Services,
    session: &str,
    cap: Duration,
    host: &str,
    stderr: &mut dyn Write,
    before: bool,
) -> Option<i32> {
    match services.flush(session, cap) {
        FlushOutcome::Flushed => None,
        outcome => {
            let detail = match outcome {
                FlushOutcome::Failed(text) => format!(" ({text})"),
                _ => String::new(),
            };
            let line = if before {
                format!("your last save has not reached {host}; nothing was run{detail}")
            } else {
                format!("sync after git on {host} has not finished on this laptop{detail}; check :EitriSync")
            };
            Some(refuse(stderr, 128, &line))
        }
    }
}

pub(super) fn refuse(stderr: &mut dyn Write, code: i32, line: &str) -> i32 {
    let _ = writeln!(stderr, "Eitri: {}", super::super::terminal_text(line));
    code
}
