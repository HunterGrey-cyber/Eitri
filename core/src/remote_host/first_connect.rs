//! First contact and mirror setup, run on a worker with the caller's prompts and progress sink.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::private_fs::PrivateLock;

use super::cli::{Context, Terminal};
use super::install;
use super::window::connecting_text;
use crate::remote_project::{host_key_changed, DirIdentity, RemoteProject, SyncRecord};
use crate::remote_sync::{actor, mirror_identity, status_file, SyncError};

/// Connects to `host`, asking first about a host key ssh does not know: on yes the key is added to the person's
/// known-hosts file and the connection must then show that same key.
fn connect_asking(cx: &mut Context<'_>, host: &str) -> Result<String, String> {
    match cx.reach.connect(host) {
        Ok(key) => Ok(key),
        Err(refusal) if refusal.stderr_line.trim() == "Host key verification failed." => {
            // A changed key is refused again here, before any question.
            let contact = cx.reach.first_contact(host)?;
            let question = format!(
                "{host} is not in your known hosts. Its {} host key has the fingerprint {}. Add it to your known \
                 hosts and connect? [y/N]",
                contact.key_type, contact.sha256
            );
            if !cx.term.ask(&question) {
                return Err(format!(
                    "not connected to {host}: its host key was not accepted; nothing was written"
                ));
            }
            let shown = contact.sha256.clone();
            (contact.accept)()?;
            let key = cx
                .reach
                .connect(host)
                .map_err(|e| format!("connecting to {host}: {e}"))?;
            if key != shown {
                return Err(format!(
                    "{host} showed the host key {key} after {shown} was accepted; nothing was recorded"
                ));
            }
            Ok(key)
        }
        Err(refusal) => Err(format!("connecting to {host}: {refusal}")),
    }
}

fn clear_unused_setup(project: &mut RemoteProject) -> std::io::Result<bool> {
    if let Some(mut record) = project.sync_record().map_err(std::io::Error::other)? {
        if record.set_up && !record.has_sessions() {
            record.set_up = false;
            project.set_sync_record(&record);
            return Ok(true);
        }
    }
    Ok(false)
}

struct RootPreparation {
    _open_lock: PrivateLock,
    replacing: Option<PrivateLock>,
    earlier: Option<RemoteProject>,
}

pub enum FolderRequest {
    Open(PathBuf),
    Complete(String),
}

pub struct FolderCompletion {
    pub prefix: String,
    pub result: Result<Vec<String>, String>,
}

pub struct WindowLaunch {
    pub pid: u32,
    pub ended: mpsc::Receiver<Option<i32>>,
}

/// Drives first contact and first sync on the caller's worker. The front end supplies a folder only
/// if the launch did not name one, and can return a project window to follow through handover.
pub fn run(cx: &mut Context<'_>, host: String, root: Option<PathBuf>) -> Result<(), String> {
    agent::ssh::validate_host(&host)?;
    if let Some(root) = &root {
        validate_root(root)?;
    }
    let locked = root.as_ref().map(|root| lock_root(cx, root)).transpose()?;
    cx.term.say(&connecting_text(&host));
    let key = connect_asking(cx, &host)?;
    ensure_open(cx.term)?;
    let mut helper = None;
    let (root, open_lock, locks) = match (root, locked) {
        (Some(root), Some((open_lock, locks))) => (root, open_lock, locks),
        _ => {
            let root = choose_folder(cx, &host, &key, &mut helper)?;
            validate_root(&root)?;
            let (open_lock, locks) = lock_root(cx, &root)?;
            (root, open_lock, locks)
        }
    };
    let prepared = prepare_root(cx, &host, &key, &root, open_lock, &locks)?;
    let (version, binary) = match helper {
        Some(helper) => helper,
        None => start_helper(cx, &host, &key)?,
    };
    ensure_open(cx.term)?;
    let RootPreparation {
        _open_lock,
        replacing,
        earlier,
    } = prepared;
    let project = RemoteProject {
        version: 1,
        host: host.clone(),
        host_key_sha256: key.clone(),
        root: root.clone(),
        helper_version: version,
        sync: None,
        created_at: earlier.as_ref().map_or(cx.now, |earlier| earlier.created_at),
        queued_stops: Vec::new(),
    };
    let existed = cx.store.find(&root).map_err(|e| e.to_string())?.is_some();
    let project = cx.store.save_opened(&project).map_err(|e| e.to_string())?;
    if let Some(earlier) = earlier.filter(|earlier| earlier.host_key_sha256 != key) {
        let tabs = crate::remote_project::carry_tabs_to_key(cx.store, &root, &earlier.host_key_sha256, &key);
        cx.term.say(&format!(
            "{}{}",
            host_key_changed(&host, &earlier.host_key_sha256, &key),
            crate::remote_project::carried_text(tabs, project.queued_stops.len())
        ));
    }
    cx.term.recorded(&project);
    let opened = project.clone();
    let result = first_sync(cx, project, &binary, replacing);
    if result.is_err() && !existed {
        // A record this run made, which never got a sync value, is taken back: left behind, it would make the
        // person's own folder at that path read as a remote project (git refused, nvim routed to the server).
        let forgotten = cx
            .store
            .forget_if(&root, |latest| latest.same_identity(&opened) && latest.sync.is_none())
            .unwrap_or(false);
        if forgotten {
            cx.term
                .say(&format!("{} is not recorded as a remote project", root.display()));
        }
    }
    result
}

