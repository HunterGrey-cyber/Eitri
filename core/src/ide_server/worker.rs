//! The listener's poll loop and the parts it runs on.
//!
//! One thread holds the listening socket, every connection, the lock's keeper and the mention socket, and sleeps in
//! `poll` over all of them and two wakes. It reads and writes only without blocking and reads a bounded amount per
//! connection per wake, so nothing one client sends or stops reading can hold up another. Whatever may touch the disk
//! goes to the judge thread as a [`Job`]; whatever needs nvim goes to the main context as an [`Event`].
//!
//! The lock file never outlives the port. [`Command::Stop`] removes the file, then closes the sockets, and every other
//! way out (a failing `poll`, a gone judge, a panic) does the same from [`Worker`]'s `Drop`, before its fields go.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Range;
use std::os::fd::{AsRawFd, RawFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::scratch::PROPOSAL_MAX_BYTES;

use super::channels::Wake;
use super::frames::{close_frame, pong_frame, text_frame, Decoder, Inbound};
use super::handshake::{self, Token, Verdict};
use super::mcp::{self, Incoming, Push};
use super::types::{
    Command, ConnId, DiagnosticsQuery, Done, Event, Housekeeping, Job, Limits, MentionPort, RpcId, Started, ToolAnswer,
};

/// The most read from one connection on one wake.
const READ_PER_WAKE: usize = 64 * 1024;
/// How long the listener stays out of the poll set after an `accept` that failed for want of descriptors, since a
/// level-triggered `poll` would otherwise report the same waiting connection without end.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);
/// The longest `poll` waits while a job is out at the judge. A judge dies only on a job, and its closed channel signals
/// nothing, so the job that killed it would otherwise leave its client waiting for a wake that may never come.
const JUDGE_CHECK: Duration = Duration::from_secs(1);
/// What an emptied queue keeps of its allocation.
const QUEUE_KEPT: usize = 256 * 1024;
const SERVICE_UNAVAILABLE: &str = "503 Service Unavailable";
const CONNECTIONS_FULL: &str = "every client slot is taken";
const WINDOW_BEHIND: &str = "the window has not taken what it was sent";
/// The close code for a message that cannot be read and cannot be answered.
const MESSAGE_TOO_BIG: u16 = 1009;
/// The close code answering a close that named none.
const NORMAL_CLOSE: u16 = 1000;
const OVER_THE_PROPOSAL_CAP: &str = "Eitri shows diffs only for regular files of at most 4 MiB";
const OVER_THE_MESSAGE_CAP: &str = "the message is larger than Eitri reads";
const OVER_THE_ANSWER_CAP: &str = "the answer is larger than a client reads";
const TOO_MANY_CALLS: &str = "too many calls are waiting on this connection";

/// The system calls the loop rests on whose failures a test cannot bring about by hand. The provided methods are the
/// real calls; [`System`] is them.
pub trait Syscalls {
    fn poll(&mut self, fds: &mut [libc::pollfd], timeout_ms: libc::c_int) -> io::Result<()> {
        // SAFETY: `fds` is a valid array of `fds.len()` entries for the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn accept(&mut self, listener: &TcpListener) -> io::Result<TcpStream> {
        listener.accept().map(|(stream, _)| stream)
    }

    /// One line of the log.
    fn log(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// The real system calls.
pub struct System;

impl Syscalls for System {}

/// What the worker runs on. `listener` is bound to 127.0.0.1 and non-blocking.
pub struct WorkerParts {
    pub listener: TcpListener,
    pub token: Token,
    pub limits: Limits,
    pub keeper: Box<dyn Housekeeping>,
    pub mentions: Box<dyn MentionPort>,
    pub events: Sender<Event>,
    pub events_wake: Arc<Wake>,
    /// Events sent and not yet taken: counted here, taken off by whoever reads `events`.
    pub events_waiting: Arc<AtomicUsize>,
    pub commands: Receiver<Command>,
    pub commands_wake: Arc<Wake>,
    pub jobs: Sender<Job>,
    pub done: Receiver<Done>,
    pub done_wake: Arc<Wake>,
}

/// Starts the lock and serves until [`Command::Stop`] or until something ends the loop. The first event is always
/// [`Event::Started`]; any end but `Stop` after that is [`Event::Lost`].
pub fn run(parts: WorkerParts) {
    run_on(parts, System);
}

/// [`run`] over the given system calls.
pub fn run_on<C: Syscalls>(parts: WorkerParts, calls: C) {
    let port = match parts.listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            let why = format!("the listener has no address: {error}");
            // The receiver gone means nobody is left to tell.
            parts.events_waiting.fetch_add(1, Ordering::Relaxed);
            let _ = parts.events.send(Event::Started(Started::NotAttached { why }));
            parts.events_wake.signal();
            return;
        }
    };
    let mut worker = Worker::new(parts, port, calls);
    if !worker.start() {
        return;
    }
    match worker.serve() {
        Ok(()) => worker.stop(),
        Err(why) => eprintln!("[ide-server] the listener ended: {why}"),
    }
}

/// How far the worker has come, which decides what its `Drop` owes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Life {
    /// The keeper's start has not answered: the first event is still owed.
    Starting,
    Serving,
    /// A failed start or a `Stop` took care of everything.
    Ended,
}

