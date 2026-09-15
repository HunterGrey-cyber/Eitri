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
//! **The lease and the persisted record are wired in here**, and their ORDERING differs between the
//! two entry points in a way that is easy to get backwards:
//!
//! - `create` cannot take a lease up front. The lease key includes `provider_session_id`, and that
//!   value does not exist until the provider's first `SessionOpened` arrives -- so the lease is
//!   acquired, and the record written, on the ingestion thread when that event is folded.
//! - `resume` knows the id before it calls anything, so it probes, acquires, and only THEN asks the
//!   provider. Acquiring after would leave a window where two clients have both started resuming
//!   the same Claude session, which is the exact hazard the lease exists to prevent.
//!
//! **There is deliberately no lease on the DIRECTORY.** One was written and withdrawn on
//! 2026-09-15: it delivered prohibition (a second Neovibe window in one project refused to start)
//! where the requirement is isolation, and it broke `agent`'s own security baseline test, which now
//! asserts that two sessions in one directory each see only their own `PreToolUse` hook. That
//! isolation is real and lives elsewhere -- the hook config travels in the CLI's own argv
//! (`--settings`, see `agent::settings`), not in a `.claude/` file two sessions could share. The
//! cross-window question "is another window running an agent on this project?" is the `supervisor`
//! dashboard's job, not a lock's.
//!
//! The lease is ADVISORY and nothing here says otherwise (design doc §8.5, §17.7). It binds
//! processes that take part in this protocol; a `claude` someone runs by hand in the same directory
//! takes no lease and is not stopped, and Claude's own documentation records that two clients
//! resuming one session interleave a single transcript.

use crate::lease::{LeaseError, SessionLease};
use crate::persistence::{save_conversation_record, ConversationRecord};
use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, PermissionDecision,
    ProviderCapabilities, ProviderError, ProviderInfo, ResolvePermissionRequest, SendTurnRequest,
};
use crate::ingestion::ConversationIngest;
use crate::projection::{AgentDomainEvent, PermissionOutcome};
use std::sync::Arc;
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
    /// The provider accepted the resume request and then terminated the session -- the usual cause
    /// is a provider session id that no longer exists. Reported as a failed resume, never as a
    /// session: handing back a dead conversation is the silent-substitution failure this whole
    /// phase exists to prevent.
    ResumeRejected { provider_session_id: String, reason: String },
    /// The provider opened a session with a DIFFERENT provider session id than the one asked for.
    ///
    /// This is the silent substitution itself, caught in the act. A resume that comes back as some
    /// other session is the exact failure the whole resume protocol exists to prevent: the
    /// conversation would look continued, carry none of the history, and -- worse -- get the
    /// requested id persisted against it, so every later resume of this workspace would chase a
    /// session that was never there.
    ///
    /// Kept distinct from `ResumeRejected` because the two say different things about the provider.
    /// A rejection is a provider being honest about a session it cannot continue; this is a
    /// provider substituting one. Collapsing them would lose exactly the distinction worth alerting
    /// on.
    ResumeIdentityMismatch { requested: String, actual: String },
    /// Another Neovibe-participating client already holds this provider session.
    ///
    /// Advisory, and honestly so: the lease binds clients that take part in this protocol. A raw
    /// `claude --resume` started outside Neovibe is NOT blocked by it, and this crate never claims
    /// otherwise (design doc §8.5).
    LeaseHeld { provider_session_id: String },
    /// The lease could not be taken for a reason other than contention.
    Lease(LeaseError),
    /// Claude's own transcript for this session is still being written, so something else is very
    /// likely driving it right now. Design doc §8.4 step 4: probe before resuming, refuse if the
    /// transcript is moving.
    TranscriptUnstable { provider_session_id: String },
}