fn choose_folder(
    cx: &mut Context<'_>,
    host: &str,
    key: &str,
    helper: &mut Option<(String, PathBuf)>,
) -> Result<PathBuf, String> {
    let mut completion = None;
    loop {
        match cx.term.folder(completion.take())? {
            FolderRequest::Open(root) => return Ok(root),
            FolderRequest::Complete(prefix) => {
                ensure_open(cx.term)?;
                if helper.is_none() {
                    *helper = Some(start_helper(cx, host, key)?);
                }
                let (_, binary) = helper.as_ref().expect("completion has a helper");
                let launcher = cx.reach.launcher(binary);
                let result = super::folders::complete(launcher.as_ref(), binary, key, &prefix);
                completion = Some(FolderCompletion { prefix, result });
            }
        }
    }
}

fn ensure_open(term: &dyn Terminal) -> Result<(), String> {
    if term.cancelled() {
        Err("first connection cancelled".into())
    } else {
        Ok(())
    }
}

pub(super) fn validate_root(root: &Path) -> Result<(), String> {
    let text = root.to_str().ok_or("a remote project's path must be UTF-8")?;
    if text == "/"
        || text.strip_prefix('/').is_some_and(|members| {
            members
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{text:?}: the project's path must be absolute, with no trailing slash, `.` or `..`"
        ))
    }
}

fn lock_root(cx: &mut Context<'_>, root: &Path) -> Result<(PrivateLock, PathBuf), String> {
    // Before any question or connection: a folder inside another mirror, or holding one, is never synced.
    if let Some(why) = mirror_identity::overlapping(cx.store, root)? {
        return Err(why);
    }
    let state = cx.store.state_home().ok_or("no state home")?;
    let locks = actor::lock_dir(&state, root);
    status_file::prepare_dir(&state, &locks).map_err(|e| e.to_string())?;
    let open_lock =
        agent::private_fs::PrivateLock::open(&locks.join("first-connect.lock")).map_err(|e| e.to_string())?;
    if !open_lock.try_lock().map_err(|e| e.to_string())? {
        return Err(format!("another eitri host open is preparing {}", root.display()));
    }
    // Withdraw unused authorization before any question, including a new host key.
    let mut cleared = false;
    match cx.store.update_checked(root, |project| {
        cleared = clear_unused_setup(project)?;
        Ok(())
    }) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
        _ => {}
    }
    if cleared {
        cx.term.say("Cleared unused sync setup from the previous connection.");
    }
    Ok((open_lock, locks))
}

