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
//! Selected by `EITRI_AGENT_BACKEND=legacy|sidecar`, defaulting to `legacy` until the sidecar
//! path clears its acceptance criteria.

use agent::{
    AgentConversation, AgentDomainEvent, AgentSession, AgentSessionProjection, ClaudeSidecarProvider,
    ConversationError, PermissionDecision, PermissionMode, ProjectionGuard, ProviderCapabilities, ProviderInfo,
    ResumableSession, RevisedDelivery, UiDelivery,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The legacy backend's capabilities, stated once so a test can assert on the value
/// `capabilities()` actually returns rather than on a copy of it.
///
/// It advertises nothing over a wire, so these are read off what its code demonstrably does: it
/// interrupts (`AgentSession::interrupt` sends a real control_request), it has no resume and no
/// fork, and it runs every session gated.
const LEGACY_CAPABILITIES: ProviderCapabilities = ProviderCapabilities {
    resume: false,
    fork: false,
    interrupt: true,
    // False since R07 (2026-09-27): this backend never starts the CLI in `bypassPermissions` any
    // more -- it always passes `--permission-mode default` and the hook. Eitri's bypass is its own
    // `allow` under that gate, which needs only the capability below.
    bypass_permission_mode: false,
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
    // R07: every offered mode is honoured through the interactive gate (bypass is Eitri answering
    // `allow` under it), and this backend never starts the CLI ungated.
    assert!(
        LEGACY_CAPABILITIES.interactive_permission_mode,
        "every offered permission mode needs the interactive gate"
    );
    assert!(
        !LEGACY_CAPABILITIES.bypass_permission_mode,
        "R07: the legacy backend never starts the CLI ungated"
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

    /// Reads `EITRI_AGENT_BACKEND` and `agent::LEGACY_BACKEND_COMPILED`.
    ///
    /// `legacy_flag` is `main()`'s own `--legacy`, read the same way it reads `--clean` -- passed
    /// in rather than read here for the same testability reason `explicit` is (see [`choose`]).
    ///
    /// **Correction (2026-09-27, v1-dist plan Task 5, spec §10, D10, D16):** this used to fall back
    /// to `legacy` on an unrecognized value or on "unset, no sidecar artifact" -- described just
    /// below until this correction. Legacy is now the gated backend (`agent::LEGACY_BACKEND_COMPILED`,
    /// off in every release), so a typo or a missing artifact can no longer land there: both now
    /// select the sidecar, with a warning for the typo (D10). Only an explicit `--legacy` or
    /// `EITRI_AGENT_BACKEND=legacy` can still select legacy, and only in a build that compiled it
    /// in -- otherwise `Err` names why. See [`choose`]'s own doc for the resulting matrix.
    ///
    /// [`choose`]: Self::choose
    pub fn from_env(legacy_flag: bool) -> Result<Self, BackendChoiceError> {
        Self::choose(
            std::env::var("EITRI_AGENT_BACKEND").ok().as_deref().map(str::trim),
            legacy_flag,
            agent::sidecar_availability(),
            agent::LEGACY_BACKEND_COMPILED,
        )
    }

    /// [`from_env`](Self::from_env)'s decision, over values a caller supplies.
    ///
    /// Separate so the matrix is testable without mutating the process environment, which every
    /// other test in this binary shares. That is not hygiene for its own sake: the same shape --
    /// a function taking an override as a signal while reading its value from the environment
    /// itself -- was a real bug in `locate_verdandi_checkout` earlier the same day, and a test is
    /// what found it. `legacy_compiled` is an input for the identical reason: it lets one test
    /// binary exercise both the release matrix (`legacy_compiled = false`) and the development one
    /// (`legacy_compiled = true`) without a `cfg`-gated test module.
    ///
    /// **The matrix (spec §10):**
    /// - `--legacy` (`legacy_flag`) or `EITRI_AGENT_BACKEND=legacy` (`explicit`) asks for legacy.
    ///   When `legacy_compiled`, that ask is granted. When it is not (every release build), it is
    ///   `Err` naming [`agent::LEGACY_NOT_IN_BUILD`] and which of the two asked -- the flag wins the
    ///   naming when both are given, because it is a decision made on *this* invocation's command
    ///   line, where the env var could be an inherited leftover nobody meant to set today.
    /// - Everything else selects the sidecar, in both builds alike: an explicit `"sidecar"`, unset,
    ///   empty (a wrapper script's `VAR=`, treated as unset), and -- since this correction -- an
    ///   unrecognized value too (D10: it used to fall back to legacy, which is exactly the direction
    ///   a typo must not go now that legacy is the gated backend) and unset with no artifact
    ///   installed (it used to fall back to legacy for want of one; now it says
    ///   [`agent::NO_SIDECAR_HINT`] and starts the sidecar path anyway, which fails with that same
    ///   hint when it actually tries to spawn one). `sidecar` ([`agent::SidecarAvailability`]) only
    ///   shapes which sentence this prints -- it is not part of the choice between backends. It was
    ///   a `bool` (`packaged_sidecar_available`) until lane A's whole-branch review, which made the
    ///   "not installed" sentence appear above a named `EITRI_VERDANDI_CHECKOUT` the spawn was
    ///   about to build; the third value names that case.
    pub fn choose(
        explicit: Option<&str>,
        legacy_flag: bool,
        sidecar: agent::SidecarAvailability,
        legacy_compiled: bool,
    ) -> Result<Self, BackendChoiceError> {
        if legacy_flag || explicit == Some("legacy") {
            if legacy_compiled {
                return Ok(BackendKind::Legacy);
            }
            // The flag is the more explicit act -- a decision made on this command line -- so it
            // wins the naming when both are somehow given at once (e.g. a launcher's leftover
            // `EITRI_AGENT_BACKEND=legacy` plus a freshly typed `--legacy`).
            let asked_via = if legacy_flag {
                "--legacy"
            } else {
                "EITRI_AGENT_BACKEND=legacy"
            };
            return Err(BackendChoiceError {
                message: format!("{asked_via}: {}", agent::LEGACY_NOT_IN_BUILD),
            });
        }

        // Every remaining case selects the sidecar (D10, D16) -- there is no fallback to legacy left
        // in this function at all, in either build. An unrecognized value still gets its own
        // warning, because a typo silently landing on ANY backend without a word about it is worse
        // than the sidecar it now lands on.
        if let Some(other) = explicit {
            if other != "sidecar" && !other.is_empty() {
                eprintln!(
                    "[agent_backend] EITRI_AGENT_BACKEND={other:?} is not recognized \
                     (expected \"legacy\" or \"sidecar\"); using sidecar"
                );
            }
        }
        match sidecar {
            agent::SidecarAvailability::Runnable => eprintln!(
                "[agent_backend] using the sidecar backend: a sidecar artifact is available \
                 with nothing to build (the `agent:` line below names which one)"
            ),
            // Development only: naming a checkout is what authorizes the build (spawn step 2).
            agent::SidecarAvailability::BuildsNamedCheckout => eprintln!(
                "[agent_backend] using the sidecar backend: EITRI_VERDANDI_CHECKOUT names a \
                 checkout with no sidecar built for this machine, so the first session start \
                 builds it there (npm)"
            ),
            // Said every time, not once: this is the line that explains why the panel's greeting
            // names no session, and a reader who does not see it will look for the reason in the
            // code. `NO_SIDECAR_HINT` names the way out (`eitri setup`) because starting the
            // sidecar path anyway is about to fail with that identical sentence.
            agent::SidecarAvailability::Missing => {
                eprintln!("[agent_backend] using the sidecar backend: {}", agent::NO_SIDECAR_HINT)
            }
        }
        Ok(BackendKind::Sidecar)
    }
}

/// What [`BackendKind::choose`] returns when `--legacy`/`EITRI_AGENT_BACKEND=legacy` asked for a
/// backend this build did not compile in. `message` is a finished, human-readable sentence for
/// `main()` to print before exiting -- the same contract `eitri_core::project_root::resolve`'s
/// `Result<_, String>` keeps, as a dedicated type here only so the error case cannot be confused
/// with a real `BackendKind` at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendChoiceError {
    pub message: String,
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
    ///
    /// No permission mode (R07): every session on both backends is gated, and the tab's mode is
    /// read at the one point that answers requests (`take_ui_delivery_with_rules`), never sent to
    /// the CLI.
    pub fn start(kind: BackendKind, project_dir: &Path, resume: Option<&str>) -> Result<Self, BackendError> {
        Self::start_holding(kind, project_dir, resume, None)
    }

    /// [`start`](Self::start) for a resume whose session lease the caller already holds
    /// (`AgentConversation::resume_holding`): the lease goes to the conversation, and is released
    /// with it or with a failed start. Ignored when nothing is being resumed.
    pub fn start_holding(
        kind: BackendKind,
        project_dir: &Path,
        resume: Option<&str>,
        lease: Option<agent::lease::SessionLease>,
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
                AgentSession::start(project_dir, agent::disallowed_tools())
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
                    Some(provider_session_id) => AgentConversation::resume_holding(
                        std::sync::Arc::new(provider),
                        project_dir,
                        provider_session_id,
                        lease,
                    )
                    .map(|c| AgentBackend::Sidecar(Box::new(c)))
                    .map_err(|e| BackendError::fatal(format!("could not continue the previous session: {e}"))),
                    None => AgentConversation::create(std::sync::Arc::new(provider), project_dir)
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

    /// Eitri's own conversation identity. `None` for the legacy backend, which has no concept of
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
    /// its code demonstrably does: it interrupts, it has no resume and no fork, and it runs every
    /// session gated (R07).
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
    /// variant-by-variant, and a caller that forgets it does not compile. `mode` is the tab's own
    /// `Tab::mode` (R07/S2, spec §2.2): in `Bypass` every request is answered here, never only what
    /// the classifier would allow in `Auto`. Builds a throwaway `host_answered` set -- a caller that
    /// wants the ids kept (every real caller does) uses `take_ui_delivery_with_rules` directly.
    pub fn take_ui_delivery(&mut self, project_root: &Path, mode: PermissionMode) -> UiDelivery {
        let mut discarded = BTreeSet::new();
        self.take_ui_delivery_with_rules(project_root, &agent::PrefixRules::default(), mode, &mut discarded)
    }

    /// `take_ui_delivery`, with the project's D7 prefix rules (`agent::permission_rules`) applied by
    /// the classifier in `Auto`. A rule can only replace the two "not on the read-only list"
    /// verdicts, never a fail-closed one; see `agent::classify_with_rules`. `host_answered` collects
    /// every id this call answers on the caller's behalf (bypass or the classifier), so the caller
    /// (`TabSet::pump`) can hide them from the tray and from a later snapshot (D9).
    ///
    /// Knows no human approvals, so in `Auto` the CLI's own prompt (O3) is always a card here; the
    /// tab set, which does know them, calls `take_ui_delivery_with_approvals`.
    pub fn take_ui_delivery_with_rules(
        &mut self,
        project_root: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
    ) -> UiDelivery {
        self.take_ui_delivery_with_approvals(
            project_root,
            rules,
            mode,
            host_answered,
            &mut HumanApprovals::default(),
            &mut AnsweredForYou::default(),
        )
    }

    /// `take_ui_delivery_with_rules`, plus the tab's `approvals`: the calls the user approved on a
    /// card (`TabSet::answer_card`), each with the tool and exact input the card showed. In `Auto` the
    /// CLI's own prompt for exactly such a call -- same non-empty tool-use id, same tool, same input --
    /// is answered `allow` here, once, and the approval is used up (O3 ruling 5, tightened by the
    /// review): the user approved that call, and the real CLI asks once. `answered_for_you` receives
    /// what was answered without a card that a row says so about ([`AnsweredForYou`]): every CLI
    /// prompt, every gate the acceptEdits fast path answered, every gate a saved prefix rule
    /// answered, and every `Write` so answered over no file.
    pub fn take_ui_delivery_with_approvals(
        &mut self,
        project_root: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
        answered_for_you: &mut AnsweredForYou,
    ) -> UiDelivery {
        self.take_revised_ui_delivery(project_root, rules, mode, host_answered, approvals, answered_for_you)
            .without_revisions()
    }

    /// Records that the panel was just handed a snapshot of this backend read at `revision`, for the
    /// next drain to take back ([`Self::take_ui_snapshot_revision`]) and leave every event tagged at
    /// or below it out of its `events` payload (P1-A2 round 2). Kept by the backend itself, never by
    /// the tab, so a snapshot's revision cannot outlive the session it was read from, whichever way
    /// that session is replaced.
    ///
    /// Only the sidecar has anything to record. Legacy folds only inside `take_revised_ui_delivery`
    /// (`AgentSession::pump_revised`), so a snapshot read between two drains holds nothing the next
    /// drain returns: every event that drain folds is newer than the snapshot.
    pub fn note_ui_snapshot(&mut self, revision: u64) {
        match self {
            AgentBackend::Legacy(_) => {}
            AgentBackend::Sidecar(conversation) => conversation.note_ui_snapshot(revision),
        }
    }

    /// Takes what [`Self::note_ui_snapshot`] recorded since the last take; always `None` on legacy.
    pub fn take_ui_snapshot_revision(&mut self) -> Option<u64> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Sidecar(conversation) => conversation.take_ui_snapshot_revision(),
        }
    }

    /// `take_ui_delivery_with_approvals`, keeping each event's fold revision (`agent::RevisedDelivery`)
    /// -- what `TabSet` calls, because it also sends snapshots and must leave out of its next `events`
    /// payload whatever the last one already carried (P1-A2 round 2, [`Self::note_ui_snapshot`]).
    /// Both backends tag an event with the projection's `last_revision` right after its own fold, so
    /// the tags compare directly with a snapshot's. An event answered here without a card is dropped
    /// with its tag; the others keep theirs, in order.
    pub fn take_revised_ui_delivery(
        &mut self,
        project_root: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
        answered_for_you: &mut AnsweredForYou,
    ) -> RevisedDelivery {
        let delivery = match self {
            AgentBackend::Legacy(session) => {
                let events = session.pump_revised();
                if events.is_empty() {
                    RevisedDelivery::Nothing
                } else {
                    RevisedDelivery::Events(events)
                }
            }
            AgentBackend::Sidecar(conversation) => conversation.take_revised_ui_delivery(),
        };
        match delivery {
            RevisedDelivery::Events(events) => {
                let kept = self.answer_what_needs_no_human(
                    events,
                    project_root,
                    AnswerContext {
                        rules,
                        mode,
                        host_answered,
                        approvals,
                        answered_for_you,
                    },
                );
                if kept.is_empty() {
                    RevisedDelivery::Nothing
                } else {
                    RevisedDelivery::Events(kept)
                }
            }
            // A `Resync` carries no events to filter: the UI is about to rebuild from the
            // projection instead. See `answer_what_needs_no_human`'s own note on the window that
            // leaves open, and `TabSet::pump`'s own Resync arm for how bypass sweeps it (D9).
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
    /// **In `Bypass` (R07/S2, 2026-09-27), every `PermissionRequested` is answered `Allow` here,
    /// unconditionally, before the classifier ever runs.** This is Eitri's own definition of
    /// bypass since R07: the CLI itself is never told to skip permissions (it always runs gated,
    /// `--permission-mode default` plus the hook, or the sidecar's INTERACTIVE mode); "bypass" means
    /// Eitri answers every request `allow` under that gate instead of showing a card. `mode` comes
    /// from the caller's `Tab::mode` on every pump -- this function holds no mode of its own, so
    /// nothing here can drift from what the tab set thinks the tab is in. An id already in
    /// `host_answered` (a duplicate delivery of a request already allowed, e.g. after a resync) is
    /// dropped without answering it a second time.
    ///
    /// **Nothing is answered once the CLI has reported an ungated mode** (spec §2.3, D12) -- neither
    /// in this batch after the `UngatedCliMode`, nor in any batch after the projection recorded one,
    /// and this holds in both modes. Such a session is about to be closed (`TabSet::pump`), and a
    /// request left unanswered is denied by that close; an automatic `allow` here -- in bypass or
    /// under the classifier -- would be a grant made to a session nobody should be granting anything.
    ///
    /// One honest gap remains, uncovered by any test here: the resolution is dropped too, but only
    /// the legacy backend's, which is returned synchronously. The sidecar's `PermissionResolved`
    /// arrives on a later pump and IS delivered, for a `permission_id` the frontend never saw; its
    /// reducer filters `pendingPermissions` by id, so that is a no-op there rather than an error.
    ///
    /// **The second gap this doc used to record is closed, not merely documented, by
    /// `host_answered` and D9:** an id this function inserts into `host_answered` is hidden from a
    /// `Resync`'s snapshot and from the attention tray by `TabSet::pump` (`SnapshotView::of`,
    /// `AttentionTracker::resync`), so a request answered here can no longer resurrect a card the
    /// user was never meant to see, in bypass or under the classifier alike.
    ///
    /// **The CLI's own prompts (O3, Verdandi b3aa188) have their own rules, checked before either
    /// path above** (`provider_prompt: Some`): the CLI raised one after the gate had already
    /// answered, for a check of its own that no hook `allow` or session rule silences (its
    /// sensitive-file check is the measured one). So the classifier and the saved prefix rules never
    /// see it (ruling 3); bypass allows it, because a real `bypassPermissions` session runs the call
    /// (ruling 4); in `Auto` it is allowed only when `approvals` holds the same call -- id, tool and
    /// input the user approved on a card -- and that approval is then used up; otherwise it is a card
    /// (ruling 5). One that `needs_a_human` -- the user's own `permissions.ask` rule forced it, or its
    /// kind is unknown to this build -- is a card in every mode: the SDK's guidance is that a host
    /// auto-approving must not approve a rule-forced ask (ruling 4; review #3 for the unknown kind).
    /// Each answered without a card goes into `answered_for_you` with its row's note.
    ///
    /// **The acceptEdits fast path's own row note is recorded here too, never inferred elsewhere**
    /// (whole-branch review finding 2, v1 trial, 2026-09-28). A `Write`/`Edit`/`NotebookEdit` this
    /// function allowed under the classifier -- which only the fast path ever does for those three
    /// (`agent::permission_policy`'s module doc: no saved rule touches them) -- goes into
    /// `answered_for_you.by_the_fast_path` by tool-use id, with the `(tool_name, input)` the answer
    /// saw (review item 2: a LATER card for the same call can carry an empty tool-use id, so the tab
    /// set's own "same id" removal cannot always find the candidate this leaves), once
    /// `respond_permission` succeeded. The tab set used to infer the same thing from absence (a call
    /// that started in Auto and completed with no card delivered for it), which was also true of a
    /// call the CLI's `validateInput` failed before any hook ran, of a card whose request carried no
    /// tool-use id, and of a gate a switch to bypass answered instead. A request with no tool-use id
    /// is answered as ever and noted nowhere: there is no row to name.
    ///
    /// **So is a `Write` that creates its file** (finding 6), in the fast path and in bypass alike:
    /// whether anything exists at its `file_path` is checked just before the `allow` is sent, when
    /// the CLI cannot have written it yet, and a `Write` over nothing goes into
    /// `answered_for_you.creates_file`. F22's `note_new_files` only ever sees a delivered card, which
    /// neither of these two answers leaves behind.
    ///
    /// **So is a saved prefix rule's own note** (v1 polish F18, pre-existing on `main`; fixed here by
    /// the same whole-branch review that found finding 2, fix round 3). `classify_with_rules`
    /// checks `agent::rule_that_allows` before anything else, so `classification.reason ==
    /// REASON_ALLOWED_BY_A_PROJECT_RULE` means a rule, not the read-only classifier, is why this call
    /// is being allowed here; `agent::rule_that_allows` is asked again for the rule's own display
    /// string (cheap, and the same re-ask the tab set used to make -- only moved to where the answer
    /// is actually given) and the pair goes into `answered_for_you.by_rule` by tool-use id, with the
    /// `(tool_name, input)` the answer saw for the same reason `by_the_fast_path` carries it (review
    /// item 2), once `respond_permission` succeeded. The tab set used to make every `ToolCallStarted` a rule WOULD
    /// answer -- re-asking `agent::rule_that_allows` on that event's own arguments, before any gate
    /// existed to answer -- a candidate, and infer the answer from no card ever having arrived: also
    /// true of a call the CLI's `validateInput` failed before any hook ran (no gate at all), and of a
    /// gate a switch to bypass answered instead of the rule.
    ///
    /// Each event travels with its fold revision (`take_revised_ui_delivery`), untouched: a kept
    /// event keeps its own, a dropped one takes its with it.
    fn answer_what_needs_no_human(
        &mut self,
        events: Vec<(u64, AgentDomainEvent)>,
        project_root: &Path,
        cx: AnswerContext<'_>,
    ) -> Vec<(u64, AgentDomainEvent)> {
        let AnswerContext {
            rules,
            mode,
            host_answered,
            approvals,
            answered_for_you,
        } = cx;
        // Its own statement, guard dropped before any answer: on the sidecar `projection()` holds
        // the ingestion mutex and `respond_permission` locks it again (the 2026-09-15 GTK freeze).
        let mut tripped = self.projection().ungated_cli_mode.is_some();
        let mut kept = Vec::with_capacity(events.len());
        for (revision, event) in events {
            tripped |= matches!(event, AgentDomainEvent::UngatedCliMode { .. });
            let AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                tool_name,
                input,
                provider_prompt,
            } = &event
            else {
                kept.push((revision, event));
                continue;
            };
            if tripped {
                eprintln!("[permission] not answering {tool_name}: the CLI reported an ungated mode");
                kept.push((revision, event));
                continue;
            }
            if let Some(prompt) = provider_prompt {
                // O3: never the classifier's or a rule's (ruling 3). The CLI's sentence embeds the
                // model's own path and a rule can come from a file in the repository, so both are
                // printed escaped (`{:?}`, review #2): they cannot forge or garble a log line.
                if prompt.needs_a_human() {
                    eprintln!(
                        "[permission] asking the user: {tool_name} (the CLI's own prompt, only you answer it: {:?})",
                        prompt.label()
                    );
                    kept.push((revision, event));
                    continue;
                }
                if host_answered.contains(permission_id) {
                    continue;
                }
                let allow_because = match mode {
                    PermissionMode::Bypass => Some("in bypass"),
                    PermissionMode::Auto => approvals
                        .matches(tool_use_id.as_deref(), tool_name, input)
                        .then_some("with your approval"),
                };
                let Some(because) = allow_because else {
                    eprintln!(
                        "[permission] asking the user: {tool_name} (the CLI's own prompt: {:?})",
                        prompt.reason.as_deref().unwrap_or("no reason given")
                    );
                    kept.push((revision, event));
                    continue;
                };
                let (permission_id, tool_name) = (permission_id.clone(), tool_name.clone());
                let (tool_use_id, label) = (tool_use_id.clone(), prompt.label());
                match self.respond_permission(&permission_id, PermissionDecision::Allow) {
                    Ok(_resolution) => {
                        host_answered.insert(permission_id);
                        if let Some(id) = tool_use_id {
                            // Used up (ruling 5, once per call): a later prompt under this id is a
                            // card. In bypass this is a no-op -- nothing was consulted.
                            approvals.consume(&id);
                            answered_for_you.prompts.push(PromptAnsweredForYou {
                                tool_use_id: id,
                                note: format!("{label} — allowed {because}"),
                            });
                        }
                        eprintln!("[permission] allowed the CLI's own prompt {because}: {tool_name}");
                    }
                    Err(error) => {
                        eprintln!(
                            "[permission] could not allow the CLI's own prompt for {tool_name}, showing a card instead: {}",
                            error.message
                        );
                        kept.push((revision, event));
                    }
                }
                continue;
            }
            if mode == PermissionMode::Bypass {
                if host_answered.contains(permission_id) {
                    // Already allowed once (the duplicate-delivery case D9 exists for): drop it
                    // silently rather than answering an id twice.
                    continue;
                }
                let (permission_id, tool_name) = (permission_id.clone(), tool_name.clone());
                // Before the answer: once it is sent, the CLI may already have written the file.
                let creates_file =
                    named_call(tool_use_id).filter(|_| write_over_no_file(&tool_name, input, project_root));
                match self.respond_permission(&permission_id, PermissionDecision::Allow) {
                    Ok(_resolution) => {
                        host_answered.insert(permission_id);
                        answered_for_you.creates_file.extend(creates_file);
                        eprintln!("[permission] allowed in bypass: {tool_name}");
                    }
                    Err(error) => {
                        eprintln!(
                            "[permission] could not allow {tool_name} in bypass, showing a card instead: {}",
                            error.message
                        );
                        kept.push((revision, event));
                    }
                }
                continue;
            }
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
                kept.push((revision, event));
                continue;
            }
            // Cloned before the mutable borrow below, not for tidiness: `event` borrows from the
            // same value `respond_permission` needs `&mut self` for.
            let (permission_id, tool_name) = (permission_id.clone(), tool_name.clone());
            // The fast path is the only allow any of these three tools gets here (this function's
            // doc); its row note, and whether a `Write` creates its file -- read before the answer,
            // since once it is sent the CLI may already have written it.
            let call_id = named_call(tool_use_id);
            let fast_path = call_id
                .clone()
                .filter(|_| ACCEPT_EDITS_TOOLS.contains(&tool_name.as_str()))
                .map(|id| FastPathAnsweredForYou {
                    tool_use_id: id,
                    tool_name: tool_name.clone(),
                    input: input.clone(),
                });
            let creates_file = call_id
                .clone()
                .filter(|_| write_over_no_file(&tool_name, input, project_root));
            // A saved rule's own row note (v1 polish F18, fixed here the same way as finding 2):
            // `classify_with_rules` checks `agent::rule_that_allows` before anything else, so this
            // reason means a rule -- never the read-only classifier -- is why this call is being
            // allowed. Ask it again for the rule's own display string, the one thing this branch
            // does not already have; a request with no tool-use id is answered as ever and noted
            // nowhere, the same as the fast path leaves it.
            let rule = (classification.reason == agent::permission_policy::REASON_ALLOWED_BY_A_PROJECT_RULE)
                .then(|| agent::rule_that_allows(&tool_name, input, project_root, rules))
                .flatten();
            let by_rule = call_id.zip(rule).map(|(id, rule)| RuleAnsweredForYou {
                tool_use_id: id,
                rule,
                tool_name: tool_name.clone(),
                input: input.clone(),
            });
            match self.respond_permission(&permission_id, PermissionDecision::Allow) {
                Ok(_resolution) => {
                    answered_for_you.by_the_fast_path.extend(fast_path);
                    answered_for_you.creates_file.extend(creates_file);
                    answered_for_you.by_rule.extend(by_rule);
                    // Said on stderr rather than silently: this is the only record anywhere of a
                    // tool call that ran without the user being asked. One short line, whose reason
                    // is a fixed label from the policy rather than anything the model wrote.
                    eprintln!(
                        "[permission] allowed without asking: {tool_name} ({})",
                        classification.reason
                    );
                    host_answered.insert(permission_id);
                }
                Err(error) => {
                    eprintln!(
                        "[permission] could not auto-answer {tool_name}, showing a card instead: {}",
                        error.message
                    );
                    kept.push((revision, event));
                }
            }
        }
        kept
    }

    /// Answers `allow` to each id in `ids` that is still pending on THIS session, in the order
    /// given (the caller sorts by `seq`); an id that is not pending is skipped silently -- resolved
    /// already, or never real. Returns the ids it actually answered, so the caller can extend
    /// `host_answered` with exactly those and no others, and the events those answers produced here
    /// (`Approved`). `why` is the log label ("on entering bypass" / "in bypass after a resync").
    ///
    /// **The events are the legacy backend's `PermissionResolved`s, and the panel is owed them**
    /// (Codex v1-mode finding 1). Legacy's CLI emits nothing for a hook reply, so
    /// `AgentSession::respond_permission` folds the resolution itself and returns it -- no pump ever
    /// carries it. Dropping it here left the approved card drawn and counted as waiting until the next
    /// snapshot. The sidecar returns none: its resolution arrives through the pump like any other.
    ///
    /// Collects the pending set in one statement, guard dropped before any answer -- the same
    /// lock-order rule `answer_what_needs_no_human` follows (the 2026-09-15 GTK freeze:
    /// `projection()` holds the sidecar's ingestion mutex and `respond_permission` locks it again).
    ///
    /// **Fails toward a card, never toward a silent grant.** If `respond_permission` errors, the id
    /// is not added to the returned list -- the caller does not hide it, so it stays drawn as a card
    /// and the user can still answer it themselves. A double answer (the user pressed `y` on a card
    /// the same tick this entered bypass) is accepted: the second `respond_permission` is refused by
    /// the provider, logged, and otherwise harmless.
    ///
    /// **Nothing is answered once the CLI has reported an ungated mode** (spec §2.3, D12) -- the same
    /// rule `answer_what_needs_no_human` holds. `TabSet::pump`'s own tripwire check already keeps this
    /// from being reached on the Resync path (D9), but `TabSet::confirm_bypass` calls this directly,
    /// off the 33ms pump, so a report the background ingestion has already folded into the projection
    /// but that pump has not yet turned into a `Failed` tab could otherwise still be approved here.
    ///
    /// **A CLI prompt only a human answers is never answered here** -- one the user's own
    /// `permissions.ask` rule forced (O3 ruling 4), or one of a kind this build does not know (review
    /// #3): it is a card in bypass too, answered only on the card itself. `TabSet::waiting_cards`
    /// leaves it out of what a bypass entry counts and lists, so the prompt's N matches what `y`
    /// approves; this skip is the backstop for any other caller.
    pub fn approve_pending(&mut self, ids: &[String], why: &str) -> Approved {
        // Its own statement, guard dropped before any answer -- the same lock-order rule the
        // collection below follows.
        if self.projection().ungated_cli_mode.is_some() {
            eprintln!("[permission] not answering {why}: the CLI reported an ungated mode");
            return Approved::default();
        }
        let pending: std::collections::BTreeMap<String, String> = {
            let projection = self.projection();
            ids.iter()
                .filter_map(|id| {
                    let p = projection.pending_permissions.get(id)?;
                    if let Some(prompt) = p.provider_prompt.as_ref().filter(|pp| pp.needs_a_human()) {
                        eprintln!(
                            "[permission] not allowing {} {why}: only you answer this prompt ({:?})",
                            p.tool_name,
                            prompt.label()
                        );
                        return None;
                    }
                    Some((id.clone(), p.tool_name.clone()))
                })
                .collect()
        };
        approve_each(ids, &pending, why, |id| {
            self.respond_permission(id, PermissionDecision::Allow)
        })
    }

    pub fn shutdown(&mut self) {
        match self {
            AgentBackend::Legacy(session) => session.shutdown(),
            AgentBackend::Sidecar(conversation) => conversation.shutdown(),
        }
    }
}

