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
    ProjectionGuard, UiDelivery,
    ConversationError, PermissionDecision, PermissionMode, ProviderCapabilities, ProviderInfo,
    ResumableSession,
};
use std::path::{Path, PathBuf};

/// The legacy backend's capabilities, stated once so a test can assert on the value
/// `capabilities()` actually returns rather than on a copy of it.
///
/// It advertises nothing over a wire, so these are read off what its code demonstrably does: it
/// interrupts (`AgentSession::interrupt` sends a real control_request), it has no resume and no
/// fork, and `PermissionMode::Bypass` is a real construction-time choice.
const LEGACY_CAPABILITIES: ProviderCapabilities = ProviderCapabilities {
    resume: false,
    fork: false,
    interrupt: true,
    bypass_permission_mode: true,
    // Its interactive gate is the `PreToolUse` hook relay, which is this backend's primary,
    // end-to-end-verified permission mechanism -- not the leaky `can_use_tool` it also listens to.
    interactive_permission_mode: true,
};

// Compile-time, not a test: resume and fork are not exposed in this milestone, and the UI gates its
// controls on exactly these flags. A runtime assertion would be a constant assertion in a test
// nobody has to run; this fails the build. Flipping either one here must happen in the same change
// that implements the path behind it -- see `CLIENT_IMPLEMENTS_RESUME` in the agent crate for the
// sidecar side of the same rule.
const _: () = {
    assert!(!LEGACY_CAPABILITIES.resume, "resume must not be advertised before it works end to end");
    assert!(!LEGACY_CAPABILITIES.fork, "fork must not be advertised before it works end to end");
};

