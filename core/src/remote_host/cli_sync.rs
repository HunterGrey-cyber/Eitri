//! `eitri host sync ...`: what sync holds for a project, and the person's decisions about it. Inspection stays available with remote
//! development off. Following sync requires it to be enabled, and file conflict actions need a live holder.
//!
//! The project is the recorded root holding the path given (or the current directory). Decisions (`exclude`,
//! `include`, `allow`, `resume`) are written into the project's sync record; the holder of the project's sync lock
//! (its window, or a `watch`) reads the record every step and acts on them.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::cli::Context;
use crate::remote_project::{RemoteProject, SyncRecord};
use crate::remote_sync::actor::{self, ActorConfig, HolderKind, SyncEvent, PAUSE_HOST_KEY, PAUSE_TRIPWIRE};
use crate::remote_sync::engine::FlushOutcome;
use crate::remote_sync::quiet::{EditorAnswer, EditorState};
use crate::remote_sync::{conflicts as actions, ignores, recreate, sessions, status_file, terminal_json};

/// `remote_dev.*` for the sync verbs, from `init.lua`.
#[derive(Clone, Debug)]
pub struct SyncSettings {
    pub enabled: bool,
    pub mutagen: Option<PathBuf>,
    pub tripwire_mib: u32,
    pub flush_cap: Duration,
    pub send_waits_for_flush: bool,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mutagen: None,
            tripwire_mib: 64,
            flush_cap: Duration::from_secs(3),
            send_waits_for_flush: true,
        }
    }
}

const USAGE: &str = "usage: eitri host sync status [--json] [<path>] | flush [<path>] | conflicts [--json] [<path>] | \
                     exclude <path> | include <path> | allow [<path>] | resume [<path>] | watch --json [<path>] | \
                     resolve <path> --keep server|laptop | show-server <path> | merge-write <path> --server-sha256 <hex> | dismiss <path>|--all";

pub fn run_sync(args: &[String], cx: &mut Context<'_>) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            cx.term.say(&format!(
                "eitri host sync: the current directory cannot be read: {error}"
            ));
            return 1;
        }
    };
    let stdout = std::io::stdout();
    let input = Box::new(std::io::BufReader::new(std::io::stdin()));
    if args.first().is_some_and(|verb| verb == "watch") {
        let mut output = match WatchOutput::new(stdout.as_fd(), &super::cli::interrupted) {
            Ok(output) => output,
            Err(error) => {
                cx.term.say(&format!("eitri host sync watch: {error}"));
                return 1;
            }
        };
        run_sync_in(args, cx, &cwd, input, &mut output)
    } else {
        run_sync_in(args, cx, &cwd, input, &mut std::io::stdout())
    }
}

/// The watch's output. It bypasses stdout's line buffer, which retries interrupted writes internally, and it sees a
/// cancel while the reader is not taking output. Its open file description is shared (with this process's own stdout,
/// the shell, the other members of a pipeline), so its status flags are left as they are: a terminal is reopened by
/// name for a description of its own, which may be non-blocking. When that fails (a terminal of another devpts
/// instance in a container, or one owned by another user after `su`), and for anything that is not a terminal, each
/// write first waits for `poll` to report room and then writes at most `PIPE_BUF` bytes, which a pipe takes without
/// blocking.
struct WatchOutput<'a> {
    file: std::fs::File,
    /// The flags of a reopened terminal before it was made non-blocking; `None` when writes wait on `poll` first.
    own_flags: Option<libc::c_int>,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a> WatchOutput<'a> {
    fn new(fd: BorrowedFd<'a>, cancelled: &'a dyn Fn() -> bool) -> std::io::Result<Self> {
        let file = std::fs::File::from(fd.try_clone_to_owned()?);
        if file.is_terminal() {
            if let Some((file, own_flags)) = reopen_nonblocking(fd) {
                return Ok(Self {
                    file,
                    own_flags: Some(own_flags),
                    cancelled,
                });
            }
        }
        Ok(Self {
            file,
            own_flags: None,
            cancelled,
        })
    }

    /// Waits up to 100 ms for room to write; `false` when none came (or a signal arrived) so the cancel is looked at.
    fn room(&self) -> std::io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: poll points at one initialized entry. Signals can arrive on another thread.
        match unsafe { libc::poll(&mut poll, 1, 100) } {
            ready if ready > 0 => Ok(true),
            0 => Ok(false),
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }
}

/// The terminal `fd` opened again by name, non-blocking, with its flags from before; `None` when it cannot be.
fn reopen_nonblocking(fd: BorrowedFd<'_>) -> Option<(std::fs::File, libc::c_int)> {
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let mut name = [0u8; libc::PATH_MAX as usize];
    // SAFETY: ttyname_r receives an open terminal and a writable buffer of the stated size.
    if unsafe { libc::ttyname_r(fd.as_raw_fd(), name.as_mut_ptr().cast(), name.len()) } != 0 {
        return None;
    }
    let path = std::ffi::CStr::from_bytes_until_nul(&name).ok()?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(std::ffi::OsStr::from_bytes(path.to_bytes()))
        .ok()?;
    // SAFETY: F_GETFL and F_SETFL act on this private description, which `file` holds open.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return None;
    }
    Some((file, flags))
}