/// What [`AgentBackend::approve_pending`] answered: the ids, and the events answering them produced
/// on this side (legacy's locally folded `PermissionResolved`s; none on the sidecar). A caller that
/// shows the panel a tab must hand it `events`, or the approved cards stay drawn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Approved {
    pub ids: Vec<String>,
    pub events: Vec<AgentDomainEvent>,
}

/// `approve_pending`'s loop, with the answer itself injected: the legacy arm of `respond_permission`
/// needs a real `claude` process to exist at all (see `user_prompt_event_records_what_was_typed_not_
/// what_went_on_the_wire`'s doc), so this is the part a test can drive with a legacy-shaped answer.
/// `pending` maps each still-pending id to its tool name (for the log only).
fn approve_each(
    ids: &[String],
    pending: &std::collections::BTreeMap<String, String>,
    why: &str,
    mut respond: impl FnMut(&str) -> Result<Vec<AgentDomainEvent>, BackendError>,
) -> Approved {
    let mut approved = Approved::default();
    for id in ids {
        let Some(tool_name) = pending.get(id) else {
            continue;
        };
        match respond(id) {
            Ok(events) => {
                eprintln!("[permission] allowed {why}: {tool_name}");
                approved.ids.push(id.clone());
                approved.events.extend(events);
            }
            Err(error) => {
                eprintln!("[permission] could not allow {tool_name} {why}: {}", error.message);
            }
        }
    }
    approved
}

