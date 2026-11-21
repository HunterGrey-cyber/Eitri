//! Durable ownership of server terminals. Disk operations and control calls belong on workers.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent::remote_client::terminal_transport::{tokio, tonic, wire};
use agent::remote_client::HelperClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::terminal_link::TerminalTarget;
use super::{invalid, read, version, RemoteConnection, RemoteProject, RemoteProjects};
use crate::onboarding::display::display_text;
use crate::state_paths::{create_state_dir, lock_store, ON_A_WRITER_THREAD};

const STOP_BUDGET: Duration = Duration::from_secs(30);
const ENTRIES_MAX: usize = 256;
const OPEN_SETTLE_SECONDS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Opening,
    Open,
    StopQueued,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalEntry {
    pub slot: u8,
    pub module: String,
    pub state: TerminalState,
    pub command_id: String,
    #[serde(deserialize_with = "nullable")]
    pub terminal_id: Option<String>,
    pub installation: String,
    #[serde(deserialize_with = "nullable")]
    pub daemon_instance: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub boot_id: Option<String>,
    pub opened_at: u64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn nullable<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

impl TerminalEntry {
    /// An unanswered open is retried with its original command id. Queued stops are never attached.
    pub fn target(&self, cwd: &Path) -> io::Result<TerminalTarget> {
        match self.state {
            TerminalState::Opening => Ok(TerminalTarget::Open {
                command_id: self.command_id.clone(),
                cwd: cwd.to_path_buf(),
            }),
            TerminalState::Open => Ok(TerminalTarget::Attach {
                terminal_id: self.terminal_id.clone().ok_or(io::ErrorKind::InvalidData)?,
                daemon_instance: self.daemon_instance.clone().ok_or(io::ErrorKind::InvalidData)?,
                boot_id: self.boot_id.clone().ok_or(io::ErrorKind::InvalidData)?,
            }),
            TerminalState::StopQueued => Err(io::Error::other("this terminal is queued to stop")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRecord {
    pub version: u32,
    pub host_key: String,
    #[serde(default)]
    pub host: Option<String>,
    pub root: PathBuf,
    pub entries: Vec<TerminalEntry>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Where a new open's record key comes from: a live connection (its project's key and host as typed) or a saved key.
/// Neither supplies the daemon's generation at intent time: asking would cost a helper connection per open, and an
/// unanswered open is settled by its age instead; the open's answer fills the generation in `opened`.
pub enum OpenSource<'a> {
    Connection(&'a RemoteConnection),
    Key(&'a str),
}

impl<'a> From<&'a RemoteConnection> for OpenSource<'a> {
    fn from(connection: &'a RemoteConnection) -> Self {
        Self::Connection(connection)
    }
}

impl<'a> From<&'a str> for OpenSource<'a> {
    fn from(key: &'a str) -> Self {
        Self::Key(key)
    }
}

impl<'a> From<&'a String> for OpenSource<'a> {
    fn from(key: &'a String) -> Self {
        Self::Key(key)
    }
}

/// A store borrows the same state location as remote projects, without consulting the process environment.
#[derive(Clone, Copy)]
pub struct TerminalRecords<'a> {
    projects: &'a RemoteProjects,
}

impl RemoteProjects {
    pub fn terminals(&self) -> TerminalRecords<'_> {
        TerminalRecords { projects: self }
    }
}

impl TerminalRecords<'_> {
    fn dir(&self) -> io::Result<PathBuf> {
        Ok(self.projects.dir()?.join("terminals"))
    }

    pub fn path(&self, key: &str, root: &Path) -> io::Result<PathBuf> {
        self.projects.project_path(root)?;
        if key.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty host key"));
        }
        let mut hash = Sha256::new();
        hash.update(key.as_bytes());
        hash.update(b"\0");
        hash.update(root.as_os_str().as_bytes());
        Ok(self
            .dir()?
            .join(format!("{}.json", &format!("{:x}", hash.finalize())[..16])))
    }

    /// Held for its life by the one window that owns slot 1 of this project's server terminals, so another full
    /// window of the same project (a `--new-window` launch, or one that found the project path's pair lock held by
    /// a window of another origin) cannot restore, offer or stop that window's shells. `None` while another window
    /// holds it. Beside the record, under the record's name; record scans read only `.json` files.
    pub fn take_first_slot(&self, key: &str, root: &Path) -> io::Result<Option<agent::private_fs::PrivateLock>> {
        let record = self.path(key, root)?;
        create_state_dir(self.projects.dir()?)?;
        let dir = self.dir()?;
        create_state_dir(&dir)?;
        let lock = agent::private_fs::PrivateLock::open(&record.with_extension("slot1.lock"))?;
        Ok(lock.try_lock()?.then_some(lock))
    }

    // One lock also serializes key migration and host-wide removal with every entry update.
    fn lock(&self) -> io::Result<agent::private_fs::PrivateLock> {
        create_state_dir(self.projects.dir()?)?;
        let dir = self.dir()?;
        create_state_dir(&dir)?;
        lock_store(&dir.join("store"), ON_A_WRITER_THREAD)
    }

    fn load(&self, key: &str, root: &Path) -> io::Result<TerminalRecord> {
        let path = self.path(key, root)?;
        let record = read::<TerminalRecord>(&path)?.unwrap_or_else(|| TerminalRecord {
            version: 1,
            host_key: key.to_owned(),
            host: None,
            root: root.to_path_buf(),
            entries: Vec::new(),
            extra: BTreeMap::new(),
        });
        version(&path, record.version)?;
        if record.host_key != key || record.root != root {
            return Err(invalid(&path, "terminal record identity does not match its file name"));
        }
        let mut owners = BTreeSet::new();
        let mut commands = BTreeSet::new();
        for entry in &record.entries {
            if !matches!(entry.slot, 1 | 2)
                || entry.module.is_empty()
                || entry.command_id.is_empty()
                || entry.installation.is_empty()
                || (entry.state != TerminalState::StopQueued && !owners.insert((entry.slot, &entry.module)))
                || !commands.insert(&entry.command_id)
                || (entry.state == TerminalState::Open
                    && [
                        entry.terminal_id.as_deref(),
                        entry.daemon_instance.as_deref(),
                        entry.boot_id.as_deref(),
                    ]
                    .iter()
                    .any(|id| id.is_none_or(str::is_empty)))
            {
                return Err(invalid(&path, "invalid terminal ownership entry"));
            }
        }
        Ok(record)
    }

    fn write(&self, record: &TerminalRecord) -> io::Result<()> {
        let path = self.path(&record.host_key, &record.root)?;
        if serde_json::to_vec(record).map_err(io::Error::other)?.len() > 4 * 1024 * 1024 {
            return Err(invalid(&path, "terminal record exceeds its byte limit"));
        }
        self.projects.write(&path, record)
    }

    pub fn record(&self, key: &str, root: &Path) -> io::Result<TerminalRecord> {
        let _lock = self.lock()?;
        self.load(key, root)
    }

    fn update<T>(
        &self,
        key: &str,
        root: &Path,
        change: impl FnOnce(&mut TerminalRecord) -> io::Result<T>,
    ) -> io::Result<T> {
        let _lock = self.lock()?;
        let mut record = self.load(key, root)?;
        let before = record.clone();
        let result = change(&mut record)?;
        if record != before {
            self.write(&record)?;
        }
        Ok(result)
    }

    /// Returns only after the intent is durable, before the caller constructs an Open link.
    pub fn begin_open<'a>(
        &self,
        source: impl Into<OpenSource<'a>>,
        root: &Path,
        slot: u8,
        module: &str,
        installation: &str,
        now: u64,
    ) -> io::Result<TerminalEntry> {
        if !matches!(slot, 1 | 2) || module.is_empty() || installation.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid terminal owner"));
        }
        let (key, host) = match source.into() {
            OpenSource::Connection(connection) => {
                let project = connection.project();
                if project.root != root {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "terminal root differs from its connection",
                    ));
                }
                (project.host_key_sha256.as_str(), Some(project.host.clone()))
            }
            OpenSource::Key(key) => (
                key,
                self.projects
                    .find(root)?
                    .filter(|p| p.host_key_sha256 == key)
                    .map(|p| p.host),
            ),
        };
        self.update(key, root, |record| {
            if let Some(entry) = record
                .entries
                .iter()
                .find(|e| e.slot == slot && e.module == module && e.state != TerminalState::StopQueued)
            {
                return if entry.state == TerminalState::Opening {
                    Ok(entry.clone())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "the module already owns a terminal",
                    ))
                };
            }
            if record.entries.len() >= ENTRIES_MAX {
                return Err(io::Error::other("a project can remember at most 256 server terminals"));
            }
            let entry = TerminalEntry {
                slot,
                module: module.to_owned(),
                state: TerminalState::Opening,
                command_id: uuid::Uuid::new_v4().simple().to_string(),
                terminal_id: None,
                installation: installation.to_owned(),
                daemon_instance: None,
                boot_id: None,
                opened_at: now,
                extra: BTreeMap::new(),
            };
            // A record written before the host was known learns it here, so a later key change can find it.
            if record.host.is_none() {
                record.host = host;
            }
            record.entries.push(entry.clone());
            Ok(entry)
        })
    }

    /// The caller persists this answer before enabling the pane's first key.
    pub fn opened(&self, key: &str, root: &Path, command: &str, id: &str, daemon: &str, boot: &str) -> io::Result<()> {
        if [id, daemon, boot].iter().any(|id| id.is_empty()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty terminal identity"));
        }
        self.update(key, root, |record| {
            let entry = record
                .entries
                .iter_mut()
                .find(|e| e.command_id == command)
                .ok_or(io::ErrorKind::NotFound)?;
            if entry.terminal_id.as_deref().is_some_and(|saved| saved != id) {
                return Err(io::Error::other("the open answer changed terminal identity"));
            }
            entry.terminal_id = Some(id.to_owned());
            entry.daemon_instance = Some(daemon.to_owned());
            entry.boot_id = Some(boot.to_owned());
            if entry.state != TerminalState::StopQueued {
                entry.state = TerminalState::Open;
            }
            Ok(())
        })
    }

    pub fn restored(&self, key: &str, root: &Path, slot: u8, modules: &[String]) -> io::Result<Vec<TerminalEntry>> {
        Ok(self
            .record(key, root)?
            .entries
            .into_iter()
            .filter(|e| e.slot == slot && modules.contains(&e.module) && e.state != TerminalState::StopQueued)
            .collect())
    }

    /// Claims include both windows' restored modules; an absent second window claims nothing.
    pub fn orphans(&self, key: &str, root: &Path, claims: &[(u8, String)]) -> io::Result<Vec<TerminalEntry>> {
        Ok(self
            .record(key, root)?
            .entries
            .into_iter()
            .filter(|e| e.state != TerminalState::StopQueued && !claims.contains(&(e.slot, e.module.clone())))
            .collect())
    }

    pub fn claim(&self, key: &str, root: &Path, command: &str, slot: u8, module: &str) -> io::Result<TerminalEntry> {
        if !matches!(slot, 1 | 2) || module.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid terminal owner"));
        }
        self.update(key, root, |record| {
            if record.entries.iter().any(|e| {
                e.slot == slot && e.module == module && e.command_id != command && e.state != TerminalState::StopQueued
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the module already owns a terminal",
                ));
            }
            let entry = record
                .entries
                .iter_mut()
                .find(|e| e.command_id == command)
                .ok_or(io::ErrorKind::NotFound)?;
            if entry.state == TerminalState::StopQueued {
                return Err(io::Error::other("this terminal is queued to stop"));
            }
            entry.slot = slot;
            entry.module = module.to_owned();
            Ok(entry.clone())
        })
    }

    /// Keep the stop before any network call. A signal, crash or mirror drop never calls this method.
    pub fn queue_stop(&self, key: &str, root: &Path, command: &str) -> io::Result<()> {
        self.update(key, root, |record| {
            let entry = record
                .entries
                .iter_mut()
                .find(|e| e.command_id == command)
                .ok_or(io::ErrorKind::NotFound)?;
            entry.state = TerminalState::StopQueued;
            Ok(())
        })
    }

    pub fn forget(&self, key: &str, root: &Path, command: &str) -> io::Result<()> {
        self.update(key, root, |record| {
            record.entries.retain(|e| e.command_id != command);
            Ok(())
        })
    }

    /// Removes a gone entry only after its reason has been presented successfully.
    pub fn show_gone(&self, key: &str, root: &Path, command: &str, show: impl FnOnce()) -> io::Result<()> {
        if self.record(key, root)?.entries.iter().any(|e| e.command_id == command) {
            show();
            self.forget(key, root, command)?;
        }
        Ok(())
    }

    /// Only a person's close queues a stop. No connection leaves it durable for the next window. Answers the server's
    /// warning when it could not confirm the terminal's processes gone.
    pub fn close(
        &self,
        key: &str,
        root: &Path,
        command: &str,
        by_hand: bool,
        control: Option<&mut dyn TerminalControl>,
    ) -> io::Result<Option<String>> {
        if !by_hand {
            return Ok(None);
        }
        self.queue_stop(key, root, command)?;
        if let Some(control) = control {
            let entry = self
                .record(key, root)?
                .entries
                .into_iter()
                .find(|e| e.command_id == command)
                .ok_or(io::ErrorKind::NotFound)?;
            let result = control.stop(root, &entry);
            if result.is_ok() || result.as_ref().is_err_and(|e| permanent_stop(e)) {
                self.forget(key, root, command)?;
            }
            return result;
        }
        Ok(None)
    }

    /// Failed stops stay queued; one failure does not hold back the rest. No lock spans a network call.
    pub fn send_queued(&self, key: &str, root: &Path, control: &mut dyn TerminalControl) -> io::Result<Vec<String>> {
        self.send_queued_before(key, root, control, Instant::now() + STOP_BUDGET, Instant::now)
    }

    fn send_queued_before(
        &self,
        key: &str,
        root: &Path,
        control: &mut dyn TerminalControl,
        deadline: Instant,
        mut now: impl FnMut() -> Instant,
    ) -> io::Result<Vec<String>> {
        let mut failures = Vec::new();
        for entry in self
            .record(key, root)?
            .entries
            .into_iter()
            .filter(|e| e.state == TerminalState::StopQueued)
        {
            if now() >= deadline {
                failures.push("terminal stops did not finish in time; the remaining stops stay queued".into());
                break;
            }
            match control.stop(root, &entry) {
                Ok(warning) => {
                    self.forget(key, root, &entry.command_id)?;
                    failures.extend(warning);
                }
                Err(e) if permanent_stop(&e) => {
                    self.forget(key, root, &entry.command_id)?;
                    failures.push(format!(
                        "terminal {}: stop refused and forgotten: {e}",
                        entry.command_id
                    ));
                }
                Err(e) => failures.push(format!("terminal {}: {e}", entry.command_id)),
            }
        }
        Ok(failures)
    }

    /// Retries interrupted moves even when the project already records the verified key.
    /// Conflicting entries remain under their original key and are reported on every open.
    pub fn reconcile_key(&self, new: &str, root: &Path) -> io::Result<Vec<String>> {
        let Some(project) = self.projects.find(root)? else {
            return Ok(Vec::new());
        };
        self.reconcile_host(&project.host, new, root)
    }

    fn reconcile_host(&self, host: &str, new: &str, root: &Path) -> io::Result<Vec<String>> {
        let _lock = self.lock()?;
        let (records, mut notices) = self.scan_matching(|header| {
            (header.host.is_none() || header.host.as_deref() == Some(host))
                && header.root == root
                && header.host_key != new
        })?;
        let mut destination = self.load(new, root)?;
        for mut source in records {
            if source.host.is_none() {
                // Without the host as typed it cannot be told from another server's record of the same path: kept
                // and said, never moved.
                if !source.entries.is_empty() {
                    notices.push(format!(
                        "{} server terminal(s) of {} are recorded under another host key with no host name; \
                         see `eitri host status`",
                        source.entries.len(),
                        source.root.display()
                    ));
                }
                continue;
            }
            let before = destination.clone();
            destination.host = Some(host.to_owned());
            let old_entries = source.entries.clone();
            source.entries.retain(|entry| {
                if let Some(saved) = destination.entries.iter().find(|e| e.command_id == entry.command_id) {
                    if saved == entry {
                        return false;
                    }
                } else if !destination
                    .entries
                    .iter()
                    .any(|e| e.slot == entry.slot && e.module == entry.module)
                {
                    destination.entries.push(entry.clone());
                    return false;
                }
                notices.push(format!(
                    "terminal {} kept under host key {}: conflicting terminal ownership during key change",
                    display_text(&entry.command_id),
                    display_text(&source.host_key)
                ));
                true
            });
            for (key, value) in &source.extra {
                destination.extra.entry(key.clone()).or_insert_with(|| value.clone());
            }
            // Persist the destination first. A crash leaves duplicates that the next open can remove.
            if destination != before {
                self.write(&destination)?;
            }
            if source.entries.is_empty() {
                std::fs::remove_file(self.path(&source.host_key, root)?)?;
                sync_dir(&self.dir()?)?;
            } else if source.entries != old_entries {
                self.write(&source)?;
            }
        }
        if destination.entries.len() > ENTRIES_MAX {
            notices.push(format!(
                "this project remembers {} server terminals, past the limit of {ENTRIES_MAX}; all entries were kept",
                destination.entries.len()
            ));
        }
        Ok(notices)
    }

    /// Includes records whose project was forgotten; never silently ignores a damaged terminal record.
    pub fn clear_host(&self, key: &str) -> io::Result<Vec<String>> {
        let _lock = self.lock()?;
        let (records, mut notices) = self.scan()?;
        for record in records.into_iter().filter(|r| r.host_key == key) {
            let path = self.path(key, &record.root)?;
            if let Err(e) = std::fs::remove_file(&path) {
                notices.push(format!("{}: {e}", path.display()));
            }
        }
        sync_dir(&self.dir()?)?;
        Ok(notices)
    }

    fn scan(&self) -> io::Result<(Vec<TerminalRecord>, Vec<String>)> {
        self.scan_matching(|_| true)
    }

    fn scan_matching(&self, matches: impl Fn(&RecordHeader) -> bool) -> io::Result<(Vec<TerminalRecord>, Vec<String>)> {
        let mut records = Vec::new();
        let mut notices = Vec::new();
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(self.dir()?)? {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(e) => notices.push(format!("terminal record: {e}")),
            }
        }
        paths.sort();
        for path in paths {
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let record = (|| {
                let Some(record) = read::<RecordHeader>(&path)? else {
                    return Ok(None);
                };
                // Ignore other origins before decoding or validating their entries.
                if !matches(&record) {
                    return Ok(None);
                }
                if self.path(&record.host_key, &record.root)? != path {
                    return Err(invalid(&path, "terminal record file name does not match its identity"));
                }
                self.load(&record.host_key, &record.root).map(Some)
            })();
            match record {
                Ok(Some(record)) => records.push(record),
                Ok(None) => {}
                Err(e) => notices.push(format!("terminal record {}: {e}", path.display())),
            }
        }
        Ok((records, notices))
    }
}

