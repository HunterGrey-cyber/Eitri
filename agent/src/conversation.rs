//! `AgentConversation`: one whole conversation, driven by any `AgentProvider`.
//!
//! This is the layer `shell` talks to. It exists because `AgentProvider` is deliberately stateless
//! -- a provider issues commands and drains events, and holds no projection --
//! while a UI needs something it can both command and read. The legacy `AgentSession` already plays
//! that role, but only for the one backend it is welded to.
//!
//! So this type is the provider-agnostic sibling of `AgentSession`, with the same method shape on
//! purpose (`send_turn` / `interrupt` / `respond_permission` / `pump` / `shutdown`, plus a public
//! `projection`) so a caller can switch between them without restructuring.
//!
//! **Three identities, deliberately not collapsed**:
//!
//! | identity              | whose        | where it comes from                               |
//! |-----------------------|--------------|---------------------------------------------------|
//! | `conversation_id`     | Eitri's    | derived here, stable for a workspace              |
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
//! 2026-09-15: it delivered prohibition (a second Eitri window in one project refused to start)
//! where the requirement is isolation, and it broke `agent`'s own security baseline test, which now
//! asserts that two sessions in one directory each see only their own `PreToolUse` hook. That
//! isolation is real and lives elsewhere -- the hook config travels in the CLI's own argv
//! (`--settings`, see `agent::settings`), not in a `.claude/` file two sessions could share. The
//! cross-window question "is another window running an agent on this project?" is the `supervisor`
//! dashboard's job, not a lock's.
//!
//! The lease is ADVISORY and nothing here says otherwise. It binds
//! processes that take part in this protocol; a `claude` someone runs by hand in the same directory
//! takes no lease and is not stopped, and Claude's own documentation records that two clients
//! resuming one session interleave a single transcript.

use crate::external::{ExternalProvider, ExternalStart, TurnPolicy};
use crate::ingestion::ConversationIngest;
use crate::lease::{LeaseError, SessionLease};
use crate::move_seed::{ChatMoveCore, ConversationHandoff, HandoffRefused, MovedProvider, PendingName};
use crate::persistence::{save_conversation_record, ConversationRecord};
use crate::projection::{AgentDomainEvent, PermissionOutcome};
use crate::provider::{
    AgentProvider, ClientToolAnswer, ClientToolCallState, ClientToolSet, CloseSessionRequest, CreateSessionRequest,
    InterruptTurnRequest, PermissionDecision, ProviderCapabilities, ProviderError, ProviderInfo,
    ResolvePermissionRequest, SendTurnRequest, TurnStamp,
};
use crate::setting_sources::ProjectTrust;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Who wrote a message the host sends: the person, at the composer (`Typed`), or the host or a plugin
/// (`Composed`: turn review's prompts, a plugin page's send, a companion send that came from no
/// keyboard, a restored queue the person did not send again). Only a `Typed` message is marked as the
/// person's to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOrigin {
    Typed,
    Composed,
}

#[derive(Debug)]
pub enum ConversationError {
    /// The underlying provider failed. `ProviderError::is_benign()` says whether the conversation
    /// can continue past it.
    Provider(ProviderError),
    /// A second turn was submitted while one was already active. Rejected here rather than queued,
    /// matching `AgentSession::send_turn`'s identical rule. Benign: the
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
    ResumeRejected {
        provider_session_id: String,
        reason: String,
    },
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
    /// Another Eitri-participating client already holds this provider session.
    ///
    /// Advisory, and honestly so: the lease binds clients that take part in this protocol. A raw
    /// `claude --resume` started outside Eitri is NOT blocked by it, and this crate never claims
    /// otherwise.
    LeaseHeld { provider_session_id: String },
    /// The lease could not be taken for a reason other than contention.
    Lease(LeaseError),
    /// Claude's own transcript for this session is still being written, so something else is very
    /// likely driving it right now. So a resume probes first and refuses while the transcript is
    /// moving: two writers in one transcript would interleave one conversation.
    TranscriptUnstable { provider_session_id: String },
    /// The call does not exist for this conversation's agent: a plain `send_turn` on a conversation
    /// whose agent takes a policy with every turn, or `send_turn_with` on one that does not.
    /// The conversation is fine; the caller used the wrong door.
    Unsupported,
}

