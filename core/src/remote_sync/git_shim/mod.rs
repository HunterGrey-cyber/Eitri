//! Git routing for a local replica: reads stay local, writes flush and execute once on the server.

mod classify;
pub mod forward;
pub use classify::{classify, reads_stdin, Route};

use super::engine::{FlushOutcome, SyncEngine};
use super::local_git::{with_flags_last, LocalGit, SANITIZING_CONFIG};
use super::mutagen::MutagenEngine;
use super::status_schema::{read_status, StatusFile};
use super::SyncKey;
use crate::remote_project::{RemoteConnection, RemoteProject, RemoteProjects};
use forward::{refuse, Services};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ShimEnv {
    pub cwd: PathBuf,
    pub state_home: PathBuf,
    pub real_git: PathBuf,
    pub exe: PathBuf,
    pub projects: Vec<RemoteProject>,
    pub flush_cap: Duration,
    pub mutagen: Option<PathBuf>,
    pub git_environment: Vec<(OsString, OsString)>,
    pub unreadable_records: Vec<PathBuf>,
    pub enabled: bool,
}

impl ShimEnv {
    pub fn from_process() -> Result<Self, String> {
        let home = std::env::var_os("HOME");
        let state = std::env::var_os("XDG_STATE_HOME");
        let state_home = crate::state_paths::state_subdir(state.as_deref(), home.as_deref(), "")
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .ok_or("no state home")?;
        let store = RemoteProjects::new(state.as_deref(), home.as_deref());
        let (projects, errors) = store.list().map_err(|e| e.to_string())?;
        let options = super::shim_dir::ShimOptions::default();
        Ok(Self {
            // An unnamed deleted cwd cannot match a recorded absolute root; untouched Git still inherits it.
            cwd: std::env::current_dir()
                .unwrap_or_else(|_| std::env::var_os("PWD").map(PathBuf::from).unwrap_or_default()),
            state_home,
            real_git: super::shim_dir::real_git_from_process()?,
            exe: std::env::current_exe().map_err(|e| e.to_string())?,
            projects,
            flush_cap: Duration::from_secs_f64(options.flush_cap),
            mutagen: options.mutagen,
            git_environment: std::env::vars_os()
                .filter(|(k, _)| k.to_string_lossy().starts_with("GIT_"))
                .collect(),
            unreadable_records: errors
                .iter()
                .filter_map(|e| e.split_once(": ").map(|(p, _)| PathBuf::from(p)))
                .collect(),
            enabled: options.enabled,
        })
    }
}

