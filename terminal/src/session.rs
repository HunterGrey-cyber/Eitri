//! One terminal: a thread that owns the PTY, the child and the [`Screen`], and a handle the host
//! keeps.
//!
//! **Everything that is not drawing happens on the session thread.** It `poll(2)`s the PTY master
//! and a doorbell pipe, and nothing else:
//! - **output** is read, handed to the tap, parsed, and answered -- a DSR or DA reply is written
//!   straight back from this thread, so it never waits on the GTK main loop;
//! - **backpressure is by construction**: while this thread parses it does not read, so the
//!   kernel's PTY buffer fills and the child blocks. There is no unbounded queue of output anywhere
//!   (the frozen pipeline and the spike's `mpsc` both buffered all 15 MB while falling behind), and
//!   what waits to be WRITTEN is bounded too: answers the program does not read stop at
//!   [`crate::REPLY_CAP`] (`outbox.rs`);
//! - **rendering** follows [`RenderClock`]: a leading edge, then at most one per frame, and none
//!   while idle or hidden. The frame goes into a latest-wins slot, so a stalled host costs one frame
//!   of memory;
//! - **a synchronized update left open** (`ESC[?2026h` from an nvim that was then killed) is ended
//!   after [`SYNC_UPDATE_TIMEOUT`], which is the policy `terminal-sync` leaves to the event loop;
//! - **input** is encoded here, against the `Term`'s live mode, so a mode change and a key cannot
//!   race;
//! - **the host is woken** by the `wake` callback, at most once until it next calls
//!   [`TerminalSession::take_update`]. The callback is the host's (GTK's is a channel into the main
//!   context); this crate never names a toolkit.
//!
//! The child is reaped only here, by its own pid -- no process-wide SIGCHLD handler, so nothing
//! competes with glib's or tokio's child watches. EOF on the master is not the child's exit: a child
//! can close the terminal and keep running (`exec nohup cmd`), and a blocking wait there would stop
//! this thread hearing `Shutdown`. Such a child is reported as having left the terminal and is
//! still hung up and killed at shutdown. A panic on this thread is contained: the child is hung up
//! and reaped (`PtyChild`'s `Drop`), and the host is told the session ended, so Enter restarts
//! rather than the pane going dead.

use std::any::Any;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::process::ExitStatusExt;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use terminal_input::NormalizedInput;
use terminal_render::PaintList;

use crate::clock::{RenderClock, FRAME_INTERVAL};
use crate::listener::HostEvents;
use crate::outbox::Outbox;
use crate::pty::{set_nonblocking, PtyChild, PtySize, SpawnSpec, HANGUP_GRACE};
use crate::screen::{CursorCell, Screen, TerminalColors};

/// One read.
const READ_CHUNK: usize = 64 * 1024;
/// Bytes read per wake-up before the thread renders and looks at commands again. Whatever is left
/// stays in the kernel, where it blocks the child.
const READ_BUDGET: usize = 1024 * 1024;
/// How long a synchronized update may hold publication back before it is ended as if its
/// `ESC[?2026l` had arrived: alacritty's own deadline (`vte-0.15.0` `SYNC_UPDATE_TIMEOUT`), which
/// its event loop enforces and `terminal-sync` leaves to ours (`SyncBarrier::abort_sync`).
pub const SYNC_UPDATE_TIMEOUT: Duration = Duration::from_millis(150);
/// After EOF on the master, how long the child gets to become reapable before it is taken to have
/// left the terminal still running. An exiting process closes its descriptors -- the EOF -- just
/// BEFORE it becomes a zombie, so a single `try_wait` at EOF can miss an ordinary exit.
const EXIT_AFTER_EOF: Duration = Duration::from_millis(200);
/// How often a child that left the terminal is checked for its own exit, so it is reaped. Only in
/// that state: an ordinary session has no timer at all.
const DETACHED_REAP_INTERVAL: Duration = Duration::from_millis(250);

