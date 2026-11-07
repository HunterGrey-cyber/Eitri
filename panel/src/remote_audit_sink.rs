//! A window's writer of the remote audit log (`eitri_core::remote_audit`): the records only a window sees (its
//! enrollments and the desk's answers about devices, the commands it carries out, the refusals it decides), written
//! under the window's instance id with no age limit, beside `serve`'s.
//!
//! Recording never waits. [`AuditSink::record`] is called on the tick's thread (and on the `rv-` socket's readers):
//! it hands the record through a queue of [`AUDIT_QUEUE`] to a thread of the sink's own, which takes the log's lock
//! and writes. A record that finds the queue full is dropped and counted, and the count rides on the next refusal the
//! queue takes (its `dropped`), since refusals are what can arrive faster than a disk takes them; so a slow or stuck
//! disk costs records, never the tick.
//!
//! The log is opened by that thread at the first record, so a window that records nothing makes no directory. When it
//! cannot be opened the sink drops every record from then on; when a record cannot be written (the directory taken
//! away, the lock held past its wait) the next is tried as usual. The first of either is said once on stderr and once
//! to the window ([`AuditSink::take_notice`], which the socket's pump shows in the base page, woken for it); later
//! failed writes are counted on stderr, one line in a thousand.

use eitri_core::remote_audit::{audit_dir, AuditLog, Event, JournalTarget, WriterKind};
use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

/// The records a window's sink holds for its thread; one more is dropped and counted.
pub const AUDIT_QUEUE: usize = 1024;

/// Writes one record.
type Writer = Box<dyn FnMut(Event) -> io::Result<()> + Send>;

/// Opens the log, on the sink's thread.
type Opener = Box<dyn FnOnce() -> io::Result<Writer> + Send>;

enum Msg {
    Record(Event),
    /// Answered once every message queued before it was handled.
    #[cfg(test)]
    Flush(SyncSender<()>),
}

/// What the sink says when the log cannot be written at all.
struct Failure {
    /// The words for the window, until it takes them.
    notice: Option<String>,
    /// Whether it was said.
    said: bool,
    /// The window's wake, so the words are shown without waiting for something else to happen there.
    wake: agent::UiWake,
}

struct Inner {
    instance: String,
    tx: Option<SyncSender<Msg>>,
    /// Records dropped at a full queue and not yet counted on a record the queue took.
    held: Mutex<u64>,
    failure: Arc<Mutex<Failure>>,
}

