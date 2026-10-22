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
//!
//! **A terminal can move to another process** with its child, its screen and what it still owes
//! the child. [`SessionCommand::Detach`] stops this thread at a point where nothing is half done --
//! no synchronized update open, the parser between sequences where it can be, output not yet
//! parsed set aside -- and hands over a duplicate of the PTY's controller side, a replay of the
//! screen, and the unwritten input and answers. From then on this thread reads, writes and renders
//! nothing until it hears how the move ended: [`SessionCommand::Committed`] (the other process has
//! the terminal: forget the child, without a hangup) or [`SessionCommand::Resume`] (it does not:
//! build a fresh screen from the same record, as the other process would have, write what was
//! owed, read again). The other side is [`TerminalSession::adopt`], which reads and writes nothing
//! before [`TerminalSession::commit`]: a process that dies before its commit leaves the PTY exactly
//! as the source left it.

use std::any::Any;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use terminal_input::NormalizedInput;
use terminal_render::PaintList;

use crate::clock::{RenderClock, FRAME_INTERVAL};
use crate::listener::HostEvents;
use crate::mouse::{self, MouseModes};
use crate::outbox::Outbox;
use crate::pty::{set_nonblocking, PtyChild, PtySize, SpawnSpec, HANGUP_GRACE};
use crate::replay::screen_replay::{ReplayRefused, ScreenReplay};
use crate::screen::{CursorCell, Screen, TerminalColors, ViewState};
use crate::scroll::ScrollRequest;
use crate::select::SelectCommand;

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
/// How long a move waits, reading on, for a sequence cut by the last read to finish: a program
/// writes a sequence in one write almost always, so its rest is either already in the kernel or
/// about to be.
pub const BOUNDARY_WAIT: Duration = Duration::from_millis(50);
/// The most bytes of an unfinished sequence a move carries; past it (a long OSC or DCS being
/// streamed) the move is refused and the terminal stays.
pub const RAW_TAIL_MAX: usize = 64 * 1024;
/// How many times a move lets a synchronized update close when the wait for a boundary opened
/// one (a read cut `ESC[?2026h` before its `h`). Each round waits at most [`SYNC_UPDATE_TIMEOUT`]
/// plus the boundary wait; a program cut that way round after round moves with its update open.
const SETTLE_ROUNDS: usize = 3;
/// How long an adopted session whose child has exited waits for the spawner's report of how it
/// ended ([`SessionCommand::ChildExited`]): only the spawner can wait for the child, and its report
/// crosses to this process after the exit this session saw. Past it, the end is shown as unknown.
pub const EXIT_REPORT_WAIT: Duration = Duration::from_secs(2);

/// A terminal read for a move: the PTY's controller side (a duplicate; the source keeps its own
/// until the move is decided), the child, the screen, and what the source owed the child.
#[derive(Debug)]
pub struct Detached {
    pub master: OwnedFd,
    pub pid: u32,
    /// The child's start time (`/proc/<pid>/stat` field 22), so the receiver can check that the
    /// pid still names the same process before it trusts it.
    pub start: u64,
    pub replay: ScreenReplay,
    pub tail: Tail,
    /// Input and answers not yet written to the PTY, in the order they were to be written.
    pub outbox: Vec<u8>,
    pub view: ViewState,
}

/// Output the source read and the replay does not show.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Tail {
    /// The parser stood between sequences; these bytes were read after that point and not fed.
    /// The new holder feeds them before anything new from the PTY.
    ReadAhead(Vec<u8>),
    /// No boundary came in time: the bytes of the unfinished sequence, all fed already, and the
    /// handler calls they caused (control characters inside the sequence, a DCS's hook). The new
    /// holder feeds them again with that many calls swallowed, so its parser waits where the
    /// source's did and nothing is applied twice.
    Raw { bytes: Vec<u8>, calls: usize },
}

/// Why a terminal does not move. Nothing was read: it goes on as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachRefused {
    /// A sequence over [`RAW_TAIL_MAX`] bytes is still open.
    RawTooLong,
    /// A keyboard-mode stack is at alacritty's depth limit, where replaying it would drop titles
    /// in another order than the source did.
    TooDeep,
    /// The PTY's controller side could not be duplicated (no descriptors left).
    NoDescriptor,
}

