//! Whether the person is at the desk, for `serve`'s notifier: while they are, a phone is not woken for an agent that
//! needs them, since the window already shows it.
//!
//! [`attach`] adds one capture-phase controller to the window that sees every key, button, scroll, touch and pointer
//! event before any widget does, stores when it came in an atomic ([`stamp`]) and lets it go on: it never claims or
//! stops an event. Lines from the editor's context feed about a file or one of Eitri's drafts or row views call
//! [`stamp`] too (`editor_feeds::FeedPump`; not a view an agent's proposal opened), so moving about a companion panel's
//! own nvim, in a terminal beside the window, counts as being at the desk.
//!
//! A thread [`attach`] starts looks at the atomic every [`PRESENCE_TOUCH_EVERY`] and, when an event came since it
//! last looked, touches `<control>/presence/<instance>` (0600 in 0700, through `private_fs`); `serve` reads the
//! directory's times. Nothing here touches the disk on the GTK thread. The file goes when the guard is dropped (by the
//! thread, which is told to); one left by a window that crashed only ages out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// How often the presence file is touched while the person is at the desk.
pub(crate) const PRESENCE_TOUCH_EVERY: Duration = Duration::from_secs(30);

/// The directory beneath the control directory, as `serve` reads it.
pub(crate) const PRESENCE_DIR: &str = eitri_remote::serve::notifier::PRESENCE_DIR;

static EPOCH: OnceLock<Instant> = OnceLock::new();
/// When the last event came: milliseconds after [`EPOCH`], plus one, so 0 is "never".
static LAST_EVENT: AtomicU64 = AtomicU64::new(0);

/// Notes that the person did something at the desk now.
pub(crate) fn stamp() {
    LAST_EVENT.store(mark_now(), Ordering::Relaxed);
}

/// The mark of the last [`stamp`]; 0 before the first.
pub(crate) fn last_stamp() -> u64 {
    LAST_EVENT.load(Ordering::Relaxed)
}

fn mark_now() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// What the controller does with an event of `kind`: stamps the ones a person makes and lets every one go on.
fn seen(kind: gdk::EventType) -> glib::Propagation {
    if matches!(
        kind,
        gdk::EventType::KeyPress
            | gdk::EventType::ButtonPress
            | gdk::EventType::MotionNotify
            | gdk::EventType::Scroll
            | gdk::EventType::TouchBegin
            | gdk::EventType::TouchUpdate
            | gdk::EventType::TouchpadSwipe
            | gdk::EventType::TouchpadPinch
    ) {
        stamp();
    }
    glib::Propagation::Proceed
}

/// Keeps the presence file while it lives; dropping it has the file removed.
pub(crate) struct PresenceGuard {
    stop: mpsc::Sender<()>,
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

/// Watches `window` for the person and keeps `<control_dir>/presence/<instance>` touched while they are there.
pub(crate) fn attach(window: &gtk4::ApplicationWindow, control_dir: &Path, instance: &str) -> PresenceGuard {
    let controller = gtk4::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    controller.connect_event(|_, event| seen(event.event_type()));
    window.add_controller(controller);
    let (stop, stopped) = mpsc::channel();
    let path = control_dir.join(PRESENCE_DIR).join(instance);
    let control = control_dir.to_owned();
    let started = std::thread::Builder::new()
        .name("eitri-presence".into())
        .spawn(move || keep_touched(&control, &path, &stopped, PRESENCE_TOUCH_EVERY));
    if let Err(e) = started {
        eprintln!("[remote] the desk's presence is not told to serve: {e}");
    }
    PresenceGuard { stop }
}

/// The thread: every `every`, touches `path` when a stamp came since the last look; removes it when told to stop.
fn keep_touched(control: &Path, path: &Path, stopped: &mpsc::Receiver<()>, every: Duration) {
    let mut looked = 0;
    let mut said = false;
    loop {
        match stopped.recv_timeout(every) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = std::fs::remove_file(path);
                return;
            }
        }
        if let Err(e) = step(control, path, &mut looked) {
            if !std::mem::replace(&mut said, true) {
                eprintln!("[remote] could not touch {}: {e}", path.display());
            }
        }
    }
}

/// One look: touches `path` when the last stamp is newer than `looked`, which it then becomes; whether it touched.
fn step(control: &Path, path: &Path, looked: &mut u64) -> std::io::Result<bool> {
    let last = last_stamp();
    if last <= *looked {
        return Ok(false);
    }
    *looked = last;
    touch(control, path)?;
    Ok(true)
}

fn touch(control: &Path, path: &Path) -> std::io::Result<()> {
    let dir: PathBuf = path.parent().map_or_else(|| control.to_owned(), Path::to_path_buf);
    agent::private_fs::create_private_dir_all(&dir, control)?;
    agent::private_fs::open_private(path)?.set_modified(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-presence")
                .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn presence_controllers_never_claim_an_event() {
        for kind in [
            gdk::EventType::KeyPress,
            gdk::EventType::KeyRelease,
            gdk::EventType::ButtonPress,
            gdk::EventType::ButtonRelease,
            gdk::EventType::MotionNotify,
            gdk::EventType::Scroll,
            gdk::EventType::TouchBegin,
            gdk::EventType::FocusChange,
        ] {
            assert_eq!(seen(kind), glib::Propagation::Proceed, "{kind:?} was stopped");
        }
        let before = last_stamp();
        std::thread::sleep(Duration::from_millis(2));
        let _ = seen(gdk::EventType::KeyPress);
        assert!(last_stamp() > before, "a key press was not noted");
    }

    #[test]
    fn the_file_is_touched_only_after_an_event_and_removed_at_the_end() {
        let control = scratch("touch");
        let path = control.join(PRESENCE_DIR).join("a3f09c1d2e4b5f60");
        let mut looked = last_stamp();
        assert!(
            !step(&control, &path, &mut looked).unwrap(),
            "touched with no event since"
        );
        assert!(!path.exists());
        std::thread::sleep(Duration::from_millis(2));
        stamp();
        assert!(step(&control, &path, &mut looked).unwrap());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            !step(&control, &path, &mut looked).unwrap(),
            "touched twice for one event"
        );
        let (stop, stopped) = mpsc::channel();
        let thread = {
            let (control, path) = (control.clone(), path.clone());
            std::thread::spawn(move || keep_touched(&control, &path, &stopped, Duration::from_secs(3600)))
        };
        drop(PresenceGuard { stop });
        thread.join().unwrap();
        assert!(!path.exists(), "the presence file outlived its guard");
        let _ = std::fs::remove_dir_all(&control);
    }
}
