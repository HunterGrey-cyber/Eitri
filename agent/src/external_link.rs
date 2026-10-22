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

/// A child that ended on its own, whose notice waits a short while for the last of its stderr: the end of
/// the output stream can be read a moment before stderr says why the child went. The provider keeps it in
/// its state and finishes it from `pump`, so nothing sleeps while holding that state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosingNotice {
    /// Why the link says the child ended.
    pub reason: String,
    /// When the notice stops waiting for stderr and goes without it.
    pub until: Instant,
}

impl ClosingNotice {
    /// A notice for a child that ended for `reason`, waiting for its stderr from `now` on.
    pub fn new(reason: &str, now: Instant) -> ClosingNotice {
        ClosingNotice {
            reason: reason.to_string(),
            until: now + CLOSING_NOTICE_WAIT,
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
    let waiting = notice.as_ref()?;
    let tail = lock(link)
        .as_ref()
        .map(|l| notice_tail(&l.stderr_tail()))
        .unwrap_or_default();
    if tail.is_empty() && !waiting.due(Instant::now()) {
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
        let link: Mutex<Option<JsonRpcLink>> = Mutex::new(None);
        let mut waiting = Some(ClosingNotice::new(
            "end of output",
            Instant::now() + Duration::from_secs(60),
        ));
        assert_eq!(finish_closing_notice(&mut waiting, "Codex", &link), None);
        assert!(waiting.is_some(), "a notice still waiting is kept");
        let mut over = Some(ClosingNotice {
            reason: "end of output".to_string(),
            until: Instant::now(),
        });
        assert_eq!(
            finish_closing_notice(&mut over, "Codex", &link).as_deref(),
            Some("Codex ended unexpectedly (end of output)")
        );
        assert!(over.is_none(), "a finished notice is taken");
        assert_eq!(finish_closing_notice(&mut over, "Codex", &link), None);
    }

    #[test]
    fn a_long_stderr_keeps_only_its_end_in_a_notice() {
        let long = format!("{}{}", "a".repeat(10), "b".repeat(NOTICE_TAIL_CHARS));
        assert_eq!(notice_tail(&long), "b".repeat(NOTICE_TAIL_CHARS));
        assert_eq!(notice_tail("  short \n"), "short");
    }
}
