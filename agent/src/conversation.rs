//! `AgentConversation`: one whole conversation, driven by any `AgentProvider`.
//!
//! This is the layer `shell` talks to. It exists because `AgentProvider` is deliberately stateless
//! -- a provider issues commands and drains events, and holds no projection (design doc §10.1) --
//! while a UI needs something it can both command and read. The legacy `AgentSession` already plays
//! that role, but only for the one backend it is welded to.
//!
//! So this type is the provider-agnostic sibling of `AgentSession`, with the same method shape on
//! purpose (`send_turn` / `interrupt` / `respond_permission` / `pump` / `shutdown`, plus a public
//! `projection`) so a caller can switch between them without restructuring.
//!
//! **Three identities, deliberately not collapsed** (handoff §5):
//!
//! | identity              | whose        | where it comes from                               |
//! |-----------------------|--------------|---------------------------------------------------|
//! | `conversation_id`     | Neovibe's    | derived here, stable for a workspace              |
//! | `session_id`          | Verdandi's   | `CreateSessionResponse.session_id`                |
//! | `provider_session_id` | Claude's     | `SessionReady.provider_session_id`, via an event  |
//!
//! They are genuinely different values for the sidecar provider. `provider_session_id` is the one
//! `claude --resume <id>` takes, and it does not exist until the session's first real event
//! arrives -- a session that has been created but has never opened has no provider identity yet.
//!
//! **Not modeled yet, on purpose**: no session lease and no on-disk conversation record. Both exist
//! in this crate (`agent::lease`, `agent::persistence`) and both are keyed on `provider_session_id`.
//! They guard one specific hazard -- two clients driving the SAME provider session -- which cannot
//! arise while every conversation creates a fresh session, because resume does not exist yet. They
//! get wired in by the phase that adds resume, which is also the first phase where they can be
//! tested against the hazard they exist for.

use crate::lease::SessionLease;
use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, ProviderCapabilities,
    ProviderError, ProviderInfo, ResolvePermissionRequest, SendTurnRequest,
};
use crate::projection::{AgentDomainEvent, AgentSessionProjection, PermissionOutcome};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ConversationError {
    /// The underlying provider failed. `ProviderError::is_benign()` says whether the conversation
    /// can continue past it.
    Provider(ProviderError),
    /// A second turn was submitted while one was already active. Rejected here rather than queued,
    /// matching design doc §5.2 and `AgentSession::send_turn`'s identical rule. Benign: the
    /// conversation is fine, the caller simply asked too early.
    TurnAlreadyActive,
    /// A command that needs a live session arrived before one existed (or after it closed).
    NoSession,
    /// `cwd` could not be canonicalized -- it does not exist, or is not readable.
    Cwd(std::io::Error),
}

impl ConversationError {
    /// True for an error the conversation survives. A caller must report these to the user and
    /// carry on; tearing down the session over one would discard a working conversation.
    pub fn is_benign(&self) -> bool {
        match self {
            ConversationError::TurnAlreadyActive => true,
            ConversationError::Provider(e) => e.is_benign(),
            ConversationError::NoSession | ConversationError::Cwd(_) => false,
        }
    }
}

impl std::fmt::Display for ConversationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversationError::Provider(e) => write!(f, "{e}"),
            ConversationError::TurnAlreadyActive => write!(f, "a turn is already in progress on this conversation"),
            ConversationError::NoSession => write!(f, "no active session"),
            ConversationError::Cwd(e) => write!(f, "could not resolve the working directory: {e}"),
        }
    }
}

impl std::error::Error for ConversationError {}

impl From<ProviderError> for ConversationError {
    fn from(e: ProviderError) -> Self {
        ConversationError::Provider(e)
    }
}

/// Neovibe's own stable id for the conversation belonging to a workspace directory.
///
/// Hashed rather than derived from the path text: `persistence::save_conversation_record` writes
/// `<conversation_id>.json`, and a raw path would need escaping to be a filename at all -- the same
/// reasoning `lease.rs` already applies to its own key. 32 hex chars of SHA-256 is far past
/// collision relevance for a per-user directory count, and stays inside `persistence`'s
/// `[A-Za-z0-9_-]` validation.
pub fn conversation_id_for_cwd(canonical_cwd: &Path) -> String {
    let digest = Sha256::digest(canonical_cwd.as_os_str().as_encoded_bytes());
    hex_prefix(&digest, 16)
}

fn hex_prefix(bytes: &[u8], take: usize) -> String {
    bytes.iter().take(take).map(|b| format!("{b:02x}")).collect()
}

