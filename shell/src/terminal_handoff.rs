//! "Continue this conversation in a real terminal": the rule for when it is possible, and the
//! command it produces.
//!
//! **What this does NOT do, and why.** It starts nothing and it takes no lease. The supported,
//! strongly-exclusive handoff the design doc describes (§8.3, and `agent::handoff::
//! prepare_neovibe_to_cli_handoff` which implements it) works by spawning
//! `neovibe-claude-handoff` as a direct child with the already-locked lease fd inherited and the
//! host's own stdio inherited -- it hands the CALLER's terminal to `claude`. `shell` is a GUI
//! process and has no terminal to hand over: under a desktop launcher its stdio is not a terminal
//! at all, and under `cargo run` it is a terminal `shell` is itself still writing `eprintln!`
//! diagnostics to, which an interactive TUI would then be sharing with it.
//!
//! Routing the lease fd through a terminal emulator instead was considered and not taken, on two
//! grounds that are about what can be ESTABLISHED here rather than about what would happen.
//! First, which emulator: `$TERMINAL` is unset on this machine, the GNOME default key names
//! `xdg-terminal-exec`, which is not installed, and five emulators are (foot, alacritty, kitty,
//! gnome-terminal, xterm) -- so the choice would be a guess. Second, and worse: nothing in a
//! terminal emulator's interface promises to preserve a file descriptor it knows nothing about
//! across the fork/exec that starts the command, and whether any of these does has NOT been
//! tested here. The failure that matters is not "the lock was silently dropped" but "fd number N
//! is open and refers to something else": `neovibe-claude-handoff`'s own sanity check is
//! `fcntl(fd, F_GETFD)`, which only asks whether the descriptor is open, so a reused number would
//! pass it and `claude` would run believing it holds a lease it does not.
//!
//! So this takes the other path the design doc names, and names it the way the doc requires:
//! design doc §8.3's closing paragraph allows handing the user the command to run themselves, on
//! the condition that it is "明确显示为 raw/manual 路径并带并发风险提示" -- shown explicitly as the
//! raw/manual path, carrying a concurrency warning. That warning is not a formality here: with no
//! lease taken, nothing prevents this workspace from also resuming the same session, and §8.5/§17.7
//! reject any claim that a lease would stop an external `claude --resume` anyway.
//!
//! What this module DOES enforce is the ordering §8.3 asks for, **steps 1-4 and only those**:
//! `agent_panel` closes the session for real and waits for that close to finish before the command
//! is ever shown. The user cannot be looking at the command while Neovibe is still driving the
//! session.
//!
//! Step 5 (保存 Neovibe projection 与 provider session ID) is NOT done here, and saying "steps 1-5"
//! would be one step too many. Nothing on this path writes a projection: the worker calls
//! `AgentBackend::shutdown()` and drops the backend, and on the default legacy backend nothing is
//! persisted at all (`BackendGreeting::for_kind` returns `resumable: None` for it, because that
//! backend cannot resume). What the panel does keep is narrower and is the reason the command does
//! not evaporate on a reload: `AgentPanelState::last_handoff` holds the produced
//! `ClaudeResumeCommand` in the Rust host, so a reloaded document is handed it again. That is the
//! command, not the transcript -- the conversation itself is gone from this panel either way.
//!
//! `agent::handoff::prepare_neovibe_to_cli_handoff` is therefore still without a consumer. It is
//! the right mechanism for a host that owns a terminal; `shell` is not one.

use agent::handoff::{ClaudeResumeCommand, ResumeCommandError};
use std::path::Path;

/// What the rule needs to know about the live conversation.
///
/// Deliberately borrowed scalars rather than a `&AgentBackend`: the rule is then a pure function
/// that a test can drive without constructing a backend, which would mean spawning a real `claude`
/// or a real sidecar. Same reasoning as `agent_bridge::SnapshotView`.
pub(crate) struct HandoffFacts<'a> {
    /// Claude's own session id, read from the backend rather than from the projection.
    ///
    /// The two can genuinely differ, but only for a RESUMED sidecar conversation: `IngestState` is
    /// seeded with `initial_provider_session_id` there (`agent/src/ingestion.rs`), so the backend
    /// knows the id before any event has been folded. For a fresh conversation -- legacy or sidecar
    /// -- the field and the projection are set together, inside one lock, when `SessionOpened` is
    /// folded, so reading either would give the same answer. `None` means this conversation has
    /// never opened a provider session.
    pub(crate) provider_session_id: Option<&'a str>,
    /// `None` when no turn is running. Read from the projection, which is where the only
    /// authoritative answer lives.
    pub(crate) active_turn_id: Option<&'a str>,
    /// The cwd the provider reported for this session, when it has reported one.
    pub(crate) reported_cwd: Option<&'a str>,
    /// Whether the conversation is still live, as the projection reports its status.
    pub(crate) liveness: ConversationLiveness,
}

