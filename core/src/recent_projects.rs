//! Recent project records, read on a worker; filesystem facts have a separate, bounded wait.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use agent::private_fs::{read_private_nofollow_bytes_capped, read_private_nofollow_capped};

use crate::instance_dir::{nonblocking_connect_outcome, ConnectOutcome};
use crate::project_chooser::{age_text, Fact, Row};
use crate::{open_socket, state_paths};

#[cfg(test)]
mod tests;

pub const FACT_DEADLINE: Duration = Duration::from_millis(150);
const RECORD_CAP: u64 = 4 * 1024 * 1024;
const CANDIDATE_CAP: usize = 200;
const PRESENT_CAP: usize = 30;
const GONE_CAP: usize = 5;
static FACT_BUSY: AtomicBool = AtomicBool::new(false);

pub struct Sources {
    pub layout: Option<PathBuf>,
    pub tabs: Option<PathBuf>,
    pub control: PathBuf,
    pub home: Option<PathBuf>,
}

impl Sources {
    pub fn from_env(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>, control: PathBuf) -> Sources {
        Sources {
            layout: state_paths::state_subdir(xdg_state_home, home, "layout"),
            tabs: state_paths::state_subdir(xdg_state_home, home, "tabs"),
            control,
            home: home.map(PathBuf::from).filter(|home| home.is_absolute()),
        }
    }
}

pub trait Probe: Send + 'static {
    fn exists(&self, path: &Path) -> bool;
    fn window_holds(&self, root: &Path) -> bool;
}

pub struct RealProbe {
    pub control: PathBuf,
}

