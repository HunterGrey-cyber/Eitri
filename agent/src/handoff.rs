//! Host-side orchestration for "continue in a real terminal" (design doc §8.3). The caller (a
//! future `shell` UI action, not built in this phase) is responsible for its own "deny new
//! turns, complete or interrupt any active turn, then close the session" sequence BEFORE calling
//! `prepare_eitri_to_cli_handoff` -- this function's only job is: acquire the lease, spawn
//! `eitri-claude-handoff` with that lease's fd inherited, and confirm the handoff genuinely
//! started before returning. It does not itself touch any `AgentSession`/`ClaudeSidecarProvider`
//! -- see this plan's "Explicitly out of scope" section for why no shared session-identity trait
//! exists yet.

use crate::lease::{LeaseError, SessionLease};
use std::process::{Command, Stdio};

/// Why no `claude --resume` invocation could be built for a conversation.
///
/// `MissingSessionId` is the one a UI has to plan for: it is what "this conversation has never
/// taken a turn" looks like from here. The other two are malformed input, not a normal state.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeCommandError {
    /// No provider session id at all: the conversation has no Claude identity yet.
    ///
    /// On the sidecar path this is a measured fact, not an inference -- the id is minted from the
    /// Agent SDK's own `system`/`init` message, which the SDK emits when a *query* starts, so a
    /// session that was created and never asked anything has none. `tests/handoff_conformance.rs`
    /// records a first version of that test timing out waiting for the event before it sent a turn.
    /// The legacy backend reads the same `system`/`init` line off the CLI's own stdout
    /// (`wire.rs`); **when that line first appears there has not been measured in this
    /// repository**, so do not read this variant as a statement about legacy's timing -- only as
    /// "no id is known yet", which is the actual precondition either way.
    MissingSessionId,
    /// The id begins with `-`, so `claude --resume <id>` would parse it as another option rather
    /// than as the session to continue. Refused rather than passed on: this is displayed to a
    /// human to run AND handed to `exec`, and neither should carry an argument that silently
    /// becomes a flag.
    SessionIdLooksLikeAFlag,
    /// No working directory. A session's cwd is part of its identity throughout this design
    /// (`prepare_eitri_to_cli_handoff` runs the wrapper with `current_dir(canonical_cwd)`, the
    /// lease key includes it, and design doc §8.4 has the provider validate it on an import), so a
    /// `--resume` with nowhere to run is not a weaker command -- it is a different one.
    MissingCwd,
}

impl std::fmt::Display for ResumeCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResumeCommandError::MissingSessionId => write!(
                f,
                "this conversation has no Claude session id yet -- one is issued when its first turn starts"
            ),
            ResumeCommandError::SessionIdLooksLikeAFlag => {
                write!(
                    f,
                    "the provider session id begins with '-', which `claude --resume` would read as an option"
                )
            }
            ResumeCommandError::MissingCwd => write!(f, "no project directory to run `claude --resume` in"),
        }
    }
}

impl std::error::Error for ResumeCommandError {}

/// The argv that continues a Claude session interactively: `claude --resume <id>`, and nothing
/// else.
///
/// The one place this project decides what that invocation is. Both consumers call it -- the
/// `eitri-claude-handoff` wrapper, which `exec`s it while holding the lease fd, and
/// `ClaudeResumeCommand`, which renders it for a human to run -- so the command a user is shown is
/// by construction the command the supported handoff path would run.
///
/// Deliberately none of the flags `agent` itself spawns `claude` with. The full list, as
/// `AgentProcess::spawn_with_binary` really builds it (`crate::process`): `--print`,
/// `--input-format stream-json`, `--output-format stream-json`, `--verbose`,
/// `--setting-sources <the session's tiers>` (`setting_sources::for_session`: the project's only when it is trusted),
/// `--permission-mode <mode>`, and -- conditionally -- `--disallowedTools <list>` and
/// `--settings <json>`. This is an ordinary interactive session in a
/// terminal, not another machine-driven one, so none of them belong.
///
/// **Two of those omissions widen what the resumed session can do, and that is the substantive
/// point, not a footnote.** `--settings` is where the `PreToolUse` gate lives, so the resumed
/// session has no Eitri permission gate at all and its tool calls raise no card anywhere.
/// `--permission-mode` and `--disallowedTools` are simply absent, so a session that Eitri ran in
/// `auto` with `Bash,Write,Edit,NotebookEdit` disallowed comes back as a plain `claude` at the
/// user's own default mode with none of those refusals -- it can do MORE than the conversation it
/// continues could. The user-facing card states the settings difference; this is the precise
/// version.
pub fn claude_resume_argv(provider_session_id: &str) -> Result<Vec<String>, ResumeCommandError> {
    let id = provider_session_id.trim();
    if id.is_empty() {
        return Err(ResumeCommandError::MissingSessionId);
    }
    if id.starts_with('-') {
        return Err(ResumeCommandError::SessionIdLooksLikeAFlag);
    }
    Ok(vec!["claude".to_string(), "--resume".to_string(), id.to_string()])
}

