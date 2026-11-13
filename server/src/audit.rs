use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

use agent::private_fs;

const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn record(state: &Path, event: &str, fields: &str) -> io::Result<()> {
    private_fs::create_private_dir_all(state, state)?;
    let lock = private_fs::PrivateLock::open(&state.join("audit.lock"))?;
    lock_exclusive(&lock)?;
    let path = state.join("audit.log");
    let line = format!(
        "{} {event} {}\n",
        crate::run_dir::unix_ms(),
        fields.replace(['\n', '\r'], " ")
    );
    if line.len() as u64 > MAX_BYTES {
        return Err(crate::invalid("audit entry exceeds the audit log cap"));
    }
    let mut file = private_fs::append_private(&path)?;
    if file.metadata()?.len() + line.len() as u64 > MAX_BYTES {
        drop(file);
        std::fs::rename(&path, state.join("audit.log.1"))?;
        file = private_fs::append_private(&path)?;
    }
    file.write_all(line.as_bytes())?;
    file.sync_data()
}

pub fn lock_exclusive(lock: &private_fs::PrivateLock) -> io::Result<()> {
    loop {
        // SAFETY: the lock owns the descriptor, and flock does not access Rust memory.
        if unsafe { libc::flock(lock.as_file().as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn audit_is_private_bounded_and_rotates_once() {
        let root = std::env::temp_dir().join(format!("audit-{}", std::process::id()));
        record(&root, "daemon-start", "version=1\nforged-event").unwrap();
        let path = root.join("audit.log");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let line = private_fs::read_private_nofollow_capped(&path, 1024).unwrap();
        assert_eq!(line.lines().count(), 1);
        private_fs::append_private(&path).unwrap().set_len(MAX_BYTES).unwrap();
        record(&root, "daemon-stop", "pid=42").unwrap();
        assert_eq!(std::fs::metadata(root.join("audit.log.1")).unwrap().len(), MAX_BYTES);
        private_fs::append_private(&path).unwrap().set_len(MAX_BYTES).unwrap();
        record(&root, "daemon-start", "pid=43").unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_BYTES);
        assert!(!root.join("audit.log.2").exists());
        assert!(private_fs::is_private_dir(&root));
        std::fs::remove_dir_all(root).unwrap();
    }
}