impl Drop for WatchOutput<'_> {
    fn drop(&mut self) {
        if let Some(flags) = self.own_flags {
            // SAFETY: the reopened terminal's own description, still held open by `file`.
            unsafe {
                libc::fcntl(self.file.as_raw_fd(), libc::F_SETFL, flags);
            }
        }
    }
}

impl Write for WatchOutput<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let waits_first = self.own_flags.is_none();
        let chunk = if waits_first {
            &buf[..buf.len().min(libc::PIPE_BUF)]
        } else {
            buf
        };
        while !(self.cancelled)() {
            if waits_first && !self.room()? {
                continue;
            }
            match self.file.write(chunk) {
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if !waits_first {
                self.room()?;
            }
        }
        Err(std::io::ErrorKind::ConnectionAborted.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`run_sync`] from `cwd`, with the watch's input and output given.
pub fn run_sync_in(
    args: &[String],
    cx: &mut Context<'_>,
    cwd: &Path,
    input: Box<dyn BufRead + Send>,
    output: &mut dyn Write,
) -> i32 {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["status"] => status(cx, cwd, None, false, output),
        ["status", "--json"] => status(cx, cwd, None, true, output),
        ["status", "--json", path] => status(cx, cwd, Some(path), true, output),
        ["status", path] => status(cx, cwd, Some(path), false, output),
        ["conflicts"] => conflicts(cx, cwd, None, false, output),
        ["conflicts", "--json"] => conflicts(cx, cwd, None, true, output),
        ["conflicts", "--json", path] => conflicts(cx, cwd, Some(path), true, output),
        ["conflicts", path] => conflicts(cx, cwd, Some(path), false, output),
        ["resolve", path, "--keep", "server"] => file_action(cx, cwd, path, actions::Action::KeepServer, output),
        ["resolve", path, "--keep", "laptop"] => file_action(cx, cwd, path, actions::Action::KeepLaptop, output),
        ["show-server", path] => file_action(
            cx,
            cwd,
            path,
            actions::Action::ShowServer { with_hashes: false },
            output,
        ),
        ["show-server", path, "--with-hashes"] => {
            file_action(cx, cwd, path, actions::Action::ShowServer { with_hashes: true }, output)
        }
        ["merge-write", path, "--server-sha256", sha] => merge_write(cx, cwd, path, sha, None, input, output),
        ["merge-write", path, "--server-sha256", sha, "--laptop-sha256", local] => {
            merge_write(cx, cwd, path, sha, Some(local), input, output)
        }
        ["dismiss", path] => dismiss(cx, cwd, path),
        ["status", "--json", "--details", path] => details(cx, cwd, path, output),
        ["flush"] => flush(cx, cwd, None),
        ["flush", path] => flush(cx, cwd, Some(path)),
        ["exclude", path] => exclude(cx, cwd, path),
        ["include", path] => include(cx, cwd, path),
        ["allow"] => allow(cx, cwd, None),
        ["allow", path] => allow(cx, cwd, Some(path)),
        ["resume"] => resume(cx, cwd, None),
        ["resume", path] => resume(cx, cwd, Some(path)),
        ["watch", "--json"] => watch(cx, cwd, None, input, output),
        ["watch", "--json", path] => watch(cx, cwd, Some(path), input, output),
        _ => {
            cx.term.say(USAGE);
            return 64;
        }
    };
    match result {
        Ok(()) => 0,
        Err(why) => {
            cx.term.say(&format!("eitri host sync: {why}"));
            1
        }
    }
}

pub fn run_git(_args: &[String], cx: &mut Context<'_>) -> i32 {
    cx.term
        .say("eitri host git: git commands go through the shim (git in an Eitri window's editor)");
    64
}

/// `path` made absolute against `cwd`, without `.` and `..` (not resolving links: the record's spelling is matched).
fn absolute(cwd: &Path, path: Option<&str>) -> PathBuf {
    let joined = match path {
        Some(path) => cwd.join(path),
        None => cwd.to_owned(),
    };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    normal
}

/// The recorded project whose root holds `path` (the longest such root), and `path` relative to it.
pub(crate) fn project_for(cx: &Context<'_>, cwd: &Path, path: Option<&str>) -> Result<(RemoteProject, String), String> {
    let wanted = absolute(cwd, path);
    let canonical = wanted.canonicalize().ok();
    let found = locate(cx, &wanted, canonical.as_deref())?;
    Ok((found.project, found.rel))
}

struct Located {
    project: RemoteProject,
    real_root: Option<PathBuf>,
    rel: String,
}

