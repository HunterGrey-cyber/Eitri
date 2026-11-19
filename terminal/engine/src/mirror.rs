//! A session that shows a terminal running on another machine.
//!
//! The program, its PTY and the screen that answers it live elsewhere (a headless terminal, [`crate::headless`]).
//! A mirror is a [`TerminalSession`] whose thread holds no child: it starts from a [`Snapshot`] of that screen and
//! parses the same bytes, in the same order, from the same state, so its screen is the other one's. Everything that
//! reaches it comes through a [`TermLink`], which the wire side implements; this crate knows no network.
//!
//! What follows from the other side being authoritative:
//! - **No answers here.** The program's queries are answered where the program runs, also while nobody watches;
//!   the mirror's screen generates no answers at all, so a query is never answered twice.
//! - **Keys are encoded here**, against the mirrored mode, and sent as bytes, as a terminal running `ssh` does.
//!   Input is admitted only through an open gate: the link is live and a snapshot of this connection has been
//!   applied, so keys are never encoded against a mode the program may have left while nobody watched. The gate is
//!   stamped on every command when the host sends it ([`crate::session::Commands::Mirror`]), so a key typed while
//!   the link was down is refused however late it is read: keystrokes delivered late can complete a command the
//!   person no longer meant. The host is told how many typed bytes were refused ([`MirrorReport`]). The link refuses
//!   a write while a newer state change waits to be read, which closes the window between the stamp and the read.
//!   One exception keeps a program reachable: when the other side says it cannot take a snapshot yet (`Waiting`),
//!   input opens against the mode last mirrored, since the program may need a key to get past what blocks it.
//! - **A resize is a request.** The grid changes at the `Resized` marker the other side puts in the stream at the
//!   byte where it resized, so both screens reflow at the same point of the output. One request is in flight at a
//!   time; sizes asked for meanwhile are folded into the next.
//! - **Everything the other side sends is checked before use** and a record that fails a check ends the mirror
//!   with its reason, never a panic: sizes no larger than the pane asked for, output contiguous with the snapshot,
//!   a replay applied through [`Screen::replayed_untrusted`]. [`SnapshotAssembler`] holds every bound on a
//!   snapshot, so the wire side keeps none of its own.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, PipeReader, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::term::TermMode;

use crate::clock::{RenderClock, FRAME_INTERVAL};
use crate::headless::{WaitWhy, TERM_COLUMNS_MAX, TERM_ROWS_MAX};
use crate::listener::HostEvents;
use crate::mouse::{self, MouseModes};
use crate::pty::{set_nonblocking, PtySize};
use crate::replay::screen_replay::ScreenReplay;
use crate::screen::{Screen, TerminalColors};
use crate::session::{contain_panic, DetachRefused, SessionCommand, Shared, TerminalSession, Update};
use crate::session::{RAW_TAIL_MAX, SYNC_UPDATE_TIMEOUT};
use crate::{ExitInfo, Snapshot, Tail};

/// The largest replay a snapshot may carry, assembled.
pub const SNAPSHOT_MAX: usize = 32 * 1024 * 1024;
/// The largest slice of a replay one snapshot part may carry.
pub const SNAPSHOT_PART_MAX: usize = 1024 * 1024;
/// Input waiting for the link's send window. A paste larger than this is refused past it rather than held.
pub const PENDING_INPUT_MAX: usize = 4 * 1024 * 1024;

/// The link to the terminal's own side, as the mirror's thread uses it. Every call is made on that thread and
/// must not block.
pub trait TermLink: Send + 'static {
    /// Readable when [`TermLink::take`] has items or the send window has opened again. Level-triggered: the
    /// mirror calls `take` after every wake-up.
    fn readiness(&self) -> BorrowedFd<'_>;
    /// Everything that arrived, in order. A snapshot arrives whole (the wire side assembles its parts with
    /// [`SnapshotAssembler`]). Each connection's `Link { state: Live }` comes before its first snapshot: input opens
    /// at that snapshot.
    fn take(&mut self) -> Vec<LinkItem>;
    /// Like a non-blocking write to a PTY: takes what fits in the send window. `WouldBlock` when the window is
    /// full (the mirror retries after the next wake-up); `NotConnected` while the link is down, and also while a
    /// change of the link's state is waiting in [`TermLink::take`]: input the mirror admitted on one connection is
    /// never written on a later one, whatever the order the mirror reads things in.
    fn write_input(&mut self, bytes: &[u8]) -> io::Result<usize>;
    /// The pane's new grid. The other side answers with a `Resized` marker, or a snapshot at that size.
    fn request_resize(&mut self, size: PtySize);
    /// The pane's colours, for the other side's answers to colour queries.
    fn set_colors(&mut self, colors: &TerminalColors);
}

