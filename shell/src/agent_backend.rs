//! Which Claude backend the agent panel drives, and the one shape the panel talks to.
//!
//! Two real backends exist and they are NOT interchangeable at the type level:
//!
//! - `agent::AgentSession` -- the legacy backend, which spawns `claude` itself, parses its
//!   stream-json, and gates tool permissions with a generated `PreToolUse` hook. Because the CLI
//!   wire protocol has no "a turn started" line, it SYNTHESIZES `TurnStarted` locally and returns
//!   the synthesized events from `send_turn`.
//! - `agent::AgentConversation` over `agent::ClaudeSidecarProvider` -- the SDK path, where Verdandi
//!   emits real, server-originated events. It synthesizes nothing, so `send_turn` returns a turn id
//!   and no events; state arrives on the next `pump()`.
//!
//! That difference is the whole point of the migration and must not be papered over: the sidecar
//! path's state is authoritative because it comes from the server. This enum keeps both callable
//! from one place without pretending they are the same thing.
//!
//! Selected by `NEOVIBE_AGENT_BACKEND=legacy|sidecar`, defaulting to `legacy` until the sidecar
//! path clears its acceptance criteria.

use agent::{
    AgentConversation, AgentSession, AgentDomainEvent, AgentSessionProjection, ClaudeSidecarProvider,
    ConversationError, PermissionMode, ProviderCapabilities, ProviderInfo, CONSERVATIVE_DISALLOWED_TOOLS,
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendKind {
    Legacy,
    Sidecar,
}

impl BackendKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BackendKind::Legacy => "legacy",
            BackendKind::Sidecar => "sidecar",
        }
    }

    /// Reads `NEOVIBE_AGENT_BACKEND`. Anything unrecognized falls back to `legacy` with a warning
    /// rather than failing to start: a typo must not cost the user their editor, and silently
    /// picking the NEW backend on a typo would be the dangerous direction.
    pub(crate) fn from_env() -> Self {
        match std::env::var("NEOVIBE_AGENT_BACKEND").as_deref().map(str::trim) {
            Ok("sidecar") => BackendKind::Sidecar,
            Ok("legacy") | Err(_) => BackendKind::Legacy,
            Ok(other) => {
                eprintln!(
                    "[agent_backend] NEOVIBE_AGENT_BACKEND={other:?} is not recognized \
                     (expected \"legacy\" or \"sidecar\"); using legacy"
                );
                BackendKind::Legacy
            }
        }
    }
}

/// One command's failure, classified by whether the conversation survives it.
///
/// `benign` means the backend rejected THIS command (wrong time, unknown id) and the session is
/// still healthy -- report it and carry on. Anything else means the session is gone and the panel
/// must tear it down. Getting this wrong in the benign direction strands a dead session in the UI;
/// getting it wrong the other way throws away a working conversation over a double-click.
pub(crate) struct BackendError {
    pub(crate) message: String,
    pub(crate) benign: bool,
}

impl BackendError {
    fn fatal(message: String) -> Self {
        Self { message, benign: false }
    }
}

impl From<ConversationError> for BackendError {
    fn from(error: ConversationError) -> Self {
        Self { benign: error.is_benign(), message: error.to_string() }
    }
}

impl From<std::io::Error> for BackendError {
    fn from(error: std::io::Error) -> Self {
        // The legacy backend's own established classification (see agent_panel's pre-existing
        // handling): InvalidInput is "a turn is already in progress", NotFound is "that permission
        // is already answered or unknown". Both are ordering complaints, not process failures.
        let benign = matches!(
            error.kind(),
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
        );
        Self { benign, message: error.to_string() }
    }
}

/// `AgentSession` is ~576 bytes while `AgentConversation` is boxed, so clippy flags the size
/// disparity. Boxing the legacy variant too would add an allocation and an indirection to the path
/// this milestone is trying to retire, for a struct that exists at most once per window. Left as is
/// deliberately.
#[allow(clippy::large_enum_variant)]
pub(crate) enum AgentBackend {
    Legacy(AgentSession),
    Sidecar(Box<AgentConversation>),
}