/// What the host tells the session.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    /// A key or a paste, encoded on the session thread against the live `TermMode`.
    Input(NormalizedInput),
    /// A new grid: `TIOCSWINSZ` on the PTY and a `Term` resize, together. Below one cell is one cell.
    Resize(PtySize),
    /// Whether the pane holds the keys: a solid cursor or a hollow one. Nothing else: no focus report
    /// (`CSI I`/`CSI O`, DECSET 1004) reaches the program before phase 3 adds one (spec §6).
    Focus(bool),
    /// Whether the pane is on screen. Hidden, output is still read, parsed and answered, and nothing
    /// is rendered; shown again, the latest screen is rendered at once.
    Visible(bool),
    SetColors(TerminalColors),
    /// Hang up the child, reap it (killing it after [`HANGUP_GRACE`]), and end the thread.
    Shutdown,
}

/// How the child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// The child closed every handle it had on the terminal and still runs (`exec nohup cmd`):
    /// nothing more can reach the screen. `code` and `signal` are `None`. It is still this
    /// terminal's child, hung up and killed when the session ends.
    pub detached: bool,
}

impl ExitInfo {
    /// Neither a status nor a detach: the child could not be waited for, or the session thread
    /// failed.
    pub const UNKNOWN: ExitInfo = ExitInfo {
        code: None,
        signal: None,
        detached: false,
    };

    fn from_status(status: ExitStatus) -> Self {
        ExitInfo {
            code: status.code(),
            signal: status.signal(),
            detached: false,
        }
    }

    fn detached() -> Self {
        ExitInfo {
            detached: true,
            ..ExitInfo::UNKNOWN
        }
    }

    /// The line the pane keeps under the last screen (spec §4.6: hold the screen, Enter restarts).
    pub fn notice(&self) -> String {
        if self.detached {
            return "[process left the terminal, still running \u{2014} Enter ends it and restarts]".to_string();
        }
        let how = match (self.code, self.signal) {
            (Some(code), _) => code.to_string(),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "?".to_string(),
        };
        format!("[process exited {how} \u{2014} Enter restarts]")
    }
}

/// Everything new since the host last looked. Bounded: one frame, one slot per event kind.
#[derive(Default)]
pub struct Update {
    pub frame: Option<PaintList>,
    /// Where the cursor is on `frame`, visible or not (bottom-terminal phase 2): set with every
    /// frame the session renders, and cleared with a contained panic's failure frame, so the two
    /// always agree.
    pub cursor: Option<CursorCell>,
    pub events: HostEvents,
    pub exited: Option<ExitInfo>,
}

/// How to start a session.
pub struct SessionConfig {
    pub spawn: SpawnSpec,
    pub size: PtySize,
    pub colors: TerminalColors,
    pub focused: bool,
    /// Called on the session thread with every chunk read from the PTY, before it is parsed. The
    /// frozen design's "one raw stream, any number of readers" contract, reduced to what phase 1
    /// needs: the corpus recorder. Must not block -- it runs where the parsing does.
    pub tap: Option<Box<dyn FnMut(&[u8]) + Send>>,
}

#[derive(Default)]
struct Shared {
    update: Mutex<Update>,
    wake_pending: AtomicBool,
    renders: AtomicU64,
    thread_wakeups: AtomicU64,
    outbox_high_water: AtomicUsize,
    replies_dropped: AtomicU64,
}

impl Shared {
    /// The pending update. A panic on the session thread is contained, and must not reach the host
    /// through a poisoned lock on its next `take_update`: a poisoned `Update` is still a valid one.
    fn update(&self) -> MutexGuard<'_, Update> {
        self.update.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, wake: &dyn Fn(), apply: impl FnOnce(&mut Update)) {
        self.apply_pending(apply);
        self.ring(wake);
    }

    /// Folds `apply` into the pending update without ringing the host. Engine review 2026-09-23,
    /// minor 1: a title or bell rides the render clock out (see `Worker::events_pending`) rather
    /// than waking the host itself, so a stream with an event on most reads wakes the host once per
    /// rendered frame, not once per loop turn.
    fn apply_pending(&self, apply: impl FnOnce(&mut Update)) {
        apply(&mut self.update());
    }

    /// Rings the host for whatever is already pending, if it has not already been rung since its
    /// last `take_update`.
    fn ring(&self, wake: &dyn Fn()) {
        if !self.wake_pending.swap(true, Ordering::SeqCst) {
            wake();
        }
    }
}

