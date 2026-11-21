//! The relay behind `eitri host term`: a shell on the project's server, shown on this process's terminal as `ssh` shows
//! one. The Terminal service and the link to it are the window's ([`TerminalLink`]: open, attach, the snapshot's
//! parts put together under their bounds, input offsets, the reconnect schedule); this is what stands where the
//! window's mirror would: the person's own terminal.
//!
//! - **Raw bytes both ways.** What the server's shell prints goes to the person's terminal as a terminal's output
//!   does (that is the point of a terminal); what the person types goes to the shell as it came. Every line this
//!   relay writes itself is a single line of escaped text (`terminal_text`), whatever the words are made of.
//! - **The screen first.** A snapshot's replay and the unfinished tail after it are written before any live output.
//!   A replay is made for a terminal in its first state, so every snapshot comes after a full reset (`RIS`), and every
//!   one after the first also after an erase of the scrollback: the replay brings its own history, which would
//!   otherwise be in the scrollback twice, and modes the last screen set (mouse reporting, a scroll region, keyboard
//!   flags) do not outlive it.
//! - **The stream is checked.** Output must continue the stream where the last record ended; a record that does not
//!   ends the relay with a line saying so, the shell left running.
//! - **The link may be down for [`REATTACH_LIMIT`].** Past it the relay exits 255 and the shell keeps running on the
//!   server (`eitri host list` shows it). Input typed while the link is down is dropped, as the mirror drops it: keys
//!   delivered late could complete a command the person no longer meant; how many is said when the relay ends (a
//!   line drawn over a screen would leave the person's terminal disagreeing with the server's). Input typed before
//!   the first screen is kept (within [`INPUT_PENDING_MAX`]) and sent once it has been drawn: it is for a shell that
//!   has not been seen yet.
//! - **Ends:** the shell's exit ends the relay with its code (`128 + n` for a signal); a hangup or termination signal
//!   closes the server's terminal and ends it, and so does end of input, once the server has written what was typed
//!   (or [`INPUT_DRAIN`] has passed: the close would otherwise overtake the keys on their way). The close is best
//!   effort: over the link's own connection, within [`CLOSE_BUDGET`], since the process that ran this one (Neovim,
//!   wiping its buffer) sends `SIGTERM` to the whole process group two seconds after the hangup.
//! - **Modes given back.** When the relay ends with the server's program still there (it never got to reset its
//!   modes), a terminal on the output is taken out of them ([`MODES_RESET`]) before the relay's last lines.
//!
//! The person's terminal is put in raw mode for as long as [`run`] goes on and given back on every way out of it,
//! a panic included (a drop guard).

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use agent::remote_client::terminal_transport::wire;
use eitri_terminal::mirror::{FrameRefused, LinkItem, LinkState, MirrorEnd, TermLink};
use eitri_terminal::{ExitInfo, PtySize, Tail};

use crate::remote_project::terminal_link::{OpenAnswer, TerminalLink};
use crate::remote_project::terminals::CLOSE_WARNING_NOTICE;
use crate::remote_sync::terminal_text;

/// How long the link may stay down, from its loss (or from the start, before the first screen), before the relay
/// gives up and leaves the shell running.
pub const REATTACH_LIMIT: Duration = Duration::from_secs(120);