impl AgentBackend {
    /// Constructs the selected backend. **Blocking, and for the sidecar path potentially for
    /// minutes** (a cold Verdandi checkout runs `npm ci` and `npm run build`), so callers must run
    /// this off the GTK main thread -- see `agent_panel`'s connect worker.
    pub(crate) fn start(
        kind: BackendKind,
        project_dir: &Path,
        mode: PermissionMode,
    ) -> Result<Self, BackendError> {
        match kind {
            BackendKind::Legacy => AgentSession::start(project_dir, mode, CONSERVATIVE_DISALLOWED_TOOLS)
                .map(AgentBackend::Legacy)
                .map_err(|e| BackendError::fatal(format!("failed to start the legacy Claude backend: {e}"))),
            BackendKind::Sidecar => {
                let instance_id = uuid::Uuid::new_v4().to_string();
                let provider = ClaudeSidecarProvider::connect(&instance_id).map_err(|e| {
                    // `e` already carries the sidecar's own stderr tail when it failed to start
                    // (agent::providers::claude_sidecar::spawn), which is the only place the real
                    // cause -- an incompatible Claude CLI, say -- exists.
                    BackendError::fatal(format!("failed to connect to the Verdandi sidecar: {e}"))
                })?;
                AgentConversation::create(Box::new(provider), project_dir, mode)
                    .map(|conversation| AgentBackend::Sidecar(Box::new(conversation)))
                    .map_err(|e| BackendError::fatal(format!("failed to create a Claude session: {e}")))
            }
        }
    }

    pub(crate) fn kind(&self) -> BackendKind {
        match self {
            AgentBackend::Legacy(_) => BackendKind::Legacy,
            AgentBackend::Sidecar(_) => BackendKind::Sidecar,
        }
    }

    pub(crate) fn projection(&self) -> &AgentSessionProjection {
        match self {
            AgentBackend::Legacy(session) => &session.projection,
            AgentBackend::Sidecar(conversation) => &conversation.projection,
        }
    }

    /// Neovibe's own conversation identity. `None` for the legacy backend, which has no concept of
    /// one -- it never distinguished its three identities in the first place.
    pub(crate) fn conversation_id(&self) -> Option<&str> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Sidecar(conversation) => Some(conversation.conversation_id()),
        }
    }

    /// The Claude session UUID, once the provider has reported it. Distinct from the projection's
    /// `session_id` (Verdandi's) for the sidecar backend; identical for the legacy one, whose CLI
    /// never separated them.
    pub(crate) fn provider_session_id(&self) -> Option<&str> {
        match self {
            AgentBackend::Legacy(session) => session.projection.provider_session_id.as_deref(),
            AgentBackend::Sidecar(conversation) => conversation.provider_session_id(),
        }
    }

    pub(crate) fn provider_info(&self) -> Option<&ProviderInfo> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Sidecar(conversation) => Some(conversation.provider_info()),
        }
    }

    /// Effective capabilities: what the server advertises, intersected with what this client can
    /// actually drive end to end (`AgentConversation`/`ClaudeSidecarProvider` apply the
    /// intersection; this just reports it).
    ///
    /// The legacy backend advertises nothing over a wire, so its capabilities are stated from what
    /// its code demonstrably does: it interrupts, it has no resume and no fork, and it supports a
    /// real bypass mode.
    pub(crate) fn capabilities(&self) -> ProviderCapabilities {
        match self {
            AgentBackend::Legacy(_) => ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
            },
            AgentBackend::Sidecar(conversation) => conversation.capabilities(),
        }
    }

    /// Submits a turn. The returned events are the ones the backend produced SYNCHRONOUSLY, which
    /// is a real difference between the two: the legacy backend returns a synthesized `TurnStarted`
    /// (its wire protocol has no such line to translate), while the sidecar backend returns nothing
    /// because Verdandi emits a real one on the event stream. The panel must not invent an event
    /// for the sidecar path to "even that out" -- server-originated state is the authority.
    pub(crate) fn send_turn(&mut self, text: &str) -> Result<Vec<AgentDomainEvent>, BackendError> {
        match self {
            AgentBackend::Legacy(session) => Ok(session.send_turn(text)?),
            AgentBackend::Sidecar(conversation) => {
                conversation.send_turn(text)?;
                Ok(Vec::new())
            }
        }
    }

    pub(crate) fn interrupt(&mut self) -> Result<Vec<AgentDomainEvent>, BackendError> {
        match self {
            AgentBackend::Legacy(session) => Ok(session.interrupt()?),
            AgentBackend::Sidecar(conversation) => {
                conversation.interrupt()?;
                Ok(Vec::new())
            }
        }
    }

    pub(crate) fn respond_permission(
        &mut self,
        permission_id: &str,
        allow: bool,
        reason: Option<&str>,
    ) -> Result<Vec<AgentDomainEvent>, BackendError> {
        match self {
            AgentBackend::Legacy(session) => Ok(session.respond_permission(permission_id, allow, reason)?),
            AgentBackend::Sidecar(conversation) => {
                conversation.respond_permission(permission_id, allow, reason)?;
                Ok(Vec::new())
            }
        }
    }

    pub(crate) fn pump(&mut self) -> Vec<AgentDomainEvent> {
        match self {
            AgentBackend::Legacy(session) => session.pump(),
            AgentBackend::Sidecar(conversation) => conversation.pump(),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        match self {
            AgentBackend::Legacy(session) => session.shutdown(),
            AgentBackend::Sidecar(conversation) => conversation.shutdown(),
        }
    }
}

