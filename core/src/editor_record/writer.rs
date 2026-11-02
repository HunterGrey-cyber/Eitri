//! The record writer: one thread per window that does every file operation of reopening -- the record, its restore
//! marker, nvim's input for a restart and the handoff nvim leaves at its exit -- so the window's thread never touches
//! the disk for them.
//!
//! What is handed over is done in the order it was handed over. Each record carries a sequence number taken when its
//! capture was asked for (or its handoff read was asked), so a capture that answers after nvim's exit handoff was read
//! never replaces it. A record is written only when it differs from the one on disk, and a record that is the
//! restore's own result is not written until something else has been for this nvim: a restore that came back partly
//! leaves the fuller record on disk until the person changes something.
//!
//! A write that fails is reported once through `agent::save_health` and not again until a different record fails; the
//! record already on disk stays, since a write goes to a temporary name and is renamed over it.

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agent::save_health::{self, SaveKind};

use super::{parse_handoff, write_input, Record, RecordFile, RECORD_MAX};
use crate::state_paths::ON_A_WRITER_THREAD;
use crate::wm::Waker;

/// How often a handoff that is not there yet is looked for again while its wait lasts.
const HANDOFF_POLL: Duration = Duration::from_millis(10);

/// What the writer tells the window, collected with [`RecordWriter::take_answers`] after a wake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterAnswer {
    /// The handoff of `token`'s nvim: the record it held, or `None` when none came within its wait or the one there
    /// could not be used. Answered once per token.
    Handoff { token: String, record: Option<Record> },
    /// The input file for `token`'s nvim is in place (or, with the writer off, will not be: nothing waits on it).
    InputWritten { token: String },
    /// The record handed over with `seq` is on disk.
    Written { seq: u64 },
}

/// One window's record writer. Clones are handles to the same thread; the thread ends once every handle is gone and
/// what was handed to it is done.
#[derive(Clone)]
pub struct RecordWriter {
    handle: Arc<Handle>,
}

/// The handles' share: dropping the last one tells the thread no more comes.
struct Handle {
    shared: Arc<Shared>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.lock().finished = true;
        self.shared.ready.notify_all();
    }
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Leaves `answers` for the window and wakes it, outside the lock.
    fn answer(&self, answers: Vec<WriterAnswer>) {
        if answers.is_empty() {
            return;
        }
        let waker = {
            let mut state = self.lock();
            state.answers.extend(answers);
            state.waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

enum Op {
    Offer {
        seq: u64,
        record: Record,
        same_as_restored: bool,
    },
    Handoff {
        seq: u64,
        token: String,
        path: PathBuf,
        until: Instant,
    },
    Session {
        token: String,
        skip: bool,
    },
    SetMarker(String),
    ClearMarkerIf(String),
    Input {
        channel_dir: PathBuf,
        token: String,
        prefixes: Vec<PathBuf>,
        skip: bool,
    },
}

impl Op {
    /// The answer an operation owes when it is dropped unperformed, so nothing waits on a writer that is off.
    fn unperformed(self) -> Option<WriterAnswer> {
        match self {
            Op::Handoff { token, .. } => Some(WriterAnswer::Handoff { token, record: None }),
            Op::Input { token, .. } => Some(WriterAnswer::InputWritten { token }),
            _ => None,
        }
    }
}

#[derive(Default)]
struct State {
    ops: VecDeque<Op>,
    /// The thread is carrying out an operation it took.
    busy: bool,
    /// Every handle is gone: the thread ends once `ops` is empty.
    finished: bool,
    /// Nothing more is done; what is queued was dropped.
    off: bool,
    answers: Vec<WriterAnswer>,
    waker: Option<Arc<dyn Waker>>,
    /// The tokens whose handoff read was asked for: each is read once.
    asked: BTreeSet<String>,
    /// The record with the highest sequence number handed over, else the one read at start.
    newest: Option<Record>,
    /// `newest`'s sequence number; `None` while it is the start's.
    newest_seq: Option<u64>,
    #[cfg(test)]
    failures_reported: usize,
}

impl State {
    fn hand_over(&mut self, seq: u64, record: &Record) {
        if self.newest_seq.is_none_or(|newest| seq >= newest) {
            self.newest_seq = Some(seq);
            self.newest = Some(record.clone());
        }
    }
}

impl std::fmt::Debug for RecordWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordWriter")
            .field("at", &Arc::as_ptr(&self.handle))
            .finish_non_exhaustive()
    }
}