/// Live or over, as the projection's own `ProjectionStatus` says.
///
/// This exists as a real term the rule matches on rather than as prose, because the decision it
/// records -- that BOTH values are allowed through -- is exactly the kind a later reader
/// "corrects" by adding the missing guard. A field nothing reads could not stop that and would not
/// even compile without a `#[allow]`; an exhaustive match can, and a test can drive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationLiveness {
    Live,
    /// The projection reached `Unavailable` or `Closed`.
    Ended,
}

impl ConversationLiveness {
    /// The one mapping from the projection's status, so the panel does not restate it at the call
    /// site.
    pub(crate) fn of(status: &agent::ProjectionStatus) -> Self {
        match status {
            agent::ProjectionStatus::Starting | agent::ProjectionStatus::Running => ConversationLiveness::Live,
            agent::ProjectionStatus::Unavailable { .. } | agent::ProjectionStatus::Closed { .. } => {
                ConversationLiveness::Ended
            }
        }
    }
}

/// Why a handoff cannot happen right now. Every variant is a real state a user can be in, and each
/// one's `message()` is what they are shown instead of a control that looks broken.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandoffRefusal {
    NoSession,
    /// The conversation has never taken a turn, so no Claude session id exists to resume.
    NoProviderSessionId,
    TurnInProgress,
    /// An id that exists but cannot be turned into a command. Carried through from `agent` rather
    /// than reshaped, so the reason shown is the real one.
    Unusable(ResumeCommandError),
}

impl HandoffRefusal {
    pub(crate) fn message(&self) -> String {
        match self {
            HandoffRefusal::NoSession => "There is no conversation to continue.".to_string(),
            HandoffRefusal::NoProviderSessionId => {
                "This conversation has no Claude session id yet — one is issued when its first turn \
                 starts. Send a message first."
                    .to_string()
            }
            HandoffRefusal::TurnInProgress => {
                "A turn is still running. Let it finish, or press Stop, before continuing in a terminal."
                    .to_string()
            }
            HandoffRefusal::Unusable(e) => e.to_string(),
        }
    }
}