/// The host's handle on one terminal. Dropping it hangs the child up and lets the thread finish on
/// its own; [`TerminalSession::shutdown_and_wait`] does the same and waits.
pub struct TerminalSession {
    shared: Arc<Shared>,
    commands: Sender<SessionCommand>,
    doorbell: PipeWriter,
    pid: u32,
    thread: Option<JoinHandle<Option<ExitInfo>>>,
}

impl TerminalSession {
    pub fn spawn(config: SessionConfig, wake: impl Fn() + Send + 'static) -> io::Result<Self> {
        let pty = PtyChild::spawn(&config.spawn, config.size)?;
        let pid = pty.pid();
        // From here on, an early return drops `pty`, whose `Drop` hangs the child up and reaps it.
        let (bell_rx, bell_tx) = std::io::pipe()?;
        set_nonblocking(bell_rx.as_fd())?;
        set_nonblocking(bell_tx.as_fd())?;
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let SessionConfig {
            size,
            colors,
            focused,
            tap,
            ..
        } = config;
        let thread = std::thread::Builder::new()
            .name(format!("terminal-{pid}"))
            .spawn(move || {
                let worker = Worker {
                    screen: Screen::new(size, colors),
                    pty,
                    size: size.clamped(),
                    focused,
                    visible: true,
                    detached: false,
                    commands: receiver,
                    doorbell: bell_rx,
                    shared: thread_shared.clone(),
                    wake: &wake,
                    tap,
                    clock: RenderClock::new(FRAME_INTERVAL),
                    outbox: Outbox::new(),
                    sync_deadline: None,
                    events_pending: false,
                };
                match panic::catch_unwind(AssertUnwindSafe(move || worker.run())) {
                    Ok(exit) => exit,
                    Err(payload) => {
                        contain_panic(&thread_shared, &wake, size, colors, payload.as_ref());
                        None
                    }
                }
            })?;
        Ok(TerminalSession {
            shared,
            commands,
            doorbell: bell_tx,
            pid,
            thread: Some(thread),
        })
    }

    pub fn send(&self, command: SessionCommand) {
        if self.commands.send(command).is_ok() {
            // Non-blocking: a full doorbell already guarantees a wake-up that drains every command.
            let _ = (&self.doorbell).write(&[1]);
        }
    }

    /// Everything new since the last call. Clears the wake-up flag FIRST, so an update published
    /// while this runs rings the host again rather than being stranded.
    pub fn take_update(&self) -> Update {
        self.shared.wake_pending.store(false, Ordering::SeqCst);
        std::mem::take(&mut *self.shared.update())
    }

    /// The child's pid, captured at spawn.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// How many frames the session has rendered. The idle guard's first counter.
    pub fn renders(&self) -> u64 {
        self.shared.renders.load(Ordering::SeqCst)
    }

    /// How many times the session thread's `poll` has returned. The idle guard's second counter: a
    /// `poll` timeout would wake the thread without rendering anything.
    pub fn thread_wakeups(&self) -> u64 {
        self.shared.thread_wakeups.load(Ordering::SeqCst)
    }

    /// The most bytes of input and answers that ever waited for the PTY at once.
    pub fn outbox_high_water(&self) -> usize {
        self.shared.outbox_high_water.load(Ordering::SeqCst)
    }

    /// Answer bytes dropped because the program was not reading them ([`crate::REPLY_CAP`]).
    pub fn replies_dropped(&self) -> u64 {
        self.shared.replies_dropped.load(Ordering::SeqCst)
    }

