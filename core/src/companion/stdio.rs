//! A panel started from nvim (`:EitriPanel`, a `jobstart` with `detach`) gets pipes from that nvim
//! as its stdin and stderr (its stdout is `/dev/null`). The panel outlives the nvim that started it,
//! and once that nvim has quit every write to the stderr pipe fails with `EPIPE`. Rust ignores
//! `SIGPIPE`, so `eprintln!` then panics, and a panic inside a GLib callback aborts the panel and every
//! session in it. While that nvim lives it would also keep everything the panel writes in memory.
//!
//! So once the panel has passed the start-up checks whose message the plugin shows, it moves its
//! standard streams off such pipes: stdin to `/dev/null`, stdout and stderr to a log file under the
//! state directory (or `/dev/null` when the file cannot be made). A terminal or a file is left alone.

use std::ffi::OsStr;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

/// `<state home>/eitri/companion`, by the same rule as the other state directories.
pub fn log_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    crate::layout::persist::state_subdir(xdg_state_home, home, "companion")
}

/// The log's name for this canonical project root: 16 hex digits of its SHA-256, `.log`. One panel
/// runs per project, so the file is the running panel's own.
pub fn log_file_name(project_root: &Path) -> String {
    format!("{}.log", &agent::conversation_id_for_cwd(project_root)[..16])
}

/// Whether `fd` is a pipe or a socket, which is what a job of nvim's gets; never a terminal or a file.
pub fn is_pipe(fd: RawFd) -> bool {
    // SAFETY: `stat` is a plain buffer `fstat` fills in; an fd that is not open makes it fail.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return false;
    }
    matches!(stat.st_mode & libc::S_IFMT, libc::S_IFIFO | libc::S_IFSOCK)
}

/// Points `fd` at what `target` is open on when `fd` is a pipe. `Ok(true)` when it moved.
pub fn redirect_if_pipe(fd: RawFd, target: RawFd) -> std::io::Result<bool> {
    if !is_pipe(fd) {
        return Ok(false);
    }
    // SAFETY: `dup2` takes two descriptors and replaces `fd` atomically, closing the pipe end it held;
    // the new `fd` does not inherit `target`'s close-on-exec flag, so children get the log too.
    if unsafe { libc::dup2(target, fd) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(true)
}

/// What [`leave_pipes`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Moved {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    /// Where stdout and stderr now go, when one of them moved to a log file rather than `/dev/null`.
    pub log: Option<PathBuf>,
}

/// Where the output would go: the log file, created 0600 (truncated) in a 0700 directory. `None`
/// when no directory is known or it cannot be made.
pub fn open_log(dir: &Path, project_root: &Path) -> Option<(std::fs::File, PathBuf)> {
    crate::layout::persist::create_state_dir(dir).ok()?;
    let path = dir.join(log_file_name(project_root));
    let file = agent::private_fs::open_private(&path).ok()?;
    Some((file, path))
}

