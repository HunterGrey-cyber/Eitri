//! A reader that falls behind owes one fresh state, requested only when it reads again.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::{Arc, Mutex, PoisonError};

use super::{Command, Door, StreamItem};
use crate::pty::set_nonblocking;

const ITEM_OVERHEAD: usize = std::mem::size_of::<StreamItem>();

pub(super) struct Queue {
    items: VecDeque<StreamItem>,
    bytes: usize,
    budget: usize,
    base_budget: usize,
    pub(super) owed: bool,
    pub(super) demand: bool,
    reader: Arc<std::io::PipeReader>,
    writer: std::io::PipeWriter,
    signaled: bool,
    pub(super) resyncs: u64,
    pub(super) high_water: usize,
    ended: bool,
}

impl Queue {
    pub(super) fn new(budget: usize) -> io::Result<Self> {
        let (reader, writer) = std::io::pipe()?;
        set_nonblocking(reader.as_fd())?;
        set_nonblocking(writer.as_fd())?;
        Ok(Self {
            items: VecDeque::new(),
            bytes: 0,
            budget,
            base_budget: budget,
            owed: true,
            demand: true,
            reader: Arc::new(reader),
            writer,
            signaled: false,
            resyncs: 0,
            high_water: 0,
            ended: false,
        })
    }

    fn signal(&mut self) {
        if !self.signaled {
            let _ = self.writer.write(&[1]);
            self.signaled = true;
        }
    }

    fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
    }

    pub(super) fn push(&mut self, item: StreamItem) {
        if self.ended {
            return;
        }
        if matches!(item, StreamItem::Detached(_)) {
            self.clear();
            self.owed = false;
            self.demand = false;
            self.ended = true;
        } else if self.owed
            && !matches!(
                item,
                StreamItem::InputWritten(_) | StreamItem::Waiting(_) | StreamItem::Exited(_)
            )
        {
            return;
        }
        if let StreamItem::InputWritten(_) = item {
            if matches!(self.items.back(), Some(StreamItem::InputWritten(_))) {
                self.items.pop_back();
                self.bytes -= ITEM_OVERHEAD;
            }
        }
        if let StreamItem::Waiting(why) = item {
            if self
                .items
                .iter()
                .any(|i| matches!(i, StreamItem::Waiting(old) if *old == why))
            {
                return;
            }
        }
        if let StreamItem::Exited(exit) = &item {
            if self
                .items
                .iter()
                .any(|item| matches!(item, StreamItem::Exited(old) if old == exit))
            {
                return;
            }
        }
        let cost = cost(&item);
        // While waiting, the client needs write acknowledgements to send the input that may finish the sequence.
        if matches!(item, StreamItem::Exited(_) | StreamItem::Waiting(_))
            || (self.owed && matches!(item, StreamItem::InputWritten(_)))
        {
            self.budget = self.budget.max(self.bytes.saturating_add(cost));
        }
        if self.bytes.saturating_add(cost) > self.budget && !self.ended {
            self.clear();
            self.owed = true;
            self.demand = false;
            self.resyncs += 1;
        } else {
            self.bytes += cost;
            self.items.push_back(item);
            self.high_water = self.high_water.max(self.bytes);
        }
        self.signal();
    }

    pub(super) fn install(&mut self, snapshot: crate::Snapshot, exit: Option<crate::ExitInfo>) {
        self.clear();
        self.budget = self.base_budget.max(snapshot.replay.bytes.len().saturating_mul(2));
        self.owed = false;
        self.demand = false;
        self.push(StreamItem::Snapshot(snapshot));
        if let Some(exit) = exit {
            self.push(StreamItem::Exited(exit));
        }
    }

    pub(super) fn failed(&mut self, exit: crate::ExitInfo) {
        self.clear();
        self.owed = false;
        self.demand = false;
        self.budget = self.budget.max(ITEM_OVERHEAD);
        self.push(StreamItem::Exited(exit));
        self.ended = true;
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }
    pub(super) fn budget(&self) -> usize {
        self.budget
    }
}

fn cost(item: &StreamItem) -> usize {
    ITEM_OVERHEAD
        + match item {
            StreamItem::Snapshot(s) => s.replay.bytes.len(),
            StreamItem::Output { bytes, .. } => bytes.len(),
            _ => 0,
        }
}

/// One client's bounded output. Dropping it ends that attachment and discards unwritten input.
/// Readiness is level-triggered: call `next` until it returns `None` before waiting again.
pub struct Attachment {
    pub(super) queue: Arc<Mutex<Queue>>,
    pub(super) door: Arc<Door>,
    pub(super) generation: u64,
    // A duplicate stays valid without borrowing a mutex guard for the poll call.
    pub(super) ready: Arc<std::io::PipeReader>,
}

impl Attachment {
    pub fn ready(&self) -> BorrowedFd<'_> {
        self.ready.as_fd()
    }

    pub fn next(&self) -> Option<StreamItem> {
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let item = q.items.pop_front();
        if let Some(ref item) = item {
            q.bytes -= cost(item);
        }
        let request = q.owed && !q.demand;
        if request {
            q.demand = true;
        }
        if q.items.is_empty() {
            let mut buf = [0u8; 64];
            while matches!((&*q.reader).read(&mut buf), Ok(n) if n > 0) {}
            q.signaled = false;
        }
        drop(q);
        if request {
            self.door.send(Command::Wake);
        }
        item
    }

    pub fn queued_bytes(&self) -> usize {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).bytes()
    }
    pub fn budget(&self) -> usize {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).budget()
    }
    pub fn resyncs(&self) -> u64 {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).resyncs
    }
    pub fn high_water(&self) -> usize {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).high_water
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.door.send(Command::Dropped(self.generation));
    }
}

