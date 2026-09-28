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
    // This backend always passes `--permission-mode default` and the hook.
    // Bypass works by answering `allow` under that gate, so it needs only
    // the interactive permission capability below.
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
    // Every offered mode uses the interactive gate. Bypass answers `allow` under it;
    // this backend never starts the CLI ungated.
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
    /// Constructs the selected backend. A cold sidecar checkout runs `npm ci` and `npm run build`,
    /// which can block for minutes. Call this off the GTK main thread, as `agent_panel`'s connect worker does.
    /// `resume` carries the provider session id to continue, or `None` for a fresh session.
    /// A resume that cannot be honored returns an error so it cannot silently start a fresh session.
    ///
    /// Every session on both backends is gated. The tab's permission mode is read when answering
    /// requests in `take_ui_delivery_with_rules`, rather than sent to the CLI.
    pub fn start(kind: BackendKind, project_dir: &Path, resume: Option<&str>) -> Result<Self, BackendError> {
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
                    Some(provider_session_id) => {
                        AgentConversation::resume(std::sync::Arc::new(provider), project_dir, provider_session_id)
                            .map(|c| AgentBackend::Sidecar(Box::new(c)))
                            .map_err(|e| BackendError::fatal(format!("could not continue the previous session: {e}")))
                    }
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

    /// Reports the server's advertised capabilities intersected with those this client implements.
    /// `AgentConversation` and `ClaudeSidecarProvider` apply that intersection.
    ///
    /// Legacy advertises nothing over a wire, so its capabilities describe its implementation:
    /// it supports interruption, has no resume or fork, and runs every session gated.
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

    /// A session tab's rename. Legacy writes no records, so its name lives only in the
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
    /// `Tab::mode`: in `Bypass` every request is answered here, never only what
    /// the classifier would allow in `Auto`. Builds a throwaway `host_answered` set -- a caller that
    /// wants the ids kept (every real caller does) uses `take_ui_delivery_with_rules` directly.
    pub fn take_ui_delivery(&mut self, project_root: &Path, mode: PermissionMode) -> UiDelivery {
        let mut discarded = BTreeSet::new();
        self.take_ui_delivery_with_rules(project_root, &agent::PrefixRules::default(), mode, &mut discarded)
    }

    /// `take_ui_delivery` with the project's prefix rules (`agent::permission_rules`) applied by
    /// the classifier in `Auto`. A rule can replace only the two "not on the read-only list"
    /// verdicts, never a fail-closed verdict; see `agent::classify_with_rules`.
    /// `host_answered` collects ids answered through bypass or classification so `TabSet::pump`
    /// can hide them from the tray and later snapshots.
    ///
    /// This method has no human approvals, so in `Auto` the CLI's own prompt always becomes a card.
    /// The tab set has those approvals and calls `take_ui_delivery_with_approvals`.
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
            &mut Vec::new(),
        )
    }

    /// `take_ui_delivery_with_rules`, plus the tab's `approvals` from `TabSet::answer_card`.
    /// In `Auto`, a CLI prompt matching an approval's non-empty tool-use id, tool and exact input
    /// is answered `allow` once and consumes that approval, which authorizes only that call.
    /// `answered_for_you` receives every CLI prompt answered without a card, for the muted note
    /// on its call's row.
    pub fn take_ui_delivery_with_approvals(
        &mut self,
        project_root: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
        answered_for_you: &mut Vec<PromptAnsweredForYou>,
    ) -> UiDelivery {
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
                    UiDelivery::Nothing
                } else {
                    UiDelivery::Events(kept)
                }
            }
            // A `Resync` carries no events to filter; the UI rebuilds from the projection.
            // `TabSet::pump` handles bypass requests in its `Resync` arm and hides ids in `host_answered`.
            other => other,
        }
    }

    /// Answers `PermissionRequested` events that need no human and returns events the UI should see.
    ///
    /// Both backends use a `PreToolUse` gate with matcher `*`. The legacy backend generates the
    /// hook; the sidecar uses its broker. Permission policy is applied here so a gate request
    /// need not show a user card for every call.
    ///
    /// A successfully answered request is removed from delivery, so no permission card is drawn.
    /// `ToolCallStarted` and `ToolCallCompleted` still reach the transcript to show what ran.
    /// If sending the answer fails, the request is delivered so the user can resolve it.
    ///
    /// For gate requests, `Bypass` sends `Allow` before classification. The CLI remains gated,
    /// using `--permission-mode default` with the hook or the sidecar's INTERACTIVE mode.
    /// The caller supplies `Tab::mode` on every pump; this function stores no mode of its own.
    /// An id already in `host_answered` is dropped without answering it again.
    ///
    /// Nothing is answered after `UngatedCliMode`, whether it appears in this batch or was
    /// already folded into the projection. This applies in both modes. `TabSet::pump` closes
    /// such sessions and thereby denies pending requests; sending `Allow` here would grant
    /// permission to a session that is about to close.
    ///
    /// The legacy backend's synchronous resolution is dropped along with its request.
    /// The sidecar's `PermissionResolved` arrives on a later pump and is delivered even if
    /// its request was hidden. Filtering `pendingPermissions` by that unseen id is a no-op.
    /// `TabSet::pump` uses `host_answered` in `SnapshotView::of` and `AttentionTracker::resync`
    /// to hide answered requests from resync snapshots and the attention tray. This prevents
    /// an automatically answered request from reappearing as a card in either mode.
    ///
    /// CLI prompts with `provider_prompt: Some` are handled before gate requests. They can
    /// follow a gate approval because CLI checks, such as sensitive-file checks, are not
    /// silenced by a hook's `allow` or a session rule. The classifier and saved prefix rules
    /// are not used for these prompts. `Bypass` allows them; `Auto` requires an entry in
    /// `approvals` matching the call's id, tool and input, and consumes that approval.
    /// Otherwise the prompt is delivered as a card. A prompt marked `needs_a_human`, either
    /// forced by `permissions.ask` or of an unknown kind, is a card in every mode.
    /// Each prompt answered without a card adds its row note to `answered_for_you`.
    fn answer_what_needs_no_human(
        &mut self,
        events: Vec<AgentDomainEvent>,
        project_root: &Path,
        cx: AnswerContext<'_>,
    ) -> Vec<AgentDomainEvent> {
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
        for event in events {
            tripped |= matches!(event, AgentDomainEvent::UngatedCliMode { .. });
            let AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                tool_name,
                input,
                provider_prompt,
            } = &event
            else {
                kept.push(event);
                continue;
            };
            if tripped {
                eprintln!("[permission] not answering {tool_name}: the CLI reported an ungated mode");
                kept.push(event);
                continue;
            }
            if let Some(prompt) = provider_prompt {
                // O3: never the classifier's or a rule's. The CLI's sentence embeds the
                // model's own path and a rule can come from a file in the repository, so both are
                // printed escaped (`{:?}`, review #2): they cannot forge or garble a log line.
                if prompt.needs_a_human() {
                    eprintln!(
                        "[permission] asking the user: {tool_name} (the CLI's own prompt, only you answer it: {:?})",
                        prompt.label()
                    );
                    kept.push(event);
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
                    kept.push(event);
                    continue;
                };
                let (permission_id, tool_name) = (permission_id.clone(), tool_name.clone());
                let (tool_use_id, label) = (tool_use_id.clone(), prompt.label());
                match self.respond_permission(&permission_id, PermissionDecision::Allow) {
                    Ok(_resolution) => {
                        host_answered.insert(permission_id);
                        if let Some(id) = tool_use_id {
                            // Used up: a later prompt under this id is a
                            // card. In bypass this is a no-op -- nothing was consulted.
                            approvals.consume(&id);
                            answered_for_you.push(PromptAnsweredForYou {
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
                        kept.push(event);
                    }
                }
                continue;
            }
            if mode == PermissionMode::Bypass {
                if host_answered.contains(permission_id) {
                    // Already allowed once. Drop the duplicate delivery rather than answering
                    // the same id twice.
                    continue;
                }
                let (permission_id, tool_name) = (permission_id.clone(), tool_name.clone());
                match self.respond_permission(&permission_id, PermissionDecision::Allow) {
                    Ok(_resolution) => {
                        host_answered.insert(permission_id);
                        eprintln!("[permission] allowed in bypass: {tool_name}");
                    }
                    Err(error) => {
                        eprintln!(
                            "[permission] could not allow {tool_name} in bypass, showing a card instead: {}",
                            error.message
                        );
                        kept.push(event);
                    }
                }
                continue;
            }
            let classification = agent::classify_with_rules(tool_name, input, project_root, rules);
            if classification.needs_a_human() {
                // The other half of the line below, added 2026-09-19 (later) because its absence
                // cost a diagnosis: auto mode was still a wall of popups, and
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
                    host_answered.insert(permission_id);
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

    /// Answers `allow` to ids still pending on this session, in the supplied order (sorted by
    /// `seq` by the caller). Skips ids already resolved or unknown. Returns only successfully
    /// answered ids so the caller can extend `host_answered`. `why` labels the log message.
    ///
    /// Returns those ids and the resulting events in `Approved`. Legacy's CLI emits no event
    /// for a hook reply, so `AgentSession::respond_permission` returns its locally folded
    /// `PermissionResolved`. Deliver it to remove the approved card and its waiting count.
    /// Sidecar resolutions arrive through the pump.
    ///
    /// Collects pending requests and drops the projection guard before answering.
    /// `projection()` holds the sidecar ingestion mutex and `respond_permission` locks it
    /// again, so retaining the guard would deadlock.
    ///
    /// If `respond_permission` fails, the id is omitted so its card remains available to the
    /// user. If the user answers concurrently, the provider refuses the duplicate answer and
    /// the error is logged.
    ///
    /// Answers nothing after the CLI reports an ungated mode. `TabSet::pump` checks this
    /// before a resync, but `TabSet::confirm_bypass` calls directly between pumps. The
    /// projection can already contain the report before the tab is marked `Failed`.
    ///
    /// Prompts forced by `permissions.ask` and unknown prompt kinds remain cards even in
    /// bypass. `TabSet::waiting_cards` excludes them from the bypass count and list so the
    /// confirmation count matches what is approved. This check also protects other callers.
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
    answered_for_you: &'a mut Vec<PromptAnsweredForYou>,
}

/// The calls the user approved on a card in one tab, each kept with the tool and the exact input the
/// card showed. `TabSet::answer_card` records
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

/// A CLI prompt answered without a card, identified by tool-use id, and the note displayed on
/// its call's row. `ProviderPrompt::label` identifies the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAnsweredForYou {
    pub tool_use_id: String,
    pub note: String,
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

/// Sets a picker row's display title from the CLI transcript, falling back to the stored
/// session title, then no title.
///
/// This only changes the value sent to the frontend. It does not write to disk, so
/// `ConversationRecord::title` keeps its first-one-wins behavior and `updated_at` is unchanged.
/// Each greeting reads the transcript again, so resuming refreshes the displayed title.
///
/// If the transcript cannot be found or read, has no `ai-title` within `AI_TITLE_TAIL_BYTES`,
/// or has a title this code cannot render, `session.title` is left unchanged. No title is
/// invented when neither source supplies one.
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
    /// Previous conversations in this workspace that can be resumed, newest first.
    /// Empty when there is nothing to continue, as in a fresh workspace.
    ///
    /// Entries require a persisted provider session id, a provider that advertised `resume`
    /// when the session was saved, and a backend kind that implements resume.
    /// The saved advertisement is used because no provider exists yet at the start screen.
    /// `ClaudeSidecarProvider::resume_session` checks the live handshake when resume is
    /// attempted. If support has disappeared, resume fails instead of starting a fresh session.
    ///
    /// `for_kind` reads the CLI's `type:"ai-title"` transcript entry through
    /// `agent::transcript::newest_ai_title` on every greeting. This display title takes
    /// precedence over the stored title without writing to disk. A row without a title
    /// shows its session id and timestamps rather than an invented label.
    pub resumable: Vec<ResumableSession>,
    /// The configured Claude account's name (`agent::account::configured`), `null` when none is
    /// set. Read, never written, here -- the same asymmetry as
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

    /// Checks that every offered string maps to a permission mode. Both modes require the
    /// interactive gate: bypass is implemented by answering `allow` locally. The gate and
    /// `bypass_permission_mode` capabilities are asserted at compile time beside
    /// `LEGACY_CAPABILITIES`.
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
        // The resume and fork capabilities are asserted at compile time beside the constant.
        // The greeting still offers bypass because the client answers `allow` through the
        // interactive gate; legacy needs no CLI bypass mode. Those capabilities are also
        // asserted beside the constant.
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

    // ---- The picker's title ladder ---------------------------------------------------------

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

    /// The CLI's transcript title takes precedence over the application's stored `title`
    /// when both are present in a session row.
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

    // ---- A rejected send still leaves the prompt recorded ----

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

    /// Folding the prompt before sending keeps the user's input in the projection even
    /// when the sidecar rejects the turn.
    ///
    /// This test covers the sidecar. Rejection in `AgentSession::send_turn` requires a live
    /// session with a turn already in flight. Creating one through `AgentSession::start`
    /// and `AgentProcess::spawn` launches a real `claude` process. The legacy path has no
    /// injectable provider like `AgentConversation`, so it has no equivalent test here.
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

    /// A rename before the session exists is included in the record written during adoption.
    /// A later rename is written into the record by the ingestion thread.
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

    /// Drives the pump until all queued events arrive or the deadline passes.
    /// Sidecar events cross the ingestion thread, so the first pump after a queue is normally empty.
    /// The thread folds every 5 ms, and events queued together can arrive in separate deliveries.
    /// Collects until 100 ms pass with nothing new so later events are not left undelivered.
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

    /// A `Read` of a file inside the project produced a
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
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
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

    /// A saved rule is applied at the shared backend permission handler. A compound command
    /// still reaches the user even when its first command matches the rule.
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

    /// A hidden chat still answers requests that need no human and counts the card it holds.
    /// The panel's 33 ms pump timer ignores visibility, so the tracker uses `on_screen = false`.
    /// The projection holds both pending requests until the provider's resolution arrives, including
    /// the automatically answered `Read`. The tracker counts only the card delivered to the panel.
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
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
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

    // ---- No automatic answers after the CLI reports an ungated mode -----------------------

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

    /// An ungated-mode tripwire prevents approvals even when bypass is reported in the same
    /// batch. The tripwire takes precedence over bypass.
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

    // ---- Bypass answers requests locally; `approve_pending` handles pending requests --------

    /// Bypass in one assertion: nothing reaches the panel as a
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
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
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
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
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

    /// Legacy's `respond_permission` returns the only `PermissionResolved` for a hook reply;
    /// the CLI emits none and the pump only yields wire events. `approve_pending` must
    /// preserve it so an approved card and its waiting count clear. Tests `approve_each` with
    /// a legacy-shaped answer because a real legacy backend requires a `claude` process.
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

    /// `TabSet::confirm_bypass` calls `approve_pending` between 33ms pumps. The function must
    /// check for an `UngatedCliMode` already folded by background ingestion, before the next
    /// pump marks the tab `Failed`.
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
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
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

    // ---- The CLI's own permission prompts ----

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
        let mut answered = Vec::new();
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
                    return (delivered, answered)
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

    /// The classifier allows an in-project `Read` gate request without a card.
    /// The CLI's own prompt for the same call bypasses classification and becomes a card in `Auto`
    /// when there is no human approval for that call.
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

    /// A saved prefix rule can allow `npm ci` at the gate.
    /// It cannot answer the CLI's own prompt for the same command.
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

    /// Bypass allows the CLI's own prompt without a card.
    /// The answer enters `host_answered` and returns the note shown on the call's row.
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

    /// A prompt forced by `permissions.ask`, or of an unknown kind, remains a card in both modes.
    /// In `Auto` it remains a card even after the user approved the gate for the exact same call.
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

    /// In `Auto`, a human approval answers one CLI prompt for the exact call approved on the card:
    /// the same non-empty tool-use id, tool and input. A different id, tool or input, a missing id,
    /// or another prompt after the approval is consumed requires a card.
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