#[derive(Deserialize)]
struct RecordHeader {
    #[serde(default)]
    host: Option<String>,
    host_key: String,
    root: PathBuf,
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    agent::private_fs::open_dir(dir)?.sync_all()
}

pub trait TerminalControl {
    /// Idempotent, through the entry's installation. An unanswered open is looked up, never resent. `Some` is the
    /// warning to show when the server could not confirm the terminal's shell and session gone.
    fn stop(&mut self, root: &Path, entry: &TerminalEntry) -> io::Result<Option<String>>;
}

/// The metadata key a server's close answer carries when ending the shell's session could not be confirmed.
const CLOSE_WARNING: &str = "eitri-terminal-close-warning";

/// The client's own sentence for that warning; the server's text is never shown.
pub const CLOSE_WARNING_NOTICE: &str =
    "the closed server terminal's shell or its session could not be confirmed gone; its programs may still run there";

fn close_warning(metadata: &tonic::metadata::MetadataMap) -> Option<String> {
    metadata
        .contains_key(CLOSE_WARNING)
        .then(|| CLOSE_WARNING_NOTICE.to_owned())
}

impl TerminalControl for RemoteConnection {
    fn stop(&mut self, _root: &Path, entry: &TerminalEntry) -> io::Result<Option<String>> {
        let client = self.terminal_control(&entry.installation)?;
        stop_on(&client, entry.clone(), Instant::now() + STOP_BUDGET)
    }
}

