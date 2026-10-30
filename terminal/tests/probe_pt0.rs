//! A headless spike on whether a terminal can change process with nothing visible lost. Three claims, each a
//! test, all `#[ignore]`d (run with `-- --ignored --nocapture`):
//!
//! - A: the alternate screen can be read and left without disturbing the primary screen, its history or its
//!   saved cursor;
//! - B: a fresh `Term` fed a naive text-and-SGR replay of a screen ends up cell for cell equal to it;
//! - C: a PTY master handed to another process over `SCM_RIGHTS` mid-flood loses and doubles no byte, and the
//!   shell on it is never hung up.
//!
//! Spike code, never product code: Claim C leaks the parent's `PtyChild` on purpose (its `Drop` would hang the
//! shell up) and reaps the shell by pid instead. Linux only: the hand-over uses `SOCK_CLOEXEC` and
//! `MSG_CMSG_CLOEXEC`, which macOS does not have, so elsewhere the file compiles to nothing.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Cursor, Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use eitri_terminal::screen::term_config;
use eitri_terminal::{PtyChild, PtySize, SpawnSpec};

const COLS: usize = 80;
const ROWS: usize = 24;
const NUMBERED_LINES: usize = 2_000;

/// A whole Claim C run must finish well inside this, or something is stuck (a lost hangup, a reader that never
/// sees EOF).
const DEADLINE: Duration = Duration::from_secs(120);

const FLOOD_LINES: u32 = 200_000;
const STOP_AFTER_LINE: u32 = 50_000;

const HELPER_ENV: &str = "PT0_HELPER";
const HELPER_OUT_ENV: &str = "PT0_OUT";
/// The descriptor number the helper finds its end of the socketpair on.
const HELPER_SOCK_FD: RawFd = 3;
const HELPER_TEST: &str = "helper_reads_a_received_pty_master_to_its_end";

fn new_term() -> Term<VoidListener> {
    Term::new(term_config(), &TermSize::new(COLS, ROWS), VoidListener)
}

fn feed(term: &mut Term<VoidListener>, bytes: &[u8]) {
    let mut parser: Processor = Processor::new();
    parser.advance(term, bytes);
}

/// Every row of `grid`, the oldest history line first.
fn rows(grid: &Grid<Cell>) -> Vec<Vec<Cell>> {
    (grid.topmost_line().0..=grid.bottommost_line().0)
        .map(|line| {
            let row = &grid[Line(line)];
            (0..grid.columns()).map(|col| row[Column(col)].clone()).collect()
        })
        .collect()
}

fn row_text(row: &[Cell]) -> String {
    row.iter()
        .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
        .map(|cell| cell.c)
        .collect::<String>()
}

/// The logical lines of a screen: rows joined where the earlier one wrapped, trailing blanks trimmed.
fn logical_lines(rows: &[Vec<Cell>]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for row in rows {
        current.push_str(&row_text(row));
        let wrapped = row.last().is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
        if !wrapped {
            lines.push(current.trim_end().to_string());
            current.clear();
        }
    }
    if !current.is_empty() {
        lines.push(current.trim_end().to_string());
    }
    lines
}

/// The primary screen's content: numbered lines in several colour forms (named, bright, 256-colour, true
/// colour, background), the text attributes, wide characters, lines that wrap and an erase that fills with a
/// background colour. Every line ends with a reset so the next line's scroll fills with the default colours.
fn numbered_lines() -> Vec<u8> {
    let mut out = String::new();
    for i in 0..NUMBERED_LINES {
        let n = format!("{i:05}");
        let line = match i % 9 {
            0 => format!("\x1b[3{}m{n}\x1b[0m plain text after a coloured number", 1 + i % 7),
            1 => format!("\x1b[9{}m{n}\x1b[0m \x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m", i % 8),
            2 => format!(
                "\x1b[38;5;{}m{n}\x1b[0m \x1b[4munderlined\x1b[0m \x1b[7minverse\x1b[0m",
                i % 256
            ),
            3 => format!(
                "\x1b[38;2;{};{};{}m{n}\x1b[0m \x1b[48;2;10;20;30mtrue-colour background\x1b[0m",
                i % 256,
                (i * 7) % 256,
                (i * 13) % 256
            ),
            4 => format!(
                "\x1b[4{}m{n}\x1b[0m \x1b[2mdim\x1b[0m \x1b[9mstruck\x1b[0m \x1b[8mhidden\x1b[0m",
                i % 8
            ),
            5 => format!("{n} 中文 wide characters \x1b[32m漢字\x1b[0m end"),
            6 => format!("\x1b[1;3;4;31;44m{n} every attribute at once\x1b[0m \x1b[4:3mcurly\x1b[0m"),
            7 => format!("{n} \x1b[44mblue to the edge\x1b[K"),
            _ => {
                let long: String = (0..130).map(|k| char::from(b'a' + (k % 26) as u8)).collect();
                format!("\x1b[35m{n}\x1b[0m {long}")
            }
        };
        out.push_str(&line);
        out.push_str("\x1b[0m\r\n");
    }
    // A prompt that leaves bold green on, so the cursor carries a non-default pen into the alternate screen.
    out.push_str("\x1b[1;32mprompt$ ");
    out.into_bytes()
}

