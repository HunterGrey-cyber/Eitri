//! Whether a terminal's shell is running a job, for the questions a window close and the last
//! module's kill ask. A terminal has a job when the foreground process group of its shell's
//! controlling terminal is set and is not the shell's own group: the shell has handed the terminal
//! to a command (`sleep 600`, `vim`, a build). A job started with `&` at a prompt leaves the
//! terminal with the shell, so it does not count, as in foot and GNOME Terminal; brought back with
//! `fg`, it does.
//!
//! The fact is read from `/proc/<shell pid>/stat` (its `tpgid` and `pgrp` fields), which gives what
//! `tcgetpgrp` on the PTY's master would without the master's descriptor: that belongs to the
//! terminal's session thread. One small procfs read per terminal, made only when a close or a kill
//! asks, never on a timer, so it is cheap enough for the GTK thread.
//!
//! A restart in a new directory instead scans the whole session on a worker: ending the shell
//! can end its background jobs too. An error leaves that terminal busy.

use super::TerminalPane;
use std::io;

/// Whether any other process belongs to this shell's session, including background and stopped jobs.
/// Reads procfs only on a worker. An unreadable session is an error, which callers must treat as busy.
pub(crate) fn session_has_other_process(shell_pid: u32) -> io::Result<bool> {
    session_has_other_process_with(
        shell_pid,
        || {
            std::fs::read_dir("/proc")?
                .filter_map(|entry| match entry {
                    Ok(entry) => entry.file_name().to_str()?.parse::<u32>().ok().map(Ok),
                    Err(error) => Some(Err(error)),
                })
                .collect()
        },
        |pid| std::fs::read_to_string(format!("/proc/{pid}/stat")),
    )
}

fn session_from_stat(stat: &str) -> io::Result<u32> {
    // Kernel processes may have session 0; it is valid but cannot match a terminal shell's session.
    stat.rfind(')')
        .and_then(|close| stat[close + 1..].split_ascii_whitespace().nth(3))
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process session in stat"))
}