/// The whole rule, in one place, for both the Rust command handler and (mirrored, and separately
/// tested) the frontend's disabled state.
///
/// A session that has already ended is deliberately allowed through: continuing it elsewhere is
/// arguably the case where this matters most. `liveness` is matched exhaustively below rather than
/// left out, so that allowance is a decision in the code instead of an absence a test cannot see.
pub(crate) fn prepare_handoff(
    project_dir: &Path,
    facts: Option<HandoffFacts<'_>>,
) -> Result<ClaudeResumeCommand, HandoffRefusal> {
    let Some(facts) = facts else { return Err(HandoffRefusal::NoSession) };
    let Some(provider_session_id) = facts.provider_session_id.filter(|id| !id.trim().is_empty()) else {
        return Err(HandoffRefusal::NoProviderSessionId);
    };
    if facts.active_turn_id.is_some() {
        return Err(HandoffRefusal::TurnInProgress);
    }
    match facts.liveness {
        // Both, on purpose. A dead conversation is the case a terminal helps most with, so ending
        // is not a blocker -- and writing that as an exhaustive match means a third state cannot be
        // added to `ConversationLiveness` without someone deciding what it means for this rule.
        ConversationLiveness::Live | ConversationLiveness::Ended => {}
    }
    // The provider's own cwd wins: it is where Claude actually stored the session, which is what
    // `--resume` resolves against. `project_dir` is only what the shell asked for, and is the
    // fallback for the window where a session id exists but no `SessionOpened` has been folded yet.
    let cwd = facts.reported_cwd.unwrap_or_else(|| project_dir.to_str().unwrap_or_default());
    ClaudeResumeCommand::for_session(cwd, provider_session_id).map_err(HandoffRefusal::Unusable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn project() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    fn ready<'a>() -> HandoffFacts<'a> {
        HandoffFacts {
            provider_session_id: Some("1857dcd5-973b-46a2"),
            active_turn_id: None,
            reported_cwd: Some("/home/user/project"),
            liveness: ConversationLiveness::Live,
        }
    }

    #[test]
    fn there_is_nothing_to_continue_before_a_session_exists() {
        assert_eq!(prepare_handoff(&project(), None).unwrap_err(), HandoffRefusal::NoSession);
    }

    /// The precondition this whole action is gated on. `provider_session_id` is the real Claude
    /// session id and it is minted when the first *query* starts, so a session that was created and
    /// never asked anything has none -- and `claude --resume` against an id that was never issued
    /// would resume nothing.
    #[test]
    fn a_conversation_that_has_never_taken_a_turn_cannot_be_handed_off() {
        let refusal = prepare_handoff(
            &project(),
            Some(HandoffFacts {
                provider_session_id: None,
                active_turn_id: None,
                reported_cwd: None,
                liveness: ConversationLiveness::Live,
            }),
        )
        .unwrap_err();
        assert_eq!(refusal, HandoffRefusal::NoProviderSessionId);
        // The disabled state has to say why. "Nothing happened" and "this cannot work yet" look
        // identical to a user otherwise.
        assert!(
            refusal.message().contains("first"),
            "the refusal must explain that a first turn is what issues the id, got: {}",
            refusal.message()
        );
    }

    /// Design doc §8.3 step 2: an in-flight turn is finished or interrupted BEFORE the session is
    /// closed. Handing off mid-turn would close the session out from under a running turn.
    #[test]
    fn a_turn_in_flight_blocks_the_handoff_rather_than_cutting_it_short() {
        let refusal = prepare_handoff(
            &project(),
            Some(HandoffFacts { active_turn_id: Some("t1"), ..ready() }),
        )
        .unwrap_err();
        assert_eq!(refusal, HandoffRefusal::TurnInProgress);
    }

    /// The provider's own reported cwd wins. It is where Claude actually stored the session, which
    /// is what `--resume` resolves against; the shell's `project_dir` is only what it asked for.
    #[test]
    fn the_command_runs_where_the_provider_says_the_session_lives() {
        let command = prepare_handoff(
            &PathBuf::from("/home/user/somewhere-else"),
            Some(HandoffFacts { reported_cwd: Some("/home/user/project"), ..ready() }),
        )
        .unwrap();
        assert_eq!(command.cwd(), "/home/user/project");
        assert_eq!(command.shell_command_line(), "cd /home/user/project && claude --resume 1857dcd5-973b-46a2");
    }

    /// A session can carry a provider session id before the projection has folded a `SessionOpened`
    /// with a cwd in it, so the shell's own project directory is the fallback -- not an error.
    #[test]
    fn the_project_directory_is_the_fallback_when_the_provider_reported_none() {
        let command = prepare_handoff(&project(), Some(HandoffFacts { reported_cwd: None, ..ready() })).unwrap();
        assert_eq!(command.cwd(), "/home/user/project");
    }

    /// A session that has already ended is still worth continuing elsewhere -- arguably the case
    /// where it matters most.
    ///
    /// This asserts a real difference, not a restatement of the happy path: `ready()` is
    /// `Live`, and the only thing changed here is `liveness`. Adding an `Ended => return Err(..)`
    /// arm to `prepare_handoff` fails this test and nothing else.
    #[test]
    fn a_session_that_has_already_ended_can_still_be_handed_off() {
        let command =
            prepare_handoff(&project(), Some(HandoffFacts { liveness: ConversationLiveness::Ended, ..ready() }))
                .expect("a conversation that ended is exactly the one a terminal helps most with");
        assert_eq!(command.shell_command_line(), "cd /home/user/project && claude --resume 1857dcd5-973b-46a2");
    }

    /// The mapping from the projection's own status, so the panel cannot restate it differently.
    /// `Starting` counts as live: a session that has not opened yet has no provider session id
    /// anyway, and `NoProviderSessionId` is the refusal a user should see for it -- not a
    /// liveness one.
    #[test]
    fn liveness_is_read_off_the_projections_own_status() {
        assert_eq!(ConversationLiveness::of(&agent::ProjectionStatus::Starting), ConversationLiveness::Live);
        assert_eq!(ConversationLiveness::of(&agent::ProjectionStatus::Running), ConversationLiveness::Live);
        assert_eq!(
            ConversationLiveness::of(&agent::ProjectionStatus::Closed { reason: "done".into() }),
            ConversationLiveness::Ended
        );
        assert_eq!(
            ConversationLiveness::of(&agent::ProjectionStatus::Unavailable { reason: "gone".into() }),
            ConversationLiveness::Ended
        );
    }

    /// A turn still blocks a conversation that has ENDED -- the two terms are independent, and a
    /// dead session with a turn id still stuck on it is not a reason to skip the refusal.
    #[test]
    fn an_ended_session_with_a_turn_still_recorded_is_still_refused_for_the_turn() {
        let refusal = prepare_handoff(
            &project(),
            Some(HandoffFacts {
                active_turn_id: Some("t1"),
                liveness: ConversationLiveness::Ended,
                ..ready()
            }),
        )
        .unwrap_err();
        assert_eq!(refusal, HandoffRefusal::TurnInProgress);
    }

    /// A malformed id reaches here as the `agent`-side error rather than being reshaped, so the
    /// reason a user sees is the real one.
    #[test]
    fn a_malformed_session_id_is_reported_as_the_agent_crate_states_it() {
        let refusal = prepare_handoff(
            &project(),
            Some(HandoffFacts { provider_session_id: Some("--resume"), ..ready() }),
        )
        .unwrap_err();
        assert_eq!(refusal, HandoffRefusal::Unusable(ResumeCommandError::SessionIdLooksLikeAFlag));
    }
}