/// An application's full-screen drawing: a box of `-` and `|`, coloured text inside it, the cursor left at
/// row 10, column 10.
fn alternate_drawing() -> Vec<u8> {
    let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");
    let width = 60;
    out.push_str(&format!("\x1b[1;1H{}", "-".repeat(width)));
    for row in 2..20 {
        out.push_str(&format!("\x1b[{row};1H|\x1b[{row};{width}H|"));
    }
    out.push_str(&format!("\x1b[20;1H{}", "-".repeat(width)));
    out.push_str("\x1b[3;3H\x1b[1;33mtitle in bold yellow\x1b[0m");
    out.push_str("\x1b[5;3H\x1b[38;5;202;48;5;17mindexed colours\x1b[0m");
    out.push_str("\x1b[7;3H\x1b[3;38;2;200;100;50mtrue colour italic\x1b[0m");
    out.push_str("\x1b[10;10H");
    out.into_bytes()
}

/// A `Term` back on its primary screen after the alternate screen was drawn and read.
struct AfterAlternate {
    term: Term<VoidListener>,
    primary_before: Vec<Vec<Cell>>,
    cursor_before: Cursor<Cell>,
}

/// Claim A's sequence: primary content, `1049h`, a drawing, a read of the alternate grid, one `swap_alt` from
/// the alternate side. Asserts what the alternate grid held while it was active.
fn draw_and_leave_the_alternate_screen() -> AfterAlternate {
    let mut term = new_term();
    feed(&mut term, &numbered_lines());
    assert!(!term.mode().contains(TermMode::ALT_SCREEN));
    let primary_before = rows(term.grid());
    let cursor_before = term.grid().cursor.clone();

    feed(&mut term, b"\x1b[?1049h");
    feed(&mut term, &alternate_drawing());
    assert!(
        term.mode().contains(TermMode::ALT_SCREEN),
        "1049h did not enter the alternate screen"
    );

    let alt = term.grid();
    assert_eq!(alt.history_size(), 0, "the alternate screen keeps no history");
    assert_eq!(
        alt.cursor.point,
        Point::new(Line(9), Column(9)),
        "the drawing leaves the cursor at row 10, column 10"
    );
    let alt_rows = rows(alt);
    assert_eq!(row_text(&alt_rows[0]).trim_end(), "-".repeat(60));
    assert_eq!(row_text(&alt_rows[19]).trim_end(), "-".repeat(60));
    for row in &alt_rows[1..19] {
        assert_eq!(row[0].c, '|');
        assert_eq!(row[59].c, '|');
    }
    let title = &alt_rows[2][2];
    assert_eq!(title.c, 't');
    assert_eq!(title.fg, Color::Named(NamedColor::Yellow));
    assert!(title.flags.contains(Flags::BOLD));
    let indexed = &alt_rows[4][2];
    assert_eq!((indexed.fg, indexed.bg), (Color::Indexed(202), Color::Indexed(17)));
    assert!(alt_rows[6][2].flags.contains(Flags::ITALIC));
    assert!(
        alt_rows.iter().all(|row| !row_text(row).contains("prompt$")),
        "the alternate screen starts cleared"
    );

    // Read from the alternate side: the branch of `swap_alt` that resets the other grid and overwrites the
    // primary's saved cursor runs only when leaving the primary, so this must leave the primary as it was.
    term.swap_alt();
    assert!(!term.mode().contains(TermMode::ALT_SCREEN));

    AfterAlternate {
        term,
        primary_before,
        cursor_before,
    }
}

