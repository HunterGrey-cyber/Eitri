//! A language server's process: how it starts, its stderr log, and the framed messages on its stdin and stdout.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use eitri_server_proto::LSP_MESSAGE_MAX;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Notify};

use super::message::TopLevelScan;
use crate::daemon::login_env::Environment;

/// A server's log keeps at most this much; past it the older half is cut away.
pub const LOG_CAP: u64 = 1024 * 1024;
/// The most header bytes one message may have.
pub const HEADER_MAX: usize = 8 * 1024;
/// The bridge's messages wait while this much is queued for the server's stdin.
const WRITE_HIGH: usize = 4 * 1024 * 1024;
/// The helper's own writes (answers, cancellations) never wait; past this much queued the server is not reading its
/// input and is stopped.
pub const WRITE_MAX: usize = LSP_MESSAGE_MAX + 16 * 1024 * 1024;

/// A server's private log: its stderr and the helper's lines about it, with the last stderr bytes kept for the
/// message that says it ended.
pub struct Log {
    file: Mutex<File>,
    tail: Mutex<VecDeque<u8>>,
    tail_max: usize,
}

impl Log {
    /// `<dir>/<id>.log`, 0600 in a 0700 `dir` under `state`.
    pub fn open(state: &Path, dir: &Path, id: &str, tail_max: usize) -> io::Result<Arc<Log>> {
        agent::private_fs::create_private_dir_all(dir, state)?;
        let file = agent::private_fs::append_private(&dir.join(format!("{id}.log")))?;
        Ok(Arc::new(Log {
            file: Mutex::new(file),
            tail: Mutex::new(VecDeque::new()),
            tail_max,
        }))
    }

    fn append(&self, bytes: &[u8]) {
        let file = self.file.lock().expect("server log");
        let _ = (&*file).write_all(bytes);
        if file.metadata().is_ok_and(|metadata| metadata.len() > LOG_CAP) {
            let _ = cut_front(&file);
        }
    }

    fn stderr(&self, bytes: &[u8]) {
        self.append(bytes);
        let mut tail = self.tail.lock().expect("stderr tail");
        tail.extend(bytes);
        let excess = tail.len().saturating_sub(self.tail_max);
        tail.drain(..excess);
    }

    /// A line of the helper's own, marked as such.
    pub fn line(&self, text: &str) {
        self.append(format!("eitri-server: {}\n", text.replace(['\r', '\n'], " ")).as_bytes());
    }

    pub fn tail(&self) -> Vec<u8> {
        self.tail.lock().expect("stderr tail").iter().copied().collect()
    }
}

/// Keeps the newer half of an append-mode log.
fn cut_front(file: &File) -> io::Result<()> {
    let len = file.metadata()?.len();
    let keep = LOG_CAP / 2;
    let reader = File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    let mut tail = vec![0; keep as usize];
    let read = reader.read_at(&mut tail, len.saturating_sub(keep))?;
    tail.truncate(read);
    file.set_len(0)?;
    (&*file).write_all(&tail)
}

/// A started server.
pub struct Started {
    pub child: Child,
    pub pid: u32,
    /// Pins the server's pid; readable once it has exited, before it is reaped.
    pub pidfd: OwnedFd,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    /// Fires when the server's stderr has ended (it and everything it started closed it).
    pub stderr_done: oneshot::Receiver<()>,
}

/// Starts `program` in the directory `root` holds, with exactly `env` (and `PWD` the root), in a session and process
/// group of its own, its stderr going to `log`.
pub fn start(
    program: &Path,
    args: &[String],
    env: &Environment,
    root: &File,
    canonical_root: &str,
    log: Arc<Log>,
) -> io::Result<Started> {
    let (reader, writer) = io::pipe()?;
    let mut child = {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(env)
            .env("PWD", canonical_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(writer))
            .kill_on_drop(true);
        let root_fd = root.as_raw_fd();
        let umask = crate::run_dir::inherited_umask();
        // SAFETY: setsid, fchdir and umask are async-signal-safe; `root` outlives the spawn.
        unsafe {
            command.pre_exec(move || crate::daemon::sidecars::child_setup(root_fd, umask));
        }
        // The command, and with it the parent's copy of the stderr pipe, is dropped here, so the reader sees the end.
        command.spawn()?
    };
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the server exited at once"))?;
    let pidfd = crate::daemon::sidecars::pin(pid)?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let (done, stderr_done) = oneshot::channel();
    std::thread::Builder::new().name("lsp-stderr".into()).spawn(move || {
        let mut reader = reader;
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => log.stderr(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = done.send(());
    })?;
    Ok(Started {
        child,
        pid,
        pidfd,
        stdin,
        stdout,
        stderr_done,
    })
}

/// One message read from the server's stdout.
pub enum FromServer {
    Message(Vec<u8>),
    /// A message past `LSP_MESSAGE_MAX`: its bytes were read and dropped; what the scan found of its id and method.
    TooLarge {
        scan: TopLevelScan,
        len: u64,
    },
}

/// Reads framed messages from the server's stdout until it ends or breaks the framing (a line in the log says how).
pub async fn read_stdout(stdout: ChildStdout, messages: mpsc::Sender<FromServer>, log: Arc<Log>) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    loop {
        let len = match read_header(&mut reader).await {
            Ok(Some(len)) => len,
            Ok(None) => return,
            Err(error) => {
                log.line(&format!("stopped reading the server's output: {error}"));
                return;
            }
        };
        let message = if len <= LSP_MESSAGE_MAX as u64 {
            let mut body = vec![0; len as usize];
            if reader.read_exact(&mut body).await.is_err() {
                return;
            }
            FromServer::Message(body)
        } else {
            let mut scan = TopLevelScan::default();
            let mut left = len;
            let mut piece = vec![0; 64 * 1024];
            while left > 0 {
                let want = left.min(piece.len() as u64) as usize;
                match reader.read(&mut piece[..want]).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        scan.feed(&piece[..read]);
                        left -= read as u64;
                    }
                }
            }
            FromServer::TooLarge { scan, len }
        };
        if messages.send(message).await.is_err() {
            return;
        }
    }
}

