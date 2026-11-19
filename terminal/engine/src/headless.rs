//! A terminal whose PTY, parser, replies and attachment ordering belong to one worker thread.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::outbox::Outbox;
use crate::pty::{set_nonblocking, HANGUP_GRACE};
use crate::session::{BOUNDARY_WAIT, SYNC_UPDATE_TIMEOUT};
use crate::{ExitInfo, PtyChild, PtySize, Screen, Snapshot, SpawnExtras, SpawnSpec, TerminalColors};

mod queue;
mod snap;
pub use queue::Attachment;

pub const TERM_COLUMNS_MAX: u16 = 1000;
pub const TERM_ROWS_MAX: u16 = 500;
const READ_SLICE: usize = 16 * 1024;
pub type ClientId = String;

pub struct HeadlessConfig {
    pub spawn: SpawnSpec,
    pub extras: SpawnExtras,
    pub size: PtySize,
    pub colors: TerminalColors,
    pub queue_budget: usize,
    pub snapshot_cap: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachWhy {
    Replaced,
    Closed,
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitWhy {
    OpenSequence,
    TooDeep,
    SnapshotTooLarge,
}

#[derive(Debug)]
pub enum StreamItem {
    Snapshot(Snapshot),
    Output { offset: u64, bytes: Vec<u8> },
    Resized { offset: u64, size: PtySize },
    InputWritten(u64),
    Exited(ExitInfo),
    Detached(DetachWhy),
    Waiting(WaitWhy),
}

/// Raw process facts. A presentation layer must escape hidden characters before showing text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub busy: bool,
    pub foreground_name: Option<OsString>,
}
#[derive(Debug, Clone)]
pub struct HeadlessStatus {
    pub alive: bool,
    pub exit: Option<ExitInfo>,
    pub pid: u32,
    pub size: PtySize,
    pub attached: Option<ClientId>,
    pub detached_since: Option<Instant>,
    pub job: Option<Job>,
    pub cwd: Option<PathBuf>,
    pub offset: u64,
}

pub struct HeadlessTerminal {
    door: Arc<Door>,
    status: Arc<Mutex<HeadlessStatus>>,
    thread: Option<JoinHandle<Option<ExitInfo>>>,
    generation: AtomicU64,
    queue_budget: usize,
    master: std::os::fd::OwnedFd,
    shell_pid: u32,
    #[cfg(target_os = "linux")]
    child_pin: Option<(std::os::fd::OwnedFd, u64)>,
    #[cfg(test)]
    counts: Arc<Counts>,
}

#[cfg(test)]
#[derive(Default)]
struct Counts {
    turns: AtomicU64,
    feeds: AtomicU64,
    snapshots: AtomicU64,
    resize_markers: AtomicU64,
    mismatches: AtomicU64,
}

struct Door {
    tx: mpsc::Sender<Command>,
    bell: std::io::PipeWriter,
    closed: Mutex<bool>,
}
impl Door {
    fn send(&self, command: Command) -> bool {
        let closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
        if !*closed && self.tx.send(command).is_ok() {
            let _ = (&self.bell).write(&[1]);
            true
        } else {
            false
        }
    }
}
enum Command {
    Attach {
        id: ClientId,
        generation: u64,
        queue: Arc<Mutex<queue::Queue>>,
        size: Option<PtySize>,
        colors: Option<TerminalColors>,
    },
    Input(ClientId, u64, Vec<u8>),
    Detach(ClientId),
    Dropped(u64),
    Resize(PtySize),
    Colors(TerminalColors),
    Close(Duration),
    Wake,
    #[cfg(test)]
    Panic,
    #[cfg(test)]
    FailPoll,
    #[cfg(test)]
    Probe(Box<dyn FnOnce(&mut Worker) + Send>),
}

impl HeadlessTerminal {
    pub fn spawn(config: HeadlessConfig) -> io::Result<Self> {
        let size = clamp(config.size);
        let pty = PtyChild::spawn_with(&config.spawn, size, &config.extras)?;
        // Before the worker exists, so the child cannot have been reaped and its pid still names it.
        #[cfg(target_os = "linux")]
        let child_pin = pin_child(pty.pid());
        let master = pty.master_fd().try_clone_to_owned()?;
        let shell_pid = pty.pid();
        let (bell_rx, bell_tx) = std::io::pipe()?;
        set_nonblocking(bell_rx.as_fd())?;
        set_nonblocking(bell_tx.as_fd())?;
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(HeadlessStatus {
            alive: true,
            exit: None,
            pid: pty.pid(),
            size,
            attached: None,
            detached_since: Some(Instant::now()),
            job: None,
            cwd: None,
            offset: 0,
        }));
        let shared = status.clone();
        let door = Arc::new(Door {
            tx,
            bell: bell_tx,
            closed: Mutex::new(false),
        });
        let thread_door = door.clone();
        #[cfg(test)]
        let counts = Arc::new(Counts::default());
        #[cfg(test)]
        let worker_counts = counts.clone();
        let thread = std::thread::Builder::new()
            .name(format!("terminal-headless-{}", pty.pid()))
            .spawn(move || {
                let mut worker = Worker {
                    pty,
                    screen: Screen::headless(size, config.colors),
                    colors: config.colors,
                    size,
                    offset: 0,
                    output_while_detached: false,
                    snapshot_cap: config.snapshot_cap,
                    client: None,
                    inputs: HashMap::new(),
                    outbox: Outbox::new(),
                    exit: None,
                    eof: false,
                    eof_since: None,
                    reaped: false,
                    last_reap: None,
                    sync_since: None,
                    waiting: None,
                    boundary_since: None,
                    waiting_announced: false,
                    snapshot_attempt: None,
                    shared,
                    rx,
                    bell: bell_rx,
                    #[cfg(test)]
                    counts: worker_counts,
                    #[cfg(test)]
                    fail_poll: false,
                };
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run())) {
                    Ok(exit) => exit,
                    Err(_) => {
                        worker.shutdown(HANGUP_GRACE);
                        worker.exit = Some(ExitInfo::UNKNOWN);
                        if let Some(c) = &worker.client {
                            c.queue
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .failed(ExitInfo::UNKNOWN);
                        }
                        worker.update_status();
                        Some(ExitInfo::UNKNOWN)
                    }
                };
                // Close admission before draining queued attaches; every admitted client gets a final item.
                *thread_door.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
                while let Ok(command) = worker.rx.try_recv() {
                    if let Command::Attach { queue, .. } = command {
                        queue
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .failed(worker.exit.unwrap_or(ExitInfo::UNKNOWN));
                    }
                }
                result
            })?;
        Ok(Self {
            door,
            status,
            thread: Some(thread),
            generation: AtomicU64::new(0),
            queue_budget: config.queue_budget,
            master,
            shell_pid,
            #[cfg(target_os = "linux")]
            child_pin,
            #[cfg(test)]
            counts,
        })
    }

    /// Replaces the previous attachment, discarding its input still waiting for a PTY write.
    pub fn attach(&self, client: ClientId) -> io::Result<Attachment> {
        self.attach_with(client, None, None)
    }

    /// Applies the incoming client's size and colours between detaching its predecessor and taking its snapshot.
    pub fn attach_with(
        &self,
        client: ClientId,
        size: Option<PtySize>,
        colors: Option<TerminalColors>,
    ) -> io::Result<Attachment> {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let a = queue::pair(self.queue_budget, self.door.clone(), generation)?;
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            a.queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .failed(self.status().exit.unwrap_or(ExitInfo::UNKNOWN));
        } else if !self.door.send(Command::Attach {
            id: client,
            generation,
            queue: a.queue.clone(),
            size,
            colors,
        }) {
            a.queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .failed(self.status().exit.unwrap_or(ExitInfo::UNKNOWN));
        }
        Ok(a)
    }
    pub fn input(&self, client: &ClientId, offset: u64, bytes: Vec<u8>) {
        self.door.send(Command::Input(client.clone(), offset, bytes));
    }
    pub fn detach(&self, client: &ClientId) {
        self.door.send(Command::Detach(client.clone()));
    }
    pub fn resize(&self, size: PtySize) {
        self.door.send(Command::Resize(size));
    }
    pub fn set_colors(&self, colors: TerminalColors) {
        self.door.send(Command::Colors(colors));
    }
    pub fn status(&self) -> HeadlessStatus {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner).clone();
        // Process facts are sampled by the caller; the parser worker only publishes cheap state.
        let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        let pid = self.shell_pid;
        if group > 0 {
            let name = read_bounded(&format!("/proc/{group}/comm"), 16).map(|mut b| {
                if b.last() == Some(&b'\n') {
                    b.pop();
                }
                OsString::from_vec(b)
            });
            status.cwd = std::fs::read_link(format!("/proc/{group}/cwd"))
                .or_else(|_| std::fs::read_link(format!("/proc/{pid}/cwd")))
                .ok()
                .filter(|p| p.as_os_str().len() <= 4096);
            status.job = Some(Job {
                busy: group as u32 != pid,
                foreground_name: name,
            });
        }
        status
    }
    /// The child pinned by a pidfd opened right after its spawn, before the worker could reap it, with its start
    /// time (clock ticks since boot, `/proc/<pid>/stat` field 22) read then: the identity of this very child, never a
    /// later process given its pid. `None` where that was not possible.
    #[cfg(target_os = "linux")]
    pub fn child_pin(&self) -> Option<(std::os::fd::OwnedFd, u64)> {
        let (fd, start) = self.child_pin.as_ref()?;
        Some((fd.try_clone().ok()?, *start))
    }

    pub fn close(mut self, grace: Duration) -> Option<ExitInfo> {
        self.door.send(Command::Close(grace));
        self.thread.take().and_then(|t| t.join().ok()).flatten()
    }
}
impl Drop for HeadlessTerminal {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.door.send(Command::Close(HANGUP_GRACE));
        }
    }
}