#[test]
#[ignore = "probe: run with --ignored"]
fn the_alternate_screen_is_read_and_left_without_touching_the_primary() {
    let AfterAlternate {
        term,
        primary_before,
        cursor_before,
    } = draw_and_leave_the_alternate_screen();
    let primary_after = rows(term.grid());

    assert_eq!(
        primary_after.len(),
        primary_before.len(),
        "the primary screen's history changed length"
    );
    let differing = primary_before
        .iter()
        .zip(&primary_after)
        .enumerate()
        .filter(|(_, (before, after))| before != after)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    assert!(
        differing.is_empty(),
        "primary rows changed: {:?}",
        &differing[..differing.len().min(10)]
    );

    let lines = logical_lines(&primary_after);
    for (i, line) in lines.iter().take(NUMBERED_LINES).enumerate() {
        assert!(
            line.starts_with(&format!("{i:05}")),
            "logical line {i} should carry its own number, holds {line:?}"
        );
    }
    assert_eq!(
        lines.len(),
        NUMBERED_LINES + 1,
        "the numbered lines and the prompt, nothing else"
    );
    assert!(lines[NUMBERED_LINES].starts_with("prompt$"));

    let grid = term.grid();
    assert_eq!(
        grid.saved_cursor, cursor_before,
        "the saved cursor must be the cursor as it was when 1049h arrived"
    );
    assert_eq!(
        grid.cursor, cursor_before,
        "the primary's live cursor must be untouched"
    );
    println!(
        "PT-0 claim A: PASS -- {} primary rows ({} history) unchanged, saved cursor {:?} bold={} fg={:?}",
        primary_after.len(),
        grid.history_size(),
        grid.saved_cursor.point,
        grid.saved_cursor.template.flags.contains(Flags::BOLD),
        grid.saved_cursor.template.fg
    );
}

/// The SGR parameter for a colour, foreground or background.
fn sgr_colour(colour: Color, foreground: bool) -> String {
    let base = if foreground { 30 } else { 40 };
    match colour {
        Color::Named(NamedColor::Foreground) if foreground => "39".into(),
        Color::Named(NamedColor::Background) if !foreground => "49".into(),
        Color::Named(named) if (named as usize) < 8 => (base + named as usize).to_string(),
        Color::Named(named) if (named as usize) < 16 => (base + 60 + named as usize - 8).to_string(),
        Color::Named(named) => panic!("no SGR writes the named colour {named:?} into a cell"),
        Color::Indexed(index) => format!("{};5;{index}", base + 8),
        Color::Spec(rgb) => format!("{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b),
    }
}

/// The flags a cell's position gives it rather than its pen.
fn layout_flags() -> Flags {
    Flags::WRAPLINE | Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER
}

/// One SGR sequence that sets exactly this pen from any previous one.
fn sgr(fg: Color, bg: Color, flags: Flags) -> String {
    let mut params = vec!["0".to_string()];
    let pen = flags - layout_flags();
    let known = [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::UNDERLINE, "4"),
        (Flags::UNDERCURL, "4:3"),
        (Flags::DOTTED_UNDERLINE, "4:4"),
        (Flags::DASHED_UNDERLINE, "4:5"),
        (Flags::DOUBLE_UNDERLINE, "21"),
        (Flags::INVERSE, "7"),
        (Flags::HIDDEN, "8"),
        (Flags::STRIKEOUT, "9"),
    ];
    let mut covered = Flags::empty();
    for (flag, param) in known {
        if pen.contains(flag) {
            params.push(param.into());
            covered |= flag;
        }
    }
    assert!(
        (pen - covered).is_empty(),
        "a flag with no SGR in this replay: {:?}",
        pen - covered
    );
    params.push(sgr_colour(fg, true));
    params.push(sgr_colour(bg, false));
    format!("\x1b[{}m", params.join(";"))
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && (cell.flags - layout_flags()).is_empty()
}

/// The naive replay: each row's cells as text, an SGR before every change of pen, CR LF between rows except
/// after a row that wrapped (printing on wraps it again), the pen reset before each CR LF so a scroll fills
/// with the default colours. Trailing blank cells of a row that did not wrap are left out.
fn naive_replay(rows: &[Vec<Cell>]) -> Vec<u8> {
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        let wrapped = row.last().is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
        let end = if wrapped {
            row.len()
        } else {
            row.iter().rposition(|cell| !is_blank(cell)).map_or(0, |last| last + 1)
        };
        let mut pen: Option<(Color, Color, Flags)> = None;
        for cell in &row[..end] {
            assert!(
                !cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER),
                "this replay does not place a wide character across a row's end"
            );
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let this = (cell.fg, cell.bg, cell.flags - layout_flags());
            if pen != Some(this) {
                out.push_str(&sgr(this.0, this.1, this.2));
                pen = Some(this);
            }
            out.push(cell.c);
        }
        if index + 1 < rows.len() && !wrapped {
            out.push_str("\x1b[0m\r\n");
        }
    }
    out.into_bytes()
}