impl ConversationError {
    /// True for an error the conversation survives. A caller must report these to the user and
    /// carry on; tearing down the session over one would discard a working conversation.
    pub fn is_benign(&self) -> bool {
        match self {
            ConversationError::TurnAlreadyActive | ConversationError::Unsupported => true,
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
            // States the advisory boundary rather than implying a guarantee the lock cannot give:
            // it binds Eitri windows only.
            ConversationError::LeaseHeld { provider_session_id } => write!(
                f,
                "session {provider_session_id} is already open in another Eitri window -- close it \
                 there first, or start a new conversation. This only binds Eitri windows: a \
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
            ConversationError::Unsupported => {
                write!(f, "this conversation's agent does not take a turn sent that way")
            }
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

/// Eitri's own stable id for the conversation belonging to a workspace directory.
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
fn ensure_transcript_is_not_being_written(cwd: &str, provider_session_id: &str) -> Result<(), ConversationError> {
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
///
/// `title` is the session's title if the caller knows one (ingestion hands over the first prompt's,
/// when that prompt was sent before the session was adopted). A title the record already has always
/// wins, the same way `created_at` does: a resume must not rename a session.
pub(crate) fn persist_record(
    provider_name: &str,
    conversation_id: &str,
    canonical_cwd: &str,
    provider_session_id: &str,
    provider_advertised_resume: bool,
    title: Option<&str>,
    name: &crate::persistence::NameUpdate,
) {
    let now = epoch_millis();
    // Preserve the original `created_at` when a record already exists: this runs again on every
    // resume of the same session, and rewriting it would turn "when did this conversation start"
    // into "when was it last touched", which `updated_at` already answers. Looked up by SESSION,
    // not by workspace -- a workspace holds one record per session now, so a second session
    // starting in the same directory must not inherit the first one's start time.
    //
    // Via `existing_record` (formerly `created_at_of_existing_record`) rather than
    // `load_conversation_record`, because that one
    // resolves to the directory layout only: resuming a session recorded solely in the
    // pre-2026-09-15 flat file restamped it to the moment of the resume, and the de-duplication in
    // `resumable_sessions` then discarded the flat record still holding the real value.
    let existing = crate::persistence::existing_record(conversation_id, provider_session_id);
    let created_at = existing
        .as_ref()
        .map(|r| r.created_at.clone())
        .unwrap_or_else(|| now.clone());
    let name = match name {
        crate::persistence::NameUpdate::Set(name) => name.clone(),
        crate::persistence::NameUpdate::Keep => existing.as_ref().and_then(|r| r.name.clone()),
    };
    let title = existing.and_then(|r| r.title).or_else(|| title.map(str::to_string));
    let record = ConversationRecord {
        conversation_id: conversation_id.to_string(),
        provider: provider_name.to_string(),
        provider_session_id: provider_session_id.to_string(),
        canonical_cwd: canonical_cwd.to_string(),
        created_at,
        updated_at: now,
        provider_advertised_resume,
        title,
        name,
    };
    if let Err(e) = save_conversation_record(&record) {
        eprintln!("agent: could not persist the conversation record for {conversation_id}: {e}");
    }
}

/// Milliseconds since the Unix epoch, as a string. No date library is a dependency of this crate
/// and adding one to stamp two fields would be a poor trade; a monotonic-enough integer is what
/// these fields are actually used for (ordering and "how stale is this record").
pub(crate) fn epoch_millis() -> String {
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
    /// The same provider seen through the external extension, for a conversation that has one.
    /// `Some` is what makes this an external conversation: its turns carry a policy, and it reads no
    /// Claude transcript.
    external: Option<Arc<dyn ExternalProvider + Send + Sync>>,
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
    /// Whether `note_title` has already taken this conversation's title, so later turns touch
    /// neither the lock nor the disk.
    title_noted: bool,
    /// Whether a `shutdown()` call has ever seen `close_session` fail.
    ///
    /// A struct field, not a local in `shutdown`, because `shutdown` is documented and tested as
    /// idempotent: a second call takes `self.session_id` as `None` (already taken by the first) and
    /// so never calls `close_session` again -- a fresh per-call local would forget the first call's
    /// failure and let the second call's `if !close_failed { release_lease() }` release the lease
    /// anyway, exactly the race this field exists to keep closed. Once set, stays set: the lease
    /// must stay held for the rest of this conversation's life, never just until the next call.
    close_failed: bool,
    /// The revision of the last snapshot the caller handed its UI, until its next drain takes it
    /// (`note_ui_snapshot`). Held here rather than by the caller so it cannot outlive this
    /// conversation: another session's revisions start again from their own seed and mean nothing
    /// against this one's snapshot.
    ui_snapshot_revision: Option<u64>,
    /// The tools this session was given (`create_with_tools`, `resume_with_tools`): the set's names when
    /// the provider offers the capability, otherwise none, whatever the caller asked for.
    declared_client_tools: BTreeSet<String>,
}

/// The tools a session gets of those `set` declares, on a provider with `capabilities`.
fn declared_tools(capabilities: &ProviderCapabilities, set: Option<&ClientToolSet>) -> BTreeSet<String> {
    match set {
        Some(set) if capabilities.client_tools => set.tools.iter().map(|tool| tool.name.clone()).collect(),
        _ => BTreeSet::new(),
    }
}

impl AgentConversation {
    /// Creates a fresh provider session for `cwd`.
    ///
    /// `cwd` is canonicalized first, and the canonical form is what the conversation id, the lease
    /// and the provider all see -- otherwise `/home/x/proj` and `/home/x/../x/proj` would be two
    /// conversations for one directory, would take two different locks, and would later map to two
    /// different persisted records.
    ///
    /// No permission mode: the session is gated whatever the tab's mode, and the host decides
    /// above this API whether to answer its requests `allow` itself.
    ///
    /// `project` decides whether the project's own settings tiers load; the caller states it.
    pub fn create(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        cwd: &Path,
        project: ProjectTrust,
    ) -> Result<Self, ConversationError> {
        Self::create_with_tools(provider, cwd, project, None)
    }

    /// [`create`](Self::create) with the tools this host answers itself.
    pub fn create_with_tools(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        cwd: &Path,
        project: ProjectTrust,
        client_tools: Option<ClientToolSet>,
    ) -> Result<Self, ConversationError> {
        let canonical_cwd = cwd.canonicalize().map_err(ConversationError::Cwd)?;
        let capabilities = provider.capabilities();
        let info = provider.info();
        let declared_client_tools = declared_tools(&capabilities, client_tools.as_ref());
        let session_id = provider.create_session(CreateSessionRequest {
            cwd: canonical_cwd.to_string_lossy().to_string(),
            // A UI client always wants incremental presentation.
            streaming: crate::provider::StreamingPreference::Partial,
            project,
            client_tools,
        })?;
        let mut conversation = Self::assembled(
            provider,
            None,
            PROVIDER_NAME,
            canonical_cwd,
            capabilities,
            info,
            session_id,
        );
        conversation.declared_client_tools = declared_client_tools;
        Ok(conversation)
    }

    /// Creates a fresh session on an external provider. The provider's name keys the lease and the
    /// persisted record, and the start carries the trust plan and the policy of the tab's mode.
    ///
    /// `start.resume` must be empty: a start that names a session to continue is a resume, and
    /// quietly starting a fresh one instead is the substitution the resume path exists to prevent.
    /// The working directory the provider sees is the canonical one, whatever `start.cwd` says.
    pub fn create_external(
        provider: Arc<dyn ExternalProvider + Send + Sync>,
        start: ExternalStart,
        cwd: &Path,
    ) -> Result<Self, ConversationError> {
        if start.resume.is_some() {
            return Err(ConversationError::Provider(ProviderError::Provider {
                code: crate::ProviderErrorCode::InvalidConfiguration,
                message: "a start that names a session to continue must go through resume_external".to_string(),
            }));
        }
        let canonical_cwd = cwd.canonicalize().map_err(ConversationError::Cwd)?;
        let as_agent: Arc<dyn AgentProvider + Send + Sync> = provider.clone();
        let capabilities = as_agent.capabilities();
        let info = as_agent.info();
        let name = provider.name();
        let session_id = provider.start(ExternalStart {
            cwd: canonical_cwd.to_string_lossy().to_string(),
            ..start
        })?;
        Ok(Self::assembled(
            as_agent,
            Some(provider),
            name,
            canonical_cwd,
            capabilities,
            info,
            session_id,
        ))
    }

    /// Wires a freshly-created conversation to its ingestion thread.
    ///
    /// Ingestion starts here, which is BEFORE this returns to the caller -- so events begin folding
    /// into canonical state the moment the provider has one to give, rather than waiting for
    /// whichever thread eventually decides to look.
    fn assembled(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        external: Option<Arc<dyn ExternalProvider + Send + Sync>>,
        provider_name: &'static str,
        canonical_cwd: PathBuf,
        capabilities: ProviderCapabilities,
        info: ProviderInfo,
        session_id: String,
    ) -> Self {
        let conversation_id = conversation_id_for_cwd(&canonical_cwd);
        let resumable = external.as_ref().map_or(capabilities.resume, |e| e.resumes());
        let ingest = ConversationIngest::start(
            Arc::clone(&provider),
            provider_name,
            conversation_id.clone(),
            canonical_cwd.to_string_lossy().to_string(),
            resumable,
            None,
            None,
            // A fresh session has nothing to restore. `create` never reads either history record --
            // not even to check -- because a conversation that has not happened yet cannot have one.
            crate::AgentSessionProjection::default(),
        );
        Self {
            conversation_id,
            canonical_cwd,
            provider,
            external,
            capabilities,
            info,
            session_id: Some(session_id),
            ingest,
            title_noted: false,
            close_failed: false,
            ui_snapshot_revision: None,
            declared_client_tools: BTreeSet::new(),
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
    /// this protocol. A raw `claude --resume` run outside Eitri is not blocked by it, and nothing
    /// here pretends otherwise.
    pub fn resume(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        cwd: &Path,
        provider_session_id: &str,
        project: ProjectTrust,
    ) -> Result<Self, ConversationError> {
        Self::resume_holding(provider, cwd, provider_session_id, None, project)
    }

    /// [`resume`](Self::resume) with the session's lease already taken by the caller, who took it
    /// ahead of time so nothing could claim the session -- or prune its record -- between deciding
    /// to resume it and the resume reaching step 4. The lease is used instead of a second
    /// `try_acquire` (which would contend with it, since `flock` is per open file description), and
    /// it is dropped with every early return, so a failed resume leaves the session free.
    ///
    /// A lease for another provider, directory or session is refused before the provider is asked
    /// for anything: a resume must never drive a session it holds no lock on.
    ///
    /// `project` is the trust the caller states now. A resumed session never inherits the tiers
    /// the conversation began with: the project may have changed, or been trusted or untrusted, in
    /// between.
    pub fn resume_holding(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        cwd: &Path,
        provider_session_id: &str,
        held: Option<SessionLease>,
        project: ProjectTrust,
    ) -> Result<Self, ConversationError> {
        Self::resume_with_tools(provider, cwd, provider_session_id, held, project, None)
    }

    /// [`resume_holding`](Self::resume_holding) with the tools this host answers itself, stated
    /// afresh as `project` is: a resumed session has only what this call names.
    pub fn resume_with_tools(
        provider: Arc<dyn AgentProvider + Send + Sync>,
        cwd: &Path,
        provider_session_id: &str,
        held: Option<SessionLease>,
        project: ProjectTrust,
        client_tools: Option<ClientToolSet>,
    ) -> Result<Self, ConversationError> {
        let canonical_cwd = cwd.canonicalize().map_err(ConversationError::Cwd)?;
        let capabilities = provider.capabilities();
        if !capabilities.resume {
            return Err(ConversationError::Provider(ProviderError::UnsupportedCapability(
                "resume",
            )));
        }
        let info = provider.info();
        let cwd_string = canonical_cwd.to_string_lossy().to_string();

        ensure_transcript_is_not_being_written(&cwd_string, provider_session_id)?;

        let lease = match held {
            Some(lease) if lease.is_for(PROVIDER_NAME, &cwd_string, provider_session_id) => lease,
            Some(_) => {
                return Err(ConversationError::Lease(LeaseError::Io(std::io::Error::other(
                    "the lease handed to this resume is for another session",
                ))))
            }
            None => match SessionLease::try_acquire(PROVIDER_NAME, &cwd_string, provider_session_id) {
                Ok(lease) => lease,
                Err(LeaseError::AlreadyHeld) => {
                    return Err(ConversationError::LeaseHeld {
                        provider_session_id: provider_session_id.to_string(),
                    })
                }
                Err(other) => return Err(ConversationError::Lease(other)),
            },
        };

        let conversation_id = conversation_id_for_cwd(&canonical_cwd);

        // Here, and nowhere else. The stability probe has just confirmed nothing is
        // writing the transcript (step 3), the lease is in hand so no other Eitri window is
        // driving this session (step 4), and the provider has not yet produced a byte -- so the
        // file cannot move under this read and no live event can precede what it restores.
        //
        // A resume the provider later REFUSES throws this work away along with the whole
        // conversation (`watch_resume_take_hold` below), which is deliberate: the alternative is
        // loading after the verdict, which means injecting history into a projection that has
        // already folded live events, and that is the one thing that would break the `seq` total
        // order.
        //
        // **This comment used to end:** "Reading the largest real transcript on this machine whole
        // takes 0.143s in Python." That was offered as the reason the cost is negligible, and it
        // was a Python measurement standing in for Rust. Kept above rather than deleted, because
        // what replaced it only moves the conclusion, not the direction: the real figures for the
        // product path on this machine's largest
        // transcript (48,805,214 B, tail-scanned), 3 runs each, hot cache -- **70-84 ms in release
        // and 513-519 ms in debug**. So release is about twice as fast as the Python number and
        // debug about 3.6x slower than it, and debug is what every `cargo test` and every
        // `cargo run -p shell` uses. It is still not a stall: this runs on the worker thread
        // `AgentBackend::start` was spawned onto (`panel/src/starts.rs`), never on the GTK
        // main loop, so what it costs is the panel filling in that much later after a resume of
        // that one outsized session. Every other transcript here is a few milliseconds.
        let seed = crate::history::load::seed_for_resume(&cwd_string, &conversation_id, provider_session_id);

        let declared_client_tools = declared_tools(&capabilities, client_tools.as_ref());
        let session_id = provider.resume_session(crate::provider::ResumeSessionRequest {
            provider_session_id: provider_session_id.to_string(),
            cwd: cwd_string.clone(),
            streaming: crate::provider::StreamingPreference::Partial,
            project,
            client_tools,
        })?;

        let ingest = ConversationIngest::start(
            Arc::clone(&provider),
            PROVIDER_NAME,
            conversation_id.clone(),
            cwd_string.clone(),
            capabilities.resume,
            // Known up front on this path, unlike `create` -- it is what we asked to resume -- and
            // the lease was taken before the provider was called at all.
            Some(provider_session_id.to_string()),
            Some(lease),
            seed,
        );
        let conversation = Self {
            conversation_id,
            canonical_cwd,
            provider,
            external: None,
            capabilities,
            info,
            session_id: Some(session_id),
            ingest,
            title_noted: false,
            close_failed: false,
            ui_snapshot_revision: None,
            declared_client_tools,
        };

        Self::confirm_resume(conversation, PROVIDER_NAME, &cwd_string, provider_session_id)
    }

    /// Resumes a session on an external provider, with the session's lease already taken by the
    /// caller (as [`resume_holding`](Self::resume_holding) does). The session to continue is
    /// `start.resume`, and a start without one is refused.
    ///
    /// Two steps of the Claude resume are skipped because they are about Claude's files: the probe of
    /// Claude's own transcript, and the seed read from it. An external agent's history comes from the
    /// provider itself (`ExternalProvider::take_history`) and is the seed instead.
    pub fn resume_external(
        provider: Arc<dyn ExternalProvider + Send + Sync>,
        start: ExternalStart,
        cwd: &Path,
        holding: SessionLease,
    ) -> Result<Self, ConversationError> {
        let Some(provider_session_id) = start.resume.clone() else {
            return Err(ConversationError::Provider(ProviderError::Provider {
                code: crate::ProviderErrorCode::InvalidConfiguration,
                message: "resume_external needs the id of the session to continue".to_string(),
            }));
        };
        let canonical_cwd = cwd.canonicalize().map_err(ConversationError::Cwd)?;
        let as_agent: Arc<dyn AgentProvider + Send + Sync> = provider.clone();
        let capabilities = as_agent.capabilities();
        if !provider.resumes() {
            return Err(ConversationError::Provider(ProviderError::UnsupportedCapability(
                "resume",
            )));
        }
        let info = as_agent.info();
        let name = provider.name();
        let cwd_string = canonical_cwd.to_string_lossy().to_string();
        if !holding.is_for(name, &cwd_string, &provider_session_id) {
            return Err(ConversationError::Lease(LeaseError::Io(std::io::Error::other(
                "the lease handed to this resume is for another session",
            ))));
        }
        let conversation_id = conversation_id_for_cwd(&canonical_cwd);
        let session_id = provider.start(ExternalStart {
            cwd: cwd_string.clone(),
            ..start
        })?;
        // Nothing is read from disk: the provider read the history back while it started, and it is
        // folded here, before ingestion exists, exactly where a Claude resume's seed goes. Delivered as
        // live events instead, every past turn would reach the host as one that just ran.
        let mut seed = crate::AgentSessionProjection::default();
        for event in provider.take_history() {
            seed.apply(&event);
        }
        // How the last past turn ended stays a row, but it is not the latest news: the band does not
        // say it (a Claude resume's seed has no ending either), and a live report of the account's
        // limits, which may fill the latest ending's reset time, must not write today's into it.
        seed.last_turn_ending = None;
        let ingest = ConversationIngest::start(
            Arc::clone(&as_agent),
            name,
            conversation_id.clone(),
            cwd_string.clone(),
            true,
            Some(provider_session_id.clone()),
            Some(holding),
            seed,
        );
        let conversation = Self {
            conversation_id,
            canonical_cwd,
            provider: as_agent,
            external: Some(provider),
            capabilities,
            info,
            session_id: Some(session_id),
            ingest,
            title_noted: false,
            close_failed: false,
            ui_snapshot_revision: None,
            declared_client_tools: BTreeSet::new(),
        };
        Self::confirm_resume(conversation, name, &cwd_string, &provider_session_id)
    }

    /// The last step of every resume: waits briefly for the provider's verdict, tears the session down
    /// on a rejection, and persists the record on success.
    fn confirm_resume(
        mut conversation: Self,
        provider_name: &'static str,
        cwd_string: &str,
        provider_session_id: &str,
    ) -> Result<Self, ConversationError> {
        let resumable = match &conversation.external {
            Some(external) => external.resumes(),
            None => conversation.capabilities.resume,
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

        persist_record(
            provider_name,
            conversation.conversation_id(),
            cwd_string,
            provider_session_id,
            resumable,
            None,
            &crate::persistence::NameUpdate::Keep,
        );
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
    /// Bounded and short: this runs on the connect worker, not the UI thread, but a user who picked
    /// a previous conversation on the start screen is waiting on it. A session that survives the window is treated
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
                        return if status.attached_to_the_requested_session(&requested, attached.as_deref(), forked) {
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

    /// `claude` for the sidecar, the external provider's own name otherwise: the component that keys
    /// the lease and the persisted record.
    pub fn provider_name(&self) -> &str {
        match &self.external {
            Some(external) => external.name(),
            None => PROVIDER_NAME,
        }
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

    /// Where this conversation's stored history stands against what it has folded
    /// (`ConversationIngest::history_progress`).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn history_progress(&self) -> crate::ingestion::HistoryProgress {
        self.ingest.history_progress()
    }

    /// Every event folded so far with its revision, and the projection they produced
    /// (`ConversationIngest::fold_log`).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn fold_log(&self) -> Option<(Vec<(u64, AgentDomainEvent)>, crate::AgentSessionProjection)> {
        self.ingest.fold_log()
    }

    /// What the provider said it can do when it connected, with `steer` asked again: a provider may
    /// learn mid-session that its agent cannot take messages while a turn runs.
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            steer: self.steers(),
            ..self.capabilities
        }
    }

    /// Whether this session can be continued later, as its provider said when it connected: an external
    /// provider's own answer (an ACP agent's `loadSession`), else the provider's `resume` capability.
    pub fn resumes(&self) -> bool {
        self.external.as_ref().map_or(self.capabilities.resume, |e| e.resumes())
    }

    /// How full the agent last said its context window is: only an external provider that reports it.
    pub fn context_gauge(&self) -> Option<crate::external::ContextGauge> {
        self.external.as_ref().and_then(|e| e.context_gauge())
    }

    /// What the tab should say about editor tools its session was left without
    /// (`ExternalProvider::editor_note`), handed over once; nothing for a Claude session.
    pub fn editor_note(&self) -> Option<String> {
        self.external.as_ref().and_then(|e| e.editor_note())
    }

    /// Whether the provider stopped this session itself with the conversation whole
    /// (`ExternalProvider::stopped_resumable`); false for a Claude session.
    pub fn stopped_resumable(&self) -> bool {
        self.external.as_ref().is_some_and(|e| e.stopped_resumable())
    }

    /// Stops the background work in the canonical roster through this session's provider.
    pub fn stop_background(&self, shown: u32) -> Result<(), ConversationError> {
        let session_id = self.require_session()?;
        let tasks: Vec<String> = self
            .projection()
            .background_tasks
            .rows
            .iter()
            .filter(|row| row.live && !row.task.ambient)
            .map(|row| row.task.task_id.clone())
            .collect();
        self.provider.stop_background(&session_id, shown, &tasks)?;
        Ok(())
    }

    pub fn stop_task(&self, task_id: &str) -> Result<crate::TaskReply<()>, ConversationError> {
        Ok(self.provider.stop_task(&self.require_session()?, task_id)?)
    }

    pub fn task_output(&self, task_id: &str) -> Result<crate::TaskReply<crate::TaskOutput>, ConversationError> {
        Ok(self.provider.task_output(&self.require_session()?, task_id)?)
    }

    /// The tools this session was given: empty on a provider without the capability.
    pub fn declared_client_tools(&self) -> &BTreeSet<String> {
        &self.declared_client_tools
    }

    /// Answers one call of a declared tool without waiting
    /// ([`AgentProvider::answer_client_tool_call`]).
    pub fn answer_client_tool_call(
        &self,
        call_id: &str,
        text: Vec<String>,
        is_error: bool,
    ) -> Result<(), ConversationError> {
        let session_id = self.require_session()?;
        self.provider.answer_client_tool_call(ClientToolAnswer {
            session_id,
            call_id: call_id.to_string(),
            text,
            is_error,
        })?;
        Ok(())
    }

    /// Reads the session's open calls on the provider's side.
    pub fn client_tool_call_state(&self) -> Result<crate::TaskReply<ClientToolCallState>, ConversationError> {
        Ok(self.provider.client_tool_call_state(&self.require_session()?)?)
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
    ///
    /// An external conversation's turns carry a policy, so this door is closed on it
    /// ([`send_turn_with`](Self::send_turn_with) is the one that opens).
    pub fn send_turn(&mut self, text: &str) -> Result<String, ConversationError> {
        if self.external.is_some() {
            return Err(ConversationError::Unsupported);
        }
        if self.projection().active_turn_id.is_some() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        let session_id = self.require_session()?;
        self.provider
            .send_turn(SendTurnRequest {
                session_id,
                text: text.to_string(),
            })
            .map_err(Self::turn_error)
    }

    /// [`send_turn`](Self::send_turn) of a message the host knows by `client_message_id`, written by
    /// `origin`: the provider names the id back if the agent reads the message later than the send
    /// (`UserMessageDelivered`, whose row shows `as_typed`) or drops it (`UserMessageDropped`), and
    /// marks a `Typed` message as the person's where its agent accepts the mark.
    pub fn send_turn_as(
        &mut self,
        text: &str,
        as_typed: &str,
        origin: PromptOrigin,
        client_message_id: &str,
    ) -> Result<String, ConversationError> {
        if self.external.is_some() {
            return Err(ConversationError::Unsupported);
        }
        if self.projection().active_turn_id.is_some() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        let session_id = self.require_session()?;
        self.provider
            .send_turn_stamped(
                SendTurnRequest {
                    session_id,
                    text: text.to_string(),
                },
                TurnStamp {
                    client_message_id: Some(client_message_id.to_string()),
                    typed_by_person: origin == PromptOrigin::Typed,
                    as_typed: Some(as_typed.to_string()),
                },
            )
            .map_err(Self::turn_error)
    }

    /// Submits one user turn on an external conversation, under the policy of the tab's mode. Same
    /// local guard as [`send_turn`](Self::send_turn); the provider keeps its own, which also covers
    /// the window before its `TurnStarted` is folded. A conversation with no external provider
    /// refuses.
    pub fn send_turn_with(&mut self, text: &str, policy: TurnPolicy) -> Result<String, ConversationError> {
        let Some(external) = self.external.clone() else {
            return Err(ConversationError::Unsupported);
        };
        if self.projection().active_turn_id.is_some() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        let session_id = self.require_session()?;
        external
            .send_turn_with(
                SendTurnRequest {
                    session_id,
                    text: text.to_string(),
                },
                policy,
            )
            .map_err(Self::turn_error)
    }

    fn turn_error(e: ProviderError) -> ConversationError {
        match e {
            ProviderError::Provider {
                code: crate::ProviderErrorCode::TurnAlreadyActive,
                ..
            } => ConversationError::TurnAlreadyActive,
            other => ConversationError::Provider(other),
        }
    }

    /// Records what this session is about, for the resume picker: the first line of `as_typed`, the
    /// prompt as the user typed it, so the row reads as the user's own first sentence. Pass the typed
    /// text, never the turn sent on the wire -- that one carries editor context above the user's
    /// words. Only the first prompt with visible text counts; after that this does nothing.
    ///
    /// The Agent SDK reports Claude's session id at the start of the first TURN, so the record
    /// usually does not exist yet when the first prompt is sent. This only hands the title to the
    /// ingestion thread (`ConversationIngest::note_title`), which writes it with the record when it
    /// adopts the session, or into the record once it is on disk. Nothing here touches the disk --
    /// this runs on the GTK main loop. Best-effort: a failed write is a row without a title next
    /// time, not a broken session, so it logs.
    ///
    /// **Correction:** an earlier version wrote the title here when the session
    /// id was already known, and claimed the lock made that race-free. It did not: the id becomes
    /// visible before the record is written, and a title handed over in between found no record and
    /// was dropped, silently, for good (reproduced 20/20). It also did a record write on the UI
    /// thread for every resumed session.
    ///
    /// A resumed session keeps the title it was first given; one recorded before titles existed
    /// takes its first prompt after the resume.
    pub fn note_title(&mut self, as_typed: &str) {
        if self.title_noted {
            return;
        }
        let Some(title) = crate::persistence::title_from_prompt(as_typed) else {
            return;
        };
        self.title_noted = true;
        self.ingest.note_title(title);
    }

    /// The session tab was renamed (`prefix ,`). Handed to the ingestion thread, which writes it
    /// into the record adoption writes, or into the record once it exists (`flush_name`). The last
    /// one wins; `None` clears it.
    pub fn note_name(&mut self, name: Option<String>) {
        self.ingest.note_name(name);
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
            return Err(ConversationError::Provider(ProviderError::UnsupportedCapability(
                "interrupt",
            )));
        }
        let session_id = self.require_session()?;
        self.provider.interrupt_turn(InterruptTurnRequest { session_id })?;
        Ok(())
    }

    /// Whether this conversation's provider can put a message into the running turn (capability
    /// `steer`). Asked of the provider each time: one may learn mid-session that its agent cannot.
    pub fn steers(&self) -> bool {
        self.provider.capabilities().steer
    }

    /// Puts one user message into the running turn without interrupting it
    /// (`AgentProvider::steer_turn`), naming `expected_turn_id`, the turn the user saw running, so a turn
    /// that has ended or changed by the time the message arrives refuses it. `Ok` means sent: whether
    /// the agent read it comes back as `UserMessageDelivered` or `UserMessageDropped`. Refused here,
    /// nothing sent, when this side's projection shows no turn or another one, or the provider lacks the
    /// capability.
    pub fn steer_turn(
        &self,
        expected_turn_id: &str,
        client_message_id: &str,
        text: &str,
        as_typed: &str,
        origin: PromptOrigin,
    ) -> Result<(), ConversationError> {
        if !self.steers() {
            return Err(ConversationError::Provider(ProviderError::UnsupportedCapability(
                "steer",
            )));
        }
        let running = self.projection().active_turn_id.clone();
        if running.as_deref() != Some(expected_turn_id) {
            let message = if running.is_some() {
                "the turn it was typed in has ended"
            } else {
                "no turn is running"
            };
            return Err(ConversationError::Provider(ProviderError::Provider {
                code: crate::ProviderErrorCode::NoActiveTurn,
                message: message.to_string(),
            }));
        }
        let expected_turn_id = expected_turn_id.to_string();
        let session_id = self.require_session()?;
        self.provider.steer_turn_stamped(
            crate::provider::SteerTurnRequest {
                session_id,
                expected_turn_id,
                client_message_id: client_message_id.to_string(),
                text: text.to_string(),
                as_typed: as_typed.to_string(),
            },
            TurnStamp {
                typed_by_person: origin == PromptOrigin::Typed,
                ..TurnStamp::default()
            },
        )?;
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

    /// Hands the person's answer to the question set `interaction_id` to the provider. Checked against
    /// the set first (`QuestionRecord::check_resolution`), so an answer to a set no longer waiting, or
    /// one breaking a rule of the set, is refused here and nothing is sent. The set is `Answering`
    /// while the answer goes out and after the provider took it, until its `QuestionSettled`; a send
    /// refused here leaves it `Waiting` for another answer. Nothing here waits for the provider: the
    /// returned send carries its reply, and whoever reads a refusal or failure from it hands it to
    /// [`Self::question_send_failed`], which puts the set back to `Waiting`. Questions never enter
    /// `pending_permissions`, so nothing that answers cards can reach one: this is the one door.
    pub fn resolve_question(
        &mut self,
        interaction_id: &str,
        resolution: crate::InteractionResolution,
    ) -> Result<crate::InteractionSend, ConversationError> {
        let refused = |code, message: String| ConversationError::Provider(ProviderError::Provider { code, message });
        // Its own statement, so the projection's lock is released before anything else locks it.
        let checked = match self.projection().question(interaction_id) {
            Some(record) => record.check_resolution(&resolution),
            None => Err("no question with that id is waiting for an answer".to_string()),
        };
        if let Err(why) = checked {
            return Err(refused(crate::ProviderErrorCode::InteractionAnswerRefused, why));
        }
        let session_id = self.require_session()?;
        if !self.ingest.mark_question_send(interaction_id, true) {
            // Settled or taken by another answer between the check and here.
            return Err(refused(
                crate::ProviderErrorCode::InteractionNotFound,
                "this question is no longer waiting for an answer".to_string(),
            ));
        }
        let sent = self.provider.resolve_interaction(crate::ResolveInteractionRequest {
            session_id,
            interaction_id: interaction_id.to_string(),
            resolution,
        });
        sent.map_err(|error| {
            self.ingest.mark_question_send(interaction_id, false);
            ConversationError::Provider(error)
        })
    }

    /// The provider refused or failed the answer to `interaction_id` sent through
    /// [`Self::resolve_question`]: the set waits for another answer. Nothing changes for a set that
    /// settled meanwhile.
    pub fn question_send_failed(&self, interaction_id: &str) {
        self.ingest.mark_question_send(interaction_id, false);
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

    /// Has `wake` called whenever something was folded for the UI to take (`ConversationIngest::set_ui_wake`).
    pub fn set_ui_wake(&self, wake: crate::ingestion::UiWake) {
        self.ingest.set_ui_wake(wake);
    }

    /// Whether the UI is told when there is something to take; without it the UI polls.
    pub fn has_ui_wake(&self) -> bool {
        self.ingest.has_ui_wake()
    }

    /// [`Self::take_ui_delivery`], with each event's fold revision, for a caller that also reads
    /// snapshots and must not deliver an event one already carried (`crate::ingestion::RevisedDelivery`).
    pub fn take_revised_ui_delivery(&self) -> crate::ingestion::RevisedDelivery {
        self.ingest.take_revised_delivery()
    }

    /// Records that the caller just handed its UI a snapshot of this conversation's projection,
    /// read at `revision` (its `last_revision`, from the guard the snapshot was serialized from).
    /// The caller's next drain takes it back ([`Self::take_ui_snapshot_revision`]): every queued
    /// event tagged at or below it is one that snapshot already carried (`RevisedDelivery`'s doc).
    /// A later snapshot replaces an earlier one's, which it covers.
    pub fn note_ui_snapshot(&mut self, revision: u64) {
        self.ui_snapshot_revision = Some(revision);
    }

    /// Takes what [`Self::note_ui_snapshot`] recorded, if anything since the last take.
    pub fn take_ui_snapshot_revision(&mut self) -> Option<u64> {
        self.ui_snapshot_revision.take()
    }

    /// Closes the provider session and folds the terminal state.
    ///
    /// Fail-closed on the way out, mirroring `AgentSession::shutdown`: any permission still pending
    /// is resolved as `CancelledBySessionClose` locally, because a request belonging to a session
    /// that is ending can never be genuinely answered and the provider's own terminal events may not
    /// be drained before this process goes away. Idempotent.
    pub fn shutdown(&mut self) {
        // A failed close means the provider session's own teardown is not known to be done -- a wedged
        // or crashed sidecar, still closing over up to `SIDECAR_EXIT_GRACE` before it is even
        // killed. Releasing the lease anyway would let another client resume the same provider
        // session, writing the same transcript, while this one might still be live.
        //
        // `self.close_failed`, not a per-call local: `shutdown` is idempotent, so a second call
        // finds `self.session_id` already `None` and never calls `close_session` again -- a local
        // would forget the first call's failure and let the SECOND call's release check below run
        // with a freshly `false` value, releasing the lease anyway. This field is only ever set,
        // never cleared, so once a close fails the lease stays held for the rest of this
        // conversation's life, not just until the next `shutdown()` call.
        if let Some(session_id) = self.session_id.take() {
            if let Err(e) = self.provider.close_session(CloseSessionRequest { session_id }) {
                eprintln!("agent: AgentConversation::shutdown: close_session failed: {e}");
                self.close_failed = true;
            }
        }
        // Ingestion stops FIRST, so the terminal events folded below are the last word rather than
        // racing a thread that is still folding whatever the dying provider emits.
        self.ingest.stop();

        let pending: Vec<String> = self.projection().pending_permissions.keys().cloned().collect();
        for permission_id in pending {
            self.fold_locally(AgentDomainEvent::PermissionResolved {
                permission_id,
                outcome: PermissionOutcome::CancelledBySessionClose,
            });
        }
        let already_closed = matches!(self.projection().status, crate::ProjectionStatus::Closed { .. });
        if !already_closed {
            self.fold_locally(AgentDomainEvent::SessionClosed {
                reason: "closed_by_host".to_string(),
            });
        }
        // Released last: the lease must outlive the provider's own session teardown, so no other
        // client can acquire it while this one is still closing. On a failed close, skip this
        // entirely and leave the lease held inside `self.ingest`: `provider` is declared before
        // `ingest` on this struct, so dropping the conversation (`drop(backend)` in `shell`) tears
        // the provider down first and only then drops `ingest`, releasing the lease with it -- never
        // before the provider's own teardown has actually run.
        if !self.close_failed {
            self.ingest.release_lease();
        }
    }

    /// Folds an event this side produced rather than the provider.
    ///
    /// `ConversationIngest::fold_locally` both applies it to the projection and queues it for the
    /// UI, so this one call is the whole sidecar half -- nothing else has to route it to the pump.
    ///
    /// Was private and named `fold`, used only by `shutdown`'s fail-closed terminal events. Widened
    /// to `pub` for a second, non-terminal case: the user's own prompt, which no provider reports
    /// back (see `core/src/agent_backend.rs::send_turn`). Still never for provider lifecycle state,
    /// which is the provider's to state.
    pub fn fold_locally(&self, event: AgentDomainEvent) {
        self.ingest.fold_locally(event);
    }

    fn require_session(&self) -> Result<String, ConversationError> {
        self.session_id.clone().ok_or(ConversationError::NoSession)
    }

    /// Stops folding at the sequence the ingest reached, runs the caller's last pump, and takes the
    /// records a move carries: what the destination folds, and the conversation's own state.
    ///
    /// `last_pump` is the caller's ordinary pump, run once more: the events folded and not yet
    /// delivered are where its answers (bypass, a saved rule, auto) and its "answered for you" notes
    /// are made, and frozen undelivered they would be made in neither window. It returns once the
    /// answers it sent were taken by the agent, or says why not. The move is refused, and folding
    /// resumes, when it fails, when it leaves anything undelivered, and when the conversation is
    /// closing or is idle with a card still waiting.
    ///
    /// The conversation stays frozen after a success: folding resumes only through
    /// [`Self::unfreeze`], when the move does not happen after all. Never waits for a batch being
    /// folded: refused as [`HandoffRefused::Folding`] meanwhile, before the last pump runs.
    pub fn freeze_for_move(
        &mut self,
        last_pump: impl FnOnce(&mut AgentConversation) -> Result<(), String>,
    ) -> Result<(ChatMoveCore, ConversationHandoff), HandoffRefused> {
        self.freeze_folding()?;
        if let Err(why) = last_pump(self) {
            self.ingest.unfreeze();
            return Err(HandoffRefused::LastPump(why));
        }
        self.take_frozen()
    }

    /// The first half of [`Self::freeze_for_move`], for a caller whose last pump needs more than the
    /// conversation (the tab that holds it, whose answers and notes the pump records): stops folding
    /// at the sequence the ingest reached. Refused, with nothing changed, for a conversation that is
    /// closing, and while its ingest folds a batch or writes after one ([`HandoffRefused::Folding`]:
    /// ask again, as nothing here waits for it). The caller runs its pump, then [`Self::take_frozen`],
    /// or [`Self::unfreeze`] when its pump failed.
    pub fn freeze_folding(&mut self) -> Result<(), HandoffRefused> {
        if self.close_failed || self.session_id.is_none() {
            return Err(HandoffRefused::Closing);
        }
        if !self.ingest.try_freeze() {
            return Err(HandoffRefused::Folding);
        }
        Ok(())
    }

    /// The second half of [`Self::freeze_for_move`], after the caller's last pump: the records a move
    /// carries. Refused, and folding resumed, when the conversation is closing, when anything folded is
    /// still undelivered, and when it is idle with a card still waiting.
    pub fn take_frozen(&mut self) -> Result<(ChatMoveCore, ConversationHandoff), HandoffRefused> {
        let taken = if self.close_failed || self.session_id.is_none() {
            Err(HandoffRefused::Closing)
        } else {
            self.ingest.take_move_record()
        };
        let (core, ingest) = match taken {
            Ok(taken) => taken,
            Err(refused) => {
                self.ingest.unfreeze();
                return Err(refused);
            }
        };
        let (provider_name, provider_advertises_resume) = self.ingest.identity();
        let AgentConversation {
            conversation_id,
            canonical_cwd,
            // Adopted by the provider's own handoff in the destination.
            provider: _,
            external: _,
            capabilities,
            info,
            session_id,
            // Its state is in `ingest` above; the thread is the source's own.
            ingest: _,
            title_noted,
            // Checked above: a conversation whose close failed does not move.
            close_failed: _,
            // Revisions are per projection: the destination notes its own first snapshot's.
            ui_snapshot_revision: _,
            declared_client_tools,
        } = &*self;
        let crate::ingestion::IngestHandoff {
            provider_session_id,
            resume_outcome,
            stats,
            pending_title,
            pending_name,
            record_written,
            history_dirty,
        } = ingest;
        let handoff = ConversationHandoff {
            conversation_id: conversation_id.clone(),
            canonical_cwd: canonical_cwd.clone(),
            capabilities: *capabilities,
            info: info.clone(),
            session_id: session_id.clone(),
            title_noted: *title_noted,
            provider_name: provider_name.to_string(),
            provider_advertises_resume,
            provider_session_id,
            resume_outcome,
            stats,
            pending_title,
            pending_name: pending_name.map(|name| PendingName { name }),
            record_written,
            history_dirty,
            declared_client_tools: declared_client_tools.clone(),
        };
        Ok((core, handoff))
    }

    /// Folding resumes after a freeze whose move did not happen: from where the provider kept its
    /// events, so nothing it produced meanwhile is lost.
    pub fn unfreeze(&mut self) {
        self.ingest.unfreeze();
    }

    /// The provider's way to move with the conversation to another process, when it has one.
    #[cfg(target_os = "linux")]
    pub fn mover(&self) -> Option<&dyn crate::chat_mover::ChatMover> {
        self.provider.mover()
    }

    /// The session lease's descriptor for the window the conversation moves to (a second descriptor
    /// on the same open file, close-on-exec). `None` when the conversation holds no lease.
    pub fn lease_descriptor(&self) -> Option<std::io::Result<std::os::fd::OwnedFd>> {
        self.ingest.lease_descriptor()
    }

    /// Continues, in this window, a conversation another window froze for a move: the projection
    /// rebuilt from `core` by folding, the conversation's own state from `handoff`, driven by the
    /// adopted provider and holding the lease that crossed as a descriptor. Folding starts at once.
    pub fn adopt_moved(
        core: ChatMoveCore,
        handoff: ConversationHandoff,
        provider: MovedProvider,
    ) -> Result<AgentConversation, HandoffRefused> {
        let projection = crate::AgentSessionProjection::adopt(&core).map_err(HandoffRefused::Seed)?;
        let ConversationHandoff {
            conversation_id,
            canonical_cwd,
            capabilities,
            info,
            session_id,
            title_noted,
            provider_name,
            provider_advertises_resume,
            provider_session_id,
            resume_outcome,
            stats,
            pending_title,
            pending_name,
            record_written,
            history_dirty,
            declared_client_tools,
        } = handoff;
        let MovedProvider { agent, external, lease } = provider;
        let name: &'static str = external.as_ref().map_or(PROVIDER_NAME, |external| external.name());
        if name != provider_name {
            return Err(HandoffRefused::Mismatch("the provider"));
        }
        if conversation_id != conversation_id_for_cwd(&canonical_cwd) {
            return Err(HandoffRefused::Mismatch("the directory"));
        }
        let cwd_string = canonical_cwd.to_string_lossy().to_string();
        if let Some(lease) = &lease {
            let for_this = provider_session_id
                .as_deref()
                .is_some_and(|id| lease.is_for(name, &cwd_string, id));
            if !for_this {
                return Err(HandoffRefused::Mismatch("the session lease"));
            }
        }
        let ingest = ConversationIngest::start_moved(
            Arc::clone(&agent),
            name,
            conversation_id.clone(),
            cwd_string,
            provider_advertises_resume,
            crate::ingestion::MovedIngest {
                projection,
                core,
                state: crate::ingestion::IngestHandoff {
                    provider_session_id,
                    resume_outcome,
                    stats,
                    pending_title,
                    pending_name: pending_name.map(|pending| pending.name),
                    record_written,
                    history_dirty,
                },
                lease,
            },
        );
        Ok(Self {
            conversation_id,
            canonical_cwd,
            provider: agent,
            external,
            capabilities,
            info,
            session_id,
            ingest,
            title_noted,
            close_failed: false,
            ui_snapshot_revision: None,
            declared_client_tools,
        })
    }

    /// The move is committed: the lease file names this process as its owner, so whoever reads it
    /// (the session chooser's "in window 2") finds the window that drives the session now. Written
    /// through the descriptor the lease holds; the lock is never let go.
    pub fn commit_moved(&mut self) -> std::io::Result<()> {
        self.ingest.rewrite_lease_owner(std::process::id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ResumeSessionRequest;
    use crate::{ContentKind, ProviderErrorCode, TurnOutcome};
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
        /// What `close_session` returns, once, so `shutdown`'s handling of a failed close can
        /// be driven without a real wedged or crashed provider.
        close_session_error: Mutex<Option<ProviderError>>,
        /// Every event ever queued. A test waits for ingestion to reach exactly this, which is a
        /// real condition rather than a sleep long enough to probably be fine.
        queued_total: std::sync::atomic::AtomicU64,
        /// The trust each `create_session` was asked under, in order.
        trusts: Mutex<Vec<ProjectTrust>>,
        /// The sequence number of the last provider event queued and of the last one pumped, for a
        /// fake that numbers its events; `None` for one that does not.
        wire_queued: Mutex<Option<u64>>,
        wire_pumped: Mutex<Option<u64>>,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self {
                capabilities: ProviderCapabilities {
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    steer: false,
                    ..Default::default()
                },
                ..Default::default()
            }
        }
        fn queue(&self, event: AgentDomainEvent) {
            self.queued_events.lock().unwrap().push(event);
            self.queued_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        /// `events`, as what the provider made of its own event number `sequence`.
        fn queue_wire(&self, sequence: u64, events: Vec<AgentDomainEvent>) {
            let mut queued = self.queued_events.lock().unwrap();
            let count = events.len() as u64;
            queued.extend(events);
            *self.wire_queued.lock().unwrap() = Some(sequence);
            self.queued_total.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
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
            ProviderInfo {
                sidecar_version: "fake".into(),
                protocol_major: 1,
                ..Default::default()
            }
        }
        fn create_session(&self, request: CreateSessionRequest) -> Result<String, ProviderError> {
            self.trusts.lock().unwrap().push(request.project);
            self.calls
                .lock()
                .unwrap()
                .push(format!("create_session(cwd={})", request.cwd));
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
            self.calls.lock().unwrap().push(format!(
                "resolve_permission({}, allow={})",
                request.permission_id,
                request.decision.allows()
            ));
            Ok(())
        }
        fn close_session(&self, _r: CloseSessionRequest) -> Result<(), ProviderError> {
            self.calls.lock().unwrap().push("close_session".into());
            if let Some(e) = self.close_session_error.lock().unwrap().take() {
                return Err(e);
            }
            Ok(())
        }
        fn pump(&self) -> Vec<AgentDomainEvent> {
            let mut queued = self.queued_events.lock().unwrap();
            *self.wire_pumped.lock().unwrap() = *self.wire_queued.lock().unwrap();
            std::mem::take(&mut *queued)
        }
        fn pumped_through(&self) -> Option<u64> {
            *self.wire_pumped.lock().unwrap()
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
    ///   `~/.local/state/eitri/` and `$XDG_RUNTIME_DIR/eitri/`. That write happens on the
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
        conversation_in_as(fake, dir, ProjectTrust::Untrusted)
    }

    fn conversation_in_as(
        fake: Arc<FakeProvider>,
        dir: &Path,
        project: ProjectTrust,
    ) -> Result<AgentConversation, ConversationError> {
        struct Shared(Arc<FakeProvider>);
        impl AgentProvider for Shared {
            fn capabilities(&self) -> ProviderCapabilities {
                self.0.capabilities()
            }
            fn info(&self) -> ProviderInfo {
                self.0.info()
            }
            fn create_session(&self, r: CreateSessionRequest) -> Result<String, ProviderError> {
                self.0.create_session(r)
            }
            fn resume_session(&self, r: ResumeSessionRequest) -> Result<String, ProviderError> {
                self.0.resume_session(r)
            }
            fn send_turn(&self, r: SendTurnRequest) -> Result<String, ProviderError> {
                self.0.send_turn(r)
            }
            fn interrupt_turn(&self, r: InterruptTurnRequest) -> Result<(), ProviderError> {
                self.0.interrupt_turn(r)
            }
            fn resolve_permission(&self, r: ResolvePermissionRequest) -> Result<(), ProviderError> {
                self.0.resolve_permission(r)
            }
            fn close_session(&self, r: CloseSessionRequest) -> Result<(), ProviderError> {
                self.0.close_session(r)
            }
            fn pump(&self) -> Vec<AgentDomainEvent> {
                self.0.pump()
            }
            fn pumped_through(&self) -> Option<u64> {
                self.0.pumped_through()
            }
        }
        AgentConversation::create(Arc::new(Shared(fake)), dir, project)
    }

    /// Whether the session's lease can be taken, asked again for up to a second while it looks held.
    /// A lock just let go can look held for a moment while another test thread starts a child
    /// process (the child shares every descriptor until it execs); one that was never let go is
    /// still held after the second. So what this checks is "released within a second", a little
    /// weaker than "released when its holder was dropped".
    fn lease_comes_free(cwd: &str, session: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match SessionLease::try_acquire(PROVIDER_NAME, cwd, session) {
                Ok(_) => return true,
                Err(LeaseError::AlreadyHeld) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(_) => return false,
            }
        }
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

    /// Asks for the freeze until the ingest has no batch in hand, as a window asks again on its next tick
    /// (the freeze itself never waits); a refusal for any other reason is left for the move to report.
    fn settled_freeze(source: &mut AgentConversation) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while matches!(source.freeze_folding(), Err(HandoffRefused::Folding)) {
            assert!(
                std::time::Instant::now() < deadline,
                "the ingest let a freeze through within 5 s"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
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

    /// The record adoption writes for `session_opened()`, once it exists.
    fn adopted_record(conversation: &AgentConversation) -> ConversationRecord {
        wait_for(conversation, "the adopted session's record on disk", |c| {
            crate::persistence::load_conversation_record(c.conversation_id(), "claude-uuid-abc").is_ok()
        });
        crate::persistence::load_conversation_record(conversation.conversation_id(), "claude-uuid-abc").unwrap()
    }

    /// The normal order: the first prompt goes out before the SDK reports Claude's session id (it
    /// does that at the start of the first turn), so the title waits for adoption and is written
    /// with the record.
    #[test]
    fn a_title_noted_before_adoption_is_written_with_the_record() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(Arc::clone(&fake));
        conversation.note_title("\n  fix   the picker\nsecond line");
        fake.queue(session_opened());
        assert_eq!(adopted_record(&conversation).title.as_deref(), Some("fix the picker"));
    }

    /// Waits until the adopted session's record carries a title, and returns it.
    fn titled_record(conversation: &AgentConversation) -> Option<String> {
        wait_for(conversation, "the record's title written", |c| {
            crate::persistence::load_conversation_record(c.conversation_id(), "claude-uuid-abc")
                .is_ok_and(|r| r.title.is_some())
        });
        adopted_record(conversation).title
    }

    /// The other order: the session was already adopted, so the record exists and gets the title
    /// -- written by the ingestion thread, never by the caller's.
    #[test]
    fn a_title_noted_after_adoption_is_written_into_the_record() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        assert_eq!(adopted_record(&conversation).title, None);
        conversation.note_title("later prompt");
        assert_eq!(titled_record(&conversation).as_deref(), Some("later prompt"));
    }

    /// The gap the first version fell into: the session id is visible (adoption has set it under
    /// the lock) but the record is not written yet. A title handed over there was dropped, 20 runs
    /// out of 20 in a reproduction. Noting it the moment the id shows is as close to that
    /// gap as a test can aim; the title must still arrive.
    #[test]
    fn a_title_noted_the_moment_the_session_id_appears_is_not_lost() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        wait_for(&conversation, "the session id visible", |c| {
            c.provider_session_id().is_some()
        });
        conversation.note_title("said in the gap");
        assert_eq!(titled_record(&conversation).as_deref(), Some("said in the gap"));
    }

    /// A blank prompt names nothing, and after the first real one nothing renames the session.
    #[test]
    fn only_the_first_prompt_with_text_names_the_session() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(Arc::clone(&fake));
        conversation.note_title("   \n\t");
        conversation.note_title("the real subject");
        conversation.note_title("a follow-up");
        fake.queue(session_opened());
        assert_eq!(adopted_record(&conversation).title.as_deref(), Some("the real subject"));
        conversation.note_title("after adoption");
        // A rename would be written by the ingestion thread within a poll or two; give it several.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let record =
            crate::persistence::load_conversation_record(conversation.conversation_id(), "claude-uuid-abc").unwrap();
        assert_eq!(record.title.as_deref(), Some("the real subject"));
    }

    fn turn_completed(turn_id: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn_id.into(),
            outcome: crate::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    /// The stored history (A) for the adopted session, once it exists.
    fn stored_history(conversation: &AgentConversation) -> crate::history::StoredHistory {
        wait_for(conversation, "the session's stored history on disk", |c| {
            crate::history::store::load(c.conversation_id(), "claude-uuid-abc").is_ok()
        });
        crate::history::store::load(conversation.conversation_id(), "claude-uuid-abc").unwrap()
    }

    fn history_path(conversation: &AgentConversation) -> std::path::PathBuf {
        crate::history::store::history_path(conversation.conversation_id(), "claude-uuid-abc").unwrap()
    }

    /// Resuming must load history into the projection the panel reads.
    ///
    /// The unit tests in `history::load` prove the loader; this proves the loader is REACHED and
    /// that what it produces is the projection the panel reads. Without it, `resume` could hand
    /// `ConversationIngest::start` a `default()` projection and every one of those tests would
    /// still pass.
    ///
    /// The transcript (B) cannot exist for a fresh uuid, so this exercises the fallback to Eitri's
    /// own copy, which is also the only half a test can set up without writing under
    /// `$CLAUDE_CONFIG_DIR` -- something this feature never does because that directory belongs to the CLI.
    #[test]
    fn a_resumed_conversation_opens_carrying_what_it_already_said() {
        let dir = unique_dir();
        let canonical = dir.canonicalize().expect("the test workspace exists");
        let conversation_id = conversation_id_for_cwd(&canonical);
        let session = format!("sess-{}", uuid::Uuid::new_v4().simple());
        crate::history::store::save(&crate::history::StoredHistory::from_collections(
            conversation_id.clone(),
            session.clone(),
            vec![crate::UserPromptRecord {
                seq: 1,
                text: "what did we say?".into(),
                delivered: false,
            }],
            vec![crate::TranscriptMessage {
                seq: 2,
                text: "this much".into(),
            }],
            vec![],
        ))
        .expect("writing the stored history should succeed");

        /// Accepts the resume and then states the verdict, so `watch_resume_take_hold` answers at
        /// once instead of spending its whole window. The live delta rides in the same pump, which
        /// is what makes it genuinely live: it is folded after the seed, by the ingestion thread.
        struct Resuming {
            events: Mutex<Vec<AgentDomainEvent>>,
        }
        impl AgentProvider for Resuming {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    resume: true,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                }
            }
            fn info(&self) -> ProviderInfo {
                ProviderInfo {
                    sidecar_version: "fake".into(),
                    protocol_major: 1,
                    ..Default::default()
                }
            }
            fn create_session(&self, _r: CreateSessionRequest) -> Result<String, ProviderError> {
                unreachable!("this test only resumes")
            }
            fn resume_session(&self, _r: ResumeSessionRequest) -> Result<String, ProviderError> {
                Ok("verdandi-session-resumed".into())
            }
            fn send_turn(&self, _r: SendTurnRequest) -> Result<String, ProviderError> {
                Ok("turn-1".into())
            }
            fn interrupt_turn(&self, _r: InterruptTurnRequest) -> Result<(), ProviderError> {
                Ok(())
            }
            fn resolve_permission(&self, _r: ResolvePermissionRequest) -> Result<(), ProviderError> {
                Ok(())
            }
            fn close_session(&self, _r: CloseSessionRequest) -> Result<(), ProviderError> {
                Ok(())
            }
            fn pump(&self) -> Vec<AgentDomainEvent> {
                std::mem::take(&mut *self.events.lock().unwrap())
            }
        }

        let provider = Arc::new(Resuming {
            events: Mutex::new(vec![
                AgentDomainEvent::ResumeOutcome {
                    requested_provider_session_id: session.clone(),
                    status: crate::ResumeStatus::Attached,
                    attached_provider_session_id: Some(session.clone()),
                    forked: false,
                    detail: None,
                },
                AgentDomainEvent::ContentDelta {
                    turn_id: "t-live".into(),
                    kind: crate::ContentKind::Text,
                    text: "and here is more".into(),
                },
            ]),
        });

        let conversation = AgentConversation::resume(provider, &dir, &session, ProjectTrust::Untrusted)
            .expect("the fake provider attaches to the session it was asked for");
        wait_for_ingest(&conversation, 2);

        let projection = conversation.projection();
        let notice = projection.history.clone().expect("the stored copy was restored");
        assert_eq!(notice.source, crate::HistorySource::EitriCopy);
        assert_eq!(notice.restored_items, 2);
        assert_eq!(
            projection
                .user_prompts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            vec!["what did we say?"]
        );
        // The live delta is its own message, not an appendix to the restored one -- the flag was
        // closed after loading -- and it sorts above the whole restored range.
        assert_eq!(
            projection
                .transcript
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["this much", "and here is more"]
        );
        assert!(projection.transcript[0].seq < notice.upto_seq);
        assert!(projection.transcript[1].seq >= notice.upto_seq);
    }

    /// History is stored at the turn boundary, never mid-turn.
    ///
    /// Check the absence of a write before completing the turn. A streamed reply can contain hundreds
    /// of events, so writing on each event would cause hundreds of extra fsyncs.
    #[test]
    fn history_is_written_at_a_turn_boundary_and_not_before() {
        let fake = Arc::new(FakeProvider::new());
        let conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        adopted_record(&conversation);
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        fake.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: crate::ContentKind::Text,
            text: "thinking about it".into(),
        });
        settle(&fake, &conversation);
        assert!(
            !history_path(&conversation).exists(),
            "an unfinished turn must not have been stored"
        );

        fake.queue(turn_completed("t1"));

        let stored = stored_history(&conversation);
        assert_eq!(stored.version, crate::history::HISTORY_FORMAT_VERSION);
        assert_eq!(stored.provider_session_id, "claude-uuid-abc");
        assert_eq!(stored.transcript.len(), 1);
        assert_eq!(stored.transcript[0].text, "thinking about it");
    }

    /// Invariant 12, and the reason it is an invariant: the title feature's first version argued
    /// the same property from a lock and was wrong, 20 reproductions out of 20. A history write is
    /// strictly heavier -- two fsyncs and a serialization of the whole conversation -- so this is
    /// asserted from the thread that actually performed the write, not from where the code sits.
    #[test]
    fn the_history_write_happens_on_the_ingestion_thread() {
        let fake = Arc::new(FakeProvider::new());
        let conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        adopted_record(&conversation);
        fake.queue(turn_completed("t1"));
        stored_history(&conversation);

        let writer = crate::ingestion::tests::history_writer_thread(conversation.conversation_id());
        let writer = writer.expect("a history write must have been recorded");
        assert_eq!(
            writer, "eitri-agent-ingest",
            "the write must not be on the caller's thread"
        );
        assert_ne!(
            Some(writer.as_str()),
            std::thread::current().name(),
            "this test's own thread must not be the writer"
        );
    }

    /// A second turn rewrites the file with both turns. The sweep runs only on the first write,
    /// so later turns do not repeat its cost.
    #[test]
    fn a_later_turn_rewrites_the_stored_history_with_everything_so_far() {
        let fake = Arc::new(FakeProvider::new());
        let conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        adopted_record(&conversation);
        fake.queue(AgentDomainEvent::UserPromptSubmitted { text: "first".into() });
        fake.queue(turn_completed("t1"));
        wait_for(&conversation, "the first turn stored", |c| {
            crate::history::store::load(c.conversation_id(), "claude-uuid-abc").is_ok_and(|h| h.user_prompts.len() == 1)
        });

        fake.queue(AgentDomainEvent::UserPromptSubmitted { text: "second".into() });
        fake.queue(turn_completed("t2"));
        wait_for(&conversation, "the second turn stored", |c| {
            crate::history::store::load(c.conversation_id(), "claude-uuid-abc").is_ok_and(|h| h.user_prompts.len() == 2)
        });
        let stored = stored_history(&conversation);
        assert_eq!(
            stored.user_prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    /// Stored history carries no live session state. Check the JSON rather than just the type to
    /// catch fields added later. Restoring a serialized `status: Running` would misrepresent the
    /// new session's state.
    #[test]
    fn the_stored_history_holds_no_live_session_state() {
        let fake = Arc::new(FakeProvider::new());
        let conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        adopted_record(&conversation);
        fake.queue(turn_completed("t1"));
        stored_history(&conversation);

        let text = std::fs::read_to_string(history_path(&conversation)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        for forbidden in [
            "status",
            "active_turn_id",
            "pending_permissions",
            "session_id",
            "model",
            "cwd",
            "usage",
            "last_revision",
            "next_seq",
        ] {
            assert!(!keys.contains(&forbidden), "{forbidden} must never be stored: {keys:?}");
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
        let verdict = conversation.watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
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
        let verdict = conversation.watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
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
        let verdict = conversation.watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        assert_eq!(verdict, ResumeVerdict::Attached);
    }

    #[test]
    fn a_refused_resume_is_reported_in_terms_of_the_session_that_is_gone() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(resume_outcome(
            "claude-uuid-abc",
            crate::ResumeStatus::Rejected,
            None,
            false,
        ));
        let verdict = conversation.watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
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
        let verdict = conversation.watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
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
        fake.queue(AgentDomainEvent::SessionUnavailable {
            reason: "stream ended early".into(),
        });
        let verdict = conversation.watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(500));
        assert_eq!(
            verdict,
            ResumeVerdict::Failed {
                reason: "stream ended early".into()
            }
        );
    }

    /// **Silence is no longer success.** The old code returned "held up" here, which is what made a
    /// rejection slower than the window -- measured at 1.7-2.3s against a 3.0s budget -- silently
    /// become an accepted, empty, dead conversation. It is now a distinct verdict meaning the
    /// answer has not arrived yet, and the caller proceeds knowing the provider still owes it one.
    #[test]
    fn an_expired_wait_means_not_yet_known_rather_than_success() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        let verdict = conversation.watch_resume_take_hold("claude-uuid-abc", std::time::Duration::from_millis(150));
        assert_eq!(verdict, ResumeVerdict::NotYetKnown);
        assert_ne!(
            verdict,
            ResumeVerdict::Attached,
            "an unanswered resume must not read as a confirmed one"
        );
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
        assert_eq!(
            fake.calls(),
            vec![format!("create_session(cwd={})", canonical.to_string_lossy())]
        );
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
            "a second Eitri window in one project must not be refused -- see this test's doc comment"
        );
        assert_eq!(
            second.calls(),
            vec![format!(
                "create_session(cwd={})",
                dir.canonicalize().unwrap().to_string_lossy()
            )],
            "the second window must really have asked the provider for its own session"
        );
    }

    #[test]
    fn create_fails_cleanly_on_a_cwd_that_does_not_exist() {
        struct Never;
        impl AgentProvider for Never {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            fn info(&self) -> ProviderInfo {
                ProviderInfo::default()
            }
            fn create_session(&self, _r: CreateSessionRequest) -> Result<String, ProviderError> {
                panic!("create_session must not be reached when cwd cannot be resolved")
            }
            fn resume_session(&self, _r: ResumeSessionRequest) -> Result<String, ProviderError> {
                unreachable!()
            }
            fn send_turn(&self, _r: SendTurnRequest) -> Result<String, ProviderError> {
                unreachable!()
            }
            fn interrupt_turn(&self, _r: InterruptTurnRequest) -> Result<(), ProviderError> {
                unreachable!()
            }
            fn resolve_permission(&self, _r: ResolvePermissionRequest) -> Result<(), ProviderError> {
                unreachable!()
            }
            fn close_session(&self, _r: CloseSessionRequest) -> Result<(), ProviderError> {
                unreachable!()
            }
            fn pump(&self) -> Vec<AgentDomainEvent> {
                vec![]
            }
        }
        let result = AgentConversation::create(
            Arc::new(Never),
            Path::new("/definitely/does/not/exist/eitri-test"),
            ProjectTrust::Untrusted,
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
        assert_eq!(
            conversation.provider_session_id(),
            None,
            "a created-but-never-opened session has no Claude identity"
        );

        fake.queue(session_opened());
        settle(&fake, &conversation);

        assert_eq!(conversation.session_id(), Some("verdandi-session-1"));
        assert_eq!(conversation.provider_session_id().as_deref(), Some("claude-uuid-abc"));
        assert_ne!(conversation.conversation_id(), "verdandi-session-1");
        assert_ne!(conversation.conversation_id(), "claude-uuid-abc");
    }

    #[test]
    fn ingestion_folds_the_projection_without_the_ui_asking() {
        let fake = Arc::new(FakeProvider::new());
        let conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        fake.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: "hi".into(),
        });

        // Nothing calls a pump. The projection advances anyway, which is the property: the reducer's
        // progress is no longer a function of the UI's.
        settle(&fake, &conversation);

        assert_eq!(conversation.projection().last_revision, 3);
        assert_eq!(
            conversation
                .projection()
                .transcript
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
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
        assert_eq!(
            conversation.projection().last_revision,
            revision_before,
            "send_turn must fold nothing"
        );
        assert_eq!(
            conversation.projection().active_turn_id,
            None,
            "state comes from the provider's own event"
        );
        assert_eq!(
            conversation.ingest_stats().events_ingested,
            1,
            "only the SessionOpened folded earlier"
        );
    }

    #[test]
    fn a_second_turn_while_one_is_active_is_rejected_locally_and_never_reaches_the_provider() {
        let fake = Arc::new(FakeProvider::new());
        let mut conversation = conversation_with(fake.clone());
        fake.queue(session_opened());
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        settle(&fake, &conversation);

        let result = conversation.send_turn("second");
        assert!(
            matches!(result, Err(ConversationError::TurnAlreadyActive)),
            "got: {result:?}"
        );
        assert!(
            result.unwrap_err().is_benign(),
            "an early second send must not tear the session down"
        );
        assert!(
            !fake.calls().iter().any(|c| c.starts_with("send_turn")),
            "got: {:?}",
            fake.calls()
        );
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
        assert!(
            matches!(result, Err(ConversationError::TurnAlreadyActive)),
            "got: {result:?}"
        );
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
            capabilities: ProviderCapabilities {
                interrupt: false,
                ..ProviderCapabilities::default()
            },
            ..Default::default()
        });
        let mut conversation = conversation_with(fake.clone());
        let result = conversation.interrupt();
        assert!(
            matches!(
                result,
                Err(ConversationError::Provider(ProviderError::UnsupportedCapability(
                    "interrupt"
                )))
            ),
            "got: {result:?}"
        );
        assert!(
            !fake.calls().iter().any(|c| c == "interrupt_turn"),
            "got: {:?}",
            fake.calls()
        );
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
            detail: Default::default(),
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
        assert!(
            result.unwrap_err().is_benign(),
            "an already-answered or unknown id is a logged no-op, not fatal"
        );
        assert!(
            !fake.calls().iter().any(|c| c.starts_with("resolve_permission")),
            "got: {:?}",
            fake.calls()
        );
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
            provider_prompt: None,
        });
        settle(&fake, &conversation);

        conversation
            .respond_permission("p1", PermissionDecision::Allow)
            .unwrap();
        assert!(
            fake.calls().iter().any(|c| c == "resolve_permission(p1, allow=true)"),
            "got: {:?}",
            fake.calls()
        );
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
            provider_prompt: None,
        });
        settle(&fake, &conversation);
        assert_eq!(conversation.projection().pending_permissions.len(), 1);

        conversation.shutdown();

        assert!(
            conversation.projection().pending_permissions.is_empty(),
            "a closing session can never answer them"
        );
        assert!(matches!(
            conversation.projection().status,
            crate::ProjectionStatus::Closed { .. }
        ));
        assert_eq!(fake.calls().iter().filter(|c| *c == "close_session").count(), 1);

        let revision_after_first = conversation.projection().last_revision;
        conversation.shutdown();
        assert_eq!(
            fake.calls().iter().filter(|c| *c == "close_session").count(),
            1,
            "close_session must not repeat"
        );
        assert_eq!(
            conversation.projection().last_revision,
            revision_after_first,
            "a repeat shutdown folds nothing"
        );
    }