fn prepare_root(
    cx: &mut Context<'_>,
    host: &str,
    key: &str,
    root: &Path,
    open_lock: PrivateLock,
    locks: &Path,
) -> Result<RootPreparation, String> {
    let earlier = cx.store.find(root).map_err(|e| e.to_string())?;
    let named = earlier
        .as_ref()
        .map(RemoteProject::sync_record)
        .transpose()?
        .flatten()
        .filter(|record| record.has_sessions());
    // A folder that is not the one the sessions synced is not this mirror, whatever its path.
    let own = earlier
        .as_ref()
        .is_some_and(|old| old.host == host && old.host_key_sha256 == key)
        && named
            .as_ref()
            .is_some_and(|record| record.host_key_sha256 == key && mirror(record, root) != Mirror::Other);
    // Protect the old binding before asking to replace it. A directory that fills up while
    // connecting is checked again when its move is proposed.
    let replacing = if !own && (named.is_some() || root.read_dir().is_ok_and(|mut dir| dir.next().is_some())) {
        Some(lock_before_move(locks, root)?)
    } else {
        None
    };
    // The record of this root on the same host follows a changed key, its queued stops and saved tabs with it; a record
    // of this root on another host is that host's, and is replaced only after a yes.
    let earlier = match earlier {
        Some(earlier) if earlier.host != host => {
            let question = format!(
                "{} is recorded as a project on {}. Record it on {host} instead? This machine would forget that record \
                 and its {} queued agent stop(s) there. [y/N]",
                root.display(),
                earlier.host,
                earlier.queued_stops.len()
            );
            if !cx.term.ask(&question) {
                return Err(format!(
                    "nothing was changed: {} stays recorded on {}",
                    root.display(),
                    earlier.host
                ));
            }
            None
        }
        earlier => earlier,
    };
    Ok(RootPreparation {
        _open_lock: open_lock,
        replacing,
        earlier,
    })
}

/// Whether the folder at `root` is the one `record`'s sessions synced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mirror {
    Own(DirIdentity),
    /// The sessions were set up by a build that noted no folder: the person says whether this is still it.
    Unconfirmed,
    Other,
}

fn mirror(record: &SyncRecord, root: &Path) -> Mirror {
    if !record.has_sessions() {
        return Mirror::Other;
    }
    let Some(found) = mirror_identity::identity(root) else {
        return Mirror::Other;
    };
    match record
        .root_identity
        .map(|recorded| mirror_identity::compare(&recorded, &found))
    {
        Some(mirror_identity::Verdict::Same) => Mirror::Own(found),
        Some(mirror_identity::Verdict::Ask) | None => Mirror::Unconfirmed,
        Some(mirror_identity::Verdict::Different) => Mirror::Other,
    }
}

/// Serializes the step from the last overlap check to the record write that sets sync up, across every first
/// connect on this machine: the per-root locks cannot stop two nested roots being set up at once. Held briefly (no
/// question is asked under it), waited for up to 30 seconds.
fn setup_lock(state_home: &Path) -> Result<PrivateLock, String> {
    setup_lock_within(state_home, Duration::from_secs(30))
}