/// General project lookups also accept a resolved argument. A file action matches only the supplied
/// spelling against the two names of the chosen root, leaving all components below it for the nofollow walk.
fn locate(cx: &Context<'_>, wanted: &Path, canonical: Option<&Path>) -> Result<Located, String> {
    let (projects, _) = cx.store.list().map_err(|error| error.to_string())?;
    let mut best = None;
    let mut longest = None;
    for project in projects {
        let real_root = project.root.canonicalize().ok();
        let direct = std::iter::once(project.root.as_path())
            .chain(real_root.as_deref())
            .filter_map(|root| Some((root.components().count(), wanted.strip_prefix(root).ok()?)))
            .max_by_key(|(depth, _)| *depth);
        let matched = direct.or_else(|| {
            let real = real_root.as_deref()?;
            Some((real.components().count(), canonical?.strip_prefix(real).ok()?))
        });
        let Some((depth, rel)) = matched else { continue };
        if longest.is_some_and(|n| n >= depth) {
            continue;
        }
        let rel = rel.to_str().ok_or("path is not UTF-8")?.to_owned();
        longest = Some(depth);
        best = Some(Located {
            project,
            real_root,
            rel,
        });
    }
    best.ok_or_else(|| format!("{} is not inside a remote project", wanted.display()))
}

fn record_of(project: &RemoteProject) -> Result<SyncRecord, String> {
    project.sync_record()?.ok_or_else(|| project.sync_not_set_up())
}

fn update_record(
    cx: &Context<'_>,
    project: &RemoteProject,
    change: impl FnOnce(&mut SyncRecord),
) -> Result<(), String> {
    let mut failed = None;
    cx.store
        .update_for_host(&project.root, &project.host, |project| match record_of(project) {
            Ok(mut record) => {
                change(&mut record);
                project.set_sync_record(&record);
            }
            Err(why) => failed = Some(why),
        })
        .map_err(|error| error.to_string())?;
    failed.map_or(Ok(()), Err)
}

fn state_home(cx: &Context<'_>) -> Result<PathBuf, String> {
    cx.store.state_home().ok_or_else(|| "no state home".to_owned())
}

fn status(
    cx: &mut Context<'_>,
    cwd: &Path,
    path: Option<&str>,
    json: bool,
    output: &mut dyn Write,
) -> Result<(), String> {
    let (project, _) = project_for(cx, cwd, path)?;
    let record = project.sync_record()?.unwrap_or_default();
    let dir = actor::project_dir(&state_home(cx)?, &project);
    let followed = status_file::read_status_file(&dir).ok();
    let mut status = followed
        .clone()
        .unwrap_or_else(|| status_file::compose(&project.host, &record, None, None, None, status_file::now_unix_ms()));
    // The record is the truth for what waits for the person, even when the holder has not written it yet.
    status.paused = record.paused.clone();
    if followed.is_none() && status.notice.is_none() {
        status.notice = Some(format!(
            "no Eitri window or eitri host sync watch follows {}; its sync is paused until one does",
            project.root.display()
        ));
    }
    if json {
        let line = serde_json::to_string(&status).map_err(|error| error.to_string())?;
        return writeln!(output, "{}", terminal_json(&line)).map_err(|e| e.to_string());
    }
    cx.term.say(&format!(
        "sync with {} for {}: {}",
        project.host,
        project.root.display(),
        status.phase
    ));
    if let Some(pause) = &status.paused {
        cx.term.say(&format!("paused: {}", pause.detail));
    }
    if let Some(notice) = &status.notice {
        cx.term.say(notice);
    }
    if !status.git_ready && record.git_session.is_some() {
        cx.term.say(&format!("history arriving: {} %", status.git_pct));
    }
    for conflict in &status.conflicts {
        cx.term.say(&format!(
            "conflict: {} (this laptop: {}, {}: {})",
            conflict.path, conflict.laptop_kind, project.host, conflict.server_kind
        ));
    }
    if status.conflicts_more > 0 {
        cx.term.say(&format!(
            "and {} more conflicts Mutagen did not list",
            status.conflicts_more
        ));
    }
    for problem in &status.problems {
        cx.term.say(&format!(
            "not synced: {} ({}): {}. To proceed, {}",
            problem.path,
            problem.side,
            problem.text,
            crate::remote_sync::problem_actions_for(&problem.side, &project.host)
        ));
    }
    if status.problems_more > 0 {
        cx.term.say(&format!(
            "and {} more paths Mutagen did not list that do not sync. To proceed, {}",
            status.problems_more,
            crate::remote_sync::problem_actions(None, &project.host)
        ));
    }
    for path in &status.copied_back {
        cx.term.say(&format!(
            "copied to the other side when sync was re-created; if you had deleted it, delete it again: {path}"
        ));
    }
    if status.copied_back_more > 0 {
        cx.term
            .say(&format!("and {} more copied back", status.copied_back_more));
    }
    for path in &status.kept_here {
        cx.term
            .say(&format!("no longer synced; this laptop keeps its copy: {path}"));
    }
    for path in &record.excludes_added {
        cx.term.say(&format!("excluded here: {path}"));
    }
    for path in &record.excludes_removed {
        cx.term.say(&format!("included here: {path}"));
    }
    Ok(())
}

