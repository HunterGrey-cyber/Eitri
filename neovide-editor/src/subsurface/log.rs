//! Opt-in presentation diagnostics. Only the writer touches the file; producers never wait for it.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::editor_area::PresentationCounts;

const HEADER: &str = "event,pane,key_id,key_ns,commit_ns,presented_ns,refresh_ns,keyval,keycode";
const CAPACITY: usize = 4096;

pub(crate) fn now_ns() -> u64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: time is writable and CLOCK_MONOTONIC needs no process-owned resource.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0 {
        (time.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(time.tv_nsec as u64)
    } else {
        0
    }
}

enum Message {
    Line(String),
    Counts(u64, PresentationCounts),
}

struct Queue {
    tx: mpsc::SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}

impl Queue {
    fn new() -> (Self, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::sync_channel(CAPACITY);
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    fn push(&self, message: Message) {
        if self.tx.try_send(message).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct Logger {
    queue: Queue,
    stop: Arc<AtomicBool>,
    done: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();

fn logger() -> Option<&'static Logger> {
    LOGGER
        .get_or_init(|| {
            let path = std::env::var_os("EITRI_EDITOR_PRESENT_LOG").filter(|p| !p.is_empty())?;
            let (queue, rx) = Queue::new();
            let dropped = queue.dropped.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("editor-present-log".into())
                .spawn(move || {
                    write_log(path.into(), rx, dropped, worker_stop);
                    let _ = done_tx.send(());
                })
                .ok()?;
            // SAFETY: the callback has no arguments and uses only process-lifetime state.
            unsafe {
                libc::atexit(finish_log);
            }
            Some(Logger {
                queue,
                stop,
                done: std::sync::Mutex::new(Some(done_rx)),
            })
        })
        .as_ref()
}

extern "C" fn finish_log() {
    if let Some(Some(logger)) = LOGGER.get() {
        logger.stop.store(true, Ordering::Release);
        let done = logger.done.try_lock().ok().and_then(|mut done| done.take());
        if let Some(done) = done {
            let _ = done.recv_timeout(Duration::from_millis(100));
        }
    }
}

/// Formats a `counts` row with a `pane=<id>` cell followed by `name=value` counter cells.
/// `dropped_lines` is included only for the process total, pane 0.
fn counts_line(pane: u64, c: PresentationCounts, dropped: Option<u64>) -> String {
    let mut line = format!(
        "counts,pane={pane},own_frames={},fallback_frames={},failures={},child_commits={},child_frames={},transaction_misses={},open_deferred={},stale_callbacks={},fallbacks={}",
        c.own_frames, c.fallback_frames, c.failures, c.child_commits, c.child_frames, c.transaction_misses,
        c.open_deferred, c.stale_callbacks, c.fallbacks
    );
    if let Some(dropped) = dropped {
        line.push_str(&format!(",dropped_lines={dropped}"));
    }
    line
}

fn write_log(path: PathBuf, rx: mpsc::Receiver<Message>, dropped: Arc<AtomicU64>, stop: Arc<AtomicBool>) {
    let file = match std::fs::File::create(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("[editor] cannot open presentation log {}: {error}", path.display());
            return;
        }
    };
    let mut out = BufWriter::new(file);
    let _ = writeln!(out, "{HEADER}");
    let mut counts = BTreeMap::new();
    let mut last_counts = Instant::now();
    loop {
        let message = if stop.load(Ordering::Acquire) {
            rx.try_recv().ok()
        } else {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(message) => Some(message),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => None,
            }
        };
        match message {
            Some(Message::Line(line)) => {
                let _ = writeln!(out, "{line}");
            }
            Some(Message::Counts(pane, snapshot)) => {
                counts.insert(pane, snapshot);
            }
            None if stop.load(Ordering::Acquire) => break,
            None => {
                let _ = out.flush();
            }
        }
        if last_counts.elapsed() >= Duration::from_secs(10) {
            write_counts(&mut out, &counts, dropped.load(Ordering::Relaxed));
            let _ = out.flush();
            last_counts = Instant::now();
        }
    }
    write_counts(&mut out, &counts, dropped.load(Ordering::Relaxed));
    let _ = out.flush();
}

fn write_counts(out: &mut impl Write, counts: &BTreeMap<u64, PresentationCounts>, dropped: u64) {
    let mut total = PresentationCounts::default();
    for (&pane, &c) in counts {
        let _ = writeln!(out, "{}", counts_line(pane, c, None));
        total.own_frames += c.own_frames;
        total.fallback_frames += c.fallback_frames;
        total.failures += c.failures;
        total.child_commits += c.child_commits;
        total.child_frames += c.child_frames;
        total.transaction_misses += c.transaction_misses;
        total.open_deferred += c.open_deferred;
        total.stale_callbacks += c.stale_callbacks;
        total.fallbacks += c.fallbacks;
    }
    let _ = writeln!(out, "{}", counts_line(0, total, Some(dropped)));
}

#[derive(Clone, Copy)]
struct Key {
    id: u64,
    ns: u64,
    keyval: u32,
    keycode: u32,
    epoch: u64,
}

#[derive(Default)]
struct Epochs {
    current: u64,
    keys: VecDeque<Key>,
}

impl Epochs {
    #[cfg(test)]
    fn batch(&mut self) {
        self.current = self.current.saturating_add(1);
    }
    fn key(&mut self, id: u64, ns: u64, keyval: u32, keycode: u32) {
        self.keys.push_back(Key {
            id,
            ns,
            keyval,
            keycode,
            epoch: self.current,
        });
    }
    fn claim(&mut self, frame_epoch: u64) -> Vec<Key> {
        let mut claimed = Vec::new();
        while self.keys.front().is_some_and(|key| key.epoch < frame_epoch) {
            if let Some(key) = self.keys.pop_front() {
                claimed.push(key);
            }
        }
        claimed
    }
}

pub(crate) struct PaneLog {
    id: u64,
    logger: &'static Logger,
    epochs: std::cell::RefCell<Epochs>,
    next_key: std::cell::Cell<u64>,
    rendered_epoch: std::cell::Cell<Option<u64>>,
}

impl PaneLog {
    pub(crate) fn new() -> Option<std::rc::Rc<Self>> {
        static NEXT_PANE: AtomicU64 = AtomicU64::new(1);
        Some(std::rc::Rc::new(Self {
            id: NEXT_PANE.fetch_add(1, Ordering::Relaxed),
            logger: logger()?,
            epochs: Default::default(),
            next_key: std::cell::Cell::new(0),
            rendered_epoch: std::cell::Cell::new(None),
        }))
    }
    pub(crate) fn begin_render(&self) {
        self.rendered_epoch.set(None);
    }
    pub(crate) fn rendered(&self, epoch: u64) {
        self.rendered_epoch.set(Some(epoch));
    }
    pub(crate) fn key(&self, epoch: Option<u64>, keyval: u32, keycode: u32) {
        let id = self.next_key.get().saturating_add(1);
        self.next_key.set(id);
        let ns = now_ns();
        self.line(format!("key,{},{id},{ns},0,0,0,{keyval},{keycode}", self.id));
        let Some(epoch) = epoch else {
            self.key_line(
                "missing",
                Key {
                    id,
                    ns,
                    keyval,
                    keycode,
                    epoch: 0,
                },
                0,
            );
            return;
        };
        let mut epochs = self.epochs.borrow_mut();
        epochs.current = epoch;
        if epochs.keys.len() == CAPACITY {
            if let Some(key) = epochs.keys.pop_front() {
                self.key_line("missing", key, 0);
            }
        }
        epochs.key(id, ns, keyval, keycode);
    }
    fn line(&self, line: String) {
        self.logger.queue.push(Message::Line(line));
    }
    fn key_line(&self, event: &str, key: Key, commit: u64) {
        self.line(format!(
            "{event},{},{},{},{commit},0,0,{},{}",
            self.id, key.id, key.ns, key.keyval, key.keycode
        ));
    }
    pub(crate) fn event(&self, event: &str, seq: u64) {
        self.line(format!("{event},{},{seq},0,{},0,0,0,0", self.id, now_ns()));
    }
    pub(crate) fn commit(&self, seq: u64) {
        let ns = now_ns();
        self.line(format!("commit,{},{seq},0,{ns},0,0,0,0", self.id));
        let Some(epoch) = self.rendered_epoch.take() else {
            return;
        };
        for key in self.epochs.borrow_mut().claim(epoch) {
            self.key_line("key_commit", key, ns);
        }
    }
    pub(crate) fn presented(&self, seq: u64, ns: u64, refresh: u32) {
        self.line(format!("presented,{},{seq},0,0,{ns},{refresh},0,0", self.id));
    }
    pub(crate) fn transaction(&self, transform: (f64, f64)) {
        self.line(format!(
            "transaction,{},0,0,{},0,0,{},{}",
            self.id,
            now_ns(),
            transform.0,
            transform.1
        ));
    }
    pub(crate) fn counts(&self, counts: PresentationCounts) {
        self.logger.queue.push(Message::Counts(self.id, counts));
    }
    pub(crate) fn missing(&self) {
        for key in self.epochs.borrow_mut().keys.drain(..) {
            self.key_line("missing", key, 0);
        }
    }
}

impl Drop for PaneLog {
    fn drop(&mut self) {
        self.missing();
    }
}

#[cfg(feature = "fault-injection")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    Globals,
    Dmabuf,
    Alloc,
    BufferFailed,
    Thread,
}