fn setup_lock_within(state_home: &Path, wait: Duration) -> Result<PrivateLock, String> {
    let dir = state_home.join("eitri/remote/sync");
    status_file::prepare_dir(state_home, &dir).map_err(|e| e.to_string())?;
    let lock = PrivateLock::open(&dir.join("setup.lock")).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + wait;
    while !lock.try_lock().map_err(|e| e.to_string())? {
        if Instant::now() >= deadline {
            return Err("another eitri host open kept setting up sync for 30 seconds; try again".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(lock)
}

/// A record whose sessions synced another folder starts again from nothing: its sessions are already terminated, and
/// what it noted about them (names, digests, the copied-back list, a pause) describes that folder. The person's own
/// choices (excludes, includes, allowed large paths) stay.
fn start_afresh(record: &mut SyncRecord) {
    let kept = SyncRecord {
        engine: record.engine.clone(),
        host_key_sha256: record.host_key_sha256.clone(),
        excludes_added: std::mem::take(&mut record.excludes_added),
        excludes_removed: std::mem::take(&mut record.excludes_removed),
        tripwire_allowed: std::mem::take(&mut record.tripwire_allowed),
        ..Default::default()
    };
    *record = kept;
}

fn start_helper(cx: &mut Context<'_>, host: &str, key: &str) -> Result<(String, PathBuf), String> {
    let mut plan = cx.reach.plan()?;
    ensure_open(cx.term)?;
    cx.term.say(plan.probe_summary());
    for warning in plan.warnings() {
        cx.term.say(warning);
    }
    let mut record = cx.store.host(key).map_err(|e| e.to_string())?;
    if !record.as_ref().is_some_and(|record| record.consented) {
        plan.check_upload_space().map_err(|e| e.to_string())?;
        if !cx.term.ask(&install::consent_text(&plan, host)) {
            return Err(format!("nothing was installed on {host} and nothing was recorded"));
        }
        plan.record_consent(cx.store, host, key).map_err(|e| e.to_string())?;
        record = cx.store.host(key).map_err(|e| e.to_string())?;
    }
    let record = record.ok_or_else(|| format!("the installation record for {host} disappeared"))?;
    ensure_open(cx.term)?;
    let binary = plan.install_path().join("bin/eitri-server");
    let launcher = cx.reach.launcher(&binary);
    let term = RefCell::new(&mut *cx.term);
    let band = |line: String| {
        // A terminal gets the upload in tens.
        let tenth = line
            .strip_suffix(" %")
            .and_then(|rest| rest.rsplit_once(' '))
            .and_then(|(_, n)| n.parse::<u64>().ok())
            .is_none_or(|percent| percent % 10 == 0);
        if tenth {
            term.borrow_mut().say(&line);
        }
    };
    super::window::start_server_on_host(launcher.as_ref(), cx.store, host, &record, &mut plan, &band, |_| Ok(()))?;
    Ok((plan.version().to_owned(), binary))
}

fn lock_before_move(locks: &Path, root: &Path) -> Result<agent::private_fs::PrivateLock, String> {
    let lock = agent::private_fs::PrivateLock::open(&locks.join("lock")).map_err(|e| e.to_string())?;
    if !lock.try_lock().map_err(|e| e.to_string())? {
        return Err(format!(
            "close the window or sync watch using {} before moving its mirror",
            root.display()
        ));
    }
    Ok(lock)
}

pub(super) const MUTAGEN_NEEDED: &str = "Remote projects need Mutagen 0.18 or later on this machine. Eitri does not bundle it \
    (its official build contains SSPL-licensed code). Install it from https://mutagen.io (the release archive on Linux, \
    Homebrew on macOS)";

fn first_sync(
    cx: &mut Context<'_>,
    project: RemoteProject,
    binary: &Path,
    mut replacing: Option<agent::private_fs::PrivateLock>,
) -> Result<(), String> {
    use crate::remote_sync::{
        actor::{ActorConfig, HolderKind},
        ignores, local_path, other_sync, recreate, sessions,
        tripwire::MIB,
        SyncKey,
    };

    let root = project.root.as_path();
    let state_home = cx.store.state_home().ok_or("no state home")?;
    if let Some(why) = actor::state_inside(&state_home, root) {
        return Err(why);
    }
    let settings = cx.reach.sync_settings();
    let config = ActorConfig::new(state_home, HolderKind::Watch, settings.tripwire_mib, settings.flush_cap);
    let record = project.sync_record()?.unwrap_or_else(|| SyncRecord {
        engine: "mutagen".into(),
        host_key_sha256: project.host_key_sha256.clone(),
        ..Default::default()
    });
    let mut found = mirror(&record, root);
    let report = super::probe_step::root_probe(
        cx.reach.launcher(binary).as_ref(),
        binary,
        &project.host_key_sha256,
        root,
    )
    .map_err(|e| format!("probing {} on {}: {e}", root.display(), project.host))?;
    use agent::ssh::RootState;
    let problem = match report.root {
        RootState::Ok => None,
        RootState::Missing => Some("the directory is missing or unreadable"),
        RootState::NotADirectory => Some("the path is not a directory"),
        RootState::NotOwned => Some("the server account does not own this directory"),
    };
    if let Some(why) = problem {
        return Err(format!("cannot sync {} on {}: {why}", root.display(), project.host));
    }
    cx.term.say(&format!("Local copy: {}", root.display()));
    if found == Mirror::Unconfirmed {
        let why = if record.root_identity.is_none() {
            "was set up by an earlier Eitri, which did not note which folder it synced"
        } else {
            "may not be the folder Eitri synced: its record cannot prove it (an earlier Eitri noted too little, or the \
             disk now reports another device)"
        };
        let question = format!(
            "{} {why}. Is this folder still that copy (not deleted and made again, replaced, cloned again or restored \
             from a backup since)? Answer n to move it aside and start a fresh copy. [y/N]",
            root.display()
        );
        found = match (cx.term.ask(&question), mirror_identity::identity(root)) {
            (true, Some(identity)) => Mirror::Own(identity),
            _ => {
                if replacing.is_none() {
                    replacing = Some(lock_before_move(&actor::lock_dir(&config.state_home, root), root)?);
                }
                Mirror::Other
            }
        };
    }
    let own = matches!(found, Mirror::Own(_));
    let approved = local_path::inspect_before_move(root, own, cx.now, config.local_git.as_ref(), cx.term, || {
        if replacing.is_none() {
            replacing = Some(lock_before_move(&actor::lock_dir(&config.state_home, root), root)?);
        }
        Ok(())
    })?;
    if let Err(refusal) = other_sync::check(root, &project.host, &report) {
        // One line each: the line to copy stays on a line of its own.
        for line in &refusal.lines {
            cx.term.say(line);
        }
        return Err(refusal.error);
    }
    let engine = match cx.reach.sync_engine() {
        Ok(engine) => engine,
        Err(e @ SyncError::Unsupported(_)) => {
            cx.term.say(&e.to_string());
            return Err(MUTAGEN_NEEDED.to_owned());
        }
        Err(e) => return Err(e.to_string()),
    };
    engine.start().map_err(|e| e.to_string())?;
    let key = SyncKey::new(&project.host_key_sha256, root);
    for name in sessions::terminate_other_keys(engine.as_ref(), &key, root).map_err(|e| e.to_string())? {
        cx.term.say(&format!(
            "stopped sync session {name}: it synced {} with another server",
            root.display()
        ));
    }
    let labels = sessions::root_labels(root);
    // Aliases of the same server share a Mutagen key. A replacement record must start from a new baseline.
    if !own {
        for old in engine.status(&labels).map_err(|e| e.to_string())? {
            engine.terminate(&old.name).map_err(|e| e.to_string())?;
            cx.term.say(&format!("stopped old sync session {}", old.name));
        }
    }
    let helper = cx.reach.sync_helper(&project)?;
    let translated = ignores::Translated::from_response(helper.sync_ignores(root)).map_err(|e| e.to_string())?;
    let patterns = ignores::effective(&translated, &record);
    let response = helper.path_sizes(root, &[], true, &patterns, config.list_budget)?;
    if !response.refusal.is_empty() {
        return Err(response.refusal);
    }
    let [total] = response.sizes.as_slice() else {
        return Err(format!("{} did not return a size for the project root", project.host));
    };
    if !total.rel.is_empty() {
        return Err("the helper measured a different path".into());
    }
    let bound = if response.truncated { "at least " } else { "" };
    let expected = format!("{bound}{}", total.bytes / MIB);
    cx.term.say(&format!("expected size: {expected} MiB"));
    ensure_open(cx.term)?;
    let own_identity = match found {
        Mirror::Own(identity) => Some(identity),
        _ => None,
    };
    // From the last overlap check to the record write, no other first connect sets up a root.
    let setting_up = setup_lock(&config.state_home)?;
    if let Some(why) = mirror_identity::overlapping(cx.store, root)? {
        return Err(why);
    }
    let prepared = local_path::prepare(root, own_identity, approved, cx.term)?;
    if !own {
        recreate::clear_progress(&actor::project_dir(&config.state_home, &project));
    }
    let project = cx
        .store
        .update_checked(root, |latest| {
            if !latest.same_identity(&project) {
                return Err(std::io::Error::other("project identity changed during first connect"));
            }
            let mut setup = latest.sync_record().map_err(std::io::Error::other)?.unwrap_or(record);
            if !own {
                start_afresh(&mut setup);
            }
            setup.root_identity = Some(prepared);
            if setup
                .paused
                .as_ref()
                .is_some_and(|pause| pause.reason == actor::PAUSE_FOLDER)
            {
                setup.paused = None;
            }
            setup.engine = "mutagen".into();
            setup.host_key_sha256.clone_from(&project.host_key_sha256);
            if !setup.has_sessions() {
                setup.tree_synced = false;
                setup.git_ready = false;
            }
            setup.set_up = true;
            latest.set_sync_record(&setup);
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    drop(setting_up);
    drop(replacing);
    if let Err(why) = follow_first_sync(cx.term, &project, engine, helper, config, &expected) {
        let mut cleared = false;
        cx.store
            .update_checked(&project.root, |latest| {
                if latest.same_identity(&project) {
                    cleared = clear_unused_setup(latest)?;
                }
                Ok(())
            })
            .map_err(|e| format!("{why}; could not withdraw unused sync setup: {e}"))?;
        if cleared {
            cx.term
                .say("No sync session was reserved. Cleared sync setup; run eitri host open again.");
        }
        return Err(why);
    }
    Ok(())
}

fn follow_first_sync(
    term: &mut dyn Terminal,
    project: &RemoteProject,
    engine: Arc<dyn crate::remote_sync::engine::SyncEngine>,
    helper: Arc<dyn crate::remote_sync::sessions::SyncHelper>,
    config: actor::ActorConfig,
    expected: &str,
) -> Result<(), String> {
    use crate::remote_sync::{
        actor::SyncEvent,
        engine::{session_name, Role, SyncState},
        quiet::NoEditor,
        sessions,
        tripwire::MIB,
        SyncKey,
    };
    let labels = sessions::root_labels(&project.root);
    let locks = actor::lock_dir(&config.state_home, &project.root);
    let key = SyncKey::new(&project.host_key_sha256, &project.root);
    let not_set_up = project.sync_not_set_up();
    let (events, received) = mpsc::channel();
    let tree_name = session_name(&key, Role::Tree);
    let git_name = session_name(&key, Role::Git);
    let attached = actor::attach(
        project.clone(),
        actor::Deps {
            engine: engine.clone(),
            helper,
            editor: Arc::new(NoEditor),
            events,
        },
        config,
    )
    .map_err(|e| e.to_string())?;
    let enter = term.watch_first_sync();
    let mut last = String::new();
    let mut last_errors = Vec::new();
    let mut tree_ready = false;
    let mut notified = false;
    let mut pct = 0;
    let mut settled_tree = false;
    let start = Instant::now();
    let (mut files, mut bytes) = (0, 0);
    let result = loop {
        if term.cancelled() {
            break Err("first connection cancelled".into());
        }
        match received.recv_timeout(Duration::from_millis(500)) {
            Ok(SyncEvent::Line(line)) => {
                term.say(&line);
                continue;
            }
            Ok(SyncEvent::Status(status)) => {
                pct = status.git_pct;
                settled_tree |= status.phase == "watching";
                tree_ready |= matches!(status.phase.as_str(), "history" | "watching");
                if let Some(pause) = status.paused {
                    break Err(pause.detail);
                }
                if let Some(why) = status.notice {
                    if why == not_set_up && !attached.is_holder() {
                        continue;
                    }
                    if status.phase == "stopped" && !why.contains("when idle") {
                        break Err(why);
                    }
                    term.say(&why);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err("sync actor stopped".to_owned()),
        }
        // Either source can see the completed tree first. An actor report can launch the window without waiting
        // for the next engine poll, and a completed poll can launch it before the actor has published its status.
        let polled = (!notified && !tree_ready).then(|| engine.status(&labels));
        tree_ready |= polled
            .as_ref()
            .and_then(|status| status.as_ref().ok())
            .is_some_and(|statuses| {
                statuses
                    .iter()
                    .any(|status| status.name == tree_name && status.state == SyncState::Watching)
            });
        if tree_ready && !notified {
            notified = true;
            match term.tree_ready(project, pct) {
                Ok(Some(child)) => {
                    break wait_for_window(
                        &child.ended,
                        || !attached.is_holder() && actor::window_holds_lock(&locks, child.pid),
                        || term.cancelled(),
                        Instant::now,
                    );
                }
                Ok(None) => {}
                Err(why) => break Err(why),
            }
        }
        if enter.try_recv().is_ok() {
            break Ok(());
        }
        let statuses = match polled.unwrap_or_else(|| engine.status(&labels)) {
            Ok(v) => v,
            Err(e) => break Err(e.to_string()),
        };
        let tree = statuses.iter().find(|s| s.name == tree_name);
        let git = statuses.iter().find(|s| s.name == git_name);
        let errors: Vec<_> = [tree, git]
            .into_iter()
            .flatten()
            .filter_map(|s| s.last_error.clone())
            .collect();
        for error in &errors {
            if !last_errors.contains(error) {
                term.say(error);
            }
        }
        last_errors = errors;
        if let Some(reason) = statuses.iter().find_map(|s| match &s.state {
            SyncState::Halted(why) => Some(why),
            _ => None,
        }) {
            break Err(reason.clone());
        }
        let line = if tree_ready {
            if git.is_some_and(|s| s.state == SyncState::Watching) && pct == 100 {
                break Ok(());
            }
            if git.is_none() && settled_tree && tree.is_some_and(|s| s.state == SyncState::Watching) {
                break Ok(());
            }
            format!("history arriving: {pct} %")
        } else {
            if let Some(staging) = tree.and_then(|s| s.staging.as_ref()) {
                files = staging.received_files;
                bytes = staging.total_received_bytes;
            }
            let rate = bytes as f64 / MIB as f64 / start.elapsed().as_secs_f64().max(0.001);
            format!(
                "syncing {files} files, {} / {expected} MiB, {rate:.1} MiB/s",
                bytes / MIB
            )
        };
        if line != last {
            term.say(&line);
            last = line;
        }
    };
    drop(attached);
    for event in received.try_iter() {
        if let SyncEvent::Line(line) = event {
            term.say(&line);
        }
    }

    result
}

fn wait_for_window(
    ended: &mpsc::Receiver<Option<i32>>,
    holds_lock: impl Fn() -> bool,
    cancelled: impl Fn() -> bool,
    now: impl Fn() -> Instant,
) -> Result<(), String> {
    let deadline = now() + Duration::from_secs(120);
    loop {
        if cancelled() {
            return Err("first connection cancelled".into());
        }
        if holds_lock() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(now());
        match ended.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(Some(0)) => return Ok(()),
            Ok(code) => return Err(crate::project_chooser::start_failed_line(code)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("could not read the project window's exit status".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) if remaining.is_zero() => {
                return Err("the project window did not take over sync within 120 seconds".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_sync::{
        actor::{tests::Lab, HolderKind},
        engine::{Role, SyncState},
    };
    use std::cell::Cell;

    /// `rm -rf` and a fresh clone at the mirror's path is not the mirror, even when the file system hands the old
    /// inode number back: reopening never takes it for the project's own.
    #[test]
    fn a_folder_deleted_and_made_again_with_its_inode_back_is_not_the_mirror() {
        let scratch = crate::test_scratch_dir::ScratchDir::new("first-connect", "reused");
        let root = scratch.join("project");
        std::fs::create_dir(&root).unwrap();
        let record = SyncRecord {
            tree_session: Some("eitri-0000000000000000-tree".into()),
            root_identity: mirror_identity::identity(&root),
            ..Default::default()
        };
        assert!(matches!(mirror(&record, &root), Mirror::Own(_)));
        let old = mirror_identity::identity(&root).unwrap();
        mirror_identity::test_reuse::after_a_clock_tick();
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        mirror_identity::test_reuse::report(&root, old.dev, old.ino);
        assert!(!matches!(mirror(&record, &root), Mirror::Own(_)));
    }

    /// Setting up sync is serialized across first connects: a second one waits for the first, reports when it waited
    /// too long, and gets the lock once the first lets go (as its process does on exit).
    #[test]
    fn a_second_setup_waits_for_the_first_and_gets_the_lock_once_it_is_let_go() {
        let scratch = crate::test_scratch_dir::ScratchDir::new("first-connect", "setup-lock");
        let state = scratch.to_path_buf();
        let first = setup_lock(&state).unwrap();
        let refused = setup_lock_within(&state, Duration::from_millis(100)).err().unwrap();
        assert!(refused.contains("try again"), "{refused}");
        let waiting = {
            let state = state.clone();
            std::thread::spawn(move || setup_lock_within(&state, Duration::from_secs(20)).map(drop))
        };
        std::thread::sleep(Duration::from_millis(100));
        drop(first);
        assert_eq!(waiting.join().unwrap(), Ok(()));
    }

    #[test]
    fn an_engine_poll_opens_the_window_before_the_actor_reports_the_tree() {
        struct Window {
            opened: bool,
            guard: Instant,
        }

        impl Terminal for Window {
            fn ask(&mut self, question: &str) -> bool {
                panic!("unexpected question: {question}");
            }

            fn say(&mut self, _: &str) {}

            fn watch_first_sync(&mut self) -> mpsc::Receiver<()> {
                mpsc::channel().1
            }

            fn cancelled(&self) -> bool {
                Instant::now() >= self.guard
            }

            fn tree_ready(&mut self, _: &RemoteProject, git_pct: u8) -> Result<Option<WindowLaunch>, String> {
                assert_eq!(git_pct, 0);
                self.opened = true;
                let (send, ended) = mpsc::channel();
                send.send(Some(0)).unwrap();
                Ok(Some(WindowLaunch { pid: 0, ended }))
            }
        }

        let lab = Lab::new();
        let locks = actor::lock_dir(&lab.state, &lab.root);
        status_file::prepare_dir(&lab.state, &locks).unwrap();
        // While the action lock is held the actor cannot start monitoring and publish a ready status.
        let _action = crate::remote_sync::conflicts::try_action_lock(&locks).unwrap().unwrap();
        lab.engine
            .insert(&lab.tree(), Role::Tree, &lab.root, SyncState::Watching);
        let mut window = Window {
            opened: false,
            guard: Instant::now() + Duration::from_secs(10),
        };
        follow_first_sync(
            &mut window,
            &lab.project(),
            lab.engine.clone(),
            lab.helper.clone(),
            lab.config(HolderKind::Watch),
            "1",
        )
        .unwrap();
        assert!(window.opened);
    }

    #[test]
    fn first_connect_follows_a_slow_window_until_handover_exit_cancellation_or_deadline() {
        let start = Instant::now();
        let elapsed = Cell::new(0);
        let now = || {
            let seconds = elapsed.get();
            elapsed.set(seconds + 1);
            start + Duration::from_secs(seconds)
        };
        let (exited, ended) = mpsc::channel();
        assert_eq!(wait_for_window(&ended, || elapsed.get() > 6, || false, now), Ok(()));
        assert!(elapsed.get() > 6);

        exited.send(Some(7)).unwrap();
        assert_eq!(
            wait_for_window(&ended, || false, || false, now),
            Err(crate::project_chooser::start_failed_line(Some(7)))
        );
        assert_eq!(
            wait_for_window(&ended, || false, || true, now),
            Err("first connection cancelled".into())
        );
        let late = Cell::new(false);
        let now = || {
            if late.replace(true) {
                start + Duration::from_secs(120)
            } else {
                start
            }
        };
        assert_eq!(
            wait_for_window(&ended, || false, || false, now),
            Err("the project window did not take over sync within 120 seconds".into())
        );
    }
}