/// What the first snapshot is drawn over: `CAN` ends any sequence the terminal stands inside, then `RIS`.
const FIRST_RESET: &[u8] = b"\x18\x1bc";
/// What every later snapshot is drawn over: the same, then the scrollback erased (`ED 3`).
const LATER_RESET: &[u8] = b"\x18\x1bc\x1b[3J";
/// The longest a close of the server's terminal may take, an open still waiting for its answer included.
pub const CLOSE_BUDGET: Duration = Duration::from_millis(1500);
/// After end of input, the longest the relay waits for what was typed to be written on the server (and, before the
/// first screen, for that screen) before it closes the shell anyway.
pub const INPUT_DRAIN: Duration = Duration::from_secs(2);
/// What a terminal is given back on when the relay ends with the server's program still running: a `CAN` for any
/// sequence left open, keyboard flags popped on the screen it is on, the main screen back, a soft reset (scroll
/// region, origin, wrap, attributes, a shown cursor), the main screen's keyboard flags popped, then mouse reporting,
/// focus events and bracketed paste off. The first snapshot's full reset emptied the keyboard stacks, so popping them
/// takes nothing the person's own programs pushed.
pub const MODES_RESET: &[u8] = b"\x18\x1b[<99u\x1b[?1049l\x1b[!p\x1b[<99u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\
\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l";
/// The most one read of the person's input takes, and the most that waits for the link's send window.
const INPUT_CHUNK: usize = 16 * 1024;
pub const INPUT_PENDING_MAX: usize = 64 * 1024;
/// The longest the loop sleeps without anything to do: the time limit and a closed send window are looked at on it.
const TICK_MS: i32 = 250;

/// Closes the server's terminal with this id through a connection of its own, when the link has none to close it on;
/// the notice to show when the server could not confirm the shell gone. Run on a thread of its own and waited for
/// only within [`CLOSE_BUDGET`].
pub type Close = Box<dyn FnMut(&str) -> std::io::Result<Option<String>> + Send>;

/// The server's side of one relay.
pub struct Session {
    /// The host as the person typed it, for the lines this relay writes.
    pub host: String,
    pub link: TerminalLink,
    /// The close for a link without a connection ([`TerminalLink::close_terminal`] is tried first).
    pub close: Close,
    pub reattach_limit: Duration,
    /// The person's terminal size now; asked again at every `SIGWINCH`.
    pub size: Box<dyn FnMut() -> Option<PtySize> + Send>,
}

/// The person's side of one relay.
pub struct Terminal {
    /// The descriptor keys are read from; put in raw mode when it is a terminal. Not owned: the caller closes it.
    pub input: RawFd,
    pub output: Box<dyn Write + Send>,
    /// Whether `output` is a terminal, which [`MODES_RESET`] is written to at an end that leaves the program running.
    pub output_is_terminal: bool,
    /// Signal numbers as bytes ([`super::signals::watch`]): `SIGWINCH` resizes, `SIGHUP`, `SIGTERM` and `SIGINT` end.
    pub signals: OwnedFd,
}

/// A terminal in raw mode until this is dropped (which also runs while a panic unwinds).
pub struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}

impl RawMode {
    /// Raw mode on `fd`; `None` when it is not a terminal (a pipe has nothing to change).
    pub fn enter(fd: RawFd) -> std::io::Result<Option<RawMode>> {
        // SAFETY: isatty only looks at a descriptor number.
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None);
        }
        // SAFETY: a zeroed termios is a valid out-parameter; tcgetattr fills it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: tcgetattr writes the terminal's attributes into `saved`.
        if unsafe { libc::tcgetattr(fd, &mut saved) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut raw = saved;
        // SAFETY: cfmakeraw edits the structure it is given.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: tcsetattr reads the structure and changes the terminal's mode.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(RawMode { fd, saved }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: tcsetattr reads the structure saved at `enter`; a terminal that is gone makes this fail, harmlessly.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// The size of the terminal `fd` is, as the server's terminal takes it (within its bounds); `None` when `fd` is not a
/// terminal or reports no size.
pub fn local_size(fd: RawFd) -> Option<PtySize> {
    // SAFETY: a zeroed winsize is a valid out-parameter; the ioctl fills it.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize through the pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut size) } < 0 || size.ws_col == 0 || size.ws_row == 0 {
        return None;
    }
    Some(PtySize {
        cols: size.ws_col.min(wire::TERM_COLUMNS_MAX as u16),
        rows: size.ws_row.min(wire::TERM_ROWS_MAX as u16),
        cell_width_px: size.ws_xpixel / size.ws_col,
        cell_height_px: size.ws_ypixel / size.ws_row,
    })
}

