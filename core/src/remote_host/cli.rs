//! `eitri host`: the terminal side of remote projects. `open <host>:<path>` connects (asking about a host key ssh
//! does not know), probes the server, asks before installing Eitri's server helper, installs and starts it and
//! records the project and follows its first sync after checking the laptop copy. `status` reads the records, `list`
//! shows the agents a host runs, `stop` stops the helper daemons there with their agents, `uninstall` removes the
//! helper and this machine's records of the host. `known-hosts-probe` is ssh's, run as its `KnownHostsCommand`, and
//! never listed. Everything here blocks: it runs in the `eitri host` process, before and without any toolkit.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::ssh::{ChannelLauncher, ConnectError, ControlMaster, DaemonInfo, SshLauncher};

use super::install::{self, InstallPlan};
use crate::remote_project::terminals::{service_call, terminal_lines, TerminalState};
use crate::remote_project::{host_key_changed, remote_dev_off, RemoteProject, RemoteProjects};
use crate::remote_sync::SyncError;
#[cfg(test)]
use crate::remote_sync::{actor, status_file};
use agent::remote_client::terminal_transport::wire;

/// The hidden subcommand ssh runs to record the host key it is checking.
pub const KNOWN_HOSTS_PROBE: &str = "known-hosts-probe";

/// What `eitri host` with no or a wrong command says.
pub const USAGE: &str = "usage: eitri host open <host>:<path> | status [<host>] | list [<host>] | stop <host> | \
                         uninstall <host>";

/// ssh's `KnownHostsCommand`: records the key it checks and prints nothing, so ssh's own known-hosts files still
/// decide. Its exit code.
pub fn known_hosts_probe(args: &[OsString]) -> i32 {
    agent::ssh::known_hosts::probe_main(args)
}

/// Where the ssh masters' sockets and the records of the keys they verified live: `<state home>/eitri/ssh`.
pub fn ssh_dir(xdg_state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    crate::state_paths::state_subdir(xdg_state_home, home, "ssh")
}

static CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn interrupted() -> bool {
    CANCELLED.load(std::sync::atomic::Ordering::Relaxed)
}

extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn listen_for_interrupts() -> std::io::Result<()> {
    // SAFETY: the handler uses only a lock-free atomic. This CLI process exits when the command finishes.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_flags = libc::SA_RESETHAND;
        action.sa_sigaction = cancel as *const () as _;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// Reads the CLI's answer without making cancellation depend on another keystroke.
pub fn read_answer(answer: &mut String) -> std::io::Result<()> {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("eitri-host-answer".into())
        .spawn(move || {
            let mut line = String::new();
            let result = std::io::stdin().read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        })?;
    while !CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
        match receive.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                *answer = line?;
                return Ok(());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err(std::io::Error::other("terminal input closed")),
        }
    }
    Err(std::io::ErrorKind::Interrupted.into())
}

/// The person at the terminal.
pub trait Terminal {
    fn say(&mut self, line: &str);
    /// Finishes when a line arrives on stdin. EOF leaves the watch running until sync completes.
    fn input_line(&mut self) -> std::sync::mpsc::Receiver<()> {
        let (send, recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if std::io::stdin().read_line(&mut String::new()).is_ok_and(|n| n > 0) {
                let _ = send.send(());
            }
        });
        recv
    }
    /// A root supplied by the caller skips the folder question.
    fn folder(
        &mut self,
        _completion: Option<super::first_connect::FolderCompletion>,
    ) -> Result<super::first_connect::FolderRequest, String> {
        Err("no server folder was selected".into())
    }

    fn watch_first_sync(&mut self) -> std::sync::mpsc::Receiver<()> {
        self.say("Press Enter to leave sync to the window.");
        self.input_line()
    }

    /// A window front end launches the project and returns the child to follow through sync handover.
    fn tree_ready(
        &mut self,
        _project: &RemoteProject,
        _git_pct: u8,
    ) -> Result<Option<super::first_connect::WindowLaunch>, String> {
        Ok(None)
    }

    fn recorded(&mut self, project: &RemoteProject) {
        self.say(&format!(
            "recorded {}:{}; open it with eitri --ssh {} {}",
            project.host,
            project.root.display(),
            project.host,
            project.root.display()
        ));
    }

    fn cancelled(&self) -> bool {
        interrupted()
    }

    /// A yes-or-no question; only an explicit yes is one.
    fn ask(&mut self, question: &str) -> bool;
}

/// A host key ssh does not know yet: what to show, and how to add it once the person said yes.
pub struct Contact {
    pub key_type: String,
    pub sha256: String,
    pub accept: Box<dyn FnOnce() -> Result<(), String>>,
}

/// An agent a host's helper runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub sidecar_id: String,
    pub root: String,
    pub account: String,
    pub alive: bool,
    pub sessions: Vec<AgentSession>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSession {
    pub session_id: String,
    pub closed: bool,
    pub pending: u64,
}

/// How `eitri host` reaches a host: [`SshReach`] in use.
pub trait Reach {
    /// Connects to `host` (or finds the connection up) and returns the SHA-256 of the host key ssh verified.
    fn connect(&mut self, host: &str) -> Result<String, ConnectError>;
    /// After [`Reach::connect`] refused a key ssh does not know: the key the host shows, for the question.
    fn first_contact(&mut self, host: &str) -> Result<Contact, String>;
    /// Steps and helper channels through the connected host, the helper at `helper`.
    fn launcher(&self, helper: &Path) -> Arc<dyn ChannelLauncher>;
    /// This build's installation plan for the connected host: its probe and the sidecar a local tab runs.
    fn plan(&mut self) -> Result<InstallPlan, String>;
    /// The agents the helper at `helper` runs on the connected host, through a connection pinned to `key`.
    fn agents(&mut self, host: &str, key: &str, helper: &Path) -> Result<Vec<Agent>, String>;
    /// Eitri's Mutagen engine, for the sync verbs and for `stop` (pause) and `uninstall` (terminate).
    fn sync_engine(&mut self) -> Result<Arc<dyn crate::remote_sync::engine::SyncEngine>, SyncError> {
        Err(SyncError::Engine("sync is not available here".into()))
    }
    /// A connection to `project`'s helper, for `eitri host sync watch`.
    fn sync_helper(
        &mut self,
        _project: &RemoteProject,
    ) -> Result<Arc<dyn crate::remote_sync::sessions::SyncHelper>, String> {
        Err("sync is not available here".into())
    }
    /// `remote_dev.*` as `init.lua` sets them.
    fn sync_settings(&self) -> super::cli_sync::SyncSettings {
        super::cli_sync::SyncSettings::default()
    }
    /// Terminal inventory of this installation. Reaches without a terminal service have no terminals.
    fn terminals(&mut self, _host: &str, _key: &str, _helper: &Path) -> Result<wire::ListTerminalsResponse, String> {
        Ok(wire::ListTerminalsResponse::default())
    }
}

/// What a command reads and writes besides its arguments.
pub struct Context<'a> {
    pub store: &'a RemoteProjects,
    pub reach: &'a mut dyn Reach,
    pub term: &'a mut dyn Terminal,
    /// `remote_dev.enabled`, checked by `open` and `sync watch`.
    pub remote_dev_enabled: &'a dyn Fn() -> bool,
    /// Seconds since the epoch, for a new record.
    pub now: u64,
}

/// Runs `eitri host <args>` and returns its exit code: 0, 1 when it failed, 64 for a command line it does not take.
pub fn run(args: &[String], cx: &mut Context<'_>) -> i32 {
    if matches!(args, [command, ..] if command == "open")
        || matches!(args, [command, verb, ..] if command == "sync" && verb == "watch")
    {
        if let Err(error) = listen_for_interrupts() {
            cx.term
                .say(&format!("eitri host: could not listen for interrupts: {error}"));
            return 1;
        }
    }
    run_command(args, cx)
}

fn run_command(args: &[String], cx: &mut Context<'_>) -> i32 {
    match args.first().map(String::as_str) {
        Some("sync") => return super::cli_sync::run_sync(&args[1..], cx),
        Some("git") => return super::cli_sync::run_git(&args[1..], cx),
        _ => {}
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = match args.as_slice() {
        ["open", spec] => open(cx, spec),
        ["status"] => status(cx, None),
        ["status", host] => status(cx, Some(host)),
        ["list"] => list(cx, None),
        ["list", host] => list(cx, Some(host)),
        ["stop", host] => stop(cx, host).and_then(|left| match left {
            true => Err(format!("a newer Eitri server on {host} was left running")),
            false => Ok(()),
        }),
        ["uninstall", host] => uninstall(cx, host, UNINSTALL_WAIT),
        _ => {
            cx.term.say(USAGE);
            return 64;
        }
    };
    match done {
        Ok(()) => 0,
        Err(why) => {
            cx.term.say(&format!("eitri host: {why}"));
            1
        }
    }
}

/// `<host>:<path>`, split at the first `:/` that does not begin `://`, so `user@host:/srv/project` and
/// `ssh://user@host:2222:/srv/project` both work. The path must be absolute and in normal form, as the server and the
/// record keep it.
pub fn parse_spec(spec: &str) -> Result<(String, PathBuf), String> {
    let split = spec
        .match_indices(":/")
        .map(|(at, _)| at)
        .find(|&at| !spec[at + 2..].starts_with('/'))
        .ok_or_else(|| format!("{spec:?}: expected <host>:<absolute path>"))?;
    let (host, path) = (&spec[..split], &spec[split + 1..]);
    agent::ssh::validate_host(host).map_err(|why| format!("{host:?}: {why}"))?;
    let root = PathBuf::from(path);
    super::first_connect::validate_root(&root)?;
    Ok((host.to_owned(), root))
}

fn open(cx: &mut Context<'_>, spec: &str) -> Result<(), String> {
    if !(cx.remote_dev_enabled)() {
        return Err(remote_dev_off("eitri host open"));
    }
    let (host, root) = parse_spec(spec)?;
    super::first_connect::run(cx, host, Some(root))?;
    cx.term.say("sync continues when you open the window");
    Ok(())
}

/// The recorded projects, `host` as typed limiting them, and a line for each record that could not be read.
fn projects(cx: &mut Context<'_>, host: Option<&str>) -> Result<Vec<RemoteProject>, String> {
    let (projects, unreadable) = cx.store.list().map_err(|e| e.to_string())?;
    for line in unreadable {
        cx.term.say(&format!("cannot read {line}"));
    }
    Ok(projects
        .into_iter()
        .filter(|project| host.is_none_or(|host| project.host == host))
        .collect())
}

/// `status [<host>]`: what this machine recorded, with no connection.
fn status(cx: &mut Context<'_>, host: Option<&str>) -> Result<(), String> {
    let projects = projects(cx, host)?;
    if projects.is_empty() {
        cx.term.say(&match host {
            Some(host) => format!("no project on {host} is recorded on this machine"),
            None => "no remote project is recorded on this machine".to_owned(),
        });
        return Ok(());
    }
    for project in projects {
        cx.term.say(&format!("{}:{}", project.host, project.root.display()));
        cx.term.say(&format!(
            "  host key {}; Eitri server {}",
            project.host_key_sha256, project.helper_version
        ));
        let installation = match cx.store.host(&project.host_key_sha256) {
            Ok(Some(record)) if record.consented => format!(
                "  {} installation(s) recorded at {}",
                record.helpers.len(),
                record.data_dir.display()
            ),
            Ok(Some(_)) => "  installation not consented to".to_owned(),
            Ok(None) => "  nothing installed from this machine under this host key".to_owned(),
            Err(e) => format!("  the installation record cannot be read: {e}"),
        };
        cx.term.say(&installation);
        let entries = match cx.store.terminals().record(&project.host_key_sha256, &project.root) {
            Ok(record) => record.entries,
            Err(e) => {
                cx.term.say(&format!("  terminal record cannot be read: {e}"));
                Vec::new()
            }
        };
        for entry in entries {
            let state = match entry.state {
                TerminalState::Opening => "opening",
                TerminalState::Open => "open",
                TerminalState::StopQueued => "stop queued",
            };
            cx.term.say(&format!(
                "  terminal {}  window {}  module {}  {state}",
                crate::onboarding::display::display_text(entry.terminal_id.as_deref().unwrap_or(&entry.command_id)),
                entry.slot,
                crate::onboarding::display::display_text(&entry.module)
            ));
        }
        if !project.queued_stops.is_empty() {
            cx.term.say(&format!(
                "  {} agent stop(s) wait until {} can be reached",
                project.queued_stops.len(),
                project.host
            ));
        }
    }
    Ok(())
}

/// What `list` says about one host's agents: they keep running with no window open, and are listed whether or not
/// a window shows them. An agent waiting for an answer says so; agents that exited are counted.
pub fn agent_lines(host: &str, agents: &[Agent]) -> Vec<String> {
    let running: Vec<&Agent> = agents.iter().filter(|agent| agent.alive).collect();
    let mut lines = vec![match running.len() {
        0 => format!("{host} runs no agent"),
        n => format!("{host} runs {n} agent(s); they keep running with no window open"),
    }];
    for agent in running {
        let account = if agent.account.is_empty() {
            String::from("the default account")
        } else {
            format!("account {}", agent.account)
        };
        let mut line = format!("  {}  {}  {account}", agent.sidecar_id, agent.root);
        let waiting: u64 = agent
            .sessions
            .iter()
            .filter(|session| !session.closed)
            .map(|session| session.pending)
            .sum();
        if waiting > 0 {
            line.push_str(&format!("  waiting for an answer to {waiting} request(s)"));
        }
        lines.push(line);
    }
    let exited = agents.iter().filter(|agent| !agent.alive).count();
    if exited > 0 {
        lines.push(format!("  {exited} exited agent(s) kept for their history"));
    }
    lines
}

/// How long `uninstall` waits for stopped daemons to let go of their lifetime locks before it gives up.
const UNINSTALL_WAIT: Duration = Duration::from_secs(30);

/// What the `UNINSTALL` step says when a daemon still holds its lifetime lock, and so removes nothing.
#[cfg(all(test, target_os = "linux"))]
const DAEMON_STILL_RUNS: &str = "an Eitri server daemon still runs";

/// One recorded host as `list`, `stop` and `uninstall` reach it: the key the connection verified, its installation
/// record, the helper the newest project record under that key last started there, and every project record under
/// that key, whatever host name it was recorded with.
struct Reached {
    key: String,
    record: crate::remote_project::RemoteHost,
    helper: PathBuf,
    installation: String,
    projects: Vec<RemoteProject>,
}

fn reach_host(cx: &mut Context<'_>, host: &str) -> Result<Reached, String> {
    let named = projects(cx, Some(host))?;
    let key = cx
        .reach
        .connect(host)
        .map_err(|e| format!("connecting to {host}: {e}"))?;
    // The newest record under the key the connection showed: an older record under a key that changed since does not
    // stand in the way of the others.
    let newest = named
        .iter()
        .filter(|project| project.host_key_sha256 == key)
        .max_by_key(|project| project.created_at)
        .cloned();
    if newest.is_none() && !named.is_empty() && cx.store.host(&key).map_err(|e| e.to_string())?.is_none() {
        let newest = named
            .iter()
            .max_by_key(|project| project.created_at)
            .expect("records were found");
        return Err(format!(
            "{}; this machine's records are kept under the old key: run eitri host open {host}:{}",
            host_key_changed(host, &newest.host_key_sha256, &key),
            newest.root.display()
        ));
    }
    let record = cx
        .store
        .host(&key)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("nothing was installed on {host} from this machine"))?;
    let installation = newest
        .map(|project| project.helper_version)
        .or_else(|| record.helpers.keys().next_back().cloned())
        .unwrap_or_default();
    let helper = record.data_dir.join(&installation).join("bin/eitri-server");
    let (all, _) = cx.store.list().map_err(|e| e.to_string())?;
    Ok(Reached {
        key: key.clone(),
        record,
        helper,
        installation,
        projects: all
            .into_iter()
            .filter(|project| project.host_key_sha256 == key)
            .collect(),
    })
}