/// What the link delivers.
#[derive(Debug)]
pub enum LinkItem {
    /// The terminal's whole state, to replace whatever the mirror shows: the first one, and every resync or
    /// reattach.
    Snapshot(Snapshot),
    /// Output, starting at `offset` in the terminal's stream.
    Output { offset: u64, bytes: Vec<u8> },
    /// The terminal took this size at `offset`.
    Resized { offset: u64, size: PtySize },
    /// The other side has written this many bytes of this mirror's input to the program.
    InputWritten(u64),
    /// The program ended.
    Exited(ExitInfo),
    /// No snapshot can be taken yet, and why.
    Waiting(WaitWhy),
    /// The terminal is no longer this mirror's to show.
    Ended(MirrorEnd),
    /// The state of the link itself, and input bytes the other side never wrote before the link last dropped.
    Link { state: LinkState, lost_input_bytes: u64 },
}

/// The state of the link to the terminal's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Live,
    /// Lost; `since_ms` on the link's own clock.
    Disconnected {
        since_ms: u64,
    },
    Reconnecting {
        attempt: u32,
        next_in_ms: u64,
    },
}

/// Why a mirror stopped showing its terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorEnd {
    /// Another client attached to the terminal.
    Replaced,
    /// The terminal was closed.
    Closed,
    /// The terminal's server stopped it.
    Stopped,
    /// The terminal is gone; the words say why. They are the wire side's own sentence, never text the server sent.
    Gone(String),
    /// The other side sent something this mirror cannot use.
    Refused(FrameRefused),
}

/// What was wrong with a record from the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRefused {
    SizeOutOfRange,
    SizeNotRequested,
    SnapshotTooLarge,
    SnapshotIncomplete,
    PartTooLarge,
    PartOutOfOrder,
    TailTooLong,
    TailCallsImpossible,
    BadLastInput,
    OffsetGap {
        expected: u64,
        got: u64,
    },
    OffsetOverflow,
    OutputBeforeSnapshot,
    /// A replay that ends inside a sequence, or whose last character is a C0 control.
    ReplayUnfinished,
}

impl fmt::Display for FrameRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameRefused::SizeOutOfRange => f.write_str("a terminal size out of range"),
            FrameRefused::SizeNotRequested => f.write_str("a terminal size this window did not ask for"),
            FrameRefused::SnapshotTooLarge => f.write_str("a screen record larger than allowed"),
            FrameRefused::SnapshotIncomplete => f.write_str("an incomplete screen record"),
            FrameRefused::PartTooLarge => f.write_str("a screen record part larger than allowed"),
            FrameRefused::PartOutOfOrder => f.write_str("screen record parts out of order"),
            FrameRefused::TailTooLong => f.write_str("an unfinished sequence longer than allowed"),
            FrameRefused::TailCallsImpossible => f.write_str("an unfinished sequence that does not add up"),
            FrameRefused::BadLastInput => f.write_str("a character that is not one"),
            FrameRefused::OffsetGap { expected, got } => {
                write!(f, "output at byte {got} where byte {expected} was next")
            }
            FrameRefused::OffsetOverflow => f.write_str("output past the end of any stream"),
            FrameRefused::OutputBeforeSnapshot => f.write_str("output before the screen it belongs to"),
            FrameRefused::ReplayUnfinished => f.write_str("a screen record that ends inside a sequence"),
        }
    }
}

/// How a snapshot part's tail is described on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireTail {
    None,
    ReadAhead,
    Raw,
}

/// One part of a snapshot as the wire carries it, before any of it is trusted.
#[derive(Debug, Clone)]
pub struct SnapshotPartWire {
    pub offset: u64,
    pub columns: u32,
    pub rows: u32,
    pub total_bytes: u64,
    pub replay: Vec<u8>,
    pub last: bool,
    // Read from the last part only.
    pub last_input: Option<u32>,
    pub tail_kind: WireTail,
    pub tail: Vec<u8>,
    pub tail_calls: u32,
    pub history_at_cap: bool,
    pub history_trimmed: u32,
    pub output_while_detached: bool,
    pub input_written: u64,
}