pub fn run(args: Vec<OsString>, mut env: ShimEnv) -> i32 {
    let parsed = classify::Invocation::parse(&args, &env.cwd, None);
    if select_project(&parsed, &env).is_none() && unreadable_record(&parsed, &env).is_none() {
        return exec_real_git(&args, &env.real_git);
    }
    if select_project(&parsed, &env)
        .is_some_and(|project| matches!(project_invocation(&args, &env, project).1, Route::Forward { .. }))
    {
        let options = super::shim_dir::ShimOptions::from_process().unwrap_or_else(|error| {
            eprintln!("Eitri: {error}; using default git shim settings");
            super::shim_dir::ShimOptions::default()
        });
        env.enabled = options.enabled;
        env.flush_cap = Duration::from_secs_f64(options.flush_cap);
        env.mutagen = options.mutagen;
    }
    let mut services = SystemServices {
        env: &env,
        engine: None,
        connection: None,
    };
    run_with(
        &args,
        &env,
        &mut services,
        Box::new(std::io::stdin()),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// Worker-only entry with injectable network and engine boundaries; local commands always use real git.
pub fn run_with(
    args: &[OsString],
    env: &ShimEnv,
    services: &mut dyn Services,
    stdin: Box<dyn Read + Send>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let initial = classify::Invocation::parse(args, &env.cwd, None);
    if let Some(path) = unreadable_record(&initial, env) {
        return refuse(
            stderr,
            128,
            &format!("project record {} is unreadable; nothing was run", path.display()),
        );
    }
    let Some(project) = select_project(&initial, env) else {
        return plain_git(&env.real_git, args, &env.cwd, stdout, stderr);
    };
    let (parsed, route) = project_invocation(args, env, project);
    if matches!(route, Route::Forward { .. }) {
        if let Some(name) = moving_environment(env, project, &initial.cwd) {
            return refuse(
                stderr,
                128,
                &format!(
                    "{name} changes the repository or configuration for {}; nothing was run",
                    project.root.display()
                ),
            );
        }
    }
    match route {
        Route::RefuseEditor => refuse(
            stderr,
            1,
            &format!("commit from the terminal (it runs on {}) or with -m", project.host),
        ),
        Route::Forward { whole_tree, paths } => forward::forward(
            env,
            project,
            &parsed.cwd,
            &parsed.forwarded_args(),
            whole_tree,
            &paths,
            services,
            stdin,
            stdout,
            stderr,
        ),
        Route::Local { extra } => {
            let start = services.now();
            let mut status = services.status(env, project);
            for _ in 0..50 {
                let elapsed = services.now().saturating_duration_since(start);
                if !status.as_ref().is_some_and(|s| s.cycle_running) || elapsed >= Duration::from_secs(1) {
                    break;
                }
                services.pause(Duration::from_millis(20).min(Duration::from_secs(1) - elapsed));
                status = services.status(env, project);
            }
            let ready = status
                .as_ref()
                .map(|s| s.git_ready)
                .unwrap_or_else(|| project.sync_record().ok().flatten().is_some_and(|s| s.git_ready));
            if !ready {
                let pct = status.as_ref().map(|s| s.git_pct).unwrap_or(0);
                return refuse(
                    stderr,
                    128,
                    &format!(
                        "git history for {} is still arriving from {} ({} %)",
                        project.root.display(),
                        project.host,
                        pct
                    ),
                );
            }
            let mut local_args = parsed.globals;
            // Caller globals precede the final overrides, so -c cannot reopen hooks or external helpers.
            for setting in SANITIZING_CONFIG {
                local_args.push("-c".into());
                local_args.push((*setting).into());
            }
            if parsed.args.first().is_some_and(|arg| arg == "grep") {
                // Grep never invokes an external diff and rejects --no-ext-diff; textconv still needs disabling.
                let extra: Vec<_> = extra.into_iter().filter(|flag| *flag != "--no-ext-diff").collect();
                local_args.extend(grep_arguments(&parsed.args, &extra));
            } else if !extra.is_empty()
                && !matches!(parsed.args.first().and_then(|s| s.to_str()), Some("blame" | "annotate"))
            {
                // Revision parsers require these options before a revision or implicit path. They do not abbreviate options.
                let split = if parsed.args.first().is_some_and(|s| s == "stash")
                    || parsed.args.first().is_some_and(|s| s == "reflog")
                        && parsed.args.get(1).is_some_and(|s| s == "show")
                {
                    2
                } else {
                    1
                };
                local_args.extend(with_flags_last(&parsed.args[..split], &extra));
                local_args.extend_from_slice(&parsed.args[split..]);
            } else {
                local_args.extend(with_flags_last(&parsed.args, &extra));
            }
            match LocalGit::new(&env.real_git)
                .command(&project.root, &local_args)
                .and_then(|mut cmd| {
                    cmd.current_dir(&parsed.cwd)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                }) {
                Ok(child) => stream_child(child, stdin, stdout, stderr),
                Err(error) => refuse(stderr, 128, &error.to_string()),
            }
        }
        Route::NotRemote => plain_git(&env.real_git, args, &env.cwd, stdout, stderr),
    }
}

fn project_invocation(args: &[OsString], env: &ShimEnv, project: &RemoteProject) -> (classify::Invocation, Route) {
    let mut parsed = classify::Invocation::parse(args, &env.cwd, Some(&project.root));
    let relative = relative_to(&parsed.cwd, &project.root).unwrap_or_default();
    parsed.cwd = project.root.join(relative);
    let mut route = classify(&parsed.args, &project.root);
    if !matches!(route, Route::RefuseEditor) && (parsed.foreign || !has_history(project)) {
        route = Route::Forward {
            whole_tree: true,
            paths: vec![],
        };
    }
    (parsed, route)
}

fn grep_arguments(args: &[OsString], extra: &[&str]) -> Vec<OsString> {
    // Grep stops accepting options at its first positional argument. Keep either option terminator intact.
    let split = grep_positionals(args);
    let mut result = with_flags_last(&args[..split], extra);
    result.extend_from_slice(&args[split..]);
    result
}

fn grep_positionals(args: &[OsString]) -> usize {
    let mut value = false;
    for (i, arg) in args.iter().enumerate().skip(1) {
        if value {
            value = false;
            continue;
        }
        let arg = arg.to_string_lossy();
        if arg == "--" || arg == "--end-of-options" || !arg.starts_with('-') {
            return i;
        }
        value = matches!(
            arg.as_ref(),
            "-e" | "--regexp"
                | "-f"
                | "--file"
                | "-m"
                | "-A"
                | "-B"
                | "-C"
                | "--max-depth"
                | "--threads"
                | "--max-count"
                | "--after-context"
                | "--before-context"
                | "--context"
        );
    }
    args.len()
}

fn stream_child(
    mut child: std::process::Child,
    mut input: Box<dyn Read + Send>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if let Some(mut pipe) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut input, &mut pipe);
        });
    }
    let (send, receive) = std::sync::mpsc::sync_channel(8);
    for (is_err, mut pipe) in [
        (false, Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>),
        (true, Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>),
    ] {
        let send = send.clone();
        std::thread::spawn(move || {
            let mut bytes = vec![0; 64 * 1024];
            loop {
                match pipe.read(&mut bytes) {
                    Ok(0) => return,
                    Ok(n) => {
                        if send.send((is_err, Ok(bytes[..n].to_vec()))).is_err() {
                            return;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = send.send((is_err, Err(error)));
                        return;
                    }
                }
            }
        });
    }
    drop(send);
    let mut error = None;
    for (is_err, bytes) in receive {
        let target: &mut dyn Write = if is_err { &mut *stderr } else { &mut *stdout };
        if let Err(why) = bytes.and_then(|bytes| {
            target.write_all(&bytes)?;
            target.flush()
        }) {
            error = Some(why);
            let _ = child.kill();
        }
    }
    match child.wait() {
        Ok(status) if error.is_none() => exit_code(status),
        result => refuse(
            stderr,
            128,
            &error
                .map(|e| e.to_string())
                .unwrap_or_else(|| result.unwrap_err().to_string()),
        ),
    }
}