/// What the host tells the session.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// A key or a paste, encoded on the session thread against the live `TermMode`.
    Input(NormalizedInput),
    /// A new grid: `TIOCSWINSZ` on the PTY and a `Term` resize, together. Below one cell is one cell.
    Resize(PtySize),
    /// Whether the pane holds the keys: a solid cursor or a hollow one, and nothing else -- the
    /// report is [`SessionCommand::FocusReport`], sent through the pane's one door (`submit`) so it
    /// can never overtake keys still waiting behind a paste. This command changes only the cursor's
    /// shape.
    Focus(bool),
    /// A mouse press, release, motion or wheel notch, encoded on the session thread against the
    /// live `TermMode` (bottom-terminal phase 3c). See [`crate::mouse`].
    Mouse(crate::mouse::MouseInput),
    /// Focus reporting (`CSI I`/`CSI O`, DECSET 1004), sent through the pane's door, unlike
    /// [`SessionCommand::Focus`] (see its own doc).
    FocusReport(bool),
    /// Whether the pane is on screen. Hidden, output is still read, parsed and answered, and nothing
    /// is rendered; shown again, the latest screen is rendered at once.
    Visible(bool),
    SetColors(TerminalColors),
    /// A host scroll gesture: `Shift+PageUp`/`Shift+PageDown`, a wheel notch,
    /// `Shift+Home`/`Shift+End`. See [`crate::scroll::ScrollView`].
    Scroll(ScrollRequest),
    /// A host selection gesture: a drag's start/extend/finish, or a clear. See
    /// [`crate::select`].
    Select(SelectCommand),
    /// `prefix [`/`prefix PageUp` opens the terminal's own read-only copy mode; leaving it (`q`,
    /// `Esc`, or the shell exiting/restarting) always snaps back to the live bottom, as tmux's does.
    /// See [`crate::screen::Screen::set_copy_mode`].
    CopyMode(bool),
    /// Read the terminal for a move (module doc): the answer is the record, or why it stays.
    /// After an `Ok`, the session reads, writes and renders nothing (keys and mouse reports are
    /// dropped: the host refuses them while a module moves) until `Committed` or `Resume`.
    Detach(Sender<Result<Detached, DetachRefused>>),
    /// The move did not happen: rebuild the screen from the record `Detach` gave, exactly as the
    /// new holder would have, write what was owed, and read again.
    Resume,
    /// The move is committed. A session that was detached forgets its child without a hangup and
    /// ends; an adopted one writes what the source owed, takes the host's size, and starts reading.
    Committed,
    /// How an adopted child ended, from the process that spawned it: only that process can wait
    /// for it. Used when it arrives before the session sees the exit, or within
    /// [`EXIT_REPORT_WAIT`] after; past that, the end is shown as unknown.
    ChildExited(ExitStatus),
    /// Hang up the child, reap it (killing it after [`HANGUP_GRACE`]), and end the thread. A
    /// detached session still holds its child until `Committed`, so this ends the child; an
    /// adopted session not yet committed lets it go untouched, since the source still holds it.
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

    /// The line the pane keeps under the last screen (hold the screen, Enter restarts), so what the
    /// program printed last stays readable after it exits.
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
    /// The live mouse-mode summary (bottom-terminal phase 3c), latest-wins like `cursor`, and
    /// `Option` for the same reason: `take_update` is `mem::take`, so an update with no mode change
    /// (a bell, a title) must not overwrite the host's last-known modes with `MouseModes::default()`
    /// and silently switch reporting off. Applied only when `Some`; see [`Worker::publish_mouse_modes`].
    pub mouse: Option<MouseModes>,
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
    /// [`BOUNDARY_WAIT`] unless the host set another, in microseconds.
    boundary_wait_us: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        let shared = Shared::default();
        shared
            .boundary_wait_us
            .store(BOUNDARY_WAIT.as_micros() as u64, Ordering::SeqCst);
        shared
    }

    fn boundary_wait(&self) -> Duration {
        Duration::from_micros(self.boundary_wait_us.load(Ordering::SeqCst))
    }

    /// The pending update. A panic on the session thread is contained, and must not reach the host
    /// through a poisoned lock on its next `take_update`: a poisoned `Update` is still a valid one.
    fn update(&self) -> MutexGuard<'_, Update> {
        self.update.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, wake: &dyn Fn(), apply: impl FnOnce(&mut Update)) {
        self.apply_pending(apply);
        self.ring(wake);
    }

    /// Folds `apply` into the pending update without ringing the host: a title or bell rides the
    /// render clock out (see `Worker::events_pending`) rather
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

/// What a session thread starts from: a child it spawned, or a terminal another process read.
enum Origin {
    Spawned,
    Adopted {
        replay: ScreenReplay,
        tail: Tail,
        outbox: Vec<u8>,
        view: ViewState,
    },
}

impl TerminalSession {
    pub fn spawn(config: SessionConfig, wake: impl Fn() + Send + 'static) -> io::Result<Self> {
        let pty = PtyChild::spawn(&config.spawn, config.size)?;
        Self::start(pty, config, wake, Origin::Spawned)
    }