fn conflicts(
    cx: &mut Context<'_>,
    cwd: &Path,
    path: Option<&str>,
    json: bool,
    output: &mut dyn Write,
) -> Result<(), String> {
    let (project, _) = project_for(cx, cwd, path)?;
    let status = status_file::read_status_file(&actor::project_dir(&state_home(cx)?, &project))?;
    if json {
        let line = serde_json::to_string(&status.conflicts).map_err(|error| error.to_string())?;
        return writeln!(output, "{}", terminal_json(&line)).map_err(|e| e.to_string());
    }
    if status.conflicts.is_empty() {
        cx.term.say(&format!("no conflicts with {}", project.host));
    }
    for conflict in &status.conflicts {
        cx.term.say(&format!(
            "{} (this laptop: {}, {}: {})",
            conflict.path, conflict.laptop_kind, project.host, conflict.server_kind
        ));
    }
    Ok(())
}

fn action_project(cx: &Context<'_>, cwd: &Path, path: &str) -> Result<Located, String> {
    actions::check_rel(path.strip_prefix('/').unwrap_or(path))?;
    let found = locate(cx, &absolute(cwd, Some(path)), None)?;
    actions::check_rel(&found.rel)?;
    Ok(found)
}

fn ready(
    store: &crate::remote_project::RemoteProjects,
    project: &RemoteProject,
    helper: &dyn sessions::SyncHelper,
    engine: &dyn crate::remote_sync::engine::SyncEngine,
    dir: &Path,
    conflict: Option<&crate::remote_sync::status_schema::ConflictEntry>,
) -> Result<actions::ConflictState, String> {
    let now = store
        .find(&project.root)
        .map_err(|e| e.to_string())?
        .ok_or("the remote project no longer exists")?;
    if !now.same_identity(project) {
        return Err("the project changed; look again".into());
    }
    let record = record_of(&now)?;
    let status = status_file::read_status_file(dir)?;
    if let Some(pause) = record.paused.as_ref().or(status.paused.as_ref()) {
        return Err(format!("sync is paused: {}", pause.detail));
    }
    if status.phase == "recreating"
        || record.accepted_host_key.is_some()
        || record.host_key_sha256 != project.host_key_sha256
    {
        return Err("sync re-creation is pending; look again when it finishes".into());
    }
    if !matches!(status.phase.as_str(), "watching" | "history" | "syncing") {
        return Err(format!("sync is {}", status.phase));
    }
    let locks = actor::lock_dir(&store.state_home().ok_or("no state home")?, &project.root);
    let holder = agent::private_fs::PrivateLock::open_existing(&locks.join("lock"))
        .map_err(|e| format!("no sync holder: {e}"))?;
    if holder.try_lock_shared().map_err(|e| e.to_string())? {
        return Err("sync is paused until a window or watch follows the project".into());
    }
    let name = record.tree_session.as_deref().ok_or("no tree session")?;
    let tree = sessions::find(engine, &project.root, name)
        .map_err(|e| e.to_string())?
        .ok_or("sync session was lost; re-creation is pending")?;
    use crate::remote_sync::engine::SyncState;
    if !matches!(
        tree.state,
        SyncState::Watching | SyncState::Scanning | SyncState::Staging | SyncState::Applying
    ) {
        return Err(format!("sync is unavailable ({:?})", tree.state));
    }
    let mut state = actions::ConflictState::Listed;
    if let Some(expected) = conflict {
        let live = status_file::compose(&project.host, &record, Some(&tree), None, None, 0);
        if tree.successful_cycles > 0
            && live.conflicts_more == 0
            && !live.conflicts.iter().any(|c| c.path == expected.path)
        {
            state = actions::ConflictState::Cleared;
        } else if tree.successful_cycles == 0
            || !live.conflicts.contains(expected)
            || !status.conflicts.contains(expected)
        {
            return Err(format!(
                "{} is no longer in the same conflict; look again",
                expected.path
            ));
        }
    }
    let translated =
        ignores::Translated::from_response(helper.sync_ignores(&project.root)).map_err(|e| e.to_string())?;
    let effective = ignores::effective(&translated, &record);
    if ignores::Translated::new(effective, vec![]).digest != record.ignore_digest {
        return Err("ignore rules changed; sync re-creation is pending".into());
    }
    actions::idle(helper, &project.root)?;
    Ok(state)
}

fn file_action(
    cx: &mut Context<'_>,
    cwd: &Path,
    path: &str,
    action: actions::Action<'_>,
    output: &mut dyn Write,
) -> Result<(), String> {
    let Located {
        project,
        real_root,
        rel,
    } = action_project(cx, cwd, path)?;
    let state = state_home(cx)?;
    let dir = actor::project_dir(&state, &project);
    let helper = cx.reach.sync_helper(&project)?;
    let engine = cx.reach.sync_engine().map_err(|e| e.to_string())?;
    let status = status_file::read_status_file(&dir)?;
    let conflict = status
        .conflicts
        .iter()
        .find(|c| c.path == rel)
        .ok_or_else(|| format!("{rel} is not in conflict"))?;
    let bytes = actions::apply(
        &state,
        &project,
        conflict,
        helper.as_ref(),
        action,
        real_root.as_deref().ok_or("cannot resolve the project root")?,
        || {
            ready(
                cx.store,
                &project,
                helper.as_ref(),
                engine.as_ref(),
                &dir,
                Some(conflict),
            )
        },
    )?;
    output.write_all(&bytes).map_err(|e| e.to_string())
}

