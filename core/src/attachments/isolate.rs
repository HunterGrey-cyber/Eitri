//! Untrusted image bytes are decoded in a short-lived child process with its own memory and CPU limits.
//!
//! A decoder can ask for far more memory than an image's declared dimensions imply: image-webp's lossless decoder
//! keeps one set of five Huffman tables for every prefix-code group up to the highest one the entropy image names,
//! so a 4 MB file with a 1x1 canvas can ask for well over a gigabyte. A failed allocation aborts the process; it does
//! not unwind. In a child, that abort ends only the child, and the caller gets a refusal.
//!
//! The child is the caller's own executable (`DecoderCommand`): `eitri attachment-decoder` in the product, the test
//! binary in tests. Its descriptors: 0 the request (a header, then the raw bytes), 3 the result pipe; stdout and
//! stderr are /dev/null, so nothing a library prints can pass for a result, and every other descriptor is closed at
//! exec. Before exec the parent sets a CPU-time limit, no core dumps and death with the spawning thread; the child
//! lowers its own address-space limit to its current size plus a headroom after reading the request header and
//! before it reads the raw bytes. These are resource limits, not a sandbox: there is no seccomp filter and no
//! `no_new_privs`, and the child runs as the user with the user's files.
//!
//! The parent holds the child through a pidfd taken right after spawning it, so neither the deadline's SIGKILL nor
//! the wait can reach another process, and decoding does not depend on how SIGCHLD is handled: when it is ignored
//! (a disposition that survives exec) the kernel reaps the child itself, and the result pipe alone decides. One
//! wall-clock deadline covers writing the request, reading the result and waiting for the exit. The result is
//! checked, not trusted: its type, sizes (against the image's own header) and bounds, and the digest is the
//! parent's own.
//!
//! Kernel floor: Linux 5.11 (`pidfd_open` 5.3, `waitid(P_PIDFD)` 5.4, `CLOSE_RANGE_CLOEXEC` 5.11). On an older
//! kernel, or under a seccomp filter that denies one of these calls (some container runtimes and sandboxes), no child
//! can be run and that import is `DecoderUnavailable`; the first such failure is written to stderr once per process and
//! kept for [`decoder_setup_failure`].

use super::normalize::normalize;
use super::*;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The hidden subcommand of the `eitri` executable that runs [`decoder_child_main`].
pub const SUBCOMMAND: &str = "attachment-decoder";

/// Address space the child may add to what it has mapped at start. The largest images a user can attach need at most
/// 480 MiB of it (an eXIf-rotated 40-megapixel 16-bit RGB PNG; a rotated 40-megapixel JPEG 240 MiB, a 40-megapixel
/// PNG with transparency 264 MiB: `measure_the_largest_legitimate_decodes`); the hostile lossless WebP above needs
/// more than 1 GiB (`measure_vp8l_prefix_group_allocation`).
pub const DECODER_ADDRESS_HEADROOM: u64 = 768 << 20;
/// CPU seconds before SIGXCPU; the hard limit, one second later, is SIGKILL.
pub const DECODER_CPU_SECONDS: u64 = 30;
/// Wall-clock time before the parent kills the child.
pub const DECODER_WALL_SECONDS: u64 = 60;

const RESULT_FD: RawFd = 3;
const MAGIC: [u8; 8] = *b"EITRIDC1";
const HEADER_LEN: usize = 24;
// Tag, type, width, height, byte count and thumbnail count around the two byte strings.
const OK_FRAME_OVERHEAD: u64 = 1 + 1 + 4 + 4 + 8 + 8;
const REFUSAL_FRAME_LEN: usize = 1 + 1 + 8 + 8;

/// The program the decoder child runs: the caller's own executable with the arguments that reach
/// [`decoder_child_main`]. Its environment is exactly `env`.
#[derive(Clone, Debug)]
pub struct DecoderCommand {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    headroom: u64,
    cpu_seconds: u64,
    wall: Duration,
}

impl DecoderCommand {
    pub fn new<A: Into<OsString>>(program: impl Into<PathBuf>, args: impl IntoIterator<Item = A>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: Vec::new(),
            headroom: DECODER_ADDRESS_HEADROOM,
            cpu_seconds: DECODER_CPU_SECONDS,
            wall: Duration::from_secs(DECODER_WALL_SECONDS),
        }
    }

    /// The running executable's `attachment-decoder` subcommand. `/proc/self/exe` still names this build after the
    /// file on disk is replaced.
    pub fn this_executable() -> Self {
        Self::new("/proc/self/exe", [SUBCOMMAND])
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    #[cfg(test)]
    pub(crate) fn limits(mut self, headroom: u64, cpu_seconds: u64, wall: Duration) -> Self {
        self.headroom = headroom;
        self.cpu_seconds = cpu_seconds;
        self.wall = wall;
        self
    }
}

