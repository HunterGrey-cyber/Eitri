use std::io;
use std::path::{Path, PathBuf};

use agent::{private_fs, socket_path};

/// Where a daemon keeps its run directory, socket and metadata. A daemon is keyed by its installation (`key` is
/// [`run_key_of`] the installation directory's name), so two installations of one version (two builds of one commit, a sidecar update,
/// a release and an unsigned build) each run a daemon of their own, and a start from one never attaches to the
/// other's. A helper run from no installation (a build tree) is keyed by its version.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The daemon's version, as `Hello` and `info` report it.
    pub version: String,
    /// The name of its directory under `state/run/`: its installation's [`run_key_of`], or its version.
    pub key: String,
    pub data: PathBuf,
    pub state: PathBuf,
    pub run: PathBuf,
    /// Where the sidecars this daemon starts live: the installation this executable runs from (`<data>/<name>` with
    /// this process's executable at its `bin/eitri-server`), or, for a helper run from no installation (a build tree),
    /// `<data>/<version>-<rev7>`; `None` for the paths of another daemon.
    pub install: Option<PathBuf>,
    /// Whether `install` is the installation this executable runs from: only then does the daemon record it, key its
    /// run directory by it and stamp it as used.
    pub installed: bool,
}

/// What a daemon publishes in its `info`: what `list-daemons` reports, and the installation it runs from, which a
/// stop checks the daemon's executable against.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Published {
    #[serde(flatten)]
    pub info: eitri_server_proto::probe::DaemonInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation: Option<String>,
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
    /// The paths of the daemon keyed `key` (another one's, for `connect --daemon` and `stop-daemon`), from the
    /// environment's homes.
    pub fn from_env(key: &str) -> io::Result<Self> {
        let (data_home, state_home) = environment_roots()?;
        Self::new(key, &data_home, &state_home, &std::env::temp_dir())
    }

    /// The paths of this helper's own daemon: keyed by the installation it runs from ([`run_key_of`]), found from its
    /// executable.
    pub fn own_from_env() -> io::Result<Self> {
        let (data_home, state_home) = environment_roots()?;
        let data = data_home.join("eitri-server");
        let install = std::env::current_exe()
            .ok()
            .and_then(|exe| installation_of(&exe, &data))
            .filter(|(_, version, revision)| version == crate::VERSION && *revision == rev7(crate::REVISION))
            .map(|(install, _, _)| install);
        Self::own(install, &data_home, &state_home, &std::env::temp_dir())
    }

    /// The paths of a daemon of this build running from `install` (`None`: from no installation, keyed by version).
    pub fn own(install: Option<PathBuf>, data_home: &Path, state_home: &Path, tmp: &Path) -> io::Result<Self> {
        let key = match &install {
            Some(install) => run_key_of(
                install
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| crate::invalid("installation name must be UTF-8"))?,
            ),
            None => crate::VERSION.to_owned(),
        };
        let mut paths = Self::new(&key, data_home, state_home, tmp)?;
        paths.version = crate::VERSION.to_owned();
        paths.installed = install.is_some();
        paths.install =
            Some(install.unwrap_or_else(|| paths.data.join(format!("{}-{}", crate::VERSION, rev7(crate::REVISION)))));
        Ok(paths)
    }

    /// The paths of the daemon keyed `key`. Its version is read as the key until its `info` says otherwise.
    pub fn new(key: &str, data_home: &Path, state_home: &Path, tmp: &Path) -> io::Result<Self> {
        validate_version(key)?;
        let state = state_home.join("eitri-server");
        let (run, fallback) = select_run(key, &state, tmp)?;
        if fallback {
            private_fs::require_safe_parent(tmp, "the helper run directory")?;
        }
        Ok(Self {
            version: key.to_owned(),
            key: key.to_owned(),
            data: data_home.join("eitri-server"),
            state,
            run,
            install: None,
            installed: false,
        })
    }

    /// The name of the installation this daemon runs from, as its `info` records it.
    pub fn installation_name(&self) -> Option<String> {
        self.install
            .as_ref()
            .filter(|_| self.installed)
            .and_then(|install| install.file_name())
            .and_then(|name| name.to_str())
            .map(str::to_owned)
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
        self.state.join("run").join(&self.key)
    }

    pub fn socket(&self) -> io::Result<PathBuf> {
        socket_path::in_dir(&self.run, "daemon.sock")
    }

    /// The daemon's `info` with the installation it records, checked against the key it is kept under.
    pub fn published(&self) -> io::Result<Published> {
        let text = private_fs::read_private_nofollow_capped(&self.metadata_run().join("info"), 16 * 1024)?;
        let published: Published = serde_json::from_str(&text).map_err(|error| crate::invalid(error.to_string()))?;
        let info = &published.info;
        let keyed_as = match &published.installation {
            Some(installation) => run_key_of(installation),
            None => info.version.clone(),
        };
        if keyed_as != self.key || info.pid < 2 || info.pid > i32::MAX as u32 {
            return Err(crate::invalid("invalid daemon identity"));
        }
        if info.rev.is_empty() || !info.rev.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(crate::invalid("invalid daemon revision"));
        }
        Ok(published)
    }
}