pub(super) fn pair(budget: usize, door: Arc<Door>, generation: u64) -> io::Result<Attachment> {
    pair_with(budget, door, generation, Queue::new)
}

fn pair_with(
    budget: usize,
    door: Arc<Door>,
    generation: u64,
    make: impl FnOnce(usize) -> io::Result<Queue>,
) -> io::Result<Attachment> {
    let q = make(budget)?;
    let ready = q.reader.clone();
    Ok(Attachment {
        queue: Arc::new(Mutex::new(q)),
        door,
        generation,
        ready,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExitInfo, PtySize, Screen, Snapshot, Tail, TerminalColors};

    #[test]
    fn overflow_owes_one_lazy_snapshot_and_bounds_control_items_too() {
        let mut screen = Screen::headless(
            PtySize {
                cols: 80,
                rows: 24,
                cell_width_px: 8,
                cell_height_px: 16,
            },
            TerminalColors::default(),
        );
        let snapshot = Snapshot {
            offset: 0,
            replay: screen.read_for_move().unwrap(),
            tail: Tail::ReadAhead(vec![]),
            history_at_cap: false,
            history_trimmed: 0,
            output_while_detached: false,
            input_written: 0,
        };
        let mut q = Queue::new(4096).unwrap();
        q.install(snapshot, None);
        assert!(!q.owed);
        for offset in 0..10_000 {
            q.push(StreamItem::Resized {
                offset,
                size: screen.size(),
            });
            assert!(q.bytes <= q.budget);
        }
        assert!(q.owed && !q.demand);
        assert_eq!(q.resyncs, 1);
        assert_eq!(q.bytes, 0);
        assert!(q.items.is_empty());
    }

    #[test]
    fn failure_is_delivered_even_before_a_snapshot_with_a_zero_budget() {
        let mut q = Queue::new(0).unwrap();
        q.failed(ExitInfo::UNKNOWN);
        assert!(!q.owed);
        assert!(matches!(
            q.items.pop_front(),
            Some(StreamItem::Exited(ExitInfo::UNKNOWN))
        ));
    }

    #[test]
    fn written_counts_advance_during_a_snapshot_wait() {
        let mut q = Queue::new(0).unwrap();
        q.push(StreamItem::InputWritten(10));
        q.push(StreamItem::Waiting(super::super::WaitWhy::OpenSequence));
        q.push(StreamItem::InputWritten(20));
        q.push(StreamItem::InputWritten(30));
        assert!(q.owed);
        assert!(matches!(q.items.pop_front(), Some(StreamItem::InputWritten(10))));
        assert!(matches!(q.items.pop_front(), Some(StreamItem::Waiting(_))));
        assert!(matches!(q.items.pop_front(), Some(StreamItem::InputWritten(30))));
        assert!(q.items.is_empty());
    }

    #[test]
    fn an_exit_passes_an_owed_snapshot_and_a_full_budget() {
        let mut q = Queue::new(0).unwrap();
        q.push(StreamItem::Waiting(super::super::WaitWhy::OpenSequence));
        q.push(StreamItem::Exited(ExitInfo::UNKNOWN));
        assert!(matches!(q.items.pop_front(), Some(StreamItem::Waiting(_))));
        assert!(matches!(
            q.items.pop_front(),
            Some(StreamItem::Exited(ExitInfo::UNKNOWN))
        ));
    }

    #[test]
    fn a_failed_readiness_pipe_returns_the_allocation_error() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (_bell_reader, bell) = std::io::pipe().unwrap();
        let door = Arc::new(Door {
            tx,
            bell,
            closed: Mutex::new(false),
        });
        let a = pair_with(0, door, 0, |_| Err(io::Error::from_raw_os_error(libc::EMFILE)));
        assert_eq!(a.err().unwrap().raw_os_error(), Some(libc::EMFILE));
    }

    #[test]
    fn a_poisoned_queue_can_still_deliver_its_final_item() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (_bell_reader, bell) = std::io::pipe().unwrap();
        let door = Arc::new(Door {
            tx,
            bell,
            closed: Mutex::new(false),
        });
        let a = pair(4096, door, 0).unwrap();
        let queue = a.queue.clone();
        assert!(std::thread::spawn(move || {
            let _guard = queue.lock().unwrap_or_else(PoisonError::into_inner);
            panic!("poisoned attachment");
        })
        .join()
        .is_err());
        a.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failed(ExitInfo::UNKNOWN);
        assert!(matches!(a.next(), Some(StreamItem::Exited(ExitInfo::UNKNOWN))));
        assert_eq!(a.queued_bytes(), 0);
    }
    #[test]
    fn a_panic_on_the_thread_hangs_up_and_reports_unknown_exit() {
        let config = super::super::tests::config("exec sleep 60");
        let term = super::super::HeadlessTerminal::spawn(config).unwrap();
        let a = term.attach("client".into()).unwrap();
        let pid = term.status().pid;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(a.next(), Some(StreamItem::Snapshot(_))) {
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        term.door.send(super::super::Command::Panic);
        let result = term.thread.as_ref().unwrap();
        while !result.is_finished() {
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(matches!(a.next(), Some(StreamItem::Exited(ExitInfo::UNKNOWN))));
        assert_eq!(term.close(std::time::Duration::ZERO), Some(ExitInfo::UNKNOWN));
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }
}