/// [`normalize`] in a child process: the only way untrusted bytes reach a decoder. Blocks for the child's life (at
/// most the wall-clock deadline), so it belongs on a worker thread.
pub fn normalize_in_child(
    raw: &[u8],
    max_out_bytes: u64,
    decoder: &DecoderCommand,
) -> Result<Normalized, ImageRefusal> {
    if raw.is_empty() {
        return Err(ImageRefusal::Empty);
    }
    if raw.len() as u64 > RAW_MAX_BYTES {
        return Err(ImageRefusal::TooLargeRaw {
            bytes: raw.len() as u64,
        });
    }
    let run = run_child(raw, max_out_bytes, decoder).map_err(|error| {
        note_setup_failure(&error);
        ImageRefusal::DecoderUnavailable(error.to_string())
    })?;
    let parsed = || {
        let result = run.frame.as_deref().and_then(|frame| parse_frame(frame, max_out_bytes));
        result.unwrap_or(Err(ImageRefusal::Undecodable))
    };
    match run.ending {
        // With the status gone, an aborted child is still told apart: it wrote no result.
        Ending::Exited(0) | Ending::ReapedByTheKernel => parsed(),
        // Rust aborts on a failed allocation, and the address-space limit is what makes one fail.
        Ending::Signaled(libc::SIGABRT) => Err(ImageRefusal::TooLargeToDecode),
        _ => Err(ImageRefusal::Undecodable),
    }
}

static SETUP_FAILURE: OnceLock<String> = OnceLock::new();

/// The first reason this process could not run a decoder child, if any: a missing executable, or a kernel or seccomp
/// filter without the calls the module doc lists. This is diagnostic history only; each result carries its own error.
pub fn decoder_setup_failure() -> Option<&'static str> {
    SETUP_FAILURE.get().map(String::as_str)
}

fn note_setup_failure(error: &io::Error) {
    if SETUP_FAILURE.set(error.to_string()).is_ok() {
        eprintln!(
            "eitri: could not run the image decoder child ({error}); this import was refused. It needs \
             Linux 5.11 or later, with pidfd_open, waitid(P_PIDFD) and close_range allowed."
        );
    }
}

// The failing step's name in front of an error, for `decoder_setup_failure`.
fn step<T>(what: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| io::Error::new(error.kind(), format!("{what}: {error}")))
}

/// The child's side: read the request on stdin, normalize, write one result on descriptor 3 and exit. It never
/// returns, and refuses to read the raw bytes when it cannot lower its address-space limit or descriptor 3 is not
/// a pipe.
pub fn decoder_child_main() -> ! {
    std::process::exit(match serve() {
        Ok(()) => 0,
        Err(_) => 2,
    })
}

fn serve() -> io::Result<()> {
    // SAFETY: `fstat` writes only the `stat` it is given.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(RESULT_FD, &mut stat) } != 0 || stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err(io::Error::other("descriptor 3 is not the result pipe"));
    }
    let mut input = io::stdin().lock();
    let mut header = [0u8; HEADER_LEN];
    input.read_exact(&mut header)?;
    if header[..8] != MAGIC {
        return Err(io::Error::other("not a decoder request"));
    }
    let max_out_bytes = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let headroom = u64::from_le_bytes(header[16..24].try_into().unwrap());
    limit_address_space(headroom)?;
    let mut raw = Vec::new();
    input.take(RAW_MAX_BYTES + 1).read_to_end(&mut raw)?;
    let result =
        catch_unwind(AssertUnwindSafe(|| normalize(&raw, max_out_bytes))).unwrap_or(Err(ImageRefusal::Undecodable));
    drop(raw);
    let frame = encode_frame(&result);
    // SAFETY: descriptor 3 was checked to be a pipe above and nothing else in this process owns it.
    let mut out = unsafe { File::from_raw_fd(RESULT_FD) };
    out.write_all(&frame)
}

fn limit_address_space(headroom: u64) -> io::Result<()> {
    let statm = std::fs::read_to_string("/proc/self/statm")?;
    let pages: u64 = statm
        .split_whitespace()
        .next()
        .and_then(|pages| pages.parse().ok())
        .ok_or_else(|| io::Error::other("unreadable /proc/self/statm"))?;
    // SAFETY: `sysconf` reads no memory of ours.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(io::Error::other("no page size"));
    }
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes only the `rlimit` it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut current) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let wanted = pages.saturating_mul(page as u64).saturating_add(headroom);
    let limit = wanted.min(current.rlim_max);
    // Soft and hard together: nothing later in this process can raise it again.
    let limit = libc::rlimit {
        rlim_cur: limit,
        rlim_max: limit,
    };
    // SAFETY: `setrlimit` reads only the `rlimit` it is given.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `pipe2` writes two descriptors into `fds`, which this function then owns.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// Runs between fork and exec: only async-signal-safe calls, no allocation.
