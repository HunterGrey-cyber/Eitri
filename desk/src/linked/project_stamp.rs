//! A project's mark on its pair lock: the window that took the lock writes the project's root into the file, and every
//! window holding a copy of the lock sets the file's mtime when it gains activation, so the mtime is the project's last
//! use and a raise counts as one. Both happen on one thread per window, so a slow state directory cannot hold the GTK
//! thread, in order and with no rate limit: a person's focus changes are few, and a dropped one would put the wrong
//! project first. The thread sleeps on its queue, so an idle window does nothing. A close waits for the queue, briefly:
//! the end of the process would drop what is left in it.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime};

use eitri_core::window_link::pair::PairLock;

/// One window's stamping thread, which ends with this and owns a copy of the lock's descriptor until it does.
pub struct ProjectStamp {
    activations: Sender<SystemTime>,
    /// Never sent on: the thread's end closes it.
    ended: Receiver<()>,
}

impl ProjectStamp {
    /// `root` is `Some` only for the window that took the lock: only the holder names the project, and a window that
    /// adopted a copy finds it named. `None`, with a line in the log, when the thread cannot be had.
    pub fn start(lock: &PairLock, root: Option<PathBuf>) -> Option<ProjectStamp> {
        let fd = lock
            .share()
            .inspect_err(|err| eprintln!("[project] the lock could not be shared for its stamp: {err}"))
            .ok()?;
        let (activations, queue) = mpsc::channel();
        let (ending, ended) = mpsc::channel();
        let started = SystemTime::now();
        std::thread::Builder::new()
            .name("project-stamp".into())
            .spawn(move || {
                let _ending: Sender<()> = ending;
                stamp_until_gone(fd, root, started, queue);
            })
            .inspect_err(|err| eprintln!("[project] the stamp's thread could not start: {err}"))
            .ok()?;
        Some(ProjectStamp { activations, ended })
    }

    /// Queues the window's activation at `at`. Never blocks, so it is safe on the GTK thread.
    pub fn activated(&self, at: SystemTime) {
        // The thread ends only when this sender goes.
        let _ = self.activations.send(at);
    }

    /// Ends the queue and waits for the thread to work through it, `within` at most, so a stalled state directory
    /// cannot hold a close. False if the thread was still busy when that ran out.
    fn finish(self, within: Duration) -> bool {
        drop(self.activations);
        self.ended.recv_timeout(within) != Err(RecvTimeoutError::Timeout)
    }
}

fn stamp_until_gone(fd: OwnedFd, root: Option<PathBuf>, started: SystemTime, queue: Receiver<SystemTime>) {
    if let Some(root) = root {
        // The write carries the moment it lands. After a stall that can be past a later use of another project, and a
        // forward-only touch could never bring it back, so the write takes the time of the start it stands for. No
        // other window held the lock when this one took it; one that links during the stall can lose a touch to this,
        // and the worst of that is one row out of order.
        let stamped = PairLock::stamp_root(&fd, &root).and_then(|()| PairLock::date_stamp(&fd, started));
        if let Err(err) = stamped {
            eprintln!("[project] the root could not be written to the lock: {err}");
        }
    }
    for at in queue {
        if let Err(err) = PairLock::touch(&fd, at) {
            eprintln!("[project] the lock's time could not be set: {err}");
        }
    }
}

/// A window's stamp, once it has one. A window that gets its lock in `welcome` can gain activation first, so the newest
/// activation waits here for the stamp: no activation is lost to the order those two arrive in.
#[derive(Default)]
pub struct StampSlot {
    stamp: RefCell<Option<ProjectStamp>>,
    waiting: Cell<Option<SystemTime>>,
}

impl StampSlot {
    pub fn fill(&self, stamp: Option<ProjectStamp>) {
        if let (Some(stamp), Some(at)) = (&stamp, self.waiting.take()) {
            stamp.activated(at);
        }
        *self.stamp.borrow_mut() = stamp;
    }