#[test]
#[ignore = "probe: run with --ignored"]
fn a_fresh_term_fed_a_naive_replay_equals_the_source_screen() {
    let AfterAlternate { term: source, .. } = draw_and_leave_the_alternate_screen();
    let source_rows = rows(source.grid());

    let mut copy = new_term();
    let replay = naive_replay(&source_rows);
    feed(&mut copy, &replay);
    let copy_rows = rows(copy.grid());

    assert_eq!(
        copy.grid().history_size(),
        source.grid().history_size(),
        "the replay must leave as much history as the source"
    );
    assert_eq!(copy_rows.len(), source_rows.len());
    let mut mismatches = Vec::new();
    for (r, (a, b)) in source_rows.iter().zip(&copy_rows).enumerate() {
        for (col, (x, y)) in a.iter().zip(b).enumerate() {
            if (x.c, x.fg, x.bg, x.flags) != (y.c, y.fg, y.bg, y.flags) {
                mismatches.push(format!(
                    "row {r} col {col}: source {:?} {:?} {:?} {:?}, replay {:?} {:?} {:?} {:?}",
                    x.c, x.fg, x.bg, x.flags, y.c, y.fg, y.bg, y.flags
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} cells differ, first: {:#?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(10)]
    );
    println!(
        "PT-0 claim B: PASS -- {} rows ({} history) x {COLS} cells equal in c, fg, bg and flags; replay {} bytes",
        copy_rows.len(),
        copy.grid().history_size(),
        replay.len()
    );
}

fn out_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("probe_pt0");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sends `fd` with one byte of payload over a connected seqpacket socket.
fn send_fd(sock: RawFd, fd: RawFd) {
    let mut byte = [b'm'];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `control` is CMSG_SPACE bytes for one descriptor, so the first header and its data fit.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            (&fd as *const RawFd).cast::<u8>(),
            libc::CMSG_DATA(header),
            std::mem::size_of::<RawFd>(),
        );
    }
    let sent = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
    assert_eq!(sent, 1, "sendmsg: {}", std::io::Error::last_os_error());
}

/// Receives one descriptor, close-on-exec from the moment it arrives.
fn recv_fd(sock: RawFd) -> RawFd {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let got = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    assert_eq!(got, 1, "recvmsg: {}", std::io::Error::last_os_error());
    assert_eq!(msg.msg_flags & libc::MSG_CTRUNC, 0, "the descriptor was cut off");
    // SAFETY: the kernel filled `control` and `msg_controllen`; the header is read only if present.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        assert!(!header.is_null(), "no descriptor arrived");
        assert_eq!(
            ((*header).cmsg_level, (*header).cmsg_type),
            (libc::SOL_SOCKET, libc::SCM_RIGHTS)
        );
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(header),
            (&mut fd as *mut RawFd).cast::<u8>(),
            std::mem::size_of::<RawFd>(),
        );
        fd
    }
}

/// Reads a non-blocking descriptor until the PTY's other side is closed (`EIO`) or end of file. The
/// descriptor's open file description is non-blocking for every holder, so this polls.
fn read_until_closed(fd: RawFd, out: &mut Vec<u8>) {
    let deadline = Instant::now() + DEADLINE;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        assert!(Instant::now() < deadline, "no end of output after {} bytes", out.len());
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            out.extend_from_slice(&buf[..n as usize]);
            continue;
        }
        if n == 0 {
            return;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) => {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe { libc::poll(&mut pfd, 1, 100) };
            }
            Some(libc::EINTR) => {}
            Some(libc::EIO) => return,
            _ => panic!("read: {err}"),
        }
    }
}

/// The helper process's side of Claim C: receive the master, read it to its end, write what it read to a
/// file. Without its environment variable (any ordinary `--ignored` run) it does nothing.
#[test]
#[ignore = "probe: run with --ignored"]
fn helper_reads_a_received_pty_master_to_its_end() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let out_path = PathBuf::from(std::env::var_os(HELPER_OUT_ENV).expect("the helper's output path"));
    let master = recv_fd(HELPER_SOCK_FD);
    let flags = unsafe { libc::fcntl(master, libc::F_GETFD) };
    assert!(
        flags & libc::FD_CLOEXEC != 0,
        "a received descriptor must be close-on-exec"
    );
    let mut got = Vec::new();
    read_until_closed(master, &mut got);
    unsafe { libc::close(master) };
    fs::write(&out_path, &got).unwrap();
}

