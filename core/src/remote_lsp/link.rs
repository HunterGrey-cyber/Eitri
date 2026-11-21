//! The bridge's connection to the helper's `Lsp` service: opening a server, attaching to it, and getting back to it
//! after the link drops. It runs on a thread of its own, takes commands from the session and reports what happens as
//! [`LinkEvent`]s; the stream's two directions run as separate tasks (a reader that reassembles chunks, and frames
//! queued without bound by the writer), so neither waits on the other: a bridge that stopped reading while it wrote
//! could deadlock against the helper's bounded queues.
//!
//! The connection is the bridge's own (a helper connection of its own through the window's second ssh master), never
//! the agents'. Its channel reconnects by itself when the connection under it breaks; a failed call is retried after
//! the shared back-off.

use std::any::Any;
use std::future::Future;
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::remote_answers::checked_id;
use agent::remote_client::terminal_transport::{tokio, tokio_stream, tonic, wire};
use tokio::runtime::Handle;
use tonic::transport::Channel;
use tonic::{Code, Status};
use wire::lsp_client::LspClient;
use wire::lsp_client_frame::Frame as Up;
use wire::lsp_server_frame::Frame as Down;
use wire::{
    LspAttachHello, LspAttached, LspChunk, LspClientFrame, LspEnded, LSP_CHUNK_MAX, LSP_MESSAGE_MAX, LSP_PATH_MAX,
};

use super::bridge::Failure;
use super::framing::{chunks, Reassembler};

/// How long one call to the helper may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stop is waited for.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Bytes of server messages the session has not handled yet, past which the reader waits (HTTP/2 flow control then
/// holds the helper back): one message of the largest size, so at most two are held.
const PENDING_MAX: usize = LSP_MESSAGE_MAX;

/// A connection to a helper that offers the service, with what keeps it alive.
pub struct Dial {
    pub channel: Channel,
    pub handle: Handle,
    /// Held for as long as the channel is used, outside the runtime's tasks.
    pub owner: Box<dyn Any + Send>,
}

pub enum DialError {
    /// The helper is older than language servers on the server: the caller runs the server locally.
    NoLsp,
    Unreachable(String),
}

pub type Redial = Box<dyn FnMut() -> Result<Dial, DialError> + Send>;

pub struct Params {
    /// The host as typed, for the lines shown.
    pub host: String,
    pub root: String,
    pub argv: Vec<String>,
    pub grace_ms: u64,
    /// 128 random bits as hex, made when the bridge started and kept across its reconnects.
    pub client_id: String,
    pub backoff: Vec<Duration>,
    /// How long the first open is retried before it is reported failed.
    pub open_deadline: Duration,
}

pub enum LinkEvent {
    Attached {
        gen: u64,
        attached: LspAttached,
    },
    Server {
        gen: u64,
        message: Vec<u8>,
    },
    /// The server is gone. `fresh`: the answer to an attach (the stream never held it), not the end of a live one.
    Ended {
        gen: u64,
        ended: LspEnded,
        fresh: bool,
    },
    /// The stream ended or failed without an `LspEnded`.
    Lost {
        gen: u64,
    },
    OpenFailed(Failure),
    /// Something a person may want to know while the link is down (once per distinct text).
    Note(String),
    /// A stop was asked for and has been tried.
    Stopped,
}

pub enum Cmd {
    /// Open a fresh server and attach to it.
    Open,
    Send {
        gen: u64,
        message: Vec<u8>,
        saved_sha256: Option<String>,
    },
    /// Stop the server (if any) and leave it.
    Stop,
    Shutdown,
}

enum Msg {
    Cmd(Cmd),
    /// The stream of this attach ended or failed.
    StreamEnd(u64),
    /// The stream of this attach ended with an `LspEnded`: the server is gone.
    ServerEnded(u64),
}

pub type Emit = Arc<dyn Fn(LinkEvent) + Send + Sync>;

pub struct Link {
    commands: mpsc::Sender<Msg>,
}

impl Link {
    pub fn start(
        params: Params,
        first: Option<Dial>,
        redial: Redial,
        emit: Emit,
        pending: Arc<AtomicUsize>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let manager = Manager {
            params,
            dial: first,
            redial,
            emit,
            pending,
            rx,
            tx: tx.clone(),
            server_id: None,
            command_id: random_id(),
            gen: 0,
            frames: None,
            mode: Mode::Idle,
            opened_once: false,
            last_note: String::new(),
        };
        std::thread::Builder::new()
            .name("lsp-link".into())
            .spawn(move || manager.run())?;
        Ok(Self { commands: tx })
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.commands.send(Msg::Cmd(cmd));
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.commands.send(Msg::Cmd(Cmd::Shutdown));
    }
}

