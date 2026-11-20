//! A socket of one process in the control directory, `<prefix>-<instance>.sock`, and the thread serving it: the
//! agents control socket (`agents::control`) and the remote observation socket (`remote_rv`) are bound, made
//! private and removed here, so the two cannot come to refuse different things.
//!
//! The posture is the panel control socket's: the control directory created 0700 and refused when its parent is unsafe
//! or group or others can write it (`panel_control::ensure_dir`),
//! a link already at the name refused (`panel_control::bind_unfollowed`), the socket 0600 through the directory's
//! descriptor, and the name removed on the way out only while it still holds the socket this bound.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent::private_fs::{remove_if_same, FileIdentity};

use crate::panel_control::{bind_unfollowed, ensure_dir, make_control_socket_private};

/// How many lowercase hex digits an instance name has.
pub const INSTANCE_HEX: usize = 16;

/// Whether `s` can be an instance name: exactly [`INSTANCE_HEX`] lowercase hex digits, so it can never name another
/// path.
pub fn is_instance(s: &str) -> bool {
    s.len() == INSTANCE_HEX && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `control_dir/<prefix>-<instance>.sock`, through `agent::socket_path::in_dir`, which refuses a path over the socket
/// path limit. `instance` must be [`INSTANCE_HEX`] lowercase hex digits, so no id read from elsewhere can name a path
/// outside the directory.
pub(crate) fn socket_path(control_dir: &Path, prefix: &str, instance: &str) -> io::Result<PathBuf> {
    if !is_instance(instance) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{instance:?} is not an instance id ({INSTANCE_HEX} lowercase hex digits)"),
        ));
    }
    agent::socket_path::in_dir(control_dir, &format!("{prefix}-{instance}.sock"))
}

/// A bound socket and the stop of the thread serving it.
///
/// Dropping it removes the socket only while the name still holds the one bound, and never waits for the thread: a
/// handler may be waiting for the very thread that stops it.
pub(crate) struct ControlListener {
    dir: std::fs::File,
    name: OsString,
    bound: FileIdentity,
    stop: Arc<AtomicBool>,
    /// The other end of the thread's wake pair: closing it wakes the thread's `poll`.
    wake: Option<UnixStream>,
}

impl ControlListener {
    /// Binds `path`, a name in `control_dir`, and starts the thread `thread` with `serve`, which gets the listener
    /// (nonblocking), its end of the wake pair and the stop flag.
    ///
    /// Fails when the directory is not private or the name is taken; a failure once the socket's identity is read
    /// removes that socket again, and only it.
    pub(crate) fn bind(
        control_dir: &Path,
        path: &Path,
        thread: &str,
        serve: impl FnOnce(UnixListener, UnixStream, Arc<AtomicBool>) + Send + 'static,
    ) -> io::Result<ControlListener> {
        let name = path
            .file_name()
            .map(OsString::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a socket path with no file name"))?;
        ensure_dir(control_dir).map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
        let dir = agent::private_fs::open_dir(control_dir)?;
        let listener = bind_unfollowed(path)?;
        let meta = path.symlink_metadata()?;
        let bound = FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        };
        #[cfg(test)]
        crate::panel_control::before_socket_chmod::run();
        // `bind` leaves the umask's mode on the file.
        let permissions = make_control_socket_private(&dir, path, bound);
        let started = (move || -> io::Result<(UnixStream, Arc<AtomicBool>)> {
            permissions?;
            listener.set_nonblocking(true)?;
            let (wake, wake_rx) = UnixStream::pair()?;
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(thread.into())
                .spawn(move || serve(listener, wake_rx, thread_stop))?;
            Ok((wake, stop))
        })();
        match started {
            Ok((wake, stop)) => Ok(ControlListener {
                dir,
                name,
                bound,
                stop,
                wake: Some(wake),
            }),
            Err(e) => {
                let _ = remove_if_same(&dir, &name, bound);
                Err(e)
            }
        }
    }
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Unlinked first, so no new peer finds it while the thread winds down.
        let _ = remove_if_same(&self.dir, &self.name, self.bound);
        self.wake.take();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    const INSTANCE: &str = "a3f09c1d2e4b5f60";

    /// A link put at the name between the bind and the chmod is refused for both sockets: the bind fails, the link
    /// stays, and what it points at keeps its mode.
    #[test]
    fn a_link_put_at_either_socket_between_bind_and_chmod_is_refused_and_its_target_keeps_its_mode() {
        type Bind = fn(&std::path::Path) -> std::io::Result<()>;
        let sockets: [(&str, Bind); 2] = [
            ("ag", |dir| {
                crate::agents::control::Server::bind(dir, INSTANCE, |_| crate::agents::control::Reply::Ok).map(drop)
            }),
            ("rv", |dir| {
                crate::remote_rv::RvListener::bind(dir, INSTANCE, |_| true).map(drop)
            }),
        ];
        for (prefix, bind) in sockets {
            let scratch = crate::test_scratch_dir::ScratchDir::new("control-listener", prefix);
            let base = std::fs::canonicalize(&*scratch).unwrap();
            let dir: PathBuf = base.join("control");
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            let target = base.join("target");
            std::fs::write(&target, "kept").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
            let path = super::socket_path(&dir, prefix, INSTANCE).unwrap();
            let (planted, planted_target) = (path.clone(), target.clone());
            crate::panel_control::before_socket_chmod::set(Box::new(move || {
                std::fs::remove_file(&planted).unwrap();
                symlink(&planted_target, &planted).unwrap();
            }));
            assert!(bind(&dir).is_err(), "{prefix}");
            assert_eq!(target.metadata().unwrap().mode() & 0o777, 0o640, "{prefix}");
            assert!(path.symlink_metadata().unwrap().file_type().is_symlink(), "{prefix}");
        }
    }
}
