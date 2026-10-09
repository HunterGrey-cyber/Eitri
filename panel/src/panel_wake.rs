//! How the panel's tick is run only when it has something to do.
//!
//! The tick used to run every [`TICK`](crate::agent_panel::AgentPanelHandle::TICK) for the window's whole
//! life, about thirty wake-ups a second for a window nobody touches. Now each thing the tick acts on either
//! says when it is ready or is polled while it is pending:
//!
//! - **told**: an agent backend folds events on its own thread and raises [`PanelWake`]; every call the host
//!   makes into the panel and every page message raise it too, so whatever they changed is sent by the
//!   tick that follows;
//! - **due**: the typing cadence knows when its next slot or the end of the typing window comes;
//! - **polled**: a connect, a teardown, the trust worker, review and scratch jobs and the like report through
//!   channels nobody is told about, so while any is pending the tick runs on its old cadence, and once more
//!   after the last one ended.
//!
//! [`PanelWake`] is one end of a socket pair the host watches with whatever its loop has (a GLib fd watch, a
//! dispatch source). A raise writes one byte unless one is already waiting, so a stream of folds costs one
//! write until the host has taken it. [`TickSchedule`] is the one-shot timer bookkeeping both hosts share:
//! a wake never runs the tick sooner than one `TICK` after the last one, which keeps a streaming turn's
//! batches as they were.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

struct Raise {
    /// A byte is waiting in the pair, or is about to be: nothing more needs writing.
    raised: AtomicBool,
    write: UnixStream,
}

impl Raise {
    fn raise(&self) {
        if !self.raised.swap(true, Ordering::SeqCst) {
            // A full buffer already holds a byte the host has not read; nothing else can fail here that a
            // retry would mend.
            let _ = (&self.write).write(&[1]);
        }
    }
}

/// The panel's wake: raised from any thread, watched by the host on its own.
pub struct PanelWake {
    raise: Arc<Raise>,
    read: UnixStream,
}

impl PanelWake {
    /// A wake, or the reason the pair could not be made (the panel then polls as it always did).
    pub fn new() -> std::io::Result<PanelWake> {
        let (read, write) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok(PanelWake {
            raise: Arc::new(Raise {
                raised: AtomicBool::new(false),
                write,
            }),
            read,
        })
    }

    /// Raises the wake.
    pub fn raise(&self) {
        self.raise.raise();
    }

    /// The raise as a closure another thread can hold (an agent backend's ingestion thread).
    pub fn waker(&self) -> agent::UiWake {
        let raise = self.raise.clone();
        Arc::new(move || raise.raise())
    }

    /// The descriptor the host watches for reading.
    pub fn fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    /// Takes what was raised: reads every byte waiting, then lowers the flag. In that order, so a raise
    /// that lands in between finds the flag still up and writes nothing, and the tick the caller runs
    /// next, being after it, sees what it raised for; a raise after the flag is lowered writes again.
    /// Whether anything was raised.
    pub fn take(&self) -> bool {
        self.drain();
        self.raise.raised.swap(false, Ordering::SeqCst)
    }

    /// Reads every byte waiting; how many there were.
    fn drain(&self) -> usize {
        let mut buf = [0u8; 64];
        let mut total = 0;
        loop {
            match (&self.read).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        total
    }
}

/// When the host runs the tick next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextTick {
    /// Never again: the window closed or there is no page.
    Stop,
    /// At this moment, unless a wake asks for an earlier one.
    At(Instant),
    /// Only when the wake is raised.
    OnWake,
}

/// The one-shot timer a host keeps for the tick, as plain bookkeeping: the host arms or cancels its own
/// timer as these calls say. One timer at a time, armed for the earliest moment anything asked for.
#[derive(Debug, Default)]
pub struct TickSchedule {
    armed: Option<Instant>,
}

impl TickSchedule {
    pub fn new() -> TickSchedule {
        TickSchedule::default()
    }

    /// The wake was taken and the tick is due at `due`. `Some(at)`: (re)arm the timer for `at`, cancelling
    /// one armed later; `None`: the timer already armed comes first, keep it.
    pub fn on_wake(&mut self, due: Instant) -> Option<Instant> {
        if self.armed.is_some_and(|armed| armed <= due) {
            return None;
        }
        self.armed = Some(due);
        Some(due)
    }

    /// The timer fired; the host is about to run the tick.
    pub fn fired(&mut self) {
        self.armed = None;
    }

    /// What the tick said comes next. `Some(at)`: arm the timer for `at`; `None`: arm nothing (wait for the
    /// wake, or for nothing at all after [`NextTick::Stop`]).
    pub fn after_tick(&mut self, next: NextTick) -> Option<Instant> {
        match next {
            NextTick::At(at) => self.on_wake(at),
            NextTick::OnWake | NextTick::Stop => None,
        }
    }

    /// The moment the timer is armed for, if it is.
    pub fn armed(&self) -> Option<Instant> {
        self.armed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_raise_is_one_byte_until_it_is_taken() {
        let wake = PanelWake::new().unwrap();
        assert!(!wake.take(), "nothing raised");
        let waker = wake.waker();
        for _ in 0..1000 {
            waker();
        }
        assert_eq!(wake.drain(), 1, "a thousand raises wrote one byte");
        assert!(wake.take(), "the flag is still up until taken");
        assert!(!wake.take(), "taken once");
        wake.raise();
        assert!(wake.take(), "a raise after the take writes again");
        assert_eq!(wake.drain(), 0);
    }

    #[test]
    fn a_raise_from_another_thread_is_seen() {
        let wake = PanelWake::new().unwrap();
        let waker = wake.waker();
        std::thread::spawn(move || waker()).join().unwrap();
        assert!(wake.take());
    }

    #[test]
    fn a_wake_never_moves_an_earlier_timer_later() {
        let now = Instant::now();
        let mut schedule = TickSchedule::new();
        assert_eq!(
            schedule.on_wake(now + Duration::from_millis(33)),
            Some(now + Duration::from_millis(33))
        );
        assert_eq!(
            schedule.on_wake(now + Duration::from_millis(50)),
            None,
            "the armed one comes first"
        );
        assert_eq!(
            schedule.on_wake(now + Duration::from_millis(10)),
            Some(now + Duration::from_millis(10)),
            "an earlier one replaces it"
        );
        schedule.fired();
        assert_eq!(schedule.armed(), None);
        assert_eq!(schedule.after_tick(NextTick::OnWake), None);
        assert_eq!(schedule.after_tick(NextTick::Stop), None);
        assert_eq!(schedule.after_tick(NextTick::At(now)), Some(now));
    }
}
