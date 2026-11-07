//! LuaLS definitions for the configuration API, installed without changing the user's editor config.
//! All file-system work here blocks and belongs on a worker thread when called by a window.

use std::ffi::OsString;
use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use agent::private_fs::{create_new_file_at, open_user_config_dir, CreateRefusal};

pub const META: &str = include_str!("eitri.meta.lua");
const META_SUBDIR: &str = "lua/meta";
const LUARC: &str = ".luarc.json";
const LUARC_COMMENTS: &str = ".luarc.jsonc";
const USAGE: &str = "usage: eitri setup lua-ls (also lua_ls or luals)";

#[derive(Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Written,
    Unchanged,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LuarcOutcome {
    Created(PathBuf),
    Existing { path: PathBuf, line: String },
}

pub fn install_meta(data_dir: &Path) -> io::Result<InstallOutcome> {
    install_bytes(data_dir, META.as_bytes())
}

/// Replaces a different version as one rename; an unchanged file keeps its inode and timestamps.
fn install_bytes(data_dir: &Path, bytes: &[u8]) -> io::Result<InstallOutcome> {
    require_absolute(data_dir)?;
    let dir = data_dir.join(META_SUBDIR);
    let path = dir.join("eitri.lua");
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) && fs::read(&path)? == bytes {
        return Ok(InstallOutcome::Unchanged);
    }
    public_dirs(&dir)?;
    let temporary = dir.join(format!(".eitri-{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temporary)?;
    let result = (|| {
        file.set_permissions(Permissions::from_mode(0o644))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        Ok(InstallOutcome::Written)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Creates only missing directories, leaving the modes of existing user directories alone.
fn public_dirs(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        public_dirs(parent)?;
    }
    match fs::DirBuilder::new().mode(0o755).create(path) {
        Ok(()) => fs::set_permissions(path, Permissions::from_mode(0o755)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

fn require_absolute(path: &Path) -> io::Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an absolute path", path.display()),
        ))
    }
}

/// Installs the matching API first, then creates a workspace file beside Eitri's init.lua, never over one.
/// The only destinations are these two directories; no Neovim configuration path is resolved here.
pub fn write_luarc(config_dir: &Path, data_dir: &Path) -> io::Result<LuarcOutcome> {
    require_absolute(config_dir)?;
    install_meta(data_dir)?;
    let dir =
        open_user_config_dir(config_dir).map_err(|e| io::Error::other(format!("{}: {e}", config_dir.display())))?;
    let library = data_dir.join(META_SUBDIR);
    // Path's serializer refuses non-UTF-8, rather than installing a lossy path that LuaLS cannot find.
    let line = format!("\"workspace.library\": {}", serde_json::to_string(&[&library])?);
    let existing = |name: &str| LuarcOutcome::Existing {
        path: config_dir.join(name),
        line: line.clone(),
    };
    for name in [LUARC, LUARC_COMMENTS] {
        if exists_at(dir.as_fd(), name)? {
            return Ok(existing(name));
        }
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "workspace.library": [library], "runtime.version": "LuaJIT"
    }))?;
    match create_new_file_at(dir.as_fd(), LUARC, &bytes, 0o644) {
        Ok(()) => Ok(LuarcOutcome::Created(config_dir.join(LUARC))),
        Err(CreateRefusal::Exists) => Ok(existing(LUARC)),
        Err(CreateRefusal::CannotCreateSafely) => {
            Err(io::Error::other("this file system cannot safely create .luarc.json"))
        }
        Err(CreateRefusal::Io(error)) => Err(error),
    }
}

/// A link, directory or other entry counts as an existing workspace too, even a dangling link.
fn exists_at(dir: BorrowedFd<'_>, name: &str) -> io::Result<bool> {
    let name = std::ffi::CString::new(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the directory handle, NUL-terminated name and stat storage all outlive the call.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(error)
    }
}

/// The absolute XDG data base, or the home fallback, with Eitri's directory appended.
pub fn data_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
    if let Some(base) = env("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Ok(base.join("eitri"));
    }
    env("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|home| home.join(".local/share/eitri"))
        .ok_or_else(|| "HOME must be set to an absolute path for the LuaLS data directory".to_string())
}

/// Refreshes definitions on window start without delaying the GTK thread or failing the window.
pub fn refresh_in_background() {
    if let Err(error) = std::thread::Builder::new().name("luals-meta".into()).spawn(|| {
        let result = data_dir(&|key| std::env::var_os(key))
            .map_err(io::Error::other)
            .and_then(|dir| install_meta(&dir));
        if let Err(error) = result {
            eprintln!("[lua-ls] could not refresh the API definition: {error}");
        }
    }) {
        eprintln!("[lua-ls] could not start the API definition worker: {error}");
    }
}

/// The setup subcommand, with environment and output passed in so it needs neither GTK nor global mutations.
pub fn setup(
    args: &[OsString],
    env: &dyn Fn(&str) -> Option<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    if !matches!(args, [target] if matches!(target.to_str(), Some("lua-ls" | "lua_ls" | "luals"))) {
        let _ = writeln!(err, "{USAGE}");
        return 64;
    }
    let result = (|| -> Result<(), String> {
        let config = crate::config_dir::resolve(env)?;
        let data = data_dir(env)?;
        let outcome = write_luarc(&config, &data).map_err(|e| e.to_string())?;
        match outcome {
            LuarcOutcome::Created(path) => writeln!(out, "{}", path.display()),
            LuarcOutcome::Existing { path, line } => writeln!(
                out,
                "{} already exists; left unchanged. Add this line to it:\n{line}",
                path.display()
            ),
        }
        .map_err(|e| e.to_string())
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "eitri setup lua-ls: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