pub struct AgentConversation {
    conversation_id: String,
    canonical_cwd: PathBuf,
    /// `+ Send` so a conversation can be CONSTRUCTED on a worker thread and moved to the UI thread
    /// afterwards. That is not a nicety: creating one spawns a real sidecar process and performs a
    /// real handshake, and on a cold Verdandi checkout it also runs `npm ci` and `npm run build` --
    /// minutes of work that must never happen on a GTK main loop.
    provider: Box<dyn AgentProvider + Send>,
    capabilities: ProviderCapabilities,
    info: ProviderInfo,
    /// Verdandi's id for this session. `None` only between construction failure modes -- a
    /// successfully created conversation always has one.
    session_id: Option<String>,
    /// Claude's own session UUID. `None` until the provider's first `SessionOpened` event, which is
    /// why a "continue in a real terminal" action cannot be offered on a conversation that has
    /// never taken a turn.
    provider_session_id: Option<String>,
    /// Reserved for the phase that adds resume; see this module's header. Always `None` today.
    lease: Option<SessionLease>,
    pub projection: AgentSessionProjection,
    event_log: Vec<AgentDomainEvent>,
}

impl AgentConversation {
    /// Creates a fresh provider session for `cwd`.
    ///
    /// `cwd` is canonicalized first, and the canonical form is what both the conversation id and the
    /// provider see -- otherwise `/home/x/proj` and `/home/x/../x/proj` would be two conversations
    /// for one directory, and would later map to two different persisted records.
    pub fn create(
        provider: Box<dyn AgentProvider + Send>,
        cwd: &Path,
        permission_mode: crate::PermissionMode,
    ) -> Result<Self, ConversationError> {
        let canonical_cwd = cwd.canonicalize().map_err(ConversationError::Cwd)?;
        let capabilities = provider.capabilities();
        let info = provider.info();
        let session_id = provider.create_session(CreateSessionRequest {
            cwd: canonical_cwd.to_string_lossy().to_string(),
            permission_mode,
        })?;
        Ok(Self {
            conversation_id: conversation_id_for_cwd(&canonical_cwd),
            canonical_cwd,
            provider,
            capabilities,
            info,
            session_id: Some(session_id),
            provider_session_id: None,
            lease: None,
            projection: AgentSessionProjection::default(),
            event_log: Vec::new(),
        })
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn canonical_cwd(&self) -> &Path {
        &self.canonical_cwd
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The real Claude session UUID, once the provider has reported it. `None` means this
    /// conversation has never opened a provider session -- notably, it means a handoff to
    /// `claude --resume <id>` is not yet possible.
    pub fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id.as_deref()
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    pub fn provider_info(&self) -> &ProviderInfo {
        &self.info
    }

    pub fn event_log(&self) -> &[AgentDomainEvent] {
        &self.event_log
    }

    /// Submits one user turn.
    ///
    /// Unlike `AgentSession::send_turn`, this does NOT synthesize a `TurnStarted` event. A real
    /// provider emits its own (the sidecar's kernel pushes `turn_started` on the event stream), so
    /// synthesizing one here would fold two `TurnStarted`s for a single turn and give the caller two
    /// different turn ids for it. The legacy backend synthesizes only because the Claude CLI's wire
    /// protocol has no such line to translate.
    ///
    /// The consequence is that `projection.active_turn_id` is not set the instant this returns --
    /// it is set when the provider's event arrives. The local guard below therefore cannot be the
    /// only defense against a double send, and it is not: the provider rejects a concurrent turn
    /// too, with a typed `TurnAlreadyActive`. Both are real; neither is optimistic shadow state.
    pub fn send_turn(&mut self, text: &str) -> Result<String, ConversationError> {
        if self.projection.active_turn_id.is_some() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        let session_id = self.require_session()?;
        self.provider
            .send_turn(SendTurnRequest { session_id, text: text.to_string() })
            .map_err(|e| match e {
                ProviderError::Provider { code: crate::ProviderErrorCode::TurnAlreadyActive, .. } => {
                    ConversationError::TurnAlreadyActive
                }
                other => ConversationError::Provider(other),
            })
    }

    /// Cancels the in-flight turn. Does not end the session -- a real interrupt is followed by a
    /// real `TurnCompleted { outcome: Interrupted }` from the provider, and the conversation stays
    /// usable (pinned by
    /// `claude_sidecar_lifecycle_conformance::real_interrupt_cancels_the_turn_and_the_session_still_accepts_another`).
    ///
    /// No `PermissionResolved` events are synthesized here either, for the same reason as
    /// `send_turn`: the provider fail-closes its own pending permissions and reports each with a
    /// real outcome. `AgentSession::interrupt` synthesizes because its backend cannot.
    pub fn interrupt(&mut self) -> Result<(), ConversationError> {
        if !self.capabilities.interrupt {
            return Err(ConversationError::Provider(ProviderError::UnsupportedCapability("interrupt")));
        }
        let session_id = self.require_session()?;
        self.provider.interrupt_turn(InterruptTurnRequest { session_id })?;
        Ok(())
    }

    /// Answers one pending permission request. Rejects an id the projection does not currently list
    /// as pending, so an already-answered card cannot produce a second decision.
    pub fn respond_permission(
        &mut self,
        permission_id: &str,
        allow: bool,
        reason: Option<&str>,
    ) -> Result<(), ConversationError> {
        if !self.projection.pending_permissions.contains_key(permission_id) {
            return Err(ConversationError::Provider(ProviderError::Provider {
                code: crate::ProviderErrorCode::PermissionNotFound,
                message: format!("no pending permission request with id {permission_id}"),
            }));
        }
        let session_id = self.require_session()?;
        self.provider.resolve_permission(ResolvePermissionRequest {
            session_id,
            permission_id: permission_id.to_string(),
            allow,
            reason: reason.map(|r| r.to_string()),
        })?;
        Ok(())
    }

    /// Drains whatever the provider has produced since the last call, folding each event into the
    /// projection (so the next event in this same batch sees current state) and into the event log.
    ///
    /// This is also where `provider_session_id` is learned: it arrives on `SessionOpened` and
    /// nowhere else.
    pub fn pump(&mut self) -> Vec<AgentDomainEvent> {
        let events = self.provider.pump();
        for event in &events {
            if let AgentDomainEvent::SessionOpened { provider_session_id, .. } = event {
                self.provider_session_id = Some(provider_session_id.clone());
            }
            self.projection.apply(event);
            self.event_log.push(event.clone());
        }
        events
    }

    /// Closes the provider session and folds the terminal state.
    ///
    /// Fail-closed on the way out, mirroring `AgentSession::shutdown`: any permission still pending
    /// is resolved as `CancelledBySessionClose` locally, because a request belonging to a session
    /// that is ending can never be genuinely answered and the provider's own terminal events may not
    /// be drained before this process goes away. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(session_id) = self.session_id.take() {
            if let Err(e) = self.provider.close_session(CloseSessionRequest { session_id }) {
                eprintln!("agent: AgentConversation::shutdown: close_session failed: {e}");
            }
        }
        let pending: Vec<String> = self.projection.pending_permissions.keys().cloned().collect();
        for permission_id in pending {
            self.fold(AgentDomainEvent::PermissionResolved {
                permission_id,
                outcome: PermissionOutcome::CancelledBySessionClose,
            });
        }
        if !matches!(self.projection.status, crate::ProjectionStatus::Closed { .. }) {
            self.fold(AgentDomainEvent::SessionClosed { reason: "closed_by_host".to_string() });
        }
        // Released last: the lease must outlive the provider's own session teardown, so no other
        // client can acquire it while this one is still closing. (Always `None` today -- see this
        // module's header.)
        self.lease = None;
    }