impl ConversationError {
    /// True for an error the conversation survives. A caller must report these to the user and
    /// carry on; tearing down the session over one would discard a working conversation.
    pub fn is_benign(&self) -> bool {
        match self {
            ConversationError::TurnAlreadyActive => true,
            ConversationError::Provider(e) => e.is_benign(),
            ConversationError::NoSession | ConversationError::Cwd(_) => false,
            // A refused resume leaves nothing running and nothing to tear down, but it is not
            // "carry on either" -- the caller asked for a conversation and does not have one.
            ConversationError::LeaseHeld { .. }
            | ConversationError::Lease(_)
            | ConversationError::TranscriptUnstable { .. }
            | ConversationError::ResumeRejected { .. }
            | ConversationError::ResumeIdentityMismatch { .. } => false,
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
            // States the advisory boundary rather than implying a guarantee the lock cannot give
            // (design doc §8.5): it binds Neovibe windows only.
            ConversationError::LeaseHeld { provider_session_id } => write!(
                f,
                "session {provider_session_id} is already open in another Neovibe window -- close it \
                 there first, or start a new conversation. This only binds Neovibe windows: a \
                 `claude --resume` you start by hand is not stopped by it"
            ),
            ConversationError::Lease(e) => write!(f, "could not take the session lease: {e}"),
            // Just the reason. It used to be wrapped in "the provider ended session X immediately
            // after accepting it ... it most likely no longer exists" -- the old inference, written
            // when this side was guessing from a termination it happened to observe. The provider
            // now states a typed verdict and `describe_failed_resume` renders it, so the wrapper
            // added a second, vaguer, and sometimes wrong account of the same event.
            ConversationError::ResumeRejected { reason, .. } => write!(f, "{reason}"),
            ConversationError::ResumeIdentityMismatch { requested, actual } => write!(
                f,
                "asked to continue session {requested}, but the provider opened {actual} instead. \
                 That session has been closed rather than handed back: continuing it would look like \
                 your previous conversation while carrying none of it. Start a new session instead."
            ),
            ConversationError::TranscriptUnstable { provider_session_id } => write!(
                f,
                "session {provider_session_id} is still being written to, so something else is \
                 driving it right now -- close that first, or start a new conversation"
            ),
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

/// The `provider` component of both the lease key and the persisted record. One constant so the two
/// cannot disagree -- a mismatch would silently produce a lease nobody else looks for.
pub(crate) const PROVIDER_NAME: &str = "claude";

/// How long `resume` waits for the provider's verdict before showing the conversation anyway.
///
/// **Not the old inference, despite the similar shape.** The previous constant decided
/// CORRECTNESS: silence for three seconds was read as a successful resume, and a rejection measured
/// at 1.7-2.3s on an idle machine had only ~0.7s of margin before a loaded machine turned a real
/// failure into an accepted, empty, dead conversation. This one decides only WHERE the failure is
/// reported -- on the start screen, or as a lost session a moment later -- because the provider now
/// states a typed verdict and `AgentSessionProjection` acts on it whenever it arrives.
///
/// It cannot be shortened to nothing without making the common case worse (a rejection would always
/// flash a conversation before removing it), and it cannot be lengthened into safety either, which
/// is the point: no value here is load-bearing any more.
const RESUME_VERDICT_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// What a just-issued resume turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum ResumeVerdict {
    /// The provider stated that it attached to the session that was asked for.
    Attached,
    /// No verdict yet, and that is not the same as success.
    ///
    /// The provider is contractually going to send one: a refusal arrives within a couple of
    /// seconds with no turn sent, while a confirmation cannot arrive until the first turn, because
    /// the provider only reports its session id at the start of one. So an expired wait means "the
    /// answer is still coming", and the projection is the authority from that point on -- a late
    /// failure surfaces as a lost session rather than as a failed construction.
    ///
    /// This is what replaced treating silence as success. The old code had no way to distinguish
    /// the two and chose the dangerous reading.
    NotYetKnown,
    /// The provider stated that it did not attach to the session that was asked for.
    Failed { reason: String },
}

/// Refuses a resume whose Claude transcript is still being written.
///
/// A missing transcript is NOT a refusal: a session that ran under an ephemeral persistence policy
/// legitimately has none, and treating "no file" as "unsafe" would make those sessions permanently
/// unresumable. Only an observably-moving file blocks.
fn ensure_transcript_is_not_being_written(
    cwd: &str,
    provider_session_id: &str,
) -> Result<(), ConversationError> {
    const PROBE: std::time::Duration = std::time::Duration::from_millis(250);
    let Ok(path) = crate::transcript::transcript_path(cwd, provider_session_id) else {
        return Ok(());
    };
    match crate::transcript::is_transcript_stable(&path, PROBE) {
        Ok(true) => Ok(()),
        Ok(false) => Err(ConversationError::TranscriptUnstable {
            provider_session_id: provider_session_id.to_string(),
        }),
        // Missing file, unreadable directory: not evidence of a writer.
        Err(_) => Ok(()),
    }
}

/// Best-effort. A conversation that runs but was not recorded is a lost resume offer next time, not
/// a broken session -- so this logs rather than failing the caller.
pub(crate) fn persist_record(
    conversation_id: &str,
    canonical_cwd: &str,
    provider_session_id: &str,
    provider_advertised_resume: bool,
) {
    let now = epoch_millis();
    // Preserve the original `created_at` when a record already exists: this runs again on every
    // resume of the same session, and rewriting it would turn "when did this conversation start"
    // into "when was it last touched", which `updated_at` already answers. Looked up by SESSION,
    // not by workspace -- a workspace holds one record per session now, so a second session
    // starting in the same directory must not inherit the first one's start time.
    let created_at = crate::persistence::load_conversation_record(conversation_id, provider_session_id)
        .map(|existing| existing.created_at)
        .unwrap_or_else(|_| now.clone());
    let record = ConversationRecord {
        conversation_id: conversation_id.to_string(),
        provider: PROVIDER_NAME.to_string(),
        provider_session_id: provider_session_id.to_string(),
        canonical_cwd: canonical_cwd.to_string(),
        created_at,
        updated_at: now,
        provider_advertised_resume,
    };
    if let Err(e) = save_conversation_record(&record) {
        eprintln!("agent: could not persist the conversation record for {conversation_id}: {e}");
    }
}

/// Milliseconds since the Unix epoch, as a string. No date library is a dependency of this crate
/// and adding one to stamp two fields would be a poor trade; a monotonic-enough integer is what
/// these fields are actually used for (ordering and "how stale is this record").
fn epoch_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
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
    /// Shared with the ingestion thread rather than owned. Every `AgentProvider` method takes
    /// `&self`, so the UI thread can issue commands while ingestion drains the same provider with no
    /// lock between them -- a mutex here would let a blocking `send_turn` stall ingestion for a whole
    /// gRPC round trip, recreating the UI/provider coupling `ingestion.rs` exists to break.
    ///
    /// `+ Send + Sync` so it can cross to that thread. (`+ Send` alone already mattered: a
    /// conversation is CONSTRUCTED on a worker thread, because creating one spawns a real sidecar
    /// process and, on a cold checkout, runs `npm ci` -- minutes that must never land on a GTK loop.)
    provider: Arc<dyn AgentProvider + Send + Sync>,
    capabilities: ProviderCapabilities,
    info: ProviderInfo,
    /// Verdandi's id for this session. `None` only between construction failure modes -- a
    /// successfully created conversation always has one.
    session_id: Option<String>,
    /// Canonical state, and the thread that keeps it current independently of the UI.
    ///
    /// Owns the projection, the Claude session id, and the session lease -- all three used to live
    /// here and be advanced only by a GTK timer calling `pump()`. `event_log` used to live here too,
    /// an unbounded `Vec` holding every raw event forever with no reader anywhere in the product;
    /// it is gone, because memory must follow the conversation rather than the event count.
    ingest: ConversationIngest,
}

impl AgentConversation {
    /// Creates a fresh provider session for `cwd`.
    ///
    /// `cwd` is canonicalized first, and the canonical form is what the conversation id, the lease
    /// and the provider all see -- otherwise `/home/x/proj` and `/home/x/../x/proj` would be two
    /// conversations for one directory, would take two different locks, and would later map to two
    /// different persisted records.
    pub fn create(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        cwd: &Path,
        permission_mode: crate::PermissionMode,
    ) -> Result<Self, ConversationError> {
        let canonical_cwd = cwd.canonicalize().map_err(ConversationError::Cwd)?;
        let capabilities = provider.capabilities();
        let info = provider.info();
        let session_id = provider.create_session(CreateSessionRequest {
            cwd: canonical_cwd.to_string_lossy().to_string(),
            permission_mode,
            // A UI client always wants incremental presentation.
            streaming: crate::provider::StreamingPreference::Partial,
        })?;
        Ok(Self::assembled(provider, canonical_cwd, capabilities, info, session_id))
    }