/// One accepted socket.
struct Conn {
    stream: TcpStream,
    /// Bytes to write, in order. A frame goes in whole, so a selection written after them never lands inside one.
    queue: VecDeque<u8>,
    stage: Stage,
}

enum Stage {
    Upgrading { head: Vec<u8>, until: Instant },
    Open(Box<Session>),
}

/// An authenticated connection.
struct Session {
    decoder: Decoder,
    /// Pushes go only to a connection that said it is ready.
    ready: bool,
    /// The newest selection not yet begun. Written only when the queue is empty, and replaced until then, so a stalled
    /// client holds one.
    selection: Option<Rc<Vec<u8>>>,
    /// The calls out at the judge, each with whether it was withdrawn. A withdrawn call keeps its place until the judge
    /// gives it back, so withdrawing and asking again cannot queue more than the bound.
    calls: HashMap<RpcId, bool>,
    pid_logged: bool,
    parse_error_logged: bool,
}

/// The newest selection: its params to compare the next with, its frame to share between the connections' slots.
struct Selection {
    params: serde_json::Value,
    frame: Rc<Vec<u8>>,
}

/// Where this pass's descriptors sit in the poll set, after the two wakes.
struct PollSet {
    listener: Option<usize>,
    keeper: Option<usize>,
    mentions: Range<usize>,
    conns_from: usize,
}

struct Worker<C: Syscalls> {
    calls: C,
    life: Life,
    port: u16,
    listener: TcpListener,
    token: Token,
    limits: Limits,
    keeper: Box<dyn Housekeeping>,
    mentions: Box<dyn MentionPort>,
    events: Sender<Event>,
    events_wake: Arc<Wake>,
    events_waiting: Arc<AtomicUsize>,
    commands: Receiver<Command>,
    commands_wake: Arc<Wake>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    done_wake: Arc<Wake>,
    /// Ordered by id, and ids only grow, so a lower id is an older connection.
    conns: BTreeMap<ConnId, Conn>,
    next_conn: ConnId,
    /// The listener is out of the poll set until then.
    listener_back: Option<Instant>,
    accept_failure_logged: bool,
    /// The refusals already told, at most one for each constant reason.
    refusals_logged: Vec<&'static str>,
    selection: Option<Selection>,
    mention_up: bool,
    judge_gone: bool,
    /// Jobs sent and not yet given back; the judge gives one [`Done`] for each.
    jobs_out: usize,
    /// The calls the main context holds, each with whether the client left it, by withdrawing it or by going. Each is a
    /// frame for nvim and a request kept on the GTK thread, and nothing bounds what waits for nvim, so a call the client
    /// left keeps its place until the main context says nvim owes it nothing. Here and not on a session, so connecting
    /// again renews none of it.
    held: HashMap<(ConnId, RpcId), bool>,
    fds: Vec<libc::pollfd>,
    /// The connections of this pass's poll set, in its order.
    polled: Vec<ConnId>,
    read_buffer: Vec<u8>,
    inbound: Vec<Inbound>,
}

impl<C: Syscalls> Drop for Worker<C> {
    /// Runs before the fields drop, so on every way out the lock is gone before the port is.
    fn drop(&mut self) {
        let first_event_owed = match self.life {
            Life::Ended => return,
            Life::Starting => true,
            Life::Serving => false,
        };
        self.release();
        self.close_all();
        self.emit(if first_event_owed {
            Event::Started(Started::NotAttached {
                why: "the listener's start failed".to_owned(),
            })
        } else {
            Event::Lost
        });
    }
}

/// Runs `call` and swallows its panic: this runs on the way out, where one escaping a `Drop` would end the process.
fn quietly(what: &str, call: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(call)).is_err() {
        eprintln!("[ide-server] {what} panicked on the way out");
    }
}