    pub fn activated(&self, at: SystemTime) {
        match &*self.stamp.borrow() {
            Some(stamp) => stamp.activated(at),
            None => self.waiting.set(Some(at)),
        }
    }

    /// The window is closing: see [`ProjectStamp::finish`]. True with no stamp to wait for.
    pub fn finish(&self, within: Duration) -> bool {
        let stamp = self.stamp.borrow_mut().take();
        stamp.is_none_or(|stamp| stamp.finish(within))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn started(&self) -> bool {
        self.stamp.borrow().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linked::tests::Scratch;
    use eitri_core::window_link::pair::pair_lock_name;
    use std::path::Path;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    fn root() -> &'static Path {
        Path::new("/home/someone/project")
    }

    fn held(dir: &Scratch) -> PairLock {
        PairLock::try_take(&dir.0, root()).unwrap().unwrap()
    }

    fn lock_path(dir: &Scratch) -> PathBuf {
        dir.0.join(pair_lock_name(root()))
    }

    fn mtime(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    /// `n` seconds past an hour from now, in whole seconds: past any mtime a write just left, and exact on a
    /// filesystem that keeps coarser times than nanoseconds.
    fn ahead(n: u64) -> SystemTime {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        UNIX_EPOCH + Duration::from_secs(now + 3600 + n)
    }

    /// `n` seconds before now, in whole seconds: behind any write that lands now.
    fn behind(n: u64) -> SystemTime {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        UNIX_EPOCH + Duration::from_secs(now - n)
    }

    /// The thread works on its own time, so a test waits for what it did; the bound only ends a test that failed.
    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn activations_reach_the_lock_in_order_with_no_rate_limit() {
        let dir = Scratch::new("stamp-order");
        let lock = held(&dir);
        let path = lock_path(&dir);
        let stamp = ProjectStamp::start(&lock, Some(root().to_path_buf())).unwrap();
        let base = ahead(0);
        // Each lands with no wait in between: a throttle would hold the second back.
        for n in 1..=20 {
            let at = base + Duration::from_secs(n);
            stamp.activated(at);
            wait_for("an activation landing", || mtime(&path) == at);
        }
        // The newest of a burst is the one that stays.
        for n in 21..=220 {
            stamp.activated(base + Duration::from_secs(n));
        }
        wait_for("the burst's last activation landing", || {
            mtime(&path) == base + Duration::from_secs(220)
        });
    }

    #[test]
    fn the_holders_root_lands_before_its_first_activation() {
        let dir = Scratch::new("stamp-first");
        let lock = held(&dir);
        let path = lock_path(&dir);
        let stamp = ProjectStamp::start(&lock, Some(root().to_path_buf())).unwrap();
        let at = ahead(0);
        stamp.activated(at);
        wait_for("the activation landing", || mtime(&path) == at);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            root().as_os_str().as_encoded_bytes(),
            "a write after the touch would have moved the mtime off it"
        );
    }

    #[test]
    fn an_adopted_stamp_moves_the_mtime_and_writes_no_content() {
        let dir = Scratch::new("stamp-adopted");
        let lock = held(&dir);
        let adopted = PairLock::adopt(lock.share().unwrap()).unwrap();
        let path = lock_path(&dir);
        let stamp = ProjectStamp::start(&adopted, None).unwrap();
        let at = ahead(0);
        stamp.activated(at);
        wait_for("the activation landing", || mtime(&path) == at);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn the_lock_goes_free_once_the_window_and_its_stamp_have_gone() {
        let dir = Scratch::new("stamp-drop");
        let lock = held(&dir);
        let stamp = ProjectStamp::start(&lock, None).unwrap();
        drop(lock);
        assert!(
            PairLock::try_take(&dir.0, root()).unwrap().is_none(),
            "the stamp's copy keeps it"
        );
        drop(stamp);
        wait_for("the lock coming free", || {
            PairLock::try_take(&dir.0, root()).unwrap().is_some()
        });
    }

    #[test]
    fn an_activation_before_the_lock_comes_lands_when_it_does() {
        let dir = Scratch::new("stamp-late");
        let lock = held(&dir);
        let path = lock_path(&dir);
        let slot = StampSlot::default();
        let at = ahead(0);
        slot.activated(ahead(0) - Duration::from_secs(1));
        slot.activated(at);
        slot.fill(ProjectStamp::start(&lock, None));
        wait_for("the waiting activation landing", || mtime(&path) == at);
        let next = at + Duration::from_secs(1);
        slot.activated(next);
        wait_for("the next activation landing", || mtime(&path) == next);
    }

    /// A root write that stalled lands after the window's first use, and that of another project. It must not count as
    /// a use of its own: the mtime stays where the start and the activations put it.
    #[test]
    fn a_root_write_that_lands_late_does_not_pass_a_later_use() {
        let dir = Scratch::new("stamp-late-root");
        let lock = held(&dir);
        let started = behind(20);
        let used = started + Duration::from_secs(1);
        let (activations, queue) = mpsc::channel();
        activations.send(used).unwrap();
        drop(activations);
        stamp_until_gone(lock.share().unwrap(), Some(root().to_path_buf()), started, queue);
        assert_eq!(
            std::fs::read(lock_path(&dir)).unwrap(),
            root().as_os_str().as_encoded_bytes()
        );
        assert_eq!(mtime(&lock_path(&dir)), used);
    }

    #[test]
    fn a_root_write_with_no_use_after_it_dates_from_the_start() {
        let dir = Scratch::new("stamp-start-time");
        let lock = held(&dir);
        let started = behind(20);
        let (activations, queue) = mpsc::channel();
        drop(activations);
        stamp_until_gone(lock.share().unwrap(), Some(root().to_path_buf()), started, queue);
        assert_eq!(mtime(&lock_path(&dir)), started);
    }

    #[test]
    fn a_close_finds_every_queued_activation_landed() {
        let dir = Scratch::new("stamp-close");
        let lock = held(&dir);
        let slot = StampSlot::default();
        slot.fill(ProjectStamp::start(&lock, Some(root().to_path_buf())));
        let base = ahead(0);
        for n in 1..=200 {
            slot.activated(base + Duration::from_secs(n));
        }
        assert!(slot.finish(Duration::from_secs(30)));
        assert_eq!(mtime(&lock_path(&dir)), base + Duration::from_secs(200));
    }

    #[test]
    fn a_stamp_that_is_stalled_does_not_hold_the_close() {
        let (activations, _queue) = mpsc::channel();
        let (_still_running, ended) = mpsc::channel();
        let stamp = ProjectStamp { activations, ended };
        assert!(!stamp.finish(Duration::from_millis(1)), "the thread is still at work");
    }

    #[test]
    fn a_window_with_no_stamp_has_nothing_to_wait_for_at_its_close() {
        assert!(StampSlot::default().finish(Duration::ZERO));
    }

    /// The window's own wiring has no window to run in here, so it is read: losing one of these leaves every test
    /// above green and the recent list silently wrong.
    #[test]
    fn every_start_stamps_and_activations_and_the_close_reach_the_stamp() {
        use eitri_core::source_scan::{code_only, functions, read_required};
        let flat = |code: &str| code.split_whitespace().collect::<String>();
        let main_rs = code_only(&read_required("shell/src/main.rs").text);

        let main = flat(functions(&main_rs, "main")[0]);
        assert_eq!(main.matches("pair.start_stamp(None)").count(), 2);
        let holder = "pair.start_stamp(Some(project_root.to_path_buf()))";
        assert_eq!(main.matches(holder).count(), 1);

        let window = flat(functions(&main_rs, "build_ui")[0]);
        let watched = "linked_window_glue::watch_activation(&window,pair.stamp())";
        assert_eq!(window.matches(watched).count(), 1);

        let services = code_only(&read_required("shell/src/gtk_host/services.rs").text);
        let close = flat(functions(&services, "pair_on_close")[0]);
        assert_eq!(close.matches("pair.stamp().finish(").count(), 1);
    }
}