fn child_setup(write_fd: RawFd, cpu_seconds: u64) -> io::Result<()> {
    // SAFETY: each call takes plain integers or a pointer to a local `rlimit`.
    unsafe {
        let placed = if write_fd == RESULT_FD {
            libc::fcntl(RESULT_FD, libc::F_SETFD, 0)
        } else {
            libc::dup2(write_fd, RESULT_FD)
        };
        if placed < 0 {
            return Err(io::Error::last_os_error());
        }
        // Whatever else this process holds without close-on-exec (a toolkit's or a library's) stays here.
        if libc::syscall(
            libc::SYS_close_range,
            4 as libc::c_uint,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        let cpu = libc::rlimit {
            rlim_cur: cpu_seconds,
            rlim_max: cpu_seconds + 1,
        };
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0
            || libc::setrlimit(libc::RLIMIT_CORE, &no_core) != 0
            || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// How the child ended. A process that ignores SIGCHLD has the kernel reap its children, and their status is gone.
enum Ending {
    Exited(i32),
    Signaled(i32),
    ReapedByTheKernel,
    StillRunning,
}

struct Run {
    ending: Ending,
    // `None` unless the result arrived whole before the deadline.
    frame: Option<Vec<u8>>,
}

// A pidfd for this process's own child. A pidfd names the process it was opened for for its whole life, so a signal
// through it can never reach another process, reaped or not.
struct ChildFd(OwnedFd);

impl ChildFd {
    fn open(pid: u32) -> io::Result<Self> {
        // SAFETY: `pidfd_open` takes a pid and flags and returns a new descriptor (close-on-exec) or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: new and owned by nothing else.
        let fd = Self(unsafe { OwnedFd::from_raw_fd(raw as RawFd) });
        // Between spawning and opening, a child the kernel reaped could have left its number to another process.
        // Only this process's own children can be waited for; anything else is ECHILD. Left open: with SIGCHLD
        // ignored, the number reused by another child of this process within that window (closing it needs
        // `clone3(CLONE_PIDFD)`, which std offers only unstably).
        // SAFETY: a zeroed `siginfo_t` is a valid buffer for `waitid`, which writes only it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        if unsafe { libc::waitid(libc::P_PIDFD, fd.0.as_raw_fd() as libc::id_t, &mut info, flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    fn kill(&self) {
        // SAFETY: `pidfd_send_signal` with a null `siginfo` reads no memory of ours.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0 as libc::c_uint,
            )
        };
    }

    // Whether the child has ended by `until` (a pidfd is readable once it has).
    fn exited_by(&self, until: Instant) -> bool {
        loop {
            let ms = until
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one `pollfd` that lives for the call.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n >= 0 {
                return n > 0;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return false;
            }
        }
    }

    fn reap(&self) -> Ending {
        // SAFETY: as in `open`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            let flags = libc::WEXITED | libc::WNOHANG;
            if unsafe { libc::waitid(libc::P_PIDFD, self.0.as_raw_fd() as libc::id_t, &mut info, flags) } == 0 {
                break;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Ending::ReapedByTheKernel;
            }
        }
        // SAFETY: `waitid` filled `info`, or left it zeroed (pid 0: still running).
        let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        if pid == 0 {
            Ending::StillRunning
        } else if info.si_code == libc::CLD_EXITED {
            Ending::Exited(status)
        } else {
            Ending::Signaled(status)
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fcntl` on a descriptor this function's caller owns reads no memory of ours.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run_child(raw: &[u8], max_out_bytes: u64, decoder: &DecoderCommand) -> io::Result<Run> {
    let (read_end, write_end) = step("pipe2", pipe())?;
    let mut command = Command::new(&decoder.program);
    command
        .args(&decoder.args)
        .env_clear()
        .envs(decoder.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let write_fd = write_end.as_raw_fd();
    let cpu_seconds = decoder.cpu_seconds;
    // SAFETY: `child_setup` makes only async-signal-safe calls.
    unsafe {
        command.pre_exec(move || child_setup(write_fd, cpu_seconds));
    }
    let mut child = step("spawn", command.spawn())?;
    drop(write_end);
    let deadline = Instant::now() + decoder.wall;
    let stdin = child.stdin.take();
    let pidfd = match ChildFd::open(child.id()) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            // Without a pidfd nothing may signal the child. Its input ends here, so it exits; reap it off this thread.
            drop(stdin);
            std::thread::spawn(move || child.wait());
            return Err(io::Error::new(
                error.kind(),
                format!("pidfd_open/waitid(P_PIDFD): {error}"),
            ));
        }
    };
    let mut result = File::from(read_end);
    let mut header = MAGIC.to_vec();
    header.extend(max_out_bytes.to_le_bytes());
    header.extend(decoder.headroom.to_le_bytes());
    let limit = OK_FRAME_OVERHEAD + max_out_bytes.min(RAW_MAX_BYTES) + THUMB_MAX_BYTES as u64;
    let mut frame = Vec::new();
    let stop = exchange(&pidfd, stdin, &mut result, [&header, raw], limit, deadline, &mut frame);
    let mut complete = matches!(stop, Ok(Stop::EndOfResult | Stop::Exited));
    if !complete || !pidfd.exited_by(deadline) {
        complete = false;
        pidfd.kill();
        pidfd.exited_by(Instant::now() + Duration::from_secs(5));
    }
    let ending = pidfd.reap();
    if matches!(ending, Ending::StillRunning) {
        // Killed but not yet gone (blocked in the kernel): reap it off this thread when it is.
        std::thread::spawn(move || child.wait());
    }
    Ok(Run {
        ending,
        frame: complete.then_some(frame),
    })
}

enum Stop {
    EndOfResult,
    // The child ended and everything it wrote has been read; whoever still holds the pipe is not the child.
    Exited,
    Deadline,
    Overflow,
}

// Writes the request and reads the result together, until the result ends, the child ends or the deadline passes.
fn exchange(
    pidfd: &ChildFd,
    mut stdin: Option<ChildStdin>,
    result: &mut File,
    request: [&[u8]; 2],
    limit: u64,
    deadline: Instant,
    frame: &mut Vec<u8>,
) -> io::Result<Stop> {
    set_nonblocking(result.as_raw_fd())?;
    if let Some(stdin) = &stdin {
        set_nonblocking(stdin.as_raw_fd())?;
    }
    let total = request[0].len() + request[1].len();
    let mut sent = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Stop::Deadline);
        }
        let mut fds = [
            result.as_raw_fd(),
            pidfd.0.as_raw_fd(),
            stdin.as_ref().map_or(-1, AsRawFd::as_raw_fd),
        ]
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        fds[2].events = libc::POLLOUT;
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: three `pollfd`s that live for the call; a negative descriptor is skipped.
        if unsafe { libc::poll(fds.as_mut_ptr(), 3, ms) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[2].revents != 0 {
            while let Some(pipe) = stdin.as_mut() {
                let pending = if sent < request[0].len() {
                    &request[0][sent..]
                } else {
                    &request[1][sent - request[0].len()..]
                };
                match pipe.write(pending) {
                    Ok(n) => {
                        sent += n;
                        if sent == total {
                            stdin = None;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    // A child that stops reading is judged by how it ends.
                    Err(_) => stdin = None,
                }
            }
        }
        // Read what is there before looking at the exit: a child writes everything before it ends.
        loop {
            let mut buffer = [0u8; 65536];
            match result.read(&mut buffer) {
                Ok(0) => return Ok(Stop::EndOfResult),
                Ok(n) => {
                    frame.extend_from_slice(&buffer[..n]);
                    if frame.len() as u64 > limit {
                        return Ok(Stop::Overflow);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        if fds[1].revents != 0 {
            return Ok(Stop::Exited);
        }
    }
}

fn encode_frame(result: &Result<Normalized, ImageRefusal>) -> Vec<u8> {
    match result {
        Ok(n) => {
            let mut out = vec![0, type_code(n.image_type)];
            out.extend(n.width.to_le_bytes());
            out.extend(n.height.to_le_bytes());
            out.extend((n.bytes.len() as u64).to_le_bytes());
            out.extend(&n.bytes);
            out.extend((n.thumbnail_png.len() as u64).to_le_bytes());
            out.extend(&n.thumbnail_png);
            out
        }
        Err(refusal) => {
            let (code, a, b) = match *refusal {
                ImageRefusal::NotAnImage => (0, 0, 0),
                ImageRefusal::UnsupportedType => (1, 0, 0),
                ImageRefusal::Undecodable | ImageRefusal::DecoderUnavailable(_) => (2, 0, 0),
                ImageRefusal::FrameOutsideScreen => (8, 0, 0),
                ImageRefusal::CanvasBuffersTooLarge => (9, 0, 0),
                ImageRefusal::TooManyPixels { width, height } => (3, u64::from(width), u64::from(height)),
                ImageRefusal::TooLargeRaw { bytes } => (4, bytes, 0),
                ImageRefusal::TooLargeAfterResize { bytes, limit } => (5, bytes, limit),
                ImageRefusal::TooLargeToDecode => (6, 0, 0),
                ImageRefusal::Empty => (7, 0, 0),
            };
            let mut out = vec![1, code];
            out.extend(u64::to_le_bytes(a));
            out.extend(u64::to_le_bytes(b));
            out
        }
    }
}

fn type_code(kind: ImageType) -> u8 {
    match kind {
        ImageType::Png => 0,
        ImageType::Jpeg => 1,
        ImageType::Gif => 2,
        ImageType::Webp => 3,
    }
}

// `None` for anything a correct child cannot have written.
fn parse_frame(frame: &[u8], max_out_bytes: u64) -> Option<Result<Normalized, ImageRefusal>> {
    let u32_at = |at: usize| Some(u32::from_le_bytes(frame.get(at..at + 4)?.try_into().unwrap()));
    let u64_at = |at: usize| Some(u64::from_le_bytes(frame.get(at..at + 8)?.try_into().unwrap()));
    match *frame.first()? {
        0 => {
            let image_type = match *frame.get(1)? {
                0 => ImageType::Png,
                1 => ImageType::Jpeg,
                _ => return None,
            };
            let (width, height) = (u32_at(2)?, u32_at(6)?);
            let len = usize::try_from(u64_at(10)?).ok()?;
            let bytes = frame.get(18..18_usize.checked_add(len)?)?;
            let thumb_len = usize::try_from(u64_at(18 + len)?).ok()?;
            let thumbnail_png = frame.get(26 + len..)?;
            let stored = stored_size(bytes, image_type)?;
            let bounded = (stored == (width, height) || stored == (height, width))
                && (1..=OUT_MAX_SIDE).contains(&width)
                && (1..=OUT_MAX_SIDE).contains(&height)
                && u64::from(width) * u64::from(height) <= OUT_MAX_AREA
                && bytes.len() as u64 <= max_out_bytes
                && sniff(bytes) == Some(image_type)
                && thumbnail_png.len() == thumb_len
                && thumb_len <= THUMB_MAX_BYTES
                && sniff(thumbnail_png) == Some(ImageType::Png)
                && stored_size(thumbnail_png, ImageType::Png)
                    .is_some_and(|(w, h)| (1..=THUMB_MAX_SIDE).contains(&w) && (1..=THUMB_MAX_SIDE).contains(&h));
            bounded.then(|| {
                Ok(Normalized {
                    image_type,
                    width,
                    height,
                    sha256: Sha256::digest(bytes).into(),
                    bytes: bytes.to_vec(),
                    thumbnail_png: thumbnail_png.to_vec(),
                })
            })
        }
        1 if frame.len() == REFUSAL_FRAME_LEN => {
            let (a, b) = (u64_at(2)?, u64_at(10)?);
            let small = |value: u64| u32::try_from(value).ok();
            Some(Err(match frame[1] {
                0 => ImageRefusal::NotAnImage,
                1 => ImageRefusal::UnsupportedType,
                2 => ImageRefusal::Undecodable,
                3 => ImageRefusal::TooManyPixels {
                    width: small(a)?,
                    height: small(b)?,
                },
                4 => ImageRefusal::TooLargeRaw { bytes: a },
                5 => ImageRefusal::TooLargeAfterResize { bytes: a, limit: b },
                6 => ImageRefusal::TooLargeToDecode,
                7 => ImageRefusal::Empty,
                8 => ImageRefusal::FrameOutsideScreen,
                9 => ImageRefusal::CanvasBuffersTooLarge,
                _ => return None,
            }))
        }
        _ => None,
    }
}

// The size the image's own header gives: PNG's IHDR, or a JPEG's first frame header (SOFn) before its scan.
fn stored_size(bytes: &[u8], kind: ImageType) -> Option<(u32, u32)> {
    let be16 = |at: usize| {
        Some(u32::from(u16::from_be_bytes(
            bytes.get(at..at + 2)?.try_into().unwrap(),
        )))
    };
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().unwrap()));
    if kind == ImageType::Png {
        return (bytes.get(12..16)? == b"IHDR").then_some((be32(16)?, be32(20)?));
    }
    let mut pos = 2;
    loop {
        if *bytes.get(pos)? != 0xff {
            return None;
        }
        while bytes.get(pos) == Some(&0xff) {
            pos += 1;
        }
        let marker = *bytes.get(pos)?;
        pos += 1;
        match marker {
            0x01 | 0xd0..=0xd8 => continue,
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => return Some((be16(pos + 5)?, be16(pos + 3)?)),
            0xd9 | 0xda => return None,
            _ => {}
        }
        let len = be16(pos)? as usize;
        if len < 2 {
            return None;
        }
        pos += len;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const CHILD_VAR: &str = "EITRI_TEST_ATTACHMENT_DECODER";
    const LEAK_FD_VAR: &str = "EITRI_TEST_ATTACHMENT_LEAK_FD";
    const SIGCHLD_VAR: &str = "EITRI_TEST_ATTACHMENT_SIGCHLD_IGNORED";

    /// This test binary as the decoder: `decoder_child_entry` serves when `CHILD_VAR` is `decode`; other values
    /// make it misbehave in a named way.
    pub(crate) fn test_decoder() -> DecoderCommand {
        test_decoder_mode("decode")
    }

    fn test_decoder_mode(mode: &str) -> DecoderCommand {
        DecoderCommand::new(
            std::env::current_exe().expect("the test binary's path"),
            [
                "--exact",
                "attachments::isolate::tests::decoder_child_entry",
                "--test-threads=1",
                "-q",
            ],
        )
        .env(CHILD_VAR, mode)
    }

    fn result_pipe() -> File {
        // SAFETY: the parent placed the result pipe at descriptor 3 for this child.
        unsafe { File::from_raw_fd(RESULT_FD) }
    }

    #[test]
    fn a_child_thumbnail_must_fit_both_sides() {
        let raw = super::super::normalize::tests::png(1, 1, 255);
        let mut image = normalize(&raw, RAW_MAX_BYTES).unwrap();
        for (width, height) in [
            (THUMB_MAX_SIDE, THUMB_MAX_SIDE),
            (THUMB_MAX_SIDE + 1, 1),
            (1, THUMB_MAX_SIDE + 1),
            (0, 1),
        ] {
            image.thumbnail_png = super::super::normalize::tests::png(width.max(1), height, 255);
            image.thumbnail_png[16..20].copy_from_slice(&width.to_be_bytes());
            let parsed = parse_frame(&encode_frame(&Ok(image)), RAW_MAX_BYTES);
            assert_eq!(
                parsed.is_some(),
                width > 0 && width <= THUMB_MAX_SIDE && height <= THUMB_MAX_SIDE,
                "{width} x {height}"
            );
            image = normalize(&raw, RAW_MAX_BYTES).unwrap();
        }
    }

    #[test]
    fn decoder_child_entry() {
        let Some(mode) = std::env::var_os(CHILD_VAR) else {
            return;
        };
        match mode.to_str() {
            Some("decode") => decoder_child_main(),
            Some("abort") => std::process::abort(),
            Some("silent") => std::process::exit(0),
            Some("hang") => {
                // Never reads stdin. Should the deadline fail, a success arrives and the test fails.
                std::thread::sleep(Duration::from_secs(120));
                let raw = super::super::normalize::tests::png(1, 1, 255);
                let _ = result_pipe().write_all(&encode_frame(&normalize(&raw, RAW_MAX_BYTES)));
                std::process::exit(0);
            }
            Some("wrong-type") => {
                let raw = super::super::normalize::tests::png(1, 1, 255);
                let mut frame = encode_frame(&normalize(&raw, RAW_MAX_BYTES));
                frame[1] = type_code(ImageType::Jpeg);
                let _ = result_pipe().write_all(&frame);
                std::process::exit(0);
            }
            Some("flood") => {
                let _ = result_pipe().write_all(&vec![0; 64 << 20]);
                std::process::exit(0);
            }
            Some("stray") => {
                // A process that outlives the child keeps a copy of the result pipe, then ends on its own.
                let _ = std::process::Command::new("/bin/sleep").arg("30").spawn();
                std::process::exit(0);
            }
            Some("fd-check") => {
                let fd: RawFd = std::env::var(LEAK_FD_VAR).unwrap().parse().unwrap();
                // SAFETY: `fcntl` on a number reads no memory of ours; a closed descriptor is EBADF.
                if unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0 {
                    std::process::exit(4);
                }
                decoder_child_main();
            }
            _ => std::process::exit(3),
        }
    }

    #[test]
    fn a_small_png_round_trips_through_the_child_byte_for_byte() {
        let raw = super::super::normalize::tests::png(12, 7, 255);
        let n = normalize_in_child(&raw, RAW_MAX_BYTES, &test_decoder()).unwrap();
        assert_eq!(n.bytes, raw);
        assert_eq!((n.width, n.height, n.image_type), (12, 7, ImageType::Png));
        assert_eq!(n.sha256, <[u8; 32]>::from(Sha256::digest(&raw)));
        assert_eq!(sniff(&n.thumbnail_png), Some(ImageType::Png));
        let resized = normalize_in_child(
            &super::super::normalize::tests::png(1600, 2, 255),
            RAW_MAX_BYTES,
            &test_decoder(),
        )
        .unwrap();
        assert_eq!((resized.width, resized.image_type), (OUT_MAX_SIDE, ImageType::Jpeg));
    }

    #[test]
    fn refusals_cross_the_child_unchanged() {
        let decoder = test_decoder();
        let png = super::super::normalize::tests::png(2, 1, 255);
        for (raw, expected) in [
            (b"plain text".to_vec(), ImageRefusal::NotAnImage),
            (b"BM\0\0\0\0".to_vec(), ImageRefusal::UnsupportedType),
            (b"\xff\xd8\xff\xe0truncated".to_vec(), ImageRefusal::Undecodable),
            (
                b"GIF89a\x02\0\x02\0\0\0\0\x2c\x01\0\0\0\x02\0\x01\0\0".to_vec(),
                ImageRefusal::FrameOutsideScreen,
            ),
            (
                {
                    let mut raw = b"RIFF\x12\0\0\0WEBPVP8L\x05\0\0\0\x2f".to_vec();
                    raw.extend((5999u32 | (5999u32 << 14)).to_le_bytes());
                    raw.push(0);
                    raw
                },
                ImageRefusal::CanvasBuffersTooLarge,
            ),
            (
                super::super::normalize::boundary_tests::header(10_000, 5_000),
                ImageRefusal::TooManyPixels {
                    width: 10_000,
                    height: 5_000,
                },
            ),
        ] {
            assert_eq!(normalize_in_child(&raw, RAW_MAX_BYTES, &decoder).err(), Some(expected));
        }
        assert_eq!(
            normalize_in_child(&png, 17, &decoder).err(),
            Some(ImageRefusal::TooLargeAfterResize {
                bytes: png.len() as u64,
                limit: 17
            })
        );
        // Refused before any child starts.
        let nowhere = DecoderCommand::new("/nonexistent/decoder", ["x"]);
        assert_eq!(
            normalize_in_child(&[], RAW_MAX_BYTES, &nowhere).err(),
            Some(ImageRefusal::Empty)
        );
        let huge = vec![0; RAW_MAX_BYTES as usize + 1];
        assert_eq!(
            normalize_in_child(&huge, RAW_MAX_BYTES, &nowhere).err(),
            Some(ImageRefusal::TooLargeRaw {
                bytes: RAW_MAX_BYTES + 1
            })
        );
        assert_eq!(
            normalize_in_child(&png, RAW_MAX_BYTES, &nowhere).err(),
            Some(ImageRefusal::DecoderUnavailable(
                "spawn: No such file or directory (os error 2)".into()
            ))
        );
    }

    #[test]
    fn a_vp8l_with_65536_prefix_code_groups_is_refused_and_the_caller_survives() {
        let hostile = super::super::normalize::regressions::vp8l_with_prefix_groups(65536);
        assert_eq!(
            normalize_in_child(&hostile, RAW_MAX_BYTES, &test_decoder()).err(),
            Some(ImageRefusal::TooLargeToDecode)
        );
        // The same file with few groups is an ordinary image.
        let modest = super::super::normalize::regressions::vp8l_with_prefix_groups(300);
        assert!(normalize_in_child(&modest, RAW_MAX_BYTES, &test_decoder()).is_ok());
    }

    #[test]
    fn a_child_that_crashes_lies_or_floods_is_a_refusal() {
        let raw = super::super::normalize::tests::png(1, 1, 255);
        for (mode, expected) in [
            ("abort", ImageRefusal::TooLargeToDecode),
            ("silent", ImageRefusal::Undecodable),
            ("wrong-type", ImageRefusal::Undecodable),
            ("flood", ImageRefusal::Undecodable),
            ("unknown", ImageRefusal::Undecodable),
        ] {
            assert_eq!(
                normalize_in_child(&raw, RAW_MAX_BYTES, &test_decoder_mode(mode)).err(),
                Some(expected),
                "{mode}"
            );
        }
    }

    // The deadline is the configured one: not before it, and not long after it.
    fn assert_refused_at_the_deadline(mode: &str) {
        // More than a pipe buffer, so the request write also blocks until the child is killed.
        let mut raw = super::super::normalize::tests::png(1, 1, 255);
        raw.resize(1 << 20, 0);
        let wall = Duration::from_secs(2);
        let decoder = test_decoder_mode(mode).limits(DECODER_ADDRESS_HEADROOM, 5, wall);
        let started = std::time::Instant::now();
        let result = normalize_in_child(&raw, RAW_MAX_BYTES, &decoder);
        let took = started.elapsed();
        assert_eq!(result.err(), Some(ImageRefusal::Undecodable));
        // The child sleeps for 120 s: anything well short of that was the deadline's doing, not the child's end.
        assert!(
            took >= wall && took < Duration::from_secs(60),
            "{mode}: refused after {took:?}"
        );
    }

    #[test]
    fn a_child_that_hangs_is_killed_at_the_deadline() {
        assert_refused_at_the_deadline("hang");
    }

    #[test]
    fn a_stray_copy_of_the_result_pipe_does_not_hold_the_caller() {
        // The child exits at once; its own child keeps the pipe's write end for 30 seconds.
        assert!(
            std::path::Path::new("/bin/sleep").exists(),
            "/bin/sleep is needed to hold the pipe"
        );
        let decoder = test_decoder_mode("stray").limits(DECODER_ADDRESS_HEADROOM, 5, Duration::from_secs(2));
        let raw = super::super::normalize::tests::png(1, 1, 255);
        let started = std::time::Instant::now();
        assert_eq!(
            normalize_in_child(&raw, RAW_MAX_BYTES, &decoder).err(),
            Some(ImageRefusal::Undecodable)
        );
        let took = started.elapsed();
        assert!(took < Duration::from_secs(15), "returned after {took:?}");
    }

    #[test]
    fn a_decoder_that_cannot_run_is_reported() {
        let nowhere = DecoderCommand::new("/nonexistent/decoder", ["x"]);
        let raw = super::super::normalize::tests::png(1, 1, 255);
        assert!(matches!(normalize_in_child(&raw, RAW_MAX_BYTES, &nowhere).err(),
            Some(ImageRefusal::DecoderUnavailable(why)) if why.contains("spawn:")));
        // Another test's failure may have been the first; either way the process now holds a named reason.
        assert!(decoder_setup_failure().is_some_and(|why| why.contains(": ")));
    }

    #[test]
    fn no_inherited_descriptor_above_three_reaches_the_child() {
        let (source, _) = pipe().unwrap();
        // SAFETY: `F_DUPFD` returns a new descriptor without close-on-exec, owned here.
        let leaked = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 900) };
        assert!(leaked >= 900);
        // SAFETY: just created and owned by nothing else.
        let leaked = unsafe { OwnedFd::from_raw_fd(leaked) };
        let decoder = test_decoder_mode("fd-check").env(LEAK_FD_VAR, leaked.as_raw_fd().to_string());
        let raw = super::super::normalize::tests::png(2, 2, 255);
        let result = normalize_in_child(&raw, RAW_MAX_BYTES, &decoder);
        drop(leaked);
        assert!(result.is_ok());
    }

    #[test]
    fn decoding_does_not_depend_on_how_sigchld_is_handled() {
        // An ignored SIGCHLD survives exec; with it the kernel reaps children itself and every wait says ECHILD.
        if std::env::var_os(SIGCHLD_VAR).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "attachments::isolate::tests::decoding_does_not_depend_on_how_sigchld_is_handled",
                    "--test-threads=1",
                ])
                .env(SIGCHLD_VAR, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // SAFETY: setting a disposition to SIG_IGN installs no handler.
        assert_ne!(unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) }, libc::SIG_ERR);
        let raw = super::super::normalize::tests::png(12, 7, 255);
        let n = normalize_in_child(&raw, RAW_MAX_BYTES, &test_decoder()).unwrap();
        assert_eq!(n.bytes, raw);
        let text = normalize_in_child(b"plain text", RAW_MAX_BYTES, &test_decoder());
        assert_eq!(text.err(), Some(ImageRefusal::NotAnImage));
        let hostile = super::super::normalize::regressions::vp8l_with_prefix_groups(65536);
        assert!(normalize_in_child(&hostile, RAW_MAX_BYTES, &test_decoder()).is_err());
        assert_refused_at_the_deadline("hang");
    }

    #[test]
    fn frames_out_of_bounds_are_rejected() {
        let raw = super::super::normalize::tests::png(3, 2, 255);
        let good = normalize(&raw, RAW_MAX_BYTES);
        let frame = encode_frame(&good);
        assert!(parse_frame(&frame, RAW_MAX_BYTES).unwrap().is_ok());
        assert!(parse_frame(&frame, raw.len() as u64 - 1).is_none());
        let mut wide = frame.clone();
        wide[2..6].copy_from_slice(&(OUT_MAX_SIDE + 1).to_le_bytes());
        assert!(parse_frame(&wide, RAW_MAX_BYTES).is_none());
        // Claimed sizes must be those of the returned image (either way round, for EXIF rotation).
        let mut taller = frame.clone();
        taller[6..10].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse_frame(&taller, RAW_MAX_BYTES).is_none());
        let mut turned = frame.clone();
        turned[2..6].copy_from_slice(&2u32.to_le_bytes());
        turned[6..10].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse_frame(&turned, RAW_MAX_BYTES).unwrap().is_ok());
        let jpeg = super::super::normalize::regressions::oriented_jpeg(1600, 20);
        let jpeg = encode_frame(&normalize(&jpeg, RAW_MAX_BYTES));
        assert!(parse_frame(&jpeg, RAW_MAX_BYTES).unwrap().is_ok());
        // A small rotated JPEG keeps its bytes: the frame names the turned size, the SOF the stored one.
        let kept = normalize(
            &super::super::normalize::regressions::oriented_jpeg(30, 10),
            RAW_MAX_BYTES,
        );
        assert_eq!(kept.as_ref().map(|n| (n.width, n.height)).ok(), Some((10, 30)));
        assert!(parse_frame(&encode_frame(&kept), RAW_MAX_BYTES).unwrap().is_ok());
        let mut wrong_jpeg = jpeg.clone();
        wrong_jpeg[2..6].copy_from_slice(&7u32.to_le_bytes());
        assert!(parse_frame(&wrong_jpeg, RAW_MAX_BYTES).is_none());
        assert!(parse_frame(&frame[..frame.len() - 1], RAW_MAX_BYTES).is_none());
        let mut longer = frame.clone();
        longer.push(0);
        assert!(parse_frame(&longer, RAW_MAX_BYTES).is_none());
        let mut refusal = encode_frame(&Err(ImageRefusal::TooManyPixels { width: 1, height: 1 }));
        refusal[2..10].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse_frame(&refusal, RAW_MAX_BYTES).is_none());
        assert!(parse_frame(&[], RAW_MAX_BYTES).is_none());
    }

    // A PNG with an eXIf chunk (orientation 6: rotate 90 degrees) after its IHDR.
    fn png_rotated_by_exif(png: &[u8]) -> Vec<u8> {
        let exif = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut body = b"eXIf".to_vec();
        body.extend(exif);
        let mut crc = !0u32;
        for byte in &body {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        let mut chunk = (exif.len() as u32).to_be_bytes().to_vec();
        chunk.extend(body);
        chunk.extend((!crc).to_be_bytes());
        let mut out = png[..33].to_vec();
        out.extend(chunk);
        out.extend(&png[33..]);
        out
    }

    /// Measures the largest images a user can attach -- an EXIF-rotated 40-megapixel JPEG (the orientation copy), a
    /// 40-megapixel PNG with transparency (the alpha scan and PNG output) and an eXIf-rotated 40-megapixel 16-bit RGB
    /// PNG (six bytes a pixel, copied by the rotation) -- by the smallest address-space headroom each decodes in, to
    /// 32 MiB, and the children's peak resident size. Run alone (`--exact --ignored`); the CPU limit is raised
    /// because test builds are unoptimized.
    #[test]
    #[ignore]
    fn measure_the_largest_legitimate_decodes() {
        let encode = |image: image::DynamicImage| {
            let mut png = std::io::Cursor::new(Vec::new());
            image.write_to(&mut png, image::ImageFormat::Png).unwrap();
            png.into_inner()
        };
        let transparent = encode(image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(
            8000,
            5000,
            |x, y| image::Rgba([x as u8, y as u8, 90, 128]),
        )));
        let deep = png_rotated_by_exif(&encode(image::DynamicImage::ImageRgb16(image::ImageBuffer::from_fn(
            8000,
            5000,
            |x, y| image::Rgb([(x * 8) as u16, (y * 13) as u16, 4000]),
        ))));
        for (name, raw) in [
            (
                "rotated JPEG",
                super::super::normalize::regressions::oriented_jpeg(8000, 5000),
            ),
            ("transparent PNG", transparent),
            ("rotated 16-bit PNG", deep),
        ] {
            assert!((raw.len() as u64) < RAW_MAX_BYTES, "{name}: {} bytes", raw.len());
            let run = |headroom: u64| {
                let decoder = test_decoder().limits(headroom, 600, Duration::from_secs(600));
                normalize_in_child(&raw, RAW_MAX_BYTES, &decoder)
            };
            let shipped = run(DECODER_ADDRESS_HEADROOM);
            let summary = shipped.as_ref().map(|n| (n.width, n.height, n.image_type));
            let (mut fails, mut fits) = (0u64, DECODER_ADDRESS_HEADROOM >> 20);
            while shipped.is_ok() && fits - fails > 32 {
                let mid = (fails + fits) / 2;
                if run(mid << 20).is_ok() {
                    fits = mid;
                } else {
                    fails = mid;
                }
            }
            // SAFETY: `getrusage` writes only the `rusage` it is given.
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
            println!(
                "{name}, {} bytes: {summary:?} at the shipped headroom; needs {fits} MiB (fails at {fails}); \
                 children's peak RSS so far {} MiB",
                raw.len(),
                usage.ru_maxrss / 1024
            );
            assert!(shipped.is_ok());
        }
    }

    /// Measures what image-webp asks for on the hostile file: the smallest headroom that lets it finish, and the
    /// child's peak resident size then. Run alone (`--exact --ignored`): the peak is over every waited child.
    #[test]
    #[ignore]
    fn measure_vp8l_prefix_group_allocation() {
        let hostile = super::super::normalize::regressions::vp8l_with_prefix_groups(65536);
        for mib in [512u64, 768, 1024, 1280, 1536, 2048, 3072] {
            let decoder = test_decoder().limits(
                mib << 20,
                DECODER_CPU_SECONDS,
                Duration::from_secs(DECODER_WALL_SECONDS),
            );
            let result = normalize_in_child(&hostile, RAW_MAX_BYTES, &decoder);
            // SAFETY: `getrusage` writes only the `rusage` it is given.
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
            println!(
                "headroom {mib} MiB: {:?}, children's peak RSS {} MiB",
                result.as_ref().map(|n| (n.width, n.height)).err(),
                usage.ru_maxrss / 1024
            );
            if result.is_ok() {
                break;
            }
        }
    }
}
