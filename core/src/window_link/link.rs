//! The link at run time: one thread per window reads and writes the link's packets, so neither window's GTK thread
//! ever waits on the other. The GTK side queues what it sends and drains what arrived behind a wake descriptor (an
//! eventfd), as the companion's link driver does; an idle link costs a sleeping thread and nothing else.
//!
//! Each received packet is checked before anything else sees it: its header, its body (a sealed memfd within its
//! kind's cap, read and closed here), its kind, and that it carries exactly the descriptors its kind declares. A
//! packet that fails any of these is refused and its descriptors are closed before the refusal is passed on.
//! `presence` is answered here, on the link thread, so its acknowledgement never waits for the GTK side.
//!
//! A packet that cannot be written for any reason but a full socket ends the link: the peer would otherwise wait
//! for an answer that never comes. The thread then shuts the socket both ways, so nothing the peer writes later is
//! accepted, passes on what the peer had already written, and only then reports [`Inbound::Closed`].

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::fdpass::{self, FdKind};
use super::moves::Moves;
use super::msg::{body_cap, check_fds, frame, from_json, read_header, Frame, Header, LinkError, Msg, INLINE_MAX};

/// How long a closing link keeps trying to write what it still has queued (its `bye` last) before it closes anyway.
const CLOSE_FLUSH: Duration = Duration::from_secs(1);
/// The most packets read in one turn of the thread, so a peer that floods the link cannot hold off its writes.
const READS_PER_TURN: usize = 64;

/// Whether the partner counts as another window for turn review: set while the partner says it could not confirm its
/// own presence (`presence{held: false}`), cleared when it says it holds it again, and set for good once the
/// partner says `bye` or the link ends: a window that can no longer be heard is never skipped.
#[derive(Debug, Clone, Default)]
pub struct PresenceFlag(pub Arc<AtomicBool>);

/// What the link thread passes to the GTK side.
#[derive(Debug)]
pub enum Inbound {
    Msg {
        msg: Msg,
        fds: Vec<OwnedFd>,
    },
    /// A packet refused whole; its descriptors are already closed. The receiver logs it; the link stays up.
    Refused(LinkError),
    /// The peer's end-of-file, a read error, or a packet of this window's that could not be written. Every packet
    /// the peer wrote before it is passed on first; nothing arrives after it.
    Closed,
}

struct Outbound {
    /// For the test hook that holds the thread before a write of one kind.
    #[cfg(any(test, feature = "test-support"))]
    kind: &'static str,
    frame: Frame,
    /// The body's memfd, made on the link thread when the packet is first tried.
    body: Option<OwnedFd>,
    fds: Vec<OwnedFd>,
    confirm: Option<Sender<io::Result<()>>>,
}

/// Answers a confirmed send and wakes the GTK side, which settles its moves on a wake as well as reading messages.
fn answer(out: Outbound, result: io::Result<()>, wake: BorrowedFd<'_>) {
    if let Some(confirm) = out.confirm {
        let _ = confirm.send(result);
        signal(wake);
    }
}

enum Command {
    Send(Outbound),
    Shutdown,
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<Command>,
    /// The thread has ended: nothing queued from here on is written.
    closed: bool,
}

fn closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the link closed before this was written")
}

pub(crate) fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: `eventfd` takes no pointers; a negative return is an error.
    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Makes an eventfd readable. The only failure is a counter at its maximum, which is readable already.
pub(crate) fn signal(fd: BorrowedFd<'_>) {
    let one: u64 = 1;
    // SAFETY: writes the 8 bytes of `one`, which lives for the call.
    unsafe { libc::write(fd.as_raw_fd(), (&one as *const u64).cast(), 8) };
}