/// A `claude --resume` invocation together with the directory it has to run in, ready either to be
/// executed or to be shown to a human.
///
/// Built by the shell's "continue this conversation in a terminal" action. Nothing here starts a
/// process and nothing here takes a lease -- see `panel/src/terminal_handoff.rs` for what that
/// action does and does not claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeResumeCommand {
    cwd: String,
    argv: Vec<String>,
}

impl ClaudeResumeCommand {
    pub fn for_session(canonical_cwd: &str, provider_session_id: &str) -> Result<Self, ResumeCommandError> {
        if canonical_cwd.trim().is_empty() {
            return Err(ResumeCommandError::MissingCwd);
        }
        Ok(Self {
            cwd: canonical_cwd.to_string(),
            argv: claude_resume_argv(provider_session_id)?,
        })
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The session id this command resumes, read back out of the argv that will actually run
    /// rather than stored beside it -- so the id anything displays and the id the command resumes
    /// are the same value by construction, with no second field to drift.
    ///
    /// Empty only for an argv with no trailing word, which `claude_resume_argv` cannot produce;
    /// returning `""` rather than panicking keeps one malformed command from taking a whole GUI
    /// process down.
    pub fn provider_session_id(&self) -> &str {
        self.argv.last().map(String::as_str).unwrap_or_default()
    }

    /// One POSIX-shell line a user can paste: `cd <dir> && claude --resume <id>`.
    ///
    /// The `cd` is part of the command, not decoration: a session's cwd is part of its identity
    /// everywhere else in this design (see `MissingCwd` above), and the supported handoff wrapper
    /// is likewise spawned with `current_dir(canonical_cwd)` rather than wherever the host happened
    /// to be. Running the same `--resume <id>` elsewhere is not the same request.
    ///
    /// Every word is quoted only when it needs to be, by the ordinary POSIX rule (a bare word of
    /// shell-safe characters, otherwise single quotes with embedded quotes escaped). A UUID and a
    /// plain path therefore read as themselves, and a directory with a space or a quote in it is
    /// still correct rather than merely tidy.
    pub fn shell_command_line(&self) -> String {
        let mut line = format!("cd {}", sh_quote(&self.cwd));
        line.push_str(" &&");
        for word in &self.argv {
            line.push(' ');
            line.push_str(&sh_quote(word));
        }
        line
    }
}

/// POSIX `sh` quoting for one word. Bare when every character is one `sh` treats literally,
/// single-quoted otherwise, with `'` written as `'\''` (close, escaped quote, reopen) -- the only
/// way to get a single quote inside single quotes.
fn sh_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

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
            HandoffError::BinaryNotFound(e) => write!(f, "could not locate eitri-claude-handoff binary: {e}"),
            HandoffError::SpawnFailed(e) => write!(f, "failed to spawn eitri-claude-handoff: {e}"),
        }
    }
}

impl std::error::Error for HandoffError {}

#[derive(Debug)]
pub struct HandoffOutcome {
    pub child_pid: u32,
}