    /// Releasing the lease when `close_session` fails would risk
    /// another client resuming the same provider session, over the same transcript, while this
    /// one might still be tearing down (a wedged or crashed provider, closing over up to
    /// `SIDECAR_EXIT_GRACE` before the child is even killed). `shutdown` skips the release on a failed
    /// close and leaves the lease inside `self.ingest`; `provider` is declared before `ingest` on
    /// [`AgentConversation`], so dropping the conversation tears the provider down first and only
    /// then drops the lease with it.
    #[test]
    fn a_failed_close_session_keeps_the_lease_held_until_the_conversation_is_dropped() {
        let fake = Arc::new(FakeProvider::new());
        *fake.close_session_error.lock().unwrap() = Some(ProviderError::Timeout);
        let mut conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        settle(&fake, &conversation);
        let cwd = conversation.canonical_cwd().to_string_lossy().into_owned();

        conversation.shutdown();
        assert_eq!(fake.calls().iter().filter(|c| *c == "close_session").count(), 1);

        assert!(
            matches!(
                SessionLease::try_acquire(PROVIDER_NAME, &cwd, "claude-uuid-abc"),
                Err(LeaseError::AlreadyHeld)
            ),
            "a failed close must not release the lease: another client must not be able to \
             re-acquire it while this one may still be tearing down"
        );

        // Not held forever: once the conversation -- and with it, in field-declaration order, the
        // provider before the lease -- is actually dropped, the lease goes too.
        drop(conversation);
        assert!(
            lease_comes_free(&cwd, "claude-uuid-abc"),
            "the lease must still be released once the conversation itself is dropped"
        );
    }