/// How the relay ends.
struct Finish {
    code: i32,
    /// Whether the server's terminal is closed on the way out: the person's side ended and the shell would go on
    /// running. Not when the shell ended (the server keeps its last screen for a while and collects it) or when the
    /// terminal is not this relay's to close.
    close: bool,
    /// Whether the shell ended by itself, as a shell over ssh does: whatever modes it left are its own to have left.
    shell_ended: bool,
}

impl Finish {
    fn kept(code: i32) -> Self {
        Self {
            code,
            close: false,
            shell_ended: false,
        }
    }
    fn closing(code: i32) -> Self {
        Self {
            code,
            close: true,
            shell_ended: false,
        }
    }
    fn shell_ended(code: i32) -> Self {
        Self {
            code,
            close: false,
            shell_ended: true,
        }
    }
}

struct Relay {
    host: String,
    output: Box<dyn Write + Send>,
    /// Whether [`MODES_RESET`] is still to be written at an end that leaves the program running.
    modes_to_reset: bool,
    answer: OpenAnswer,
    /// Where the stream goes on, once a snapshot has said where it stands.
    expected: Option<u64>,
    snapshots: u32,
    /// Since when no connection has shown a screen; `None` while one has.
    down_since: Option<Instant>,
    was_up: bool,
    /// Typed bytes the host never wrote (told by a reattach), or kept for a screen that never came: said when the
    /// relay ends.
    lost_input: u64,
}

impl Relay {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Finish> {
        // The person's terminal is gone (a pipe closed, a hangup): the shell is closed with it.
        self.output.write_all(bytes).map_err(|_| Finish::closing(1))
    }

    /// One line of the relay's own, on a line by itself: escaped, since the words may carry a host name or a server's,
    /// and after a `CAN`, since the server's output may have stopped inside a sequence the line would become part of.
    fn say(&mut self, text: &str) {
        let line = format!("\x18\r\n{}\r\n", terminal_text(text));
        let _ = self.output.write_all(line.as_bytes());
        let _ = self.output.flush();
    }

    fn host(&self) -> String {
        terminal_text(&self.host)
    }

    /// Takes the person's terminal out of the modes the server's program set, once, when the relay ends while that
    /// program goes on (or is closed by the relay): it never got to do so itself.
    fn reset_modes(&mut self) {
        if std::mem::take(&mut self.modes_to_reset) {
            let _ = self.output.write_all(MODES_RESET);
        }
    }

    /// Checks that a record continues the stream where the last one ended, and moves the end past it.
    fn advance(&mut self, offset: u64, len: usize) -> Result<(), Finish> {
        let refused = match self.expected {
            None => FrameRefused::OutputBeforeSnapshot,
            Some(expected) if expected != offset => FrameRefused::OffsetGap { expected, got: offset },
            Some(_) => match offset.checked_add(len as u64) {
                Some(end) => {
                    self.expected = Some(end);
                    return Ok(());
                }
                None => FrameRefused::OffsetOverflow,
            },
        };
        Err(self.refused(refused))
    }

    fn refused(&mut self, why: FrameRefused) -> Finish {
        self.reset_modes();
        let host = self.host();
        self.say(&format!(
            "the Eitri server on {host} sent {why}; the shell on {host} keeps running; `eitri host list` shows it"
        ));
        Finish::kept(255)
    }