fn has_history(project: &RemoteProject) -> bool {
    project
        .sync_record()
        .ok()
        .flatten()
        .is_some_and(|r| r.git_session.is_some())
        && std::fs::symlink_metadata(project.root.join(".git")).is_ok_and(|m| m.is_dir())
}

fn read_disk_status(env: &ShimEnv, project: &RemoteProject) -> Option<StatusFile> {
    let key = SyncKey::new(&project.host_key_sha256, &project.root);
    let path = env
        .state_home
        .join("eitri/remote/sync")
        .join(key.as_str())
        .join("status.json");
    let bytes = agent::private_fs::read_private_nofollow_bytes_capped(&path, 16 * 1024 * 1024).ok()?;
    read_status(&bytes).ok()
}

fn plain_git(git: &Path, args: &[OsString], cwd: &Path, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    match Command::new(git).args(args).current_dir(cwd).output() {
        Ok(output) => {
            let _ = stdout.write_all(&output.stdout);
            let _ = stderr.write_all(&output.stderr);
            exit_code(output.status)
        }
        Err(error) => refuse(stderr, 128, &error.to_string()),
    }
}

pub fn exec_real_git(args: &[OsString], git: &Path) -> i32 {
    use std::os::unix::process::CommandExt;
    let error = Command::new(git).args(args).exec();
    eprintln!("git: {error}");
    128
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

struct SystemServices<'a> {
    env: &'a ShimEnv,
    engine: Option<MutagenEngine>,
    connection: Option<RemoteConnection>,
}

impl Services for SystemServices<'_> {
    fn flush(&mut self, session: &str, cap: Duration) -> FlushOutcome {
        if self.engine.is_none() {
            match MutagenEngine::open(
                &self.env.state_home,
                self.env.mutagen.as_deref(),
                &self.env.state_home.join("eitri/mutagen/transport"),
            ) {
                Ok(engine) => self.engine = Some(engine),
                Err(error) => return FlushOutcome::Failed(error.to_string()),
            }
        }
        self.engine.as_ref().unwrap().flush(session, cap)
    }

    fn conflicts(&mut self, session: &str) -> Result<forward::Conflicts, String> {
        self.engine
            .as_ref()
            .ok_or("sync engine is not open")?
            .session_status(session)
            .map(forward::Conflicts::from)
            .map_err(|e| e.to_string())
    }

    fn connect(&mut self, project: &RemoteProject) -> Result<(), String> {
        if self.connection.is_none() {
            let store = RemoteProjects::new(Some(self.env.state_home.as_os_str()), None);
            let ssh_dir = self.env.state_home.join("eitri/ssh");
            let master = agent::ssh::ControlMaster::ensure(&project.host, &ssh_dir, &self.env.exe)
                .map_err(|_| offline(project))?;
            let connection = RemoteConnection::open_over_ssh(
                &master,
                &store,
                project,
                &project.helper_version,
                agent::BUNDLED_SIDECAR_REVISION,
            )
            .map_err(|error| format!("{} ({error})", offline(project)))?;
            self.connection = Some(connection);
        }
        Ok(())
    }

    fn exec(
        &mut self,
        project: &RemoteProject,
        cwd: &Path,
        args: &[OsString],
        stdin: Box<dyn Read + Send>,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<i32, String> {
        self.connect(project)?;
        self.connection
            .as_ref()
            .unwrap()
            .exec_git(cwd, args, stdin, stdout, stderr)
    }
}