/// Puts a snapshot together from its parts, checking every bound before it holds anything: sizes in range, the
/// announced length within [`SNAPSHOT_MAX`] and the same in every part, each part within [`SNAPSHOT_PART_MAX`],
/// the parts adding up exactly, the tail within [`RAW_TAIL_MAX`]. Its buffer grows only with what arrives, never
/// to an announced length.
#[derive(Default)]
pub struct SnapshotAssembler {
    header: Option<(u64, u32, u32, u64)>,
    bytes: Vec<u8>,
}

impl SnapshotAssembler {
    pub fn new() -> Self {
        SnapshotAssembler::default()
    }

    /// Whether a snapshot has begun and not yet completed.
    pub fn in_progress(&self) -> bool {
        self.header.is_some()
    }

    /// Takes one part. `Ok(Some)` with the last; after an error the assembler is empty again.
    pub fn part(&mut self, part: SnapshotPartWire) -> Result<Option<Snapshot>, FrameRefused> {
        let result = self.take_part(part);
        if !matches!(result, Ok(None)) {
            *self = SnapshotAssembler::default();
        }
        result
    }

    fn take_part(&mut self, part: SnapshotPartWire) -> Result<Option<Snapshot>, FrameRefused> {
        let header = (part.offset, part.columns, part.rows, part.total_bytes);
        match self.header {
            None => {
                let columns_ok = (1..=u32::from(TERM_COLUMNS_MAX)).contains(&part.columns);
                let rows_ok = (1..=u32::from(TERM_ROWS_MAX)).contains(&part.rows);
                if !columns_ok || !rows_ok {
                    return Err(FrameRefused::SizeOutOfRange);
                }
                if part.total_bytes > SNAPSHOT_MAX as u64 {
                    return Err(FrameRefused::SnapshotTooLarge);
                }
                self.header = Some(header);
            }
            Some(started) if started != header => return Err(FrameRefused::PartOutOfOrder),
            Some(_) => {}
        }
        if part.replay.len() > SNAPSHOT_PART_MAX {
            return Err(FrameRefused::PartTooLarge);
        }
        if (self.bytes.len() + part.replay.len()) as u64 > part.total_bytes {
            return Err(FrameRefused::SnapshotTooLarge);
        }
        self.bytes.reserve_exact(part.replay.len());
        self.bytes.extend_from_slice(&part.replay);
        if !part.last {
            return Ok(None);
        }
        if self.bytes.len() as u64 != part.total_bytes {
            return Err(FrameRefused::SnapshotIncomplete);
        }
        if part.tail.len() > RAW_TAIL_MAX {
            return Err(FrameRefused::TailTooLong);
        }
        let tail = match part.tail_kind {
            WireTail::None if part.tail.is_empty() && part.tail_calls == 0 => Tail::ReadAhead(Vec::new()),
            WireTail::ReadAhead if part.tail_calls == 0 => Tail::ReadAhead(part.tail),
            WireTail::Raw if (part.tail_calls as usize) <= part.tail.len() => Tail::Raw {
                bytes: part.tail,
                calls: part.tail_calls as usize,
            },
            _ => return Err(FrameRefused::TailCallsImpossible),
        };
        let last_input = match part.last_input {
            None => None,
            Some(c) => Some(
                char::from_u32(c)
                    .filter(|&c| c >= ' ')
                    .ok_or(FrameRefused::BadLastInput)?,
            ),
        };
        let replay = ScreenReplay {
            columns: part.columns as u16,
            rows: part.rows as u16,
            bytes: std::mem::take(&mut self.bytes),
            last_input,
        };
        Ok(Some(Snapshot {
            offset: part.offset,
            replay,
            tail,
            history_at_cap: part.history_at_cap,
            history_trimmed: part.history_trimmed,
            output_while_detached: part.output_while_detached,
            input_written: part.input_written,
        }))
    }
}

/// What a snapshot said about the history it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryNote {
    /// Oldest history lines left out so the snapshot fit its byte cap.
    pub trimmed: u32,
    /// The history was full and output arrived while nobody watched: some of it is older than the history keeps.
    pub older_output_gone: bool,
}