    fn item(&mut self, item: LinkItem) -> Result<(), Finish> {
        match item {
            LinkItem::Snapshot(snapshot) => {
                self.write(if self.snapshots == 0 { FIRST_RESET } else { LATER_RESET })?;
                self.snapshots += 1;
                self.write(&snapshot.replay.bytes)?;
                let (tail, end) = match &snapshot.tail {
                    // Read after the screen's state and not yet parsed there: part of the stream from `offset` on.
                    Tail::ReadAhead(bytes) => (bytes, snapshot.offset.checked_add(bytes.len() as u64)),
                    // The unfinished sequence the server's parser stands inside, already counted in `offset`.
                    Tail::Raw { bytes, .. } => (bytes, Some(snapshot.offset)),
                };
                self.write(tail)?;
                let Some(end) = end else {
                    return Err(self.refused(FrameRefused::OffsetOverflow));
                };
                self.expected = Some(end);
                self.shows_a_screen();
            }
            LinkItem::Output { offset, bytes } => {
                self.advance(offset, bytes.len())?;
                self.write(&bytes)?;
            }
            LinkItem::Resized { offset, .. } => self.advance(offset, 0)?,
            LinkItem::InputWritten(_) => {}
            LinkItem::Waiting(_) => self.shows_a_screen(),
            LinkItem::Exited(exit) => return Err(self.exited(exit)),
            LinkItem::Ended(end) => return Err(self.ended(end)),
            LinkItem::Link {
                state,
                lost_input_bytes,
            } => {
                if state != LinkState::Live {
                    self.expected = None;
                    if self.down_since.is_none() {
                        self.down_since = Some(Instant::now());
                        if self.was_up {
                            let host = self.host();
                            self.say(&format!("the link to {host} was lost; reconnecting"));
                        }
                    }
                }
                self.lost_input = self.lost_input.saturating_add(lost_input_bytes);
            }
        }
        Ok(())
    }

    fn say_lost_input(&mut self) {
        let lost = std::mem::take(&mut self.lost_input);
        if lost > 0 {
            self.say(&format!("{lost} typed byte(s) did not reach the shell"));
        }
    }

    fn shows_a_screen(&mut self) {
        self.down_since = None;
        self.was_up = true;
    }

    fn exited(&mut self, exit: ExitInfo) -> Finish {
        let host = self.host();
        if exit.detached {
            self.reset_modes();
            self.say(&format!(
                "the shell on {host} left its terminal and keeps running; `eitri host list` shows it"
            ));
            return Finish::kept(0);
        }
        // The shell is gone: nothing is left to close.
        let code = match (exit.code, exit.signal) {
            (Some(code), _) => code,
            (None, Some(signal)) => {
                self.say(&format!("the shell on {host} ended with signal {signal}"));
                128 + signal
            }
            (None, None) => {
                self.say(&format!("how the shell on {host} ended could not be told"));
                1
            }
        };
        Finish::shell_ended(code)
    }

    fn ended(&mut self, end: MirrorEnd) -> Finish {
        self.reset_modes();
        let host = self.host();
        match end {
            MirrorEnd::Replaced => {
                self.say(&format!(
                    "another client took over the shell on {host}; it keeps running"
                ));
                Finish::kept(1)
            }
            MirrorEnd::Closed => {
                self.say(&format!("the shell on {host} was closed"));
                Finish::kept(1)
            }
            MirrorEnd::Stopped => {
                self.say(&format!("the Eitri server on {host} stopped the shell"));
                Finish::kept(1)
            }
            MirrorEnd::Gone(why) => {
                self.say(&why);
                // 255 leaves a shell that may still run; 1 says there is none.
                Finish::kept(if self.answer.terminal_gone() { 1 } else { 255 })
            }
            MirrorEnd::Refused(why) => self.refused(why),
        }
    }

    fn gave_up(&mut self) -> Finish {
        self.reset_modes();
        let host = self.host();
        if self.answer.get().is_some() {
            self.say(&format!(
                "the link to {host} did not come back; the shell on {host} keeps running; `eitri host list` shows it"
            ));
        } else {
            self.say(&format!(
                "{host} could not be reached; a shell may have been started there; `eitri host list` shows it"
            ));
        }
        Finish::kept(255)
    }

