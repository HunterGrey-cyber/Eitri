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
    AgentConversation, AgentDomainEvent, AgentSession, AgentSessionProjection, ClaudeSidecarProvider,
    ConversationError, PermissionDecision, PermissionMode, ProjectionGuard, ProviderCapabilities, ProviderInfo,
    ResumableSession, UiDelivery,
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
    assert!(
        !LEGACY_CAPABILITIES.resume,
        "resume must not be advertised before it works end to end"
    );
    assert!(
        !LEGACY_CAPABILITIES.fork,
        "fork must not be advertised before it works end to end"
    );
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
#[derive(Debug)]
pub struct BackendError {
    pub message: String,
    pub benign: bool,
    /// Events the backend had ALREADY folded into its own projection before this command failed,
    /// and which the caller therefore still owes the UI.
    ///
    /// Not bookkeeping: it is what keeps the live panel and a reloaded one telling the same story.
    /// `send_turn` folds the user's prompt BEFORE the send is attempted (see its own doc for the
    /// ordering race that forces that), so a REFUSED send still leaves the prompt in the
    /// projection -- and therefore in the next snapshot. The legacy arm used to build
    /// `[UserPromptSubmitted]` and then throw the vec away on `?`, so the user saw "that message
    /// was not sent" and NO prompt row, and then `Ctrl+Shift+R` -- a supported, separately verified
    /// workflow -- resynced from the snapshot and the row appeared, reading as a message that had
    /// been sent. Snapshot-vs-live indistinguishability is the property this whole panel rests on.
    ///
    /// The sidecar arm never had the defect and does not use this field: its `fold_locally` queues
    /// to the ingestion pump, which delivers regardless of what the send returns. This carries the
    /// legacy arm's synchronous events, which have no other route out.
    ///
    /// Empty for every error that folded nothing, which is all of them but that one.
    pub folded_events: Vec<AgentDomainEvent>,
}

impl BackendError {
    fn fatal(message: String) -> Self {
        Self {
            message,
            benign: false,
            folded_events: Vec::new(),
        }
    }

    /// Attaches events the projection has already taken, so the caller can deliver them even though
    /// the command failed. See `folded_events`.
    fn with_folded_events(mut self, events: Vec<AgentDomainEvent>) -> Self {
        self.folded_events = events;
        self
    }
}

impl From<ConversationError> for BackendError {
    fn from(error: ConversationError) -> Self {
        Self {
            benign: error.is_benign(),
            message: error.to_string(),
            folded_events: Vec::new(),
        }
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
        Self {
            benign,
            message: error.to_string(),
            folded_events: Vec::new(),
        }
    }
}