impl Probe for RealProbe {
    fn exists(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn window_holds(&self, root: &Path) -> bool {
        open_socket::socket_path(&self.control, root)
            .is_ok_and(|socket| nonblocking_connect_outcome(&socket) == ConnectOutcome::Listener)
    }
}

#[derive(Debug)]
pub struct Candidate {
    pub root: PathBuf,
    pub used: SystemTime,
}

struct Record {
    path: PathBuf,
    used: SystemTime,
}

#[derive(Default)]
struct Group {
    lock: Option<Record>,
    layout: Option<Record>,
    tabs: Option<Record>,
    cm: Option<Record>,
}

impl Group {
    fn newest(&self) -> SystemTime {
        [&self.lock, &self.layout, &self.tabs, &self.cm]
            .into_iter()
            .flatten()
            .map(|record| record.used)
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    fn json_root(&self) -> Option<PathBuf> {
        let record = self.layout.as_ref().or(self.tabs.as_ref())?;
        let text = read_private_nofollow_capped(&record.path, RECORD_CAP).ok()?;
        #[derive(serde::Deserialize)]
        struct Root {
            project_root: PathBuf,
        }
        serde_json::from_str::<Root>(&text)
            .ok()
            .map(|record| record.project_root)
    }
}

fn collect(dir: Option<&Path>, tabs: bool, groups: &mut BTreeMap<String, Group>) {
    let Some(entries) = dir.and_then(|dir| fs::read_dir(dir).ok()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((key, suffix)) = name.get(..16).zip(name.get(16..)) else {
            continue;
        };
        if !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if !matches!(
            (tabs, suffix),
            (true, ".json") | (false, ".pair.lock" | ".v2.json" | ".cm.json")
        ) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        // A compositor record contributes only a time; its content never supplies a root.
        if suffix == ".cm.json"
            && (!metadata.is_file() || metadata.uid() != agent::private_fs::euid() || metadata.len() > RECORD_CAP)
        {
            continue;
        }
        let Ok(used) = metadata.modified() else { continue };
        let record = Some(Record { path, used });
        let group = groups.entry(key.to_owned()).or_default();
        match suffix {
            ".pair.lock" => group.lock = record,
            ".v2.json" => group.layout = record,
            ".cm.json" => group.cm = record,
            ".json" => group.tabs = record,
            _ => unreachable!(),
        }
    }
}

/// Read only the two state directories and keep the newest groups before resolving their roots.
pub fn candidates(sources: &Sources) -> Vec<Candidate> {
    candidates_under(sources).0
}

/// The candidates and the home they were ordered against: the canonical one when it resolves, since
/// persisted roots are canonical and the text shown for a row must strip the same prefix.
fn candidates_under(sources: &Sources) -> (Vec<Candidate>, Option<PathBuf>) {
    let mut groups = BTreeMap::new();
    collect(sources.layout.as_deref(), false, &mut groups);
    collect(sources.tabs.as_deref(), true, &mut groups);
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(key, group)| {
            let stamped = group.lock.as_ref().is_some_and(|record| {
                read_private_nofollow_bytes_capped(&record.path, RECORD_CAP).is_ok_and(|bytes| !bytes.is_empty())
            });
            let used = if stamped {
                group.lock.as_ref().unwrap().used
            } else {
                group.newest()
            };
            (key, group, used)
        })
        .collect();
    groups.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    groups.truncate(CANDIDATE_CAP);
    let mut found: Vec<_> = groups
        .into_iter()
        .filter_map(|(key, group, used)| {
            // Re-read selected locks so unselected groups retain no potentially large bodies.
            let lock = group
                .lock
                .as_ref()
                .map(|record| read_private_nofollow_bytes_capped(&record.path, RECORD_CAP));
            let root = match lock {
                Some(Ok(bytes)) if !bytes.is_empty() => PathBuf::from(OsString::from_vec(bytes)),
                Some(Err(_)) => return None,
                _ => group.json_root()?,
            };
            (state_paths::project_key(&root) == key).then_some(Candidate { root, used })
        })
        .collect();
    // Persisted roots are canonical. Resolve HOME too, which may name them through a symlink. No
    // candidate is statted here, since a dead project mount belongs to the fact worker; HOME itself
    // is resolved on this thread, so a HOME on a dead mount holds this read, as its state directory would.
    let home = sources
        .home
        .as_deref()
        .map(|home| home.canonicalize().unwrap_or_else(|_| home.to_path_buf()));
    found.sort_by_key(|candidate| {
        candidate.root == Path::new("/")
            || Some(candidate.root.as_path()) == home.as_deref()
            || Some(candidate.root.as_path()) == sources.home.as_deref()
    });
    (found, home)
}

/// Called off the window thread; a dead mount can occupy at most one fact worker per process.
pub fn read(sources: &Sources) -> Vec<Row> {
    read_with(
        sources,
        Box::new(RealProbe {
            control: sources.control.clone(),
        }),
        FACT_DEADLINE,
        &FACT_BUSY,
    )
}

struct Busy(&'static AtomicBool);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub fn read_with(sources: &Sources, probe: Box<dyn Probe>, deadline: Duration, busy: &'static AtomicBool) -> Vec<Row> {
    let (candidates, home) = candidates_under(sources);
    let now = SystemTime::now();
    let mut rows: Vec<_> = candidates
        .into_iter()
        .map(|candidate| {
            Row::new(
                candidate.root,
                home.as_deref(),
                age_text(candidate.used, now),
                Fact::Unknown,
            )
        })
        .collect();
    if !rows.is_empty()
        && busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        let slot = Busy(busy);
        let paths: Vec<_> = rows.iter().map(|row| row.path.clone()).collect();
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        let worker = std::thread::Builder::new().name("project-facts".into()).spawn(move || {
            let _slot = slot;
            for (index, path) in paths.iter().enumerate() {
                let fact = if !probe.exists(path) {
                    Fact::Gone
                } else if probe.window_holds(path) {
                    Fact::Open
                } else {
                    Fact::Present
                };
                if sender.send((index, fact)).is_err() {
                    break;
                }
            }
        });
        if worker.is_ok() {
            while let Some(left) = deadline.checked_sub(start.elapsed()) {
                let Ok((index, fact)) = receiver.recv_timeout(left) else {
                    break;
                };
                rows[index].fact = fact;
            }
        }
        // Dropping the handle detaches a stuck stat; Busy remains with that worker until it exits.
    }
    let mut present = Vec::new();
    let mut gone = Vec::new();
    for row in rows {
        if row.fact == Fact::Gone {
            if gone.len() < GONE_CAP {
                gone.push(row);
            }
        } else if present.len() < PRESENT_CAP {
            present.push(row);
        }
    }
    present.extend(gone);
    present
}

pub fn newest_present(rows: &[Row]) -> Option<&Row> {
    rows.iter().find(|row| matches!(row.fact, Fact::Present | Fact::Open))
}