    fn fold(&mut self, event: AgentDomainEvent) {
        self.projection.apply(&event);
        self.event_log.push(event);
    }

    fn require_session(&self) -> Result<String, ConversationError> {
        self.session_id.clone().ok_or(ConversationError::NoSession)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ResumeSessionRequest;
    use crate::{ContentKind, PermissionMode, ProviderErrorCode, TurnOutcome};
    use std::sync::{Arc, Mutex};

    /// A provider that records what it was asked to do and replays scripted events. No process, no
    /// network, no cost -- everything `AgentConversation` itself decides is testable here.
    /// `Mutex`, not `RefCell`: `AgentConversation` now requires `Box<dyn AgentProvider + Send>` so
    /// a real provider can be constructed off the UI thread, and the fake has to satisfy the same
    /// bound. Every method still takes `&self`, so the lock is never held across a call.
    #[derive(Default)]
    struct FakeProvider {
        capabilities: ProviderCapabilities,
        calls: Mutex<Vec<String>>,
        queued_events: Mutex<Vec<AgentDomainEvent>>,
        send_turn_error: Mutex<Option<ProviderError>>,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self {
                capabilities: ProviderCapabilities {
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                },
                ..Default::default()
            }
        }
        fn queue(&self, event: AgentDomainEvent) {
            self.queued_events.lock().unwrap().push(event);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl AgentProvider for FakeProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            self.capabilities
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo { sidecar_version: "fake".into(), protocol_major: 1, ..Default::default() }
        }
        fn create_session(&self, request: CreateSessionRequest) -> Result<String, ProviderError> {
            self.calls.lock().unwrap().push(format!("create_session(cwd={})", request.cwd));
            Ok("verdandi-session-1".into())
        }
        fn resume_session(&self, _r: ResumeSessionRequest) -> Result<String, ProviderError> {
            Err(ProviderError::UnsupportedCapability("resume"))
        }
        fn send_turn(&self, request: SendTurnRequest) -> Result<String, ProviderError> {
            self.calls.lock().unwrap().push(format!("send_turn({})", request.text));
            if let Some(e) = self.send_turn_error.lock().unwrap().take() {
                return Err(e);
            }
            Ok("turn-1".into())
        }
        fn interrupt_turn(&self, _r: InterruptTurnRequest) -> Result<(), ProviderError> {
            self.calls.lock().unwrap().push("interrupt_turn".into());
            Ok(())
        }
        fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError> {
            self.calls.lock().unwrap().push(format!("resolve_permission({}, allow={})", request.permission_id, request.allow));
            Ok(())
        }
        fn close_session(&self, _r: CloseSessionRequest) -> Result<(), ProviderError> {
            self.calls.lock().unwrap().push("close_session".into());
            Ok(())
        }
        fn pump(&self) -> Vec<AgentDomainEvent> {
            std::mem::take(&mut *self.queued_events.lock().unwrap())
        }
    }

