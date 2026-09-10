//! Host-side orchestration for "continue in a real terminal" (design doc §8.3). The caller (a
//! future `shell` UI action, not built in this phase) is responsible for its own "deny new
//! turns, complete or interrupt any active turn, then close the session" sequence BEFORE calling
//! `prepare_neovibe_to_cli_handoff` -- this function's only job is: acquire the lease, spawn
//! `neovibe-claude-handoff` with that lease's fd inherited, and confirm the handoff genuinely
//! started before returning. It does not itself touch any `AgentSession`/`ClaudeSidecarProvider`
//! -- see this plan's "Explicitly out of scope" section for why no shared session-identity trait
//! exists yet.

use crate::lease::{LeaseError, SessionLease};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub enum HandoffError {
    Lease(LeaseError),
    BinaryNotFound(std::io::Error),
    SpawnFailed(std::io::Error),
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffError::Lease(e) => write!(f, "handoff lease error: {e}"),
            HandoffError::BinaryNotFound(e) => write!(f, "could not locate neovibe-claude-handoff binary: {e}"),
            HandoffError::SpawnFailed(e) => write!(f, "failed to spawn neovibe-claude-handoff: {e}"),
        }
    }
}

impl std::error::Error for HandoffError {}

#[derive(Debug)]
pub struct HandoffOutcome {
    pub child_pid: u32,
}

/// Locates the `neovibe-claude-handoff` binary next to whichever binary is currently running --
/// the same "same cargo build, sibling binary" convention as `supervisor::locate_supervisor_binary`
/// and this crate's own `settings::locate_agent_hook_binary`, whose body this mirrors line for line
/// with only the binary name changed. Keep all three in step: if the lookup rule needs to change,
/// it needs to change in every one of them, not just here.
fn locate_handoff_binary() -> std::io::Result<std::path::PathBuf> {
    let current = std::env::current_exe()?;
    let dir = current.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "current_exe has no parent directory")
    })?;
    let candidate = dir.join("neovibe-claude-handoff");
    if candidate.exists() {
        return Ok(candidate);
    }
    let one_dir_deeper = matches!(dir.file_name().and_then(|n| n.to_str()), Some("deps") | Some("examples"));
    if one_dir_deeper {
        if let Some(parent) = dir.parent() {
            let fallback = parent.join("neovibe-claude-handoff");
            if fallback.exists() {
                return Ok(fallback);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("neovibe-claude-handoff binary not found at {candidate:?} -- was it built in the same cargo build?"),
    ))
}

/// Design doc §8.3 steps 6-7: acquire the lease, spawn the wrapper with the lease's fd inherited
/// (never released between acquiring and the child's own successful spawn -- there is no window
/// where neither this process nor the child holds it), and only return success once `spawn()`
/// itself succeeds (proving the child process genuinely exists and inherited the fd -- this
/// plan's own "Verified facts" point 3 confirms `Command::spawn`'s real fd-inheritance behavior,
/// not assumed).
///
/// # Precondition the caller must enforce
///
/// `provider_session_id` must be a real Claude session UUID that actually exists, which means
/// **the conversation must already have completed at least one turn**. That id is minted from the
/// Agent SDK's own `system`/`init` message, which the SDK emits when a *query* starts -- so a
/// session that was created but never asked anything has no id to resume, and `create_session`
/// alone never produces one. This function cannot check that for you: it only ever sees a `&str`
/// and holds no session state, so it cannot tell "never took a turn" from any other caller
/// mistake. Whichever caller owns the `AgentSessionProjection` is the one that can, and a
/// "continue in a real terminal" UI action must stay disabled until the first turn completes.
///
/// Getting it wrong is not unsafe, just useless: the companion binary fail-closes on an empty id,
/// and a `claude --resume` against a nonexistent one exits on its own, releasing the lease. But it
/// wastes a real lease acquisition and a real process spawn to accomplish nothing.
pub fn prepare_neovibe_to_cli_handoff(
    provider: &str,
    canonical_cwd: &str,
    provider_session_id: &str,
) -> Result<HandoffOutcome, HandoffError> {
    let lease = SessionLease::try_acquire(provider, canonical_cwd, provider_session_id).map_err(HandoffError::Lease)?;
    let binary = locate_handoff_binary().map_err(HandoffError::BinaryNotFound)?;
    let fd = lease.into_inherited_fd().map_err(|e| HandoffError::Lease(LeaseError::Io(e)))?;

    let child = match Command::new(&binary)
        .env("NEOVIBE_LEASE_FD", fd.to_string())
        .env("NEOVIBE_RESUME_SESSION_ID", provider_session_id)
        .current_dir(canonical_cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            // The lease fd was deliberately leaked out of `SessionLease` (its `Drop` was
            // suppressed) so the child could inherit it. No child exists now, so this process
            // is the only holder -- close it, releasing the flock, rather than holding an
            // exclusive lock nothing owns for the rest of this process's lifetime.
            unsafe { libc::close(fd) };
            return Err(HandoffError::SpawnFailed(e));
        }
    };

    // Hand sole ownership of the lock to the child.
    //
    // `fd` and the child's inherited copy refer to the SAME open file description, and an flock
    // lives on the description, not on the descriptor -- so the lock is held once and referenced
    // twice, and the kernel only releases it once EVERY referencing descriptor is closed. Closing
    // ours therefore does not release anything while the child is alive; it just stops this
    // process from being one of the holders.
    //
    // That matters because the alternative is a real bug, not a style preference. If this process
    // kept its reference, the lock would outlive the child and stay held for this process's entire
    // remaining lifetime -- so once the user finished in the terminal and quit the CLI, neither
    // Neovibe nor anything else could ever re-acquire that session again, and the failure would
    // present as `AlreadyHeld` naming a holder that no longer exists.
    //
    // Verified for real, both directions, rather than reasoned from the flock(2) man page: with
    // the parent keeping its fd, a fresh `LOCK_EX|LOCK_NB` still failed after the child was
    // killed (lock stuck forever); with the parent closing it, the same probe still failed while
    // the child was alive and succeeded once the child died.
    unsafe { libc::close(fd) };

    Ok(HandoffOutcome { child_pid: child.id() })
}
