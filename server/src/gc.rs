use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use agent::private_fs;

use crate::run_dir::{pid_alive, Paths};

const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub fn collect(paths: &Paths, now: SystemTime) -> io::Result<()> {
    if !paths.data.exists() {
        return Ok(());
    }
    // Installation and collection share a lock so a refresh cannot race removal.
    let lock = private_fs::PrivateLock::open(&paths.data.join("gc.lock"))?;
    crate::audit::lock_exclusive(&lock)?;
    let running = paths.install.as_deref().filter(|_| paths.installed);
    for entry in std::fs::read_dir(&paths.data)? {
        let entry = entry?;
        let path = entry.path();
        if running.is_some_and(|running| running.file_name() == path.file_name()) || !private_fs::is_private_dir(&path)
        {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((version, _)) = crate::run_dir::installation_identity(name) else {
            continue;
        };
        // A live daemon keyed by this installation, or one of its version from a helper that keyed daemons by
        // version alone (it records no installation), keeps it. Surviving sidecars keep their adoption artifacts too.
        let own_key = crate::run_dir::run_key_of(name);
        let live = [own_key.as_str(), version].into_iter().any(|key| {
            let mut candidate = paths.clone();
            candidate.key = key.to_owned();
            candidate.version = key.to_owned();
            candidate.published().is_ok_and(|published| {
                published
                    .installation
                    .as_deref()
                    .is_none_or(|installation| installation == name)
                    && pid_alive(published.info.pid)
            }) || crate::daemon::sidecars::recorded_alive_in(&candidate.metadata_run()).unwrap_or(true)
        });
        if live {
            continue;
        }
        if expired(&path.join("last-used"), now) {
            std::fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

fn expired(stamp: &Path, now: SystemTime) -> bool {
    private_fs::read_private_nofollow_capped(stamp, 128)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .and_then(|ms| SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(ms)))
        .and_then(|time| now.duration_since(time).ok())
        .is_some_and(|age| age >= RETENTION)
}

/// Stamps the installation this helper runs from as used now. A helper run from anywhere else (a build tree) has no
/// installation to keep.
pub fn touch_install(paths: &Paths) -> io::Result<()> {
    let Some(install) = paths.install.as_ref().filter(|_| paths.installed) else {
        return Ok(());
    };
    let lock = private_fs::PrivateLock::open(&paths.data.join("gc.lock"))?;
    crate::audit::lock_exclusive(&lock)?;
    let stamp = install.join("last-used");
    if !stamp.exists() {
        let _ = crate::audit::record(
            &paths.state,
            "install-seen",
            &format!("version={} rev={}", paths.version, crate::REVISION),
        );
    }
    private_fs::write_private(&stamp, crate::run_dir::unix_ms().to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dormant_daemon_keeps_its_installation_while_a_recorded_sidecar_runs() {
        let root = std::env::temp_dir().join(format!("gc-live-{}-{}", std::process::id(), crate::run_dir::unix_ms()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let install = root.join("data/0.3.0-abcdef0");
        private_fs::create_private_dir_all(&install, &root).unwrap();
        let paths = Paths {
            version: "0.3.0".into(),
            key: "current".into(),
            data: root.join("data"),
            state: root.join("state"),
            run: root.join("run"),
            install: Some(install),
            installed: true,
        };
        paths.prepare().unwrap();
        let older = "0.2.0-abcdef0";
        let old = paths.data.join(older);
        private_fs::create_private_dir_all(&old, &paths.data).unwrap();
        private_fs::write_private(&old.join("last-used"), b"0").unwrap();
        let run = paths.state.join("run").join(crate::run_dir::run_key_of(older));
        private_fs::create_private_dir_all(&run, &paths.state).unwrap();
        let pid = std::process::id();
        let mut record = crate::daemon::sidecars::Record {
            pid,
            start_time: crate::daemon::sidecars::start_time(pid).unwrap(),
            socket: std::path::PathBuf::new(),
            sidecar_id: "0123456789abcdef".into(),
            root: "/srv/project".into(),
            account: String::new(),
            command_id: "kept".into(),
            started_unix_ms: 0,
            ready: true,
            failure: None,
            exited_unix_ms: None,
            started: None,
        };
        let file = run.join("sc-0123456789abcdef.pid");
        private_fs::write_private(&file, &serde_json::to_vec(&record).unwrap()).unwrap();
        let now = SystemTime::UNIX_EPOCH + RETENTION + Duration::from_secs(1);
        collect(&paths, now).unwrap();
        assert!(old.exists());
        record.start_time += 1;
        private_fs::write_private(&file, &serde_json::to_vec(&record).unwrap()).unwrap();
        collect(&paths, now).unwrap();
        assert!(!old.exists());
    }
}