#[cfg(feature = "fault-injection")]
impl Fault {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "globals" => Some(Self::Globals),
            "dmabuf" => Some(Self::Dmabuf),
            "alloc" => Some(Self::Alloc),
            "buffer-failed" => Some(Self::BufferFailed),
            "thread" => Some(Self::Thread),
            _ => None,
        }
    }
}

#[cfg(feature = "fault-injection")]
pub(super) fn fault_requested(point: Fault) -> bool {
    static FAULT: OnceLock<Option<Fault>> = OnceLock::new();
    *FAULT.get_or_init(|| {
        let value = std::env::var("EITRI_EDITOR_SUBSURFACE_FAIL").ok()?;
        let fault = Fault::parse(&value);
        if fault.is_none() {
            eprintln!("[editor] unknown subsurface fault point: {value}");
        }
        fault
    }) == Some(point)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process-total row of an empty log.
    const TOTAL_ZERO: &str = "counts,pane=0,own_frames=0,fallback_frames=0,failures=0,child_commits=0,child_frames=0,transaction_misses=0,open_deferred=0,stale_callbacks=0,fallbacks=0,dropped_lines=0";

    #[test]
    fn counts_rows_name_every_counter_and_the_total_comes_last_as_present_log_reads_them() {
        let one = PresentationCounts {
            child_commits: 2,
            child_frames: 1,
            transaction_misses: 3,
            ..Default::default()
        };
        let mut counts = BTreeMap::new();
        counts.insert(1, one);
        counts.insert(2, one);
        let mut out = Vec::new();
        write_counts(&mut out, &counts, 7);
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "counts,pane=1,own_frames=0,fallback_frames=0,failures=0,child_commits=2,child_frames=1,transaction_misses=3,open_deferred=0,stale_callbacks=0,fallbacks=0"
        );
        assert_eq!(
            lines[2],
            "counts,pane=0,own_frames=0,fallback_frames=0,failures=0,child_commits=4,child_frames=2,transaction_misses=6,open_deferred=0,stale_callbacks=0,fallbacks=0,dropped_lines=7"
        );
    }

    #[test]
    fn the_log_header_and_a_key_commit_line() {
        let path = std::env::temp_dir().join(format!("eitri-present-{}.csv", std::process::id()));
        let (queue, rx) = Queue::new();
        let logger = Box::leak(Box::new(Logger {
            queue,
            stop: Arc::new(AtomicBool::new(true)),
            done: std::sync::Mutex::new(None),
        }));
        let pane = PaneLog {
            id: 1,
            logger,
            epochs: Default::default(),
            next_key: std::cell::Cell::new(0),
            rendered_epoch: std::cell::Cell::new(None),
        };
        pane.key_line(
            "key_commit",
            Key {
                id: 2,
                ns: 3,
                keyval: 97,
                keycode: 38,
                epoch: 0,
            },
            4,
        );
        write_log(
            path.clone(),
            rx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(true)),
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], HEADER);
        assert_eq!(lines[1], "key_commit,1,2,3,4,0,0,97,38");
        assert_eq!(lines[2], TOTAL_ZERO);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_log_never_blocks_when_the_channel_is_full() {
        let (queue, _rx) = Queue::new();
        for _ in 0..4097 {
            queue.push(Message::Line(String::new()));
        }
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_old_frame_cannot_claim_a_newer_key() {
        let mut epochs = Epochs::default();
        epochs.batch();
        let old_frame = epochs.current;
        epochs.batch();
        epochs.key(1, 10, 97, 38);
        assert!(epochs.claim(old_frame).is_empty());
        assert!(epochs.claim(epochs.current).is_empty());
        epochs.batch();
        assert_eq!(epochs.claim(epochs.current).len(), 1);
        assert!(epochs.claim(epochs.current).is_empty());
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn fail_points_parse_and_refuse_unknown() {
        for name in ["globals", "dmabuf", "alloc", "buffer-failed", "thread"] {
            assert!(Fault::parse(name).is_some());
        }
        for name in ["", "GLOBALS", "globals ", "unknown"] {
            assert!(Fault::parse(name).is_none());
        }
    }
}