    /// Wires a freshly-created conversation to its ingestion thread.
    ///
    /// Ingestion starts here, which is BEFORE this returns to the caller -- so events begin folding
    /// into canonical state the moment the provider has one to give, rather than waiting for
    /// whichever thread eventually decides to look.
    fn assembled(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        canonical_cwd: PathBuf,
        capabilities: ProviderCapabilities,
        info: ProviderInfo,
        session_id: String,
    ) -> Self {
        let conversation_id = conversation_id_for_cwd(&canonical_cwd);
        let ingest = ConversationIngest::start(
            Arc::clone(&provider),
            conversation_id.clone(),
            canonical_cwd.to_string_lossy().to_string(),
            capabilities.resume,
            None,
            None,
        );
        Self {
            conversation_id,
            canonical_cwd,
            provider,
            capabilities,
            info,
            session_id: Some(session_id),
            ingest,
        }
    }

    /// Continues an existing Claude session.
    ///
    /// Step order matters and is not the same as `create`'s (see this module's header):
    ///   1. canonicalize `cwd` -- both the lease key and the persisted record use the canonical form;
    ///   2. refuse if the provider does not advertise resume, rather than sending a command on the
    ///      theory the server will ignore it;
    ///   3. probe Claude's own transcript for this session: if it is still being written, something
    ///      else is driving the session right now and resuming would interleave two writers into one
    ///      conversation. A MISSING transcript is not a refusal -- an ephemeral or
    ///      never-persisted session legitimately has none;
    ///   4. take the lease, BEFORE asking the provider for anything. Taking it afterwards leaves a
    ///      window in which two clients have both begun resuming the same session;
    ///   5. only then resume, and persist the mapping.
    ///
    /// The lease is advisory and this crate says so plainly: it binds clients that participate in
    /// this protocol. A raw `claude --resume` run outside Neovibe is not blocked by it, and nothing
    /// here pretends otherwise (design doc §8.5).
    pub fn resume(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        cwd: &Path,
        provider_session_id: &str,
        permission_mode: crate::PermissionMode,
    ) -> Result<Self, ConversationError> {
        let canonical_cwd = cwd.canonicalize().map_err(ConversationError::Cwd)?;
        let capabilities = provider.capabilities();
        if !capabilities.resume {
            return Err(ConversationError::Provider(ProviderError::UnsupportedCapability("resume")));
        }
        let info = provider.info();
        let cwd_string = canonical_cwd.to_string_lossy().to_string();

        ensure_transcript_is_not_being_written(&cwd_string, provider_session_id)?;

        let lease = match SessionLease::try_acquire(PROVIDER_NAME, &cwd_string, provider_session_id) {
            Ok(lease) => lease,
            Err(LeaseError::AlreadyHeld) => {
                return Err(ConversationError::LeaseHeld {
                    provider_session_id: provider_session_id.to_string(),
                })
            }
            Err(other) => return Err(ConversationError::Lease(other)),
        };

        let session_id = provider.resume_session(crate::provider::ResumeSessionRequest {
            provider_session_id: provider_session_id.to_string(),
            cwd: cwd_string.clone(),
            permission_mode,
            streaming: crate::provider::StreamingPreference::Partial,
        })?;

        let conversation_id = conversation_id_for_cwd(&canonical_cwd);

        let ingest = ConversationIngest::start(
            Arc::clone(&provider),
            conversation_id.clone(),
            cwd_string.clone(),
            capabilities.resume,
            // Known up front on this path, unlike `create` -- it is what we asked to resume -- and
            // the lease was taken before the provider was called at all.
            Some(provider_session_id.to_string()),
            Some(lease),
        );
        let mut conversation = Self {
            conversation_id,
            canonical_cwd,
            provider,
            capabilities,
            info,
            session_id: Some(session_id),
            ingest,
        };

        // A resume the provider ACCEPTS can still fail moments later. The sidecar's CreateSession
        // returns as soon as the session object exists; the SDK only discovers that the session id
        // does not exist when it tries to attach, and the session then terminates. Measured: a
        // resume of `00000000-dead-beef-...` returned Ok and produced a working-looking, empty
        // conversation that was already closed.
        //
        // So wait, briefly, for a terminal event -- and report it as a failed RESUME rather than
        // handing back a dead conversation. Nothing is synthesized here: this waits for the
        // provider's own event and folds it normally.
        match conversation.watch_resume_take_hold(provider_session_id, RESUME_VERDICT_WAIT) {
            // Confirmed, or not yet answered. Both continue: the provider is still going to state a
            // verdict, and `AgentSessionProjection` turns a late failure into a visibly lost session
            // rather than letting it pass as a working conversation.
            ResumeVerdict::Attached | ResumeVerdict::NotYetKnown => {}
            ResumeVerdict::Failed { reason } => {
                // Torn down rather than handed back. A session may genuinely have opened here (the
                // provider can attach to the wrong id), and leaving it running would keep a
                // conversation alive that nobody asked for.
                conversation.shutdown();
                return Err(ConversationError::ResumeRejected {
                    provider_session_id: provider_session_id.to_string(),
                    reason,
                });
            }
        }

        persist_record(conversation.conversation_id(), &cwd_string, provider_session_id, capabilities.resume);
        Ok(conversation)
    }