fn permanent_stop(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|e| e.downcast_ref::<tonic::Status>())
        .is_some_and(|status| {
            matches!(
                status.code(),
                tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition | tonic::Code::DataLoss
            )
        })
}

fn stop_on(client: &HelperClient, entry: TerminalEntry, deadline: Instant) -> io::Result<Option<String>> {
    let (channel, _) = client.terminal_channel().map_err(io::Error::other)?;
    service_call_before(client, deadline, move |mut service| async move {
        let hello = if entry.terminal_id.is_none() {
            Some(
                wire::helper_client::HelperClient::new(channel)
                    .hello(wire::HelloRequest {
                        protocol_major: wire::PROTOCOL_MAJOR,
                        protocol_minor: wire::PROTOCOL_MINOR,
                        client_version: env!("CARGO_PKG_VERSION").into(),
                    })
                    .await?
                    .into_inner(),
            )
        } else {
            None
        };
        let inventory = if entry.terminal_id.is_none() {
            Some(
                service
                    .list_terminals(wire::ListTerminalsRequest {})
                    .await?
                    .into_inner(),
            )
        } else {
            None
        };
        let Some(request) = stop_request(&entry, inventory.as_ref(), hello.as_ref(), unix_seconds())? else {
            return Ok(None);
        };
        match service.close_terminal(request).await {
            Ok(answer) => Ok(close_warning(answer.metadata())),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(status),
        }
    })
}