/// A helper daemon `list-daemons` found: its identity, whether it speaks another protocol, and the key `stop-daemon`
/// takes for it (a helper that keyed daemons by version alone names none: the version is the key).
pub(super) struct Running {
    pub(super) daemon: DaemonInfo,
    incompatible: bool,
    pub(super) run: String,
}

/// The helper daemons `launcher`'s helper finds running.
pub(super) fn daemons(launcher: &dyn ChannelLauncher, key: &str) -> Result<Vec<Running>, String> {
    let tail = [
        OsString::from("list-daemons"),
        OsString::from("--args-hex"),
        OsString::new(),
    ];
    let child = {
        let _key = launcher.hold_host_key(key).map_err(|e| e.to_string())?;
        install::attached(launcher.spawn(&tail).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
    };
    let out = install::communicate(child, &[], &|_, _| {}).map_err(|e| e.to_string())?;
    let text = String::from_utf8(out).map_err(|_| "the helper's daemon list is not UTF-8".to_owned())?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).map_err(|e| format!("the helper's daemon list: {e}"))?;
            let incompatible = value["incompatible_protocol"].as_bool().unwrap_or(false);
            let run = value["run"].as_str().map(str::to_owned);
            let daemon: DaemonInfo =
                serde_json::from_value(value).map_err(|e| format!("the helper's daemon list: {e}"))?;
            let run = run.unwrap_or_else(|| daemon.version.clone());
            Ok(Running {
                daemon,
                incompatible,
                run,
            })
        })
        .collect()
}

/// The daemons running on the reached host, as the first recorded installation that answers lists them (the one the
/// newest record started first): a removed installation does not hide another. The launcher of the helper that
/// answered comes back with them; `Err` when none answered, with the last reason.
fn running(cx: &mut Context<'_>, reached: &Reached) -> Result<(Arc<dyn ChannelLauncher>, Vec<Running>), String> {
    let mut helpers = vec![reached.helper.clone()];
    for installation in reached.record.helpers.keys() {
        let helper = reached.record.data_dir.join(installation).join("bin/eitri-server");
        if !helpers.contains(&helper) {
            helpers.push(helper);
        }
    }
    let mut why = String::from("no installation is recorded");
    for helper in helpers {
        let launcher = cx.reach.launcher(&helper);
        match daemons(launcher.as_ref(), &reached.key) {
            Ok(running) => return Ok((launcher, running)),
            Err(e) => why = e,
        }
    }
    Err(why)
}

/// `list [<host>]`: each recorded host's running helper daemons and each installation's agents.
fn list(cx: &mut Context<'_>, host: Option<&str>) -> Result<(), String> {
    let mut hosts: Vec<String> = match host {
        Some(host) => vec![host.to_owned()],
        None => projects(cx, None)?.into_iter().map(|project| project.host).collect(),
    };
    hosts.sort();
    hosts.dedup();
    if hosts.is_empty() {
        return Err(match host {
            Some(host) => format!("no project on {host} is recorded on this machine"),
            None => "no remote project is recorded on this machine".to_owned(),
        });
    }
    let mut failed = Vec::new();
    for host in hosts {
        if let Err(why) = list_one(cx, &host) {
            cx.term.say(&format!("{host}: {why}"));
            failed.push(host);
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("could not list {}", failed.join(", ")))
    }
}

fn list_one(cx: &mut Context<'_>, host: &str) -> Result<(), String> {
    let reached = reach_host(cx, host)?;
    let (_, running) = running(cx, &reached)?;
    if running.is_empty() {
        cx.term.say(&format!("no Eitri server runs on {host}"));
        cx.term.say(&format!(
            "if a daemon died, run eitri host stop {host} to stop its surviving agents"
        ));
        return Ok(());
    }
    let mut failures = Vec::new();
    for Running {
        daemon,
        incompatible,
        run,
    } in &running
    {
        cx.term.say(&format!(
            "Eitri server {} ({}) runs on {host} as pid {}",
            daemon.version, daemon.rev, daemon.pid
        ));
        let exact = reached
            .record
            .helpers
            .keys()
            .find(|name| installation_run_key(name) == *run || *name == run);
        let legacy_prefix = format!("{}-{}", daemon.version, daemon.rev.get(..7).unwrap_or(&daemon.rev));
        let mut legacy = reached.record.helpers.keys().filter(|name| {
            *run == daemon.version
                && name
                    .strip_prefix(&legacy_prefix)
                    .is_some_and(|tail| tail.is_empty() || tail.starts_with('-'))
        });
        let first = legacy.next();
        let installation = exact.or_else(|| first.filter(|_| legacy.next().is_none()));
        let Some(installation) =
            installation.filter(|_| !incompatible && daemon.protocol_major == agent::ssh::PROTOCOL_MAJOR)
        else {
            cx.term.say(&format!(
                "agents of Eitri server {} ({run}) not listed: another protocol or no recorded installation",
                daemon.version
            ));
            continue;
        };
        let helper = reached.record.data_dir.join(installation).join("bin/eitri-server");
        match cx.reach.terminals(host, &reached.key, &helper) {
            Ok(terminals) => {
                for line in terminal_lines(&terminals) {
                    cx.term.say(&format!("[{installation}] {line}"));
                }
            }
            Err(why) => {
                cx.term.say(&format!(
                    "terminals of Eitri server {} ({installation}) not listed: {why}",
                    daemon.version
                ));
                failures.push(installation.clone());
            }
        }
        match cx.reach.agents(host, &reached.key, &helper) {
            Ok(agents) => {
                let mut lines = agent_lines(host, &agents);
                if !agents.iter().any(|agent| agent.alive) {
                    lines[0] = "no running agents in this installation".into();
                }
                for line in lines {
                    cx.term.say(&format!("[{installation}] {line}"));
                }
            }
            Err(why) => {
                cx.term.say(&format!(
                    "agents of Eitri server {} ({installation}) not listed: {why}",
                    daemon.version
                ));
                failures.push(installation.clone());
            }
        }
    }
    if !failures.is_empty() {
        return Err(format!("could not list agents of {}", failures.join(", ")));
    }
    Ok(())
}

pub(super) fn installation_run_key(installation: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "i-{}",
        &format!(
            "{:x}",
            Sha256::digest(format!("eitri-server-installation\0{installation}").as_bytes())
        )[..16]
    )
}

#[derive(Default)]
struct Recovery {
    failures: Vec<String>,
    runs: Vec<String>,
}

#[derive(serde::Deserialize)]
struct LiveRun {
    run: String,
    daemon: Option<DaemonInfo>,
    installation: Option<String>,
}

/// Read the version from both content-keyed and legacy installation names, independently of saved run info.
fn installation_identity(name: &str) -> Option<DaemonInfo> {
    let hex16 = |part: &str| part.len() == 16 && part.bytes().all(|b| b.is_ascii_hexdigit());
    let keyed = name
        .rsplit_once('-')
        .filter(|(_, hash)| hex16(hash))
        .and_then(|(rest, _)| rest.rsplit_once('-'))
        .filter(|(_, hash)| hex16(hash))
        .map(|(rest, _)| rest);
    let (version, rev) = keyed.unwrap_or(name).rsplit_once('-')?;
    if rev.is_empty() || !rev.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(DaemonInfo {
        version: version.into(),
        rev: rev.into(),
        pid: 0,
        protocol_major: agent::ssh::PROTOCOL_MAJOR,
        protocol_minor: 0,
        started_unix_ms: 0,
    })
}

/// Checked inventory finds surviving agents; unavailable inventory falls back to checked recorded recovery.
fn recover_recorded(cx: &mut Context<'_>, host: &str, reached: &Reached, before: &[Running]) -> Recovery {
    let mut recovery = Recovery::default();
    let mut inventory = None;
    let mut installations: Vec<_> = reached.record.helpers.keys().collect();
    installations.sort_by_key(|name| name.as_str() != reached.installation);
    for installation in &installations {
        let helper = reached.record.data_dir.join(installation).join("bin/eitri-server");
        let launcher = cx.reach.launcher(&helper);
        match install::live_recorded_runs_with(launcher.as_ref(), &reached.record, installation) {
            Ok((bytes, diagnostic)) => {
                let parsed = std::str::from_utf8(&bytes).ok().and_then(|text| {
                    text.lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(serde_json::from_str::<LiveRun>)
                        .collect::<Result<Vec<_>, _>>()
                        .ok()
                });
                if let Some(runs) = parsed {
                    for line in String::from_utf8_lossy(&diagnostic).lines() {
                        cx.term.say(line);
                    }
                    inventory = Some(runs);
                    break;
                }
            }
            Err(install::InstallError::SidecarsStillRun(why)) => {
                recovery.failures.push(format!("could not check surviving agents on {host}: {why}; stop them with the Eitri that installed it, or kill <pid> for an alive record"));
                return recovery;
            }
            Err(_) => {}
        }
    }
    let inventoried = inventory.is_some();
    let candidates: Vec<_> = if let Some(inventory) = inventory {
        let mut candidates = Vec::new();
        for live in inventory {
            if before.iter().any(|running| running.run == live.run) {
                continue;
            }
            let installation = live
                .installation
                .as_ref()
                .filter(|name| reached.record.helpers.contains_key(*name) && installation_run_key(name) == live.run)
                .or_else(|| {
                    installations
                        .iter()
                        .copied()
                        .find(|name| installation_run_key(name) == live.run)
                });
            let stoppable = live
                .daemon
                .as_ref()
                .is_some_and(|daemon| !super::skew::is_newer_than_this_build(daemon))
                && installation
                    .and_then(|name| installation_identity(name))
                    .is_some_and(|identity| !super::skew::is_newer_than_this_build(&identity));
            if !stoppable {
                let version = live
                    .daemon
                    .as_ref()
                    .map(|daemon| daemon.version.as_str())
                    .unwrap_or("unknown version");
                recovery.failures.push(format!("agents of Eitri server {version} (run {}) survive on {host}; stop them with the Eitri that installed it, or kill <pid> for an alive record", live.run));
                continue;
            }
            candidates.push((live.run, installation.expect("stoppable installation").clone()));
        }
        candidates
    } else {
        cx.term.say(&format!(
            "inventory was not available on {host}; idle installations may have been started and stopped"
        ));
        installations
            .into_iter()
            .filter_map(|installation| {
                let identity = installation_identity(installation)?;
                let run = installation_run_key(installation);
                if super::skew::is_newer_than_this_build(&identity) || before.iter().any(|running| running.run == run) {
                    return None;
                }
                Some((run, installation.clone()))
            })
            .collect()
    };
    for (run, installation) in candidates {
        let helper = reached.record.data_dir.join(&installation).join("bin/eitri-server");
        let launcher = cx.reach.launcher(&helper);
        match install::recover_recorded_with(launcher.as_ref(), &reached.record, &installation) {
            Ok(()) => recovery.runs.push(run),
            Err(install::InstallError::Reinstall(reason)) if !inventoried && reason == agent::remote_client::MISSING_INSTALLATION => {}
            Err(why) => recovery.failures.push(format!(
                "could not recover {installation} (run {run}) on {host} to stop its agents: {why}; stop them with the Eitri that installed it, or kill <pid> for an alive record"
            )),
        }
    }
    recovery
}

/// Several failures, each on a line of its own: all but the last are said here as `run_command` says an error, and
/// the last is returned for it to say. A line break inside one line would reach the terminal escaped.
fn say_all_but_last(cx: &mut Context<'_>, mut failures: Vec<String>) -> String {
    let last = failures.pop().unwrap_or_default();
    for failure in failures {
        cx.term.say(&format!("eitri host: {failure}"));
    }
    last
}

/// `stop <host>`: every helper daemon on `host` this build may stop, with the agents each runs. Returns whether a
/// daemon was left running (a newer Eitri's).
fn stop(cx: &mut Context<'_>, host: &str) -> Result<bool, String> {
    let reached = reach_host(cx, host)?;
    super::cli_sync::pause_host_sessions(cx, &reached.key, "stop", host);
    let before = running(cx, &reached);
    let mut recovery = recover_recorded(
        cx,
        host,
        &reached,
        before.as_ref().map(|(_, runs)| runs.as_slice()).unwrap_or_default(),
    );
    let listed = if recovery.runs.is_empty() {
        before
    } else {
        running(cx, &reached)
    };
    let result = listed.and_then(|(launcher, running)| {
        stop_running(
            cx,
            host,
            &reached,
            launcher.as_ref(),
            running,
            &recovery.runs,
            !recovery.failures.is_empty(),
        )
    });
    let stopped = match result {
        Ok(stopped) => stopped,
        Err(why) => {
            recovery.failures.push(why);
            return Err(say_all_but_last(cx, recovery.failures));
        }
    };
    if !recovery.failures.is_empty() {
        return Err(say_all_but_last(cx, recovery.failures));
    }
    if !stopped.left {
        for notice in cx
            .store
            .terminals()
            .clear_host(&reached.key)
            .map_err(|e| e.to_string())?
        {
            cx.term.say(&notice);
        }
    }
    Ok(stopped.left)
}

#[derive(Debug)]
struct Stopped {
    left: bool,
    count: usize,
}

fn stop_running(
    cx: &mut Context<'_>,
    host: &str,
    reached: &Reached,
    launcher: &dyn ChannelLauncher,
    running: Vec<Running>,
    recovered: &[String],
    unresolved: bool,
) -> Result<Stopped, String> {
    if running.is_empty() {
        if !unresolved {
            cx.term.say(&format!("no Eitri server runs on {host}"));
        }
        return Ok(Stopped { left: false, count: 0 });
    }
    let mut count = 0;
    let mut left = false;
    let mut first_error = None;
    for Running { daemon, run, .. } in running {
        if super::skew::is_newer_than_this_build(&daemon) {
            cx.term.say(&format!(
                "{host} runs a newer Eitri server {}; stop it with that Eitri",
                daemon.version
            ));
            left = true;
            continue;
        }
        if let Err(e) =
            super::skew::stop_daemon_with(launcher, &reached.record, Some(&reached.installation), &daemon, &run)
        {
            first_error.get_or_insert_with(|| format!("stopping Eitri server {} on {host}: {e}", daemon.version));
            continue;
        }
        count += 1;
        let action = if recovered.contains(&run) {
            "recovered and stopped"
        } else {
            "stopped"
        };
        cx.term.say(&format!(
            "{action} Eitri server {} on {host}, with its agents",
            daemon.version
        ));
    }
    first_error.map_or(Ok(Stopped { left, count }), |why| {
        Err(format!("{why}; stopped {count} daemon(s) and their agents"))
    })
}