fn merge_write(
    cx: &mut Context<'_>,
    cwd: &Path,
    path: &str,
    sha: &str,
    laptop_sha: Option<&str>,
    input: Box<dyn BufRead + Send>,
    output: &mut dyn Write,
) -> Result<(), String> {
    if !sha.is_empty() && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) {
        return Err("expected a lowercase SHA-256 digest".into());
    }
    let mut bytes = Vec::new();
    input
        .take(actions::MERGE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > actions::MERGE_LIMIT {
        return Err("a merge is limited to 8 MiB".into());
    }
    file_action(
        cx,
        cwd,
        path,
        actions::Action::Merge {
            expected: sha,
            laptop_sha,
            bytes: &bytes,
        },
        output,
    )
}

fn dismiss(cx: &mut Context<'_>, cwd: &Path, path: &str) -> Result<(), String> {
    let all = path == "--all";
    let (project, rel) = if all {
        project_for(cx, cwd, None)?
    } else {
        let found = action_project(cx, cwd, path)?;
        (found.project, found.rel)
    };
    let dir = actor::project_dir(&state_home(cx)?, &project);
    let helper = cx.reach.sync_helper(&project)?;
    let engine = cx.reach.sync_engine().map_err(|e| e.to_string())?;
    let _lock = actions::action_lock(&actor::lock_dir(&state_home(cx)?, &project.root))?;
    ready(cx.store, &project, helper.as_ref(), engine.as_ref(), &dir, None)?;
    if !all && !record_of(&project)?.copied_back.contains(&rel) {
        return Err(format!("{rel} is not listed as copied back"));
    }
    update_record(cx, &project, |record| {
        if all {
            record.copied_back.clear();
            record.copied_back_more = 0;
        } else {
            record.copied_back.retain(|p| p != &rel);
        }
    })
}

/// An on-demand listing for the sync view. Opening an editor does not walk the whole server tree.
fn details(cx: &mut Context<'_>, cwd: &Path, path: &str, output: &mut dyn Write) -> Result<(), String> {
    let Located { project, real_root, .. } = locate(cx, &absolute(cwd, Some(path)), None)?;
    let dir = actor::project_dir(&state_home(cx)?, &project);
    let status = status_file::read_status_file(&dir)?;
    let record = record_of(&project)?;
    let patterns = recreate::read_created_patterns(&dir);
    let mut data = serde_json::json!({
        "excludes": patterns.as_deref().unwrap_or_default(),
        "excludes_added": record.excludes_added,
        "includes": record.excludes_removed,
        "allowed": record.tripwire_allowed,
    });
    let listing = (|| {
        let patterns = patterns.as_deref().ok_or("the effective ignore list is unavailable")?;
        let helper = cx.reach.sync_helper(&project)?;
        let listing = helper.list_tree(&project.root, patterns, recreate::LIST_BUDGET)?;
        let mut entries = std::collections::BTreeMap::new();
        let mut problems = Vec::new();
        for entry in &listing {
            if !entry.refusal.is_empty() {
                return Err(entry.refusal.clone());
            }
            if entry.truncated {
                return Err(recreate::TOO_MANY_FILES.into());
            }
            if matches!(entry.kind.as_str(), "other" | "not-utf8") {
                problems.push(serde_json::json!({"path": entry.rel, "side":"server", "text":entry.kind}));
            }
            entries.insert(&entry.rel, entry);
        }
        let real_root = real_root.as_deref().ok_or("cannot resolve the project root")?;
        let mut choices = serde_json::Map::new();
        for c in &status.conflicts {
            choices.insert(
                c.path.clone(),
                actions::inspect(&project, real_root, c, entries.get(&c.path).copied()),
            );
        }
        data["choices"] = choices.into();
        data["problems"] = problems.into();
        Ok(())
    })();
    if let Err(why) = listing {
        data["notice"] = format!("could not list the server tree: {why}").into();
    }
    let document = serde_json::to_string(&data).map_err(|e| e.to_string())?;
    output
        .write_all(terminal_json(&document).as_bytes())
        .map_err(|e| e.to_string())
}

fn flush(cx: &mut Context<'_>, cwd: &Path, path: Option<&str>) -> Result<(), String> {
    let (project, _) = project_for(cx, cwd, path)?;
    let cap = cx.reach.sync_settings().flush_cap;
    flush_project(cx, &project, cap)?;
    // A completed cycle; a conflicted or unsyncable file is still only on this laptop.
    cx.term.say(&format!(
        "sync with {} finished a cycle; eitri host sync status lists anything it could not carry",
        project.host
    ));
    Ok(())
}

/// Flushes `project`'s tree session within `cap` and returns the session's name once a cycle completed.
pub(crate) fn flush_project(cx: &mut Context<'_>, project: &RemoteProject, cap: Duration) -> Result<String, String> {
    record_of(project)?;
    let engine = cx.reach.sync_engine().map_err(|e| e.to_string())?;
    flush_tree(engine.as_ref(), project, cap)
}