/// Closes the terminal `terminal_id` on the server `client` reaches, as a call of its own: `Some` is the warning to show
/// when the server could not confirm the shell and its session gone. A terminal the server no longer knows is closed.
pub fn close_on(client: &HelperClient, terminal_id: &str) -> io::Result<Option<String>> {
    close_before(client, terminal_id, Instant::now() + STOP_BUDGET)
}

/// [`close_on`], given up at `deadline`.
pub fn close_before(client: &HelperClient, terminal_id: &str, deadline: Instant) -> io::Result<Option<String>> {
    let request = wire::CloseTerminalRequest {
        terminal_id: terminal_id.to_owned(),
        command_id: format!("{:x}", Sha256::digest(format!("close:{terminal_id}").as_bytes())),
    };
    service_call_before(client, deadline, move |mut service| async move {
        match service.close_terminal(request).await {
            Ok(answer) => Ok(close_warning(answer.metadata())),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(status),
        }
    })
}

impl RemoteConnection {
    /// Closes the terminal `terminal_id` of this project's current installation, through a terminal connection of its
    /// own ([`close_on`]). Blocking: a worker's.
    pub fn close_terminal(&self, terminal_id: &str) -> io::Result<Option<String>> {
        close_on(&self.terminal_control("")?, terminal_id)
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn stop_request(
    entry: &TerminalEntry,
    inventory: Option<&wire::ListTerminalsResponse>,
    hello: Option<&wire::HelloResponse>,
    now: u64,
) -> Result<Option<wire::CloseTerminalRequest>, tonic::Status> {
    let id = if let Some(id) = &entry.terminal_id {
        id.clone()
    } else {
        let list = inventory.ok_or_else(|| tonic::Status::unavailable("terminal inventory was not read"))?;
        let Some(info) = list
            .terminals
            .iter()
            .take(wire::TERM_LIST_MAX)
            .find(|info| info.open_command_id == entry.command_id)
        else {
            if list.truncated || list.terminals.len() > wire::TERM_LIST_MAX {
                return Err(tonic::Status::unavailable(
                    "terminal inventory was truncated; stop stays queued",
                ));
            }
            let generation_changed = hello.is_some_and(|hello| {
                entry
                    .daemon_instance
                    .as_deref()
                    .is_some_and(|id| id != hello.daemon_instance)
                    || entry.boot_id.as_deref().is_some_and(|id| id != hello.boot_id)
            });
            if !generation_changed && now.saturating_sub(entry.opened_at) <= OPEN_SETTLE_SECONDS {
                return Err(tonic::Status::unavailable(
                    "the terminal open may still finish; stop stays queued",
                ));
            }
            return Ok(None);
        };
        info.terminal_id.clone()
    };
    if id.len() != 32
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(tonic::Status::data_loss(
            "the terminal server sent an invalid terminal id",
        ));
    }
    Ok(Some(wire::CloseTerminalRequest {
        terminal_id: id,
        command_id: format!("{:x}", Sha256::digest(format!("close:{}", entry.command_id).as_bytes())),
    }))
}

/// One worker owns the batch's connections, including a cached failure for an unavailable installation.
/// The caller's deadline includes connection setup; late setup cannot send a stop after the deadline.
struct StopBatch {
    sender: std::sync::mpsc::Sender<(TerminalEntry, std::sync::mpsc::SyncSender<io::Result<Option<String>>>)>,
    deadline: Instant,
}

impl StopBatch {
    fn new(connection: RemoteConnection, deadline: Instant) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            stop_worker(
                receiver,
                deadline,
                |installation| connection.terminal_control(installation),
                |client, entry| stop_on(client, entry, deadline),
            );
        });
        Self { sender, deadline }
    }
}