/// `uninstall <host>`: stops every helper daemon, removes the helper's data and state directories on the server and
/// forgets this machine's records of the host: the host record and every project record under its key. The removal
/// itself refuses while any daemon holds its lifetime lock or a recorded sidecar may still be alive. This waits
/// up to `wait` for stopped daemons to release their locks; uncertain sidecars retain their records and helper.
fn uninstall(cx: &mut Context<'_>, host: &str, wait: Duration) -> Result<(), String> {
    // Paused first, so nothing syncs while the helper goes; terminated once it is gone.
    let reached = reach_host(cx, host)?;
    super::cli_sync::pause_host_sessions(cx, &reached.key, "uninstall", host);
    let before = running(cx, &reached);
    let recovery = recover_recorded(
        cx,
        host,
        &reached,
        before.as_ref().map(|(_, runs)| runs.as_slice()).unwrap_or_default(),
    );
    for why in &recovery.failures {
        cx.term.say(why);
    }
    let mut stopped = 0;
    let listed = if recovery.runs.is_empty() {
        before
    } else {
        running(cx, &reached)
    };
    match listed {
        Ok((launcher, running)) => {
            let result = stop_running(
                cx,
                host,
                &reached,
                launcher.as_ref(),
                running,
                &recovery.runs,
                !recovery.failures.is_empty(),
            )
            .map_err(|why| format!("{why}; the helper was not removed"))?;
            stopped = result.count;
            if result.left {
                return Err(format!("an Eitri server still runs on {host}; stopped {stopped} daemon(s) and their agents; the helper was not removed"));
            }
        }
        Err(why) => cx.term.say(&format!(
            "no recorded Eitri server on {host} lists its daemons ({why}); removing what no running daemon holds"
        )),
    }
    let (data, state) = (|| {
        let data = install::checked_path(&reached.record.data_dir).map_err(|e| e.to_string())?;
        let state = install::checked_path(&reached.record.state_dir).map_err(|e| e.to_string())?;
        for dir in [data, state] {
            if !dir.ends_with("/eitri-server") {
                return Err(format!("{dir} is not an Eitri server directory; nothing was removed"));
            }
        }
        let launcher = cx.reach.launcher(&reached.helper);
        let checkers = install::recorded_checkers(&reached.record, &reached.installation).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + wait;
        loop {
            let child = {
                let _key = launcher.hold_host_key(&reached.key).map_err(|e| e.to_string())?;
                install::attached(
                    launcher
                        .spawn_step(super::steps::UNINSTALL, &[data, state])
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?
            };
            match install::communicate(child, &checkers, &|_, _| {}) {
                Ok(_) => break,
                Err(install::InstallError::DaemonStillRuns(why)) => {
                    if Instant::now() >= deadline {
                        return Err(format!("an Eitri server still runs on {host} ({why})"));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(install::InstallError::SidecarsStillRun(why)) => {
                    return Err(format!("recorded agents or terminals on {host} may still run ({why}); stop the run key with the Eitri that installed it, or kill <pid> for an alive record; if no checked checker is installed, open a project with eitri host open {host}:/path/to/project to reinstall the helper"));
                }
                Err(e) => return Err(format!("removing Eitri server from {host}: {e}")),
            }
        }
        Ok((data, state))
    })()
    .map_err(|why: String| {
        format!("{why}; stopped {stopped} daemon(s) and their agents; the helper was not removed")
    })?;
    super::cli_sync::terminate_host_sessions(cx, host, &reached.key);
    let mut forgotten = 0;
    for notice in cx
        .store
        .terminals()
        .clear_host(&reached.key)
        .map_err(|e| e.to_string())?
    {
        cx.term.say(&notice);
    }
    for project in &reached.projects {
        if cx.store.forget(&project.root).map_err(|e| e.to_string())? {
            forgotten += 1;
        }
    }
    cx.store.forget_host(&reached.key).map_err(|e| e.to_string())?;
    cx.term.say(&format!(
        "removed Eitri server from {host} ({data} and {state}); this machine forgot {forgotten} project record(s)"
    ));
    Ok(())
}

/// [`Reach`] over ssh: one master per host (the person's own ssh config applies), a host key ssh does not know
/// shown from a scratch connection and added to `known_hosts` only after a yes.
pub struct SshReach {
    ssh_dir: PathBuf,
    exe: PathBuf,
    known_hosts: PathBuf,
    master: Option<ControlMaster>,
    sync: super::cli_sync::SyncSettings,
    engine: Option<Arc<dyn crate::remote_sync::engine::SyncEngine>>,
}

impl SshReach {
    /// `ssh_dir` holds the masters ([`ssh_dir`]), `exe` is this Eitri (ssh runs its `host known-hosts-probe`), and
    /// `known_hosts` is the person's own file an accepted key is added to.
    pub fn new(ssh_dir: PathBuf, exe: PathBuf, known_hosts: PathBuf) -> Self {
        Self {
            ssh_dir,
            exe,
            known_hosts,
            master: None,
            sync: Default::default(),
            engine: None,
        }
    }

    /// `remote_dev.*` as `init.lua` sets them, for the sync verbs.
    pub fn with_sync_settings(mut self, settings: super::cli_sync::SyncSettings) -> Self {
        self.sync = settings;
        self
    }

    /// The state home the masters' directory (`<state home>/eitri/ssh`) is under.
    fn state_home(&self) -> Result<PathBuf, String> {
        self.ssh_dir
            .parent()
            .and_then(Path::parent)
            .map(Path::to_owned)
            .ok_or_else(|| "no state home".to_owned())
    }

    fn master(&self) -> Result<&ControlMaster, String> {
        self.master.as_ref().ok_or_else(|| "not connected".to_owned())
    }
}

impl Reach for SshReach {
    fn terminals(&mut self, host: &str, key: &str, helper: &Path) -> Result<wire::ListTerminalsResponse, String> {
        let client = agent::remote_client::HelperClient::connect(self.launcher(helper), host, Some(key))
            .map_err(|e| e.to_string())?;
        service_call(&client, |mut service| async move {
            match service.list_terminals(wire::ListTerminalsRequest {}).await {
                Ok(response) => Ok(response.into_inner()),
                Err(status)
                    if status.code() == agent::remote_client::terminal_transport::tonic::Code::Unimplemented =>
                {
                    Ok(wire::ListTerminalsResponse::default())
                }
                Err(status) => Err(status),
            }
        })
        .map_err(|e| e.to_string())
    }

    fn connect(&mut self, host: &str) -> Result<String, ConnectError> {
        let master = ControlMaster::ensure(host, &self.ssh_dir, &self.exe)?;
        let key = master.verified_host_key().map_err(ConnectError::from_io)?;
        self.master = Some(master);
        Ok(key.sha256)
    }

    fn first_contact(&mut self, host: &str) -> Result<Contact, String> {
        let contact = super::first_contact(host, &self.ssh_dir).map_err(|e| e.to_string())?;
        let known_hosts = self.known_hosts.clone();
        Ok(Contact {
            key_type: contact.key_type.clone(),
            sha256: contact.sha256.clone(),
            accept: Box::new(move || {
                super::accept_first_contact(contact, &known_hosts)
                    .map_err(|e| format!("adding the host key to {}: {e}", known_hosts.display()))
            }),
        })
    }

    fn launcher(&self, helper: &Path) -> Arc<dyn ChannelLauncher> {
        let master = self
            .master
            .clone()
            .expect("a launcher is asked for only after a connection");
        Arc::new(SshLauncher {
            master,
            helper: helper.to_path_buf(),
        })
    }

    fn plan(&mut self) -> Result<InstallPlan, String> {
        let master = self.master()?;
        let probe = super::probe_step::probe(master).map_err(|e| format!("probing {}: {e}", master.host))?;
        let sidecar = agent::sidecar_for_upload()?;
        install::plan_connection(&probe, &sidecar.path).map_err(|e| e.to_string())
    }

    fn sync_engine(&mut self) -> Result<Arc<dyn crate::remote_sync::engine::SyncEngine>, SyncError> {
        if let Some(engine) = &self.engine {
            return Ok(engine.clone());
        }
        let state_home = self.state_home().map_err(SyncError::Io)?;
        let engine: Arc<dyn crate::remote_sync::engine::SyncEngine> =
            Arc::new(crate::remote_sync::mutagen::MutagenEngine::open(
                &state_home,
                self.sync.mutagen.as_deref(),
                &state_home.join("eitri/mutagen/transport"),
            )?);
        self.engine = Some(engine.clone());
        Ok(engine)
    }

    fn sync_helper(
        &mut self,
        project: &RemoteProject,
    ) -> Result<Arc<dyn crate::remote_sync::sessions::SyncHelper>, String> {
        let state_home = self.state_home()?;
        let store = RemoteProjects::new(Some(state_home.as_os_str()), None);
        let master = ControlMaster::ensure(&project.host, &self.ssh_dir, &self.exe)
            .map_err(|error| format!("connecting to {}: {error}", project.host))?;
        let connection = crate::remote_project::RemoteConnection::open_over_ssh(
            &master,
            &store,
            project,
            &project.helper_version,
            agent::BUNDLED_SIDECAR_REVISION,
        )?;
        Ok(Arc::new(connection))
    }

    fn sync_settings(&self) -> super::cli_sync::SyncSettings {
        self.sync.clone()
    }

    fn agents(&mut self, host: &str, key: &str, helper: &Path) -> Result<Vec<Agent>, String> {
        let client = agent::remote_client::HelperClient::connect(self.launcher(helper), host, Some(key))
            .map_err(|e| e.to_string())?;
        let sidecars = client.list().map_err(|status| status.message().to_owned())?;
        Ok(sidecars
            .into_iter()
            .map(|sidecar| Agent {
                sidecar_id: sidecar.sidecar_id,
                root: sidecar.root,
                account: sidecar.account,
                alive: sidecar.alive,
                sessions: sidecar
                    .sessions
                    .into_iter()
                    .map(|session| AgentSession {
                        session_id: session.session_id,
                        closed: session.closed,
                        pending: session.pending,
                    })
                    .collect(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::super::first_connect::MUTAGEN_NEEDED;
    #[cfg(target_os = "linux")]
    use super::super::window::installed;
    use super::*;
    use crate::remote_project::RemoteHost;
    use crate::test_scratch_dir::ScratchDir;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::rc::Rc;

    #[derive(Default)]
    struct Scripted {
        answers: VecDeque<bool>,
        said: Vec<String>,
        asked: Vec<String>,
        on_ask: Option<Box<dyn FnMut()>>,
        on_say: Option<Box<dyn FnMut(&str)>>,
        wait_for_sync: bool,
    }

    impl Terminal for Scripted {
        fn input_line(&mut self) -> std::sync::mpsc::Receiver<()> {
            let (send, recv) = std::sync::mpsc::channel();
            if !self.wait_for_sync {
                let _ = send.send(());
            }
            recv
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_owned());
            if let Some(hook) = &mut self.on_say {
                hook(line);
            }
        }
        fn ask(&mut self, question: &str) -> bool {
            self.asked.push(question.to_owned());
            if let Some(hook) = &mut self.on_ask {
                hook();
            }
            self.answers.pop_front().unwrap_or(false)
        }
    }

    use crate::remote_sync::actor::tests::{FakeEngine, FakeHelper};

    /// A host reached without ssh: a key ssh does not know until it is accepted, and the steps and helper of a local
    /// fixture.
    struct Fixture {
        engine: Arc<FakeEngine>,
        helper: Arc<FakeHelper>,
        engine_error: Option<SyncError>,
        root: PathBuf,
        known: Rc<Cell<bool>>,
        connects: usize,
        launcher: Arc<dyn ChannelLauncher>,
        agents: Vec<Agent>,
        no_upload_space: bool,
    }

    impl Reach for Fixture {
        fn sync_engine(&mut self) -> Result<Arc<dyn crate::remote_sync::engine::SyncEngine>, SyncError> {
            if let Some(error) = &self.engine_error {
                Err(error.clone())
            } else {
                Ok(self.engine.clone())
            }
        }
        fn sync_helper(
            &mut self,
            _: &RemoteProject,
        ) -> Result<Arc<dyn crate::remote_sync::sessions::SyncHelper>, String> {
            Ok(self.helper.clone())
        }

        fn connect(&mut self, host: &str) -> Result<String, ConnectError> {
            if host != "example-host" {
                return Err("unknown host".to_owned().into());
            }
            self.connects += 1;
            if self.known.get() {
                Ok("SHA256:example".into())
            } else {
                Err(ConnectError::from_stderr(
                    host,
                    "No ED25519 host key is known for example-host and you have requested strict checking.\nHost \
                     key verification failed.\n",
                ))
            }
        }
        fn first_contact(&mut self, _host: &str) -> Result<Contact, String> {
            let known = self.known.clone();
            Ok(Contact {
                key_type: "ssh-ed25519".into(),
                sha256: "SHA256:example".into(),
                accept: Box::new(move || {
                    known.set(true);
                    Ok(())
                }),
            })
        }
        fn launcher(&self, _helper: &Path) -> Arc<dyn ChannelLauncher> {
            self.launcher.clone()
        }
        fn plan(&mut self) -> Result<InstallPlan, String> {
            Ok(if self.no_upload_space {
                install::test_plan_without_upload_space(&self.root)
            } else {
                install::test_plan(&self.root, "open").0
            })
        }
        fn agents(&mut self, _host: &str, key: &str, _helper: &Path) -> Result<Vec<Agent>, String> {
            assert_eq!(key, "SHA256:example");
            Ok(self.agents.clone())
        }
    }

    fn fixture(root: &Path, known: bool) -> Fixture {
        let (_, launcher) = install::test_plan(root, "open");
        let log = Arc::default();
        Fixture {
            engine: FakeEngine::new(Arc::clone(&log), &root.join("project")),
            helper: FakeHelper::new(log),
            engine_error: None,
            root: root.to_path_buf(),
            known: Rc::new(Cell::new(known)),
            connects: 0,
            launcher: Arc::new(launcher),
            agents: Vec::new(),
            no_upload_space: false,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn window_flow_asks_the_same_questions_as_host_open() {
        use super::super::first_connect::{FolderCompletion, FolderRequest, WindowLaunch};
        use crate::remote_sync::{
            engine::{session_name, Role, SyncState},
            SyncKey,
        };

        struct Window {
            root: PathBuf,
            prompt: Scripted,
            chose_folder: bool,
            opened: bool,
        }

        impl Terminal for Window {
            fn say(&mut self, line: &str) {
                self.prompt.say(line);
            }

            fn ask(&mut self, question: &str) -> bool {
                self.prompt.ask(question)
            }

            fn folder(&mut self, completion: Option<FolderCompletion>) -> Result<FolderRequest, String> {
                assert!(completion.is_none());
                assert_eq!(self.prompt.asked.len(), 1, "only the host key precedes a typed folder");
                self.chose_folder = true;
                Ok(FolderRequest::Open(self.root.clone()))
            }

            fn watch_first_sync(&mut self) -> std::sync::mpsc::Receiver<()> {
                self.prompt.input_line()
            }

            fn tree_ready(&mut self, project: &RemoteProject, git_pct: u8) -> Result<Option<WindowLaunch>, String> {
                assert_eq!(project.root, self.root);
                assert_eq!(git_pct, 0);
                self.opened = true;
                let (exited, ended) = std::sync::mpsc::channel();
                exited.send(Some(0)).unwrap();
                Ok(Some(WindowLaunch { pid: 0, ended }))
            }
        }

        let server = ScratchDir::new("remote", "question-server");
        let run = |scratch: &Path, window: bool, other_host: bool| {
            let root = scratch.join("project");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("local-work"), b"saved here").unwrap();
            let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
            if other_host {
                let mut earlier = project_record("older");
                earlier.host = "previous-host".into();
                earlier.root = root.clone();
                store.save(&earlier).unwrap();
            }
            let mut reach = fixture(&server, false);
            let tree = session_name(&SyncKey::new("SHA256:example", &root), Role::Tree);
            let engine = reach.engine.clone();
            let prompt = Scripted {
                wait_for_sync: window,
                on_say: window.then(|| {
                    let finish_tree: Box<dyn FnMut(&str)> = Box::new(move |line| {
                        if line.starts_with("syncing ") {
                            crate::remote_sync::actor::tests::wait(|| engine.status_of(&tree).is_some());
                            let mut status = engine.status_of(&tree).unwrap();
                            status.state = SyncState::Watching;
                            status.successful_cycles = 1;
                            engine.push(status);
                        }
                    });
                    finish_tree
                }),
                answers: [true, true, true, true].into(),
                ..Default::default()
            };
            let result = if window {
                let mut window = Window {
                    root,
                    prompt,
                    chose_folder: false,
                    opened: false,
                };
                super::super::first_connect::run(
                    &mut Context {
                        store: &store,
                        reach: &mut reach,
                        term: &mut window,
                        remote_dev_enabled: &|| true,
                        now: 1_700_000_000,
                    },
                    "example-host".into(),
                    None,
                )
                .unwrap();
                assert!(window.chose_folder && window.opened);
                window.prompt
            } else {
                let mut cli = prompt;
                assert_eq!(
                    run_with(
                        &store,
                        &mut reach,
                        &mut cli,
                        &["open", &format!("example-host:{}", root.display())]
                    ),
                    0
                );
                cli
            };
            assert!(result.said.iter().any(|line| line.starts_with("Server: Linux x86_64")));
            assert!(result.said.iter().any(|line| line.starts_with("expected size: ")));
            result
                .asked
                .iter()
                .map(|line| line.replace(scratch.to_str().unwrap(), "/srv"))
                .collect::<Vec<_>>()
        };

        let cli = ScratchDir::new("remote", "terminal-questions");
        let window = ScratchDir::new("remote", "window-questions");
        let questions = run(&cli, false, false);
        assert_eq!(questions, run(&window, true, false));
        assert_eq!(questions.len(), 3);
        assert!(questions[0].contains("SHA256:example"));
        assert!(questions[1].starts_with("Install unsigned build"));
        assert!(questions[2].starts_with("Move /srv/project to"));

        let cli = ScratchDir::new("remote", "terminal-replacement-questions");
        let window = ScratchDir::new("remote", "window-replacement-questions");
        let questions = run(&cli, false, true);
        assert_eq!(questions, run(&window, true, true));
        assert_eq!(questions.len(), 4);
        assert!(questions[1].contains("recorded as a project on previous-host. Record it on example-host instead?"));
        assert!(questions[2].starts_with("Install unsigned build"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn aborted_reopen_withdraws_unused_setup_before_the_first_question() {
        let scratch = ScratchDir::new("remote", "unused-setup");
        let mut project = project_record("old");
        project.root = scratch.join("project");
        project.set_sync_record(&crate::remote_project::SyncRecord {
            engine: "mutagen".into(),
            set_up: true,
            host_key_sha256: project.host_key_sha256.clone(),
            ..Default::default()
        });
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        store.save(&project).unwrap();
        let during = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let root = project.root.clone();
        let mut term = Scripted {
            on_ask: Some(Box::new(move || {
                let record = during.find(&root).unwrap().unwrap().sync_record().unwrap().unwrap();
                assert!(!record.set_up && !record.has_sessions());
            })),
            ..Default::default()
        };
        let mut reach = fixture(&scratch, false);
        let spec = format!("example-host:{}", project.root.display());
        assert_eq!(run_with(&store, &mut reach, &mut term, &["open", &spec]), 1);
        assert_eq!(term.asked.len(), 1);
        assert!(
            !store
                .find(&project.root)
                .unwrap()
                .unwrap()
                .sync_record()
                .unwrap()
                .unwrap()
                .set_up
        );
        assert!(reach.engine.calls("create").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_marker_refusal_keeps_the_existing_tree_and_leaves_sync_disabled() {
        let scratch = ScratchDir::new("remote", "refused-setup");
        let root = scratch.join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("local-work"), b"saved here").unwrap();
        std::fs::create_dir(scratch.join(".stfolder")).unwrap();
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut reach = fixture(&scratch, true);
        let mut term = Scripted {
            answers: [true, true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(
                &store,
                &mut reach,
                &mut term,
                &["open", &format!("example-host:{}", root.display())]
            ),
            1
        );
        // Each line is said on its own: a line break inside one would reach the terminal escaped.
        assert!(term.said.iter().all(|line| !line.contains('\n')), "{:?}", term.said);
        let at = term
            .said
            .iter()
            .position(|line| line.ends_with(".stignore:"))
            .unwrap_or_else(|| panic!("{:?}", term.said));
        assert_eq!(term.said[at + 1], "/project");
        assert!(term
            .said
            .last()
            .unwrap()
            .contains("other devices stop receiving updates"));
        assert!(term.said.iter().any(|s| s.contains("1 files, 10 bytes")));
        assert!(
            store.find(&root).unwrap().is_none(),
            "the record this run made is taken back"
        );
        assert!(reach.engine.calls("create").is_empty());
        assert_eq!(std::fs::read(root.join("local-work")).unwrap(), b"saved here");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_mutagen_explains_and_stops() {
        let scratch = ScratchDir::new("remote", "no-engine");
        let root = scratch.join("project");
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut reach = fixture(&scratch, true);
        reach.engine_error = Some(SyncError::Unsupported("Mutagen is not installed".into()));
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(
                &store,
                &mut reach,
                &mut term,
                &["open", &format!("example-host:{}", root.display())]
            ),
            1
        );
        assert!(term.said.last().unwrap().contains(MUTAGEN_NEEDED));
        assert!(term.said.iter().all(|line| !line.contains('\n')), "{:?}", term.said);
        assert!(term.said.iter().any(|line| line.ends_with("Mutagen is not installed")));
        assert!(
            store.find(&root).unwrap().is_none(),
            "the record this run made is taken back"
        );
        assert!(reach.engine.calls("create").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn engine_setup_errors_keep_their_own_diagnostic() {
        let scratch = ScratchDir::new("remote", "engine-error");
        let root = scratch.join("project");
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut reach = fixture(&scratch, true);
        reach.engine_error = Some(SyncError::Io("cannot write transport scripts".into()));
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(
                &store,
                &mut reach,
                &mut term,
                &["open", &format!("example-host:{}", root.display())]
            ),
            1
        );
        assert_eq!(term.said.last().unwrap(), "eitri host: cannot write transport scripts");
        assert!(!term.said.iter().any(|line| line.contains(MUTAGEN_NEEDED)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_held_root_lock_refuses_the_move_before_consent_even_without_a_sync_record() {
        let scratch = ScratchDir::new("remote", "held-root");
        let root = scratch.join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("work"), b"original").unwrap();
        let locks = actor::lock_dir(&scratch, &root);
        status_file::prepare_dir(&scratch, &locks).unwrap();
        let held = agent::private_fs::PrivateLock::open(&locks.join("lock")).unwrap();
        assert!(held.try_lock().unwrap());
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut old = project_record("old");
        old.root = root.clone();
        old.host = "previous-host".into();
        store.save(&old).unwrap();
        let mut reach = fixture(&scratch, true);
        let mut term = Scripted {
            answers: [true, true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(
                &store,
                &mut reach,
                &mut term,
                &["open", &format!("example-host:{}", root.display())]
            ),
            1
        );
        assert!(term
            .said
            .last()
            .unwrap()
            .contains("close the window or sync watch using"));
        assert!(term.asked.is_empty());
        assert_eq!(store.find(&root).unwrap().unwrap().host, old.host);
        assert_eq!(std::fs::read(root.join("work")).unwrap(), b"original");
        assert!(reach.engine.calls("create").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_attachment_or_watch_withdraws_setup_without_a_session() {
        for lock_name in ["lock", "action.lock"] {
            let scratch = ScratchDir::new("remote", "attach-fails");
            let root = scratch.join("project");
            let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
            let mut reach = fixture(&scratch, true);
            let blocked_lock = actor::lock_dir(&scratch, &root).join(lock_name);
            let mut term = Scripted {
                answers: [true].into(),
                wait_for_sync: true,
                on_say: Some(Box::new(move |line| {
                    if line.starts_with("expected size:") {
                        std::fs::create_dir(&blocked_lock).unwrap();
                    }
                })),
                ..Default::default()
            };
            assert_eq!(
                run_with(
                    &store,
                    &mut reach,
                    &mut term,
                    &["open", &format!("example-host:{}", root.display())]
                ),
                1
            );
            let record = store.find(&root).unwrap().unwrap().sync_record().unwrap().unwrap();
            assert!(!record.set_up && !record.has_sessions());
            assert!(term.said.iter().any(|s| s.contains("Cleared sync setup")));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reopen_of_own_mirror_is_not_moved_aside() {
        let scratch = ScratchDir::new("remote", "own");
        let root = scratch.join("project");
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut reach = fixture(&scratch, true);
        let spec = format!("example-host:{}", root.display());
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", &spec]),
            0,
            "{:?}",
            term.said
        );
        let creates = reach.engine.calls("create");
        assert_eq!(creates.len(), 2);
        std::fs::write(root.join("work"), b"still here").unwrap();
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", &spec]),
            0,
            "{:?}",
            term.said
        );
        assert!(term.asked.is_empty());
        assert_eq!(std::fs::read(root.join("work")).unwrap(), b"still here");
        assert_eq!(reach.engine.calls("create"), creates);
        assert!(!reach.engine.calls("resume").is_empty());
    }

    /// The record names the folder first connect prepared: another folder put at the same path is not the mirror,
    /// so reopening asks the move question and starts from a new baseline instead of resuming the old sessions.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_folder_replaced_at_a_mirror_path_is_asked_about_and_starts_afresh() {
        let scratch = ScratchDir::new("remote", "replaced");
        let root = scratch.join("project");
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut reach = fixture(&scratch, true);
        let spec = format!("example-host:{}", root.display());
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", &spec]),
            0,
            "{:?}",
            term.said
        );
        let first = store.find(&root).unwrap().unwrap().sync_record().unwrap().unwrap();
        assert!(first.root_identity.is_some() && first.has_sessions());
        let aside = scratch.join("moved-mirror");
        std::fs::rename(&root, &aside).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clone.txt"), b"a fresh clone").unwrap();
        let terminated = reach.engine.calls("terminate").len();
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", &spec]),
            0,
            "{:?}",
            term.said
        );
        assert_eq!(term.asked.len(), 1, "{:?}", term.asked);
        assert!(
            term.asked[0].starts_with(&format!("Move {} to ", root.display())),
            "{:?}",
            term.asked
        );
        assert!(
            reach.engine.calls("terminate").len() > terminated,
            "the old sessions were resumed"
        );
        let kept = term
            .said
            .iter()
            .find_map(|line| line.strip_prefix("Kept the existing copy at "))
            .unwrap();
        assert_eq!(
            std::fs::read(Path::new(kept).join("clone.txt")).unwrap(),
            b"a fresh clone"
        );
        let record = store.find(&root).unwrap().unwrap().sync_record().unwrap().unwrap();
        assert_eq!(
            record.root_identity,
            crate::remote_sync::mirror_identity::identity(&root)
        );
        assert_ne!(record.root_identity, first.root_identity);
    }

    /// A record an earlier build wrote with sessions and no folder: the person says whether this is still the copy.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_mirror_set_up_without_a_folder_identity_is_confirmed_by_the_person() {
        for confirmed in [true, false] {
            let scratch = ScratchDir::new("remote", "unconfirmed");
            let root = scratch.join("project");
            let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
            let mut reach = fixture(&scratch, true);
            let spec = format!("example-host:{}", root.display());
            let mut term = Scripted {
                answers: [true].into(),
                ..Default::default()
            };
            assert_eq!(
                run_with(&store, &mut reach, &mut term, &["open", &spec]),
                0,
                "{:?}",
                term.said
            );
            std::fs::write(root.join("work"), b"synced").unwrap();
            // The fake engine brings no history: an empty replica would stop the listing before the question.
            std::fs::remove_dir_all(root.join(".git")).unwrap();
            store
                .update(&root, |project| {
                    let mut record = project.sync_record().unwrap().unwrap();
                    record.root_identity = None;
                    project.set_sync_record(&record);
                })
                .unwrap();
            let mut term = Scripted {
                answers: [confirmed, false].into(),
                ..Default::default()
            };
            let code = run_with(&store, &mut reach, &mut term, &["open", &spec]);
            assert!(
                term.asked[0].contains("did not note which folder it synced"),
                "{:?}",
                term.asked
            );
            let record = store.find(&root).unwrap().unwrap().sync_record().unwrap().unwrap();
            if confirmed {
                assert_eq!(code, 0, "{:?}", term.said);
                assert_eq!(term.asked.len(), 1);
                assert_eq!(
                    record.root_identity,
                    crate::remote_sync::mirror_identity::identity(&root)
                );
            } else {
                // No: the move question, declined, changes nothing.
                assert_eq!(code, 1);
                assert!(term.asked[1].starts_with("Move "), "{:?}", term.asked);
                assert_eq!(record.root_identity, None);
                assert!(record.has_sessions());
            }
            assert_eq!(std::fs::read(root.join("work")).unwrap(), b"synced");
        }
    }

    /// A folder inside another recorded mirror, or holding one, is refused before any connection or question.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_folder_nested_with_another_mirror_is_refused_before_anything() {
        for (recorded, opened) in [("x/a", "x/a/b"), ("x/a", "x")] {
            let scratch = ScratchDir::new("remote", "nested");
            let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
            let mut other = project_record("old");
            other.host = "host-a".into();
            other.root = scratch.join(recorded);
            other.set_sync_record(&crate::remote_project::SyncRecord {
                engine: "mutagen".into(),
                set_up: true,
                host_key_sha256: other.host_key_sha256.clone(),
                ..Default::default()
            });
            store.save(&other).unwrap();
            let opened = scratch.join(opened);
            let mut reach = fixture(&scratch, true);
            let mut term = Scripted::default();
            let spec = format!("example-host:{}", opened.display());
            assert_eq!(run_with(&store, &mut reach, &mut term, &["open", &spec]), 1);
            let said = term.said.last().unwrap();
            assert!(said.contains(" is inside ") && said.contains("host-a"), "{said}");
            assert!(term.asked.is_empty());
            assert_eq!(reach.connects, 0);
            assert!(store.find(&opened).unwrap().is_none());
            assert!(!opened.exists());
        }
    }

    /// A first connect that stops before sync is set up takes back the record it made, so the person's own folder
    /// does not read as a remote project; a record that was there before stays.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_declined_move_leaves_no_record_behind_and_an_earlier_one_untouched() {
        for earlier in [false, true] {
            let scratch = ScratchDir::new("remote", "declined");
            let root = scratch.join("project");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("mine"), b"local work").unwrap();
            let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
            if earlier {
                let mut old = project_record("old");
                old.root = root.clone();
                store.save(&old).unwrap();
            }
            let mut reach = fixture(&scratch, true);
            let mut term = Scripted {
                answers: [true, false].into(),
                ..Default::default()
            };
            let spec = format!("example-host:{}", root.display());
            assert_eq!(run_with(&store, &mut reach, &mut term, &["open", &spec]), 1);
            assert!(term.asked.last().unwrap().starts_with("Move "), "{:?}", term.asked);
            assert_eq!(store.find(&root).unwrap().is_some(), earlier);
            assert_eq!(std::fs::read(root.join("mine")).unwrap(), b"local work");
            assert!(reach.engine.calls("create").is_empty());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mirror_of_another_host_is_moved_aside_and_old_sessions_terminated() {
        use crate::remote_sync::{
            engine::{session_name, Role, SyncState},
            SyncKey,
        };
        let scratch = ScratchDir::new("remote", "another-mirror");
        let root = scratch.join("project");
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut reach = fixture(&scratch, true);
        let key = SyncKey::new("SHA256:old", &root);
        let name = session_name(&key, Role::Tree);
        reach.engine.insert(&name, Role::Tree, &root, SyncState::Watching);
        let mut old = project_record("old");
        old.host = "other-host".into();
        old.host_key_sha256 = "SHA256:old".into();
        old.root = root.clone();
        old.set_sync_record(&crate::remote_project::SyncRecord {
            engine: "mutagen".into(),
            host_key_sha256: old.host_key_sha256.clone(),
            tree_session: Some(name.clone()),
            ..Default::default()
        });
        store.save(&old).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("work"), b"original").unwrap();
        let mut term = Scripted {
            answers: [true, true, true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(
                &store,
                &mut reach,
                &mut term,
                &["open", &format!("example-host:{}", root.display())]
            ),
            0,
            "{:?}",
            term.said
        );
        let stopped = term
            .said
            .iter()
            .position(|l| l.contains(&format!("stopped sync session {name}")))
            .unwrap();
        let saved = term
            .said
            .iter()
            .position(|l| l.starts_with("Kept the existing copy at "))
            .unwrap();
        assert!(stopped < saved);
        let copy = Path::new(term.said[saved].strip_prefix("Kept the existing copy at ").unwrap());
        assert_eq!(std::fs::read(copy.join("work")).unwrap(), b"original");
        assert!(!root.join("work").exists());
        assert_eq!(reach.engine.calls("terminate"), [format!("terminate {name}")]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn moved_copy_is_never_deleted() {
        use crate::remote_sync::{
            engine::{session_name, Role, Side, Staging, SyncState},
            SyncKey,
        };
        let scratch = ScratchDir::new("remote", "first-progress");
        let root = scratch.join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("local-notes"), b"keep this").unwrap();
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let mut reach = fixture(&scratch, true);
        let key = SyncKey::new("SHA256:example", &root);
        let tree = session_name(&key, Role::Tree);
        let git = session_name(&key, Role::Git);
        let engine = reach.engine.clone();
        let lock_path = actor::lock_dir(&scratch, &root).join("lock");
        reach.helper.truncate_sizes();
        let start = Instant::now();
        let mut term = Scripted {
            answers: [true, true].into(),
            wait_for_sync: true,
            on_say: Some(Box::new(move |line| {
                assert!(start.elapsed() < Duration::from_secs(30), "stuck at {line}");
                if line.contains(" bytes counted") || line.starts_with("Kept the existing copy at ") {
                    let lock = agent::private_fs::PrivateLock::open(&lock_path).unwrap();
                    assert!(!lock.try_lock().unwrap(), "{line}");
                }
                if line.starts_with("syncing 0 files") {
                    crate::remote_sync::actor::tests::wait(|| engine.status_of(&git).is_some());
                    for name in [&tree, &git] {
                        let mut status = engine.status_of(name).unwrap();
                        status.state = SyncState::Staging;
                        status.staging = Some(Staging {
                            side: Side::Laptop,
                            expected_bytes: 80 << 20,
                            received_bytes: 75 << 20,
                            total_received_bytes: 75 << 20,
                            received_files: 2,
                            expected_files: 4,
                            path: None,
                        });
                        engine.push(status);
                    }
                }
                if line.starts_with("syncing 2 files") || line == "history arriving: 50 %" {
                    let name = if line.starts_with("syncing") { &tree } else { &git };
                    let mut status = engine.status_of(name).unwrap();
                    status.state = SyncState::Watching;
                    status.staging = None;
                    status.successful_cycles += 1;
                    engine.push(status);
                }
            })),
            ..Default::default()
        };
        assert_eq!(
            run_with(
                &store,
                &mut reach,
                &mut term,
                &["open", &format!("example-host:{}", root.display())]
            ),
            0,
            "{:?}",
            term.said
        );
        let backup = term
            .said
            .iter()
            .find_map(|line| line.strip_prefix("Kept the existing copy at "))
            .unwrap();
        assert_eq!(
            std::fs::read(Path::new(backup).join("local-notes")).unwrap(),
            b"keep this"
        );
        assert!(!root.join("local-notes").exists());
        let index = |prefix| term.said.iter().position(|s| s.starts_with(prefix)).unwrap();
        assert!(index("expected size: at least 80 MiB") < index("syncing 2 files, 75 / at least 80 MiB, "));
        assert!(index("syncing 2 files") < index("history arriving: 50 %"));
        assert_eq!(term.said.last().unwrap(), "sync continues when you open the window");
        assert!(store
            .find(&root)
            .unwrap()
            .unwrap()
            .sync_record()
            .unwrap()
            .unwrap()
            .paused
            .is_none());
        assert!(reach.engine.calls("pause").len() >= 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn first_sync_releases_sessions_and_lock_on_terminal_signals() {
        const CHILD: &str = "EITRI_TEST_FIRST_SYNC_SIGNAL";
        let Some(signal) = std::env::var_os(CHILD) else {
            for signal in [libc::SIGINT, libc::SIGTERM] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "remote_host::cli::tests::first_sync_releases_sessions_and_lock_on_terminal_signals",
                    ])
                    .env(CHILD, signal.to_string())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        let signal: i32 = signal.to_str().unwrap().parse().unwrap();
        let scratch = ScratchDir::new("remote", "first-sync-signal");
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let root = scratch.join("project");
        let mut reach = fixture(&scratch, true);
        let engine = reach.engine.clone();
        let mut term = Scripted {
            answers: [true, true].into(),
            wait_for_sync: true,
            on_say: Some(Box::new(move |line| {
                if line.starts_with("syncing 0 files") {
                    crate::remote_sync::actor::tests::wait(|| engine.calls("create").len() >= 2);
                    // SAFETY: the CLI installed its handlers in this isolated test process.
                    assert_eq!(unsafe { libc::raise(signal) }, 0);
                    // A second interrupt during slow cleanup must use the default disposition.
                    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                    assert_eq!(unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) }, 0);
                    assert_eq!(action.sa_sigaction, libc::SIG_DFL);
                }
            })),
            ..Default::default()
        };
        let args = ["open".into(), format!("example-host:{}", root.display())];
        assert_eq!(
            run(
                &args,
                &mut Context {
                    store: &store,
                    reach: &mut reach,
                    term: &mut term,
                    remote_dev_enabled: &|| true,
                    now: 1
                }
            ),
            1
        );
        assert!(
            term.said.last().unwrap().contains("first connection cancelled"),
            "{:?}",
            term.said
        );
        let sync = store.find(&root).unwrap().unwrap().sync_record().unwrap().unwrap();
        for name in [sync.tree_session.unwrap(), sync.git_session.unwrap()] {
            assert_eq!(
                reach.engine.status_of(&name).unwrap().state,
                crate::remote_sync::engine::SyncState::Paused
            );
        }
        let lock = agent::private_fs::PrivateLock::open(&actor::lock_dir(&scratch, &root).join("lock")).unwrap();
        assert!(lock.try_lock().unwrap());
    }

    fn run_with(store: &RemoteProjects, reach: &mut dyn Reach, term: &mut Scripted, args: &[&str]) -> i32 {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let enabled = || true;
        run_command(
            &args,
            &mut Context {
                store,
                reach,
                term,
                remote_dev_enabled: &enabled,
                now: 1_700_000_000,
            },
        )
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_sync_and_git_verbs_need_a_verb_and_never_connect() {
        let root = ScratchDir::new("remote", "sync-cli");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut reach = fixture(&root, true);
        let mut term = Scripted::default();
        for args in [
            vec!["sync"],
            vec!["sync", "nonsense"],
            vec!["git"],
            vec!["git", "status"],
        ] {
            assert_eq!(run_with(&store, &mut reach, &mut term, &args), 64);
        }
        assert_eq!(reach.connects, 0);
        assert!(term.asked.is_empty());
        assert!(term.said.iter().any(|line| line.starts_with("usage: eitri host sync")));
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn idle_recorded_installations_are_never_started_by_stop_or_uninstall() {
        for installation in ["99.0.0-abcdef0", "0.0.1-abcdef0"] {
            for command in ["stop", "uninstall"] {
                let root = ScratchDir::new("remote", "idle-recovery");
                let store = RemoteProjects::new(Some(root.as_os_str()), None);
                let data = root.join("data/eitri-server");
                let state = root.join("state/eitri-server");
                let helper = data.join(installation).join("bin/eitri-server");
                std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
                std::fs::write(&helper, format!(
                    "#!/bin/sh\ncase \"$1\" in\nensure) touch '{started}';;\nlist-daemons|check-sidecars) exit 0;;\n*) exit 64;;\nesac\n",
                    started = root.join("started").display(),
                )).unwrap();
                std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
                std::fs::write(data.join(installation).join("last-used"), b"unchanged").unwrap();
                store
                    .save_host(&RemoteHost {
                        version: 1,
                        host_key_sha256: "SHA256:example".into(),
                        data_dir: data.clone(),
                        state_dir: state.clone(),
                        helpers: [(installation.into(), super::super::manifest::hash_file(&helper).unwrap())].into(),
                        sidecars: Default::default(),
                        consented: true,
                    })
                    .unwrap();
                store.save(&project_record(installation)).unwrap();
                let mut reach = fixture(&root, true);
                reach.launcher = Arc::new(agent::ssh::LocalExec { helper });
                let mut term = Scripted::default();
                assert_eq!(
                    run_with(&store, &mut reach, &mut term, &[command, "example-host"]),
                    0,
                    "{:?}",
                    term.said
                );
                assert!(
                    !root.join("started").exists(),
                    "{command} must leave {installation} idle"
                );
                if command == "stop" {
                    assert_eq!(term.said, ["no Eitri server runs on example-host"]);
                    assert_eq!(
                        std::fs::read(data.join(installation).join("last-used")).unwrap(),
                        b"unchanged"
                    );
                } else {
                    assert!(!data.exists());
                }
            }
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn skipped_survivors_are_named_and_keep_inventory_diagnostics() {
        for mode in ["newer", "unrecorded", "legacy", "stale-identity", "running"] {
            let root = ScratchDir::new("remote", "skipped-survivors");
            let store = RemoteProjects::new(Some(root.as_os_str()), None);
            let installation = if mode == "newer" || mode == "stale-identity" {
                "99.0.0-abcdef0"
            } else {
                "0.0.1-abcdef0"
            };
            let owner = if mode == "unrecorded" {
                "0.0.1-fedcba0"
            } else {
                installation
            };
            let key = if mode == "legacy" {
                "0.0.1".into()
            } else {
                installation_run_key(owner)
            };
            let version = if mode == "newer" { "99.0.0" } else { "0.0.1" };
            let data = root.join("data/eitri-server");
            let state = root.join("state/eitri-server");
            let helper = data.join(installation).join("bin/eitri-server");
            std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
            std::fs::create_dir_all(state.join("run").join(&key)).unwrap();
            std::fs::write(
                state.join("run").join(&key).join("sc-0123456789abcdef.pid"),
                b"survivor",
            )
            .unwrap();
            let daemon = serde_json::json!({"version":version,"rev":"abcdef0","pid":42,
                "protocol_major":agent::ssh::PROTOCOL_MAJOR,"protocol_minor":0,"started_unix_ms":0});
            let mut listed = daemon.clone();
            listed["run"] = key.clone().into();
            let list = if mode == "running" {
                format!("echo '{listed}'")
            } else {
                "exit 0".into()
            };
            let live = serde_json::json!({"run":key,"daemon":daemon,"installation":owner});
            let diagnostic = format!("run {key} sidecar 0123456789abcdef pid 123 alive");
            std::fs::write(&helper, format!(
                "#!/bin/sh\ncase \"$1\" in\nensure) touch '{started}';;\nlist-daemons) {list};;\ncheck-sidecars) echo '{live}'; echo '{diagnostic}' >&2;;\nstop-daemon) touch '{stopped}';;\n*) exit 64;;\nesac\n",
                started=root.join("started").display(), stopped=root.join("stopped").display(),
            )).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
            store
                .save_host(&RemoteHost {
                    version: 1,
                    host_key_sha256: "SHA256:example".into(),
                    data_dir: data,
                    state_dir: state,
                    helpers: [(installation.into(), super::super::manifest::hash_file(&helper).unwrap())].into(),
                    sidecars: Default::default(),
                    consented: true,
                })
                .unwrap();
            store.save(&project_record(installation)).unwrap();
            let mut reach = fixture(&root, true);
            reach.launcher = Arc::new(agent::ssh::LocalExec { helper });
            let mut term = Scripted::default();
            assert_eq!(
                run_with(&store, &mut reach, &mut term, &["stop", "example-host"]),
                i32::from(mode != "running"),
                "{mode}: {:?}",
                term.said
            );
            assert!(!root.join("started").exists(), "{mode}");
            if mode == "running" {
                assert!(root.join("stopped").exists());
                continue;
            }
            assert!(
                term.said.iter().any(|line| line.contains(&key)
                    && line.contains("Eitri that installed it")
                    && line.contains("kill <pid>")),
                "{mode}: {:?}",
                term.said
            );
            assert!(
                term.said.iter().any(|line| line.contains(&diagnostic)),
                "{mode}: {:?}",
                term.said
            );
            assert!(
                !term.said.iter().any(|line| line.contains("no Eitri server runs")),
                "{mode}: {:?}",
                term.said
            );
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_errors_survive_a_later_daemon_list_failure() {
        let root = ScratchDir::new("remote", "recovery-list-error");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, state) = installed_host(&root, &store);
        let helper = data.join("0.3.0-current/bin/eitri-server");
        std::fs::write(&helper, "#!/bin/sh\ncase \"$1\" in\nlist-daemons) echo listing-error >&2;;\ncheck-sidecars|ensure) echo recovery-error >&2;;\nesac\nexit 1\n").unwrap();
        std::fs::write(state.join("run/0.2.0/sc-0123456789abcdef.pid"), b"survivor").unwrap();
        let mut host = store.host("SHA256:example").unwrap().unwrap();
        host.helpers.insert(
            "0.3.0-current".into(),
            super::super::manifest::hash_file(&helper).unwrap(),
        );
        store.save_host(&host).unwrap();
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec { helper });
        let mut term = Scripted::default();
        assert_eq!(run_with(&store, &mut reach, &mut term, &["stop", "example-host"]), 1);
        // Each failure is a line of its own, not joined to the next by a line break inside one line.
        assert!(
            !term
                .said
                .iter()
                .any(|line| line.contains("recovery-error") && line.contains("listing-error")),
            "{:?}",
            term.said
        );
        for error in ["recovery-error", "listing-error"] {
            assert!(
                term.said
                    .iter()
                    .any(|line| line.starts_with("eitri host: ") && line.contains(error)),
                "{:?}",
                term.said
            );
        }
    }

    #[test]
    fn an_empty_host_list_explains_crash_recovery_without_starting() {
        let root = ScratchDir::new("remote", "list-recovery-hint");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        installed_host(&root, &store);
        let helper = root.join("empty-list");
        std::fs::write(&helper, "#!/bin/sh\ntest \"$1\" = list-daemons\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec { helper });
        let mut term = Scripted::default();
        assert_eq!(run_with(&store, &mut reach, &mut term, &["list", "example-host"]), 0);
        assert!(
            term.said
                .iter()
                .any(|line| line.contains("eitri host stop example-host")),
            "{:?}",
            term.said
        );
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_checks_only_recorded_hashes_and_closes_the_child_lock() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for mode in ["unrecorded", "tampered", "checked"] {
            let root = ScratchDir::new("remote", "uninstall-checker");
            let data = root.join("data/eitri-server");
            let state = root.join("state/eitri-server");
            let helper = data.join("0.0.1-abcdef0/bin/eitri-server");
            std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
            std::fs::create_dir_all(state.join("run/orphan")).unwrap();
            std::fs::write(state.join("run/orphan/sc-0123456789abcdef.pid"), b"retained").unwrap();
            std::fs::write(&helper, format!(
                "#!/bin/sh\ntest \"$1\" = check-sidecars || exit 64\nif test -e /proc/self/fd/9; then echo inherited-lock >&2; exit 1; fi\ntouch '{}'\n",
                root.join("executed").display(),
            )).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
            let hash = super::super::manifest::hash_file(&helper).unwrap();
            if mode == "unrecorded" {
                std::fs::write(
                    &helper,
                    format!("#!/bin/sh\ntouch '{}'\n", root.join("executed").display()),
                )
                .unwrap();
            }
            if mode == "tampered" {
                std::fs::write(
                    &helper,
                    format!("#!/bin/sh\ntouch '{}'\n", root.join("executed").display()),
                )
                .unwrap();
            }
            let mut child = Command::new("sh")
                .args(["-c", super::super::steps::UNINSTALL, "checker-test"])
                .arg(&data)
                .arg(&state)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            if mode != "unrecorded" {
                writeln!(child.stdin.take().unwrap(), "0.0.1-abcdef0 {hash}").unwrap();
            } else {
                drop(child.stdin.take());
            }
            let output = child.wait_with_output().unwrap();
            if mode == "checked" {
                assert!(output.status.success(), "{:?}", output);
                assert!(root.join("executed").exists());
            } else {
                assert_eq!(output.status.code(), Some(76), "{mode}: {:?}", output);
                assert!(!root.join("executed").exists(), "{mode} helper executed");
                assert!(state.exists() && data.exists());
                assert!(String::from_utf8_lossy(&output.stderr).contains("no checked sidecar checker installed"));
            }
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn crashed_daemons_are_recovered_before_stop_and_uninstall() {
        crashed_recovery("inventory", agent::ssh::PROTOCOL_MAJOR);
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn unavailable_inventory_still_recovers_and_stops_orphans() {
        for mode in ["reject-flag", "inventory-failure"] {
            crashed_recovery(mode, agent::ssh::PROTOCOL_MAJOR);
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn older_other_protocol_installations_are_recovered_and_stopped() {
        crashed_recovery("inventory", agent::ssh::PROTOCOL_MAJOR + 1);
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn inventory_refusal_and_held_daemon_lock_never_start_recovery() {
        for mode in ["refusal", "held-lock"] {
            crashed_recovery(mode, agent::ssh::PROTOCOL_MAJOR);
        }
    }

    #[cfg(target_os = "linux")]
    fn crashed_recovery(mode: &str, major: u32) {
        use std::process::{Command, Stdio};
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for command in ["stop", "uninstall"] {
            if mode == "held-lock" && command == "uninstall" {
                continue;
            }
            let root = ScratchDir::new("remote", "crashed-stop");
            let store = RemoteProjects::new(Some(root.as_os_str()), None);
            let mut sidecar = ChildGuard(Command::new("sleep").arg("3600").stdin(Stdio::null()).spawn().unwrap());
            let mut daemon = ChildGuard(Command::new("sleep").arg("3600").spawn().unwrap());
            daemon.0.kill().unwrap();
            daemon.0.wait().unwrap();
            let data = root.join("data/eitri-server");
            let state = root.join("state/eitri-server");
            let installation = "0.0.1-abcdef0";
            let helper = data.join(installation).join("bin/eitri-server");
            std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
            let run_key = installation_run_key(installation);
            let record = state.join("run").join(&run_key).join("sc-0123456789abcdef.pid");
            std::fs::create_dir_all(record.parent().unwrap()).unwrap();
            std::fs::write(&record, b"surviving agent record").unwrap();
            let _live_lock = if mode == "held-lock" {
                use std::os::fd::AsRawFd;
                let lock = std::fs::File::create(record.parent().unwrap().join("live.lock")).unwrap();
                // SAFETY: the descriptor stays open until this iteration ends.
                assert_eq!(
                    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                Some(lock)
            } else {
                None
            };
            let info = serde_json::json!({"version":"0.0.1", "rev":"abcdef0", "pid":42,
                "protocol_major":major,"protocol_minor":0,"started_unix_ms":0,"run":run_key});
            let inventory = serde_json::json!({"run":run_key,"daemon":info,"installation":installation});
            let inventory_command = match mode {
                "reject-flag" => "echo 'unknown argument --live-runs' >&2; exit 1".into(),
                "inventory-failure" | "held-lock" => "echo 'inventory failed' >&2; exit 1".into(),
                "refusal" => "echo 'recorded sidecars may still run' >&2; exit 76".into(),
                _ => format!("echo '{inventory}'"),
            };
            std::fs::write(&helper, format!(
                "#!/bin/sh\ncase \"$1\" in\nensure) touch '{adopted}';;\nlist-daemons) test -e '{adopted}' || exit 0; test -e '{stopped}' && exit 0; echo '{info}';;\nstop-daemon) kill {pid}; touch '{stopped}';;\ncheck-sidecars) if test \"${{3:-}}\" = --live-runs; then {inventory_command}; else test -e '{stopped}'; fi;;\n*) exit 64;;\nesac\n",
                adopted=root.join("adopted").display(), stopped=root.join("stopped").display(), pid=sidecar.0.id()
            )).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
            store
                .save_host(&RemoteHost {
                    version: 1,
                    host_key_sha256: "SHA256:example".into(),
                    data_dir: data.clone(),
                    state_dir: state.clone(),
                    helpers: [(installation.into(), super::super::manifest::hash_file(&helper).unwrap())].into(),
                    sidecars: Default::default(),
                    consented: true,
                })
                .unwrap();
            store.save(&project_record(installation)).unwrap();
            let mut reach = fixture(&root, true);
            reach.launcher = Arc::new(agent::ssh::LocalExec { helper });
            let mut term = Scripted::default();
            if mode == "refusal" || mode == "held-lock" {
                assert_eq!(
                    run_with(&store, &mut reach, &mut term, &[command, "example-host"]),
                    1,
                    "{:?}",
                    term.said
                );
                assert!(!root.join("adopted").exists() && !root.join("stopped").exists());
                assert!(record.exists() && data.exists());
                assert!(sidecar.0.try_wait().unwrap().is_none());
                assert!(
                    !term.said.iter().any(|line| line.contains("no Eitri server runs")),
                    "{:?}",
                    term.said
                );
                if mode == "refusal" {
                    assert!(!term
                        .said
                        .iter()
                        .any(|line| line.contains("inventory was not available")));
                }
                continue;
            }
            assert_eq!(
                run_with(&store, &mut reach, &mut term, &[command, "example-host"]),
                0,
                "{:?}",
                term.said
            );
            assert!(
                root.join("adopted").exists(),
                "checked START must recover the dead daemon's agents"
            );
            let deadline = Instant::now() + Duration::from_secs(60);
            while sidecar.0.try_wait().unwrap().is_none() {
                assert!(Instant::now() < deadline, "the surviving sidecar was not stopped");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                term.said
                    .iter()
                    .any(|line| line.contains("recovered and stopped Eitri server 0.0.1")),
                "{:?}",
                term.said
            );
            assert_eq!(
                term.said
                    .iter()
                    .filter(|line| line.contains("inventory was not available")
                        && line.contains("idle installations may have been started and stopped"))
                    .count(),
                usize::from(mode != "inventory"),
                "{:?}",
                term.said
            );
            if command == "stop" {
                assert!(record.exists());
            } else {
                assert!(!data.exists() && !state.exists());
            }
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_preserves_sidecar_records_without_a_daemon_or_a_checking_helper() {
        let root = ScratchDir::new("remote", "surviving-sidecar");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, state) = installed_host(&root, &store);
        let record = state.join("run/0.2.0/sc-0123456789abcdef.pid");
        std::fs::write(&record, b"sidecar record must remain").unwrap();
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec {
            helper: daemon_helper(&root, "0.2.0"),
        });
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["uninstall", "example-host"]),
            1
        );
        assert!(record.exists() && data.exists());
        assert!(
            term.said.iter().any(|line| line.contains("kill <pid>")
                && line.contains("eitri host open example-host")
                && line.contains("Eitri that installed")),
            "{:?}",
            term.said
        );
        assert!(store.find(Path::new("/srv/project")).unwrap().is_some());
        assert!(
            term.said
                .iter()
                .any(|line| line.contains("stopped 1 daemon(s) and their agents; the helper was not removed")),
            "{:?}",
            term.said
        );
    }

    #[test]
    fn host_list_queries_each_running_installations_own_helper() {
        struct PerInstallation {
            fixture: Fixture,
            paths: Vec<PathBuf>,
            older: PathBuf,
        }
        impl Reach for PerInstallation {
            fn connect(&mut self, host: &str) -> Result<String, ConnectError> {
                self.fixture.connect(host)
            }
            fn first_contact(&mut self, _: &str) -> Result<Contact, String> {
                unreachable!()
            }
            fn launcher(&self, helper: &Path) -> Arc<dyn ChannelLauncher> {
                self.fixture.launcher(helper)
            }
            fn plan(&mut self) -> Result<InstallPlan, String> {
                unreachable!()
            }
            fn agents(&mut self, _: &str, _: &str, helper: &Path) -> Result<Vec<Agent>, String> {
                self.paths.push(helper.into());
                Ok(if helper == self.older {
                    vec![Agent {
                        sidecar_id: "sc-older".into(),
                        root: "/srv/project".into(),
                        account: String::new(),
                        alive: true,
                        sessions: Vec::new(),
                    }]
                } else {
                    Vec::new()
                })
            }
        }
        let root = ScratchDir::new("remote", "list-installations");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, _) = installed_host(&root, &store);
        let older = "0.2.0-abcdef0";
        store
            .update_host("SHA256:example", |record| {
                let mut record = record.unwrap();
                record.helpers.insert(older.into(), "a".repeat(64));
                record
            })
            .unwrap();
        let listing = root.join("listing");
        let lines: String = [
            ("0.3.0", "i-fd2fee27987d879e"),
            ("0.2.0", "i-c93b55937b31aaef"),
        ].into_iter().map(|(version, run)| format!("echo '{{\"version\":\"{version}\",\"rev\":\"abcdef0\",\"pid\":42,\"protocol_major\":1,\"protocol_minor\":0,\"started_unix_ms\":0,\"run\":\"{run}\"}}'\n")).collect();
        std::fs::write(&listing, format!("#!/bin/sh\n{lines}")).unwrap();
        std::fs::set_permissions(&listing, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut fixture = fixture(&root, true);
        fixture.launcher = Arc::new(agent::ssh::LocalExec { helper: listing });
        let mut reach = PerInstallation {
            fixture,
            paths: Vec::new(),
            older: data.join(older).join("bin/eitri-server"),
        };
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["list", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert_eq!(reach.paths, [data.join("0.3.0-current/bin/eitri-server"), reach.older]);
        assert!(
            term.said
                .iter()
                .any(|line| line.contains("[0.2.0-abcdef0]") && line.contains("sc-older")),
            "{:?}",
            term.said
        );
        assert!(!term.said.iter().any(|line| line.contains("runs no agent")));
    }
    #[test]
    fn host_specs_split_at_the_path() {
        assert_eq!(
            parse_spec("example-host:/srv/project"),
            Ok(("example-host".into(), "/srv/project".into()))
        );
        assert_eq!(
            parse_spec("user@example-host:/srv/project"),
            Ok(("user@example-host".into(), "/srv/project".into()))
        );
        assert_eq!(
            parse_spec("ssh://user@example-host:2222:/srv/project"),
            Ok(("ssh://user@example-host:2222".into(), "/srv/project".into()))
        );
        for bad in [
            "example-host",
            "example-host:srv",
            "example-host:/srv/project/",
            "example-host:/srv/../etc",
            ":/srv/project",
            "-oProxyCommand=x:/srv",
        ] {
            assert!(parse_spec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_open_checks_upload_space_before_installation_consent() {
        let root = ScratchDir::new("remote", "host-open-space");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut reach = fixture(&root, true);
        reach.no_upload_space = true;
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", "example-host:/srv/project"]),
            1
        );
        assert!(
            term.asked.is_empty(),
            "an impossible upload asks no installation question"
        );
        assert!(term.said.iter().any(|line| line.contains("100 MiB")));
        assert_eq!(store.host("SHA256:example").unwrap(), None);
        assert_eq!(store.find(Path::new("/srv/project")).unwrap(), None);
    }

    /// The record is written only after both questions were answered yes: the host key ssh did not know, then the
    /// installation. A no to either writes no project record, installs nothing, and adds no key after a no to the
    /// first.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_open_writes_the_record_after_both_yeses() {
        let root = ScratchDir::new("remote", "host-open");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let project_path = root.join("project");
        let project = project_path.as_path();
        let spec = &format!("example-host:{}", project.display());

        let mut reach = fixture(&root, false);
        let mut term = Scripted {
            answers: [false].into(),
            ..Default::default()
        };
        assert_eq!(run_with(&store, &mut reach, &mut term, &["open", spec]), 1);
        assert_eq!(term.asked.len(), 1);
        assert!(term.asked[0].contains("SHA256:example") && term.asked[0].contains("ssh-ed25519"));
        assert!(!reach.known.get(), "a no adds no key");
        assert_eq!(store.find(project).unwrap(), None);

        let mut reach = fixture(&root, false);
        let mut term = Scripted {
            answers: [true, false].into(),
            ..Default::default()
        };
        assert_eq!(run_with(&store, &mut reach, &mut term, &["open", spec]), 1);
        assert_eq!(term.asked.len(), 2);
        assert!(term.asked[1].contains("[y/N]") && term.asked[1].contains("example-host"));
        assert_eq!(store.find(project).unwrap(), None);
        assert_eq!(store.host("SHA256:example").unwrap(), None);
        let (plan, _) = install::test_plan(&root, "open");
        assert!(!plan.install_path().exists(), "nothing installed before consent");

        let mut reach = fixture(&root, false);
        let mut term = Scripted {
            answers: [true, true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", spec]),
            0,
            "{:?}",
            term.said
        );
        assert_eq!(term.asked.len(), 2);
        let record = store.find(project).unwrap().unwrap();
        assert_eq!(record.host, "example-host");
        assert_eq!(record.host_key_sha256, "SHA256:example");
        assert_eq!(record.helper_version, plan.version());
        assert_eq!(record.created_at, 1_700_000_000);
        let host = store.host("SHA256:example").unwrap().unwrap();
        assert!(host.consented && installed(&host, &plan));
        assert!(plan.install_path().join("started").exists());
        assert!(term.said.iter().any(|line| line
            == &format!(
                "recorded {spec}; open it with eitri --ssh example-host {}",
                project.display()
            )));

        // Opened again: no question, the record keeps its creation time.
        let mut reach = fixture(&root, true);
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", spec]),
            0,
            "{:?}",
            term.said
        );
        assert!(term.asked.is_empty());
        assert_eq!(store.find(project).unwrap().unwrap().created_at, 1_700_000_000);

        let off = || false;
        let mut term = Scripted::default();
        let args = vec!["open".to_string(), spec.to_string()];
        let code = run(
            &args,
            &mut Context {
                store: &store,
                reach: &mut fixture(&root, true),
                term: &mut term,
                remote_dev_enabled: &off,
                now: 0,
            },
        );
        assert_eq!(code, 1);
        assert!(term.said[0].contains("remote_dev.enabled"), "{:?}", term.said);
    }

    /// `list` names the agents a host runs, which keep running whether or not a window shows them, and says which
    /// wait for an answer; `status` reads only this machine's records.
    #[test]
    fn host_list_shows_windowless_agents() {
        let agents = [
            Agent {
                sidecar_id: "sc-1".into(),
                root: "/srv/project".into(),
                account: String::new(),
                alive: true,
                sessions: vec![AgentSession {
                    session_id: "s-1".into(),
                    closed: false,
                    pending: 2,
                }],
            },
            Agent {
                sidecar_id: "sc-2".into(),
                root: "/srv/project".into(),
                account: "work".into(),
                alive: true,
                sessions: Vec::new(),
            },
            Agent {
                sidecar_id: "sc-3".into(),
                root: "/srv/project".into(),
                account: String::new(),
                alive: false,
                sessions: Vec::new(),
            },
        ];
        let lines = agent_lines("example-host", &agents);
        assert_eq!(
            lines,
            [
                "example-host runs 2 agent(s); they keep running with no window open",
                "  sc-1  /srv/project  the default account  waiting for an answer to 2 request(s)",
                "  sc-2  /srv/project  account work",
                "  1 exited agent(s) kept for their history",
            ]
        );
        assert_eq!(agent_lines("example-host", &[]), ["example-host runs no agent"]);

        let root = ScratchDir::new("remote", "host-list");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut term = Scripted::default();
        assert_eq!(run_with(&store, &mut fixture(&root, true), &mut term, &["status"]), 0);
        assert_eq!(term.said, ["no remote project is recorded on this machine"]);
        let (plan, _) = install::test_plan(&root, "open");
        let mut host = install::test_host_record(&plan);
        host.data_dir = root.join("share/eitri-server");
        store.save_host(&host).unwrap();
        store.save(&project_record(plan.version())).unwrap();
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec {
            helper: daemon_helper(&root, "0.3.0"),
        });
        reach.agents = agents.to_vec();
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["list", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert_eq!(
            term.said[0],
            "Eitri server 0.3.0 (123abcd) runs on example-host as pid 42"
        );
        assert_eq!(
            &term.said[1..],
            lines
                .iter()
                .map(|line| format!("[{}] {line}", plan.version()))
                .collect::<Vec<_>>()
                .as_slice()
        );
        let mut term = Scripted::default();
        let mut offline = fixture(&root, true);
        assert_eq!(
            run_with(&store, &mut offline, &mut term, &["status", "example-host"]),
            0
        );
        assert_eq!(term.said[0], "example-host:/srv/project");
        assert_eq!(offline.connects, 0, "status connects to nothing");
        assert_eq!(
            run_with(&store, &mut fixture(&root, true), &mut term, &["list", "other-host"]),
            1
        );
        assert_eq!(
            run_with(&store, &mut fixture(&root, true), &mut term, &["frobnicate"]),
            64
        );
    }

    fn project_record(helper_version: &str) -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: helper_version.into(),
            sync: None,
            created_at: 1,
            queued_stops: Vec::new(),
        }
    }

    /// A helper whose `list-daemons` reports one daemon of `version` until `<root>/stopped` exists.
    fn daemon_helper(root: &Path, version: &str) -> PathBuf {
        let path = root.join(format!("daemons-{version}"));
        let stopped = root.join("stopped");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ntest \"$1\" = list-daemons || exit 1\ntest -e '{}' && exit 0\nprintf '%s\\n' \
                 '{{\"version\":\"{version}\",\"rev\":\"123abcd\",\"pid\":42,\"protocol_major\":1,\
                 \"protocol_minor\":0,\"started_unix_ms\":0,\"incompatible_protocol\":false}}'\n",
                stopped.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// `stop` reaches a daemon of an older Eitri through the current installation's own `stop-daemon` (its hash
    /// checked first); `uninstall` stops it too, then removes the helper's directories and this machine's records.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_and_uninstall_reach_an_older_daemon() {
        let root = ScratchDir::new("remote", "host-stop");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let data = root.join("share/eitri-server");
        let state = root.join("state/eitri-server");
        std::fs::create_dir_all(state.join("run")).unwrap();
        let helper = data.join("0.3.0-current/bin/eitri-server");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\ntest \"$1\" = ensure && exit 0\ntest \"$1\" = stop-daemon && test \"$2\" = --args-hex || exit 1\nprintf '%s' \"$3\" > '{}'\n",
                root.join("stopped").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = RemoteHost {
            version: 1,
            host_key_sha256: "SHA256:example".into(),
            data_dir: data.clone(),
            state_dir: state.clone(),
            helpers: [(
                "0.3.0-current".to_string(),
                super::super::manifest::hash_file(&helper).unwrap(),
            )]
            .into(),
            sidecars: Default::default(),
            consented: true,
        };
        store.save_host(&host).unwrap();
        store.save(&project_record("0.3.0-current")).unwrap();
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec {
            helper: daemon_helper(&root, "0.2.0"),
        });

        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["stop", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        let older = agent::ssh::encode_args(&[std::ffi::OsStr::new("0.2.0")]);
        assert_eq!(std::fs::read_to_string(root.join("stopped")).unwrap(), older);
        assert!(term
            .said
            .contains(&"stopped Eitri server 0.2.0 on example-host, with its agents".to_string()));
        let mut term = Scripted::default();
        assert_eq!(run_with(&store, &mut reach, &mut term, &["stop", "example-host"]), 0);
        assert_eq!(term.said, ["no Eitri server runs on example-host"]);

        std::fs::remove_file(root.join("stopped")).unwrap();
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["uninstall", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert_eq!(std::fs::read_to_string(root.join("stopped")).unwrap(), older);
        assert!(!data.exists() && !state.exists());
        assert_eq!(store.host("SHA256:example").unwrap(), None);
        assert_eq!(store.find(Path::new("/srv/project")).unwrap(), None);

        // A newer Eitri's daemon is never stopped, and nothing is removed while it runs.
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::remove_file(root.join("stopped")).unwrap();
        store.save_host(&host).unwrap();
        store.save(&project_record("0.3.0-current")).unwrap();
        reach.launcher = Arc::new(agent::ssh::LocalExec {
            helper: daemon_helper(&root, "99.0.0"),
        });
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["uninstall", "example-host"]),
            1
        );
        assert!(!root.join("stopped").exists());
        assert!(store.host("SHA256:example").unwrap().is_some());
        assert!(
            term.said.iter().any(|line| line.contains("newer Eitri server 99.0.0")),
            "{:?}",
            term.said
        );
        // `stop` says so in its exit code too: a script can tell the newer daemon was left running.
        let mut term = Scripted::default();
        assert_eq!(run_with(&store, &mut reach, &mut term, &["stop", "example-host"]), 1);
        assert!(!root.join("stopped").exists());
    }

    /// A helper whose `list-daemons` reports one daemon of `version`, and goes on reporting it for two more lists
    /// after `<root>/stopped` appears, as a daemon stopping its agents after SIGTERM still shows.
    #[cfg(target_os = "linux")]
    fn lingering_daemon_helper(root: &Path, version: &str) -> PathBuf {
        let path = root.join(format!("lingering-{version}"));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ntest \"$1\" = list-daemons || exit 1\nif test -e '{stopped}'; then n=$(cat '{count}' 2>/dev/null \
                 || echo 0); test \"$n\" -ge 2 && exit 0; echo $((n + 1)) > '{count}'; fi\nprintf '%s\\n' \
                 '{{\"version\":\"{version}\",\"rev\":\"abcdef0\",\"pid\":42,\"protocol_major\":0,\
                 \"protocol_minor\":0,\"started_unix_ms\":0,\"incompatible_protocol\":false}}'\n",
                stopped = root.join("stopped").display(),
                count = root.join("lists-after-stop").display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// The installation `stop_and_uninstall_reach_an_older_daemon` sets up: the helper and state directories, a
    /// current helper whose `stop-daemon` writes `<root>/stopped`, and the host and project records.
    fn installed_host(root: &Path, store: &RemoteProjects) -> (PathBuf, PathBuf) {
        let data = root.join("share/eitri-server");
        let state = root.join("state/eitri-server");
        std::fs::create_dir_all(state.join("run/0.2.0")).unwrap();
        let helper = data.join("0.3.0-current/bin/eitri-server");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\ntest \"$1\" = ensure && exit 0\ntest \"$1\" = stop-daemon && test \"$2\" = --args-hex || exit 1\nprintf '%s' \"$3\" > '{}'\n",
                root.join("stopped").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        store
            .save_host(&RemoteHost {
                version: 1,
                host_key_sha256: "SHA256:example".into(),
                data_dir: data.clone(),
                state_dir: state.clone(),
                helpers: [(
                    "0.3.0-current".to_string(),
                    super::super::manifest::hash_file(&helper).unwrap(),
                )]
                .into(),
                sidecars: Default::default(),
                consented: true,
            })
            .unwrap();
        store.save(&project_record("0.3.0-current")).unwrap();
        (data, state)
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_keeps_the_restart_lock_until_its_delayed_attachment() {
        let root = ScratchDir::new("remote", "uninstall-attach");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, state) = installed_host(&root, &store);
        let launcher = Arc::new(super::super::tests::DelayedAttachment {
            local: agent::ssh::LocalExec {
                helper: root.join("no-such-helper"),
            },
            target: super::super::steps::UNINSTALL,
            root: root.to_path_buf(),
            held: Default::default(),
            refused: Default::default(),
        });
        let mut reach = fixture(&root, true);
        reach.launcher = launcher.clone();
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["uninstall", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert!(
            launcher.refused.load(std::sync::atomic::Ordering::SeqCst),
            "a master replacement must not pass an unattached UNINSTALL"
        );
        assert!(!launcher.held.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!data.exists() && !state.exists());
        assert!(store.host("SHA256:example").unwrap().is_none());
    }

    /// A daemon keeps its lifetime lock until it has stopped its agents and ended: `uninstall` waits for it after
    /// sending the stop, removes nothing while it is held, and refuses once its wait is over.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_never_removes_under_a_daemon_still_running() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = ScratchDir::new("remote", "host-uninstall-live");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, state) = installed_host(&root, &store);
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec {
            helper: lingering_daemon_helper(&root, "0.2.0"),
        });
        let live = agent::private_fs::PrivateLock::open(&state.join("run/0.2.0/live.lock")).unwrap();
        assert!(live.try_lock().unwrap());
        let whole_while_held = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(800));
                whole_while_held.store(data.exists() && state.exists(), Ordering::SeqCst);
                drop(live);
            });
            let mut term = Scripted::default();
            assert_eq!(
                run_with(&store, &mut reach, &mut term, &["uninstall", "example-host"]),
                0,
                "{:?}",
                term.said
            );
        });
        assert!(
            whole_while_held.load(Ordering::SeqCst),
            "nothing was removed while the lock was held"
        );
        assert!(root.join("stopped").exists(), "the daemon was told to stop");
        assert!(!data.exists() && !state.exists());
        assert_eq!(store.host("SHA256:example").unwrap(), None);

        // A daemon that never lets go: nothing is removed and nothing is forgotten.
        std::fs::remove_file(root.join("stopped")).unwrap();
        let (data, state) = installed_host(&root, &store);
        let held = agent::private_fs::PrivateLock::open(&state.join("run/0.2.0/live.lock")).unwrap();
        assert!(held.try_lock().unwrap());
        let mut term = Scripted::default();
        let enabled = || true;
        let mut cx = Context {
            store: &store,
            reach: &mut reach,
            term: &mut term,
            remote_dev_enabled: &enabled,
            now: 0,
        };
        let refused = uninstall(&mut cx, "example-host", Duration::from_millis(600)).unwrap_err();
        assert!(
            refused.contains("still runs")
                && refused.contains("stopped 1 daemon(s) and their agents; the helper was not removed"),
            "{refused}"
        );
        assert!(data.exists() && state.exists());
        assert!(store.host("SHA256:example").unwrap().is_some());
        assert!(store.find(Path::new("/srv/project")).unwrap().is_some());
        drop(held);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stop_and_uninstall_pause_aliases_of_the_reached_key_with_the_command_named() {
        for command in ["stop", "uninstall"] {
            let root = ScratchDir::new("remote", "pause-by-key");
            let store = RemoteProjects::new(Some(root.as_os_str()), None);
            installed_host(&root, &store);
            let mut project = project_record("0.3.0-current");
            project.host = "another-alias".into();
            project.set_sync_record(&crate::remote_project::SyncRecord {
                tree_session: Some("tree".into()),
                host_key_sha256: project.host_key_sha256.clone(),
                ..Default::default()
            });
            store.save(&project).unwrap();
            let mut reach = fixture(&root, true);
            // Leave the installation in place, so the pause can be inspected for both commands.
            reach.launcher = Arc::new(agent::ssh::LocalExec {
                helper: daemon_helper(&root, "99.0.0"),
            });
            let mut term = Scripted::default();
            run_with(&store, &mut reach, &mut term, &[command, "example-host"]);
            assert_eq!(reach.engine.calls("pause"), ["pause tree"]);
            let pause = store
                .find(&project.root)
                .unwrap()
                .unwrap()
                .sync_record()
                .unwrap()
                .unwrap()
                .paused
                .unwrap();
            assert!(
                pause.detail.contains(&format!("eitri host {command} example-host")),
                "{}",
                pause.detail
            );
        }
    }

    /// An installation removed already (from another machine, or by hand) leaves no helper to list the daemons:
    /// `uninstall` still removes what is left and forgets the records, every one under the host's key, whatever host
    /// name it was recorded with; a record under a key the host no longer shows does not stand in the way.
    #[test]
    fn uninstall_works_when_the_installation_is_gone() {
        let root = ScratchDir::new("remote", "host-uninstall-gone");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, state) = installed_host(&root, &store);
        std::fs::remove_dir_all(&data).unwrap();
        let mut aliased = project_record("0.3.0-current");
        aliased.host = "alias-host".into();
        aliased.root = "/srv/aliased".into();
        store.save(&aliased).unwrap();
        let mut stale = project_record("0.2.0-old");
        stale.host_key_sha256 = "SHA256:before".into();
        stale.root = "/srv/stale".into();
        stale.created_at = 99;
        store.save(&stale).unwrap();
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec {
            helper: root.join("no-such-helper"),
        });
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["uninstall", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert!(
            term.said.iter().any(|line| line.contains("lists its daemons")),
            "{:?}",
            term.said
        );
        assert!(!state.exists());
        assert_eq!(store.host("SHA256:example").unwrap(), None);
        assert_eq!(store.find(Path::new("/srv/project")).unwrap(), None);
        assert_eq!(store.find(Path::new("/srv/aliased")).unwrap(), None);
        assert!(
            store.find(Path::new("/srv/stale")).unwrap().is_some(),
            "another key's record stays"
        );
        assert!(
            term.said.last().unwrap().contains("forgot 2 project record(s)"),
            "{:?}",
            term.said
        );
    }

    /// The UNINSTALL step itself refuses while a daemon holds its lifetime lock, with the sentence `uninstall` waits on.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_uninstall_step_refuses_under_a_lifetime_lock() {
        let root = ScratchDir::new("remote", "host-uninstall-step");
        let data = root.join("share/eitri-server");
        let state = root.join("state/eitri-server");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(state.join("run/0.3.0")).unwrap();
        let launcher = agent::ssh::LocalExec {
            helper: root.join("fixture"),
        };
        let args = [data.to_str().unwrap(), state.to_str().unwrap()];
        let held = agent::private_fs::PrivateLock::open(&state.join("run/0.3.0/live.lock")).unwrap();
        assert!(held.try_lock().unwrap());
        let child = launcher
            .spawn_step(crate::remote_host::steps::UNINSTALL, &args)
            .unwrap();
        let refused = install::communicate(child, &[], &|_, _| {}).unwrap_err().to_string();
        assert!(refused.contains(DAEMON_STILL_RUNS), "{refused}");
        assert!(data.exists() && state.exists());
        drop(held);
        let child = launcher
            .spawn_step(crate::remote_host::steps::UNINSTALL, &args)
            .unwrap();
        install::communicate(child, &[], &|_, _| {}).unwrap();
        assert!(!data.exists() && !state.exists());
    }

    /// The same root recorded on another host is that host's record: `open` asks before replacing it, and a no
    /// changes nothing.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_asks_before_replacing_another_hosts_record() {
        let root = ScratchDir::new("remote", "host-open-other");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut other = project_record("0.3.0-other");
        let project = root.join("project");
        let spec = format!("example-host:{}", project.display());
        other.root = project.clone();
        other.host = "other-host".into();
        other.queued_stops = vec!["sc-9".into()];
        store.save(&other).unwrap();
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut fixture(&root, true), &mut term, &["open", &spec]),
            1
        );
        assert!(
            term.asked[0].contains("recorded as a project on other-host")
                && term.asked[0].contains("1 queued agent stop(s)"),
            "{:?}",
            term.asked
        );
        assert!(
            !term.said.iter().any(|line| line.contains("host key changed")),
            "{:?}",
            term.said
        );
        assert_eq!(store.find(&project).unwrap(), Some(other));
        let mut term = Scripted {
            answers: [true, true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut fixture(&root, true), &mut term, &["open", &spec]),
            0,
            "{:?}",
            term.said
        );
        let replaced = store.find(&project).unwrap().unwrap();
        assert_eq!(replaced.host, "example-host");
        assert!(replaced.queued_stops.is_empty());
    }

    /// A record of the same host under a key that changed keeps its queued stops and its creation time, and the
    /// sentence says what goes with it.
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_after_a_key_change_keeps_the_queued_stops() {
        let root = ScratchDir::new("remote", "host-open-rekey");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut before = project_record("0.3.0-old");
        let project = root.join("project");
        let spec = format!("example-host:{}", project.display());
        before.root = project.clone();
        before.host_key_sha256 = "SHA256:before".into();
        before.queued_stops = vec!["sc-1".into()];
        store.save(&before).unwrap();
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut fixture(&root, true), &mut term, &["open", &spec]),
            0,
            "{:?}",
            term.said
        );
        let record = store.find(&project).unwrap().unwrap();
        assert_eq!(record.host_key_sha256, "SHA256:example");
        assert_eq!(record.queued_stops, ["sc-1"]);
        assert_eq!(record.created_at, 1);
        assert_eq!(
            term.said
                .iter()
                .filter(|line| line.contains("host key changed"))
                .count(),
            1
        );
        assert!(
            term.said
                .iter()
                .any(|line| line.contains("host key changed") && line.contains("1 queued agent stop(s) go with it")),
            "{:?}",
            term.said
        );
    }
    struct Keyed {
        local: agent::ssh::LocalExec,
        key: Arc<std::sync::Mutex<String>>,
    }
    impl ChannelLauncher for Keyed {
        fn host_key(&self) -> Option<std::io::Result<String>> {
            Some(Ok(self.key.lock().unwrap().clone()))
        }
        fn spawn(&self, tail: &[OsString]) -> std::io::Result<std::process::Child> {
            self.local.spawn(tail)
        }
    }

    #[test]
    fn accepting_an_install_after_the_master_changes_key_writes_no_server_files() {
        let root = ScratchDir::new("remote", "consent-key-change");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut reach = fixture(&root, true);
        let key = Arc::new(std::sync::Mutex::new("SHA256:example".to_owned()));
        reach.launcher = Arc::new(Keyed {
            local: agent::ssh::LocalExec {
                helper: root.join("fixture"),
            },
            key: key.clone(),
        });
        let mut term = Scripted {
            answers: [true].into(),
            on_ask: Some(Box::new(move || *key.lock().unwrap() = "SHA256:another".into())),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", "example-host:/srv/project"]),
            1
        );
        assert!(!root.join("data").exists());
        assert!(store.find(Path::new("/srv/project")).unwrap().is_none());
        assert!(
            term.said.iter().any(|s| s.contains("refusing lifecycle command")),
            "{:?}",
            term.said
        );
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_commits_the_latest_stop_queue_and_moves_tabs_only_after_success() {
        use crate::saved_tabs::{self, SavedTabs};
        use crate::window_link::pair::Role;
        let root = ScratchDir::new("remote", "open-commit");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut before = project_record("0.2.0-old");
        let project = root.join("project");
        let spec = format!("example-host:{}", project.display());
        before.root = project.clone();
        before.host_key_sha256 = "SHA256:before".into();
        before.queued_stops = vec!["sc-old".into()];
        store.save(&before).unwrap();
        let base = store.saved_tabs_dir().unwrap();
        let old = base.join(crate::remote_project::origin_key("SHA256:before"));
        let new = base.join(crate::remote_project::origin_key("SHA256:example"));
        saved_tabs::save_as(&old, &before.root, Role::Main, &SavedTabs::default()).unwrap();
        let mut reach = fixture(&root, true);
        let mut no = Scripted::default();
        assert_eq!(run_with(&store, &mut reach, &mut no, &["open", &spec]), 1);
        assert!(matches!(
            saved_tabs::load(&old, &before.root),
            saved_tabs::Loaded::Saved(_)
        ));
        assert_eq!(saved_tabs::load(&new, &before.root), saved_tabs::Loaded::Missing);
        let during = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut yes = Scripted {
            answers: [true].into(),
            on_ask: Some(Box::new(move || {
                during.stop_sent(&project, "sc-old").unwrap();
                during.queue_stop(&project, "sc-new").unwrap();
            })),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut yes, &["open", &spec]),
            0,
            "{:?}",
            yes.said
        );
        assert_eq!(store.find(&before.root).unwrap().unwrap().queued_stops, ["sc-new"]);
        assert_eq!(saved_tabs::load(&old, &before.root), saved_tabs::Loaded::Missing);
        assert!(matches!(
            saved_tabs::load(&new, &before.root),
            saved_tabs::Loaded::Saved(_)
        ));
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn management_reaches_an_installation_with_no_successful_project_open() {
        let root = ScratchDir::new("remote", "host-no-project");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, state) = installed_host(&root, &store);
        store.forget(Path::new("/srv/project")).unwrap();
        let mut reach = fixture(&root, true);
        reach.launcher = Arc::new(agent::ssh::LocalExec {
            helper: daemon_helper(&root, "0.2.0"),
        });
        let mut term = Scripted::default();
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["stop", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert!(root.join("stopped").exists());
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["uninstall", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert!(!data.exists() && !state.exists());
        assert!(store.host("SHA256:example").unwrap().is_none());
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_removes_only_its_own_temporary_run_directories() {
        let root = ScratchDir::new("remote", "uninstall-sockets");
        let data = root.join("data/eitri-server");
        let state = root.join("state/eitri-server");
        let ours = format!("test-own-{}", uuid::Uuid::new_v4().simple());
        let theirs = format!("test-other-{}", uuid::Uuid::new_v4().simple());
        let temporary = std::env::temp_dir().join(format!("eitri-server-{}", agent::private_fs::euid()));
        let own_tmp = temporary.join(&ours);
        let other_tmp = temporary.join(&theirs);
        std::fs::create_dir_all(state.join("run").join(&ours)).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&own_tmp).unwrap();
        std::fs::create_dir_all(&other_tmp).unwrap();
        std::fs::write(other_tmp.join("kept"), "live socket stand-in").unwrap();
        let launcher = agent::ssh::LocalExec {
            helper: root.join("fixture"),
        };
        let child = launcher
            .spawn_step(
                crate::remote_host::steps::UNINSTALL,
                &[data.to_str().unwrap(), state.to_str().unwrap()],
            )
            .unwrap();
        install::communicate(child, &[], &|_, _| {}).unwrap();
        assert!(!own_tmp.exists());
        assert_eq!(
            std::fs::read_to_string(other_tmp.join("kept")).unwrap(),
            "live socket stand-in"
        );
        std::fs::remove_dir_all(other_tmp).unwrap();
    }
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_tries_remaining_daemons_after_one_refuses() {
        let root = ScratchDir::new("remote", "stop-remaining");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, _) = installed_host(&root, &store);
        let helper = data.join("0.3.0-current/bin/eitri-server");
        let bad = agent::ssh::encode_args(&[std::ffi::OsStr::new("bad")]);
        let good = agent::ssh::encode_args(&[std::ffi::OsStr::new("good")]);
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$3\" >> '{}'\ntest \"$3\" != '{bad}'\n",
                root.join("attempts").display()
            ),
        )
        .unwrap();
        let mut record = store.host("SHA256:example").unwrap().unwrap();
        record.helpers.insert(
            "0.3.0-current".into(),
            super::super::manifest::hash_file(&helper).unwrap(),
        );
        store.save_host(&record).unwrap();
        let mut reach = fixture(&root, true);
        let mut term = Scripted::default();
        let enabled = || true;
        let mut cx = Context {
            store: &store,
            reach: &mut reach,
            term: &mut term,
            remote_dev_enabled: &enabled,
            now: 0,
        };
        let reached = reach_host(&mut cx, "example-host").unwrap();
        let running = ["bad", "good"]
            .into_iter()
            .map(|run| Running {
                daemon: DaemonInfo {
                    version: "0.2.0".into(),
                    rev: "abcdef0".into(),
                    pid: 42,
                    protocol_major: 0,
                    protocol_minor: 0,
                    started_unix_ms: 0,
                },
                incompatible: false,
                run: run.into(),
            })
            .collect();
        let launcher = agent::ssh::LocalExec {
            helper: root.join("fixture"),
        };
        assert!(
            stop_running(&mut cx, "example-host", &reached, &launcher, running, &[], false)
                .unwrap_err()
                .contains("stopping Eitri")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("attempts")).unwrap(),
            format!("{bad}\n{good}\n")
        );
    }
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_start_commits_no_key_or_tab_move_and_its_installation_remains_manageable() {
        use crate::saved_tabs::{self, SavedTabs};
        let root = ScratchDir::new("remote", "open-start-fails");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut before = project_record("0.2.0-old");
        before.host_key_sha256 = "SHA256:before".into();
        store.save(&before).unwrap();
        let base = store.saved_tabs_dir().unwrap();
        let old = base.join(crate::remote_project::origin_key("SHA256:before"));
        let new = base.join(crate::remote_project::origin_key("SHA256:example"));
        saved_tabs::save(&old, &before.root, &SavedTabs::default()).unwrap();
        let mut reach = fixture(&root, true);
        let plan = install::test_plan(&root, "open").0;
        std::fs::create_dir_all(plan.install_path()).unwrap();
        std::fs::write(plan.install_path().join("ensure-fails"), "").unwrap();
        let mut term = Scripted {
            answers: [true].into(),
            ..Default::default()
        };
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["open", "example-host:/srv/project"]),
            1
        );
        assert_eq!(store.find(&before.root).unwrap(), Some(before.clone()));
        assert!(matches!(
            saved_tabs::load(&old, &before.root),
            saved_tabs::Loaded::Saved(_)
        ));
        assert_eq!(saved_tabs::load(&new, &before.root), saved_tabs::Loaded::Missing);
        assert!(store
            .host("SHA256:example")
            .unwrap()
            .unwrap()
            .helpers
            .contains_key(plan.version()));
        let enabled = || true;
        let mut cx = Context {
            store: &store,
            reach: &mut reach,
            term: &mut term,
            remote_dev_enabled: &enabled,
            now: 0,
        };
        assert_eq!(
            reach_host(&mut cx, "example-host").unwrap().installation,
            plan.version()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn terminal_records_are_cleared_only_after_host_stop_or_uninstall_succeeds() {
        for command in ["stop", "uninstall"] {
            for succeeds in [false, true] {
                let root = ScratchDir::new("remote", "terminal-host-clear");
                let store = RemoteProjects::new(Some(root.as_os_str()), None);
                installed_host(&root, &store);
                for project in ["/srv/project", "/srv/other"] {
                    store
                        .terminals()
                        .begin_open("SHA256:example", Path::new(project), 1, "term", "0.3.0-current", 42)
                        .unwrap();
                }
                store
                    .terminals()
                    .begin_open(
                        "SHA256:other",
                        Path::new("/srv/project"),
                        1,
                        "term",
                        "0.3.0-current",
                        42,
                    )
                    .unwrap();
                let mut reach = fixture(&root, true);
                reach.launcher = Arc::new(agent::ssh::LocalExec {
                    helper: daemon_helper(&root, if succeeds { "0.2.0" } else { "99.0.0" }),
                });
                let mut term = Scripted::default();
                let result = run_with(&store, &mut reach, &mut term, &[command, "example-host"]);
                assert_eq!(result, if succeeds { 0 } else { 1 }, "{:?}", term.said);
                for project in ["/srv/project", "/srv/other"] {
                    assert_eq!(
                        store
                            .terminals()
                            .record("SHA256:example", Path::new(project))
                            .unwrap()
                            .entries
                            .is_empty(),
                        succeeds
                    );
                }
                assert_eq!(
                    store
                        .terminals()
                        .record("SHA256:other", Path::new("/srv/project"))
                        .unwrap()
                        .entries
                        .len(),
                    1
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_list_reaches_terminal_inventory_and_status_reports_queued_stops_without_connecting() {
        struct Inventory {
            fixture: Fixture,
            helper: PathBuf,
            calls: usize,
        }
        impl Reach for Inventory {
            fn connect(&mut self, host: &str) -> Result<String, ConnectError> {
                self.fixture.connect(host)
            }
            fn first_contact(&mut self, _: &str) -> Result<Contact, String> {
                unreachable!()
            }
            fn launcher(&self, helper: &Path) -> Arc<dyn ChannelLauncher> {
                self.fixture.launcher(helper)
            }
            fn plan(&mut self) -> Result<InstallPlan, String> {
                unreachable!()
            }
            fn agents(&mut self, _: &str, _: &str, _: &Path) -> Result<Vec<Agent>, String> {
                Ok(Vec::new())
            }
            fn terminals(
                &mut self,
                host: &str,
                key: &str,
                helper: &Path,
            ) -> Result<wire::ListTerminalsResponse, String> {
                assert_eq!(
                    (host, key, helper),
                    ("example-host", "SHA256:example", self.helper.as_path())
                );
                self.calls += 1;
                Ok(wire::ListTerminalsResponse {
                    terminals: vec![wire::TerminalInfo {
                        terminal_id: "0123456789abcdef".into(),
                        cwd: "/srv/project\u{1b}[31m".into(),
                        foreground_job: Some(true),
                        foreground_name: "job\u{202e}".into(),
                        attached: true,
                        ..Default::default()
                    }],
                    truncated: true,
                })
            }
        }
        let root = ScratchDir::new("remote", "terminal-list");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let (data, _) = installed_host(&root, &store);
        let entry = store
            .terminals()
            .begin_open(
                "SHA256:example",
                Path::new("/srv/project"),
                1,
                "term",
                "0.3.0-current",
                42,
            )
            .unwrap();
        store
            .terminals()
            .queue_stop("SHA256:example", Path::new("/srv/project"), &entry.command_id)
            .unwrap();
        let listing = root.join("listing");
        let info = serde_json::json!({"version":"0.3.0", "rev":"123abcd", "pid":42, "protocol_major":1,
            "protocol_minor":1, "started_unix_ms":0, "run":installation_run_key("0.3.0-current")});
        std::fs::write(&listing, format!("#!/bin/sh\nprintf '%s\\n' '{info}'\n")).unwrap();
        std::fs::set_permissions(&listing, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut fixture = fixture(&root, true);
        fixture.launcher = Arc::new(agent::ssh::LocalExec { helper: listing });
        let mut reach = Inventory {
            fixture,
            helper: data.join("0.3.0-current/bin/eitri-server"),
            calls: 0,
        };
        let mut term = Scripted::default();
        assert_eq!(run_with(&store, &mut reach, &mut term, &["status", "example-host"]), 0);
        assert_eq!(reach.fixture.connects, 0);
        assert!(term
            .said
            .iter()
            .any(|line| line.contains("window 1") && line.ends_with("stop queued")));
        assert_eq!(
            run_with(&store, &mut reach, &mut term, &["list", "example-host"]),
            0,
            "{:?}",
            term.said
        );
        assert_eq!(reach.calls, 1);
        assert!(term
            .said
            .iter()
            .any(|line| line.contains("0123456789ab") && line.contains("attached") && line.contains("running job")));
        assert!(term.said.iter().any(|line| line.ends_with("terminal list truncated")));
        assert!(term
            .said
            .iter()
            .all(|line| !line.contains('\u{1b}') && !line.contains('\u{202e}')));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_reports_a_damaged_terminal_record_and_keeps_the_rest() {
        let root = ScratchDir::new("remote", "terminal-status-damaged");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let first = project_record("0.3.0-example");
        let mut second = first.clone();
        second.root = "/srv/other".into();
        for project in [&first, &second] {
            store.save(project).unwrap();
            store
                .terminals()
                .begin_open(
                    &project.host_key_sha256,
                    &project.root,
                    1,
                    "terminal",
                    "0.3.0-example",
                    42,
                )
                .unwrap();
        }
        let damaged = store.terminals().path(&first.host_key_sha256, &first.root).unwrap();
        agent::private_fs::write_private(&damaged, b"damaged").unwrap();
        let mut reach = fixture(&root, true);
        let mut term = Scripted::default();
        assert_eq!(run_with(&store, &mut reach, &mut term, &["status"]), 0);
        assert_eq!(reach.connects, 0);
        assert!(term
            .said
            .iter()
            .any(|line| line.contains("terminal record cannot be read")));
        assert!(term
            .said
            .iter()
            .any(|line| line.contains("terminal ") && line.ends_with("opening")));
        assert!(term.said.iter().any(|line| line == "example-host:/srv/other"));
        assert!(term.said.iter().any(|line| line == "example-host:/srv/project"));
    }
}
