//! What the Codex and ACP providers share around their [`JsonRpcLink`]: reading the link's queue, stopping
//! its child, the error a failed call shows, and the notice of a child that ended on its own. Each provider
//! keeps its own protocol; only the handling of the link itself lives here, so the two cannot drift apart.

use crate::jsonrpc_link::{Incoming, JsonRpcLink, LinkError};
use crate::provider::{ProviderError, ProviderErrorCode};
use crate::SIDECAR_EXIT_GRACE;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The longest stderr tail put in a notice.
pub(crate) const NOTICE_TAIL_CHARS: usize = 400;

/// How long a child's end waits for its stderr before the notice goes without it.
pub(crate) const CLOSING_NOTICE_WAIT: Duration = Duration::from_millis(100);

/// A lock that a panicked holder does not poison: the state it guards stays readable, and the provider
/// goes on answering rather than failing every later call.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn provider_error(code: ProviderErrorCode, message: impl Into<String>) -> ProviderError {
    ProviderError::Provider {
        code,
        message: message.into(),
    }
}

/// The end of the child's stderr, short enough for a notice. Email addresses are already replaced.
pub(crate) fn notice_tail(tail: &str) -> String {
    let tail = tail.trim();
    let count = tail.chars().count();
    if count <= NOTICE_TAIL_CHARS {
        return tail.to_string();
    }
    tail.chars().skip(count - NOTICE_TAIL_CHARS).collect()
}

/// A failed call on the link as the error the host shows, naming the child as `agent`.
pub(crate) fn link_error(agent: &str, link: &JsonRpcLink, what: &str, e: LinkError) -> ProviderError {
    match e {
        LinkError::Timeout => ProviderError::Timeout,
        LinkError::Closed => {
            let tail = notice_tail(&link.stderr_tail());
            let suffix = if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            };
            ProviderError::Transport(format!("{agent} ended before it answered {what}{suffix}"))
        }
        LinkError::Busy => ProviderError::Transport(format!("{agent} is not reading its input")),
        LinkError::TooLarge => provider_error(
            ProviderErrorCode::ProviderProtocolError,
            format!("{agent}'s answer to {what} was larger than 16 MiB"),
        ),
        LinkError::Rpc { message, .. } => provider_error(
            ProviderErrorCode::ProviderProtocolError,
            format!("{agent} refused {what}: {message}"),
        ),
    }
}

/// The next message the link's reader thread filed, if one waits; nothing once the link is gone. The
/// link's own lock is held only for the take, never while the message is handled.
pub(crate) fn try_recv(link: &Mutex<Option<JsonRpcLink>>) -> Option<Incoming> {
    lock(link).as_ref().and_then(JsonRpcLink::try_recv)
}

/// Ends the child and everything it runs: its stdin is closed, it gets a short grace to leave on its own,
/// then its process group is killed. That waits, so a caller holding a lock others need runs it on a
/// thread of its own.
pub(crate) fn stop_child(link: JsonRpcLink) {
    link.shutdown(SIDECAR_EXIT_GRACE);
}

/// Takes the link out of its slot and ends its child; nothing when the link is gone already. The slot is
/// unlocked before the child is waited for.
pub(crate) fn stop_linked_child(slot: &Mutex<Option<JsonRpcLink>>) {
    let link = lock(slot).take();
    if let Some(link) = link {
        stop_child(link);
    }
}

/// Ends a failed connection off the caller's thread. The ended link comes back with its stderr tail.
pub(crate) fn stop_failed_link(slot: &Mutex<Option<JsonRpcLink>>) -> Option<std::thread::JoinHandle<JsonRpcLink>> {
    let mut link = lock(slot).take()?;
    Some(std::thread::spawn(move || {
        link.finish(SIDECAR_EXIT_GRACE);
        link
    }))
}

/// Restores the ended link only after shutdown completes; polling never waits for the worker.
pub(crate) fn finish_failed_stop(
    worker: &mut Option<std::thread::JoinHandle<JsonRpcLink>>,
    slot: &Mutex<Option<JsonRpcLink>>,
) -> bool {
    if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
        return false;
    }
    if let Some(worker) = worker.take() {
        *lock(slot) = worker.join().ok();
    }
    true
}