fn stop_worker<C>(
    receiver: std::sync::mpsc::Receiver<(TerminalEntry, std::sync::mpsc::SyncSender<io::Result<Option<String>>>)>,
    deadline: Instant,
    mut connect: impl FnMut(&str) -> io::Result<C>,
    mut stop: impl FnMut(&C, TerminalEntry) -> io::Result<Option<String>>,
) {
    let mut clients = BTreeMap::new();
    while let Ok((entry, reply)) = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if Instant::now() >= deadline {
            break;
        }
        let client = clients
            .entry(entry.installation.clone())
            .or_insert_with(|| connect(&entry.installation).map_err(|e| e.to_string()));
        if Instant::now() >= deadline {
            break;
        }
        let result = match client {
            Ok(client) => stop(client, entry),
            Err(why) => Err(io::Error::other(why.clone())),
        };
        let _ = reply.send(result);
    }
}

impl TerminalControl for StopBatch {
    fn stop(&mut self, _root: &Path, entry: &TerminalEntry) -> io::Result<Option<String>> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.sender.send((entry.clone(), sender)).map_err(io::Error::other)?;
        receiver
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "terminal stops did not finish in time"))?
    }
}

/// Runs on the pinned client's runtime, with both RPC and caller deadlines. The runtime owner stays on the worker.
pub(crate) fn service_call<T, F, Fut>(client: &HelperClient, call: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce(wire::terminal_client::TerminalClient<tonic::transport::Channel>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, tonic::Status>> + Send + 'static,
{
    service_call_before(client, Instant::now() + STOP_BUDGET, call)
}

fn service_call_before<T, F, Fut>(client: &HelperClient, deadline: Instant, call: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce(wire::terminal_client::TerminalClient<tonic::transport::Channel>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, tonic::Status>> + Send + 'static,
{
    let (channel, runtime) = client.terminal_channel().map_err(io::Error::other)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let task = runtime.spawn(async move {
        let service = wire::terminal_client::TerminalClient::new(channel).max_decoding_message_size(2 * 1024 * 1024);
        let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), call(service)).await;
        let _ = sender.send(match result {
            Ok(result) => result
                .map_err(|status| io::Error::other(tonic::Status::new(status.code(), display_text(status.message())))),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal service did not answer",
            )),
        });
    });
    let result = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "terminal service did not answer"));
    if result.is_err() {
        task.abort();
    }
    result?
}