struct Client {
    id: ClientId,
    generation: u64,
    queue: Arc<Mutex<queue::Queue>>,
    mismatch_logged: bool,
}
#[derive(Default)]
struct Input {
    expected: u64,
    written: u64,
    generation: u64,
}
struct SnapshotAttempt {
    at: Instant,
    size: PtySize,
    offset: u64,
}

struct Worker {
    pty: PtyChild,
    screen: Screen,
    colors: TerminalColors,
    size: PtySize,
    offset: u64,
    output_while_detached: bool,
    snapshot_cap: usize,
    client: Option<Client>,
    inputs: HashMap<ClientId, Input>,
    outbox: Outbox,
    exit: Option<ExitInfo>,
    eof: bool,
    eof_since: Option<Instant>,
    reaped: bool,
    last_reap: Option<Instant>,
    sync_since: Option<(u64, Instant)>,
    waiting: Option<WaitWhy>,
    boundary_since: Option<Instant>,
    waiting_announced: bool,
    snapshot_attempt: Option<SnapshotAttempt>,
    shared: Arc<Mutex<HeadlessStatus>>,
    rx: mpsc::Receiver<Command>,
    bell: std::io::PipeReader,
    #[cfg(test)]
    counts: Arc<Counts>,
    #[cfg(test)]
    fail_poll: bool,
}