/// Every number from 0 below `FLOOD_LINES`, exactly once and in order, in the tty's `\r\n` lines.
fn check_numbers(bytes: &[u8]) {
    let text = String::from_utf8(bytes.to_vec()).expect("the shell wrote only digits and line ends");
    let mut expected = 0u32;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let n: u32 = line
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {line:?} (expected {expected})"));
        assert_eq!(n, expected, "a number was lost, doubled or reordered");
        expected += 1;
    }
    assert_eq!(expected, FLOOD_LINES, "the output stops early");
}

#[test]
#[ignore = "probe: run with --ignored"]
fn a_pty_master_handed_to_another_process_mid_flood_loses_and_doubles_nothing() {
    let script = format!("i=0; while [ $i -lt {FLOOD_LINES} ]; do echo $i; i=$((i+1)); done; sleep 2");
    let spec = SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from(script)],
        cwd: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        env: Vec::new(),
    };
    let size = PtySize {
        cols: 80,
        rows: 24,
        cell_width_px: 9,
        cell_height_px: 18,
    };
    let mut child = PtyChild::spawn(&spec, size).unwrap();
    let shell_pid = child.pid() as libc::pid_t;
    let master = child.master_fd().as_raw_fd();

    // The parent's share: read until the line for STOP_AFTER_LINE is complete, then stop mid-stream.
    let marker = format!("\n{STOP_AFTER_LINE}\r\n").into_bytes();
    let deadline = Instant::now() + DEADLINE;
    let mut parent_bytes = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        assert!(Instant::now() < deadline, "line {STOP_AFTER_LINE} never arrived");
        match child.read(&mut buf) {
            Ok(0) => panic!("the shell's output ended before line {STOP_AFTER_LINE}"),
            Ok(n) => {
                parent_bytes.extend_from_slice(&buf[..n]);
                let tail_from = parent_bytes.len().saturating_sub(n + marker.len());
                if parent_bytes[tail_from..].windows(marker.len()).any(|w| w == marker) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let mut pfd = libc::pollfd {
                    fd: master,
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe { libc::poll(&mut pfd, 1, 100) };
            }
            Err(e) => panic!("read: {e}"),
        }
    }
    // Its `Drop` would hang the shell up; the shell is reaped by pid below instead.
    std::mem::forget(child);

    let mut socks = [-1 as RawFd; 2];
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            socks.as_mut_ptr(),
        )
    };
    assert_eq!(rc, 0, "socketpair: {}", std::io::Error::last_os_error());
    let [mine, theirs] = socks;

    let out_path = out_dir().join(format!("helper-{}.bin", std::process::id()));
    let _ = fs::remove_file(&out_path);
    let mut helper = Command::new(std::env::current_exe().unwrap());
    helper
        .args([
            HELPER_TEST,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(HELPER_ENV, "1")
        .env(HELPER_OUT_ENV, &out_path);
    // SAFETY: runs in the forked child before exec and calls only async-signal-safe functions.
    unsafe {
        helper.pre_exec(move || {
            if theirs == HELPER_SOCK_FD {
                // dup2 onto itself would keep close-on-exec; clear it instead.
                let flags = libc::fcntl(theirs, libc::F_GETFD);
                if flags == -1 || libc::fcntl(theirs, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(theirs, HELPER_SOCK_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut helper = helper.spawn().unwrap();
    unsafe { libc::close(theirs) };

    send_fd(mine, master);
    // The in-flight descriptor keeps the master open: this process gives up its own copy at once.
    unsafe {
        libc::close(master);
        libc::close(mine);
    }

    let deadline = Instant::now() + DEADLINE;
    let helper_status = loop {
        if let Some(status) = helper.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = helper.kill();
            let _ = helper.wait();
            panic!("the helper did not finish");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(helper_status.success(), "the helper failed: {helper_status}");

    let mut status = 0;
    let reaped = unsafe { libc::waitpid(shell_pid, &mut status, 0) };
    assert_eq!(reaped, shell_pid, "waitpid: {}", std::io::Error::last_os_error());
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the shell must finish its loop and its sleep, not die of a hangup (status {status:#x})"
    );

    let helper_bytes = fs::read(&out_path).unwrap();
    let _ = fs::remove_file(&out_path);
    let mut all = parent_bytes.clone();
    all.extend_from_slice(&helper_bytes);
    check_numbers(&all);
    println!(
        "PT-0 claim C: PASS -- parent read {} bytes, helper {} bytes; 0..{} each once in order; shell exited 0",
        parent_bytes.len(),
        helper_bytes.len(),
        FLOOD_LINES - 1
    );
}
