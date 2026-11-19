//! A private git entry point, contributed only to the embedded editor's environment.

use crate::remote_project::WindowOrigin;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub const SCRIPT: &str = "#!/bin/sh\nexec \"$EITRI_GIT_SHIM_EXE\" host git -- \"$@\"\n";
pub const OPTIONS_ENV: &str = "EITRI_GIT_SHIM_OPTIONS";
pub const OPTIONS_FILE_ENV: &str = "EITRI_GIT_SHIM_OPTIONS_FILE";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShimOptions {
    pub enabled: bool,
    pub flush_cap: f64,
    pub mutagen: Option<PathBuf>,
}

impl Default for ShimOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            flush_cap: 3.0,
            mutagen: None,
        }
    }
}

impl ShimOptions {
    pub fn from_process() -> Result<Self, String> {
        let value = if let Some(path) = std::env::var_os(OPTIONS_FILE_ENV) {
            Some(
                agent::private_fs::read_private_nofollow_bytes_capped(std::path::Path::new(&path), 64 * 1024)
                    .map_err(|e| format!("git shim settings are unavailable ({e})"))?,
            )
        } else {
            None
        };
        let options: Self = if let Some(value) = value {
            serde_json::from_slice(&value).map_err(|e| format!("invalid git shim settings ({e})"))?
        } else {
            match std::env::var(OPTIONS_ENV) {
                Ok(value) => serde_json::from_str(&value).map_err(|e| format!("invalid git shim options: {e}"))?,
                Err(std::env::VarError::NotPresent) => Self::default(),
                Err(error) => return Err(error.to_string()),
            }
        };
        if !options.flush_cap.is_finite()
            || !(0.5..=30.0).contains(&options.flush_cap)
            || options.mutagen.as_ref().is_some_and(|p| !p.is_absolute())
        {
            return Err("invalid git shim options".into());
        }
        Ok(options)
    }
}

pub struct ShimDir {
    dir: PathBuf,
    exe: PathBuf,
    real_git: PathBuf,
    options: ShimOptions,
}

impl ShimDir {
    /// Filesystem preparation belongs before GTK starts. The directory stays alive with the window.
    pub fn prepare(exe: PathBuf, real_git: PathBuf, options: ShimOptions) -> io::Result<Self> {
        if !exe.is_absolute() || !real_git.is_absolute() {
            return Err(io::Error::other("git shim executables must be absolute"));
        }
        let base = crate::instance_dir::feed_base("git shim")?;
        Self::prepare_at(&base, exe, real_git, options)
    }

