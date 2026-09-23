//! Does the session keep up with a noisy program? Measured against the same program read and
//! thrown away, so the number compares the pipeline with its source, not with a guess.
//!
//! Release-only and `#[ignore]`d -- a debug build measures the debug build:
//!
//!     cargo test --release -p neovibe-terminal --test throughput -- --ignored --nocapture

mod common;

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use common::{size, spec, Harness};
use neovibe_terminal::PtyChild;

const FLOOD: &str = "yes | head -n 5000000"; // 15 MB once the tty turns each \n into \r\n

/// Reads the PTY to EOF and discards it: the source's own rate, including the tty.
fn source_rate() -> (usize, Duration) {
    let started = Instant::now();
    let mut child = PtyChild::spawn(&spec("/bin/sh", &["-c", FLOOD]), size(200, 50)).unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0;
    loop {
        let mut fd = libc::pollfd {
            fd: child.master_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut fd, 1, -1) };
        match child.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
    }
    child.wait().unwrap();
    (total, started.elapsed())
}

#[test]
#[ignore = "release-only measurement; see the module doc"]
fn a_flood_keeps_up_with_its_source() {
    let (bytes, source) = source_rate();
    let started = Instant::now();
    let mut h = Harness::start(spec("/bin/sh", &["-c", FLOOD]), size(200, 50));
    h.wait_for(Duration::from_secs(120), |h| h.exited.is_some());
    let session = started.elapsed();
    let mb = bytes as f64 / 1e6;
    eprintln!(
        "{mb:.1} MB: source {:.2} MB/s ({source:?}), session {:.2} MB/s ({session:?}), {} renders",
        mb / source.as_secs_f64(),
        mb / session.as_secs_f64(),
        h.session.renders()
    );
    assert!(bytes > 14_000_000, "the flood is the size it claims: {bytes}");
    assert!(
        session.as_secs_f64() <= source.as_secs_f64() * 1.3,
        "the session took {session:?} for what the source produced in {source:?}"
    );
}