/// 128 random bits as 32 hex digits.
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut urandom| urandom.read_exact(&mut bytes));
    if read.is_err() {
        // No urandom: the bits of two version-4 UUIDs, the fixed ones left out.
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        bytes[..8].copy_from_slice(&a.as_bytes()[8..]);
        bytes[8..].copy_from_slice(&b.as_bytes()[8..]);
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

enum Mode {
    /// No server, nothing to do until told.
    Idle,
    /// Opening a server: the next attempt is due at `due`.
    Opening {
        since: Instant,
        attempt: usize,
        due: Instant,
    },
    Attached,
    /// The link is down: the next attempt to attach again is due at `due`.
    Reattaching {
        attempt: usize,
        due: Instant,
    },
}

struct Manager {
    params: Params,
    dial: Option<Dial>,
    redial: Redial,
    emit: Emit,
    pending: Arc<AtomicUsize>,
    rx: mpsc::Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
    server_id: Option<String>,
    command_id: String,
    gen: u64,
    frames: Option<tokio::sync::mpsc::UnboundedSender<LspClientFrame>>,
    mode: Mode,
    opened_once: bool,
    last_note: String,
}

/// Runs `future` on the connection's runtime and waits for it here, for at most `timeout`.
fn call<T: Send + 'static>(
    handle: &Handle,
    future: impl Future<Output = Result<T, Status>> + Send + 'static,
    timeout: Duration,
) -> Result<T, Status> {
    let (tx, rx) = mpsc::channel();
    let task = handle.spawn(async move {
        let _ = tx.send(future.await);
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            task.abort();
            Err(Status::deadline_exceeded("the Eitri server did not answer in time"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Status::unavailable("the connection ended")),
    }
}

/// A call that may succeed when tried again later.
fn retryable(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::Unavailable
            | Code::DeadlineExceeded
            | Code::Cancelled
            | Code::Aborted
            | Code::Unknown
            | Code::ResourceExhausted
    )
}

fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

impl Manager {
    fn run(mut self) {
        loop {
            let due = match &self.mode {
                Mode::Opening { due, .. } | Mode::Reattaching { due, .. } => Some(*due),
                _ => None,
            };
            let received = match due {
                Some(due) => match self.rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                    Ok(msg) => Some(msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                },
                None => match self.rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return,
                },
            };
            match received {
                Some(Msg::Cmd(Cmd::Shutdown)) => return,
                Some(Msg::Cmd(Cmd::Open)) => {
                    self.command_id = random_id();
                    let now = Instant::now();
                    self.mode = Mode::Opening {
                        since: now,
                        attempt: 0,
                        due: now,
                    };
                }
                Some(Msg::Cmd(Cmd::Send {
                    gen,
                    message,
                    saved_sha256,
                })) => self.send(gen, &message, saved_sha256),
                Some(Msg::Cmd(Cmd::Stop)) => self.stop(),
                Some(Msg::StreamEnd(gen)) if gen == self.gen && matches!(self.mode, Mode::Attached) => {
                    self.frames = None;
                    self.schedule_reattach(0);
                }
                // Only the end of the stream in use: a server this side stopped also ends its stream, and that end
                // must not undo an open asked for since.
                Some(Msg::ServerEnded(gen)) if gen == self.gen && matches!(self.mode, Mode::Attached) => {
                    self.frames = None;
                    self.server_id = None;
                    self.mode = Mode::Idle;
                }
                Some(Msg::StreamEnd(_) | Msg::ServerEnded(_)) => {}
                None => match self.mode {
                    Mode::Opening { since, attempt, .. } => self.try_open(since, attempt),
                    Mode::Reattaching { attempt, .. } => self.try_attach(attempt),
                    _ => {}
                },
            }
        }
    }

    fn note(&mut self, text: String) {
        if text != self.last_note {
            self.last_note = text.clone();
            (self.emit)(LinkEvent::Note(text));
        }
    }

    fn delay(&self, attempt: usize) -> Duration {
        let steps = &self.params.backoff;
        steps
            .get(attempt.min(steps.len().saturating_sub(1)))
            .copied()
            .unwrap_or(Duration::from_secs(1))
    }

    fn ensure_dial(&mut self) -> Result<(Channel, Handle), DialError> {
        if self.dial.is_none() {
            self.dial = Some((self.redial)()?);
        }
        let dial = self.dial.as_ref().expect("a dial was just made");
        Ok((dial.channel.clone(), dial.handle.clone()))
    }

    fn try_open(&mut self, since: Instant, attempt: usize) {
        let why = match self.ensure_dial() {
            Ok((channel, handle)) => {
                let request = wire::OpenServerRequest {
                    command_id: self.command_id.clone(),
                    root: self.params.root.clone(),
                    argv: self.params.argv.clone(),
                    grace_ms: self.params.grace_ms,
                    client_id: self.params.client_id.clone(),
                };
                let mut client = LspClient::new(channel);
                match call(&handle, async move { client.open_server(request).await }, CALL_TIMEOUT) {
                    Ok(response) => return self.opened(response.into_inner()),
                    Err(status) if retryable(&status) => status.message().to_owned(),
                    Err(status) => {
                        return self.open_failed(Failure::Refused(cut(status.message(), LSP_PATH_MAX)));
                    }
                }
            }
            Err(DialError::NoLsp) => {
                let why = "the Eitri server there no longer offers language servers".to_owned();
                return self.open_failed(Failure::Unreachable(why));
            }
            Err(DialError::Unreachable(why)) => why,
        };
        if !self.opened_once && since.elapsed() >= self.params.open_deadline {
            return self.open_failed(Failure::Unreachable(cut(&why, LSP_PATH_MAX)));
        }
        self.note(format!("{} cannot be reached: {}", self.params.host, cut(&why, 1024)));
        self.mode = Mode::Opening {
            since,
            attempt: attempt + 1,
            due: Instant::now() + self.delay(attempt),
        };
    }

    /// No server could be had: the helper connection is let go at once (the bridge either ends or only answers Neovim
    /// from here on; a later open would dial again).
    fn open_failed(&mut self, failure: Failure) {
        self.mode = Mode::Idle;
        self.frames = None;
        self.dial = None;
        (self.emit)(LinkEvent::OpenFailed(failure));
    }

    /// The helper's answer to `OpenServer`, checked before it is used.
    fn opened(&mut self, response: wire::OpenServerResponse) {
        let refused = if !response.missing_command.is_empty() {
            Some(Failure::Missing(cut(&response.missing_command, LSP_PATH_MAX)))
        } else if !response.refusal.is_empty() {
            Some(Failure::Refused(cut(&response.refusal, LSP_PATH_MAX)))
        } else if checked_id("the server id", &response.server_id).is_err()
            || response.resolved_command.len() > LSP_PATH_MAX
        {
            Some(Failure::Refused(
                "the Eitri server answered the open with a server it named wrongly".into(),
            ))
        } else {
            None
        };
        if let Some(failure) = refused {
            return self.open_failed(failure);
        }
        self.server_id = Some(response.server_id);
        self.opened_once = true;
        self.try_attach(0);
    }

    fn schedule_reattach(&mut self, attempt: usize) {
        self.mode = Mode::Reattaching {
            attempt: attempt + 1,
            due: Instant::now() + self.delay(attempt),
        };
    }

    fn try_attach(&mut self, attempt: usize) {
        let Some(server_id) = self.server_id.clone() else {
            self.mode = Mode::Idle;
            return;
        };
        let (channel, handle) = match self.ensure_dial() {
            Ok(dial) => dial,
            Err(DialError::NoLsp) => {
                self.note(format!("{} no longer offers language servers", self.params.host));
                return self.schedule_reattach(attempt);
            }
            Err(DialError::Unreachable(why)) => {
                self.note(format!("{} cannot be reached: {}", self.params.host, cut(&why, 1024)));
                return self.schedule_reattach(attempt);
            }
        };
        let (frames, queue) = tokio::sync::mpsc::unbounded_channel();
        let hello = LspAttachHello {
            server_id,
            client_id: self.params.client_id.clone(),
        };
        let _ = frames.send(LspClientFrame {
            frame: Some(Up::Hello(hello)),
        });
        let mut client = LspClient::new(channel);
        let attach = async move {
            let mut stream = client
                .attach(tokio_stream::wrappers::UnboundedReceiverStream::new(queue))
                .await?
                .into_inner();
            // The first frame is read before anything is sent on the stream.
            let first = stream.message().await?;
            Ok((stream, first))
        };
        match call(&handle, attach, CALL_TIMEOUT) {
            Err(status) => {
                self.note(format!("{}: {}", self.params.host, cut(status.message(), 1024)));
                self.schedule_reattach(attempt);
            }
            Ok((_, None)) => self.schedule_reattach(attempt),
            Ok((stream, Some(first))) => {
                self.gen += 1;
                let gen = self.gen;
                match first.frame {
                    Some(Down::Attached(attached)) => {
                        self.last_note.clear();
                        self.frames = Some(frames);
                        self.mode = Mode::Attached;
                        (self.emit)(LinkEvent::Attached { gen, attached });
                        let reader = read_stream(stream, gen, self.emit.clone(), self.pending.clone(), self.tx.clone());
                        handle.spawn(reader);
                    }
                    Some(Down::Ended(ended)) => {
                        self.server_id = None;
                        self.mode = Mode::Idle;
                        (self.emit)(LinkEvent::Ended {
                            gen,
                            ended,
                            fresh: true,
                        });
                    }
                    Some(Down::Chunk(_)) | None => {
                        self.note(format!(
                            "{} sent a stream that did not begin with its attach answer",
                            self.params.host
                        ));
                        self.schedule_reattach(attempt);
                    }
                }
            }
        }
    }

    fn send(&mut self, gen: u64, message: &[u8], saved_sha256: Option<String>) {
        if gen != self.gen || !matches!(self.mode, Mode::Attached) {
            return;
        }
        let Some(frames) = &self.frames else { return };
        for (data, last) in chunks(message) {
            let chunk = LspChunk {
                data: data.to_vec(),
                last,
                saved_sha256: if last {
                    saved_sha256.clone().unwrap_or_default()
                } else {
                    String::new()
                },
            };
            if frames
                .send(LspClientFrame {
                    frame: Some(Up::Chunk(chunk)),
                })
                .is_err()
            {
                return;
            }
        }
    }

    fn stop(&mut self) {
        if let (Some(server_id), Some(dial)) = (self.server_id.take(), &self.dial) {
            let mut client = LspClient::new(dial.channel.clone());
            let request = wire::StopServerRequest {
                server_id,
                command_id: random_id(),
            };
            // Best effort: with the link down the server ends at its grace.
            let _ = call(
                &dial.handle,
                async move { client.stop_server(request).await },
                STOP_TIMEOUT,
            );
        }
        self.frames = None;
        // The stopped server's stream is no longer the one in use, whatever it still reports.
        self.gen += 1;
        self.mode = Mode::Idle;
        (self.emit)(LinkEvent::Stopped);
    }
}