    fn prepare_at(base: &std::path::Path, exe: PathBuf, real_git: PathBuf, options: ShimOptions) -> io::Result<Self> {
        let dir = crate::instance_dir::create_instance_dir(base, "eitri-git-", "", "git shim")?;
        let result = (|| {
            let bin = dir.join("bin");
            agent::private_fs::create_private_dir_all(&bin, &dir)?;
            let path = bin.join("git");
            use std::io::Write;
            let mut file = agent::private_fs::open_private(&path)?;
            file.write_all(SCRIPT.as_bytes())?;
            file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            Ok(Self {
                dir: dir.clone(),
                exe,
                real_git,
                options,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        result
    }

    pub fn options_path(&self) -> PathBuf {
        self.dir.join("options.json")
    }

    /// Merge into the already composed environment, replacing its PATH exactly once and keeping pane-switch.
    pub fn compose_child_env(
        &self,
        origin: &WindowOrigin,
        env: &mut Vec<(String, String)>,
        inherited_path: Option<&OsStr>,
    ) -> Result<(), String> {
        if matches!(origin, WindowOrigin::Local) {
            return Ok(());
        }
        let old = env
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| OsStr::new(value))
            .or(inherited_path);
        let mut entries = vec![self.dir.join("bin")];
        if let Some(old) = old {
            entries.extend(std::env::split_paths(old));
        }
        let path = std::env::join_paths(entries)
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|_| "git shim PATH is not UTF-8")?;
        let exe = self.exe.to_str().ok_or("git shim executable is not UTF-8")?.to_owned();
        let real_git = self
            .real_git
            .to_str()
            .ok_or("real git executable is not UTF-8")?
            .to_owned();
        let options = serde_json::to_string(&self.options).map_err(|e| e.to_string())?;
        env.retain(|(key, _)| {
            !matches!(
                key.as_str(),
                "PATH" | "EITRI_GIT_SHIM_EXE" | "EITRI_REAL_GIT" | OPTIONS_ENV
            )
        });
        env.extend([
            ("PATH".into(), path),
            ("EITRI_GIT_SHIM_EXE".into(), exe),
            ("EITRI_REAL_GIT".into(), real_git),
            (OPTIONS_ENV.into(), options),
        ]);
        Ok(())
    }
}

impl Drop for ShimDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The git shim is the product binary's early host arm.
pub fn shim_executable(product: &std::path::Path) -> PathBuf {
    product.to_owned()
}

pub fn locate_real_git(path: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .and_then(|path| path.canonicalize().ok())
}

pub fn real_git_from_process() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("EITRI_REAL_GIT")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(path);
    }
    locate_real_git(&std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")))
        .ok_or_else(|| "git was not found on PATH".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_script_is_fixed_text() {
        assert_eq!(SCRIPT, "#!/bin/sh\nexec \"$EITRI_GIT_SHIM_EXE\" host git -- \"$@\"\n");
        let shim = ShimDir::prepare("/usr/bin/eitri".into(), "/usr/bin/git".into(), ShimOptions::default()).unwrap();
        assert_eq!(std::fs::read_to_string(shim.dir.join("bin/git")).unwrap(), SCRIPT);
        assert_eq!(
            std::fs::metadata(shim.dir.join("bin/git"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn nvim_child_path_has_the_shim_first_in_remote_and_local_copy_windows_and_keeps_pane_switch() {
        let shim = ShimDir::prepare("/usr/bin/eitri".into(), "/usr/bin/git".into(), ShimOptions::default()).unwrap();
        let remote: crate::remote_project::RemoteProject = serde_json::from_value(serde_json::json!({
            "version":1,"host":"example-host","host_key_sha256":"SHA256:example","root":"/srv/project","helper_version":"0.4.0","sync":null,"created_at":0
        })).unwrap();
        for origin in [
            WindowOrigin::Remote(remote),
            WindowOrigin::LocalCopy {
                host: "example-host".into(),
            },
            WindowOrigin::Local,
        ] {
            let mut env = vec![
                ("PATH".into(), "/srv/pane-switch/bin:/usr/bin".into()),
                ("TMUX".into(), "kept".into()),
            ];
            shim.compose_child_env(&origin, &mut env, Some(OsStr::new("/bin")))
                .unwrap();
            assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
            let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
            let expected = if matches!(origin, WindowOrigin::Local) {
                "/srv/pane-switch/bin:/usr/bin".into()
            } else {
                format!("{}:/srv/pane-switch/bin:/usr/bin", shim.dir.join("bin").display())
            };
            assert_eq!(path, &expected);
            assert!(env.contains(&("TMUX".into(), "kept".into())));
        }
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn directory_creation_failure_returns_an_error_and_installs_nothing() {
        let root = agent::state_dirs::test_workspace_dir("git-shim-directory-failure");
        std::fs::create_dir_all(&root).unwrap();
        let base = root.join("file");
        std::fs::write(&base, "occupied").unwrap();
        let error = ShimDir::prepare_at(
            &base,
            "/usr/bin/eitri".into(),
            "/usr/bin/git".into(),
            ShimOptions::default(),
        )
        .err()
        .expect("creation must fail");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::NotADirectory | io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shim_executable_always_uses_the_product_even_with_an_executable_sibling() {
        let root = agent::state_dirs::test_workspace_dir("git-shim-product");
        std::fs::create_dir_all(&root).unwrap();
        let sibling = root.join("eitri-git-shim");
        std::fs::write(&sibling, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o700)).unwrap();
        let product = root.join("shell");
        assert_eq!(shim_executable(&product), product);
        std::fs::remove_dir_all(root).unwrap();
    }
}