impl<C: Syscalls> Worker<C> {
    fn new(parts: WorkerParts, port: u16, calls: C) -> Worker<C> {
        Worker {
            calls,
            life: Life::Starting,
            port,
            listener: parts.listener,
            token: parts.token,
            limits: parts.limits,
            keeper: parts.keeper,
            mentions: parts.mentions,
            events: parts.events,
            events_wake: parts.events_wake,
            events_waiting: parts.events_waiting,
            commands: parts.commands,
            commands_wake: parts.commands_wake,
            jobs: parts.jobs,
            done: parts.done,
            done_wake: parts.done_wake,
            conns: BTreeMap::new(),
            next_conn: 1,
            listener_back: None,
            accept_failure_logged: false,
            refusals_logged: Vec::new(),
            selection: None,
            mention_up: false,
            judge_gone: false,
            jobs_out: 0,
            held: HashMap::new(),
            fds: Vec::new(),
            polled: Vec::new(),
            read_buffer: vec![0; READ_PER_WAKE],
            inbound: Vec::new(),
        }
    }

    fn emit(&self, event: Event) {
        // Counted before it is sent, so whoever takes it never takes off more than was counted.
        self.events_waiting.fetch_add(1, Ordering::Relaxed);
        // The receiver gone means the main context is: its end of `commands` closing ends the loop on the next pass.
        let _ = self.events.send(event);
        self.events_wake.signal();
    }

    fn start(&mut self) -> bool {
        match self.keeper.start(self.port, self.token.as_str()) {
            Ok(()) => {
                self.life = Life::Serving;
                self.emit(Event::Started(Started::Attached { port: self.port }));
                true
            }
            Err(why) => {
                self.life = Life::Ended;
                self.emit(Event::Started(Started::NotAttached { why }));
                false
            }
        }
    }