/// What one `answer_what_needs_no_human` call reads and writes besides the batch: bundled so the
/// signature stays readable. See `take_ui_delivery_with_approvals` for each field.
struct AnswerContext<'a> {
    rules: &'a agent::PrefixRules,
    mode: PermissionMode,
    host_answered: &'a mut BTreeSet<String>,
    approvals: &'a mut HumanApprovals,
    answered_for_you: &'a mut AnsweredForYou,
}

/// The calls the user approved on a card in one tab, each kept with the tool and the exact input the
/// card showed (O3 ruling 5, tightened by the Codex and Opus reviews). `TabSet::answer_card` records
/// one on an Approve that reached the provider; `matches` is the only test the Auto rule applies to
/// the CLI's own prompt, and `consume` uses an approval up once it answered one.
///
/// The input is kept whole rather than as a digest: the comparison is then exact (`Value` equality,
/// which ignores object key order) with no canonical form to get wrong and no collision to argue
/// about. The set is small and short-lived -- emptied at every turn end, every `Resync`, `r` and with
/// the backend (`TabSet`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HumanApprovals {
    calls: std::collections::BTreeMap<String, (String, serde_json::Value)>,
}

impl HumanApprovals {
    /// The user approved `tool_name` with `input` for the call `tool_use_id`. An empty id is no link
    /// to anything and is never recorded.
    pub fn record(&mut self, tool_use_id: &str, tool_name: &str, input: &serde_json::Value) {
        if !tool_use_id.is_empty() {
            self.calls
                .insert(tool_use_id.to_string(), (tool_name.to_string(), input.clone()));
        }
    }