/// Moves this process's standard streams off pipes, as the module says. `before` is called once,
/// with the log's path, just before stdout or stderr moves, while stderr still reaches the starter.
/// Nothing is created when no stream is a pipe.
pub fn leave_pipes(log_dir: Option<&Path>, project_root: &Path, before: &dyn Fn(Option<&Path>)) -> Moved {
    let mut moved = Moved::default();
    if is_pipe(0) {
        if let Ok(null) = std::fs::File::open("/dev/null") {
            moved.stdin = redirect_if_pipe(0, null.as_raw_fd()).unwrap_or(false);
        }
    }
    if !is_pipe(1) && !is_pipe(2) {
        return moved;
    }
    let log = log_dir.and_then(|dir| open_log(dir, project_root));
    let (file, path) = match log {
        Some((file, path)) => (file, Some(path)),
        None => match std::fs::OpenOptions::new().write(true).open("/dev/null") {
            Ok(null) => (null, None),
            Err(_) => return moved,
        },
    };
    before(path.as_deref());
    moved.stdout = redirect_if_pipe(1, file.as_raw_fd()).unwrap_or(false);
    moved.stderr = redirect_if_pipe(2, file.as_raw_fd()).unwrap_or(false);
    moved.log = path;
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;

    /// Close-on-exec, so a child another test spawns meanwhile cannot hold the write end open.
    fn pipe() -> (OwnedFd, OwnedFd) {
        let (read, write) = std::io::pipe().unwrap();
        (OwnedFd::from(read), OwnedFd::from(write))
    }

    fn scratch(label: &str) -> crate::test_scratch_dir::ScratchDir {
        crate::test_scratch_dir::ScratchDir::new("companion-stdio", label)
    }

    #[test]
    fn a_pipe_is_a_pipe_and_a_file_is_not() {
        let (read, write) = pipe();
        assert!(is_pipe(read.as_raw_fd()));
        assert!(is_pipe(write.as_raw_fd()));
        let dir = scratch("kinds");
        let file = std::fs::File::create(dir.join("f")).unwrap();
        assert!(!is_pipe(file.as_raw_fd()));
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!is_pipe(null.as_raw_fd()));
        assert!(!is_pipe(-1));
    }

    /// The case that crashed: the starter holds the read end, the panel writes. After the move the
    /// writes land in the log, and the starter's end sees the stream end at once, so a starter that
    /// buffers the stream until it ends (nvim's `stderr_buffered`) is not kept waiting or growing.
    #[test]
    fn a_redirected_pipe_writes_to_the_log_and_its_reader_sees_the_end() {
        let (mut read, write) = {
            let (r, w) = pipe();
            (std::fs::File::from(r), w)
        };
        let dir = scratch("redirect");
        let (log, path) = open_log(&dir.join("eitri/companion"), Path::new("/some/project")).unwrap();
        assert!(redirect_if_pipe(write.as_raw_fd(), log.as_raw_fd()).unwrap());
        drop(log);
        let mut seen = Vec::new();
        read.read_to_end(&mut seen).unwrap();
        assert!(seen.is_empty(), "the starter's end saw {seen:?}");
        // The starter quits: its end closes. A write that would have failed with EPIPE now succeeds.
        drop(read);
        let mut panel_side = std::fs::File::from(write);
        panel_side.write_all(b"[permission] allowed\n").unwrap();
        drop(panel_side);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[permission] allowed\n");
        // A file is not a pipe, so nothing moves it.
        let file = std::fs::File::open(&path).unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!redirect_if_pipe(file.as_raw_fd(), null.as_raw_fd()).unwrap());
    }

    #[test]
    fn the_log_is_private_and_named_for_the_project() {
        use std::os::unix::fs::PermissionsExt;
        let base = scratch("modes");
        let dir = base.join("eitri/companion");
        let (_file, path) = open_log(&dir, Path::new("/some/project")).unwrap();
        assert_eq!(
            path.file_name().unwrap(),
            OsStr::new(&log_file_name(Path::new("/some/project")))
        );
        assert!(log_file_name(Path::new("/some/project")).ends_with(".log"));
        assert_eq!(log_file_name(Path::new("/some/project")).len(), 20);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            log_dir(Some(OsStr::new("/state")), None),
            Some(PathBuf::from("/state/eitri/companion"))
        );
        assert_eq!(log_dir(Some(OsStr::new("rel")), None), None);
    }

    #[test]
    fn a_symlink_in_the_logs_place_is_not_followed() {
        let base = scratch("symlink");
        let dir = base.join("eitri/companion");
        std::fs::create_dir_all(&dir).unwrap();
        let victim = base.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join(log_file_name(Path::new("/p")))).unwrap();
        assert!(open_log(&dir, Path::new("/p")).is_none());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    }

    /// A FIFO planted where the log goes costs the panel its log, not its window: `open_log` returns
    /// instead of waiting for a reader that will never come.
    #[test]
    fn a_fifo_in_the_logs_place_does_not_hang_the_start() {
        use std::os::unix::ffi::OsStrExt;
        let base = scratch("fifo");
        let dir = base.join("eitri/companion");
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join(log_file_name(Path::new("/p")));
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_log(&dir, Path::new("/p")).is_none());
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("open_log blocked on a FIFO"));
    }
}