    /// Watches a just-issued resume long enough to decide whether it actually took hold.
    ///
    /// TWO things can go wrong, and only one of them announces itself:
    ///
    /// 1. The provider accepts the request and then terminates the session (the usual cause is an id
    ///    that no longer exists). That arrives as a real terminal event.
    /// 2. The provider opens a session that is not the one that was asked for. Nothing announces
    ///    that at all -- it looks exactly like success, which is what makes it the dangerous one.
    ///
    /// So `SessionOpened` is not treated as proof of a successful resume; only `SessionOpened`
    /// **carrying the requested provider session id** is. That check is the one the acceptance
    /// criteria always named, and it lived only in the tests until now: the runtime accepted any
    /// `SessionOpened` and moved on.
    ///
    /// Bounded and short: this runs on the connect worker, not the UI thread, but a user waiting on
    /// "Continue previous session" is waiting on it. A session that survives the window is treated
    /// as started -- a later termination is a normal session-death, reported through the usual
    /// status path rather than as a construction failure.
    ///
    /// **Protocol debt.** Waiting at all is a workaround: `CreateSession` returns success for a
    /// resume the provider has not yet tried to attach, so there is nothing to read and the only
    /// option is to watch. A `CreateSessionResponse` that reported resume acceptance directly would
    /// retire both the window and its guesswork; until then the window is an upper bound on
    /// confidence, not a proof, and a rejection slower than it still surfaces as a session death.
    fn watch_resume_take_hold(
        &mut self,
        requested_provider_session_id: &str,
        window: std::time::Duration,
    ) -> ResumeVerdict {
        let deadline = std::time::Instant::now() + window;
        while std::time::Instant::now() < deadline {
            // Read from the ingestion thread's record rather than by draining events. Draining here
            // would consume the very events the UI has not been shown yet -- and ingestion is
            // already folding continuously, so there is nothing to drive by hand.
            if let Some(outcome) = self.ingest.resume_outcome() {
                let crate::ingestion::ResumeOutcomeRecord {
                    requested,
                    status,
                    attached,
                    forked,
                    detail,
                } = outcome;
                {
                    {
                        // The provider echoes back the id it was asked for. If that disagrees with
                        // what this client sent, the verdict is about some other session and cannot
                        // be trusted as an answer to this request -- treated as a failure rather
                        // than ignored, because accepting a verdict meant for a different session is
                        // exactly how a substitution would slip through.
                        if requested != requested_provider_session_id {
                            return ResumeVerdict::Failed {
                                reason: format!(
                                    "the provider answered about session {requested}, but this client asked to \
                                     continue {requested_provider_session_id}. Start a new session instead."
                                ),
                            };
                        }
                        return if status.attached_to_the_requested_session(
                            &requested,
                            attached.as_deref(),
                            forked,
                        ) {
                            ResumeVerdict::Attached
                        } else {
                            ResumeVerdict::Failed {
                                reason: crate::describe_failed_resume(
                                    &requested,
                                    status,
                                    attached.as_deref(),
                                    detail.as_deref(),
                                ),
                            }
                        };
                    }
                }
            }
            // A session that dies before saying anything about the resume. Still a failure, but
            // reported in the provider's own terms rather than as a resume verdict it never gave.
            let terminal_reason = match &self.projection().status {
                crate::ProjectionStatus::Unavailable { reason } | crate::ProjectionStatus::Closed { reason } => {
                    Some(reason.clone())
                }
                _ => None,
            };
            if let Some(reason) = terminal_reason {
                return ResumeVerdict::Failed { reason };
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        ResumeVerdict::NotYetKnown
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
    pub fn provider_session_id(&self) -> Option<String> {
        self.ingest.provider_session_id()
    }

    /// Canonical conversation state. A borrow through the ingestion lock rather than a clone: the
    /// UI reads this on every 33ms tick.
    pub fn projection(&self) -> crate::ingestion::ProjectionGuard<'_> {
        self.ingest.projection()
    }

    /// What ingestion is costing right now -- see `IngestStats`.
    pub fn ingest_stats(&self) -> crate::ingestion::IngestStats {
        self.ingest.stats()
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    pub fn provider_info(&self) -> &ProviderInfo {
        &self.info
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
        if self.projection().active_turn_id.is_some() {
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
    ///
    /// The projection is NOT updated here. `PermissionResolved` arrives as a real provider event --
    /// including when the provider resolves it some other way entirely (an interrupt, a session
    /// close, a provider failure), which is why the outcome vocabulary has six variants and not
    /// two. Clearing the card optimistically here would mean this side deciding an outcome the
    /// provider is the only one that knows.
    pub fn respond_permission(
        &mut self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), ConversationError> {
        if !self.projection().pending_permissions.contains_key(permission_id) {
            return Err(ConversationError::Provider(ProviderError::Provider {
                code: crate::ProviderErrorCode::PermissionNotFound,
                message: format!("no pending permission request with id {permission_id}"),
            }));
        }
        let session_id = self.require_session()?;
        self.provider.resolve_permission(ResolvePermissionRequest {
            session_id,
            permission_id: permission_id.to_string(),
            decision,
        })?;
        Ok(())
    }

    /// Drains whatever the provider has produced since the last call, folding each event into the
    /// projection (so the next event in this same batch sees current state) and into the event log.
    ///
    /// This is also where `provider_session_id` is learned: it arrives on `SessionOpened` and
    /// nowhere else.
    /// What the UI should apply next.
    ///
    /// This no longer drains the provider, and that is the entire point of the change. It used to do
    /// both -- drain AND fold -- with a 33ms GTK timer as the only caller, which made the reducer's
    /// progress a function of repaint progress. Folding now happens on the ingestion thread, so a UI
    /// that stops calling this stops RENDERING; it no longer stops the conversation from advancing,
    /// and raw events no longer queue up waiting for it.
    pub fn take_ui_delivery(&self) -> crate::ingestion::UiDelivery {
        self.ingest.take_delivery()
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
        // Ingestion stops FIRST, so the terminal events folded below are the last word rather than
        // racing a thread that is still folding whatever the dying provider emits.
        self.ingest.stop();

        let pending: Vec<String> = self.projection().pending_permissions.keys().cloned().collect();
        for permission_id in pending {
            self.fold(AgentDomainEvent::PermissionResolved {
                permission_id,
                outcome: PermissionOutcome::CancelledBySessionClose,
            });
        }
        let already_closed = matches!(self.projection().status, crate::ProjectionStatus::Closed { .. });
        if !already_closed {
            self.fold(AgentDomainEvent::SessionClosed { reason: "closed_by_host".to_string() });
        }
        // Released last: the lease must outlive the provider's own session teardown, so no other
        // client can acquire it while this one is still closing.
        self.ingest.release_lease();
    }

    /// Folds an event this side produced because the provider no longer can -- shutdown's
    /// fail-closed terminal events, and nothing else. Provider lifecycle events stay the provider's
    /// to state.
    fn fold(&mut self, event: AgentDomainEvent) {
        self.ingest.fold_locally(event);
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
        /// Every event ever queued. A test waits for ingestion to reach exactly this, which is a
        /// real condition rather than a sleep long enough to probably be fine.
        queued_total: std::sync::atomic::AtomicU64,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self {
                capabilities: ProviderCapabilities {
                    interactive_permission_mode: true,
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
            self.queued_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        fn queued_total(&self) -> u64 {
            self.queued_total.load(std::sync::atomic::Ordering::Relaxed)
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
            self.calls.lock().unwrap().push(format!("resolve_permission({}, allow={})", request.permission_id, request.decision.allows()));
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

    /// A directory no other test shares, inside this process's disposable state root.
    ///
    /// Two reasons, and the second is the load-bearing one:
    ///
    /// - a distinct cwd is a distinct `conversation_id`, so tests cannot read each other's
    ///   persisted records or contend on each other's session leases (every test here folds the
    ///   same `session_opened()`, whose `provider_session_id` is a fixed string);
    /// - `state_dirs::test_workspace_dir` calls `redirect_state_to_a_test_root()`, which is what
    ///   stops those records and leases being written into the developer's own
    ///   `~/.local/state/neovibe/` and `$XDG_RUNTIME_DIR/neovibe/`. That write happens on the
    ///   INGESTION thread, so no amount of care on this thread avoids it -- only the redirect does.
    fn unique_dir() -> PathBuf {
        crate::state_dirs::test_workspace_dir("conversation")
    }

    /// `Box<dyn AgentProvider>` consumes the fake, so tests that need to inspect it afterward keep
    /// a second handle. `AgentProvider`'s methods all take `&self`, so an `Rc` is enough.
    fn conversation_with(fake: Arc<FakeProvider>) -> AgentConversation {
        conversation_in(fake, &unique_dir()).expect("create should succeed against the fake provider")
    }

    fn conversation_in(fake: Arc<FakeProvider>, dir: &Path) -> Result<AgentConversation, ConversationError> {
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
        AgentConversation::create(Arc::new(Shared(fake)), dir, PermissionMode::Bypass)
    }

    /// Waits for the ingestion thread to reach a state, or fails.
    ///
    /// These tests used to drive folding by calling `pump()` themselves, which no longer exists:
    /// folding happens continuously on its own thread now, so a test observes it rather than
    /// pumping it. Bounded and asserted rather than a bare sleep -- a sleep long enough to be safe
    /// on a loaded machine is long enough to make the suite unpleasant, and one that is too short
    /// fails as a mystery.
    fn wait_for(conversation: &AgentConversation, what: &str, mut done: impl FnMut(&AgentConversation) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if done(conversation) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("ingestion never reached: {what}");
    }

    /// Waits until every queued event has been folded.
    fn wait_for_ingest(conversation: &AgentConversation, count: u64) {
        wait_for(conversation, &format!("{count} event(s) ingested"), |c| {
            c.ingest_stats().events_ingested >= count
        });
    }

    /// Waits until every event queued on the fake so far has been folded.
    fn settle(fake: &FakeProvider, conversation: &AgentConversation) {
        wait_for_ingest(conversation, fake.queued_total());
    }

    fn session_opened() -> AgentDomainEvent {
        AgentDomainEvent::SessionOpened {
            session_id: "verdandi-session-1".into(),
            provider_session_id: "claude-uuid-abc".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/tmp".into(),
        }
    }

    fn resume_outcome(
        requested: &str,
        status: crate::ResumeStatus,
        attached: Option<&str>,
        forked: bool,
    ) -> AgentDomainEvent {
        AgentDomainEvent::ResumeOutcome {
            requested_provider_session_id: requested.into(),
            status,
            attached_provider_session_id: attached.map(str::to_string),
            forked,
            detail: None,
        }
    }

    /// The substitution the whole resume protocol exists to prevent.
    ///
    /// These tests used to drive the check with a `SessionOpened` queued synchronously by a fake
    /// provider. They passed, and they were testing a path the real kernel cannot take: the provider
    /// reports its session id in an init message that only arrives at the start of a TURN, and
    /// `resume()` never sends one -- so `SessionOpened` never arrived inside the window and the
    /// check never actually ran in production. The provider now states a typed verdict instead, and
    /// that is what these drive.
    #[test]
    fn a_provider_that_attached_to_a_different_session_is_a_failed_resume() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(resume_outcome(
            "claude-uuid-abc",
            crate::ResumeStatus::Attached,
            Some("some-other-session"),
            false,
        ));
        let verdict = conversation
            .watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        match verdict {
            ResumeVerdict::Failed { reason } => {
                assert!(reason.contains("some-other-session"), "{reason}");
                assert!(reason.contains("claude-uuid-abc"), "{reason}");
            }
            other => panic!("expected a failed resume, got {other:?}"),
        }
    }

    #[test]
    fn a_provider_that_attached_to_the_requested_session_is_a_real_resume() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(resume_outcome(
            "claude-uuid-abc",
            crate::ResumeStatus::Attached,
            Some("claude-uuid-abc"),
            false,
        ));
        let verdict = conversation
            .watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        assert_eq!(verdict, ResumeVerdict::Attached);
    }

    /// A fork legitimately comes back under a different id, so the ids disagreeing is CORRECT here.
    /// Without this, enabling fork would report every successful one as a substitution.
    #[test]
    fn a_fork_attaching_under_a_new_id_is_not_a_substitution() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(resume_outcome(
            "claude-uuid-abc",
            crate::ResumeStatus::Attached,
            Some("a-brand-new-forked-id"),
            true,
        ));
        let verdict = conversation
            .watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        assert_eq!(verdict, ResumeVerdict::Attached);
    }

    #[test]
    fn a_refused_resume_is_reported_in_terms_of_the_session_that_is_gone() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(resume_outcome("claude-uuid-abc", crate::ResumeStatus::Rejected, None, false));
        let verdict = conversation
            .watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        match verdict {
            ResumeVerdict::Failed { reason } => {
                assert!(reason.contains("claude-uuid-abc"), "{reason}");
                // Ends with what to do instead. An error that only names a fault leaves the reader
                // stuck on a start screen with no next step.
                assert!(reason.contains("Start a new session"), "{reason}");
            }
            other => panic!("expected a failed resume, got {other:?}"),
        }
    }

    /// A provider that could not start is kept distinct from one that refused the id: the session
    /// may well still exist, and telling someone their conversation was deleted when it was not is
    /// a different and worse error than saying the provider broke.
    #[test]
    fn a_provider_that_failed_to_start_is_not_reported_as_a_missing_conversation() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(resume_outcome(
            "claude-uuid-abc",
            crate::ResumeStatus::InitializationFailed,
            None,
            false,
        ));
        let verdict = conversation
            .watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        match verdict {
            ResumeVerdict::Failed { reason } => {
                assert!(reason.contains("provider problem"), "{reason}");
                assert!(!reason.contains("does not have session"), "{reason}");
            }
            other => panic!("expected a failed resume, got {other:?}"),
        }
    }

    #[test]
    fn a_session_that_dies_before_any_verdict_is_still_a_failed_resume() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(AgentDomainEvent::SessionUnavailable { reason: "stream ended early".into() });
        let verdict = conversation
            .watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        assert_eq!(verdict, ResumeVerdict::Failed { reason: "stream ended early".into() });
    }

    /// **Silence is no longer success.** The old code returned "held up" here, which is what made a
    /// rejection slower than the window -- measured at 1.7-2.3s against a 3.0s budget -- silently
    /// become an accepted, empty, dead conversation. It is now a distinct verdict meaning the
    /// answer has not arrived yet, and the caller proceeds knowing the provider still owes it one.
    #[test]
    fn an_expired_wait_means_not_yet_known_rather_than_success() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        let verdict = conversation
            .watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(150));
        assert_eq!(verdict, ResumeVerdict::NotYetKnown);
        assert_ne!(verdict, ResumeVerdict::Attached, "an unanswered resume must not read as a confirmed one");
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
        let dir = unique_dir();
        let conversation = conversation_in(fake.clone(), &dir).unwrap();
        let canonical = dir.canonicalize().unwrap();
        assert_eq!(conversation.canonical_cwd(), canonical.as_path());
        assert_eq!(conversation.conversation_id(), conversation_id_for_cwd(&canonical));
        assert_eq!(fake.calls(), vec![format!("create_session(cwd={})", canonical.to_string_lossy())]);
    }

    /// **The withdrawn A3, pinned so it cannot come back by accident.**
    ///
    /// A workspace-wide lease was written on 2026-09-15 and withdrawn the same day: it refused the
    /// second window outright, where the requirement is that two windows not step on each other. It
    /// also made `agent`'s own security baseline test
    /// (`backend_conformance::real_two_sessions_in_the_same_project_dir_each_see_only_their_own_permission_hooks`)
    /// impossible to run, since that test starts two sessions in one directory on purpose.
    ///
    /// The isolation that test asserts is real and lives elsewhere: the `PreToolUse` hook config
    /// travels in each CLI process's own argv (`--settings`), so nothing outside that process can
    /// read, overwrite or delete it. See `agent::settings`.
    #[test]
    fn two_fresh_conversations_in_one_directory_can_both_start() {
        let dir = unique_dir();
        let _first = conversation_in(Arc::new(FakeProvider::new()), &dir).expect("the first window starts normally");

        let second = Arc::new(FakeProvider::new());
        assert!(
            conversation_in(second.clone(), &dir).is_ok(),
            "a second Neovibe window in one project must not be refused -- see this test's doc comment"
        );
        assert_eq!(
            second.calls(),
            vec![format!("create_session(cwd={})", dir.canonicalize().unwrap().to_string_lossy())],
            "the second window must really have asked the provider for its own session"
        );
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
            Arc::new(Never),
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
        let conversation = conversation_with(fake.clone());

        // Before any event: Verdandi's id is known, Claude's is not.
        assert_eq!(conversation.session_id(), Some("verdandi-session-1"));
        assert_eq!(conversation.provider_session_id(), None, "a created-but-never-opened session has no Claude identity");

        fake.queue(session_opened());
        settle(&fake, &conversation);

        assert_eq!(conversation.session_id(), Some("verdandi-session-1"));
        assert_eq!(conversation.provider_session_id().as_deref(), Some("claude-uuid-abc"));
        assert_ne!(conversation.conversation_id(), "verdandi-session-1");
        assert_ne!(conversation.conversation_id(), "claude-uuid-abc");
    }

    #[test]
    fn ingestion_folds_the_projection_without_the_ui_asking()  {
        let fake = Arc::new(FakeProvider::new());
        let conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        fake.queue(AgentDomainEvent::ContentDelta { turn_id: "t1".into(), kind: ContentKind::Text, text: "hi".into() });

        // Nothing calls a pump. The projection advances anyway, which is the property: the reducer's
        // progress is no longer a function of the UI's.
        settle(&fake, &conversation);

        assert_eq!(conversation.projection().last_revision, 3);
        assert_eq!(
            conversation.projection().transcript.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            vec!["hi"],
        );
        assert_eq!(conversation.projection().active_turn_id.as_deref(), Some("t1"));

        // And the UI, whenever it gets round to it, is handed exactly those three.
        match conversation.take_ui_delivery() {
            crate::ingestion::UiDelivery::Events(events) => assert_eq!(events.len(), 3),
            other => panic!("expected three events, got {other:?}"),
        }
        assert!(
            matches!(conversation.take_ui_delivery(), crate::ingestion::UiDelivery::Nothing),
            "a second look with nothing new returns nothing"
        );
    }

    #[test]
    fn send_turn_does_not_synthesize_a_turn_started_event() {
        // The regression this pins: copying AgentSession's behavior here would fold a second,
        // locally-invented TurnStarted for a turn the provider also reports, giving one turn two
        // ids and two revisions.
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        settle(&fake, &conversation);
        let revision_before = conversation.projection().last_revision;

        let turn_id = conversation.send_turn("hello").unwrap();

        assert_eq!(turn_id, "turn-1");
        assert_eq!(conversation.projection().last_revision, revision_before, "send_turn must fold nothing");
        assert_eq!(conversation.projection().active_turn_id, None, "state comes from the provider's own event");
        assert_eq!(conversation.ingest_stats().events_ingested, 1, "only the SessionOpened folded earlier");
    }

    #[test]
    fn a_second_turn_while_one_is_active_is_rejected_locally_and_never_reaches_the_provider() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        settle(&fake, &conversation);

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
        settle(&fake, &conversation);

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
        settle(&fake, &conversation);

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
        settle(&fake, &conversation);
        let revision_before = conversation.projection().last_revision;

        conversation.interrupt().unwrap();

        assert_eq!(conversation.projection().last_revision, revision_before);
        assert!(fake.calls().iter().any(|c| c == "interrupt_turn"));

        // The provider's own terminal event is what moves state.
        fake.queue(AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Interrupted,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
        });
        settle(&fake, &conversation);
        assert_eq!(conversation.projection().active_turn_id, None);
    }

    #[test]
    fn responding_to_an_unknown_permission_is_rejected_without_reaching_the_provider() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        settle(&fake, &conversation);

        let result = conversation.respond_permission("never-existed", PermissionDecision::Allow);
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
        settle(&fake, &conversation);

        conversation.respond_permission("p1", PermissionDecision::Allow).unwrap();
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
        settle(&fake, &conversation);
        assert_eq!(conversation.projection().pending_permissions.len(), 1);

        conversation.shutdown();

        assert!(conversation.projection().pending_permissions.is_empty(), "a closing session can never answer them");
        assert!(matches!(conversation.projection().status, crate::ProjectionStatus::Closed { .. }));
        assert_eq!(fake.calls().iter().filter(|c| *c == "close_session").count(), 1);

        let revision_after_first = conversation.projection().last_revision;
        conversation.shutdown();
        assert_eq!(fake.calls().iter().filter(|c| *c == "close_session").count(), 1, "close_session must not repeat");
        assert_eq!(conversation.projection().last_revision, revision_after_first, "a repeat shutdown folds nothing");
    }
}