/// What a mirror reports to its host beside the frame, in [`Update::mirror`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MirrorReport {
    /// The link's latest state.
    pub link: Option<LinkState>,
    /// The latest wait transition, including a snapshot clearing it. `Some(Some(why))` gives the wait reason.
    pub waiting: Option<Option<WaitWhy>>,
    /// A snapshot was applied (the first, a resync or a reattach); what it said about its history.
    pub synced: Option<HistoryNote>,
    /// Typed or pasted bytes refused: the gate was closed when they were sent (the link down, or no snapshot of
    /// this connection yet), the link refused them, the mirror stopped with them unsent, or one paste was larger
    /// than [`PENDING_INPUT_MAX`]. Focus and mouse reports are dropped uncounted.
    pub refused_input_bytes: u64,
    /// Input bytes sent before the link last dropped that the other side never wrote.
    pub lost_input_bytes: u64,
    /// The mirror has stopped, and why.
    pub ended: Option<MirrorEnd>,
}

impl MirrorReport {
    fn merge(&mut self, newer: MirrorReport) {
        if newer.link.is_some() {
            self.link = newer.link;
        }
        if newer.waiting.is_some() {
            self.waiting = newer.waiting;
        }
        if newer.synced.is_some() {
            self.synced = newer.synced;
        }
        self.refused_input_bytes += newer.refused_input_bytes;
        self.lost_input_bytes += newer.lost_input_bytes;
        if newer.ended.is_some() {
            self.ended = newer.ended;
        }
    }
}

/// How to start a mirror.
pub struct MirrorConfig {
    /// The pane's grid. Shown blank until the first snapshot, and asked of the other side at once.
    pub size: PtySize,
    pub colors: TerminalColors,
    pub focused: bool,
}

