//! One bridge's I/O: Neovim's stdio on two threads (a reader that frames, a writer that never makes the loop wait),
//! the link to the helper ([`super::link`]), and the loop between them that feeds the [`Bridge`] one event at a time
//! and carries out what it returns. Nothing in the loop blocks on a write: Neovim and the helper both have bounded
//! queues, and a loop stopped on one while the other waits for it to read is a deadlock.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use agent::remote_client::terminal_transport::wire::LSP_MESSAGE_MAX;
use serde_json::Value;

use super::bridge::{Action, Bridge, Config, Disk, Out};
use super::framing::{self, encode_frame, Frame, FrameError, FrameReader, HEADERS_MAX};
use super::link::{random_id, Cmd, Dial, Link, LinkEvent, Params, Redial};
use crate::remote_sync::terminal_text;

/// How long the end of the bridge waits for the server's stop and for what is queued to Neovim.
const FINISH_WAIT: Duration = Duration::from_secs(8);
/// What may wait to be written to Neovim before the bridge gives up on it: room for two messages of the largest size.
/// With the link's own bound on messages not yet handled, the most a server can make the bridge hold is about
/// this, the link's bound and one message being parsed, so a flooding server cannot grow it without limit.
pub const NVIM_QUEUE_MAX: usize = 2 * LSP_MESSAGE_MAX;
/// How long a bridge whose server could not be had waits for Neovim to say anything before it ends by itself.
pub const QUIET_EXIT: Duration = Duration::from_secs(10);

/// Asks the project's sync to carry the laptop's changes now and waits for its answer (`Err` with why it did not).
/// Called on a thread of its own, never on the loop.
pub type Flush = Box<dyn FnMut() -> Result<(), String> + Send>;

pub struct Io {
    pub stdin: Box<dyn Read + Send>,
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
}

pub struct Options {
    /// The host as typed.
    pub host: String,
    /// The server's working directory, which is the mirror's path.
    pub root: PathBuf,
    pub argv: Vec<String>,
    pub grace_ms: u64,
    pub backoff: Vec<Duration>,
    pub open_deadline: Duration,
    /// Bytes queued for Neovim past which the bridge ends ([`NVIM_QUEUE_MAX`]).
    pub nvim_queue_max: usize,
    /// The read end of the pipe SIGTERM and SIGHUP arrive on ([`stop_signal_pipe`]): they stop the server before the
    /// bridge ends. Only the process that is the bridge has one (tests that run a session among others leave the
    /// process's signals alone).
    pub stop_signals: Option<OwnedFd>,
    /// How saves reach the server sooner than sync would notice them ([`Flush`]); `None` where there is no sync to ask.
    pub flush: Option<Flush>,
    /// How long a bridge whose server could not be had waits for Neovim ([`QUIET_EXIT`]).
    pub quiet_exit: Duration,
}

/// Makes SIGTERM and SIGHUP arrive as bytes on a pipe, for [`Options::stop_signals`]. The process that is the bridge
/// calls it first, before anything starts a process (ssh for the connection, a server run here): a signal that comes
/// while the first connection is being made then waits on the pipe and is acted on once the session runs, when
/// nothing has been opened on the server yet.
pub fn stop_signal_pipe() -> std::io::Result<OwnedFd> {
    super::signals::watch(&[libc::SIGTERM, libc::SIGHUP])
}