/// An ended output stream, whose notice waits a short while for the last of stderr: the end of
/// the output stream can be read a moment before stderr says why the child went. The provider keeps it in
/// its state and finishes it from `pump`, so nothing sleeps while holding that state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosingNotice {
    /// Why the link says the child ended.
    pub reason: String,
    /// When the notice stops waiting for stderr and goes without it.
    pub until: Instant,
    /// The notice's stderr tail as it stood when the end was read: what the child wrote before. The
    /// notice goes early only once the tail differs from it, so earlier log lines alone never stand in
    /// for the reason the child is about to give.
    pub stderr_at_close: String,
}

impl ClosingNotice {
    /// A notice for a child that ended for `reason`, waiting for its stderr from `now` on.
    pub fn new(reason: &str, now: Instant) -> ClosingNotice {
        ClosingNotice {
            reason: reason.to_string(),
            until: now + CLOSING_NOTICE_WAIT,
            stderr_at_close: String::new(),
        }
    }

    /// [`ClosingNotice::new`] for the child on `link`, remembering what its stderr said so far.
    pub(crate) fn after(reason: &str, now: Instant, link: &Mutex<Option<JsonRpcLink>>) -> ClosingNotice {
        ClosingNotice {
            stderr_at_close: current_tail(link),
            ..ClosingNotice::new(reason, now)
        }
    }

    /// Whether the wait for stderr is over at `now`.
    pub fn due(&self, now: Instant) -> bool {
        now >= self.until
    }
}

/// The sentence for the tab once the child's stderr says why it ended, or once the wait is over without
/// it; the notice is taken then. `None` while it still waits, or when no child ended. `agent` names the
/// child as the sentence shows it.
pub(crate) fn finish_closing_notice(
    notice: &mut Option<ClosingNotice>,
    agent: &str,
    link: &Mutex<Option<JsonRpcLink>>,
) -> Option<String> {
    finish_closing_notice_with_clock(notice, agent, link, Instant::now)
}

fn finish_closing_notice_with_clock(
    notice: &mut Option<ClosingNotice>,
    agent: &str,
    link: &Mutex<Option<JsonRpcLink>>,
    now: impl FnOnce() -> Instant,
) -> Option<String> {
    let waiting = notice.as_ref()?;
    let tail = current_tail(link);
    let fresh = !tail.is_empty() && tail != waiting.stderr_at_close;
    if !fresh && !waiting.due(now()) {
        return None;
    }
    let reason = notice.take()?.reason;
    let suffix = if tail.is_empty() {
        String::new()
    } else {
        format!(": {tail}")
    };
    Some(format!("{agent} ended unexpectedly ({reason}){suffix}"))
}