/// Two handles are equal when they are handles to the same writer.
impl PartialEq for RecordWriter {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.handle, &other.handle)
    }
}

impl RecordWriter {
    /// Starts the thread for `file`. `seed` is the record read at start: what the file holds, so the same again is
    /// not rewritten, and what [`newest`](Self::newest) answers until a record is handed over.
    pub fn start(file: RecordFile, root: PathBuf, seed: Option<Record>) -> RecordWriter {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                newest: seed.clone(),
                ..State::default()
            }),
            ready: Condvar::new(),
        });
        let thread_shared = shared.clone();
        let worker = Worker {
            file,
            root,
            last_written: seed,
            floor: None,
            written_for_nvim: false,
            session: None,
            last_failed: None,
        };
        let spawned = std::thread::Builder::new()
            .name("eitri-editor-rec".into())
            .spawn(move || worker.run(&thread_shared));
        if let Err(e) = spawned {
            eprintln!("[editor] reopen: the record writer could not start ({e}); the open files are not kept");
            shared.lock().off = true;
        }
        RecordWriter {
            handle: Arc::new(Handle { shared }),
        }
    }

    fn shared(&self) -> &Shared {
        &self.handle.shared
    }

    /// The writer calls `waker.wake()` after each answer it leaves.
    pub fn set_waker(&self, waker: Arc<dyn Waker>) {
        self.shared().lock().waker = Some(waker);
    }

    /// Hands over a record a capture answered with `seq`, the number taken when it was asked for.
    /// `same_as_restored`: the record is the restore's own result. Returns at once.
    pub fn offer(&self, seq: u64, record: Record, same_as_restored: bool) {
        let mut state = self.shared().lock();
        if state.off {
            return;
        }
        state.hand_over(seq, &record);
        state.ops.push_back(Op::Offer {
            seq,
            record,
            same_as_restored,
        });
        self.shared().ready.notify_all();
    }

    /// Reads the handoff nvim leaves at `path` at its exit, waiting up to `wait` from now for it to appear, and
    /// answers [`WriterAnswer::Handoff`]. The record in it is handed over with `seq` and the file removed. Once per
    /// token: a second call for a token already asked is ignored, its answer being the first one's.
    pub fn read_handoff(&self, seq: u64, token: String, path: PathBuf, wait: Duration) {
        let now = Instant::now();
        let until = now.checked_add(wait).unwrap_or(now);
        if !self.shared().lock().asked.insert(token.clone()) {
            return;
        }
        self.push(Op::Handoff {
            seq,
            token,
            path,
            until,
        });
    }

    /// A new nvim starts: nothing is written for it yet. With `skip` its restore was skipped because the last one did
    /// not finish, and the marker that says so stays until a record is actually written.
    pub fn begin_session(&self, token: String, skip: bool) {
        self.push(Op::Session { token, skip });
    }

    /// Writes the restore marker, holding `token`.
    pub fn set_marker(&self, token: String) {
        self.push(Op::SetMarker(token));
    }

    /// Removes the restore marker when it holds `token`; another nvim's marker stays.
    pub fn clear_marker_if(&self, token: String) {
        self.push(Op::ClearMarkerIf(token));
    }

    /// Writes nvim's input (`reopen-in.json` in `channel_dir`) for `token`'s nvim from [`newest`](Self::newest) as it
    /// stands once everything handed over before has been done, then answers [`WriterAnswer::InputWritten`].
    pub fn write_input(&self, channel_dir: PathBuf, token: String, prefixes: Vec<PathBuf>, skip: bool) {
        self.push(Op::Input {
            channel_dir,
            token,
            prefixes,
            skip,
        });
    }

    /// The answers left since the last call.
    pub fn take_answers(&self) -> Vec<WriterAnswer> {
        std::mem::take(&mut self.shared().lock().answers)
    }

    /// The record with the highest sequence number handed over, written or not, else the one read at start.
    pub fn newest(&self) -> Option<Record> {
        self.shared().lock().newest.clone()
    }

    /// Waits until everything handed over is done, at most `bound`: whether it was.
    pub fn flush(&self, bound: Duration) -> bool {
        let start = Instant::now();
        let shared = self.shared();
        let mut state = shared.lock();
        while !state.ops.is_empty() || state.busy {
            let left = bound.saturating_sub(start.elapsed());
            if left.is_zero() {
                return false;
            }
            state = shared
                .ready
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Stops all writing for good, what is queued included. A handoff read or an input still owed is answered
    /// without touching the disk, so nothing waits on a writer that is off; one under way when this is called ends
    /// at its next look.
    pub fn disable(&self) {
        let answers: Vec<WriterAnswer> = {
            let mut state = self.shared().lock();
            state.off = true;
            let dropped = std::mem::take(&mut state.ops);
            dropped.into_iter().filter_map(Op::unperformed).collect()
        };
        self.shared().ready.notify_all();
        self.shared().answer(answers);
    }

    fn push(&self, op: Op) {
        let mut state = self.shared().lock();
        if state.off {
            drop(state);
            self.shared().answer(op.unperformed().into_iter().collect());
            return;
        }
        state.ops.push_back(op);
        self.shared().ready.notify_all();
    }

    #[cfg(test)]
    fn failures_reported(&self) -> usize {
        self.shared().lock().failures_reported
    }
}

/// The thread's own state.
struct Worker {
    file: RecordFile,
    root: PathBuf,
    /// The record on disk as it was handed over (before any trim), so the same record again is not rewritten.
    last_written: Option<Record>,
    /// The highest sequence number the write rule took, written or skipped as unchanged: a record older than it is
    /// dropped.
    floor: Option<u64>,
    /// Something was written since the current nvim began.
    written_for_nvim: bool,
    /// The current nvim's token, while its restore was skipped and nothing has been written for it.
    session: Option<String>,
    /// The last record whose write failed, reported already.
    last_failed: Option<Record>,
}

impl Worker {
    fn run(mut self, shared: &Shared) {
        loop {
            let op = {
                let mut state = shared.lock();
                loop {
                    if state.off {
                        state.busy = false;
                        shared.ready.notify_all();
                        return;
                    }
                    if let Some(op) = state.ops.pop_front() {
                        state.busy = true;
                        break op;
                    }
                    if state.finished {
                        return;
                    }
                    state = shared.ready.wait(state).unwrap_or_else(PoisonError::into_inner);
                }
            };
            self.carry_out(shared, op);
            shared.lock().busy = false;
            shared.ready.notify_all();
        }
    }

    fn carry_out(&mut self, shared: &Shared, op: Op) {
        match op {
            Op::Offer {
                seq,
                record,
                same_as_restored,
            } => self.take(shared, seq, record, same_as_restored),
            Op::Handoff {
                seq,
                token,
                path,
                until,
            } => {
                let read = self.read_handoff(shared, &path, until);
                if let Some((record, same)) = &read {
                    shared.lock().hand_over(seq, record);
                    self.take(shared, seq, record.clone(), *same);
                }
                shared.answer(vec![WriterAnswer::Handoff {
                    token,
                    record: read.map(|(record, _)| record),
                }]);
            }
            Op::Session { token, skip } => {
                self.written_for_nvim = false;
                self.session = skip.then_some(token);
            }
            Op::SetMarker(token) => {
                if let Err(e) = self.locked(|file| file.set_marker(&token)) {
                    eprintln!("[editor] reopen: could not write the restore marker: {e}");
                }
            }
            Op::ClearMarkerIf(token) => {
                if let Some(skipping) = &self.session {
                    eprintln!(
                        "[editor] reopen: the restore marker stays until a record is written for nvim {skipping}"
                    );
                    return;
                }
                let cleared = self.locked(|file| match file.marker() {
                    Some(held) if held == token => file.clear_marker(),
                    _ => Ok(()),
                });
                if let Err(e) = cleared {
                    eprintln!("[editor] reopen: could not remove the restore marker: {e}");
                }
            }
            Op::Input {
                channel_dir,
                token,
                prefixes,
                skip,
            } => {
                let newest = shared.lock().newest.clone();
                if let Err(e) = write_input(&channel_dir, &token, &self.root, &prefixes, newest.as_ref(), skip) {
                    eprintln!("[editor] reopen: could not write the editor's input: {e}");
                }
                shared.answer(vec![WriterAnswer::InputWritten { token }]);
            }
        }
    }

    /// The write rule.
    fn take(&mut self, shared: &Shared, seq: u64, record: Record, same_as_restored: bool) {
        if self.floor.is_some_and(|floor| seq < floor) {
            return;
        }
        self.floor = Some(seq);
        if same_as_restored && !self.written_for_nvim {
            return;
        }
        if self.last_written.as_ref() == Some(&record) {
            return;
        }
        let mut fitted = record.clone();
        for dropped in fitted.fit_budget(RECORD_MAX) {
            eprintln!("[editor] reopen: the record is too large; left out {dropped}");
        }
        let written = self.locked(|file| {
            file.write(&fitted)?;
            if let Err(e) = file.clear_marker() {
                eprintln!("[editor] reopen: could not remove the restore marker: {e}");
            }
            Ok(())
        });
        match written {
            Ok(()) => {
                if self.last_failed.take().is_some() {
                    save_health::saved(SaveKind::Editor);
                }
                self.last_written = Some(record);
                self.written_for_nvim = true;
                self.session = None;
                shared.answer(vec![WriterAnswer::Written { seq }]);
            }
            Err(e) => {
                eprintln!(
                    "[editor] reopen: could not save the editor's open files to {}: {e}",
                    self.file.path().display()
                );
                if self.last_failed.as_ref() != Some(&record) {
                    save_health::failed(SaveKind::Editor, &e);
                    #[cfg(test)]
                    {
                        shared.lock().failures_reported += 1;
                    }
                }
                self.last_failed = Some(record);
            }
        }
    }

    /// Runs `change` holding the record's store lock.
    fn locked(&self, change: impl FnOnce(&RecordFile) -> io::Result<()>) -> io::Result<()> {
        let _lock = self.file.lock(ON_A_WRITER_THREAD)?;
        change(&self.file)
    }

    /// Reads the handoff at `path`, looking for it until `until`. Read without following a link, as a regular file
    /// of this user within [`RECORD_MAX`], and removed once read; anything else there is refused and removed too.
    fn read_handoff(&self, shared: &Shared, path: &Path, until: Instant) -> Option<(Record, bool)> {
        let text = loop {
            match agent::private_fs::read_private_nofollow_capped(path, RECORD_MAX as u64) {
                Ok(text) => break text,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() || shared.lock().off {
                        eprintln!(
                            "[editor] reopen: no handoff at {}; the last record stands",
                            path.display()
                        );
                        return None;
                    }
                    std::thread::sleep(left.min(HANDOFF_POLL));
                }
                Err(e) => {
                    eprintln!("[editor] reopen: the handoff at {} was refused: {e}", path.display());
                    let _ = std::fs::remove_file(path);
                    return None;
                }
            }
        };
        let _ = std::fs::remove_file(path);
        match parse_handoff(&text, &self.root) {
            Ok(read) => Some(read),
            Err(e) => {
                eprintln!(
                    "[editor] reopen: the handoff at {} could not be used: {e}",
                    path.display()
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
