use std::io;
use std::path::{Path, PathBuf};

use agent::{private_fs, socket_path};

#[derive(Clone, Debug)]
pub struct Paths {
    pub version: String,
    pub data: PathBuf,
    pub state: PathBuf,
    pub run: PathBuf,
}

pub fn rev7(revision: &str) -> String {
    revision.chars().take(7).collect()
}

pub fn validate_version(version: &str) -> io::Result<()> {
    if version.len() <= 64
        && version.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        Ok(())
    } else {
        Err(crate::invalid("invalid daemon version"))
    }
}

impl Paths {
    pub fn from_env(version: &str) -> io::Result<Self> {
        let (data_home, state_home) = environment_roots()?;
        Self::new(version, &data_home, &state_home, &std::env::temp_dir())
    }

    pub fn new(version: &str, data_home: &Path, state_home: &Path, tmp: &Path) -> io::Result<Self> {
        validate_version(version)?;
        let state = state_home.join("eitri-server");
        let (run, fallback) = select_run(version, &state, tmp)?;
        if fallback {
            private_fs::require_safe_parent(tmp, "the helper run directory")?;
        }
        Ok(Self {
            version: version.to_owned(),
            data: data_home.join("eitri-server"),
            state,
            run,
        })
    }

    pub fn prepare(&self) -> io::Result<()> {
        private_fs::create_private_dir_all(&self.state.join("run"), &self.state)?;
        for name in ["journal", "pending"] {
            private_fs::create_private_dir_all(&self.state.join(name), &self.state)?;
        }
        let metadata = self.metadata_run();
        private_fs::create_private_dir_all(&metadata, &self.state)?;
        let root = if self.run == metadata {
            self.state.clone()
        } else {
            self.run.parent().expect("version parent").to_path_buf()
        };
        private_fs::create_private_dir_all(&self.run, &root)?;
        if !private_fs::is_private_dir(&root)
            || !private_fs::is_private_dir(&self.run)
            || !private_fs::is_private_dir(&metadata)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "helper run directories must be private real directories",
            ));
        }
        Ok(())
    }

    /// Persistent metadata and lifetime locks never move into the temporary socket namespace.
    pub fn metadata_run(&self) -> PathBuf {
        self.state.join("run").join(&self.version)
    }

    pub fn socket(&self) -> io::Result<PathBuf> {
        socket_path::in_dir(&self.run, "daemon.sock")
    }

    pub fn install(&self, revision: &str) -> PathBuf {
        self.data.join(format!("{}-{}", self.version, rev7(revision)))
    }

    pub fn info(&self) -> io::Result<eitri_server_proto::probe::DaemonInfo> {
        let text = private_fs::read_private_nofollow_capped(&self.metadata_run().join("info"), 16 * 1024)?;
        let info: eitri_server_proto::probe::DaemonInfo =
            serde_json::from_str(&text).map_err(|error| crate::invalid(error.to_string()))?;
        if info.version != self.version || info.pid < 2 || info.pid > i32::MAX as u32 {
            return Err(crate::invalid("invalid daemon identity"));
        }
        if info.rev.is_empty() || !info.rev.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(crate::invalid("invalid daemon revision"));
        }
        Ok(info)
    }
}

pub fn environment_roots() -> io::Result<(PathBuf, PathBuf)> {
    let xdg = |key, fallback| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(fallback))
            })
            .ok_or_else(|| crate::invalid(format!("{key} or HOME must be absolute")))
    };
    Ok((
        xdg("XDG_DATA_HOME", ".local/share")?,
        xdg("XDG_STATE_HOME", ".local/state")?,
    ))
}

fn select_run(version: &str, state: &Path, tmp: &Path) -> io::Result<(PathBuf, bool)> {
    let ordinary = state.join("run").join(version);
    if socket_path::in_dir(&ordinary, "sc-0123456789abcdef.sock").is_ok() {
        return Ok((ordinary, false));
    }
    let fallback = tmp.join(format!("eitri-server-{}", private_fs::euid())).join(version);
    socket_path::in_dir(&fallback, "sc-0123456789abcdef.sock")?;
    Ok((fallback, true))
}