/// Reads one attach's stream until it ends, handing every whole message to the session.
async fn read_stream(
    mut stream: tonic::Streaming<wire::LspServerFrame>,
    gen: u64,
    emit: Emit,
    pending: Arc<AtomicUsize>,
    manager: mpsc::Sender<Msg>,
) {
    let mut assembler = Reassembler::new(LSP_MESSAGE_MAX);
    loop {
        // What the helper did wrong, when it did something wrong rather than the link failing.
        let violation = match stream.message().await {
            Ok(Some(frame)) => match frame.frame {
                Some(Down::Chunk(chunk)) if chunk.data.len() <= LSP_CHUNK_MAX => {
                    match assembler.push(&chunk.data, chunk.last) {
                        Ok(Some(message)) => {
                            while pending.load(Ordering::Acquire) > PENDING_MAX {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            pending.fetch_add(message.len(), Ordering::AcqRel);
                            emit(LinkEvent::Server { gen, message });
                            continue;
                        }
                        Ok(None) => continue,
                        Err(_) => Some("the language server sent a message over the size limit"),
                    }
                }
                Some(Down::Ended(ended)) => {
                    let _ = manager.send(Msg::ServerEnded(gen));
                    emit(LinkEvent::Ended {
                        gen,
                        ended,
                        fresh: false,
                    });
                    return;
                }
                _ => Some("the helper sent a frame the stream cannot carry"),
            },
            Ok(None) | Err(_) => None,
        };
        if let Some(text) = violation {
            emit(LinkEvent::Note(text.to_owned()));
        }
        let _ = manager.send(Msg::StreamEnd(gen));
        emit(LinkEvent::Lost { gen });
        return;
    }
}
