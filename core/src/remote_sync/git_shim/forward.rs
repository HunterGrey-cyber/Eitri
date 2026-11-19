use super::{read_disk_status, ShimEnv};
use crate::remote_project::RemoteProject;
use crate::remote_sync::engine::FlushOutcome;
use crate::remote_sync::status_schema::StatusFile;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Conflicts {
    pub paths: Vec<String>,
    pub excluded: u64,
}

impl From<crate::remote_sync::engine::SessionStatus> for Conflicts {
    fn from(status: crate::remote_sync::engine::SessionStatus) -> Self {
        Self {
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
    cwd: &Path,
    args: &[OsString],
    whole_tree: bool,
    paths: &[PathBuf],
    services: &mut dyn Services,
    stdin: Box<dyn Read + Send>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
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
    let affected: Vec<_> = conflicts
        .paths
        .iter()
        .filter(|entry| {
            if whole_tree || conflicts.excluded > 0 {
                return true;
            }
            let conflict = project.root.join(entry);
            paths.iter().any(|path| {
                let path = super::classify::normalize(&cwd.join(path));
                let text = path.to_string_lossy();
                text.contains(['*', '?', '[', ':'])
                    || !path.starts_with(&project.root)
                    || conflict.starts_with(&path)
                    || path.starts_with(&conflict)
            })
        })
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
    let stdin: Box<dyn Read + Send> = if super::reads_stdin(args, false) {
        stdin
    } else {
        Box::new(std::io::empty())
    };
    let result = services.exec(project, cwd, args, stdin, stdout, stderr);
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
    let _ = writeln!(stderr, "Eitri: {line}");
    code
}