fn offline(project: &RemoteProject) -> String {
    format!(
        "git writes in this copy go to {}, which cannot be reached; nothing was run",
        project.host
    )
}

fn relative_to(cwd: &Path, root: &Path) -> Option<PathBuf> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| classify::normalize(cwd));
    let canonical_root = root.canonicalize().unwrap_or_else(|_| classify::normalize(root));
    cwd.strip_prefix(&canonical_root)
        .ok()
        .or_else(|| cwd.strip_prefix(root).ok())
        .map(Path::to_path_buf)
}

fn repository_targets(parsed: &classify::Invocation, env: &ShimEnv) -> Vec<PathBuf> {
    let get = |name: &str| {
        env.git_environment
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| parsed.cwd.join(v))
    };
    let explicit = parsed.git_dir.is_some() || parsed.work_tree.is_some();
    let git = if explicit {
        parsed.git_dir.clone()
    } else {
        get("GIT_DIR")
    };
    let tree = if explicit {
        parsed.work_tree.clone()
    } else {
        get("GIT_WORK_TREE")
    };
    if git.is_none() && tree.is_none() {
        return vec![parsed.cwd.clone()];
    }
    // A work-tree override alone leaves Git discovering its gitdir from cwd.
    let discovery = git.is_none().then(|| parsed.cwd.clone());
    tree.into_iter()
        .chain(git.map(|p| {
            if p.file_name().is_some_and(|n| n == ".git") {
                p.parent().unwrap().to_owned()
            } else {
                p
            }
        }))
        .chain(discovery)
        .collect()
}

fn select_project<'a>(parsed: &classify::Invocation, env: &'a ShimEnv) -> Option<&'a RemoteProject> {
    let targets = repository_targets(parsed, env);
    env.projects
        .iter()
        .filter(|p| targets.iter().any(|t| relative_to(t, &p.root).is_some()))
        .max_by_key(|p| p.root.components().count())
}

fn moving_environment(env: &ShimEnv, project: &RemoteProject, cwd: &Path) -> Option<String> {
    env.git_environment.iter().find_map(|(key, value)| {
        let name = key.to_string_lossy();
        if matches!(name.as_ref(), "GIT_DIR" | "GIT_WORK_TREE") {
            let expected = if name == "GIT_DIR" {
                project.root.join(".git")
            } else {
                project.root.clone()
            };
            let equal = cwd
                .join(value)
                .canonicalize()
                .ok()
                .zip(expected.canonicalize().ok())
                .is_some_and(|(a, b)| a == b);
            (!equal).then(|| name.into_owned())
        } else if matches!(
            name.as_ref(),
            "GIT_INDEX_FILE"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_COMMON_DIR"
                | "GIT_NAMESPACE"
                | "GIT_CONFIG_PARAMETERS"
                | "GIT_CONFIG_COUNT"
        ) || name.starts_with("GIT_CONFIG_KEY_")
            || name.starts_with("GIT_CONFIG_VALUE_")
        {
            Some(name.into_owned())
        } else {
            None
        }
    })
}

fn unreadable_record(parsed: &classify::Invocation, env: &ShimEnv) -> Option<PathBuf> {
    for target in repository_targets(parsed, env) {
        let target = target.canonicalize().unwrap_or_else(|_| classify::normalize(&target));
        for ancestor in target.ancestors() {
            let key = crate::remote_project::project_key(ancestor);
            if let Some(path) = env
                .unreadable_records
                .iter()
                .find(|p| p.file_stem().is_some_and(|n| n == key.as_str()))
            {
                return Some(path.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grep_preserves_end_of_options_after_the_safety_flags() {
        let args: Vec<_> = ["grep", "-e", "first", "--end-of-options", "file"]
            .map(OsString::from)
            .to_vec();
        assert_eq!(
            grep_arguments(&args, &["--no-textconv"]),
            ["grep", "-e", "first", "--no-textconv", "--end-of-options", "file"].map(OsString::from)
        );
    }

    #[test]
    fn grep_positionals_consumes_values_and_both_option_terminators() {
        for (words, index) in [
            (vec!["grep", "--regexp", "pattern", "--file", "patterns", "path"], 5),
            (vec!["grep", "--max-depth=2", "pattern"], 2),
            (vec!["grep", "--regexp=pattern", "--end-of-options", "path"], 2),
            (vec!["grep", "--max-depth", "2", "-e", "pattern", "--", "path"], 5),
        ] {
            assert_eq!(
                grep_positionals(&words.into_iter().map(OsString::from).collect::<Vec<_>>()),
                index
            );
        }
    }
}