fn clamp(size: PtySize) -> PtySize {
    PtySize {
        cols: size.cols.clamp(1, TERM_COLUMNS_MAX),
        rows: size.rows.clamp(1, TERM_ROWS_MAX),
        ..size
    }
}
fn exit_info(status: std::process::ExitStatus) -> ExitInfo {
    ExitInfo {
        code: status.code(),
        signal: status.signal(),
        detached: false,
    }
}

impl Worker {
    fn run(&mut self) -> Option<ExitInfo> {
        let mut buf = [0u8; READ_SLICE];
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: if self.eof { -1 } else { self.pty.master_fd().as_raw_fd() },
                    events: libc::POLLIN | if self.outbox.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.bell.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if let Err(error) = self.poll(&mut fds) {
                if error.kind() != io::ErrorKind::Interrupted {
                    let exit = self.shutdown(HANGUP_GRACE).unwrap_or(ExitInfo::UNKNOWN);
                    self.exit = Some(exit);
                    self.update_status();
                    if let Some(client) = &self.client {
                        client.queue.lock().unwrap_or_else(PoisonError::into_inner).failed(exit);
                    }
                    return Some(exit);
                }
            }
            #[cfg(test)]
            self.counts.turns.fetch_add(1, Ordering::Relaxed);
            let mut drain = [0u8; 64];
            while matches!(self.bell.read(&mut drain), Ok(n) if n > 0) {}
            // Commands always precede writes, so a stream ending drops its pending input first.
            while let Ok(command) = self.rx.try_recv() {
                match command {
                    Command::Attach {
                        id,
                        generation,
                        queue,
                        size,
                        colors,
                    } => {
                        self.drop_client(Some(DetachWhy::Replaced));
                        if let Some(size) = size {
                            self.resize(size);
                        }
                        if let Some(colors) = colors {
                            self.set_colors(colors);
                        }
                        if !self.inputs.contains_key(&id) && self.inputs.len() >= 16 {
                            let oldest = self
                                .inputs
                                .iter()
                                .min_by_key(|(_, input)| input.generation)
                                .map(|(id, _)| id.clone())
                                .unwrap();
                            self.inputs.remove(&oldest);
                        }
                        let returning = self.inputs.contains_key(&id);
                        let input = self.inputs.entry(id.clone()).or_default();
                        input.expected = input.written;
                        input.generation = generation;
                        let written = input.written;
                        self.client = Some(Client {
                            id,
                            generation,
                            queue,
                            mismatch_logged: false,
                        });
                        // A returning client needs its starting offset before a wait can open its input gate.
                        if returning {
                            self.publish(StreamItem::InputWritten(written));
                        }
                    }
                    Command::Input(id, offset, bytes) => {
                        if self.exit.is_none() && self.client.as_ref().is_some_and(|c| c.id == id) {
                            let input = self.inputs.get_mut(&id).unwrap();
                            if offset == input.expected {
                                if let Some(end) = offset.checked_add(bytes.len() as u64) {
                                    input.expected = end;
                                    self.outbox.push_input(&bytes);
                                }
                            } else if !self.client.as_ref().unwrap().mismatch_logged {
                                self.client.as_mut().unwrap().mismatch_logged = true;
                                #[cfg(test)]
                                self.counts.mismatches.fetch_add(1, Ordering::Relaxed);
                                // Both numbers are byte offsets the client counted, never the bytes themselves.
                                let expected = input.expected;
                                eprintln!("[terminal] input offset mismatch: expected {expected}, got {offset}");
                            }
                        }
                    }
                    Command::Detach(id) => {
                        if self.client.as_ref().is_some_and(|c| c.id == id) {
                            self.drop_client(None);
                        }
                    }
                    Command::Dropped(generation) => {
                        if self.client.as_ref().is_some_and(|c| c.generation == generation) {
                            self.drop_client(None);
                        }
                    }
                    Command::Resize(size) => self.resize(size),
                    Command::Colors(colors) => self.set_colors(colors),
                    Command::Close(grace) => {
                        let exit = self.shutdown(grace);
                        self.publish(StreamItem::Detached(DetachWhy::Closed));
                        return exit;
                    }
                    Command::Wake => {}
                    #[cfg(test)]
                    Command::Panic => panic!("injected headless failure"),
                    #[cfg(test)]
                    Command::FailPoll => self.fail_poll = true,
                    #[cfg(test)]
                    Command::Probe(probe) => probe(self),
                }
            }
            self.track_sync();
            self.snapshot_if_ready();
            if !self.eof {
                self.flush();
                if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    match self.pty.read(&mut buf) {
                        Ok(0) => self.eof = true,
                        Ok(n) => self.feed(&buf[..n]),
                        Err(e) if e.raw_os_error() == Some(libc::EIO) => self.eof = true,
                        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                        Err(_) => self.eof = true,
                    }
                }
                self.flush();
            }
            if self.eof && !self.reaped {
                let since = *self.eof_since.get_or_insert_with(Instant::now);
                let ended = match self.pty.try_wait() {
                    Ok(Some(status)) => {
                        self.reaped = true;
                        Some(exit_info(status))
                    }
                    Err(_) => {
                        self.reaped = true;
                        Some(ExitInfo::UNKNOWN)
                    }
                    Ok(None) if since.elapsed() >= Duration::from_millis(200) => Some(ExitInfo {
                        detached: true,
                        ..ExitInfo::UNKNOWN
                    }),
                    _ => None,
                };
                self.last_reap = Some(Instant::now());
                if let Some(exit) = ended {
                    if self.exit != Some(exit) {
                        self.exit = Some(exit);
                        self.finish_snapshot();
                        self.publish(StreamItem::Exited(exit));
                    }
                }
                self.finish_snapshot();
            }
            self.update_status();
        }
    }

    fn resize(&mut self, size: PtySize) {
        let size = clamp(size);
        if self.pty.resize(size).is_ok() {
            self.screen.resize(size);
            self.size = size;
            self.publish(StreamItem::Resized {
                offset: self.offset,
                size,
            });
        }
    }

    fn set_colors(&mut self, colors: TerminalColors) {
        self.colors = colors;
        self.screen.set_colors(colors);
    }

    fn drop_client(&mut self, why: Option<DetachWhy>) {
        if let Some(c) = self.client.take() {
            if let Some(why) = why {
                c.queue
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(StreamItem::Detached(why));
            }
            self.outbox.discard_input();
            self.output_while_detached = false;
            self.waiting = None;
            self.boundary_since = None;
            self.waiting_announced = false;
            self.snapshot_attempt = None;
        }
    }
    fn publish(&mut self, item: StreamItem) {
        if let Some(c) = &self.client {
            #[cfg(test)]
            if matches!(item, StreamItem::Resized { .. }) {
                self.counts.resize_markers.fetch_add(1, Ordering::Relaxed);
            }
            c.queue.lock().unwrap_or_else(PoisonError::into_inner).push(item);
        }
    }
    fn feed_part(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        self.counts.feeds.fetch_add(1, Ordering::Relaxed);
        self.screen.feed(bytes);
        self.outbox.push_reply(self.screen.take_replies());
        self.screen.take_events();
        let offset = self.offset;
        self.offset += bytes.len() as u64;
        if self.client.is_none() {
            self.output_while_detached = true;
        } else {
            self.publish(StreamItem::Output {
                offset,
                bytes: bytes.to_vec(),
            });
        }
    }
    fn feed(&mut self, bytes: &[u8]) {
        let mut start = 0;
        while start < bytes.len() && self.snapshot_owed() && self.waiting == Some(WaitWhy::OpenSequence) {
            self.feed_part(&bytes[start..start + 1]);
            start += 1;
            self.track_sync();
            self.snapshot_if_ready();
        }
        if start < bytes.len() {
            self.feed_part(&bytes[start..]);
        }
        self.track_sync();
        self.snapshot_if_ready();
    }
    fn flush(&mut self) {
        let before = self.outbox.input_written();
        // Yield after one parse slice of writes, even for a child consuming a large paste quickly.
        let mut budget = READ_SLICE;
        self.outbox.flush(|bytes| {
            if budget == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = self.pty.write(&bytes[..bytes.len().min(budget)])?;
            budget -= n;
            Ok(n)
        });
        let n = self.outbox.input_written() - before;
        if n != 0 {
            if let Some(c) = &self.client {
                let input = self.inputs.get_mut(&c.id).unwrap();
                input.written += n;
                let written = input.written;
                self.publish(StreamItem::InputWritten(written));
            }
        }
    }
    fn track_sync(&mut self) {
        let Some(ordinal) = self.screen.open_update() else {
            self.sync_since = None;
            return;
        };
        let (seen, since) = self.sync_since.get_or_insert((ordinal, Instant::now()));
        if *seen != ordinal {
            *seen = ordinal;
            *since = Instant::now();
        }
        if since.elapsed() >= SYNC_UPDATE_TIMEOUT {
            self.screen.abort_sync();
            self.sync_since = None;
        }
    }
    fn update_status(&self) {
        let mut status = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        status.alive = self.exit.is_none_or(|e| e.detached);
        status.exit = self.exit;
        status.size = self.size;
        status.attached = self.client.as_ref().map(|c| c.id.clone());
        status.offset = self.offset;
        if self.client.is_some() {
            status.detached_since = None;
        } else if status.detached_since.is_none() {
            status.detached_since = Some(Instant::now());
        }
    }

    fn poll_timeout(&self) -> i32 {
        let mut deadline = self.sync_since.map(|(_, since)| since + SYNC_UPDATE_TIMEOUT);
        let mut include = |next: Instant| {
            deadline = Some(deadline.map_or(next, |old| old.min(next)));
        };
        if let Some(since) = self.boundary_since {
            if !self.waiting_announced && !self.eof {
                include(since + BOUNDARY_WAIT);
            }
        }
        // A read during the cooldown can make the replay fit. An unchanged oversized screen can stay asleep.
        if let Some(attempt) = &self.snapshot_attempt
            && self.offset != attempt.offset
            && self.waiting == Some(WaitWhy::SnapshotTooLarge)
            && self.snapshot_owed()
        {
            include(attempt.at + snap::RETRY_AFTER);
        }
        if self.eof && !self.reaped {
            if let Some(since) = self.eof_since {
                include(if self.exit.is_none() {
                    since + Duration::from_millis(200)
                } else {
                    self.last_reap.unwrap_or(since) + Duration::from_secs(1)
                });
            }
        }
        deadline.map_or(-1, |at| {
            let left = at.saturating_duration_since(Instant::now());
            left.as_millis()
                .saturating_add(u128::from(left.subsec_nanos() % 1_000_000 != 0))
                .min(i32::MAX as u128) as i32
        })
    }

    fn poll(&self, fds: &mut [libc::pollfd]) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_poll {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, self.poll_timeout()) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn finish_snapshot(&mut self) {
        if let Some(client) = &self.client {
            let mut queue = client.queue.lock().unwrap_or_else(PoisonError::into_inner);
            if queue.owed {
                queue.demand = true;
            }
        }
        self.track_sync();
        self.snapshot_if_ready();
    }

    fn shutdown(&mut self, grace: Duration) -> Option<ExitInfo> {
        self.outbox.discard_input();
        if let Ok(Some(status)) = self.pty.try_wait() {
            self.exit = Some(exit_info(status));
        } else {
            self.pty.hangup();
            let until = Instant::now() + grace;
            loop {
                match self.pty.try_wait() {
                    Ok(Some(status)) => {
                        self.exit = Some(exit_info(status));
                        break;
                    }
                    _ if Instant::now() >= until => {
                        self.pty.kill();
                        self.exit = self.pty.wait().ok().map(exit_info);
                        break;
                    }
                    _ => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        }
        self.update_status();
        self.exit
    }
}

/// A pidfd for `pid` and its start time, read while the caller still holds the unreaped child.
#[cfg(target_os = "linux")]
fn pin_child(pid: u32) -> Option<(std::os::fd::OwnedFd, u64)> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open returns a new descriptor or -1 and touches no Rust memory.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: the descriptor was just opened and is owned here alone.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let end = stat.iter().rposition(|b| *b == b')')?;
    let start = std::str::from_utf8(&stat[end + 1..])
        .ok()?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    Some((fd, start))
}

