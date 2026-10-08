//! What a provider other than the Claude sidecar needs beyond `AgentProvider`: the policy of the tab's
//! mode on every turn, and the workspace-trust plan the user's answer was bound to.
//!
//! `CreateSessionRequest` and `SendTurnRequest` carry no mode on purpose -- a mode field the wire
//! honoured would be a way back to an ungated session -- so an external provider cannot be started or
//! driven through them. Its plain `create_session`, `resume_session` and `send_turn` refuse, and the
//! only way in is the extension below, where a caller cannot start a session without stating the plan.
//!
//! Nothing here depends on `core`: the trust plan is built there from the very discovery the user's
//! answer was bound to, and plain values cross.

use crate::projection::AgentDomainEvent;
use crate::provider::{AgentProvider, ProviderError, SendTurnRequest};
use std::path::PathBuf;

/// Who reviews what the agent asks to run outside its sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reviewer {
    /// The agent's own reviewer decides (the tab's Auto mode).
    Agent,
    /// Every request reaches the user as a card (the tab's Bypass mode answers them itself).
    User,
}

/// The policy a turn runs under. The approval policy and the sandbox are constants of the provider,
/// so the reviewer is the only thing the tab's mode changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnPolicy {
    pub reviewer: Reviewer,
}

/// Whether the agent may treat one directory as trusted. Each provider spells it in its own wire
/// format (Codex: `wire::trust_level_word`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirTrust {
    Trusted,
    Untrusted,
}

/// The trust decisions of one session start, built by the host from the discovery the user's answer
/// was bound to. A provider applies it as given and never recomputes the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustPlan {
    /// Every ancestor of the project root up to `/`, each with its level.
    pub dirs: Vec<(PathBuf, DirTrust)>,
    /// Whether the project's skill instructions stay out of the agent's prompt.
    pub skills_off: bool,
    /// The discovery this plan came from, for the record and for the tripwires.
    pub fingerprint: String,
    /// Names of tool servers found only in project configuration.
    pub project_only: Vec<String>,
}

/// Everything a start or a resume needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalStart {
    pub cwd: String,
    pub plan: TrustPlan,
    pub policy: TurnPolicy,
    /// The agent's own id of the session to continue; `None` starts a fresh one.
    pub resume: Option<String>,
}

/// A provider that is not the Claude sidecar. Its plain `AgentProvider` session and turn methods
/// answer `UnsupportedCapability("external")`.
pub trait ExternalProvider: AgentProvider {
    /// The name records and leases are keyed by (`codex`, `acp:<name>`).
    fn name(&self) -> &'static str;
    /// Whether a session this provider started can be continued later through `start` with
    /// `resume` set. Recorded with the session, so a launch's restore and the resume picker offer
    /// only what can come back. The Claude-shaped `ProviderCapabilities::resume` stays off on an
    /// external provider: it names the plain `resume_session` door, which is closed here.
    fn resumes(&self) -> bool;
    /// Starts a session, or resumes the one `req.resume` names. Returns the session's own id.
    fn start(&self, req: ExternalStart) -> Result<String, ProviderError>;
    /// Sends one turn under `policy`. Returns the turn's id, taken from the agent's own answer.
    fn send_turn_with(&self, req: SendTurnRequest, policy: TurnPolicy) -> Result<String, ProviderError>;
    /// The past of the session a `start` resumed, oldest first, handed over once; empty for a fresh start
    /// or an agent that replays nothing. The host folds it into the projection before any live event, as a
    /// Claude resume's transcript is, so a past turn is never taken for one happening now. `pump` never
    /// returns these events.
    fn take_history(&self) -> Vec<AgentDomainEvent> {
        Vec::new()
    }
}