    /// Hangs up, reaps, and returns how the child ended (`None` if it could not be reaped).
    pub fn shutdown_and_wait(mut self) -> Option<ExitInfo> {
        self.send(SessionCommand::Shutdown);
        self.thread.take().and_then(|t| t.join().ok()).flatten()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Not joined: the GTK thread must not wait out a child's grace period. If the process exits
        // first, closing the master fd hangs the child up anyway.
        self.send(SessionCommand::Shutdown);
    }
}

/// The session thread panicked -- in the emulator on hostile input, in a host's tap -- and the
/// worker has already unwound: its `PtyChild` hung the child up and handed it to a reaper. What is
/// left is to say so where the pane looks: a frame with one line, and `exited`, so Enter restarts.
fn contain_panic(shared: &Shared, wake: &dyn Fn(), size: PtySize, colors: TerminalColors, payload: &(dyn Any + Send)) {
    let why = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_string());
    eprintln!("[terminal] the session thread failed ({why}); its child was hung up");
    let frame = panic::catch_unwind(|| {
        let mut screen = Screen::new(size, colors);
        screen.feed("\x1b[0;2m[the terminal failed \u{2014} Enter restarts]\x1b[0m".as_bytes());
        screen.render(false)
    });
    shared.publish(wake, |update| {
        if let Ok(frame) = frame {
            update.frame = Some(frame);
        }
        // `Update::cursor` agrees with `frame`, and the failure frame has no cursor a composition
        // could sit at. Without this, a frame rendered and not yet taken when the thread panicked
        // left its cursor beside the failure frame (whole-branch review 2026-09-24, engine minor 1).
        update.cursor = None;
        update.exited = Some(ExitInfo::UNKNOWN);
    });
}

struct Worker<'w> {
    pty: PtyChild,
    screen: Screen,
    size: PtySize,
    focused: bool,
    visible: bool,
    /// The child left the terminal and still runs: the master is not polled any more, and the
    /// thread only waits for the child's own exit or for `Shutdown`.
    detached: bool,
    commands: Receiver<SessionCommand>,
    doorbell: PipeReader,
    shared: Arc<Shared>,
    wake: &'w dyn Fn(),
    tap: Option<Box<dyn FnMut(&[u8]) + Send>>,
    clock: RenderClock,
    /// Input and answers the PTY has not taken yet. The master is non-blocking, so a program that
    /// stops reading its input cannot stall output parsing behind a blocked write.
    outbox: Outbox,
    /// The open synchronized update (`Screen::open_update`'s ordinal) and when it must be ended.
    sync_deadline: Option<(u64, Instant)>,
    /// A title or bell arrived and is waiting to ride the render clock out to the host, the same
    /// way a dirty screen does (engine review 2026-09-23, minor 1): [`Worker::render_if_due`]'s own
    /// gate does not see a title/bell change at all (`Screen::take_dirty` answers for pixels, not
    /// for these), so without this flag a stream with events but no other screen change would never
    /// be judged "changed" and its title/bell would wait forever rather than for one frame.
    events_pending: bool,
}

enum Flow {
    Continue,
    ChildGone,
}