/// Locates the `eitri-claude-handoff` binary next to whichever binary is currently running --
/// the same "same cargo build, sibling binary" convention as `supervisor::locate_supervisor_binary`
/// and this crate's own `settings::locate_agent_hook_binary`, whose body this mirrors line for line
/// with only the binary name changed. Keep all three in step: if the lookup rule needs to change,
/// it needs to change in every one of them, not just here.
fn locate_handoff_binary() -> std::io::Result<std::path::PathBuf> {
    let current = std::env::current_exe()?;
    let dir = current
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "current_exe has no parent directory"))?;
    let candidate = dir.join("eitri-claude-handoff");
    if candidate.exists() {
        return Ok(candidate);
    }
    let one_dir_deeper = matches!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("deps") | Some("examples")
    );
    if one_dir_deeper {
        if let Some(parent) = dir.parent() {
            let fallback = parent.join("eitri-claude-handoff");
            if fallback.exists() {
                return Ok(fallback);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("eitri-claude-handoff binary not found at {candidate:?} -- was it built in the same cargo build?"),
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
pub fn prepare_eitri_to_cli_handoff(
    provider: &str,
    canonical_cwd: &str,
    provider_session_id: &str,
) -> Result<HandoffOutcome, HandoffError> {
    let lease = SessionLease::try_acquire(provider, canonical_cwd, provider_session_id).map_err(HandoffError::Lease)?;
    let binary = locate_handoff_binary().map_err(HandoffError::BinaryNotFound)?;
    let fd = lease
        .into_inherited_fd()
        .map_err(|e| HandoffError::Lease(LeaseError::Io(e)))?;

    let child = match Command::new(&binary)
        .env("EITRI_LEASE_FD", fd.to_string())
        .env("EITRI_RESUME_SESSION_ID", provider_session_id)
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
    // Eitri nor anything else could ever re-acquire that session again, and the failure would
    // present as `AlreadyHeld` naming a holder that no longer exists.
    //
    // Verified for real, both directions, rather than reasoned from the flock(2) man page: with
    // the parent keeping its fd, a fresh `LOCK_EX|LOCK_NB` still failed after the child was
    // killed (lock stuck forever); with the parent closing it, the same probe still failed while
    // the child was alive and succeeded once the child died.
    unsafe { libc::close(fd) };

    Ok(HandoffOutcome { child_pid: child.id() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the invocation: an ordinary interactive resume, with none of the
    /// machine-driving flags `agent` itself passes. If this ever grew `--print` or a `--settings`,
    /// the "continue in a terminal" action would hand the user a session that is not a terminal
    /// session at all.
    #[test]
    fn the_resume_argv_is_the_plain_interactive_invocation() {
        assert_eq!(
            claude_resume_argv("1857dcd5-973b-46a2-8d1e-0f0c9b2a4e11").unwrap(),
            vec!["claude", "--resume", "1857dcd5-973b-46a2-8d1e-0f0c9b2a4e11"]
        );
    }

    /// The precondition a UI has to enforce, stated as a value rather than as prose: a conversation
    /// that has never taken a turn has no Claude session id, and there is no command to give.
    #[test]
    fn a_conversation_with_no_session_id_yields_no_command_at_all() {
        assert_eq!(
            claude_resume_argv("").unwrap_err(),
            ResumeCommandError::MissingSessionId
        );
        assert_eq!(
            claude_resume_argv("   ").unwrap_err(),
            ResumeCommandError::MissingSessionId
        );
        assert_eq!(
            ClaudeResumeCommand::for_session("/tmp/project", "").unwrap_err(),
            ResumeCommandError::MissingSessionId
        );
    }

    /// `claude --resume --anything` would read the id as another option. Refused at the one place
    /// the argv is built, so neither the displayed line nor the `exec` can carry it.
    #[test]
    fn an_id_that_would_read_as_a_flag_is_refused_rather_than_passed_on() {
        assert_eq!(
            claude_resume_argv("--dangerously-skip-permissions").unwrap_err(),
            ResumeCommandError::SessionIdLooksLikeAFlag
        );
        assert_eq!(
            claude_resume_argv("-r").unwrap_err(),
            ResumeCommandError::SessionIdLooksLikeAFlag
        );
    }

    /// Claude stores a session under the directory it was created in, so a command with nowhere to
    /// run is not a weaker command, it is a different one.
    #[test]
    fn a_command_with_no_directory_to_run_in_is_refused() {
        assert_eq!(
            ClaudeResumeCommand::for_session("", "abc").unwrap_err(),
            ResumeCommandError::MissingCwd
        );
        assert_eq!(
            ClaudeResumeCommand::for_session("  ", "abc").unwrap_err(),
            ResumeCommandError::MissingCwd
        );
    }

    /// What this actually pins, stated exactly, because an earlier version of this doc claimed
    /// more than the body can deliver: the command a user is SHOWN is built from
    /// `claude_resume_argv` and renders as that literal line. It says nothing about the
    /// `eitri-claude-handoff` wrapper -- the first assert is `f(x) == f(x)` (`for_session` calls
    /// `claude_resume_argv`), and the binary is not in scope of a unit test in this crate at all.
    ///
    /// The wrapper's own argv is guarded for real, by running the real binary, in
    /// `tests/handoff_wrapper_argv.rs`.
    #[test]
    fn the_displayed_line_is_the_literal_resume_invocation() {
        let command = ClaudeResumeCommand::for_session("/home/user/project", "abc-123").unwrap();
        assert_eq!(command.argv(), claude_resume_argv("abc-123").unwrap().as_slice());
        assert_eq!(
            command.shell_command_line(),
            "cd /home/user/project && claude --resume abc-123"
        );
        assert_eq!(command.provider_session_id(), "abc-123");
    }

    /// A path that needs no quoting reads as itself; one that does is still correct. Both matter:
    /// the common case is a line a human reads and trusts, and the awkward case is a line that has
    /// to actually work when pasted.
    #[test]
    fn a_directory_is_quoted_only_when_the_shell_would_need_it() {
        let plain = ClaudeResumeCommand::for_session("/home/user/project", "abc").unwrap();
        assert_eq!(
            plain.shell_command_line(),
            "cd /home/user/project && claude --resume abc"
        );

        let spaced = ClaudeResumeCommand::for_session("/home/user/my project", "abc").unwrap();
        assert_eq!(
            spaced.shell_command_line(),
            "cd '/home/user/my project' && claude --resume abc"
        );

        let quoted = ClaudeResumeCommand::for_session("/home/user/it's", "abc").unwrap();
        assert_eq!(
            quoted.shell_command_line(),
            r"cd '/home/user/it'\''s' && claude --resume abc"
        );
    }

    /// The id is trimmed on the way in, so a stray newline out of a record or a wire field cannot
    /// reach either consumer.
    #[test]
    fn surrounding_whitespace_on_an_id_is_dropped_rather_than_quoted_into_the_command() {
        let command = ClaudeResumeCommand::for_session("/tmp/p", " abc-123\n").unwrap();
        assert_eq!(command.argv()[2], "abc-123");
        assert_eq!(command.shell_command_line(), "cd /tmp/p && claude --resume abc-123");
    }
}
