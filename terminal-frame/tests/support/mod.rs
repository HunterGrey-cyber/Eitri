//! A real PTY, a real child, a real `Term`, and a frame meter.
//!
//! NOT `alacritty_terminal::tty`: that inherits the parent environment
//! wholesale, so a measurement would depend on the developer's shell. This one
//! calls `env_clear()` and hands the child a short explicit environment.
//!
//! The read loop is the engine's model: one `read()` on the PTY master is one
//! wakeup, and one wakeup publishes one frame. That is the SAME publication
//! point `terminal-sync` produces outside a synchronized update -- inside one,
//! `terminal-sync` SUPPRESSES publication until the ESU, so the frame rate
//! measured here is an UPPER BOUND on what the real engine publishes. The
//! number of DECSET-2026 updates in the stream is counted alongside, so the
//! difference is visible rather than assumed.
#![allow(dead_code)]

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use terminal_frame::{encode_naive, encode_rle, FrameKind, FrameStats, Projector};

// ---------------------------------------------------------------------------
// PTY
// ---------------------------------------------------------------------------

pub struct Pty {
    master: OwnedFd,
    child: Child,
}

fn ptsname(master: RawFd) -> std::io::Result<CString> {
    let mut buf = [0i8; 128];
    let rc = unsafe { libc::ptsname_r(master, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let bytes: Vec<u8> = buf.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
    Ok(CString::new(bytes).unwrap())
}

impl Pty {
    pub fn spawn(program: &str, args: &[&str], env: &[(&str, &str)], cols: u16, rows: u16) -> std::io::Result<Pty> {
        let master_raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master_raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(master_raw) };
        if unsafe { libc::grantpt(master_raw) } != 0 || unsafe { libc::unlockpt(master_raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Resize is expressed in CELLS; the pixel fields are advisory.
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: cols * 8,
            ws_ypixel: rows * 16,
        };
        if unsafe { libc::ioctl(master_raw, libc::TIOCSWINSZ, &size) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let slave_name = ptsname(master_raw)?;
        let slave_raw = unsafe { libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if slave_raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };

        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave.try_clone()?));
        for (key, value) in env {
            cmd.env(key, value);
        }
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_raw, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave);

        let flags = unsafe { libc::fcntl(master_raw, libc::F_GETFL) };
        unsafe { libc::fcntl(master_raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        Ok(Pty { master, child })
    }

    fn file(&self) -> std::mem::ManuallyDrop<std::fs::File> {
        std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(self.master.as_raw_fd()) })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let mut file = self.file();
        let _ = file.write_all(bytes);
        let _ = file.flush();
    }

    /// One non-blocking `read()`. `None` means "nothing available right now".
    pub fn read(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self.file().read(buf) {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => None,
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows + 10_000
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Captures what the terminal wants to send back to the application. Without
/// this, nvim stalls on its unanswered queries and the measurement is of a
/// hung program.
#[derive(Clone, Default)]
pub struct Replies(Arc<Mutex<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        let mut buf = self.0.lock().unwrap();
        match event {
            Event::PtyWrite(text) => buf.extend_from_slice(text.as_bytes()),
            Event::ColorRequest(_, fmt) => {
                let rgb = alacritty_terminal::vte::ansi::Rgb {
                    r: 0x18,
                    g: 0x18,
                    b: 0x18,
                };
                buf.extend_from_slice(fmt(rgb).as_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                let size = WindowSize {
                    num_lines: 40,
                    num_cols: 120,
                    cell_width: 8,
                    cell_height: 16,
                };
                buf.extend_from_slice(fmt(size).as_bytes());
            }
            _ => {}
        }
    }
}

impl Replies {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

// ---------------------------------------------------------------------------
// The meter
// ---------------------------------------------------------------------------

/// One workload's measurement.
#[derive(Debug, Clone, Default)]
pub struct Measurement {
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub seconds: f64,
    pub pty_reads: u64,
    pub pty_bytes: u64,
    pub frames: u64,
    pub full_frames: u64,
    pub delta_frames: u64,
    pub cells: u64,
    pub naive_bytes: u64,
    pub rle_bytes: u64,
    pub naive_full_bytes: u64,
    pub rle_full_bytes: u64,
    pub naive_delta_bytes: u64,
    pub rle_delta_bytes: u64,
    pub largest_rle_frame: u64,
    /// DECSET 2026 updates seen in the raw stream. Publication under
    /// `terminal-sync` is one frame per update, not one per read.
    pub sync_updates: u64,
}

impl Measurement {
    pub fn frames_per_second(&self) -> f64 {
        self.frames as f64 / self.seconds.max(f64::MIN_POSITIVE)
    }

    pub fn naive_bytes_per_frame(&self) -> f64 {
        self.naive_bytes as f64 / self.frames.max(1) as f64
    }

    pub fn rle_bytes_per_frame(&self) -> f64 {
        self.rle_bytes as f64 / self.frames.max(1) as f64
    }

    pub fn naive_bytes_per_second(&self) -> f64 {
        self.naive_bytes as f64 / self.seconds.max(f64::MIN_POSITIVE)
    }

    pub fn rle_bytes_per_second(&self) -> f64 {
        self.rle_bytes as f64 / self.seconds.max(f64::MIN_POSITIVE)
    }

    pub fn full_fraction(&self) -> f64 {
        self.full_frames as f64 / self.frames.max(1) as f64
    }

    pub fn header(&self) -> String {
        format!(
            "{:<26} {:>6} {:>7} {:>7} {:>9} {:>9} {:>10} {:>10} {:>8}",
            "workload", "frames", "full", "partial", "naive B/f", "rle B/f", "naive B/s", "rle B/s", "fps"
        )
    }

    pub fn row(&self) -> String {
        format!(
            "{:<26} {:>6} {:>7} {:>7} {:>9.0} {:>9.0} {:>10.0} {:>10.0} {:>8.1}",
            self.name,
            self.frames,
            self.full_frames,
            self.delta_frames,
            self.naive_bytes_per_frame(),
            self.rle_bytes_per_frame(),
            self.naive_bytes_per_second(),
            self.rle_bytes_per_second(),
            self.frames_per_second()
        )
    }
}

/// Count non-overlapping occurrences of an ESU (`CSI ? 2026 l`).
fn count_esu(haystack: &[u8], carry: &mut Vec<u8>) -> u64 {
    const ESU: &[u8] = b"\x1b[?2026l";
    carry.extend_from_slice(haystack);
    let mut count = 0;
    let mut at = 0;
    while at + ESU.len() <= carry.len() {
        if &carry[at..at + ESU.len()] == ESU {
            count += 1;
            at += ESU.len();
        } else {
            at += 1;
        }
    }
    // Keep the last few bytes in case a sequence straddles two reads.
    let keep = carry.len().saturating_sub(ESU.len() - 1).max(at);
    carry.drain(..keep.min(carry.len()));
    count
}

/// What to send to the child, and when.
pub enum Step {
    /// Write these bytes to the PTY.
    Send(&'static [u8]),
    /// Wait, still reading and metering.
    Wait(Duration),
}

/// Run one workload and measure it.
///
/// `settle` is drained and metered like everything else -- an application's
/// start-up redraw is part of what the transport has to carry.
pub fn measure(
    name: &str,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    size: Size,
    script: &[Step],
    tail: Duration,
) -> std::io::Result<Measurement> {
    let replies = Replies::default();
    let config = Config {
        scrolling_history: 10_000,
        ..Config::default()
    };
    let mut term = Term::new(config, &size, replies.clone());
    let mut parser: Processor = Processor::new();
    let mut projector = Projector::new();
    let mut pty = Pty::spawn(program, args, env, size.cols as u16, size.rows as u16)?;

    let mut measurement = Measurement {
        name: name.to_string(),
        cols: size.cols as u16,
        rows: size.rows as u16,
        ..Default::default()
    };
    let mut buf = vec![0u8; 65536];
    let mut carry = Vec::new();
    let start = Instant::now();

    // HARD RESOURCE BUDGET. This harness forks real interactive programs -- bash,
    // nvim, and a nested Claude Code TUI -- and drains their PTY as fast as the
    // kernel will hand bytes over. On 2026-09-12 a build of this file reached
    // 23.5 GB RSS (99 GB virtual) and was OOM-killed, taking the machine's memory
    // with it. The committed version does not reproduce that, but "does not
    // reproduce" is not a bound: a child that never stops talking, a script step
    // that never completes, or an edit to this file that loses a loop condition
    // all put us back there, and the failure mode is a dead workstation rather
    // than a red test. So the budget is enforced here, once, for every workload.
    //
    // These ceilings are ~50x the largest values any real workload has produced
    // (worst measured: 128 KB of PTY bytes, 401 frames, 12.5 s). Exceeding one
    // means something is wrong, not that a workload grew.
    const MAX_PTY_BYTES: u64 = 256 * 1024 * 1024;
    const MAX_FRAMES: u64 = 500_000;
    const MAX_WALL: Duration = Duration::from_secs(180);
    macro_rules! check_budget {
        ($m:expr, $start:expr) => {
            if $m.pty_bytes > MAX_PTY_BYTES || $m.frames > MAX_FRAMES || $start.elapsed() > MAX_WALL {
                panic!(
                    "bandwidth harness exceeded its resource budget and was stopped before it \
                     could exhaust memory: {} pty_bytes (max {}), {} frames (max {}), {:.1}s \
                     elapsed (max {}s). This is the guard added after the 2026-09-12 OOM; \
                     a workload hitting it is a bug in the harness or an unterminated child, \
                     never a reason to raise the ceiling without finding out why.",
                    $m.pty_bytes,
                    MAX_PTY_BYTES,
                    $m.frames,
                    MAX_FRAMES,
                    $start.elapsed().as_secs_f64(),
                    MAX_WALL.as_secs(),
                );
            }
        };
    }

    let mut pump = |pty: &mut Pty,
                    term: &mut Term<Replies>,
                    parser: &mut Processor,
                    projector: &mut Projector,
                    measurement: &mut Measurement,
                    carry: &mut Vec<u8>,
                    until: Instant| {
        while Instant::now() < until {
            check_budget!(measurement, start);
            match pty.read(&mut buf) {
                Some(n) => {
                    measurement.pty_reads += 1;
                    measurement.pty_bytes += n as u64;
                    measurement.sync_updates += count_esu(&buf[..n], carry);
                    parser.advance(term, &buf[..n]);

                    // ONE wakeup, ONE frame, ONE damage()/reset_damage() pair.
                    let frame = projector.next(term);
                    let naive = encode_naive(&frame).len() as u64;
                    let rle = encode_rle(&frame).len() as u64;
                    measurement.frames += 1;
                    measurement.cells += frame.cell_count() as u64;
                    measurement.naive_bytes += naive;
                    measurement.rle_bytes += rle;
                    measurement.largest_rle_frame = measurement.largest_rle_frame.max(rle);
                    match frame.kind {
                        FrameKind::Full => {
                            measurement.full_frames += 1;
                            measurement.naive_full_bytes += naive;
                            measurement.rle_full_bytes += rle;
                        }
                        FrameKind::Delta => {
                            measurement.delta_frames += 1;
                            measurement.naive_delta_bytes += naive;
                            measurement.rle_delta_bytes += rle;
                        }
                    }

                    let answer = replies.take();
                    if !answer.is_empty() {
                        pty.write(&answer);
                    }
                }
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    };

    for step in script {
        match step {
            Step::Send(bytes) => pty.write(bytes),
            Step::Wait(duration) => {
                let until = Instant::now() + *duration;
                pump(
                    &mut pty,
                    &mut term,
                    &mut parser,
                    &mut projector,
                    &mut measurement,
                    &mut carry,
                    until,
                );
            }
        }
    }
    let until = Instant::now() + tail;
    pump(
        &mut pty,
        &mut term,
        &mut parser,
        &mut projector,
        &mut measurement,
        &mut carry,
        until,
    );

    measurement.seconds = start.elapsed().as_secs_f64();
    let stats: FrameStats = projector.stats();
    assert_eq!(
        stats.frames(),
        measurement.frames,
        "the meter and the projector disagree"
    );
    pty.kill();
    Ok(measurement)
}

pub fn base_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("TERM", "xterm-256color"),
        ("PATH", "/usr/bin:/bin:/usr/local/bin"),
        ("HOME", "/tmp"),
        ("LANG", "C.UTF-8"),
        ("COLORTERM", "truecolor"),
    ]
}

pub fn have(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}