pub fn pid_alive(pid: u32) -> bool {
    if pid < 2 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal zero only probes the process; pid has been range checked.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_reserves_the_longest_socket_not_just_the_daemon_socket() {
        let suffix = "/eitri-server/run/1";
        let home_len = socket_path::MAX_SOCKET_PATH_BYTES - 1 - "sc-0123456789abcdef.sock".len() - suffix.len();
        let state_home = PathBuf::from(format!("/{}", "x".repeat(home_len - 1)));
        // These are path calculations only; no directory or socket is created.
        let fits = Paths::new("1", Path::new("/data"), &state_home, Path::new("/")).unwrap();
        assert!(fits.run.starts_with(&state_home));
        assert_eq!(
            socket_path::in_dir(&fits.run, "sc-0123456789abcdef.sock")
                .unwrap()
                .as_os_str()
                .len(),
            103
        );
        let longer = PathBuf::from(format!("{}x", state_home.display()));
        let (fallback, used_fallback) = select_run("1", &longer.join("eitri-server"), Path::new("/")).unwrap();
        assert!(used_fallback);
        assert_eq!(
            fallback,
            PathBuf::from(format!("/eitri-server-{}/1", private_fs::euid()))
        );
        assert!(socket_path::in_dir(&longer.join("eitri-server/run/1"), "daemon.sock").is_ok());
    }

    #[test]
    fn daemon_versions_cannot_escape_the_run_directory() {
        for invalid in ["", ".", "..", "../1", "/1", "1/2", "-1", "1\n", "1 2"] {
            assert!(validate_version(invalid).is_err(), "{invalid:?}");
        }
        for valid in ["0.3.0", "1.0.0-alpha+build", "older"] {
            validate_version(valid).unwrap();
        }
    }
    #[test]
    fn fallback_refuses_a_symlink_root_and_keeps_metadata_in_state() {
        let root = PathBuf::from(format!("/tmp/rd-unit-{}", std::process::id()));
        private_fs::create_private_dir_all(&root.join("tmp"), &root).unwrap();
        let paths = Paths::new(
            "test",
            &root.join("d"),
            &root.join("long".repeat(30)),
            &root.join("tmp"),
        )
        .unwrap();
        assert!(!paths.run.starts_with(&paths.state));
        assert_eq!(paths.metadata_run(), paths.state.join("run/test"));
        let fallback_root = paths.run.parent().unwrap();
        let target = root.join("target");
        private_fs::create_private_dir_all(&target, &root).unwrap();
        std::os::unix::fs::symlink(&target, fallback_root).unwrap();
        assert!(paths.prepare().is_err());
        std::fs::remove_file(fallback_root).unwrap();
        paths.prepare().unwrap();
        assert!(private_fs::is_private_dir(fallback_root));
        assert!(private_fs::is_private_dir(&paths.run));
        assert!(private_fs::is_private_dir(&paths.metadata_run()));
        std::fs::remove_dir_all(root).unwrap();
    }
}

static INHERITED_UMASK: std::sync::OnceLock<libc::mode_t> = std::sync::OnceLock::new();

/// Called before runtime threads are started; the helper keeps its caller's process-wide mask.
pub fn save_inherited_umask() {
    let mask = unsafe {
        let mask = libc::umask(0);
        libc::umask(mask);
        mask
    };
    let _ = INHERITED_UMASK.set(mask);
}

pub fn inherited_umask() -> libc::mode_t {
    *INHERITED_UMASK.get_or_init(|| {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Umask:").map(str::trim).map(str::to_owned))
            })
            .and_then(|s| u32::from_str_radix(&s, 8).ok())
            .unwrap_or(0o022)
    })
}