    /// Closes the server's terminal, best effort and within [`CLOSE_BUDGET`]: over the link's own connection, through
    /// `fallback` only when the link has none. An open still on its way is waited for within the same budget.
    fn close(&mut self, link: &TerminalLink, fallback: Close) {
        let deadline = Instant::now() + CLOSE_BUDGET;
        let host = self.host();
        let id = link
            .terminal_id()
            .or_else(|| self.answer.wait(deadline).map(|(id, _, _)| id));
        let Some(id) = id else {
            self.say(&format!(
                "a shell may have been started on {host}; `eitri host list` shows it"
            ));
            return;
        };
        let closed = match link.close_terminal(&id, deadline) {
            Some(closed) => closed,
            None => close_on_a_thread(fallback, id, deadline),
        };
        match closed {
            Ok(Some(_)) => self.say(CLOSE_WARNING_NOTICE),
            Ok(None) => {}
            Err(error) => self.say(&format!(
                "the shell on {host} could not be closed ({error}); it may keep running; `eitri host list` shows it"
            )),
        }
    }
}

/// Runs `close` on a thread of its own and waits for it until `deadline`: a connection it opens may take longer than
/// the process is given. The thread is left behind when it does.
fn close_on_a_thread(mut close: Close, id: String, deadline: Instant) -> std::io::Result<Option<String>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("term-close".into()).spawn(move || {
        let _ = sender.send(close(&id));
    })?;
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the server did not answer in time",
            ))
        })
}