    /// The destination of a move: a session over a terminal another process read with
    /// [`SessionCommand::Detach`]. `pidfd` names the child; the caller opened it and checked the
    /// record's start time against it. The screen is rebuilt from the replay on the session thread
    /// (with the output the source had not fed, and its view), and may be rendered, but nothing is
    /// written to or read from the PTY until [`TerminalSession::commit`]. Dropped before that, the
    /// session lets the child go untouched. `config.spawn` is not used; `config.size` is the
    /// host's size, taken at the commit.
    #[cfg(target_os = "linux")]
    pub fn adopt(
        detached: Detached,
        pidfd: OwnedFd,
        config: SessionConfig,
        wake: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        let Detached {
            master,
            pid,
            start: _,
            replay,
            tail,
            outbox,
            view,
        } = detached;
        // Uncommitted: if anything fails before the commit -- this start, the rebuild, a panic on
        // the session thread -- the child is left untouched, still the source's to resume.
        let pty = PtyChild::adopt_uncommitted(master, pidfd, pid)?;
        let origin = Origin::Adopted {
            replay,
            tail,
            outbox,
            view,
        };
        Self::start(pty, config, wake, origin)
    }

    /// The move this session was adopted from is committed: write what the source owed (its
    /// unwritten input and answers, then the answers the adoption's own feeding produced), take
    /// the host's size, and start reading. Nothing on a session that was not adopted.
    pub fn commit(&self) {
        self.send(SessionCommand::Committed);
    }

    /// How long a move waits for a cut sequence to finish ([`BOUNDARY_WAIT`] unless set).
    pub fn set_boundary_wait(&self, wait: Duration) {
        self.shared
            .boundary_wait_us
            .store(wait.as_micros().min(u128::from(u64::MAX)) as u64, Ordering::SeqCst);
    }

    fn start(
        pty: PtyChild,
        config: SessionConfig,
        wake: impl Fn() + Send + 'static,
        origin: Origin,
    ) -> io::Result<Self> {
        let pid = pty.pid();
        // From here on, an early return drops `pty`, whose `Drop` hangs a spawned child up and
        // reaps it. An adopted one is not committed yet, and its `Drop` leaves the child alone.
        let (bell_rx, bell_tx) = std::io::pipe()?;
        set_nonblocking(bell_rx.as_fd())?;
        set_nonblocking(bell_tx.as_fd())?;
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::new());
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
                // The rebuild runs inside the containment too: a replay that makes the emulator
                // panic ends this session like any other panic, with the host told.
                let run = AssertUnwindSafe(|| {
                    let (screen, phase) = match origin {
                        Origin::Spawned => (Screen::new(size, colors), Phase::Live),
                        Origin::Adopted {
                            replay,
                            tail,
                            outbox,
                            view,
                        } => {
                            let (screen, replies) = rebuild(&replay, &tail, &view, size, colors);
                            let phase = Phase::Adopted {
                                carried: outbox,
                                replies,
                                held: Vec::new(),
                            };
                            (screen, phase)
                        }
                    };
                    let worker = Worker {
                        screen,
                        pty,
                        size: size.clamped(),
                        colors,
                        focused,
                        visible: true,
                        detached: false,
                        report_due: None,
                        phase,
                        commands: receiver,
                        doorbell: bell_rx,
                        shared: thread_shared.clone(),
                        wake: &wake,
                        tap,
                        clock: RenderClock::new(FRAME_INTERVAL),
                        outbox: Outbox::new(),
                        sync_deadline: None,
                        events_pending: false,
                        frame_owed: false,
                        last_mouse: None,
                    };
                    worker.run()
                });
                match panic::catch_unwind(run) {
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
/// worker has already unwound: its `PtyChild` hung the child up and handed it to a reaper (unless
/// it was adopted for a move not yet committed, whose child stays the source's). What is left is
/// to say so where the pane looks: a frame with one line, and `exited`, so Enter restarts.
fn contain_panic(shared: &Shared, wake: &dyn Fn(), size: PtySize, colors: TerminalColors, payload: &(dyn Any + Send)) {
    let why = payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
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
        // left its cursor beside the failure frame.
        update.cursor = None;
        update.exited = Some(ExitInfo::UNKNOWN);
    });
}

/// A fresh screen from a move's record, as the source read it plus the output it had not fed, and
/// the answers that output asked for.
fn rebuild(
    replay: &ScreenReplay,
    tail: &Tail,
    view: &ViewState,
    size: PtySize,
    colors: TerminalColors,
) -> (Screen, Vec<u8>) {
    let mut screen = Screen::replayed(replay, size, colors);
    screen.apply_view(view);
    match tail {
        Tail::ReadAhead(bytes) => screen.feed(bytes),
        Tail::Raw { bytes, calls } => screen.feed_unfinished(bytes, *calls),
    }
    let replies = screen.take_replies();
    (screen, replies)
}