/// The signal waiting on the stop-signal pipe, if one is, taken off it without waiting.
pub fn pending_signal(read_end: &OwnedFd) -> Option<i32> {
    let mut poll = libc::pollfd {
        fd: read_end.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd for a descriptor this process holds, no wait.
    if unsafe { libc::poll(&mut poll, 1, 0) } != 1 || poll.revents & libc::POLLIN == 0 {
        return None;
    }
    let mut byte = [0u8; 1];
    // SAFETY: a read of one byte into a buffer of one byte, from a descriptor poll just said is readable.
    let read = unsafe { libc::read(read_end.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
    (read == 1).then_some(i32::from(byte[0]))
}

enum Event {
    Frame(Frame),
    NvimEof,
    NvimBroken(FrameError),
    Link(LinkEvent),
    Signal(i32),
    /// A flush asked for by a save has ended.
    Flushed(Result<(), String>),
}

/// Runs the bridge until Neovim ends it or it ends itself, and gives the exit code. `first` is the connection already
/// made (the capability check happens before this); `redial` makes another.
pub fn run(mut options: Options, io: Io, first: Option<Dial>, redial: Redial, disk: Box<dyn Disk + Send>) -> i32 {
    let (events, inbox) = mpsc::channel::<Event>();
    let Io {
        stdin,
        stdout,
        mut stderr,
    } = io;

    // Neovim's stdin, framed.
    let reader_events = events.clone();
    let spawned = std::thread::Builder::new().name("lsp-stdin".into()).spawn(move || {
        read_nvim(stdin, &reader_events);
    });
    if spawned.is_err() {
        let _ = writeln!(stderr, "eitri host lsp: could not start a thread");
        return 1;
    }

    let Ok(writer) = spawn_writer(stdout) else {
        let _ = writeln!(stderr, "eitri host lsp: could not start a thread");
        return 1;
    };

    let flusher = match options.flush.take().map(|flush| spawn_flusher(flush, events.clone())) {
        Some(Ok(asks)) => Some(asks),
        Some(Err(_)) => {
            let _ = writeln!(stderr, "eitri host lsp: could not start a thread");
            return 1;
        }
        None => None,
    };

    if let Some(read_end) = options.stop_signals.take() {
        if let Err(error) = watch_stop_signals(read_end, events.clone()) {
            let _ = writeln!(stderr, "eitri host lsp: could not watch for stop signals: {error}");
            return 1;
        }
    }

    // The link.
    let pending = Arc::new(AtomicUsize::new(0));
    let link_events = events.clone();
    let params = Params {
        host: options.host.clone(),
        root: options.root.to_string_lossy().into_owned(),
        argv: options.argv.clone(),
        grace_ms: options.grace_ms,
        client_id: random_id(),
        backoff: options.backoff.clone(),
        open_deadline: options.open_deadline,
    };
    let link = match Link::start(
        params,
        first,
        redial,
        Arc::new(move |event| {
            let _ = link_events.send(Event::Link(event));
        }),
        Arc::clone(&pending),
    ) {
        Ok(link) => link,
        Err(error) => {
            let _ = writeln!(stderr, "eitri host lsp: could not start the link: {error}");
            return 1;
        }
    };

    let host = options.host.clone();
    let bridge = Bridge::new(
        Config {
            host: options.host,
            root: options.root,
            command: options.argv,
        },
        disk,
    );
    let mut session = Session {
        bridge,
        link,
        to_nvim: writer.to_nvim,
        queued: writer.queued,
        nvim_queue_max: options.nvim_queue_max,
        pending,
        stderr,
        gen: None,
        stops_owed: 0,
        exit: None,
        host,
        flusher,
        flush_failure: None,
        quiet_exit: options.quiet_exit,
        quiet: None,
    };
    while session.exit.is_none() {
        let event = match session.quiet {
            Some((code, until)) => match inbox.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    session.log(&format!(
                        "the editor sent nothing for {} s after the language server could not be started; ending",
                        session.quiet_exit.as_secs()
                    ));
                    session.exit = Some(code);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match inbox.recv() {
                Ok(event) => event,
                Err(_) => break,
            },
        };
        if let (Some((code, _)), Event::Frame(_)) = (session.quiet, &event) {
            session.quiet = Some((code, Instant::now() + session.quiet_exit));
        }
        let out = session.handle(event);
        session.apply(out);
    }
    let code = session.exit.unwrap_or(1);
    finish(session, &inbox, &writer.finished);
    code
}

/// Neovim's stdout, written by a thread of its own.
struct Writer {
    to_nvim: mpsc::Sender<Vec<u8>>,
    /// Bytes sent to the thread and not yet taken by it.
    queued: Arc<AtomicUsize>,
    /// Signalled when the thread has written all it was sent, or could not write.
    finished: mpsc::Receiver<()>,
}

fn spawn_writer(stdout: Box<dyn Write + Send>) -> std::io::Result<Writer> {
    let (to_nvim, written) = mpsc::channel::<Vec<u8>>();
    let queued = Arc::new(AtomicUsize::new(0));
    let (done, finished) = mpsc::channel::<()>();
    let writer_queued = Arc::clone(&queued);
    std::thread::Builder::new().name("lsp-stdout".into()).spawn(move || {
        let mut stdout = stdout;
        for bytes in written {
            writer_queued.fetch_sub(bytes.len(), Ordering::AcqRel);
            if stdout.write_all(&bytes).and_then(|()| stdout.flush()).is_err() {
                break;
            }
        }
        let _ = done.send(());
    })?;
    Ok(Writer {
        to_nvim,
        queued,
        finished,
    })
}

/// The end of a run: the server's stop is awaited, so a bridge that ends does not leave its server to its grace when
/// it can help it, then what is queued for Neovim is given its chance to be written.
fn finish(mut session: Session, inbox: &mpsc::Receiver<Event>, writer_finished: &mpsc::Receiver<()>) {
    let until = Instant::now() + FINISH_WAIT;
    while session.stops_owed > 0 {
        match inbox.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Event::Link(LinkEvent::Stopped)) => session.stops_owed -= 1,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    drop(session.to_nvim);
    let _ = writer_finished.recv_timeout(FINISH_WAIT);
}

/// The thread that runs [`Flush`]es, one at a time: a save asks with a unit on the channel it returns, and asks made
/// while a flush runs are answered by one more flush after it (the newest save is what the server waits for).
fn spawn_flusher(mut flush: Flush, events: mpsc::Sender<Event>) -> std::io::Result<mpsc::Sender<()>> {
    let (asks, asked) = mpsc::channel::<()>();
    std::thread::Builder::new().name("lsp-flush".into()).spawn(move || {
        while asked.recv().is_ok() {
            while asked.try_recv().is_ok() {}
            if events.send(Event::Flushed(flush())).is_err() {
                return;
            }
        }
    })?;
    Ok(asks)
}

/// Frames Neovim's stdin into events until it ends or is malformed.
fn read_nvim(mut stdin: Box<dyn Read + Send>, events: &mpsc::Sender<Event>) {
    let mut reader = FrameReader::new(LSP_MESSAGE_MAX);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = events.send(Event::NvimEof);
                return;
            }
            Ok(read) => read,
        };
        reader.push(&buf[..read]);
        loop {
            match reader.next_frame() {
                Ok(Some(frame)) => {
                    if events.send(Event::Frame(frame)).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = events.send(Event::NvimBroken(error));
                    return;
                }
            }
        }
    }
}