impl TerminalSession {
    /// A session over a terminal on another machine, reached through `link`. It owns no child: `pid` panics and
    /// [`TerminalSession::local_pid`] is `None`. Commands and updates work as for a local session, except that a
    /// move is refused ([`DetachRefused::ServerTerminal`]) and [`Update::mirror`] carries the link's news.
    pub fn mirror(
        link: Box<dyn TermLink>,
        config: MirrorConfig,
        wake: impl Fn() + Send + 'static,
    ) -> io::Result<TerminalSession> {
        let (bell_rx, bell_tx) = std::io::pipe()?;
        set_nonblocking(bell_rx.as_fd())?;
        set_nonblocking(bell_tx.as_fd())?;
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::new());
        let thread_shared = shared.clone();
        let MirrorConfig { size, colors, focused } = config;
        let size = clamp(size);
        let thread = std::thread::Builder::new()
            .name("terminal-mirror".into())
            .spawn(move || {
                let run = AssertUnwindSafe(|| {
                    let worker = MirrorWorker {
                        link,
                        screen: Screen::mirror(size, colors),
                        synced: false,
                        expected: 0,
                        current: size,
                        in_flight: None,
                        earlier: Vec::new(),
                        snapshot_too_large: false,
                        wanted: size,
                        pending: VecDeque::new(),
                        pending_typed: VecDeque::new(),
                        link_state: LinkState::Connecting,
                        epoch: 0,
                        open: false,
                        colors,
                        focused,
                        visible: true,
                        commands: receiver,
                        doorbell: bell_rx,
                        shared: thread_shared.clone(),
                        wake: &wake,
                        clock: RenderClock::new(FRAME_INTERVAL),
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
        Ok(TerminalSession::from_mirror(shared, commands, bell_tx, thread))
    }
}

/// The pane's grid within what the other side accepts.
fn clamp(size: PtySize) -> PtySize {
    PtySize {
        cols: size.cols.clamp(1, TERM_COLUMNS_MAX),
        rows: size.rows.clamp(1, TERM_ROWS_MAX),
        ..size
    }
}

/// Distinct grids kept across ordinary reconnects until the next snapshot.
const EARLIER_REQUESTS_KEPT: usize = 16;

struct EarlierSize {
    size: PtySize,
    /// The connection on which this was sent during a snapshot wait, so its marker can still arrive.
    pending_epoch: Option<u64>,
}

/// Why the mirror's loop ends.
enum Stop {
    Exited(ExitInfo),
    Ended,
    Shutdown,
}

struct MirrorWorker<'w> {
    link: Box<dyn TermLink>,
    screen: Screen,
    /// A snapshot has been applied: output may follow.
    synced: bool,
    /// The stream offset the next output must start at.
    expected: u64,
    /// The grid the screen has now.
    current: PtySize,
    /// The size asked of the other side on this connection and not yet answered.
    in_flight: Option<PtySize>,
    /// Superseded sizes: the other side may have applied them before the latest request.
    earlier: Vec<EarlierSize>,
    snapshot_too_large: bool,
    /// The size the pane wants; asked for once nothing is in flight.
    wanted: PtySize,
    /// Encoded input the link's send window has not taken yet.
    pending: VecDeque<u8>,
    /// One flag per pending byte. Reports share the send order, but only typed or pasted bytes are counted.
    pending_typed: VecDeque<bool>,
    link_state: LinkState,
    /// Counts connections: every change of the link's state starts a new one.
    epoch: u64,
    /// Input is admitted: the link is live and a snapshot of this connection has been applied.
    open: bool,
    colors: TerminalColors,
    focused: bool,
    visible: bool,
    commands: Receiver<(u64, SessionCommand)>,
    doorbell: PipeReader,
    shared: Arc<Shared>,
    wake: &'w dyn Fn(),
    clock: RenderClock,
    sync_deadline: Option<(u64, Instant)>,
    events_pending: bool,
    frame_owed: bool,
    last_mouse: Option<MouseModes>,
}

impl MirrorWorker<'_> {
    fn run(mut self) -> Option<ExitInfo> {
        self.link.set_colors(&self.colors);
        self.ask_size();
        self.publish_gate();
        self.render_if_due(Instant::now());
        let stop = loop {
            let mut fds = [
                libc::pollfd {
                    fd: self.link.readiness().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.doorbell.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = self.poll_timeout(Instant::now());
            // SAFETY: two initialized pollfds remain live for the call.
            let polled = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            self.shared.thread_wakeups.fetch_add(1, Ordering::SeqCst);
            if polled < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break self.end(MirrorEnd::Gone(
                    "the terminal's view failed to wait for its link".into(),
                ));
            }
            // The link first, so a command is judged against what the link has said by now.
            if let Some(stop) = self.handle_link() {
                break stop;
            }
            if fds[1].revents != 0 {
                let mut drain = [0u8; 64];
                while matches!(self.doorbell.read(&mut drain), Ok(n) if n > 0) {}
                if let Some(stop) = self.handle_commands() {
                    break stop;
                }
            }
            self.flush_pending();
            self.track_sync(Instant::now());
            self.publish_mouse_modes();
            self.render_if_due(Instant::now());
        };
        // What was still waiting for the send window never reaches the program now.
        self.drop_pending();
        self.open = false;
        self.publish_gate();
        match stop {
            Stop::Exited(exit) => Some(exit),
            Stop::Ended | Stop::Shutdown => None,
        }
    }

    fn poll_timeout(&self, now: Instant) -> libc::c_int {
        let render = if self.visible { self.clock.deadline() } else { None };
        let sync = self.sync_deadline.map(|(_, due)| due);
        match [render, sync].into_iter().flatten().min() {
            None => -1,
            Some(due) => due
                .saturating_duration_since(now)
                .as_micros()
                .div_ceil(1000)
                .min(i32::MAX as u128) as i32,
        }
    }

    /// The gate value commands are stamped with: the connection, and whether input is admitted on it.
    fn gate(&self) -> u64 {
        self.epoch << 1 | u64::from(self.open)
    }

    fn publish_gate(&self) {
        self.shared.mirror_gate.store(self.gate(), Ordering::SeqCst);
    }

    fn handle_commands(&mut self) -> Option<Stop> {
        loop {
            let (gate, command) = match self.commands.try_recv() {
                Ok(next) => next,
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => return Some(Stop::Shutdown),
            };
            // Input is admitted only if the gate was open when it was sent and is still the same one.
            let admitted = self.open && gate == self.gate();
            match command {
                SessionCommand::Input(input) => {
                    self.screen.note_input(&input);
                    let bytes = terminal_input::encode(&input, self.screen.mode());
                    if admitted {
                        self.send_input(&bytes, true);
                    } else {
                        self.refuse_input(bytes.len());
                    }
                }
                SessionCommand::Mouse(m) => {
                    let mode = self.screen.mode();
                    let bytes = match m.kind {
                        mouse::MouseKind::Wheel(dir) if !mode.intersects(TermMode::MOUSE_MODE) => {
                            mouse::alternate_scroll(dir, 1, mode, mode.contains(TermMode::ALT_SCREEN))
                        }
                        _ => mouse::encode_mouse(&m, mode),
                    };
                    // A report is not typing: dropped uncounted when it cannot go.
                    if let (Some(bytes), true) = (bytes, admitted) {
                        self.send_input(&bytes, false);
                    }
                }
                SessionCommand::FocusReport(focused) => {
                    if let (Some(bytes), true) = (mouse::encode_focus(focused, self.screen.mode()), admitted) {
                        self.send_input(bytes, false);
                    }
                }
                SessionCommand::Resize(size) => {
                    self.wanted = clamp(size.clamped());
                    self.ask_size();
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
                    self.link.set_colors(&colors);
                }
                SessionCommand::Scroll(req) => self.screen.scroll(req),
                SessionCommand::Select(cmd) => {
                    self.screen.select(cmd);
                    self.publish_events(Instant::now());
                }
                SessionCommand::CopyMode(active) => self.screen.set_copy_mode(active),
                SessionCommand::Detach(reply) => {
                    let _ = reply.send(Err(DetachRefused::ServerTerminal));
                }
                // A mirror is never the destination of a move and has no child of its own.
                SessionCommand::Resume | SessionCommand::Committed | SessionCommand::ChildExited(_) => {}
                SessionCommand::Shutdown => return Some(Stop::Shutdown),
            }
        }
    }

    /// Normally waits for the in-flight resize to be answered. While an oversized snapshot is owed, resize
    /// markers are suppressed, so a different size must go through without waiting for one.
    fn ask_size(&mut self) {
        if let Some(asked) = self.in_flight {
            if !self.snapshot_too_large || self.wanted == asked {
                return;
            }
            self.remember_size(asked);
        } else if self.synced && self.wanted == self.current {
            return;
        }
        self.in_flight = Some(self.wanted);
        self.link.request_resize(self.wanted);
    }

    fn remember_size(&mut self, asked: PtySize) {
        self.earlier
            .retain(|earlier| earlier.size.cols != asked.cols || earlier.size.rows != asked.rows);
        self.earlier.push(EarlierSize {
            size: asked,
            pending_epoch: self.snapshot_too_large.then_some(self.epoch),
        });
        // A snapshot taken during a drag can arrive at any of the grids sent while markers were suppressed.
        if !self.snapshot_too_large {
            let excess = self.earlier.len().saturating_sub(EARLIER_REQUESTS_KEPT);
            self.earlier.drain(..excess);
        }
    }

    /// The size a snapshot or marker carries, if this mirror asked for it (or already has it): never a grid the
    /// other side picked on its own.
    fn accept_size(&mut self, columns: u16, rows: u16) -> Result<PtySize, FrameRefused> {
        let grid = |size: &PtySize| size.cols == columns && size.rows == rows;
        if let Some(asked) = self.in_flight.filter(|asked| grid(asked)) {
            self.in_flight = None;
            if !grid(&self.current) && !self.earlier.iter().any(|earlier| grid(&earlier.size)) {
                self.earlier.clear();
            }
            return Ok(asked);
        }
        if grid(&self.current) {
            return Ok(self.current);
        }
        self.earlier
            .iter()
            .rev()
            .map(|earlier| &earlier.size)
            .find(|size| grid(size))
            .copied()
            .ok_or(FrameRefused::SizeNotRequested)
    }

    fn handle_link(&mut self) -> Option<Stop> {
        let mut items = self.link.take().into_iter();
        while let Some(item) = items.next() {
            match self.handle_item(item) {
                Ok(None) => {}
                Ok(stop) => return stop,
                Err(why) => {
                    // The pane must release its claim even if an earlier record in this batch was invalid.
                    if items.any(|item| matches!(item, LinkItem::Ended(MirrorEnd::Replaced))) {
                        return Some(self.end(MirrorEnd::Replaced));
                    }
                    return Some(self.refuse(why));
                }
            }
        }
        None
    }

    fn handle_item(&mut self, item: LinkItem) -> Result<Option<Stop>, FrameRefused> {
        match item {
            LinkItem::Snapshot(snapshot) => self.apply_snapshot(snapshot)?,
            LinkItem::Output { offset, bytes } => {
                self.advance(offset, bytes.len())?;
                self.screen.feed(&bytes);
                self.publish_events(Instant::now());
            }
            LinkItem::Resized { offset, size } => {
                self.advance(offset, 0)?;
                let size = self.accept_size(size.cols, size.rows)?;
                self.current = size;
                self.screen.resize(size);
                self.ask_size();
            }
            LinkItem::InputWritten(_) => {}
            LinkItem::Exited(exit) => {
                self.show_end(exit);
                // A program that left the terminal still runs; its real end may follow.
                if !exit.detached {
                    return Ok(Some(Stop::Exited(exit)));
                }
            }
            LinkItem::Waiting(why) => {
                if why == WaitWhy::SnapshotTooLarge {
                    self.snapshot_too_large = true;
                    self.ask_size();
                }
                // The program may need a key to finish what keeps the snapshot from being taken (an open sequence, a
                // deep keyboard stack), so input opens without one: keys are encoded against the mode last seen.
                if self.link_state == LinkState::Live && !self.open {
                    self.open = true;
                    self.publish_gate();
                }
                self.report(MirrorReport {
                    waiting: Some(Some(why)),
                    ..MirrorReport::default()
                });
            }
            LinkItem::Ended(MirrorEnd::Refused(why)) => return Err(why),
            LinkItem::Ended(end) => return Ok(Some(self.end(end))),
            LinkItem::Link {
                state,
                lost_input_bytes,
            } => self.link_changed(state, lost_input_bytes),
        }
        Ok(None)
    }

    /// Checks that a record continues the stream where the last one ended, and moves the end past it.
    fn advance(&mut self, offset: u64, len: usize) -> Result<(), FrameRefused> {
        if !self.synced {
            return Err(FrameRefused::OutputBeforeSnapshot);
        }
        if offset != self.expected {
            return Err(FrameRefused::OffsetGap {
                expected: self.expected,
                got: offset,
            });
        }
        self.expected = offset.checked_add(len as u64).ok_or(FrameRefused::OffsetOverflow)?;
        Ok(())
    }

    fn link_changed(&mut self, state: LinkState, lost_input_bytes: u64) {
        let was = self.link_state;
        self.link_state = state;
        if state != was {
            // A new connection: input sent on the old one is never written now, and nothing is admitted until a
            // snapshot of the new one shows the mode keys are encoded against.
            self.epoch += 1;
            self.open = false;
            self.snapshot_too_large = false;
            self.drop_pending();
            if let Some(asked) = self.in_flight.take() {
                self.remember_size(asked);
            }
            if state == LinkState::Live {
                // Asked again on every connection, so the other side knows the size and colours however the last
                // one ended.
                self.link.set_colors(&self.colors);
                self.in_flight = Some(self.wanted);
                self.link.request_resize(self.wanted);
            }
            self.publish_gate();
        }
        self.report(MirrorReport {
            link: Some(state),
            lost_input_bytes,
            ..MirrorReport::default()
        });
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) -> Result<(), FrameRefused> {
        let size = self.accept_size(snapshot.replay.columns, snapshot.replay.rows)?;
        let mut screen =
            Screen::replayed_untrusted(&snapshot.replay, size, self.colors).ok_or(FrameRefused::ReplayUnfinished)?;
        let expected = match &snapshot.tail {
            // Read after the snapshot's state and not yet parsed there: part of the stream from `offset` on.
            Tail::ReadAhead(bytes) => {
                let end = snapshot
                    .offset
                    .checked_add(bytes.len() as u64)
                    .ok_or(FrameRefused::OffsetOverflow)?;
                screen.feed(bytes);
                end
            }
            // Already counted in `offset`: the other side's parser stands inside this sequence.
            Tail::Raw { bytes, calls } => {
                screen.feed_unfinished(bytes, *calls);
                snapshot.offset
            }
        };
        self.screen = screen;
        self.current = size;
        // A snapshot retires earlier connections' sizes. A resync can happen while requests sent on this
        // connection are still in transit, so their markers can follow several snapshots.
        self.earlier.retain(|earlier| earlier.pending_epoch == Some(self.epoch));
        self.snapshot_too_large = false;
        self.expected = expected;
        self.synced = true;
        self.sync_deadline = None;
        if self.link_state == LinkState::Live && !self.open {
            self.open = true;
            self.publish_gate();
        }
        self.report(MirrorReport {
            waiting: Some(None),
            synced: Some(HistoryNote {
                trimmed: snapshot.history_trimmed,
                older_output_gone: snapshot.history_at_cap && snapshot.output_while_detached,
            }),
            ..MirrorReport::default()
        });
        // The replay's title (or none) goes to the host like a program's.
        self.publish_events(Instant::now());
        self.ask_size();
        Ok(())
    }

    fn refuse(&mut self, why: FrameRefused) -> Stop {
        eprintln!("[terminal] the server's terminal record was refused: {why}");
        self.end(MirrorEnd::Refused(why))
    }

    /// The terminal is no longer this mirror's: the screen as last parsed stays on the host's pane.
    fn end(&mut self, end: MirrorEnd) -> Stop {
        self.screen.abort_sync();
        self.frame_owed |= self.screen.take_dirty();
        let frame = self.frame_owed.then(|| self.screen.render(self.focused));
        let cursor = self.screen.cursor_cell();
        self.report(MirrorReport {
            ended: Some(end),
            ..MirrorReport::default()
        });
        if let Some(frame) = frame {
            self.shared.renders.fetch_add(1, Ordering::SeqCst);
            self.publish(|update| {
                update.frame = Some(frame);
                update.cursor = Some(cursor);
            });
        }
        Stop::Ended
    }

    /// Admit a command only if all its bytes fit in the pending buffer. The link may take a prefix before it drops.
    fn send_input(&mut self, bytes: &[u8], typed: bool) {
        if bytes.is_empty() {
            return;
        }
        if self.pending.len() + bytes.len() > PENDING_INPUT_MAX {
            if typed {
                self.refuse_input(bytes.len());
            }
            return;
        }
        self.pending.extend(bytes);
        self.pending_typed.extend(std::iter::repeat_n(typed, bytes.len()));
        self.flush_pending();
    }

    fn drop_pending(&mut self) {
        self.pending = VecDeque::new();
        let refused = std::mem::take(&mut self.pending_typed)
            .into_iter()
            .filter(|&typed| typed)
            .count();
        self.refuse_input(refused);
    }

    fn refuse_input(&self, bytes: usize) {
        if bytes > 0 {
            self.report(MirrorReport {
                refused_input_bytes: bytes as u64,
                ..MirrorReport::default()
            });
        }
    }

    fn flush_pending(&mut self) {
        while !self.pending.is_empty() {
            let (front, _) = self.pending.as_slices();
            let len = front.len();
            match self.link.write_input(front) {
                Ok(0) => break,
                Ok(n) => {
                    let n = n.min(len);
                    self.pending.drain(..n);
                    self.pending_typed.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => self.drop_pending(),
            }
        }
    }

    /// Like the local session's: title, bell and clipboard ride the render clock (clipboard rings at once).
    fn publish_events(&mut self, now: Instant) {
        let events = self.screen.take_events();
        if events == HostEvents::default() {
            return;
        }
        let has_clipboard = events.clipboard.is_some() || events.primary.is_some() || events.selection.is_some();
        self.shared.apply_pending(|update| update.events.merge(events));
        if !self.visible {
            if has_clipboard {
                self.shared.ring(self.wake);
            }
            return;
        }
        self.events_pending = true;
        self.clock.changed(now);
    }

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
        self.frame_owed |= self.screen.take_dirty();
        let changed = (self.frame_owed || self.events_pending) && self.clock.changed(now);
        if !(changed || self.clock.is_due(now)) {
            return;
        }
        self.clock.rendered(now);
        self.events_pending = false;
        if !std::mem::take(&mut self.frame_owed) {
            self.shared.ring(self.wake);
            return;
        }
        let frame = self.screen.render(self.focused);
        let cursor = self.screen.cursor_cell();
        let modes = MouseModes::from_mode(self.screen.mode());
        self.last_mouse = Some(modes);
        self.shared.renders.fetch_add(1, Ordering::SeqCst);
        self.publish(|update| {
            update.frame = Some(frame);
            update.cursor = Some(cursor);
            update.mouse = Some(modes);
        });
    }

    fn publish_mouse_modes(&mut self) {
        let modes = MouseModes::from_mode(self.screen.mode());
        if self.last_mouse != Some(modes) {
            self.last_mouse = Some(modes);
            self.publish(|update| update.mouse = Some(modes));
        }
    }

    /// Keeps the screen, says how the program ended under it, and tells the host, as a local session does.
    fn show_end(&mut self, exit: ExitInfo) {
        self.screen.abort_sync();
        self.screen
            .feed(format!("\r\n\x1b[0;2m{}\x1b[0m", exit.notice()).as_bytes());
        let _ = self.screen.take_events();
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
    }

    fn report(&self, report: MirrorReport) {
        self.publish(|update| update.mirror.get_or_insert_with(MirrorReport::default).merge(report));
    }

    fn publish(&self, apply: impl FnOnce(&mut Update)) {
        self.shared.publish(self.wake, apply);
    }
}

#[cfg(test)]
mod tests;