/// A window's handle on the audit log; clones share one queue and one thread.
#[derive(Clone)]
pub struct AuditSink {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AuditSink {
    /// The sink of the window `instance`, writing `audit/` inside `remote_state` (where the device store is). Nothing
    /// is opened or made until the first record. `wake` is raised when the window has something to show about the log
    /// ([`take_notice`](Self::take_notice)), which may be long after the tick that recorded.
    pub fn open(remote_state: &Path, instance: &str, wake: agent::UiWake) -> AuditSink {
        let dir = audit_dir(remote_state);
        let writer = WriterKind::Instance(instance.to_owned());
        AuditSink::start(
            instance,
            AUDIT_QUEUE,
            wake,
            Box::new(move || match AuditLog::open(&dir, writer, None, journal()) {
                Ok(mut log) => Ok(Box::new(move |event| {
                    log.record(event).map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!(
                                "a record was not written to the remote audit log in {}: {e}",
                                dir.display()
                            ),
                        )
                    })
                }) as Writer),
                Err(e) => Err(io::Error::new(
                    e.kind(),
                    format!(
                        "the remote audit log could not be opened in {}: {e}; this window records nothing in it",
                        dir.display()
                    ),
                )),
            }),
        )
    }

    /// A sink with a queue of `capacity` whose thread opens its writer with `open`.
    fn start(instance: &str, capacity: usize, wake: agent::UiWake, open: Opener) -> AuditSink {
        let failure = Arc::new(Mutex::new(Failure {
            notice: None,
            said: false,
            wake,
        }));
        let (tx, rx) = mpsc::sync_channel(capacity);
        let theirs = Arc::clone(&failure);
        let spawned = std::thread::Builder::new()
            .name("rv-audit".into())
            .spawn(move || run(rx, open, &theirs));
        let tx = match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                fail(
                    &failure,
                    &format!("no thread for the remote audit log: {e}; this window records nothing in it"),
                );
                None
            }
        };
        AuditSink {
            inner: Arc::new(Inner {
                instance: instance.to_owned(),
                tx,
                held: Mutex::new(0),
                failure,
            }),
        }
    }

    /// Whether `other` is this sink or a clone of it.
    pub fn is(&self, other: &AuditSink) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// The window's instance id, which its records are written under.
    pub fn instance(&self) -> &str {
        &self.inner.instance
    }

    /// Hands `event` to the sink's thread and returns at once. At a full queue it is dropped and counted; a refusal
    /// the queue takes carries every count not yet carried.
    pub fn record(&self, mut event: Event) {
        let Some(tx) = &self.inner.tx else { return };
        let mut held = lock(&self.inner.held);
        let carried = match &mut event {
            Event::Refused { dropped, .. } if *held > 0 => {
                *dropped = Some(dropped.unwrap_or(0).saturating_add(*held));
                *held
            }
            _ => 0,
        };
        match tx.try_send(Msg::Record(event)) {
            Ok(()) => *held -= carried,
            Err(TrySendError::Full(_)) => *held = held.saturating_add(1),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// What the window is to show about the log, once: the first time a record could not be written, and why.
    pub fn take_notice(&self) -> Option<String> {
        lock(&self.inner.failure).notice.take()
    }

    /// Waits until the thread has handled every record queued before this call.
    #[cfg(test)]
    pub(crate) fn flush(&self) {
        let Some(tx) = &self.inner.tx else { return };
        let (done, wait) = mpsc::sync_channel(1);
        if tx.send(Msg::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

/// The journal a window's records are copied to; none in this crate's own tests, which must never reach the real one.
fn journal() -> JournalTarget {
    #[cfg(test)]
    {
        JournalTarget::Off
    }
    #[cfg(not(test))]
    {
        eitri_core::remote_audit::journal_target()
    }
}

/// Says `why` on stderr and keeps it for the window, the first time only, raising the window's wake so it is shown
/// now: the tick that recorded has long finished, and nothing else may be due there.
fn fail(failure: &Mutex<Failure>, why: &str) {
    let wake = {
        let mut failure = lock(failure);
        if failure.said {
            return;
        }
        failure.said = true;
        eprintln!("[agent_panel] remote: {why}");
        failure.notice = Some(why.to_owned());
        Arc::clone(&failure.wake)
    };
    wake();
}

/// The sink's thread: opens the writer at the first record and writes each record in turn until every sender is gone.
fn run(rx: Receiver<Msg>, open: Opener, failure: &Mutex<Failure>) {
    let mut open = Some(open);
    let mut writer: Option<Writer> = None;
    let mut failed: u64 = 0;
    for msg in rx {
        let event = match msg {
            Msg::Record(event) => event,
            #[cfg(test)]
            Msg::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        if writer.is_none() {
            let Some(open) = open.take() else { continue };
            match open() {
                Ok(opened) => writer = Some(opened),
                Err(e) => {
                    fail(failure, &e.to_string());
                    continue;
                }
            }
        }
        let Some(write) = writer.as_mut() else { continue };
        if let Err(e) = write(event) {
            failed += 1;
            if failed == 1 {
                fail(failure, &e.to_string());
            } else if failed % 1000 == 1 {
                eprintln!("[agent_panel] remote: {e} ({failed} so far)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::remote_audit::{read, DeviceRef, Filter, Reason};
    use std::time::Duration;

    fn no_wake() -> agent::UiWake {
        Arc::new(|| {})
    }

    fn refused(n: u64) -> Event {
        Event::Refused {
            reason: Reason::Busy,
            device: Some(DeviceRef {
                id: format!("d{n}"),
                name: "phone".into(),
            }),
            listener: None,
            peer: None,
            key: None,
            kind: Some("send".into()),
            dropped: None,
        }
    }

    #[test]
    fn a_slow_disk_never_blocks_the_tick() {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let written: Arc<Mutex<Vec<Event>>> = Arc::default();
        let kept = Arc::clone(&written);
        let mut gate = Some(gate_rx);
        let sink = AuditSink::start(
            "a1b2c3d4e5f60718",
            16,
            no_wake(),
            Box::new(move || {
                Ok(Box::new(move |event| {
                    // The first write waits until the test lets the disk go.
                    if let Some(gate) = gate.take() {
                        let _ = gate.recv();
                    }
                    lock(&kept).push(event);
                    Ok(())
                }) as Writer)
            }),
        );
        let recording = sink.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for n in 0..10_000 {
                recording.record(refused(n));
            }
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("every record returns while the disk is held");
        let carried_before: u64 = lock(&written)
            .iter()
            .filter_map(|e| match e {
                Event::Refused { dropped, .. } => *dropped,
                _ => None,
            })
            .sum();
        assert_eq!(carried_before, 0, "nothing was written while the disk was held");
        drop(gate_tx);
        sink.flush();
        let kept_now = lock(&written).len() as u64;
        assert!(kept_now < 10_000, "the bounded queue dropped some: {kept_now} kept");
        sink.record(refused(10_000));
        sink.flush();
        let written = lock(&written);
        assert_eq!(written.len() as u64, kept_now + 1);
        let counts: Vec<u64> = written
            .iter()
            .map(|e| match e {
                Event::Refused { dropped, .. } => dropped.unwrap_or(0),
                _ => 0,
            })
            .collect();
        assert!(
            *counts.last().unwrap() > 0,
            "the next record says how many were dropped"
        );
        assert_eq!(
            counts.iter().sum::<u64>(),
            10_000 - kept_now,
            "every record was written or counted, once"
        );
    }

    #[test]
    fn a_count_waits_for_a_refusal_and_is_carried_once() {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let written: Arc<Mutex<Vec<Event>>> = Arc::default();
        let kept = Arc::clone(&written);
        let mut gate = Some(gate_rx);
        let sink = AuditSink::start(
            "a1b2c3d4e5f60718",
            2,
            no_wake(),
            Box::new(move || {
                Ok(Box::new(move |event| {
                    if let Some(gate) = gate.take() {
                        let _ = entered_tx.send(());
                        let _ = gate.recv();
                    }
                    lock(&kept).push(event);
                    Ok(())
                }) as Writer)
            }),
        );
        // The thread holds the first record; two more fill the queue, and the fourth is dropped.
        sink.record(refused(0));
        entered_rx.recv().unwrap();
        for n in 1..=3 {
            sink.record(refused(n));
        }
        assert_eq!(*lock(&sink.inner.held), 1);
        drop(gate_tx);
        sink.flush();
        // A record that is not a refusal does not carry the count; the next refusal does, once.
        sink.record(Event::Disable {
            why: "not a refusal".into(),
        });
        sink.flush();
        sink.record(refused(4));
        sink.flush();
        sink.record(refused(5));
        sink.flush();
        let counts: Vec<Option<u64>> = lock(&written)
            .iter()
            .filter_map(|e| match e {
                Event::Refused { dropped, .. } => Some(*dropped),
                _ => None,
            })
            .collect();
        assert_eq!(counts, [None, None, None, Some(1), None]);
    }

    #[test]
    fn unopenable_log_says_so_once() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("audit-sink-unopenable");
        // A file where the state directory goes, so the audit directory inside it cannot be made.
        let state = dir.join("state");
        std::fs::write(&state, b"not a directory").unwrap();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let raised = Arc::clone(&wakes);
        let wake: agent::UiWake = Arc::new(move || {
            raised.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let sink = AuditSink::open(&state, "a1b2c3d4e5f60718", wake);
        assert_eq!(sink.take_notice(), None, "nothing is opened before a record");
        for n in 0..3 {
            sink.record(refused(n));
        }
        sink.flush();
        // Nothing else may be due on the tick: the wake is what gets the notice shown.
        assert_eq!(
            wakes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the window is woken to show it"
        );
        let notice = sink.take_notice().expect("the window is told");
        assert!(notice.contains("remote audit log"), "{notice}");
        sink.record(refused(4));
        sink.flush();
        assert_eq!(sink.take_notice(), None, "said once");
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1, "and woken once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_reach_the_file_under_the_instance_and_a_window_that_records_nothing_makes_nothing() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("audit-sink-file");
        let quiet = AuditSink::open(&dir, "a1b2c3d4e5f60718", no_wake());
        quiet.flush();
        assert!(!audit_dir(&dir).exists(), "no record, no directory");
        quiet.record(refused(1));
        quiet.flush();
        let records = read(&audit_dir(&dir), &Filter::default(), None).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].writer, "a1b2c3d4e5f60718");
        assert_eq!(records[0].event, "refused");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