/// Where the session stands with respect to a move.
enum Phase {
    /// Reading, writing and rendering.
    Live,
    /// Read for a move: the screen's `Term` is spent, and nothing is read, written or rendered.
    /// Holds this side's copy of the record until the move is decided.
    Detached(Kept),
    /// Adopted, not yet committed: the screen is rebuilt and may be rendered, nothing is read or
    /// written. The source's unwritten bytes, the answers the rebuild produced, and keys typed
    /// since, written in that order at the commit.
    Adopted {
        carried: Vec<u8>,
        replies: Vec<u8>,
        held: Vec<u8>,
    },
}

/// The source's copy of what it handed over.
struct Kept {
    replay: ScreenReplay,
    tail: Tail,
    outbox: Vec<u8>,
    view: ViewState,
}

/// What the run loop does after the commands.
enum Next {
    Continue,
    ShutDown,
    /// Let the child go untouched and end.
    Forget,
}

struct Worker<'w> {
    pty: PtyChild,
    screen: Screen,
    size: PtySize,
    /// The colours a rebuilt screen starts with.
    colors: TerminalColors,
    focused: bool,
    visible: bool,
    /// The child left the terminal and still runs: the master is not polled any more, and the
    /// thread only waits for the child's own exit or for `Shutdown`.
    detached: bool,
    /// An adopted child has exited before its spawner said how: until this time the thread waits
    /// for [`SessionCommand::ChildExited`], still hearing commands, with the master not polled.
    report_due: Option<Instant>,
    phase: Phase,
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
    /// way a dirty screen does: [`Worker::render_if_due`]'s own
    /// gate does not see a title/bell change at all (`Screen::take_dirty` answers for pixels, not
    /// for these), so without this flag a stream with events but no other screen change would never
    /// be judged "changed" and its title/bell would wait forever rather than for one frame.
    events_pending: bool,
    /// The screen changed since the last frame was published and a frame is owed: taken from
    /// `Screen::take_dirty` at every turn, and kept until the clock lets the frame go. A turn with
    /// only `events_pending` publishes the events and builds no frame -- a title or a bell changes
    /// nothing the frame shows (`Screen::feed` no longer marks the screen dirty for them), and the
    /// host shows a bell itself.
    frame_owed: bool,
    /// The mouse-mode summary last published (bottom-terminal phase 3c): `None` until
    /// [`Worker::publish_mouse_modes`] first runs, then always `Some`. Compared against the live
    /// mode every loop turn so a change is published at once rather than waiting for the next frame
    /// -- a synchronized update can hold a frame back for up to 150ms, and a host still routing
    /// clicks on stale modes during that window would start a selection in a program that just
    /// turned mouse reporting on (`htop`).
    last_mouse: Option<MouseModes>,
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
                    // report POLLHUP forever, and one in the middle of a move is not read.
                    fd: if self.detached || self.report_due.is_some() || !self.live() {
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
                // or the pane goes silent with no notice and Enter does not restart. `shut_down`
                // consumes `self`, so the pieces `publish` needs are
                // captured first.
                let (wake, shared) = (self.wake, self.shared.clone());
                let exit = self.shut_down();
                shared.publish(wake, |update| update.exited = exit.or(Some(ExitInfo::UNKNOWN)));
                return exit;
            }
            if fds[1].revents != 0 {
                let mut drain = [0u8; 64];
                while matches!(self.doorbell.read(&mut drain), Ok(n) if n > 0) {}
                match self.handle_commands() {
                    Next::Continue => {}
                    Next::ShutDown => return self.shut_down(),
                    Next::Forget => return self.forget_child(),
                }
            }
            if !self.live() {
                // Mid-move: the screen is spent (detached) or not yet the terminal's (adopted),
                // so only the commands above can change anything. An adopted screen still
                // renders, which nothing reads from the PTY for.
                if matches!(self.phase, Phase::Adopted { .. }) {
                    self.publish_mouse_modes();
                    self.render_if_due(Instant::now());
                }
                continue;
            }
            if let Some(due) = self.report_due {
                match self.pty.try_wait() {
                    Ok(Some(status)) => return Some(self.show_end(ExitInfo::from_status(status))),
                    _ if Instant::now() < due => {}
                    _ => {
                        self.show_end(ExitInfo::UNKNOWN);
                        return None;
                    }
                }
            } else if self.detached {
                match self.pty.try_wait() {
                    Ok(None) => {}
                    // The host already has `exited: Some(detached)`; it must also hear the real
                    // end, not just get it back from `shutdown_and_wait` if something calls that
                    // -- otherwise the pane keeps saying "still
                    // running" about a process that is gone.
                    Ok(Some(status)) => return Some(self.show_end(ExitInfo::from_status(status))),
                    Err(_) if self.pty.is_adopted() => self.await_report(),
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
                            Err(_) if self.pty.is_adopted() => self.await_report(),
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
            self.publish_mouse_modes();
            self.render_if_due(Instant::now());
        }
    }

    fn live(&self) -> bool {
        matches!(self.phase, Phase::Live)
    }

    fn poll_timeout(&self, now: Instant) -> libc::c_int {
        let renders = self.visible && !matches!(self.phase, Phase::Detached(_));
        let render = if renders { self.clock.deadline() } else { None };
        let sync = self.sync_deadline.filter(|_| self.live()).map(|(_, due)| due);
        let reap = (self.detached && self.live() && self.report_due.is_none()).then(|| now + DETACHED_REAP_INTERVAL);
        let report = self.report_due.filter(|_| self.live());
        match [render, sync, reap, report].into_iter().flatten().min() {
            None => -1,
            Some(due) => due
                .saturating_duration_since(now)
                .as_micros()
                .div_ceil(1000)
                .min(i32::MAX as u128) as i32,
        }
    }

    fn handle_commands(&mut self) -> Next {
        loop {
            let command = match self.commands.try_recv() {
                Ok(command) => command,
                Err(TryRecvError::Empty) => return Next::Continue,
                Err(TryRecvError::Disconnected) => return self.on_shutdown(),
            };
            if let Phase::Detached(_) = self.phase {
                match command {
                    SessionCommand::Resume => self.resume(),
                    SessionCommand::Committed => return Next::Forget,
                    SessionCommand::Shutdown => return Next::ShutDown,
                    SessionCommand::ChildExited(status) => self.pty.set_exit_status(status),
                    // Applied to the rebuilt screen if the move does not happen.
                    SessionCommand::Resize(size) => self.size = size.clamped(),
                    SessionCommand::Focus(focused) => self.focused = focused,
                    SessionCommand::Visible(visible) => self.visible = visible,
                    SessionCommand::SetColors(colors) => self.colors = colors,
                    // Input waits for no one: written after a move that commits, it would reach
                    // a program the destination already answers for, so the host refuses it
                    // while the module moves and anything that still arrives is dropped. The
                    // view gestures have no screen to act on.
                    SessionCommand::Input(_)
                    | SessionCommand::Mouse(_)
                    | SessionCommand::FocusReport(_)
                    | SessionCommand::Scroll(_)
                    | SessionCommand::Select(_)
                    | SessionCommand::CopyMode(_)
                    | SessionCommand::Detach(_) => {}
                }
                continue;
            }
            match command {
                SessionCommand::Input(input) => {
                    // Typing (never a bare modifier) and a paste both snap a scrolled-back view to
                    // the bottom before the byte reaches the program -- alacritty's own
                    // `on_terminal_input_start` (`Screen::note_input`'s own doc).
                    self.screen.note_input(&input);
                    let bytes = terminal_input::encode(&input, self.screen.mode());
                    self.push_input(&bytes);
                }
                SessionCommand::Resize(size) => {
                    let size = size.clamped();
                    if !self.live() {
                        // Taken at the commit, with the PTY's size.
                        self.size = size;
                    } else if size != self.size {
                        self.size = size;
                        if let Err(e) = self.pty.resize(size) {
                            eprintln!("[terminal] TIOCSWINSZ {}x{} failed: {e}", size.cols, size.rows);
                        }
                        self.screen.resize(size);
                    }
                }
                SessionCommand::Focus(focused) => {
                    if focused != self.focused {
                        self.focused = focused;
                        self.screen.mark_dirty();
                    }
                }
                SessionCommand::Visible(visible) => {
                    if visible != self.visible {
                        self.visible = visible;
                        if visible {
                            self.screen.mark_dirty();
                        }
                    }
                }
                SessionCommand::SetColors(colors) => {
                    self.colors = colors;
                    self.screen.set_colors(colors);
                }
                SessionCommand::Scroll(req) => self.screen.scroll(req),
                SessionCommand::Select(cmd) => {
                    self.screen.select(cmd);
                    // `Finish`'s text has nowhere else to reach the host: unlike a PTY read, a
                    // command never falls through to the run loop's own `publish_events` call, so
                    // this command handler is the one place that has to make it (a copy
                    // made with nothing typed afterward would otherwise sit unpublished until the
                    // next byte of PTY output, which may never come).
                    self.publish_events(Instant::now());
                }
                SessionCommand::Mouse(m) => {
                    // No `note_input`: a report is not typing (`mouse`'s own module doc) -- it must
                    // not snap a scrolled-back view to the bottom or clear a selection.
                    let mode = self.screen.mode();
                    let bytes = match m.kind {
                        // The wheel with no mouse mode on: the program gets local alternate-scroll
                        // arrows instead of a report (the host already decided to send `Wheel` here
                        // rather than a `Scroll` command, but only the live mode -- read on this
                        // thread, never the host's possibly-stale copy -- may pick between the two).
                        mouse::MouseKind::Wheel(dir) if !mode.intersects(TermMode::MOUSE_MODE) => {
                            mouse::alternate_scroll(dir, 1, mode, mode.contains(TermMode::ALT_SCREEN))
                        }
                        _ => mouse::encode_mouse(&m, mode),
                    };
                    if let Some(bytes) = bytes {
                        self.push_input(&bytes);
                    }
                }
                SessionCommand::FocusReport(focused) => {
                    if let Some(bytes) = mouse::encode_focus(focused, self.screen.mode()) {
                        self.push_input(bytes);
                    }
                }
                SessionCommand::CopyMode(active) => self.screen.set_copy_mode(active),
                SessionCommand::Detach(reply) => {
                    if self.live() {
                        self.detach(reply);
                    }
                }
                SessionCommand::Committed => self.start_after_commit(),
                // Only a detached session has something to resume.
                SessionCommand::Resume => {}
                SessionCommand::ChildExited(status) => self.pty.set_exit_status(status),
                SessionCommand::Shutdown => return self.on_shutdown(),
            }
        }
    }

    /// The host asked to end, or went away.
    fn on_shutdown(&self) -> Next {
        match self.phase {
            // The source still holds the child; leaving it alone is the abort of the move.
            Phase::Adopted { .. } => Next::Forget,
            Phase::Live | Phase::Detached(_) => Next::ShutDown,
        }
    }

    /// Input for the program: written now, or, on an adopted session not yet committed, after
    /// what the source owed.
    fn push_input(&mut self, bytes: &[u8]) {
        if let Phase::Adopted { held, .. } = &mut self.phase {
            held.extend_from_slice(bytes);
            return;
        }
        self.outbox.push_input(bytes);
        self.flush_outbox();
    }

    /// Steps 1-5 of a move's read (module doc). Refused, the session goes on as if nothing had
    /// happened: what can be refused without reading is refused first, and output read on the way
    /// to a boundary and not yet fed is fed before the refusal is answered.
    fn detach(&mut self, reply: Sender<Result<Detached, DetachRefused>>) {
        let master = match self.pty.master_fd().try_clone_to_owned() {
            Ok(master) => master,
            Err(err) => {
                eprintln!("[terminal] pid {}: the PTY cannot move: {err}", self.pty.pid());
                let _ = reply.send(Err(DetachRefused::NoDescriptor));
                return;
            }
        };
        if self.screen.too_deep_to_move() {
            let _ = reply.send(Err(DetachRefused::TooDeep));
            return;
        }
        let mut buf = vec![0u8; READ_CHUNK];
        let refuse = |this: &mut Self, unfed: &[u8], why: DetachRefused| {
            // These bytes have left the PTY: fed now, they reach the screen (and any question in
            // them its answer) as they would have with no move asked for.
            this.feed_output(unfed);
            this.flush_outbox();
            let _ = reply.send(Err(why));
        };
        let tail = match self.settle_for_move(&mut buf) {
            Ok(tail) => tail,
            Err(why) => return refuse(self, &[], why),
        };
        let view = self.screen.view_state();
        let replay = match self.screen.read_for_move() {
            Ok(replay) => replay,
            // The bytes that ended the cut sequence can push a keyboard stack to the limit.
            Err(ReplayRefused::TooDeep) => {
                let unfed = match &tail {
                    Tail::ReadAhead(bytes) => bytes.as_slice(),
                    Tail::Raw { .. } => &[],
                };
                return refuse(self, unfed, DetachRefused::TooDeep);
            }
        };
        let outbox = self.outbox.take_unwritten();
        let pid = self.pty.pid();
        #[cfg(target_os = "linux")]
        let start = crate::pty::start_time(pid).unwrap_or(0);
        #[cfg(not(target_os = "linux"))]
        let start = 0;
        self.sync_deadline = None;
        self.phase = Phase::Detached(Kept {
            replay: replay.clone(),
            tail: tail.clone(),
            outbox: outbox.clone(),
            view: view.clone(),
        });
        let detached = Detached {
            master,
            pid,
            start,
            replay,
            tail,
            outbox,
            view,
        };
        if reply.send(Ok(detached)).is_err() {
            // Nobody took the record: the move cannot happen.
            self.resume();
        }
    }

    /// Steps 1-3: no synchronized update open, then the parser at a boundary. The byte that
    /// reaches the boundary can itself open an update (a read cut `ESC[?2026h` before its `h`);
    /// the record carries no open update, so the bytes read past it are fed and the update is let
    /// close again, for at most [`SETTLE_ROUNDS`] rounds.
    fn settle_for_move(&mut self, buf: &mut [u8]) -> Result<Tail, DetachRefused> {
        let mut round = 1;
        loop {
            self.close_sync_for_move(buf);
            match self.reach_boundary(buf)? {
                Tail::ReadAhead(unfed) if self.screen.open_update().is_some() && round < SETTLE_ROUNDS => {
                    self.feed_output(&unfed);
                    round += 1;
                }
                tail => return Ok(tail),
            }
        }
    }

    /// Output read and not fed yet, fed as a read would have fed it; its answers wait in the
    /// outbox.
    fn feed_output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.screen.feed(bytes);
        let replies = self.screen.take_replies();
        if !replies.is_empty() {
            self.outbox.push_reply(replies);
        }
        self.publish_events(Instant::now());
    }

    /// Step 1: an open synchronized update is let close, reading on as usual, for at most the
    /// time any update gets; then it is ended as the deadline would have ended it.
    fn close_sync_for_move(&mut self, buf: &mut [u8]) {
        let Some(open) = self.screen.open_update() else {
            return;
        };
        let due = match self.sync_deadline {
            Some((armed, due)) if armed == open => due,
            _ => Instant::now() + SYNC_UPDATE_TIMEOUT,
        };
        while self.screen.open_update().is_some() && !self.detached && self.wait_readable(due) {
            if let Flow::ChildGone = self.read_output(buf) {
                break;
            }
        }
        if self.screen.open_update().is_some() {
            self.screen.abort_sync();
        }
        self.sync_deadline = None;
        self.publish_events(Instant::now());
    }

    /// Steps 2-3: no more reads as usual. A sequence cut by the last read is fed on, a byte at a
    /// time, from what the PTY has or sends within the boundary wait; whatever is read past the
    /// boundary is kept unfed. Answers this feeding owes stay in the outbox, which goes along.
    fn reach_boundary(&mut self, buf: &mut [u8]) -> Result<Tail, DetachRefused> {
        let mut read_ahead = Vec::new();
        if !self.screen.at_boundary() && !self.detached {
            let deadline = Instant::now() + self.shared.boundary_wait();
            'wait: while self.wait_readable(deadline) {
                let n = match self.pty.read(buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => continue,
                    Err(_) => break,
                };
                if let Some(tap) = self.tap.as_mut() {
                    tap(&buf[..n]);
                }
                for (at, &byte) in buf[..n].iter().enumerate() {
                    self.screen.feed(&[byte]);
                    if self.screen.at_boundary() {
                        read_ahead.extend_from_slice(&buf[at + 1..n]);
                        break 'wait;
                    }
                }
            }
            let replies = self.screen.take_replies();
            if !replies.is_empty() {
                self.outbox.push_reply(replies);
            }
            self.publish_events(Instant::now());
        }
        if self.screen.at_boundary() {
            return Ok(Tail::ReadAhead(read_ahead));
        }
        let (bytes, len, calls) = self.screen.unfinished();
        if len > RAW_TAIL_MAX {
            return Err(DetachRefused::RawTooLong);
        }
        Ok(Tail::Raw {
            bytes: bytes.to_vec(),
            calls,
        })
    }

    /// Whether the master has output before `until`.
    fn wait_readable(&self, until: Instant) -> bool {
        let now = Instant::now();
        if now >= until {
            return false;
        }
        let mut fd = libc::pollfd {
            fd: self.pty.master_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (until - now).as_micros().div_ceil(1000).min(i32::MAX as u128) as libc::c_int;
        // SAFETY: one live pollfd for the duration of the call.
        let polled = unsafe { libc::poll(&mut fd, 1, ms) };
        polled > 0 || (polled < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted)
    }

    /// The move did not happen: a fresh screen from the kept record, exactly as the destination
    /// would have built it, then what was owed written, at the size the host has now.
    fn resume(&mut self) {
        let Phase::Detached(kept) = std::mem::replace(&mut self.phase, Phase::Live) else {
            return;
        };
        let (screen, replies) = rebuild(&kept.replay, &kept.tail, &kept.view, self.size, self.colors);
        self.screen = screen;
        self.outbox.push_input(&kept.outbox);
        self.outbox.push_input(&replies);
        self.take_size();
        self.flush_outbox();
        self.last_mouse = None;
        self.screen.mark_dirty();
        self.publish_events(Instant::now());
    }

    /// An adopted session's commit: the source's unwritten bytes, then the answers the rebuild
    /// produced, then keys typed since, are written; the host's size is set; reading starts.
    fn start_after_commit(&mut self) {
        let Phase::Adopted { carried, replies, held } = std::mem::replace(&mut self.phase, Phase::Live) else {
            return;
        };
        // From here on the child is this session's to end.
        self.pty.commit_adoption();
        self.outbox.push_input(&carried);
        self.outbox.push_input(&replies);
        self.outbox.push_input(&held);
        self.take_size();
        self.flush_outbox();
        self.screen.mark_dirty();
    }

    /// The size the host has now, on the PTY and the screen. An unchanged size sends the program
    /// no `SIGWINCH`.
    fn take_size(&mut self) {
        if let Err(e) = self.pty.resize(self.size) {
            eprintln!(
                "[terminal] TIOCSWINSZ {}x{} failed: {e}",
                self.size.cols, self.size.rows
            );
        }
        let now = self.screen.size();
        if now != self.size {
            self.screen.resize(self.size);
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
    /// Folds title/bell/clipboard/selection into the pending update. Rides the render clock out
    /// rather than ringing the host itself: a stream with an
    /// event on most reads (a bell in a `cat` of a binary, say) used to wake the host once per loop
    /// turn instead of once per rendered frame, and did so even while hidden, where phase 1's pump
    /// does nothing with the wake anyway. Clipboard is the one exception: it is the one event this
    /// pump already acts on, so hidden or not it rings at once rather than waiting for a clock this
    /// pane, while hidden, never polls. A copy to the primary selection is the same kind of event
    /// (phase 2), and so is a finished host selection: the user's clipboard should not
    /// wait behind a clock for a gesture that already ended.
    fn publish_events(&mut self, now: Instant) {
        let events = self.screen.take_events();
        if events == HostEvents::default() {
            return;
        }
        // Any of the three: the pump puts them on the desktop at once (bottom-terminal phases 2/3).
        let has_clipboard = events.clipboard.is_some() || events.primary.is_some() || events.selection.is_some();
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
            if events.selection.is_some() {
                update.events.selection = events.selection;
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
        if !self.live() {
            return;
        }
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
        // one publish below -- one ring, whatever the reason. Only owed pixels build a frame.
        self.frame_owed |= self.screen.take_dirty();
        let changed = (self.frame_owed || self.events_pending) && self.clock.changed(now);
        if !(changed || self.clock.is_due(now)) {
            return;
        }
        self.clock.rendered(now);
        self.events_pending = false;
        if !std::mem::take(&mut self.frame_owed) {
            // The events are in the pending update already (`publish_events`).
            self.ring();
            return;
        }
        let frame = self.screen.render(self.focused);
        let cursor = self.screen.cursor_cell();
        // Beside `cursor`, not instead of `publish_mouse_modes`'s own change-detected publish:
        // every frame carries the mode it was rendered under, so a host that only ever reads
        // `update.mouse` off a frame it took (never off a bare mode-change ring) still sees it.
        let modes = MouseModes::from_mode(self.screen.mode());
        self.last_mouse = Some(modes);
        self.shared.renders.fetch_add(1, Ordering::SeqCst);
        self.publish(|update| {
            update.frame = Some(frame);
            update.cursor = Some(cursor);
            update.mouse = Some(modes);
        });
    }

    /// The live mouse-mode summary, published (and the host rung) the moment it changes, called
    /// once per loop turn regardless of whether a frame renders this turn (bottom-terminal phase
    /// 3c). Change-detected rather than tied to a frame: a synchronized update can hold every frame
    /// back for up to [`SYNC_UPDATE_TIMEOUT`], and a host still routing a click on stale modes
    /// during that gap would start a selection in a program (`htop`) that just turned mouse
    /// reporting on.
    fn publish_mouse_modes(&mut self) {
        let modes = MouseModes::from_mode(self.screen.mode());
        if self.last_mouse != Some(modes) {
            self.last_mouse = Some(modes);
            self.publish(|update| update.mouse = Some(modes));
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

    /// An adopted child has exited and only its spawner can say how: wait for that report for at
    /// most [`EXIT_REPORT_WAIT`], hearing commands meanwhile (the run loop).
    fn await_report(&mut self) {
        self.report_due = Some(Instant::now() + EXIT_REPORT_WAIT);
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
        // pre-update snapshot for as long as one stays open.
        self.screen.abort_sync();
        self.screen
            .feed(format!("\r\n\x1b[0;2m{}\x1b[0m", exit.notice()).as_bytes());
        let frame = self.screen.render(self.focused);
        let cursor = self.screen.cursor_cell();
        let modes = MouseModes::from_mode(self.screen.mode());
        self.last_mouse = Some(modes);
        self.shared.renders.fetch_add(1, Ordering::SeqCst);
        self.publish(|update| {
            update.frame = Some(frame);
            update.cursor = Some(cursor);
            update.exited = Some(exit);
            update.mouse = Some(modes);
        });
        exit
    }

    /// The child is someone else's now, or still the source's: no hangup, no wait. The controller
    /// side this session held is closed; another holder keeps the PTY open.
    fn forget_child(self) -> Option<ExitInfo> {
        let Worker { pty, .. } = self;
        let (master, _pid) = pty.forget();
        drop(master);
        None
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

    /// A panic can replace a rendered frame that the host has not taken yet.
    /// The failure frame must not retain the previous frame's cursor, which would place preedit
    /// and the input method on a screen that is gone. Calling `contain_panic` with a render still
    /// pending exercises this case deterministically.
    #[test]
    fn a_contained_panic_does_not_leave_the_previous_frames_cursor_beside_its_own() {
        let size = PtySize {
            cols: 20,
            rows: 3,
            cell_width_px: 9,
            cell_height_px: 18,
        };
        let shared = Shared::new();
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
