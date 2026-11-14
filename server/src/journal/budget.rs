use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::time::SystemTime;

use super::segment::{
    bounds, corruption, crash_temp, decode, evicted_path, evicted_range, invalid, segment_paths, validate_tail,
};
use super::{Clock, Disk, FileSystem, GapWhy, JournalConfig, Record, SystemClock};

pub struct SessionUsage {
    pub session: String,
    pub bytes: u64,
    pub closed_at: Option<SystemTime>,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Evicted {
    pub session: String,
    pub first: u64,
    pub last: u64,
}
#[derive(Debug)]
pub enum BudgetPressure {
    LiveOverCap { live_bytes: u64 },
    BelowFloor { free: u64 },
}
#[derive(Debug)]
pub enum BudgetOutcome {
    Within,
    Evicted(Vec<Evicted>),
    LiveOverCap {
        live_bytes: u64,
    },
    BelowFloor {
        free: u64,
    },
    /// Independent failures and residual pressure never hide ranges already evicted.
    Partial {
        evicted: Vec<Evicted>,
        errors: Vec<io::Error>,
        pressure: Option<BudgetPressure>,
    },
    /// A failure that prevents the eviction run from starting.
    Failed(io::Error),
}
pub trait FreeSpace {
    fn available(&self, state_dir: &Path) -> io::Result<u64>;
}
impl<F: Fn(&Path) -> io::Result<u64>> FreeSpace for F {
    fn available(&self, state_dir: &Path) -> io::Result<u64> {
        self(state_dir)
    }
}

/// Space available to an unprivileged writer, including for a directory not created yet.
pub fn available_bytes(dir: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;

    let existing = dir
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no existing ancestor for free-space check"))?;
    let path = std::ffi::CString::new(existing.as_os_str().as_bytes())?;
    // SAFETY: `path` is NUL-terminated and `stat` is filled by the call before it is read.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    (stat.f_bavail as u64)
        .checked_mul(stat.f_frsize as u64)
        .ok_or_else(|| io::Error::other("available filesystem space overflows u64"))
}

pub fn enforce_budget(
    state_dir: &Path,
    sessions: &[SessionUsage],
    config: &JournalConfig,
    free: &dyn FreeSpace,
) -> BudgetOutcome {
    enforce_budget_with(state_dir, sessions, config, free, &Disk, &SystemClock::default())
}
pub fn enforce_budget_with(
    state_dir: &Path,
    sessions: &[SessionUsage],
    config: &JournalConfig,
    free: &dyn FreeSpace,
    fs: &dyn FileSystem,
    clock: &dyn Clock,
) -> BudgetOutcome {
    match enforce(state_dir, sessions, config, free, fs, clock) {
        Ok(outcome) => outcome,
        Err(error) => BudgetOutcome::Failed(error),
    }
}
fn report(errors: &mut Vec<io::Error>, path: &Path, error: io::Error) {
    let message = format!("{}: {error}", path.display());
    if !errors.iter().any(|error| error.to_string() == message) {
        errors.push(io::Error::new(error.kind(), message));
    }
}
/// Counts all helper files; an unsafe or unreadable subtree is reported without stopping its siblings.
fn state_bytes(fs: &dyn FileSystem, dir: &Path, errors: &mut Vec<io::Error>) -> io::Result<u64> {
    let mut total = 0u64;
    let mut removed = false;
    for entry in fs.entries(dir)? {
        let bytes = if entry.directory {
            match state_bytes(fs, &entry.path, errors) {
                Ok(bytes) => bytes,
                Err(error) => {
                    report(errors, &entry.path, error);
                    accounting_bytes(fs, &entry.path, errors)?
                }
            }
        } else if crash_temp(&entry.path) {
            match fs.remove_file(&entry.path) {
                Ok(()) => {
                    removed = true;
                    0
                }
                Err(error) => {
                    report(errors, &entry.path, error);
                    entry.bytes
                }
            }
        } else {
            entry.bytes
        };
        total = total.checked_add(bytes).ok_or_else(|| invalid("state size overflow"))?;
    }
    if removed {
        if let Err(error) = fs.sync_dir(dir) {
            report(errors, dir, error);
        }
    }
    Ok(total)
}

// This walk only measures bytes: it never cleans temps or changes an unsafe subtree.
fn accounting_bytes(fs: &dyn FileSystem, dir: &Path, errors: &mut Vec<io::Error>) -> io::Result<u64> {
    let entries = match fs.accounting_entries(dir) {
        Ok(entries) => entries,
        Err(error) => {
            report(errors, dir, error);
            return Ok(0);
        }
    };
    let mut total = 0u64;
    for entry in entries {
        let bytes = if entry.directory {
            accounting_bytes(fs, &entry.path, errors)?
        } else {
            entry.bytes
        };
        total = total.checked_add(bytes).ok_or_else(|| invalid("state size overflow"))?;
    }
    Ok(total)
}

fn unlink_closed(
    fs: &dyn FileSystem,
    dir: &Path,
    session: &str,
    config: &JournalConfig,
    errors: &mut Vec<io::Error>,
    evicted: &mut Vec<Evicted>,
) -> io::Result<()> {
    // Publish terminal state before deleting any history. A restart must never adopt the retained ring
    // as fresh history, including when deletion is interrupted part way through.
    match fs.read(&dir.join("identity")) {
        Ok(identity) => fs.replace(
            &dir.parent().unwrap().parent().unwrap().join("expired").join(session),
            &identity,
        )?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let lengths: BTreeMap<_, _> = fs
        .entries(dir)?
        .into_iter()
        .map(|entry| (entry.path, entry.bytes))
        .collect();
    let paths = segment_paths(fs, dir)?;
    for (index, (name_first, path)) in paths.iter().enumerate() {
        let range = match evicted_range(path)? {
            Some((first, last)) => (lengths.get(path) != Some(&0)).then_some((first, last, true)),
            None => match segment_range(fs, path, *name_first, index + 1 == paths.len(), config) {
                Ok(range) => range,
                Err(error) => {
                    report(errors, path, error);
                    None
                }
            },
        };
        match fs.remove_file(path) {
            Ok(()) => {
                if let Some((first, last, _)) = range {
                    evicted.push(Evicted {
                        session: session.into(),
                        first,
                        last,
                    });
                }
            }
            Err(error) => report(errors, path, error),
        }
    }
    let poison = dir.join("poisoned");
    if lengths.contains_key(&poison) {
        if let Err(error) = fs.remove_file(&poison) {
            report(errors, &poison, error);
        }
    }
    fs.sync_dir(dir)?;
    for name in ["identity", "closed-at"] {
        let path = dir.join(name);
        if lengths.contains_key(&path) {
            fs.remove_file(&path)?;
        }
    }
    let pending = dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("pending")
        .join(format!("{session}.bin"));
    match fs.remove_file(&pending) {
        Ok(()) => fs.sync_dir(pending.parent().unwrap())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs.remove_dir(dir)?;
    fs.sync_dir(dir.parent().unwrap())
}

fn enforce(
    state_dir: &Path,
    sessions: &[SessionUsage],
    config: &JournalConfig,
    free: &dyn FreeSpace,
    fs: &dyn FileSystem,
    clock: &dyn Clock,
) -> io::Result<BudgetOutcome> {
    let mut names = BTreeSet::new();
    for session in sessions {
        if !names.insert(&session.session) {
            return Err(invalid("session usage contains a duplicate session"));
        }
        if session.session.is_empty()
            || session.session == "."
            || session.session == ".."
            || !session
                .session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(invalid("session name must be one plain path component"));
        }
    }
    // Only the caller's root may be linked. Descendant directories retain the no-follow private-dir check.
    let root = fs.state_root(state_dir)?;
    let mut errors = Vec::new();
    let mut total = state_bytes(fs, &root, &mut errors)?;
    let mut available = free.available(state_dir)?;
    let mut closed: Vec<_> = sessions
        .iter()
        .filter_map(|session| session.closed_at.map(|at| (at, session)))
        .collect();
    closed.sort_by_key(|(at, _)| *at);
    let mut evicted = Vec::new();
    for (closed_at, session) in closed {
        let expired = clock
            .now()
            .duration_since(closed_at)
            .is_ok_and(|age| age >= config.closed_keep);
        if !expired && total <= config.cap_bytes && available >= config.free_floor_bytes {
            break;
        }
        let dir = root.join("journal").join(&session.session);
        let mut changed = false;
        let result = (|| -> io::Result<()> {
            // Traversal may skip an unsafe subtree; eviction must not enter it through a constructed path.
            match fs.entries(dir.parent().unwrap()) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
            let paths = match segment_paths(fs, &dir) {
                Ok(paths) => paths,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            if expired {
                changed = true;
                return unlink_closed(fs, &dir, &session.session, config, &mut errors, &mut evicted);
            }
            let lengths: BTreeMap<_, _> = fs
                .entries(&dir)?
                .into_iter()
                .map(|entry| (entry.path, entry.bytes))
                .collect();
            for (index, (name_first, path)) in paths.iter().enumerate() {
                if !expired && total <= config.cap_bytes && available >= config.free_floor_bytes {
                    break;
                }
                let range = if let Some(range) = evicted_range(path)? {
                    if lengths.get(path) == Some(&0) {
                        continue;
                    }
                    fs.read_at(path, 0, 0)?;
                    Some((range.0, range.1, true))
                } else {
                    segment_range(fs, path, *name_first, index + 1 == paths.len(), config)?
                };
                let Some((first, last, only_evicted)) = range else {
                    fs.remove_file(path)?;
                    changed = true;
                    fs.sync_dir(&dir)?;
                    continue;
                };
                let marker = evicted_path(&dir, first, last);
                if !only_evicted || *path != marker {
                    match fs.rename(path, &marker) {
                        Ok(()) => changed = true,
                        Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => {
                            changed = true;
                            return unlink_closed(fs, &dir, &session.session, config, &mut errors, &mut evicted);
                        }
                        Err(error) => return Err(error),
                    }
                    evicted.push(Evicted {
                        session: session.session.clone(),
                        first,
                        last,
                    });
                    // The durable name is the gap, including if truncation or its sync fails afterwards.
                    fs.sync_dir(&dir)?;
                }
                changed = true;
                fs.truncate(&marker, 0)?;
                fs.sync(&marker)?;
                total = state_bytes(fs, &root, &mut errors)?;
                available = free.available(state_dir)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            report(&mut errors, &dir, error);
        }
        // Recount after partial mutation too, but completed zero-byte markers need no new tree walk.
        if changed {
            match state_bytes(fs, &root, &mut errors) {
                Ok(bytes) => total = bytes,
                Err(error) => report(&mut errors, &root, error),
            }
            match free.available(state_dir) {
                Ok(bytes) => available = bytes,
                Err(error) => report(&mut errors, state_dir, error),
            }
        }
    }
    let live_bytes = sessions
        .iter()
        .filter(|session| session.closed_at.is_none())
        .fold(0u64, |total, session| total.saturating_add(session.bytes));
    let pressure = if available < config.free_floor_bytes {
        Some(BudgetPressure::BelowFloor { free: available })
    } else if total > config.cap_bytes || live_bytes > config.cap_bytes {
        Some(BudgetPressure::LiveOverCap { live_bytes })
    } else {
        None
    };
    if !errors.is_empty() || (!evicted.is_empty() && pressure.is_some()) {
        return Ok(BudgetOutcome::Partial {
            evicted,
            errors,
            pressure,
        });
    }
    Ok(match pressure {
        Some(BudgetPressure::BelowFloor { free }) => BudgetOutcome::BelowFloor { free },
        Some(BudgetPressure::LiveOverCap { live_bytes }) => BudgetOutcome::LiveOverCap { live_bytes },
        None if evicted.is_empty() => BudgetOutcome::Within,
        None => BudgetOutcome::Evicted(evicted),
    })
}

fn segment_range(
    fs: &dyn FileSystem,
    path: &Path,
    name_first: u64,
    final_segment: bool,
    config: &JournalConfig,
) -> io::Result<Option<(u64, u64, bool)>> {
    let bytes = fs.read(path)?;
    let mut offset = 0;
    let mut range: Option<(u64, u64)> = None;
    let mut only_evicted = true;
    while offset < bytes.len() {
        let Some((record, size)) = decode(path, &bytes, offset)? else {
            if !final_segment {
                return Err(corruption(path, offset, "torn record before the final segment"));
            }
            let expected = match range {
                Some((_, last)) => last.checked_add(1).ok_or_else(|| invalid("sequence overflow"))?,
                None => name_first,
            };
            validate_tail(path, &bytes, offset, expected, config.segment_bytes)?;
            break;
        };
        let (first, last) = bounds(&record);
        match range {
            None if first == name_first => range = Some((first, last)),
            Some((start, previous)) if previous.checked_add(1) == Some(first) => range = Some((start, last)),
            _ => return Err(corruption(path, offset, "closed segment is not dense")),
        }
        only_evicted &= matches!(
            record,
            Record::Gap {
                why: GapWhy::Evicted,
                ..
            }
        );
        offset += size;
    }
    Ok(range.map(|(first, last)| (first, last, only_evicted)))
}