    /// Whether the user approved exactly this call: the same non-empty tool-use id, the same tool and
    /// the same input. A prompt with no id matches nothing.
    pub fn matches(&self, tool_use_id: Option<&str>, tool_name: &str, input: &serde_json::Value) -> bool {
        tool_use_id
            .filter(|id| !id.is_empty())
            .and_then(|id| self.calls.get(id))
            .is_some_and(|(tool, approved)| tool == tool_name && approved == input)
    }

    /// The approval for `tool_use_id` answered a prompt: it answers no other.
    pub fn consume(&mut self, tool_use_id: &str) {
        self.calls.remove(tool_use_id);
    }

    pub fn contains(&self, tool_use_id: &str) -> bool {
        self.calls.contains_key(tool_use_id)
    }

    pub fn clear(&mut self) {
        self.calls.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// A CLI prompt answered without a card, and the note its call's row shows (review item 7, the
/// spike's row note): "Claude Code safety check — allowed in bypass" / "— allowed with your
/// approval" (`ProviderPrompt::label` names whose question it was).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAnsweredForYou {
    pub tool_use_id: String,
    pub note: String,
}

/// A `Write`/`Edit`/`NotebookEdit` call whose gate the acceptEdits fast path answered `allow`
/// ("allowed by auto" on the row once the call completes), with the exact `(tool_name, input)` the
/// answer saw. Kept alongside the tool-use id (review item 2, v1 trial whole-branch review fix round
/// 3's own follow-up) so a LATER card for the exact same call -- the CLI's own follow-up prompt,
/// whose request can carry an EMPTY tool-use id (Verdandi's `permissionBroker.ts` sends `toolUseId:
/// ''` when the CLI gave none; `translate.rs` maps that to `None`) -- can still be matched by
/// content and drop the candidate in `note_auto_edit_answers`, which the ordinary "same id" removal
/// there can never see at all: it has no id to compare against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathAnsweredForYou {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// A call whose gate a saved prefix rule answered `allow`, with the rule in Claude Code's own syntax
/// (v1 polish F18's note, "allowed by rule `Bash(npm ci *)`" once the call completes) and the exact
/// `(tool_name, input)` the answer saw, kept for the same reason and matched the same way as
/// [`FastPathAnsweredForYou`] (review item 2) in `note_rule_answers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleAnsweredForYou {
    pub tool_use_id: String,
    pub rule: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// What one delivery answered without a card that a call's row says something about, each by
/// tool-use id, recorded by `answer_what_needs_no_human` at the moment it answered (whole-branch
/// review findings 2 and 6, v1 trial; the same fix applied to the pre-existing rule note, v1 trial
/// whole-branch review, fix round 3): the tab set turns these into row notes and never infers them
/// from a card's absence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnsweredForYou {
    /// The CLI's own prompts (O3), each with its note.
    pub prompts: Vec<PromptAnsweredForYou>,
    /// `Write`/`Edit`/`NotebookEdit` calls whose gate the acceptEdits fast path answered `allow`.
    pub by_the_fast_path: Vec<FastPathAnsweredForYou>,
    /// Calls whose gate a saved prefix rule answered `allow`. Every entry here came from
    /// `classify_with_rules` actually answering THIS gate -- never from re-asking
    /// `agent::rule_that_allows` on a `ToolCallStarted`'s own arguments, which is true of a call
    /// whose gate never arrived at all (the CLI's `validateInput` failed first), of a gate a switch
    /// to bypass answered instead of the rule, and of a card whose request named no tool-use id
    /// (there is no row to name, so it is answered as ever and noted nowhere).
    pub by_rule: Vec<RuleAnsweredForYou>,
    /// `Write` calls answered `allow` without a card -- by the fast path in `Auto`, or in bypass --
    /// whose `file_path` named nothing just before the answer was sent: the row says the call
    /// creates the file, never that it overwrites one it cannot see.
    pub creates_file: Vec<String>,
}

/// The tools the acceptEdits fast path applies to -- `agent::permission_policy`'s own (private)
/// `EDIT_TOOLS`, restated here only to recognise its answer, never to decide one.
const ACCEPT_EDITS_TOOLS: [&str; 3] = ["Write", "Edit", "NotebookEdit"];

/// A request's tool-use id, when it names a call a row can show: `None` for a missing or empty id.
fn named_call(tool_use_id: &Option<String>) -> Option<String> {
    tool_use_id.clone().filter(|id| !id.is_empty())
}