struct Session {
    bridge: Bridge,
    link: Link,
    to_nvim: mpsc::Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    nvim_queue_max: usize,
    pending: Arc<AtomicUsize>,
    stderr: Box<dyn Write + Send>,
    /// The attach whose events count; none while a server is being replaced or stopped.
    gen: Option<u64>,
    /// Stops asked of the link and not yet reported done.
    stops_owed: usize,
    exit: Option<i32>,
    /// The host as typed, for the lines shown.
    host: String,
    /// Where a save asks for a flush; `None` without sync to ask.
    flusher: Option<mpsc::Sender<()>>,
    /// The last flush failure said, so a failure that repeats is said once until a flush works or the words change.
    flush_failure: Option<String>,
    quiet_exit: Duration,
    /// Set while the bridge waits for Neovim after a failed open: the exit code and when the wait ends.
    quiet: Option<(i32, Instant)>,
}

impl Session {
    fn log(&mut self, line: &str) {
        let _ = writeln!(self.stderr, "eitri host lsp: {}", terminal_text(line));
    }

    fn handle(&mut self, event: Event) -> Out {
        match event {
            Event::Frame(Frame::Message(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(message) => self.bridge.from_nvim(message),
                Err(error) => {
                    self.log(&format!("dropped a message from the editor that is not JSON: {error}"));
                    Out::default()
                }
            },
            Event::Frame(Frame::Oversized(probe)) => self.bridge.from_nvim_oversized(probe),
            Event::NvimEof => self.bridge.nvim_closed(),
            Event::NvimBroken(error) => {
                let mut out = self.bridge.nvim_closed();
                out.log.push(format!(
                    "the editor sent a message whose header cannot be read ({error}); a header is at most \
                     {HEADERS_MAX} bytes"
                ));
                out.actions.retain(|action| !matches!(action, Action::Exit(_)));
                out.actions.push(Action::Exit(1));
                out
            }
            Event::Link(event) => self.handle_link(event),
            Event::Signal(signal) => self.bridge.signalled(signal),
            Event::Flushed(result) => {
                self.flushed(result);
                Out::default()
            }
        }
    }

    fn flushed(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.flush_failure = None,
            Err(why) if self.flush_failure.as_ref() != Some(&why) => {
                self.log(&format!(
                    "a save may reach {} only when sync next notices it: sync was asked to carry it and did not \
                     ({why})",
                    self.host
                ));
                self.flush_failure = Some(why);
            }
            Err(_) => {}
        }
    }