/// [`flush_project`] through an engine already opened: the session's name is read from the record each time, since
/// the session is re-created under another name when the project's excludes change.
pub(crate) fn flush_tree(
    engine: &dyn crate::remote_sync::engine::SyncEngine,
    project: &RemoteProject,
    cap: Duration,
) -> Result<String, String> {
    let record = record_of(project)?;
    let tree = record
        .tree_session
        .ok_or_else(|| format!("{} has no sync session", project.root.display()))?;
    match engine.flush(&tree, cap) {
        FlushOutcome::Flushed => Ok(tree),
        FlushOutcome::TimedOut => Err(format!(
            "{} did not confirm within {} s",
            project.host,
            cap.as_secs_f64()
        )),
        FlushOutcome::Failed(why) => Err(format!("sync with {} did not flush: {why}", project.host)),
    }
}

/// A relative path inside the project the record can hold: no `..`, no `.git`, not the root.
fn checked_rel(rel: &str) -> Result<String, String> {
    let rel = rel.trim_end_matches('/');
    if rel.is_empty() {
        return Err("name a path inside the project, not its root".into());
    }
    if crate::agent_bridge::path_holds_control(rel) {
        return Err("a path to exclude or include cannot contain control characters".into());
    }
    if rel
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git"))
    {
        return Err(format!("{rel} is not a path sync can exclude or include"));
    }
    Ok(rel.to_owned())
}

fn exclude(cx: &mut Context<'_>, cwd: &Path, path: &str) -> Result<(), String> {
    let (project, rel) = project_for(cx, cwd, Some(path))?;
    record_of(&project)?;
    let rel = checked_rel(&rel)?;
    update_record(cx, &project, |record| {
        record.excludes_removed.retain(|p| p != &rel);
        if !record.excludes_added.contains(&rel) {
            record.excludes_added.push(rel.clone());
        }
    })?;
    cx.term.say(&format!(
        "{rel} stays on {} and leaves sync; the sync session is re-created when the project is quiet (this laptop \
         keeps its copy)",
        project.host
    ));
    Ok(())
}

fn include(cx: &mut Context<'_>, cwd: &Path, path: &str) -> Result<(), String> {
    let (project, rel) = project_for(cx, cwd, Some(path))?;
    let record = record_of(&project)?;
    let rel = checked_rel(&rel)?;
    if !record.excludes_added.contains(&rel) {
        let dir = actor::project_dir(&state_home(cx)?, &project);
        if let Some(patterns) = recreate::read_created_patterns(&dir) {
            ignores::include(&patterns, &rel)?;
        }
    }
    update_record(cx, &project, |record| {
        if record.excludes_added.contains(&rel) {
            record.excludes_added.retain(|p| p != &rel);
        } else if !record.excludes_removed.contains(&rel) {
            record.excludes_removed.push(rel.clone());
        }
    })?;
    cx.term.say(&format!(
        "{rel} joins sync with {}; the sync session is re-created when the project is quiet",
        project.host
    ));
    Ok(())
}

fn allow(cx: &mut Context<'_>, cwd: &Path, path: Option<&str>) -> Result<(), String> {
    let (project, rel) = project_for(cx, cwd, path)?;
    let record = record_of(&project)?;
    let tripped = record.paused.as_ref().filter(|pause| pause.reason == PAUSE_TRIPWIRE);
    let named = match path {
        Some(_) => Some(checked_rel(&rel)?),
        None => match tripped {
            Some(pause) => pause.path.clone(),
            None => return Err("sync is not paused for a large change; name the path to let through".into()),
        },
    };
    update_record(cx, &project, |record| {
        match &named {
            Some(rel) if !record.tripwire_allowed.contains(rel) => record.tripwire_allowed.push(rel.clone()),
            Some(_) => {}
            None => record.tripwire_allow_batch = true,
        }
        if record
            .paused
            .as_ref()
            .is_some_and(|pause| pause.reason == PAUSE_TRIPWIRE)
        {
            record.paused = None;
        }
    })?;
    match named {
        Some(rel) => cx
            .term
            .say(&format!("{rel} syncs from {} whatever its size", project.host)),
        None => cx.term.say(&format!(
            "this change from {} syncs; later large ones pause again",
            project.host
        )),
    }
    Ok(())
}