/// A borrow of whichever projection this backend owns.
///
/// The sidecar's lives behind the ingestion thread's lock and the legacy backend's is a plain field.
/// Both deref to the same type, so call sites read `backend.projection().status` either way.
pub enum ProjectionRef<'a> {
    Borrowed(&'a AgentSessionProjection),
    Guarded(ProjectionGuard<'a>),
}

impl std::ops::Deref for ProjectionRef<'_> {
    type Target = AgentSessionProjection;
    fn deref(&self) -> &AgentSessionProjection {
        match self {
            ProjectionRef::Borrowed(projection) => projection,
            ProjectionRef::Guarded(guard) => guard,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Legacy,
    Sidecar,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Legacy => "legacy",
            BackendKind::Sidecar => "sidecar",
        }
    }

    /// Reads `NEOVIBE_AGENT_BACKEND`. Anything unrecognized falls back to `legacy` with a warning
    /// rather than failing to start: a typo must not cost the user their editor, and silently
    /// picking the NEW backend on a typo would be the dangerous direction.
    pub fn from_env() -> Self {
        Self::choose(
            std::env::var("NEOVIBE_AGENT_BACKEND").ok().as_deref().map(str::trim),
            agent::packaged_sidecar_available(),
        )
    }

    /// [`from_env`](Self::from_env)'s decision, over values a caller supplies.
    ///
    /// Separate so the matrix is testable without mutating the process environment, which every
    /// other test in this binary shares. That is not hygiene for its own sake: the same shape --
    /// a function taking an override as a signal while reading its value from the environment
    /// itself -- was a real bug in `locate_verdandi_checkout` earlier the same day, and a test is
    /// what found it.
    pub fn choose(explicit: Option<&str>, packaged_sidecar_available: bool) -> Self {
        match explicit {
            Some("sidecar") => BackendKind::Sidecar,
            Some("legacy") => BackendKind::Legacy,
            // Unset is the interesting case, and it is no longer a constant.
            //
            // **The sidecar is the intended backend** -- it has partial streaming, resume, bounded
            // ingestion and typed permission outcomes, and it is immune to the class of failure
            // that broke legacy's only permission gate on this host (a wrapper owning `--settings`;
            // the sidecar delivers its hook as an in-process SDK callback). What kept legacy the
            // default was never a preference: the sidecar needed a source checkout and Node, which
            // an installed copy does not have.
            //
            // So the default follows what this installation can actually run, and the question is
            // NARROW: is a sidecar artifact here that can be run **without building anything**?
            // `agent::packaged_sidecar_available` answers it from three places -- a named binary,
            // one beside this binary, or one already built in a Verdandi checkout -- and a checkout
            // with nothing built still answers no, because a build is what must never happen behind
            // a backend nobody chose. (That predicate said "a checkout never counts" until
            // 2026-09-18; its own doc records why the object was wrong.)
            //
            // This says WHICH backend and WHY, in one word. It does not say which binary, because
            // it cannot: a `bool` carries no path. `agent`'s own `agent: ...` line at session start
            // names the exact program, which is the more precise answer to the same question.
            // An empty value is "not set", the way a wrapper script's `VAR=` means it.
            None | Some("") => {
                if packaged_sidecar_available {
                    eprintln!(
                        "[agent_backend] using the sidecar backend: a sidecar artifact is available \
                         with nothing to build (the `agent:` line below names which one)"
                    );
                    BackendKind::Sidecar
                } else {
                    // Said every time, not once: this is the line that explains why streaming and
                    // resume are missing, and a reader who does not see it will look for the reason
                    // in the code. It names the way out, because on a development machine there is
                    // one and it is a single command.
                    eprintln!(
                        "[agent_backend] using the legacy backend: no sidecar artifact can be run \
                         without building one -- NEOVIBE_SIDECAR_BINARY is unset, none sits beside \
                         this binary, and no Verdandi checkout has one built. Build it once with \
                         `npm run build:binary -w @verdandi/claude-sidecar` in the checkout, or set \
                         NEOVIBE_AGENT_BACKEND=sidecar to build from source on first start"
                    );
                    BackendKind::Legacy
                }
            }
            Some(other) => {
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
pub struct BackendError {
    pub message: String,
    pub benign: bool,
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
pub enum AgentBackend {
    Legacy(AgentSession),
    Sidecar(Box<AgentConversation>),
}

impl AgentBackend {
    /// Constructs the selected backend. **Blocking, and for the sidecar path potentially for
    /// minutes** (a cold Verdandi checkout runs `npm ci` and `npm run build`), so callers must run
    /// this off the GTK main thread -- see `agent_panel`'s connect worker.
    /// `resume` carries the Claude provider session id to continue, or `None` for a fresh session.
    ///
    /// A resume that cannot be honored fails as a resume. It is never downgraded into a fresh
    /// session anywhere on this path -- the whole Phase 4 validation exists to catch exactly that
    /// silent substitution, and doing it in the host would reintroduce one layer above where the
    /// protocol was fixed.
    pub fn start(
        kind: BackendKind,
        project_dir: &Path,
        mode: PermissionMode,
        resume: Option<&str>,
    ) -> Result<Self, BackendError> {
        match kind {
            BackendKind::Legacy => {
                if resume.is_some() {
                    return Err(BackendError::fatal(
                        "this backend cannot continue a previous session: the legacy Claude backend has \
                         no resume. Start a new session instead."
                            .to_string(),
                    ));
                }
                AgentSession::start(project_dir, mode, agent::disallowed_tools_for(mode))
                    .map(AgentBackend::Legacy)
                    .map_err(|e| BackendError::fatal(format!("failed to start the legacy Claude backend: {e}")))
            }
            BackendKind::Sidecar => {
                let instance_id = uuid::Uuid::new_v4().to_string();
                let provider = ClaudeSidecarProvider::connect(&instance_id).map_err(|e| {
                    // `e` already carries the sidecar's own stderr tail when it failed to start
                    // (agent::providers::claude_sidecar::spawn), which is the only place the real
                    // cause -- an incompatible Claude CLI, say -- exists.
                    BackendError::fatal(format!("failed to connect to the Verdandi sidecar: {e}"))
                })?;
                match resume {
                    Some(provider_session_id) => {
                        AgentConversation::resume(std::sync::Arc::new(provider), project_dir, provider_session_id, mode)
                            .map(|c| AgentBackend::Sidecar(Box::new(c)))
                            .map_err(|e| BackendError::fatal(format!("could not continue the previous session: {e}")))
                    }
                    None => AgentConversation::create(std::sync::Arc::new(provider), project_dir, mode)
                        .map(|c| AgentBackend::Sidecar(Box::new(c)))
                        .map_err(|e| BackendError::fatal(format!("failed to create a Claude session: {e}"))),
                }
            }
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            AgentBackend::Legacy(_) => BackendKind::Legacy,
            AgentBackend::Sidecar(_) => BackendKind::Sidecar,
        }
    }

    /// The reason a session terminated WITHOUT ever having opened, if that is what happened.
    ///
    /// "Opened" means the provider reported a real `SessionOpened` -- which is also where `model`
    /// comes from, so its absence is the signal. A session in a terminal state that never opened
    /// never worked at all, and presenting it as a conversation (empty, marked closed, no
    /// explanation) is indistinguishable to a user from "the agent has nothing to say". The
    /// commonest cause is a resume of a provider session that no longer exists.
    pub fn terminated_before_opening(&self) -> Option<String> {
        let projection = self.projection();
        if projection.model.is_some() {
            return None;
        }
        match &projection.status {
            agent::ProjectionStatus::Closed { reason } | agent::ProjectionStatus::Unavailable { reason } => {
                Some(reason.clone())
            }
            _ => None,
        }
    }

    /// Canonical conversation state, for whichever backend this is.
    ///
    /// Returns a borrow rather than a clone: the panel reads this on every 33ms tick, and the
    /// sidecar path's projection can hold a whole conversation. The sidecar's arrives through the
    /// ingestion thread's lock, the legacy backend's is a plain field -- `ProjectionRef` is what lets
    /// one call site cover both without either paying for the other's shape.
    pub fn projection(&self) -> ProjectionRef<'_> {
        match self {
            AgentBackend::Legacy(session) => ProjectionRef::Borrowed(&session.projection),
            AgentBackend::Sidecar(conversation) => ProjectionRef::Guarded(conversation.projection()),
        }
    }

    /// Neovibe's own conversation identity. `None` for the legacy backend, which has no concept of
    /// one -- it never distinguished its three identities in the first place.
    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Sidecar(conversation) => Some(conversation.conversation_id()),
        }
    }

    /// Verdandi's own session id -- known from `CreateSession`'s reply, before any event arrives.
    ///
    /// The projection only learns it when the first `SessionOpened` is folded, and on the sidecar
    /// backend that does not happen until the first TURN (the Agent SDK emits its system/init per
    /// turn). Reading the projection alone therefore showed an empty session id for the entire
    /// window between "session created" and "first turn sent" -- visible in the panel header as a
    /// dash where the real id already existed.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            AgentBackend::Legacy(session) => session.projection.session_id.as_deref(),
            AgentBackend::Sidecar(conversation) => conversation.session_id(),
        }
    }

    /// The Claude session UUID, once the provider has reported it. Distinct from the projection's
    /// `session_id` (Verdandi's) for the sidecar backend; identical for the legacy one, whose CLI
    /// never separated them.
    pub fn provider_session_id(&self) -> Option<String> {
        match self {
            AgentBackend::Legacy(session) => session.projection.provider_session_id.clone(),
            AgentBackend::Sidecar(conversation) => conversation.provider_session_id(),
        }
    }

    pub fn provider_info(&self) -> Option<&ProviderInfo> {
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
    pub fn capabilities(&self) -> ProviderCapabilities {
        match self {
            AgentBackend::Legacy(_) => LEGACY_CAPABILITIES,
            AgentBackend::Sidecar(conversation) => conversation.capabilities(),
        }
    }

    /// Submits a turn. The returned events are the ones the backend produced SYNCHRONOUSLY, which
    /// is a real difference between the two: the legacy backend returns a synthesized `TurnStarted`
    /// (its wire protocol has no such line to translate), while the sidecar backend returns nothing
    /// because Verdandi emits a real one on the event stream. The panel must not invent an event
    /// for the sidecar path to "even that out" -- server-originated state is the authority.
    pub fn send_turn(&mut self, text: &str) -> Result<Vec<AgentDomainEvent>, BackendError> {
        match self {
            AgentBackend::Legacy(session) => Ok(session.send_turn(text)?),
            AgentBackend::Sidecar(conversation) => {
                conversation.send_turn(text)?;
                Ok(Vec::new())
            }
        }
    }

    pub fn interrupt(&mut self) -> Result<Vec<AgentDomainEvent>, BackendError> {
        match self {
            AgentBackend::Legacy(session) => Ok(session.interrupt()?),
            AgentBackend::Sidecar(conversation) => {
                conversation.interrupt()?;
                Ok(Vec::new())
            }
        }
    }

    /// Answers one pending permission request.
    ///
    /// The two arms return different things for a real reason, not an oversight: the legacy backend
    /// has no provider event for a resolution and hands back the one it folded itself, while the
    /// sidecar returns nothing because its `PermissionResolved` arrives through `pump()` like every
    /// other provider event. Nothing here invents one on the sidecar's behalf.
    pub fn respond_permission(
        &mut self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<Vec<AgentDomainEvent>, BackendError> {
        match self {
            AgentBackend::Legacy(session) => Ok(session.respond_permission(permission_id, decision)?),
            AgentBackend::Sidecar(conversation) => {
                conversation.respond_permission(permission_id, decision)?;
                Ok(Vec::new())
            }
        }
    }

    /// What the UI should apply next.
    ///
    /// The two backends reach this differently and the difference is the point of the sidecar
    /// refactor. The legacy backend still drains its provider here, on the caller's thread -- so a
    /// stalled UI still stalls its reducer. The sidecar path folds continuously on its own thread and
    /// this only collects what is already canonical, which is why a stalled UI there can no longer
    /// make raw events pile up. Legacy is left as it was deliberately: it is the path being retired,
    /// it has no partial streaming, and one message per turn is not a backlog.
    pub fn take_ui_delivery(&mut self) -> UiDelivery {
        match self {
            AgentBackend::Legacy(session) => {
                let events = session.pump();
                if events.is_empty() {
                    UiDelivery::Nothing
                } else {
                    UiDelivery::Events(events)
                }
            }
            AgentBackend::Sidecar(conversation) => conversation.take_ui_delivery(),
        }
    }

    pub fn shutdown(&mut self) {
        match self {
            AgentBackend::Legacy(session) => session.shutdown(),
            AgentBackend::Sidecar(conversation) => conversation.shutdown(),
        }
    }
}

/// Every permission policy this CLIENT can drive end to end, in the order a start screen should
/// offer them.
///
/// The **client half** of the same two-term rule capabilities everywhere else in this codebase obey:
/// effective capability = what the provider advertises, intersected with what this side implements.
/// The start screen has to decide what to offer before any provider exists, so it can only state
/// this half; the provider half is checked for real at session creation, where an unsupported mode
/// is refused outright (`ClaudeSidecarProvider::require_permission_mode`) rather than quietly mapped
/// onto whatever the provider does support.
///
/// It is deliberately NOT keyed on `BackendKind`. Both backends have a real, separately verified
/// interactive gate -- the legacy one through its `PreToolUse` hook relay, the sidecar through a
/// real `PermissionRequested`/`ResolvePermission` round trip (`agent/tests/claude_sidecar_conformance
/// ::real_pretooluse_permission_allow_end_to_end`, which runs in `PermissionMode::Auto`). An earlier
/// revision of this file hardcoded `Sidecar => ["bypass"]`, which hid a working, tested capability
/// behind a backend's name: exactly the failure mode capability advertisement exists to prevent,
/// just pointed inward.
///
/// `verdandi_rules` is absent because it is confirmed to behave identically to `interactive` in the
/// current sidecar. A third button that does nothing different is a worse lie than a missing one.
pub const CLIENT_IMPLEMENTED_PERMISSION_MODES: &[&str] = &["auto", "bypass"];

/// Whether THIS CLIENT can drive a resume for a backend kind, before any provider exists.
///
/// Read off the capability constants the two backends own rather than matched on the backend's
/// name, so the gate cannot drift away from the thing it is standing in for. The legacy value is
/// additionally pinned at compile time beside `LEGACY_CAPABILITIES`; the sidecar's is
/// `agent::CLIENT_IMPLEMENTS_RESUME`, whose own doc explains that it stays false until the call
/// behind it does something real.
///
/// This is only the CLIENT half. The server half -- whether the provider advertises resume -- is
/// the `provider_advertised_resume` flag on each persisted record (last known, since no provider
/// exists yet), and is re-checked live when a resume is actually attempted.
fn client_implements_resume(kind: BackendKind) -> bool {
    match kind {
        BackendKind::Legacy => LEGACY_CAPABILITIES.resume,
        BackendKind::Sidecar => agent::CLIENT_IMPLEMENTS_RESUME,
    }
}

/// What the frontend is told at handshake time, before any session exists.
pub struct BackendGreeting {
    pub kind: BackendKind,
    pub project_dir: PathBuf,
    pub permission_modes: &'static [&'static str],
    pub expected_verdandi_revision: Option<&'static str>,
    /// Every previous conversation in THIS workspace worth offering, newest first. Empty when
    /// there is nothing to continue, which is the normal case for a fresh workspace.
    ///
    /// Non-empty only when all three terms hold: the provider advertised `resume` when the record
    /// was written (server side), this backend kind implements resume (client side), and this
    /// workspace has at least one persisted provider session id.
    ///
    /// The server term is the LAST-KNOWN advertisement rather than a live one, because the start
    /// screen has to decide before any provider exists. It is re-checked for real when resume is
    /// attempted -- `ClaudeSidecarProvider::resume_session` refuses outright if the live handshake
    /// no longer advertises it -- so a stale `true` here produces an honest failure, never a silent
    /// fresh session.
    ///
    /// **What one entry can say is thin, and deliberately not padded.** `agent::ResumableSession`
    /// carries a provider name, a Claude session id and two timestamps -- no title, no first
    /// prompt, no turn count. See its own doc for why the one place on disk that could supply a
    /// subject line (Claude's private transcript) is off limits. A picker built on this offers
    /// "which session" and "when"; it must not invent "what about".
    pub resumable: Vec<ResumableSession>,
}

impl BackendGreeting {
    /// **Does synchronous file I/O, and its caller is the GTK main loop** (`agent_panel`'s
    /// `InboundMessage::Ready` handler). `agent::resumable_sessions` reads one directory and parses
    /// the small JSON records in it; `agent::persistence`'s own retention cap is what keeps that
    /// bounded rather than growing with every session the workspace has ever had. It has not been
    /// moved off the main thread, and if the cap ever rises far it should be.
    pub fn for_kind(kind: BackendKind, project_dir: PathBuf) -> Self {
        // Keyed on the CANONICAL directory, matching what `AgentConversation` persists -- otherwise
        // `/x/proj` and `/x/../x/proj` would look up two different records for one workspace.
        //
        // Gated on the capability rather than on the backend's name: a backend this client cannot
        // drive a resume through offers nothing, however many records the directory holds. That is
        // the whole legacy case today, and it also means the lookup is skipped entirely there.
        let resumable: Vec<ResumableSession> = if client_implements_resume(kind) {
            project_dir
                .canonicalize()
                .ok()
                .map(|cwd| agent::conversation_id_for_cwd(&cwd))
                .map(|id| agent::resumable_sessions(&id))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        match kind {
            BackendKind::Legacy => Self {
                kind,
                project_dir,
                permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
                expected_verdandi_revision: None,
                resumable,
            },
            BackendKind::Sidecar => Self {
                kind,
                project_dir,
                permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
                expected_verdandi_revision: Some(agent::EXPECTED_VERDANDI_REVISION),
                resumable,
            },
        }
    }
}

#[cfg(test)]
mod tests {

    /// The explicit variable wins in both directions, whatever is installed. Someone who names a
    /// backend is testing that backend, and an installation that happens to carry an artifact must
    /// not quietly overrule them.
    #[test]
    fn an_explicit_choice_beats_what_is_installed() {
        for available in [true, false] {
            assert_eq!(BackendKind::choose(Some("sidecar"), available), BackendKind::Sidecar);
            assert_eq!(BackendKind::choose(Some("legacy"), available), BackendKind::Legacy);
        }
    }

    /// The default follows what this installation can actually run. This is the change that makes
    /// the sidecar the real default the moment the artifact ships -- with no further edit here,
    /// which is the point: a constant would have to be flipped by hand in a release nobody
    /// remembers to do it in.
    #[test]
    fn with_nothing_set_the_default_is_whichever_backend_this_installation_can_run() {
        assert_eq!(BackendKind::choose(None, true), BackendKind::Sidecar);
        assert_eq!(BackendKind::choose(None, false), BackendKind::Legacy);
    }

    /// `VAR=` in a wrapper script means "not set", and honouring it literally would take the
    /// unrecognised branch and pin every such launch to legacy however the machine is equipped.
    #[test]
    fn an_empty_variable_is_not_a_choice() {
        assert_eq!(BackendKind::choose(Some(""), true), BackendKind::Sidecar);
        assert_eq!(BackendKind::choose(Some(""), false), BackendKind::Legacy);
    }

    /// A typo must not silently select the newer backend -- that is the dangerous direction, and it
    /// stays legacy even where the sidecar is available.
    #[test]
    fn an_unrecognized_value_falls_back_to_legacy_even_where_the_sidecar_is_available() {
        assert_eq!(BackendKind::choose(Some("sidcar"), true), BackendKind::Legacy);
    }
    use super::*;

    #[test]
    fn backend_kind_round_trips_its_wire_name() {
        assert_eq!(BackendKind::Legacy.as_str(), "legacy");
        assert_eq!(BackendKind::Sidecar.as_str(), "sidecar");
    }

    #[test]
    fn the_sidecar_greeting_names_its_verdandi_baseline() {
        let greeting = BackendGreeting::for_kind(BackendKind::Sidecar, PathBuf::from("/tmp"));
        assert_eq!(greeting.expected_verdandi_revision, Some(agent::EXPECTED_VERDANDI_REVISION));
    }

    #[test]
    fn the_legacy_greeting_has_no_verdandi_baseline_to_name() {
        let greeting = BackendGreeting::for_kind(BackendKind::Legacy, PathBuf::from("/tmp"));
        assert_eq!(greeting.expected_verdandi_revision, None, "the legacy backend has no Verdandi dependency");
    }

    /// The permission offer is a statement about what this CLIENT implements, so it does not vary by
    /// backend name. An earlier revision returned `["bypass"]` for the sidecar, which hid a real,
    /// conformance-tested interactive gate behind a string comparison on the backend's name -- and a
    /// hidden capability is indistinguishable from a missing one to everyone downstream.
    ///
    /// What makes this safe rather than optimistic is the other half of the check: an unsupported
    /// mode is refused at session creation (`ClaudeSidecarProvider::require_permission_mode`) instead
    /// of being mapped onto whatever the provider does support.
    #[test]
    fn both_backends_offer_the_same_permission_policies_because_both_implement_them() {
        for kind in [BackendKind::Legacy, BackendKind::Sidecar] {
            let greeting = BackendGreeting::for_kind(kind, PathBuf::from("/tmp"));
            assert_eq!(
                greeting.permission_modes,
                CLIENT_IMPLEMENTED_PERMISSION_MODES,
                "{} must not narrow the offer by its own name",
                kind.as_str()
            );
        }
    }

    /// Ties the offered list to the capability struct it claims to describe, so adding a mode to one
    /// without the other fails here rather than producing a button whose mode is never honored.
    #[test]
    fn every_offered_mode_is_one_the_legacy_capabilities_actually_claim() {
        for mode in CLIENT_IMPLEMENTED_PERMISSION_MODES {
            let parsed = match *mode {
                "auto" => PermissionMode::Auto,
                "bypass" => PermissionMode::Bypass,
                other => panic!("offered permission mode {other:?} has no PermissionMode to map to"),
            };
            assert!(
                LEGACY_CAPABILITIES.supports_permission_mode(parsed),
                "{mode} is offered but LEGACY_CAPABILITIES does not claim it"
            );
        }
    }

    #[test]
    fn the_legacy_greeting_and_its_capabilities_agree_about_resume() {
        // Not a constant assertion: this crosses two independently-written surfaces -- the
        // capability constant `AgentBackend::capabilities()` returns, and the `hello` envelope's
        // own `resumeAvailable`. They are produced by different code and must not drift apart.
        // (resume/fork being false at all is enforced at compile time next to the constant.)
        //
        // A redirected, empty workspace rather than `/tmp`: `for_kind` really does read the disk
        // when the capability allows it, so against the real `$XDG_STATE_HOME` this assertion would
        // depend on which sessions the developer's own machine happens to remember.
        let greeting = BackendGreeting::for_kind(BackendKind::Legacy, an_empty_workspace());
        let hello: serde_json::Value =
            serde_json::from_str(&crate::agent_bridge::serialize_hello_for_js(&greeting)).unwrap();
        // `resumableSessions` replaced the old boolean `resumeAvailable`: the gate is now "WHICH
        // sessions are there to continue", not "could this backend resume in principle". For legacy
        // the list is always empty, and the two surfaces must agree about that.
        //
        // Asserted as an empty ARRAY rather than as "not present": a `serde_json::Value` index with
        // an unknown key yields `Null`, so an `is_null()` check here would keep passing if the field
        // were renamed or dropped -- which is exactly how the previous version of this line survived
        // the rename that made it meaningless.
        assert_eq!(hello["resumableSessions"], serde_json::json!([]));
        // resume/fork being false is enforced at compile time beside the constant; asserting it
        // again here would be a constant assertion, which is what the previous version of this
        // test was.
        // Cross-checked against the greeting rather than asserted directly: the greeting offers
        // "bypass" as a real choice, and a backend that advertises the mode must support it.
        let modes: Vec<&str> = greeting.permission_modes.to_vec();
        assert_eq!(modes.contains(&"bypass"), LEGACY_CAPABILITIES.bypass_permission_mode);
        assert_eq!(modes, vec!["auto", "bypass"]);
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

    // ---- The resume offer is a list, and it is gated on a capability ---------------------------

    /// A fresh workspace directory with no conversation records at all, under the redirected root.
    fn an_empty_workspace() -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        agent::state_dirs::test_workspace_dir("no-records")
    }

    /// A workspace holding one genuinely offerable record, and the id it is filed under.
    ///
    /// Real records under a real (redirected) state root, written through `agent`'s own writer --
    /// not a stub. `redirect_state_to_a_test_root` is what keeps `cargo test -p shell` out of the
    /// developer's own `$XDG_STATE_HOME`; without it these tests would read whatever real sessions
    /// this machine happens to have, which is a machine-dependent answer, not a test.
    fn a_workspace_with_one_offerable_session() -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("resume-gate");
        let canonical = dir.canonicalize().expect("a just-created directory canonicalizes");
        agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
            conversation_id: agent::conversation_id_for_cwd(&canonical),
            provider: "claude".into(),
            provider_session_id: "prov-offerable".into(),
            canonical_cwd: canonical.to_string_lossy().into_owned(),
            created_at: "1000".into(),
            updated_at: "2000".into(),
            provider_advertised_resume: true,
        })
        .expect("writing a record into the test state root should succeed");
        dir
    }

    /// The gate really is `LEGACY_CAPABILITIES.resume`, observed through what the greeting offers.
    ///
    /// The `assert_eq!` is against the CONSTANT, not against `false`, and the workspace really does
    /// hold an offerable record -- which is what makes this a test rather than a restatement of the
    /// function body. Three ways to break it, all caught:
    ///
    /// - delete the gate in `for_kind`: the record is found, `true != false`, fail;
    /// - keep the gate but write it as `Legacy => false` (the name match this is supposed to
    ///   prevent), then flip `LEGACY_CAPABILITIES.resume` to `true`: nothing is offered,
    ///   `false != true`, fail -- this is the drift the previous version of this test claimed to
    ///   catch and could not, because it compared two values that moved together by construction;
    /// - flip the constant with the gate correctly derived from it: both sides move, pass. A
    ///   legitimate capability change is not a test failure, and this must not become a change
    ///   detector for one.
    ///
    /// The sidecar half is asserted first, and it is load-bearing rather than decorative: without
    /// it "Legacy offers nothing" cannot be told apart from "there was nothing to offer".
    #[test]
    fn the_resume_offer_is_gated_on_the_capability_not_on_the_backend_name() {
        let workspace = a_workspace_with_one_offerable_session();

        let sidecar = BackendGreeting::for_kind(BackendKind::Sidecar, workspace.clone());
        assert_eq!(
            !sidecar.resumable.is_empty(),
            agent::CLIENT_IMPLEMENTS_RESUME,
            "the planted record has to be genuinely offerable, or the legacy half below proves nothing"
        );

        let legacy = BackendGreeting::for_kind(BackendKind::Legacy, workspace);
        assert_eq!(
            !legacy.resumable.is_empty(),
            LEGACY_CAPABILITIES.resume,
            "the same record, the same workspace -- only the capability differs"
        );
    }

    /// A refused resume must fail AS a resume. The legacy backend has none, and the check happens
    /// before anything is spawned, so this runs no `claude` process.
    ///
    /// The picker makes this reachable from more places (any row, not just one), so it is worth
    /// pinning here rather than trusting the call site: a resume that quietly became a fresh session
    /// would hand the user a conversation with none of the history they picked it for.
    #[test]
    fn the_legacy_backend_refuses_a_resume_rather_than_starting_a_fresh_session() {
        let error = AgentBackend::start(
            BackendKind::Legacy,
            Path::new("/tmp"),
            PermissionMode::Bypass,
            Some("claude-abc"),
        )
        .err()
        .expect("the legacy backend must refuse a resume");
        assert!(!error.benign, "a refused resume ends the attempt; it is not an ordering complaint");
        assert!(error.message.contains("cannot continue a previous session"), "got: {}", error.message);
    }
}