fn report(result: io::Result<Vec<String>>, band: &dyn Fn(String)) {
    match result {
        Ok(notices) => notices.into_iter().for_each(band),
        Err(e) => band(format!("terminal records: {e}")),
    }
}

/// Terminal bookkeeping is best effort: a damaged record cannot deny the project's window.
pub fn reconcile_records(store: &RemoteProjects, project: &RemoteProject, band: &dyn Fn(String)) {
    report(
        store
            .terminals()
            .reconcile_host(&project.host, &project.host_key_sha256, &project.root),
        band,
    );
}

pub fn send_queued_stops(store: &RemoteProjects, connection: &RemoteConnection, band: &dyn Fn(String)) {
    report(
        (|| {
            let project = connection.project();
            if let Some(current) = store.find(&project.root)? {
                if !current.same_identity(project) {
                    return Err(io::Error::other(
                        "the project now points at another server; reopen the window",
                    ));
                }
            }
            let deadline = Instant::now() + STOP_BUDGET;
            store.terminals().send_queued_before(
                &project.host_key_sha256,
                &project.root,
                &mut StopBatch::new(connection.clone(), deadline),
                deadline,
                Instant::now,
            )
        })(),
        band,
    );
}

pub fn orphan_notice(host: &str, count: usize) -> String {
    if count == 1 {
        format!("1 terminal on {host} has no window")
    } else {
        format!("{count} terminals on {host} have no window")
    }
}

pub fn queued_notice(host: &str) -> String {
    format!("the terminal on {host} will be stopped when a window of this project next connects")
}

pub(crate) fn local_time(unix_ms: u64) -> String {
    let Ok(seconds) = libc::time_t::try_from(unix_ms / 1000) else {
        return "unknown time".into();
    };
    let mut time = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut text = [0u8; 80];
    // The libc routines receive valid pointers and write only to these stack buffers.
    unsafe {
        if libc::localtime_r(&seconds, time.as_mut_ptr()).is_null() {
            return "unknown time".into();
        }
        let len = libc::strftime(
            text.as_mut_ptr().cast(),
            text.len(),
            c"%Y-%m-%d %H:%M:%S %Z".as_ptr(),
            time.as_ptr(),
        );
        if len == 0 {
            return "unknown time".into();
        }
        String::from_utf8_lossy(&text[..len]).into_owned()
    }
}