/// The link's stderr tail as a notice shows it, empty without a link.
fn current_tail(link: &Mutex<Option<JsonRpcLink>>) -> String {
    lock(link)
        .as_ref()
        .map(|l| notice_tail(&l.stderr_tail()))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::path::PathBuf;

    pub struct Scratch(pub PathBuf);
    impl Scratch {
        pub fn new() -> Self {
            let path = std::env::temp_dir().join(format!("eitri-process-test-{}", uuid::Uuid::new_v4()));
            crate::private_fs::create_private_dir_all(&path, &path).unwrap();
            Self(path)
        }

        pub fn script(&self, body: &str) -> PathBuf {
            let path = self.0.join("stand-in");
            script_fixture::write_executable_script(&path, format!("#!/bin/sh\n{body}\n"), 0o700).unwrap();
            path
        }

        pub fn link(&self, script: &str) -> JsonRpcLink {
            JsonRpcLink::spawn(crate::jsonrpc_link::LinkSpec {
                program: PathBuf::from("/bin/sh"),
                args: vec![
                    "-c".into(),
                    script.into(),
                    "stand-in".into(),
                    self.0.clone().into_os_string(),
                ],
                cwd: self.0.clone(),
                framing: crate::jsonrpc_link::Framing::WithJsonrpcMember,
                env: Vec::new(),
                env_remove: Vec::new(),
                over_cap: crate::providers::acp::provider::over_cap_reply,
            })
            .unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn wait_for(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(Instant::now() < deadline, "stand-in did not reach the expected state");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn read_pid(path: &std::path::Path) -> u32 {
        let mut pid = None;
        wait_for(|| {
            pid = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok());
            pid.is_some()
        });
        pid.unwrap()
    }

    #[cfg(target_os = "linux")]
    pub fn running(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit(')')
                    .next()
                    .map(|s| !matches!(s.trim_start().chars().next(), Some('Z' | 'X')))
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closing_notice_is_due_only_once_its_wait_is_over() {
        let start = Instant::now();
        let notice = ClosingNotice::new("end of output", start);
        assert!(!notice.due(start), "a notice just made still waits for stderr");
        assert!(!notice.due(start + CLOSING_NOTICE_WAIT - Duration::from_millis(1)));
        assert!(notice.due(start + CLOSING_NOTICE_WAIT), "the wait ends at its deadline");
    }

    #[test]
    fn a_notice_without_a_link_goes_once_its_wait_is_over_and_only_once() {
        let now = Instant::now();
        let link: Mutex<Option<JsonRpcLink>> = Mutex::new(None);
        let mut waiting = Some(ClosingNotice::new("end of output", now + Duration::from_secs(60)));
        assert_eq!(
            finish_closing_notice_with_clock(&mut waiting, "Codex", &link, || now),
            None
        );
        assert!(waiting.is_some(), "a notice still waiting is kept");
        let mut over = Some(ClosingNotice {
            reason: "end of output".to_string(),
            until: now,
            stderr_at_close: String::new(),
        });
        assert_eq!(
            finish_closing_notice_with_clock(&mut over, "Codex", &link, || now).as_deref(),
            Some("Codex ended unexpectedly (end of output)")
        );
        assert!(over.is_none(), "a finished notice is taken");
        assert_eq!(
            finish_closing_notice_with_clock(&mut over, "Codex", &link, || now),
            None
        );
    }

    /// A child that logged to stderr before it ended: the notice waits for what it says after its end,
    /// rather than going at once with the earlier lines.
    #[test]
    fn a_notice_waits_for_stderr_written_after_the_end_even_when_earlier_stderr_exists() {
        use test_support::{wait_for, Scratch};
        let scratch = Scratch::new();
        let link = scratch.link(
            "echo early >&2; while [ ! -f close ]; do sleep 0.002; done; exec 1>&-; \
             while [ ! -f final ]; do sleep 0.002; done; echo final-reason >&2; cat >/dev/null",
        );
        wait_for(|| link.stderr_tail().contains("early"));
        std::fs::write(scratch.0.join("close"), b"").unwrap();
        let slot = Mutex::new(Some(link));
        let mut reason = None;
        wait_for(|| {
            if let Some(Incoming::Closed { reason: ended }) = try_recv(&slot) {
                reason = Some(ended);
            }
            reason.is_some()
        });
        let now = Instant::now();
        let mut notice = Some(ClosingNotice::after(&reason.unwrap(), now, &slot));
        assert!(finish_closing_notice_with_clock(&mut notice, "Codex", &slot, || now).is_none());
        std::fs::write(scratch.0.join("final"), b"").unwrap();
        wait_for(|| current_tail(&slot).contains("final-reason"));
        let said =
            finish_closing_notice_with_clock(&mut notice, "Codex", &slot, || now).expect("fresh stderr ends the wait");
        assert!(said.contains("final-reason"), "{said}");
        stop_linked_child(&slot);
    }

    #[test]
    fn a_long_stderr_keeps_only_its_end_in_a_notice() {
        let long = format!("{}{}", "a".repeat(10), "b".repeat(NOTICE_TAIL_CHARS));
        assert_eq!(notice_tail(&long), "b".repeat(NOTICE_TAIL_CHARS));
        assert_eq!(notice_tail("  short \n"), "short");
    }
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
