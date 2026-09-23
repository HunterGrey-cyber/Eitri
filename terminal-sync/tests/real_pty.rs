//! The barrier against a REAL child process on a REAL PTY, under real read chunking.
//!
//! Synthetic byte streams can be split however the test author imagines. This test lets the
//! kernel do the splitting: the child sleeps *inside* each synchronized update, so read
//! boundaries genuinely land between BSU and ESU, and the test asserts that it observed that
//! happening (`mid_update_boundaries > 0`) rather than assuming it.
//!
//! It is also the NEGATIVE CONTROL for the byte-buffering axis. `run_child` is generic over the
//! parser's `vte::ansi::Timeout`:
//!   * with [`NeverBuffer`], `Term` keeps mutating inside the update and vte's raw sync buffer
//!     stays empty;
//!   * with vte's stock `StdSyncHandler`, vte buffers the raw PTY bytes and `Term` is frozen for
//!     the whole update.
//! `real_pty_one_snapshot_per_update` MUST FAIL when built with
//! `--features control-stock-sync-handler`, which swaps `UnderTest` to `StdSyncHandler`.

mod common;

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use alacritty_terminal::vte::ansi::{StdSyncHandler, Timeout};
use common::{new_term, snapshot};
#[allow(unused_imports)]
use terminal_sync::{NeverBuffer, PublishReason, SyncDriver};

#[cfg(not(feature = "control-stock-sync-handler"))]
type UnderTest = NeverBuffer;
#[cfg(feature = "control-stock-sync-handler")]
type UnderTest = StdSyncHandler;

const FRAMES: usize = 5;

/// Five synchronized updates. Each one home-and-clears the top line three times and sleeps
/// between the writes, so a PTY read almost always ends in the middle of an update.
const SCRIPT: &str = r#"
i=1
while [ "$i" -le 5 ]; do
  printf '\033[?2026h'
  printf '\033[H\033[Kframe-%s-aaa' "$i"
  sleep 0.05
  printf '\033[H\033[Kframe-%s-bbb' "$i"
  sleep 0.05
  printf '\033[H\033[Kframe-%s-final' "$i"
  printf '\033[?2026l'
  sleep 0.05
  i=$((i+1))
done
"#;

#[derive(Debug, Default)]
struct Observations {
    reads: usize,
    publications: Vec<(PublishReason, String)>,
    /// Reads that ended with the barrier still inside a synchronized update.
    mid_update_boundaries: usize,
    /// Mid-update boundaries at which `Term` had already advanced past the last published
    /// snapshot. This is the locked model ("Term keeps parsing and mutating"); it is exactly
    /// what vte's raw-byte buffering destroys.
    mid_update_term_advanced: usize,
    /// Largest value ever seen from `Processor::sync_bytes_count()`.
    max_sync_bytes: usize,
}

