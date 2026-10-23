//! `↑`/`↓` and `Ctrl+r` over this project's prompts, "per working
//! directory" as Claude Code keeps them (docs, #command-history). `$XDG_STATE_HOME/eitri/history/
//! <16 hex>.jsonl`, named like the layout file, written atomically, never inside the project. Two
//! windows of one project write the same file, so each write holds the store lock beside it
//! (`<file>.lock`) from its read to its rename, and a window writes it only from its
//! [`HistoryWriter`]'s thread. A file that cannot be read is set aside as `.jsonl.unusable` and read
//! as empty.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state_paths::{file_name, state_subdir, temporary};

pub const HISTORY_LIMIT: usize = 500;

#[derive(Serialize, Deserialize)]
struct Line {
    text: String,
    at: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    Entries(Vec<String>),
    Missing,
    Unusable(String),
}

pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "history")
}

pub fn path(dir: &Path, project_root: &Path) -> PathBuf {
    dir.join(file_name(project_root).replace(".json", ".jsonl"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_lines(file: &Path) -> Result<Option<Vec<Line>>, String> {
    let text = match agent::private_fs::read_private_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str::<Line>(l).map_err(|e| format!("{} line {}: {e}", file.display(), i + 1)))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub fn load(dir: &Path, project_root: &Path) -> Loaded {
    match read_lines(&path(dir, project_root)) {
        Ok(None) => Loaded::Missing,
        Ok(Some(lines)) => Loaded::Entries(lines.into_iter().map(|l| l.text).collect()),
        Err(why) => Loaded::Unusable(why),
    }
}

/// The rule every append follows, on the file and on a window's own copy alike: blank texts and a
/// repeat of the current last entry are dropped, and only the newest [`HISTORY_LIMIT`] are kept.
fn push_texts<T>(items: &mut Vec<T>, texts: &[String], text_of: impl Fn(&T) -> &str, make: impl Fn(&String) -> T) {
    for text in texts {
        if text.trim().is_empty() || items.last().is_some_and(|l| text_of(l) == text) {
            continue;
        }
        items.push(make(text));
    }
    let drop = items.len().saturating_sub(HISTORY_LIMIT);
    items.drain(..drop);
}

/// `entries` with `texts` appended by the rule the file follows, touching no disk: what a window shows
/// at once, before its [`HistoryWriter`] has written them and read back what other windows added.
pub fn appended(entries: &[String], texts: &[String]) -> Vec<String> {
    let mut entries = entries.to_vec();
    push_texts(&mut entries, texts, String::as_str, String::clone);
    entries
}

/// Under the history's store lock (`state_paths::lock_store`), re-reads the file, appends `texts`
/// (blank ones dropped, a repeat of the current last one dropped), keeps the newest
/// [`HISTORY_LIMIT`], and rewrites it atomically. Returns every entry, oldest first. An unreadable
/// file is set aside first and the append starts from empty. It may wait for another window's write
/// for as long as [`ON_A_WRITER_THREAD`](crate::state_paths::ON_A_WRITER_THREAD) allows, so a window
/// calls it only from its [`HistoryWriter`]'s thread.
pub fn append(dir: &Path, project_root: &Path, texts: &[String]) -> std::io::Result<Vec<String>> {
    append_with(dir, project_root, texts, crate::state_paths::ON_A_WRITER_THREAD)
}

fn append_with(
    dir: &Path,
    project_root: &Path,
    texts: &[String],
    tries: crate::state_paths::LockTries,
) -> std::io::Result<Vec<String>> {
    crate::state_paths::create_state_dir(dir)?;
    let file = path(dir, project_root);
    // Held until the rename below is done: another window's write between this read and that rename
    // would be replaced by a file made without it.
    let _lock = crate::state_paths::lock_store(&file, tries)?;
    let mut lines = match read_lines(&file) {
        Ok(lines) => lines.unwrap_or_default(),
        Err(why) => {
            eprintln!("[history] {why}; setting it aside");
            set_aside(dir, project_root)?;
            Vec::new()
        }
    };
    #[cfg(test)]
    crate::state_paths::test_hooks::after_read();
    let at = now_ms();
    push_texts(
        &mut lines,
        texts,
        |l| l.text.as_str(),
        |text| Line { text: text.clone(), at },
    );
    let body: String = lines
        .iter()
        .map(|l| serde_json::to_string(l).expect("a history line always serializes") + "\n")
        .collect();
    let tmp = temporary(&file);
    if let Err(err) =
        agent::private_fs::write_private(&tmp, body.as_bytes()).and_then(|()| std::fs::rename(&tmp, &file))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(lines.into_iter().map(|l| l.text).collect())
}

/// Renames the history file aside as `.jsonl.unusable`. The caller holds the store lock, or the file
/// it moves may be one another window has just written.
fn set_aside(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    let file = path(dir, project_root);
    let aside = file.with_extension("jsonl.unusable");
    std::fs::rename(&file, &aside)?;
    Ok(aside)
}

/// The entries a window starts with, and what to log. Never fails: no directory, a missing file
/// and an unusable one all read as an empty history. An unusable file is set aside only under the
/// store lock and only if it is still unusable when read again there; when another window holds the
/// lock it is left in place (this runs on the window's thread, which does not wait), and the next
/// write that still finds it unusable sets it aside.
pub fn startup(dir: Option<&Path>, project_root: &Path) -> (Vec<String>, Vec<String>) {
    let Some(dir) = dir else {
        return (Vec::new(), Vec::new());
    };
    let why = match load(dir, project_root) {
        Loaded::Entries(entries) => return (entries, Vec::new()),
        Loaded::Missing => return (Vec::new(), Vec::new()),
        Loaded::Unusable(why) => why,
    };
    let mut notes = vec![format!("[history] unusable ({why}); starting with none")];
    let file = path(dir, project_root);
    let _lock = match crate::state_paths::lock_store(&file, crate::state_paths::ON_THE_WINDOW_THREAD) {
        Ok(lock) => lock,
        Err(e) => {
            notes.push(format!("[history] left in place: {e}"));
            return (Vec::new(), notes);
        }
    };
    match load(dir, project_root) {
        // Another window replaced it between the two reads.
        Loaded::Entries(entries) => return (entries, Vec::new()),
        Loaded::Missing => return (Vec::new(), Vec::new()),
        Loaded::Unusable(_) => {}
    }
    match set_aside(dir, project_root) {
        Ok(aside) => notes.push(format!("[history] set aside as {}", aside.display())),
        Err(e) => notes.push(format!("[history] could not set it aside: {e}")),
    }
    (Vec::new(), notes)
}

/// [`append`] on a thread of its own, so the window's thread never reads, writes or waits for the
/// history file.
///
/// [`submit`](Self::submit) hands texts over and returns at once; the window shows them straight away
/// from its own copy ([`appended`]). The thread takes every batch waiting, appends them in one write
/// under the store lock, and reports the file's entries after it, other windows' lines included;
/// [`poll`](Self::poll) gives those only when no later batch is still out, so a report never takes back
/// a text the window already shows. Texts a write could not put in (the lock held by another window for
/// every try, or any other failure) are held by the thread and tried again by themselves after a pause
/// and with the next batch; while the lock is what keeps them out, the batch stays unreported, so
/// [`is_busy`](Self::is_busy) stays true and the window polls until they are written. Held texts live
/// only in this process until written, so a crash meanwhile loses them. [`finish`](Self::finish) at the
/// window's close hands over nothing more; the thread finishes the round under way, makes one more for
/// what it still holds, and ends ([`CLOSE_WAIT`] bounds both); what that last round cannot write is given
/// up and logged.
pub struct HistoryWriter {
    /// `None`: nothing is ever written (no state directory).
    dir: Option<PathBuf>,
    project_root: PathBuf,
    tries: crate::state_paths::LockTries,
    /// Started at the first batch.
    worker: Option<Worker>,
    /// Batches handed over so far; each is numbered by this count when it is handed over.
    handed: u64,
    /// The newest batch the thread has reported on.
    answered: u64,
    /// Rounds the thread found the lock held for every try, counted for a test that must wait for one.
    refused: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

struct Worker {
    /// Dropped at [`HistoryWriter::finish`] (or with the writer): the thread then writes what it holds
    /// and ends.
    batches: Option<std::sync::mpsc::Sender<(u64, Vec<String>)>>,
    reports: std::sync::mpsc::Receiver<Report>,
    done: Option<std::sync::mpsc::Receiver<()>>,
}

/// One write's outcome: the newest batch it covered and, when it was written, the file's entries.
struct Report {
    batch: u64,
    entries: Option<Vec<String>>,
}

impl HistoryWriter {
    pub fn new(dir: Option<PathBuf>, project_root: PathBuf) -> Self {
        HistoryWriter {
            dir,
            project_root,
            tries: crate::state_paths::ON_A_WRITER_THREAD,
            worker: None,
            handed: 0,
            answered: 0,
            refused: std::sync::Arc::default(),
        }
    }

    /// How many write rounds have found the lock held for every try so far.
    #[cfg(any(test, feature = "test-support"))]
    pub fn refused_rounds(&self) -> u64 {
        self.refused.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The writer with fewer tries at a held lock, for a test that holds it on purpose. Set before the
    /// first batch: the thread takes it when it starts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_tries(mut self, tries: crate::state_paths::LockTries) -> Self {
        self.tries = tries;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// Hands `texts` to the thread and returns at once; touches no disk. Nothing to write (no state
    /// directory, or only blank texts) starts nothing.
    pub fn submit(&mut self, texts: &[String]) {
        let Some(dir) = self.dir.clone() else { return };
        if texts.iter().all(|t| t.trim().is_empty()) {
            return;
        }
        let batch = self.handed + 1;
        let sent = self
            .worker_for(dir)
            .and_then(|worker| worker.batches.as_ref())
            .is_some_and(|batches| batches.send((batch, texts.to_vec())).is_ok());
        if sent {
            self.handed = batch;
        } else {
            eprintln!(
                "[history] the history's writer is gone; {} prompt(s) not kept",
                texts.len()
            );
        }
    }

    /// The file's entries once the thread has written the newest batch handed over, other windows'
    /// lines included; `None` while a later batch is still out, after a write that failed, or with
    /// nothing new.
    pub fn poll(&mut self) -> Option<Vec<String>> {
        let worker = self.worker.as_ref()?;
        let mut newest = None;
        loop {
            match worker.reports.try_recv() {
                Ok(report) => {
                    self.answered = self.answered.max(report.batch);
                    newest = report.entries.filter(|_| report.batch == self.handed);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if self.answered < self.handed {
                        eprintln!("[history] the history's writer ended without reporting");
                        self.answered = self.handed;
                    }
                    break;
                }
            }
        }
        newest
    }

    /// Whether a batch handed over has not been reported on yet -- not written yet, or kept out by a held
    /// lock: until it has, the window polls. A write that failed otherwise is reported (and retried by the
    /// thread), so a lasting failure does not keep the window polling.
    pub fn is_busy(&self) -> bool {
        self.answered < self.handed
    }

    /// No more batches: the thread writes what it still holds and ends. The receiver reports once it
    /// has (or is gone when the thread died); `None` when no thread ever started, so nothing is owed.
    pub fn finish(&mut self) -> Option<std::sync::mpsc::Receiver<()>> {
        let worker = self.worker.as_mut()?;
        worker.batches = None;
        worker.done.take()
    }

    /// Waits until the thread has reported on every batch handed over, for a test to read the file.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_answered(&mut self) -> Option<Vec<String>> {
        let mut newest = None;
        while self.is_busy() {
            let Some(worker) = self.worker.as_ref() else { break };
            match worker.reports.recv() {
                Ok(report) => {
                    self.answered = self.answered.max(report.batch);
                    newest = report.entries.filter(|_| report.batch == self.handed);
                }
                Err(_) => self.answered = self.handed,
            }
        }
        newest
    }

    fn worker_for(&mut self, dir: PathBuf) -> Option<&Worker> {
        if self.worker.is_none() {
            let (batches_tx, batches_rx) = std::sync::mpsc::channel();
            let (reports_tx, reports_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let root = self.project_root.clone();
            let tries = self.tries;
            let refused = self.refused.clone();
            let spawned = std::thread::Builder::new().name("eitri-history".into()).spawn(move || {
                write_loop(&dir, &root, tries, &batches_rx, &reports_tx, &refused);
                let _ = done_tx.send(());
            });
            if let Err(e) = spawned {
                eprintln!("[history] the history's writer could not start ({e}); prompts are not kept");
                self.dir = None;
                return None;
            }
            self.worker = Some(Worker {
                batches: Some(batches_tx),
                reports: reports_rx,
                done: Some(done_rx),
            });
        }
        self.worker.as_ref()
    }
}

/// How long the writer waits before trying again texts a held lock kept out. Another window holds it only
/// across one small write, so a round that found it held for about two seconds met a window that is
/// stopped or hung, and a quarter of a second between rounds is soon enough for when it lets go.
const BUSY_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

/// How long the writer first waits before trying again texts a write failed on for another reason (the
/// lock or the file could not be opened, the disk is full), doubled after each failure up to
/// [`FAILED_RETRY_CAP`]: such a failure may well last, and trying every moment would only fill the log.
const FAILED_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
const FAILED_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// Rounds of tries a close may wait for: the one under way when the window closes, and one more for what
/// the thread still holds then.
const CLOSE_ROUNDS: u64 = 2;

/// The longest a window's close may have to wait for its writer's last write: [`CLOSE_ROUNDS`] rounds of
/// [`ON_A_WRITER_THREAD`](crate::state_paths::ON_A_WRITER_THREAD)'s tries, and a second more for the
/// writes themselves (a small file renamed over the old one).
pub const CLOSE_WAIT: std::time::Duration = std::time::Duration::from_millis(
    CLOSE_ROUNDS
        * crate::state_paths::ON_A_WRITER_THREAD.attempts as u64
        * crate::state_paths::ON_A_WRITER_THREAD.pause.as_millis() as u64
        + 1000,
);

/// The writer thread. Every batch waiting goes into one append. Texts a write could not put in are held
/// and tried again by themselves after a pause ([`BUSY_RETRY`], or the growing [`FAILED_RETRY_FIRST`]),
/// and sooner with the next batch; nothing held is dropped before the close. Once the window has closed,
/// one more round is made for what is held, and only what that round cannot write is given up, and
/// logged.
fn write_loop(
    dir: &Path,
    root: &Path,
    tries: crate::state_paths::LockTries,
    batches: &std::sync::mpsc::Receiver<(u64, Vec<String>)>,
    reports: &std::sync::mpsc::Sender<Report>,
    refused: &std::sync::atomic::AtomicU64,
) {
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
    let mut held: Vec<String> = Vec::new();
    let mut batch = 0;
    // `None` while nothing is held: wait for the next batch however long it takes.
    let mut pause: Option<std::time::Duration> = None;
    let mut failed_pause = FAILED_RETRY_FIRST;
    // A run of failed rounds is logged once, at its first.
    let mut failing = false;
    loop {
        let waited = match pause {
            None => batches.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(pause) => batches.recv_timeout(pause),
        };
        let mut closing = false;
        match waited {
            Ok((n, texts)) => {
                batch = n;
                held.extend(texts);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => closing = true,
        }
        loop {
            match batches.try_recv() {
                Ok((n, texts)) => {
                    batch = n;
                    held.extend(texts);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closing = true;
                    break;
                }
            }
        }
        if held.is_empty() {
            if closing {
                return;
            }
            pause = None;
            continue;
        }
        let failure = match append_with(dir, root, &held, tries) {
            Ok(entries) => {
                held.clear();
                failing = false;
                failed_pause = FAILED_RETRY_FIRST;
                pause = None;
                // A writer that went away reads no more reports; what is still to come is written all the same.
                let _ = reports.send(Report {
                    batch,
                    entries: Some(entries),
                });
                None
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                refused.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // No report: the batch stays out, so the window keeps polling until these are written.
                pause = Some(BUSY_RETRY);
                Some(e)
            }
            Err(e) => {
                // Reported as failed, so the window does not poll through a failure that may last; the
                // texts stay held and are tried again here.
                let _ = reports.send(Report { batch, entries: None });
                pause = Some(failed_pause);
                failed_pause = (failed_pause * 2).min(FAILED_RETRY_CAP);
                Some(e)
            }
        };
        if let Some(e) = failure {
            if closing {
                eprintln!("[history] {} prompt(s) not kept: {e}", held.len());
                return;
            }
            if !failing {
                eprintln!("[history] {e}; {} prompt(s) held and tried again", held.len());
                failing = true;
            }
        }
        if closing {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn scratch(label: &str) -> ScratchDir {
        ScratchDir::new("nv-history", label)
    }
    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }
    fn texts(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn it_lives_beside_the_layout_and_agent_directories_as_jsonl() {
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), None),
            Some(PathBuf::from("/s/eitri/history"))
        );
        let name = path(Path::new("/s/eitri/history"), &root());
        let layout_name = crate::state_paths::file_name(&root());
        assert_eq!(
            name.file_name().unwrap().to_string_lossy(),
            layout_name.replace(".json", ".jsonl"),
            "keyed by the layout file's hash"
        );
    }

    #[test]
    fn appended_prompts_come_back_oldest_first_and_a_repeat_of_the_last_is_dropped() {
        let dir = scratch("append");
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        append(&dir, &root(), &texts(&["fix the parser"])).unwrap();
        append(&dir, &root(), &texts(&["run the tests", "run the tests"])).unwrap();
        let all = append(&dir, &root(), &texts(&["多行\n中文"])).unwrap();
        assert_eq!(all, texts(&["fix the parser", "run the tests", "多行\n中文"]));
        assert_eq!(load(&dir, &root()), Loaded::Entries(all));
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let file = path(&dir, &root()).file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(names, vec![file.clone(), format!("{file}.lock")], "no .tmp left behind");
    }

    #[test]
    fn it_keeps_the_newest_five_hundred() {
        let dir = scratch("ring");
        let many: Vec<String> = (0..HISTORY_LIMIT + 7).map(|i| format!("prompt {i}")).collect();
        let kept = append(&dir, &root(), &many).unwrap();
        assert_eq!(kept.len(), HISTORY_LIMIT);
        assert_eq!(kept.first().unwrap(), "prompt 7");
        assert_eq!(kept.last().unwrap(), &format!("prompt {}", HISTORY_LIMIT + 6));
    }

    /// Two windows of one project: each append re-reads the file first, so neither erases the other.
    #[test]
    fn an_append_keeps_what_another_window_wrote_meanwhile() {
        let dir = scratch("two-windows");
        append(&dir, &root(), &texts(&["from window A"])).unwrap();
        let all = append(&dir, &root(), &texts(&["from window B"])).unwrap();
        assert_eq!(all, texts(&["from window A", "from window B"]));
    }

    /// Holds `file`'s store lock through a descriptor of its own, as another window's process would.
    fn held_by_another_window(file: &Path) -> std::fs::File {
        use std::os::unix::io::AsRawFd;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(format!("{}.lock", file.display()))
            .unwrap();
        // SAFETY: the descriptor is `lock`'s own and open for the call.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        lock
    }

    /// Two windows of one project append at about the same time. Window A stops right after it read the
    /// file; window B runs until it first finds the lock held (or, with nothing to stop it, to the end);
    /// only then does A go on. B must not have written past A's write under way, or A's rewrite, made
    /// from what A read, drops B's line.
    #[test]
    fn an_append_racing_another_windows_keeps_both_lines() {
        use std::sync::mpsc;
        let dir = scratch("race");
        let (a_read_tx, a_read) = mpsc::channel();
        let (go_tx, go) = mpsc::channel::<()>();
        let a = {
            let dir = dir.to_path_buf();
            std::thread::spawn(move || {
                crate::state_paths::test_hooks::set_after_read(move || {
                    a_read_tx.send(()).unwrap();
                    go.recv().unwrap();
                });
                append(&dir, &root(), &texts(&["from window A"]))
            })
        };
        a_read.recv().unwrap();
        let (b_tx, b_seen) = mpsc::channel();
        let b = {
            let dir = dir.to_path_buf();
            std::thread::spawn(move || {
                let busy = b_tx.clone();
                crate::state_paths::test_hooks::set_on_busy(move || {
                    let _ = busy.send("found the lock held");
                });
                // Enough tries that only a hang runs out of them: B must wait for A however slowly a busy
                // machine runs A, and a refusal here would say nothing about the lock.
                let tries = crate::state_paths::LockTries {
                    attempts: 100_000,
                    pause: std::time::Duration::from_millis(1),
                };
                let appended = append_with(&dir, &root(), &texts(&["from window B"]), tries);
                let _ = b_tx.send("done");
                appended
            })
        };
        let first = b_seen.recv().unwrap();
        go_tx.send(()).unwrap();
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
        assert_eq!(
            load(&dir, &root()),
            Loaded::Entries(texts(&["from window A", "from window B"])),
            "window B, while A's write was under way: {first}"
        );
    }

    /// A file another window is writing is not set aside at a launch: the rename would move away what
    /// that window is about to put in place. It stays, and is set aside by the next write that still
    /// finds it unusable.
    #[test]
    fn a_launch_leaves_an_unusable_file_another_window_is_writing() {
        let dir = scratch("startup-busy");
        let file = path(&dir, &root());
        std::fs::write(&file, "not json\n").unwrap();
        let held = held_by_another_window(&file);
        let (entries, notes) = startup(Some(&dir), &root());
        assert!(entries.is_empty());
        assert!(file.exists(), "set aside while another window held it: {notes:?}");
        assert!(!notes.iter().any(|n| n.contains("set aside as")), "{notes:?}");
        drop(held);
        let all = append(&dir, &root(), &texts(&["after"])).unwrap();
        assert_eq!(all, texts(&["after"]));
    }

    /// A window's own copy follows the file's rule: blanks and a repeat of the last dropped, the newest
    /// five hundred kept.
    #[test]
    fn a_windows_own_copy_is_appended_by_the_files_rule() {
        let start = texts(&["one"]);
        assert_eq!(
            appended(&start, &texts(&["", "one", "two", "two", " "])),
            texts(&["one", "two"])
        );
        let many: Vec<String> = (0..HISTORY_LIMIT + 3).map(|i| format!("p{i}")).collect();
        let kept = appended(&start, &many);
        assert_eq!(kept.len(), HISTORY_LIMIT);
        assert_eq!(kept.first().unwrap(), "p3");
    }

    /// Two windows' writers, each on its own thread with its own descriptors, appending at once: every
    /// prompt of each is in the file, in its own order, and the writer that wrote last reports them all.
    #[test]
    fn two_windows_writers_appending_at_once_lose_nothing() {
        let dir = scratch("two-writers");
        let per = 60;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let run = |tag: &'static str| {
            let dir = dir.to_path_buf();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut writer = HistoryWriter::new(Some(dir), root());
                barrier.wait();
                for i in 0..per {
                    writer.submit(&[format!("{tag} {i}")]);
                }
                let last = writer.wait_answered();
                let done = writer.finish().expect("a writer that wrote has a thread");
                done.recv().unwrap();
                last
            })
        };
        let (a, b) = (run("a"), run("b"));
        let (a_last, b_last) = (a.join().unwrap(), b.join().unwrap());
        let Loaded::Entries(all) = load(&dir, &root()) else {
            panic!("usable")
        };
        for tag in ["a", "b"] {
            let mine: Vec<&String> = all.iter().filter(|e| e.starts_with(tag)).collect();
            let expected: Vec<String> = (0..per).map(|i| format!("{tag} {i}")).collect();
            assert_eq!(mine.into_iter().cloned().collect::<Vec<_>>(), expected, "{tag}");
        }
        assert_eq!(all.len(), 2 * per);
        // Whichever wrote last saw everything; the other at least everything of its own.
        let longest = a_last.iter().chain(b_last.iter()).map(Vec::len).max();
        assert_eq!(longest, Some(2 * per), "{a_last:?} {b_last:?}");
    }

    /// Tries enough for the moment after `drop(held)`: a child another test is starting holds a copy of the
    /// test's descriptor until it execs, so the lock may stay held a little past the drop. While the test
    /// holds it, every round fails whatever the count.
    fn tries_past_a_release() -> crate::state_paths::LockTries {
        crate::state_paths::LockTries {
            attempts: 50,
            pause: std::time::Duration::from_millis(1),
        }
    }

    /// Waits until the writer has had a round refused by the held lock, so the test knows its texts are
    /// held when it goes on.
    fn wait_refused(writer: &HistoryWriter) {
        while writer.refused_rounds() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Texts a held lock kept out are written with the next batch once it is free, in order: delayed,
    /// never dropped. Until then the window keeps the copy it showed, and the batch stays out.
    #[test]
    fn texts_a_held_lock_kept_out_are_written_with_the_next_batch() {
        let dir = scratch("held");
        let file = path(&dir, &root());
        let mut writer = HistoryWriter::new(Some(dir.to_path_buf()), root()).with_tries(tries_past_a_release());
        let held = held_by_another_window(&file);
        writer.submit(&texts(&["while held"]));
        wait_refused(&writer);
        assert!(writer.is_busy(), "the batch stays out while the lock keeps it out");
        assert_eq!(writer.poll(), None);
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        drop(held);
        writer.submit(&texts(&["after"]));
        assert_eq!(writer.wait_answered(), Some(texts(&["while held", "after"])));
        assert_eq!(load(&dir, &root()), Loaded::Entries(texts(&["while held", "after"])));
    }

    /// Texts held when the window closes are written by the close, once the lock is free.
    #[test]
    fn the_close_writes_what_a_held_lock_kept_out() {
        let dir = scratch("held-close");
        let file = path(&dir, &root());
        let mut writer = HistoryWriter::new(Some(dir.to_path_buf()), root()).with_tries(tries_past_a_release());
        let held = held_by_another_window(&file);
        writer.submit(&texts(&["queued at the close"]));
        wait_refused(&writer);
        drop(held);
        writer.finish().expect("a thread was started").recv().unwrap();
        assert_eq!(load(&dir, &root()), Loaded::Entries(texts(&["queued at the close"])));
    }

    /// A lock held through the close does not keep the thread: the close's one more round fails, the
    /// thread gives the texts up and ends, and nothing is written.
    #[test]
    fn a_lock_held_through_the_close_ends_the_writer_after_one_more_round() {
        let dir = scratch("held-through-close");
        let file = path(&dir, &root());
        let mut writer =
            HistoryWriter::new(Some(dir.to_path_buf()), root()).with_tries(crate::state_paths::LockTries {
                attempts: 3,
                pause: std::time::Duration::ZERO,
            });
        let held = held_by_another_window(&file);
        writer.submit(&texts(&["given up"]));
        wait_refused(&writer);
        writer.finish().expect("a thread was started").recv().unwrap();
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        drop(held);
    }

    /// The close's wait covers what the writer can take: the round under way and one more, each at most
    /// `ON_A_WRITER_THREAD`'s tries.
    #[test]
    fn the_close_wait_covers_two_rounds_of_tries() {
        let tries = crate::state_paths::ON_A_WRITER_THREAD;
        assert!(CLOSE_WAIT > tries.pause * (2 * tries.attempts));
    }

    /// A report on an earlier batch never replaces the window's copy while a later batch is out: the
    /// window would lose, for a moment, a prompt it already showed. The thread's side is played here by
    /// hand, so the order of reports is the test's.
    #[test]
    fn a_report_is_given_only_for_the_newest_batch() {
        let (reports_tx, reports) = std::sync::mpsc::channel();
        let mut writer = HistoryWriter::new(Some(PathBuf::from("/unused")), root());
        writer.worker = Some(Worker {
            batches: None,
            reports,
            done: None,
        });
        writer.handed = 2;
        reports_tx
            .send(Report {
                batch: 1,
                entries: Some(texts(&["first"])),
            })
            .unwrap();
        assert_eq!(writer.poll(), None, "batch 2 is still out");
        assert!(writer.is_busy());
        reports_tx
            .send(Report {
                batch: 2,
                entries: Some(texts(&["first", "second"])),
            })
            .unwrap();
        assert_eq!(writer.poll(), Some(texts(&["first", "second"])));
        assert!(!writer.is_busy());
        assert_eq!(writer.poll(), None, "nothing new");
        // A thread that ended without reporting leaves nothing to poll for.
        writer.handed = 3;
        drop(reports_tx);
        assert_eq!(writer.poll(), None);
        assert!(!writer.is_busy());
    }

    #[test]
    fn a_writer_with_no_state_directory_starts_nothing() {
        let mut off = HistoryWriter::new(None, root());
        off.submit(&texts(&["no state directory"]));
        assert!(!off.is_busy());
        assert!(off.poll().is_none());
        assert!(off.finish().is_none(), "no thread was started");
        let dir = scratch("blank-batch");
        let mut writer = HistoryWriter::new(Some(dir.to_path_buf()), root());
        writer.submit(&texts(&["", "  "]));
        assert!(writer.finish().is_none(), "only blanks: no thread was started");
    }

    /// A write that could not even open the lock (here a directory stands in its place) keeps its texts:
    /// once the lock can be opened again they are written, ahead of the next prompt.
    #[test]
    fn texts_a_failed_write_kept_are_written_once_it_can_be() {
        let dir = scratch("transient");
        let blocker = crate::state_paths::lock_path(&path(&dir, &root()));
        std::fs::create_dir(&blocker).unwrap();
        let mut writer = HistoryWriter::new(Some(dir.to_path_buf()), root());
        writer.submit(&texts(&["while it failed"]));
        assert_eq!(
            writer.wait_answered(),
            None,
            "nothing written while the lock cannot be opened"
        );
        std::fs::remove_dir(&blocker).unwrap();
        writer.submit(&texts(&["after"]));
        assert_eq!(writer.wait_answered(), Some(texts(&["while it failed", "after"])));
        assert_eq!(
            load(&dir, &root()),
            Loaded::Entries(texts(&["while it failed", "after"]))
        );
    }

    /// Texts a held lock kept out land in the file once it is released, with no further prompt: the
    /// writer tries again by itself, so they are neither left in memory, unseen by the other windows, nor
    /// lost with the process.
    #[test]
    fn texts_a_held_lock_kept_out_land_once_it_is_released_with_no_further_prompt() {
        let dir = scratch("held-retry");
        let file = path(&dir, &root());
        let mut writer =
            HistoryWriter::new(Some(dir.to_path_buf()), root()).with_tries(crate::state_paths::LockTries {
                attempts: 1,
                pause: std::time::Duration::ZERO,
            });
        let held = held_by_another_window(&file);
        writer.submit(&texts(&["while held"]));
        wait_refused(&writer);
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        drop(held);
        assert_eq!(writer.wait_answered(), Some(texts(&["while held"])));
        assert_eq!(load(&dir, &root()), Loaded::Entries(texts(&["while held"])));
    }

    #[test]
    fn empty_or_whitespace_prompts_are_not_history() {
        let dir = scratch("blank");
        let all = append(&dir, &root(), &texts(&["", "   \n", "real"])).unwrap();
        assert_eq!(all, texts(&["real"]));
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_read_as_empty() {
        let dir = scratch("unusable");
        std::fs::write(path(&dir, &root()), "{\"text\":\"ok\",\"at\":1}\nnot json\n").unwrap();
        assert!(matches!(load(&dir, &root()), Loaded::Unusable(_)));
        let (entries, notes) = startup(Some(&dir), &root());
        assert!(entries.is_empty());
        assert!(notes.iter().any(|n| n.contains("set aside")), "{notes:?}");
        assert!(dir
            .join(format!(
                "{}.unusable",
                path(&dir, &root()).file_name().unwrap().to_string_lossy()
            ))
            .exists());
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        assert_eq!(
            startup(None, &root()).0,
            Vec::<String>::new(),
            "no state directory: no history, no failure"
        );
    }
}
