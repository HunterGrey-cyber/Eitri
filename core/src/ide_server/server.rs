//! Starting and stopping the listener's threads.
//!
//! An [`IdeServer`] is what the window holds: the port, the way to the worker and the way back. The bind runs on the
//! caller's thread, so the port is known before anything else starts. The directory checks, the sweep and the publish
//! touch the disk, so they run on the worker, and whether the lock was written is an event like any other, one the
//! window may wait a moment for. No lock, no listener: a worker whose keeper refuses ends there.
//!
//! The threads are not joined. The worker's end is seen through a channel it closes, so that waiting for it has a bound,
//! and a judge held by a hung path ends by itself once the worker's channels have closed.

use std::cell::Cell;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::RawFd;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::channels::Wake;
use super::files;
use super::handshake::{getrandom_fill, Token};
use super::keeper::LockKeeper;
use super::mention::LiveMentions;
use super::types::{Command, Event, Housekeeping, Limits, ServerConfig, Started};
use super::worker::{self, WorkerParts};

/// One listener, from the bind to its [`IdeServer::stop`]. Lives on the main context.
pub struct IdeServer {
    port: u16,
    events: Receiver<Event>,
    events_wake: Arc<Wake>,
    commands: Sender<Command>,
    commands_wake: Arc<Wake>,
    /// Nothing is ever sent on it: it closes when the worker's thread ends, however it ends, by which point the lock
    /// and the port have gone.
    worker_gone: Receiver<()>,
    /// The first event is no longer waited for: it was, or a drain may already have taken it.
    first_settled: Cell<bool>,
}

impl IdeServer {
    /// `None`: no random bytes or no bind, so no listener and no lock. The bind runs here, on the caller's thread.
    pub fn start(config: ServerConfig) -> Option<IdeServer> {
        let keeper = lock_keeper(&config);
        IdeServer::launch(config, Limits::default(), getrandom_fill, keeper)
    }

    /// As [`IdeServer::start`], with the bounds and the random source given.
    #[cfg(any(test, feature = "test-support"))]
    pub fn start_with(
        config: ServerConfig,
        limits: Limits,
        fill: impl FnOnce(&mut [u8; 32]) -> io::Result<()>,
    ) -> Option<IdeServer> {
        let keeper = lock_keeper(&config);
        IdeServer::launch(config, limits, fill, keeper)
    }

    /// `start_with`, with a keeper given: the seam for a slow or failing lock.
    #[cfg(any(test, feature = "test-support"))]
    pub fn start_with_keeper(
        config: ServerConfig,
        limits: Limits,
        fill: impl FnOnce(&mut [u8; 32]) -> io::Result<()>,
        keeper: Box<dyn Housekeeping>,
    ) -> Option<IdeServer> {
        IdeServer::launch(config, limits, fill, keeper)
    }

    fn launch(
        config: ServerConfig,
        limits: Limits,
        fill: impl FnOnce(&mut [u8; 32]) -> io::Result<()>,
        keeper: Box<dyn Housekeeping>,
    ) -> Option<IdeServer> {
        match IdeServer::bring_up(config, limits, fill, keeper) {
            Ok(server) => Some(server),
            Err(error) => {
                eprintln!("[ide-server] no listener: {error}");
                None
            }
        }
    }

    /// A failure anywhere here comes before the worker runs, so before any lock. A worker that cannot start drops its
    /// parts, and with them the judge's channel, which ends the judge.
    fn bring_up(
        config: ServerConfig,
        limits: Limits,
        fill: impl FnOnce(&mut [u8; 32]) -> io::Result<()>,
        keeper: Box<dyn Housekeeping>,
    ) -> io::Result<IdeServer> {
        let token = Token::generate(fill)?;
        // IPv4 loopback alone, since that is what the CLI dials. The standard library's sockets are close-on-exec.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();

        let events_wake = Arc::new(Wake::new()?);
        let commands_wake = Arc::new(Wake::new()?);
        let done_wake = Arc::new(Wake::new()?);
        let (events_sender, events) = channel();
        let (commands, commands_receiver) = channel();
        let (jobs, jobs_receiver) = channel();
        let (done_sender, done) = channel();

        files::spawn(config, jobs_receiver, done_sender, Arc::clone(&done_wake))?;

        let parts = WorkerParts {
            listener,
            token,
            limits,
            keeper,
            mentions: Box::new(LiveMentions::new()),
            events: events_sender,
            events_wake: Arc::clone(&events_wake),
            commands: commands_receiver,
            commands_wake: Arc::clone(&commands_wake),
            jobs,
            done,
            done_wake,
        };
        let (still_running, worker_gone) = channel();
        thread::Builder::new().name("ide-worker".to_owned()).spawn(move || {
            // Dropped last, also on a panic: `run` has given up the lock and the port by then.
            let _still_running: Sender<()> = still_running;
            worker::run(parts);
        })?;

        Ok(IdeServer {
            port,
            events,
            events_wake,
            commands,
            commands_wake,
            worker_gone,
            first_settled: Cell::new(false),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Waits at most `within` for `Event::Started`, once; later calls and a timeout answer `None`, and it then arrives
    /// through `drain`. Blocks, so a caller on the main context keeps `within` small.
    pub fn first_answer(&self, within: Duration) -> Option<Started> {
        if self.first_settled.replace(true) {
            return None;
        }
        match self.events.recv_timeout(within) {
            Ok(Event::Started(started)) => Some(started),
            // `Started` is the worker's first event, so anything else cannot be here; a closed channel is a worker gone.
            Ok(_) | Err(_) => None,
        }
    }

    /// Readable while `drain` has something to take. Valid for as long as this server lives.
    pub fn wake_fd(&self) -> RawFd {
        self.events_wake.fd()
    }

    /// Every event waiting. Never blocks.
    pub fn drain(&self) -> Vec<Event> {
        self.first_settled.set(true);
        // Before the queue is read, so anything sent after this read wakes the next watch.
        self.events_wake.drain();
        self.events.try_iter().collect()
    }

    /// In order. Commands for a worker already gone go nowhere.
    pub fn send(&self, commands: Vec<Command>) {
        for command in commands {
            if self.commands.send(command).is_err() {
                break;
            }
        }
        self.commands_wake.signal();
    }

    /// Stops the worker: the file goes, then the port. Waits at most `within`. Past that the worker carries on to the
    /// same end by itself, once whatever holds it lets go.
    pub fn stop(self, within: Duration) {
        self.send(vec![Command::Stop]);
        // Nothing is sent on it, so this only ever ends by the channel closing or the time running out.
        let _ = self.worker_gone.recv_timeout(within);
    }
}

impl Drop for IdeServer {
    /// A closed channel signals no eventfd, so without this a worker asleep in `poll` would never look at it. After a
    /// `stop`, this is a second `Stop` that nothing reads.
    fn drop(&mut self) {
        // The worker gone means there is nobody to tell.
        let _ = self.commands.send(Command::Stop);
        self.commands_wake.signal();
    }
}

fn lock_keeper(config: &ServerConfig) -> Box<dyn Housekeeping> {
    Box::new(LockKeeper::new(config.home.clone(), std::process::id()))
}