fn run_child<T: Timeout>() -> Observations {
    let pty = rustix_openpty::openpty(None, None).expect("openpty");

    // NOTE: `Command` keeps the `Stdio` fds alive until IT is dropped, so it lives in a block.
    // If the parent keeps any copy of the PTY user side open, the read loop below never sees
    // EOF and the test hangs forever instead of failing.
    let mut child = {
        let user = pty.user;
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(SCRIPT);
        cmd.env("TERM", "xterm-256color");
        cmd.stdin(Stdio::from(user.try_clone().unwrap()));
        cmd.stdout(Stdio::from(user.try_clone().unwrap()));
        cmd.stderr(Stdio::from(user));
        unsafe {
            cmd.pre_exec(|| {
                // New session, then make fd 0 (the PTY user side) the controlling terminal.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn child on pty")
    };

    let mut controller = File::from(pty.controller);
    let mut term = new_term();
    let mut driver = SyncDriver::<T>::default();
    let mut obs = Observations::default();
    // Deliberately small and odd, so the kernel's chunking is not aligned to anything.
    let mut buf = [0u8; 13];

    loop {
        let n = match controller.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            // Linux returns EIO on the controller once the last user-side fd is closed.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => panic!("pty read failed: {e}"),
        };
        obs.reads += 1;

        let publications = &mut obs.publications;
        driver.feed(&mut term, &buf[..n], |t, why| publications.push((why, snapshot(t))));

        obs.max_sync_bytes = obs.max_sync_bytes.max(driver.sync_bytes_count());
        if driver.barrier().in_sync() {
            obs.mid_update_boundaries += 1;
            let last = obs.publications.last().map(|(_, s)| s.as_str()).unwrap_or("");
            if snapshot(&term) != last {
                obs.mid_update_term_advanced += 1;
            }
        }
    }

    let status = child.wait().expect("child wait");
    assert!(status.success(), "child exited with {status}");
    obs
}

#[test]
fn real_pty_one_snapshot_per_update() {
    let obs = run_child::<UnderTest>();
    println!(
        "under test: reads={} publications={} mid_update_boundaries={} \
         mid_update_term_advanced={} max_sync_bytes={}",
        obs.reads,
        obs.publications.len(),
        obs.mid_update_boundaries,
        obs.mid_update_term_advanced,
        obs.max_sync_bytes
    );

    assert!(obs.reads > FRAMES, "expected chunked reads, got {} reads", obs.reads);
    assert!(
        obs.mid_update_boundaries > 0,
        "the child never got interrupted inside an update, so this test proved nothing \
         ({} reads)",
        obs.reads
    );

    assert_eq!(
        obs.publications.len(),
        FRAMES,
        "expected exactly one consolidated snapshot per synchronized update, got {:#?}",
        obs.publications
    );
    for (i, (why, content)) in obs.publications.iter().enumerate() {
        assert_eq!(*why, PublishReason::FrameComplete, "publication {i}");
        assert_eq!(content, &format!("frame-{}-final", i + 1), "publication {i}");
        assert!(!content.contains("aaa"), "intermediate frame leaked: {content}");
        assert!(!content.contains("bbb"), "intermediate frame leaked: {content}");
    }

    // The locked semantic model, and the axis the control below measures.
    assert_eq!(
        obs.max_sync_bytes, 0,
        "NeverBuffer must never let vte buffer raw PTY bytes (saw {} bytes)",
        obs.max_sync_bytes
    );
    assert!(
        obs.mid_update_term_advanced > 0,
        "Term was frozen inside the synchronized update -- parsing was delayed, which the \
         locked model forbids ({} mid-update boundaries observed)",
        obs.mid_update_boundaries
    );
}

/// THE CONTROL. Restoring vte's stock `StdSyncHandler` re-enables its raw-byte sync buffer, and
/// the two assertions above about the byte-buffering axis invert. If this test ever starts
/// looking like the one above, the test above is no longer measuring anything.
#[test]
fn real_pty_control_stock_sync_handler_buffers_bytes_and_freezes_term() {
    let obs = run_child::<StdSyncHandler>();
    println!(
        "control   : reads={} publications={} mid_update_boundaries={} \
         mid_update_term_advanced={} max_sync_bytes={}",
        obs.reads,
        obs.publications.len(),
        obs.mid_update_boundaries,
        obs.mid_update_term_advanced,
        obs.max_sync_bytes
    );

    assert!(
        obs.mid_update_boundaries > 0,
        "control never observed a mid-update read boundary"
    );
    assert!(
        obs.max_sync_bytes > 0,
        "expected vte's stock handler to buffer raw PTY bytes, but sync_bytes_count() stayed 0"
    );
    assert_eq!(
        obs.mid_update_term_advanced, 0,
        "expected Term to be FROZEN for the whole update under StdSyncHandler, but it advanced \
         at {} of {} mid-update boundaries",
        obs.mid_update_term_advanced, obs.mid_update_boundaries
    );
}