/// The event a submitted prompt becomes. Free function so `send_turn` and its test build it the
/// same way.
fn user_prompt_event(as_typed: &str) -> AgentDomainEvent {
    AgentDomainEvent::UserPromptSubmitted {
        text: as_typed.to_string(),
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
    ///
    /// Two strings because they are two different things. `wire_text` is what goes to the model --
    /// the user's text with wire 1's editor context composed in (`shell/src/agent_panel.rs`, the
    /// one composition point). `as_typed` is what the user wrote, which is what a reader of the
    /// conversation is owed.
    ///
    /// **The prompt is folded BEFORE the backend's send is even attempted, on both arms -- not
    /// after acceptance, which is what an earlier revision of this comment argued for and got
    /// wrong.** `AgentConversation::send_turn` returns as soon as the server ACKs the unary RPC;
    /// `active_turn_id` is set later, when the provider's own event arrives on the ingestion
    /// thread (see that method's own doc). Folding the prompt AFTER that ack raced this thread's
    /// `fold_locally` against the ingestion thread's `IngestState::fold` for the SAME mutex, with
    /// no ordering guarantee -- if the server's `TurnStarted`/first `ContentDelta` won that race,
    /// the reply got a lower `seq` than the prompt that caused it, inverting the panel's display
    /// order for that turn. Folding first closes the race by construction rather than by timing:
    /// before the RPC is even sent, the ingestion thread has nothing of this turn's to fold yet.
    ///
    /// The accepted cost: a rejected send (no session, a turn already active) now leaves the
    /// prompt recorded even though its turn never happened. That is deliberate, and it is the
    /// better failure -- the user genuinely typed it, and the caller's own error reporting
    /// accompanies the row, whereas racing the ordering the way the previous revision did was a
    /// silent, undetectable corruption of the transcript.
    ///
    /// **Both arms pay that cost in the same currency, which took a second fix.** A rejection's
    /// events go out on `BackendError::folded_events` rather than being dropped by `?`: the legacy
    /// arm's prompt is returned synchronously and has no other route to the UI, so discarding it
    /// left the panel showing no prompt row LIVE while the projection -- and therefore the next
    /// snapshot, one `Ctrl+Shift+R` away -- held one. The sidecar arm never needed it (its
    /// `fold_locally` queues to the ingestion pump regardless of the send), and that asymmetry is
    /// exactly what made the legacy hole easy to miss.
    pub fn send_turn(&mut self, wire_text: &str, as_typed: &str) -> Result<Vec<AgentDomainEvent>, BackendError> {
        match self {
            AgentBackend::Legacy(session) => {
                let prompt = session.fold_locally(user_prompt_event(as_typed));
                let mut events = vec![prompt];
                match session.send_turn(wire_text) {
                    Ok(from_send) => {
                        events.extend(from_send);
                        Ok(events)
                    }
                    // NOT `?`. The prompt is already in the projection; dropping it here is what
                    // made live and post-reload disagree.
                    Err(error) => Err(BackendError::from(error).with_folded_events(events)),
                }
            }
            AgentBackend::Sidecar(conversation) => {
                // Reaches the UI through the pump like every other event, because `fold_locally`
                // queues it -- and it does so regardless of whether `send_turn` below succeeds,
                // which is exactly what makes a rejected turn still show its prompt.
                conversation.fold_locally(user_prompt_event(as_typed));
                conversation.send_turn(wire_text)?;
                // The resume picker's title for this session: the prompt as typed, never
                // `wire_text`, which carries the editor context composed above it. Only a turn the
                // provider accepted names a session. Legacy writes no records, so it has nothing to
                // name.
                conversation.note_title(as_typed);
                Ok(Vec::new())
            }
        }
    }

    /// A session tab's rename (spec §3.5). Legacy writes no records, so its name lives only in the
    /// tab (`crate::tab_set`).
    pub fn note_name(&mut self, name: Option<String>) {
        if let AgentBackend::Sidecar(conversation) = self {
            conversation.note_name(name);
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
    /// **This is also where a tool call that needs no human is answered and never becomes a card**
    /// -- see `answer_what_needs_no_human`. `project_root` is the session's canonical project
    /// directory, which is the boundary `Read`/`Grep`/`Glob` are judged against; it is a parameter
    /// rather than state on this enum because `AgentBackend` is an enum its own tests construct
    /// variant-by-variant, and a caller that forgets it does not compile.
    pub fn take_ui_delivery(&mut self, project_root: &Path) -> UiDelivery {
        self.take_ui_delivery_with_rules(project_root, &agent::PrefixRules::default())
    }

    /// `take_ui_delivery`, with the project's D7 prefix rules (`agent::permission_rules`) applied by
    /// the classifier. A rule can only replace the two "not on the read-only list" verdicts, never a
    /// fail-closed one; see `agent::classify_with_rules`.
    pub fn take_ui_delivery_with_rules(&mut self, project_root: &Path, rules: &agent::PrefixRules) -> UiDelivery {
        let delivery = match self {
            AgentBackend::Legacy(session) => {
                let events = session.pump();
                if events.is_empty() {
                    UiDelivery::Nothing
                } else {
                    UiDelivery::Events(events)
                }
            }
            AgentBackend::Sidecar(conversation) => conversation.take_ui_delivery(),
        };
        match delivery {
            UiDelivery::Events(events) => {
                let kept = self.answer_what_needs_no_human(events, project_root, rules);
                if kept.is_empty() {
                    UiDelivery::Nothing
                } else {
                    UiDelivery::Events(kept)
                }
            }
            // A `Resync` carries no events to filter: the UI is about to rebuild from the
            // projection instead. See `answer_what_needs_no_human`'s own note on the window that
            // leaves open.
            other => other,
        }
    }

    /// Answers every `PermissionRequested` in this batch that `agent::permission_policy` says needs
    /// no human, and returns the events the UI should still see.
    ///
    /// **This is the single point both backends converge on, and that is why the fix lives here.**
    /// Both install a `PreToolUse` gate whose matcher is `*` -- legacy generates the hook, the
    /// sidecar gets one from Verdandi's own broker -- and both are right to: "the host sees every
    /// call" is the correct layering, because the policy of what needs a human belongs to the
    /// product that has the human. What was wrong was equating "the host was asked" with "the user
    /// must answer", which is what put a card in front of the owner for every single `Read`. Fixing
    /// it here needs no change in Verdandi and no change on any wire.
    ///
    /// An auto-answered request is dropped from the delivery entirely, so the frontend never learns
    /// it existed and no card is drawn. The tool call itself still arrives as
    /// `ToolCallStarted`/`ToolCallCompleted`, so the transcript still shows what ran -- this
    /// suppresses the question, never the record.
    ///
    /// **If the answer fails to send, the event is delivered after all.** A request left pending in
    /// the projection with no card on screen is a session the user cannot unstick, which is a worse
    /// failure than an extra click.
    ///
    /// Two honest gaps, neither of which any test here covers:
    ///
    /// - The resolution is dropped too, but only the legacy backend's, which is returned
    ///   synchronously. The sidecar's `PermissionResolved` arrives on a later pump and IS delivered,
    ///   for a `permission_id` the frontend never saw; its reducer filters `pendingPermissions` by
    ///   id, so that is a no-op there rather than an error.
    /// - On the sidecar path the projection keeps the request pending until that resolution
    ///   arrives. A `UiDelivery::Resync` landing inside that window would rebuild the frontend from
    ///   a snapshot that still holds the card, which would then vanish on the next pump. A resync
    ///   needs 256 queued events to happen at all, so this has never been observed; it is written
    ///   down because it is reachable, not because it was seen.
    fn answer_what_needs_no_human(
        &mut self,
        events: Vec<AgentDomainEvent>,
        project_root: &Path,
        rules: &agent::PrefixRules,
    ) -> Vec<AgentDomainEvent> {
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            let AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_name,
                input,
                ..
            } = &event
            else {
                kept.push(event);
                continue;
            };
            let classification = agent::classify_with_rules(tool_name, input, project_root, rules);
            if classification.needs_a_human() {
                // The other half of the line below, added 2026-09-19 (later) because its absence
                // cost a diagnosis: the owner reported auto mode still being a wall of popups, and
                // nothing anywhere recorded WHY any particular card was drawn, so the cause --
                // `ToolSearch` falling off the end of the policy's table, on a build whose tools are
                // deferred so the model calls it before everything -- had to be reconstructed by
                // reading the classifier against the CLI's own `system/init`. One line per card,
                // with the policy's own fixed reason, never anything the model wrote.
                eprintln!("[permission] asking the user: {tool_name} ({})", classification.reason);
                kept.push(event);
                continue;
            }
            // Cloned before the mutable borrow below, not for tidiness: `event` borrows from the
            // same value `respond_permission` needs `&mut self` for.
            let (permission_id, tool_name) = (permission_id.clone(), tool_name.clone());
            match self.respond_permission(&permission_id, PermissionDecision::Allow) {
                Ok(_resolution) => {
                    // Said on stderr rather than silently: this is the only record anywhere of a
                    // tool call that ran without the user being asked. One short line, whose reason
                    // is a fixed label from the policy rather than anything the model wrote.
                    eprintln!(
                        "[permission] allowed without asking: {tool_name} ({})",
                        classification.reason
                    );
                }
                Err(error) => {
                    eprintln!(
                        "[permission] could not auto-answer {tool_name}, showing a card instead: {}",
                        error.message
                    );
                    kept.push(event);
                }
            }
        }
        kept
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

/// Whether a session's permission mode can change after it starts (D6, keymap/tabs spec §4.1).
/// `false` until Verdandi ships a `SetPermissionMode` RPC and a handshake capability for it (filed in
/// Verdandi's own notes); the pinned protocol (`28a5e4c`) has neither. The
/// panel reads it as `capabilities.modeSwitch` and hides every mode control while it is `false`.
/// There is deliberately no reconnect or resume-in-another-mode fallback.
pub const MODE_SWITCH_AVAILABLE: bool = false;

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

/// One picker row's title: the CLI's own, then Neovibe's own, then none (design §3.1.3's
/// Amendment ②, the owner's ruling of 2026-09-20).
///
/// The layering is **display only**. `session` is a value on its way to the frontend; nothing here
/// writes, so `ConversationRecord::title` keeps its first-one-wins rule, `updated_at` stays what it
/// means, and the question "what if the CLI changes its title later" never has to be answered
/// (`agent::transcript`'s constraint 3). Each greeting re-reads, so a resume refreshes the row.
///
/// **A row never invents a title.** A transcript that cannot be found, cannot be read, holds no
/// `ai-title` within `AI_TITLE_TAIL_BYTES`, or holds one this code cannot render, all leave
/// `session.title` exactly as it was -- which is the row as it looked before this existed. That
/// degradation is silent by design and is pinned by a test rather than by this comment
/// (`with_no_ai_title_a_row_reads_exactly_as_it_did_before`).
fn title_for(canonical_cwd: &str, mut session: ResumableSession) -> ResumableSession {
    if let Some(title) = agent::transcript::transcript_path(canonical_cwd, &session.provider_session_id)
        .ok()
        .and_then(|path| agent::transcript::newest_ai_title(&path))
    {
        session.title = Some(title);
    }
    session
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
    ///
    /// **Correction (2026-09-20, the owner's ruling): that transcript is no longer off limits for a
    /// title.** `for_kind` below layers the CLI's own `type:"ai-title"` line over `title` at
    /// DISPLAY time (`agent::transcript::newest_ai_title`), read fresh on every greeting and never
    /// written to disk -- see `agent::transcript`'s module doc, constraint 3, and
    /// `agent::ResumableSession`'s own doc, both corrected in the same commit. The sentence this
    /// correction sits under is otherwise unchanged, and its last clause is the load-bearing half:
    /// each level of the ladder is a real title from a real source, and a row with none still shows
    /// its id and its timestamps rather than an invented label.
    pub resumable: Vec<ResumableSession>,
}

impl BackendGreeting {
    /// **Does synchronous file I/O, and its caller is the GTK main loop** (`agent_panel`'s
    /// `InboundMessage::Ready` handler). `agent::resumable_sessions` reads one directory and parses
    /// the small JSON records in it; `agent::persistence`'s own retention cap is what keeps that
    /// bounded rather than growing with every session the workspace has ever had. It has not been
    /// moved off the main thread, and if the cap ever rises far it should be.
    ///
    /// **Since 2026-09-20 it also reads a fixed 64 KiB tail of each row's Claude transcript**, for
    /// the display title (`title_for`). That is one more bounded read per row, and the row count is
    /// what `agent::persistence` already bounds (`MAX_RECORDS_PER_CONVERSATION` + 1, so 17 rather
    /// than 16 -- that doc explains the extra one). Measured at 1.74 ms hot and 5.92 ms cold over
    /// this machine's 16 largest sessions.
    /// **It must stay a fixed-size read**: parsing those files whole would swap a bounded read of
    /// Neovibe's own records for one that grows with a file this project neither writes nor knows a
    /// bound for, and it would do it here, on the loop that draws the editor.
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
                .map(|cwd| {
                    let id = agent::conversation_id_for_cwd(&cwd);
                    let cwd = cwd.to_string_lossy().into_owned();
                    agent::resumable_sessions(&id)
                        .into_iter()
                        .map(|session| title_for(&cwd, session))
                        .collect()
                })
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
    use crate::editor_context::{EditorContext, Selection};
    use crate::test_providers::RecordingProvider;

    #[test]
    fn backend_kind_round_trips_its_wire_name() {
        assert_eq!(BackendKind::Legacy.as_str(), "legacy");
        assert_eq!(BackendKind::Sidecar.as_str(), "sidecar");
    }

    #[test]
    fn the_sidecar_greeting_names_its_verdandi_baseline() {
        let greeting = BackendGreeting::for_kind(BackendKind::Sidecar, PathBuf::from("/tmp"));
        assert_eq!(
            greeting.expected_verdandi_revision,
            Some(agent::EXPECTED_VERDANDI_REVISION)
        );
    }

    #[test]
    fn the_legacy_greeting_has_no_verdandi_baseline_to_name() {
        let greeting = BackendGreeting::for_kind(BackendKind::Legacy, PathBuf::from("/tmp"));
        assert_eq!(
            greeting.expected_verdandi_revision, None,
            "the legacy backend has no Verdandi dependency"
        );
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
        assert!(
            !broken_pipe.benign,
            "a dead process must never be treated as a benign ordering complaint"
        );
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
            title: None,
            name: None,
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

    // ---- The picker's title ladder (design §3.1.3's Amendment; invariants 16 and 17) -----------

    /// Points `CLAUDE_CONFIG_DIR` at one disposable root for this whole test binary, and returns
    /// the `projects/` directory under it.
    ///
    /// A process-wide variable set from a multi-threaded test binary, which this project normally
    /// refuses (`agent::state_dirs`'s own module doc says so). It is set exactly ONCE, to a value
    /// that never changes, from inside a `OnceLock` initializer, so no thread can observe it being
    /// two different things -- and the alternative would have been a second global redirect in
    /// `agent` competing with the `CLAUDE_CONFIG_DIR` the external-writer tests already set. Any
    /// other test in this crate that reaches a transcript path finds an empty directory here, which
    /// is what it would have found anyway.
    fn a_claude_projects_root() -> &'static std::path::Path {
        static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            let root = std::env::temp_dir().join(format!("neovibe-core-claude-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("projects")).expect("creating the fake config root should succeed");
            std::env::set_var("CLAUDE_CONFIG_DIR", &root);
            root
        })
        .join("projects")
        .leak()
    }

    /// A workspace holding one offerable record, and optionally a transcript beside it.
    ///
    /// `recorded_title` is what Neovibe itself wrote at write time; `ai_title` is what the CLI
    /// wrote into its own transcript. Either can be absent, which is what makes the three levels of
    /// the ladder reachable from one helper.
    fn a_workspace_with(recorded_title: Option<&str>, ai_title: Option<&str>) -> PathBuf {
        let projects = a_claude_projects_root();
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("title-ladder");
        let canonical = dir.canonicalize().expect("a just-created directory canonicalizes");
        let session = format!("prov-{}", uuid::Uuid::new_v4());
        agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
            conversation_id: agent::conversation_id_for_cwd(&canonical),
            provider: "claude".into(),
            provider_session_id: session.clone(),
            canonical_cwd: canonical.to_string_lossy().into_owned(),
            created_at: "1000".into(),
            updated_at: "2000".into(),
            provider_advertised_resume: true,
            title: recorded_title.map(str::to_owned),
            name: None,
        })
        .expect("writing a record into the test state root should succeed");
        if let Some(ai_title) = ai_title {
            let bucket = projects.join(agent::transcript::sanitize_cwd_for_claude_projects(
                &canonical.to_string_lossy(),
            ));
            std::fs::create_dir_all(&bucket).expect("creating the transcript bucket should succeed");
            std::fs::write(
                bucket.join(format!("{session}.jsonl")),
                format!("{}\n", serde_json::json!({"type": "ai-title", "aiTitle": ai_title})),
            )
            .expect("writing a fake transcript should succeed");
        }
        dir
    }

    fn the_only_row(workspace: PathBuf) -> ResumableSession {
        let greeting = BackendGreeting::for_kind(BackendKind::Sidecar, workspace);
        assert_eq!(greeting.resumable.len(), 1, "the planted record must be offerable");
        greeting.resumable.into_iter().next().expect("checked just above")
    }

    /// Level 1 of the ladder: the CLI's own title is layered ABOVE Neovibe's own, at display time.
    /// Both exist here, and the CLI's wins -- which is the owner's ruling (2026-09-20) and the only
    /// thing this change buys, since 37 of this machine's 44 sessions already have one on disk while
    /// Neovibe's own `title` only exists for sessions started after 2026-09-19.
    #[test]
    fn the_clis_own_title_is_what_a_row_shows_when_the_transcript_has_one() {
        let row = the_only_row(a_workspace_with(
            Some("what neovibe recorded"),
            Some("what the CLI recorded"),
        ));
        assert_eq!(row.title.as_deref(), Some("what the CLI recorded"));
    }

    /// **The negative control, and the whole promise of the fallback** (invariant 17). The same
    /// record with no transcript beside it must read EXACTLY as it did before this change: the
    /// title Neovibe recorded, then the bare id. Without this pair, "it degrades back to today's
    /// behaviour" would only be a sentence -- and the degradation is silent, so nothing else would
    /// notice the day the CLI stops writing that line.
    #[test]
    fn with_no_ai_title_a_row_reads_exactly_as_it_did_before() {
        // Level 2: Neovibe's own recorded title.
        assert_eq!(
            the_only_row(a_workspace_with(Some("what neovibe recorded"), None))
                .title
                .as_deref(),
            Some("what neovibe recorded")
        );
        // Level 3: no title at all -- the row falls back to its id and timestamps, and invents
        // nothing.
        assert_eq!(the_only_row(a_workspace_with(None, None)).title, None);
        // And the two really are different answers for the same absence, which is what makes this
        // a control rather than a restatement.
        assert_ne!(
            the_only_row(a_workspace_with(
                Some("what neovibe recorded"),
                Some("what the CLI recorded")
            ))
            .title,
            the_only_row(a_workspace_with(Some("what neovibe recorded"), None)).title
        );
    }

    /// An `ai-title` this code cannot render drops to the next level rather than rendering blank:
    /// the value is someone else's process's output, normalized through the same
    /// `title_from_prompt` as every other title, and a blank one declines.
    #[test]
    fn an_unusable_ai_title_falls_through_to_the_recorded_one() {
        let row = the_only_row(a_workspace_with(Some("what neovibe recorded"), Some("   ")));
        assert_eq!(row.title.as_deref(), Some("what neovibe recorded"));
    }

    /// **Invariant 16: nothing read from a transcript is ever persisted.** The record on disk is
    /// byte-identical before and after the picker is built, so the CLI's title cannot collide with
    /// `title`'s own first-one-wins rule and cannot move `updated_at`.
    #[test]
    fn building_the_picker_never_writes_the_clis_title_into_the_record() {
        let workspace = a_workspace_with(Some("what neovibe recorded"), Some("what the CLI recorded"));
        let canonical = workspace.canonicalize().expect("the workspace canonicalizes");
        let records = agent::state_dirs::redirect_state_to_a_test_root()
            .join("conversations")
            .join(agent::conversation_id_for_cwd(&canonical));
        let before = every_file_under(&records);
        assert!(!before.is_empty(), "one planted record has to be on disk");
        let row = the_only_row(workspace);
        assert_eq!(
            row.title.as_deref(),
            Some("what the CLI recorded"),
            "the transcript really was read, or this proves nothing"
        );
        assert_eq!(
            every_file_under(&records),
            before,
            "reading a title must leave the record byte-identical"
        );
        // And what stayed there is Neovibe's own title, unchanged by the one the row showed.
        assert!(
            before
                .iter()
                .any(|(_, bytes)| String::from_utf8_lossy(bytes).contains("what neovibe recorded")),
            "the record on disk still holds the title this project recorded"
        );
        assert!(
            !before
                .iter()
                .any(|(_, bytes)| String::from_utf8_lossy(bytes).contains("what the CLI recorded")),
            "the CLI's title must never reach disk"
        );
    }

    /// Every file under `dir`, path and bytes, sorted. The bytes, not a parsed record: invariant 16
    /// is about what is written, and a comparison of parsed values would pass over a rewrite that
    /// only changed formatting or a field this crate does not read.
    fn every_file_under(dir: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut found: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_file())
            .map(|entry| {
                let bytes = std::fs::read(entry.path()).expect("a file just listed should be readable");
                (entry.path(), bytes)
            })
            .collect();
        found.sort();
        found
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
        assert!(
            !error.benign,
            "a refused resume ends the attempt; it is not an ordering complaint"
        );
        assert!(
            error.message.contains("cannot continue a previous session"),
            "got: {}",
            error.message
        );
    }

    /// The turn that goes on the wire and the turn the panel shows are different strings, and the
    /// difference is wire 1's editor context. A panel that echoed the wire text would show the user
    /// a file path and a selection they never typed.
    ///
    /// **Scope, stated plainly because the name used to overclaim it:** this pins `user_prompt_event`
    /// itself -- the free function both arms of `send_turn` call to build the folded event -- and
    /// nothing more. It never calls `AgentBackend::send_turn`, on either arm.
    ///
    /// The Sidecar arm's actual use of this helper (that a real `send_turn` call folds `as_typed`,
    /// not `wire_text`) IS separately verified end-to-end by
    /// `a_rejected_sidecar_turn_still_leaves_the_prompt_recorded` below, which builds a real
    /// `AgentBackend::Sidecar` and calls `send_turn` on it.
    ///
    /// **The Legacy arm has no equivalent, anywhere in this file.** No test in this module ever
    /// holds an `AgentBackend::Legacy` and calls `send_turn` on it -- the only way to get one is
    /// `AgentBackend::start` -> `AgentSession::start` -> `AgentProcess::spawn`, which spawns a real
    /// `claude` process, and there is no injectable provider on that path the way `AgentConversation`
    /// has one (same asymmetry `a_rejected_sidecar_turn_still_leaves_the_prompt_recorded`'s own doc
    /// records for that fix). So the Legacy arm of `send_turn` --
    /// `session.fold_locally(user_prompt_event(as_typed))` -- is dead under `cargo test`: if it were
    /// changed to fold `wire_text` instead, a plausible copy/paste since the two arms sit five lines
    /// apart and look alike, nothing here would fail.
    #[test]
    fn user_prompt_event_records_what_was_typed_not_what_went_on_the_wire() {
        let typed = "what does this do?";
        // `crate::`, not `neovibe_core::` -- this module IS that crate, and its own unit tests
        // cannot address it by its external name the way `shell` does.
        let wire = crate::editor_context::compose_turn_text(
            typed,
            Some(&EditorContext {
                file: "/p/src/main.rs".into(),
                selection: Some(Selection {
                    start_line: 3,
                    end_line: 4,
                    text: "fn main() {}".into(),
                }),
            }),
        );
        assert!(
            wire.contains("/p/src/main.rs"),
            "the fixture must actually differ: {wire}"
        );

        let event = user_prompt_event(typed);
        let AgentDomainEvent::UserPromptSubmitted { text } = event else {
            panic!("wrong variant")
        };
        assert_eq!(text, typed);
        assert!(!text.contains("/p/src/main.rs"));
    }

    // ---- Fix round 1's accepted cost: a rejected send still leaves the prompt recorded ----------

    /// A minimal `agent::AgentProvider` built only to make `AgentConversation::create` succeed and
    /// its `send_turn` genuinely fail -- the public equivalent of `agent::conversation`'s own
    /// private `FakeProvider` (its `send_turn_error` field does exactly this), rewritten here
    /// against the PUBLIC trait because that private double belongs to `agent` and is not
    /// exported. Nothing in `agent` was widened to make this possible: `AgentProvider` and every
    /// request/error type it uses were already `pub` at the crate root.
    struct RejectingProvider;

    impl agent::AgentProvider for RejectingProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo::default()
        }
        fn create_session(&self, _request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
            Ok("fake-session".into())
        }
        fn resume_session(&self, _request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
            Err(agent::ProviderError::UnsupportedCapability("resume"))
        }
        /// The one rigged method. `TurnAlreadyActive` is the same rejection reason
        /// `AgentBackend::send_turn`'s own doc names ("a turn already active") and the same one
        /// `agent::conversation`'s sibling test
        /// (`a_providers_own_turn_already_active_maps_to_the_same_benign_error`) uses for the
        /// identical shape of failure.
        fn send_turn(&self, _request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
            Err(agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::TurnAlreadyActive,
                message: "a turn is already in progress on this session".into(),
            })
        }
        fn interrupt_turn(&self, _request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
            Ok(())
        }
        fn resolve_permission(&self, _request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
            Ok(())
        }
        fn close_session(&self, _request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
            Ok(())
        }
        fn pump(&self) -> Vec<AgentDomainEvent> {
            // Never queued, so the ingestion thread this spawns just polls and sleeps for the
            // whole test -- harmless, and the same shape every sibling test in this file that
            // never calls `fake.queue(...)` already relies on.
            Vec::new()
        }
    }

    /// Pins fix round 1's accepted cost (see `AgentBackend::send_turn`'s own doc) rather than
    /// leaving it to inspection: folding the prompt BEFORE the send means a REJECTED turn still
    /// leaves it in the projection, on purpose -- the user genuinely typed it.
    ///
    /// **Sidecar half only.** The legacy half of the same claim has no equivalent test here: the
    /// only way to make `AgentSession::send_turn` reject is a live session with a turn already in
    /// flight, and the only way to get a live `AgentSession` at all is `AgentSession::start` ->
    /// `AgentProcess::spawn`, which spawns a REAL `claude` process -- there is no injectable
    /// provider/trait on that path the way `AgentConversation` has one. That asymmetry is a real
    /// fact about the two backends (this module's own header doc says as much: "NOT
    /// interchangeable at the type level"), not an oversight in this test.
    #[test]
    fn a_rejected_sidecar_turn_still_leaves_the_prompt_recorded() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("rejected-turn-keeps-prompt");
        let conversation =
            AgentConversation::create(std::sync::Arc::new(RejectingProvider), &dir, PermissionMode::Bypass)
                .expect("create_session succeeds on RejectingProvider; only send_turn is rigged to fail");
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        let result = backend.send_turn("wire text with context composed in", "what does this do?");

        let error = result.expect_err("RejectingProvider's send_turn always fails");
        assert_eq!(
            backend
                .projection()
                .user_prompts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            vec!["what does this do?"],
            "the prompt was folded BEFORE the rejected send, so it survives the rejection"
        );
        // And the sidecar arm carries NOTHING in `folded_events`, which is not an omission: its
        // `fold_locally` queued the prompt to the ingestion pump, so the UI gets it by the ordinary
        // route whether or not the send succeeded. The legacy arm has no such route -- its events
        // are returned synchronously -- which is why that arm attaches them here instead, and why
        // the hole was in exactly one of the two.
        assert!(
            error.folded_events.is_empty(),
            "the sidecar's prompt reaches the UI through the pump, never through the error"
        );
    }

    /// The resume picker's title is the prompt AS TYPED. `wire_text` carries the editor context
    /// above the user's words, and its first line would name every session after a file path.
    #[test]
    fn a_sidecar_sessions_title_is_the_prompt_as_typed_not_the_wire_text() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("title-as-typed");
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, PermissionMode::Bypass).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let conversation_id = backend
            .conversation_id()
            .expect("a sidecar backend has one")
            .to_string();

        let sent = backend.send_turn(
            "Editor context: /p/src/main.rs, line 3\n\nfix the picker",
            "fix the picker",
        );
        assert!(sent.is_ok(), "RecordingProvider accepts every turn");
        provider.queue(AgentDomainEvent::SessionOpened {
            session_id: "fake-session".into(),
            provider_session_id: "claude-title".into(),
            model: "m".into(),
            cwd: dir.to_string_lossy().into_owned(),
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let record = loop {
            if let Ok(record) = agent::persistence::load_conversation_record(&conversation_id, "claude-title") {
                break record;
            }
            assert!(std::time::Instant::now() < deadline, "adoption never wrote the record");
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        assert_eq!(record.title.as_deref(), Some("fix the picker"));
    }

    /// Spec §3.5: a tab renamed before its session existed gets the name into the record adoption
    /// writes; a rename after that is written into the record by the ingestion thread.
    #[test]
    fn a_rename_reaches_the_record_before_and_after_adoption() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("name-before-adoption");
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, PermissionMode::Bypass).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let conversation_id = backend.conversation_id().unwrap().to_string();

        backend.note_name(Some("docs".into()));
        provider.queue(AgentDomainEvent::SessionOpened {
            session_id: "fake-session".into(),
            provider_session_id: "claude-named".into(),
            model: "m".into(),
            cwd: dir.to_string_lossy().into_owned(),
        });
        let wait_for = |want: Option<&str>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Ok(record) = agent::persistence::load_conversation_record(&conversation_id, "claude-named") {
                    if record.name.as_deref() == want {
                        return;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "the record never said {want:?}");
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        wait_for(Some("docs"));
        backend.note_name(Some("docs v2".into()));
        wait_for(Some("docs v2"));
        backend.note_name(None);
        wait_for(None);
        backend.shutdown();
    }

    // ---- The permission policy, wired through the one point both backends converge on ---------

    /// A real workspace with a real file in it, because the policy's path branch canonicalizes for
    /// real -- a fabricated root would make "inside the project" and "does not exist" the same
    /// answer.
    fn a_workspace_holding_one_file() -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("permission-policy");
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    /// Drives the pump until it yields something or the deadline passes. The sidecar's events cross
    /// the ingestion thread, so the first call after a queue is normally empty.
    fn pump_until_delivery(backend: &mut AgentBackend, project_root: &Path) -> Vec<AgentDomainEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match backend.take_ui_delivery(project_root) {
                UiDelivery::Events(events) => return events,
                _ if std::time::Instant::now() > deadline => return Vec::new(),
                _ => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    }

    /// The owner's defect, in one assertion: a `Read` of a file inside his own project produced a
    /// card. It must now produce no card at all, and the request must really have been answered --
    /// not merely hidden, which would leave the model waiting forever.
    #[test]
    fn a_read_inside_the_project_is_answered_here_and_never_becomes_a_card() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, PermissionMode::Auto).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-read".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
        });
        // One `ToolCallStarted` alongside it, so this also pins that the FILTER is selective: the
        // record of what ran must still reach the transcript.
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "turn-1".into(),
            tool_use_id: "tool-1".into(),
            name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
        });

        let delivered = pump_until_delivery(&mut backend, &dir);
        assert!(
            !delivered
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
            "a read inside the project must not reach the UI as a card: {delivered:?}"
        );
        assert!(
            delivered
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. })),
            "the tool call itself is still owed to the transcript: {delivered:?}"
        );
        assert_eq!(
            provider.resolutions(),
            vec![("perm-read".to_string(), true)],
            "suppressing the card without answering the request would hang the model"
        );
        backend.shutdown();
    }

    /// The other direction, which is the one a permissive bug would break silently: a `Write` still
    /// reaches the user, and nothing answers it on their behalf.
    #[test]
    fn a_write_still_reaches_the_user_and_is_answered_by_nobody_else() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, PermissionMode::Auto).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
        });

        let delivered = pump_until_delivery(&mut backend, &dir);
        assert!(
            delivered.iter().any(|e| matches!(
                e,
                AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "perm-write"
            )),
            "a write must reach the user: {delivered:?}"
        );
        assert!(
            provider.resolutions().is_empty(),
            "nothing may answer a write for the user"
        );
        backend.shutdown();
    }

    /// A read OUTSIDE the project root is the same tool and the same shape of input, and it still
    /// gets a card. Asserted through the backend rather than only against the policy function,
    /// because the wiring has to pass the real project root down -- passing `"/"`, or the process
    /// cwd, would auto-allow this and every test that only calls `classify_permission_request`
    /// directly would still pass.
    #[test]
    fn a_read_outside_the_project_still_reaches_the_user() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, PermissionMode::Auto).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-escape".into(),
            tool_use_id: Some("tool-3".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "/etc/hostname" }),
        });

        let delivered = pump_until_delivery(&mut backend, &dir);
        assert!(
            delivered
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
            "a read outside the project must reach the user: {delivered:?}"
        );
        assert!(provider.resolutions().is_empty());
        backend.shutdown();
    }

    /// D7 through the backend, not only the policy: a rule reaches the one point both backends
    /// converge, and a compound command with a matching first word still reaches the user.
    #[test]
    fn a_project_rule_answers_a_bash_call_and_never_a_compound_one() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, PermissionMode::Auto).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap());
        for (id, command) in [("perm-rule", "npm ci"), ("perm-compound", "npm ci && rm -rf .")] {
            provider.queue(AgentDomainEvent::PermissionRequested {
                permission_id: id.into(),
                tool_use_id: None,
                tool_name: "Bash".into(),
                input: serde_json::json!({ "command": command }),
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let delivered = loop {
            match backend.take_ui_delivery_with_rules(&dir, &rules) {
                UiDelivery::Events(events) => break events,
                _ => {
                    assert!(std::time::Instant::now() < deadline, "nothing was delivered");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };
        assert_eq!(provider.resolutions(), vec![("perm-rule".to_string(), true)]);
        assert!(delivered.iter().any(|e| matches!(e,
            AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "perm-compound")));
        backend.shutdown();
    }

    /// Modules spec §11, P2: "an `agent_backend` test driving a real `AgentConversation` to a pending
    /// permission while hidden, asserting `attention() == 1` and that auto-allowed requests are
    /// still answered". Hiding the chat changes nothing in the pump -- `agent_panel.rs`'s 33ms timer
    /// never looks at visibility -- so "hidden" is the tracker told `on_screen = false`. The
    /// projection holds TWO pending requests here, the `Read` the policy answered included, until
    /// the provider's resolution comes back; the tracker counts the one card the panel was handed,
    /// which is why it exists (`crate::attention`'s module doc).
    #[test]
    fn a_hidden_chat_counts_the_card_it_holds_and_still_answers_what_needs_no_human() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, PermissionMode::Auto).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let mut tracker = crate::attention::AttentionTracker::default();

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-read".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
        });
        let delivered = pump_until_delivery(&mut backend, &dir);
        tracker.observe(&delivered, false);
        tracker.retain_pending(|id| backend.projection().pending_permissions.contains_key(id));

        assert_eq!(tracker.attention().pending, 1, "the Write's card, and not the Read's");
        assert_eq!(backend.projection().pending_permissions.len(), 2, "the premise");
        assert_eq!(
            provider.resolutions(),
            vec![("perm-read".to_string(), true)],
            "the Read was answered while the chat was hidden"
        );

        // The user answers it; the provider's resolution comes back on a later pump.
        backend
            .respond_permission("perm-write", PermissionDecision::Allow)
            .map_err(|e| e.message)
            .expect("a pending card can be answered");
        provider.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "perm-write".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        let delivered = pump_until_delivery(&mut backend, &dir);
        tracker.observe(&delivered, false);
        tracker.retain_pending(|id| backend.projection().pending_permissions.contains_key(id));
        assert_eq!(tracker.attention().pending, 0);
        backend.shutdown();
    }

    /// A refusal that now carries events must still be a refusal.
    ///
    /// `AgentBackend::send_turn`'s legacy arm builds its error as
    /// `BackendError::from(io_error).with_folded_events(..)`, and `benign` is what decides whether
    /// the panel reports the refusal or tears the whole conversation down (`apply_command_outcome`).
    /// Attaching the prompt must not flip that: a turn refused because one is already running is an
    /// ordering complaint, and treating it as fatal would throw away a working session over a
    /// double Enter. The fatal direction is pinned too, so this cannot pass by `benign` being
    /// hardcoded either way.
    #[test]
    fn attaching_folded_events_does_not_change_how_an_error_is_classified() {
        let folded = || vec![user_prompt_event("what does this do?")];

        let benign: BackendError = BackendError::from(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a turn is already in progress",
        ))
        .with_folded_events(folded());
        assert!(
            benign.benign,
            "a refused turn must stay benign once it carries the prompt"
        );
        assert_eq!(benign.folded_events.len(), 1);

        let fatal: BackendError = BackendError::from(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            .with_folded_events(folded());
        assert!(!fatal.benign, "a dead process is still fatal, events or no events");
        assert_eq!(fatal.folded_events.len(), 1);
    }
}