    /// On `a_failed_close_session_keeps_the_lease_held_until_the_conversation_is_dropped`:
    /// the row above proves the lease survives the ONE `shutdown()` call whose
    /// `close_session` actually failed. `shutdown` is documented as idempotent
    /// (`shutdown_closes_the_session_fail_closes_pending_permissions_and_is_idempotent` calls it
    /// twice as a supported pattern) and every real caller (`panel/src/teardown.rs`'s
    /// `tear_down`/`tear_down_all` and the handoff
    /// path) is not the only conceivable one. A per-call local `close_failed` forgets the first
    /// call's failure: on the second call `self.session_id` is already `None` (taken by the first),
    /// so `close_session` is never retried, the local is freshly `false`, and the unconditional
    /// `if !close_failed { release_lease() }` releases the lease anyway -- reopening exactly the
    /// race the fix closed, on the second call rather than the first.
    #[test]
    fn a_second_idempotent_shutdown_after_a_failed_close_still_does_not_release_the_lease() {
        let fake = Arc::new(FakeProvider::new());
        *fake.close_session_error.lock().unwrap() = Some(ProviderError::Timeout);
        let mut conversation = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        settle(&fake, &conversation);
        let cwd = conversation.canonical_cwd().to_string_lossy().into_owned();

        conversation.shutdown();
        assert_eq!(fake.calls().iter().filter(|c| *c == "close_session").count(), 1);
        assert!(
            matches!(
                SessionLease::try_acquire(PROVIDER_NAME, &cwd, "claude-uuid-abc"),
                Err(LeaseError::AlreadyHeld)
            ),
            "sanity: the first, failing shutdown must not release the lease"
        );

        // The second call is the idempotent repeat the doc comment and the sibling test both say is
        // supported. `session_id` is already `None`, so `close_session` cannot be asked again --
        // this call learns nothing new about the close, and must not forget what the first call
        // already learned.
        conversation.shutdown();
        assert_eq!(
            fake.calls().iter().filter(|c| *c == "close_session").count(),
            1,
            "close_session must still not repeat on the second call"
        );
        assert!(
            matches!(
                SessionLease::try_acquire(PROVIDER_NAME, &cwd, "claude-uuid-abc"),
                Err(LeaseError::AlreadyHeld)
            ),
            "a second, idempotent shutdown() must not release a lease the first call's failed \
             close was supposed to keep held"
        );

        drop(conversation);
        assert!(
            lease_comes_free(&cwd, "claude-uuid-abc"),
            "the lease must still be released once the conversation itself is dropped"
        );
    }

