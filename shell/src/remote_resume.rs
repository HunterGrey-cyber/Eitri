//! A remote window's ssh connection after a suspend or a network change. A master on a path that died with the
//! network takes a minute to notice, and every channel hangs on it meanwhile, so logind's `PrepareForSleep(false)` on
//! the system bus and gio's network monitor each run `remote_host::on_resume` for the window's masters: one round
//! trip through each, and a fresh master for one that fails. That blocks on ssh, so it runs on a worker, never on the
//! GTK thread; a signal that comes while one runs is folded into one more run after it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use agent::ssh::ControlMaster;
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;

/// What woke the check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
    Resumed,
    NetworkChanged,
}

#[derive(Default)]
struct Runs {
    running: bool,
    /// The latest signal that came while a run was going: one more run, for it, follows.
    again: Option<Signal>,
}

/// Runs a blocking check on a worker per signal, at most one at a time.
pub(crate) struct Resume {
    check: Arc<dyn Fn(Signal) + Send + Sync>,
    runs: Arc<Mutex<Runs>>,
}

impl Resume {
    pub(crate) fn new(check: impl Fn(Signal) + Send + Sync + 'static) -> Resume {
        Resume {
            check: Arc::new(check),
            runs: Arc::default(),
        }
    }

    /// The check of `masters`: `on_resume` for each, its outcome logged.
    pub(crate) fn for_masters(masters: Vec<ControlMaster>) -> Resume {
        Resume::new(move |signal| {
            for action in eitri_core::remote_host::on_resume(&masters) {
                eprintln!("[remote] after {signal:?}: {action:?}");
            }
        })
    }

    #[cfg(test)]
    fn running(&self) -> bool {
        self.runs.lock().unwrap().running
    }

    /// Starts the check on a worker and returns at once: `true` when a worker started, `false` when one was running
    /// and runs once more after it instead.
    pub(crate) fn signal(&self, signal: Signal) -> bool {
        {
            let mut runs = self.runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if runs.running {
                runs.again = Some(signal);
                return false;
            }
            runs.running = true;
        }
        let check = Arc::clone(&self.check);
        let runs = Arc::clone(&self.runs);
        let started = std::thread::Builder::new()
            .name("eitri-remote-resume".into())
            .spawn(move || {
                let mut signal = signal;
                loop {
                    check(signal);
                    let mut runs = runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(next) = runs.again.take() {
                        signal = next;
                        continue;
                    }
                    runs.running = false;
                    break;
                }
            });
        if let Err(e) = started {
            eprintln!("[remote] the check after {signal:?} could not start: {e}");
            self.runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .running = false;
            return false;
        }
        true
    }
}

/// Whether logind's `PrepareForSleep` parameters say the machine has just woken (`false`; `true` is going to sleep).
pub(crate) fn woke(parameters: &glib::Variant) -> bool {
    parameters.get::<(bool,)>().is_some_and(|(going,)| !going)
}

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";
const LOGIND_PATH: &str = "/org/freedesktop/login1";

/// The window's subscriptions: the network monitor's handler and, once the system bus answers, logind's signal.
/// Dropping it ends both.
pub(crate) struct Listener {
    _resume: Rc<Resume>,
    monitor: gio::NetworkMonitor,
    network: Option<glib::SignalHandlerId>,
    sleep: Rc<RefCell<Option<gio::SignalSubscription>>>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(id) = self.network.take() {
            self.monitor.disconnect(id);
        }
        self.sleep.borrow_mut().take();
    }
}

/// Makes gio's network monitor before the window's main loop runs: the first one sets up its implementation with a
/// synchronous call on the system bus, which must not hold up the GTK thread. [`listen`] then gets the same monitor at
/// once. For a remote project's window only, at its launch.
pub(crate) fn prepare_network_monitor() {
    let _ = gio::NetworkMonitor::default();
}

/// Subscribes `resume` to resume and network changes. The system bus is reached asynchronously; when it cannot be,
/// only network changes wake the check, and the log says why.
pub(crate) fn listen(resume: Resume) -> Listener {
    let resume = Rc::new(resume);
    let monitor = gio::NetworkMonitor::default();
    let on_network = Rc::downgrade(&resume);
    let network = monitor.connect_network_changed(move |_, available| {
        if let Some(resume) = on_network.upgrade().filter(|_| available) {
            resume.signal(Signal::NetworkChanged);
        }
    });
    let sleep = Rc::new(RefCell::new(None));
    let slot = Rc::downgrade(&sleep);
    let on_sleep = Rc::downgrade(&resume);
    glib::spawn_future_local(async move {
        let connection = match gio::bus_get_future(gio::BusType::System).await {
            Ok(connection) => connection,
            Err(e) => {
                eprintln!("[remote] no system bus ({e}); a resume from suspend is noticed only by the network");
                return;
            }
        };
        let subscription = connection.subscribe_to_signal(
            Some(LOGIND),
            Some(LOGIND_MANAGER),
            Some("PrepareForSleep"),
            Some(LOGIND_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                if woke(signal.parameters) {
                    if let Some(resume) = on_sleep.upgrade() {
                        resume.signal(Signal::Resumed);
                    }
                }
            },
        );
        if let Some(slot) = slot.upgrade() {
            *slot.borrow_mut() = Some(subscription);
        }
    });
    Listener {
        _resume: resume,
        monitor,
        network: Some(network),
        sleep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A signal returns at once and the check runs on a worker of its own; a signal while it runs is folded into one
    /// more run after it, not a second worker. logind's parameters are read as a wake only for `false`.
    #[test]
    fn resume_signal_runs_on_resume_off_the_gtk_thread() {
        let (ran, runs) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let resume = Resume::new(move |signal| {
            ran.send((std::thread::current().id(), signal)).unwrap();
            released.lock().unwrap().recv_timeout(Duration::from_secs(60)).unwrap();
        });
        let here = std::thread::current().id();
        assert!(resume.signal(Signal::Resumed), "a worker started");
        let (worker, signal) = runs.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_ne!(worker, here, "the check never runs on the caller's thread");
        assert_eq!(signal, Signal::Resumed);
        assert!(!resume.signal(Signal::NetworkChanged), "folded into the running one");
        assert!(!resume.signal(Signal::NetworkChanged));
        release.send(()).unwrap();
        let (again, signal) = runs.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(again, worker, "the same worker runs once more");
        assert_eq!(signal, Signal::NetworkChanged, "for the signal that asked for it");
        release.send(()).unwrap();
        assert!(
            runs.recv_timeout(Duration::from_millis(300)).is_err(),
            "two signals made one more run"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while resume.running() {
            assert!(std::time::Instant::now() < deadline, "the worker ended");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            resume.signal(Signal::NetworkChanged),
            "a later signal starts a new worker"
        );
        runs.recv_timeout(Duration::from_secs(60)).unwrap();
        release.send(()).unwrap();

        assert!(woke(&(false,).to_variant()));
        assert!(!woke(&(true,).to_variant()));
        assert!(!woke(&"false".to_variant()));
    }
}