    fn handle_link(&mut self, event: LinkEvent) -> Out {
        match event {
            LinkEvent::Attached { gen, attached } => {
                self.gen = Some(gen);
                self.bridge.link_back(&attached)
            }
            LinkEvent::Server { gen, message } => {
                self.pending.fetch_sub(message.len(), Ordering::AcqRel);
                if self.gen != Some(gen) {
                    return Out::default();
                }
                match serde_json::from_slice::<Value>(&message) {
                    Ok(message) => self.bridge.from_server(message),
                    Err(error) => self
                        .bridge
                        .from_server_unreadable(framing::probe(&message), &error.to_string()),
                }
            }
            LinkEvent::Ended { gen, ended, fresh } if fresh || self.gen == Some(gen) => {
                self.gen = None;
                self.bridge.server_ended(&ended, Instant::now())
            }
            LinkEvent::Lost { gen } if self.gen == Some(gen) => {
                self.gen = None;
                self.bridge.link_lost()
            }
            LinkEvent::OpenFailed(failure) => self.bridge.open_failed(failure),
            LinkEvent::Note(text) => self.bridge.link_note(&text),
            LinkEvent::Stopped => {
                self.stops_owed = self.stops_owed.saturating_sub(1);
                Out::default()
            }
            LinkEvent::Ended { .. } | LinkEvent::Lost { .. } => Out::default(),
        }
    }

    /// Sends what the bridge decided, in order.
    fn apply(&mut self, out: Out) {
        for line in &out.log {
            self.log(line);
        }
        for message in out.to_nvim {
            let Ok(body) = serde_json::to_vec(&message) else {
                continue;
            };
            if self.queued.load(Ordering::Acquire) > self.nvim_queue_max {
                // Ending here must not leave the server to its grace: the stop the bridge asks on any end is asked
                // now, whatever else was decided.
                self.log("the editor stopped reading; ending");
                self.stop_server();
                self.exit = Some(1);
                return;
            }
            let framed = encode_frame(&body);
            self.queued.fetch_add(framed.len(), Ordering::AcqRel);
            let _ = self.to_nvim.send(framed);
        }
        let mut unsent = Vec::new();
        for sent in out.to_server {
            let Some(gen) = self.gen else { continue };
            let Ok(body) = serde_json::to_vec(&sent.message) else {
                continue;
            };
            if body.len() > LSP_MESSAGE_MAX {
                unsent.push(sent.message);
                continue;
            }
            self.link.send(Cmd::Send {
                gen,
                message: body,
                saved_sha256: sent.saved_sha256,
            });
        }
        for action in out.actions {
            match action {
                Action::OpenServer => {
                    self.gen = None;
                    self.link.send(Cmd::Open);
                }
                Action::StopServer => self.stop_server(),
                Action::Exit(code) => self.exit = Some(code),
                Action::ExitWhenQuiet(code) => self.quiet = Some((code, Instant::now() + self.quiet_exit)),
                Action::FlushSync => {
                    if let Some(asks) = &self.flusher {
                        let _ = asks.send(());
                    }
                }
            }
        }
        for message in unsent {
            let more = self.bridge.unsent(&message);
            self.apply(more);
        }
    }

    fn stop_server(&mut self) {
        self.gen = None;
        self.stops_owed += 1;
        self.link.send(Cmd::Stop);
    }
}

/// SIGTERM and SIGHUP become [`Event::Signal`]s: a handler writes the signal's number to a pipe a thread reads. A
/// handler (not a blocked mask) so that the processes the bridge starts (ssh, a locally run server) get the signals'
/// usual dispositions back at their `exec`.
fn watch_stop_signals(read_end: OwnedFd, events: mpsc::Sender<Event>) -> std::io::Result<()> {
    std::thread::Builder::new().name("lsp-signals".into()).spawn(move || {
        let mut pipe = std::fs::File::from(read_end);
        let mut byte = [0u8; 1];
        while let Ok(1) = pipe.read(&mut byte) {
            if events.send(Event::Signal(i32::from(byte[0]))).is_err() {
                return;
            }
        }
    })?;
    Ok(())
}