/// Whether nothing exists at `path` at all -- a dangling symlink counts (a `Write` there, or a
/// check of what it is about to replace, would follow it). Any lookup failure OTHER than "not
/// found" (a parent directory this process may not search, a path through a regular file) says
/// nothing about what is there, so it does NOT count as absent: an uncertain answer resolves toward
/// the overwrite warning, as the permission policy's own uncertainties resolve toward a card.
pub(crate) fn file_absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Whether `tool_name`/`input` is a `Write` whose `file_path` (read against `project_root` when
/// relative, as the tool's own absolute path needs no root) names nothing right now.
fn write_over_no_file(tool_name: &str, input: &serde_json::Value, project_root: &Path) -> bool {
    tool_name == "Write"
        && input
            .get("file_path")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .is_some_and(|p| file_absent(&project_root.join(p)))
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

/// One picker row's title: the CLI's own, then Eitri's own, then none (design §3.1.3's
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
    /// The configured Claude account's name (`agent::account::configured`), `null` when none is
    /// set (panel round 2 plan's §7, Task 5). Read, never written, here -- the same asymmetry as
    /// `expected_verdandi_revision`.
    pub account: Option<String>,
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
    /// Eitri's own records for one that grows with a file this project neither writes nor knows a
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
        let account = agent::account::configured().map(|a| a.name().to_string());
        match kind {
            BackendKind::Legacy => Self {
                kind,
                project_dir,
                permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
                expected_verdandi_revision: None,
                resumable,
                account,
            },
            BackendKind::Sidecar => Self {
                kind,
                project_dir,
                permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
                expected_verdandi_revision: Some(agent::EXPECTED_VERDANDI_REVISION),
                resumable,
                account,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use agent::SidecarAvailability::{self, BuildsNamedCheckout, Missing, Runnable};

    /// Every answer `agent::sidecar_availability` can give. None of them may change which backend is
    /// chosen: since v1-dist Task 5 it only picks the startup line.
    const ALL_AVAILABILITIES: [SidecarAvailability; 3] = [Runnable, BuildsNamedCheckout, Missing];

    /// The explicit variable wins over what is installed, whatever is installed -- and now that
    /// legacy is the gated backend (v1-dist plan Task 5, spec §10, D16), it wins only where this
    /// build compiled legacy in at all. Someone who names a backend is testing that backend, and
    /// an installation that happens to carry a sidecar artifact must not quietly overrule them.
    #[test]
    fn an_explicit_choice_beats_what_is_installed() {
        for available in ALL_AVAILABILITIES {
            for legacy_compiled in [true, false] {
                assert_eq!(
                    BackendKind::choose(Some("sidecar"), false, available, legacy_compiled),
                    Ok(BackendKind::Sidecar)
                );
            }
            assert_eq!(
                BackendKind::choose(Some("legacy"), false, available, true),
                Ok(BackendKind::Legacy)
            );
        }
    }

    /// The default follows what this installation can actually run -- in every build alike, since
    /// this correction: legacy is never the fallback for "unset, no sidecar artifact" any more,
    /// whether or not this build could have compiled legacy in. This is the change that makes the
    /// sidecar the real default the moment the artifact ships, with no further edit here.
    #[test]
    fn with_nothing_set_the_default_is_the_sidecar_in_every_build() {
        for legacy_compiled in [true, false] {
            for available in ALL_AVAILABILITIES {
                assert_eq!(
                    BackendKind::choose(None, false, available, legacy_compiled),
                    Ok(BackendKind::Sidecar)
                );
            }
        }
    }

    /// `VAR=` in a wrapper script means "not set", and selects the sidecar exactly like `None`
    /// does, in every build.
    #[test]
    fn an_empty_variable_is_not_a_choice() {
        for legacy_compiled in [true, false] {
            assert_eq!(
                BackendKind::choose(Some(""), false, Runnable, legacy_compiled),
                Ok(BackendKind::Sidecar)
            );
            assert_eq!(
                BackendKind::choose(Some(""), false, Missing, legacy_compiled),
                Ok(BackendKind::Sidecar)
            );
        }
    }

    /// A typo selects the sidecar with a warning (D10) -- it used to fall back to legacy, which is
    /// exactly the dangerous direction now that legacy is the gated backend. True in every build:
    /// there is no "well this one compiled legacy in, so fall back there" exception left.
    #[test]
    fn an_unrecognized_value_selects_the_sidecar_in_every_build() {
        for legacy_compiled in [true, false] {
            for available in ALL_AVAILABILITIES {
                assert_eq!(
                    BackendKind::choose(Some("sidcar"), false, available, legacy_compiled),
                    Ok(BackendKind::Sidecar)
                );
            }
        }
    }

    /// A release build (legacy not compiled in) refuses `--legacy` and `EITRI_AGENT_BACKEND=legacy`
    /// alike, `Err` naming `LEGACY_NOT_IN_BUILD` and which of the two asked -- and the refusal does
    /// not depend on whether a sidecar artifact happens to be installed.
    #[test]
    fn a_release_build_refuses_legacy_by_flag_or_by_variable() {
        let by_flag = BackendKind::choose(None, true, Runnable, false).unwrap_err();
        assert!(by_flag.message.contains(agent::LEGACY_NOT_IN_BUILD), "{by_flag:?}");
        assert!(by_flag.message.contains("--legacy"), "{by_flag:?}");

        let by_var = BackendKind::choose(Some("legacy"), false, Runnable, false).unwrap_err();
        assert!(by_var.message.contains(agent::LEGACY_NOT_IN_BUILD), "{by_var:?}");
        assert!(by_var.message.contains("EITRI_AGENT_BACKEND=legacy"), "{by_var:?}");

        // Not "an artifact happens to be missing" -- it refuses with one available too, above.
        assert!(BackendKind::choose(None, true, Missing, false).is_err());
    }

    /// A development build (legacy compiled in) grants `--legacy` regardless of the environment,
    /// and the flag beats a conflicting `EITRI_AGENT_BACKEND=sidecar` -- it is the more explicit
    /// act, made on this invocation's own command line, where the env var could be an inherited
    /// leftover nobody meant to set today.
    #[test]
    fn a_development_build_grants_legacy_by_flag_over_a_conflicting_variable() {
        assert_eq!(
            BackendKind::choose(Some("sidecar"), true, Runnable, true),
            Ok(BackendKind::Legacy)
        );
        assert_eq!(BackendKind::choose(None, true, Runnable, true), Ok(BackendKind::Legacy));
        assert_eq!(
            BackendKind::choose(Some("legacy"), false, Runnable, true),
            Ok(BackendKind::Legacy)
        );
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
    ///
    /// Since R07 every offered mode needs the same thing from a backend: its interactive gate. Both
    /// modes run the CLI gated; bypass is Eitri answering `allow`, not a CLI posture, so the
    /// backend's own `bypass_permission_mode` is no longer what honours it. That half is a constant
    /// and is asserted at compile time beside `LEGACY_CAPABILITIES`; what is left here is that
    /// every offered string is a mode at all.
    #[test]
    fn every_offered_mode_is_one_the_legacy_capabilities_actually_claim() {
        for mode in CLIENT_IMPLEMENTED_PERMISSION_MODES {
            let _: PermissionMode = match *mode {
                "auto" => PermissionMode::Auto,
                "bypass" => PermissionMode::Bypass,
                other => panic!("offered permission mode {other:?} has no PermissionMode to map to"),
            };
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
        // The greeting offers "bypass" as a real choice, and since R07 what honours it is the
        // interactive gate (Eitri answers `allow` under it), not a CLI bypass mode the legacy
        // backend no longer has -- both halves asserted at compile time beside the constant.
        let modes: Vec<&str> = greeting.permission_modes.to_vec();
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
            let root = std::env::temp_dir().join(format!("eitri-core-claude-{}", std::process::id()));
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
    /// `recorded_title` is what Eitri itself wrote at write time; `ai_title` is what the CLI
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

    /// Level 1 of the ladder: the CLI's own title is layered ABOVE Eitri's own, at display time.
    /// Both exist here, and the CLI's wins -- which is the owner's ruling (2026-09-20) and the only
    /// thing this change buys, since 37 of this machine's 44 sessions already have one on disk while
    /// Eitri's own `title` only exists for sessions started after 2026-09-19.
    #[test]
    fn the_clis_own_title_is_what_a_row_shows_when_the_transcript_has_one() {
        let row = the_only_row(a_workspace_with(
            Some("what eitri recorded"),
            Some("what the CLI recorded"),
        ));
        assert_eq!(row.title.as_deref(), Some("what the CLI recorded"));
    }

    /// **The negative control, and the whole promise of the fallback** (invariant 17). The same
    /// record with no transcript beside it must read EXACTLY as it did before this change: the
    /// title Eitri recorded, then the bare id. Without this pair, "it degrades back to today's
    /// behaviour" would only be a sentence -- and the degradation is silent, so nothing else would
    /// notice the day the CLI stops writing that line.
    #[test]
    fn with_no_ai_title_a_row_reads_exactly_as_it_did_before() {
        // Level 2: Eitri's own recorded title.
        assert_eq!(
            the_only_row(a_workspace_with(Some("what eitri recorded"), None))
                .title
                .as_deref(),
            Some("what eitri recorded")
        );
        // Level 3: no title at all -- the row falls back to its id and timestamps, and invents
        // nothing.
        assert_eq!(the_only_row(a_workspace_with(None, None)).title, None);
        // And the two really are different answers for the same absence, which is what makes this
        // a control rather than a restatement.
        assert_ne!(
            the_only_row(a_workspace_with(
                Some("what eitri recorded"),
                Some("what the CLI recorded")
            ))
            .title,
            the_only_row(a_workspace_with(Some("what eitri recorded"), None)).title
        );
    }

    /// An `ai-title` this code cannot render drops to the next level rather than rendering blank:
    /// the value is someone else's process's output, normalized through the same
    /// `title_from_prompt` as every other title, and a blank one declines.
    #[test]
    fn an_unusable_ai_title_falls_through_to_the_recorded_one() {
        let row = the_only_row(a_workspace_with(Some("what eitri recorded"), Some("   ")));
        assert_eq!(row.title.as_deref(), Some("what eitri recorded"));
    }

    /// **Invariant 16: nothing read from a transcript is ever persisted.** The record on disk is
    /// byte-identical before and after the picker is built, so the CLI's title cannot collide with
    /// `title`'s own first-one-wins rule and cannot move `updated_at`.
    #[test]
    fn building_the_picker_never_writes_the_clis_title_into_the_record() {
        let workspace = a_workspace_with(Some("what eitri recorded"), Some("what the CLI recorded"));
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
        // And what stayed there is Eitri's own title, unchanged by the one the row showed.
        assert!(
            before
                .iter()
                .any(|(_, bytes)| String::from_utf8_lossy(bytes).contains("what eitri recorded")),
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
        let error = AgentBackend::start(BackendKind::Legacy, Path::new("/tmp"), Some("claude-abc"))
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
        // `crate::`, not `eitri_core::` -- this module IS that crate, and its own unit tests
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
        let conversation = AgentConversation::create(std::sync::Arc::new(RejectingProvider), &dir)
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
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
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
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
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

    /// Drives the pump until what the provider queued has been delivered, or the deadline passes.
    /// The sidecar's events cross the ingestion thread, so the first call after a queue is normally
    /// empty.
    ///
    /// Everything queued, not only the first batch (P1-B2, 2026-09-28): the ingestion thread folds
    /// every 5 ms, so under a loaded test run events queued together can arrive in two deliveries,
    /// and a helper that returned the first left a test's later events undelivered -- a forced 30 ms
    /// pause between two `queue` calls reproduced `left: 1, right: 3` in
    /// `approve_pending_never_answers_a_prompt_only_a_human_answers`. Collected until 100 ms pass
    /// with nothing new, as the panel's own pump keeps taking deliveries; the same fix `deliver()`
    /// got in `5c70398`.
    fn pump_until_delivery(
        backend: &mut AgentBackend,
        project_root: &Path,
        mode: PermissionMode,
    ) -> Vec<AgentDomainEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let quiet = std::time::Duration::from_millis(100);
        let mut delivered = Vec::new();
        let mut last_new: Option<std::time::Instant> = None;
        loop {
            let now = std::time::Instant::now();
            match backend.take_ui_delivery(project_root, mode) {
                UiDelivery::Events(events) if !events.is_empty() => {
                    delivered.extend(events);
                    last_new = Some(now);
                }
                _ if last_new.is_some_and(|at| now.duration_since(at) >= quiet) || now > deadline => return delivered,
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
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-read".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        });
        // One `ToolCallStarted` alongside it, so this also pins that the FILTER is selective: the
        // record of what ran must still reach the transcript.
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "turn-1".into(),
            tool_use_id: "tool-1".into(),
            name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
        });

        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
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
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // fix round 1 (2026-09-28): still cards under item 4A's fast path
            provider_prompt: None,
        });

        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
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
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-escape".into(),
            tool_use_id: Some("tool-3".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "/etc/hostname" }),
            provider_prompt: None,
        });

        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
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
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap());
        for (id, command) in [("perm-rule", "npm ci"), ("perm-compound", "npm ci && rm -rf .")] {
            provider.queue(AgentDomainEvent::PermissionRequested {
                permission_id: id.into(),
                tool_use_id: None,
                tool_name: "Bash".into(),
                input: serde_json::json!({ "command": command }),
                provider_prompt: None,
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut host_answered = BTreeSet::new();
        let delivered = loop {
            match backend.take_ui_delivery_with_rules(&dir, &rules, PermissionMode::Auto, &mut host_answered) {
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
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let mut tracker = crate::attention::AttentionTracker::default();

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-read".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // fix round 1 (2026-09-28): still cards under item 4A's fast path
            provider_prompt: None,
        });
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
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
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
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

    // ---- R07, D12: nothing is answered for a CLI that reported an ungated mode ----

    /// A `Read` inside the project is what the classifier answers `allow` on its own (the test above).
    /// Once the CLI has reported an ungated mode, not even that: the session is about to be closed,
    /// and every request is left for the close to deny -- the one queued behind the report in the
    /// same batch, and one arriving in a later batch (which only the projection's record can catch,
    /// the event having gone by).
    #[test]
    fn after_an_ungated_report_nothing_is_answered_automatically() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let read = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        };

        provider.queue(AgentDomainEvent::UngatedCliMode {
            reported: "acceptEdits".into(),
            detail: "SessionReady".into(),
        });
        provider.queue(read("behind-it"));
        let mut delivered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !delivered
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
        {
            assert!(std::time::Instant::now() < deadline, "timed out: {delivered:?}");
            if let UiDelivery::Events(events) = backend.take_ui_delivery(&dir, PermissionMode::Auto) {
                delivered.extend(events);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            provider.resolutions().is_empty(),
            "a request behind the trip is left for the close to deny: {:?}",
            provider.resolutions()
        );

        provider.queue(read("later"));
        let later = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert!(
            later.iter().any(
                |e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "later")
            ),
            "{later:?}"
        );
        assert!(provider.resolutions().is_empty(), "nor one in a later batch");
        backend.shutdown();
    }

    /// A tripwire riding in the SAME batch as a `Bypass` mode must not be approved either -- D12
    /// outranks R07's bypass rule, not the other way round.
    #[test]
    fn a_tripwire_in_bypass_answers_nothing_either() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let read = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        };

        provider.queue(AgentDomainEvent::UngatedCliMode {
            reported: "acceptEdits".into(),
            detail: "SessionReady".into(),
        });
        provider.queue(read("behind-it"));
        let mut delivered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !delivered
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
        {
            assert!(std::time::Instant::now() < deadline, "timed out: {delivered:?}");
            if let UiDelivery::Events(events) = backend.take_ui_delivery(&dir, PermissionMode::Bypass) {
                delivered.extend(events);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            provider.resolutions().is_empty(),
            "bypass must not allow a request behind the tripwire either: {:?}",
            provider.resolutions()
        );
        backend.shutdown();
    }

    // ---- R07/S2: bypass is Eitri answering every request itself, and `approve_pending` -------

    /// The owner's whole definition of bypass in one assertion: nothing reaches the panel as a
    /// card, and both requests really were answered -- the in-root write and the out-of-root read
    /// alike, since bypass never consults the classifier's path boundary at all. The tool call
    /// itself is still owed to the transcript.
    #[test]
    fn in_bypass_every_request_is_answered_and_never_delivered() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let rules = agent::PrefixRules::default();

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
            provider_prompt: None,
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-outside".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "/etc/hostname" }),
            provider_prompt: None,
        });
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "turn-1".into(),
            tool_use_id: "tool-1".into(),
            name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
        });

        let mut host_answered = BTreeSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let delivered = loop {
            match backend.take_ui_delivery_with_rules(&dir, &rules, PermissionMode::Bypass, &mut host_answered) {
                UiDelivery::Events(events) => break events,
                _ => {
                    assert!(std::time::Instant::now() < deadline, "nothing was delivered");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };

        assert!(
            !delivered
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
            "bypass must show no card at all: {delivered:?}"
        );
        assert!(
            delivered
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. })),
            "the tool call itself is still owed to the transcript: {delivered:?}"
        );
        let mut resolutions = provider.resolutions();
        resolutions.sort();
        assert_eq!(
            resolutions,
            vec![("perm-outside".to_string(), true), ("perm-write".to_string(), true)]
        );
        assert_eq!(
            host_answered,
            BTreeSet::from(["perm-outside".to_string(), "perm-write".to_string()])
        );
        backend.shutdown();
    }

    /// The other half of `host_answered`, in `Auto`: only the id the classifier itself allowed goes
    /// into the set, never one that was delivered as a card.
    #[test]
    fn the_classifier_records_what_it_answers() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let rules = agent::PrefixRules::default();

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-read".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // fix round 1 (2026-09-28): still cards under item 4A's fast path
            provider_prompt: None,
        });

        let mut host_answered = BTreeSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match backend.take_ui_delivery_with_rules(&dir, &rules, PermissionMode::Auto, &mut host_answered) {
                UiDelivery::Events(_) => break,
                _ => {
                    assert!(std::time::Instant::now() < deadline, "nothing was delivered");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
        assert_eq!(host_answered, BTreeSet::from(["perm-read".to_string()]));
        backend.shutdown();
    }

    /// `approve_pending` walks its ids in the order given, skips one no longer pending (already
    /// resolved, or never real) silently, and returns exactly what it answered.
    #[test]
    fn approve_pending_skips_ids_no_longer_pending_and_returns_what_it_answered() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let write = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // fix round 1 (2026-09-28): still cards under item 4A's fast path
            provider_prompt: None,
        };

        provider.queue(write("a"));
        provider.queue(write("b"));
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 2, "both must card in Auto: {delivered:?}");

        provider.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "b".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backend.projection().pending_permissions.contains_key("b") {
            assert!(std::time::Instant::now() < deadline, "b never resolved");
            backend.take_ui_delivery(&dir, PermissionMode::Auto);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            backend.projection().pending_permissions.contains_key("a"),
            "the premise: a is still pending, b is not"
        );

        let ids = vec!["a".to_string(), "b".to_string(), "zzz".to_string()];
        let answered = backend.approve_pending(&ids, "test");
        assert_eq!(answered.ids, vec!["a".to_string()]);
        assert!(
            answered.events.is_empty(),
            "the sidecar's resolution comes through the pump, never from here: {:?}",
            answered.events
        );
        assert_eq!(provider.resolutions(), vec![("a".to_string(), true)]);
        backend.shutdown();
    }

    /// Codex v1-mode finding 1: legacy's `respond_permission` is the only place its
    /// `PermissionResolved` exists (the CLI emits nothing for a hook reply, and `pump()` yields only
    /// wire events), and `approve_pending` used to drop it -- so a card approved by entering bypass
    /// stayed drawn and counted as waiting. Driven through `approve_each` with a legacy-shaped answer,
    /// since a real `AgentBackend::Legacy` needs a real `claude` process (the asymmetry
    /// `user_prompt_event_records_what_was_typed_not_what_went_on_the_wire` records).
    #[test]
    fn approve_pending_keeps_the_resolutions_a_legacy_answer_returns() {
        let pending: std::collections::BTreeMap<String, String> = [("p1", "Write"), ("p2", "Bash"), ("p3", "Edit")]
            .into_iter()
            .map(|(id, tool)| (id.to_string(), tool.to_string()))
            .collect();
        let ids: Vec<String> = ["p1", "gone", "p2", "p3"].iter().map(|s| s.to_string()).collect();
        let mut asked = Vec::new();
        let approved = approve_each(&ids, &pending, "test", |id| {
            asked.push(id.to_string());
            if id == "p2" {
                // Fails toward a card: not answered, not hidden, no event.
                return Err(BackendError {
                    message: "the provider refused the answer".into(),
                    benign: true,
                    folded_events: Vec::new(),
                });
            }
            Ok(vec![AgentDomainEvent::PermissionResolved {
                permission_id: id.to_string(),
                outcome: agent::PermissionOutcome::Allowed,
            }])
        });
        assert_eq!(
            asked,
            vec!["p1", "p2", "p3"],
            "an id that is no longer pending is never answered"
        );
        assert_eq!(approved.ids, vec!["p1".to_string(), "p3".to_string()]);
        assert_eq!(
            approved.events,
            vec![
                AgentDomainEvent::PermissionResolved {
                    permission_id: "p1".into(),
                    outcome: agent::PermissionOutcome::Allowed,
                },
                AgentDomainEvent::PermissionResolved {
                    permission_id: "p3".into(),
                    outcome: agent::PermissionOutcome::Allowed,
                },
            ],
            "every resolution the answers returned, in order, for the caller to hand the panel"
        );
    }

    /// D12: `TabSet::confirm_bypass` calls `approve_pending` directly, off the 33ms pump that would
    /// otherwise catch an `UngatedCliMode` report and fail the tab first -- so this function must hold
    /// the same rule on its own once the background ingestion has folded the report into the
    /// projection, even before any pump has turned it into a `Failed` tab.
    #[test]
    fn approve_pending_answers_nothing_once_the_cli_reported_an_ungated_mode() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));
        let write = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // fix round 1 (2026-09-28): still cards under item 4A's fast path
            provider_prompt: None,
        };

        provider.queue(write("still-pending"));
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 1, "the premise: it really is a delivered card");

        provider.queue(AgentDomainEvent::UngatedCliMode {
            reported: "bypassPermissions".into(),
            detail: "PermissionModeChanged".into(),
        });
        // Folded by the background ingestion thread; no pump call here, deliberately -- this is the
        // window between the report landing in the projection and the next `TabSet::pump` tick.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backend.projection().ungated_cli_mode.is_none() {
            assert!(std::time::Instant::now() < deadline, "the report was never folded");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let answered = backend.approve_pending(&["still-pending".to_string()], "on entering bypass");
        assert_eq!(answered, Approved::default());
        assert!(
            provider.resolutions().is_empty(),
            "nothing was answered on the host's side: {:?}",
            provider.resolutions()
        );
        backend.shutdown();
    }

    /// Fail toward a card: a bypass allow the provider refuses is not silently lost -- the event
    /// stays in the delivery.
    #[test]
    fn a_failed_allow_in_bypass_draws_the_card() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        provider.refuse_resolutions(true);
        let conversation = AgentConversation::create(provider.clone(), &dir).unwrap();
        let mut backend = AgentBackend::Sidecar(Box::new(conversation));

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
            provider_prompt: None,
        });
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Bypass);
        assert!(
            delivered.iter().any(
                |e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "perm-write")
            ),
            "a failed allow in bypass must fall back to a card: {delivered:?}"
        );
        assert!(provider.resolutions().is_empty());
        backend.shutdown();
    }

    // ---- O3: the CLI's own permission prompts (Verdandi b3aa188) ----------------------------------

    /// The CLI's own prompt for `tool_use_id`, as the sidecar translation produces it.
    fn provider_prompt(
        permission_id: &str,
        tool_use_id: Option<&str>,
        tool_name: &str,
        input: serde_json::Value,
        ask_rule: Option<&str>,
    ) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: tool_use_id.map(str::to_string),
            tool_name: tool_name.into(),
            input,
            provider_prompt: Some(agent::ProviderPrompt {
                reason: Some("Claude requested permissions to edit /p/.git/probe which is a sensitive file.".into()),
                description: Some(".git/probe".into()),
                blocked_path: None,
                matched_ask_rule: ask_rule.map(|content| agent::MatchedAskRule {
                    source: "projectSettings".into(),
                    tool_name: tool_name.into(),
                    rule_content: Some(content.into()),
                }),
                unrecognized_origin: None,
            }),
        }
    }

    fn probe_write() -> serde_json::Value {
        serde_json::json!({ "file_path": ".git/probe", "content": "o3" })
    }

    /// One delivery through the O3 entry point, with the tab's human approvals; returns what was
    /// delivered and the CLI prompts answered without a card (for the row notes).
    fn deliver(
        backend: &mut AgentBackend,
        dir: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
    ) -> (Vec<AgentDomainEvent>, Vec<PromptAnsweredForYou>) {
        // Everything the provider queued, not only the first batch: the ingestion thread folds every
        // 5 ms, so under a loaded test run the queued events can arrive in two deliveries, and a
        // helper that returned the first one left a test's later events undelivered (seen failing
        // about one run in three in the full suite, never alone). Collected until 100 ms pass with
        // nothing new, as the panel's own pump keeps taking deliveries.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let quiet = std::time::Duration::from_millis(100);
        let mut answered = AnsweredForYou::default();
        let mut delivered = Vec::new();
        let mut last_new: Option<std::time::Instant> = None;
        loop {
            let now = std::time::Instant::now();
            match backend.take_ui_delivery_with_approvals(dir, rules, mode, host_answered, approvals, &mut answered) {
                UiDelivery::Events(events) if !events.is_empty() => {
                    delivered.extend(events);
                    last_new = Some(now);
                }
                _ if last_new.is_some_and(|at| now.duration_since(at) >= quiet) || now > deadline => {
                    return (delivered, answered.prompts)
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    }

    fn sidecar_backend(dir: &Path) -> (std::sync::Arc<RecordingProvider>, AgentBackend) {
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), dir).unwrap();
        (provider, AgentBackend::Sidecar(Box::new(conversation)))
    }

    fn carded(delivered: &[AgentDomainEvent], id: &str) -> bool {
        delivered
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == id))
    }

    /// O3 ruling 3: a `Read` inside the project is what the classifier allows unasked -- as the
    /// gate's request. The CLI's own prompt for the very same call never goes through the classifier:
    /// in Auto, with no human approval of that call, it is a card.
    #[test]
    fn the_classifier_never_answers_the_clis_own_prompt() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        provider.queue(provider_prompt(
            "perm-prov",
            Some("tu-1"),
            "Read",
            serde_json::json!({ "file_path": "main.rs" }),
            None,
        ));
        let mut host_answered = BTreeSet::new();
        let (delivered, _) = deliver(
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut host_answered,
            &mut HumanApprovals::default(),
        );
        assert!(carded(&delivered, "perm-prov"), "{delivered:?}");
        assert!(provider.resolutions().is_empty(), "{:?}", provider.resolutions());
        assert!(host_answered.is_empty());
        backend.shutdown();
    }

    /// O3 ruling 3, the saved-rule half: a prefix rule that answers `npm ci` for the gate does not
    /// answer the CLI's own prompt about the same command.
    #[test]
    fn a_saved_rule_never_answers_the_clis_own_prompt() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap());
        provider.queue(provider_prompt(
            "perm-prov",
            Some("tu-1"),
            "Bash",
            serde_json::json!({ "command": "npm ci" }),
            None,
        ));
        let (delivered, _) = deliver(
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert!(carded(&delivered, "perm-prov"), "{delivered:?}");
        assert!(provider.resolutions().is_empty());
        backend.shutdown();
    }

    /// O3 ruling 4: bypass allows the CLI's own prompt with no card, as a real bypassPermissions
    /// session runs the call -- says so in `host_answered`, like every answer bypass makes, and hands
    /// back the note its row shows (review item 7).
    #[test]
    fn bypass_allows_the_clis_own_prompt_without_a_card() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        provider.queue(provider_prompt("perm-prov", Some("tu-1"), "Write", probe_write(), None));
        provider.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "turn-1".into(),
            tool_use_id: "tu-1".into(),
            name: "Write".into(),
            input: probe_write(),
        });
        let mut host_answered = BTreeSet::new();
        let (delivered, answered) = deliver(
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Bypass,
            &mut host_answered,
            &mut HumanApprovals::default(),
        );
        assert!(!carded(&delivered, "perm-prov"), "{delivered:?}");
        assert_eq!(provider.resolutions(), vec![("perm-prov".to_string(), true)]);
        assert_eq!(host_answered, BTreeSet::from(["perm-prov".to_string()]));
        assert_eq!(
            answered,
            vec![PromptAnsweredForYou {
                tool_use_id: "tu-1".into(),
                note: "Claude Code safety check — allowed in bypass".into(),
            }]
        );
        backend.shutdown();
    }

    /// O3 ruling 4's exception, and review #3: the user's own `permissions.ask` rule forced this
    /// prompt, or its kind is unknown to this build -- a card in bypass too, and in Auto even after
    /// the human approved the gate's card for exactly this call.
    #[test]
    fn a_prompt_only_a_human_answers_is_a_card_in_every_mode() {
        let dir = a_workspace_holding_one_file();
        let cat = serde_json::json!({ "command": "cat notes.txt" });
        let mut unknown = provider_prompt("perm-unknown", Some("tu-1"), "Bash", cat.clone(), None);
        if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut unknown {
            provider_prompt.as_mut().unwrap().unrecognized_origin = Some(7);
        }
        let ruled = provider_prompt("perm-ask", Some("tu-1"), "Bash", cat.clone(), Some("cat:*"));
        for (event, id) in [(ruled, "perm-ask"), (unknown, "perm-unknown")] {
            for mode in [PermissionMode::Bypass, PermissionMode::Auto] {
                let (provider, mut backend) = sidecar_backend(&dir);
                provider.queue(event.clone());
                let mut approvals = HumanApprovals::default();
                approvals.record("tu-1", "Bash", &cat);
                let mut host_answered = BTreeSet::new();
                let (delivered, answered) = deliver(
                    &mut backend,
                    &dir,
                    &agent::PrefixRules::default(),
                    mode,
                    &mut host_answered,
                    &mut approvals,
                );
                assert!(carded(&delivered, id), "{id} {mode:?}: {delivered:?}");
                assert!(provider.resolutions().is_empty(), "{id} {mode:?}");
                assert!(host_answered.is_empty() && answered.is_empty(), "{id} {mode:?}");
                assert!(
                    approvals.contains("tu-1"),
                    "{id} {mode:?}: an approval a card kept is not used up"
                );
                backend.shutdown();
            }
        }
    }

    /// Item 4A fix round 2 (2026-09-28): the acceptEdits fast path answers the GATE's request for an
    /// in-project `Write`, and nothing more. When the user's own `permissions.ask` rule
    /// (`Edit(notes.txt)`) makes the CLI raise its own prompt for that same call after the gate's
    /// `allow` -- CLI 2.1.283's `EQn` runs `DR` after a hook allow, and `DR` returns an ask a rule
    /// forced -- that prompt is a card in Auto. So is the CLI's own prompt for the call with no rule
    /// behind it: the fast path's allow is not a human approval (`HumanApprovals` stays empty).
    /// Before 4A every edit carded here anyway; now this chain is what keeps a user's ask rule for an
    /// edit from being answered silently. Bypass is covered by the test above.
    #[test]
    fn a_users_own_ask_rule_still_cards_an_edit_the_fast_path_allowed() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        let write = serde_json::json!({ "file_path": "notes.txt", "content": "fast path" });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-gate".into(),
            tool_use_id: Some("tu-1".into()),
            tool_name: "Write".into(),
            input: write.clone(),
            provider_prompt: None,
        });
        provider.queue(provider_prompt(
            "perm-ruled",
            Some("tu-1"),
            "Write",
            write.clone(),
            Some("notes.txt"),
        ));
        provider.queue(provider_prompt("perm-cli", Some("tu-1"), "Write", write, None));
        let mut host_answered = BTreeSet::new();
        let mut approvals = HumanApprovals::default();
        let (delivered, answered) = deliver(
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut host_answered,
            &mut approvals,
        );
        assert!(
            !carded(&delivered, "perm-gate"),
            "the gate's request is the fast path's to answer: {delivered:?}"
        );
        assert!(
            carded(&delivered, "perm-ruled"),
            "the user's ask rule wins: {delivered:?}"
        );
        assert!(
            carded(&delivered, "perm-cli"),
            "the fast path is no human approval: {delivered:?}"
        );
        assert_eq!(provider.resolutions(), vec![("perm-gate".to_string(), true)]);
        assert_eq!(host_answered, BTreeSet::from(["perm-gate".to_string()]));
        assert!(approvals.is_empty() && answered.is_empty());
        backend.shutdown();
    }

    /// O3 ruling 5, as the review tightened it: in Auto the human's approval answers the CLI's own
    /// prompt only for the SAME call -- same non-empty tool-use id, same tool, same input the card
    /// showed -- and only once. Another call's approval, another tool or input under the approved id,
    /// no id at all, and a second prompt after the approval was used are all cards.
    #[test]
    fn in_auto_an_approval_answers_one_prompt_for_exactly_the_call_it_approved() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        let other_input = serde_json::json!({ "file_path": ".git/hooks/pre-commit", "content": "evil" });
        provider.queue(provider_prompt(
            "perm-input",
            Some("tu-approved"),
            "Write",
            other_input,
            None,
        ));
        provider.queue(provider_prompt(
            "perm-tool",
            Some("tu-approved"),
            "Bash",
            serde_json::json!({ "command": "cp evil .git/hooks/pre-commit" }),
            None,
        ));
        // The matching prompt comes AFTER the mismatches, so an id-only comparison would have let
        // one of them take the approval first.
        provider.queue(provider_prompt(
            "perm-same",
            Some("tu-approved"),
            "Write",
            probe_write(),
            None,
        ));
        provider.queue(provider_prompt(
            "perm-again",
            Some("tu-approved"),
            "Write",
            probe_write(),
            None,
        ));
        provider.queue(provider_prompt(
            "perm-other",
            Some("tu-other"),
            "Write",
            probe_write(),
            None,
        ));
        provider.queue(provider_prompt("perm-noid", None, "Write", probe_write(), None));
        let mut approvals = HumanApprovals::default();
        approvals.record("tu-approved", "Write", &probe_write());
        let mut host_answered = BTreeSet::new();
        let (delivered, answered) = deliver(
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut host_answered,
            &mut approvals,
        );
        assert!(!carded(&delivered, "perm-same"), "{delivered:?}");
        for id in ["perm-again", "perm-input", "perm-tool", "perm-other", "perm-noid"] {
            assert!(carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert_eq!(provider.resolutions(), vec![("perm-same".to_string(), true)]);
        assert_eq!(host_answered, BTreeSet::from(["perm-same".to_string()]));
        assert!(approvals.is_empty(), "used up by the one prompt it answered");
        assert_eq!(
            answered,
            vec![PromptAnsweredForYou {
                tool_use_id: "tu-approved".into(),
                note: "Claude Code safety check — allowed with your approval".into(),
            }]
        );
        backend.shutdown();
    }

    /// A prompt that gave no reason is not called a safety check on its row either (review #5).
    #[test]
    fn a_prompt_with_no_reason_is_noted_neutrally() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        let mut event = provider_prompt("perm-prov", Some("tu-1"), "Write", probe_write(), None);
        if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut event {
            provider_prompt.as_mut().unwrap().reason = None;
        }
        provider.queue(event);
        let (_, answered) = deliver(
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Bypass,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert_eq!(answered[0].note, "Claude Code asked — allowed in bypass");
        backend.shutdown();
    }

    /// Fail toward a card, as every automatic answer here does: an allow of the CLI's own prompt that
    /// the provider refuses stays in the delivery -- and an approval it would have used is kept.
    #[test]
    fn a_failed_allow_of_the_clis_own_prompt_draws_the_card() {
        let dir = a_workspace_holding_one_file();
        for mode in [PermissionMode::Bypass, PermissionMode::Auto] {
            let (provider, mut backend) = sidecar_backend(&dir);
            provider.refuse_resolutions(true);
            provider.queue(provider_prompt("perm-prov", Some("tu-1"), "Write", probe_write(), None));
            let mut approvals = HumanApprovals::default();
            approvals.record("tu-1", "Write", &probe_write());
            let mut host_answered = BTreeSet::new();
            let (delivered, answered) = deliver(
                &mut backend,
                &dir,
                &agent::PrefixRules::default(),
                mode,
                &mut host_answered,
                &mut approvals,
            );
            assert!(carded(&delivered, "perm-prov"), "{mode:?}: {delivered:?}");
            assert!(host_answered.is_empty() && answered.is_empty(), "{mode:?}");
            assert!(approvals.contains("tu-1"), "{mode:?}");
            backend.shutdown();
        }
    }

    /// `approve_pending` (entering bypass with cards waiting, and the bypass resync sweep) answers a
    /// list in one go; a prompt only a human answers -- an ask rule's, or one of unknown kind -- is
    /// never in what it answers.
    #[test]
    fn approve_pending_never_answers_a_prompt_only_a_human_answers() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        provider.queue(provider_prompt(
            "perm-ask",
            Some("tu-1"),
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        let mut unknown = provider_prompt("perm-unknown", Some("tu-3"), "Write", probe_write(), None);
        if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut unknown {
            provider_prompt.as_mut().unwrap().unrecognized_origin = Some(7);
        }
        provider.queue(unknown);
        provider.queue(provider_prompt(
            "perm-plain",
            Some("tu-2"),
            "Write",
            probe_write(),
            None,
        ));
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 3, "all card in Auto with no approval: {delivered:?}");
        let answered = backend.approve_pending(
            &[
                "perm-ask".to_string(),
                "perm-unknown".to_string(),
                "perm-plain".to_string(),
            ],
            "test",
        );
        assert_eq!(answered.ids, vec!["perm-plain".to_string()]);
        assert_eq!(provider.resolutions(), vec![("perm-plain".to_string(), true)]);
        backend.shutdown();
    }
}