fn read_bounded(path: &str, cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    File::open(path)
        .ok()?
        .take((cap + 1) as u64)
        .read_to_end(&mut out)
        .ok()?;
    (out.len() <= cap).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn config(script: &str) -> HeadlessConfig {
        HeadlessConfig {
            spawn: SpawnSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: "/tmp".into(),
                env: vec![],
                extra_env: vec![],
            },
            extras: SpawnExtras::default(),
            size: PtySize {
                cols: 80,
                rows: 24,
                cell_width_px: 8,
                cell_height_px: 16,
            },
            colors: TerminalColors::default(),
            queue_budget: 4 * 1024 * 1024,
            snapshot_cap: 32 * 1024 * 1024,
        }
    }

    fn wait(mut ready: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(60);
        while !ready() {
            assert!(Instant::now() < until, "worker did not make progress");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn next(a: &Attachment) -> StreamItem {
        let mut item = None;
        wait(|| {
            item = a.next();
            item.is_some()
        });
        item.unwrap()
    }

    fn probe<T: Send + 'static>(term: &HeadlessTerminal, f: impl FnOnce(&mut Worker) -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        assert!(term.door.send(Command::Probe(Box::new(move |worker| {
            let _ = tx.send(f(worker));
        }))));
        rx.recv_timeout(Duration::from_secs(60)).unwrap()
    }

    // The pin is read from /proc, which only Linux has.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_child_is_pinned_at_its_spawn() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        let (_fd, start) = term.child_pin().unwrap();
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", term.status().pid)).unwrap();
        let recorded: u64 = stat[stat.rfind(')').unwrap() + 1..]
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(start, recorded);
        term.close(Duration::ZERO);
    }

    #[test]
    fn replacement_delivers_no_resize_to_the_previous_attachment() {
        let config = config("exec sleep 60");
        let size = PtySize {
            cols: config.size.cols + 10,
            ..config.size
        };
        let term = HeadlessTerminal::spawn(config).unwrap();
        let first = term.attach("first".into()).unwrap();
        assert!(matches!(next(&first), StreamItem::Snapshot(_)));
        let second = term.attach_with("second".into(), Some(size), None).unwrap();
        assert!(matches!(next(&second), StreamItem::Snapshot(s) if s.replay.columns == size.cols));
        assert_eq!(term.counts.resize_markers.load(Ordering::Relaxed), 0);
        assert!(matches!(next(&first), StreamItem::Detached(DetachWhy::Replaced)));
        assert!(first.next().is_none());
        term.close(Duration::ZERO);
    }

    #[test]
    fn a_deferred_snapshot_retries_without_more_output() {
        let config = config("exec sleep 60");
        let mut empty = Screen::headless(config.size, config.colors);
        let cap = empty.read_for_move().unwrap().bytes.len();
        let term = HeadlessTerminal::spawn(HeadlessConfig {
            snapshot_cap: cap,
            ..config
        })
        .unwrap();
        probe(&term, |worker| worker.feed(&[b'x'; 100]));
        let a = term.attach("client".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::SnapshotTooLarge)));
        let (attempts, timeout) = probe(&term, |worker| {
            assert_eq!(worker.poll_timeout(), -1);
            worker.snapshot_attempt.as_mut().unwrap().at = Instant::now();
            worker.feed(b"\x1bc");
            (worker.counts.snapshots.load(Ordering::Relaxed), worker.poll_timeout())
        });
        assert!(timeout >= 0, "a deferred retry needs a poll deadline");
        assert!(matches!(next(&a), StreamItem::Snapshot(_)));
        assert_eq!(term.counts.snapshots.load(Ordering::Relaxed), attempts + 1);
        term.close(Duration::ZERO);
    }

    #[test]
    fn a_short_csi_resync_finishes_before_announcing_waiting() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        let a = term.attach("client".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Snapshot(_)));
        probe(&term, |worker| {
            worker.feed(b"\x1b[");
            let queue = worker.client.as_ref().unwrap().queue.clone();
            let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
            let budget = q.budget();
            q.push(StreamItem::Output {
                offset: 2,
                bytes: vec![0; budget],
            });
        });
        while a.next().is_some() {}
        probe(&term, |worker| {
            worker.snapshot_if_ready();
            worker.feed(b"31mred");
        });
        assert!(matches!(next(&a), StreamItem::Snapshot(_)));
        term.close(Duration::ZERO);
    }

    #[test]
    fn boundary_wait_is_timed_but_long_sequences_are_announced_immediately() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        probe(&term, |worker| worker.feed(b"\x1b["));
        let a = term.attach("client".into()).unwrap();
        probe(&term, |worker| {
            worker.snapshot_if_ready();
            assert_eq!(worker.waiting, Some(WaitWhy::OpenSequence));
            assert!(!worker.waiting_announced);
            assert!(worker.poll_timeout() >= 0);
            worker.boundary_since = Some(Instant::now() - BOUNDARY_WAIT);
            assert_eq!(worker.poll_timeout(), 0);
            worker.snapshot_if_ready();
            assert_eq!(worker.poll_timeout(), -1);
        });
        assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::OpenSequence)));
        term.close(Duration::ZERO);
    }

    #[test]
    fn depth_waits_feed_reads_whole_and_retry_once_per_read() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        probe(&term, |worker| worker.feed(&b"\x1b[>1u".repeat(4096)));
        let a = term.attach("client".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::TooDeep)));
        let counts = term.counts.clone();
        probe(&term, move |worker| {
            let before = counts.feeds.load(Ordering::Relaxed);
            worker.feed(&[b'x'; 1024]);
            assert_eq!(counts.feeds.load(Ordering::Relaxed) - before, 1);
            worker.feed(b"\x1b[<1u");
        });
        assert!(matches!(next(&a), StreamItem::Snapshot(_)));
        term.close(Duration::ZERO);
    }

    #[test]
    fn a_returning_client_learns_its_written_count_before_a_wait() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        probe(&term, |worker| worker.feed(&b"\x1b[>1u".repeat(4096)));
        let a = term.attach("client".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::TooDeep)));
        term.input(&"client".into(), 0, b"key".to_vec());
        assert!(matches!(next(&a), StreamItem::InputWritten(3)));
        let b = term.attach("client".into()).unwrap();
        assert!(matches!(next(&b), StreamItem::InputWritten(3)));
        assert!(matches!(next(&b), StreamItem::Waiting(WaitWhy::TooDeep)));
        term.close(Duration::ZERO);
    }

    #[test]
    fn oversized_snapshots_retry_on_resize_or_after_a_second_of_reads() {
        let mut c = config("exec sleep 60");
        c.snapshot_cap = 1;
        let term = HeadlessTerminal::spawn(c).unwrap();
        let a = term.attach("client".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Waiting(WaitWhy::SnapshotTooLarge)));
        let counts = term.counts.clone();
        probe(&term, move |worker| {
            let before = counts.snapshots.load(Ordering::Relaxed);
            for _ in 0..100 {
                worker.feed(b"x");
                worker.feed(b"\x1b[");
                worker.feed(b"31m");
            }
            assert_eq!(counts.snapshots.load(Ordering::Relaxed), before);
            let feeds = counts.feeds.load(Ordering::Relaxed);
            worker.feed(&[b'x'; 1024]);
            assert_eq!(counts.feeds.load(Ordering::Relaxed) - feeds, 1);
            worker.size.cols += 1;
            worker.screen.resize(worker.size);
            worker.feed(b"y");
            assert_eq!(counts.snapshots.load(Ordering::Relaxed), before + 1);
            worker.snapshot_attempt.as_mut().unwrap().at = Instant::now() - snap::RETRY_AFTER;
            worker.feed(b"z");
            assert_eq!(counts.snapshots.load(Ordering::Relaxed), before + 2);
        });
        term.close(Duration::ZERO);
    }

    #[test]
    fn idle_workers_do_not_wake_periodically_or_when_status_is_read() {
        let idle = HeadlessTerminal::spawn(config("printf READY; exec sleep 60")).unwrap();
        let active = HeadlessTerminal::spawn(config(
            "stty raw -echo; printf READY; while dd bs=1 count=1 of=/dev/null 2>/dev/null; do printf x; done",
        ))
        .unwrap();
        wait(|| idle.status().offset == 5 && active.status().offset == 5);
        let a = active.attach("client".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Snapshot(_)));
        let before_idle = idle.counts.turns.load(Ordering::Relaxed);
        let before_active = active.counts.turns.load(Ordering::Relaxed);
        // The interval is workload pacing; work counters, rather than elapsed time, decide the result.
        for i in 0..40 {
            active.input(&"client".into(), i, vec![b'x']);
            wait(|| active.status().offset == 6 + i);
            idle.status();
            std::thread::sleep(Duration::from_millis(50));
        }
        let idle_turns = idle.counts.turns.load(Ordering::Relaxed) - before_idle;
        let active_turns = active.counts.turns.load(Ordering::Relaxed) - before_active;
        assert!(idle_turns <= 3, "idle turns: {idle_turns}");
        assert!(
            active_turns >= 40 && active_turns > idle_turns + 10,
            "active turns: {active_turns}, idle: {idle_turns}"
        );
        idle.close(Duration::ZERO);
        active.close(Duration::ZERO);
    }

    #[test]
    fn poll_failure_delivers_a_final_exit_even_with_a_snapshot_owed() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        let a = term.attach("client".into()).unwrap();
        assert!(matches!(next(&a), StreamItem::Snapshot(_)));
        probe(&term, |worker| {
            let mut q = worker
                .client
                .as_ref()
                .unwrap()
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            q.owed = true;
            q.demand = false;
        });
        term.door.send(Command::FailPoll);
        wait(|| term.thread.as_ref().unwrap().is_finished());
        assert!(matches!(a.next(), Some(StreamItem::Exited(_))));
        term.close(Duration::ZERO);
    }

    #[test]
    fn one_doorbell_drains_more_than_sixty_four_commands() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        let (queued_tx, queued_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        term.door.send(Command::Probe(Box::new(move |_| {
            queued_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        })));
        queued_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        let before = term.counts.turns.load(Ordering::Relaxed);
        for _ in 0..192 {
            term.door.tx.send(Command::Wake).unwrap();
        }
        let (done_tx, done_rx) = mpsc::channel();
        term.door
            .tx
            .send(Command::Probe(Box::new(move |worker| {
                done_tx.send(worker.counts.turns.load(Ordering::Relaxed)).unwrap();
            })))
            .unwrap();
        release_tx.send(()).unwrap();
        let after = done_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(before, after, "all commands run in the same turn");
        term.close(Duration::ZERO);
    }

    #[test]
    fn client_accounting_evicts_the_oldest_and_keeps_the_attached_client() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        let mut attachments = Vec::new();
        for i in 0..20 {
            let a = term.attach(format!("client-{i}")).unwrap();
            assert!(matches!(next(&a), StreamItem::Snapshot(_)));
            attachments.push(a);
        }
        probe(&term, |worker| {
            assert_eq!(worker.inputs.len(), 16);
            assert!(!worker.inputs.contains_key("client-0"));
            assert!(!worker.inputs.contains_key("client-3"));
            assert!(worker.inputs.contains_key("client-4"));
            assert!(worker.inputs.contains_key("client-19"));
        });
        term.close(Duration::ZERO);
    }

    #[test]
    fn mismatched_offsets_log_only_once_per_attachment() {
        let term = HeadlessTerminal::spawn(config("exec sleep 60")).unwrap();
        for i in 1..=2 {
            let a = term.attach("client".into()).unwrap();
            let mut item = next(&a);
            // A returning client may read its written count before the snapshot replaces it.
            if i == 2 && matches!(item, StreamItem::InputWritten(0)) {
                item = next(&a);
            }
            assert!(matches!(item, StreamItem::Snapshot(_)));
            for _ in 0..100 {
                term.input(&"client".into(), 99, vec![1]);
            }
            probe(&term, |_| ());
            assert_eq!(term.counts.mismatches.load(Ordering::Relaxed), i);
        }
        term.close(Duration::ZERO);
    }
}