fn resume(cx: &mut Context<'_>, cwd: &Path, path: Option<&str>) -> Result<(), String> {
    let (project, _) = project_for(cx, cwd, path)?;
    let record = record_of(&project)?;
    let Some(pause) = record.paused.clone() else {
        cx.term.say(&format!("sync with {} is not paused", project.host));
        return Ok(());
    };
    cx.term.say(&pause.detail);
    if pause.reason == crate::remote_sync::actor::PAUSE_FOLDER {
        // Resuming would sync the sessions over a folder they never synced; only first connect sets it up again.
        return Err(format!(
            "nothing was resumed: run eitri host open {}:{} to set up the folder there",
            project.host,
            project.root.display()
        ));
    }
    // A key pause under a record that already has the project's key was written by a window whose connection saw
    // another key: this machine has not settled that key, so the question names none (that window pauses again
    // while it shows the other key; one opened again settles it).
    let question = if pause.reason == PAUSE_HOST_KEY && record.host_key_sha256 != project.host_key_sha256 {
        format!(
            "Sync {} with the server {} now shows (host key {})? Its sessions are re-created, and files changed on \
             one side only while paused are copied to the other. [y/N]",
            project.root.display(),
            project.host,
            project.host_key_sha256
        )
    } else if pause.reason == crate::remote_sync::actor::PAUSE_RECREATE {
        format!(
            "Try re-creating sync of {} with {} again? Both trees are listed first; if they are still too large, \
             leave big directories on the server with eitri host sync exclude <path> first. [y/N]",
            project.root.display(),
            project.host
        )
    } else {
        format!("Resume sync of {} with {}? [y/N]", project.root.display(), project.host)
    };
    if !cx.term.ask(&question) {
        cx.term.say("nothing changed");
        return Ok(());
    }
    let key = project.host_key_sha256.clone();
    let mut changed = false;
    update_record(cx, &project, |record| {
        // Only the pause the person was shown: another written meanwhile is asked about again.
        if record.paused.as_ref() != Some(&pause) {
            changed = true;
            return;
        }
        if pause.reason == PAUSE_HOST_KEY && record.host_key_sha256 != key {
            record.accepted_host_key = Some(key.clone());
        }
        record.paused = None;
    })?;
    if changed {
        return Err(
            "the pause changed while you were asked; nothing was resumed: run eitri host sync resume again".into(),
        );
    }
    cx.term
        .say("sync resumes as soon as a window or eitri host sync watch follows this project");
    Ok(())
}

/// The watch's own editor: the reports `eitri.nvim` writes on its stdin, one JSON object per line
/// (`{"unsaved": [...], "files": [[<path>, <mtime ms>], ...]}`, or `{"unsaved": null}` when it cannot tell).
#[derive(Clone, Default)]
struct StdinEditor(Arc<Mutex<Option<(std::time::Instant, Option<EditorAnswer>)>>>);

impl StdinEditor {
    fn answer(&self) -> Option<EditorAnswer> {
        let report = self.0.lock().unwrap();
        let Some((at, answer)) = report.as_ref() else {
            return Some(EditorAnswer::default());
        };
        (at.elapsed() <= crate::remote_sync::quiet::REPORT_STALE_AFTER)
            .then(|| answer.clone())
            .flatten()
    }
}

impl EditorState for StdinEditor {
    fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>> {
        Some(
            self.answer()?
                .unsaved
                .into_iter()
                .filter(|path| path.starts_with(root))
                .collect(),
        )
    }
    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
        self.answer()
            .map(|a| (a.generation().max(generation), a.saved_after(generation)))
            .unwrap_or((generation, vec![]))
    }
}

/// One report line from `eitri.nvim`; `None` when the line is not one.
fn parse_report(line: &str) -> Option<Option<EditorAnswer>> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("unsaved").is_some_and(serde_json::Value::is_null) {
        return Some(None);
    }
    value.get("unsaved")?.as_array()?;
    serde_json::from_value::<EditorAnswer>(value).ok().map(Some)
}

/// One JSON line of the watch's output, with server-chosen names escaped as [`terminal_json`] escapes them.
pub fn event_json(event: &SyncEvent) -> String {
    terminal_json(&match event {
        SyncEvent::Status(status) => serde_json::json!({"type": "status", "status": status}).to_string(),
        SyncEvent::Line(text) => serde_json::json!({"type": "line", "text": text}).to_string(),
    })
}

/// `eitri host sync watch --json`: follows the project's sync until its input ends, holding the lock when no window
/// does, printing every status change and line as JSON, and taking `eitri.nvim`'s reports on its input.
fn watch(
    cx: &mut Context<'_>,
    cwd: &Path,
    path: Option<&str>,
    input: Box<dyn BufRead + Send>,
    output: &mut dyn Write,
) -> Result<(), String> {
    // Following a project starts or resumes its sessions, so the switch must be on (the other verbs only read or
    // settle what sync holds; file actions require a live holder).
    if !(cx.remote_dev_enabled)() {
        return Err(crate::remote_project::remote_dev_off("eitri host sync watch"));
    }
    let (project, _) = project_for(cx, cwd, path)?;
    let settings = cx.reach.sync_settings();
    let engine = cx.reach.sync_engine().map_err(|e| e.to_string())?;
    let helper = cx.reach.sync_helper(&project)?;
    let config = ActorConfig::new(
        state_home(cx)?,
        HolderKind::Watch,
        settings.tripwire_mib,
        settings.flush_cap,
    );
    // The watch owns this directory as long as its editor is attached. Its child settings come from
    // the local installation and init.lua, never from status.json or helper output.
    use crate::remote_sync::shim_dir::{real_git_from_process, ShimDir, ShimOptions};
    let shim = ShimDir::prepare(
        std::env::current_exe().map_err(|e| e.to_string())?,
        real_git_from_process()?,
        ShimOptions {
            enabled: settings.enabled,
            mutagen: settings.mutagen,
            flush_cap: settings.flush_cap.as_secs_f64(),
        },
    )
    .map_err(|e| e.to_string())?;
    let mut env = vec![];
    shim.compose_child_env(
        &crate::remote_project::WindowOrigin::Remote(project.clone()),
        &mut env,
        None,
    )?;
    follow_with_env(project, engine, helper, config, input, output, Some(&env), &|| {
        cx.term.cancelled()
    })
}