/// The run key of a daemon of the installation `installation`: `i-<16 hex>` of the name's SHA-256, short enough that
/// the longest sidecar socket below it fits the socket path limit where the full name would not.
pub fn run_key_of(installation: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("eitri-server-installation\0{installation}").as_bytes());
    format!("i-{}", &format!("{digest:x}")[..16])
}

/// The version and seven-character revision an installation directory's name holds: `<version>-<rev7>`, or
/// `<version>-<rev7>-<16 hex>-<16 hex>`, where the two hashes (of the helper and of the installation's manifest) keep
/// two builds of one revision apart.
pub fn installation_identity(name: &str) -> Option<(&str, &str)> {
    let hex16 = |part: &str| part.len() == 16 && part.bytes().all(|b| b.is_ascii_hexdigit());
    let keyed = name
        .rsplit_once('-')
        .filter(|(_, manifest)| hex16(manifest))
        .and_then(|(rest, _)| rest.rsplit_once('-'))
        .filter(|(_, helper)| hex16(helper))
        .map(|(rest, _)| rest);
    let (version, revision) = keyed.unwrap_or(name).rsplit_once('-')?;
    let valid = validate_version(version).is_ok()
        && !revision.is_empty()
        && revision.bytes().all(|byte| byte.is_ascii_alphanumeric());
    valid.then_some((version, revision))
}

/// The installation `exe` runs from when it is `<data>/<installation>/bin/eitri-server` (as `/proc/<pid>/exe` reads,
/// so also after the file was replaced in place): the installation directory, its version and its revision. `data`
/// is resolved first, since an executable's path has no symbolic links in it.
pub fn installation_of(exe: &Path, data: &Path) -> Option<(PathBuf, String, String)> {
    let name = exe.file_name()?.to_str()?;
    if name != "eitri-server" && name != "eitri-server (deleted)" {
        return None;
    }
    let bin = exe.parent()?;
    let install = bin.parent()?;
    if bin.file_name()? != "bin" || !exe.is_absolute() || install.parent()? != data.canonicalize().ok()? {
        return None;
    }
    let (version, revision) = installation_identity(install.file_name()?.to_str()?)?;
    Some((install.to_path_buf(), version.to_owned(), revision.to_owned()))
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

fn select_run(key: &str, state: &Path, tmp: &Path) -> io::Result<(PathBuf, bool)> {
    let ordinary = state.join("run").join(key);
    if socket_path::in_dir(&ordinary, "sc-0123456789abcdef.sock").is_ok() {
        return Ok((ordinary, false));
    }
    let fallback = tmp.join(format!("eitri-server-{}", private_fs::euid())).join(key);
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
    fn installations_are_named_by_version_revision_and_content() {
        assert_eq!(installation_identity("0.3.0-abcdef0"), Some(("0.3.0", "abcdef0")));
        assert_eq!(
            installation_identity("0.3.0-abcdef0-0123456789abcdef-fedcba9876543210"),
            Some(("0.3.0", "abcdef0"))
        );
        assert_eq!(
            installation_identity("0.3.0-rc.1-abcdef0-0123456789abcdef-fedcba9876543210"),
            Some(("0.3.0-rc.1", "abcdef0"))
        );
        for bad in ["", "abcdef0", ".install-0123", "-abcdef0", "0.3.0-", "0.3.0-abc/def"] {
            assert_eq!(installation_identity(bad), None, "{bad:?}");
        }
        let data = std::env::temp_dir().join(format!("rd-install-{}", std::process::id()));
        std::fs::create_dir_all(&data).unwrap();
        let canonical = data.canonicalize().unwrap();
        let keyed = canonical.join("0.3.0-abcdef0-0123456789abcdef-fedcba9876543210");
        for exe in [keyed.join("bin/eitri-server"), keyed.join("bin/eitri-server (deleted)")] {
            assert_eq!(
                installation_of(&exe, &data),
                Some((keyed.clone(), "0.3.0".into(), "abcdef0".into()))
            );
        }
        for exe in [
            keyed.join("bin/other"),
            keyed.join("libexec/eitri-server"),
            canonical.join("bin/eitri-server"),
            PathBuf::from("/usr/bin/eitri-server"),
            canonical.join("x/0.3.0-abcdef0/bin/eitri-server"),
        ] {
            assert_eq!(installation_of(&exe, &data), None, "{}", exe.display());
        }
        std::fs::remove_dir_all(data).unwrap();
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