/// The `Content-Length` of the next message; `None` at a clean end of output.
async fn read_header(reader: &mut BufReader<ChildStdout>) -> io::Result<Option<u64>> {
    let mut used = 0;
    let mut length = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let room = (HEADER_MAX - used) as u64;
        let read = (&mut *reader).take(room).read_until(b'\n', &mut line).await?;
        if read == 0 {
            return if used == 0 {
                Ok(None)
            } else if room == 0 {
                Err(io::Error::other(format!("a header longer than {HEADER_MAX} bytes")))
            } else {
                Err(io::Error::other("the output ended inside a header"))
            };
        }
        used += read;
        if line.last() != Some(&b'\n') {
            return Err(if used >= HEADER_MAX {
                io::Error::other(format!("a header longer than {HEADER_MAX} bytes"))
            } else {
                io::Error::other("the output ended inside a header")
            });
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            return length
                .map(Some)
                .ok_or_else(|| io::Error::other("a header without Content-Length"));
        }
        let Some((name, value)) = text.split_once(':') else {
            return Err(io::Error::other("a header line without a colon"));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| io::Error::other("a Content-Length that is not a number"))?,
            );
        }
    }
}

struct Budget {
    queued: AtomicUsize,
    room: Notify,
}

/// The server's stdin: messages are written in the order they are handed over, each with its `Content-Length`
/// header, by a task of its own, so neither direction waits on the other.
#[derive(Clone)]
pub struct Writer {
    messages: mpsc::UnboundedSender<Vec<u8>>,
    budget: Arc<Budget>,
}

impl Writer {
    pub fn start(stdin: ChildStdin) -> Writer {
        let (messages, mut receiver) = mpsc::unbounded_channel::<Vec<u8>>();
        let budget = Arc::new(Budget {
            queued: AtomicUsize::new(0),
            room: Notify::new(),
        });
        let accounts = budget.clone();
        tokio::spawn(async move {
            let mut stdin = Some(stdin);
            while let Some(body) = receiver.recv().await {
                if let Some(pipe) = stdin.as_mut() {
                    let header = format!("Content-Length: {}\r\n\r\n", body.len());
                    let written = async {
                        pipe.write_all(header.as_bytes()).await?;
                        pipe.write_all(&body).await?;
                        pipe.flush().await
                    }
                    .await;
                    if written.is_err() {
                        // The server stopped reading for good: what follows is dropped, its bytes still counted off.
                        stdin = None;
                    }
                }
                accounts.queued.fetch_sub(body.len(), Ordering::SeqCst);
                accounts.room.notify_waiters();
            }
        });
        Writer { messages, budget }
    }

    /// Waits until less than `WRITE_HIGH` is queued (or the writer has stopped). The caller then decides, under its
    /// own lock, whether its message still goes, and hands it over with `send_now`.
    pub async fn room(&self) {
        loop {
            let room = self.budget.room.notified();
            tokio::pin!(room);
            room.as_mut().enable();
            if self.budget.queued.load(Ordering::SeqCst) < WRITE_HIGH || self.messages.is_closed() {
                return;
            }
            room.await;
        }
    }

    /// Hands over a message at once; answers how much is queued after it.
    pub fn send_now(&self, body: Vec<u8>) -> usize {
        let len = body.len();
        let queued = self.budget.queued.fetch_add(len, Ordering::SeqCst) + len;
        if self.messages.send(body).is_err() {
            self.budget.queued.fetch_sub(len, Ordering::SeqCst);
        }
        queued
    }
}

/// `true` once the process `pidfd` pins has exited.
pub fn exited(pidfd: &OwnedFd) -> bool {
    crate::daemon::sidecars::exited(pidfd).unwrap_or(true)
}

/// Signals the process group the unreaped leader `pid` reserves.
pub fn signal_group(pid: u32, signal: i32) {
    // SAFETY: kill takes no pointers; the leader is not reaped yet, so its group id is still its own.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}
