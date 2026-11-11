//! `serve`'s notifier: a thread that reads the agents layer's changes and sends each wake the rules say is due to the
//! helper, which posts it ([`crate::notify::helper`]).
//!
//! It owns an agents [`Reader`] of its own (the changes as values, its first `Reset` the silent baseline the rules
//! need), and once a second it ticks the rules so held and coalesced wakes go when they are due. The context the rules
//! decide by is read at each step: the store's devices with a push endpoint and the finished keys each set
//! ([`Hub::push_targets`]), the devices connected now ([`Hub::connected_devices`], which are not woken), and whether
//! the desk is in use: a window touches `<control>/presence/<instance>` while its person types or moves the pointer,
//! and while any file there was touched within [`DESK_ACTIVE_HOLD`] a needs-you wake is held. Each wake is recorded as
//! `push` with the endpoint's host alone, the wake's kind and the status the helper answered (or `dropped`).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

use eitri_core::agents::{Change, Reader, ReaderEvent, SCHEMA_V};
use eitri_core::remote_audit::{DeviceRef, Event as AuditEvent};

use super::session::{endpoint_host, Audit, Hub};
use crate::notify::helper::Helper;
use crate::notify::{Ctx, Target, Wake, WakeRules, DESK_ACTIVE_HOLD};

/// The directory beneath the control directory the windows' presence files are in.
pub const PRESENCE_DIR: &str = "presence";
/// How often the rules are ticked while no change arrives.
pub const NOTIFY_TICK: Duration = Duration::from_secs(1);

/// Whether a queued wake is still due when its turn comes: its device still has that endpoint and is not connected.
pub type StillDue = Box<dyn Fn() -> bool + Send>;

/// Where a wake goes.
pub trait Poster: Send + 'static {
    /// Queues one wake to `endpoint`, to go only if `still_due` says so when its turn comes; `done` gets the status
    /// word. `Err` when it was dropped.
    fn post(&self, endpoint: &str, still_due: StillDue, done: Box<dyn FnOnce(String) + Send>) -> io::Result<()>;
}

impl Poster for Helper {
    fn post(&self, endpoint: &str, still_due: StillDue, done: Box<dyn FnOnce(String) + Send>) -> io::Result<()> {
        self.wake(endpoint, still_due, done)
    }
}

/// The store's name for a device id now.
pub type NameOf = Box<dyn Fn(&str) -> Option<String> + Send>;

/// What the notifier reads its context from at each step, and a queued wake again before it goes.
pub struct Sources {
    pub targets: Arc<dyn Fn() -> Vec<Target> + Send + Sync>,
    pub connected: Arc<dyn Fn() -> BTreeSet<String> + Send + Sync>,
    pub name_of: NameOf,
    /// `<control>/presence`.
    pub presence: PathBuf,
}

impl Sources {
    /// The hub's targets and connections, and the presence directory beneath `control`.
    pub fn of_hub(hub: &Arc<Hub>, control: &Path) -> Sources {
        let (targets, connected, names) = (hub.clone(), hub.clone(), hub.clone());
        Sources {
            targets: Arc::new(move || targets.push_targets()),
            connected: Arc::new(move || connected.connected_devices()),
            name_of: Box::new(move |device| names.device_name(device)),
            presence: control.join(PRESENCE_DIR),
        }
    }
}

/// The rules and where their wakes go.
pub struct Notifier<P: Poster> {
    rules: WakeRules,
    sources: Sources,
    post: P,
    audit: Arc<Audit>,
    #[cfg(feature = "remote-test-hooks")]
    pending: Arc<std::sync::atomic::AtomicUsize>,
}

impl<P: Poster> Notifier<P> {
    pub fn new(sources: Sources, post: P, audit: Arc<Audit>) -> Notifier<P> {
        Notifier {
            rules: WakeRules::new(),
            sources,
            post,
            audit,
            #[cfg(feature = "remote-test-hooks")]
            pending: Arc::default(),
        }
    }

    /// One change of the agents layer, at `now`.
    pub fn on_change(&mut self, change: &Change, now: Instant) {
        self.on_change_at(change, now, SystemTime::now);
    }

    fn on_change_at(&mut self, change: &Change, now: Instant, wall: impl FnOnce() -> SystemTime) {
        let (targets, connected) = ((self.sources.targets)(), (self.sources.connected)());
        let ctx = Ctx {
            targets: &targets,
            connected: &connected,
            desk_active_until: desk_active_until(&self.sources.presence, wall(), now),
        };
        let due = self.rules.on_change(change, now, &ctx);
        self.send(due);
    }

    /// The once-a-second step: wakes held or coalesced until now.
    pub fn tick(&mut self, now: Instant) {
        self.tick_at(now, SystemTime::now);
    }