    /// `Ok`: told to stop. `Err`: why the loop cannot go on.
    fn serve(&mut self) -> Result<(), String> {
        loop {
            let now = Instant::now();
            let set = self.poll_set(now);
            let timeout = self.timeout_ms(now);
            match self.calls.poll(&mut self.fds, timeout) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(format!("poll failed: {error}")),
            }
            let now = Instant::now();
            // Before the queues are read, so anything sent after these reads wakes the next `poll`.
            self.commands_wake.drain();
            self.done_wake.drain();
            if self.take_commands()? {
                return Ok(());
            }
            self.take_done()?;
            self.keeper_turn(&set, now);
            self.mention_turn(&set, now);
            // Reads first, so a connection has had its read before a newer one can evict it.
            self.read_turn(&set);
            if set.listener.is_some_and(|at| self.fds[at].revents != 0) {
                self.accept_turn(now);
            }
            self.expire(now);
            self.flush_all();
            if self.judge_gone {
                return Err("the judge went away".to_owned());
            }
        }
    }

    fn poll_set(&mut self, now: Instant) -> PollSet {
        fn entry(fd: RawFd, events: libc::c_short) -> libc::pollfd {
            libc::pollfd { fd, events, revents: 0 }
        }

        self.fds.clear();
        self.polled.clear();
        self.fds.push(entry(self.commands_wake.fd(), libc::POLLIN));
        self.fds.push(entry(self.done_wake.fd(), libc::POLLIN));

        if self.listener_back.is_some_and(|back| back <= now) {
            self.listener_back = None;
        }
        let listener = self.listener_back.is_none().then(|| {
            self.fds.push(entry(self.listener.as_raw_fd(), libc::POLLIN));
            self.fds.len() - 1
        });
        let keeper = self.keeper.watch_fd().map(|fd| {
            self.fds.push(entry(fd, libc::POLLIN));
            self.fds.len() - 1
        });
        let mentions_from = self.fds.len();
        for fd in self.mentions.fds() {
            self.fds.push(entry(fd, libc::POLLIN));
        }
        let mentions = mentions_from..self.fds.len();

        let conns_from = self.fds.len();
        for (id, conn) in &self.conns {
            let events = if conn.has_bytes_waiting() {
                libc::POLLIN | libc::POLLOUT
            } else {
                libc::POLLIN
            };
            self.fds.push(entry(conn.stream.as_raw_fd(), events));
            self.polled.push(*id);
        }
        PollSet {
            listener,
            keeper,
            mentions,
            conns_from,
        }
    }

    /// The earliest moment something is owed with nothing readable. It never rounds down, so a wait under a millisecond
    /// does not become a turn of the loop with nothing due.
    fn timeout_ms(&self, now: Instant) -> libc::c_int {
        let handshake = self.conns.values().filter_map(Conn::upgrade_deadline).min();
        let keeper = self.keeper.retry_in(now).and_then(|wait| now.checked_add(wait));
        let judge = (self.jobs_out > 0).then(|| now + JUDGE_CHECK);
        let earliest = [
            handshake,
            keeper,
            self.mentions.next_deadline(),
            self.listener_back,
            judge,
        ]
        .into_iter()
        .flatten()
        .min();
        match earliest {
            None => -1,
            Some(at) => {
                let wait = at.saturating_duration_since(now).as_nanos().div_ceil(1_000_000);
                wait.min(libc::c_int::MAX as u128) as libc::c_int
            }
        }
    }

    /// `Ok(true)`: told to stop.
    fn take_commands(&mut self) -> Result<bool, String> {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Stop) => return Ok(true),
                Ok(command) => self.on_command(command),
                Err(TryRecvError::Empty) => return Ok(false),
                Err(TryRecvError::Disconnected) => return Err("the main context went away".to_owned()),
            }
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Answer { conn, id, answer } => {
                self.let_go(conn, &id);
                self.answer(conn, &id, &answer);
            }
            Command::Entries {
                conn,
                id,
                named_uri,
                answer,
            } => {
                self.let_go(conn, &id);
                self.send_to_judge(conn, id, |id| Job::Entries {
                    conn,
                    id,
                    named_uri,
                    answer,
                });
            }
            Command::Saved { conn, id, lines, shape } => {
                self.let_go(conn, &id);
                self.send_to_judge(conn, id, |id| Job::Join { conn, id, lines, shape });
            }
            Command::Released { conn, id } => {
                let call = (conn, id);
                // Only a call the client left: the id may have been asked again since.
                if self.held.get(&call) == Some(&true) {
                    self.held.remove(&call);
                }
            }
            Command::Context(update) => self.send_job(Job::Context(update)),
            // `take_commands` ends the loop on it.
            Command::Stop => {}
        }
    }

    fn send_job(&mut self, job: Job) {
        // The judge's end of `jobs` closes only with the judge.
        if self.jobs.send(job).is_err() {
            self.judge_gone = true;
        } else {
            self.jobs_out += 1;
        }
    }

    fn take_done(&mut self) -> Result<(), String> {
        loop {
            match self.done.try_recv() {
                Ok(done) => {
                    self.jobs_out = self.jobs_out.saturating_sub(1);
                    self.on_done(done);
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Err("the judge went away".to_owned()),
            }
        }
    }

    fn on_done(&mut self, done: Done) {
        match done {
            Done::Answer { conn, id, answer } => {
                if self.settle(conn, &id) {
                    self.answer(conn, &id, &answer);
                }
            }
            Done::Host(event) => {
                if call_of(&event).is_none_or(|(conn, id)| self.settle(conn, id)) {
                    self.send_to_host(event);
                }
            }
            Done::Selection(params) => self.on_selection(params),
            Done::Mention(params) => {
                let frame = text_frame(&mcp::notification(Push::AtMentioned, params));
                let ready: Vec<ConnId> = self
                    .conns
                    .iter()
                    .filter(|(_, conn)| conn.is_ready())
                    .map(|(id, _)| *id)
                    .collect();
                for conn in ready {
                    self.queue(conn, &frame);
                }
            }
        }
    }

    /// The judge gave a call back. `true`: it is still wanted. A gone connection, a withdrawn call and an id never out
    /// all answer `false`.
    fn settle(&mut self, conn: ConnId, id: &RpcId) -> bool {
        self.session(conn).and_then(|session| session.calls.remove(id)) == Some(false)
    }

    fn on_selection(&mut self, params: serde_json::Value) {
        if self.selection.as_ref().is_some_and(|newest| newest.params == params) {
            return;
        }
        let frame = Rc::new(text_frame(&mcp::notification(Push::SelectionChanged, params.clone())));
        for conn in self.conns.values_mut() {
            if let Stage::Open(session) = &mut conn.stage {
                if session.ready {
                    session.selection = Some(Rc::clone(&frame));
                }
            }
        }
        self.selection = Some(Selection { params, frame });
    }

    fn keeper_turn(&mut self, set: &PollSet, now: Instant) {
        let readable = set.keeper.is_some_and(|at| self.fds[at].revents != 0);
        let due = self.keeper.retry_in(now).is_some_and(|wait| wait.is_zero());
        if readable || due {
            self.keeper.on_wake(now);
        }
    }

    fn mention_turn(&mut self, set: &PollSet, now: Instant) {
        let readable = self.fds[set.mentions.clone()].iter().any(|fd| fd.revents != 0);
        let due = self.mentions.next_deadline().is_some_and(|at| at <= now);
        if readable || due {
            // Read either way, or the socket stays readable. nvim writes these whether or not the window is taking
            // anything, and a mention the window cannot keep up with is worth less than its queue.
            for line in self.mentions.poll(now) {
                if self.jobs_out < self.limits.calls_in_all() && !self.window_behind() {
                    self.send_job(Job::Mention(line));
                }
            }
        }
    }

    /// At most `unauthed`, so that an older connection is there to evict for each one accepted, and none accepted here
    /// has to evict another accepted here: all of them get a read first. `poll` is level-triggered, so the rest come on the
    /// next pass.
    fn accept_turn(&mut self, now: Instant) {
        // Ids only grow, so this is where this pass's connections begin.
        let this_pass = self.next_conn;
        for _ in 0..self.limits.unauthed {
            match self.calls.accept(&self.listener) {
                Ok(stream) => {
                    self.accept_failure_logged = false;
                    self.admit(stream, now, this_pass);
                }
                Err(error) => match error.kind() {
                    ErrorKind::WouldBlock => return,
                    ErrorKind::Interrupted | ErrorKind::ConnectionAborted => {}
                    _ => {
                        if !std::mem::replace(&mut self.accept_failure_logged, true) {
                            eprintln!("[ide-server] accept failed: {error}");
                        }
                        self.listener_back = Some(now + ACCEPT_RETRY);
                        return;
                    }
                },
            }
        }
    }

    fn admit(&mut self, stream: TcpStream, now: Instant, this_pass: ConnId) {
        if stream.set_nonblocking(true).is_err() || stream.set_nodelay(true).is_err() {
            return;
        }
        if let Some(bytes) = self.limits.send_buffer {
            set_send_buffer(&stream, bytes);
        }
        let upgrading = self.conns.values().filter(|conn| !conn.is_open()).count();
        if upgrading >= self.limits.unauthed {
            // The fewest bytes go first, the oldest of those: an idle connection has shown nothing, and a client part way
            // through its head is the one with something to lose. One accepted in this pass has had no read, so its count
            // says nothing yet.
            let evicted = self
                .conns
                .range(..this_pass)
                .filter_map(|(id, conn)| conn.upgrading_with().map(|bytes| (bytes, *id)));
            if let Some((_, id)) = evicted.min() {
                self.conns.remove(&id);
            }
        }
        let id = self.next_conn;
        self.next_conn += 1;
        self.conns.insert(
            id,
            Conn {
                stream,
                queue: VecDeque::new(),
                stage: Stage::Upgrading {
                    head: Vec::new(),
                    until: now + self.limits.handshake_within,
                },
            },
        );
    }

    fn read_turn(&mut self, set: &PollSet) {
        let mut buffer = std::mem::take(&mut self.read_buffer);
        for at in 0..self.polled.len() {
            if self.fds[set.conns_from + at].revents != 0 {
                self.read_conn(self.polled[at], &mut buffer);
            }
        }
        self.read_buffer = buffer;
    }

    /// One read, at most [`READ_PER_WAKE`], and what it brought.
    fn read_conn(&mut self, id: ConnId, buffer: &mut [u8]) {
        // A command or a judged result earlier in this pass can have ended it.
        let Some(conn) = self.conns.get_mut(&id) else {
            return;
        };
        let room = match &conn.stage {
            Stage::Upgrading { head, .. } => READ_PER_WAKE.min(self.limits.handshake_bytes.saturating_sub(head.len())),
            Stage::Open(_) => READ_PER_WAKE,
        };
        match conn.stream.read(&mut buffer[..room]) {
            Ok(0) => self.end(id),
            Ok(read) => {
                if conn.is_open() {
                    self.take_frames(id, &buffer[..read]);
                } else {
                    self.take_head(id, &buffer[..read]);
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
            Err(_) => self.end(id),
        }
    }

    fn take_head(&mut self, id: ConnId, bytes: &[u8]) {
        let Some(Conn {
            stage: Stage::Upgrading { head, .. },
            ..
        }) = self.conns.get_mut(&id)
        else {
            return;
        };
        head.extend_from_slice(bytes);
        match handshake::judge(head, self.port, &self.token, &self.limits) {
            Verdict::NeedMore => {}
            Verdict::Refuse { answer, why } => {
                self.log_refusal(why);
                self.refuse(id, &answer);
            }
            Verdict::Upgrade { answer } => {
                let full = self.open_count() >= self.limits.authed;
                if full || self.window_behind() {
                    self.log_refusal(if full { CONNECTIONS_FULL } else { WINDOW_BEHIND });
                    self.refuse(id, &handshake::refusal(SERVICE_UNAVAILABLE));
                } else {
                    self.join(id, &answer);
                }
            }
        }
    }

    /// Once for each reason in a listener's life: this line is all a person has to tell why a `claude` did nothing, and a
    /// program knocking at the port must not fill the log with it.
    fn log_refusal(&mut self, why: &'static str) {
        if !self.refusals_logged.contains(&why) {
            self.refusals_logged.push(why);
            self.calls.log(&format!("[ide-server] refused a client: {why}"));
        }
    }

    /// One try to write the answer: a new socket's buffer holds it, and a client that has not read that far has no
    /// claim on a descriptor.
    fn refuse(&mut self, id: ConnId, answer: &[u8]) {
        if let Some(mut conn) = self.conns.remove(&id) {
            conn.queue.extend(answer);
            let _ = conn.flush();
        }
    }

    fn join(&mut self, id: ConnId, answer: &[u8]) {
        let first = self.open_count() == 0;
        let Some(conn) = self.conns.get_mut(&id) else {
            return;
        };
        conn.queue.extend(answer);
        conn.stage = Stage::Open(Box::new(Session {
            decoder: Decoder::new(self.limits.message_bytes, self.limits.discard_bytes),
            ready: false,
            selection: None,
            calls: HashMap::new(),
            pid_logged: false,
            parse_error_logged: false,
        }));
        if first {
            if let Some(socket) = self.mentions.open() {
                self.mention_up = true;
                self.emit(Event::MentionUp { socket });
            }
        }
    }

    fn take_frames(&mut self, id: ConnId, bytes: &[u8]) {
        // Kept between reads, since a read of small frames is thousands of items.
        let mut items = std::mem::take(&mut self.inbound);
        if let Some(session) = self.session(id) {
            session.decoder.feed(bytes, &mut items);
        }
        for item in items.drain(..) {
            // One item can end the connection with more behind it.
            if !self.conns.contains_key(&id) {
                break;
            }
            self.on_inbound(id, item);
        }
        self.inbound = items;
    }

    fn on_inbound(&mut self, conn: ConnId, item: Inbound) {
        match item {
            Inbound::Text(message) => match mcp::parse(&message) {
                Ok(incoming) => self.on_message(conn, incoming),
                Err(why) => {
                    if let Some(session) = self.session(conn) {
                        if !std::mem::replace(&mut session.parse_error_logged, true) {
                            eprintln!("[ide-server] an unreadable message: {why}");
                        }
                    }
                }
            },
            Inbound::Oversized { tail } => match mcp::tail_id(&tail) {
                Some(id) => self.reply(conn, &mcp::answer_error(&id, OVER_THE_MESSAGE_CAP)),
                None => self.close_with(conn, MESSAGE_TOO_BIG),
            },
            Inbound::Ping(payload) => self.queue(conn, &pong_frame(&payload)),
            Inbound::Pong => {}
            Inbound::Close(code) => self.close_with(conn, code.unwrap_or(NORMAL_CLOSE)),
            Inbound::Violation(code) => self.close_with(conn, code),
        }
    }

    /// A close frame is the last thing on the wire, so a selection still in the slot goes with it unsent.
    fn close_with(&mut self, conn: ConnId, code: u16) {
        if let Some(session) = self.session(conn) {
            session.selection = None;
        }
        self.queue(conn, &close_frame(code));
        self.end(conn);
    }

    fn on_message(&mut self, conn: ConnId, message: Incoming) {
        match message {
            Incoming::Initialize { id, version } => {
                self.reply(conn, &mcp::answer_initialize(&id, version.as_deref()));
            }
            Incoming::ToolsList { id } => self.reply(conn, &mcp::answer_tools_list(&id)),
            Incoming::UnknownMethod { id } => self.reply(conn, &mcp::answer_method_not_found(&id)),
            Incoming::NotOffered { id, name } => self.reply(conn, &mcp::answer_not_offered(&id, &name)),
            Incoming::BadParams { id, why } => self.reply(conn, &mcp::answer_error(&id, &why)),
            Incoming::GetDiagnostics { id, uri: Some(uri) } => {
                self.send_to_judge(conn, id, |id| Job::Uri { conn, id, uri });
            }
            Incoming::GetDiagnostics { id, uri: None } => self.send_to_host(Event::Diagnostics {
                conn,
                id,
                query: DiagnosticsQuery::All,
            }),
            Incoming::OpenDiff { id, args } => {
                if args.new_file_contents.len() as u64 > PROPOSAL_MAX_BYTES {
                    self.reply(conn, &mcp::answer_error(&id, OVER_THE_PROPOSAL_CAP));
                } else {
                    self.send_to_judge(conn, id, |id| Job::Diff {
                        conn,
                        id,
                        args: Box::new(args),
                    });
                }
            }
            Incoming::CloseTab { id, tab_name } => {
                self.send_to_judge(conn, id, |id| Job::Pass(Event::CloseTab { conn, id, tab_name }));
            }
            Incoming::CloseAllDiffTabs { id } => {
                self.send_to_judge(conn, id, |id| Job::Pass(Event::CloseAll { conn, id }));
            }
            Incoming::Initialized => self.mark_ready(conn),
            Incoming::IdeConnected { pid } => {
                if let Some(session) = self.session(conn) {
                    if !std::mem::replace(&mut session.pid_logged, true) {
                        eprintln!("[ide-server] a client says it is pid {pid:?}");
                    }
                }
                self.mark_ready(conn);
            }
            Incoming::Cancelled { request } => self.on_cancelled(conn, request),
            Incoming::Ignored => {}
        }
    }

    /// Past the bound, or with an id already out, the call is answered here and never queued, whether it comes from the
    /// client or back from the host: the judge's queue holds what a connection may ask of it and no more, however long
    /// the judge is stuck. A gone connection's jobs stay in that queue, so they count against [`Limits::calls_in_all`],
    /// and connecting again renews nothing. A gone connection drops it.
    fn send_to_judge(&mut self, conn: ConnId, id: RpcId, job: impl FnOnce(RpcId) -> Job) {
        let bound = self.limits.jobs_per_conn;
        let judge_full = self.jobs_out >= self.limits.calls_in_all();
        let Some(session) = self.session(conn) else {
            return;
        };
        if judge_full || session.calls.len() >= bound || session.calls.contains_key(&id) {
            self.reply(conn, &mcp::answer_error(&id, TOO_MANY_CALLS));
            return;
        }
        session.calls.insert(id.clone(), false);
        self.send_job(job(id));
    }

    /// As [`Worker::send_to_judge`], for what the main context will hold: past the bound, past
    /// [`Limits::calls_in_all`] held by every connection and those gone, or with an id already out at either end, a call
    /// is answered here. A place comes free when the call is answered or once nvim owes it nothing, so a client that
    /// withdraws its calls or connects again gets no more out to nvim. One the host gave up on a slow nvim comes free too,
    /// with its frame still queued there. The event that is no call only says something in the message area, so a
    /// window behind goes without it.
    fn send_to_host(&mut self, event: Event) {
        let Some((conn, id)) = call_of(&event).map(|(conn, id)| (conn, id.clone())) else {
            if !self.window_behind() {
                self.emit(event);
            }
            return;
        };
        let of_this = self.held.keys().filter(|(of, _)| *of == conn).count();
        let call = (conn, id);
        let full = self.window_behind()
            || self.held.len() >= self.limits.calls_in_all()
            || of_this >= self.limits.jobs_per_conn
            || self.held.contains_key(&call);
        let Some(session) = self.session(conn) else {
            return;
        };
        if full || session.calls.contains_key(&call.1) {
            self.reply(conn, &mcp::answer_error(&call.1, TOO_MANY_CALLS));
            return;
        }
        self.held.insert(call, false);
        self.emit(event);
    }

    /// Whether the main context has as many events waiting as all connections may have calls out. Past this nothing
    /// new is handed to it, and no new connection opens, since each brings events of its own.
    fn window_behind(&self) -> bool {
        self.events_waiting.load(Ordering::Relaxed) >= self.limits.calls_in_all()
    }

    /// The main context has answered a call.
    fn let_go(&mut self, conn: ConnId, id: &RpcId) {
        self.held.remove(&(conn, id.clone()));
    }

    fn mark_ready(&mut self, conn: ConnId) {
        let newest = self.selection.as_ref().map(|newest| Rc::clone(&newest.frame));
        if let Some(session) = self.session(conn) {
            if !std::mem::replace(&mut session.ready, true) {
                session.selection = newest;
            }
        }
    }

    fn on_cancelled(&mut self, conn: ConnId, request: RpcId) {
        let Some(session) = self.session(conn) else {
            return;
        };
        match session.calls.get_mut(&request) {
            Some(withdrawn) => *withdrawn = true,
            None => {
                // An id out nowhere has nothing to withdraw, and a client's cancels are no reason to wake the main context.
                let call = self.held.get_mut(&(conn, request.clone()));
                if call.is_some_and(|left| !std::mem::replace(left, true)) {
                    self.emit(Event::Cancelled { conn, id: request });
                }
            }
        }
    }

    /// A frame over what the client reads, or over what the queue may hold, is an error on its id: one large accepted
    /// text falls back to the terminal prompt and costs no connection.
    fn answer(&mut self, conn: ConnId, id: &RpcId, answer: &ToolAnswer) {
        let frame = text_frame(&mcp::answer_tool(id, answer));
        if frame.len() > self.limits.answer_frame_bytes.min(self.limits.outbound_bytes) {
            self.reply(conn, &mcp::answer_error(id, OVER_THE_ANSWER_CAP));
        } else {
            self.queue(conn, &frame);
        }
    }

    fn reply(&mut self, conn: ConnId, message: &[u8]) {
        self.queue(conn, &text_frame(message));
    }

    /// Past the outbound bound the connection closes. A gone connection drops the frame.
    fn queue(&mut self, conn: ConnId, frame: &[u8]) {
        let Some(entry) = self.conns.get_mut(&conn) else {
            return;
        };
        entry.queue.extend(frame);
        if entry.queue.len() > self.limits.outbound_bytes {
            self.end(conn);
        }
    }

    fn session(&mut self, conn: ConnId) -> Option<&mut Session> {
        match &mut self.conns.get_mut(&conn)?.stage {
            Stage::Open(session) => Some(session),
            Stage::Upgrading { .. } => None,
        }
    }

    fn open_count(&self) -> usize {
        self.conns.values().filter(|conn| conn.is_open()).count()
    }

    /// Ends a connection, whichever stage it is in, after one try at what is queued: a client that is not reading
    /// holds nothing up, and one that is has what the socket took.
    fn end(&mut self, id: ConnId) {
        let Some(mut conn) = self.conns.remove(&id) else {
            return;
        };
        let _ = conn.flush();
        if conn.is_open() {
            self.held
                .iter_mut()
                .filter(|(call, _)| call.0 == id)
                .for_each(|(_, left)| *left = true);
            self.emit(Event::ClientGone { conn: id });
            if self.open_count() == 0 {
                self.mentions.close();
                if std::mem::replace(&mut self.mention_up, false) {
                    self.emit(Event::MentionDown);
                }
            }
        }
    }

    fn expire(&mut self, now: Instant) {
        self.conns
            .retain(|_, conn| conn.upgrade_deadline().is_none_or(|until| until > now));
    }

    fn flush_all(&mut self) {
        let failed: Vec<ConnId> = self
            .conns
            .iter_mut()
            .filter(|(_, conn)| conn.has_bytes_waiting())
            .filter_map(|(id, conn)| conn.flush().is_err().then_some(*id))
            .collect();
        for id in failed {
            self.end(id);
        }
    }

    /// The lock file first, so nothing can write it back once the port has gone.
    fn release(&mut self) {
        quietly("the keeper", || self.keeper.finish());
        quietly("the mention socket", || self.mentions.close());
    }

    fn close_all(&mut self) {
        for (id, mut conn) in std::mem::take(&mut self.conns) {
            let _ = conn.flush();
            if conn.is_open() {
                self.emit(Event::ClientGone { conn: id });
            }
        }
    }

    fn stop(&mut self) {
        self.life = Life::Ended;
        self.release();
        self.close_all();
    }
}

impl Conn {
    fn is_open(&self) -> bool {
        matches!(self.stage, Stage::Open(_))
    }

    fn is_ready(&self) -> bool {
        matches!(&self.stage, Stage::Open(session) if session.ready)
    }

    /// How much of its head has arrived, while it is upgrading.
    fn upgrading_with(&self) -> Option<usize> {
        match &self.stage {
            Stage::Upgrading { head, .. } => Some(head.len()),
            Stage::Open(_) => None,
        }
    }

    fn upgrade_deadline(&self) -> Option<Instant> {
        match self.stage {
            Stage::Upgrading { until, .. } => Some(until),
            Stage::Open(_) => None,
        }
    }

    fn has_bytes_waiting(&self) -> bool {
        !self.queue.is_empty() || matches!(&self.stage, Stage::Open(session) if session.selection.is_some())
    }

    /// Writes until the socket is full. The selection stays in its slot, where a newer one can replace it, until its
    /// first byte has gone; what is left of it then is a queued frame like any other.
    fn flush(&mut self) -> io::Result<()> {
        loop {
            if self.queue.is_empty() {
                if self.queue.capacity() > QUEUE_KEPT {
                    self.queue.shrink_to(QUEUE_KEPT);
                }
                let Stage::Open(session) = &mut self.stage else {
                    return Ok(());
                };
                let Some(frame) = &session.selection else {
                    return Ok(());
                };
                let Some(written) = write_some(&mut self.stream, frame)? else {
                    return Ok(());
                };
                self.queue.extend(&frame[written..]);
                session.selection = None;
            } else {
                let (front, _) = self.queue.as_slices();
                let Some(written) = write_some(&mut self.stream, front)? else {
                    return Ok(());
                };
                self.queue.drain(..written);
            }
        }
    }
}

/// `None`: the socket is full.
fn write_some(stream: &mut TcpStream, bytes: &[u8]) -> io::Result<Option<usize>> {
    loop {
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(written) => return Ok(Some(written)),
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// The call an event hands the main context: `None` for an event no call is waiting on.
fn call_of(event: &Event) -> Option<(ConnId, &RpcId)> {
    match event {
        Event::Diagnostics { conn, id, .. }
        | Event::OpenDiff { conn, id, .. }
        | Event::CloseTab { conn, id, .. }
        | Event::CloseAll { conn, id } => Some((*conn, id)),
        Event::Started(_)
        | Event::Cancelled { .. }
        | Event::ClientGone { .. }
        | Event::MentionUp { .. }
        | Event::MentionDown
        | Event::MentionOutside
        | Event::Lost => None,
    }
}

fn set_send_buffer(stream: &TcpStream, bytes: usize) {
    let value = bytes.min(libc::c_int::MAX as usize) as libc::c_int;
    // SAFETY: `setsockopt` reads `size_of::<c_int>()` bytes of `value`, which lives for the call. A refusal leaves the
    // system's size, which is only ever a slower way to fill.
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}