/// Makes an eventfd unreadable until the next `signal`.
pub(crate) fn clear(fd: BorrowedFd<'_>) {
    let mut count: u64 = 0;
    // SAFETY: reads at most 8 bytes into `count`, which lives for the call; a non-blocking eventfd at zero fails at once.
    unsafe { libc::read(fd.as_raw_fd(), (&mut count as *mut u64).cast(), 8) };
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: `fcntl` on an open descriptor.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn poll(fds: &mut [libc::pollfd], timeout_ms: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: `fds` is a valid array of `fds.len()` entries for the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if rc >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn pollfd(fd: BorrowedFd<'_>, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    }
}

fn outbound(msg: &Msg, fds: Vec<OwnedFd>, confirm: Option<Sender<io::Result<()>>>) -> Result<Outbound, LinkError> {
    let frame = frame(msg)?;
    let body = matches!(frame, Frame::Body { .. });
    check_fds(msg.kind(), body, fds.len() + usize::from(body))?;
    Ok(Outbound {
        #[cfg(any(test, feature = "test-support"))]
        kind: msg.kind(),
        frame,
        body: None,
        fds,
        confirm,
    })
}

/// One window's end of the link.
pub struct LinkHandle {
    shared: Arc<Mutex<Shared>>,
    /// Readable while the GTK side has queued a command for the thread.
    commands: Arc<OwnedFd>,
    /// Readable while received messages wait (see [`LinkHandle::drain_wake`]).
    wake: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

/// The sending side of a link for a thread of its own, which must not touch the GTK side's [`LinkHandle`]. Every
/// clone queues on the same link, and sending never waits.
#[derive(Clone)]
pub struct LinkSender {
    shared: Arc<Mutex<Shared>>,
    commands: Arc<OwnedFd>,
}

impl LinkSender {
    /// Queues `msg`; `false` when its kind's rules refuse it or the link has closed.
    pub fn send(&self, msg: Msg) -> bool {
        let Ok(out) = outbound(&msg, Vec::new(), None) else {
            return false;
        };
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if shared.closed {
            return false;
        }
        shared.queue.push_back(Command::Send(out));
        signal(self.commands.as_fd());
        true
    }

    /// Whether the link thread has stopped. This is ahead of the GTK side, which may have messages to read yet.
    pub fn is_closed(&self) -> bool {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).closed
    }
}

impl LinkHandle {
    /// Starts the link thread on `sock` (one end of the pair, close-on-exec). What arrives comes out of the receiver.
    pub fn start(sock: OwnedFd, presence: PresenceFlag) -> io::Result<(LinkHandle, Receiver<Inbound>)> {
        #[cfg(any(test, feature = "test-support"))]
        return Self::start_inner(sock, presence, None);
        #[cfg(not(any(test, feature = "test-support")))]
        return Self::start_inner(sock, presence);
    }

    fn start_inner(
        sock: OwnedFd,
        presence: PresenceFlag,
        #[cfg(any(test, feature = "test-support"))] park: Option<Arc<park::ParkHook>>,
    ) -> io::Result<(LinkHandle, Receiver<Inbound>)> {
        set_nonblocking(sock.as_fd())?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let commands = Arc::new(eventfd()?);
        let wake = Arc::new(eventfd()?);
        let (tx, rx) = mpsc::channel();
        let thread = LinkThread {
            socket: sock,
            shared: Arc::clone(&shared),
            commands: Arc::clone(&commands),
            wake: Arc::clone(&wake),
            inbound: tx,
            presence,
            out: VecDeque::new(),
            write_failed: false,
            #[cfg(any(test, feature = "test-support"))]
            park,
        };
        let handle = std::thread::Builder::new()
            .name("window-link".into())
            .spawn(move || thread.run())?;
        Ok((
            LinkHandle {
                shared,
                commands,
                wake,
                thread: Some(handle),
            },
            rx,
        ))
    }

    /// A sender for a thread other than the GTK side's.
    pub fn sender(&self) -> LinkSender {
        LinkSender {
            shared: Arc::clone(&self.shared),
            commands: Arc::clone(&self.commands),
        }
    }

    fn push(&self, command: Command) -> Result<(), Command> {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if shared.closed {
            return Err(command);
        }
        shared.queue.push_back(command);
        signal(self.commands.as_fd());
        Ok(())
    }

    /// Queues `msg` with `fds`; never blocks. The descriptors are closed in this process once written, so a caller
    /// that keeps one passes a copy. On a link that has already closed the message is dropped: the receiver has
    /// [`Inbound::Closed`] already. Refused at once when the message is over its kind's cap or its descriptors do
    /// not match its kind.
    pub fn send(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<(), LinkError> {
        let out = outbound(&msg, fds, None)?;
        let _ = self.push(Command::Send(out));
        Ok(())
    }

    /// As [`LinkHandle::send`], and the receiver answers once the link thread's write of this packet returned: `Ok`
    /// when the packet is in the peer's receive queue (where the peer reads it before any end-of-file, even if this
    /// process dies next), an error when the write failed or the link closed first.
    pub fn send_confirmed(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<Receiver<io::Result<()>>, LinkError> {
        let (tx, rx) = mpsc::channel();
        let out = outbound(&msg, fds, Some(tx))?;
        if let Err(Command::Send(out)) = self.push(Command::Send(out)) {
            answer(out, Err(closed_error()), self.wake.as_fd());
        }
        Ok(rx)
    }

    /// Readable from the moment a message is passed on, or a confirmed send is answered, until
    /// [`LinkHandle::drain_wake`]. The GTK side watches it, and on each wake calls `drain_wake` first, then empties
    /// the receiver and looks at its pending answers (`Moves::settle`): one that arrives after the drain makes it
    /// readable again.
    pub fn wake_fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    pub fn drain_wake(&self) {
        clear(self.wake.as_fd());
    }

    /// Says `bye{why}`, writes what is still queued (bounded), closes the link and waits for its thread. A confirmed
    /// send still queued is not written and answers an error: a window that gives the link up must not commit a
    /// move (`move_done`) while it drops the module's descriptors. When this returns, every confirmed send has its
    /// answer.
    pub fn close(mut self, why: &str) {
        let bye = outbound(&Msg::Bye { why: why.to_owned() }, Vec::new(), None);
        if let Ok(bye) = bye {
            let _ = self.push(Command::Send(bye));
        }
        self.stop();
    }

    /// [`LinkHandle::close`] for a caller that must not wait: `bye` is queued here, and the wait for what is still
    /// queued, bounded the same way, runs on a thread of its own. With no thread to be had the wait happens here.
    pub fn close_detached(self, why: &str) {
        let bye = outbound(&Msg::Bye { why: why.to_owned() }, Vec::new(), None);
        if let Ok(bye) = bye {
            let _ = self.push(Command::Send(bye));
        }
        // Dropping the handle stops the link's thread and waits for it.
        let spawned = std::thread::Builder::new()
            .name("link-close".into())
            .spawn(move || drop(self));
        if let Err(err) = spawned {
            eprintln!("[link] no thread for the link's close ({err}); closed from here");
        }
    }

    /// Closes the link as [`LinkHandle::close`] does, then closes the descriptors of every move this window
    /// received whose `move_done` was not written: no descriptor of a half-made move outlives the link. The link
    /// goes first, so a `move_done` cannot leave after its move was dropped. Answers the caller holds itself rather
    /// than through `Moves::done_queued` must reach `Moves::done_sent` first; use `close` and `link_closing` then.
    pub fn close_with(self, why: &str, moves: &mut Moves) {
        self.close(why);
        moves.link_closing();
    }

    fn stop(&mut self) {
        let _ = self.push(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LinkHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Ended {
    /// The peer's end-of-file or a read error: the receiver is told.
    PeerGone,
    /// This window closed the link.
    Shutdown,
}

struct LinkThread {
    socket: OwnedFd,
    shared: Arc<Mutex<Shared>>,
    commands: Arc<OwnedFd>,
    wake: Arc<OwnedFd>,
    inbound: Sender<Inbound>,
    presence: PresenceFlag,
    out: VecDeque<Outbound>,
    /// A packet could not be written: the link ends.
    write_failed: bool,
    #[cfg(any(test, feature = "test-support"))]
    park: Option<Arc<park::ParkHook>>,
}

impl LinkThread {
    fn run(mut self) {
        let ended = self.serve();
        // Before anything is told of the end: from here on nothing the partner does can be heard.
        self.presence.0.store(true, Ordering::SeqCst);
        if let Ended::PeerGone = ended {
            self.deliver(Inbound::Closed);
        }
        let queued = {
            let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            shared.closed = true;
            std::mem::take(&mut shared.queue)
        };
        for out in std::mem::take(&mut self.out) {
            answer(out, Err(closed_error()), self.wake.as_fd());
        }
        for command in queued {
            if let Command::Send(out) = command {
                answer(out, Err(closed_error()), self.wake.as_fd());
            }
        }
        // The socket closes as `self` drops, after every unanswered send was answered.
    }

    fn serve(&mut self) -> Ended {
        loop {
            self.flush();
            if self.write_failed {
                return self.end_after_failed_write();
            }
            let mut events = libc::POLLIN;
            if !self.out.is_empty() {
                events |= libc::POLLOUT;
            }
            let mut fds = [
                pollfd(self.socket.as_fd(), events),
                pollfd(self.commands.as_fd(), libc::POLLIN),
            ];
            if poll(&mut fds, -1).is_err() {
                return Ended::PeerGone;
            }
            if fds[1].revents != 0 {
                clear(self.commands.as_fd());
                let commands = std::mem::take(&mut self.shared.lock().unwrap_or_else(|e| e.into_inner()).queue);
                for command in commands {
                    match command {
                        Command::Send(out) => self.out.push_back(out),
                        Command::Shutdown => {
                            self.cancel_confirmed();
                            self.flush_for(CLOSE_FLUSH);
                            return Ended::Shutdown;
                        }
                    }
                }
            }
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                if let Some(end) = self.read_some() {
                    return end;
                }
                if self.write_failed {
                    return self.end_after_failed_write();
                }
            }
        }
    }

    /// A closing link writes no confirmed send that is still queued; each answers an error.
    fn cancel_confirmed(&mut self) {
        let mut kept = VecDeque::new();
        for out in std::mem::take(&mut self.out) {
            if out.confirm.is_some() {
                answer(out, Err(closed_error()), self.wake.as_fd());
            } else {
                kept.push_back(out);
            }
        }
        self.out = kept;
    }

    /// After a packet could not be written: the socket is shut both ways, so a packet the peer writes from here on
    /// fails at the peer (a `move_done` among them, which the peer then does not act on), and everything the peer
    /// wrote before is read and passed on before the end. Seqpacket keeps what is queued readable after the shutdown,
    /// then reads end-of-file.
    fn end_after_failed_write(&mut self) -> Ended {
        // SAFETY: `shutdown` on the socket this thread owns.
        unsafe { libc::shutdown(self.socket.as_raw_fd(), libc::SHUT_RDWR) };
        loop {
            match fdpass::recv(self.socket.as_fd(), INLINE_MAX) {
                Ok(Some(packet)) => self.take(packet),
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    self.deliver(Inbound::Refused(LinkError::Malformed(e.to_string())))
                }
                Ok(None) | Err(_) => return Ended::PeerGone,
            }
        }
    }

    fn deliver(&self, inbound: Inbound) {
        let _ = self.inbound.send(inbound);
        signal(self.wake.as_fd());
    }

    /// Writes queued packets until the socket would block. A packet that fails otherwise is answered with its
    /// error and the link is marked to end; nothing after it is written.
    fn flush(&mut self) {
        while !self.write_failed {
            let Some(front) = self.out.front_mut() else {
                return;
            };
            #[cfg(any(test, feature = "test-support"))]
            let injected = match &self.park {
                Some(park) => park.before_write(front.kind),
                None => Ok(()),
            };
            #[cfg(not(any(test, feature = "test-support")))]
            let injected = Ok(());
            let result = injected.and_then(|()| write_one(self.socket.as_fd(), front));
            if matches!(&result, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
                return;
            }
            self.write_failed = result.is_err();
            let out = self.out.pop_front().expect("the front exists");
            answer(out, result, self.wake.as_fd());
        }
    }

    fn flush_for(&mut self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            self.flush();
            let left = deadline.saturating_duration_since(Instant::now());
            if self.out.is_empty() || self.write_failed || left.is_zero() {
                return;
            }
            let mut fds = [pollfd(self.socket.as_fd(), libc::POLLOUT)];
            if poll(&mut fds, left.as_millis().max(1) as libc::c_int).is_err() {
                return;
            }
        }
    }

    fn read_some(&mut self) -> Option<Ended> {
        for _ in 0..READS_PER_TURN {
            match fdpass::recv(self.socket.as_fd(), INLINE_MAX) {
                Ok(Some(packet)) => {
                    self.take(packet);
                    if self.write_failed {
                        return None;
                    }
                }
                Ok(None) => return Some(Ended::PeerGone),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                // A packet longer than any inline message: it is gone from the queue, its descriptors closed.
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    self.deliver(Inbound::Refused(LinkError::Malformed(e.to_string())))
                }
                Err(_) => return Some(Ended::PeerGone),
            }
        }
        None
    }

    fn take(&mut self, packet: fdpass::Received) {
        match decode(packet) {
            Ok((msg, fds)) => {
                // A window that has not confirmed its own presence when it says hello or welcome counts as another
                // window until its `presence{held: true}`; set here, in order with that message.
                if let Msg::Hello { presence_ready, .. } | Msg::Welcome { presence_ready, .. } = &msg {
                    self.presence.0.store(!presence_ready, Ordering::SeqCst);
                }
                // A partner on its way out is counted from its `bye` on, before its socket closes.
                if let Msg::Bye { .. } = &msg {
                    self.presence.0.store(true, Ordering::SeqCst);
                }
                if let Msg::Presence { held } = msg {
                    // The flag is set before the acknowledgement can leave, so the partner may rely on it once
                    // the ack arrives.
                    self.presence.0.store(!held, Ordering::SeqCst);
                    let ack = outbound(&Msg::PresenceAck { held }, Vec::new(), None);
                    if let (Ok(ack), false) = (ack, self.write_failed) {
                        self.out.push_back(ack);
                        self.flush();
                    }
                }
                self.deliver(Inbound::Msg { msg, fds });
            }
            Err(e) => self.deliver(Inbound::Refused(e)),
        }
    }
}

/// One packet's message and the descriptors that belong to it. On any error every descriptor of the packet is
/// closed by the time this returns.
fn decode(packet: fdpass::Received) -> Result<(Msg, Vec<OwnedFd>), LinkError> {
    let fdpass::Received { bytes, mut fds } = packet;
    match read_header(&bytes)? {
        Header::Inline => {
            let msg = from_json(&bytes)?;
            check_fds(msg.kind(), false, fds.len())?;
            Ok((msg, fds))
        }
        Header::Body { kind, len } => {
            check_fds(&kind, true, fds.len())?;
            let body = fds.remove(0);
            fdpass::check(body.as_fd(), FdKind::Memfd).map_err(|e| LinkError::Malformed(format!("{kind}: {e}")))?;
            let json = fdpass::read_sealed(&body, len, body_cap(&kind))
                .map_err(|e| LinkError::Malformed(format!("{kind}: {e}")))?;
            drop(body);
            let msg = from_json(&json)?;
            if msg.kind() != kind {
                return Err(LinkError::Malformed(format!(
                    "a {kind} header over a {} body",
                    msg.kind()
                )));
            }
            Ok((msg, fds))
        }
    }
}

/// One packet: a body's memfd first among its descriptors, then the message's own.
fn write_one(sock: BorrowedFd<'_>, out: &mut Outbound) -> io::Result<()> {
    if out.body.is_none() {
        if let Frame::Body { json, .. } = &out.frame {
            out.body = Some(fdpass::sealed_memfd(json)?);
        }
    }
    let bytes = match &out.frame {
        Frame::Inline(bytes) => bytes,
        Frame::Body { header, .. } => header,
    };
    let mut fds: Vec<BorrowedFd<'_>> = out.body.iter().map(|fd| fd.as_fd()).collect();
    fds.extend(out.fds.iter().map(|fd| fd.as_fd()));
    fdpass::send(sock, bytes, &fds)
}

/// A hook for tests that must stop the link thread at an exact point: it holds the thread just before it writes
/// the first packet of one kind, until the test releases it. Built only for tests, never into a release.
#[cfg(any(test, feature = "test-support"))]
pub mod park {
    use super::*;
    use std::sync::Condvar;

    /// Holds the link thread just before it writes the first packet of one kind, until released; with `fail`, that
    /// write then fails as a full descriptor table would make it fail.
    pub struct ParkHook {
        kind: &'static str,
        fail: bool,
        /// (parked, released)
        state: Mutex<(bool, bool)>,
        cv: Condvar,
    }

    impl ParkHook {
        pub(super) fn before_write(&self, kind: &str) -> io::Result<()> {
            if kind != self.kind {
                return Ok(());
            }
            let mut state = self.state.lock().unwrap();
            if state.1 {
                return Ok(());
            }
            state.0 = true;
            self.cv.notify_all();
            while !state.1 {
                state = self.cv.wait(state).unwrap();
            }
            if self.fail {
                return Err(io::Error::from_raw_os_error(libc::EMFILE));
            }
            Ok(())
        }

        pub fn wait_parked(&self, limit: Duration) -> bool {
            let state = self.state.lock().unwrap();
            let (state, _) = self.cv.wait_timeout_while(state, limit, |s| !s.0).unwrap();
            state.0
        }

        pub fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.cv.notify_all();
        }
    }

    pub fn start_parked(
        sock: OwnedFd,
        presence: PresenceFlag,
        kind: &'static str,
    ) -> io::Result<(LinkHandle, Receiver<Inbound>, Arc<ParkHook>)> {
        start_hooked(sock, presence, kind, false)
    }

    /// As [`start_parked`], and the held write fails once released.
    pub fn start_parked_failing(
        sock: OwnedFd,
        presence: PresenceFlag,
        kind: &'static str,
    ) -> io::Result<(LinkHandle, Receiver<Inbound>, Arc<ParkHook>)> {
        start_hooked(sock, presence, kind, true)
    }

    fn start_hooked(
        sock: OwnedFd,
        presence: PresenceFlag,
        kind: &'static str,
        fail: bool,
    ) -> io::Result<(LinkHandle, Receiver<Inbound>, Arc<ParkHook>)> {
        let park = Arc::new(ParkHook {
            kind,
            fail,
            state: Mutex::new((false, false)),
            cv: Condvar::new(),
        });
        let (link, rx) = LinkHandle::start_inner(sock, presence, Some(Arc::clone(&park)))?;
        Ok((link, rx, park))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_link::msg::{to_json, FocusTarget, ReplyResult};
    use park::start_parked;
    use serde_json::Value;
    use std::fs::File;
    use std::io::{PipeReader, PipeWriter, Write};

    const LONG: Duration = Duration::from_secs(20);

    fn started() -> (LinkHandle, Receiver<Inbound>, OwnedFd) {
        let (a, b) = fdpass::seqpacket_pair().unwrap();
        let (link, rx) = LinkHandle::start(a, PresenceFlag::default()).unwrap();
        (link, rx, b)
    }

    fn readable(fd: BorrowedFd<'_>, ms: i32) -> bool {
        let mut fds = [pollfd(fd, libc::POLLIN)];
        poll(&mut fds, ms).unwrap();
        fds[0].revents & libc::POLLIN != 0
    }

    fn raw_send(sock: &OwnedFd, msg: &Msg) {
        fdpass::send(sock.as_fd(), &to_json(msg), &[]).unwrap();
    }

    fn raw_recv(sock: &OwnedFd, limit: Duration) -> Option<fdpass::Received> {
        if !readable(sock.as_fd(), limit.as_millis() as i32) {
            return None;
        }
        fdpass::recv(sock.as_fd(), 4 * INLINE_MAX).unwrap()
    }

    fn next(rx: &Receiver<Inbound>) -> Inbound {
        rx.recv_timeout(LONG).expect("the link passes something on")
    }

    fn next_msg(rx: &Receiver<Inbound>) -> (Msg, Vec<OwnedFd>) {
        match next(rx) {
            Inbound::Msg { msg, fds } => (msg, fds),
            other => panic!("a message was due, got {other:?}"),
        }
    }

    fn pipe() -> (PipeReader, PipeWriter) {
        std::io::pipe().unwrap()
    }

    /// Whether the read end of the pipe whose write end this is has been closed everywhere. A child that another
    /// test is starting holds a copy of every descriptor of this process until it execs, so the end is waited for.
    fn reader_gone(w: &mut PipeWriter) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match w.write(b"x") {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return true,
                _ if std::time::Instant::now() >= deadline => return false,
                _ => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    }

    fn focus() -> Msg {
        Msg::Focus {
            target: FocusTarget::Window,
            token: None,
        }
    }

    fn big_reply() -> Msg {
        Msg::EditorReply {
            id: 1,
            result: ReplyResult::Ok {
                value: Value::String("x".repeat(1 << 20)),
            },
        }
    }

    #[test]
    fn a_message_over_64_kib_travels_as_a_sealed_body_and_arrives_whole() {
        let (link, _rx, peer) = started();
        link.send(big_reply(), Vec::new()).unwrap();
        let got = raw_recv(&peer, LONG).expect("the body's packet arrives");
        let Header::Body { kind, len } = read_header(&got.bytes).unwrap() else {
            panic!("a 1 MiB reply travels as a body");
        };
        assert_eq!(kind, "editor_reply");
        assert_eq!(got.fds.len(), 1, "the body's memfd and nothing else");
        fdpass::check(got.fds[0].as_fd(), FdKind::Memfd).unwrap();
        let json = fdpass::read_sealed(&got.fds[0], len, body_cap(&kind)).unwrap();
        assert_eq!(from_json(&json).unwrap(), big_reply());

        let (_link, rx, peer) = started();
        let Frame::Body { header, json } = frame(&big_reply()).unwrap() else {
            panic!("a body");
        };
        let memfd = fdpass::sealed_memfd(&json).unwrap();
        fdpass::send(peer.as_fd(), &header, &[memfd.as_fd()]).unwrap();
        let (msg, fds) = next_msg(&rx);
        assert_eq!(msg, big_reply());
        assert!(fds.is_empty(), "the body's memfd is not handed on");
    }

    fn named_sealed(name: &str, bytes: &[u8]) -> OwnedFd {
        let cname = std::ffi::CString::new(name).unwrap();
        // SAFETY: a NUL-terminated name.
        let raw = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        assert!(raw >= 0);
        // SAFETY: new.
        let mut file = File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        file.write_all(bytes).unwrap();
        let fd = OwnedFd::from(file);
        let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: open for the call.
        assert_eq!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) }, 0);
        fd
    }

    /// Whether no descriptor of this process refers to the memfd of that name any more. Waited for, as a child
    /// another test is starting holds copies until it execs.
    fn memfd_closed(name: &str) -> bool {
        let wanted = format!("/memfd:{name} ");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let open = std::fs::read_dir("/proc/self/fd").unwrap().flatten().any(|entry| {
                std::fs::read_link(entry.path())
                    .map(|target| target.to_string_lossy().starts_with(&wanted))
                    .unwrap_or(false)
            });
            if !open {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_body_over_its_cap_or_a_body_for_a_kind_without_one_is_refused_and_its_memfd_closed() {
        let (_link, rx, peer) = started();
        type Case = (&'static str, String, fn(&LinkError) -> bool);
        let cases: [Case; 3] = [
            (
                "no-body-kind",
                r#"{"v":1,"k":"focus","body":{"len":5}}"#.to_string(),
                |e| *e == LinkError::NoBody("focus".into()),
            ),
            (
                "over-cap",
                format!(
                    r#"{{"v":1,"k":"editor_reply","body":{{"len":{}}}}}"#,
                    16 * 1024 * 1024 + 1
                ),
                |e| matches!(e, LinkError::TooLarge { .. }),
            ),
            (
                "wrong-size",
                r#"{"v":1,"k":"editor_reply","body":{"len":9}}"#.to_string(),
                |e| matches!(e, LinkError::Malformed(_)),
            ),
        ];
        for (case, header, expected) in cases {
            let name = format!("eitri-link-test-{}-{case}", std::process::id());
            let memfd = named_sealed(&name, b"hello");
            fdpass::send(peer.as_fd(), header.as_bytes(), &[memfd.as_fd()]).unwrap();
            drop(memfd);
            match next(&rx) {
                Inbound::Refused(e) => assert!(expected(&e), "{case}: {e:?}"),
                other => panic!("{case}: refused expected, got {other:?}"),
            }
            assert!(memfd_closed(&name), "{case}: the refused body's memfd is closed");
        }
        raw_send(&peer, &Msg::Bye { why: "x".into() });
        assert!(matches!(next_msg(&rx).0, Msg::Bye { .. }), "the link stays up");
    }

    #[test]
    fn descriptors_outside_their_kind_are_refused_and_closed() {
        let (_link, rx, peer) = started();
        let (r, mut w) = pipe();
        fdpass::send(peer.as_fd(), &to_json(&focus()), &[r.as_fd()]).unwrap();
        drop(r);
        match next(&rx) {
            Inbound::Refused(e) => assert_eq!(
                e,
                LinkError::Fds {
                    kind: "focus".into(),
                    count: 1
                }
            ),
            other => panic!("refused expected, got {other:?}"),
        }
        assert!(reader_gone(&mut w), "the refused pipe end is closed");

        let Frame::Body { header, json } = frame(&big_reply()).unwrap() else {
            panic!("a body");
        };
        let memfd = fdpass::sealed_memfd(&json).unwrap();
        let (r, mut w) = pipe();
        fdpass::send(peer.as_fd(), &header, &[memfd.as_fd(), r.as_fd()]).unwrap();
        drop(r);
        assert!(matches!(next(&rx), Inbound::Refused(LinkError::Fds { .. })));
        assert!(
            reader_gone(&mut w),
            "a body may not carry descriptors its kind does not declare"
        );
    }

    #[test]
    fn a_confirmed_send_answers_only_after_the_packet_was_written() {
        let (a, peer) = fdpass::seqpacket_pair().unwrap();
        let (link, _rx, park) = start_parked(a, PresenceFlag::default(), "focus").unwrap();
        let answer = link.send_confirmed(focus(), Vec::new()).unwrap();
        assert!(park.wait_parked(LONG), "the link thread reaches the write");
        assert!(
            matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "not written, not answered"
        );
        assert!(raw_recv(&peer, Duration::from_millis(100)).is_none());
        park.release();
        assert!(matches!(answer.recv_timeout(LONG), Ok(Ok(()))));
        let got = raw_recv(&peer, LONG).expect("the packet arrives");
        assert_eq!(from_json(&got.bytes).unwrap(), focus());
    }

    #[test]
    fn a_confirmed_send_on_a_closed_link_answers_an_error() {
        let (link, rx, peer) = started();
        drop(peer);
        assert!(matches!(next(&rx), Inbound::Closed));
        let answer = link.send_confirmed(Msg::MoveDone { mv: 1 }, Vec::new()).unwrap();
        assert!(answer.recv_timeout(LONG).unwrap().is_err());
        link.send(focus(), Vec::new()).unwrap();
    }

    #[test]
    fn presence_is_acknowledged_by_the_link_thread_while_the_gtk_side_drains_nothing() {
        let (a, peer) = fdpass::seqpacket_pair().unwrap();
        let flag = PresenceFlag::default();
        // The receiver is held and never read.
        let (_link, _rx) = LinkHandle::start(a, flag.clone()).unwrap();
        for held in [false, true, false] {
            raw_send(&peer, &Msg::Presence { held });
            // Generous on purpose: the point is that the ack comes with nobody draining, not how fast.
            let got = raw_recv(&peer, Duration::from_secs(2)).expect("the ack comes without a drain");
            assert_eq!(
                flag.0.load(Ordering::SeqCst),
                !held,
                "the flag is set before the ack leaves"
            );
            assert_eq!(from_json(&got.bytes).unwrap(), Msg::PresenceAck { held });
        }
    }

    #[test]
    fn a_partner_that_says_bye_or_goes_is_counted_as_another_window_at_once() {
        for says_bye in [true, false] {
            let (a, peer) = fdpass::seqpacket_pair().unwrap();
            let flag = PresenceFlag::default();
            let (_link, rx) = LinkHandle::start(a, flag.clone()).unwrap();
            raw_send(&peer, &Msg::Presence { held: true });
            assert!(matches!(next(&rx), Inbound::Msg { .. }));
            assert!(
                !flag.0.load(Ordering::SeqCst),
                "a partner that holds its presence is skipped"
            );
            if says_bye {
                raw_send(&peer, &Msg::Bye { why: "closing".into() });
                assert!(matches!(next(&rx), Inbound::Msg { .. }));
                assert!(
                    flag.0.load(Ordering::SeqCst),
                    "counted from its bye, before its socket closes"
                );
            } else {
                drop(peer);
                assert!(matches!(next(&rx), Inbound::Closed));
                assert!(flag.0.load(Ordering::SeqCst), "counted once the link is gone");
            }
        }
    }

    #[test]
    fn a_handshake_not_yet_ready_counts_its_sender_as_another_window_until_it_says_so() {
        let (a, peer) = fdpass::seqpacket_pair().unwrap();
        let flag = PresenceFlag::default();
        let (_link, rx) = LinkHandle::start(a, flag.clone()).unwrap();
        let hello = |presence_ready| Msg::Hello {
            build: "b".into(),
            window: "2".into(),
            pid: 1,
            start: 1,
            presence_ready,
            running: Vec::new(),
            summary: Default::default(),
        };
        raw_send(&peer, &hello(false));
        assert!(matches!(next(&rx), Inbound::Msg { .. }));
        assert!(flag.0.load(Ordering::SeqCst), "not ready: counted as another window");
        raw_send(&peer, &Msg::Presence { held: true });
        assert!(matches!(next(&rx), Inbound::Msg { .. }));
        assert!(!flag.0.load(Ordering::SeqCst), "its presence confirmed: no longer");
        raw_send(&peer, &hello(true));
        assert!(matches!(next(&rx), Inbound::Msg { .. }));
        assert!(!flag.0.load(Ordering::SeqCst), "a ready hello leaves it skipped");
    }

    #[test]
    fn an_unknown_kind_is_refused_by_name_and_the_link_stays_up() {
        let (_link, rx, peer) = started();
        fdpass::send(peer.as_fd(), br#"{"v":1,"k":"run_lua"}"#, &[]).unwrap();
        raw_send(&peer, &Msg::Bye { why: "done".into() });
        match next(&rx) {
            Inbound::Refused(e) => assert_eq!(e, LinkError::UnknownKind("run_lua".into())),
            other => panic!("refused expected, got {other:?}"),
        }
        assert_eq!(next_msg(&rx).0, Msg::Bye { why: "done".into() });
    }

    #[test]
    fn the_peers_end_of_file_arrives_after_its_last_message() {
        let (_link, rx, peer) = started();
        raw_send(&peer, &Msg::MoveDone { mv: 3 });
        drop(peer);
        assert_eq!(next_msg(&rx).0, Msg::MoveDone { mv: 3 });
        assert!(matches!(next(&rx), Inbound::Closed));
        assert!(rx.recv_timeout(LONG).is_err(), "nothing after the end");
    }

    /// Drains the wake and says how many messages were passed on since the last drain.
    fn wake_count(link: &LinkHandle) -> u64 {
        let mut count: u64 = 0;
        // SAFETY: reads 8 bytes into `count`; a non-blocking eventfd at zero fails at once and leaves it 0.
        unsafe { libc::read(link.wake.as_raw_fd(), (&mut count as *mut u64).cast(), 8) };
        count
    }

    #[test]
    fn the_wake_fd_is_readable_exactly_while_messages_wait() {
        let (link, rx, peer) = started();
        assert!(!readable(link.wake_fd(), 0), "nothing waits yet");
        raw_send(&peer, &Msg::ReviewIdle);
        assert!(readable(link.wake_fd(), 20_000));
        assert!(readable(link.wake_fd(), 0), "it stays readable while nobody drains");
        link.drain_wake();
        assert_eq!(rx.try_recv().map(|i| matches!(i, Inbound::Msg { .. })), Ok(true));
        assert!(!readable(link.wake_fd(), 0), "drained, nothing waits");

        // Three messages: the wake stays readable until all three have been signalled and drained, and the
        // receiver then holds exactly three.
        for _ in 0..3 {
            raw_send(&peer, &Msg::ReviewIdle);
        }
        let mut signalled = 0;
        while signalled < 3 {
            assert!(readable(link.wake_fd(), 20_000), "a waiting message makes it readable");
            signalled += wake_count(&link);
        }
        assert_eq!(signalled, 3);
        for _ in 0..3 {
            assert!(matches!(rx.try_recv(), Ok(Inbound::Msg { .. })));
        }
        assert!(rx.try_recv().is_err());
        assert!(!readable(link.wake_fd(), 0), "drained, nothing waits");
        // A message that arrives after the drain makes it readable again.
        raw_send(&peer, &Msg::ReviewIdle);
        assert!(readable(link.wake_fd(), 20_000));
    }

    #[test]
    fn a_packet_that_cannot_be_written_ends_the_link_after_what_the_peer_wrote() {
        let (a, peer) = fdpass::seqpacket_pair().unwrap();
        let (link, rx, park) = park::start_parked_failing(a, PresenceFlag::default(), "focus").unwrap();
        // Unconfirmed: nobody would hear of its failure if the link stayed up.
        link.send(focus(), Vec::new()).unwrap();
        let behind = link.send_confirmed(Msg::MoveDone { mv: 1 }, Vec::new()).unwrap();
        assert!(park.wait_parked(LONG));
        // Written while the link thread is held in its failing write.
        raw_send(&peer, &Msg::MoveDone { mv: 3 });
        park.release();
        assert_eq!(
            next_msg(&rx).0,
            Msg::MoveDone { mv: 3 },
            "what the peer wrote comes first"
        );
        assert!(matches!(next(&rx), Inbound::Closed));
        assert!(
            behind.recv_timeout(LONG).unwrap().is_err(),
            "nothing after the failure is written"
        );
        assert!(
            fdpass::send(peer.as_fd(), &to_json(&focus()), &[]).is_err(),
            "the peer's later writes fail, so it acts on none of them"
        );
        assert!(
            raw_recv(&peer, LONG).is_none(),
            "the peer reads the end and nothing else"
        );
    }

    #[test]
    fn closing_writes_no_confirmed_send_still_queued_and_drops_its_move() {
        let (a, peer) = fdpass::seqpacket_pair().unwrap();
        let (link, _rx, park) = start_parked(a, PresenceFlag::default(), "focus").unwrap();
        let shared = Arc::clone(&link.shared);
        let mut moves = Moves::new(super::super::moves::Window::Two);
        let id = super::super::moves::MoveId(7);
        let (r, mut w) = pipe();
        assert!(moves.accept_offer(id));
        assert!(moves.prepared(id));
        moves.received(id, vec![r.into()]).unwrap();

        // The thread is held on an earlier packet, so `move_done` is still queued when the close comes.
        link.send(focus(), Vec::new()).unwrap();
        assert!(park.wait_parked(LONG));
        let answer = link.send_confirmed(Msg::MoveDone { mv: id.0 }, Vec::new()).unwrap();
        assert!(moves.done_queued(id, answer));
        let closer = std::thread::spawn(move || {
            link.close_with("window closed", &mut moves);
            moves
        });
        let deadline = Instant::now() + LONG;
        while !shared
            .lock()
            .unwrap()
            .queue
            .iter()
            .any(|c| matches!(c, Command::Shutdown))
        {
            assert!(Instant::now() < deadline, "the close is queued");
            std::thread::sleep(Duration::from_millis(1));
        }
        park.release();
        let moves = closer.join().unwrap();

        assert!(moves.confirmed().is_empty());
        assert!(!moves.holds(super::super::moves::Role::Destination, id));
        assert!(reader_gone(&mut w), "the dropped move's descriptor is closed");
        let mut seen = Vec::new();
        while let Some(got) = raw_recv(&peer, LONG) {
            seen.push(from_json(&got.bytes).unwrap());
        }
        assert_eq!(
            seen,
            vec![
                focus(),
                Msg::Bye {
                    why: "window closed".into()
                }
            ],
            "no move_done reaches the source of a move this window dropped"
        );
    }

    #[test]
    fn a_confirmed_sends_answer_makes_the_wake_fd_readable() {
        let (link, rx, _peer) = started();
        let answer = link.send_confirmed(focus(), Vec::new()).unwrap();
        assert!(readable(link.wake_fd(), 20_000), "the answer wakes the GTK side");
        link.drain_wake();
        assert!(matches!(answer.try_recv(), Ok(Ok(()))));
        assert!(rx.try_recv().is_err(), "no message came: the wake was the answer's");
        assert!(!readable(link.wake_fd(), 0));
    }

    /// A peer that reads nothing leaves the `bye` queued behind a full socket. `close_detached` has returned before
    /// the peer starts reading, so the flush is still running and the `bye` gets out; a close that waited would have
    /// used up its bound with nobody reading, every time. A try counts only if the peer reads within that bound, so a
    /// thread descheduled for the whole of it gets another.
    #[test]
    fn a_detached_close_returns_with_the_flush_still_to_run() {
        let bye = Msg::Bye {
            why: "window closed".into(),
        };
        let bye_got_out = || {
            let (link, _rx, peer) = started();
            let long = "x".repeat(32 * 1024);
            for _ in 0..512 {
                link.send(Msg::Bye { why: long.clone() }, Vec::new()).unwrap();
            }
            link.close_detached("window closed");
            let mut last = None;
            while let Some(got) = raw_recv(&peer, LONG) {
                last = Some(from_json(&got.bytes).unwrap());
            }
            last.as_ref() == Some(&bye)
        };
        assert!((0..3).any(|_| bye_got_out()));
    }

    #[test]
    fn close_says_bye_before_the_end_of_file() {
        let (link, _rx, peer) = started();
        link.close("window closed");
        let got = raw_recv(&peer, LONG).expect("the bye arrives");
        assert_eq!(
            from_json(&got.bytes).unwrap(),
            Msg::Bye {
                why: "window closed".into()
            }
        );
        assert!(raw_recv(&peer, LONG).is_none(), "then the end-of-file");
    }
}