    fn tick_at(&mut self, now: Instant, wall: impl FnOnce() -> SystemTime) {
        let (targets, connected) = ((self.sources.targets)(), (self.sources.connected)());
        let ctx = Ctx {
            targets: &targets,
            connected: &connected,
            desk_active_until: desk_active_until(&self.sources.presence, wall(), now),
        };
        let due = self.rules.tick(now, &ctx);
        self.send(due);
    }

    fn send(&self, due: Vec<(Target, Wake)>) {
        for (target, wake) in due {
            let device = DeviceRef {
                name: (self.sources.name_of)(&target.device_id).unwrap_or_default(),
                id: target.device_id.clone(),
            };
            let host = endpoint_host(&target.endpoint).unwrap_or_default();
            let wake = match wake {
                Wake::NeedsYou => "needs_you",
                Wake::Finished => "finished",
            };
            let record = {
                let (audit, device, host) = (self.audit.clone(), device.clone(), host.clone());
                #[cfg(feature = "remote-test-hooks")]
                let pending = self.pending.clone();
                move |status: String| {
                    audit.record(AuditEvent::Push {
                        device,
                        host,
                        wake: wake.to_owned(),
                        status,
                    });
                    #[cfg(feature = "remote-test-hooks")]
                    pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            };
            let still_due: StillDue = {
                let (targets, connected) = (self.sources.targets.clone(), self.sources.connected.clone());
                let (device, endpoint) = (target.device_id.clone(), target.endpoint.clone());
                Box::new(move || {
                    !connected().contains(&device)
                        && targets()
                            .iter()
                            .any(|t| t.device_id == device && t.endpoint == endpoint)
                })
            };
            #[cfg(feature = "remote-test-hooks")]
            self.pending.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.post.post(&target.endpoint, still_due, Box::new(record)).is_err() {
                #[cfg(feature = "remote-test-hooks")]
                self.pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                self.audit.record(AuditEvent::Push {
                    device,
                    host,
                    wake: wake.to_owned(),
                    status: "dropped".to_owned(),
                });
            }
        }
    }
}

/// Starts the notifier of a `serve` whose hub is `hub`, posting through `helper`. Call it after the sandbox.
pub fn start(control: &Path, hub: &Arc<Hub>, helper: Helper) -> io::Result<()> {
    start_with(control, Sources::of_hub(hub, control), helper, hub.audit().clone())
}

/// [`start`] with the sources and the poster given.
pub fn start_with(control: &Path, sources: Sources, post: impl Poster, audit: Arc<Audit>) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let reader = Reader::start(control, SCHEMA_V, move |event| {
        let _ = tx.send(event);
    })?;
    let mut notifier = Notifier::new(sources, post, audit);
    #[cfg(feature = "remote-test-hooks")]
    let turn_file = notifier.audit.notifier_turn_path_for_tests();
    #[cfg(feature = "remote-test-hooks")]
    let observe_turns = std::env::var_os(crate::notify::post::POST_ALLOW_TEST_VAR).is_some();
    std::thread::Builder::new()
        .name("eitri-remote-notify".into())
        .spawn(move || {
            let _reader = reader;
            #[cfg(feature = "remote-test-hooks")]
            let mut turns = 0u64;
            loop {
                match rx.recv_timeout(NOTIFY_TICK) {
                    Ok(ReaderEvent::Change(change)) => notifier.on_change(&change, Instant::now()),
                    Ok(ReaderEvent::Error(why)) => {
                        eprintln!("[remote] no more wakes: the rows are no longer read: {why}");
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        notifier.tick(Instant::now());
                        #[cfg(feature = "remote-test-hooks")]
                        if observe_turns && notifier.pending.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                            turns += 1;
                            // The count follows the rules step and completion of all earlier posts.
                            if let Some(path) = &turn_file {
                                record_turn_for_tests(path, turns)
                                    .expect("the notifier marker is writable inside the audit directory");
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        })?;
    Ok(())
}

#[cfg(feature = "remote-test-hooks")]
fn record_turn_for_tests(path: &Path, turns: u64) -> io::Result<()> {
    // The sandbox allows creation and replacement inside audit, but forbids truncation.
    // Publish whole counts so a reader cannot mistake a partial update for completed turns.
    let dir = agent::private_fs::open_dir(path.parent().expect("the marker has an audit directory"))?;
    agent::private_fs::publish_private_file(
        &dir,
        path.file_name().expect("the marker has a file name"),
        turns.to_string().as_bytes(),
    )?;
    Ok(())
}

/// Until when the desk counts as in use: [`DESK_ACTIVE_HOLD`] after the newest touch of a file in `presence` (whose
/// clock is `wall`; `now` is the same moment on the monotonic clock). `None` when no file was touched within it.
pub fn desk_active_until(presence: &Path, wall: SystemTime, now: Instant) -> Option<Instant> {
    let entries = std::fs::read_dir(presence).ok()?;
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            let age = wall.duration_since(meta.modified().ok()?).unwrap_or(Duration::ZERO);
            DESK_ACTIVE_HOLD.checked_sub(age).filter(|left| !left.is_zero())
        })
        .max()
        .map(|left| now + left)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::agents::{Class, Row, View};
    use std::sync::Mutex;

    fn scratch(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-notifier-tests")
                .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[derive(Clone, Default)]
    struct Posted(Arc<Mutex<Vec<String>>>);

    impl Poster for Posted {
        fn post(&self, endpoint: &str, _still_due: StillDue, done: Box<dyn FnOnce(String) + Send>) -> io::Result<()> {
            self.0.lock().unwrap().push(endpoint.to_owned());
            done("200".to_owned());
            Ok(())
        }
    }

    fn sources(presence: &Path) -> Sources {
        Sources {
            targets: Arc::new(|| {
                vec![Target {
                    device_id: "d1".into(),
                    endpoint: "https://push.example/up/d1".into(),
                    finished_keys: BTreeSet::new(),
                }]
            }),
            connected: Arc::new(BTreeSet::new),
            name_of: Box::new(|_| Some("phone".into())),
            presence: presence.to_owned(),
        }
    }

    fn row(key: &str, class: Class) -> Change {
        Change::Row {
            row: Row {
                key: key.into(),
                class,
                ..Row::default()
            },
        }
    }

    fn touch(path: &Path, at: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .unwrap();
        file.set_modified(at).unwrap();
    }

    #[cfg(feature = "remote-test-hooks")]
    #[test]
    fn idle_turn_markers_use_the_writable_audit_directory() {
        let dir = scratch("turn-marker");
        let audit_dir = dir.join("audit");
        let log = eitri_core::remote_audit::AuditLog::open(
            &audit_dir,
            eitri_core::remote_audit::WriterKind::Serve,
            Some(90),
            eitri_core::remote_audit::JournalTarget::Off,
        )
        .unwrap();
        let audit = Audit::new(log, &audit_dir);
        let notifier = Notifier::new(sources(&dir.join(PRESENCE_DIR)), Posted::default(), audit);
        let marker = notifier.audit.notifier_turn_path_for_tests().unwrap();
        assert_eq!(
            marker.parent(),
            Some(audit_dir.as_path()),
            "only the audit directory permits creation"
        );
        // Landlock confines just this worker and its descendants, leaving libtest unchanged.
        // Use serve's audit-directory rights, which allow replacement but no truncation.
        std::thread::spawn(move || {
            use landlock::{
                path_beneath_rules, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
            };
            let status = Ruleset::default()
                .handle_access(AccessFs::from_all(ABI::V9))
                .unwrap()
                .create()
                .unwrap()
                .add_rules(path_beneath_rules(
                    [audit_dir.as_path()],
                    AccessFs::ReadFile
                        | AccessFs::ReadDir
                        | AccessFs::WriteFile
                        | AccessFs::MakeReg
                        | AccessFs::RemoveFile,
                ))
                .unwrap()
                .restrict_self()
                .unwrap();
            for turns in [1, 9, 10, 20, 100, 1] {
                record_turn_for_tests(&marker, turns).unwrap();
                assert_eq!(std::fs::read_to_string(&marker).unwrap(), turns.to_string());
            }
            let file = std::fs::File::options().write(true).open(&marker).unwrap();
            if status.ruleset == RulesetStatus::FullyEnforced {
                // A supporting kernel must also reject the operation the marker avoids.
                assert_eq!(file.set_len(0).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            }
        })
        .join()
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn presence_file_holds_needs_you() {
        let dir = scratch("presence");
        let presence = dir.join(PRESENCE_DIR);
        std::fs::create_dir_all(&presence).unwrap();
        let wall = SystemTime::now();
        touch(&presence.join("a3f09c1d2e4b5f60"), wall);
        let posted = Posted::default();
        let mut notifier = Notifier::new(sources(&presence), posted.clone(), Audit::off());
        let start = Instant::now();
        notifier.on_change_at(&Change::Reset { view: View::default() }, start, || wall);
        notifier.on_change_at(&row("k-1", Class::Needs), start, || wall);
        notifier.tick_at(start + Duration::from_secs(1), || wall + Duration::from_secs(1));
        assert!(
            posted.0.lock().unwrap().is_empty(),
            "a needs-you went while the desk was in use"
        );
        // The desk last touched long enough ago: the held wake goes.
        touch(
            &presence.join("a3f09c1d2e4b5f60"),
            wall - DESK_ACTIVE_HOLD - Duration::from_secs(1),
        );
        notifier.tick_at(start + Duration::from_secs(2), || wall + Duration::from_secs(2));
        assert_eq!(*posted.0.lock().unwrap(), ["https://push.example/up/d1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn desk_presence_is_the_newest_touch_within_the_hold() {
        let dir = scratch("until");
        let (wall, now) = (SystemTime::now(), Instant::now());
        assert_eq!(desk_active_until(&dir.join("missing"), wall, now), None);
        assert_eq!(desk_active_until(&dir, wall, now), None);
        touch(&dir.join("old"), wall - DESK_ACTIVE_HOLD - Duration::from_secs(5));
        assert_eq!(desk_active_until(&dir, wall, now), None);
        touch(&dir.join("recent"), wall - Duration::from_secs(20));
        let until = desk_active_until(&dir, wall, now).unwrap();
        assert_eq!(until, now + DESK_ACTIVE_HOLD - Duration::from_secs(20));
        // A directory there is not a window's file.
        std::fs::create_dir(dir.join("sub")).unwrap();
        assert_eq!(desk_active_until(&dir, wall, now), Some(until));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_queued_wake_is_checked_again_before_it_goes() {
        struct Held(Arc<Mutex<Vec<StillDue>>>);
        impl Poster for Held {
            fn post(&self, _: &str, still_due: StillDue, _: Box<dyn FnOnce(String) + Send>) -> io::Result<()> {
                self.0.lock().unwrap().push(still_due);
                Ok(())
            }
        }
        let dir = scratch("queued");
        let endpoint = Arc::new(Mutex::new(Some("https://push.example/up/d1".to_owned())));
        let connected = Arc::new(Mutex::new(BTreeSet::new()));
        let sources = Sources {
            targets: Arc::new({
                let endpoint = endpoint.clone();
                move || {
                    endpoint
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|endpoint| Target {
                            device_id: "d1".into(),
                            endpoint: endpoint.clone(),
                            finished_keys: BTreeSet::new(),
                        })
                        .collect()
                }
            }),
            connected: Arc::new({
                let connected = connected.clone();
                move || connected.lock().unwrap().clone()
            }),
            name_of: Box::new(|_| Some("phone".into())),
            presence: dir.join(PRESENCE_DIR),
        };
        let held = Arc::new(Mutex::new(Vec::new()));
        let mut notifier = Notifier::new(sources, Held(held.clone()), Audit::off());
        let now = Instant::now();
        notifier.on_change(&Change::Reset { view: View::default() }, now);
        notifier.on_change(&row("k-1", Class::Needs), now);
        let still_due = held.lock().unwrap().pop().expect("a wake was queued");
        assert!(still_due(), "a wake still due was held back");
        connected.lock().unwrap().insert("d1".to_owned());
        assert!(!still_due(), "a phone that connected meanwhile would be woken");
        connected.lock().unwrap().clear();
        *endpoint.lock().unwrap() = Some("https://push.example/up/new".to_owned());
        assert!(!still_due(), "a replaced endpoint would get the wake");
        *endpoint.lock().unwrap() = None;
        assert!(!still_due(), "a revoked phone would be woken");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dropped_wake_is_recorded_as_dropped() {
        struct Full;
        impl Poster for Full {
            fn post(&self, _: &str, _: StillDue, _: Box<dyn FnOnce(String) + Send>) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "dropped"))
            }
        }
        let dir = scratch("dropped");
        let log = eitri_core::remote_audit::AuditLog::open(
            &dir.join("audit"),
            eitri_core::remote_audit::WriterKind::Serve,
            Some(90),
            eitri_core::remote_audit::JournalTarget::Off,
        )
        .unwrap();
        let audit = Audit::new(log, &dir.join("audit"));
        let mut notifier = Notifier::new(sources(&dir.join(PRESENCE_DIR)), Full, audit);
        let now = Instant::now();
        notifier.on_change(&Change::Reset { view: View::default() }, now);
        notifier.on_change(&row("k-1", Class::Needs), now);
        let records =
            eitri_core::remote_audit::read(&dir.join("audit"), &eitri_core::remote_audit::Filter::default(), None)
                .unwrap();
        let push = records.iter().find(|r| r.event == "push").expect("a push record");
        let line = serde_json::to_string(&push.fields).unwrap();
        assert!(
            line.contains("dropped") && line.contains("push.example") && !line.contains("/up/d1"),
            "{line}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