/// The watch's loop over an attachment it makes: until its input ends.
#[cfg(test)]
pub(crate) fn follow(
    project: RemoteProject,
    engine: Arc<dyn crate::remote_sync::engine::SyncEngine>,
    helper: Arc<dyn crate::remote_sync::sessions::SyncHelper>,
    config: ActorConfig,
    input: Box<dyn BufRead + Send>,
    output: &mut dyn Write,
) -> Result<(), String> {
    follow_with_env(project, engine, helper, config, input, output, None, &|| false)
}

fn follow_with_env(
    project: RemoteProject,
    engine: Arc<dyn crate::remote_sync::engine::SyncEngine>,
    helper: Arc<dyn crate::remote_sync::sessions::SyncHelper>,
    config: ActorConfig,
    input: Box<dyn BufRead + Send>,
    output: &mut dyn Write,
    shim: Option<&[(String, String)]>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let editor = StdinEditor::default();
    let (events, received) = mpsc::channel();
    let attached = actor::attach(
        project,
        actor::Deps {
            engine,
            helper,
            editor: Arc::new(editor.clone()),
            events,
        },
        config,
    )
    .map_err(|error| error.to_string())?;
    let result = watch_loop(&editor, &received, input, output, shim, cancelled);
    drop(attached);
    // What the actor said while letting go.
    for event in received.try_iter() {
        if cancelled() || writeln!(output, "{}", event_json(&event)).is_err() {
            break;
        }
    }
    result
}

fn watch_loop(
    editor: &StdinEditor,
    events: &mpsc::Receiver<SyncEvent>,
    mut input: Box<dyn BufRead + Send>,
    output: &mut dyn Write,
    shim: Option<&[(String, String)]>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    // The input is read on a thread of its own, left behind when the output closes: a reader blocked on an idle
    // input must never keep this watch (and its hold on the project) alive.
    let (lines_in, lines) = mpsc::channel::<Option<String>>();
    let _ = std::thread::Builder::new()
        .name("eitri-sync-watch-input".into())
        .spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match input.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = lines_in.send(None);
                        return;
                    }
                    Ok(_) => {
                        if lines_in.send(Some(line.trim().to_owned())).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    while !cancelled() {
        while let Ok(event) = events.try_recv() {
            let line = match (&event, shim) {
                (SyncEvent::Status(status), Some(env)) => {
                    terminal_json(&serde_json::json!({"type": "status", "status": status, "shim": env}).to_string())
                }
                _ => event_json(&event),
            };
            if writeln!(output, "{line}").and_then(|()| output.flush()).is_err() {
                return Ok(());
            }
        }
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(line)) => {
                if let Some(report) = parse_report(&line) {
                    *editor.0.lock().unwrap() = Some((std::time::Instant::now(), report));
                }
            }
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}

/// `eitri host stop <host>`: every sync session of the host's projects pauses.
pub fn pause_host_sessions(cx: &mut Context<'_>, host_key: &str, command: &str, host: &str) {
    if !has_sessions(cx, host_key) {
        return;
    }
    match cx.reach.sync_engine() {
        Ok(engine) => {
            for line in crate::remote_sync::sessions::pause_host(engine.as_ref(), cx.store, host_key, command, host) {
                cx.term.say(&line);
            }
        }
        Err(why) => cx.term.say(&format!("sync with {host} was not paused: {why}")),
    }
}

/// `eitri host uninstall <host>`: every sync session of the projects under the server's verified key ends; the mirrors
/// stay on disk.
pub fn terminate_host_sessions(cx: &mut Context<'_>, host: &str, host_key: &str) {
    if !has_sessions(cx, host_key) {
        return;
    }
    match cx.reach.sync_engine() {
        Ok(engine) => {
            for line in crate::remote_sync::sessions::terminate_host(engine.as_ref(), cx.store, host_key) {
                cx.term.say(&line);
            }
        }
        Err(why) => cx.term.say(&format!("sync with {host} was not stopped: {why}")),
    }
}

fn has_sessions(cx: &Context<'_>, host_key: &str) -> bool {
    cx.store.list().is_ok_and(|(projects, _)| {
        projects.iter().any(|project| {
            project.host_key_sha256 == host_key
                && project
                    .sync_record()
                    .ok()
                    .flatten()
                    .is_some_and(|record| record.tree_session.is_some() || record.git_session.is_some())
        })
    })
}

#[cfg(test)]
#[path = "cli_sync_tests.rs"]
mod tests;