/// What the frontend is told at handshake time, before any session exists.
///
/// The permission policy is part of it because the two backends genuinely differ: the legacy
/// backend has a real, tested interactive permission gate, while the sidecar path in this milestone
/// ships BYPASS only -- its `interactive` and `verdandi_rules` modes are confirmed to behave
/// identically in the current sidecar, so offering them as distinct choices would be a lie.
pub(crate) struct BackendGreeting {
    pub(crate) kind: BackendKind,
    pub(crate) project_dir: PathBuf,
    pub(crate) permission_modes: &'static [&'static str],
    pub(crate) expected_verdandi_revision: Option<&'static str>,
}

impl BackendGreeting {
    pub(crate) fn for_kind(kind: BackendKind, project_dir: PathBuf) -> Self {
        match kind {
            BackendKind::Legacy => Self {
                kind,
                project_dir,
                permission_modes: &["auto", "bypass"],
                expected_verdandi_revision: None,
            },
            BackendKind::Sidecar => Self {
                kind,
                project_dir,
                // BYPASS only, deliberately. Not a capability gap to paper over later: until a
                // permission policy with genuinely distinct runtime behavior exists on this path,
                // presenting a choice would invite the user to pick a mode that does nothing
                // different.
                permission_modes: &["bypass"],
                expected_verdandi_revision: Some(agent::EXPECTED_VERDANDI_REVISION),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_round_trips_its_wire_name() {
        assert_eq!(BackendKind::Legacy.as_str(), "legacy");
        assert_eq!(BackendKind::Sidecar.as_str(), "sidecar");
    }

    #[test]
    fn the_sidecar_greeting_offers_bypass_only_and_names_its_verdandi_baseline() {
        let greeting = BackendGreeting::for_kind(BackendKind::Sidecar, PathBuf::from("/tmp"));
        assert_eq!(greeting.permission_modes, &["bypass"]);
        assert_eq!(greeting.expected_verdandi_revision, Some(agent::EXPECTED_VERDANDI_REVISION));
    }

    #[test]
    fn the_legacy_greeting_keeps_its_real_two_mode_choice() {
        let greeting = BackendGreeting::for_kind(BackendKind::Legacy, PathBuf::from("/tmp"));
        assert_eq!(greeting.permission_modes, &["auto", "bypass"]);
        assert_eq!(greeting.expected_verdandi_revision, None, "the legacy backend has no Verdandi dependency");
    }

    #[test]
    fn neither_backend_reports_resume_in_this_milestone() {
        // Resume is not exposed in this phase. If this ever fails, the UI would grow a Resume
        // control before the path behind it exists.
        let legacy = ProviderCapabilities { resume: false, fork: false, interrupt: true, bypass_permission_mode: true };
        assert!(!legacy.resume);
        assert!(!legacy.fork);
    }

    #[test]
    fn a_benign_io_error_is_classified_benign_and_anything_else_is_not() {
        let already_running: BackendError =
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "a turn is already in progress").into();
        assert!(already_running.benign);

        let unknown_permission: BackendError =
            std::io::Error::new(std::io::ErrorKind::NotFound, "no pending permission request").into();
        assert!(unknown_permission.benign);

        let broken_pipe: BackendError = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone").into();
        assert!(!broken_pipe.benign, "a dead process must never be treated as a benign ordering complaint");
    }

    #[test]
    fn a_benign_conversation_error_survives_and_a_fatal_one_does_not() {
        let benign: BackendError = ConversationError::TurnAlreadyActive.into();
        assert!(benign.benign);

        let fatal: BackendError = ConversationError::NoSession.into();
        assert!(!fatal.benign);
    }
}
