//! Resolving the paths of live edits that go through `..`, off the GTK thread and one batch at a time.
//!
//! `..` after a link leads where the link leads, which only the disk says, and the disk may not
//! answer (a hung network file system). So at most one worker resolves at a time, batches wait in
//! arrival order behind it and come out in that order, and a worker that has not answered within
//! [`BUFFER_ANSWER_WAIT`] is given up on: its batch is read by its text, and so is every batch after
//! it until that worker answers, without starting another. The window's tick stops polling as soon
//! as nothing waits, a stuck worker included.

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use super::flow::{live_path, resolve_live_path, BUFFER_ANSWER_WAIT};
use crate::tab_set::LiveEdit;

/// How a worker resolves a path with `..` in it: `(project root, named path)` to the absolute path.
pub(crate) type Resolve = Arc<dyn Fn(&Path, &Path) -> Option<PathBuf> + Send + Sync>;

/// Whether `path` goes through `..`, so that only the disk can say what it names.
pub(crate) fn has_parent_dir(path: &Path) -> bool {
    path.components().any(|c| c == Component::ParentDir)
}

struct Batch {
    root: PathBuf,
    edits: Vec<LiveEdit>,
}

struct InFlight {
    batch: Batch,
    rx: mpsc::Receiver<Vec<LiveEdit>>,
    since: Instant,
}

pub(crate) struct LiveResolver {
    resolve: Resolve,
    waiting: VecDeque<Batch>,
    current: Option<InFlight>,
    /// A worker given up on, until it answers: no other is started meanwhile.
    abandoned: Option<mpsc::Receiver<Vec<LiveEdit>>>,
    gave_up: u32,
    logged: bool,
    started: usize,
}

impl Default for LiveResolver {
    fn default() -> LiveResolver {
        LiveResolver::with(Arc::new(resolve_live_path))
    }
}

impl LiveResolver {
    pub(crate) fn with(resolve: Resolve) -> LiveResolver {
        LiveResolver {
            resolve,
            waiting: VecDeque::new(),
            current: None,
            abandoned: None,
            gave_up: 0,
            logged: false,
            started: 0,
        }
    }

    /// Something waits to be resolved or handed out.
    pub(crate) fn busy(&self) -> bool {
        self.current.is_some() || !self.waiting.is_empty()
    }

    /// Queues one tick's edits, all of them, so they come out after every earlier batch.
    pub(crate) fn push(&mut self, root: PathBuf, edits: Vec<LiveEdit>) {
        if !edits.is_empty() {
            self.waiting.push_back(Batch { root, edits });
        }
    }

    /// The batches resolved by now, in the order they were pushed; starts the next worker when the
    /// last one has answered or been given up on. Never waits.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<LiveEdit> {
        let mut out = Vec::new();
        if let Some(rx) = &self.abandoned {
            match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {}
                // Its answer belongs to a batch already handed out by its text.
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => self.abandoned = None,
            }
        }
        loop {
            if let Some(flight) = &self.current {
                match flight.rx.try_recv() {
                    Ok(edits) => {
                        out.extend(edits);
                        self.current = None;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let flight = self.current.take().unwrap();
                        out.extend(by_text(flight.batch));
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        if now.saturating_duration_since(flight.since) < BUFFER_ANSWER_WAIT {
                            return out;
                        }
                        let flight = self.current.take().unwrap();
                        self.gave_up += 1;
                        if !self.logged {
                            self.logged = true;
                            eprintln!(
                                "eitri: resolving an edited file's path through `..` did not answer within {}s; \
                                 such paths are read by their text until it does",
                                BUFFER_ANSWER_WAIT.as_secs()
                            );
                        }
                        self.abandoned = Some(flight.rx);
                        out.extend(by_text(flight.batch));
                    }
                }
            }
            let Some(batch) = self.waiting.pop_front() else {
                return out;
            };
            if self.abandoned.is_some() || !batch.edits.iter().any(|edit| has_parent_dir(&edit.path)) {
                out.extend(by_text(batch));
                continue;
            }
            let (tx, rx) = mpsc::channel();
            let resolve = self.resolve.clone();
            let (root, edits) = (batch.root.clone(), batch.edits.clone());
            let spawned = std::thread::Builder::new().name("live-path".into()).spawn(move || {
                let resolved = edits
                    .into_iter()
                    .filter_map(|mut edit| {
                        edit.path = if has_parent_dir(&edit.path) {
                            resolve(&root, &edit.path)?
                        } else {
                            live_path(&root, &edit.path)?
                        };
                        Some(edit)
                    })
                    .collect();
                // The flow may be gone (the window closed); the answer has nowhere to go.
                let _ = tx.send(resolved);
            });
            match spawned {
                Ok(_) => {
                    self.started += 1;
                    self.current = Some(InFlight { batch, rx, since: now });
                    return out;
                }
                Err(_) => out.extend(by_text(batch)),
            }
        }
    }

    /// How many workers were given up on.
    #[cfg(test)]
    pub(crate) fn gave_up(&self) -> u32 {
        self.gave_up
    }

    /// How many workers were started.
    #[cfg(test)]
    pub(crate) fn started(&self) -> usize {
        self.started
    }
}

