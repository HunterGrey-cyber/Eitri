//! Waiting for a child process to exit, with a deadline.

use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// Polls the child until it exits or `deadline` passes (`Ok(None)`). Polling rather than a waiting
/// thread keeps this thread the only one that can reap the child, so the pid a timeout kills is
/// still the child's.
pub(crate) fn wait_until(
    child: &mut Child,
    deadline: Instant,
    now: &dyn Fn() -> Instant,
) -> std::io::Result<Option<ExitStatus>> {
    let mut pause = Duration::from_micros(200);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(Duration::from_millis(10));
    }
}