    /// `Box<dyn AgentProvider>` consumes the fake, so tests that need to inspect it afterward keep
    /// a second handle. `AgentProvider`'s methods all take `&self`, so an `Rc` is enough.
    fn conversation_with(fake: Arc<FakeProvider>) -> AgentConversation {
        struct Shared(Arc<FakeProvider>);
        impl AgentProvider for Shared {
            fn capabilities(&self) -> ProviderCapabilities { self.0.capabilities() }
            fn info(&self) -> ProviderInfo { self.0.info() }
            fn create_session(&self, r: CreateSessionRequest) -> Result<String, ProviderError> { self.0.create_session(r) }
            fn resume_session(&self, r: ResumeSessionRequest) -> Result<String, ProviderError> { self.0.resume_session(r) }
            fn send_turn(&self, r: SendTurnRequest) -> Result<String, ProviderError> { self.0.send_turn(r) }
            fn interrupt_turn(&self, r: InterruptTurnRequest) -> Result<(), ProviderError> { self.0.interrupt_turn(r) }
            fn resolve_permission(&self, r: ResolvePermissionRequest) -> Result<(), ProviderError> { self.0.resolve_permission(r) }
            fn close_session(&self, r: CloseSessionRequest) -> Result<(), ProviderError> { self.0.close_session(r) }
            fn pump(&self) -> Vec<AgentDomainEvent> { self.0.pump() }
        }
        // std::env::temp_dir() is guaranteed to exist and canonicalize.
        AgentConversation::create(Box::new(Shared(fake)), &std::env::temp_dir(), PermissionMode::Bypass)
            .expect("create should succeed against the fake provider")
    }

    fn session_opened() -> AgentDomainEvent {
        AgentDomainEvent::SessionOpened {
            session_id: "verdandi-session-1".into(),
            provider_session_id: "claude-uuid-abc".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/tmp".into(),
        }
    }

    #[test]
    fn conversation_id_is_stable_for_a_directory_and_differs_between_directories() {
        let a = conversation_id_for_cwd(Path::new("/home/x/proj"));
        assert_eq!(a, conversation_id_for_cwd(Path::new("/home/x/proj")));
        assert_ne!(a, conversation_id_for_cwd(Path::new("/home/x/other")));
    }

