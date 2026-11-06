//! The device store as `serve` sees it, and the watch that takes a revoked phone out.
//!
//! [`StoreView`] reads the store again whenever the file it names is another file or was changed (its device, inode,
//! size, modification and change times), so a check costs one `stat` while nothing changed. Every write of the store
//! renames a new file into place (`remote_devices::update`), so a write is always seen. A store that cannot be read
//! (missing, refused) holds no device.
//!
//! [`watch_store`] watches the store's directory with inotify, and looks again every [`STORE_RECHECK`] in any case
//! (a watch that could not be set, a directory made again): on any change it calls back, and the server closes every
//! connection whose device is gone or whose grant fell, before it reads anything else from them. The writer of each
//! connection asks the view again before every frame, so a frame is not written to a phone the store no longer holds
//! even in the moment before the watch's callback runs.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eitri_core::remote_devices::{DeviceStore, Grant};

/// How often the store is looked at even with no inotify event.
pub const STORE_RECHECK: Duration = Duration::from_secs(2);

/// What identifies one version of the store file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn identity(path: &Path) -> Option<Identity> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(Identity {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
    })
}

struct Cached {
    identity: Option<Identity>,
    store: Arc<DeviceStore>,
}

/// The store at one path, read again when it changes.
pub struct StoreView {
    path: PathBuf,
    cached: Mutex<Option<Cached>>,
}

impl StoreView {
    pub fn new(path: &Path) -> StoreView {
        StoreView {
            path: path.to_owned(),
            cached: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn current(&self) -> Arc<DeviceStore> {
        let now = identity(&self.path);
        let mut cached = self.cached.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(held) = cached.as_ref() {
            if held.identity == now && now.is_some() {
                return held.store.clone();
            }
        }
        let store = match now {
            None => DeviceStore::default(),
            Some(_) => DeviceStore::load(&self.path).unwrap_or_else(|e| {
                eprintln!("[remote] the device store holds no device: {e}");
                DeviceStore::default()
            }),
        };
        let store = Arc::new(store);
        *cached = Some(Cached {
            identity: now,
            store: store.clone(),
        });
        store
    }

    /// The grant of `device` when `token` is its token in the store now.
    pub fn verify(&self, device: &str, token: &str) -> Option<Grant> {
        self.current().verify(device, token).map(|(_, grant)| grant)
    }

    /// Whether `device`, holding `token`, may still do what `grant` lets it: it is in the store with that token and a
    /// grant at least as wide.
    pub fn still(&self, device: &str, token: &str, grant: Grant) -> bool {
        match (self.verify(device, token), grant) {
            (Some(Grant::WatchSteer), _) => true,
            (Some(Grant::Watch), Grant::Watch) => true,
            (Some(Grant::Watch), Grant::WatchSteer) | (None, _) => false,
        }
    }
}

/// Watches `dir`, the store's directory, on a thread of its own, and calls `changed` after every change in it and every
/// [`STORE_RECHECK`]. The thread ends when `changed` returns `false`.
pub fn watch_store(dir: &Path, changed: impl FnMut() -> bool + Send + 'static) -> io::Result<()> {
    let watch = Inotify::on(dir).map_err(|e| eprintln!("[remote] the device store is looked at every 2 s only: {e}"));
    std::thread::Builder::new()
        .name("eitri-remote-store".into())
        .spawn(move || run_watch(watch.ok(), changed))?;
    Ok(())
}

fn run_watch(watch: Option<Inotify>, mut changed: impl FnMut() -> bool) {
    loop {
        if let Some(watch) = &watch {
            watch.wait(STORE_RECHECK);
        } else {
            std::thread::sleep(STORE_RECHECK);
        }
        if !changed() {
            return;
        }
    }
}

struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    fn on(dir: &Path) -> io::Result<Inotify> {
        // SAFETY: no pointers; the flags are valid.
        let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` was just opened by this call and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the store's path"))?;
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_TO
            | libc::IN_MOVED_FROM
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        // SAFETY: `fd` is open, `path` is a NUL-terminated string that lives for the call.
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Inotify { fd })
    }

    /// Waits until an event comes or `within` passes, then drains what came.
    fn wait(&self, within: Duration) {
        let mut fds = [libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: `fds` is one initialized `pollfd` whose descriptor stays open for the call.
        unsafe { libc::poll(fds.as_mut_ptr(), 1, within.as_millis().min(i32::MAX as u128) as i32) };
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: reads at most `buf.len()` bytes into `buf`, which lives for the call.
            let got = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if got <= 0 {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::remote_devices::{token_hash_hex, update};
    use std::sync::mpsc;

    fn scratch(name: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let base =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-tests")
                .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();
        base
    }

    fn write_store(path: &Path, grant: &str) {
        let body = serde_json::json!({
            "v": 1,
            "devices": [ { "id": "d1", "name": "phone", "token_sha256": token_hash_hex("tok"), "grant": grant } ],
        });
        agent::private_fs::write_private(path, body.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn the_view_reads_the_store_again_after_every_write() {
        let dir = scratch("store-view");
        let path = dir.join("devices.json");
        let view = StoreView::new(&path);
        assert_eq!(view.verify("d1", "tok"), None, "no store holds no device");
        write_store(&path, "watch+steer");
        assert_eq!(view.verify("d1", "tok"), Some(Grant::WatchSteer));
        assert!(view.still("d1", "tok", Grant::WatchSteer));
        assert_eq!(view.verify("d1", "wrong"), None);
        update(&path, |store| store.get_mut("d1").unwrap().grant = Grant::Watch).unwrap();
        assert!(!view.still("d1", "tok", Grant::WatchSteer), "the grant fell");
        assert!(view.still("d1", "tok", Grant::Watch));
        update(&path, |store| store.remove("d1")).unwrap();
        assert!(!view.still("d1", "tok", Grant::Watch), "the device is gone");
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(
            view.verify("d1", "tok"),
            None,
            "a store that is refused holds no device"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_watch_hears_a_rename_into_the_directory() {
        let dir = scratch("store-watch");
        let path = dir.join("devices.json");
        write_store(&path, "watch+steer");
        let (tx, rx) = mpsc::channel();
        let view = Arc::new(StoreView::new(&path));
        let seen = view.clone();
        assert!(seen.verify("d1", "tok").is_some());
        watch_store(&dir, move || tx.send(seen.verify("d1", "tok")).is_ok()).unwrap();
        update(&path, |store| store.remove("d1")).unwrap();
        // The periodic look would also find it; within a recheck an event must have come.
        let deadline = std::time::Instant::now() + STORE_RECHECK * 10;
        loop {
            let got = rx.recv_timeout(STORE_RECHECK * 2).expect("the watch calls back");
            if got.is_none() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the removal was never seen");
        }
        drop(rx);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
