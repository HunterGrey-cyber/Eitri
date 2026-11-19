//! A terminal host's files in the daemon's run directory: its pid record (the daemon's), its exit record (the host's),
//! and what a starting daemon adopts from them.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;

use agent::private_fs;
use serde::{Deserialize, Serialize};

use super::super::sidecars;
use super::{Faults, HostRecord};
use crate::run_dir::Paths;

const RECORD_CAP: u64 = 64 * 1024;
const EXIT_CAP: u64 = 4 * 1024;

/// How a terminal's shell ended, written by its host as `th-<id>.exit` when it sees the exit. It outlives the host and
/// the daemon, so a later daemon still reports the exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// The shell let go of its terminal and still ran (`ExitInfo::detached`); it was hung up when the host ended.
    pub left: bool,
    pub exited_unix_ms: u64,
}

/// A terminal id: 128 bits as 32 lowercase hex digits.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn persist(paths: &Paths, record: &HostRecord) -> io::Result<()> {
    private_fs::publish_private_file(
        &private_fs::open_dir(&paths.metadata_run())?,
        OsStr::new(&format!("th-{}.pid", record.terminal_id)),
        &serde_json::to_vec(record).map_err(io::Error::other)?,
    )?;
    Ok(())
}

pub fn record_ids(paths: &Paths, _faults: &Faults) -> io::Result<Vec<String>> {
    let read = |path: &std::path::Path| std::fs::read_dir(path);
    #[cfg(test)]
    let read = _faults.read_record_dir.unwrap_or(read);
    let mut ids = Vec::new();
    for file in read(&paths.metadata_run())? {
        let name = file?.file_name();
        if let Some(id) = name.to_str().and_then(|s| s.strip_prefix("th-")?.strip_suffix(".pid")) {
            if valid_id(id) {
                ids.push(id.to_owned());
            }
        }
    }
    Ok(ids)
}

/// A refused record keeps the process identity and a valid open id for duplicate detection. Descriptor exhaustion leaves
/// adoption pending so a later pass can read the record and check the process.
pub fn read_record(paths: &Paths, id: &str, _faults: &Faults) -> io::Result<(HostRecord, Option<String>)> {
    let read = private_fs::read_private_nofollow_bytes_capped;
    #[cfg(test)]
    let read = _faults.host_record_read.unwrap_or(read);
    let path = paths.metadata_run().join(format!("th-{id}.pid"));
    let record = read(&path, RECORD_CAP).and_then(|bytes| serde_json::from_slice(&bytes).map_err(io::Error::other));
    let mut record: HostRecord = match record {
        Ok(record) => record,
        Err(error) if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) => return Err(error),
        Err(error) => {
            let why = format!("unreadable record: {error}");
            return Ok((
                HostRecord {
                    terminal_id: id.to_owned(),
                    failure: Some(why.clone()),
                    ..HostRecord::default()
                },
                Some(why),
            ));
        }
    };
    let socket = agent::socket_path::in_dir(&paths.run, &format!("th-{id}.sock")).ok();
    let valid_command = sidecars::validate_command_id(&record.command_id).is_ok();
    let refusal = if socket.as_ref() != Some(&record.socket) {
        Some("unadoptable record: another socket")
    } else if !valid_command {
        Some("unadoptable record: invalid command id")
    } else if record.start_dir.len() > 4096 || record.failure.as_ref().is_some_and(|s| s.len() > 4096) {
        Some("unadoptable record: oversized field")
    } else {
        None
    };
    record.terminal_id = id.to_owned();
    if let Some(why) = refusal {
        record.socket = socket.unwrap_or_default();
        if !valid_command {
            record.command_id.clear();
        }
        record.start_dir = String::new();
        record.ready = false;
        record.failure = Some(why.to_owned());
    }
    Ok((record, refusal.map(str::to_owned)))
}

/// The shell's identity its host recorded, when it did.
pub fn read_shell(paths: &Paths, id: &str) -> io::Result<Option<super::session::ShellRecord>> {
    let path = paths.metadata_run().join(format!("th-{id}.shell"));
    match private_fs::read_private_nofollow_bytes_capped(&path, EXIT_CAP) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The host's exit record, when it has written one.
pub fn read_exit(paths: &Paths, id: &str) -> Option<ExitRecord> {
    let path = paths.metadata_run().join(format!("th-{id}.exit"));
    if path.symlink_metadata().is_err() {
        return None;
    }
    let bytes = private_fs::read_private_nofollow_bytes_capped(&path, EXIT_CAP).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Pins a matching live process. An error means its identity could not be checked.
pub fn pin_record(record: &HostRecord) -> io::Result<Option<OwnedFd>> {
    if record.pid < 2
        || record.pid > i32::MAX as u32
        || record.boot_id.is_empty()
        || record.boot_id != super::current_boot_id()?
    {
        return Ok(None);
    }
    let fd = match sidecars::pin(record.pid) {
        Ok(fd) => fd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
        Err(error) => return Err(error),
    };
    let start = sidecars::identity_start_time(record.pid);
    if sidecars::exited(&fd)? {
        return Ok(None);
    }
    Ok((start? == record.start_time).then_some(fd))
}

/// Removes an expired host's records, logs and socket; a file already gone is not an error.
pub fn remove_files(paths: &Paths, id: &str) -> io::Result<()> {
    let remove = |path: std::path::PathBuf| match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    for extension in ["out", "err", "out.1", "err.1", "exit", "shell", "pid"] {
        remove(paths.metadata_run().join(format!("th-{id}.{extension}")))?;
    }
    if let Ok(socket) = agent::socket_path::in_dir(&paths.run, &format!("th-{id}.sock")) {
        remove(socket)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_record_from_another_boot_is_never_pinned() {
        let pid = std::process::id();
        let record = |boot_id: String| HostRecord {
            pid,
            start_time: sidecars::identity_start_time(pid).unwrap(),
            boot_id,
            socket: std::path::PathBuf::new(),
            terminal_id: String::new(),
            start_dir: String::new(),
            command_id: String::new(),
            started_unix_ms: 0,
            ready: true,
            failure: None,
            ended_unix_ms: None,
        };
        assert!(pin_record(&record(super::super::current_boot_id().unwrap()))
            .unwrap()
            .is_some());
        assert!(pin_record(&record(String::new())).unwrap().is_none());
        assert!(pin_record(&record("00000000-0000-0000-0000-000000000000".into()))
            .unwrap()
            .is_none());
    }
}
