//! Readiness watches and scheduling on the linked windows' host loop.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// The descriptor event that wakes a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    Readable,
}

/// The host's normal dispatch priority for link traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Default,
}

/// `Stop` tells the host to remove the watch or timer after the callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// The caller is off the loop's thread, so no watch was armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotOnLoop;

/// An armed watch. Dropping its token alone leaves removal to the host.
pub trait Armed {
    /// Removes the watch before its descriptor is closed here.
    fn disarm(self: Box<Self>);
}

/// Arms on the loop's thread and dispatches readiness there after `arm` returns.
pub trait LinkWatch {
    fn arm(
        &self,
        fd: RawFd,
        interest: Interest,
        priority: Priority,
        on_ready: Box<dyn FnMut() -> Flow>,
    ) -> Result<Box<dyn Armed>, NotOnLoop>;
}

/// An opaque hold on the running application. Dropping the guard releases it.
pub trait HoldGuard {}

/// The pair's event loop, including the clock used for its deadlines.
pub trait PairClock: LinkWatch {
    /// Reads the time used to set deadlines and check them in callbacks.
    fn now(&self) -> Instant;
    /// Schedules `run` once on the loop's thread after `within`.
    fn after(&self, within: Duration, run: Box<dyn FnOnce()>);
    /// Schedules `tick` on the loop's thread every `period` until it returns `Flow::Stop`.
    fn every(&self, period: Duration, tick: Box<dyn FnMut() -> Flow>);
    /// Queues `job` on a worker and dispatches `done` on the loop's thread when it finishes.
    fn blocking(&self, job: Box<dyn FnOnce() + Send>, done: Box<dyn FnOnce()>);
    /// Keeps the application running after window 1's last window closes, so its thread or timer can finish
    /// an ending still owed to another window. Returns `None` when no application runs.
    fn hold(&self) -> Option<Box<dyn HoldGuard>>;
}

#[cfg(any(test, feature = "test-support"))]
pub mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    type Ready = Box<dyn FnMut() -> Flow>;

    /// Records registrations and disarms without dispatching callbacks on its own.
    #[derive(Default)]
    pub(crate) struct RecordingWatch {
        pub arms: RefCell<Vec<(RawFd, Interest, Priority)>>,
        pub callbacks: RefCell<Vec<Ready>>,
        pub disarms: Rc<RefCell<Vec<RawFd>>>,
    }

    impl LinkWatch for RecordingWatch {
        fn arm(
            &self,
            fd: RawFd,
            interest: Interest,
            priority: Priority,
            on_ready: Ready,
        ) -> Result<Box<dyn Armed>, NotOnLoop> {
            self.arms.borrow_mut().push((fd, interest, priority));
            self.callbacks.borrow_mut().push(on_ready);
            Ok(Box::new(RecordingArmed {
                fd,
                disarms: self.disarms.clone(),
            }))
        }
    }

    struct RecordingArmed {
        fd: RawFd,
        disarms: Rc<RefCell<Vec<RawFd>>>,
    }

    impl Armed for RecordingArmed {
        fn disarm(self: Box<Self>) {
            self.disarms.borrow_mut().push(self.fd);
        }
    }

    enum Timer {
        Once(Box<dyn FnOnce()>),
        Repeat(Duration, Ready),
    }

    type Blocking = (Box<dyn FnOnce() + Send>, Box<dyn FnOnce()>);

    struct ManualHold(Rc<Cell<usize>>);

    impl HoldGuard for ManualHold {}

    impl Drop for ManualHold {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Dispatches only when the test advances time or runs a queued job.
    pub(crate) struct ManualClock {
        now: Cell<Instant>,
        timers: RefCell<Vec<(Instant, Timer)>>,
        jobs: RefCell<Vec<Blocking>>,
        holds_taken: Cell<usize>,
        holds_released: Rc<Cell<usize>>,
        pub(crate) after_delays: RefCell<Vec<Duration>>,
        pub(crate) every_delays: RefCell<Vec<Duration>>,
        pub watches: RecordingWatch,
    }

    impl Default for ManualClock {
        fn default() -> Self {
            Self {
                now: Cell::new(Instant::now()),
                timers: RefCell::default(),
                jobs: RefCell::default(),
                holds_taken: Cell::default(),
                holds_released: Rc::default(),
                after_delays: RefCell::default(),
                every_delays: RefCell::default(),
                watches: RecordingWatch::default(),
            }
        }
    }

    impl ManualClock {
        pub(crate) fn holds_taken(&self) -> usize {
            self.holds_taken.get()
        }

        pub(crate) fn holds_released(&self) -> usize {
            self.holds_released.get()
        }

        pub fn advance(&self, duration: Duration) {
            let until = self.now() + duration;
            loop {
                let next = {
                    let timers = self.timers.borrow();
                    timers
                        .iter()
                        .enumerate()
                        .filter(|(_, (due, _))| *due <= until)
                        .min_by_key(|(_, (due, _))| *due)
                        .map(|(index, _)| index)
                };
                let Some(next) = next else { break };
                let (due, timer) = self.timers.borrow_mut().remove(next);
                self.now.set(due);
                match timer {
                    Timer::Once(run) => run(),
                    Timer::Repeat(period, mut tick) => {
                        if tick() == Flow::Continue {
                            self.timers
                                .borrow_mut()
                                .push((due + period, Timer::Repeat(period, tick)));
                        }
                    }
                }
            }
            self.now.set(until);
        }

        pub(crate) fn pending_timers(&self) -> usize {
            self.timers.borrow().len()
        }

        /// Drops the callbacks and queued jobs that can keep the clock or pair alive after the fixture ends.
        pub(crate) fn clear_timers(&self) {
            drop(self.timers.take());
            drop(self.jobs.take());
        }

        pub(crate) fn pending_jobs(&self) -> usize {
            self.jobs.borrow().len()
        }

        pub(crate) fn run_blocking(&self) {
            let jobs = self.jobs.take();
            for (job, done) in jobs {
                job();
                done();
            }
        }
    }

    impl LinkWatch for ManualClock {
        fn arm(
            &self,
            fd: RawFd,
            interest: Interest,
            priority: Priority,
            on_ready: Ready,
        ) -> Result<Box<dyn Armed>, NotOnLoop> {
            self.watches.arm(fd, interest, priority, on_ready)
        }
    }

    impl PairClock for ManualClock {
        fn now(&self) -> Instant {
            self.now.get()
        }

        fn after(&self, within: Duration, run: Box<dyn FnOnce()>) {
            self.after_delays.borrow_mut().push(within);
            self.timers.borrow_mut().push((self.now() + within, Timer::Once(run)));
        }

        fn every(&self, period: Duration, tick: Ready) {
            self.every_delays.borrow_mut().push(period);
            self.timers
                .borrow_mut()
                .push((self.now() + period, Timer::Repeat(period, tick)));
        }

        fn blocking(&self, job: Box<dyn FnOnce() + Send>, done: Box<dyn FnOnce()>) {
            self.jobs.borrow_mut().push((job, done));
        }

        fn hold(&self) -> Option<Box<dyn HoldGuard>> {
            self.holds_taken.set(self.holds_taken.get() + 1);
            Some(Box::new(ManualHold(self.holds_released.clone())))
        }
    }
}