/// A batch read by its text alone, as a path without `..` always is.
fn by_text(batch: Batch) -> Vec<LiveEdit> {
    batch
        .edits
        .into_iter()
        .filter_map(|mut edit| {
            edit.path = live_path(&batch.root, &edit.path)?;
            Some(edit)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::TabId;
    use std::sync::Mutex;
    use std::time::Duration;

    fn edit(path: &str) -> LiveEdit {
        LiveEdit {
            tab: TabId(1),
            session: "s".into(),
            turn_id: "t".into(),
            path: PathBuf::from(path),
        }
    }

    fn paths(edits: &[LiveEdit]) -> Vec<String> {
        edits.iter().map(|e| e.path.to_string_lossy().into_owned()).collect()
    }

    /// A resolver that blocks until the returned sender is dropped, then names `/w/project/<leaf>`.
    fn blocked() -> (Resolve, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let resolve: Resolve = Arc::new(move |root: &Path, named: &Path| {
            let _ = rx.lock().unwrap().recv();
            Some(root.join(named.file_name()?))
        });
        (resolve, tx)
    }

    /// Polls until `want` edits came out (or 30 s passed, for a loaded machine), at the fixed `now`.
    fn drain(r: &mut LiveResolver, now: Instant, want: usize) -> Vec<LiveEdit> {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while out.len() < want && Instant::now() < deadline {
            out.extend(r.poll(now));
            std::thread::sleep(Duration::from_millis(2));
        }
        out
    }

    #[test]
    fn a_blocked_resolver_is_one_thread_and_the_tick_stops_after_the_wait() {
        let (resolve, release) = blocked();
        let mut r = LiveResolver::with(resolve);
        let root = PathBuf::from("/w/project");
        let start = Instant::now();
        for n in 0..50 {
            r.push(root.clone(), vec![edit(&format!("x/../f{n}.txt"))]);
            let now = start + Duration::from_millis(50 * n);
            assert!(r.poll(now).is_empty(), "nothing comes out before the wait");
        }
        assert_eq!(r.started(), 1, "one worker however many ticks pass");
        assert!(r.busy());
        let late = start + BUFFER_ANSWER_WAIT + Duration::from_secs(1);
        let out = r.poll(late);
        assert_eq!(r.gave_up(), 1);
        assert_eq!(out.len(), 50, "every batch read by its text, in order");
        assert_eq!(paths(&out[..2]), ["/w/project/f0.txt", "/w/project/f1.txt"]);
        assert!(!r.busy(), "nothing left: the tick stops polling");
        // Later batches go by their text too while the given-up worker has not answered.
        r.push(root.clone(), vec![edit("y/../g.txt")]);
        assert_eq!(paths(&r.poll(late)), ["/w/project/g.txt"]);
        assert_eq!(r.started(), 1, "no second worker while the first is stuck");
        drop(release);
    }

    #[test]
    fn batches_come_out_in_the_order_they_were_pushed() {
        // The first batch's worker is slow, the second's would be fast: the second waits its turn.
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let gate = std::sync::Mutex::new(gate);
        let resolve: Resolve = Arc::new(move |root: &Path, named: &Path| {
            if named.to_string_lossy().contains("slow") {
                entered.send(()).unwrap();
                gate.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60))
                    .expect("release the first batch");
            }
            Some(root.join(named.file_name()?))
        });
        let mut r = LiveResolver::with(resolve);
        let root = PathBuf::from("/w/project");
        let now = Instant::now();
        r.push(root.clone(), vec![edit("slow/../a.txt")]);
        assert!(r.poll(now).is_empty());
        r.push(root.clone(), vec![edit("b.txt")]);
        r.push(root.clone(), vec![edit("fast/../c.txt"), edit("d.txt")]);
        ready
            .recv_timeout(Duration::from_secs(60))
            .expect("first resolver entered");
        for _ in 0..20 {
            assert!(r.poll(now).is_empty(), "later batches cannot pass the held first batch");
        }
        release.send(()).unwrap();
        let out = drain(&mut r, now, 4);
        assert_eq!(
            paths(&out),
            [
                "/w/project/a.txt",
                "/w/project/b.txt",
                "/w/project/c.txt",
                "/w/project/d.txt"
            ]
        );
        assert_eq!(r.started(), 2, "a batch with no `..` needs no worker");
        assert!(!r.busy());
    }
}