    #[test]
    fn conversation_id_is_a_valid_persistence_key() {
        // persistence::save_conversation_record rejects anything outside [A-Za-z0-9_-], and it
        // joins the id straight into a filename. A path-shaped id would escape its directory.
        let id = conversation_id_for_cwd(Path::new("/home/x/../x/a b/c:d"));
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "got: {id}");
    }

    #[test]
    fn create_canonicalizes_cwd_before_deriving_the_id_or_calling_the_provider() {
        let fake = Arc::new(FakeProvider::new());
        let conversation = conversation_with(fake.clone());
        let canonical = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(conversation.canonical_cwd(), canonical.as_path());
        assert_eq!(conversation.conversation_id(), conversation_id_for_cwd(&canonical));
        assert_eq!(fake.calls(), vec![format!("create_session(cwd={})", canonical.to_string_lossy())]);
    }

    #[test]
    fn create_fails_cleanly_on_a_cwd_that_does_not_exist() {
        struct Never;
        impl AgentProvider for Never {
            fn capabilities(&self) -> ProviderCapabilities { ProviderCapabilities::default() }
            fn info(&self) -> ProviderInfo { ProviderInfo::default() }
            fn create_session(&self, _r: CreateSessionRequest) -> Result<String, ProviderError> {
                panic!("create_session must not be reached when cwd cannot be resolved")
            }
            fn resume_session(&self, _r: ResumeSessionRequest) -> Result<String, ProviderError> { unreachable!() }
            fn send_turn(&self, _r: SendTurnRequest) -> Result<String, ProviderError> { unreachable!() }
            fn interrupt_turn(&self, _r: InterruptTurnRequest) -> Result<(), ProviderError> { unreachable!() }
            fn resolve_permission(&self, _r: ResolvePermissionRequest) -> Result<(), ProviderError> { unreachable!() }
            fn close_session(&self, _r: CloseSessionRequest) -> Result<(), ProviderError> { unreachable!() }
            fn pump(&self) -> Vec<AgentDomainEvent> { vec![] }
        }
        let result = AgentConversation::create(
            Box::new(Never),
            Path::new("/definitely/does/not/exist/neovibe-test"),
            PermissionMode::Bypass,
        );
        // `AgentConversation` is deliberately not Debug (it owns a Box<dyn AgentProvider>), so
        // match the error out rather than formatting the whole Result.
        match result {
            Err(ConversationError::Cwd(_)) => {}
            Err(other) => panic!("expected a Cwd error, got: {other:?}"),
            Ok(_) => panic!("expected create to fail on a nonexistent cwd"),
        }
    }

    #[test]
    fn the_three_identities_stay_distinct() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());

        // Before any event: Verdandi's id is known, Claude's is not.
        assert_eq!(conversation.session_id(), Some("verdandi-session-1"));
        assert_eq!(conversation.provider_session_id(), None, "a created-but-never-opened session has no Claude identity");

        fake.queue(session_opened());
        conversation.pump();

        assert_eq!(conversation.session_id(), Some("verdandi-session-1"));
        assert_eq!(conversation.provider_session_id(), Some("claude-uuid-abc"));
        assert_ne!(conversation.conversation_id(), "verdandi-session-1");
        assert_ne!(conversation.conversation_id(), "claude-uuid-abc");
    }

    #[test]
    fn pump_folds_into_the_projection_and_the_event_log_together() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        fake.queue(AgentDomainEvent::ContentDelta { turn_id: "t1".into(), kind: ContentKind::Text, text: "hi".into() });

        let drained = conversation.pump();

        assert_eq!(drained.len(), 3);
        assert_eq!(conversation.event_log().len(), 3);
        assert_eq!(conversation.projection.last_revision, 3);
        assert_eq!(conversation.projection.transcript, vec!["hi".to_string()]);
        assert_eq!(conversation.projection.active_turn_id.as_deref(), Some("t1"));
        assert!(conversation.pump().is_empty(), "a second drain with nothing new returns nothing");
    }

    #[test]
    fn send_turn_does_not_synthesize_a_turn_started_event() {
        // The regression this pins: copying AgentSession's behavior here would fold a second,
        // locally-invented TurnStarted for a turn the provider also reports, giving one turn two
        // ids and two revisions.
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        conversation.pump();
        let revision_before = conversation.projection.last_revision;

        let turn_id = conversation.send_turn("hello").unwrap();

        assert_eq!(turn_id, "turn-1");
        assert_eq!(conversation.projection.last_revision, revision_before, "send_turn must fold nothing");
        assert_eq!(conversation.projection.active_turn_id, None, "state comes from the provider's own event");
        assert_eq!(conversation.event_log().len(), 1, "only the SessionOpened folded earlier");
    }

    #[test]
    fn a_second_turn_while_one_is_active_is_rejected_locally_and_never_reaches_the_provider() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        conversation.pump();

        let result = conversation.send_turn("second");
        assert!(matches!(result, Err(ConversationError::TurnAlreadyActive)), "got: {result:?}");
        assert!(result.unwrap_err().is_benign(), "an early second send must not tear the session down");
        assert!(!fake.calls().iter().any(|c| c.starts_with("send_turn")), "got: {:?}", fake.calls());
    }

    #[test]
    fn a_providers_own_turn_already_active_maps_to_the_same_benign_error() {
        // The local guard cannot catch a double send that races the provider's TurnStarted event,
        // so the provider's own typed rejection must land as the same benign error, not as a
        // generic provider failure a caller would treat as fatal.
        let fake = Arc::new(FakeProvider::new());
        *fake.send_turn_error.lock().unwrap() = Some(ProviderError::Provider {
            code: ProviderErrorCode::TurnAlreadyActive,
            message: "a turn is already in progress on this session".into(),
        });
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        conversation.pump();

        let result = conversation.send_turn("racing");
        assert!(matches!(result, Err(ConversationError::TurnAlreadyActive)), "got: {result:?}");
    }

    #[test]
    fn a_fatal_provider_error_stays_fatal() {
        let fake = Arc::new(FakeProvider::new());
        *fake.send_turn_error.lock().unwrap() = Some(ProviderError::Provider {
            code: ProviderErrorCode::SessionNotFound,
            message: "gone".into(),
        });
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        conversation.pump();

        let error = conversation.send_turn("hello").unwrap_err();
        assert!(!error.is_benign(), "got: {error:?}");
    }

    #[test]
    fn interrupt_is_refused_when_the_provider_does_not_advertise_it() {
        let fake = Arc::new(FakeProvider {
            capabilities: ProviderCapabilities { interrupt: false, ..ProviderCapabilities::default() },
            ..Default::default()
        });
        let mut conversation = conversation_with(fake.clone());
        let result = conversation.interrupt();
        assert!(
            matches!(result, Err(ConversationError::Provider(ProviderError::UnsupportedCapability("interrupt")))),
            "got: {result:?}"
        );
        assert!(!fake.calls().iter().any(|c| c == "interrupt_turn"), "got: {:?}", fake.calls());
    }

    #[test]
    fn interrupt_folds_nothing_locally() {
        // Same reasoning as send_turn: the provider emits the real TurnCompleted{Interrupted} and
        // the real PermissionResolved outcomes. Synthesizing them here would produce two terminal
        // events for one turn.
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        conversation.pump();
        let revision_before = conversation.projection.last_revision;

        conversation.interrupt().unwrap();

        assert_eq!(conversation.projection.last_revision, revision_before);
        assert!(fake.calls().iter().any(|c| c == "interrupt_turn"));

        // The provider's own terminal event is what moves state.
        fake.queue(AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Interrupted,
            result_text: String::new(),
            stop_reason: None,
            total_cost_usd: 0.0,
            num_turns: 0,
        });
        conversation.pump();
        assert_eq!(conversation.projection.active_turn_id, None);
    }

    #[test]
    fn responding_to_an_unknown_permission_is_rejected_without_reaching_the_provider() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        conversation.pump();

        let result = conversation.respond_permission("never-existed", true, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_benign(), "an already-answered or unknown id is a logged no-op, not fatal");
        assert!(!fake.calls().iter().any(|c| c.starts_with("resolve_permission")), "got: {:?}", fake.calls());
    }

    #[test]
    fn responding_to_a_real_pending_permission_reaches_the_provider() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "p1".into(),
            tool_use_id: Some("tu1".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({}),
        });
        conversation.pump();

        conversation.respond_permission("p1", true, None).unwrap();
        assert!(fake.calls().iter().any(|c| c == "resolve_permission(p1, allow=true)"), "got: {:?}", fake.calls());
    }

    #[test]
    fn shutdown_closes_the_session_fail_closes_pending_permissions_and_is_idempotent() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "p1".into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({}),
        });
        conversation.pump();
        assert_eq!(conversation.projection.pending_permissions.len(), 1);

        conversation.shutdown();

        assert!(conversation.projection.pending_permissions.is_empty(), "a closing session can never answer them");
        assert!(matches!(conversation.projection.status, crate::ProjectionStatus::Closed { .. }));
        assert_eq!(fake.calls().iter().filter(|c| *c == "close_session").count(), 1);

        let revision_after_first = conversation.projection.last_revision;
        conversation.shutdown();
        assert_eq!(fake.calls().iter().filter(|c| *c == "close_session").count(), 1, "close_session must not repeat");
        assert_eq!(conversation.projection.last_revision, revision_after_first, "a repeat shutdown folds nothing");
    }
}
