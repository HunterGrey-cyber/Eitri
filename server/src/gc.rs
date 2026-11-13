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
    let running = paths.install(crate::REVISION);
    for entry in std::fs::read_dir(&paths.data)? {
        let entry = entry?;
        let path = entry.path();
        if path == running || !private_fs::is_private_dir(&path) {
            continue;
        }
        let name = entry.file_name();
        let Some((version, revision)) = name.to_str().and_then(|name| name.rsplit_once('-')) else {
            continue;
        };
        if crate::run_dir::validate_version(version).is_err()
            || revision.is_empty()
            || !revision.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            continue;
        }
        let mut candidate = paths.clone();
        candidate.version = version.to_owned();
        let live = candidate.info().is_ok_and(|info| pid_alive(info.pid));
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

pub fn touch_install(paths: &Paths) -> io::Result<()> {
    private_fs::create_private_dir_all(&paths.data, &paths.data)?;
    let lock = private_fs::PrivateLock::open(&paths.data.join("gc.lock"))?;
    crate::audit::lock_exclusive(&lock)?;
    let install = paths.install(crate::REVISION);
    private_fs::create_private_dir_all(&install.join("bin"), &paths.data)?;
    private_fs::create_private_dir_all(
        &install
            .join("sidecar")
            .join(crate::run_dir::rev7(agent::BUNDLED_SIDECAR_REVISION)),
        &paths.data,
    )?;
    let stamp = install.join("last-used");
    if !stamp.exists() {
        crate::audit::record(
            &paths.state,
            "install-seen",
            &format!("version={} rev={}", paths.version, crate::REVISION),
        )?;
    }
    private_fs::write_private(&stamp, crate::run_dir::unix_ms().to_string().as_bytes())
}