/// Takes what the link's send window will take of `pending`. What the link refuses (it is not connected, or a screen
/// of this connection has not arrived) is dropped, since it was typed for a screen the person no longer sees; but
/// before the first screen (`keep`) it waits for it.
fn send_input(link: &mut TerminalLink, pending: &mut Vec<u8>, keep: bool) {
    while !pending.is_empty() {
        match link.write_input(pending) {
            Ok(0) => break,
            Ok(taken) => {
                pending.drain(..taken);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if keep && error.kind() == std::io::ErrorKind::NotConnected => break,
            Err(_) => pending.clear(),
        }
    }
}

enum Woke {
    Winch,
    Stop(i32),
}

/// The stop signal (`SIGHUP`, `SIGTERM` or `SIGINT`) already on `pipe`, without waiting for one: for a caller that
/// could not look while it connected and must not start a shell for a person who asked to stop. Resizes found on the
/// way are dropped; the relay asks the size when it starts.
pub fn stop_waiting(pipe: &OwnedFd) -> Option<i32> {
    loop {
        let mut fd = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll reads and writes the one structure, and does not wait.
        let ready = unsafe { libc::poll(&mut fd, 1, 0) };
        if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if ready <= 0 {
            return None;
        }
        let woke = signals(pipe.as_raw_fd());
        if woke.is_empty() {
            return None;
        }
        if let Some(signal) = woke.into_iter().find_map(|woke| match woke {
            Woke::Stop(signal) => Some(signal),
            Woke::Winch => None,
        }) {
            return Some(signal);
        }
    }
}

/// The signals waiting on `pipe` (it was polled readable).
fn signals(pipe: RawFd) -> Vec<Woke> {
    let mut bytes = [0u8; 16];
    // SAFETY: read into a buffer of that length, from a descriptor the caller holds.
    let read = unsafe { libc::read(pipe, bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes[..read.max(0) as usize]
        .iter()
        .filter_map(|&byte| match i32::from(byte) {
            libc::SIGWINCH => Some(Woke::Winch),
            signal @ (libc::SIGHUP | libc::SIGTERM | libc::SIGINT) => Some(Woke::Stop(signal)),
            _ => None,
        })
        .collect()
}

/// Runs the relay until the shell ends, the person's side ends or the link stays down past the limit, and gives the
/// exit code: the shell's, 0 after the person's end of input, `128 + n` after a signal, 255 when the shell is left
/// running on a server that cannot be reached, 1 for a shell that is gone.
///
/// The caller starts watching the signals before it starts the link, so that a hangup while it connects is seen here.
pub fn run(session: Session, terminal: Terminal) -> i32 {
    let Session {
        host,
        mut link,
        close,
        reattach_limit,
        mut size,
    } = session;
    let Terminal {
        input,
        output,
        output_is_terminal,
        signals: signal_pipe,
    } = terminal;
    // The link's own lines go where the relay's go: stderr is this terminal too, and raw.
    link.keep_notes();
    let mut relay = Relay {
        host,
        output,
        modes_to_reset: output_is_terminal,
        answer: link.answer_handle(),
        expected: None,
        snapshots: 0,
        down_since: Some(Instant::now()),
        was_up: false,
        lost_input: 0,
    };
    let (raw, finish) = match RawMode::enter(input) {
        Ok(raw) => {
            let finish = relay_loop(&mut relay, &mut link, input, signal_pipe, reattach_limit, size.as_mut());
            (raw, finish)
        }
        Err(error) => {
            relay.say(&format!(
                "the terminal could not be put in raw mode: {}",
                terminal_text(&error.to_string())
            ));
            (None, Finish::closing(1))
        }
    };
    if !finish.shell_ended {
        relay.reset_modes();
    }
    for note in link.take_notes() {
        relay.say(&note);
    }
    relay.say_lost_input();
    if finish.close {
        relay.close(&link, close);
    }
    let _ = relay.output.flush();
    drop(link);
    drop(raw);
    finish.code
}

fn relay_loop(
    relay: &mut Relay,
    link: &mut TerminalLink,
    input: RawFd,
    signal_pipe: OwnedFd,
    reattach_limit: Duration,
    size: &mut dyn FnMut() -> Option<PtySize>,
) -> Finish {
    if let Some(now) = size() {
        link.request_resize(now);
    }
    let mut pending: Vec<u8> = Vec::new();
    // Since end of input: after it, the loop goes on only until what was typed has been written on the server, within
    // `INPUT_DRAIN`.
    let mut input_ended: Option<Instant> = None;
    let mut buffer = vec![0u8; INPUT_CHUNK];
    loop {
        for item in link.take() {
            if let Err(finish) = relay.item(item) {
                return finish;
            }
        }
        for note in link.take_notes() {
            relay.say(&note);
        }
        let _ = relay.output.flush();
        if relay.down_since.is_some_and(|since| since.elapsed() >= reattach_limit) {
            return relay.gave_up();
        }
        send_input(link, &mut pending, relay.snapshots == 0);
        if let Some(ended) = input_ended {
            // The close goes on its own call and could overtake the keys on their stream: it waits until the server
            // has written them, and not past the drain, since a link that is down (or a first screen that never
            // comes) would otherwise hold the person's end for the whole reattach limit.
            if pending.is_empty() && link.input_unwritten() == 0 {
                return Finish::closing(0);
            }
            if ended.elapsed() >= INPUT_DRAIN {
                relay.lost_input = relay.lost_input.saturating_add(pending.len() as u64);
                return Finish::closing(0);
            }
        }

        let mut fds = [
            libc::pollfd {
                fd: signal_pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: link.readiness().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: input,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let watching = if input_ended.is_none() && pending.len() < INPUT_PENDING_MAX {
            3
        } else {
            2
        };
        // SAFETY: poll reads and writes the array of that length.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), watching, TICK_MS) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            relay.say("the terminal could not be waited on");
            return Finish::closing(1);
        }
        if fds[0].revents != 0 {
            for woke in signals(signal_pipe.as_raw_fd()) {
                match woke {
                    Woke::Winch => {
                        if let Some(now) = size() {
                            link.request_resize(now);
                        }
                    }
                    Woke::Stop(signal) => return Finish::closing(128 + signal),
                }
            }
        }
        if watching == 3 && fds[2].revents != 0 {
            // SAFETY: read into a buffer of that length, from the person's input.
            let read = unsafe { libc::read(input, buffer.as_mut_ptr().cast(), buffer.len()) };
            match read {
                n if n > 0 => pending.extend_from_slice(&buffer[..n as usize]),
                n if n < 0
                    && matches!(
                        std::io::Error::last_os_error().kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) => {}
                // End of input, or a terminal hung up (a read past its end fails): the person is done once what they
                // typed has been written on the server, or dropped with a lost link.
                _ => input_ended = Some(Instant::now()),
            }
        }
    }
}
