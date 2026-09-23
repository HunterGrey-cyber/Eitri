//! A minimal real PTY + real terminal harness for the application-level proofs.
//!
//! Two halves:
//!
//!   * [`Pty`] -- a PTY pair from `posix_openpt` and a child process attached to
//!     the slave side as its controlling terminal. NOT `alacritty_terminal::tty`,
//!     which inherits the parent environment wholesale; this one calls
//!     `env_clear()` and hands the child a short explicit environment, so a test
//!     cannot pass or fail because of the developer's shell.
//!
//!   * [`Screen`] -- an authoritative `alacritty_terminal::Term` driven by
//!     `vte::ansi::Processor`, i.e. the same terminal the engine will own. It is
//!     a REAL terminal, not a transcript scraper: replies the terminal generates
//!     (primary DA, DSR, the kitty keyboard query, colour and text-area requests)
//!     are captured through an `EventListener` and written back to the PTY, which
//!     is what makes nvim actually negotiate `REPORT_EVENT_TYPES` instead of
//!     falling back to legacy keys.
//!
//! This harness is for tests only. It is not the engine's PTY loop.

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
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;

// ---------------------------------------------------------------------------
// PTY
// ---------------------------------------------------------------------------

pub struct Pty {
    master: OwnedFd,
    child: Child,
}