impl Worker<'_> {
    fn run(mut self) -> Option<ExitInfo> {
        let mut buf = vec![0u8; READ_CHUNK];
        // The first frame, so a pane whose program prints nothing yet still shows its background.
        self.render_if_due(Instant::now());
        loop {
            let mut fds = [
                libc::pollfd {
                    // A negative fd is ignored by poll: a master whose every user is gone would
                    // report POLLHUP forever.
                    fd: if self.detached {
                        -1
                    } else {
                        self.pty.master_fd().as_raw_fd()
                    },
                    events: libc::POLLIN | if self.outbox.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.doorbell.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = self.poll_timeout(Instant::now());
            // SAFETY: `fds` is a live array of two initialised pollfds for the duration of the call.
            let polled = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            self.shared.thread_wakeups.fetch_add(1, Ordering::SeqCst);
            if polled < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Rare (`ENOMEM`, ...), and not host-initiated like the other `shut_down` calls in
                // this loop: unlike those, the host has no idea this is happening and needs telling,
                // or the pane goes silent with no notice and Enter does not restart (fix round 1,
                // review finding 5). `shut_down` consumes `self`, so the pieces `publish` needs are
                // captured first.
                let (wake, shared) = (self.wake, self.shared.clone());
                let exit = self.shut_down();
                shared.publish(wake, |update| update.exited = exit.or(Some(ExitInfo::UNKNOWN)));
                return exit;
            }
            if fds[1].revents != 0 {
                let mut drain = [0u8; 64];
                while matches!(self.doorbell.read(&mut drain), Ok(n) if n > 0) {}
                if self.handle_commands() {
                    return self.shut_down();
                }
            }
            if self.detached {
                match self.pty.try_wait() {
                    Ok(None) => {}
                    // The host already has `exited: Some(detached)`; it must also hear the real
                    // end, not just get it back from `shutdown_and_wait` if something calls that
                    // (fix round 1, review finding 4) -- otherwise the pane keeps saying "still
                    // running" about a process that is gone.
                    Ok(Some(status)) => return Some(self.show_end(ExitInfo::from_status(status))),
                    Err(_) => return None,
                }
            } else {
                if fds[0].revents & libc::POLLOUT != 0 {
                    self.flush_outbox();
                }
                if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    let flow = self.read_output(&mut buf);
                    // Before acting on EOF: the last read before it can carry a title or a copy.
                    self.publish_events(Instant::now());
                    if let Flow::ChildGone = flow {
                        match self.wait_after_eof() {
                            Ok(Some(status)) => return Some(self.show_end(ExitInfo::from_status(status))),
                            Ok(None) => {
                                self.detached = true;
                                self.show_end(ExitInfo::detached());
                            }
                            Err(_) => {
                                self.show_end(ExitInfo::UNKNOWN);
                                return None;
                            }
                        }
                    }
                }
            }
            // In both states: a deadline left armed when the child detached would otherwise stay
            // in the past and turn `poll` into a busy loop.
            self.track_sync(Instant::now());
            self.render_if_due(Instant::now());
        }
    }

    fn poll_timeout(&self, now: Instant) -> libc::c_int {
        let render = if self.visible { self.clock.deadline() } else { None };
        let sync = self.sync_deadline.map(|(_, due)| due);
        let reap = self.detached.then(|| now + DETACHED_REAP_INTERVAL);
        match [render, sync, reap].into_iter().flatten().min() {
            None => -1,
            Some(due) => due
                .saturating_duration_since(now)
                .as_micros()
                .div_ceil(1000)
                .min(i32::MAX as u128) as i32,
        }
    }

    /// `true`: shut down.
    fn handle_commands(&mut self) -> bool {
        loop {
            match self.commands.try_recv() {
                Ok(SessionCommand::Input(input)) => {
                    let bytes = terminal_input::encode(&input, self.screen.mode());
                    self.outbox.push_input(&bytes);
                    self.flush_outbox();
                }
                Ok(SessionCommand::Resize(size)) => {
                    let size = size.clamped();
                    if size != self.size {
                        self.size = size;
                        if let Err(e) = self.pty.resize(size) {
                            eprintln!("[terminal] TIOCSWINSZ {}x{} failed: {e}", size.cols, size.rows);
                        }
                        self.screen.resize(size);
                    }
                }
                Ok(SessionCommand::Focus(focused)) => {
                    if focused != self.focused {
                        self.focused = focused;
                        self.screen.mark_dirty();
                    }
                }
                Ok(SessionCommand::Visible(visible)) => {
                    if visible != self.visible {
                        self.visible = visible;
                        if visible {
                            self.screen.mark_dirty();
                        }
                    }
                }
                Ok(SessionCommand::SetColors(colors)) => self.screen.set_colors(colors),
                Ok(SessionCommand::Shutdown) | Err(TryRecvError::Disconnected) => return true,
                Err(TryRecvError::Empty) => return false,
            }
        }
    }

    fn read_output(&mut self, buf: &mut [u8]) -> Flow {
        let mut total = 0;
        while total < READ_BUDGET {
            match self.pty.read(buf) {
                Ok(0) => return Flow::ChildGone,
                Ok(n) => {
                    total += n;
                    if let Some(tap) = self.tap.as_mut() {
                        tap(&buf[..n]);
                    }
                    self.screen.feed(&buf[..n]);
                    let replies = self.screen.take_replies();
                    if !replies.is_empty() {
                        self.outbox.push_reply(replies);
                        self.flush_outbox();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // EIO: every holder of the user side has closed it.
                Err(_) => return Flow::ChildGone,
            }
        }
        Flow::Continue
    }

    /// Title, bell and the two copies from the reads just done, merged into the pending update:
    /// latest title and copy win, bells fold into one.
    /// Folds title/bell/clipboard into the pending update. Rides the render clock out rather than
    /// ringing the host itself (engine review 2026-09-23, minor 1): a stream with an event on most
    /// reads (a bell in a `cat` of a binary, say) used to wake the host once per loop turn instead
    /// of once per rendered frame, and did so even while hidden, where phase 1's pump does nothing
    /// with the wake anyway. Clipboard is the one exception: it is the one event this pump already
    /// acts on, so hidden or not it rings at once rather than waiting for a clock this pane, while
    /// hidden, never polls. A copy to the primary selection is the same kind of event (phase 2).
    fn publish_events(&mut self, now: Instant) {
        let events = self.screen.take_events();
        if events == HostEvents::default() {
            return;
        }
        // Either copy: the pump puts both on the desktop at once (bottom-terminal phase 2).
        let has_clipboard = events.clipboard.is_some() || events.primary.is_some();
        self.apply_pending(|update| {
            if events.title.is_some() {
                update.events.title = events.title;
            }
            update.events.bell |= events.bell;
            if events.clipboard.is_some() {
                update.events.clipboard = events.clipboard;
            }
            if events.primary.is_some() {
                update.events.primary = events.primary;
            }
        });
        if !self.visible {
            if has_clipboard {
                self.ring();
            }
            return;
        }
        self.events_pending = true;
        self.clock.changed(now);
    }

    fn flush_outbox(&mut self) {
        let pty = &mut self.pty;
        // `Closed`: the child is gone, and the read side will see it.
        let _ = self.outbox.flush(|bytes| pty.write(bytes));
        self.shared
            .outbox_high_water
            .store(self.outbox.high_water(), Ordering::SeqCst);
        let dropped = self.outbox.dropped();
        if self.shared.replies_dropped.swap(dropped, Ordering::SeqCst) == 0 && dropped > 0 {
            eprintln!(
                "[terminal] pid {} is not reading its input: answers past {} KiB are dropped",
                self.pty.pid(),
                crate::REPLY_CAP / 1024
            );
        }
    }

    /// Arms, keeps or clears the deadline of the open synchronized update, and ends the update once
    /// the deadline has passed. A new update (a new ordinal) gets a fresh deadline, so a program
    /// drawing frame after frame, each inside its own update, is never cut into.
    fn track_sync(&mut self, now: Instant) {
        self.sync_deadline = match (self.screen.open_update(), self.sync_deadline) {
            (None, _) => None,
            (Some(open), Some((armed, due))) if open == armed => {
                if now < due {
                    Some((armed, due))
                } else {
                    self.screen.abort_sync();
                    None
                }
            }
            (Some(open), _) => Some((open, now + SYNC_UPDATE_TIMEOUT)),
        };
    }

    fn render_if_due(&mut self, now: Instant) {
        if !self.visible {
            return;
        }
        // A title/bell rides this same clock (`events_pending`, set by `publish_events`): pixels
        // dirty OR an event pending both count as "something changed", and both are cleared by the
        // one publish below -- one ring, whatever the reason.
        let dirty = self.screen.take_dirty();
        let changed = (dirty || self.events_pending) && self.clock.changed(now);
        if changed || self.clock.is_due(now) {
            let frame = self.screen.render(self.focused);
            let cursor = self.screen.cursor_cell();
            self.clock.rendered(now);
            self.events_pending = false;
            self.shared.renders.fetch_add(1, Ordering::SeqCst);
            self.publish(|update| {
                update.frame = Some(frame);
                update.cursor = Some(cursor);
            });
        }
    }

    fn publish(&self, apply: impl FnOnce(&mut Update)) {
        self.shared.publish(self.wake, apply);
    }

    fn apply_pending(&self, apply: impl FnOnce(&mut Update)) {
        self.shared.apply_pending(apply);
    }

    fn ring(&self) {
        self.shared.ring(self.wake);
    }

    /// EOF on the master: wait briefly for the child to become reapable (see [`EXIT_AFTER_EOF`]).
    /// `Ok(None)`: it still runs, having closed the terminal.
    fn wait_after_eof(&mut self) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + EXIT_AFTER_EOF;
        loop {
            match self.pty.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                None => return Ok(None),
            }
        }
    }

    /// Keeps the screen, says how the child ended under it, and tells the host.
    fn show_end(&mut self, exit: ExitInfo) -> ExitInfo {
        // The child is gone (or has left the terminal for good): no `ESC[?2026l` can ever arrive
        // to close a synchronized update it left open, and `Screen::render` shows only the stale
        // pre-update snapshot for as long as one stays open (fix round 1, review finding 1).
        self.screen.abort_sync();
        self.screen
            .feed(format!("\r\n\x1b[0;2m{}\x1b[0m", exit.notice()).as_bytes());
        let frame = self.screen.render(self.focused);
        let cursor = self.screen.cursor_cell();
        self.shared.renders.fetch_add(1, Ordering::SeqCst);
        self.publish(|update| {
            update.frame = Some(frame);
            update.cursor = Some(cursor);
            update.exited = Some(exit);
        });
        exit
    }

    /// The host asked (or went away): hang up, give the child [`HANGUP_GRACE`], then kill it by
    /// the pid captured at spawn.
    fn shut_down(mut self) -> Option<ExitInfo> {
        self.pty.hangup();
        let deadline = Instant::now() + HANGUP_GRACE;
        loop {
            match self.pty.try_wait() {
                Ok(Some(status)) => return Some(ExitInfo::from_status(status)),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    self.pty.kill();
                    return self.pty.wait().ok().map(ExitInfo::from_status);
                }
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whole-branch review 2026-09-24 (engine minor 1, window minor 2): a frame the session rendered,
    /// rang the host for and the host has not taken yet, followed by a panic on the thread. The
    /// failure frame replaces it in the pending update, and must not go out beside that frame's
    /// cursor: the pane would draw a composition, and point the input method, at a cell of a screen
    /// that is gone. Deterministic by construction -- the panic is `contain_panic` itself, called
    /// with the render still pending, which a live session reaches only by losing a race.
    #[test]
    fn a_contained_panic_does_not_leave_the_previous_frames_cursor_beside_its_own() {
        let size = PtySize {
            cols: 20,
            rows: 3,
            cell_width_px: 9,
            cell_height_px: 18,
        };
        let shared = Shared::default();
        let mut screen = Screen::new(size, TerminalColors::default());
        screen.feed(b"abc");
        let rendered = screen.render(true);
        let cursor = screen.cursor_cell();
        assert_eq!((cursor.row, cursor.col), (0, 3));
        shared.publish(&|| {}, |update| {
            update.frame = Some(rendered.clone());
            update.cursor = Some(cursor);
        });
        contain_panic(&shared, &|| {}, size, TerminalColors::default(), &"hostile input");
        let update = std::mem::take(&mut *shared.update());
        assert_eq!(update.exited, Some(ExitInfo::UNKNOWN));
        assert!(
            update.frame.as_ref().is_some_and(|frame| *frame != rendered),
            "the failure frame replaced the pending one"
        );
        assert_eq!(update.cursor, None, "the failure frame carries no cursor");
    }
}