    /// A provider that accepts a resume and says it attached, recording what it was asked.
    struct AcceptingResume {
        asked: Mutex<Vec<String>>,
        /// The trust each resume was asked under, in order.
        trusts: Mutex<Vec<ProjectTrust>>,
        events: Mutex<Vec<AgentDomainEvent>>,
    }

    impl AcceptingResume {
        fn attaching_to(session: &str) -> Arc<Self> {
            Arc::new(Self {
                asked: Mutex::new(Vec::new()),
                trusts: Mutex::new(Vec::new()),
                events: Mutex::new(vec![AgentDomainEvent::ResumeOutcome {
                    requested_provider_session_id: session.to_string(),
                    status: crate::ResumeStatus::Attached,
                    attached_provider_session_id: Some(session.to_string()),
                    forked: false,
                    detail: None,
                }]),
            })
        }
    }

    impl AgentProvider for AcceptingResume {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            }
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                sidecar_version: "fake".into(),
                protocol_major: 1,
                ..Default::default()
            }
        }
        fn create_session(&self, _r: CreateSessionRequest) -> Result<String, ProviderError> {
            unreachable!("these tests only resume")
        }
        fn resume_session(&self, r: ResumeSessionRequest) -> Result<String, ProviderError> {
            self.trusts.lock().unwrap().push(r.project);
            self.asked.lock().unwrap().push(r.provider_session_id);
            Ok("verdandi-session-resumed".into())
        }
        fn send_turn(&self, _r: SendTurnRequest) -> Result<String, ProviderError> {
            Ok("turn-1".into())
        }
        fn interrupt_turn(&self, _r: InterruptTurnRequest) -> Result<(), ProviderError> {
            Ok(())
        }
        fn resolve_permission(&self, _r: ResolvePermissionRequest) -> Result<(), ProviderError> {
            Ok(())
        }
        fn close_session(&self, _r: CloseSessionRequest) -> Result<(), ProviderError> {
            Ok(())
        }
        fn pump(&self) -> Vec<AgentDomainEvent> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }
    }

    /// A lease taken before the resume is the one the resume keeps: it neither contends with it
    /// (a plain `resume` of a session whose lease is already held is refused) nor lets go of it while
    /// the conversation lives.
    #[test]
    fn a_resume_keeps_the_lease_it_was_handed_instead_of_contending_with_it() {
        let dir = unique_dir();
        let cwd = dir.to_string_lossy().to_string();
        let session = "claude-uuid-held-ahead";

        let lease = SessionLease::try_acquire(PROVIDER_NAME, &cwd, session).unwrap();
        let plain = AgentConversation::resume(
            AcceptingResume::attaching_to(session),
            &dir,
            session,
            ProjectTrust::Untrusted,
        );
        assert!(
            matches!(plain, Err(ConversationError::LeaseHeld { .. })),
            "a resume that takes its own lease cannot share the one already held"
        );

        let conversation = AgentConversation::resume_holding(
            AcceptingResume::attaching_to(session),
            &dir,
            session,
            Some(lease),
            ProjectTrust::Untrusted,
        )
        .expect("the resume attaches under the lease it was handed");
        assert!(
            matches!(
                SessionLease::try_acquire(PROVIDER_NAME, &cwd, session),
                Err(LeaseError::AlreadyHeld)
            ),
            "the handed lease is still the one held while the conversation lives"
        );
        drop(conversation);
        assert!(lease_comes_free(&cwd, session), "and it goes with the conversation");
    }

    /// Eitri's own copy ending in a turn the CLI started: the restored turn is over, so the first
    /// prompt after the resume reaches the provider instead of being refused as a turn in progress.
    #[test]
    fn a_resume_whose_stored_copy_ends_in_a_cli_turn_takes_the_first_prompt() {
        let dir = unique_dir();
        let conversation_id = conversation_id_for_cwd(&dir.canonicalize().expect("the test workspace exists"));
        let session = format!("sess-{}", uuid::Uuid::new_v4().simple());
        let parts = crate::history::HistoryParts {
            user_prompts: vec![crate::UserPromptRecord {
                seq: 1,
                text: "check every hour".into(),
                delivered: false,
            }],
            turn_starts: vec![crate::TurnStartRecord {
                seq: 2,
                turn_id: "cli-9".into(),
                origin: crate::TurnOrigin::Cli,
                cause: None,
            }],
            ..Default::default()
        };
        crate::history::store::save(&crate::history::StoredHistory::from_parts(
            conversation_id,
            session.clone(),
            parts,
        ))
        .expect("writing the stored history should succeed");

        let mut conversation = AgentConversation::resume(
            AcceptingResume::attaching_to(&session),
            &dir,
            &session,
            ProjectTrust::Untrusted,
        )
        .expect("the fake provider attaches to the session it was asked for");
        wait_for_ingest(&conversation, 1);
        assert_eq!(
            conversation.projection().turn_starts.len(),
            1,
            "the CLI's turn was restored"
        );

        let sent = conversation.send_turn("and now?");
        assert!(sent.is_ok(), "the first prompt after the resume is taken: {sent:?}");
    }

    /// A lease for some other session must never stand in for this one: nothing is asked of the
    /// provider, and the session the lease was for is released with it.
    #[test]
    fn a_lease_for_another_session_is_refused_before_the_provider_is_asked() {
        let dir = unique_dir();
        let cwd = dir.to_string_lossy().to_string();
        let provider = AcceptingResume::attaching_to("claude-uuid-asked-for");

        let lease = SessionLease::try_acquire(PROVIDER_NAME, &cwd, "claude-uuid-someone-else").unwrap();
        let result = AgentConversation::resume_holding(
            provider.clone(),
            &dir,
            "claude-uuid-asked-for",
            Some(lease),
            ProjectTrust::Untrusted,
        );
        assert!(matches!(result, Err(ConversationError::Lease(_))), "{:?}", result.err());
        assert!(
            provider.asked.lock().unwrap().is_empty(),
            "the provider was never asked"
        );
        assert!(
            SessionLease::try_acquire(PROVIDER_NAME, &cwd, "claude-uuid-someone-else").is_ok(),
            "a refused lease is not kept"
        );
    }

    /// A resume that fails gives its handed lease back, as one that took its own always did.
    #[test]
    fn a_failed_resume_releases_the_lease_it_was_handed() {
        let dir = unique_dir();
        let cwd = dir.to_string_lossy().to_string();
        let session = "claude-uuid-rejected";
        let provider = Arc::new(AcceptingResume {
            asked: Mutex::new(Vec::new()),
            trusts: Mutex::new(Vec::new()),
            events: Mutex::new(vec![AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id: session.to_string(),
                status: crate::ResumeStatus::Rejected,
                attached_provider_session_id: None,
                forked: false,
                detail: Some("no such session".into()),
            }]),
        });
        let lease = SessionLease::try_acquire(PROVIDER_NAME, &cwd, session).unwrap();
        let result = AgentConversation::resume_holding(provider, &dir, session, Some(lease), ProjectTrust::Untrusted);
        assert!(matches!(result, Err(ConversationError::ResumeRejected { .. })));
        assert!(
            lease_comes_free(&cwd, session),
            "the session is free again once its resume failed"
        );
    }

    /// The trust a caller states reaches the provider as stated, on a fresh session and on a resume
    /// alike; neither path picks one of its own.
    #[test]
    fn create_and_resume_hand_the_sessions_trust_to_the_provider() {
        for project in [ProjectTrust::Trusted, ProjectTrust::Untrusted] {
            let fake = Arc::new(FakeProvider::new());
            let conversation =
                conversation_in_as(Arc::clone(&fake), &unique_dir(), project).expect("create against the fake");
            assert_eq!(*fake.trusts.lock().unwrap(), vec![project]);
            drop(conversation);
        }

        for project in [ProjectTrust::Untrusted, ProjectTrust::Trusted] {
            let dir = unique_dir();
            let session = "claude-uuid-trust";
            let provider = AcceptingResume::attaching_to(session);
            let conversation = AgentConversation::resume_holding(provider.clone(), &dir, session, None, project)
                .expect("the fake provider attaches");
            assert_eq!(*provider.trusts.lock().unwrap(), vec![project]);
            drop(conversation);
        }
    }

    // ---- moving a conversation to the other window ----

    /// The caller's last pump at its plainest: hands the page everything folded so far.
    fn deliver(conversation: &mut AgentConversation) -> Result<(), String> {
        let _ = conversation.take_ui_delivery();
        Ok(())
    }

    fn moved_provider(lease: Option<SessionLease>) -> MovedProvider {
        let agent: Arc<dyn AgentProvider + Send + Sync> = Arc::new(FakeProvider::new());
        MovedProvider {
            agent,
            external: None,
            lease,
        }
    }

    fn json_of(conversation: &AgentConversation) -> serde_json::Value {
        serde_json::to_value(&*conversation.projection()).unwrap()
    }

    fn prompt(text: &str) -> AgentDomainEvent {
        AgentDomainEvent::UserPromptSubmitted { text: text.into() }
    }

    fn reply(turn_id: &str, text: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: turn_id.into(),
            kind: ContentKind::Text,
            text: text.into(),
        }
    }

    /// A source that opened its session and finished one turn.
    fn after_one_turn(fake: &Arc<FakeProvider>) -> AgentConversation {
        let conversation = conversation_with(Arc::clone(fake));
        fake.queue(session_opened());
        wait_for_ingest(&conversation, 1);
        conversation.fold_locally(prompt("first"));
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        fake.queue(reply("t1", "done"));
        fake.queue(turn_completed("t1"));
        wait_for_ingest(&conversation, 5);
        conversation
    }

    #[test]
    fn folded_through_is_the_providers_own_sequence_not_a_count_of_events() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = conversation_with(Arc::clone(&fake));
        // The provider's first event becomes two domain events, its second one.
        fake.queue_wire(
            1,
            vec![
                session_opened(),
                AgentDomainEvent::CliPermissionMode {
                    reported: "auto".into(),
                },
            ],
        );
        wait_for_ingest(&source, 2);
        fake.queue_wire(2, vec![AgentDomainEvent::BackgroundCalls { running: 0 }]);
        wait_for_ingest(&source, 3);

        settled_freeze(&mut source);
        let (core, _handoff) = source.freeze_for_move(deliver).expect("an idle chat moves");
        assert_eq!(
            core.folded_through, 2,
            "the destination reads on after the provider's second event, not its third"
        );
    }

    #[test]
    fn a_session_lost_before_it_opened_moves_unavailable() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = conversation_with(Arc::clone(&fake));
        source.fold_locally(prompt("first"));
        let lost = AgentDomainEvent::SessionUnavailable {
            reason: "the event stream failed".into(),
        };
        fake.queue(lost.clone());
        wait_for_ingest(&source, 2);

        settled_freeze(&mut source);
        let (core, handoff) = source.freeze_for_move(deliver).expect("a lost chat still moves");
        assert_eq!(core.lifecycle, vec![lost]);
        let adopted = AgentConversation::adopt_moved(core, handoff, moved_provider(None)).unwrap();
        assert_eq!(
            adopted.projection().status,
            crate::ProjectionStatus::Unavailable {
                reason: "the event stream failed".into()
            }
        );
        assert_eq!(json_of(&adopted), json_of(&source));
    }

    #[test]
    fn an_idle_chat_moves_with_no_running_turn_events() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        settled_freeze(&mut source);
        let (core, handoff) = source.freeze_for_move(deliver).expect("an idle chat moves");
        assert!(core.running_turn.is_empty(), "nothing runs");
        assert_eq!(core.lifecycle, vec![session_opened()]);
        assert_eq!(core.seed.through_revision, source.projection().last_revision);
        assert_eq!(
            core.folded_through, 0,
            "a provider that numbers nothing reports no position"
        );
        assert_eq!(handoff.provider_session_id.as_deref(), Some("claude-uuid-abc"));
        assert_eq!(handoff.session_id.as_deref(), Some("verdandi-session-1"));

        let adopted = AgentConversation::adopt_moved(core, handoff, moved_provider(None)).unwrap();
        assert_eq!(json_of(&adopted), json_of(&source));
        assert_eq!(adopted.projection().status, crate::ProjectionStatus::Running);
        assert_eq!(adopted.provider_session_id().as_deref(), Some("claude-uuid-abc"));
    }

    #[test]
    fn a_running_turn_moves_with_its_events_and_the_sources_seqs() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        source.fold_locally(prompt("second"));
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        fake.queue(session_opened());
        fake.queue(reply("t2", "working"));
        fake.queue(AgentDomainEvent::ToolCallStarted {
            turn_id: "t2".into(),
            tool_use_id: "call-1".into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "ls" }),
        });
        fake.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("call-1".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "ls" }),
            provider_prompt: None,
        });
        wait_for_ingest(&source, 11);

        settled_freeze(&mut source);
        let (core, handoff) = source.freeze_for_move(deliver).expect("a running turn moves");
        assert_eq!(core.lifecycle, vec![session_opened()], "as of the turn's start");
        assert_eq!(core.running_turn.len(), 6, "the prompt and every event since");
        assert_eq!(core.running_turn[0], prompt("second"));
        let wire = serde_json::to_string(&core).unwrap();
        let core: ChatMoveCore = serde_json::from_str(&wire).unwrap();
        let adopted = AgentConversation::adopt_moved(core, handoff, moved_provider(None)).unwrap();
        assert_eq!(
            json_of(&adopted),
            json_of(&source),
            "every seq and the revision, the card included"
        );
        assert!(adopted.projection().pending_permissions.contains_key("perm-1"));
        assert_eq!(adopted.projection().active_turn_id.as_deref(), Some("t2"));
    }

    #[test]
    fn the_cli_mode_and_account_limits_survive_an_idle_move() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        fake.queue(AgentDomainEvent::CliPermissionMode {
            reported: "auto".into(),
        });
        fake.queue(AgentDomainEvent::AccountLimits {
            used_percent: 63.0,
            window_minutes: Some(300),
            resets_at: Some(1_800_000_000),
            plan: None,
        });
        fake.queue(AgentDomainEvent::BackgroundCalls { running: 2 });
        fake.queue(AgentDomainEvent::CommandsAvailable {
            commands: vec![crate::SlashCommand {
                name: "compact".into(),
                description: "compact".into(),
                hint: None,
            }],
        });
        fake.queue(AgentDomainEvent::PolicyMismatch {
            requested: "default".into(),
            reported: "plan".into(),
        });
        wait_for_ingest(&source, 10);

        settled_freeze(&mut source);
        let (core, handoff) = source.freeze_for_move(deliver).unwrap();
        let adopted = AgentConversation::adopt_moved(core, handoff, moved_provider(None)).unwrap();
        let (moved, kept) = (adopted.projection(), source.projection());
        assert!(moved.cli_runs_auto(), "the CLI's own auto mode still decides there");
        assert_eq!(moved.account_limits, kept.account_limits);
        assert_eq!(moved.background_calls, 2);
        assert_eq!(moved.background_at_last_turn_end, kept.background_at_last_turn_end);
        assert_eq!(moved.available_commands, kept.available_commands);
        assert_eq!(moved.policy_mismatch, kept.policy_mismatch);
        assert_eq!(moved.export_settled(), kept.export_settled());
    }

    #[test]
    fn freeze_refuses_a_conversation_whose_close_failed() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        *fake.close_session_error.lock().unwrap() = Some(ProviderError::Timeout);
        source.shutdown();
        assert!(source.close_failed);
        settled_freeze(&mut source);
        let refused = source.freeze_for_move(|_| panic!("a closing conversation is not pumped for a move"));
        assert!(matches!(refused, Err(HandoffRefused::Closing)));
    }

    #[test]
    fn freeze_runs_the_last_pump_before_it_takes_the_record() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        let _ = source.take_ui_delivery();
        source.fold_locally(prompt("second"));
        fake.queue(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        fake.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-2".into(),
            tool_use_id: Some("call-2".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "a.rs" }),
            provider_prompt: None,
        });
        wait_for_ingest(&source, 8);

        let mut seen = Vec::new();
        settled_freeze(&mut source);
        let (core, _) = source
            .freeze_for_move(|conversation| {
                if let crate::UiDelivery::Events(events) = conversation.take_ui_delivery() {
                    seen = events;
                }
                // What the caller's pump does with a request its mode answers.
                conversation
                    .respond_permission("perm-2", crate::PermissionDecision::Allow)
                    .map_err(|e| e.to_string())
            })
            .expect("the pump delivered everything");
        assert_eq!(
            seen.len(),
            3,
            "the prompt, the turn's start and the request were still undelivered"
        );
        assert!(fake
            .calls()
            .contains(&"resolve_permission(perm-2, allow=true)".to_string()));
        assert!(core
            .running_turn
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })));

        // Frozen: what the provider produces now waits for the destination.
        fake.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "perm-2".into(),
            outcome: crate::PermissionOutcome::Allowed,
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(source.ingest_stats().events_ingested, 8, "nothing folded while frozen");
        assert_eq!(fake.queued_events.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_refused_freeze_leaves_the_conversation_folding() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        // A pump that hands the page nothing leaves the events undelivered.
        settled_freeze(&mut source);
        let refused = source.freeze_for_move(|_| Ok(()));
        assert!(matches!(refused, Err(HandoffRefused::Undelivered)));
        settled_freeze(&mut source);
        let refused = source.freeze_for_move(|_| Err("the agent did not take the answers".to_string()));
        assert!(matches!(refused, Err(HandoffRefused::LastPump(_))));

        fake.queue(AgentDomainEvent::BackgroundCalls { running: 1 });
        wait_for_ingest(&source, 6);
        assert_eq!(source.projection().background_calls, 1);
    }

    #[test]
    fn a_card_left_waiting_after_its_turn_ended_moves_with_it() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        let _ = source.take_ui_delivery();
        // A request still waiting when its turn ended: only folding it again rebuilds the card.
        source.fold_locally(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-late".into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: serde_json::json!({}),
            provider_prompt: None,
        });
        source.fold_locally(turn_completed("t-late"));
        settled_freeze(&mut source);
        let (core, handoff) = source.freeze_for_move(deliver).unwrap();
        assert_eq!(core.running_turn.len(), 2, "the turn is not over while its card waits");
        let adopted = AgentConversation::adopt_moved(core, handoff, moved_provider(None)).unwrap();
        assert!(adopted.projection().pending_permissions.contains_key("perm-late"));
        assert_eq!(json_of(&adopted), json_of(&source));
    }

    #[test]
    fn commit_moved_names_this_process_in_the_lease_without_unlocking_it() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        let _ = adopted_record(&source);
        let cwd = source.canonical_cwd().to_string_lossy().to_string();
        let held = || {
            matches!(
                SessionLease::try_acquire(PROVIDER_NAME, &cwd, "claude-uuid-abc"),
                Err(LeaseError::AlreadyHeld)
            )
        };
        assert!(held(), "the source holds the session");

        settled_freeze(&mut source);
        let (core, handoff) = source.freeze_for_move(deliver).unwrap();
        let descriptor = source.lease_descriptor().expect("the source holds a lease").unwrap();
        let lease = SessionLease::from_moved(descriptor, PROVIDER_NAME, &cwd, "claude-uuid-abc").unwrap();
        let path = lease.path().to_path_buf();
        // As the file reads while the source's process still owns it.
        std::fs::write(
            &path,
            r#"{"owner_pid":4194001,"instance_id":"first","started_at":"then"}"#,
        )
        .unwrap();

        let mut adopted = AgentConversation::adopt_moved(core, handoff, moved_provider(Some(lease))).unwrap();
        assert!(held());
        drop(source);
        assert!(
            held(),
            "the source letting go of its descriptor does not free the session"
        );

        adopted.commit_moved().unwrap();
        let owner: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(owner["owner_pid"], serde_json::json!(std::process::id()));
        assert_eq!(owner["instance_id"], serde_json::json!("first"));
        assert!(held(), "rewriting the owner never unlocks");

        drop(adopted);
        assert!(lease_comes_free(&cwd, "claude-uuid-abc"));
    }

    #[test]
    fn a_lease_for_another_session_is_not_adopted() {
        let fake = Arc::new(FakeProvider::new());
        let mut source = after_one_turn(&fake);
        let _ = adopted_record(&source);
        let cwd = source.canonical_cwd().to_string_lossy().to_string();
        settled_freeze(&mut source);
        let (core, handoff) = source.freeze_for_move(deliver).unwrap();
        let other = SessionLease::try_acquire(PROVIDER_NAME, &cwd, "claude-uuid-other").unwrap();
        let refused = AgentConversation::adopt_moved(core, handoff, moved_provider(Some(other)));
        assert!(matches!(refused, Err(HandoffRefused::Mismatch(_))));
    }

    #[test]
    fn history_dirty_set_by_a_turn_end_just_before_the_move_is_carried() {
        let fake = Arc::new(FakeProvider::new());
        let source = conversation_with(Arc::clone(&fake));
        fake.queue(session_opened());
        let _ = adopted_record(&source);
        let mut source = source;
        settled_freeze(&mut source);
        let (core, handoff) = source
            .freeze_for_move(|conversation| {
                // A turn ending just before the move: frozen, the source writes no history for it.
                conversation.fold_locally(turn_completed("t1"));
                let _ = conversation.take_ui_delivery();
                Ok(())
            })
            .unwrap();
        assert!(handoff.history_dirty);
        assert!(handoff.record_written);
        assert!(!history_path(&source).exists(), "the frozen source wrote nothing");
        drop(source);

        let adopted = AgentConversation::adopt_moved(core, handoff, moved_provider(None)).unwrap();
        wait_for(&adopted, "the destination writing the turn's history", |c| {
            history_path(c).exists()
        });
    }
}