fn cstr_from_ptsname(master: RawFd) -> std::io::Result<CString> {
    let mut buf = [0i8; 128];
    // ptsname_r is glibc; it avoids ptsname's static buffer.
    let rc = unsafe { libc::ptsname_r(master, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let bytes: Vec<u8> = buf.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
    Ok(CString::new(bytes).unwrap())
}

impl Pty {
    /// Spawn `program` on a fresh PTY of `cols` x `rows`.
    ///
    /// `env` fully replaces the child's environment. Nothing from this process
    /// leaks through.
    pub fn spawn(program: &str, args: &[&str], env: &[(&str, &str)], cols: u16, rows: u16) -> std::io::Result<Pty> {
        let master_raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master_raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(master_raw) };

        if unsafe { libc::grantpt(master_raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::unlockpt(master_raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: cols * 8,
            ws_ypixel: rows * 16,
        };
        if unsafe { libc::ioctl(master_raw, libc::TIOCSWINSZ, &size) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let slave_name = cstr_from_ptsname(master_raw)?;
        let slave_raw = unsafe { libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if slave_raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };

        let stdin = slave.try_clone()?;
        let stdout = slave.try_clone()?;
        let stderr = slave.try_clone()?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        for (k, v) in env {
            cmd.env(k, v);
        }

        let slave_for_child = slave_raw;
        unsafe {
            cmd.pre_exec(move || {
                // New session, then claim the slave as the controlling terminal.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_for_child, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        drop(slave);

        // Non-blocking master: reads must never wedge a test.
        let flags = unsafe { libc::fcntl(master_raw, libc::F_GETFL) };
        unsafe { libc::fcntl(master_raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };

        Ok(Pty { master, child })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let mut f = unsafe { std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(self.master.as_raw_fd())) };
        f.write_all(bytes).expect("write to pty");
        f.flush().ok();
    }

    /// Drain the master until `quiet` has elapsed with no new bytes, giving up
    /// after `deadline`.
    pub fn read_quiescent(&mut self, quiet: Duration, deadline: Duration) -> Vec<u8> {
        let mut out = Vec::new();
        let start = Instant::now();
        let mut last = Instant::now();
        let mut buf = [0u8; 8192];
        let mut f = unsafe { std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(self.master.as_raw_fd())) };
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    last = Instant::now();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if last.elapsed() >= quiet {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
            if start.elapsed() >= deadline {
                break;
            }
        }
        out
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
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Captures everything the terminal wants to send back to the application.
#[derive(Clone, Default)]
pub struct Replies(Arc<Mutex<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        let mut buf = self.0.lock().unwrap();
        match event {
            Event::PtyWrite(s) => buf.extend_from_slice(s.as_bytes()),
            Event::ColorRequest(index, fmt) => {
                // Any plausible colour will do; the point is to answer at all,
                // because an unanswered query makes some applications stall.
                let rgb = alacritty_terminal::vte::ansi::Rgb {
                    r: 0x18,
                    g: 0x18,
                    b: 0x18,
                };
                let _ = index;
                buf.extend_from_slice(fmt(rgb).as_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                let size = WindowSize {
                    num_lines: 24,
                    num_cols: 80,
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
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

pub struct Screen {
    pub term: Term<Replies>,
    parser: Processor,
    pub replies: Replies,
    size: Size,
}

impl Screen {
    pub fn new(size: Size) -> Screen {
        Screen::with_kitty(size, true)
    }

    /// `kitty_keyboard = false` makes the terminal refuse the kitty keyboard
    /// query, exactly as a terminal without the protocol would. Applications then
    /// fall back to legacy keys -- which is the only regime in which the D2 stray
    /// ESC is observable, because under kitty every key has an encoding and
    /// `build_sequence` never returns empty.
    pub fn with_kitty(size: Size, kitty_keyboard: bool) -> Screen {
        let replies = Replies::default();
        let config = Config {
            kitty_keyboard,
            ..Config::default()
        };
        let term = Term::new(config, &size, replies.clone());
        Screen {
            term,
            parser: Processor::new(),
            replies,
            size,
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn line(&self, row: usize) -> String {
        let mut s = String::new();
        for col in 0..self.size.cols {
            let point = Point::new(Line(row as i32), Column(col));
            s.push(self.term.grid()[point].c);
        }
        s.trim_end().to_owned()
    }

    pub fn screen_text(&self) -> String {
        (0..self.size.rows).map(|r| self.line(r)).collect::<Vec<_>>().join("\n")
    }

    /// Non-empty lines, trimmed; the useful view for assertions.
    pub fn visible_lines(&self) -> Vec<String> {
        (0..self.size.rows)
            .map(|r| self.line(r))
            .filter(|l| !l.is_empty())
            .collect()
    }

    pub fn cursor(&self) -> (usize, usize) {
        let p = self.term.grid().cursor.point;
        (p.line.0.max(0) as usize, p.column.0)
    }
}

// ---------------------------------------------------------------------------
// Driver: PTY + terminal wired together, with the reply path closed.
// ---------------------------------------------------------------------------

pub struct App {
    pub pty: Pty,
    pub screen: Screen,
}

impl App {
    pub fn launch(program: &str, args: &[&str], env: &[(&str, &str)], size: Size) -> std::io::Result<App> {
        App::launch_with_kitty(program, args, env, size, true)
    }

    pub fn launch_with_kitty(
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        size: Size,
        kitty_keyboard: bool,
    ) -> std::io::Result<App> {
        let pty = Pty::spawn(program, args, env, size.cols as u16, size.rows as u16)?;
        Ok(App {
            pty,
            screen: Screen::with_kitty(size, kitty_keyboard),
        })
    }

    /// Read whatever the application produced, feed it to the terminal, and send
    /// the terminal's own replies back. Repeats until nothing new arrives, so
    /// multi-round negotiations (query -> reply -> mode set) complete.
    pub fn settle(&mut self, quiet: Duration, deadline: Duration) {
        let start = Instant::now();
        loop {
            let bytes = self.pty.read_quiescent(quiet, deadline);
            let replies = {
                self.screen.advance(&bytes);
                self.screen.replies.take()
            };
            if !replies.is_empty() {
                self.pty.write(&replies);
            }
            if bytes.is_empty() && replies.is_empty() {
                break;
            }
            if start.elapsed() >= deadline {
                break;
            }
        }
    }

    /// Encode one input against the terminal's CURRENT mode and send it.
    ///
    /// This is the whole point of the harness: the mode is read back from the
    /// authoritative `Term`, so the bytes under test are the bytes the engine
    /// would really produce against this application in this state.
    pub fn send(&mut self, input: &terminal_input::NormalizedInput) -> Vec<u8> {
        let mode = self.screen.mode();
        let bytes = terminal_input::encode(input, mode);
        if !bytes.is_empty() {
            self.pty.write(&bytes);
        }
        bytes
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.pty.write(bytes);
    }
}
