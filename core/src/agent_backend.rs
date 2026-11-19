//! Which Claude backend the agent panel drives, and the one shape the panel talks to.
//!
//! Two real backends exist and they are NOT interchangeable at the type level:
//!
//! - `agent::AgentSession` -- the legacy backend, which spawns `claude` itself, parses its
//!   stream-json, and gates tool permissions with a generated `PreToolUse` hook. Because the CLI
//!   wire protocol has no "a turn started" line, it synthesizes `TurnStarted` locally and queues
//!   it for the next pump with its fold revision.
//! - `agent::AgentConversation` over `agent::ClaudeSidecarProvider` -- the SDK path, where Verdandi
//!   emits real, server-originated events. It synthesizes nothing, so `send_turn` returns a turn id
//!   and no events; state arrives on the next `pump()`.
//!
//! That difference is the whole point of the migration and must not be papered over: the sidecar
//! path's state is authoritative because it comes from the server. This enum keeps both callable
//! from one place without pretending they are the same thing.
//!
//! Selected by `EITRI_AGENT_BACKEND=legacy|sidecar`, defaulting to `sidecar`.

use agent::setting_sources::ProjectTrust;
use agent::GateRoot;
use agent::{
    AgentConversation, AgentDomainEvent, AgentSession, AgentSessionProjection, ClaudeSidecarProvider,
    ConversationError, PermissionDecision, PermissionMode, ProjectionGuard, ProviderCapabilities, ProviderInfo,
    ResumableSession, Reviewer, RevisedDelivery, Sandbox, TrustPlan, TurnPolicy,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::agent_bridge::SessionModeChoice;
use crate::editor_tools::SERVER_NAME;

/// `reason` first, then the remedies of the sidecar `info` describes, each once. The panel recognises a
/// failure by substrings of its text, so the text itself stays as it was.
fn with_provider_notes(mut reason: String, info: &ProviderInfo) -> String {
    let note = agent::UNTESTED_CLAUDE_NOTE;
    if info.startup_diagnostics.iter().any(|line| line == note) && !reason.contains(note) {
        reason.push('\n');
        reason.push_str(note);
    }
    if let Some(version) = &info.sidecar_update {
        let note = format!("Sidecar update {version} ran this tab. To go back: eitri sidecar use bundled");
        if !reason.contains(&note) {
            reason.push('\n');
            reason.push_str(&note);
        }
    }
    reason
}

fn task_command_error(error: ConversationError) -> BackendError {
    match error {
        ConversationError::Provider(agent::ProviderError::UnsupportedCapability(method)) => BackendError {
            message: format!("this agent does not support {method}"),
            benign: true,
        },
        other => other.into(),
    }
}

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
    // False since 2026-09-27: this backend never starts the CLI in `bypassPermissions` any
    // more -- it always passes `--permission-mode default` and the hook. Eitri's bypass is its own
    // `allow` under that gate, which needs only the capability below.
    bypass_permission_mode: false,
    // Its interactive gate is the `PreToolUse` hook relay, which is this backend's primary,
    // end-to-end-verified permission mechanism -- not the leaky `can_use_tool` it also listens to.
    interactive_permission_mode: true,
    // Its CLI always runs `default` (`--permission-mode default`), and its hook relay has no way
    // to say "no decision": every request it raises is answered with Eitri's own policy.
    cli_auto_mode: false,
    steer: false,
    // The legacy backend drives the CLI directly and reports none of the sidecar's protocol 3.22
    // events.
    cli_turns: false,
    human_origin: false,
    background_tasks: false,
    workflow_progress: false,
    task_control: false,
    task_output: false,
    subagent_frames: false,
    session_notices: false,
    conversation_reset: false,
    structured_questions_v1: false,
    client_tools: false,
    direct_settings: false,
    task_output_range: false,
    plan_mode_v1: false,
    turn_content_v1: false,
    turn_content_limits: None,
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
    // Every offered mode is honoured through the interactive gate (bypass is Eitri answering
    // `allow` under it), and this backend never starts the CLI ungated.
    assert!(
        LEGACY_CAPABILITIES.interactive_permission_mode,
        "every offered permission mode needs the interactive gate"
    );
    assert!(
        !LEGACY_CAPABILITIES.bypass_permission_mode,
        "the legacy backend never starts the CLI ungated"
    );
    assert!(
        !LEGACY_CAPABILITIES.cli_auto_mode,
        "the legacy backend never runs the CLI's auto mode and cannot defer"
    );
};

/// A borrow of whichever projection this backend owns.
///
/// The sidecar's lives behind the ingestion thread's lock and the legacy backend's is a plain field.
/// Both deref to the same type, so call sites read `backend.projection().status` either way.
///
/// A `Guarded` one holds that lock, which is not reentrant: while it lives, its thread calls nothing
/// else on the same backend that reads conversation state, or that call waits on its own thread (a debug
/// build panics instead, naming the call). [`AgentBackend::provider_session_id`] is the exception, kept
/// behind a lock of its own.
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

/// Which agent a tab runs. Claude runs on the window's Claude backend ([`BackendKind`]); every other
/// agent is a provider of its own, started per tab ([`AgentBackend::start_external`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentId {
    Claude,
    Codex,
    /// An agent spoken to over ACP, by the name it was registered under.
    Acp(String),
}

impl AgentId {
    /// The name its conversation records and session leases carry: `claude`, `codex`, `acp:<name>`.
    pub fn record_name(&self) -> String {
        match self {
            AgentId::Claude => "claude".to_string(),
            AgentId::Codex => "codex".to_string(),
            AgentId::Acp(name) => format!("acp:{name}"),
        }
    }

    /// The reverse of [`record_name`](Self::record_name); `None` for anything else, an ACP name that is
    /// empty or carries a control character included.
    pub fn from_record_name(text: &str) -> Option<AgentId> {
        match text {
            "claude" => Some(AgentId::Claude),
            "codex" => Some(AgentId::Codex),
            other => {
                let name = other.strip_prefix("acp:")?;
                (!name.trim().is_empty() && !name.chars().any(char::is_control)).then(|| AgentId::Acp(name.to_string()))
            }
        }
    }

    /// Whether this agent runs as an external provider rather than on the window's Claude backend.
    pub fn is_external(&self) -> bool {
        !matches!(self, AgentId::Claude)
    }

    /// Whether the agent's own permission mode is set once, when its session starts, and cannot
    /// follow a change of the tab's mode: an ACP agent's (`set_mode` is sent at the start only).
    /// Claude's CLI never changes its mode either, but Eitri answers every request it raises, and
    /// Codex's reviewer follows the tab's mode at the next turn.
    pub fn mode_fixed_at_start(&self) -> bool {
        matches!(self, AgentId::Acp(_))
    }

    /// The permission modes a tab of this agent offers, in `Shift+Tab`'s order.
    pub fn permission_modes(&self) -> &'static [&'static str] {
        if self.mode_fixed_at_start() {
            MODES_OF_AN_AGENT_FIXED_AT_START
        } else {
            CLIENT_IMPLEMENTED_PERMISSION_MODES
        }
    }

    /// `mode` if this agent offers it, else the mode a tab of it takes instead: only Manual can be
    /// missing, and Auto stands in for it (the start screen says so when that happens).
    pub fn offered_mode(&self, mode: SessionModeChoice) -> SessionModeChoice {
        if self.permission_modes().contains(&mode.as_str()) {
            mode
        } else {
            SessionModeChoice::Auto
        }
    }

    /// How a person reads it: `Claude`, `Codex`, or the ACP agent's own name.
    pub fn label(&self) -> String {
        match self {
            AgentId::Claude => "Claude".to_string(),
            AgentId::Codex => "Codex".to_string(),
            AgentId::Acp(name) => name.clone(),
        }
    }
}

impl serde::Serialize for AgentId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.record_name())
    }
}

impl<'de> serde::Deserialize<'de> for AgentId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        AgentId::from_record_name(&text).ok_or_else(|| serde::de::Error::custom(format!("not an agent: {text:?}")))
    }
}

/// What starts an external agent's child: the `codex` binary, already located, or a registered ACP
/// agent with its command.
#[derive(Debug, Clone)]
pub enum ExternalLaunch {
    Codex(PathBuf),
    Acp(agent::providers::acp::AcpAgentSpec),
}

/// The policy a turn of an external agent runs under, from the tab's mode: in Auto the agent's own
/// reviewer decides what still needs asking; in Manual and Bypass every request reaches Eitri, which
/// shows each one as a card in Manual and answers it in Bypass.
///
/// At a root that is no boundary the agent writes nothing unasked, and an Auto tab's requests come to
/// Eitri as well: a reviewer that approves an escalation would make the sandbox no gate at all, and
/// every uncertainty here ends on a card, as a Claude tab's does at such a root.
pub fn turn_policy(mode: SessionModeChoice, root_is_a_boundary: bool) -> TurnPolicy {
    TurnPolicy {
        reviewer: match mode {
            SessionModeChoice::Auto if root_is_a_boundary => Reviewer::Agent,
            SessionModeChoice::Auto | SessionModeChoice::Manual | SessionModeChoice::Bypass => Reviewer::User,
        },
        sandbox: if root_is_a_boundary {
            Sandbox::Project
        } else {
            Sandbox::ReadOnly
        },
    }
}

/// A conversation with an external agent, and which agent it is.
pub struct ExternalConversation {
    agent: AgentId,
    conversation: AgentConversation,
}

impl ExternalConversation {
    /// Refuses a conversation that is not an external provider's, or one whose provider names
    /// another agent than `agent`: the tab's agent, its records and its leases must agree.
    pub fn new(agent: AgentId, conversation: AgentConversation) -> Result<Self, BackendError> {
        if !agent.is_external() || conversation.provider_name() != agent.record_name() {
            return Err(BackendError::fatal(format!(
                "a {} session cannot be held as a {} tab",
                conversation.provider_name(),
                agent.record_name()
            )));
        }
        Ok(ExternalConversation { agent, conversation })
    }
}

/// The checked way into an external agent's backend: `AgentBackend::Conversation`'s fields are
/// public, so only a conversation that passed [`ExternalConversation::new`] is known to be its
/// agent's.
impl From<ExternalConversation> for AgentBackend {
    fn from(external: ExternalConversation) -> Self {
        // Every field named: what a moved tab's conversation is rebuilt from must account for each
        // part of the holder, or a part added later would be lost on the way into a backend.
        let ExternalConversation { agent, conversation } = external;
        AgentBackend::Conversation {
            agent,
            conversation: Box::new(conversation),
        }
    }
}

/// The backend's own part of a chat's move record, beside the conversation's: which agent the
/// conversation is with. The destination rebuilds the backend from it through the same checked way an
/// external agent's backend is always made ([`ExternalConversation::new`]), so a record naming another
/// agent than the adopted provider's is refused.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackendHandoff {
    pub agent: AgentId,
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
    /// **Correction (2026-09-27):** this used to fall back
    /// to `legacy` on an unrecognized value or on "unset, no sidecar artifact" -- described just
    /// below until this correction. Legacy is now the gated backend (`agent::LEGACY_BACKEND_COMPILED`,
    /// off in every release), so a typo or a missing artifact can no longer land there: both now
    /// select the sidecar, with a warning for the typo. Only an explicit `--legacy` or
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
    /// **The matrix:**
    /// - `--legacy` (`legacy_flag`) or `EITRI_AGENT_BACKEND=legacy` (`explicit`) asks for legacy.
    ///   When `legacy_compiled`, that ask is granted. When it is not (every release build), it is
    ///   `Err` naming [`agent::LEGACY_NOT_IN_BUILD`] and which of the two asked -- the flag wins the
    ///   naming when both are given, because it is a decision made on *this* invocation's command
    ///   line, where the env var could be an inherited leftover nobody meant to set today.
    /// - Everything else selects the sidecar, in both builds alike: an explicit `"sidecar"`, unset,
    ///   empty (a wrapper script's `VAR=`, treated as unset), and -- since this correction -- an
    ///   unrecognized value too (it used to fall back to legacy, which is exactly the direction
    ///   a typo must not go now that legacy is the gated backend) and unset with no artifact
    ///   installed (it used to fall back to legacy for want of one; now it says
    ///   [`agent::NO_SIDECAR_HINT`] and starts the sidecar path anyway, which fails with that same
    ///   hint when it actually tries to spawn one). `sidecar` ([`agent::SidecarAvailability`]) only
    ///   shapes which sentence this prints -- it is not part of the choice between backends. It was
    ///   a `bool` (`packaged_sidecar_available`) until that made the "not installed" sentence
    ///   appear above a named `EITRI_VERDANDI_CHECKOUT` the spawn was about to build; the third
    ///   value names that case.
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

        // Every remaining case selects the sidecar -- there is no fallback to legacy left
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
                eprintln!("[agent_backend] using the sidecar backend: {}", agent::NO_SIDECAR_HINT);
            }
        }
        Ok(BackendKind::Sidecar)
    }
}

/// What [`BackendKind::choose`] returns when `--legacy`/`EITRI_AGENT_BACKEND=legacy` asked for a
/// backend this build did not compile in. `message` is a finished, human-readable sentence for
/// `main()` to print before exiting -- the same contract `eitri_core::project_root::resolve_launch`'s
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
}

impl BackendError {
    fn fatal(message: String) -> Self {
        Self { message, benign: false }
    }

    /// The refusal of a turn sent while the agent already had one open: the provider's own, or this
    /// conversation's guard in front of it. The message is not an error to show; the tab holds it as a
    /// steer instead ([`BackendError::is_turn_busy`]).
    fn turn_busy() -> Self {
        ConversationError::TurnAlreadyActive.into()
    }

    /// Whether this is the refusal [`AgentBackend::send_turn`] made of a turn sent while one was open.
    pub fn is_turn_busy(&self) -> bool {
        self.benign && self.message == ConversationError::TurnAlreadyActive.to_string()
    }

    /// The same refusal for a turn carrying images. Kept apart from [`BackendError::is_turn_busy`] because a
    /// message put into a running turn carries text alone: the tab holds this one whole, for a turn of its own
    /// after the running one ends, so its images are never dropped on the way.
    fn turn_busy_with_images() -> Self {
        Self {
            message: TURN_BUSY_WITH_IMAGES.to_string(),
            benign: true,
        }
    }

    /// Whether this is [`AgentBackend::send_turn`]'s refusal of a turn with images sent while one was open.
    pub fn is_turn_busy_with_images(&self) -> bool {
        self.benign && self.message == TURN_BUSY_WITH_IMAGES
    }
}

/// [`BackendError::turn_busy_with_images`]'s words, shown only if the tab cannot hold the message.
const TURN_BUSY_WITH_IMAGES: &str = "a turn is already running: a message with images waits for it to end";

/// A slash command's turn carries no images: what the CLI does with images beside a command is not known.
pub const NO_IMAGES_WITH_A_COMMAND: &str = "a message starting with / takes no images: remove the images first";

/// An answer to one of Claude's questions on its way to the agent ([`AgentBackend::answer_question`]),
/// read back with [`AgentBackend::finish_question_send`] on the backend that sent it. Holding it may
/// keep the chat in this window (a refused answer has to put its question back here), so it is
/// dropped once its reply was read.
#[derive(Debug)]
pub struct QuestionSend {
    interaction_id: String,
    send: agent::InteractionSend,
}

impl QuestionSend {
    /// The question set the answer is for.
    pub fn interaction_id(&self) -> &str {
        &self.interaction_id
    }

    /// The reply as the page is told it, `None` while the answer is still out. Puts nothing back: for
    /// a send whose session is gone.
    pub(crate) fn reply(&self) -> Option<Result<(), BackendError>> {
        self.send
            .try_reply()
            .map(|reply| reply.map(|_| ()).map_err(answer_refusal))
    }
}

/// A decision on one of Claude's plans on its way to the agent ([`AgentBackend::approve_plan`],
/// [`AgentBackend::keep_planning`]), or an approval waiting for the calls the CLI is still deciding in plan
/// mode before it may go (`TabSet::approve_plan`). Read back with `TabSet::finish_plan_send`; holding it may
/// keep the chat in this window, as a question's answer does.
#[derive(Debug)]
pub struct PlanSend {
    interaction_id: String,
    /// The mode an approval names; `None` for "keep planning".
    approval: Option<agent::PlanExecutionMode>,
    pub(crate) state: PlanSendState,
}

#[derive(Debug)]
pub(crate) enum PlanSendState {
    /// An approval not sent yet: calls deferred in plan mode are still running. `since_ms` is when it was
    /// asked for; past `PLAN_TRANSITION_GUARD_MS` it is refused and the plan keeps waiting.
    Draining {
        plan_sha256: String,
        since_ms: u64,
        waiting: usize,
    },
    Sent(agent::InteractionSend),
}

impl PlanSend {
    /// The plan the decision is for.
    pub fn interaction_id(&self) -> &str {
        &self.interaction_id
    }

    /// The mode an approval names, `None` for "keep planning".
    pub fn approval(&self) -> Option<agent::PlanExecutionMode> {
        self.approval
    }

    /// An approval that waits for `waiting` calls before it is sent.
    pub(crate) fn draining(
        interaction_id: &str,
        mode: agent::PlanExecutionMode,
        plan_sha256: &str,
        since_ms: u64,
        waiting: usize,
    ) -> Self {
        PlanSend {
            interaction_id: interaction_id.to_string(),
            approval: Some(mode),
            state: PlanSendState::Draining {
                plan_sha256: plan_sha256.to_string(),
                since_ms,
                waiting,
            },
        }
    }

    /// The reply as it stands, putting nothing back: for a send whose session is gone.
    pub(crate) fn reply(&self) -> Option<Result<(), agent::ProviderError>> {
        match &self.state {
            PlanSendState::Draining { .. } => None,
            PlanSendState::Sent(send) => send.try_reply().map(|reply| reply.map(|_| ())),
        }
    }

    /// How many calls an approval still waits for before it is sent; 0 once it went.
    pub fn waiting_calls(&self) -> usize {
        match &self.state {
            PlanSendState::Draining { waiting, .. } => *waiting,
            PlanSendState::Sent(_) => 0,
        }
    }
}

/// A person's own move into or out of plan mode on its way to the agent ([`AgentBackend::set_planning`]).
/// Its reply is the agent's acknowledgement only: the CLI's report of its mode confirms the move.
#[derive(Debug)]
pub struct PlanningSwitch {
    enter: bool,
    send: agent::PlanningSend,
}

impl PlanningSwitch {
    /// Whether this is a move into plan mode.
    pub fn enter(&self) -> bool {
        self.enter
    }

    /// The reply as it stands, handed to no session: for a switch whose session is gone.
    pub(crate) fn reply(&self) -> Option<agent::PlanningReply> {
        self.send.try_reply()
    }
}

/// Why a plan decision did not go through, and whether it may still have reached the agent: a timeout or a
/// lost connection after the request left says nothing of what the CLI did.
#[derive(Debug)]
pub struct PlanSendFailure {
    pub error: BackendError,
    pub outcome_unknown: bool,
}

/// Whether `error`, the reply to a request that left, leaves its outcome unknown.
fn outcome_unknown(error: &agent::ProviderError) -> bool {
    matches!(
        error,
        agent::ProviderError::Timeout
            | agent::ProviderError::Transport(_)
            | agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::DeadlineExceeded,
                ..
            }
    )
}

/// An answer the agent refused or that did not reach it, for the page: benign, since the question just
/// waits for another answer and the session goes on (a lost connection is reported by the session's own
/// stream). The agent's refusal is shown in its own words.
fn answer_refusal(error: agent::ProviderError) -> BackendError {
    let message = match error {
        agent::ProviderError::Provider { message, .. } => message,
        other => format!("the answer may not have reached Claude ({other}); the question is waiting again"),
    };
    BackendError { message, benign: true }
}

/// A plan decision or a plan-mode switch the agent refused or that did not reach it, for the page: benign,
/// since the plan waits for another decision or the tab stays where it was. The agent's refusal is shown in
/// its own words.
fn plan_refusal(error: agent::ProviderError) -> BackendError {
    let message = match error {
        agent::ProviderError::Provider { message, .. } => message,
        agent::ProviderError::UnsupportedCapability(_) => "plan mode is not offered on this session".to_string(),
        other => format!("the decision may not have reached Claude ({other})"),
    };
    BackendError { message, benign: true }
}

impl From<ConversationError> for BackendError {
    fn from(error: ConversationError) -> Self {
        Self {
            benign: error.is_benign(),
            message: error.to_string(),
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
        }
    }
}

/// Why a message typed while a turn runs was not put into it ([`AgentBackend::steer`]); it is held
/// until the turn ends instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerRefusal {
    /// The agent cannot take messages while a turn runs at all.
    Unsupported,
    /// It could, but not this one now: the turn was ending, or the agent said no.
    Refused(String),
}

/// Identifies a submitted opening prompt when the agent defers reading it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubmittedPrompt {
    pub client_message_id: String,
    pub turn_id: Option<String>,
    pub after_revision: u64,
}

/// A turn for [`AgentBackend::send_turn_spawned`]: what goes to the model, what was typed, and who wrote it.
pub struct TurnRequest {
    pub sync: Option<crate::remote_sync::flush::MessageSync>,
    pub wire: String,
    pub typed: String,
    pub origin: agent::PromptOrigin,
}

/// What [`AgentBackend::send_turn_spawned`] knows of a turn before the provider has answered: the host's id for
/// its message and the revision it was sent after. With the turn id the answer brings, a [`SubmittedPrompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedTurn {
    pub client_message_id: String,
    pub after_revision: u64,
}

impl SpawnedTurn {
    /// The prompt as [`AgentBackend::send_turn`] would have reported it, once the provider named its turn.
    pub fn submitted(self, turn_id: String) -> SubmittedPrompt {
        SubmittedPrompt {
            client_message_id: self.client_message_id,
            turn_id: Some(turn_id),
            after_revision: self.after_revision,
        }
    }
}

/// The words of a provider's refusal of a turn's images (`TurnContentRefused`), as they stand.
fn content_refusal(error: &ConversationError) -> Option<String> {
    match error {
        ConversationError::Provider(agent::ProviderError::Provider {
            code: agent::ProviderErrorCode::TurnContentRefused,
            message,
        }) => Some(message.clone()),
        _ => None,
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
    Conversation {
        agent: AgentId,
        conversation: Box<AgentConversation>,
    },
}

impl AgentBackend {
    /// The files named by one successful changing call, read while holding only the projection's guard.
    pub fn edited_by(&self, tool_use_id: &str) -> Vec<PathBuf> {
        crate::editor_tools::follow::edited_by(&self.projection().tool_calls, tool_use_id)
    }

    /// Constructs the selected backend. **Blocking, and for the sidecar path potentially for
    /// minutes** (a cold Verdandi checkout runs `npm ci` and `npm run build`), so callers must run
    /// this off the GTK main thread -- see `agent_panel`'s connect worker.
    /// `resume` carries the Claude provider session id to continue, or `None` for a fresh session.
    ///
    /// A resume that cannot be honored fails as a resume. It is never downgraded into a fresh
    /// session anywhere on this path -- resume is checked end to end precisely to catch that
    /// silent substitution, and doing it in the host would reintroduce one layer above where the
    /// protocol was fixed.
    ///
    /// No permission mode: every session on both backends is gated, and the tab's mode is
    /// read at the one point that answers requests (`take_revised_ui_delivery`), never sent to
    /// the CLI.
    ///
    /// `project` decides whether the project's own Claude configuration (its `.claude/`,
    /// `.mcp.json`, `CLAUDE.md`) loads, on both backends. The caller states it for every session,
    /// a resume included; nothing here defaults it.
    pub fn start(
        kind: BackendKind,
        project_dir: &Path,
        resume: Option<&str>,
        project: ProjectTrust,
    ) -> Result<Self, BackendError> {
        Self::start_holding(kind, project_dir, resume, None, project, None)
    }

    /// [`start`](Self::start) for a resume whose session lease the caller already holds
    /// (`AgentConversation::resume_holding`): the lease goes to the conversation, and is released
    /// with it or with a failed start. Ignored when nothing is being resumed.
    ///
    /// `client_tools` is what this host answers itself, stated for every session of the sidecar; the
    /// legacy backend has no way to carry it.
    pub fn start_holding(
        kind: BackendKind,
        project_dir: &Path,
        resume: Option<&str>,
        lease: Option<agent::lease::SessionLease>,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
    ) -> Result<Self, BackendError> {
        Self::start_showing_history(kind, project_dir, resume, lease, project, client_tools, None)
    }

    /// [`start_holding`](Self::start_holding) that hands a sidecar resume's loaded history to `early`
    /// (`agent::EarlyHistory`) before the provider is asked to resume. Nothing is handed over by a
    /// fresh session, by the legacy backend, or by a resume refused before its history was loaded.
    pub fn start_showing_history(
        kind: BackendKind,
        project_dir: &Path,
        resume: Option<&str>,
        lease: Option<agent::lease::SessionLease>,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
        early: Option<agent::EarlyHistory>,
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
                AgentSession::start(project_dir, agent::disallowed_tools(), project)
                    .map(AgentBackend::Legacy)
                    .map_err(|e| BackendError::fatal(format!("failed to start the legacy Claude backend: {e}")))
            }
            BackendKind::Sidecar => {
                let instance_id = uuid::Uuid::new_v4().to_string();
                // `e` already carries the sidecar's own stderr tail when it failed to start
                // (agent::providers::claude_sidecar::spawn), which is the only place the real
                // cause -- an incompatible Claude CLI, say -- exists.
                let connected = |connect: std::io::Result<ClaudeSidecarProvider>| {
                    connect.map_err(|e| BackendError::fatal(format!("failed to connect to the Verdandi sidecar: {e}")))
                };
                let Some(provider_session_id) = resume else {
                    let provider = connected(ClaudeSidecarProvider::connect(&instance_id))?;
                    return Self::open_claude_session_showing(
                        std::sync::Arc::new(provider),
                        project_dir,
                        None,
                        lease,
                        project,
                        client_tools,
                        early,
                    );
                };
                // A resume's provider-free half (the transcript probe, the lease, the history, handed to `early`)
                // runs while the sidecar starts: neither waits for the other, and the provider is asked to resume
                // only once both are done.
                let connecting = std::thread::spawn(move || ClaudeSidecarProvider::connect(&instance_id));
                let prepared = AgentConversation::prepare_resume(project_dir, provider_session_id, lease, early);
                let provider = match connecting.join() {
                    Ok(connect) => connected(connect)?,
                    Err(panic) => std::panic::resume_unwind(panic),
                };
                let provider: std::sync::Arc<dyn agent::AgentProvider + Send + Sync> = std::sync::Arc::new(provider);
                let info = provider.info();
                prepared
                    .and_then(|prepared| AgentConversation::resume_prepared(provider, prepared, project, client_tools))
                    .map(|conversation| AgentBackend::Conversation {
                        agent: AgentId::Claude,
                        conversation: Box::new(conversation),
                    })
                    .map_err(|e| {
                        BackendError::fatal(with_provider_notes(
                            format!("could not continue the previous session: {e}"),
                            &info,
                        ))
                    })
            }
        }
    }

    /// Opens a session on a connected Claude sidecar. A sidecar can connect and then fail here, on a
    /// selected update as anywhere else, so the failure carries the same remedies a tab that was running
    /// gets: the conversation that would have held the provider's facts never came to exist.
    pub fn open_claude_session(
        provider: std::sync::Arc<dyn agent::AgentProvider + Send + Sync>,
        project_dir: &Path,
        resume: Option<&str>,
        lease: Option<agent::lease::SessionLease>,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
    ) -> Result<Self, BackendError> {
        Self::open_claude_session_showing(provider, project_dir, resume, lease, project, client_tools, None)
    }

    /// [`open_claude_session`](Self::open_claude_session) with a resume's early history sink.
    fn open_claude_session_showing(
        provider: std::sync::Arc<dyn agent::AgentProvider + Send + Sync>,
        project_dir: &Path,
        resume: Option<&str>,
        lease: Option<agent::lease::SessionLease>,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
        early: Option<agent::EarlyHistory>,
    ) -> Result<Self, BackendError> {
        let info = provider.info();
        let backend = |conversation| AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        match resume {
            Some(provider_session_id) => AgentConversation::resume_showing_history(
                provider,
                project_dir,
                provider_session_id,
                lease,
                project,
                client_tools,
                early,
            )
            .map(backend)
            .map_err(|e| {
                BackendError::fatal(with_provider_notes(
                    format!("could not continue the previous session: {e}"),
                    &info,
                ))
            }),
            None => AgentConversation::create_with_tools(provider, project_dir, project, client_tools)
                .map(backend)
                .map_err(|e| {
                    BackendError::fatal(with_provider_notes(
                        format!("failed to create a Claude session: {e}"),
                        &info,
                    ))
                }),
        }
    }

    /// Starts a remote Claude tab (`agent::remote_client::open_remote_session`): the root is checked
    /// here first, the sidecar runs on the server, started and held by the helper behind `client`, and
    /// its session runs in `start.root` exactly as the remote project records it; a sidecar whose
    /// session could not be created is stopped again, and when that stop does not go through (the link broke
    /// during the create) it is given to `keep_stop`, to be sent once the server can be reached. The tab's link
    /// state is the provider's from here on. **Blocking**: ssh and the helper's start, so callers run it off the
    /// GTK main loop.
    pub fn start_remote(
        client: agent::remote_client::HelperClient,
        host_key_sha256: &str,
        start: &agent::remote_client::SidecarStart,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
        keep_stop: impl FnOnce(&agent::HelperHandoff),
    ) -> Result<Self, BackendError> {
        agent::remote_client::open_remote_session(client, host_key_sha256, start, project, client_tools)
            .map(|conversation| AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            })
            .map_err(|failure| {
                if let Some(sidecar) = &failure.unstopped {
                    keep_stop(sidecar);
                }
                BackendError::fatal(match &failure.info {
                    Some(info) => with_provider_notes(failure.message, info),
                    None => failure.message,
                })
            })
    }

    /// Takes a remote Claude tab up again from its saved state (`handoff`, over `client`, opened for its
    /// host key): its history is folded afresh from the journal's start and nothing is resumed. When the
    /// server no longer runs the sidecar the journal is still shown and the tab then ends with the agent
    /// gone. **Blocking**: ssh and the helper.
    pub fn reattach_remote(
        client: agent::remote_client::HelperClient,
        handoff: &agent::HelperHandoff,
        root: &Path,
    ) -> Result<Self, BackendError> {
        let host = handoff.host.clone();
        let provider = ClaudeSidecarProvider::reattach_through_helper(client, handoff)
            .map_err(|e| BackendError::fatal(format!("the agent on {host} could not be reached: {e}")))?;
        AgentConversation::reattach_remote(std::sync::Arc::new(provider), root, &handoff.session_id)
            .map(|conversation| Self::reattached(conversation, handoff.last_folded_sequence))
            .map_err(|e| BackendError::fatal(format!("the agent on {host} could not be taken up: {e}")))
    }

    /// The backend of a remote tab taken up again (`conversation`, from `AgentConversation::reattach_remote`), whose
    /// saving window had folded its session through `attached_through`: what is folded up to there is old history
    /// read back ([`Self::replaying_history`]).
    pub fn reattached(mut conversation: AgentConversation, attached_through: u64) -> Self {
        conversation.replays_history_through(attached_through);
        AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        }
    }

    /// Whether this backend is still folding history read back from its server (`AgentConversation::
    /// replaying_history`): only a remote tab taken up again does, until it has caught up.
    pub fn replaying_history(&self) -> bool {
        match self {
            AgentBackend::Legacy(_) => false,
            AgentBackend::Conversation { conversation, .. } => conversation.replaying_history(),
        }
    }

    /// Continues the CLI's own session `provider_session_id` on the server through a new sidecar started
    /// for `start`, with the resume request a local tab sends: a remote tab whose agent the server no longer
    /// runs. The new sidecar is stopped again when the resume fails, or given to `keep_stop` when that stop does
    /// not go through. **Blocking.**
    pub fn resume_remote(
        client: agent::remote_client::HelperClient,
        host_key_sha256: &str,
        start: &agent::remote_client::SidecarStart,
        provider_session_id: &str,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
        keep_stop: impl FnOnce(&agent::HelperHandoff),
    ) -> Result<Self, BackendError> {
        let provider = match ClaudeSidecarProvider::connect_through_helper(client, host_key_sha256, start) {
            Ok(provider) => std::sync::Arc::new(provider),
            Err(error) => {
                if let Some(sidecar) = ClaudeSidecarProvider::unstopped_sidecar(&error) {
                    keep_stop(&sidecar);
                }
                return Err(BackendError::fatal(error.to_string()));
            }
        };
        let info = agent::AgentProvider::info(provider.as_ref());
        AgentConversation::resume_remote_with_tools(
            provider.clone(),
            &start.root,
            provider_session_id,
            project,
            client_tools,
        )
        .map(|conversation| AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        })
        .map_err(|e| {
            if let Err(stop) = provider.stop_remote_sidecar() {
                eprintln!("[agent_backend] the sidecar of a resume that failed was not stopped: {stop}");
                if let Some(sidecar) = provider.remote_sidecar() {
                    keep_stop(&sidecar);
                }
            }
            BackendError::fatal(with_provider_notes(
                format!("could not continue the previous session: {e}"),
                &info,
            ))
        })
    }

    /// How this remote tab's session ended, once the helper said; `None` while it runs and for a local tab.
    pub fn remote_ending(&self) -> Option<agent::RemoteEnding> {
        match self {
            AgentBackend::Conversation { conversation, .. } => conversation.remote_ending(),
            AgentBackend::Legacy(_) => None,
        }
    }

    /// What reaches this remote tab's agent again from a later window; `None` for a local tab.
    pub fn helper_handoff(&self) -> Option<agent::HelperHandoff> {
        match self {
            AgentBackend::Conversation { conversation, .. } => conversation.helper_handoff(),
            AgentBackend::Legacy(_) => None,
        }
    }

    /// After a gap: denies each listed waiting request through the provider's ordinary answer and stops
    /// the agent, on a thread of its own (`AgentConversation::deny_waiting_and_stop`).
    pub fn deny_waiting_and_stop(
        &self,
        permission_ids: Vec<String>,
    ) -> std::sync::mpsc::Receiver<Result<agent::DenyAndStopOutcome, String>> {
        match self {
            AgentBackend::Conversation { conversation, .. } => conversation.deny_waiting_and_stop(permission_ids),
            AgentBackend::Legacy(_) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = tx.send(Err("this tab's agent does not run on a server".to_string()));
                rx
            }
        }
    }

    /// Why the last [`Self::shutdown`] did not stop this remote tab's agent on its server
    /// (`AgentConversation::remote_stop_failure`); `None` when it did, or for a local tab.
    pub fn remote_stop_failure(&self) -> Option<String> {
        match self {
            AgentBackend::Conversation { conversation, .. } => conversation.remote_stop_failure().map(str::to_string),
            AgentBackend::Legacy(_) => None,
        }
    }

    /// Lets go of a remote tab without ending its agent on the server (a window closing, "leave it
    /// running"); a local tab is shut down. Blocking like [`Self::shutdown`]: a worker's.
    pub fn detach(&mut self) {
        match self {
            AgentBackend::Legacy(session) => session.shutdown(),
            AgentBackend::Conversation { conversation, .. } => conversation.detach(),
        }
    }

    /// Starts, or resumes, a session of an external agent. **Blocking** like [`start`](Self::start):
    /// it runs the agent's own child, and for Codex the per-binary check first, so callers run it off
    /// the GTK main loop.
    ///
    /// `plan` is the trust plan the user's answer was bound to and `mode` the tab's, which picks the
    /// reviewer of the session's first turn. `resume` names the agent's own session to continue; its
    /// lease is taken here and held by the conversation. Every start of an external tab goes through
    /// the same trust gate as a Claude start, so nothing here decides what the project may load.
    ///
    /// `launch` is what starts the agent's child: the located `codex`, or a registered ACP agent's
    /// command. One that does not belong to `agent` is refused. `editor_server` is the tool server the
    /// session is to run, when it was given tools.
    pub fn start_external(
        agent: AgentId,
        launch: ExternalLaunch,
        project_dir: &Path,
        plan: TrustPlan,
        mode: SessionModeChoice,
        resume: Option<&str>,
        editor_server: Option<agent::EditorServerSpec>,
    ) -> Result<Self, BackendError> {
        let canonical = project_dir
            .canonicalize()
            .map_err(|e| BackendError::fatal(format!("the project directory cannot be resolved: {e}")))?;
        let provider: std::sync::Arc<dyn agent::ExternalProvider + Send + Sync> = match (&agent, launch) {
            (AgentId::Codex, ExternalLaunch::Codex(program)) => {
                let state_home = crate::state_paths::state_subdir(
                    std::env::var_os("XDG_STATE_HOME").as_deref(),
                    std::env::var_os("HOME").as_deref(),
                    "",
                )
                .ok_or_else(|| {
                    BackendError::fatal("Eitri has no state directory to keep Codex's check in".to_string())
                })?;
                std::sync::Arc::new(
                    agent::providers::codex::CodexProvider::connect(agent::providers::codex::CodexLaunch {
                        program,
                        cwd: canonical.clone(),
                        state_home,
                    })
                    .map_err(|e| BackendError::fatal(format!("Codex could not start: {e}")))?,
                )
            }
            (AgentId::Acp(name), ExternalLaunch::Acp(spec)) if spec.name == *name => std::sync::Arc::new(
                agent::providers::acp::AcpProvider::connect(
                    spec.at_root(root_is_a_boundary(project_dir)),
                    canonical.clone(),
                )
                .map_err(|e| BackendError::fatal(format!("{name} could not start: {e}")))?,
            ),
            _ => {
                return Err(BackendError::fatal(format!(
                    "this build cannot start a {} tab",
                    agent.label()
                )))
            }
        };
        Self::start_external_serving(agent, provider, project_dir, plan, mode, resume, editor_server)
    }

    /// [`start_external`](Self::start_external) over a provider the caller already has, with no tool
    /// server: the way a test drives an external tab without a child.
    pub fn start_external_with(
        agent: AgentId,
        provider: std::sync::Arc<dyn agent::ExternalProvider + Send + Sync>,
        project_dir: &Path,
        plan: TrustPlan,
        mode: SessionModeChoice,
        resume: Option<&str>,
    ) -> Result<Self, BackendError> {
        Self::start_external_serving(agent, provider, project_dir, plan, mode, resume, None)
    }

    /// What [`start_external`](Self::start_external) does once its provider is connected.
    /// `project_dir` is the window's root as the window holds it, not what it resolves to: the judgement
    /// of the root reads the difference.
    pub fn start_external_serving(
        agent: AgentId,
        provider: std::sync::Arc<dyn agent::ExternalProvider + Send + Sync>,
        project_dir: &Path,
        plan: TrustPlan,
        mode: SessionModeChoice,
        resume: Option<&str>,
        editor_server: Option<agent::EditorServerSpec>,
    ) -> Result<Self, BackendError> {
        let canonical = project_dir
            .canonicalize()
            .map_err(|e| BackendError::fatal(format!("the project directory cannot be resolved: {e}")))?;
        let start = agent::ExternalStart {
            cwd: canonical.to_string_lossy().into_owned(),
            plan,
            policy: turn_policy(mode, root_is_a_boundary(project_dir)),
            resume: resume.map(str::to_string),
            editor_server,
        };
        let conversation = match resume {
            Some(id) => {
                let cwd = canonical.to_string_lossy().into_owned();
                let lease = agent::lease::SessionLease::try_acquire(&agent.record_name(), &cwd, id).map_err(|e| {
                    BackendError::fatal(format!(
                        "could not continue the previous {} session: {e}",
                        agent.label()
                    ))
                })?;
                AgentConversation::resume_external(provider, start, &canonical, lease).map_err(|e| {
                    BackendError::fatal(format!(
                        "could not continue the previous {} session: {e}",
                        agent.label()
                    ))
                })?
            }
            None => AgentConversation::create_external(provider, start, &canonical)
                .map_err(|e| BackendError::fatal(format!("failed to create a {} session: {e}", agent.label())))?,
        };
        ExternalConversation::new(agent, conversation).map(AgentBackend::from)
    }

    /// Which Claude backend this is; `None` for an external agent, which runs on neither.
    pub fn kind(&self) -> Option<BackendKind> {
        match self {
            AgentBackend::Legacy(_) => Some(BackendKind::Legacy),
            AgentBackend::Conversation { agent, .. } => (!agent.is_external()).then_some(BackendKind::Sidecar),
        }
    }

    /// The agent this backend runs.
    pub fn agent(&self) -> AgentId {
        match self {
            AgentBackend::Legacy(_) => AgentId::Claude,
            AgentBackend::Conversation { agent, .. } => agent.clone(),
        }
    }

    /// Has `wake` called whenever this backend folded something for the UI to take, unless one is
    /// installed already. The legacy backend cannot: its reader thread tells nobody, so a window holding
    /// one keeps polling ([`announces_to_ui`](Self::announces_to_ui)).
    pub fn ensure_ui_wake(&self, wake: &agent::UiWake) {
        let conversation = match self {
            AgentBackend::Legacy(_) => return,
            AgentBackend::Conversation { conversation, .. } => conversation,
        };
        if !conversation.has_ui_wake() {
            conversation.set_ui_wake(wake.clone());
        }
    }

    /// Whether the UI is told when this backend has something to take; if not, the UI polls it.
    pub fn announces_to_ui(&self) -> bool {
        match self {
            AgentBackend::Legacy(_) => false,
            AgentBackend::Conversation { conversation, .. } => conversation.has_ui_wake(),
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
    /// Returns a borrow rather than a clone: the panel reads this on every tick (up to 30 a second), and the
    /// sidecar path's projection can hold a whole conversation. The sidecar's arrives through the
    /// ingestion thread's lock, the legacy backend's is a plain field -- `ProjectionRef` is what lets
    /// one call site cover both without either paying for the other's shape.
    pub fn projection(&self) -> ProjectionRef<'_> {
        match self {
            AgentBackend::Legacy(session) => ProjectionRef::Borrowed(&session.projection),
            AgentBackend::Conversation { conversation, .. } => ProjectionRef::Guarded(conversation.projection()),
        }
    }

    /// Eitri's own conversation identity. `None` for the legacy backend, which has no concept of
    /// one -- it never distinguished its three identities in the first place.
    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => Some(conversation.conversation_id()),
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
            AgentBackend::Conversation { conversation, .. } => conversation.session_id(),
        }
    }

    /// The Claude session UUID, once the provider has reported it. Distinct from the projection's
    /// `session_id` (Verdandi's) for the sidecar backend; identical for the legacy one, whose CLI
    /// never separated them. It takes no lock the projection holds, so it may be read while a
    /// [`ProjectionRef`] from this backend lives.
    pub fn provider_session_id(&self) -> Option<String> {
        match self {
            AgentBackend::Legacy(session) => session.projection.provider_session_id.clone(),
            AgentBackend::Conversation { conversation, .. } => conversation.provider_session_id(),
        }
    }

    pub fn link_state(&self) -> Option<agent::RemoteLinkState> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => conversation.link_state(),
        }
    }

    pub fn provider_info(&self) -> Option<&ProviderInfo> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => Some(conversation.provider_info()),
        }
    }

    /// Keeps the failure's own text first, followed by the remedies of the sidecar that ran it.
    pub fn failure_with_provider_notes(&self, reason: String) -> String {
        match self.provider_info() {
            Some(info) => with_provider_notes(reason, info),
            None => reason,
        }
    }

    /// The provider's way to move with this backend's chat to the other window, when it has one; the legacy
    /// backend has none.
    #[cfg(target_os = "linux")]
    pub fn mover(&self) -> Option<&dyn agent::chat_mover::ChatMover> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => conversation.mover(),
        }
    }

    /// A second descriptor on the session lease, for the window the chat moves to; `None` when the session holds
    /// no lease.
    pub fn lease_descriptor(&self) -> Option<std::io::Result<std::os::fd::OwnedFd>> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => conversation.lease_descriptor(),
        }
    }

    /// A moved chat's commit in the window that took it up: the lease names this process from here on.
    pub fn commit_moved(&mut self) -> std::io::Result<()> {
        match self {
            AgentBackend::Legacy(_) => Ok(()),
            AgentBackend::Conversation { conversation, .. } => conversation.commit_moved(),
        }
    }

    /// Effective capabilities: what the server advertises, intersected with what this client can
    /// actually drive end to end (`AgentConversation`/`ClaudeSidecarProvider` apply the
    /// intersection; this just reports it).
    ///
    /// The legacy backend advertises nothing over a wire, so its capabilities are stated from what
    /// its code demonstrably does: it interrupts, it has no resume and no fork, and it runs every
    /// session gated.
    pub fn capabilities(&self) -> ProviderCapabilities {
        match self {
            AgentBackend::Legacy(_) => LEGACY_CAPABILITIES,
            AgentBackend::Conversation { conversation, .. } => conversation.capabilities(),
        }
    }

    /// Submits a turn. Locally produced events and provider events both reach the page through
    /// the pump. Legacy synthesizes its turn start because the CLI wire has no such line;
    /// the sidecar's turn start comes from the provider and remains authoritative.
    ///
    /// Two strings because they are two different things. `wire_text` is what goes to the model --
    /// the user's text with wire 1's editor context composed in (`panel/src/inbound.rs`, the
    /// one composition point). `as_typed` is what the user wrote, which is what a reader of the
    /// conversation is owed.
    ///
    /// **The prompt is folded BEFORE the backend's send is even attempted, on both arms -- not
    /// after acceptance.** `AgentConversation::send_turn_as` returns as soon as the server ACKs the unary RPC;
    /// `active_turn_id` is set later, when the provider's own event arrives on the ingestion
    /// thread (see that method's own doc). Folding the prompt AFTER that ack raced this thread's
    /// `fold_locally` against the ingestion thread's `IngestState::fold` for the SAME mutex, with
    /// no ordering guarantee -- if the server's `TurnStarted`/first `ContentDelta` won that race,
    /// the reply got a lower `seq` than the prompt that caused it, inverting the panel's display
    /// order for that turn. Folding first closes the race by construction rather than by timing:
    /// before the RPC is even sent, the ingestion thread has nothing of this turn's to fold yet.
    ///
    /// A busy refusal withdraws this local row so the tab can hold the prompt for the running turn.
    /// Other benign failures retain it with the caller's error reporting. The returned client id
    /// correlates later delivery or drop events when the CLI runs its own work ahead of this prompt;
    /// `origin` marks it as the person's input only when it was admitted from the composer.
    ///
    /// A prompt whose send is refused benignly reaches the UI through the next pump, since the
    /// session lives on. A fatal failure retires the session before another pump, so its queued
    /// prompt is never drawn: the page says the message was not sent and puts it back in the box,
    /// and a failed tab's state carries no conversation, so a reload shows no row for it either.
    ///
    /// `images` go with the text, after it and in order, only to a Claude sidecar that takes them
    /// (`ProviderCapabilities::turn_content_v1`). Any other backend refuses a turn with images before
    /// anything is folded or sent, and so does the provider when they break the limits its sidecar stated
    /// (or the sidecar itself): that refusal withdraws the row folded for the prompt, since no part of the
    /// turn was sent. The prompt's row records what each image was ([`agent::AttachmentMeta`]), never its
    /// bytes.
    pub fn send_turn(
        &mut self,
        wire_text: &str,
        as_typed: &str,
        origin: agent::PromptOrigin,
        images: Vec<agent::TurnImage>,
    ) -> Result<SubmittedPrompt, BackendError> {
        if self.agent().is_external() {
            return Err(BackendError {
                message: "this tab's turns carry its permission mode; send it through its tab".to_string(),
                benign: true,
            });
        }
        if let Some(why) = self.why_no_images().filter(|_| !images.is_empty()) {
            return Err(BackendError {
                message: why,
                benign: true,
            });
        }
        if !images.is_empty() && as_typed.trim_start().starts_with('/') {
            return Err(BackendError {
                message: NO_IMAGES_WITH_A_COMMAND.to_string(),
                benign: true,
            });
        }
        let carries_images = !images.is_empty();
        let client_message_id = uuid::Uuid::new_v4().to_string();
        let after_revision = self.projection().last_revision;
        match self {
            AgentBackend::Legacy(session) => {
                if origin != agent::PromptOrigin::ShellLine {
                    session.fold_locally(user_prompt_event(as_typed));
                }
                session.send_turn(wire_text)?;
                Ok(SubmittedPrompt {
                    client_message_id,
                    turn_id: session.projection.active_turn_id.clone(),
                    after_revision,
                })
            }
            AgentBackend::Conversation { conversation, .. } => {
                // Reaches the UI through the pump like every other event, because `fold_locally`
                // queues it -- and it does so regardless of whether `send_turn` below succeeds,
                // which is exactly what makes a rejected turn still show its prompt.
                let row = origin != agent::PromptOrigin::ShellLine;
                if row {
                    conversation.fold_locally(user_prompt_event(as_typed));
                    if !images.is_empty() {
                        conversation.fold_locally(AgentDomainEvent::UserPromptAttachments {
                            attachments: images.iter().map(|image| image.meta.clone()).collect(),
                        });
                    }
                }
                let turn_id = match conversation.send_turn_as(wire_text, as_typed, origin, &client_message_id, images) {
                    Ok(turn_id) => turn_id,
                    Err(error) => {
                        // A turn was open that this side had not folded yet: the prompt it recorded was not
                        // put into any turn, and the tab holds the message to steer into the open one.
                        if matches!(error, ConversationError::TurnAlreadyActive) {
                            if row {
                                conversation.fold_locally(AgentDomainEvent::UserPromptWithdrawn);
                            }
                            if carries_images {
                                return Err(BackendError::turn_busy_with_images());
                            }
                            return Err(BackendError::turn_busy());
                        }
                        // The images were refused before any of the turn left: no row for a prompt never sent.
                        if let Some(message) = content_refusal(&error) {
                            if row {
                                conversation.fold_locally(AgentDomainEvent::UserPromptWithdrawn);
                            }
                            return Err(BackendError { message, benign: true });
                        }
                        return Err(error.into());
                    }
                };
                // The resume picker's title for this session: the prompt as typed, never
                // `wire_text`, which carries the editor context composed above it. Only a turn the
                // provider accepted names a session. Legacy writes no records, so it has nothing to
                // name.
                conversation.note_title(as_typed);
                Ok(SubmittedPrompt {
                    client_message_id,
                    turn_id: Some(turn_id),
                    after_revision,
                })
            }
        }
    }

    /// The row of a person's `!` line, before it runs: the turn that reports it comes after what it printed.
    pub fn fold_shell_line(&mut self, text: &str) {
        self.fold_row(user_prompt_event(text));
    }

    /// What a person's `!` line printed, under its row.
    pub fn fold_shell_output(&mut self, text: String) {
        self.fold_row(AgentDomainEvent::SessionNotice {
            kind: agent::SessionNoticeKind::ShellOutput,
            level: agent::NoticeLevel::Notice,
            content: Some(text),
            scalars: Default::default(),
            task_id: None,
            tool_use_id: None,
            task_status: None,
        });
    }

    /// A host notice that stays as a plain-text row in the conversation and its history.
    pub fn fold_notice(&mut self, text: String) {
        self.fold_row(AgentDomainEvent::SessionNotice {
            kind: agent::SessionNoticeKind::Informational,
            level: agent::NoticeLevel::Notice,
            content: Some(text),
            scalars: Default::default(),
            task_id: None,
            tool_use_id: None,
            task_status: None,
        });
    }

    fn fold_row(&mut self, event: AgentDomainEvent) {
        match self {
            AgentBackend::Legacy(session) => {
                session.fold_locally(event);
            }
            AgentBackend::Conversation { conversation, .. } => conversation.fold_locally(event),
        }
    }

    /// [`send_turn`](Self::send_turn) in a tab whose mode is `mode`. A Claude turn ignores the mode:
    /// its tab's mode is applied where requests are answered (`take_revised_ui_delivery`), never
    /// sent to the CLI. An external agent's turn carries it as the turn's reviewer, so a mode change
    /// reaches the agent at the next turn, while Eitri's own answering follows the mode at once.
    ///
    /// `project_root` is the window's root, which the gate judges; `None` before the window's first
    /// pump. The conversation's own is what that path resolved to when it started, so it cannot show a
    /// folder that had already been replaced by a link.
    pub fn send_turn_in(
        &mut self,
        wire_text: &str,
        as_typed: &str,
        mode: SessionModeChoice,
        project_root: Option<&Path>,
        origin: agent::PromptOrigin,
        images: Vec<agent::TurnImage>,
    ) -> Result<SubmittedPrompt, BackendError> {
        if !self.agent().is_external() {
            return self.send_turn(wire_text, as_typed, origin, images);
        }
        if let Some(why) = self.why_no_images().filter(|_| !images.is_empty()) {
            return Err(BackendError {
                message: why,
                benign: true,
            });
        }
        let AgentBackend::Conversation { conversation, .. } = self else {
            unreachable!();
        };
        // Folded before the send, as on the sidecar, so the prompt keeps its place before the
        // agent's first event of the turn.
        let after_revision = conversation.projection().last_revision;
        conversation.fold_locally(user_prompt_event(as_typed));
        let root = project_root.unwrap_or(conversation.canonical_cwd());
        let policy = turn_policy(mode, root_is_a_boundary(root));
        let turn_id = conversation.send_turn_with(wire_text, policy)?;
        conversation.note_title(as_typed);
        Ok(SubmittedPrompt {
            client_message_id: uuid::Uuid::new_v4().to_string(),
            turn_id: Some(turn_id),
            after_revision,
        })
    }

    /// Why this tab's turns cannot carry images, in words for the composer, or `None` when they can: only a
    /// Claude sidecar that lists `turn_content_v1` takes them, and an external agent's turns are text alone.
    pub fn why_no_images(&self) -> Option<String> {
        let agent = self.agent();
        match self {
            AgentBackend::Legacy(_) => Some("this tab cannot take images: its backend sends text only".to_string()),
            AgentBackend::Conversation { conversation, .. } => {
                if agent.is_external() {
                    Some(format!(
                        "this tab cannot take images: {} takes text only here",
                        agent.label()
                    ))
                } else if !conversation.capabilities().turn_content_v1
                    || conversation.capabilities().turn_content_limits.is_none()
                {
                    // A sidecar that lists the capability without its limits has the provider refuse the turn
                    // as an unsupported capability, which retires the session: it is refused here instead.
                    Some("this tab cannot take images: its Claude sidecar does not accept them".to_string())
                } else {
                    None
                }
            }
        }
    }

    /// Whether a message typed while a turn runs can go into that turn (the provider's capability
    /// `steer`). The legacy backend's CLI reads its input only between turns.
    pub fn steers(&self) -> bool {
        match self {
            AgentBackend::Legacy(_) => false,
            AgentBackend::Conversation { conversation, .. } => conversation.steers(),
        }
    }

    /// Puts one message into the running turn without interrupting it, under the host's id for it.
    /// Nothing is folded here: the transcript row comes when the provider reports the agent read it
    /// (`UserMessageDelivered`), at that point of the conversation. A refusal means nothing was sent;
    /// [`SteerRefusal`] says why in words the panel shows after "held until the turn ends:".
    ///
    /// `shown_turn` is the turn the page showed running when Enter was pressed; another turn running now
    /// refuses it.
    pub fn steer(
        &mut self,
        shown_turn: &str,
        client_message_id: &str,
        wire_text: &str,
        as_typed: &str,
        origin: agent::PromptOrigin,
        sync: Option<crate::remote_sync::flush::MessageSync>,
    ) -> Result<(), SteerRefusal> {
        let result = match self {
            AgentBackend::Legacy(_) => return Err(SteerRefusal::Unsupported),
            AgentBackend::Conversation { conversation, .. } => {
                let wait = sync.map(|sync| {
                    sync.wait(
                        client_message_id,
                        None,
                        conversation.projection().last_revision,
                        as_typed,
                    )
                });
                conversation.steer_turn_after(shown_turn, client_message_id, wire_text, as_typed, origin, wait)
            }
        };
        result.map_err(|e| match e {
            ConversationError::Provider(agent::ProviderError::UnsupportedCapability(_)) => SteerRefusal::Unsupported,
            ConversationError::Provider(agent::ProviderError::Provider { message, .. }) => {
                SteerRefusal::Refused(message)
            }
            other => SteerRefusal::Refused(other.to_string()),
        })
    }

    /// A session tab's rename. Legacy writes no records, so its name lives only in the
    /// tab (`crate::tab_set`).
    pub fn note_name(&mut self, name: Option<String>) {
        match self {
            AgentBackend::Legacy(_) => {}
            AgentBackend::Conversation { conversation, .. } => conversation.note_name(name),
        }
    }

    pub fn interrupt(&mut self) -> Result<(), BackendError> {
        match self {
            AgentBackend::Legacy(session) => {
                session.interrupt()?;
                Ok(())
            }
            AgentBackend::Conversation { conversation, .. } => {
                conversation.interrupt()?;
                Ok(())
            }
        }
    }

    /// [`interrupt`](Self::interrupt) for a caller that must not wait on the provider: the call runs on a thread
    /// of its own and `done` gets its outcome there (`AgentConversation::interrupt_spawned`). The legacy backend
    /// stops its own child at once, and `done` is told before this returns.
    pub fn interrupt_spawned(&mut self, done: impl FnOnce(Result<(), BackendError>) + Send + 'static) {
        match self {
            AgentBackend::Legacy(session) => done(session.interrupt().map(|_| ()).map_err(BackendError::from)),
            AgentBackend::Conversation { conversation, .. } => {
                conversation.interrupt_spawned(move |outcome| done(outcome.map_err(BackendError::from)))
            }
        }
    }

    /// [`send_turn`](Self::send_turn) for a caller that must not wait on the provider. The prompt's row is folded
    /// here, as `send_turn` folds it, and the provider's call runs on a thread of its own: `done` gets the turn id
    /// there, or why not. A turn the agent had open that this side had not folded comes back as
    /// [`BackendError::is_turn_busy`], and the caller withdraws the row ([`Self::withdraw_prompt`]). A refusal known
    /// here (an external agent's tab, a turn already folded as running, no session) is returned with nothing
    /// folded, and `done` is never called. The legacy backend sends at once, and `done` is told before this returns.
    pub fn send_turn_spawned(
        &mut self,
        turn: TurnRequest,
        done: impl FnOnce(Result<String, BackendError>) + Send + 'static,
    ) -> Result<SpawnedTurn, BackendError> {
        if self.agent().is_external() {
            return Err(BackendError {
                message: "this tab's turns carry its permission mode; send it through its tab".to_string(),
                benign: true,
            });
        }
        let client_message_id = uuid::Uuid::new_v4().to_string();
        let after_revision = self.projection().last_revision;
        let spawned = SpawnedTurn {
            client_message_id: client_message_id.clone(),
            after_revision,
        };
        if matches!(self, AgentBackend::Legacy(_)) {
            let outcome = self.send_turn(&turn.wire, &turn.typed, turn.origin, Vec::new());
            done(outcome.map(|submitted| submitted.turn_id.unwrap_or_default()));
            return Ok(spawned);
        }
        let AgentBackend::Conversation { conversation, .. } = self else {
            unreachable!("the legacy backend returned above");
        };
        if conversation.projection().active_turn_id.is_some() {
            return Err(BackendError::turn_busy());
        }
        let row = turn.origin != agent::PromptOrigin::ShellLine;
        if row {
            conversation.fold_locally(user_prompt_event(&turn.typed));
        }
        let turn_error = |error: ConversationError| match error {
            ConversationError::TurnAlreadyActive => BackendError::turn_busy(),
            other => other.into(),
        };
        let receipt = turn
            .sync
            .as_ref()
            .map(|sync| (sync.tab.clone(), client_message_id.clone()));
        let answered = move |outcome: Result<String, ConversationError>| {
            if let Some((tab, id)) = receipt {
                tab.settle(&id, outcome.is_ok());
            }
            done(outcome.map_err(turn_error));
        };
        let seq = row
            .then(|| conversation.projection().user_prompts.last().map(|p| p.seq))
            .flatten();
        let wait = turn
            .sync
            .map(|sync| sync.wait(&client_message_id, seq, after_revision, &turn.typed));
        let sent =
            conversation.send_turn_as_spawned(&turn.wire, &turn.typed, turn.origin, &client_message_id, wait, answered);
        match sent {
            Ok(()) => Ok(spawned),
            Err(error) => {
                if row {
                    conversation.fold_locally(AgentDomainEvent::UserPromptWithdrawn);
                }
                Err(turn_error(error))
            }
        }
    }

    /// The prompt a turn sent by [`Self::send_turn_spawned`] was accepted with names the session for the resume
    /// picker, as an accepted [`Self::send_turn`] names it.
    pub fn note_accepted_prompt(&mut self, as_typed: &str) {
        if let AgentBackend::Conversation { conversation, .. } = self {
            conversation.note_title(as_typed);
        }
    }

    /// Takes back the row of a prompt [`Self::send_turn_spawned`] folded, once the agent said it already had a turn
    /// open: the prompt was put into no turn.
    pub fn withdraw_prompt(&mut self) {
        self.fold_row(AgentDomainEvent::UserPromptWithdrawn);
    }

    /// What the tab should say about editor tools its session was given and the agent's provider left out,
    /// handed over once (`AgentConversation::editor_note`).
    pub fn editor_note(&self) -> Option<String> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => conversation.editor_note(),
        }
    }

    /// Whether the agent stopped this session itself with the conversation whole, so `r` on the ended tab
    /// continues it (`AgentConversation::stopped_resumable`). Only an external agent's can be.
    pub fn stopped_resumable(&self) -> bool {
        match self {
            AgentBackend::Conversation { agent, conversation } if agent.is_external() => {
                conversation.stopped_resumable()
            }
            _ => false,
        }
    }

    /// Stops work left running between turns. Claude stops the tasks and keeps its session;
    /// Codex stops its session and offers resume. Unsupported providers get a benign refusal.
    pub fn stop_background(&mut self, shown: u32) -> Result<(), BackendError> {
        let benign = |message: String| BackendError { message, benign: true };
        let AgentBackend::Conversation { agent, conversation } = self else {
            return Err(benign(
                "this agent leaves nothing running in the background".to_string(),
            ));
        };
        match conversation.stop_background(shown) {
            Ok(()) => Ok(()),
            Err(ConversationError::Provider(agent::ProviderError::UnsupportedCapability(_))) => {
                Err(benign(format!("{} cannot stop what it left running", agent.label())))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn stop_task(&mut self, task_id: &str) -> Result<agent::TaskReply<()>, BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation,
            } => conversation.stop_task(task_id).map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent cannot stop one task".into(),
                benign: true,
            }),
        }
    }

    pub fn task_output(&mut self, task_id: &str) -> Result<agent::TaskReply<agent::TaskOutput>, BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation,
            } => conversation.task_output(task_id).map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent cannot read task output".into(),
                benign: true,
            }),
        }
    }

    pub fn task_output_range(
        &mut self,
        task_id: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<agent::TaskReply<agent::TaskOutput>, BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation,
            } => conversation
                .task_output_range(task_id, offset, max_bytes)
                .map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent cannot read task output".into(),
                benign: true,
            }),
        }
    }

    pub fn list_models(&mut self) -> Result<agent::TaskReply<agent::ModelList>, BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude | AgentId::Acp(_),
                conversation,
            } => conversation.list_models().map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent has no list of models".into(),
                benign: true,
            }),
        }
    }

    pub fn set_model(&mut self, model: &str) -> Result<agent::TaskReply<agent::AppliedSettings>, BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude | AgentId::Acp(_),
                conversation,
            } => conversation.set_model(model).map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent cannot change its model here".into(),
                benign: true,
            }),
        }
    }

    pub fn apply_effort(&mut self, effort: &str) -> Result<agent::TaskReply<agent::AppliedSettings>, BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude | AgentId::Acp(_),
                conversation,
            } => conversation.apply_effort(effort).map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent cannot change its effort here".into(),
                benign: true,
            }),
        }
    }

    /// The sessions the agent of this tab lists as its own, including ones started in its own CLI: an ACP agent
    /// that lists them; none otherwise.
    pub fn listed_sessions(&self) -> Vec<agent::ListedSession> {
        match self {
            AgentBackend::Conversation { conversation, .. } => conversation.listed_sessions(),
            AgentBackend::Legacy(_) => Vec::new(),
        }
    }

    /// Asks the agent for its list of sessions again; the answer shows in [`listed_sessions`](Self::listed_sessions)
    /// a moment later.
    pub fn refresh_listed_sessions(&self) {
        if let AgentBackend::Conversation { conversation, .. } = self {
            conversation.refresh_listed_sessions();
        }
    }

    /// Whether the CLI left its own auto mode by itself on this session. Its own statement: on the
    /// sidecar `projection()` holds the ingestion mutex.
    pub fn cli_left_auto(&self) -> bool {
        self.projection().cli_left_auto
    }

    /// The calls of declared tools still waiting for an answer, oldest first. Read here rather than
    /// from delivered events, which a resync can lose. Its own statement: on the sidecar `projection()`
    /// holds the ingestion mutex, so the guard must be gone before a caller answers one of these.
    pub fn open_client_calls(&self) -> Vec<agent::OpenClientCall> {
        let mut open: Vec<_> = self.projection().open_client_calls.values().cloned().collect();
        open.sort_by_key(|call| call.sequence);
        open
    }

    /// The tools this session was given; none on the legacy backend and on an external agent, whose
    /// tools reach it some other way.
    pub fn declared_client_tools(&self) -> &std::collections::BTreeSet<String> {
        static NONE: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation,
            } => conversation.declared_client_tools(),
            _ => &NONE,
        }
    }

    /// Whether this session was given tools and the CLI's latest report names a server of the same name
    /// that fails [`agent::AgentSessionProjection::single_sdk_server`]: the session's calls under that
    /// name are then no different from any other MCP tool's. Its own statement, for the reason
    /// [`Self::open_client_calls`] gives.
    pub fn foreign_tool_server(&self) -> bool {
        if self.declared_client_tools().is_empty() {
            return false;
        }
        let projection = self.projection();
        projection.init_servers.iter().any(|server| server.name == SERVER_NAME)
            && !projection.single_sdk_server(SERVER_NAME)
    }

    /// Answers one call of a declared tool. Never waits on the agent, so the GTK thread may call it. An
    /// answer to a call that has settled is no error.
    pub fn answer_client_tool_call(
        &self,
        call_id: &str,
        text: Vec<String>,
        is_error: bool,
    ) -> Result<(), BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation,
            } => conversation
                .answer_client_tool_call(call_id, text, is_error)
                .map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent cannot take a client tool answer".into(),
                benign: true,
            }),
        }
    }

    /// Reads the session's open calls from the provider. The receiver carries the outcome.
    pub fn client_tool_call_state(&self) -> Result<agent::TaskReply<agent::ClientToolCallState>, BackendError> {
        match self {
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation,
            } => conversation.client_tool_call_state().map_err(task_command_error),
            _ => Err(BackendError {
                message: "this agent has no client tool calls".into(),
                benign: true,
            }),
        }
    }

    /// Answers one pending permission request.
    ///
    /// Legacy synthesizes the resolution because its wire reports no hook reply; every backend
    /// delivers resolutions through the pump, keeping their fold revisions and order.
    pub fn respond_permission(
        &mut self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), BackendError> {
        match self {
            AgentBackend::Legacy(session) => {
                session.respond_permission(permission_id, decision)?;
                Ok(())
            }
            AgentBackend::Conversation { conversation, .. } => {
                conversation.respond_permission(permission_id, decision)?;
                Ok(())
            }
        }
    }

    /// Sends a card answer through the provider's asynchronous path; legacy replies synchronously.
    pub fn respond_permission_async(
        &mut self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<agent::InteractionSend, BackendError> {
        match self {
            Self::Legacy(_) => {
                self.respond_permission(permission_id, decision)?;
                Ok(agent::InteractionSend::ready(Ok(None)))
            }
            Self::Conversation { conversation, .. } => {
                Ok(conversation.respond_permission_async(permission_id, decision)?)
            }
        }
    }

    /// Answers Claude's question set `interaction_id` with the person's `answers`.
    ///
    /// Reached only from a page's `question_answer`, after the page and the window agreed to it, and
    /// from nothing that answers cards: a question is never in `pending_permissions`, so Bypass,
    /// `approve_pending`, `confirm_bypass`, `Ctrl+y`, `a`/`d` and `answer_what_needs_no_human` cannot
    /// see one. The answer is checked against the set before anything is sent
    /// (`QuestionRecord::check_resolution`). Nothing waits for the agent here (this runs on the GTK
    /// thread): the answer goes out and the returned send carries the agent's reply, read with
    /// [`Self::finish_question_send`]. Every refusal or failure, here or in the reply, is benign and
    /// leaves the question waiting for another answer. Nothing answers a question because time passed.
    pub fn answer_question(
        &mut self,
        interaction_id: &str,
        answers: Vec<agent::QuestionAnswer>,
    ) -> Result<QuestionSend, BackendError> {
        self.resolve_question(interaction_id, agent::InteractionResolution::Answers(answers))
    }

    /// Declines Claude's question set `interaction_id`: `Cancel` (Escape) or `Chat` ("Chat about
    /// this", with what was answered `so_far`). The same door and checks as [`Self::answer_question`].
    pub fn decline_question(
        &mut self,
        interaction_id: &str,
        kind: agent::QuestionDeclineKind,
        so_far: Vec<agent::QuestionAnswer>,
    ) -> Result<QuestionSend, BackendError> {
        self.resolve_question(interaction_id, agent::InteractionResolution::Decline { kind, so_far })
    }

    fn resolve_question(
        &mut self,
        interaction_id: &str,
        resolution: agent::InteractionResolution,
    ) -> Result<QuestionSend, BackendError> {
        match self {
            // The legacy backend's CLI is never offered the question tool, so it has none to answer.
            AgentBackend::Legacy(_) => Err(BackendError {
                message: "this session asks no questions".to_string(),
                benign: true,
            }),
            AgentBackend::Conversation { conversation, .. } => {
                match conversation.resolve_question(interaction_id, resolution) {
                    Ok(send) => Ok(QuestionSend {
                        interaction_id: interaction_id.to_string(),
                        send,
                    }),
                    Err(ConversationError::Provider(error)) => Err(answer_refusal(error)),
                    Err(error) => Err(BackendError {
                        message: error.to_string(),
                        benign: true,
                    }),
                }
            }
        }
    }

    /// What became of `send`, an answer this backend sent: `None` while it is still on its way, never
    /// waiting for it. A refusal or failure puts the question back to waiting before it is returned,
    /// so the page that is told can answer again.
    pub fn finish_question_send(&mut self, send: &QuestionSend) -> Option<Result<(), BackendError>> {
        let reply = send.send.try_reply()?;
        Some(self.question_reply(send, reply))
    }

    /// [`Self::finish_question_send`] for a caller that may block (not the GTK thread): waits up to
    /// `timeout` for the reply, and a reply not come by then is a failure like any other.
    pub fn wait_question_send(
        &mut self,
        send: &QuestionSend,
        timeout: std::time::Duration,
    ) -> Result<(), BackendError> {
        let reply = send.send.wait(timeout);
        self.question_reply(send, reply)
    }

    fn question_reply(&mut self, send: &QuestionSend, reply: agent::InteractionReply) -> Result<(), BackendError> {
        let error = match reply {
            Ok(_) => return Ok(()),
            Err(error) => error,
        };
        if let AgentBackend::Conversation { conversation, .. } = self {
            conversation.question_send_failed(&send.interaction_id);
        }
        Err(answer_refusal(error))
    }

    /// Approves Claude's plan `interaction_id` into `mode`, naming the digest of the plan the person read.
    ///
    /// Reached only from a page's own plan answer through `TabSet::approve_plan`: a plan is never in
    /// `pending_permissions` or `questions`, so nothing that answers cards or questions reaches one. The
    /// decision is checked against the record before anything is sent (`PlanRecord::check_resolution`), and
    /// nothing waits for the agent here. Every refusal is benign and leaves the plan waiting.
    pub fn approve_plan(
        &mut self,
        interaction_id: &str,
        mode: agent::PlanExecutionMode,
        plan_sha256: &str,
    ) -> Result<PlanSend, BackendError> {
        self.resolve_plan(
            interaction_id,
            Some(mode),
            agent::InteractionResolution::PlanApprove {
                mode,
                plan_sha256: plan_sha256.to_string(),
            },
        )
    }

    /// "Keep planning" on Claude's plan `interaction_id`, with the person's `feedback` for the model
    /// (possibly empty). The same door and checks as [`Self::approve_plan`].
    pub fn keep_planning(&mut self, interaction_id: &str, feedback: String) -> Result<PlanSend, BackendError> {
        self.resolve_plan(
            interaction_id,
            None,
            agent::InteractionResolution::PlanKeep { feedback },
        )
    }

    fn resolve_plan(
        &mut self,
        interaction_id: &str,
        approval: Option<agent::PlanExecutionMode>,
        resolution: agent::InteractionResolution,
    ) -> Result<PlanSend, BackendError> {
        match self {
            // The legacy backend's CLI is never offered the plan tools.
            AgentBackend::Legacy(_) => Err(BackendError {
                message: "this session has no plan mode".to_string(),
                benign: true,
            }),
            AgentBackend::Conversation { conversation, .. } => {
                match conversation.resolve_plan(interaction_id, resolution) {
                    Ok(send) => Ok(PlanSend {
                        interaction_id: interaction_id.to_string(),
                        approval,
                        state: PlanSendState::Sent(send),
                    }),
                    Err(ConversationError::Provider(error)) => Err(plan_refusal(error)),
                    Err(error) => Err(BackendError {
                        message: error.to_string(),
                        benign: true,
                    }),
                }
            }
        }
    }

    /// What became of a decision this backend sent: `None` while it is on its way, never waiting for it. A
    /// failure puts the plan back to waiting before it is returned (`AgentConversation::plan_send_failed`,
    /// which also undoes the move out of plan mode an approval started when the agent refused it).
    pub fn finish_plan_send(&mut self, send: &PlanSend) -> Option<Result<(), PlanSendFailure>> {
        let PlanSendState::Sent(sent) = &send.state else {
            return None;
        };
        let error = match sent.try_reply()? {
            Ok(_) => return Some(Ok(())),
            Err(error) => error,
        };
        if let AgentBackend::Conversation { conversation, .. } = self {
            conversation.plan_send_failed(&send.interaction_id, &error);
        }
        Some(Err(PlanSendFailure {
            outcome_unknown: outcome_unknown(&error),
            error: plan_refusal(error),
        }))
    }

    /// Asks the agent to move the session into plan mode (`enter`) or out of it, for a person's own switch
    /// (`TabSet::set_planning`, which checks the tab is idle). Refused, with nothing sent, by the legacy
    /// backend and by a session that did not offer plan mode or is not where the move starts.
    pub fn set_planning(&mut self, enter: bool, now_ms: u64) -> Result<PlanningSwitch, BackendError> {
        match self {
            AgentBackend::Legacy(_) => Err(BackendError {
                message: "this session has no plan mode".to_string(),
                benign: true,
            }),
            AgentBackend::Conversation { conversation, .. } => match conversation.set_planning(enter, now_ms) {
                Ok(send) => Ok(PlanningSwitch { enter, send }),
                Err(ConversationError::Provider(error)) => Err(plan_refusal(error)),
                Err(error) => Err(BackendError {
                    message: error.to_string(),
                    benign: true,
                }),
            },
        }
    }

    /// What became of a switch this backend sent: `None` while it is on its way. Either way the reply is
    /// handed to the conversation, so the guard counts from it and a refusal undoes the move.
    pub fn finish_planning_switch(&mut self, switch: &PlanningSwitch) -> Option<Result<(), BackendError>> {
        let reply = switch.send.try_reply()?;
        let AgentBackend::Conversation { conversation, .. } = self else {
            return Some(reply.map(|_| ()).map_err(plan_refusal));
        };
        Some(match reply {
            Ok(_) => {
                conversation.planning_send_taken(switch.enter);
                Ok(())
            }
            Err(error) => {
                conversation.planning_send_failed(switch.enter, &error);
                Err(plan_refusal(error))
            }
        })
    }

    /// Why the session's move into or out of plan mode is overdue at `now_ms`, if it is
    /// (`AgentConversation::plan_transition_overdue`). Never for the legacy backend, which has no plan mode.
    pub fn plan_transition_overdue(&self, now_ms: u64) -> Option<String> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => conversation.plan_transition_overdue(now_ms),
        }
    }

    /// Records that the panel was just handed a snapshot of this backend read at `revision`, for the
    /// next drain to take back ([`Self::take_ui_snapshot_revision`]) and leave every event tagged at
    /// or below it out of its `events` payload, so the page never applies an event twice that the
    /// snapshot already holds. Kept by the backend itself, never by the tab, so a snapshot's
    /// revision cannot outlive the session it was read from, whichever way that session is replaced.
    pub fn note_ui_snapshot(&mut self, revision: u64) {
        match self {
            AgentBackend::Legacy(session) => {
                #[cfg(feature = "legacy-backend")]
                session.note_ui_snapshot(revision);
                #[cfg(not(feature = "legacy-backend"))]
                let _ = session;
            }
            AgentBackend::Conversation { conversation, .. } => conversation.note_ui_snapshot(revision),
        }
    }

    /// Takes what [`Self::note_ui_snapshot`] recorded since the last take.
    pub fn take_ui_snapshot_revision(&mut self) -> Option<u64> {
        match self {
            AgentBackend::Legacy(session) => {
                #[cfg(feature = "legacy-backend")]
                {
                    session.take_ui_snapshot_revision()
                }
                #[cfg(not(feature = "legacy-backend"))]
                {
                    let _ = session;
                    None
                }
            }
            AgentBackend::Conversation { conversation, .. } => conversation.take_ui_snapshot_revision(),
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
    ///
    /// **This is also where a tool call that needs no human is answered and never becomes a card**
    /// -- see `answer_what_needs_no_human`. `project_root` is the session's canonical project
    /// directory, which is the boundary `Read`/`Grep`/`Glob` are judged against; it is a parameter
    /// rather than state on this enum because a caller that forgets it then does not compile.
    /// `mode` is the tab's own `Tab::mode`: in `Bypass` every request is answered here, never only
    /// what the classifier would allow in `Auto`. `rules` are the project's saved prefix rules
    /// (`agent::permission_rules`), applied by the classifier in `Auto`; a rule can only replace the
    /// two "not on the read-only list" verdicts, never a fail-closed one (`agent::classify_with_rules`).
    /// `host_answered` collects every id this call answers on the caller's behalf (bypass or the
    /// classifier), so the caller (`TabSet::pump`) can hide them from the tray and from a later
    /// snapshot.
    ///
    /// `approvals` are the calls the user approved on a card (`TabSet::answer_card`), each with the
    /// tool and exact input the card showed. In `Auto` the CLI's own prompt for exactly such a call
    /// -- same non-empty tool-use id, same tool, same input -- is answered `allow` here, once, and the
    /// approval is used up: the user approved that call, and the real CLI asks once. A caller passing
    /// no approvals sees every such prompt as a card. `answered_for_you` receives what was answered
    /// without a card that a row says so about ([`AnsweredForYou`]): every CLI prompt, every gate the
    /// acceptEdits fast path answered, every gate a saved prefix rule answered, and every `Write` so
    /// answered over no file.
    ///
    /// Each event keeps its fold revision so a caller sending snapshots can leave out of its next
    /// events payload whatever the last snapshot already carried ([`Self::note_ui_snapshot`]); a
    /// caller that sends none drops them with `RevisedDelivery::without_revisions`. Both backends tag
    /// an event with the projection's `last_revision` right after its own fold, so the tags compare
    /// directly with a snapshot's. An event answered here without a card, and one only the host reads,
    /// is dropped with its tag; the others keep theirs, in order.
    pub fn take_revised_ui_delivery<'a>(
        &mut self,
        project_root: GateRoot<'a>,
        rules: &agent::PrefixRules,
        mode: impl Into<GateMode>,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
        answered_for_you: &mut AnsweredForYou,
    ) -> RevisedDelivery {
        self.take_revised_ui_delivery_after_answers(
            project_root,
            rules,
            mode,
            host_answered,
            approvals,
            &BTreeSet::new(),
            answered_for_you,
            &mut PlanBook::default(),
        )
    }

    /// [`Self::take_revised_ui_delivery`] for a caller whose user answers cards: `user_answered` are
    /// the ids answered there, sends in flight included. Such a request is never held for another
    /// approval and never reconsidered here, so nothing answers it a second time.
    #[allow(clippy::too_many_arguments)]
    pub fn take_revised_ui_delivery_after_answers<'a>(
        &mut self,
        project_root: GateRoot<'a>,
        rules: &agent::PrefixRules,
        mode: impl Into<GateMode>,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
        user_answered: &BTreeSet<String>,
        answered_for_you: &mut AnsweredForYou,
        plan: &mut PlanBook,
    ) -> RevisedDelivery {
        let GateMode {
            mode,
            followed_out_of_auto,
            plan_followed,
        } = mode.into();
        // How far the projection had folded before this drain took its events. A sidecar's ingestion thread
        // folds on while the drain runs, so a request the projection no longer lists when the drain looks
        // later may have left by an event the next drain delivers; a lost answer is dropped with its request
        // only when nothing was folded since this point (see the pruning below).
        let folded_before_take = (!approvals.lost.0.is_empty() || !approvals.host_sends.0.is_empty())
            .then(|| self.projection().last_revision);
        let delivery = match self {
            AgentBackend::Legacy(session) => {
                let events = session.pump_revised();
                if events.is_empty() {
                    RevisedDelivery::Nothing
                } else {
                    RevisedDelivery::Events(events)
                }
            }
            AgentBackend::Conversation { conversation, .. } => conversation.take_revised_ui_delivery(),
        };
        #[cfg(test)]
        drain_test_hooks::after_take(self);
        // A prompt can precede the acknowledgement of the approval that covers it. Keep it
        // pending, and reconsider it from canonical state after that send settles. A backend's
        // resync may have dropped a turn boundary, so recorded approvals end with it; this is the
        // only place they end on a resync.
        let backend_resync = match &delivery {
            RevisedDelivery::Resync { revision } => Some(*revision),
            _ => None,
        };
        // The newest revision this drain took off the queue, in fold order; what a resync for a released card
        // replaces, and so the furthest it may claim to cover.
        let drained_through = match &delivery {
            RevisedDelivery::Events(events) => events.last().map(|(revision, _)| *revision),
            _ => None,
        };
        let resync = backend_resync.is_some();
        if resync {
            approvals.forget_recorded();
        }
        let lost_before: BTreeSet<String> = approvals.lost.ids().cloned().collect();
        Self::note_resolutions_of_answers_out(approvals, &delivery);
        let refused: BTreeSet<String> = Self::settle_host_sends(approvals, host_answered, answered_for_you)
            .into_iter()
            .collect();
        Self::record_lost_answers_taken(approvals, host_answered, answered_for_you);
        // An answer lost on the way (the link broke, or no reply came in time) is its request's card,
        // delivered once with the drain that read the loss, and is sent once more by a later drain that
        // reads the link live, if it still stands then.
        let newly_lost: BTreeSet<String> = approvals
            .lost
            .ids()
            .filter(|id| !lost_before.contains(*id))
            .cloned()
            .collect();
        self.resend_lost_when_back(approvals, user_answered, mode, &newly_lost, plan, plan_followed);
        let cards_only: BTreeSet<String> = refused.iter().chain(newly_lost.iter()).cloned().collect();
        let (mut retry, waiting) = if !approvals.in_flight.is_empty() || !approvals.held.is_empty() {
            let projection = self.projection();
            approvals.held.retain(|id| !user_answered.contains(id));
            for (id, p) in &projection.pending_permissions {
                if !user_answered.contains(id) && approvals.holds(p, &projection.active_turn_id) {
                    approvals.held.insert(id.clone());
                }
            }
            let waiting = approvals.held.clone();
            let mut retry = Vec::new();
            for id in std::mem::take(&mut approvals.held) {
                let Some(p) = projection.pending_permissions.get(&id) else {
                    continue;
                };
                if approvals.holds(p, &projection.active_turn_id) {
                    approvals.held.insert(id);
                    continue;
                }
                retry.push((
                    projection.last_revision,
                    AgentDomainEvent::PermissionRequested {
                        permission_id: id,
                        tool_use_id: p.tool_use_id.clone(),
                        tool_name: p.tool_name.clone(),
                        input: p.input.clone(),
                        provider_prompt: p.provider_prompt.clone(),
                    },
                ));
            }
            (retry, waiting)
        } else {
            (Vec::new(), BTreeSet::new())
        };
        // Pruned on every drain to what the projection still lists, so an id stays only while its request
        // waits.
        if !approvals.resolved_out.0.is_empty() {
            let HumanApprovals {
                host_sends,
                lost,
                resolved_out,
                ..
            } = &mut *approvals;
            resolved_out
                .0
                .retain(|id, _| host_sends.contains(id) || lost.ids().any(|lost| lost == id));
        }
        if !approvals.fell_back.is_empty() || !approvals.lost.0.is_empty() {
            let projection = self.projection();
            approvals
                .fell_back
                .retain(|id| projection.pending_permissions.contains_key(id));
            // A lost answer is kept for its resolution (`record_lost_answers_taken`), so it goes with its
            // request only once every fold is one a drain has delivered: had the projection moved since it was
            // read before this drain's take, the request may have left by a resolution the next drain brings,
            // and dropping the answer now would leave that resolution nothing to record.
            if folded_before_take == Some(projection.last_revision) {
                approvals
                    .lost
                    .0
                    .retain(|lost| projection.pending_permissions.contains_key(&lost.planned.permission_id));
            }
        }
        if !cards_only.is_empty() {
            let projection = self.projection();
            approvals.fell_back.extend(refused.iter().cloned());
            for id in &cards_only {
                let Some(p) = projection.pending_permissions.get(id) else {
                    continue;
                };
                retry.push((
                    projection.last_revision,
                    AgentDomainEvent::PermissionRequested {
                        permission_id: id.clone(),
                        tool_use_id: p.tool_use_id.clone(),
                        tool_name: p.tool_name.clone(),
                        input: p.input.clone(),
                        provider_prompt: p.provider_prompt.clone(),
                    },
                ));
            }
        }
        // Requests held while the session left plan mode are judged once more as soon as nothing holds them
        // any more, under the mode the tab took up; one that is a card then is drawn by a snapshot, since its
        // event was kept from the page.
        if !plan.held.is_empty() {
            let projection = self.projection();
            if !plan_holds(&projection, plan_followed) {
                for id in std::mem::take(&mut plan.held) {
                    let Some(p) = projection.pending_permissions.get(&id) else {
                        continue;
                    };
                    if user_answered.contains(&id) || retry.iter().any(|(_, event)| matches!(event, AgentDomainEvent::PermissionRequested { permission_id, .. } if *permission_id == id)) {
                        continue;
                    }
                    retry.push((
                        projection.last_revision,
                        AgentDomainEvent::PermissionRequested {
                            permission_id: id,
                            tool_use_id: p.tool_use_id.clone(),
                            tool_name: p.tool_name.clone(),
                            input: p.input.clone(),
                            provider_prompt: p.provider_prompt.clone(),
                        },
                    ));
                }
            }
        }
        // Requests waiting for their call are judged once their call has been folded or the wait is over; one that
        // is a card then is drawn by a snapshot, since its event was kept from the page. The others stay hidden and
        // are looked at again on the next drain.
        if !plan.unsequenced.is_empty() {
            let now_ms = wall_clock_ms();
            let projection = self.projection();
            for (id, first_ms) in plan.unsequenced.iter_mut() {
                // The wall clock stepped back: the wait starts again from now rather than lasting until the clock has
                // caught up, with the CLI waiting on the answer all that time. A step forward ends it early, which
                // only judges the request in both modes.
                if *first_ms > now_ms {
                    *first_ms = now_ms;
                }
                let Some(p) = projection.pending_permissions.get(id) else {
                    continue;
                };
                if user_answered.contains(id)
                    || retry.iter().any(|(_, event)| matches!(event, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == id))
                {
                    continue;
                }
                // A plan card is a person's to answer whatever its call turns out to be: it is drawn at once.
                let sequence = Sequence::read(&projection, id, named_call(&p.tool_use_id).as_deref());
                if sequence.waits()
                    && !plan.cards.contains(id)
                    && now_ms.saturating_sub(*first_ms) < PLAN_SEQUENCE_WAIT_MS
                {
                    continue;
                }
                retry.push((
                    projection.last_revision,
                    AgentDomainEvent::PermissionRequested {
                        permission_id: id.clone(),
                        tool_use_id: p.tool_use_id.clone(),
                        tool_name: p.tool_name.clone(),
                        input: p.input.clone(),
                        provider_prompt: p.provider_prompt.clone(),
                    },
                ));
            }
        }
        let retried: BTreeSet<String> = retry
            .iter()
            .filter_map(|(_, event)| match event {
                AgentDomainEvent::PermissionRequested { permission_id, .. } => Some(permission_id.clone()),
                _ => None,
            })
            .collect();
        let mut events = match delivery {
            RevisedDelivery::Events(events) => events,
            _ => Vec::new(),
        };
        events.retain(|(_, event)| {
            !matches!(event, AgentDomainEvent::PermissionRequested { permission_id, .. }
            if waiting.contains(permission_id))
        });
        let mut all = retry;
        all.extend(events);
        let kept = self.answer_what_needs_no_human(
            all,
            project_root,
            AnswerContext {
                rules,
                mode,
                followed_out_of_auto,
                plan_followed,
                host_answered,
                approvals,
                answered_for_you,
                cards_only: &cards_only,
                plan,
            },
        );
        // Releasing a held card needs a snapshot: its fold revision may already have
        // been covered by a snapshot which deliberately hid it. One answered here needs none.
        let released = kept.iter().any(|(_, event)| {
            matches!(event, AgentDomainEvent::PermissionRequested { permission_id, .. }
                if retried.contains(permission_id))
        });
        // Such a snapshot replaces this batch, and the tab set ends a turn's approvals from the
        // events it delivers: a turn end in the batch ends them here. Without one they are kept, as
        // they still answer their own calls' prompts.
        if released
            && !resync
            && kept.iter().any(|(_, event)| {
                matches!(event, AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id != agent::NO_OPEN_TURN_ID)
            })
        {
            approvals.forget_recorded();
            approvals.forget_in_flight(&self.projection(), user_answered);
        }
        if let Some(revision) = backend_resync {
            RevisedDelivery::Resync { revision }
        } else if released {
            // The snapshot stands in for exactly what this drain took. Folds after it are still queued and arrive
            // as events; the tab set bounds evicted calls by this revision, so a call whose turn's events are still
            // queued waits for them. A drain that took nothing claims nothing.
            RevisedDelivery::Resync {
                revision: drained_through.unwrap_or(0),
            }
        } else if kept.is_empty() {
            RevisedDelivery::Nothing
        } else {
            RevisedDelivery::Events(kept)
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
    /// must answer", which is what put a card in front of the user for every single `Read`. Fixing
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
    /// **In `Bypass` (since 2026-09-27), every `PermissionRequested` is answered `Allow` here,
    /// unconditionally, before the classifier ever runs.** This is Eitri's own definition of
    /// bypass: the CLI itself is never told to skip permissions (it always runs gated,
    /// `--permission-mode default` plus the hook, or the sidecar's INTERACTIVE mode); "bypass" means
    /// Eitri answers every request `allow` under that gate instead of showing a card. `mode` comes
    /// from the caller's `Tab::mode` on every pump -- this function holds no mode of its own. The one
    /// difference it allows itself is toward a card: an Auto tab that has not yet followed the CLI out of
    /// auto answers as Manual.
    ///
    /// **An id already in `host_answered` is dropped without a second answer, whatever the mode**
    /// (a duplicate delivery of a request already answered, e.g. after a resync). One check before
    /// any answer is chosen covers every way of answering below, and the copy is no card either:
    /// the tab set keeps an id there only while the projection still lists it, so the request it
    /// repeats has been answered and is only waiting for its resolution.
    ///
    /// **Nothing is answered once the CLI has reported an ungated mode** -- neither
    /// in this batch after the `UngatedCliMode`, nor in any batch after the projection recorded one,
    /// and this holds in both modes. Such a session is about to be closed (`TabSet::pump`), and a
    /// request left unanswered is denied by that close; an automatic `allow` here -- in bypass or
    /// under the classifier -- would be a grant made to a session nobody should be granting anything.
    ///
    /// Resolutions arrive on a later pump, including legacy's locally folded hook answers. A
    /// resolution for a request hidden here is harmless: the page removes pending requests by id.
    ///
    /// **The second gap this doc used to record is closed, not merely documented, by
    /// `host_answered`:** an id this function inserts into `host_answered` is hidden from a
    /// `Resync`'s snapshot and from the attention tray by `TabSet::pump` (`SnapshotView::of`,
    /// `AttentionTracker::resync`), so a request answered here can no longer resurrect a card the
    /// user was never meant to see, in bypass or under the classifier alike.
    ///
    /// **The CLI's own prompts have their own rules, checked before either
    /// path above** (`provider_prompt: Some`): the CLI raised one after the gate had already
    /// answered, for a check of its own that no hook `allow` or session rule silences (its
    /// sensitive-file check is the measured one). So the classifier and the saved prefix rules never
    /// see it: their `allow` would silence the very check the CLI added after the gate. **Bypass
    /// allows every one**, whatever it says about why it asked: one the
    /// user's own `permissions.ask` rule forced, one of a kind this build does not know and one that
    /// gives no reason are answered like the sensitive-file check, because bypass lets everything
    /// through. In `Auto` it is allowed only when `approvals` holds the same call -- id, tool and
    /// input the user approved on a card -- and that approval is then used up; otherwise it is a card,
    /// so Auto never passes a check of the CLI's that the user has not seen. One that
    /// `needs_its_own_card` -- forced by the user's own ask rule, of an unknown
    /// kind, or giving no reason for asking (a content-scoped ask rule looks like that) -- is a card in
    /// `Auto` even with such an approval.
    /// Each answered without a card goes into `answered_for_you` with its row's note.
    ///
    /// **The acceptEdits fast path's own row note is recorded here too, never inferred elsewhere.**
    /// A `Write`/`Edit`/`NotebookEdit` this
    /// function allowed under the classifier -- which only the fast path ever does for those three
    /// (`agent::permission_policy`'s module doc: no saved rule touches them) -- goes into
    /// `answered_for_you.by_the_fast_path` by tool-use id, with the `(tool_name, input)` the answer
    /// saw (a LATER card for the same call can carry an empty tool-use id, so the tab
    /// set's own "same id" removal cannot always find the candidate this leaves), once
    /// `respond_permission` succeeded. The tab set used to infer the same thing from absence (a call
    /// that started in Auto and completed with no card delivered for it), which was also true of a
    /// call the CLI's `validateInput` failed before any hook ran, of a card whose request carried no
    /// tool-use id, and of a gate a switch to bypass answered instead. A request with no tool-use id
    /// is answered as ever and noted nowhere: there is no row to name.
    ///
    /// **So is a `Write` that creates its file**, in the fast path and in bypass alike:
    /// whether anything exists at its `file_path` is checked just before the `allow` is sent, when
    /// the CLI cannot have written it yet, and a `Write` over nothing goes into
    /// `answered_for_you.creates_file`. `note_new_files` only ever sees a delivered card, which
    /// neither of these two answers leaves behind.
    ///
    /// **So is a saved prefix rule's own note**, which before 2026-09-28 was inferred the same way.
    /// `classify_with_rules`
    /// checks `agent::rule_that_allows` before anything else, so `classification.reason ==
    /// REASON_ALLOWED_BY_A_PROJECT_RULE` means a rule, not the read-only classifier, is why this call
    /// is being allowed here; `agent::rule_that_allows` is asked again for the rule's own display
    /// string (cheap, and the same re-ask the tab set used to make -- only moved to where the answer
    /// is actually given) and the pair goes into `answered_for_you.by_rule` by tool-use id, with the
    /// `(tool_name, input)` the answer saw for the same reason `by_the_fast_path` carries it, once
    /// `respond_permission` succeeded. The tab set used to make every `ToolCallStarted` a rule WOULD
    /// answer -- re-asking `agent::rule_that_allows` on that event's own arguments, before any gate
    /// existed to answer -- a candidate, and infer the answer from no card ever having arrived: also
    /// true of a call the CLI's `validateInput` failed before any hook ran (no gate at all), and of a
    /// gate a switch to bypass answered instead of the rule.
    ///
    /// **On a session whose CLI runs its own auto mode under the gate, `Auto` defers** (2026-10-02,
    /// Verdandi protocol 3.14). Such a session asked for it (`ProviderCapabilities::cli_auto_mode`:
    /// the sidecar offered both `cli_auto_mode` and `permission_defer`) and the CLI's latest report
    /// says `auto` (`AgentSessionProjection::cli_runs_auto`). There, at a project root that is a
    /// boundary (`agent::project_boundary`), a gate request in `Auto` is answered `allow` when a saved prefix rule allows it -- `agent::rule_that_allows`, the same
    /// rule path `classify_with_rules` takes, with the same limits -- and otherwise
    /// `PermissionDecision::Defer`: Eitri's own classifier is not consulted and no card is drawn, and
    /// the CLI's classifier decides. A refusal comes back as `AgentDomainEvent::PermissionDenied`
    /// and the call's error result. A deferral that fails to send is a card, like any failed answer.
    /// The projection is read again immediately before each request, not once per batch: ingestion
    /// keeps folding while the batch is answered, and a trip or a fallback to `default` it folds
    /// there must reach the next request, which then is not deferred (a trip: not answered).
    ///
    /// **At a root that is no boundary nothing is deferred.** The classifier the CLI runs was given
    /// a project to protect, and such a root is not one: `$HOME`, a tree others can write to. Eitri's
    /// own policy answers instead, as for a Manual tab on the same session, so every path-judged
    /// call is a card and no saved rule replaces one.
    ///
    /// Everything above is unchanged on such a session: the tripwire, the CLI's own prompts and
    /// bypass. A session that did not ask for auto, or whose CLI fell back to `default`, is answered
    /// exactly as before, by the classifier below.
    ///
    /// **`Manual` never defers and never takes the fast path**, on any session: Eitri's own policy
    /// answers every gate request (`agent::classify_in_manual`, which cards every file write or edit
    /// and is otherwise the Auto classifier with its saved rules), whatever the CLI runs under it.
    /// The CLI's own prompts are answered as in `Auto`, and an external agent's requests are all
    /// cards, as in `Auto`. It is never more permissive than `Auto` on any request.
    ///
    /// **The session's own declared tools are answered `allow` here, in every mode**, ahead of the
    /// bypass answer, the deferral and both classifiers: the call then reaches the editor broker, which
    /// judges it by a table of its own and raises its own card where that table says so. The hook's
    /// answer is the same in `Auto` and `Manual`, so `Manual` stays no more permissive than `Auto`, and
    /// the read tools cost no call to the CLI's classifier. It holds only for the exact declared name,
    /// and only while the CLI's latest report lists one server of that name, in process, read again for
    /// each request as the tripwire is. A same-named project or `--mcp-config` server shows as that one
    /// entry with its own source while its tools run beside the host's, and a server named
    /// `eitri__x` has tools under the same prefix: both are unknown MCP tools. An external agent never
    /// comes here (its tools reach Eitri some other way), and the CLI's own prompts keep their rules.
    ///
    /// Each event travels with its fold revision (`take_revised_ui_delivery`), untouched: a kept
    /// event keeps its own, a dropped one takes its with it.
    ///
    /// The choice of answer is `decide`'s; sending it and recording it is `answer_and_record`'s, the
    /// one place an answer chosen here reaches the provider.
    fn answer_what_needs_no_human(
        &mut self,
        events: Vec<(u64, AgentDomainEvent)>,
        // GateRoot enters the answer path here; a remote tab supplies no laptop root.
        project_root: GateRoot<'_>,
        cx: AnswerContext<'_>,
    ) -> Vec<(u64, AgentDomainEvent)> {
        let AnswerContext {
            rules,
            mode,
            followed_out_of_auto,
            plan_followed,
            host_answered,
            approvals,
            answered_for_you,
            cards_only,
            plan,
        } = cx;
        // The tab's own mode, what a remote answer is sent under: a lost one is sent again only while the tab is
        // still in it.
        let tab_mode = mode;
        // What the batch's own events have shown so far; the projection is read again for every
        // request below, because ingestion keeps folding while earlier answers of this batch are
        // sent, and a trip or a fallback to `default` it folds there must reach the next request.
        let mut tripped = false;
        // Read once per batch: the environment and the running binary do not move under a window.
        let places = agent::EitriPlaces::from_process();
        let mut kept = Vec::with_capacity(events.len());
        for (revision, event) in events {
            // The host reads these from the projection, where a resync cannot lose them; no page has
            // anything to do with one.
            if matches!(
                event,
                AgentDomainEvent::ClientToolCall { .. }
                    | AgentDomainEvent::ClientToolCallSettled { .. }
                    | AgentDomainEvent::SessionServers { .. }
                    | AgentDomainEvent::CliLeftAuto
            ) {
                continue;
            }
            tripped |= matches!(
                event,
                AgentDomainEvent::UngatedCliMode { .. } | AgentDomainEvent::PolicyMismatch { .. }
            );
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
            plan.looked_at.insert(permission_id.clone());
            // Read afresh for each request, as its own statement: on the sidecar `projection()`
            // holds the ingestion mutex and `respond_permission` locks it again (the 2026-09-15 GTK
            // freeze), so the guard is gone before any answer below. Both halves are needed for a
            // deferral: the session asked for the CLI's auto mode, and the CLI says it is running it.
            let call = named_call(tool_use_id);
            let (projection_tripped, cli_reports_auto, one_own_server, cli_left_auto, plan_hold, main_thread, sequence) = {
                let projection = self.projection();
                (
                    session_tripped(&projection),
                    projection.cli_runs_auto(),
                    projection.single_sdk_server(SERVER_NAME),
                    projection.cli_left_auto,
                    plan_holds(&projection, plan_followed),
                    on_the_main_thread(&projection, call.as_deref()),
                    Sequence::read(&projection, permission_id, call.as_deref()),
                )
            };
            tripped |= projection_tripped;
            // The tab follows the CLI out of auto between batches (`TabSet::follow_cli_out_of_auto`). Until it
            // has, an Auto tab answers as the Manual tab it is about to be; whether it has is the tab's own to
            // say, because a report can be folded at any moment around this read. Once it has, `Auto` is a
            // person's choice made after the move.
            let mode = if mode == PermissionMode::Auto && cli_left_auto && !followed_out_of_auto {
                PermissionMode::Manual
            } else {
                mode
            };
            // Only an Auto tab leaves anything to the CLI's classifier: Manual is Eitri's policy
            // alone, whatever the CLI runs under it. Nor does an Auto tab at a root that is no
            // boundary: Eitri's policy answers there, and every path-judged call is a card.
            let defers_to_the_cli = mode == PermissionMode::Auto
                && self.capabilities().cli_auto_mode
                && cli_reports_auto
                && gate_root_is_a_boundary(project_root);
            if tripped {
                eprintln!("[permission] not answering {tool_name}: the agent reported a policy it was not asked for");
                kept.push((revision, event));
                continue;
            }
            // One check for every way of answering, before any is chosen: an id the host already
            // answered (a duplicate delivery, e.g. after a resync) is dropped without a second answer,
            // and so is one whose answer is still on its way.
            if host_answered.contains(permission_id) || approvals.host_sends.contains(permission_id) {
                continue;
            }
            // A host answer that failed to reach a remote agent is shown as the card it fell back to.
            if cards_only.contains(permission_id) {
                plan.unsequenced.remove(permission_id);
                kept.push((revision, event));
                continue;
            }
            // A card raised while the CLI ran plan mode is a person's to answer, whatever the tab's mode now. It waits
            // for nothing: left among `unsequenced`, it would stay hidden, and hold back an approval, for good.
            if plan.cards.contains(permission_id) {
                eprintln!("[permission] asking the user: {tool_name} (asked in plan mode)");
                plan.unsequenced.remove(permission_id);
                kept.push((revision, event));
                continue;
            }
            let declared = self.declared_client_tools();
            let own_tools = (one_own_server && !declared.is_empty()).then_some(declared);
            let external = self.agent().is_external();
            // On a session that offered plan mode a request is not judged before the call it gates has been
            // folded: the sidecar sends hook requests ahead of its message events, so until then the host does
            // not know whether the CLI made the call in plan mode. It waits hidden, answered by nobody, and every
            // drain looks at it again (`take_revised_ui_delivery_after_answers`) until its call is seen or
            // `PLAN_SEQUENCE_WAIT_MS` has passed (a call the sidecar never shows, a request that names none). One whose
            // wait ran out is judged in either mode (`Sequence::phase`): nothing else says where the CLI stood.
            let ordered = sequence.offered
                && !external
                && waits_for_its_call(tool_name, provider_prompt.is_some(), mode, own_tools);
            if ordered && sequence.waits() {
                if plan.waited_ms(permission_id, wall_clock_ms()) < PLAN_SEQUENCE_WAIT_MS {
                    continue;
                }
                eprintln!("[permission] judging {tool_name} without its call: the call did not arrive in time");
            }
            plan.unsequenced.remove(permission_id);
            let request = GateRequest {
                permission_id,
                tool_use_id,
                tool_name,
                input,
                provider_prompt: provider_prompt.as_ref(),
            };
            let judge = |planning: Option<PlanGate>| Judge {
                mode,
                external,
                defers_to_the_cli,
                own_tools,
                rules,
                project_root,
                approvals: &*approvals,
                planning,
                places: &places,
            };
            // The tab's mode is the mode plan mode was entered from: while the CLI runs plan mode, its table
            // decides (`plan_verdict`).
            let in_plan = PlanGate {
                origin: mode,
                cli_runs_auto: cli_reports_auto,
                main_thread,
            };
            let phase = sequence.phase(ordered);
            let planned = match phase {
                RequestPhase::Executing => decide(request, judge(None)),
                RequestPhase::Planning => decide(request, judge(Some(in_plan))),
                // Either mode may be the one the CLI asked in: an answer only where both give the same one, and
                // otherwise a card. A deferral is no safe middle: outside plan mode a Manual tab's deferral would
                // reach an auto CLI's classifier.
                RequestPhase::Uncertain => {
                    let executing = decide(request, judge(None));
                    let planning = decide(request, judge(Some(in_plan)));
                    match (executing, planning) {
                        (Some(executing), Some(planning)) if executing.decision == planning.decision => Some(planning),
                        (None, _) | (_, None) => None,
                        (Some(_), Some(_)) => {
                            eprintln!("[permission] asking the user: {tool_name} (plan mode may or may not apply)");
                            None
                        }
                    }
                }
            };
            let Some(planned) = planned else {
                if phase != RequestPhase::Executing {
                    plan.make_card(permission_id);
                }
                kept.push((revision, event));
                continue;
            };
            // Nothing is answered automatically while the session leaves plan mode, nor before the tab has
            // taken up the mode leaving gave it: the request waits, drawn as no card, to be judged under
            // that mode (`take_revised_ui_delivery_after_answers`).
            if plan_hold {
                eprintln!("[permission] holding {tool_name} until the mode change settles");
                plan.held.insert(permission_id.clone());
                continue;
            }
            let deferred_call = (phase != RequestPhase::Executing && planned.decision.defers())
                .then(|| planned.call.clone())
                .flatten();
            // As `approve_pending` decides it: a backend that reports a link answers over the network, whatever
            // root its tab named.
            let remote = matches!(project_root, GateRoot::RemoteWithoutFacts) || self.link_state().is_some();
            if self.answer_and_record(
                planned,
                host_answered,
                approvals,
                answered_for_you,
                remote.then_some(tab_mode),
            ) {
                plan.deferred_in_flight.extend(deferred_call);
            } else {
                kept.push((revision, event));
            }
        }
        kept
    }

    /// Sends the answer `decide` chose and, once the provider took it, records it: the gate answer,
    /// the id in `host_answered`, the approval it used up and each row note it carries. Every way of
    /// answering without a card ends here, so none can send an answer it does not record or record
    /// one it did not send. For a local tab the reply is waited for here. For a remote tab the answer
    /// goes out without waiting and `true` means only that it went: the recording happens when a later
    /// delivery reads its reply (`settle_host_sends`), and a refused or lost one is delivered then as
    /// its card. `false` when the answer could not be sent: the caller delivers the request as a card,
    /// since a request left pending with nothing on screen is a session the user cannot unstick.
    fn answer_and_record(
        &mut self,
        planned: PlannedAnswer,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
        answered_for_you: &mut AnsweredForYou,
        remote: Option<PermissionMode>,
    ) -> bool {
        if let Some(mode) = remote {
            // A remote tab's answer crosses the network: it goes out without this thread waiting, and is
            // recorded when its reply is read; a refused or lost one becomes the card then.
            return match self.respond_permission_async(&planned.permission_id, planned.decision.clone()) {
                Ok(send) => {
                    // Used up as the answer goes, not when its reply is read: a later prompt in this batch, or one
                    // before the reply, under another permission id is a card. A refused or lost answer falls back
                    // to the person's card, never to the approval.
                    if let Some(id) = &planned.consumes_approval {
                        approvals.consume(id);
                    }
                    approvals.host_sends.0.push(HostSend {
                        planned,
                        send,
                        mode,
                        resent: false,
                    });
                    true
                }
                Err(error) => {
                    eprintln!(
                        "[permission] could not {}, showing a card instead: {}",
                        planned.could_not, error.message
                    );
                    false
                }
            };
        }
        match self.respond_permission(&planned.permission_id, planned.decision.clone()) {
            Ok(_resolution) => {
                record_host_answer(planned, host_answered, approvals, answered_for_you);
                true
            }
            Err(error) => {
                eprintln!(
                    "[permission] could not {}, showing a card instead: {}",
                    planned.could_not, error.message
                );
                false
            }
        }
    }

    /// Reads the replies of a remote tab's host answers that came: a taken one is recorded as the
    /// synchronous path records it; the ids of refused ones are returned, for a card the person answers
    /// from then on. A lost one (the connection broke, or no reply came in time: the agent may never
    /// have had it) is kept in `approvals.lost` with the mode it was sent under, a card until it is sent
    /// again ([`Self::resend_lost_when_back`]); one lost a second time stays a card, with its planned answer
    /// kept for a later resolution to identify, so no answer goes round again. A retry refused as already
    /// resolved keeps that evidence too: the first answer may have arrived before its resolution did.
    fn settle_host_sends(
        approvals: &mut HumanApprovals,
        host_answered: &mut BTreeSet<String>,
        answered_for_you: &mut AnsweredForYou,
    ) -> Vec<String> {
        let mut refused = Vec::new();
        let mut waiting = Vec::new();
        for sent in std::mem::take(&mut approvals.host_sends.0) {
            match sent.send.try_reply() {
                None => waiting.push(sent),
                Some(Ok(_)) => record_host_answer(sent.planned, host_answered, approvals, answered_for_you),
                Some(Err(error @ (agent::ProviderError::Transport(_) | agent::ProviderError::Timeout))) => {
                    eprintln!(
                        "[permission] could not {}, showing a card until the link is back: {error}",
                        sent.planned.could_not
                    );
                    approvals.lost.0.push(LostAnswer {
                        planned: sent.planned,
                        mode: sent.mode,
                        retry: !sent.resent,
                        person: None,
                    });
                }
                Some(Err(error)) => {
                    eprintln!(
                        "[permission] could not {}, showing a card instead: {error}",
                        sent.planned.could_not
                    );
                    refused.push(sent.planned.permission_id.clone());
                    // A retry can find the first answer already taken before its resolution arrives.
                    // Keep that answer's evidence for the resolution, without ever retrying it again.
                    if sent.resent
                        && matches!(
                            error,
                            agent::ProviderError::Provider {
                                code: agent::ProviderErrorCode::PermissionAlreadyResolved,
                                ..
                            }
                        )
                    {
                        approvals.lost.0.push(LostAnswer {
                            planned: sent.planned,
                            mode: sent.mode,
                            retry: false,
                            person: None,
                        });
                    }
                }
            }
        }
        approvals.host_sends.0.extend(waiting);
        refused
    }

    /// Keeps what this drain's folded resolutions say about requests whose host answer is on its way or lost. A
    /// resync delivers no events, so it says nothing here: such an answer is then left as it was.
    fn note_resolutions_of_answers_out(approvals: &mut HumanApprovals, delivery: &RevisedDelivery) {
        let RevisedDelivery::Events(events) = delivery else {
            return;
        };
        if approvals.host_sends.0.is_empty() && approvals.lost.0.is_empty() {
            return;
        }
        for (_, event) in events {
            if let AgentDomainEvent::PermissionResolved { permission_id, outcome } = event {
                let tracked = approvals.host_sends.contains(permission_id)
                    || approvals.lost.ids().any(|lost| lost == permission_id);
                if tracked {
                    approvals.resolved_out.0.insert(permission_id.clone(), *outcome);
                }
            }
        }
    }

    /// A lost host answer the agent took after all: its reply never came back, but the agent resolved the request
    /// as the answer said. It is recorded as a taken answer is (the gate's answer, the id in `host_answered`, the
    /// approval it used up, its row notes and its `[permission]` line, which says the reply was lost), and is sent
    /// no more. A resolution matching the person's later answer is ambiguous and records nothing for the host;
    /// one matching only the host's answer records it. Another outcome leaves with its request.
    fn record_lost_answers_taken(
        approvals: &mut HumanApprovals,
        host_answered: &mut BTreeSet<String>,
        answered_for_you: &mut AnsweredForYou,
    ) {
        if approvals.lost.0.is_empty() || approvals.resolved_out.0.is_empty() {
            return;
        }
        for lost in std::mem::take(&mut approvals.lost.0) {
            let id = lost.planned.permission_id.clone();
            let Some(outcome) = approvals.resolved_out.0.get(&id).copied() else {
                approvals.lost.0.push(lost);
                continue;
            };
            if lost
                .person
                .as_ref()
                .is_some_and(|decision| taken_as_planned(decision, outcome))
                || !taken_as_planned(&lost.planned.decision, outcome)
            {
                approvals.lost.0.push(lost);
                continue;
            }
            approvals.resolved_out.0.remove(&id);
            let mut planned = lost.planned;
            planned.sent = format!(
                "{} (its reply was lost; the agent's resolution shows it was taken)",
                planned.sent
            );
            record_host_answer(planned, host_answered, approvals, answered_for_you);
        }
    }

    /// Sends each lost host answer once more, tracked like the first, at a drain that reads this tab's link live:
    /// never the drain that read the loss (that one delivers its card), and never while the link is down, where
    /// every send would fail at once. Each lost answer goes again on its own, whenever it was lost and whatever
    /// the link did meanwhile; an answer lost when no outage was seen (no reply in time) goes at the next drain.
    /// It goes only if it still stands: the request still waits, the person has not answered its card (any
    /// answer moves it to `fell_back` for good, see [`HumanApprovals::person_answers`]) and the tab is still in
    /// the mode it was planned under. Nor does it go while anything would keep the gate from answering automatically
    /// now: the session reported a policy it was not asked for, plans or changes mode (`plan_holds`, `plan_followed`
    /// as the drain's), the request is a plan card, its call began outside executing, or it cannot be placed. One
    /// that no longer stands stays the person's card.
    fn resend_lost_when_back(
        &mut self,
        approvals: &mut HumanApprovals,
        user_answered: &BTreeSet<String>,
        mode: PermissionMode,
        read_now: &BTreeSet<String>,
        plan: &PlanBook,
        plan_followed: bool,
    ) {
        if approvals.lost.0.is_empty() || self.link_state() != Some(agent::RemoteLinkState::Live) {
            return;
        }
        for lost in std::mem::take(&mut approvals.lost.0) {
            let LostAnswer {
                planned,
                mode: planned_under,
                mut retry,
                person,
            } = lost;
            // Its own statement, guard dropped before any answer below.
            let barred = {
                let projection = self.projection();
                session_tripped(&projection)
                    || projection.is_planning()
                    || plan_holds(&projection, plan_followed)
                    || plan.cards.contains(&planned.permission_id)
                    || projection.began_outside_executing(planned.call.as_deref())
                    || !projection.request_placed(&planned.permission_id)
            };
            if user_answered.contains(&planned.permission_id) || planned_under != mode || person.is_some() || barred {
                retry = false;
                approvals.fell_back.insert(planned.permission_id.clone());
            }
            if !retry || read_now.contains(&planned.permission_id) {
                approvals.lost.0.push(LostAnswer {
                    planned,
                    mode: planned_under,
                    retry,
                    person,
                });
                continue;
            }
            if approvals.host_sends.contains(&planned.permission_id) {
                continue;
            }
            // Its own statement, guard dropped before the answer locks the projection again.
            let pending = self
                .projection()
                .pending_permissions
                .contains_key(&planned.permission_id);
            if !pending {
                // Kept for a resolution still on its way; the drain's pruning drops it with its request.
                approvals.lost.0.push(LostAnswer {
                    planned,
                    mode: planned_under,
                    retry,
                    person,
                });
                continue;
            }
            match self.respond_permission_async(&planned.permission_id, planned.decision.clone()) {
                Ok(send) => approvals.host_sends.0.push(HostSend {
                    planned,
                    send,
                    mode: planned_under,
                    resent: true,
                }),
                Err(error) => {
                    eprintln!("[permission] could not {}: {}", planned.could_not, error.message);
                    approvals.fell_back.insert(planned.permission_id);
                }
            }
        }
    }

    /// Whether the session has reported a policy it was not asked for. The projection guard is a
    /// temporary of this call, so it is gone before the caller sends any answer.
    fn tripped(&self) -> bool {
        session_tripped(&self.projection())
    }

    /// Answers `allow` to each id in `ids` that is still pending on THIS session, in the order
    /// given (the caller sorts by `seq`); an id that is not pending is skipped silently -- resolved
    /// already, or never real. Returns the ids it actually answered, so the caller can extend
    /// `host_answered` with exactly those and no others (`Approved`). `why` is the log label ("on entering bypass" / "in bypass after a resync").
    ///
    /// Resolutions arrive through the pump, including the legacy backend's local hook replies.
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
    /// **Nothing is answered once the CLI has reported an ungated mode** -- the same
    /// rule `answer_what_needs_no_human` holds. `TabSet::pump`'s own tripwire check already keeps this
    /// from being reached on the Resync path, but `TabSet::confirm_bypass` calls this directly,
    /// off the 33ms pump, so a report the background ingestion has already folded into the projection
    /// but that pump has not yet turned into a `Failed` tab could otherwise still be approved here.
    /// The projection is read again before each answer, not once per batch: ingestion keeps folding
    /// while the earlier answers are sent, and a trip it folds there stops every answer after it.
    ///
    /// **The CLI's own prompts are answered here like any other request**, whatever they say about
    /// why they asked (one the user's own `permissions.ask` rule forced, one of a kind this build does
    /// not know, one that gives no reason): both callers are bypass, which lets everything through.
    /// Each one that names its call comes back in `Approved::prompts` with the row note a bypass
    /// tab's own answer gives it, for the caller to put on that call's row.
    ///
    /// **A remote tab's answers are tracked, not waited for** (`remote`, or any provider reporting a
    /// link): each joins `approvals`' host answers on their way, as an answer the gate chose does, and
    /// none is reported here; see [`Self::approve_pending_remote`].
    pub fn approve_pending(
        &mut self,
        ids: &[String],
        why: &str,
        remote: bool,
        approvals: &mut HumanApprovals,
    ) -> Approved {
        // Its own statement, guard dropped before any answer -- the same lock-order rule the
        // collection below follows.
        if self.tripped() {
            eprintln!("[permission] not answering {why}: the agent reported a policy it was not asked for");
            return Approved::default();
        }
        // Plan mode, or a move into or out of it: every card is a person's (`PlanBook::cards`, and every call that
        // began then, `began_outside_executing`), and a request answered on entering bypass now would be answered
        // under a mode the CLI is not in.
        if self.projection().is_planning() {
            eprintln!("[permission] not answering {why}: the session is in plan mode");
            return Approved::default();
        }
        if remote || self.link_state().is_some() {
            self.approve_pending_remote(ids, why, approvals);
            return Approved::default();
        }
        let external = self.agent().is_external();
        let pending: std::collections::BTreeMap<String, PendingAnswer> = {
            let projection = self.projection();
            ids.iter()
                .filter_map(|id| {
                    let p = projection.pending_permissions.get(id)?;
                    // A request for a call that began while the session planned or changed mode is a person's
                    // alone, whatever mode the tab is in now.
                    if projection.began_outside_executing(named_call(&p.tool_use_id).as_deref()) {
                        return None;
                    }
                    // Nor, on a session that offered plan mode, one that cannot be placed yet: its call has not been
                    // folded (the sidecar sends requests ahead of its message events), so where the CLI stood when
                    // it asked is not known. The gate judges it once it can be placed (`PlanBook::unsequenced`).
                    if !projection.request_placed(id) {
                        return None;
                    }
                    // Every external request is an escalation past the agent's own sandbox, so each
                    // one approved here gets its row note; a Claude gate request does not.
                    let prompt = if external {
                        named_call(&p.tool_use_id).map(|call| (call, EXTERNAL_BYPASS_NOTE.to_string()))
                    } else {
                        p.provider_prompt.as_ref().and_then(|prompt| {
                            named_call(&p.tool_use_id)
                                .map(|call| (call, format!("{} — allowed in bypass", prompt.label())))
                        })
                    };
                    Some((
                        id.clone(),
                        PendingAnswer {
                            tool_name: p.tool_name.clone(),
                            prompt,
                        },
                    ))
                })
                .collect()
        };
        approve_each(ids, &pending, why, |id| {
            // Its own statement, guard dropped before the answer locks the projection again.
            if self.tripped() {
                return Err(BackendError {
                    message: "the agent reported a policy it was not asked for".into(),
                    benign: true,
                });
            }
            self.respond_permission(id, PermissionDecision::Allow)
        })
    }

    /// [`Self::approve_pending`] for a remote tab, whose answers cross the network: each `allow` goes out
    /// without this thread waiting and joins the host answers on their way in `approvals`, with the row
    /// note a bypass answer gives a CLI prompt. While one is on its way no page draws its request, no
    /// card answer is taken for it and no later sweep answers it again. A later delivery reads its reply
    /// (`settle_host_sends`): a taken one is recorded as every host answer is, its note included, and a
    /// refused or lost one comes back as its card. One that could not be sent at all stays a card.
    fn approve_pending_remote(&mut self, ids: &[String], why: &str, approvals: &mut HumanApprovals) {
        let external = self.agent().is_external();
        let planned: Vec<PlannedAnswer> = {
            let projection = self.projection();
            ids.iter()
                .filter(|id| !approvals.host_sends.contains(id))
                .filter_map(|id| {
                    let p = projection.pending_permissions.get(id)?;
                    let call = named_call(&p.tool_use_id);
                    // As on a local tab: a request for a call that began while the session planned or changed
                    // mode is a person's alone, and one that cannot be placed yet is the gate's to judge later.
                    if projection.began_outside_executing(call.as_deref()) || !projection.request_placed(id) {
                        return None;
                    }
                    let note = if external {
                        Some(EXTERNAL_BYPASS_NOTE.to_string())
                    } else {
                        p.provider_prompt
                            .as_ref()
                            .map(|prompt| format!("{} — allowed in bypass", prompt.label()))
                    };
                    let prompt = call
                        .clone()
                        .zip(note)
                        .map(|(tool_use_id, note)| PromptAnsweredForYou { tool_use_id, note });
                    Some(PlannedAnswer {
                        permission_id: id.clone(),
                        decision: PermissionDecision::Allow,
                        origin: if p.provider_prompt.is_some() {
                            GateOrigin::ProviderPrompt
                        } else {
                            GateOrigin::GateRequest
                        },
                        call,
                        tool_name: p.tool_name.clone(),
                        consumes_approval: None,
                        prompt,
                        fast_path: None,
                        by_rule: None,
                        creates_file: None,
                        sent: format!("allowed {why}: {}", p.tool_name),
                        could_not: format!("allow {} {why}", p.tool_name),
                    })
                })
                .collect()
        };
        for planned in planned {
            // Its own statement, guard dropped before the answer locks the projection again.
            if self.tripped() {
                eprintln!("[permission] not answering {why}: the agent reported a policy it was not asked for");
                break;
            }
            match self.respond_permission_async(&planned.permission_id, PermissionDecision::Allow) {
                Ok(send) => approvals.host_sends.0.push(HostSend {
                    planned,
                    send,
                    mode: PermissionMode::Bypass,
                    resent: false,
                }),
                Err(error) => eprintln!("[permission] could not {}: {}", planned.could_not, error.message),
            }
        }
    }

    pub fn shutdown(&mut self) {
        match self {
            AgentBackend::Legacy(session) => session.shutdown(),
            AgentBackend::Conversation { conversation, .. } => conversation.shutdown(),
        }
    }

    /// Reservations and display-page installation share the conversation's ingest lock.
    pub fn history_ingest(&self) -> Option<&agent::ingestion::ConversationIngest> {
        match self {
            Self::Conversation { conversation, .. } => Some(conversation.history_ingest()),
            Self::Legacy(_) => None,
        }
    }

    /// A move begins: the conversation stops folding where its ingest stands
    /// (`AgentConversation::freeze_folding`), so the caller's last pump delivers everything there is.
    /// The legacy backend has no conversation to hand over and is refused.
    pub fn freeze_folding(&mut self) -> Result<(), agent::HandoffRefused> {
        match self {
            AgentBackend::Legacy(_) => Err(agent::HandoffRefused::Mismatch("a legacy session")),
            AgentBackend::Conversation { conversation, .. } => conversation.freeze_folding(),
        }
    }

    /// After the last pump: the records the move carries. Refused, and folding resumed, as
    /// `AgentConversation::take_frozen` says.
    pub fn take_frozen(
        &mut self,
    ) -> Result<(agent::ChatMoveCore, agent::ConversationHandoff, BackendHandoff), agent::HandoffRefused> {
        match self {
            AgentBackend::Legacy(_) => Err(agent::HandoffRefused::Mismatch("a legacy session")),
            AgentBackend::Conversation { agent, conversation } => {
                let (core, handoff) = conversation.take_frozen()?;
                Ok((core, handoff, BackendHandoff { agent: agent.clone() }))
            }
        }
    }

    /// The move did not happen: folding resumes from where the provider kept its events.
    pub fn unfreeze_folding(&mut self) {
        if let AgentBackend::Conversation { conversation, .. } = self {
            conversation.unfreeze();
        }
    }

    /// The backend of a chat another window froze for a move: its conversation rebuilt by folding
    /// (`AgentConversation::adopt_moved`), held for the agent the record names. An external agent's is
    /// checked as every external backend is ([`ExternalConversation::new`]).
    pub fn adopt_moved(
        core: agent::ChatMoveCore,
        conversation: agent::ConversationHandoff,
        backend: BackendHandoff,
        provider: agent::MovedProvider,
    ) -> Result<AgentBackend, BackendError> {
        let BackendHandoff { agent } = backend;
        let conversation = AgentConversation::adopt_moved(core, conversation, provider)
            .map_err(|refused| BackendError::fatal(format!("the moved chat could not be taken up: {refused}")))?;
        if agent.is_external() {
            return ExternalConversation::new(agent, conversation).map(AgentBackend::from);
        }
        if conversation.provider_name() != agent.record_name() {
            return Err(BackendError::fatal(format!(
                "a {} session cannot be held as a {} tab",
                conversation.provider_name(),
                agent.record_name()
            )));
        }
        Ok(AgentBackend::Conversation {
            agent,
            conversation: Box::new(conversation),
        })
    }
}

/// What [`AgentBackend::approve_pending`] answered: the successful ids and the row notes for
/// requests that name their calls. Permission resolutions reach the page through the pump.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Approved {
    pub ids: Vec<String>,
    pub prompts: Vec<PromptAnsweredForYou>,
}

/// One still-pending request `approve_pending` is about to answer: its tool (for the log) and, for a
/// request whose row says it was allowed (a CLI prompt, or any external agent's request) that names
/// its call, that call's tool-use id and the row note.
struct PendingAnswer {
    tool_name: String,
    prompt: Option<(String, String)>,
}

/// Whether the agent resolving a request with `outcome` is it taking the host's `decision`.
fn taken_as_planned(decision: &PermissionDecision, outcome: agent::PermissionOutcome) -> bool {
    matches!(
        (decision, outcome),
        (PermissionDecision::Allow, agent::PermissionOutcome::Allowed)
            | (PermissionDecision::Deny { .. }, agent::PermissionOutcome::Denied)
            | (PermissionDecision::Defer, agent::PermissionOutcome::Deferred)
    )
}

/// Records an answer the provider took: the gate answer, the id in `host_answered`, the approval it
/// used up and each row note it carries.
fn record_host_answer(
    planned: PlannedAnswer,
    host_answered: &mut BTreeSet<String>,
    approvals: &mut HumanApprovals,
    answered_for_you: &mut AnsweredForYou,
) {
    let PlannedAnswer {
        permission_id,
        decision,
        origin,
        call,
        tool_name,
        consumes_approval,
        prompt,
        fast_path,
        by_rule,
        creates_file,
        sent,
        could_not: _,
    } = planned;
    answered_for_you.gate_answers.push(GateAnswer {
        origin,
        permission_id: permission_id.clone(),
        tool_use_id: call,
        tool_name,
        deferred: decision.defers(),
    });
    host_answered.insert(permission_id);
    // Used up once it answered one prompt: a later prompt under this id is a card.
    if let Some(id) = consumes_approval {
        approvals.consume(&id);
    }
    answered_for_you.prompts.extend(prompt);
    answered_for_you.by_the_fast_path.extend(fast_path);
    answered_for_you.creates_file.extend(creates_file);
    answered_for_you.by_rule.extend(by_rule);
    // Said on stderr rather than silently: this is the only record anywhere of a tool call that ran
    // without the user being asked. One short line, whose reason is a fixed label rather than anything
    // the model wrote.
    eprintln!("[permission] {sent}");
}

/// Answers the still-pending ids in order. Keeping the answer injectable lets tests check
/// partial success without involving a CLI transport.
fn approve_each(
    ids: &[String],
    pending: &std::collections::BTreeMap<String, PendingAnswer>,
    why: &str,
    mut respond: impl FnMut(&str) -> Result<(), BackendError>,
) -> Approved {
    let mut approved = Approved::default();
    for id in ids {
        let Some(PendingAnswer { tool_name, prompt }) = pending.get(id) else {
            continue;
        };
        match respond(id) {
            Ok(()) => {
                eprintln!("[permission] allowed {why}: {tool_name}");
                approved.ids.push(id.clone());
                if let Some((call, note)) = prompt {
                    // Both callers are bypass, so the note is the one a bypass tab's own answer gives.
                    approved.prompts.push(PromptAnsweredForYou {
                        tool_use_id: call.clone(),
                        note: note.clone(),
                    });
                }
            }
            Err(error) => {
                eprintln!("[permission] could not allow {tool_name} {why}: {}", error.message);
            }
        }
    }
    approved
}

/// What one `answer_what_needs_no_human` call reads and writes besides the batch: bundled so the
/// signature stays readable. The fields retain the tab's permission answers and row notes.
/// The mode a tab's requests are answered in, and whether the tab has followed the CLI out of auto. A
/// tab that has not been told of a move yet is told apart from one a person put back in Auto after it.
#[derive(Debug, Clone, Copy)]
pub struct GateMode {
    pub mode: PermissionMode,
    pub followed_out_of_auto: bool,
    /// The tab has taken up the mode the end of its plan mode gave it (`TabSet::follow_plan_mode`): until
    /// it has, `mode` may still be the mode the plan was entered from, and nothing is answered
    /// automatically once the session executes again.
    pub plan_followed: bool,
}

impl From<PermissionMode> for GateMode {
    fn from(mode: PermissionMode) -> Self {
        Self {
            mode,
            followed_out_of_auto: false,
            plan_followed: true,
        }
    }
}

struct AnswerContext<'a> {
    rules: &'a agent::PrefixRules,
    mode: PermissionMode,
    followed_out_of_auto: bool,
    plan_followed: bool,
    host_answered: &'a mut BTreeSet<String>,
    approvals: &'a mut HumanApprovals,
    answered_for_you: &'a mut AnsweredForYou,
    /// Requests whose host answer failed to reach the agent: delivered as cards, never answered again.
    cards_only: &'a BTreeSet<String>,
    plan: &'a mut PlanBook,
}

/// What a tab keeps about its requests because of plan mode, read and written where requests are answered.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanBook {
    /// Requests that would have been answered automatically while the session was leaving plan mode, or
    /// before the tab took up the mode leaving gave it. Answered by nobody and drawn as no card until then;
    /// then each is judged once more under the tab's new mode, and a card is drawn for what needs one.
    #[serde(default)]
    pub held: BTreeSet<String>,
    /// Requests that became cards while the CLI ran plan mode. Only a person's answer on the card settles
    /// one: no sweep, no bypass and no later automatic answer allows it, whatever mode the tab is in by then.
    #[serde(default)]
    pub cards: BTreeSet<String>,
    /// The calls a request was deferred for while the CLI ran plan mode, until they complete, are refused,
    /// or their turn ends. An approval of a plan waits for these: the CLI is still deciding them in plan mode.
    #[serde(default)]
    pub deferred_in_flight: BTreeSet<String>,
    /// Requests on a session that offered plan mode whose call has not been folded yet, each with when it was
    /// first seen (milliseconds since the epoch). Answered by nobody and drawn as no card: until its call arrives
    /// the host cannot tell whether the CLI asked in plan mode. Each drain judges again those whose call has
    /// arrived, whose turn has ended, or that have waited [`PLAN_SEQUENCE_WAIT_MS`]. A move carries them, with
    /// their times, so the wait is not started again.
    #[serde(default)]
    pub unsequenced: std::collections::BTreeMap<String, u64>,
    /// Pending requests a drain has looked at: the gate judged them or put them among `unsequenced`, or a resync drew
    /// them. A request the ingestion folded after the last drain is in none of these, and an approval of a plan waits
    /// until it is (`approval_not_ready`): the CLI may have made it in plan mode and still be deciding it there. A move
    /// carries it, as the ids do not change.
    #[serde(default)]
    pub looked_at: BTreeSet<String>,
    /// The plan an approval made in this window waits on calls for (`TabSet::approve_plan`): while it does, no
    /// second approval of it is taken. This window's own bookkeeping, so a move does not carry it: the waiting
    /// approval stays with the window that holds it, which then finds the tab gone.
    #[serde(skip)]
    pub draining: Option<String>,
}

impl PlanBook {
    /// Drops every deferred call the projection shows finished, refused or out of its turn: what is left is
    /// still running in the CLI. A call not seen yet counts while a turn runs.
    pub fn prune_deferred(&mut self, projection: &agent::AgentSessionProjection) {
        if projection.active_turn_id.is_none() {
            self.deferred_in_flight.clear();
            return;
        }
        self.deferred_in_flight.retain(|id| {
            !projection
                .tool_calls
                .iter()
                .any(|call| call.tool_use_id == *id && (call.result.is_some() || call.denied.is_some()))
        });
    }

    /// Puts the request `permission_id` among those waiting for their call, from now, unless it already waits.
    pub fn wait_for_its_call(&mut self, permission_id: String) {
        self.unsequenced.entry(permission_id).or_insert_with(wall_clock_ms);
    }

    /// How long the request `permission_id` has waited for its call at `now_ms`, putting it among those waiting from
    /// then unless it already waits. A start later than `now_ms` (the wall clock stepped back) is moved to `now_ms`, so
    /// the wait starts again instead of lasting until the clock has caught up.
    fn waited_ms(&mut self, permission_id: &str, now_ms: u64) -> u64 {
        let first_ms = self.unsequenced.entry(permission_id.to_string()).or_insert(now_ms);
        if *first_ms > now_ms {
            *first_ms = now_ms;
        }
        now_ms - *first_ms
    }

    /// How long until the first request waiting for its call has waited [`PLAN_SEQUENCE_WAIT_MS`] and is judged
    /// without it: zero for one already past it, `None` when none waits, and never more than the wait itself (after
    /// the wall clock stepped back, the drain starts that request's wait again). Nothing else may wake the drain that
    /// judges it -- the CLI waits on the answer, and for a call the sidecar never shows nothing more arrives -- so the
    /// window runs its tick by then.
    pub fn next_sequence_release(&self) -> Option<std::time::Duration> {
        let now = wall_clock_ms();
        self.unsequenced
            .values()
            .map(|first_ms| {
                let left = (first_ms + PLAN_SEQUENCE_WAIT_MS).saturating_sub(now);
                std::time::Duration::from_millis(left.min(PLAN_SEQUENCE_WAIT_MS))
            })
            .min()
    }

    /// Makes the pending request `permission_id` a plan card, which only a person's answer settles. A card waits for
    /// nothing: it leaves the requests waiting for their call, which are drawn as no card, so a request is never
    /// both. It has been looked at.
    pub fn make_card(&mut self, permission_id: &str) {
        self.unsequenced.remove(permission_id);
        self.looked_at.insert(permission_id.to_string());
        self.cards.insert(permission_id.to_string());
    }

    /// Drops every id the projection no longer lists as pending.
    pub fn prune_answered(&mut self, still: &std::collections::HashSet<String>) {
        self.held.retain(|id| still.contains(id));
        self.cards.retain(|id| still.contains(id));
        self.unsequenced.retain(|id, _| still.contains(id));
        self.looked_at.retain(|id| still.contains(id));
    }
}

/// One `PermissionRequested`, as `decide` reads it.
#[derive(Clone, Copy)]
struct GateRequest<'a> {
    permission_id: &'a str,
    tool_use_id: &'a Option<String>,
    tool_name: &'a str,
    input: &'a serde_json::Value,
    provider_prompt: Option<&'a agent::ProviderPrompt>,
}

/// Whether the gate would take `project_root` for a boundary, with the home directory
/// `agent::classify_permission_request` reads, so a deferral, a card and an external agent's sandbox
/// never disagree about a root.
pub fn root_is_a_boundary(project_root: &Path) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    agent::project_boundary::refusal(
        project_root,
        home.as_deref(),
        &agent::project_boundary::Identity::current(),
    )
    .is_none()
}

/// Remote roots have no local boundary facts; only the local arm may inspect this machine.
pub fn gate_root_is_a_boundary(root: GateRoot<'_>) -> bool {
    match root {
        GateRoot::Local(path) => root_is_a_boundary(path),
        GateRoot::RemoteWithoutFacts => false,
    }
}

/// What `decide` judges a request by besides the request itself.
struct Judge<'a> {
    mode: PermissionMode,
    /// The session is an external agent's, whose requests no policy here judges.
    external: bool,
    /// An Auto tab on a session whose CLI runs its own auto mode, at a root that is a boundary
    /// (`answer_what_needs_no_human`).
    defers_to_the_cli: bool,
    /// The tools this session declared, while the CLI's latest report lists exactly one server of the
    /// tool server's name and that one is in process; otherwise nothing here is the session's own.
    own_tools: Option<&'a BTreeSet<String>>,
    rules: &'a agent::PrefixRules,
    /// Carries the same root facts as the batch; the judge never substitutes a laptop path.
    project_root: GateRoot<'a>,
    approvals: &'a HumanApprovals,
    /// Set when the request is judged as one the CLI made in plan mode (`RequestPhase::Planning`, and the plan
    /// half of an uncertain one): the plan table answers first, and every prompt of the CLI's own is a card.
    planning: Option<PlanGate>,
    /// Where Eitri reads its own configuration and code at start: a call that may write there is a card in every
    /// mode but Bypass, ahead of the plan table and the deferral to the CLI.
    places: &'a agent::EitriPlaces,
}

/// The CLI runs plan mode, entered from the tab's mode `origin`. `cli_runs_auto` is whether the session runs
/// the CLI's own auto mode when it executes (a `plan` report leaves that as it was).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanGate {
    pub origin: PermissionMode,
    pub cli_runs_auto: bool,
    /// The request's call is one of the main thread's (a call the projection holds at the top level, not a
    /// subagent's): only there was the CLI's plan mode measured to ask before writing.
    pub main_thread: bool,
}

/// What the plan table says of a gate request while the CLI runs plan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanVerdict {
    /// Left to the CLI's plan mode, which lets read-only tools through and asks about the rest.
    Defer,
    /// Judged as a tab of this mode judges it outside plan mode.
    AsMode(PermissionMode),
    /// A card, for a person alone to answer.
    Card,
}

/// The tools that write files or run commands. While the CLI runs plan mode the host never answers `allow`
/// for one: a gate request is left to the CLI only where its plan mode was seen to ask before acting
/// ([`PLAN_DEFERRED_TOOLS`]), and is a card everywhere else; every prompt the CLI raises is a card.
pub const PLAN_ASKED_TOOLS: [&str; 4] = ["Edit", "Write", "NotebookEdit", "Bash"];

/// Of [`PLAN_ASKED_TOOLS`], those a main-thread gate request for is left to the CLI in plan mode. Claude Code
/// 2.1.311 in plan mode writes its own plan file and asks the host before a `Write` anywhere else, and sends a
/// `Bash` that is not read-only to its classifier, which Verdandi turns into the CLI's own ask. `Edit` and
/// `NotebookEdit` were not seen doing the same, so they are cards.
pub const PLAN_DEFERRED_TOOLS: [&str; 2] = ["Write", "Bash"];

/// Whether `tool_use_id` names a call of the main thread: one the projection holds at the top level. A
/// subagent's calls are held under the call that started it, and a request with no id or for a call not seen
/// yet cannot be tied to the main thread.
fn on_the_main_thread(projection: &agent::AgentSessionProjection, tool_use_id: Option<&str>) -> bool {
    tool_use_id.is_some_and(|id| projection.tool_calls.iter().any(|call| call.tool_use_id == id))
}

/// How long a request on a session that offered plan mode waits for the call it gates before it is judged
/// without it. The sidecar sends hook requests ahead of its message events; in every recorded run (Claude Code
/// 2.1.311, sidecar 0.5.0: 136 requests whose call the sidecar showed, the c40 family and every other row) the
/// call's `ToolCallStarted` came 0.1 to 0.95 s after its request, 0.2 s at the median and 0.66 s at the 95th
/// percentile, and it kept coming while the request waited unanswered (2 s held, 0.5 s). Twice the longest leaves
/// room for a slower machine; only a call the sidecar never shows (a workflow's own calls) waits the whole time.
pub const PLAN_SEQUENCE_WAIT_MS: u64 = 2_000;

/// Milliseconds since the epoch; 0 on a clock before 1970.
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How a request is judged: as one the CLI made outside plan mode, in it, or in either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestPhase {
    Executing,
    Planning,
    Uncertain,
}

/// What the projection says about where the CLI stood for one request, read in one statement.
struct Sequence {
    /// The session offered plan mode: nothing else here changes a session without it.
    offered: bool,
    /// The request's call has been folded: never for a request that names none.
    seen: bool,
    /// Where the session stood when that call began, when it was anywhere but executing.
    mark: Option<agent::CallPlanMark>,
    /// Where the session stands now.
    now: Option<agent::CallPlanMark>,
    /// The request is still pending: one that is not has nothing to wait for, since whatever answers it goes nowhere.
    pending: bool,
}

impl Sequence {
    fn read(projection: &agent::AgentSessionProjection, permission_id: &str, call: Option<&str>) -> Sequence {
        Sequence {
            offered: projection.planning.offered,
            seen: call.is_some_and(|id| projection.call_seen(id)),
            mark: call.and_then(|id| projection.call_mark(id)),
            now: projection.plan_mark_now(),
            pending: projection.pending_permissions.contains_key(permission_id),
        }
    }

    /// Whether the request should wait before it is judged: while it is pending and its call has not been folded. Only
    /// that call's own start places it (`AgentSessionProjection::request_placed`), so a request that names no call
    /// waits the whole [`PLAN_SEQUENCE_WAIT_MS`], as one whose call never comes does, and nothing on the message path
    /// ends a wait early: a turn's end folded after the request may have been sent before it, and the CLI may then
    /// have started a turn of its own whose call the request is.
    fn waits(&self) -> bool {
        self.pending && !self.seen
    }

    /// How the request is judged: by where the CLI stood for its call, and where the session stands now. Plan
    /// mode at either is plan mode; a move into it at either, or plan mode now for a call never seen, is either.
    /// A session leaving plan mode whose CLI has reported its way out is executing here; nothing is answered
    /// automatically until the tab has taken up its new mode (`plan_holds`).
    ///
    /// The scripted host of layer (a) keeps a hand copy of this rule, which cannot depend on this crate
    /// (`agent::testkit::claude_scripted::gate_answer::judged_phase`); change both together.
    ///
    /// `ordered` is whether the request is one that waits to be placed (`waits_for_its_call` on a session that
    /// offered plan mode). Such a request is executing only when its own call was folded with no mark. Nothing else is
    /// evidence. A wait that ran out is none: the message path may simply lag, with a `plan` report still queued. Nor
    /// is any event folded after the request, since requests overtake the message events sent before them: another
    /// call's start, a subagent's or a turn's end folded after the request may have been sent before it, and before
    /// the `EnterPlanMode` and the `plan` report that came between. Such a request is judged in either mode, which
    /// gives a card wherever the two disagree.
    fn phase(&self, ordered: bool) -> RequestPhase {
        use agent::CallPlanMark::{Moving, Planning};
        match (self.seen.then_some(self.mark).flatten(), self.now) {
            (Some(Moving), _) | (_, Some(Moving)) => RequestPhase::Uncertain,
            (_, Some(Planning)) if !self.seen => RequestPhase::Uncertain,
            (Some(Planning), _) | (_, Some(Planning)) => RequestPhase::Planning,
            (None, None) if ordered && !self.seen => RequestPhase::Uncertain,
            (None, None) => RequestPhase::Executing,
        }
    }
}

/// Whether a request must wait for its call before it is judged: unless the plan table and the tab's mode give it
/// the same verdict whatever the call turns out to be. That holds for a session's own editor tools (answered
/// before the plan table, in every mode) and, in a Manual or Auto tab, for a gate request for a tool that neither
/// writes nor runs (the plan table judges it as the tab's mode does); everything else waits, every prompt of the
/// CLI's own and everything in a Bypass tab included. Layer (a)'s scripted host keeps a hand copy
/// (`agent::testkit::claude_scripted::gate_answer::waits_for_its_call`); change both together.
fn waits_for_its_call(
    tool_name: &str,
    provider_prompt: bool,
    mode: PermissionMode,
    own_tools: Option<&BTreeSet<String>>,
) -> bool {
    if is_own_tool(tool_name, own_tools) && !provider_prompt {
        return false;
    }
    provider_prompt || mode == PermissionMode::Bypass || PLAN_ASKED_TOOLS.contains(&tool_name)
}

/// The plan table for one request while the CLI runs plan mode. A session's own editor tools are answered
/// before it (`decide`), in every mode, as outside plan mode. `boundary` is whether the project root is a
/// boundary (`root_is_a_boundary`): at a root that is none nothing is deferred, as everywhere else.
pub fn plan_verdict(tool_name: &str, provider_prompt: bool, gate: PlanGate, boundary: bool) -> PlanVerdict {
    // Every prompt the CLI raises in plan mode, for whatever tool: no saved approval, no rule and no bypass
    // answers it.
    if provider_prompt {
        return PlanVerdict::Card;
    }
    if PLAN_ASKED_TOOLS.contains(&tool_name) {
        // A subagent may run another mode than the main thread's plan mode, and nobody measured what the CLI
        // does with the other edit tools: those fail toward a card.
        return if boundary && gate.main_thread && PLAN_DEFERRED_TOOLS.contains(&tool_name) {
            PlanVerdict::Defer
        } else {
            PlanVerdict::Card
        };
    }
    match gate.origin {
        PermissionMode::Manual => PlanVerdict::AsMode(PermissionMode::Manual),
        PermissionMode::Auto => PlanVerdict::AsMode(PermissionMode::Auto),
        // Bypass answers nothing itself in plan mode: what the CLI's own auto mode would decide is left to
        // it, and the rest is a card. No saved rule allows anything here.
        PermissionMode::Bypass if boundary && gate.cli_runs_auto => PlanVerdict::Defer,
        PermissionMode::Bypass => PlanVerdict::Card,
    }
}

/// Whether nothing is answered automatically now: the session is leaving plan mode, or executes again and
/// the tab has not yet taken up the mode that gave it (`GateMode::plan_followed`).
fn plan_holds(projection: &agent::AgentSessionProjection, plan_followed: bool) -> bool {
    matches!(projection.planning.phase, agent::PlanPhase::Leaving { .. })
        || (!projection.is_planning() && !plan_followed)
}

/// One host answer to one request, with everything its row says once it is sent: one `decide` chose,
/// which only `AgentBackend::answer_and_record` sends, or a remote tab's bypass approval, which
/// `approve_pending_remote` sends.
struct PlannedAnswer {
    permission_id: String,
    decision: PermissionDecision,
    origin: GateOrigin,
    /// The call the request gates, when it named one.
    call: Option<String>,
    tool_name: String,
    /// The user's approval this answer uses up.
    consumes_approval: Option<String>,
    prompt: Option<PromptAnsweredForYou>,
    fast_path: Option<FastPathAnsweredForYou>,
    by_rule: Option<RuleAnsweredForYou>,
    creates_file: Option<String>,
    /// The log line once the answer was sent.
    sent: String,
    /// What could not be done, for the log line of an answer that failed to send.
    could_not: String,
}

impl PlannedAnswer {
    /// An answer that carries no row note; the caller adds the ones it has.
    fn plain(
        request: &GateRequest<'_>,
        decision: PermissionDecision,
        origin: GateOrigin,
        sent: String,
        could_not: String,
    ) -> Self {
        PlannedAnswer {
            permission_id: request.permission_id.to_string(),
            decision,
            origin,
            call: named_call(request.tool_use_id),
            tool_name: request.tool_name.to_string(),
            consumes_approval: None,
            prompt: None,
            fast_path: None,
            by_rule: None,
            creates_file: None,
            sent,
            could_not,
        }
    }
}

/// Whether `tool_name` is a tool the session declared, under the tool server's name. The rest of the
/// name is looked up whole, never matched by prefix: a foreign server called `eitri__x` has tools named
/// `mcp__eitri__x__<tool>`, and the check on the name `eitri` says nothing about that server.
fn is_own_tool(tool_name: &str, own_tools: Option<&BTreeSet<String>>) -> bool {
    let Some(declared) = own_tools else {
        return false;
    };
    tool_name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(SERVER_NAME))
        .and_then(|rest| rest.strip_prefix("__"))
        .is_some_and(|tool| declared.contains(tool))
}

/// How the host answers one gate request without a card, or `None` when it is a card (the reason
/// logged here). The rules are `AgentBackend::answer_what_needs_no_human`'s; this only chooses, and
/// reads whatever must be read before an answer is sent (whether a `Write` creates its file: once
/// the answer is sent the CLI may already have written it).
fn decide(request: GateRequest<'_>, judge: Judge<'_>) -> Option<PlannedAnswer> {
    let Judge {
        mode,
        external,
        defers_to_the_cli,
        own_tools,
        rules,
        project_root,
        approvals,
        planning,
        places,
    } = judge;
    let GateRequest {
        tool_use_id,
        tool_name,
        input,
        provider_prompt,
        ..
    } = request;
    let call = named_call(tool_use_id);
    // GateRoot enters the decision here too: even bypass row notes must not inspect a laptop copy.
    let creates_file = || match project_root {
        GateRoot::Local(path) => call.clone().filter(|_| write_over_no_file(tool_name, input, path)),
        GateRoot::RemoteWithoutFacts => None,
    };
    if external {
        // An external agent's requests are judged by nobody here. In Auto its own reviewer has
        // already passed on what it let through, and in Manual nothing reviewed it, so what reaches
        // Eitri is the user's to answer; in Bypass every one is allowed (the requests its provider
        // declines itself never arrive). Eitri's classifier and the saved prefix rules are about
        // Claude's tools and are never consulted.
        if mode != PermissionMode::Bypass {
            eprintln!("[permission] asking the user: {tool_name} (the agent asked)");
            return None;
        }
        return Some(PlannedAnswer {
            // The agent asked because its sandbox would not let the call through; the row says this
            // host let it through, or the escalation would pass unseen.
            prompt: call.clone().map(|call| PromptAnsweredForYou {
                tool_use_id: call,
                note: EXTERNAL_BYPASS_NOTE.to_string(),
            }),
            ..PlannedAnswer::plain(
                &request,
                PermissionDecision::Allow,
                GateOrigin::GateRequest,
                format!("allowed in bypass: {tool_name}"),
                format!("allow {tool_name} in bypass"),
            )
        });
    }
    if planning.is_some() && provider_prompt.is_some() {
        eprintln!("[permission] asking the user: {tool_name} (the CLI asked in plan mode)");
        return None;
    }
    if let Some(prompt) = provider_prompt {
        // Never the classifier's or a rule's: the CLI asked after the gate had already allowed the
        // call. The CLI's sentence embeds the model's own path and a rule can come from a file in
        // the repository, so both are printed escaped (`{:?}`): they cannot forge or garble a log
        // line.
        let allow_because = match mode {
            // Every prompt, whatever it says about why it asked.
            PermissionMode::Bypass => Ok("in bypass"),
            // Manual treats the CLI's own prompts as Auto does: never allowed by a rule, never
            // deferred, allowed only for a call the user approved on its card.
            PermissionMode::Auto | PermissionMode::Manual if prompt.needs_its_own_card() => Err(prompt.label()),
            PermissionMode::Auto | PermissionMode::Manual => approvals
                .matches(tool_use_id.as_deref(), tool_name, input)
                .then_some("with your approval")
                .ok_or_else(|| prompt.reason.clone().unwrap_or_else(|| "no reason given".to_string())),
        };
        let because = match allow_because {
            Ok(because) => because,
            // The CLI's reason can quote the command, so it goes on the card and not in the log.
            Err(_) => {
                eprintln!("[permission] asking the user: {tool_name} (the CLI's own prompt)");
                return None;
            }
        };
        let label = prompt.label();
        return Some(PlannedAnswer {
            // Once per call: the approval that answered this prompt answers no other. In bypass
            // nothing was consulted, and an approval left for this call goes too.
            consumes_approval: tool_use_id.clone(),
            prompt: tool_use_id.clone().map(|id| PromptAnsweredForYou {
                tool_use_id: id,
                note: format!("{label} — allowed {because}"),
            }),
            ..PlannedAnswer::plain(
                &request,
                PermissionDecision::Allow,
                GateOrigin::ProviderPrompt,
                format!("allowed the CLI's own prompt {because}: {tool_name}"),
                format!("allow the CLI's own prompt for {tool_name}"),
            )
        });
    }
    if is_own_tool(tool_name, own_tools) {
        // Allowed at this gate so that the broker's table decides, the same in every mode: a card is
        // the broker's to raise, and neither a classifier here nor the CLI's sees the call.
        return Some(PlannedAnswer::plain(
            &request,
            PermissionDecision::Allow,
            GateOrigin::GateRequest,
            format!("left to the editor broker: {tool_name}"),
            format!("leave {tool_name} to the editor broker"),
        ));
    }
    // Ahead of the plan table, bypass and the deferral: a write into Eitri's own configuration takes effect at its
    // next start, so neither the CLI's classifier nor a saved rule answers it. Bypass outside plan mode allows it, as
    // it allows a write into the other protected paths.
    // In plan mode a Bypass tab answers nothing itself (`plan_verdict`), so the check holds there too.
    if let (true, GateRoot::Local(path)) = (mode != PermissionMode::Bypass || planning.is_some(), project_root) {
        if let Some(card) = agent::eitri_places_card(tool_name, input, path, places) {
            eprintln!("[permission] asking the user: {tool_name} ({})", card.reason);
            return None;
        }
    }
    let mode = match planning.map(|gate| plan_verdict(tool_name, false, gate, gate_root_is_a_boundary(project_root))) {
        None => mode,
        Some(PlanVerdict::AsMode(mode)) => mode,
        Some(PlanVerdict::Card) => {
            eprintln!("[permission] asking the user: {tool_name} (plan mode)");
            return None;
        }
        Some(PlanVerdict::Defer) => {
            return Some(PlannedAnswer::plain(
                &request,
                PermissionDecision::Defer,
                GateOrigin::GateRequest,
                format!("left to the CLI's plan mode: {tool_name}"),
                format!("leave to the CLI's plan mode {tool_name}"),
            ));
        }
    };
    if mode == PermissionMode::Bypass {
        return Some(PlannedAnswer {
            creates_file: creates_file(),
            ..PlannedAnswer::plain(
                &request,
                PermissionDecision::Allow,
                GateOrigin::GateRequest,
                format!("allowed in bypass: {tool_name}"),
                format!("allow {tool_name} in bypass"),
            )
        });
    }
    if let (true, GateRoot::Local(path)) = (defers_to_the_cli, project_root) {
        // A saved rule still answers what it answers today, and says so on the row; it is the only
        // allow this host gives an Auto call on such a session. Asked here rather than through
        // `classify_with_rules`, whose classifier half is exactly what is not consulted.
        let planned = match agent::rule_that_allows(tool_name, input, path, rules) {
            Some(rule) => PlannedAnswer {
                by_rule: call.clone().map(|id| RuleAnsweredForYou {
                    tool_use_id: id,
                    rule,
                    tool_name: tool_name.to_string(),
                    input: input.clone(),
                }),
                ..PlannedAnswer::plain(
                    &request,
                    PermissionDecision::Allow,
                    GateOrigin::GateRequest,
                    format!(
                        "allowed without asking: {tool_name} ({})",
                        agent::permission_policy::REASON_ALLOWED_BY_A_PROJECT_RULE
                    ),
                    format!("auto-answer {tool_name}"),
                )
            },
            None => PlannedAnswer::plain(
                &request,
                PermissionDecision::Defer,
                GateOrigin::GateRequest,
                format!("left to the CLI's auto mode: {tool_name}"),
                format!("leave to the CLI's auto mode {tool_name}"),
            ),
        };
        // A display fact about the call, not a grant.
        return Some(PlannedAnswer {
            creates_file: creates_file(),
            ..planned
        });
    }
    let classification = match project_root {
        GateRoot::RemoteWithoutFacts => {
            agent::classify_without_a_boundary(tool_name, input, rules, mode == PermissionMode::Manual)
        }
        GateRoot::Local(path) if mode == PermissionMode::Manual => {
            agent::classify_in_manual(tool_name, input, path, rules)
        }
        GateRoot::Local(path) => agent::classify_with_rules(tool_name, input, path, rules),
    };
    if classification.needs_a_human() {
        // One line per card, with the policy's own fixed reason, never anything the model wrote:
        // without it nothing records why a particular card was drawn, and a wall of cards in auto
        // has to be reconstructed by reading the classifier against the CLI's own tool list.
        eprintln!("[permission] asking the user: {tool_name} ({})", classification.reason);
        return None;
    }
    // The fast path is the only allow any of the three edit tools gets here
    // (`answer_what_needs_no_human`'s doc).
    let fast_path = call
        .clone()
        .filter(|_| mode == PermissionMode::Auto && ACCEPT_EDITS_TOOLS.contains(&tool_name))
        .map(|id| FastPathAnsweredForYou {
            tool_use_id: id,
            tool_name: tool_name.to_string(),
            input: input.clone(),
        });
    // `classify_with_rules` checks `agent::rule_that_allows` before anything else, so this reason
    // means a rule -- never the read-only classifier -- is why this call is being allowed. Ask it
    // again for the rule's own display string, the one thing the classification does not carry; a
    // request with no tool-use id is answered as ever and noted nowhere, the same as the fast path
    // leaves it.
    let rule = (classification.reason == agent::permission_policy::REASON_ALLOWED_BY_A_PROJECT_RULE)
        .then(|| match project_root {
            GateRoot::Local(path) => agent::rule_that_allows(tool_name, input, path, rules),
            GateRoot::RemoteWithoutFacts => None,
        })
        .flatten();
    let by_rule = call.clone().zip(rule).map(|(id, rule)| RuleAnsweredForYou {
        tool_use_id: id,
        rule,
        tool_name: tool_name.to_string(),
        input: input.clone(),
    });
    Some(PlannedAnswer {
        fast_path,
        by_rule,
        creates_file: creates_file(),
        ..PlannedAnswer::plain(
            &request,
            PermissionDecision::Allow,
            GateOrigin::GateRequest,
            format!("allowed without asking: {tool_name} ({})", classification.reason),
            format!("auto-answer {tool_name}"),
        )
    })
}

/// The calls the user approved on a card in one tab, each kept with the tool and the exact input the
/// card showed, so an approval answers only the very call the user saw. `TabSet::answer_card` records
/// one on an Approve that reached the provider; `matches` is the only test the Auto rule applies to
/// the CLI's own prompt, and `consume` uses an approval up once it answered one.
///
/// The input is kept whole rather than as a digest: the comparison is then exact (`Value` equality,
/// which ignores object key order) with no canonical form to get wrong and no collision to argue
/// about. Recorded approvals expire at turn end, on a resync the backend reports and with the backend;
/// a snapshot the host sends to show a released prompt keeps them. In-flight sends survive resync so
/// their matching prompts can wait for the acknowledgement without being approved. A prompt they
/// hide is kept in `held` until a drain reconsiders it, also once its send settles or its turn ends.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HumanApprovals {
    calls: std::collections::BTreeMap<String, (String, serde_json::Value)>,
    #[serde(skip)]
    in_flight: std::collections::BTreeMap<String, InFlightApproval>,
    #[serde(skip)]
    held: BTreeSet<String>,
    /// Answers this host chose for a remote tab, sent without waiting and recorded once the agent
    /// took them ([`AgentBackend::take_revised_ui_delivery_after_answers`]).
    #[serde(skip)]
    host_sends: HostSends,
    /// Requests whose host answer was refused, pruned on every drain to what the projection still lists:
    /// each is the person's card from then on. No bypass sweep answers one again, or the resync that
    /// shows it would send the same answer, meet the same refusal and show it again, as often as the
    /// tick runs.
    #[serde(skip)]
    fell_back: BTreeSet<String>,
    /// Host answers lost on their way to a remote agent (the connection broke, or no reply came in time),
    /// pruned to pending requests at a drain during which nothing was folded since the projection was read
    /// before its take (a resolution folded later is the next drain's to record). Each may be retried once; a person's card answer or another loss stops
    /// retries while retaining the plan to identify a later resolution.
    #[serde(skip)]
    lost: HostSendsLost,
    /// How the agent resolved requests whose host answer is on its way or lost, as its folded resolutions said:
    /// a resolution can be folded before its answer's reply is read. Pruned to the answers still tracked.
    #[serde(skip)]
    resolved_out: ResolvedWhileOut,
}

/// The agent's resolutions of requests whose host answer is still tracked. A copy carries none, as with
/// [`HostSends`].
#[derive(Default)]
pub(crate) struct ResolvedWhileOut(std::collections::BTreeMap<String, agent::PermissionOutcome>);

impl Clone for ResolvedWhileOut {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for ResolvedWhileOut {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for ResolvedWhileOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResolvedWhileOut({})", self.0.len())
    }
}

/// Host answers lost on their way, with what each was. A copy carries none, as with [`HostSends`].
#[derive(Default)]
pub(crate) struct HostSendsLost(Vec<LostAnswer>);

/// A host answer lost on its way, and the tab's mode it was planned under: it is sent again only in that mode.
struct LostAnswer {
    planned: PlannedAnswer,
    mode: PermissionMode,
    /// A second loss and a person's card answer stop retries, but retain evidence for a later resolution.
    retry: bool,
    person: Option<PermissionDecision>,
}

impl HostSendsLost {
    fn ids(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|lost| &lost.planned.permission_id)
    }
}

impl Clone for HostSendsLost {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for HostSendsLost {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for HostSendsLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostSendsLost({})", self.0.len())
    }
}

/// Host answers on their way to a remote tab's agent. A copy carries none: a send belongs to the
/// window that sent it, which reads its reply.
#[derive(Default)]
pub(crate) struct HostSends(Vec<HostSend>);

struct HostSend {
    planned: PlannedAnswer,
    send: agent::InteractionSend,
    /// The tab's mode the answer was planned under.
    mode: PermissionMode,
    /// Whether this is a lost answer's second send: lost again, it is not sent a third time.
    resent: bool,
}

impl HostSends {
    fn contains(&self, permission_id: &str) -> bool {
        self.0.iter().any(|sent| sent.planned.permission_id == permission_id)
    }
}

impl HumanApprovals {
    /// The requests this host answered for a remote tab whose replies have not been read: being
    /// answered, so no page draws them, the bypass sweep skips them and no card answer is taken.
    pub(crate) fn host_send_ids(&self) -> impl Iterator<Item = &String> {
        self.host_sends.0.iter().map(|sent| &sent.planned.permission_id)
    }

    /// Whether a host answer is still on its way; the panel's tick then keeps reading until its reply
    /// is read.
    pub(crate) fn has_host_sends(&self) -> bool {
        !self.host_sends.0.is_empty()
    }

    /// Whether the host's answer to `permission_id` was refused, or lost and not sent again yet, leaving
    /// its card to the person.
    pub(crate) fn fell_back(&self, permission_id: &str) -> bool {
        self.fell_back.contains(permission_id) || self.lost.ids().any(|id| id == permission_id)
    }

    /// The person answers `permission_id`'s card: a lost host answer to it is never sent again, whatever
    /// becomes of the person's answer (one that fails leaves the card the person's, not the host's).
    pub(crate) fn person_answers(&mut self, permission_id: &str, decision: PermissionDecision) {
        for lost in &mut self.lost.0 {
            if lost.planned.permission_id == permission_id {
                lost.retry = false;
                lost.person = Some(decision.clone());
                self.fell_back.insert(permission_id.to_string());
            }
        }
    }
}

impl Clone for HostSends {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for HostSends {
    /// In-flight sends are no part of what the approvals say.
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for HostSends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostSends({} in flight)", self.0.len())
    }
}

/// An approval whose acknowledgement is pending, by its card's permission id: the turn it was sent
/// in, and the call's tool-use id, tool and input.
type InFlightApproval = (Option<String>, String, String, serde_json::Value);

impl HumanApprovals {
    pub(crate) fn sending(
        &mut self,
        permission: &str,
        turn: Option<String>,
        call: &(String, String, serde_json::Value),
    ) {
        self.in_flight.insert(
            permission.into(),
            (turn, call.0.clone(), call.1.clone(), call.2.clone()),
        );
    }

    /// The send for `permission` settled. Every pending prompt it could be hiding is kept in `held`
    /// first, so the next drain reconsiders it and, when it is a card, shows it with a snapshot: a
    /// snapshot may have left it out before any drain saw it, and its event is then covered.
    /// `answered` are the ids the user answered on a card: their answer stands and they are never held.
    pub(crate) fn finished(
        &mut self,
        permission: &str,
        projection: Option<&agent::AgentSessionProjection>,
        answered: &BTreeSet<String>,
    ) {
        if let Some(sent) = self.in_flight.remove(permission) {
            if let Some(projection) = projection {
                self.keep_what_could_hide(&[(permission.to_string(), sent)], projection, answered);
            }
        }
    }

    /// Every in-flight send is forgotten (the turn ended); what any of them could be hiding is kept in
    /// `held` as [`Self::finished`] keeps it.
    pub(crate) fn forget_in_flight(&mut self, projection: &agent::AgentSessionProjection, answered: &BTreeSet<String>) {
        let sent: Vec<_> = std::mem::take(&mut self.in_flight).into_iter().collect();
        self.keep_what_could_hide(&sent, projection, answered);
    }

    /// The turn is not compared: a snapshot taken while the send's turn ran may have hidden the prompt
    /// before that turn ended.
    fn keep_what_could_hide(
        &mut self,
        sent: &[(String, InFlightApproval)],
        projection: &agent::AgentSessionProjection,
        answered: &BTreeSet<String>,
    ) {
        for (id, p) in &projection.pending_permissions {
            if !answered.contains(id)
                && Self::holdable(p)
                && sent
                    .iter()
                    .any(|(perm, (_, call, tool, input))| perm != id && Self::same_call(p, call, tool, input))
            {
                self.held.insert(id.clone());
            }
        }
    }

    /// Whether the in-flight approval of another card covers `p`, a CLI prompt that can wait for it.
    /// The card being answered never holds itself: its own send is no reason to hide it.
    pub(crate) fn holds(&self, p: &agent::PermissionRequestRecord, turn: &Option<String>) -> bool {
        Self::holdable(p)
            && self.in_flight.iter().any(|(perm, (sent_turn, call, tool, input))| {
                perm != &p.permission_id && sent_turn == turn && Self::same_call(p, call, tool, input)
            })
    }

    fn holdable(p: &agent::PermissionRequestRecord) -> bool {
        p.provider_prompt
            .as_ref()
            .is_some_and(|prompt| !prompt.needs_its_own_card())
    }

    fn same_call(p: &agent::PermissionRequestRecord, call: &str, tool: &str, input: &serde_json::Value) -> bool {
        p.tool_use_id.as_deref() == Some(call) && p.tool_name == tool && &p.input == input
    }

    pub(crate) fn held_ids(&self) -> impl Iterator<Item = &String> {
        self.held.iter()
    }

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

    pub(crate) fn forget_recorded(&mut self) {
        self.calls.clear();
    }

    /// Forgets approvals and in-flight matches, for tests only: production drops in-flight entries
    /// through `finished` and `forget_in_flight`, which first keep in `held` every prompt an entry
    /// could hide, so no hidden request is left pending without a drain to release it.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.calls.clear();
        self.in_flight.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// The row note of an external agent's request this host allowed because the tab is in bypass.
pub const EXTERNAL_BYPASS_NOTE: &str = "allowed in bypass";

/// A CLI prompt answered without a card, and the note its call's row shows, so the transcript says
/// who answered it: "Claude Code asked — allowed in bypass" / "— allowed with your
/// approval" (`ProviderPrompt::label` names whose question it was).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAnsweredForYou {
    pub tool_use_id: String,
    pub note: String,
}

/// A `Write`/`Edit`/`NotebookEdit` call whose gate the acceptEdits fast path answered `allow`
/// ("allowed by auto" on the row once the call completes), with the exact `(tool_name, input)` the
/// answer saw. Kept alongside the tool-use id so a LATER card for the exact same call -- the CLI's
/// own follow-up prompt,
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
/// ("allowed by rule `Bash(npm ci *)`" once the call completes) and the exact
/// `(tool_name, input)` the answer saw, kept for the same reason and matched the same way as
/// [`FastPathAnsweredForYou`] in `note_rule_answers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleAnsweredForYou {
    pub tool_use_id: String,
    pub rule: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// What one delivery answered without a card that a call's row says something about, each by
/// tool-use id, recorded by `answer_what_needs_no_human` at the moment it answered: the tab set
/// turns these into row notes and never infers them from a card's absence, which was also true of
/// a call that failed before any hook ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnsweredForYou {
    /// The CLI's own prompts, each with its note.
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
    /// Every request this host answered itself in the batch, whichever way (a deferral, an `allow`
    /// by rule, by the fast path, in bypass, or for the CLI's own prompt), recorded where the answer
    /// is sent. The projection forgets a request the moment its resolution folds, which on the
    /// sidecar can be before the next read, so a test that wants to know which calls the host
    /// answered cannot rebuild that from the projection; this is the one record that cannot miss one.
    pub gate_answers: Vec<GateAnswer>,
}

/// Which question the host answered: the `PreToolUse` gate's request, or the CLI's own permission
/// prompt that follows an allowed call (its sensitive-file check). The second is a separate
/// question the CLI asks after the gate has allowed the call, so it is no evidence that the gate
/// saw the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GateOrigin {
    GateRequest,
    ProviderPrompt,
}

/// One request the host answered itself: see [`AnsweredForYou::gate_answers`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GateAnswer {
    pub origin: GateOrigin,
    pub permission_id: String,
    /// The call the request gates, when it named one.
    pub tool_use_id: Option<String>,
    pub tool_name: String,
    /// `true` for a deferral to the CLI's own auto mode, `false` for an `allow`.
    pub deferred: bool,
}

/// The tools the acceptEdits fast path applies to -- `agent::permission_policy`'s own (private)
/// `EDIT_TOOLS`, restated here only to recognise its answer, never to decide one.
const ACCEPT_EDITS_TOOLS: [&str; 3] = ["Write", "Edit", "NotebookEdit"];

/// Whether the session has reported a policy other than the one Eitri asked for: the Claude CLI's
/// ungated permission mode, or any other agent's sandbox, approval policy or trust difference. Either
/// way the session is about to be closed and nothing is answered for it, bypass included.
fn session_tripped(projection: &agent::AgentSessionProjection) -> bool {
    projection.ungated_cli_mode.is_some() || projection.policy_mismatch.is_some()
}

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
/// current sidecar. A button that does nothing different is a worse lie than a missing one.
///
/// In `Shift+Tab`'s order, which is Claude Code's. The first is NOT the mode a new tab starts in:
/// that is Auto unless `init.lua` says otherwise (`agent_prefs::startup_mode`).
pub const CLIENT_IMPLEMENTED_PERMISSION_MODES: &[&str] = &["manual", "auto", "bypass"];

/// The modes a tab offers when its agent's own permission mode is set once, at the start of its
/// session, and cannot follow the tab's mode afterwards (an ACP agent). Manual would promise a
/// stricter agent than Eitri can make it, so it is not offered there; Auto and Bypass are Eitri's
/// own answers and need nothing of the agent.
pub const MODES_OF_AN_AGENT_FIXED_AT_START: &[&str] = &["auto", "bypass"];

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

/// One picker row's title: the CLI's own, then Eitri's own, then none -- each a real title from a
/// real source, so a row is recognisable without an invented label.
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

/// Where a greeting's Claude rows take the CLI's own titles from.
pub enum Titles<'a> {
    /// Each transcript read now, on the caller's thread (`title_for`).
    ReadNow,
    /// The window's cache, which answers from memory and reads on its own worker
    /// (`crate::transcript_titles`).
    Cached(&'a crate::transcript_titles::TitleCache),
}

/// Every resumable session of the project at `project_dir` that an agent other than Claude recorded,
/// newest first: what a restore matches the other agents' saved tabs with.
pub fn external_sessions(project_dir: &Path) -> Vec<ResumableSession> {
    project_dir
        .canonicalize()
        .map(|cwd| {
            agent::resumable_sessions(&agent::conversation_id_for_cwd(&cwd))
                .into_iter()
                .filter(|s| AgentId::from_record_name(&s.provider).is_some_and(|a| a.is_external()))
                .collect()
        })
        .unwrap_or_default()
}

/// `sessions` with the ones an agent lists as its own added, newest first: the session list a person already
/// uses, with the sessions the agent started in its own CLI beside the ones Eitri recorded. A listed session
/// Eitri already has a record of keeps the record's row (its name and its dates); the rest are rows with the
/// agent's title and its last-active time.
pub fn with_listed_sessions(
    agent: &AgentId,
    mut sessions: Vec<ResumableSession>,
    listed: Vec<agent::ListedSession>,
) -> Vec<ResumableSession> {
    let name = agent.record_name();
    for session in listed {
        if sessions
            .iter()
            .any(|s| s.provider == name && s.provider_session_id == session.session_id)
        {
            continue;
        }
        let stamp = session.updated_at_ms.map_or_else(String::new, |ms| ms.to_string());
        sessions.push(ResumableSession {
            provider: name.clone(),
            provider_session_id: session.session_id,
            created_at: stamp.clone(),
            updated_at: stamp,
            title: session.title,
            name: None,
        });
    }
    let rank = |s: &ResumableSession| s.updated_at.parse::<u128>().ok();
    sessions.sort_by_key(|s| std::cmp::Reverse(rank(s)));
    sessions
}

/// The sessions of `sessions` that `agent` itself recorded, in their order: what a tab of that agent
/// may offer to continue.
pub fn sessions_of(agent: &AgentId, sessions: Vec<ResumableSession>) -> Vec<ResumableSession> {
    let name = agent.record_name();
    sessions.into_iter().filter(|s| s.provider == name).collect()
}

/// What the frontend is told at handshake time, before any session exists.
pub struct BackendGreeting {
    pub kind: BackendKind,
    pub project_dir: PathBuf,
    pub permission_modes: &'static [&'static str],
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
    /// **Correction (2026-09-20): that transcript is no longer off limits for a
    /// title.** `for_kind` below layers the CLI's own `type:"ai-title"` line over `title` at
    /// DISPLAY time (`agent::transcript::newest_ai_title`), read fresh on every greeting and never
    /// written to disk -- see `agent::transcript`'s module doc, constraint 3, and
    /// `agent::ResumableSession`'s own doc, both corrected in the same commit. The sentence this
    /// correction sits under is otherwise unchanged, and its last clause is the load-bearing half:
    /// each level of the ladder is a real title from a real source, and a row with none still shows
    /// its id and its timestamps rather than an invented label.
    pub resumable: Vec<ResumableSession>,
    /// The configured Claude account's name (`agent::account::configured`), `null` when none is
    /// set. Read, never written, here.
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
    /// what `agent::persistence` already bounds (`MAX_RECORDS_PER_CONVERSATION` + 1, so 201 rather
    /// than 200 -- that doc explains the extra one). Measured at 1.74 ms hot and 5.92 ms cold over
    /// the measured corpus's 16 largest sessions; the window titles its rows through the cache instead, so none
    /// of this is on its thread.
    /// **It must stay a fixed-size read**: parsing those files whole would swap a bounded read of
    /// Eitri's own records for one that grows with a file this project neither writes nor knows a
    /// bound for, and it would do it here, on the loop that draws the editor. The window itself titles
    /// its rows through [`Titles::Cached`] ([`for_agent_titled`](Self::for_agent_titled)), which reads no
    /// transcript on its thread.
    pub fn for_kind(kind: BackendKind, project_dir: PathBuf) -> Self {
        Self::for_agent(kind, &AgentId::Claude, project_dir)
    }

    /// [`for_kind`](Self::for_kind) for a tab running `agent`: what it offers to continue is that
    /// agent's sessions only, so a Codex thread is never offered to a Claude tab or the other way
    /// round. An external agent's sessions need no Claude backend to resume, and their titles are
    /// Eitri's own (no Claude transcript to read one from).
    pub fn for_agent(kind: BackendKind, agent: &AgentId, project_dir: PathBuf) -> Self {
        Self::for_agent_titled(kind, agent, project_dir, Titles::ReadNow)
    }

    /// [`for_agent`](Self::for_agent), the Claude rows titled from `titles`.
    pub fn for_agent_titled(kind: BackendKind, agent: &AgentId, project_dir: PathBuf, titles: Titles<'_>) -> Self {
        if agent.is_external() {
            // Only this agent's records: the Claude records and their transcript titles are not read
            // at all, since nothing here would show them.
            let resumable = project_dir
                .canonicalize()
                .ok()
                .map(|cwd| sessions_of(agent, agent::resumable_sessions(&agent::conversation_id_for_cwd(&cwd))))
                .unwrap_or_default();
            return Self::with_resumable(kind, project_dir, resumable);
        }
        let resumable = Self::claude_sessions_titled(kind, &project_dir, titles);
        Self::with_resumable(kind, project_dir, resumable)
    }

    /// A remote tab never offers this machine's session records and reads none of them.
    pub fn for_agent_at_root(
        kind: BackendKind,
        agent: &AgentId,
        project_dir: PathBuf,
        titles: Titles<'_>,
        root: GateRoot<'_>,
    ) -> Self {
        match root {
            GateRoot::Local(_) => Self::for_agent_titled(kind, agent, project_dir, titles),
            GateRoot::RemoteWithoutFacts => Self::with_resumable(kind, project_dir, Vec::new()),
        }
    }

    /// The Claude sessions of the project at `project_dir` a `kind` backend can continue, titled,
    /// newest first: what a Claude tab offers, and what a restore matches saved Claude tabs with,
    /// whichever agent the active tab runs. The same bounded reads as [`for_kind`](Self::for_kind).
    pub fn claude_sessions(kind: BackendKind, project_dir: &Path) -> Vec<ResumableSession> {
        Self::claude_sessions_titled(kind, project_dir, Titles::ReadNow)
    }

    /// [`claude_sessions`](Self::claude_sessions), titled from `titles`.
    pub fn claude_sessions_titled(kind: BackendKind, project_dir: &Path, titles: Titles<'_>) -> Vec<ResumableSession> {
        // Keyed on the CANONICAL directory, matching what `AgentConversation` persists -- otherwise
        // `/x/proj` and `/x/../x/proj` would look up two different records for one workspace.
        //
        // Gated on the capability rather than on the backend's name: a backend this client cannot
        // drive a resume through offers nothing, however many records the directory holds. That is
        // the whole legacy case today, and it also means the lookup is skipped entirely there.
        if !client_implements_resume(kind) {
            return Vec::new();
        }
        project_dir
            .canonicalize()
            .ok()
            .map(|cwd| {
                let id = agent::conversation_id_for_cwd(&cwd);
                let cwd = cwd.to_string_lossy().into_owned();
                let sessions = sessions_of(&AgentId::Claude, agent::resumable_sessions(&id));
                match titles {
                    Titles::ReadNow => sessions.into_iter().map(|session| title_for(&cwd, session)).collect(),
                    Titles::Cached(cache) => {
                        let mut sessions = sessions;
                        cache.apply(&cwd, &mut sessions);
                        sessions
                    }
                }
            })
            .unwrap_or_default()
    }

    fn with_resumable(kind: BackendKind, project_dir: PathBuf, resumable: Vec<ResumableSession>) -> Self {
        let account = agent::account::configured().map(|a| a.name().to_string());
        Self {
            kind,
            project_dir,
            permission_modes: CLIENT_IMPLEMENTED_PERMISSION_MODES,
            resumable,
            account,
        }
    }
}

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
#[cfg(test)]
pub(crate) fn test_claude_projects_root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        // Under the agent crate's test state root, which the next run prunes once this process is gone.
        let root = agent::state_dirs::redirect_state_to_a_test_root().join("claude-config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("projects")).expect("creating the fake config root should succeed");
        std::env::set_var("CLAUDE_CONFIG_DIR", &root);
        root
    })
    .join("projects")
}

/// A hook a test sets on its own thread to run right after a drain took its events and before it reads the
/// projection again, so a fold can be made to land in that gap as an ingestion thread's can.
#[cfg(test)]
pub(crate) mod drain_test_hooks {
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce(&super::AgentBackend)>;

    thread_local! {
        static AFTER_TAKE: RefCell<Option<Hook>> = RefCell::new(None);
    }

    pub(crate) fn set_after_take(hook: impl FnOnce(&super::AgentBackend) + 'static) {
        AFTER_TAKE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn after_take(backend: &super::AgentBackend) {
        let hook = AFTER_TAKE.with(|h| h.borrow_mut().take());
        if let Some(hook) = hook {
            hook(backend);
        }
    }
}

#[cfg(test)]
mod tests {
    use agent::SidecarAvailability::{self, BuildsNamedCheckout, Missing, Runnable};

    /// Every answer `agent::sidecar_availability` can give affects only the startup line,
    /// never which backend is chosen.
    const ALL_AVAILABILITIES: [SidecarAvailability; 3] = [Runnable, BuildsNamedCheckout, Missing];

    /// An explicit backend selection wins over installed artifacts whenever that backend is compiled
    /// in. Someone selecting a backend is testing it; an installed sidecar must not overrule them.
    /// Selecting legacy without compiling it in must fail rather than silently select another backend.
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

    /// A typo selects the sidecar with a warning in every build. Falling back to the compile-gated
    /// legacy backend would silently run a different backend from the default.
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
    use agent::UiDelivery;

    #[test]
    fn backend_kind_round_trips_its_wire_name() {
        assert_eq!(BackendKind::Legacy.as_str(), "legacy");
        assert_eq!(BackendKind::Sidecar.as_str(), "sidecar");
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
    /// Every offered mode needs the backend's interactive gate. Both modes run the CLI gated;
    /// bypass means Eitri answers `allow`. The backend's `bypass_permission_mode` is a constant
    /// asserted beside `LEGACY_CAPABILITIES`; this test pins that every offered string is a mode.
    #[test]
    fn every_offered_mode_is_one_the_legacy_capabilities_actually_claim() {
        for mode in CLIENT_IMPLEMENTED_PERMISSION_MODES {
            let _: PermissionMode = match *mode {
                "manual" => PermissionMode::Manual,
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
        // The greeting offers bypass through the interactive gate: Eitri answers `allow`, while the
        // CLI stays gated. Both capabilities are asserted at compile time beside the constant.
        let modes: Vec<&str> = greeting.permission_modes.to_vec();
        assert_eq!(modes, vec!["manual", "auto", "bypass"]);
    }

    /// Modes are offered by what the agent can honour: an agent whose own mode is set once at the
    /// start (ACP) is offered no Manual, and a Manual default gives one of its tabs Auto.
    #[test]
    fn an_agent_whose_mode_is_fixed_at_start_offers_no_manual() {
        for agent in [AgentId::Claude, AgentId::Codex] {
            assert_eq!(agent.permission_modes(), &["manual", "auto", "bypass"], "{agent:?}");
            assert_eq!(agent.offered_mode(SessionModeChoice::Manual), SessionModeChoice::Manual);
        }
        let acp = AgentId::Acp("gem".into());
        assert_eq!(acp.permission_modes(), &["auto", "bypass"]);
        assert_eq!(acp.offered_mode(SessionModeChoice::Manual), SessionModeChoice::Auto);
        assert_eq!(acp.offered_mode(SessionModeChoice::Bypass), SessionModeChoice::Bypass);
        assert_eq!(acp.offered_mode(SessionModeChoice::Auto), SessionModeChoice::Auto);
    }

    /// Manual gives Codex's reviewer to the user, as Bypass does: every escalation reaches Eitri.
    #[test]
    fn a_manual_turn_hands_codex_reviews_to_the_user() {
        let at_a_boundary = |mode| turn_policy(mode, true);
        assert_eq!(at_a_boundary(SessionModeChoice::Manual).reviewer, Reviewer::User);
        assert_eq!(at_a_boundary(SessionModeChoice::Auto).reviewer, Reviewer::Agent);
        assert_eq!(at_a_boundary(SessionModeChoice::Bypass).reviewer, Reviewer::User);
        for mode in [
            SessionModeChoice::Manual,
            SessionModeChoice::Auto,
            SessionModeChoice::Bypass,
        ] {
            assert_eq!(at_a_boundary(mode).sandbox, Sandbox::Project);
            // Nobody but the user reviews where there is no boundary, in Auto too: a reviewer that approves
            // an escalation would be a way past the read-only sandbox with no card.
            assert_eq!(
                turn_policy(mode, false),
                TurnPolicy {
                    reviewer: Reviewer::User,
                    sandbox: Sandbox::ReadOnly,
                }
            );
        }
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

    // ---- The picker's title ladder: the CLI's title only when present, nothing of it persisted --

    fn a_claude_projects_root() -> PathBuf {
        super::test_claude_projects_root()
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

    /// The CLI's own title takes precedence over Eitri's title at display time. Existing sessions
    /// may already have a CLI title even when Eitri never stored one for them.
    #[test]
    fn the_clis_own_title_is_what_a_row_shows_when_the_transcript_has_one() {
        let row = the_only_row(a_workspace_with(
            Some("what eitri recorded"),
            Some("what the CLI recorded"),
        ));
        assert_eq!(row.title.as_deref(), Some("what the CLI recorded"));
    }

    /// **The negative control, and the whole promise of the fallback**: a record with no CLI title
    /// reads as it always has. The same record with no transcript beside it must read EXACTLY as a
    /// record did before the picker read transcripts at all: the
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

    /// **Nothing read from a transcript is ever persisted.** The record on disk is
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

    /// Every file under `dir`, path and bytes, sorted. The bytes, not a parsed record: "nothing read
    /// from a transcript is persisted" is about what is written, and a comparison of parsed values
    /// would pass over a rewrite that only changed formatting or a field this crate does not read.
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
            Some("claude-abc"),
            ProjectTrust::Untrusted,
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
    /// A Claude conversation's use of this helper (that `send_turn` folds `as_typed`,
    /// not `wire_text`) is exercised by
    /// `a_rejected_sidecar_turn_still_leaves_the_prompt_recorded` below, which builds a real
    /// `AgentBackend::Conversation` and calls `send_turn` on it.
    ///
    /// Legacy send and refused-send delivery are exercised by the transport-free session tests
    /// in `tab_set`, including the distinction between typed text and wire text.
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
                edit_request: None,
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

    // A rejected send still leaves the prompt recorded.

    /// A minimal `agent::AgentProvider` built only to make `AgentConversation::create` succeed and
    /// its `send_turn` genuinely fail -- the public equivalent of `agent::conversation`'s own
    /// private `FakeProvider` (its `send_turn_error` field does exactly this), rewritten here
    /// against the PUBLIC trait because that private double belongs to `agent` and is not
    /// exported. Nothing in `agent` was widened to make this possible: `AgentProvider` and every
    /// request/error type it uses were already `pub` at the crate root.
    struct RejectingProvider(agent::ProviderErrorCode);

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
        /// The one rigged method: it refuses with the code the provider was built with.
        fn send_turn(&self, _request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
            Err(agent::ProviderError::Provider {
                code: self.0,
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

    /// Folding the prompt before a send keeps the typed words in the projection when the provider
    /// refuses the turn, so live delivery and later snapshots describe the same conversation.
    #[test]
    fn a_rejected_sidecar_turn_still_leaves_the_prompt_recorded() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("rejected-turn-keeps-prompt");
        let conversation = AgentConversation::create(
            std::sync::Arc::new(RejectingProvider(agent::ProviderErrorCode::IdempotencyConflict)),
            &dir,
            ProjectTrust::Untrusted,
        )
        .expect("create_session succeeds on RejectingProvider; only send_turn is rigged to fail");
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

        let result = backend.send_turn(
            "wire text with context composed in",
            "what does this do?",
            agent::PromptOrigin::Composed,
            Vec::new(),
        );

        let error = result.expect_err("RejectingProvider's send_turn always fails");
        assert!(!error.is_turn_busy(), "any other refusal stays an error");
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
    }

    /// A turn refused because one is open was never put into any turn: its recorded prompt is taken
    /// back, and the refusal is told apart so the tab can hold the message instead of showing an error.
    #[test]
    fn a_send_refused_as_busy_takes_its_prompt_back() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("busy-turn-withdraws-prompt");
        let conversation = AgentConversation::create(
            std::sync::Arc::new(RejectingProvider(agent::ProviderErrorCode::TurnAlreadyActive)),
            &dir,
            ProjectTrust::Untrusted,
        )
        .expect("create_session succeeds on RejectingProvider; only send_turn is rigged to fail");
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

        let error = backend
            .send_turn("wire text", "and the docs", agent::PromptOrigin::Composed, Vec::new())
            .expect_err("the provider refuses it as busy");

        assert!(error.is_turn_busy());
        assert!(
            backend.projection().user_prompts.is_empty(),
            "no row for a message no turn read"
        );
    }

    /// The resume picker's title is the prompt AS TYPED. `wire_text` carries the editor context
    /// above the user's words, and its first line would name every session after a file path.
    #[test]
    fn a_sidecar_sessions_title_is_the_prompt_as_typed_not_the_wire_text() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("title-as-typed");
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let conversation_id = backend
            .conversation_id()
            .expect("a sidecar backend has one")
            .to_string();

        let sent = backend.send_turn(
            "Editor context: /p/src/main.rs, line 3\n\nfix the picker",
            "fix the picker",
            agent::PromptOrigin::Composed,
            Vec::new(),
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

    /// A rename before session creation is included when adoption writes the record.
    /// A later rename is persisted by the ingestion thread.
    #[test]
    fn a_rename_reaches_the_record_before_and_after_adoption() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("name-before-adoption");
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
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
        // Whatever the umask: a root others can write to is no boundary.
        chmod(&dir, 0o700);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    /// Samples twenty UI turns, each after ingestion has folded the provider's queued events.
    /// A later empty provider pump proves a preceding batch has left the folding gate.
    fn pump_until_delivery(
        provider: &RecordingProvider,
        backend: &mut AgentBackend,
        project_root: &Path,
        mode: PermissionMode,
    ) -> Vec<AgentDomainEvent> {
        let mut delivered = Vec::new();
        for _ in 0..20 {
            provider.wait_until_folded();
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(project_root),
                    &agent::PrefixRules::default(),
                    mode,
                    &mut BTreeSet::new(),
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
                UiDelivery::Events(events) if !events.is_empty() => {
                    delivered.extend(events);
                }
                _ => {}
            }
        }
        delivered
    }

    /// An in-project Read must produce no card and actually be answered. Merely hiding its
    /// request would leave the model waiting forever.
    #[test]
    fn a_read_inside_the_project_is_answered_here_and_never_becomes_a_card() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

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

        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
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
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // still cards under the fast path
            provider_prompt: None,
        });

        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
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
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-escape".into(),
            tool_use_id: Some("tool-3".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "/etc/hostname" }),
            provider_prompt: None,
        });

        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        assert!(
            delivered
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
            "a read outside the project must reach the user: {delivered:?}"
        );
        assert!(provider.resolutions().is_empty());
        backend.shutdown();
    }

    /// A saved rule reaches the common backend answer path, while a compound command with a
    /// matching first word still reaches the user.
    #[test]
    fn a_project_rule_answers_a_bash_call_and_never_a_compound_one() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
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
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &rules,
                    PermissionMode::Auto,
                    &mut host_answered,
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
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

    /// A hidden conversation still answers auto-allowed requests and counts its one waiting card.
    /// Hiding the chat does not stop its 33ms pump, so the tracker is told `on_screen = false`.
    /// The projection holds both requests, including the answered `Read`, until the provider's
    /// resolution returns; attention counts the one card delivered to the panel.
    #[test]
    fn a_hidden_chat_counts_the_card_it_holds_and_still_answers_what_needs_no_human() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
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
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // still cards under the fast path
            provider_prompt: None,
        });
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
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
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        tracker.observe(&delivered, false);
        tracker.retain_pending(|id| backend.projection().pending_permissions.contains_key(id));
        assert_eq!(tracker.attention().pending, 0);
        backend.shutdown();
    }

    #[test]
    fn ordering_errors_are_benign_and_broken_pipes_are_fatal() {
        let benign = BackendError::from(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a turn is already in progress",
        ));
        assert!(benign.benign, "an ordering refusal leaves the session alive");
        let fatal = BackendError::from(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"));
        assert!(!fatal.benign, "a dead process ends the session");
    }

    // Nothing is answered for a CLI that reported an ungated mode.

    /// A `Read` inside the project is what the classifier answers `allow` on its own (the test above).
    /// Once the CLI has reported an ungated mode, not even that: the session is about to be closed,
    /// and every request is left for the close to deny -- the one queued behind the report in the
    /// same batch, and one arriving in a later batch (which only the projection's record can catch,
    /// the event having gone by).
    #[test]
    fn after_an_ungated_report_nothing_is_answered_automatically() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
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
            if let UiDelivery::Events(events) = backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &agent::PrefixRules::default(),
                    PermissionMode::Auto,
                    &mut BTreeSet::new(),
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
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
        let later = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        assert!(
            later.iter().any(
                |e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "later")
            ),
            "{later:?}"
        );
        assert!(provider.resolutions().is_empty(), "nor one in a later batch");
        backend.shutdown();
    }

    /// An ungated-mode report in the same batch as a bypass choice stops automatic approval.
    /// That applies to both gate requests and CLI prompts, even though bypass otherwise approves both.
    #[test]
    fn a_tripwire_in_bypass_answers_nothing_either() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
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
        provider.queue(provider_prompt(
            "prompt-behind-it",
            Some("tu-1"),
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        let mut delivered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while delivered
            .iter()
            .filter(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
            .count()
            < 2
        {
            assert!(std::time::Instant::now() < deadline, "timed out: {delivered:?}");
            if let UiDelivery::Events(events) = backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &agent::PrefixRules::default(),
                    PermissionMode::Bypass,
                    &mut BTreeSet::new(),
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
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

    /// A `PolicyMismatch` (another agent's sandbox, approval policy or trust differed from what Eitri
    /// asked for) outranks bypass exactly as the Claude CLI's ungated mode does: not the request
    /// queued behind it in the same batch, not the CLI's own prompt, not one in a later batch (which
    /// only the projection's record can catch), and not the direct `approve_pending` call.
    #[test]
    fn policy_mismatch_stops_bypass_answers() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let read = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        };

        provider.queue(AgentDomainEvent::PolicyMismatch {
            requested: "sandbox workspaceWrite".into(),
            reported: "sandbox dangerFullAccess".into(),
        });
        provider.queue(read("behind-it"));
        provider.queue(provider_prompt(
            "prompt-behind-it",
            Some("tu-1"),
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        let mut delivered = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while delivered
            .iter()
            .filter(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
            .count()
            < 2
        {
            assert!(std::time::Instant::now() < deadline, "timed out: {delivered:?}");
            if let UiDelivery::Events(events) = backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &agent::PrefixRules::default(),
                    PermissionMode::Bypass,
                    &mut BTreeSet::new(),
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
                delivered.extend(events);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            provider.resolutions().is_empty(),
            "bypass must not allow a request behind the mismatch: {:?}",
            provider.resolutions()
        );

        provider.queue(read("later"));
        let later = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Bypass);
        assert!(later.iter().any(
            |e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "later")
        ));
        assert!(provider.resolutions().is_empty(), "nor one in a later batch");

        let answered = backend.approve_pending(
            &["behind-it".to_string()],
            "on entering bypass",
            false,
            &mut HumanApprovals::default(),
        );
        assert_eq!(answered, Approved::default());
        assert!(provider.resolutions().is_empty(), "nor a direct approval");
        backend.shutdown();
    }

    /// The mismatch withholds the automatic answers of Auto too, the classifier's `allow` of an
    /// in-project `Read` included.
    #[test]
    fn policy_mismatch_withholds_the_automatic_answers_in_auto() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        provider.queue(AgentDomainEvent::PolicyMismatch {
            requested: "approvalPolicy on-request".into(),
            reported: "approvalPolicy never".into(),
        });
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "behind-it".into(),
            tool_use_id: None,
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        });
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        assert!(delivered
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })));
        assert!(provider.resolutions().is_empty(), "{:?}", provider.resolutions());
        backend.shutdown();
    }

    // Bypass answers every request through `approve_pending`.

    /// Bypass answers in-root writes and out-of-root reads without a card or classifier path
    /// checks. The tool call itself must still reach the transcript.
    #[test]
    fn in_bypass_every_request_is_answered_and_never_delivered() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
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
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &rules,
                    PermissionMode::Bypass,
                    &mut host_answered,
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
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
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
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
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // still cards under the fast path
            provider_prompt: None,
        });

        let mut host_answered = BTreeSet::new();
        let mut delivered = false;
        for _ in 0..20 {
            provider.wait_until_folded();
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &rules,
                    PermissionMode::Auto,
                    &mut host_answered,
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
                UiDelivery::Events(_) => {
                    delivered = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(delivered, "nothing was delivered within twenty UI samples");
        assert_eq!(host_answered, BTreeSet::from(["perm-read".to_string()]));
        backend.shutdown();
    }

    /// The same gate request can reach the host twice (a replayed batch, a resync); in auto, as in
    /// bypass, the classifier answers it once. A second `allow` for an id already allowed is at best
    /// refused by the provider and at worst a second grant nobody decided on, and the copy is no card
    /// either: the request it repeats was answered.
    #[test]
    fn a_duplicate_gate_request_in_auto_is_answered_once() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let rules = agent::PrefixRules::default();
        let read = AgentDomainEvent::PermissionRequested {
            permission_id: "perm-read".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({ "file_path": "main.rs" }),
            provider_prompt: None,
        };
        // One lock acquisition, so both copies normally fold in the same batch; a split across two
        // deliveries is covered too, since the answered ids are kept across pumps as the tab set keeps them.
        provider.queue_all(vec![read.clone(), read]);

        let mut host_answered = BTreeSet::new();
        let mut delivered = Vec::new();
        for _ in 0..20 {
            provider.wait_until_folded();
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &rules,
                    PermissionMode::Auto,
                    &mut host_answered,
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
                UiDelivery::Events(events) if !events.is_empty() => {
                    delivered.extend(events);
                }
                _ => {}
            }
        }

        assert_eq!(
            provider.resolutions(),
            vec![("perm-read".to_string(), true)],
            "a request delivered twice must reach the provider with one answer"
        );
        assert!(
            !delivered
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
            "the copy of an answered request must not become a card: {delivered:?}"
        );
        assert_eq!(host_answered, BTreeSet::from(["perm-read".to_string()]));
        backend.shutdown();
    }

    /// `approve_pending` walks its ids in the order given, skips one no longer pending (already
    /// resolved, or never real) silently, and returns exactly what it answered.
    #[test]
    fn approve_pending_skips_ids_no_longer_pending_and_returns_what_it_answered() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let write = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // still cards under the fast path
            provider_prompt: None,
        };

        provider.queue(write("a"));
        provider.queue(write("b"));
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 2, "both must card in Auto: {delivered:?}");

        provider.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "b".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backend.projection().pending_permissions.contains_key("b") {
            assert!(std::time::Instant::now() < deadline, "b never resolved");
            backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &agent::PrefixRules::default(),
                    PermissionMode::Auto,
                    &mut BTreeSet::new(),
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            backend.projection().pending_permissions.contains_key("a"),
            "the premise: a is still pending, b is not"
        );

        let ids = vec!["a".to_string(), "b".to_string(), "zzz".to_string()];
        let answered = backend.approve_pending(&ids, "test", false, &mut HumanApprovals::default());
        assert_eq!(answered.ids, vec!["a".to_string()]);
        assert_eq!(provider.resolutions(), vec![("a".to_string(), true)]);
        backend.shutdown();
    }

    /// A refused answer leaves its card pending while successful answers retain their input order.
    #[test]
    fn approve_pending_counts_successful_answers_in_order() {
        let pending: std::collections::BTreeMap<String, PendingAnswer> =
            [("p1", "Write"), ("p2", "Bash"), ("p3", "Edit")]
                .into_iter()
                .map(|(id, tool)| {
                    (
                        id.to_string(),
                        PendingAnswer {
                            tool_name: tool.to_string(),
                            prompt: None,
                        },
                    )
                })
                .collect();
        let ids: Vec<String> = ["p1", "gone", "p2", "p3"].iter().map(ToString::to_string).collect();
        let mut asked = Vec::new();
        let approved = approve_each(&ids, &pending, "test", |id| {
            asked.push(id.to_string());
            if id == "p2" {
                // Fails toward a card: not answered, not hidden, no event.
                return Err(BackendError {
                    message: "the provider refused the answer".into(),
                    benign: true,
                });
            }
            Ok(())
        });
        assert_eq!(
            asked,
            vec!["p1", "p2", "p3"],
            "an id that is no longer pending is never answered"
        );
        assert_eq!(approved.ids, vec!["p1".to_string(), "p3".to_string()]);
    }

    /// `TabSet::confirm_bypass` calls `approve_pending` outside the 33ms pump. It must refuse answers
    /// once ingestion has folded an `UngatedCliMode`, even before a pump marks the tab failed.
    #[test]
    fn approve_pending_answers_nothing_once_the_cli_reported_an_ungated_mode() {
        let dir = a_workspace_holding_one_file();
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let write = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // still cards under the fast path
            provider_prompt: None,
        };

        provider.queue(write("still-pending"));
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
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

        let answered = backend.approve_pending(
            &["still-pending".to_string()],
            "on entering bypass",
            false,
            &mut HumanApprovals::default(),
        );
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
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
            provider_prompt: None,
        });
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Bypass);
        assert!(
            delivered.iter().any(
                |e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "perm-write")
            ),
            "a failed allow in bypass must fall back to a card: {delivered:?}"
        );
        assert!(provider.resolutions().is_empty());
        backend.shutdown();
    }

    // ---- The CLI's own permission prompts --------------------------------------

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

    /// One delivery through the CLI-prompt entry point, with the tab's human approvals; returns what was
    /// delivered and the CLI prompts answered without a card (for the row notes).
    fn deliver(
        provider: &crate::test_providers::RecordingProvider,
        backend: &mut AgentBackend,
        dir: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
    ) -> (Vec<AgentDomainEvent>, Vec<PromptAnsweredForYou>) {
        // Sample repeated UI turns only after each queued batch has finished folding.
        let mut answered = AnsweredForYou::default();
        let mut delivered = Vec::new();
        for _ in 0..20 {
            provider.wait_until_folded();
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(dir),
                    rules,
                    mode,
                    host_answered,
                    approvals,
                    &mut answered,
                )
                .without_revisions()
            {
                UiDelivery::Events(events) if !events.is_empty() => {
                    delivered.extend(events);
                }
                _ => {}
            }
        }
        (delivered, answered.prompts)
    }

    #[test]
    fn leader_k_on_an_idle_claude_tab_sends_one_stop_task_per_live_task_and_keeps_the_session() {
        let dir = agent::state_dirs::test_workspace_dir("claude-task-control");
        let provider = std::sync::Arc::new(RecordingProvider::task_controlling());
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let task = |id: &str, kind, ambient| agent::BackgroundTask {
            task_id: id.into(),
            run_id: None,
            kind,
            task_type: "test".into(),
            description: id.into(),
            subagent_type: None,
            parent_task_id: None,
            tool_use_id: None,
            ambient,
            workflow_name: None,
        };
        provider.queue(AgentDomainEvent::BackgroundTasksChanged {
            tasks: vec![task("ended", agent::TaskKind::Shell, false)],
            occurred_at: None,
        });
        provider.queue(AgentDomainEvent::BackgroundTasksChanged {
            tasks: vec![
                task("shell", agent::TaskKind::Shell, false),
                task("workflow", agent::TaskKind::Workflow, false),
                task("agent", agent::TaskKind::LocalAgent, false),
                task("ambient", agent::TaskKind::Monitor, true),
            ],
            occurred_at: None,
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while backend.projection().background_calls != 3 {
            assert!(std::time::Instant::now() < deadline, "roster never folded");
            std::thread::yield_now();
        }
        backend.stop_background(3).unwrap();
        assert_eq!(provider.stopped_tasks(), ["shell", "workflow", "agent"]);
        assert_eq!(provider.closes(), 0);
        assert!(backend.projection().active_turn_id.is_none());
        assert!(matches!(
            backend.projection().status,
            agent::ProjectionStatus::Running | agent::ProjectionStatus::Starting
        ));
        backend.shutdown();
    }

    fn one_tool(name: &str) -> agent::ClientToolSet {
        agent::ClientToolSet {
            server_name: "eitri".into(),
            timeout_ms: 900_000,
            tools: vec![agent::ClientToolDecl {
                name: name.into(),
                description: "a tool".into(),
                input_schema_json: r#"{"type":"object"}"#.into(),
                always_load: true,
            }],
        }
    }

    fn sidecar_backend_with(provider: std::sync::Arc<RecordingProvider>, dir: &Path) -> AgentBackend {
        let conversation =
            AgentConversation::create_with_tools(provider, dir, ProjectTrust::Untrusted, Some(one_tool("open_file")))
                .unwrap();
        AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        }
    }

    /// The destination's gate reads this set to know the session's own tools, so a record that lost it
    /// would send them to the classifier. The record crosses processes as JSON.
    #[test]
    fn the_declared_tools_survive_a_move() {
        let dir = agent::state_dirs::test_workspace_dir("client-tools-move");
        let provider = std::sync::Arc::new(RecordingProvider::client_tooling());
        let mut source = sidecar_backend_with(provider.clone(), &dir);
        provider.queue(AgentDomainEvent::SessionOpened {
            session_id: "fake-session".into(),
            provider_session_id: "claude-client-tools-move".into(),
            model: "stand-in".into(),
            cwd: dir.to_string_lossy().into_owned(),
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while source.projection().session_id.is_none() {
            assert!(std::time::Instant::now() < until, "the session never opened");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let (core, handoff, backend) = loop {
            assert!(std::time::Instant::now() < until, "the chat never came to rest");
            match source.freeze_folding() {
                Ok(()) => {}
                Err(agent::HandoffRefused::Folding) => continue,
                Err(other) => panic!("the freeze was refused: {other}"),
            }
            let AgentBackend::Conversation { conversation, .. } = &source else {
                unreachable!("a sidecar backend");
            };
            let _ = conversation.take_ui_delivery();
            match source.take_frozen() {
                Ok(frozen) => break frozen,
                Err(agent::HandoffRefused::Undelivered) => {}
                Err(other) => panic!("the move was refused: {other}"),
            }
        };
        let over_the_wire = serde_json::to_string(&handoff).unwrap();
        let handoff: agent::ConversationHandoff = serde_json::from_str(&over_the_wire).unwrap();
        let adopted = AgentBackend::adopt_moved(
            core,
            handoff,
            backend,
            agent::MovedProvider {
                agent: provider,
                external: None,
                lease: None,
            },
        )
        .unwrap();
        let names: Vec<&str> = adopted.declared_client_tools().iter().map(String::as_str).collect();
        assert_eq!(names, ["open_file"]);
    }

    #[test]
    fn a_provider_without_client_tools_gives_a_session_none() {
        let dir = agent::state_dirs::test_workspace_dir("client-tools-unoffered");
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let mut backend = sidecar_backend_with(provider, &dir);
        assert!(backend.declared_client_tools().is_empty());
        let error = backend.answer_client_tool_call("call", vec![], false).unwrap_err();
        assert!(error.benign, "an answer to a provider without tools ends nothing");
        backend.shutdown();
    }

    #[test]
    fn the_open_calls_read_comes_in_call_order_and_answers_reach_the_provider() {
        let dir = agent::state_dirs::test_workspace_dir("client-tools-open");
        let provider = std::sync::Arc::new(RecordingProvider::client_tooling());
        let mut backend = sidecar_backend_with(provider.clone(), &dir);
        for (call_id, sequence) in [("z-first", 1), ("a-second", 2)] {
            provider.queue(AgentDomainEvent::ClientToolCall {
                call_id: call_id.into(),
                tool_use_id: None,
                tool: "open_file".into(),
                arguments_json: "{}".into(),
                sequence,
            });
        }
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backend.open_client_calls().len() < 2 {
            assert!(std::time::Instant::now() < until, "the calls never folded");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let order: Vec<String> = backend.open_client_calls().into_iter().map(|c| c.call_id).collect();
        assert_eq!(order, ["z-first", "a-second"]);

        backend
            .answer_client_tool_call("z-first", vec!["done".into()], false)
            .unwrap();
        let answers = provider.client_answers();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].call_id, "z-first");
        assert_eq!(answers[0].session_id, "fake-session");
        assert_eq!(answers[0].text, ["done"]);

        let state = agent::ClientToolCallState {
            through_sequence: 9,
            open: vec![],
        };
        provider.serve_client_state(state.clone());
        let reply = backend.client_tool_call_state().unwrap();
        assert_eq!(reply.recv().unwrap().unwrap(), state);
        backend.shutdown();
    }

    #[test]
    fn a_provider_without_the_method_gets_the_benign_wording() {
        let dir = agent::state_dirs::test_workspace_dir("claude-task-control");
        let (_, mut backend) = sidecar_backend(&dir);
        let error = backend.stop_background(1).unwrap_err();
        assert!(error.benign);
        assert_eq!(error.message, "Claude cannot stop what it left running");
        backend.shutdown();
    }

    fn sidecar_backend(dir: &Path) -> (std::sync::Arc<RecordingProvider>, AgentBackend) {
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    fn carded(delivered: &[AgentDomainEvent], id: &str) -> bool {
        delivered
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == id))
    }

    /// The classifier allows an in-project `Read` at the gate. The CLI's prompt for the same call
    /// bypasses the classifier and remains a card in Auto without a human approval.
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
            &provider,
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

    /// A prefix rule that answers `npm ci` at the gate cannot answer the CLI's prompt for that command.
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
            &provider,
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

    /// Bypass allows the CLI's prompt without a card, records the answer in `host_answered`, and
    /// returns the note displayed on its row, just as for other bypass answers.
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
            &provider,
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
                note: "Claude Code asked — allowed in bypass".into(),
            }]
        );
        backend.shutdown();
    }

    /// Every kind of the CLI's own prompt that Auto keeps as a card of its own
    /// (`ProviderPrompt::needs_its_own_card`), each under the id `tu-1` for a `Bash` of `cat notes.txt`:
    /// one a bare and one a content-scoped ask rule forced, one of a kind this build does not know, one
    /// that gives no reason, one that only describes its subject, one that only names a path, and one
    /// with an empty and one with a blank reason. Paired with the label its row note carries.
    fn prompts_that_need_their_own_card() -> Vec<(&'static str, AgentDomainEvent, &'static str)> {
        let cat = serde_json::json!({ "command": "cat notes.txt" });
        let with = |id: &'static str, change: &dyn Fn(&mut agent::ProviderPrompt)| {
            let mut event = provider_prompt(id, Some("tu-1"), "Bash", cat.clone(), None);
            if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut event {
                change(provider_prompt.as_mut().unwrap());
            }
            event
        };
        let maybe_a_rule = "Claude Code asked (maybe your ask rule)";
        vec![
            (
                "perm-scoped-rule",
                provider_prompt("perm-scoped-rule", Some("tu-1"), "Bash", cat.clone(), Some("cat:*")),
                "your ask rule: Bash(cat:*)",
            ),
            (
                "perm-bare-rule",
                with("perm-bare-rule", &|p| {
                    p.reason = None;
                    p.matched_ask_rule = Some(agent::MatchedAskRule {
                        source: "userSettings".into(),
                        tool_name: "Bash".into(),
                        rule_content: None,
                    });
                }),
                "your ask rule: Bash",
            ),
            (
                "perm-unknown",
                with("perm-unknown", &|p| p.unrecognized_origin = Some(7)),
                "Claude Code asked",
            ),
            ("perm-silent", with("perm-silent", &|p| p.reason = None), maybe_a_rule),
            (
                "perm-described",
                with("perm-described", &|p| {
                    p.reason = None;
                    p.description = Some("Print the notes".into());
                }),
                maybe_a_rule,
            ),
            (
                "perm-path-only",
                with("perm-path-only", &|p| {
                    p.reason = None;
                    p.blocked_path = Some("/p/notes.txt".into());
                }),
                maybe_a_rule,
            ),
            (
                "perm-empty-reason",
                with("perm-empty-reason", &|p| {
                    p.reason = Some(String::new());
                    p.blocked_path = Some("/p/notes.txt".into());
                }),
                maybe_a_rule,
            ),
            (
                "perm-blank-reason",
                with("perm-blank-reason", &|p| p.reason = Some("  \n".into())),
                maybe_a_rule,
            ),
        ]
    }

    /// Bypass approves every request, the CLI's own prompts included, whatever they say about why they
    /// asked: one the user's own ask rule forced (bare or content-scoped), one of a kind this build does
    /// not know, and one that gives no reason are each answered `allow` with no card, recorded in
    /// `host_answered`, and noted on the call's row under their own label.
    #[test]
    fn bypass_allows_every_kind_of_the_clis_own_prompt() {
        let dir = a_workspace_holding_one_file();
        for (id, event, label) in prompts_that_need_their_own_card() {
            let (provider, mut backend) = sidecar_backend(&dir);
            provider.queue(event);
            let mut host_answered = BTreeSet::new();
            let (delivered, answered) = deliver(
                &provider,
                &mut backend,
                &dir,
                &agent::PrefixRules::default(),
                PermissionMode::Bypass,
                &mut host_answered,
                &mut HumanApprovals::default(),
            );
            assert!(!carded(&delivered, id), "{id}: {delivered:?}");
            assert_eq!(provider.resolutions(), vec![(id.to_string(), true)], "{id}");
            assert_eq!(host_answered, BTreeSet::from([id.to_string()]), "{id}");
            assert_eq!(
                answered,
                vec![PromptAnsweredForYou {
                    tool_use_id: "tu-1".into(),
                    note: format!("{label} — allowed in bypass"),
                }],
                "{id}"
            );
            backend.shutdown();
        }
    }

    /// In Auto the same prompts are cards of their own: the user's approval of the gate's card for
    /// exactly this call does not answer them, and is not used up by them.
    #[test]
    fn in_auto_a_prompt_that_needs_its_own_card_is_one_even_after_an_approval() {
        let dir = a_workspace_holding_one_file();
        let cat = serde_json::json!({ "command": "cat notes.txt" });
        for (id, event, _) in prompts_that_need_their_own_card() {
            let (provider, mut backend) = sidecar_backend(&dir);
            provider.queue(event);
            let mut approvals = HumanApprovals::default();
            approvals.record("tu-1", "Bash", &cat);
            let mut host_answered = BTreeSet::new();
            let (delivered, answered) = deliver(
                &provider,
                &mut backend,
                &dir,
                &agent::PrefixRules::default(),
                PermissionMode::Auto,
                &mut host_answered,
                &mut approvals,
            );
            assert!(carded(&delivered, id), "{id}: {delivered:?}");
            assert!(provider.resolutions().is_empty(), "{id}");
            assert!(host_answered.is_empty() && answered.is_empty(), "{id}");
            assert!(
                approvals.contains("tu-1"),
                "{id}: an approval a card kept is not used up"
            );
            backend.shutdown();
        }
    }

    /// The acceptEdits fast path answers only the gate's request for an in-project `Write`.
    /// A user's `permissions.ask` rule (`Edit(notes.txt)`) can still make the CLI prompt afterward:
    /// CLI 2.1.283 runs its ask-rule check after a hook allow. That prompt remains a card in Auto,
    /// as does a CLI prompt with no rule behind it: an automatic allow is not a human approval
    /// (`HumanApprovals` stays empty). This preserves explicit ask rules even when edits normally
    /// need no card. Bypass answers that prompt like every other request
    /// (`bypass_allows_every_kind_of_the_clis_own_prompt`).
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
            &provider,
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

    /// In Auto, human approval answers the CLI's prompt only once for the same call: the same
    /// non-empty tool-use id, tool and input shown on the card. Another call, changed tool or input,
    /// no id, or a second prompt after consuming the approval must produce a card.
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
            &provider,
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
                note: "Claude Code asked — allowed with your approval".into(),
            }]
        );
        backend.shutdown();
    }

    /// Fail toward a card, as every automatic answer here does: an allow of the CLI's own prompt that
    /// the provider refuses stays in the delivery -- and an approval it would have used is kept. In
    /// bypass that holds for a prompt the user's own ask rule forced as much as for a plain one.
    #[test]
    fn a_failed_allow_of_the_clis_own_prompt_draws_the_card() {
        let dir = a_workspace_holding_one_file();
        let plain = provider_prompt("perm-prov", Some("tu-1"), "Write", probe_write(), None);
        let ruled = provider_prompt("perm-prov", Some("tu-1"), "Write", probe_write(), Some(".git/probe"));
        for (mode, event) in [
            (PermissionMode::Bypass, plain.clone()),
            (PermissionMode::Bypass, ruled),
            (PermissionMode::Auto, plain),
        ] {
            let (provider, mut backend) = sidecar_backend(&dir);
            provider.refuse_resolutions(true);
            provider.queue(event);
            let mut approvals = HumanApprovals::default();
            approvals.record("tu-1", "Write", &probe_write());
            let mut host_answered = BTreeSet::new();
            let (delivered, answered) = deliver(
                &provider,
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
    /// list in one go, the CLI's own prompts included: an ask rule's, one of unknown kind and one that
    /// gives no reason are answered like a plain one, in the order given.
    #[test]
    fn approve_pending_answers_every_kind_of_the_clis_own_prompt() {
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
        let mut silent = provider_prompt("perm-silent", Some("tu-4"), "Write", probe_write(), None);
        if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut silent {
            provider_prompt.as_mut().unwrap().reason = None;
        }
        provider.queue(silent);
        provider.queue(provider_prompt(
            "perm-plain",
            Some("tu-2"),
            "Write",
            probe_write(),
            None,
        ));
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 4, "all card in Auto with no approval: {delivered:?}");
        let ids: Vec<String> = ["perm-ask", "perm-unknown", "perm-silent", "perm-plain"]
            .map(str::to_string)
            .to_vec();
        let answered = backend.approve_pending(&ids, "test", false, &mut HumanApprovals::default());
        assert_eq!(answered.ids, ids);
        assert_eq!(
            provider.resolutions(),
            ids.iter().map(|id| (id.clone(), true)).collect::<Vec<_>>()
        );
        backend.shutdown();
    }

    /// What `approve_pending` answers of the CLI's own prompts carries the row note a bypass tab's own
    /// answer carries ("<label> — allowed in bypass"), by tool-use id, so a call approved on entering
    /// bypass or by the resync sweep says so on its row too. The gate's own request, and a prompt that
    /// names no call, give none.
    #[test]
    fn approve_pending_notes_each_cli_prompt_it_answers() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        provider.queue(provider_prompt(
            "perm-ask",
            Some("tu-1"),
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        let mut silent = provider_prompt("perm-silent", Some("tu-2"), "Write", probe_write(), None);
        if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut silent {
            provider_prompt.as_mut().unwrap().reason = None;
        }
        provider.queue(silent);
        provider.queue(provider_prompt("perm-noid", None, "Write", probe_write(), None));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-gate".into(),
            tool_use_id: Some("tu-4".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "rm -rf build" }),
            provider_prompt: None,
        });
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 4, "{delivered:?}");
        let ids: Vec<String> = ["perm-ask", "perm-silent", "perm-noid", "perm-gate"]
            .map(str::to_string)
            .to_vec();
        let answered = backend.approve_pending(&ids, "on entering bypass", false, &mut HumanApprovals::default());
        assert_eq!(answered.ids, ids);
        assert_eq!(
            answered.prompts,
            vec![
                PromptAnsweredForYou {
                    tool_use_id: "tu-1".into(),
                    note: "your ask rule: Bash(cat:*) — allowed in bypass".into(),
                },
                PromptAnsweredForYou {
                    tool_use_id: "tu-2".into(),
                    note: "Claude Code asked (maybe your ask rule) — allowed in bypass".into(),
                },
            ]
        );
        backend.shutdown();
    }

    /// An ungated-mode report folded while the first answer is being sent stops later answers in
    /// that `approve_pending` batch, as in `answer_what_needs_no_human`
    /// (`a_trip_folded_mid_batch_stops_answering_the_next_request`).
    #[test]
    fn approve_pending_stops_at_a_trip_folded_mid_batch() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = sidecar_backend(&dir);
        provider.queue(provider_prompt(
            "perm-1",
            Some("tu-1"),
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        provider.queue(provider_prompt("perm-2", Some("tu-2"), "Write", probe_write(), None));
        let delivered = pump_until_delivery(&provider, &mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 2, "{delivered:?}");
        provider.fold_after_next_resolution(vec![AgentDomainEvent::UngatedCliMode {
            reported: "acceptEdits".into(),
            detail: "SessionReady".into(),
        }]);
        let answered = backend.approve_pending(
            &["perm-1".to_string(), "perm-2".to_string()],
            "on entering bypass",
            false,
            &mut HumanApprovals::default(),
        );
        assert_eq!(answered.ids, vec!["perm-1".to_string()]);
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        backend.shutdown();
    }

    // ---- The CLI's own auto mode under the gate (protocol 3.14) ---------------

    fn auto_backend(dir: &Path) -> (std::sync::Arc<RecordingProvider>, AgentBackend) {
        let provider = std::sync::Arc::new(RecordingProvider::cli_auto());
        let conversation = AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    fn cli_reports(mode: &str) -> AgentDomainEvent {
        AgentDomainEvent::CliPermissionMode { reported: mode.into() }
    }

    fn gate(permission_id: &str, tool_use_id: &str, tool_name: &str, input: serde_json::Value) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: Some(tool_use_id.into()),
            tool_name: tool_name.into(),
            input,
            provider_prompt: None,
        }
    }

    /// Three gate requests the classifier judges three ways today: a `Read` it allows, a `Bash` it
    /// cards ("not on the read-only list"), and a force push it cards. On an auto session in an Auto
    /// tab all three are deferred to the CLI, none is allowed by Eitri, and none is a card.
    fn three_gates() -> Vec<AgentDomainEvent> {
        vec![
            gate(
                "perm-read",
                "tu-r",
                "Read",
                serde_json::json!({ "file_path": "main.rs" }),
            ),
            gate(
                "perm-build",
                "tu-b",
                "Bash",
                serde_json::json!({ "command": "cargo build" }),
            ),
            gate(
                "perm-push",
                "tu-p",
                "Bash",
                serde_json::json!({ "command": "git push --force origin main" }),
            ),
        ]
    }

    /// One delivery, keeping everything `AnsweredForYou` recorded (`deliver` keeps the prompts only).
    fn deliver_noting(
        provider: &RecordingProvider,
        backend: &mut AgentBackend,
        dir: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
    ) -> (Vec<AgentDomainEvent>, AnsweredForYou) {
        let mut answered = AnsweredForYou::default();
        let mut delivered = Vec::new();
        for _ in 0..20 {
            provider.wait_until_folded();
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(dir),
                    rules,
                    mode,
                    host_answered,
                    approvals,
                    &mut answered,
                )
                .without_revisions()
            {
                UiDelivery::Events(events) if !events.is_empty() => {
                    delivered.extend(events);
                }
                _ => {}
            }
        }
        (delivered, answered)
    }

    /// A write into Eitri's own configuration takes effect at its next start, so on an auto session it is a card and
    /// is never left to the CLI's classifier, by whichever tool it comes. The places are this process's own (`$HOME`
    /// as the test runs), as the window reads them; nothing is written there.
    #[test]
    fn an_auto_tab_on_an_auto_session_cards_a_write_into_eitris_own_places() {
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME is set");
        let home = home.display();
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        let config = format!("{home}/.config/eitri/init.lua");
        let plugin = format!("{home}/.local/share/eitri/plugins/md/main.lua");
        let rules_file = format!("{home}/.local/state/eitri/permissions/rules.json");
        let carded_ids = [
            "perm-write",
            "perm-edit",
            "perm-multi",
            "perm-notebook",
            "perm-echo",
            "perm-cp",
            "perm-monitor",
            "perm-mcp",
        ];
        provider.queue_all(vec![
            cli_reports("auto"),
            gate(
                "perm-write",
                "tu-1",
                "Write",
                serde_json::json!({ "file_path": config, "content": "x" }),
            ),
            gate(
                "perm-edit",
                "tu-2",
                "Edit",
                serde_json::json!({ "file_path": config, "old_string": "a", "new_string": "b" }),
            ),
            gate(
                "perm-multi",
                "tu-3",
                "MultiEdit",
                serde_json::json!({ "file_path": plugin, "edits": [] }),
            ),
            gate(
                "perm-notebook",
                "tu-4",
                "NotebookEdit",
                serde_json::json!({ "notebook_path": rules_file, "new_source": "x" }),
            ),
            gate(
                "perm-echo",
                "tu-5",
                "Bash",
                serde_json::json!({ "command": "echo 'x' >> ~/.config/eitri/init.lua" }),
            ),
            gate(
                "perm-cp",
                "tu-6",
                "Bash",
                serde_json::json!({ "command": format!("cp evil.lua {plugin}") }),
            ),
            gate(
                "perm-monitor",
                "tu-7",
                "Monitor",
                serde_json::json!({ "command": "sh -c 'cat a > $HOME/.config/eitri/init.lua'" }),
            ),
            gate(
                "perm-mcp",
                "tu-8",
                "mcp__fs__write_file",
                serde_json::json!({ "path": config, "content": "x" }),
            ),
            gate(
                "perm-ok",
                "tu-9",
                "Write",
                serde_json::json!({ "file_path": "main.rs", "content": "x" }),
            ),
            gate(
                "perm-build",
                "tu-10",
                "Bash",
                serde_json::json!({ "command": "cargo build" }),
            ),
        ]);
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(cp *)").unwrap());
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        for id in carded_ids {
            assert!(carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert_eq!(
            provider.decisions(),
            vec![
                ("perm-ok".to_string(), PermissionDecision::Defer),
                ("perm-build".to_string(), PermissionDecision::Defer),
            ],
            "only the calls that name no place are left to the CLI"
        );
        backend.shutdown();
    }

    #[test]
    fn an_auto_tab_on_an_auto_session_defers_every_gate_request_and_draws_no_card() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let mut host_answered = BTreeSet::new();
        let (delivered, answered) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut host_answered,
            &mut HumanApprovals::default(),
        );
        for id in ["perm-read", "perm-build", "perm-push"] {
            assert!(!carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert_eq!(
            provider.decisions(),
            vec![
                ("perm-read".to_string(), PermissionDecision::Defer),
                ("perm-build".to_string(), PermissionDecision::Defer),
                ("perm-push".to_string(), PermissionDecision::Defer),
            ],
            "the classifier's own allow for the Read is not given: the CLI decides"
        );
        assert_eq!(host_answered.len(), 3, "hidden from a resync like any host answer");
        // The record of what the host answered says all three were deferrals, in order.
        assert!(
            answered
                .gate_answers
                .iter()
                .all(|a| a.origin == GateOrigin::GateRequest),
            "every one of these is the gate's own request: {:?}",
            answered.gate_answers
        );
        let answered_by_the_host: Vec<(String, bool)> = answered
            .gate_answers
            .iter()
            .map(|a| (a.permission_id.clone(), a.deferred))
            .collect();
        assert_eq!(
            answered_by_the_host,
            vec![
                ("perm-read".to_string(), true),
                ("perm-build".to_string(), true),
                ("perm-push".to_string(), true),
            ]
        );
        assert_eq!(
            AnsweredForYou {
                gate_answers: Vec::new(),
                ..answered
            },
            AnsweredForYou::default(),
            "nothing was allowed by Eitri, so no row says it was"
        );
        backend.shutdown();
    }

    fn chmod(dir: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A workspace anyone on the machine can write to, which the gate takes for no boundary.
    fn a_workspace_others_can_write() -> PathBuf {
        let dir = a_workspace_holding_one_file();
        chmod(&dir, 0o777);
        dir
    }

    #[test]
    fn an_auto_tab_on_a_cli_auto_session_cards_at_a_root_that_is_no_boundary() {
        let dir = a_workspace_others_can_write();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let mut host_answered = BTreeSet::new();
        let (delivered, answered) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut host_answered,
            &mut HumanApprovals::default(),
        );
        for id in ["perm-read", "perm-build", "perm-push"] {
            assert!(carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert_eq!(provider.decisions(), Vec::new(), "no deferral and no allow");
        assert!(host_answered.is_empty());
        assert_eq!(answered, AnsweredForYou::default());
        assert_eq!(
            agent::classify_permission_request("Read", &serde_json::json!({ "file_path": "main.rs" }), &dir).reason,
            agent::project_boundary::NotABoundary::OthersCanWrite.reason(),
            "the card stands for the root, not for the call"
        );
        backend.shutdown();
    }

    #[test]
    fn a_saved_rule_answers_nothing_there() {
        let dir = a_workspace_others_can_write();
        let (provider, mut backend) = auto_backend(&dir);
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap());
        provider.queue_all(vec![
            cli_reports("auto"),
            gate("perm-ci", "tu-ci", "Bash", serde_json::json!({ "command": "npm ci" })),
        ]);
        let (delivered, answered) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert!(carded(&delivered, "perm-ci"), "{delivered:?}");
        assert_eq!(provider.decisions(), Vec::new());
        assert!(answered.by_rule.is_empty(), "{answered:?}");
        backend.shutdown();
    }

    #[test]
    fn a_usable_root_still_defers() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        for id in ["perm-read", "perm-build", "perm-push"] {
            assert!(!carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert_eq!(
            provider.decisions(),
            ["perm-read", "perm-build", "perm-push"].map(|id| (id.to_string(), PermissionDecision::Defer))
        );
        backend.shutdown();
    }

    #[test]
    fn a_manual_tab_at_such_a_root_still_cards() {
        let dir = a_workspace_others_can_write();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Manual,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        for id in ["perm-read", "perm-build", "perm-push"] {
            assert!(carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert_eq!(provider.decisions(), Vec::new());
        backend.shutdown();
    }

    /// The user chose bypass, and it asks nothing of the root.
    #[test]
    fn a_bypass_tab_at_such_a_root_still_allows() {
        let dir = a_workspace_others_can_write();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Bypass,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert!(!carded(&delivered, "perm-push"), "{delivered:?}");
        assert_eq!(
            provider.decisions(),
            ["perm-read", "perm-build", "perm-push"].map(|id| (id.to_string(), PermissionDecision::Allow))
        );
        backend.shutdown();
    }

    /// Bypass is unchanged on an auto session: every gate request is allowed, which also skips the
    /// CLI's classifier (a hook allow does, measured on CLI 2.1.284).
    #[test]
    fn a_bypass_tab_on_an_auto_session_still_allows_every_gate_request() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Bypass,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert!(!carded(&delivered, "perm-push"), "{delivered:?}");
        assert_eq!(
            provider.decisions(),
            ["perm-read", "perm-build", "perm-push"].map(|id| (id.to_string(), PermissionDecision::Allow))
        );
        backend.shutdown();
    }

    /// Which events reach the page when a gate request is answered by the host and the CLI's own
    /// prompt for the same call carries a rewritten input: the answered gate request never arrives,
    /// but the derived `ToolInputChanged` does, between the call and the prompt's card.
    fn changed_input_delivery(
        mode: PermissionMode,
        auto_session: bool,
        gate_tool: &str,
        gate_input: serde_json::Value,
    ) {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = if auto_session {
            auto_backend(&dir)
        } else {
            sidecar_backend(&dir)
        };
        let rewritten = serde_json::json!({ "command": "rtk git push --force origin main" });
        let mut batch = vec![];
        if auto_session {
            batch.push(cli_reports("auto"));
        }
        batch.extend([
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t".into(),
                tool_use_id: "tu-1".into(),
                name: gate_tool.into(),
                input: gate_input.clone(),
            },
            gate("perm-gate", "tu-1", gate_tool, gate_input),
            // The CLI's own prompt: no reason, so an Auto tab cards it (bypass would answer it).
            AgentDomainEvent::PermissionRequested {
                permission_id: "perm-cli".into(),
                tool_use_id: Some("tu-1".into()),
                tool_name: gate_tool.into(),
                input: rewritten.clone(),
                provider_prompt: Some(agent::ProviderPrompt::default()),
            },
        ]);
        provider.queue_all(batch);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            mode,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert!(
            !carded(&delivered, "perm-gate"),
            "the gate request was answered by the host: {delivered:?}"
        );
        let changed: Vec<_> = delivered
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::ToolInputChanged { tool_use_id, change } => Some((tool_use_id.as_str(), change)),
                _ => None,
            })
            .collect();
        assert_eq!(changed.len(), 1, "{delivered:?}");
        assert_eq!(changed[0].0, "tu-1");
        assert_eq!(changed[0].1.fields["command"], rewritten["command"]);
        let at = |pred: &dyn Fn(&AgentDomainEvent) -> bool| delivered.iter().position(|e| pred(e)).unwrap();
        let started = at(&|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. }));
        let mark = at(&|e| matches!(e, AgentDomainEvent::ToolInputChanged { .. }));
        assert!(started < mark, "the mark follows its call");
        backend.shutdown();
    }

    #[test]
    fn a_deferred_gate_request_still_lets_the_page_learn_of_the_changed_input() {
        changed_input_delivery(
            PermissionMode::Auto,
            true,
            "Bash",
            serde_json::json!({ "command": "git push --force origin main" }),
        );
    }

    #[test]
    fn a_classifier_allowed_gate_request_still_lets_the_page_learn_of_the_changed_input() {
        // A read-only command the classifier allows without a card, on a session whose CLI runs `default`.
        changed_input_delivery(
            PermissionMode::Auto,
            false,
            "Bash",
            serde_json::json!({ "command": "ls" }),
        );
    }

    #[test]
    fn a_bypass_allowed_gate_request_still_lets_the_page_learn_of_the_changed_input() {
        changed_input_delivery(
            PermissionMode::Bypass,
            true,
            "Bash",
            serde_json::json!({ "command": "git push --force origin main" }),
        );
    }

    /// A saved prefix rule answers on an auto session exactly what it answers today, `allow`, with
    /// its row note; a call the rule does not reach is deferred, and a rule never reaches past a
    /// fail-closed check (`rule_that_allows`): `npm ci; rm -rf x` carries shell syntax, so even with
    /// `Bash(npm ci *)` saved it is the CLI's to decide, never allowed here.
    #[test]
    fn a_saved_rule_allows_on_an_auto_session_and_everything_else_is_deferred() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap());
        provider.queue_all(vec![
            cli_reports("auto"),
            gate("perm-ci", "tu-ci", "Bash", serde_json::json!({ "command": "npm ci" })),
            gate(
                "perm-chain",
                "tu-chain",
                "Bash",
                serde_json::json!({ "command": "npm ci; rm -rf x" }),
            ),
            gate(
                "perm-test",
                "tu-test",
                "Bash",
                serde_json::json!({ "command": "npm test" }),
            ),
        ]);
        let (_, answered) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert_eq!(
            provider.decisions(),
            vec![
                ("perm-ci".to_string(), PermissionDecision::Allow),
                ("perm-chain".to_string(), PermissionDecision::Defer),
                ("perm-test".to_string(), PermissionDecision::Defer),
            ]
        );
        assert_eq!(
            answered.by_rule,
            vec![RuleAnsweredForYou {
                tool_use_id: "tu-ci".into(),
                rule: "Bash(npm ci *)".into(),
                tool_name: "Bash".into(),
                input: serde_json::json!({ "command": "npm ci" }),
            }]
        );
        backend.shutdown();
    }

    /// The CLI's own prompts keep their rules on an auto session: never deferred (Verdandi refuses
    /// that, and it is the CLI's own question already), a card in Auto without the human's approval
    /// of the same call, allowed with it, allowed in bypass, and one the user's ask rule forced a card
    /// in Auto even with the approval and allowed in bypass.
    #[test]
    fn the_clis_own_prompts_are_answered_as_before_on_an_auto_session() {
        let dir = a_workspace_holding_one_file();
        let cat = serde_json::json!({ "command": "cat notes.txt" });
        // Auto, no approval: a card.
        let (provider, mut backend) = auto_backend(&dir);
        provider.queue_all(vec![
            cli_reports("auto"),
            provider_prompt("perm-prov", Some("tu-1"), "Write", probe_write(), None),
        ]);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert!(carded(&delivered, "perm-prov"), "{delivered:?}");
        assert!(provider.decisions().is_empty());
        backend.shutdown();
        // Auto, approved on a card: allowed once, never deferred.
        let (provider, mut backend) = auto_backend(&dir);
        provider.queue_all(vec![
            cli_reports("auto"),
            provider_prompt("perm-prov", Some("tu-1"), "Write", probe_write(), None),
        ]);
        let mut approvals = HumanApprovals::default();
        approvals.record("tu-1", "Write", &probe_write());
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut approvals,
        );
        assert!(!carded(&delivered, "perm-prov"), "{delivered:?}");
        assert_eq!(
            provider.decisions(),
            vec![("perm-prov".to_string(), PermissionDecision::Allow)]
        );
        backend.shutdown();
        // An ask rule's prompt: a card in Auto even with the approval, allowed in bypass, never
        // deferred in either.
        for mode in [PermissionMode::Auto, PermissionMode::Bypass] {
            let (provider, mut backend) = auto_backend(&dir);
            provider.queue_all(vec![
                cli_reports("auto"),
                provider_prompt("perm-ask", Some("tu-2"), "Bash", cat.clone(), Some("cat:*")),
            ]);
            let mut approvals = HumanApprovals::default();
            approvals.record("tu-2", "Bash", &cat);
            let (delivered, _) = deliver_noting(
                &provider,
                &mut backend,
                &dir,
                &agent::PrefixRules::default(),
                mode,
                &mut BTreeSet::new(),
                &mut approvals,
            );
            if mode == PermissionMode::Auto {
                assert!(carded(&delivered, "perm-ask"), "{mode:?}: {delivered:?}");
                assert!(provider.decisions().is_empty(), "{mode:?}");
            } else {
                assert!(!carded(&delivered, "perm-ask"), "{mode:?}: {delivered:?}");
                assert_eq!(
                    provider.decisions(),
                    vec![("perm-ask".to_string(), PermissionDecision::Allow)],
                    "{mode:?}"
                );
            }
            backend.shutdown();
        }
    }

    /// Today's policy, exactly, wherever the CLI is not known to run auto: a provider without the
    /// capability (an older sidecar) even if something reported `auto`, a session whose CLI fell
    /// back to `default`, and one whose CLI has not reported yet. The classifier allows the `Read`
    /// and cards both `Bash` calls; nothing is deferred.
    #[test]
    fn without_auto_running_the_answers_are_todays() {
        let dir = a_workspace_holding_one_file();
        let cases: [(&str, bool, Option<&str>); 3] = [
            ("an older sidecar", false, Some("auto")),
            ("a fallback to default", true, Some("default")),
            ("no report yet", true, None),
        ];
        for (case, capable, report) in cases {
            let provider = std::sync::Arc::new(if capable {
                RecordingProvider::cli_auto()
            } else {
                RecordingProvider::default()
            });
            let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
            let mut backend = AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            };
            let mut batch: Vec<AgentDomainEvent> = report.map(cli_reports).into_iter().collect();
            batch.extend(three_gates());
            provider.queue_all(batch);
            let (delivered, _) = deliver_noting(
                &provider,
                &mut backend,
                &dir,
                &agent::PrefixRules::default(),
                PermissionMode::Auto,
                &mut BTreeSet::new(),
                &mut HumanApprovals::default(),
            );
            assert_eq!(
                provider.decisions(),
                vec![("perm-read".to_string(), PermissionDecision::Allow)],
                "{case}"
            );
            assert!(!carded(&delivered, "perm-read"), "{case}: {delivered:?}");
            assert!(carded(&delivered, "perm-build"), "{case}: {delivered:?}");
            assert!(carded(&delivered, "perm-push"), "{case}: {delivered:?}");
            backend.shutdown();
        }
    }

    /// A deferral that does not reach the provider is a card, never a silent grant; and a session
    /// whose CLI reported an ungated mode answers nothing, deferrals included.
    #[test]
    fn a_failed_deferral_is_a_card_and_a_tripped_auto_session_defers_nothing() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        provider.refuse_resolutions(true);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let mut host_answered = BTreeSet::new();
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut host_answered,
            &mut HumanApprovals::default(),
        );
        for id in ["perm-read", "perm-build", "perm-push"] {
            assert!(carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert!(host_answered.is_empty());
        backend.shutdown();

        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![
            cli_reports("auto"),
            AgentDomainEvent::UngatedCliMode {
                reported: "acceptEdits".into(),
                detail: "SessionReady".into(),
            },
        ];
        batch.extend(three_gates());
        provider.queue_all(batch);
        for mode in [PermissionMode::Auto, PermissionMode::Bypass] {
            let _ = deliver_noting(
                &provider,
                &mut backend,
                &dir,
                &agent::PrefixRules::default(),
                mode,
                &mut BTreeSet::new(),
                &mut HumanApprovals::default(),
            );
        }
        assert!(provider.decisions().is_empty(), "{:?}", provider.decisions());
        backend.shutdown();
    }

    /// Ingestion keeps folding while a batch is answered. If answering the first request is when
    /// the CLI reports it fell back to `default`, the second request of the same batch is judged by
    /// Eitri's own policy (a `Read` is allowed, not deferred): a deferral to a CLI that is no
    /// longer in auto mode would be decided by its `default` mode instead of the classifier the
    /// session was promised.
    #[test]
    fn a_fallback_to_default_folded_mid_batch_stops_deferring_the_next_request() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        provider.queue_all(vec![
            cli_reports("auto"),
            gate(
                "perm-1",
                "tu-1",
                "Bash",
                serde_json::json!({ "command": "cargo build" }),
            ),
            gate("perm-2", "tu-2", "Read", serde_json::json!({ "file_path": "main.rs" })),
            gate(
                "perm-3",
                "tu-3",
                "Bash",
                serde_json::json!({ "command": "cargo build" }),
            ),
        ]);
        provider.fold_after_next_resolution(vec![cli_reports("default")]);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert_eq!(
            provider.decisions(),
            vec![
                ("perm-1".to_string(), PermissionDecision::Defer),
                ("perm-2".to_string(), PermissionDecision::Allow),
            ],
            "the first was deferred while the CLI ran auto; after the fallback the classifier answers"
        );
        assert!(carded(&delivered, "perm-3"), "{delivered:?}");
        backend.shutdown();
    }

    /// The same, for a trip folded mid-batch: nothing after it is answered, deferrals included, and
    /// the rest of the batch is cards.
    #[test]
    fn a_trip_folded_mid_batch_stops_answering_the_next_request() {
        let dir = a_workspace_holding_one_file();
        for mode in [PermissionMode::Auto, PermissionMode::Bypass] {
            let (provider, mut backend) = auto_backend(&dir);
            provider.queue_all(vec![
                cli_reports("auto"),
                gate(
                    "perm-1",
                    "tu-1",
                    "Bash",
                    serde_json::json!({ "command": "cargo build" }),
                ),
                gate("perm-2", "tu-2", "Read", serde_json::json!({ "file_path": "main.rs" })),
            ]);
            provider.fold_after_next_resolution(vec![AgentDomainEvent::UngatedCliMode {
                reported: "acceptEdits".into(),
                detail: "SessionReady".into(),
            }]);
            let (delivered, _) = deliver_noting(
                &provider,
                &mut backend,
                &dir,
                &agent::PrefixRules::default(),
                mode,
                &mut BTreeSet::new(),
                &mut HumanApprovals::default(),
            );
            let expected = if mode == PermissionMode::Auto {
                PermissionDecision::Defer
            } else {
                PermissionDecision::Allow
            };
            assert_eq!(provider.decisions(), vec![("perm-1".to_string(), expected)], "{mode:?}");
            assert!(carded(&delivered, "perm-2"), "{mode:?}: {delivered:?}");
            backend.shutdown();
        }
    }

    // ---- Manual: Eitri's policy alone, every edit a card ----------------------------------------

    /// The gate requests of a manual table: an in-project read, a read-only and a rule-matched
    /// command, a command on no list, a force push, an in-project write and edit, and a write on a
    /// protected path.
    fn manual_gates() -> Vec<AgentDomainEvent> {
        vec![
            gate(
                "perm-read",
                "tu-r",
                "Read",
                serde_json::json!({ "file_path": "main.rs" }),
            ),
            gate("perm-ls", "tu-l", "Bash", serde_json::json!({ "command": "ls" })),
            gate(
                "perm-rule",
                "tu-t",
                "Bash",
                serde_json::json!({ "command": "cargo test" }),
            ),
            gate(
                "perm-build",
                "tu-b",
                "Bash",
                serde_json::json!({ "command": "cargo build" }),
            ),
            gate(
                "perm-push",
                "tu-p",
                "Bash",
                serde_json::json!({ "command": "git push --force origin main" }),
            ),
            gate(
                "perm-write",
                "tu-w",
                "Write",
                serde_json::json!({ "file_path": "new.txt", "content": "x" }),
            ),
            gate(
                "perm-edit",
                "tu-e",
                "Edit",
                serde_json::json!({ "file_path": "main.rs", "old_string": "main", "new_string": "start" }),
            ),
            gate("perm-git", "tu-g", "Write", probe_write()),
        ]
    }

    fn cargo_test_rule() -> agent::PrefixRules {
        agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(cargo test *)").unwrap())
    }

    /// On a session whose CLI runs its own auto mode, a Manual tab defers nothing: Eitri's policy
    /// answers, in-project reads and read-only commands run, a saved rule still allows, and every
    /// edit is a card (no deferral, no fast path).
    #[test]
    fn a_manual_tab_on_an_auto_session_defers_nothing_and_cards_every_edit() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(manual_gates());
        provider.queue_all(batch);
        let mut host_answered = BTreeSet::new();
        let (delivered, answered) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &cargo_test_rule(),
            PermissionMode::Manual,
            &mut host_answered,
            &mut HumanApprovals::default(),
        );
        assert_eq!(
            provider.decisions(),
            vec![
                ("perm-read".to_string(), PermissionDecision::Allow),
                ("perm-ls".to_string(), PermissionDecision::Allow),
                ("perm-rule".to_string(), PermissionDecision::Allow),
            ],
            "nothing deferred, nothing else allowed"
        );
        for id in ["perm-build", "perm-push", "perm-write", "perm-edit", "perm-git"] {
            assert!(carded(&delivered, id), "{id}: {delivered:?}");
        }
        assert!(answered.gate_answers.iter().all(|a| !a.deferred));
        assert!(answered.by_the_fast_path.is_empty(), "no edit was let through");
        assert_eq!(answered.by_rule.len(), 1, "the saved rule says so on its row");
        backend.shutdown();
    }

    /// Manual is never more permissive than Auto, on a CLI-auto session and on a `default` one alike:
    /// every request Auto cards, Manual cards (and allows only what Auto allowed or deferred); and
    /// every edit Auto lets through, Manual cards.
    #[test]
    fn manual_never_answers_what_auto_leaves_to_a_human() {
        let dir = a_workspace_holding_one_file();
        for auto_session in [false, true] {
            let run = |mode: PermissionMode| {
                let (provider, mut backend) = if auto_session {
                    auto_backend(&dir)
                } else {
                    sidecar_backend(&dir)
                };
                let mut batch = if auto_session {
                    vec![cli_reports("auto")]
                } else {
                    vec![]
                };
                batch.extend(manual_gates());
                provider.queue_all(batch);
                let (delivered, _) = deliver_noting(
                    &provider,
                    &mut backend,
                    &dir,
                    &cargo_test_rule(),
                    mode,
                    &mut BTreeSet::new(),
                    &mut HumanApprovals::default(),
                );
                let decisions = provider.decisions();
                backend.shutdown();
                (delivered, decisions)
            };
            let (auto_cards, auto_decisions) = run(PermissionMode::Auto);
            let (manual_cards, manual_decisions) = run(PermissionMode::Manual);
            for gate in manual_gates() {
                let AgentDomainEvent::PermissionRequested { permission_id: id, .. } = gate else {
                    unreachable!()
                };
                // Auto either allowed it or left it to the CLI's classifier, which may allow it:
                // either way not a card. Manual may allow only such a request.
                let auto_answered = auto_decisions.iter().any(|(answered, _)| *answered == id);
                let manual_allowed = manual_decisions.contains(&(id.clone(), PermissionDecision::Allow));
                assert!(
                    !manual_allowed || auto_answered,
                    "{id} (auto session {auto_session}): Manual allowed what Auto put to a human"
                );
                if carded(&auto_cards, &id) {
                    assert!(carded(&manual_cards, &id), "{id} (auto session {auto_session})");
                }
            }
            assert!(
                manual_decisions.iter().all(|(_, d)| *d == PermissionDecision::Allow),
                "Manual never defers: {manual_decisions:?}"
            );
            for id in ["perm-write", "perm-edit"] {
                assert!(carded(&manual_cards, id), "{id} (auto session {auto_session})");
            }
            if !auto_session {
                assert!(
                    auto_decisions.contains(&("perm-write".to_string(), PermissionDecision::Allow)),
                    "the premise: Auto's fast path allows the in-project write on a `default` session"
                );
            }
        }
    }

    /// The CLI's own prompts in Manual are answered as in Auto: a card unless the user approved that
    /// same call, and a card even then when the prompt needs one of its own. Never deferred.
    #[test]
    fn in_manual_the_clis_own_prompt_is_answered_as_in_auto() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        provider.queue(cli_reports("auto"));
        provider.queue(provider_prompt(
            "perm-unapproved",
            Some("tu-other"),
            "Write",
            probe_write(),
            None,
        ));
        provider.queue(provider_prompt(
            "perm-approved",
            Some("tu-ok"),
            "Write",
            probe_write(),
            None,
        ));
        provider.queue(provider_prompt(
            "perm-ask-rule",
            Some("tu-rule"),
            "Write",
            probe_write(),
            Some("Write(.git/**)"),
        ));
        let mut approvals = HumanApprovals::default();
        approvals.record("tu-ok", "Write", &probe_write());
        approvals.record("tu-rule", "Write", &probe_write());
        let (delivered, answered) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Manual,
            &mut BTreeSet::new(),
            &mut approvals,
        );
        assert!(carded(&delivered, "perm-unapproved"), "{delivered:?}");
        assert!(carded(&delivered, "perm-ask-rule"), "{delivered:?}");
        assert!(!carded(&delivered, "perm-approved"), "{delivered:?}");
        assert_eq!(
            provider.decisions(),
            vec![("perm-approved".to_string(), PermissionDecision::Allow)]
        );
        assert_eq!(answered.prompts.len(), 1);
        assert!(
            answered.prompts[0].note.ends_with("allowed with your approval"),
            "{:?}",
            answered.prompts
        );
        backend.shutdown();
    }

    /// A tripped session answers nothing in Manual either.
    #[test]
    fn a_tripped_session_answers_nothing_in_manual() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        provider.queue_all(vec![
            cli_reports("auto"),
            AgentDomainEvent::UngatedCliMode {
                reported: "acceptEdits".into(),
                detail: "SessionReady".into(),
            },
            gate(
                "perm-read",
                "tu-r",
                "Read",
                serde_json::json!({ "file_path": "main.rs" }),
            ),
        ]);
        let (delivered, _) = deliver_noting(
            &provider,
            &mut backend,
            &dir,
            &agent::PrefixRules::default(),
            PermissionMode::Manual,
            &mut BTreeSet::new(),
            &mut HumanApprovals::default(),
        );
        assert!(provider.decisions().is_empty(), "{:?}", provider.decisions());
        assert!(carded(&delivered, "perm-read"), "{delivered:?}");
        backend.shutdown();
    }
}

/// An external agent's tab: how its requests are answered, and what it offers to continue.
#[cfg(test)]
mod external_tests {
    use super::*;
    use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider};
    use agent::UiDelivery;
    use std::sync::Arc;

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn codex_backend(dir: &Path) -> (Arc<ExternalRecordingProvider>, AgentBackend) {
        let provider = Arc::new(ExternalRecordingProvider::codex());
        let backend = AgentBackend::start_external_with(
            AgentId::Codex,
            provider.clone(),
            dir,
            empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .expect("a scripted external start succeeds");
        (provider, backend)
    }

    #[test]
    fn a_providers_editor_note_reaches_the_backend_once() {
        let dir = workspace("codex-editor-note");
        let (provider, backend) = codex_backend(&dir);
        assert_eq!(backend.editor_note(), None);
        provider.owe_editor_note("the tools are off");
        assert_eq!(backend.editor_note().as_deref(), Some("the tools are off"));
        assert_eq!(backend.editor_note(), None);
    }

    #[test]
    fn a_codex_tab_still_stops_its_session() {
        let dir = workspace("codex-stop-background");
        let (provider, mut backend) = codex_backend(&dir);
        backend.stop_background(1).unwrap();
        assert_eq!(provider.stops(), 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(backend.projection().status, agent::ProjectionStatus::Unavailable { .. }) {
            assert!(std::time::Instant::now() < deadline, "session never ended");
            std::thread::yield_now();
        }
        backend.shutdown();
    }

    fn request(id: &str, tool: &str, input: serde_json::Value) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: Some(format!("call-{id}")),
            tool_name: tool.into(),
            input,
            provider_prompt: None,
        }
    }

    /// Samples twenty deliveries, each after the queued batch has finished folding.
    fn deliver(
        provider: &crate::test_providers::RecordingProvider,
        backend: &mut AgentBackend,
        dir: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
    ) -> Vec<AgentDomainEvent> {
        let mut delivered = Vec::new();
        for _ in 0..20 {
            provider.wait_until_folded();
            match backend
                .take_revised_ui_delivery(
                    GateRoot::Local(dir),
                    rules,
                    mode,
                    host_answered,
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
                UiDelivery::Events(events) if !events.is_empty() => {
                    delivered.extend(events);
                }
                _ => {}
            }
        }
        delivered
    }

    fn carded(delivered: &[AgentDomainEvent], id: &str) -> bool {
        delivered
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == id))
    }

    #[test]
    fn external_tab_bypass_allows_and_auto_cards() {
        let dir = workspace("external-bypass-auto");
        let (provider, mut backend) = codex_backend(&dir);
        assert_eq!(backend.agent(), AgentId::Codex);
        assert_eq!(backend.kind(), None, "an external agent runs on neither Claude backend");
        let rules = agent::PrefixRules::default();

        // Auto: the agent's own reviewer already passed these on, so each is the user's to answer.
        provider.inner.queue(request(
            "a1",
            "commandExecution",
            serde_json::json!({ "command": "rm -rf build" }),
        ));
        provider.inner.queue(request(
            "a2",
            "fileChange",
            serde_json::json!({ "changes": [{ "path": "main.rs" }] }),
        ));
        let mut answered = BTreeSet::new();
        let delivered = deliver(
            &provider.inner,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut answered,
        );
        assert!(carded(&delivered, "a1") && carded(&delivered, "a2"), "{delivered:?}");
        assert!(
            provider.inner.decisions().is_empty(),
            "Auto answers nothing for an external tab"
        );
        assert!(answered.is_empty());

        // Bypass: every request is allowed here and none is drawn.
        provider.inner.queue(request(
            "b1",
            "commandExecution",
            serde_json::json!({ "command": "curl https://example.org" }),
        ));
        let delivered = deliver(
            &provider.inner,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Bypass,
            &mut answered,
        );
        assert!(!carded(&delivered, "b1"), "{delivered:?}");
        assert_eq!(
            provider.inner.decisions(),
            vec![("b1".to_string(), PermissionDecision::Allow)]
        );
        assert!(answered.contains("b1"));
        backend.shutdown();
    }

    /// Manual on an external tab cards every request, as Auto does; Eitri answers none.
    #[test]
    fn external_tab_manual_cards_every_request() {
        let dir = workspace("external-manual");
        let (provider, mut backend) = codex_backend(&dir);
        provider.inner.queue(request(
            "m1",
            "commandExecution",
            serde_json::json!({ "command": "ls" }),
        ));
        provider.inner.queue(request(
            "m2",
            "fileChange",
            serde_json::json!({ "changes": [{ "path": "main.rs" }] }),
        ));
        let mut answered = BTreeSet::new();
        let delivered = deliver(
            &provider.inner,
            &mut backend,
            &dir,
            &agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(ls *)").unwrap()),
            PermissionMode::Manual,
            &mut answered,
        );
        assert!(carded(&delivered, "m1") && carded(&delivered, "m2"), "{delivered:?}");
        assert!(provider.inner.decisions().is_empty());
        assert!(answered.is_empty());
        backend.shutdown();
    }

    /// A request a bypass tab allowed says so on its call's row, whether it arrived in bypass or was
    /// waiting when the tab entered bypass: the escalation past the agent's sandbox is never silent.
    #[test]
    fn external_bypass_approvals_leave_a_row_note() {
        let dir = workspace("external-bypass-note");
        let (provider, mut backend) = codex_backend(&dir);
        let rules = agent::PrefixRules::default();
        provider.inner.queue(request(
            "b1",
            "commandExecution",
            serde_json::json!({ "command": "curl https://example.org" }),
        ));
        let mut host_answered = BTreeSet::new();
        let mut approvals = HumanApprovals::default();
        let mut answered = AnsweredForYou::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while answered.gate_answers.is_empty() && std::time::Instant::now() < deadline {
            backend
                .take_revised_ui_delivery(
                    GateRoot::Local(&dir),
                    &rules,
                    PermissionMode::Bypass,
                    &mut host_answered,
                    &mut approvals,
                    &mut answered,
                )
                .without_revisions();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            answered.prompts,
            vec![PromptAnsweredForYou {
                tool_use_id: "call-b1".into(),
                note: EXTERNAL_BYPASS_NOTE.into(),
            }]
        );

        // A card waiting in Auto, approved on entering bypass.
        provider.inner.queue(request(
            "a1",
            "fileChange",
            serde_json::json!({ "changes": [{ "path": "main.rs" }] }),
        ));
        let delivered = deliver(
            &provider.inner,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut host_answered,
        );
        assert!(carded(&delivered, "a1"), "{delivered:?}");
        let approved = backend.approve_pending(
            &["a1".to_string()],
            "on entering bypass",
            false,
            &mut HumanApprovals::default(),
        );
        assert_eq!(approved.ids, vec!["a1".to_string()]);
        assert_eq!(
            approved.prompts,
            vec![PromptAnsweredForYou {
                tool_use_id: "call-a1".into(),
                note: EXTERNAL_BYPASS_NOTE.into(),
            }]
        );
        backend.shutdown();
    }

    #[test]
    fn external_tab_never_consults_the_classifier_or_rules() {
        let dir = workspace("external-no-classifier");
        let (provider, mut backend) = codex_backend(&dir);
        // A rule that allows the command, and a read inside the project that Claude's classifier
        // would allow without asking: neither answers an external agent's request.
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(ls *)").unwrap());
        provider
            .inner
            .queue(request("r1", "Bash", serde_json::json!({ "command": "ls -la" })));
        provider
            .inner
            .queue(request("r2", "Read", serde_json::json!({ "file_path": "main.rs" })));
        provider.inner.queue(request(
            "r3",
            "commandExecution",
            serde_json::json!({ "command": "ls -la" }),
        ));
        let mut answered = BTreeSet::new();
        let delivered = deliver(
            &provider.inner,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut answered,
        );
        for id in ["r1", "r2", "r3"] {
            assert!(carded(&delivered, id), "{id} must be a card: {delivered:?}");
        }
        assert!(
            provider.inner.decisions().is_empty(),
            "{:?}",
            provider.inner.decisions()
        );
        backend.shutdown();
    }

    /// Every request of an external agent is the user's to answer already, so a root that is no
    /// boundary changes nothing: Auto cards, Bypass allows, and no policy of Eitri's is consulted.
    #[test]
    fn an_external_agent_is_not_judged_at_such_a_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workspace("external-no-boundary");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let (provider, mut backend) = codex_backend(&dir);
        let rules = agent::PrefixRules::default();
        provider.inner.queue(request(
            "a1",
            "commandExecution",
            serde_json::json!({ "command": "ls" }),
        ));
        let mut answered = BTreeSet::new();
        let delivered = deliver(
            &provider.inner,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Auto,
            &mut answered,
        );
        assert!(carded(&delivered, "a1"), "{delivered:?}");
        assert!(provider.inner.decisions().is_empty());
        provider.inner.queue(request(
            "b1",
            "commandExecution",
            serde_json::json!({ "command": "ls" }),
        ));
        let delivered = deliver(
            &provider.inner,
            &mut backend,
            &dir,
            &rules,
            PermissionMode::Bypass,
            &mut answered,
        );
        assert!(!carded(&delivered, "b1"), "{delivered:?}");
        assert_eq!(
            provider.inner.decisions(),
            vec![("b1".to_string(), PermissionDecision::Allow)]
        );
        backend.shutdown();
    }

    #[test]
    fn a_delegating_conversation_keeps_its_agent_identity() {
        let dir = workspace("conversation-agent-identity");
        let (_, mut backend) = codex_backend(&dir);
        assert!(matches!(
            &backend,
            AgentBackend::Conversation {
                agent: AgentId::Codex,
                ..
            }
        ));
        assert_eq!(backend.agent(), AgentId::Codex);
        assert_eq!(backend.kind(), None);
        backend.shutdown();
    }

    #[test]
    fn an_external_turn_refuses_the_door_without_a_mode() {
        let dir = workspace("external-plain-send");
        let (provider, mut backend) = codex_backend(&dir);
        let refused = backend
            .send_turn("hello", "hello", agent::PromptOrigin::Composed, Vec::new())
            .expect_err("no mode, no turn");
        assert!(refused.benign, "{}", refused.message);
        assert!(provider.policies().is_empty());
        backend
            .send_turn_in(
                "hello",
                "hello",
                SessionModeChoice::Bypass,
                Some(&dir),
                agent::PromptOrigin::Composed,
                Vec::new(),
            )
            .expect("the tab's door carries the mode");
        assert_eq!(
            provider.policies(),
            vec![TurnPolicy {
                reviewer: Reviewer::User,
                sandbox: Sandbox::Project,
            }]
        );
        assert_eq!(provider.starts()[0].policy.reviewer, Reviewer::Agent, "started in Auto");
        backend.shutdown();
    }

    /// The root is judged at a start and again at every turn, by what the gate reads: other users able to write
    /// to the folder is what makes it no boundary here.
    #[test]
    fn an_external_tab_is_read_only_while_its_root_is_no_boundary() {
        use std::os::unix::fs::PermissionsExt;
        let set_mode = |dir: &Path, mode: u32| {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let dir = workspace("external-no-boundary");
        set_mode(&dir, 0o777);
        let (provider, mut backend) = codex_backend(&dir);
        let read_only = TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: Sandbox::ReadOnly,
        };
        assert_eq!(provider.starts()[0].policy, read_only, "an Auto tab, at the start");
        let send = |backend: &mut AgentBackend| {
            backend
                .send_turn_in(
                    "hello",
                    "hello",
                    SessionModeChoice::Auto,
                    Some(&dir),
                    agent::PromptOrigin::Composed,
                    Vec::new(),
                )
                .expect("the turn goes");
        };
        send(&mut backend);

        // The folder closes up, and the next turn is the workspace's with the reviewer the tab asked for.
        set_mode(&dir, 0o700);
        send(&mut backend);

        // And it opens again, with no restart: the next turn narrows at once.
        set_mode(&dir, 0o777);
        send(&mut backend);
        set_mode(&dir, 0o700);

        assert_eq!(
            provider.policies(),
            vec![
                read_only,
                TurnPolicy {
                    reviewer: Reviewer::Agent,
                    sandbox: Sandbox::Project,
                },
                read_only,
            ]
        );
        backend.shutdown();
    }

    /// A window's folder replaced by a link to a private one, before the tab starts. What it resolves to is a
    /// fine boundary, so judging that would miss what the gate calls a root that has moved, and the tab would
    /// write there: the judgement is of the path the window holds, at the start and at every turn after it.
    #[test]
    fn a_root_replaced_by_a_link_is_read_only_at_the_start_and_after_it() {
        let target = workspace("external-moved-target");
        let window_root = target.with_file_name(format!("{}-link", target.file_name().unwrap().to_string_lossy()));
        std::os::unix::fs::symlink(&target, &window_root).unwrap();
        assert!(root_is_a_boundary(&target), "the folder it points at is one");
        let read_only = TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: Sandbox::ReadOnly,
        };
        let (provider, mut backend) = codex_backend(&window_root);
        assert_eq!(provider.starts()[0].policy, read_only);
        assert_eq!(
            provider.starts()[0].cwd,
            target.to_string_lossy(),
            "the agent runs in what the link resolves to, so the conversation's own path looks clean"
        );
        backend
            .send_turn_in(
                "hello",
                "hello",
                SessionModeChoice::Auto,
                Some(&window_root),
                agent::PromptOrigin::Composed,
                Vec::new(),
            )
            .expect("the turn goes");
        assert_eq!(provider.policies(), vec![read_only]);
        backend.shutdown();
    }

    /// A launch that is not the tab's own agent's never starts a child: an ACP registration of another
    /// name, a `codex` for an ACP tab, or anything for a Claude tab.
    #[test]
    fn a_launch_for_another_agent_is_refused() {
        let dir = workspace("external-wrong-launch");
        let spec = |name: &str| {
            ExternalLaunch::Acp(agent::providers::acp::AcpAgentSpec {
                name: name.into(),
                argv: vec!["/nonexistent/eitri-never-run".into()],
                auto_mode: None,
                read_only_mode: None,
            })
        };
        for (agent, launch) in [
            (AgentId::Acp("gem".into()), spec("other")),
            (
                AgentId::Acp("gem".into()),
                ExternalLaunch::Codex("/nonexistent/codex".into()),
            ),
            (AgentId::Codex, spec("gem")),
            (AgentId::Claude, spec("gem")),
        ] {
            let label = agent.label();
            let refused = AgentBackend::start_external(
                agent,
                launch,
                &dir,
                empty_trust_plan(),
                SessionModeChoice::Auto,
                None,
                None,
            );
            match refused {
                Err(e) => assert_eq!(e.message, format!("this build cannot start a {label} tab")),
                Ok(_) => panic!("{label} started on another agent's launch"),
            }
        }
    }

    #[test]
    fn an_external_conversation_must_be_its_agents() {
        let dir = workspace("external-wrong-agent");
        let provider = Arc::new(ExternalRecordingProvider::codex());
        let start = agent::ExternalStart {
            cwd: dir.to_string_lossy().into_owned(),
            plan: empty_trust_plan(),
            policy: turn_policy(SessionModeChoice::Auto, true),
            resume: None,
            editor_server: None,
        };
        let conversation = AgentConversation::create_external(provider, start, &dir).unwrap();
        let refused = ExternalConversation::new(AgentId::Acp("gemini".into()), conversation);
        assert!(refused.is_err(), "a codex session is not an ACP tab's");
    }

    #[test]
    fn an_agents_own_sessions_join_the_records_without_repeating_one() {
        let gem = AgentId::Acp("gem".into());
        let recorded = |id: &str, updated: &str| ResumableSession {
            provider: "acp:gem".into(),
            provider_session_id: id.into(),
            created_at: "1".into(),
            updated_at: updated.into(),
            title: Some(format!("record of {id}")),
            name: Some("my name".into()),
        };
        let listed = |id: &str, ms: Option<u64>| agent::ListedSession {
            session_id: id.into(),
            title: Some(format!("agent's {id}")),
            updated_at_ms: ms,
        };
        let merged = with_listed_sessions(
            &gem,
            vec![recorded("both", "3000"), recorded("mine", "1000")],
            vec![
                listed("both", Some(9000)),
                listed("theirs", Some(2000)),
                listed("untimed", None),
            ],
        );
        let ids: Vec<&str> = merged.iter().map(|s| s.provider_session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["both", "theirs", "mine", "untimed"],
            "newest first, a session with no time last"
        );
        assert_eq!(
            merged[0].name.as_deref(),
            Some("my name"),
            "a recorded session keeps its record's row"
        );
        assert_eq!(merged[0].updated_at, "3000");
        assert_eq!(merged[1].title.as_deref(), Some("agent's theirs"));
        assert_eq!(merged[1].provider, "acp:gem");
        assert_eq!(
            (merged[1].created_at.as_str(), merged[1].updated_at.as_str()),
            ("2000", "2000")
        );
        assert_eq!(merged[3].updated_at, "");
        // Another agent's record of the same id is a different session.
        let other = ResumableSession {
            provider: "acp:other".into(),
            ..recorded("theirs", "5")
        };
        let merged = with_listed_sessions(&gem, vec![other], vec![listed("theirs", Some(2000))]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn agent_ids_round_trip_their_record_names() {
        for agent in [AgentId::Claude, AgentId::Codex, AgentId::Acp("gemini".into())] {
            assert_eq!(AgentId::from_record_name(&agent.record_name()), Some(agent.clone()));
            let json = serde_json::to_string(&agent).unwrap();
            assert_eq!(serde_json::from_str::<AgentId>(&json).unwrap(), agent);
        }
        for bad in ["", "acp:", "acp: ", "acp:a\nb", "Codex", "openai"] {
            assert_eq!(AgentId::from_record_name(bad), None, "{bad:?}");
        }
    }

    fn plant(dir: &Path, provider: &str, session: &str) {
        let canonical = dir.canonicalize().unwrap();
        agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
            conversation_id: agent::conversation_id_for_cwd(&canonical),
            provider: provider.into(),
            provider_session_id: session.into(),
            canonical_cwd: canonical.to_string_lossy().into_owned(),
            created_at: "1000".into(),
            updated_at: "2000".into(),
            provider_advertised_resume: true,
            title: Some(format!("{provider} work")),
            name: None,
        })
        .unwrap();
    }

    #[test]
    fn resume_picker_lists_only_the_tab_agent() {
        let dir = workspace("picker-per-agent");
        plant(&dir, "claude", "claude-session");
        plant(&dir, "codex", "codex-thread");
        plant(&dir, "acp:gemini", "gemini-session");
        let ids = |agent: &AgentId| -> Vec<String> {
            BackendGreeting::for_agent(BackendKind::Sidecar, agent, dir.clone())
                .resumable
                .into_iter()
                .map(|s| s.provider_session_id)
                .collect()
        };
        assert_eq!(ids(&AgentId::Claude), vec!["claude-session".to_string()]);
        assert_eq!(ids(&AgentId::Codex), vec!["codex-thread".to_string()]);
        assert_eq!(ids(&AgentId::Acp("gemini".into())), vec!["gemini-session".to_string()]);
        // A Codex tab's records need no Claude backend that resumes.
        let legacy = BackendGreeting::for_agent(BackendKind::Legacy, &AgentId::Codex, dir.clone());
        assert_eq!(legacy.resumable.len(), 1);
        assert!(BackendGreeting::for_kind(BackendKind::Legacy, dir.clone())
            .resumable
            .is_empty());
        let mut external: Vec<String> = external_sessions(&dir)
            .into_iter()
            .map(|s| s.provider_session_id)
            .collect();
        external.sort();
        assert_eq!(external, vec!["codex-thread".to_string(), "gemini-session".to_string()]);
    }
}

#[cfg(test)]
mod plan_tests {
    //! A plan is settled only through `approve_plan` / `keep_planning`, checked before anything is sent.
    use super::*;
    use crate::test_providers::RecordingProvider;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const SHA: &str = "00000000000000000000000000000000000000000000000000000000000000aa";

    /// A session that offers plan mode, planning, with plan `p1` waiting.
    fn planning(label: &str) -> (Arc<RecordingProvider>, AgentBackend) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let provider = Arc::new(RecordingProvider::planning(true));
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        provider.queue_all(vec![
            AgentDomainEvent::CliPermissionMode {
                reported: "auto".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_enter".into(),
                name: "EnterPlanMode".into(),
                input: serde_json::json!({}),
            },
            AgentDomainEvent::CliPlanMode {
                source: "CliModeReported".into(),
            },
            AgentDomainEvent::PlanProposed {
                interaction_id: "p1".into(),
                tool_use_id: Some("toolu_exit".into()),
                plan: "# Plan".into(),
                plan_sha256: SHA.into(),
                allowed_prompts: Vec::new(),
                unsupported_reason: None,
            },
        ]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.projection().waiting_plan().is_none() {
            assert!(Instant::now() < deadline, "the plan never arrived");
            std::thread::sleep(Duration::from_millis(2));
        }
        (provider, backend)
    }

    #[test]
    fn approve_plan_validates_before_sending() {
        let (provider, mut backend) = planning("plan-validates");
        let refused = backend
            .approve_plan("p1", agent::PlanExecutionMode::Auto, "not-the-plan-read")
            .unwrap_err();
        assert!(refused.benign);
        assert!(
            refused.message.contains("not the plan that is waiting"),
            "{}",
            refused.message
        );
        assert!(backend.approve_plan("p9", agent::PlanExecutionMode::Auto, SHA).is_err());
        assert!(
            provider.interactions().is_empty(),
            "nothing is sent for a decision that cannot stand"
        );
        assert_eq!(backend.projection().planning.phase, agent::PlanPhase::Planning);
        let send = backend.approve_plan("p1", agent::PlanExecutionMode::Auto, SHA).unwrap();
        assert!(backend.finish_plan_send(&send).unwrap().is_ok());
        assert_eq!(provider.interactions().len(), 1);
        backend.shutdown();
    }

    #[test]
    fn a_refused_approval_keeps_the_plan_waiting() {
        let (provider, mut backend) = planning("plan-refused-approval");
        provider.refuse_interactions(Some(agent::ProviderErrorCode::InteractionAnswerRefused));
        let send = backend
            .approve_plan("p1", agent::PlanExecutionMode::Manual, SHA)
            .unwrap();
        let failure = backend.finish_plan_send(&send).unwrap().unwrap_err();
        assert!(failure.error.benign);
        assert!(!failure.outcome_unknown, "the agent said no: nothing may have happened");
        let projection = backend.projection();
        assert_eq!(projection.waiting_plan().unwrap().state, agent::PlanState::Waiting);
        assert_eq!(
            projection.planning.phase,
            agent::PlanPhase::Planning,
            "the move out is undone"
        );
        drop(projection);
        // A move into plan mode while it plans is refused before anything is sent.
        assert!(backend.set_planning(true, 0).is_err());
        assert!(provider.planning_calls().is_empty());
        backend.shutdown();
    }

    /// A plan the sidecar sent ahead of the CLI's `plan` report: an approval now would lose its move out of plan
    /// mode, so the conversation refuses it, benign, with nothing sent; "keep planning" needs no plan mode.
    #[test]
    fn an_approval_is_refused_until_the_cli_reports_plan_mode() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("plan-not-yet");
        let provider = Arc::new(RecordingProvider::planning(true));
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        provider.queue_all(vec![
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::PlanProposed {
                interaction_id: "p1".into(),
                tool_use_id: Some("toolu_exit".into()),
                plan: "# Plan".into(),
                plan_sha256: SHA.into(),
                allowed_prompts: Vec::new(),
                unsupported_reason: None,
            },
        ]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.projection().waiting_plan().is_none() {
            assert!(Instant::now() < deadline, "the plan never arrived");
            std::thread::sleep(Duration::from_millis(2));
        }
        let refused = backend
            .approve_plan("p1", agent::PlanExecutionMode::Manual, SHA)
            .unwrap_err();
        assert!(refused.benign);
        assert!(
            refused.message.contains("has not reported plan mode"),
            "{}",
            refused.message
        );
        assert!(provider.interactions().is_empty());
        assert_eq!(backend.projection().planning.phase, agent::PlanPhase::Executing);
        assert_eq!(
            backend.projection().waiting_plan().unwrap().state,
            agent::PlanState::Waiting
        );
        let kept = backend.keep_planning("p1", String::new()).unwrap();
        assert!(backend.finish_plan_send(&kept).unwrap().is_ok());
        assert_eq!(provider.interactions().len(), 1);
        backend.shutdown();
    }
}

#[cfg(test)]
mod image_tests;

#[cfg(test)]
mod question_tests {
    //! Only the person answers one of Claude's questions, through `answer_question` or
    //! `decline_question`. A question never enters `pending_permissions`, so nothing that answers
    //! cards -- Bypass on arrival, `answer_what_needs_no_human`, `approve_pending` -- reaches one.
    use super::*;
    use crate::test_providers::RecordingProvider;
    use agent::{
        InteractionResolution, QuestionAnswer, QuestionDeclineKind, QuestionOptionSpec, QuestionSpec, QuestionState,
        ResolveInteractionRequest, UiDelivery,
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn backend_on(provider: RecordingProvider, dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(provider);
        let conversation = AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    fn options(labels: &[&str]) -> Vec<QuestionOptionSpec> {
        labels
            .iter()
            .map(|label| QuestionOptionSpec {
                label: label.to_string(),
                description: String::new(),
                preview: None,
            })
            .collect()
    }

    /// A set of a single-select question and a multi-select one.
    fn asked(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::QuestionAsked {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            questions: vec![
                QuestionSpec {
                    question: "Which layout?".into(),
                    header: "Layout".into(),
                    multi_select: false,
                    options: options(&["Grid", "List"]),
                },
                QuestionSpec {
                    question: "Which checks?".into(),
                    header: "Checks".into(),
                    multi_select: true,
                    options: options(&["fmt", "clippy", "test"]),
                },
            ],
            unsupported_reason: None,
        }
    }

    /// A `Write` into `.git`: a card in every mode but Bypass.
    fn card(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        }
    }

    fn answer(question_index: u32, option_indexes: &[u32]) -> QuestionAnswer {
        QuestionAnswer {
            question_index,
            option_indexes: option_indexes.to_vec(),
            ..QuestionAnswer::default()
        }
    }

    fn state_of(backend: &AgentBackend, id: &str) -> Option<QuestionState> {
        backend.projection().question(id).map(|record| record.state.clone())
    }

    /// Takes deliveries in `mode` as the panel's pump does, answering what needs no human on the way,
    /// until `done`; returns every event delivered.
    fn deliver_until(
        backend: &mut AgentBackend,
        dir: &Path,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        what: &str,
        done: impl Fn(&AgentBackend, &[AgentDomainEvent]) -> bool,
    ) -> Vec<AgentDomainEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut delivered = Vec::new();
        while !done(backend, &delivered) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            if let UiDelivery::Events(events) = backend
                .take_revised_ui_delivery(
                    GateRoot::Local(dir),
                    &agent::PrefixRules::default(),
                    mode,
                    host_answered,
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
                delivered.extend(events);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        delivered
    }

    fn holds_question(delivered: &[AgentDomainEvent], id: &str) -> bool {
        delivered.iter().any(
            |event| matches!(event, AgentDomainEvent::QuestionAsked { interaction_id, .. } if interaction_id == id),
        )
    }

    /// No page draws anything from these: the host reads them from the projection, so they are kept
    /// from every delivery in every mode, the projection folds them all, and what follows still arrives.
    #[test]
    fn client_tool_events_never_reach_a_page() {
        let dir = workspace("client-tool-events");
        let call = |id: &str| AgentDomainEvent::ClientToolCall {
            call_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            tool: "open_file".into(),
            arguments_json: "{}".into(),
            sequence: 1,
        };
        for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
            let (provider, mut backend) = backend_on(RecordingProvider::default(), &dir);
            provider.queue(AgentDomainEvent::SessionServers {
                servers: vec![agent::InitServer {
                    name: "eitri".into(),
                    status: "connected".into(),
                    source: "sdk".into(),
                }],
            });
            provider.queue(call("c1"));
            provider.queue(AgentDomainEvent::ClientToolCallSettled { call_id: "c1".into() });
            provider.queue(call("c2"));
            provider.queue(asked("q1"));
            let delivered = deliver_until(
                &mut backend,
                &dir,
                mode,
                &mut BTreeSet::new(),
                "what follows the client tool events",
                |_, delivered| holds_question(delivered, "q1"),
            );
            assert!(
                !delivered.iter().any(|event| matches!(
                    event,
                    AgentDomainEvent::SessionServers { .. }
                        | AgentDomainEvent::ClientToolCall { .. }
                        | AgentDomainEvent::ClientToolCallSettled { .. }
                )),
                "{mode:?}: {delivered:?}"
            );
            let projection = backend.projection();
            assert_eq!(
                projection.open_client_calls.keys().collect::<Vec<_>>(),
                ["c2"],
                "{mode:?}"
            );
            assert!(projection.single_sdk_server("eitri"), "{mode:?}");
            drop(projection);
            backend.shutdown();
        }
    }

    #[test]
    fn bypass_never_answers_a_question() {
        let dir = workspace("question-bypass-on-arrival");
        let (provider, mut backend) = backend_on(RecordingProvider::default(), &dir);
        provider.queue_all(vec![card("c1"), asked("q1")]);
        let mut host_answered = BTreeSet::new();
        let delivered = deliver_until(
            &mut backend,
            &dir,
            PermissionMode::Bypass,
            &mut host_answered,
            "the card answered and the question delivered",
            |_, delivered| holds_question(delivered, "q1"),
        );
        assert!(holds_question(&delivered, "q1"), "the question reaches the page");
        assert_eq!(
            provider.resolutions(),
            vec![("c1".to_string(), true)],
            "bypass answers the card"
        );
        assert!(provider.interactions().is_empty(), "and nothing answers the question");
        assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Waiting));
        assert!(!backend.projection().pending_permissions.contains_key("q1"));
        backend.shutdown();
    }

    /// In every mode, and on a session whose CLI runs its own auto mode (where Auto defers cards to the
    /// CLI), the step that answers what needs no human leaves a question to the person.
    #[test]
    fn answer_what_needs_no_human_skips_a_question() {
        let dir = workspace("question-needs-a-human");
        for (provider, mode) in [
            (RecordingProvider::default(), PermissionMode::Manual),
            (RecordingProvider::default(), PermissionMode::Auto),
            (RecordingProvider::cli_auto(), PermissionMode::Auto),
        ] {
            let (provider, mut backend) = backend_on(provider, &dir);
            provider.queue(asked("q1"));
            deliver_until(
                &mut backend,
                &dir,
                mode,
                &mut BTreeSet::new(),
                "the question delivered",
                |_, delivered| holds_question(delivered, "q1"),
            );
            assert!(provider.interactions().is_empty(), "{mode:?}: nothing answered it");
            assert!(provider.decisions().is_empty(), "{mode:?}: nor took it for a card");
            assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Waiting), "{mode:?}");
            backend.shutdown();
        }
    }

    /// A remote tab's host answer goes out without the pump waiting for its reply. It is recorded once
    /// the agent took it, and a refused or lost one comes back as its card, never answered again.
    #[test]
    fn a_remote_tabs_host_answer_does_not_wait_for_its_reply() {
        let dir = workspace("remote-host-answer");
        let (provider, mut backend) = backend_on(RecordingProvider::default(), &dir);
        provider.hold_permission_replies();
        let mut host_answered = BTreeSet::new();
        let mut approvals = HumanApprovals::default();
        let mut take = |backend: &mut AgentBackend, host_answered: &mut BTreeSet<String>| {
            backend.take_revised_ui_delivery(
                GateRoot::RemoteWithoutFacts,
                &agent::PrefixRules::default(),
                PermissionMode::Bypass,
                host_answered,
                &mut approvals,
                &mut AnsweredForYou::default(),
            )
        };
        let holds_card = |delivery: &agent::RevisedDelivery, id: &str| {
            match delivery {
            agent::RevisedDelivery::Events(events) => events.iter().any(|(_, event)| {
                matches!(event, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == id)
            }),
            _ => false,
        }
        };
        let replies: [(&str, agent::InteractionReply); 2] = [
            ("p1", Ok(None)),
            ("p2", Err(agent::ProviderError::Transport("the link went away".into()))),
        ];
        for (id, reply) in replies {
            let refused = reply.is_err();
            provider.queue(card(id));
            let deadline = Instant::now() + Duration::from_secs(5);
            while provider.held_permissions() == 0 {
                assert!(Instant::now() < deadline, "{id}: the answer never went out");
                let delivery = take(&mut backend, &mut host_answered);
                assert!(!holds_card(&delivery, id), "{id}: no card while the answer is out");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!host_answered.contains(id), "{id}: not recorded before its reply");
            let delivery = take(&mut backend, &mut host_answered);
            assert!(
                !holds_card(&delivery, id),
                "{id}: nor answered twice nor carded meanwhile"
            );
            assert_eq!(provider.held_permissions(), 1, "{id}: sent once");
            provider.reply_to_held_permissions(reply);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(Instant::now() < deadline, "{id}: the reply was never read");
                let delivery = take(&mut backend, &mut host_answered);
                if refused {
                    if holds_card(&delivery, id) || matches!(delivery, agent::RevisedDelivery::Resync { .. }) {
                        assert!(!host_answered.contains(id), "{id}: a refused answer is not recorded");
                        assert!(backend.projection().pending_permissions.contains_key(id));
                        break;
                    }
                } else if host_answered.contains(id) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(provider.held_permissions(), 0, "{id}: not sent again");
        }
        backend.shutdown();
    }

    /// Entering Bypass and the bypass resync answer through `approve_pending`; handed a question's id
    /// among the cards', it answers the cards only.
    #[test]
    fn approve_pending_never_sees_a_question() {
        let dir = workspace("question-approve-pending");
        let (provider, mut backend) = backend_on(RecordingProvider::default(), &dir);
        provider.queue_all(vec![card("c1"), asked("q1")]);
        deliver_until(
            &mut backend,
            &dir,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            "the card and the question",
            |backend, delivered| {
                holds_question(delivered, "q1") && backend.projection().pending_permissions.contains_key("c1")
            },
        );
        let answered = backend.approve_pending(
            &["q1".to_string(), "c1".to_string()],
            "on entering bypass",
            false,
            &mut HumanApprovals::default(),
        );
        assert_eq!(answered.ids, vec!["c1".to_string()]);
        assert_eq!(provider.resolutions(), vec![("c1".to_string(), true)]);
        assert!(provider.interactions().is_empty());
        assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Waiting));
        backend.shutdown();
    }

    #[test]
    fn answer_question_validates_before_sending() {
        let dir = workspace("question-validates");
        let (provider, mut backend) = backend_on(RecordingProvider::default(), &dir);
        provider.queue(asked("q1"));
        deliver_until(
            &mut backend,
            &dir,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            "the question",
            |backend, _| state_of(backend, "q1").is_some(),
        );

        // Two choices on a single-select question, an option the question does not have, a question the
        // set does not have, a set that is not waiting: each refused here, and nothing sent.
        for (id, answers, why) in [
            ("q1", vec![answer(0, &[0, 1])], "one choice"),
            ("q1", vec![answer(1, &[3])], "no option 3"),
            ("q1", vec![answer(2, &[0])], "no question 2"),
            ("q9", vec![answer(0, &[0])], "no question with that id"),
        ] {
            let refused = backend.answer_question(id, answers).unwrap_err();
            assert!(refused.benign, "a refused answer leaves the session alone");
            assert!(refused.message.contains(why), "{why}: {}", refused.message);
            assert!(
                !refused.message.contains("provider error"),
                "told in its own words: {}",
                refused.message
            );
        }
        assert!(provider.interactions().is_empty(), "nothing reached the agent");
        assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Waiting));

        let mut typed = answer(1, &[0, 2]);
        typed.other_text = Some("bench".into());
        let given = vec![answer(0, &[1]), typed];
        let send = backend.answer_question("q1", given.clone()).unwrap();
        assert_eq!(send.interaction_id(), "q1");
        assert!(matches!(backend.finish_question_send(&send), Some(Ok(()))));
        assert_eq!(
            provider.interactions(),
            vec![ResolveInteractionRequest {
                session_id: "fake-session".into(),
                interaction_id: "q1".into(),
                resolution: InteractionResolution::Answers(given),
            }]
        );
        assert_eq!(
            state_of(&backend, "q1"),
            Some(QuestionState::Answering),
            "on its way until Claude's side settles it"
        );

        // One answer per question: a second, or a decline, while the first is on its way is refused here.
        let again = backend.answer_question("q1", vec![answer(0, &[0])]).unwrap_err();
        assert!(
            again.benign && again.message.contains("already on its way"),
            "{}",
            again.message
        );
        let decline = backend
            .decline_question("q1", QuestionDeclineKind::Cancel, Vec::new())
            .unwrap_err();
        assert!(decline.message.contains("already on its way"), "{}", decline.message);
        assert_eq!(provider.interactions().len(), 1);
        backend.shutdown();
    }

    #[test]
    fn a_refused_answer_keeps_the_question_waiting() {
        let dir = workspace("question-refused");
        let (provider, mut backend) = backend_on(RecordingProvider::default(), &dir);
        provider.queue(asked("q1"));
        deliver_until(
            &mut backend,
            &dir,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            "the question",
            |backend, _| state_of(backend, "q1").is_some(),
        );

        // The agent's own refusals: a rule of the set, and an id it no longer holds (a late answer).
        for code in [
            agent::ProviderErrorCode::InteractionAnswerRefused,
            agent::ProviderErrorCode::InteractionNotFound,
        ] {
            provider.refuse_interactions(Some(code));
            let send = backend.answer_question("q1", vec![answer(0, &[0])]).unwrap();
            let refused = backend.finish_question_send(&send).unwrap().unwrap_err();
            assert!(refused.benign, "{code:?}");
            assert_eq!(refused.message, "answers refused by the test", "a plain refusal");
            assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Waiting), "{code:?}");
        }
        let send = backend
            .decline_question("q1", QuestionDeclineKind::Chat, vec![answer(0, &[1])])
            .unwrap();
        let chat = backend.finish_question_send(&send).unwrap().unwrap_err();
        assert!(chat.benign);
        assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Waiting));
        assert_eq!(provider.interactions().len(), 3, "each refusal was the agent's");

        // Settled on Claude's side: the id is spent, and a later answer is refused here.
        provider.refuse_interactions(None);
        provider.queue(AgentDomainEvent::QuestionSettled {
            interaction_id: "q1".into(),
            outcome: agent::QuestionOutcome::Expired,
            answers: Vec::new(),
            reason: None,
        });
        deliver_until(
            &mut backend,
            &dir,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            "the settlement",
            |backend, _| matches!(state_of(backend, "q1"), Some(QuestionState::Settled { .. })),
        );
        let late = backend.answer_question("q1", vec![answer(0, &[0])]).unwrap_err();
        assert!(
            late.benign && late.message.contains("no longer waiting"),
            "{}",
            late.message
        );
        assert_eq!(provider.interactions().len(), 3, "nothing more was sent");
        backend.shutdown();
    }

    /// The answer goes out without waiting for the agent: while its reply is out the question stays
    /// `Answering` and reading the send says nothing yet; a reply that never came (a timeout, a lost
    /// connection) is a benign failure that puts the question back to waiting.
    #[test]
    fn an_answer_goes_out_without_waiting_and_a_failed_send_puts_the_question_back() {
        let dir = workspace("question-slow-agent");
        let (provider, mut backend) = backend_on(RecordingProvider::default(), &dir);
        provider.queue(asked("q1"));
        deliver_until(
            &mut backend,
            &dir,
            PermissionMode::Auto,
            &mut BTreeSet::new(),
            "the question",
            |backend, _| state_of(backend, "q1").is_some(),
        );
        provider.hold_interaction_replies(true);
        let send = backend.answer_question("q1", vec![answer(0, &[0])]).unwrap();
        assert!(backend.finish_question_send(&send).is_none(), "no reply yet");
        assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Answering));

        provider.reply_to_held_interactions(|| Err(agent::ProviderError::Timeout));
        let failed = backend.finish_question_send(&send).unwrap().unwrap_err();
        assert!(failed.benign, "the session goes on: {}", failed.message);
        assert!(failed.message.contains("waiting again"), "{}", failed.message);
        assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Waiting));

        // Answered again, the reply waited for by a caller that may block.
        let send = backend.answer_question("q1", vec![answer(0, &[1])]).unwrap();
        provider.reply_to_held_interactions(|| Ok(None));
        backend.wait_question_send(&send, Duration::from_secs(5)).unwrap();
        assert_eq!(state_of(&backend, "q1"), Some(QuestionState::Answering));
        backend.shutdown();
    }
}

/// A session's own declared tools at the gate: which calls are left to the editor broker, on what check,
/// and what every other `mcp__eitri__*` call gets.
#[cfg(test)]
mod eitri_gate_tests {
    use super::*;
    use crate::tab_set::{TabBackend, TabSet};
    use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider, RecordingProvider};
    use agent::{ClientToolDecl, ClientToolSet, InitServer, UiDelivery};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const OURS: &str = "mcp__eitri__open_file";
    const LINE: &str = "a server named eitri that is not Eitri's is loaded; its tools get no special treatment";

    #[test]
    fn remote_tab_gate_path_reads_no_file() {
        use crate::source_scan::{self, function_in};
        let policy = vec![source_scan::read_required("agent/src/permission_policy.rs")];
        for name in ["classify_without_a_boundary", "classify_by_name", "ask", "allow"] {
            let (_, body) = function_in(&policy, name);
            for forbidden in ["std::fs", "Identity::current", "resolve_in_root", "write_over_no_file"] {
                assert!(
                    !body.contains(forbidden),
                    "{name} reads local facts through {forbidden}"
                );
            }
            assert_eq!(
                source_scan::calls(&body, "boundary"),
                0,
                "{name} calls the local boundary check"
            );
            // Every callee is listed, so a new helper cannot hide a disk read outside the scanned body.
            let allowed = [
                "is_object",
                "Some",
                "ask",
                "allow",
                "contains",
                "matches",
                "classify_by_name",
                "unwrap_or_else",
                "reason",
            ];
            let bytes = body.as_bytes();
            for (at, _) in body.char_indices().filter(|(_, ch)| *ch == '(') {
                let mut end = at;
                while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                let mut start = end;
                while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
                    start -= 1;
                }
                if start != end {
                    let callee = &body[start..end];
                    assert!(allowed.contains(&callee), "{name} calls unscanned {callee}");
                }
            }
        }
        let backend = vec![source_scan::read_required("core/src/agent_backend.rs")];
        let compact = |text: &str| {
            text.chars()
                .filter(|ch| !ch.is_whitespace() && ![',', '{', '}'].contains(ch))
                .collect::<String>()
        };
        for (name, remote_arm) in [
            ("gate_root_is_a_boundary", "GateRoot::RemoteWithoutFacts=>false,"),
            ("decide", "GateRoot::RemoteWithoutFacts=>None,"),
            ("decide", "GateRoot::RemoteWithoutFacts=>agent::classify_without_a_boundary(tool_name,input,rules,mode==PermissionMode::Manual),"),
        ] {
            let (_, body) = function_in(&backend, name);
            assert!(compact(&body).contains(&compact(remote_arm)), "{name}: remote branch must remain free of local facts");
        }
        // Blank local match arms, then inspect everything a remote request can reach in the common body.
        fn without_local_arms(body: &str) -> (String, usize) {
            let mut spans = Vec::new();
            for (at, _) in body.match_indices("GateRoot::Local(") {
                let after_pattern = at + body[at..].find(')').unwrap() + 1;
                let tail = body[after_pattern..].trim_start();
                if !tail.starts_with("=>") && !tail.starts_with("if ") {
                    continue;
                }
                let expression = after_pattern + body[after_pattern..].find("=>").unwrap() + 2;
                let expression = expression + body[expression..].len() - body[expression..].trim_start().len();
                if body[expression..].starts_with('{') {
                    spans.push(at..expression + source_scan::braced(body, expression).len());
                    continue;
                }
                let mut depth = 0;
                let end = body[expression..]
                    .char_indices()
                    .find_map(|(offset, ch)| {
                        match ch {
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' | '}' => depth -= 1,
                            ',' if depth == 0 => return Some(expression + offset),
                            _ => {}
                        }
                        None
                    })
                    .unwrap_or(body.len());
                spans.push(at..end);
            }
            let count = spans.len();
            let mut common = body.to_string();
            for span in spans.into_iter().rev() {
                common.replace_range(span.clone(), &" ".repeat(span.len()));
            }
            (common, count)
        }
        for (name, expected_local_arms) in [("decide", 4), ("gate_root_is_a_boundary", 1)] {
            let (_, body) = function_in(&backend, name);
            let (common, removed) = without_local_arms(&body);
            assert_eq!(removed, expected_local_arms, "{name}: local arms must remain explicit");
            for forbidden in ["std::fs", "Identity::current"] {
                assert!(
                    !common.contains(forbidden),
                    "{name} reads {forbidden} outside a local arm"
                );
            }
            for forbidden in [
                "boundary",
                "resolve_in_root",
                "write_over_no_file",
                "root_is_a_boundary",
            ] {
                assert_eq!(
                    source_scan::calls(&common, forbidden),
                    0,
                    "{name} calls {forbidden} outside a local arm"
                );
            }
        }
        let (_, decide) = function_in(&backend, "decide");
        assert_eq!(source_scan::calls(&decide, "write_over_no_file"), 1);
        assert!(compact(&decide).contains(&compact(
            "GateRoot::Local(path)=>call.clone().filter(|_|write_over_no_file(tool_name,input,path))"
        )));
        let tabs = vec![source_scan::read_required("core/src/tab_set.rs")];
        let (_, events) = function_in(&tabs, "pump_arm_events");
        assert!(compact(&events).contains(&compact("if !live.remote { live_edits::collect(live, &events); }")));
        assert!(compact(&events).contains(&compact(
            "creates_file: if live.remote { Default::default() } else { note_new_files(&mut live.notes.creates_file, &events, project_root) }"
        )));
        assert_eq!(source_scan::calls(&events, "note_new_files"), 1);
        assert_eq!(source_scan::calls(&events, "collect"), 1);
        let (_, offers) = function_in(&tabs, "refresh_offers");
        let guard = offers
            .find("if tab.is_remote()")
            .expect("remote tabs must skip rule offers");
        let open = guard + offers[guard..].find('{').unwrap();
        let guarded = source_scan::braced(&offers, open);
        assert!(guarded.contains("return had;"));
        assert!(guard + guarded.len() < offers.find("permission_rules::offer").unwrap());
        let (_, answer) = function_in(&backend, "answer_what_needs_no_human");
        assert!(answer.contains("gate_root_is_a_boundary(project_root)"));
        for forbidden in ["std::fs", "Identity::current", "resolve_in_root", "write_over_no_file"] {
            assert!(
                !answer.contains(forbidden),
                "answer path reads local facts through {forbidden}"
            );
        }
    }

    fn remote_calls() -> Vec<(&'static str, serde_json::Value)> {
        use serde_json::json;
        vec![
            ("Read", json!({"file_path":"main.rs"})),
            ("Read", json!({"file_path":"/srv/project/main.rs"})),
            (
                "Edit",
                json!({"file_path":"main.rs", "old_string":"a", "new_string":"b"}),
            ),
            ("Write", json!({"file_path":"/srv/project/new.rs", "content":"x"})),
            ("Write", json!({"file_path":"new.rs", "content":"x"})),
            (
                "Edit",
                json!({"file_path":"/srv/project/main.rs", "old_string":"a", "new_string":"b"}),
            ),
            ("NotebookEdit", json!({"notebook_path":"main.ipynb"})),
            ("Grep", json!({"path":".", "pattern":"x"})),
            ("Grep", json!({"path":"/srv/project", "pattern":"x"})),
            ("Glob", json!({"pattern":"**/*.rs"})),
            ("Glob", json!({"path":"/srv/project", "pattern":"*.rs"})),
            ("Bash", json!({"command":"cat main.rs"})),
            ("Bash", json!({"command":"cat /srv/project/main.rs"})),
            ("Bash", json!({"command":"git -C . log"})),
            ("Bash", json!({"command":"git -C /srv/project log"})),
        ]
    }

    fn at_root(
        root: GateRoot<'_>,
        mode: PermissionMode,
        tool: &str,
        input: &serde_json::Value,
    ) -> Option<PlannedAnswer> {
        let rules = agent::PrefixRules::default()
            .with(agent::PrefixRule::parse("Bash(cat *)").unwrap())
            .with(agent::PrefixRule::parse("Bash(git *)").unwrap());
        decide(
            GateRequest {
                permission_id: "p1",
                tool_use_id: &Some("tu-1".into()),
                tool_name: tool,
                input,
                provider_prompt: None,
            },
            Judge {
                mode,
                external: false,
                defers_to_the_cli: false,
                own_tools: None,
                rules: &rules,
                project_root: root,
                approvals: &HumanApprovals::default(),
                planning: None,
                places: &agent::EitriPlaces::default(),
            },
        )
    }

    #[test]
    fn remote_tab_cards_every_path_judged_call_in_manual_and_auto() {
        for mode in [PermissionMode::Manual, PermissionMode::Auto] {
            for (tool, input) in remote_calls() {
                assert!(
                    at_root(GateRoot::RemoteWithoutFacts, mode, tool, &input).is_none(),
                    "{mode:?} {tool} {input}"
                );
            }
        }
    }

    #[test]
    fn remote_tab_bypass_allows_as_at_a_local_no_boundary_root() {
        for (tool, input) in remote_calls() {
            let remote = at_root(GateRoot::RemoteWithoutFacts, PermissionMode::Bypass, tool, &input).unwrap();
            let local = at_root(GateRoot::Local(Path::new("/")), PermissionMode::Bypass, tool, &input).unwrap();
            assert_eq!(remote.decision, local.decision);
            assert!(remote.creates_file.is_none(), "no laptop existence note even in bypass");
        }
    }

    #[test]
    fn remote_tab_name_allowed_tools_as_at_a_local_no_boundary_root() {
        for tool in agent::permission_policy::NEVER_ASK_TOOLS {
            for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
                let input = serde_json::json!({});
                let remote = at_root(GateRoot::RemoteWithoutFacts, mode, tool, &input).unwrap();
                let local = at_root(GateRoot::Local(Path::new("/")), mode, tool, &input).unwrap();
                assert_eq!(remote.decision, local.decision);
            }
        }
    }

    #[test]
    fn remote_tab_never_defers_to_the_cli_classifier() {
        use crate::agent_bridge::SessionModeChoice;
        use crate::remote_project::TabOrigin;
        let dir = workspace("remote-tab-gate");
        assert!(root_is_a_boundary(&dir));
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        set.set_project_origin(TabOrigin::Remote {
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
        })
        .unwrap();
        let tab = set.active();
        let (provider, backend) = session(RecordingProvider::client_tooling_cli_auto(), &dir, false);
        set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        provider.queue(cli_runs_auto());
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "remote-read".into(),
            tool_use_id: Some("tu-read".into()),
            tool_name: "Read".into(),
            input: serde_json::json!({"file_path": dir.join("main.rs")}),
            provider_prompt: None,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            set.pump(&dir, true);
            if set
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .pending_permissions
                .contains_key("remote-read")
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        set.pump(&dir, true);
        assert!(provider.decisions().is_empty());
        assert!(set.get(tab).unwrap().gate_answers().is_empty());
        assert!(!gate_root_is_a_boundary(set.get(tab).unwrap().gate_root(&dir)));
        close(&mut set, tab);
    }

    #[test]
    fn remote_tab_gate_ignores_the_laptop_disk() {
        let dir = workspace("remote-tab-laptop");
        let input = serde_json::json!({"file_path":dir.join("main.rs")});
        assert!(at_root(GateRoot::Local(&dir), PermissionMode::Auto, "Read", &input).is_some());
        assert!(at_root(GateRoot::RemoteWithoutFacts, PermissionMode::Auto, "Read", &input).is_none());
    }

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    /// A Claude session over `provider`, given the tool `open_file` or none.
    fn session(provider: RecordingProvider, dir: &Path, tools: bool) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(provider);
        let set = tools.then(|| ClientToolSet {
            server_name: SERVER_NAME.into(),
            timeout_ms: 900_000,
            tools: vec![ClientToolDecl {
                name: "open_file".into(),
                description: "opens a file".into(),
                input_schema_json: r#"{"type":"object"}"#.into(),
                always_load: true,
            }],
        });
        let conversation =
            AgentConversation::create_with_tools(provider.clone(), dir, ProjectTrust::Untrusted, set).unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    fn servers(entries: &[(&str, &str)]) -> AgentDomainEvent {
        AgentDomainEvent::SessionServers {
            servers: entries
                .iter()
                .map(|(name, source)| InitServer {
                    name: (*name).into(),
                    status: "connected".into(),
                    source: (*source).into(),
                })
                .collect(),
        }
    }

    fn only_ours() -> AgentDomainEvent {
        servers(&[(SERVER_NAME, "sdk")])
    }

    fn cli_runs_auto() -> AgentDomainEvent {
        AgentDomainEvent::CliPermissionMode {
            reported: "auto".into(),
        }
    }

    fn call(id: &str, tool: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: Some(format!("tu-{id}")),
            tool_name: tool.into(),
            input: serde_json::json!({ "path": "main.rs" }),
            provider_prompt: None,
        }
    }

    struct Answered {
        cards: Vec<String>,
        decisions: Vec<(String, PermissionDecision)>,
        gate_answers: Vec<GateAnswer>,
    }

    /// Takes deliveries until each of `ids` is a card or an answer. Most batches here leave nothing to
    /// deliver, so a helper that waits for a delivery would wait out its deadline.
    fn settle(
        provider: &RecordingProvider,
        backend: &mut AgentBackend,
        dir: &Path,
        mode: PermissionMode,
        ids: &[&str],
    ) -> Answered {
        settle_as(provider, backend, dir, mode, ids)
    }

    fn settle_as(
        provider: &RecordingProvider,
        backend: &mut AgentBackend,
        dir: &Path,
        mode: impl Into<GateMode> + Copy,
        ids: &[&str],
    ) -> Answered {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut answered = AnsweredForYou::default();
        let mut cards = Vec::new();
        let rules = agent::PrefixRules::default();
        loop {
            let delivery = backend
                .take_revised_ui_delivery(
                    GateRoot::Local(dir),
                    &rules,
                    mode,
                    &mut BTreeSet::new(),
                    &mut HumanApprovals::default(),
                    &mut answered,
                )
                .without_revisions();
            if let UiDelivery::Events(events) = delivery {
                cards.extend(events.into_iter().filter_map(|event| match event {
                    AgentDomainEvent::PermissionRequested { permission_id, .. } => Some(permission_id),
                    _ => None,
                }));
            }
            let decisions = provider.decisions();
            let done = |id: &&str| cards.iter().any(|c| c == id) || decisions.iter().any(|(d, _)| d == id);
            if ids.iter().all(done) {
                return Answered {
                    cards,
                    decisions,
                    gate_answers: answered.gate_answers,
                };
            }
            assert!(
                Instant::now() < deadline,
                "{ids:?} never settled: {cards:?} {decisions:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// One session from start to shutdown: `batch` folds as one, and one request `p1` for `tool` closes it.
    fn gated(
        provider: RecordingProvider,
        tools: bool,
        mode: PermissionMode,
        batch: Vec<AgentDomainEvent>,
        tool: &str,
    ) -> Answered {
        let dir = workspace("gate");
        let (provider, mut backend) = session(provider, &dir, tools);
        let mut events = batch;
        events.push(call("p1", tool));
        provider.queue_all(events);
        let answered = settle(&provider, &mut backend, &dir, mode, &["p1"]);
        backend.shutdown();
        answered
    }

    fn allowed(id: &str) -> Vec<(String, PermissionDecision)> {
        vec![(id.to_string(), PermissionDecision::Allow)]
    }

    /// Any other MCP tool's path, in the three places it differs from a session's own: a card in Manual,
    /// a card in Auto under Eitri's policy, and the CLI's classifier on a session that runs it.
    fn assert_unknown_tool(tools: bool, batch: Vec<AgentDomainEvent>, tool: &str) {
        for (mode, what) in [(PermissionMode::Manual, "Manual"), (PermissionMode::Auto, "Auto")] {
            let got = gated(RecordingProvider::client_tooling(), tools, mode, batch.clone(), tool);
            assert_eq!(got.cards, ["p1"], "{what}: a card");
            assert!(got.decisions.is_empty(), "{what}: {:?}", got.decisions);
        }
        let mut on_cli_auto = vec![cli_runs_auto()];
        on_cli_auto.extend(batch);
        let got = gated(
            RecordingProvider::client_tooling_cli_auto(),
            tools,
            PermissionMode::Auto,
            on_cli_auto,
            tool,
        );
        assert!(got.cards.is_empty(), "{:?}", got.cards);
        assert_eq!(got.decisions, [("p1".to_string(), PermissionDecision::Defer)]);
    }

    /// What `decide` says of one request at `root`, with these places and no declared tools.
    fn decided_at(
        root: &Path,
        mode: PermissionMode,
        defers_to_the_cli: bool,
        planning: Option<PlanGate>,
        tool: &str,
        input: &serde_json::Value,
        places: &agent::EitriPlaces,
    ) -> Option<PlannedAnswer> {
        let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(cp *)").unwrap());
        decide(
            GateRequest {
                permission_id: "p1",
                tool_use_id: &Some("tu-1".into()),
                tool_name: tool,
                input,
                provider_prompt: None,
            },
            Judge {
                mode,
                external: false,
                defers_to_the_cli,
                own_tools: None,
                rules: &rules,
                project_root: GateRoot::Local(root),
                approvals: &HumanApprovals::default(),
                planning,
                places,
            },
        )
    }

    /// Every mode but Bypass cards a write into Eitri's own places: Manual, Auto on Eitri's policy, Auto on an
    /// auto session (where every other call is left to the CLI), and plan mode (where a main-thread `Write` is left to
    /// the CLI's plan mode). Bypass allows it, as it allows a write into the other protected paths.
    #[test]
    fn a_write_into_eitris_own_places_is_a_card_in_every_mode_but_bypass() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let root = agent::state_dirs::test_workspace_dir("eitri-places-decide");
        // Whatever the umask: a root others can write to is no boundary.
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let places = agent::EitriPlaces::from_values(&agent::EitriPlaceInputs {
            home: Some(std::ffi::OsStr::new("/h")),
            ..Default::default()
        });
        let plan = |origin| {
            Some(PlanGate {
                origin,
                cli_runs_auto: true,
                main_thread: true,
            })
        };
        let calls = [
            (
                "Write",
                serde_json::json!({ "file_path": "/h/.config/eitri/init.lua", "content": "x" }),
            ),
            (
                "Bash",
                serde_json::json!({ "command": "cp evil.lua /h/.config/eitri/init.lua" }),
            ),
            (
                "Bash",
                serde_json::json!({ "command": "echo x > ~/.local/share/eitri/plugins/md/main.lua" }),
            ),
        ];
        for (tool, input) in &calls {
            for (mode, defers, planning) in [
                (PermissionMode::Manual, false, None),
                (PermissionMode::Auto, false, None),
                (PermissionMode::Auto, true, None),
                (PermissionMode::Auto, true, plan(PermissionMode::Auto)),
                (PermissionMode::Manual, false, plan(PermissionMode::Manual)),
                (PermissionMode::Bypass, true, plan(PermissionMode::Bypass)),
            ] {
                assert!(
                    decided_at(&root, mode, defers, planning, tool, input, &places).is_none(),
                    "{mode:?} defers={defers} plan={planning:?} {tool} {input}"
                );
            }
            let bypass = decided_at(&root, PermissionMode::Bypass, true, None, tool, input, &places).unwrap();
            assert_eq!(bypass.decision, PermissionDecision::Allow, "{tool} {input}");
        }
        // With no places the same calls are left to the CLI, so the cards above are the places' own.
        let none = agent::EitriPlaces::default();
        let (tool, input) = &calls[0];
        let deferred = decided_at(&root, PermissionMode::Auto, true, None, tool, input, &none).unwrap();
        assert_eq!(deferred.decision, PermissionDecision::Defer);
        let in_plan = decided_at(
            &root,
            PermissionMode::Auto,
            true,
            plan(PermissionMode::Auto),
            tool,
            input,
            &none,
        );
        assert_eq!(in_plan.unwrap().decision, PermissionDecision::Defer);
    }

    /// What `decide` says about one request, with the declared set in hand: the log line tells the broker's
    /// answer from bypass's, which are the same `allow`.
    fn decided(mode: PermissionMode, own: bool) -> Option<String> {
        let declared: BTreeSet<String> = ["open_file".to_string()].into();
        let (id, tool_use_id, input) = ("p1", Some("tu-1".to_string()), serde_json::json!({}));
        decide(
            GateRequest {
                permission_id: id,
                tool_use_id: &tool_use_id,
                tool_name: OURS,
                input: &input,
                provider_prompt: None,
            },
            Judge {
                mode,
                external: false,
                defers_to_the_cli: false,
                own_tools: own.then_some(&declared),
                rules: &agent::PrefixRules::default(),
                project_root: GateRoot::Local(Path::new("/nonexistent")),
                approvals: &HumanApprovals::default(),
                planning: None,
                places: &agent::EitriPlaces::default(),
            },
        )
        .map(|planned| planned.sent)
    }

    #[test]
    fn our_tool_is_allowed_before_the_defer_branch_on_an_auto_session() {
        let got = gated(
            RecordingProvider::client_tooling_cli_auto(),
            true,
            PermissionMode::Auto,
            vec![cli_runs_auto(), only_ours()],
            OURS,
        );
        assert_eq!(got.decisions, allowed("p1"), "answered here, never deferred");
        assert!(got.cards.is_empty(), "{:?}", got.cards);
        assert_eq!(got.gate_answers.len(), 1);
        assert!(!got.gate_answers[0].deferred);
        assert_eq!(got.gate_answers[0].origin, GateOrigin::GateRequest);
    }

    /// `classify_in_manual` cards any MCP tool, so an allow here means it was never asked.
    #[test]
    fn in_manual_too() {
        for (provider, mut batch) in [
            (RecordingProvider::client_tooling(), vec![]),
            (RecordingProvider::client_tooling_cli_auto(), vec![cli_runs_auto()]),
        ] {
            batch.push(only_ours());
            let got = gated(provider, true, PermissionMode::Manual, batch, OURS);
            assert_eq!(got.decisions, allowed("p1"));
            assert!(got.cards.is_empty(), "{:?}", got.cards);
        }
    }

    #[test]
    fn in_bypass_too() {
        let got = gated(
            RecordingProvider::client_tooling(),
            true,
            PermissionMode::Bypass,
            vec![only_ours()],
            OURS,
        );
        assert_eq!(got.decisions, allowed("p1"));
        let broker = format!("left to the editor broker: {OURS}");
        assert_eq!(decided(PermissionMode::Bypass, true), Some(broker), "ahead of bypass");
        assert_eq!(
            decided(PermissionMode::Bypass, false),
            Some(format!("allowed in bypass: {OURS}")),
            "the premise: bypass allows it too, under its own line"
        );
    }

    #[test]
    fn no_declared_tools_is_an_unknown_mcp_tool() {
        assert_unknown_tool(false, vec![only_ours()], OURS);
    }

    #[test]
    fn two_servers_named_eitri_are_an_unknown_mcp_tool() {
        assert_unknown_tool(true, vec![servers(&[("eitri", "sdk"), ("eitri", "sdk")])], OURS);
    }

    #[test]
    fn a_project_eitri_shows_one_entry_of_source_project_and_is_unknown() {
        assert_unknown_tool(true, vec![servers(&[("eitri", "project")])], OURS);
    }

    #[test]
    fn a_dynamic_eitri_likewise() {
        assert_unknown_tool(true, vec![servers(&[("eitri", "dynamic")])], OURS);
    }

    #[test]
    fn a_user_scope_eitri_is_an_unknown_mcp_tool() {
        assert_unknown_tool(true, vec![servers(&[("eitri", "user")])], OURS);
    }

    #[test]
    fn a_missing_source_fails_the_check() {
        assert_unknown_tool(true, vec![servers(&[("eitri", "")])], OURS);
    }

    #[test]
    fn no_session_servers_yet_is_an_unknown_mcp_tool() {
        assert_unknown_tool(true, vec![], OURS);
    }

    /// A report folded while a batch is answered reaches the next request of that batch.
    #[test]
    fn the_check_reads_the_latest_init_for_each_request() {
        for mode in [PermissionMode::Manual, PermissionMode::Auto] {
            let dir = workspace("gate-latest-init");
            let (provider, mut backend) = session(RecordingProvider::client_tooling(), &dir, true);
            provider.fold_after_next_resolution(vec![servers(&[("eitri", "project")])]);
            provider.queue_all(vec![only_ours(), call("p1", OURS), call("p2", OURS)]);
            let got = settle(&provider, &mut backend, &dir, mode, &["p1", "p2"]);
            assert_eq!(got.decisions, allowed("p1"), "{mode:?}");
            assert_eq!(got.cards, ["p2"], "{mode:?}");
            backend.shutdown();
        }
    }

    #[test]
    fn a_tool_outside_the_declared_list_is_an_unknown_mcp_tool() {
        for tool in [
            "mcp__eitri__x__y",
            "mcp__eitri__nvim_exec",
            "mcp__eitri__open_file_again",
            "mcp__eitri__",
            "mcp__eitri2__open_file",
            "mcp__other__open_file",
        ] {
            assert_unknown_tool(true, vec![only_ours()], tool);
        }
    }

    /// The batch a change to a model that cannot run auto delivers on a session that ran it.
    fn left_auto() -> Vec<AgentDomainEvent> {
        vec![
            cli_runs_auto(),
            AgentDomainEvent::CliPermissionMode {
                reported: "default".into(),
            },
            AgentDomainEvent::CliLeftAuto,
        ]
    }

    fn on_cli_auto(mode: PermissionMode, batch: Vec<AgentDomainEvent>, tool: &str) -> Answered {
        gated(RecordingProvider::client_tooling_cli_auto(), false, mode, batch, tool)
    }

    /// A call of `tool` on a file inside the project, with everything the tool needs: the input Auto and Manual
    /// answer differently, where a call with no path is a card in both.
    fn project_edit(dir: &Path, id: &str, tool: &str) -> AgentDomainEvent {
        let file = dir.join("main.rs").display().to_string();
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: Some(format!("tu-{id}")),
            tool_name: tool.into(),
            input: serde_json::json!({
                "file_path": file, "notebook_path": file, "content": "x", "new_source": "x",
                "old_string": "fn main", "new_string": "fn start",
            }),
            provider_prompt: None,
        }
    }

    fn edit_on_cli_auto(mode: impl Into<GateMode> + Copy, batch: Vec<AgentDomainEvent>, tool: &str) -> Answered {
        let dir = workspace("gate-project-edit");
        let (provider, mut backend) = session(RecordingProvider::client_tooling_cli_auto(), &dir, false);
        let mut events = batch;
        events.push(project_edit(&dir, "p1", tool));
        provider.queue_all(events);
        let answered = settle_as(&provider, &mut backend, &dir, mode, &["p1"]);
        backend.shutdown();
        answered
    }

    /// The CLI out of auto with no word of a move: Eitri's own Auto policy, which lets an edit inside the project
    /// run. What makes the tests below able to fail.
    fn fell_back() -> Vec<AgentDomainEvent> {
        vec![AgentDomainEvent::CliPermissionMode {
            reported: "default".into(),
        }]
    }

    /// The tools a tab may be asked about: the ones each mode tells apart, and one it cannot know.
    const SWITCH_TOOLS: [&str; 7] = [
        "Write",
        "Edit",
        "NotebookEdit",
        "Bash",
        "Read",
        "WebFetch",
        "mcp__other__tool",
    ];

    #[test]
    fn the_premise_on_a_session_in_auto_a_gate_request_is_left_to_the_cli() {
        for tool in SWITCH_TOOLS {
            let got = on_cli_auto(PermissionMode::Auto, vec![cli_runs_auto()], tool);
            assert_eq!(got.decisions, [("p1".to_string(), PermissionDecision::Defer)], "{tool}");
        }
    }

    /// From the moment the CLI leaves auto nothing is left to a classifier that no longer runs, and an Auto
    /// tab answers what a Manual tab answers, in the very batch that says so.
    #[test]
    fn out_of_auto_an_auto_tab_answers_as_a_manual_tab_and_defers_nothing() {
        for tool in SWITCH_TOOLS {
            let auto = on_cli_auto(PermissionMode::Auto, left_auto(), tool);
            let manual = on_cli_auto(PermissionMode::Manual, left_auto(), tool);
            assert!(
                !auto.decisions.iter().any(|(_, d)| *d == PermissionDecision::Defer),
                "{tool}: {:?}",
                auto.decisions
            );
            assert_eq!(auto.cards, manual.cards, "{tool}");
            assert_eq!(auto.decisions, manual.decisions, "{tool}");
        }
    }

    #[test]
    fn the_premise_an_auto_tab_under_eitris_own_policy_lets_a_project_edit_run() {
        for tool in ACCEPT_EDITS_TOOLS {
            let got = edit_on_cli_auto(PermissionMode::Auto, fell_back(), tool);
            assert_eq!(got.decisions, allowed("p1"), "{tool}");
            assert!(got.cards.is_empty(), "{tool}");
        }
    }

    /// A switch never widens what runs without a card: a file edit, which the classifier could still refuse
    /// a moment ago, is a card now, on an Auto tab as on a Manual one.
    #[test]
    fn out_of_auto_a_file_edit_is_a_card() {
        for tool in ACCEPT_EDITS_TOOLS {
            for mode in [PermissionMode::Auto, PermissionMode::Manual] {
                let got = edit_on_cli_auto(mode, left_auto(), tool);
                assert_eq!(got.cards, ["p1"], "{tool} in {mode:?}");
                assert!(got.decisions.is_empty(), "{tool} in {mode:?}: {:?}", got.decisions);
            }
        }
    }

    /// Whichever route said so: a `SessionReady` saying `default` on a session last seen in auto is the move.
    #[test]
    fn a_report_of_default_after_auto_with_no_move_named_is_the_move() {
        let mut batch = vec![cli_runs_auto()];
        batch.extend(fell_back());
        let got = edit_on_cli_auto(PermissionMode::Auto, batch, "Write");
        assert_eq!(got.cards, ["p1"]);
        assert!(got.decisions.is_empty(), "{:?}", got.decisions);
    }

    /// After the tab followed, a person who put it back in Auto chose Eitri's own Auto policy.
    #[test]
    fn a_tab_that_followed_and_was_cycled_back_answers_as_auto() {
        let cycled_back = GateMode {
            mode: PermissionMode::Auto,
            followed_out_of_auto: true,
            plan_followed: true,
        };
        let got = edit_on_cli_auto(cycled_back, left_auto(), "Write");
        assert_eq!(got.decisions, allowed("p1"));
        assert!(got.cards.is_empty());
    }

    /// The batch order does not matter: a request ahead of the report in one batch is answered knowing it.
    #[test]
    fn a_request_queued_ahead_of_the_report_is_answered_by_it() {
        let dir = workspace("gate-left-auto-order");
        let (provider, mut backend) = session(RecordingProvider::client_tooling_cli_auto(), &dir, false);
        provider.queue_all(vec![
            cli_runs_auto(),
            project_edit(&dir, "p1", "Write"),
            AgentDomainEvent::CliPermissionMode {
                reported: "default".into(),
            },
            AgentDomainEvent::CliLeftAuto,
        ]);
        let got = settle(&provider, &mut backend, &dir, PermissionMode::Auto, &["p1"]);
        assert_eq!(got.cards, ["p1"]);
        assert!(got.decisions.is_empty(), "{:?}", got.decisions);
        backend.shutdown();
    }

    /// A move folded while a batch is answered reaches the batch's next request.
    #[test]
    fn a_move_folded_while_a_batch_is_answered_reaches_its_next_request() {
        let dir = workspace("gate-left-auto-late");
        let (provider, mut backend) = session(RecordingProvider::client_tooling_cli_auto(), &dir, false);
        provider.fold_after_next_resolution(vec![
            AgentDomainEvent::CliPermissionMode {
                reported: "default".into(),
            },
            AgentDomainEvent::CliLeftAuto,
        ]);
        provider.queue_all(vec![
            cli_runs_auto(),
            project_edit(&dir, "p1", "Write"),
            project_edit(&dir, "p2", "Write"),
        ]);
        let got = settle(&provider, &mut backend, &dir, PermissionMode::Auto, &["p1", "p2"]);
        assert_eq!(got.decisions, [("p1".to_string(), PermissionDecision::Defer)]);
        assert_eq!(got.cards, ["p2"]);
        backend.shutdown();
    }

    #[test]
    fn bypass_is_what_it_was_before_the_move() {
        for tool in SWITCH_TOOLS {
            let got = on_cli_auto(PermissionMode::Bypass, left_auto(), tool);
            assert_eq!(got.decisions, allowed("p1"), "{tool}");
        }
    }

    /// The CLI saying `default` with no move, on a session that never ran auto, keeps today's answers: Eitri's
    /// own policy, and the tab where the person put it.
    #[test]
    fn a_session_that_never_ran_auto_has_nothing_to_follow() {
        let got = on_cli_auto(
            PermissionMode::Auto,
            vec![AgentDomainEvent::CliPermissionMode {
                reported: "default".into(),
            }],
            "Bash",
        );
        assert!(!got.decisions.iter().any(|(_, d)| *d == PermissionDecision::Defer));
    }

    /// A report folded ahead of the batch that names it, with the tab not yet told: nothing but the tab can say
    /// it has followed, so the request is answered as Manual, a card.
    #[test]
    fn a_report_folded_before_the_tab_has_followed_is_a_card_with_no_event_in_the_batch() {
        let dir = workspace("gate-left-auto-unfollowed");
        let (provider, mut backend) = session(RecordingProvider::client_tooling_cli_auto(), &dir, false);
        provider.queue_all(left_auto());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !backend.cli_left_auto() {
            assert!(Instant::now() < deadline, "the report was never folded");
            let _ = backend.take_revised_ui_delivery(
                GateRoot::Local(&dir),
                &agent::PrefixRules::default(),
                PermissionMode::Auto,
                &mut BTreeSet::new(),
                &mut HumanApprovals::default(),
                &mut AnsweredForYou::default(),
            );
        }
        provider.queue_all(vec![project_edit(&dir, "p1", "Write")]);
        let got = settle(&provider, &mut backend, &dir, PermissionMode::Auto, &["p1"]);
        assert_eq!(got.cards, ["p1"]);
        assert!(got.decisions.is_empty(), "{:?}", got.decisions);
        backend.shutdown();
    }

    /// The tab shows what it does: it moves to manual when the batch is pumped, says why once, and a person
    /// may cycle it back, which holds.
    #[test]
    fn the_tab_follows_the_cli_out_of_auto_once_and_says_so() {
        let dir = workspace("tab-left-auto");
        let (provider, backend) = session(RecordingProvider::client_tooling_cli_auto(), &dir, false);
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = tab_over(&mut set, backend);
        provider.queue_all(left_auto());
        let deadline = Instant::now() + Duration::from_secs(5);
        while set.get(tab).unwrap().mode() != SessionModeChoice::Manual {
            assert!(Instant::now() < deadline, "the tab never followed");
            set.pump(&dir, true);
        }
        assert_eq!(set.take_left_auto_notes(), [tab]);
        assert_eq!(set.take_left_auto_notes(), [], "once");
        set.get_mut(tab).unwrap().set_mode_for_test(SessionModeChoice::Auto);
        for _ in 0..5 {
            set.pump(&dir, true);
        }
        assert_eq!(
            set.get(tab).unwrap().mode(),
            SessionModeChoice::Auto,
            "the person's choice holds"
        );
        close(&mut set, tab);
    }

    #[test]
    fn a_bypass_tab_stays_in_bypass_through_the_move() {
        let dir = workspace("tab-left-auto-bypass");
        let (provider, backend) = session(RecordingProvider::client_tooling_cli_auto(), &dir, false);
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Bypass);
        let tab = tab_over(&mut set, backend);
        set.get_mut(tab).unwrap().set_mode_for_test(SessionModeChoice::Bypass);
        provider.queue_all(left_auto());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            set.pump(&dir, true);
            let TabBackend::Live(backend) = set.get(tab).unwrap().backend() else {
                panic!("the tab lost its session");
            };
            if backend.cli_left_auto() {
                break;
            }
            assert!(Instant::now() < deadline, "the CLI report never folded");
            std::thread::yield_now();
        }
        for _ in 0..50 {
            set.pump(&dir, true);
            assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass);
        }
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass);
        assert_eq!(set.take_left_auto_notes(), []);
        close(&mut set, tab);
    }

    fn tab_over(set: &mut TabSet, backend: AgentBackend) -> crate::tabs::TabId {
        let tab = set.active();
        set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        tab
    }

    fn close(set: &mut TabSet, tab: crate::tabs::TabId) {
        if let TabBackend::Live(mut backend) = set.get_mut(tab).unwrap().set_backend(TabBackend::NotStarted) {
            backend.shutdown();
        }
    }

    /// Pumps until the session's latest report is `want`, and once more.
    fn pump_past_report(set: &mut TabSet, tab: crate::tabs::TabId, dir: &Path, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            set.pump(dir, true);
            let TabBackend::Live(backend) = set.get(tab).unwrap().backend() else {
                panic!("the tab lost its session");
            };
            if backend
                .projection()
                .init_servers
                .iter()
                .any(|server| server.source == want)
            {
                break;
            }
            assert!(Instant::now() < deadline, "the report never folded");
            std::thread::sleep(Duration::from_millis(2));
        }
        set.pump(dir, true);
    }

    #[test]
    fn a_foreign_eitri_is_said_once() {
        let dir = workspace("gate-said-once");
        let (provider, backend) = session(RecordingProvider::client_tooling(), &dir, true);
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = tab_over(&mut set, backend);
        set.pump(&dir, true);
        assert!(set.take_editor_notes().is_empty(), "no report yet");

        provider.queue(servers(&[("eitri", "project")]));
        pump_past_report(&mut set, tab, &dir, "project");
        assert_eq!(set.take_editor_notes(), [(tab, LINE.to_string())]);
        assert!(set.take_editor_notes().is_empty(), "taken");

        provider.queue(servers(&[("eitri", "dynamic")]));
        pump_past_report(&mut set, tab, &dir, "dynamic");
        assert!(
            set.take_editor_notes().is_empty(),
            "a later report of the same session says nothing"
        );
        close(&mut set, tab);
    }

    /// A line taken with no page to show it comes back with the next take, and the pump does not make a
    /// second one.
    #[test]
    fn a_line_handed_back_is_still_said_once() {
        let dir = workspace("gate-handed-back");
        let (provider, backend) = session(RecordingProvider::client_tooling(), &dir, true);
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = tab_over(&mut set, backend);
        provider.queue(servers(&[("eitri", "project")]));
        pump_past_report(&mut set, tab, &dir, "project");
        assert_eq!(set.take_editor_notes().len(), 1);
        set.owe_editor_note(tab);
        set.pump(&dir, true);
        assert_eq!(set.take_editor_notes(), [(tab, LINE.to_string())]);
        assert!(set.take_editor_notes().is_empty());
        close(&mut set, tab);
    }

    /// What a provider owes is taken from the backend by the pump, so it is the tab's, in the provider's words.
    #[test]
    fn a_providers_line_is_its_tabs_and_is_said_once() {
        use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        let dir = workspace("gate-provider-line");
        let provider = Arc::new(ExternalRecordingProvider::codex());
        let backend = AgentBackend::start_external_with(
            AgentId::Codex,
            provider.clone(),
            &dir,
            empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .expect("a scripted external start succeeds");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = tab_over(&mut set, backend);
        set.pump(&dir, true);
        assert!(set.take_editor_notes().is_empty(), "nothing is owed");

        provider.owe_editor_note("the tools are off");
        set.pump(&dir, true);
        let said = set.take_editor_notes();
        assert_eq!(said, [(tab, "the tools are off".to_string())]);
        set.owe_editor_note(tab);
        set.pump(&dir, true);
        assert_eq!(set.take_editor_notes(), said, "handed back in the same words");
        assert!(set.take_editor_notes().is_empty(), "once");
        close(&mut set, tab);
    }

    #[test]
    fn our_own_server_and_a_session_with_no_tools_say_nothing() {
        for (tools, report) in [
            (true, servers(&[("eitri", "sdk")])),
            (true, servers(&[("other", "project")])),
            (false, servers(&[("eitri", "project")])),
        ] {
            let dir = workspace("gate-says-nothing");
            let (provider, backend) = session(RecordingProvider::client_tooling(), &dir, tools);
            let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            let tab = tab_over(&mut set, backend);
            let AgentDomainEvent::SessionServers { servers: entries } = &report else {
                unreachable!()
            };
            let source = entries[0].source.clone();
            provider.queue(report);
            pump_past_report(&mut set, tab, &dir, &source);
            assert!(set.take_editor_notes().is_empty(), "tools {tools}, {source}");
            close(&mut set, tab);
        }
    }

    /// The shape of `manual_never_answers_what_auto_leaves_to_a_human`, on sessions that declare a tool:
    /// the mock backends of that test declare none, so rows added there pass whatever `decide` does.
    #[test]
    fn manual_and_auto_answer_our_tools_alike() {
        for auto_session in [false, true] {
            let run = |mode: PermissionMode, report: AgentDomainEvent| {
                let mut batch = if auto_session { vec![cli_runs_auto()] } else { vec![] };
                batch.push(report);
                let provider = if auto_session {
                    RecordingProvider::client_tooling_cli_auto()
                } else {
                    RecordingProvider::client_tooling()
                };
                gated(provider, true, mode, batch, OURS)
            };
            for report in [only_ours(), servers(&[("eitri", "project")])] {
                let holds =
                    matches!(&report, AgentDomainEvent::SessionServers { servers } if servers[0].source == "sdk");
                let auto = run(PermissionMode::Auto, report.clone());
                let manual = run(PermissionMode::Manual, report);
                let manual_allowed = manual.decisions == allowed("p1");
                assert!(
                    !manual_allowed || !auto.decisions.is_empty(),
                    "auto session {auto_session}: Manual allowed what Auto put to a human"
                );
                if !auto.cards.is_empty() {
                    assert_eq!(manual.cards, ["p1"], "auto session {auto_session}");
                }
                assert!(
                    manual.decisions.iter().all(|(_, d)| *d == PermissionDecision::Allow),
                    "Manual never defers: {:?}",
                    manual.decisions
                );
                if holds {
                    assert_eq!(auto.decisions, allowed("p1"), "auto session {auto_session}");
                    assert_eq!(manual.decisions, allowed("p1"), "auto session {auto_session}");
                } else {
                    assert_eq!(manual.cards, ["p1"], "a failed check is a card in Manual");
                    assert!(manual.decisions.is_empty(), "{:?}", manual.decisions);
                }
            }
        }
    }

    #[test]
    fn an_external_agent_is_not_touched() {
        for (mode, id) in [(PermissionMode::Auto, "p1"), (PermissionMode::Manual, "p2")] {
            let dir = workspace("gate-external");
            let provider = Arc::new(ExternalRecordingProvider::codex());
            let mut backend = AgentBackend::start_external_with(
                AgentId::Codex,
                provider.clone(),
                &dir,
                empty_trust_plan(),
                SessionModeChoice::Auto,
                None,
            )
            .expect("a scripted external start succeeds");
            provider.inner.queue_all(vec![only_ours(), call(id, OURS)]);
            let got = settle(&provider.inner, &mut backend, &dir, mode, &[id]);
            assert_eq!(got.cards, [id], "{mode:?}");
            assert!(got.decisions.is_empty(), "{mode:?}: {:?}", got.decisions);
            backend.shutdown();
        }
    }
}