/// Every server string is escaped before it leaves the terminal grid.
pub fn terminal_lines(response: &wire::ListTerminalsResponse) -> Vec<String> {
    let mut lines = Vec::new();
    for terminal in response.terminals.iter().take(wire::TERM_LIST_MAX) {
        let id: String = terminal.terminal_id.chars().take(12).collect();
        let cwd = if terminal.cwd.is_empty() {
            &terminal.start_dir
        } else {
            &terminal.cwd
        };
        let attached = if terminal.attached {
            "attached".to_owned()
        } else if terminal.exit.is_some() {
            if terminal.detached_since_unix_ms == 0 {
                "last shown time unknown".to_owned()
            } else {
                format!("last shown {}", local_time(terminal.detached_since_unix_ms))
            }
        } else if terminal.detached_since_unix_ms == 0 {
            "no window yet".to_owned()
        } else {
            format!("no window since {}", local_time(terminal.detached_since_unix_ms))
        };
        let mut line = format!("  {}  {}  {attached}", display_text(&id), display_text(cwd));
        if let Some(exit) = &terminal.exit {
            let status = if exit.left {
                "process left the terminal, still running".to_owned()
            } else if let Some(code) = exit.code {
                format!("exited with status {code}")
            } else if let Some(signal) = exit.signal {
                format!("exited on signal {signal}")
            } else {
                "exited with unknown status".to_owned()
            };
            line.push_str(&format!("  {status}"));
        } else if terminal.foreground_job == Some(true) {
            line.push_str(&format!("  running {}", display_text(&terminal.foreground_name)));
        }
        lines.push(line);
    }
    if response.truncated || response.terminals.len() > wire::TERM_LIST_MAX {
        lines.push("  terminal list truncated".to_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;

    #[test]
    fn orphan_notices_agree_with_the_terminal_count() {
        assert_eq!(
            orphan_notice("example-host", 1),
            "1 terminal on example-host has no window"
        );
        assert_eq!(
            orphan_notice("example-host", 2),
            "2 terminals on example-host have no window"
        );
    }

    #[test]
    fn exited_terminals_report_how_they_ended_and_when_they_were_last_shown() {
        for (exit, status) in [
            (
                wire::TerminalExited {
                    code: Some(7),
                    signal: None,
                    left: false,
                },
                "exited with status 7",
            ),
            (
                wire::TerminalExited {
                    code: None,
                    signal: Some(15),
                    left: false,
                },
                "exited on signal 15",
            ),
            (
                wire::TerminalExited {
                    code: None,
                    signal: None,
                    left: true,
                },
                "process left the terminal, still running",
            ),
            (wire::TerminalExited::default(), "exited with unknown status"),
        ] {
            for timestamp in [0, 42] {
                let response = wire::ListTerminalsResponse {
                    terminals: vec![wire::TerminalInfo {
                        terminal_id: "exited".into(),
                        start_dir: "/srv/project".into(),
                        exit: Some(exit.clone()),
                        detached_since_unix_ms: timestamp,
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                let shown = if timestamp == 0 {
                    "last shown time unknown".into()
                } else {
                    format!("last shown {}", local_time(timestamp))
                };
                assert_eq!(
                    terminal_lines(&response),
                    [format!("  exited  /srv/project  {shown}  {status}")]
                );
            }
        }
    }

    #[test]
    fn an_open_intent_and_its_answer_sync_before_and_after_rename() {
        let state = ScratchDir::new("remote", "terminal-sync");
        let projects = RemoteProjects::new(Some(state.as_os_str()), None);
        let records = projects.terminals();
        let before = agent::private_fs::replace_test_hooks::syncs();
        let entry = records
            .begin_open(
                "SHA256:example",
                Path::new("/srv/project"),
                1,
                "terminal-1",
                "0.3.0-example",
                42,
            )
            .unwrap();
        assert_eq!(agent::private_fs::replace_test_hooks::syncs() - before, 2);
        let before = agent::private_fs::replace_test_hooks::syncs();
        records
            .opened(
                "SHA256:example",
                Path::new("/srv/project"),
                &entry.command_id,
                "terminal",
                "daemon",
                "boot",
            )
            .unwrap();
        assert_eq!(agent::private_fs::replace_test_hooks::syncs() - before, 2);
    }
    fn entry() -> TerminalEntry {
        TerminalEntry {
            slot: 1,
            module: "terminal".into(),
            state: TerminalState::StopQueued,
            command_id: "open-command".into(),
            terminal_id: None,
            installation: "0.3.0-example".into(),
            daemon_instance: None,
            boot_id: None,
            opened_at: 42,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn an_unanswered_stop_closes_the_inventory_match_or_nothing() {
        let entry = entry();
        let inventory = wire::ListTerminalsResponse {
            terminals: vec![
                wire::TerminalInfo {
                    terminal_id: "b".repeat(32),
                    open_command_id: "other-command".into(),
                    ..Default::default()
                },
                wire::TerminalInfo {
                    terminal_id: "a".repeat(32),
                    open_command_id: entry.command_id.clone(),
                    ..Default::default()
                },
            ],
            truncated: false,
        };
        let request = stop_request(&entry, Some(&inventory), None, 103).unwrap().unwrap();
        assert_eq!(request.terminal_id, "a".repeat(32));
        assert_ne!(request.command_id, entry.command_id);
        assert_eq!(
            request.command_id,
            stop_request(&entry, Some(&inventory), None, 103)
                .unwrap()
                .unwrap()
                .command_id
        );
        assert!(
            stop_request(&entry, Some(&wire::ListTerminalsResponse::default()), None, 103)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            stop_request(
                &entry,
                Some(&wire::ListTerminalsResponse {
                    truncated: true,
                    ..Default::default()
                }),
                None,
                103,
            )
            .unwrap_err()
            .code(),
            tonic::Code::Unavailable
        );
    }

    #[test]
    fn a_recent_unlisted_open_keeps_its_stop_queued() {
        let mut entry = entry();
        entry.opened_at = 42;
        entry.daemon_instance = Some("daemon".into());
        entry.boot_id = Some("boot".into());
        let result = stop_request(
            &entry,
            Some(&wire::ListTerminalsResponse::default()),
            None,
            entry.opened_at,
        );
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
    }

    #[test]
    fn an_unlisted_open_is_forgotten_only_after_its_deadline_or_generation_change() {
        let mut entry = entry();
        entry.daemon_instance = Some("daemon".into());
        entry.boot_id = Some("boot".into());
        let list = wire::ListTerminalsResponse::default();
        let hello = wire::HelloResponse {
            daemon_instance: "daemon".into(),
            boot_id: "boot".into(),
            ..Default::default()
        };
        for age in [0, 59, 60] {
            assert_eq!(
                stop_request(&entry, Some(&list), Some(&hello), 42 + age)
                    .unwrap_err()
                    .code(),
                tonic::Code::Unavailable
            );
        }
        assert!(stop_request(&entry, Some(&list), Some(&hello), 103).unwrap().is_none());
        // A backwards clock and unknown saved identity cannot prove an open has finished.
        assert!(stop_request(&entry, Some(&list), Some(&hello), 41).is_err());
        for (daemon, boot) in [("new-daemon", "boot"), ("daemon", "new-boot")] {
            let hello = wire::HelloResponse {
                daemon_instance: daemon.into(),
                boot_id: boot.into(),
                ..Default::default()
            };
            assert!(stop_request(&entry, Some(&list), Some(&hello), 42).unwrap().is_none());
        }
        entry.daemon_instance = None;
        entry.boot_id = None;
        assert!(stop_request(&entry, Some(&list), Some(&hello), 42).is_err());
    }

    #[test]
    fn a_stop_batch_connects_once_per_installation_including_failed_connections() {
        for failed in [false, true] {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut replies = Vec::new();
            for installation in ["first", "second", "first", "second"] {
                let mut entry = entry();
                entry.installation = installation.into();
                let (reply, received) = std::sync::mpsc::sync_channel(1);
                sender.send((entry, reply)).unwrap();
                replies.push(received);
            }
            drop(sender);
            let mut connections = Vec::new();
            let mut stops = Vec::new();
            stop_worker(
                receiver,
                Instant::now() + STOP_BUDGET,
                |installation| {
                    connections.push(installation.to_owned());
                    if failed && installation == "first" {
                        Err(io::ErrorKind::NotConnected.into())
                    } else {
                        Ok(installation.to_owned())
                    }
                },
                |client, entry| {
                    assert_eq!(client, &entry.installation);
                    stops.push(entry.installation);
                    Ok(None)
                },
            );
            assert_eq!(connections, ["first", "second"]);
            assert_eq!(stops.len(), if failed { 2 } else { 4 });
            for (i, reply) in replies.into_iter().enumerate() {
                assert_eq!(reply.recv().unwrap().is_err(), failed && i % 2 == 0);
            }
        }
    }

    struct Control {
        code: Option<tonic::Code>,
        stopped: Vec<String>,
    }
    impl TerminalControl for Control {
        fn stop(&mut self, _: &Path, entry: &TerminalEntry) -> io::Result<Option<String>> {
            self.stopped.push(entry.command_id.clone());
            match self.code {
                Some(code) => Err(io::Error::other(tonic::Status::new(code, "stop refused"))),
                None => Ok(None),
            }
        }
    }

    #[test]
    fn a_close_answer_with_the_cleanup_warning_is_said_in_the_clients_own_words() {
        let mut metadata = tonic::metadata::MetadataMap::new();
        assert_eq!(close_warning(&metadata), None);
        metadata.insert(
            CLOSE_WARNING,
            tonic::metadata::MetadataValue::from_static("server prose, never shown"),
        );
        assert_eq!(close_warning(&metadata).as_deref(), Some(CLOSE_WARNING_NOTICE));
    }

    #[test]
    fn a_close_and_a_queued_stop_pass_the_warning_on_and_still_forget_the_entry() {
        struct Warned;
        impl TerminalControl for Warned {
            fn stop(&mut self, _: &Path, _: &TerminalEntry) -> io::Result<Option<String>> {
                Ok(Some(CLOSE_WARNING_NOTICE.into()))
            }
        }
        let state = ScratchDir::new("remote", "terminal-close-warning");
        let projects = RemoteProjects::new(Some(state.as_os_str()), None);
        let records = projects.terminals();
        let root = Path::new("/srv/project");
        let entry = records
            .begin_open("key", root, 1, "terminal", "installation", 42)
            .unwrap();
        let warning = records
            .close("key", root, &entry.command_id, true, Some(&mut Warned))
            .unwrap();
        assert_eq!(warning.as_deref(), Some(CLOSE_WARNING_NOTICE));
        assert!(records.record("key", root).unwrap().entries.is_empty());
        let queued = records
            .begin_open("key", root, 1, "terminal", "installation", 42)
            .unwrap();
        records.queue_stop("key", root, &queued.command_id).unwrap();
        assert_eq!(
            records.send_queued("key", root, &mut Warned).unwrap(),
            [CLOSE_WARNING_NOTICE.to_owned()]
        );
        assert!(records.record("key", root).unwrap().entries.is_empty());
    }

    #[test]
    fn a_total_stop_deadline_leaves_the_remaining_entries_queued() {
        let state = ScratchDir::new("remote", "terminal-stop-deadline");
        let projects = RemoteProjects::new(Some(state.as_os_str()), None);
        let records = projects.terminals();
        let root = Path::new("/srv/project");
        for module in ["first", "second"] {
            let entry = records.begin_open("key", root, 1, module, "installation", 42).unwrap();
            records.queue_stop("key", root, &entry.command_id).unwrap();
        }
        let start = Instant::now();
        let mut clock = start;
        let mut control = Control {
            code: None,
            stopped: Vec::new(),
        };
        let notices = records
            .send_queued_before("key", root, &mut control, start + STOP_BUDGET, || {
                let now = clock;
                clock += STOP_BUDGET;
                now
            })
            .unwrap();
        assert_eq!(control.stopped.len(), 1);
        assert_eq!(records.record("key", root).unwrap().entries.len(), 1);
        assert!(notices[0].contains("remaining stops stay queued"));
    }

    #[test]
    fn permanent_stop_refusals_are_forgotten_with_a_notice_and_transient_ones_are_kept() {
        for code in [
            tonic::Code::InvalidArgument,
            tonic::Code::FailedPrecondition,
            tonic::Code::Unavailable,
        ] {
            let state = ScratchDir::new("remote", "terminal-stop-refused");
            let projects = RemoteProjects::new(Some(state.as_os_str()), None);
            let records = projects.terminals();
            let root = Path::new("/srv/project");
            let entry = records
                .begin_open("key", root, 1, "terminal", "installation", 42)
                .unwrap();
            records.queue_stop("key", root, &entry.command_id).unwrap();
            let notices = records
                .send_queued(
                    "key",
                    root,
                    &mut Control {
                        code: Some(code),
                        stopped: Vec::new(),
                    },
                )
                .unwrap();
            let permanent = code != tonic::Code::Unavailable;
            assert_eq!(records.record("key", root).unwrap().entries.is_empty(), permanent);
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].contains("forgotten"), permanent);
        }
    }

    #[test]
    fn bookkeeping_failures_are_band_notices() {
        let notices = std::cell::RefCell::new(Vec::new());
        for kind in [
            io::ErrorKind::InvalidData,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::PermissionDenied,
        ] {
            report(Err(io::Error::new(kind, "record unavailable")), &|line| {
                notices.borrow_mut().push(line)
            });
        }
        assert_eq!(notices.borrow().len(), 3);
        assert!(notices.borrow().iter().all(|line| line.contains("record unavailable")));
    }
}