fn session_has_other_process_with(
    shell_pid: u32,
    list: impl FnOnce() -> io::Result<Vec<u32>>,
    mut read: impl FnMut(u32) -> io::Result<String>,
) -> io::Result<bool> {
    let pids = list()?;
    let session = session_from_stat(&read(shell_pid)?)?;
    for pid in pids {
        if pid == shell_pid {
            continue;
        }
        let stat = match read(pid) {
            Ok(stat) => stat,
            // The process may exit after its directory was listed.
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => continue,
            Err(error) => return Err(error),
        };
        if session_from_stat(&stat)? == session {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reads one `/proc/<pid>/stat` line: `Some(true)` when the controlling terminal's foreground group
/// is set and differs from the process's own group, `Some(false)` when it is the process's own group
/// or there is none (`-1`: no controlling terminal), `None` for a line that does not parse.
///
/// The command name (field 2) is in parentheses and may itself hold spaces and parentheses, so the
/// numeric fields are counted from the last `)` in the line.
pub(crate) fn foreground_job_from_stat(stat: &str) -> Option<bool> {
    let (pgrp, tpgid) = stat_groups(stat)?;
    Some(tpgid > 0 && tpgid != pgrp)
}

/// The process's own group (field 5) and its controlling terminal's foreground group (field 8).
fn stat_groups(stat: &str) -> Option<(i64, i64)> {
    let close = stat.rfind(')')?;
    // Field 3 (the state) is the first word after the name.
    let mut fields = stat[close + 1..].split_ascii_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp: i64 = fields.next()?.parse().ok()?;
    let _session = fields.next()?;
    let _tty_nr = fields.next()?;
    let tpgid: i64 = fields.next()?.parse().ok()?;
    Some((pgrp, tpgid))
}

/// Whether the shell `shell_pid` has handed its terminal to a job. A shell whose stat cannot be read
/// (it has just exited) or does not parse has no job to lose.
pub(crate) fn has_foreground_job(shell_pid: u32) -> bool {
    std::fs::read(format!("/proc/{shell_pid}/stat"))
        .ok()
        .and_then(|bytes| foreground_job_from_stat(&String::from_utf8_lossy(&bytes)))
        .unwrap_or(false)
}

/// How many of the window's terminals run a job: every pane it holds, the base terminal, each live
/// instance and the idle pool alike, since a hidden terminal's job ends with the window too.
pub(crate) fn count_terminal_jobs(terminals: &[TerminalPane]) -> usize {
    terminals
        .iter()
        .filter_map(TerminalPane::shell_pid)
        .filter(|&pid| has_foreground_job(pid))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_terminal::pty::{PtyChild, PtySize, SpawnSpec};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn the_stat_line_is_read_past_a_name_holding_spaces_and_parentheses() {
        // pid (comm) state ppid pgrp session tty_nr tpgid flags ...
        let own_group = "4242 (my (odd) shell) S 1 4242 4242 34817 4242 4194304 120 0 0 0";
        assert_eq!(
            foreground_job_from_stat(own_group),
            Some(false),
            "the shell holds its terminal"
        );
        let handed_over = "4242 (my (odd) shell) S 1 4242 4242 34817 4300 4194304 120 0 0 0";
        assert_eq!(
            foreground_job_from_stat(handed_over),
            Some(true),
            "a job holds the terminal"
        );
        let no_tty = "4242 (a) S 1 4242 4242 0 -1 4194304 120 0 0 0";
        assert_eq!(foreground_job_from_stat(no_tty), Some(false), "no controlling terminal");
        let spaced = "4242 (two words) R 1 4242 4242 34817 4243 0";
        assert_eq!(foreground_job_from_stat(spaced), Some(true));
    }

    #[test]
    fn a_malformed_stat_line_is_not_read() {
        assert_eq!(foreground_job_from_stat(""), None);
        assert_eq!(
            foreground_job_from_stat("4242 no parentheses S 1 4242 4242 0 4242"),
            None
        );
        assert_eq!(foreground_job_from_stat("4242 (cut) S 1 4242"), None, "too few fields");
        assert_eq!(
            foreground_job_from_stat("4242 (x) S 1 abc 4242 0 4242"),
            None,
            "pgrp is not a number"
        );
        assert_eq!(
            foreground_job_from_stat("4242 (x) S 1 4242 4242 0 tp"),
            None,
            "tpgid is not a number"
        );
    }

    #[test]
    fn a_process_that_does_not_exist_has_no_job() {
        // Above the kernel's largest pid (`PID_MAX_LIMIT`, 2^22), so it never names a process.
        assert!(!has_foreground_job(u32::MAX));
    }

    /// A real `/bin/sh` on a PTY, as a terminal pane runs its shell, with its scratch working
    /// directory beside the test binary; every process it started is killed by pid when it drops.
    struct Shell {
        child: PtyChild,
        dir: PathBuf,
        output: Vec<u8>,
    }

    impl Shell {
        fn spawn(name: &str) -> Self {
            let exe = std::env::current_exe().expect("the test binary's path");
            let dir = exe
                .parent()
                .expect("the test binary's directory")
                .join(format!("terminal-jobs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch directory beside the test binary");
            let size = PtySize {
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
            };
            let child = PtyChild::spawn(&SpawnSpec::shell(PathBuf::from("/bin/sh"), dir.clone()), size)
                .expect("/bin/sh starts on a PTY");
            Shell {
                child,
                dir,
                output: Vec::new(),
            }
        }

        fn pid(&self) -> u32 {
            self.child.pid()
        }

        fn type_line(&mut self, line: &str) {
            let bytes = format!("{line}\n");
            let mut written = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while written < bytes.len() {
                match self.child.write(&bytes.as_bytes()[written..]) {
                    Ok(n) => written += n,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(err) => panic!("typing into the shell failed: {err}"),
                }
                assert!(Instant::now() < deadline, "the PTY took no input for 2 s");
                self.drain();
            }
        }

        /// Reads what the shell printed, so its writes never wait on a full PTY.
        fn drain(&mut self) {
            let mut buf = [0u8; 4096];
            while let Ok(n) = self.child.read(&mut buf) {
                if n == 0 {
                    break;
                }
                self.output.extend_from_slice(&buf[..n]);
            }
        }

        /// The shell's children, from procfs.
        fn children(&self) -> Vec<u32> {
            let pid = self.pid();
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap_or_default()
                .split_ascii_whitespace()
                .filter_map(|word| word.parse().ok())
                .collect()
        }

        /// Polls `has_foreground_job` for up to 2 s until it reads `want`.
        fn wait_for_job(&mut self, want: bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                self.drain();
                if has_foreground_job(self.pid()) == want {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn wait_for_a_child(&mut self) -> bool {
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.children().is_empty() {
                if Instant::now() >= deadline {
                    return false;
                }
                self.drain();
                std::thread::sleep(Duration::from_millis(20));
            }
            true
        }
    }

    impl Drop for Shell {
        fn drop(&mut self) {
            let mut doomed = self.children();
            // A foreground job's group leader, in case procfs lists no children on this kernel.
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", self.pid())) {
                if let Some((pgrp, tpgid)) = stat_groups(&stat) {
                    if tpgid > 0 && tpgid != pgrp {
                        doomed.push(tpgid as u32);
                    }
                }
            }
            for pid in doomed {
                // SAFETY: a plain syscall on a pid this test's shell started and has not reaped.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_background_sleep_reads_busy_though_no_foreground_job() {
        let mut shell = Shell::spawn("session-background");
        shell.type_line("sleep 600 &");
        assert!(shell.wait_for_a_child());
        assert!(shell.wait_for_job(false));
        assert!(!has_foreground_job(shell.pid()));
        assert!(session_has_other_process(shell.pid()).unwrap());
    }

    #[test]
    fn an_idle_shell_reads_idle() {
        let mut shell = Shell::spawn("session-idle");
        assert!(shell.wait_for_job(false));
        assert!(!session_has_other_process(shell.pid()).unwrap());
    }

    #[test]
    fn a_foreground_sleep_reads_busy() {
        let mut shell = Shell::spawn("session-foreground");
        shell.type_line("sleep 600");
        assert!(shell.wait_for_job(true));
        assert!(session_has_other_process(shell.pid()).unwrap());
    }

    #[test]
    fn a_command_name_with_a_paren_does_not_shift_the_session_field() {
        let busy = session_has_other_process_with(
            42,
            || Ok(vec![42, 43, 44]),
            |pid| {
                Ok(match pid {
                    42 => "42 (shell) S 1 42 42 0 42",
                    43 => "43 (other) S 1 43 99 0 -1",
                    44 => "44 (a) b) T 42 44 42 0 42",
                    _ => unreachable!(),
                }
                .into())
            },
        );
        assert!(
            busy.unwrap(),
            "a stopped background process belongs to the shell's session"
        );
    }

    #[test]
    fn the_shells_session_is_read_instead_of_assumed_to_be_its_pid() {
        let busy = session_has_other_process_with(
            42,
            || Ok(vec![42, 43]),
            |pid| {
                Ok(match pid {
                    42 => "42 (shell) S 1 42 21 0 42",
                    43 => "43 (sleep) S 42 43 21 0 42",
                    _ => unreachable!(),
                }
                .into())
            },
        );
        assert!(busy.unwrap());
    }

    #[test]
    fn a_process_with_session_zero_does_not_make_an_idle_shell_busy() {
        let busy = session_has_other_process_with(
            42,
            || Ok(vec![42, 43]),
            |pid| {
                Ok(match pid {
                    42 => "42 (shell) S 1 42 42 0 42",
                    43 => "43 (kworker) I 2 0 0 0 -1",
                    _ => unreachable!(),
                }
                .into())
            },
        );
        assert!(!busy.unwrap());
    }

    #[test]
    fn a_pid_vanishing_mid_scan_is_skipped() {
        for errno in [libc::ENOENT, libc::ESRCH] {
            let busy = session_has_other_process_with(
                42,
                || Ok(vec![42, 43]),
                |pid| {
                    if pid == 42 {
                        Ok("42 (shell) S 1 42 42 0 42".into())
                    } else {
                        Err(std::io::Error::from_raw_os_error(errno))
                    }
                },
            );
            assert!(!busy.unwrap());
        }
    }

    #[test]
    fn a_scan_error_reads_busy() {
        let denied = || std::io::Error::from_raw_os_error(libc::EACCES);
        for errno in [libc::ENOENT, libc::ESRCH, libc::EACCES] {
            let shell_error =
                session_has_other_process_with(42, || Ok(vec![42]), |_| Err(std::io::Error::from_raw_os_error(errno)));
            assert!(
                shell_error.unwrap_or(true),
                "an unreadable shell cannot establish idleness"
            );
        }
        let stat_error = session_has_other_process_with(
            42,
            || Ok(vec![42, 43]),
            |pid| {
                if pid == 42 {
                    Ok("42 (shell) S 1 42 42 0 42".into())
                } else {
                    Err(denied())
                }
            },
        );
        assert!(stat_error.unwrap_or(true));
        let listing_error = session_has_other_process_with(42, || Err(denied()), |_| unreachable!());
        assert!(listing_error.unwrap_or(true));
        let malformed = session_has_other_process_with(
            42,
            || Ok(vec![43]),
            |pid| {
                Ok(if pid == 42 {
                    "42 (shell) S 1 42 42 0 42"
                } else {
                    "cut off"
                }
                .into())
            },
        );
        assert!(malformed.unwrap_or(true));
    }

    #[test]
    fn a_background_job_at_an_idle_prompt_is_not_a_terminal_job() {
        let mut shell = Shell::spawn("background");
        shell.type_line("sleep 30 &");
        assert!(shell.wait_for_a_child(), "the background sleep never started");
        shell.type_line("printf '\\036background-ready\\037'");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            shell.drain();
            if shell
                .output
                .windows(b"\x1ebackground-ready\x1f".len())
                .any(|part| part == b"\x1ebackground-ready\x1f")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the shell never reached the marker after starting its background job"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !has_foreground_job(shell.pid()),
            "a job left in the background at a prompt holds no terminal"
        );
        shell.type_line("fg");
        assert!(
            shell.wait_for_job(true),
            "a background job brought back with fg holds the terminal again"
        );
    }

    #[test]
    fn a_foreground_sleep_is_a_terminal_job() {
        let mut shell = Shell::spawn("foreground");
        assert!(shell.wait_for_job(false), "an idle shell holds its own terminal");
        shell.type_line("sleep 30");
        assert!(
            shell.wait_for_job(true),
            "a command in the foreground holds the terminal"
        );
    }
}
