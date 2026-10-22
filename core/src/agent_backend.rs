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
use agent::{
    AgentConversation, AgentDomainEvent, AgentSession, AgentSessionProjection, ClaudeSidecarProvider,
    ConversationError, PermissionDecision, PermissionMode, ProjectionGuard, ProviderCapabilities, ProviderInfo,
    ResumableSession, Reviewer, RevisedDelivery, TrustPlan, TurnPolicy,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::agent_bridge::SessionModeChoice;

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
    // Its CLI always runs `default` (`--permission-mode default`), and its hook relay has no way
    // to say "no decision": every request it raises is answered with Eitri's own policy.
    cli_auto_mode: false,
    steer: false,
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
    assert!(
        !LEGACY_CAPABILITIES.cli_auto_mode,
        "the legacy backend never runs the CLI's auto mode and cannot defer"
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
pub fn turn_policy(mode: SessionModeChoice) -> TurnPolicy {
    TurnPolicy {
        reviewer: match mode {
            SessionModeChoice::Auto => Reviewer::Agent,
            SessionModeChoice::Manual | SessionModeChoice::Bypass => Reviewer::User,
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
        AgentBackend::Conversation {
            agent: external.agent,
            conversation: Box::new(external.conversation),
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

    /// Reads the backend environment variable and `agent::LEGACY_BACKEND_COMPILED`.
    ///
    /// `legacy_flag` carries `main()`'s `--legacy` value. Passing it in keeps the selection testable,
    /// as with `explicit` in [`choose`].
    ///
    /// Unrecognized values and missing sidecar artifacts select the sidecar. A typo emits a warning.
    /// Legacy requires an explicit `--legacy` flag or the environment value `legacy`, and a build
    /// with legacy support compiled in. Otherwise the explicit legacy request returns `Err`.
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

    /// [`from_env`](Self::from_env)'s decision over caller-supplied values.
    /// Inputs keep tests from mutating the shared process environment or reading an override
    /// from a different source. `legacy_compiled` lets one test binary exercise both build modes.
    ///
    /// `legacy_flag` or `explicit = Some("legacy")` selects legacy only when it is compiled in.
    /// Otherwise the result is an error naming [`agent::LEGACY_NOT_IN_BUILD`] and the source
    /// of the request. The flag takes precedence because the environment may be inherited.
    /// All other values select the sidecar, including unset, empty and unrecognized values.
    /// A typo or missing artifact must not cause a fallback to the gated legacy backend.
    /// [`agent::SidecarAvailability`] affects the message, not the backend choice. It distinguishes
    /// an installed artifact, a checkout to build, and a missing sidecar. The missing case emits
    /// [`agent::NO_SIDECAR_HINT`]; attempting to spawn it then fails with the same hint.
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

        // Every remaining case selects the sidecar in either build.
        // Warn on an unrecognized value so a typo cannot silently choose a backend.
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
}

impl BackendError {
    fn fatal(message: String) -> Self {
        Self { message, benign: false }
    }
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
    /// Constructs the selected backend. A cold sidecar checkout runs `npm ci` and `npm run build`,
    /// which can block for minutes. Call this off the GTK main thread, as `agent_panel`'s connect worker does.
    /// `resume` carries the provider session id to continue, or `None` for a fresh session.
    /// A resume that cannot be honored returns an error so it cannot silently start a fresh session.
    ///
    /// Every session on both backends is gated. The tab's permission mode is read when answering
    /// requests in `take_revised_ui_delivery`, rather than sent to the CLI.
    ///
    /// `project` controls loading the project's Claude configuration (`.claude/`, `.mcp.json`,
    /// `CLAUDE.md`) on both backends. The caller supplies it for every session, including resumes.
    pub fn start(
        kind: BackendKind,
        project_dir: &Path,
        resume: Option<&str>,
        project: ProjectTrust,
    ) -> Result<Self, BackendError> {
        Self::start_holding(kind, project_dir, resume, None, project)
    }

    /// [`start`](Self::start) for a resume whose session lease the caller already holds
    /// (`AgentConversation::resume_holding`): the lease goes to the conversation, and is released
    /// with it or with a failed start. Ignored when nothing is being resumed.
    pub fn start_holding(
        kind: BackendKind,
        project_dir: &Path,
        resume: Option<&str>,
        lease: Option<agent::lease::SessionLease>,
        project: ProjectTrust,
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
                        project,
                    )
                    .map(|c| AgentBackend::Conversation {
                        agent: AgentId::Claude,
                        conversation: Box::new(c),
                    })
                    .map_err(|e| BackendError::fatal(format!("could not continue the previous session: {e}"))),
                    None => AgentConversation::create(std::sync::Arc::new(provider), project_dir, project)
                        .map(|c| AgentBackend::Conversation {
                            agent: AgentId::Claude,
                            conversation: Box::new(c),
                        })
                        .map_err(|e| BackendError::fatal(format!("failed to create a Claude session: {e}"))),
                }
            }
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
    /// command. One that does not belong to `agent` is refused.
    pub fn start_external(
        agent: AgentId,
        launch: ExternalLaunch,
        project_dir: &Path,
        plan: TrustPlan,
        mode: SessionModeChoice,
        resume: Option<&str>,
    ) -> Result<Self, BackendError> {
        let canonical = project_dir
            .canonicalize()
            .map_err(|e| BackendError::fatal(format!("the project directory cannot be resolved: {e}")))?;
        let provider: std::sync::Arc<dyn agent::ExternalProvider + Send + Sync> = match (&agent, launch) {
            (AgentId::Codex, ExternalLaunch::Codex(program)) => {
                let state_home = crate::layout::persist::state_subdir(
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
                agent::providers::acp::AcpProvider::connect(spec, canonical.clone())
                    .map_err(|e| BackendError::fatal(format!("{name} could not start: {e}")))?,
            ),
            _ => {
                return Err(BackendError::fatal(format!(
                    "this build cannot start a {} tab",
                    agent.label()
                )))
            }
        };
        Self::start_external_with(agent, provider, &canonical, plan, mode, resume)
    }

    /// [`start_external`](Self::start_external) over a provider the caller already has: what it does
    /// once its provider is connected, and the way a test drives an external tab without a child.
    pub fn start_external_with(
        agent: AgentId,
        provider: std::sync::Arc<dyn agent::ExternalProvider + Send + Sync>,
        project_dir: &Path,
        plan: TrustPlan,
        mode: SessionModeChoice,
        resume: Option<&str>,
    ) -> Result<Self, BackendError> {
        let canonical = project_dir
            .canonicalize()
            .map_err(|e| BackendError::fatal(format!("the project directory cannot be resolved: {e}")))?;
        let start = agent::ExternalStart {
            cwd: canonical.to_string_lossy().into_owned(),
            plan,
            policy: turn_policy(mode),
            resume: resume.map(str::to_string),
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
    /// never separated them.
    pub fn provider_session_id(&self) -> Option<String> {
        match self {
            AgentBackend::Legacy(session) => session.projection.provider_session_id.clone(),
            AgentBackend::Conversation { conversation, .. } => conversation.provider_session_id(),
        }
    }

    pub fn provider_info(&self) -> Option<&ProviderInfo> {
        match self {
            AgentBackend::Legacy(_) => None,
            AgentBackend::Conversation { conversation, .. } => Some(conversation.provider_info()),
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
            AgentBackend::Conversation { conversation, .. } => conversation.capabilities(),
        }
    }

    /// Submits a turn. Locally produced events and provider events both reach the page through
    /// the pump. Legacy synthesizes its turn start because the CLI wire has no such line;
    /// the sidecar's turn start comes from the provider and remains authoritative.
    ///
    /// Two strings because they are two different things. `wire_text` is what goes to the model --
    /// the user's text with wire 1's editor context composed in (`panel/src/agent_panel.rs`, the
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
    /// A prompt whose send is refused benignly reaches the UI through the next pump, since the
    /// session lives on. A fatal failure retires the session before another pump, so its queued
    /// prompt is never drawn: the page says the message was not sent and puts it back in the box,
    /// and a failed tab's state carries no conversation, so a reload shows no row for it either.
    pub fn send_turn(&mut self, wire_text: &str, as_typed: &str) -> Result<(), BackendError> {
        if self.agent().is_external() {
            return Err(BackendError {
                message: "this tab's turns carry its permission mode; send it through its tab".to_string(),
                benign: true,
            });
        }
        match self {
            AgentBackend::Legacy(session) => {
                session.fold_locally(user_prompt_event(as_typed));
                session.send_turn(wire_text)?;
                Ok(())
            }
            AgentBackend::Conversation { conversation, .. } => {
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
                Ok(())
            }
        }
    }

    /// [`send_turn`](Self::send_turn) in a tab whose mode is `mode`. A Claude turn ignores the mode:
    /// its tab's mode is applied where requests are answered (`take_revised_ui_delivery`), never
    /// sent to the CLI. An external agent's turn carries it as the turn's reviewer, so a mode change
    /// reaches the agent at the next turn, while Eitri's own answering follows the mode at once.
    pub fn send_turn_in(
        &mut self,
        wire_text: &str,
        as_typed: &str,
        mode: SessionModeChoice,
    ) -> Result<(), BackendError> {
        if !self.agent().is_external() {
            return self.send_turn(wire_text, as_typed);
        }
        let AgentBackend::Conversation { conversation, .. } = self else {
            unreachable!();
        };
        // Folded before the send, as on the sidecar, so the prompt keeps its place before the
        // agent's first event of the turn.
        conversation.fold_locally(user_prompt_event(as_typed));
        conversation.send_turn_with(wire_text, turn_policy(mode))?;
        conversation.note_title(as_typed);
        Ok(())
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
    ) -> Result<(), SteerRefusal> {
        let result = match self {
            AgentBackend::Legacy(_) => return Err(SteerRefusal::Unsupported),
            AgentBackend::Conversation { conversation, .. } => {
                conversation.steer_turn(shown_turn, client_message_id, wire_text, as_typed)
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

    /// Stops the commands the agent left running after its turns ended, by stopping its session
    /// (`AgentConversation::stop_background`); the session then ends through the pump, and its
    /// conversation can be resumed. A refusal because of when it was asked is benign (a turn started,
    /// the commands ended), and so is an agent that keeps no such count: the session goes on.
    pub fn stop_background(&mut self, shown: u32) -> Result<(), BackendError> {
        let benign = |message: String| BackendError { message, benign: true };
        let AgentBackend::Conversation { agent, conversation } = self else {
            return Err(benign(
                "this agent leaves nothing running in the background".to_string(),
            ));
        };
        if !agent.is_external() {
            return Err(benign(
                "this agent leaves nothing running in the background".to_string(),
            ));
        }
        match conversation.stop_background(shown) {
            Ok(()) => Ok(()),
            Err(ConversationError::Provider(agent::ProviderError::UnsupportedCapability(_))) => {
                Err(benign(format!("{} cannot stop what it left running", agent.label())))
            }
            Err(e) => Err(e.into()),
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

    /// Stores the revision of the snapshot handed to the panel. The next drain takes it
    /// through [`Self::take_ui_snapshot_revision`] and omits events at or below that
    /// revision. Keeping it on the backend prevents a snapshot revision from surviving
    /// replacement of its session.
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
    /// directly with a snapshot's. An event answered here without a card is dropped with its tag; the
    /// others keep theirs, in order.
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
            AgentBackend::Conversation { conversation, .. } => conversation.take_revised_ui_delivery(),
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
            // A `Resync` carries no events to filter; the UI rebuilds from the projection.
            // `TabSet::pump` handles bypass requests in its `Resync` arm and hides ids in `host_answered`.
            other => other,
        }
    }

    /// Answers permission requests that need no human and returns the events the UI should see.
    /// Both backends gate every tool call with `PreToolUse`, so the UI's permission policy is
    /// applied here before deciding whether to show a card.
    ///
    /// Successfully answered requests are hidden from the UI. `ToolCallStarted` and
    /// `ToolCallCompleted` still show what ran. If sending an answer fails, the request is
    /// delivered so the user can resolve it instead of leaving an invisible pending request.
    ///
    /// `Bypass` allows gate requests before classification. The CLI remains gated, using
    /// `--permission-mode default` with a hook or the sidecar's INTERACTIVE mode.
    /// The caller supplies `Tab::mode` on every pump; this function stores no mode of its own.
    /// An id already in `host_answered` is dropped without another answer or a card.
    /// The tab set retains such an id only while the projection still lists the request,
    /// which has already been answered and is waiting for its resolution.
    ///
    /// Nothing is answered after `UngatedCliMode`, whether it arrives in this batch or is
    /// already in the projection. `TabSet::pump` closes the session and thereby denies pending
    /// requests, so no automatic grant is sent to a session that must be closed.
    ///
    /// Resolutions arrive on a later pump, including legacy's locally folded hook answers.
    /// A resolution for a hidden request is harmless because the UI removes pending requests by id.
    ///
    /// `host_answered` also keeps answered requests out of resync snapshots and the attention
    /// tray through `TabSet::pump`, `SnapshotView::of`, and `AttentionTracker::resync`.
    /// This prevents an answered request from reappearing as a card.
    ///
    /// The CLI's own prompts (`provider_prompt: Some`) are handled before gate requests.
    /// They can ask for checks, such as sensitive files, that a hook allowance or session
    /// rule does not silence. These prompts bypass the classifier and saved prefix rules.
    /// `Bypass` allows all such prompts, including forced asks, unknown kinds and prompts
    /// with no reason. In `Auto`, a prompt requires an approval matching its id, tool and
    /// input, which is consumed when used. Otherwise a card is shown. A prompt marked
    /// `needs_its_own_card` requires a card in `Auto` even with a matching approval: it was
    /// forced by the user's ask rule, has an unknown kind, or gives no reason for asking.
    /// A content-scoped ask rule can also give no reason.
    /// Each prompt answered without a card is added to `answered_for_you` with its row note.
    ///
    /// After `respond_permission` succeeds, a `Write`, `Edit` or `NotebookEdit` allowed by
    /// the classifier is added to `answered_for_you.by_the_fast_path` by tool-use id.
    /// Only the acceptEdits fast path allows these tools under the classifier; saved prefix
    /// rules do not apply to them. The entry includes `(tool_name, input)` because a later
    /// card can lack a tool-use id, preventing removal by id alone. Requests without a
    /// tool-use id are still answered but have no row note.
    ///
    /// File existence at `file_path` is checked just before allowing a `Write`, before the
    /// CLI can write it. A write to a missing file is added to `answered_for_you.creates_file`
    /// in both the fast path and bypass. `note_new_files` only sees delivered cards, so it
    /// cannot detect files created by these automatic answers.
    ///
    /// `classify_with_rules` checks `agent::rule_that_allows` first.
    /// `REASON_ALLOWED_BY_A_PROJECT_RULE` identifies a saved prefix rule allowance.
    /// After a successful answer, the rule's display string and `(tool_name, input)` are
    /// stored in `answered_for_you.by_rule` by tool-use id. The arguments let a later card
    /// remove the candidate even when that card lacks a tool-use id.
    /// These notes track actual answers: the absence of a card alone can also mean input
    /// validation failed before the hook ran, the request had no tool-use id, or bypass
    /// answered the gate instead of the classifier or saved rule.
    ///
    /// When the session requested CLI auto mode, the sidecar offered `cli_auto_mode` and
    /// `permission_defer`, and `AgentSessionProjection::cli_runs_auto` is true, `Auto`
    /// allows gate requests matched by `agent::rule_that_allows` and otherwise sends
    /// `PermissionDecision::Defer`. The local classifier is skipped and the CLI's classifier
    /// decides without a card. A refusal produces `AgentDomainEvent::PermissionDenied` and
    /// the call's error result. A failed deferral is delivered as a card.
    /// The projection is read before each request because ingestion can fold a trip or
    /// fallback during the batch. A fallback to `default` restores local classification;
    /// a trip stops answers. CLI prompts and bypass keep their behavior.
    ///
    /// `Manual` never defers or uses the fast path. `agent::classify_in_manual` requires a
    /// card for every file write or edit and otherwise uses the Auto classifier with saved
    /// rules. CLI prompts follow the Auto behavior. External agent requests require cards
    /// in both modes. `Manual` is never more permissive than `Auto`.
    ///
    /// Each event keeps its fold revision from `take_revised_ui_delivery`.
    /// Dropping an event also drops its revision.
    /// `decide` chooses the answer. `answer_and_record` sends it and records the result.
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
        // What the batch's own events have shown so far; the projection is read again for every
        // request below, because ingestion keeps folding while earlier answers of this batch are
        // sent, and a trip or a fallback to `default` it folds there must reach the next request.
        let mut tripped = false;
        let mut kept = Vec::with_capacity(events.len());
        for (revision, event) in events {
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
            // Read afresh for each request, as its own statement: on the sidecar `projection()`
            // holds the ingestion mutex and `respond_permission` locks it again (the 2026-09-15 GTK
            // freeze), so the guard is gone before any answer below. Both halves are needed for a
            // deferral: the session asked for the CLI's auto mode, and the CLI says it is running it.
            let (projection_tripped, cli_reports_auto) = {
                let projection = self.projection();
                (session_tripped(&projection), projection.cli_runs_auto())
            };
            tripped |= projection_tripped;
            // Only an Auto tab leaves anything to the CLI's classifier: Manual is Eitri's policy
            // alone, whatever the CLI runs under it.
            let defers_to_the_cli =
                mode == PermissionMode::Auto && self.capabilities().cli_auto_mode && cli_reports_auto;
            if tripped {
                eprintln!("[permission] not answering {tool_name}: the agent reported a policy it was not asked for");
                kept.push((revision, event));
                continue;
            }
            // One check for every way of answering, before any is chosen: an id the host already
            // answered (a duplicate delivery, e.g. after a resync) is dropped without a second answer.
            if host_answered.contains(permission_id) {
                continue;
            }
            let planned = decide(
                GateRequest {
                    permission_id,
                    tool_use_id,
                    tool_name,
                    input,
                    provider_prompt: provider_prompt.as_ref(),
                },
                Judge {
                    mode,
                    external: self.agent().is_external(),
                    defers_to_the_cli,
                    rules,
                    project_root,
                    approvals,
                },
            );
            let answered = planned
                .is_some_and(|planned| self.answer_and_record(planned, host_answered, approvals, answered_for_you));
            if !answered {
                kept.push((revision, event));
            }
        }
        kept
    }

    /// Sends the answer `decide` chose and, once the provider took it, records it: the gate answer,
    /// the id in `host_answered`, the approval it used up and each row note it carries. Every way of
    /// answering without a card ends here, so none can send an answer it does not record or record
    /// one it did not send. `false` when the answer could not be sent: the caller delivers the
    /// request as a card, since a request left pending with nothing on screen is a session the user
    /// cannot unstick.
    fn answer_and_record(
        &mut self,
        planned: PlannedAnswer,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
        answered_for_you: &mut AnsweredForYou,
    ) -> bool {
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
            could_not,
        } = planned;
        let deferred = decision.defers();
        match self.respond_permission(&permission_id, decision) {
            Ok(_resolution) => {
                answered_for_you.gate_answers.push(GateAnswer {
                    origin,
                    permission_id: permission_id.clone(),
                    tool_use_id: call,
                    tool_name,
                    deferred,
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
                // Said on stderr rather than silently: this is the only record anywhere of a tool
                // call that ran without the user being asked. One short line, whose reason is a
                // fixed label rather than anything the model wrote.
                eprintln!("[permission] {sent}");
                true
            }
            Err(error) => {
                eprintln!(
                    "[permission] could not {could_not}, showing a card instead: {}",
                    error.message
                );
                false
            }
        }
    }

    /// Whether the session has reported a policy it was not asked for. The projection guard is a
    /// temporary of this call, so it is gone before the caller sends any answer.
    fn tripped(&self) -> bool {
        session_tripped(&self.projection())
    }

    /// Answers `allow` to ids still pending on this session, in the supplied order (sorted by
    /// `seq` by the caller). Skips ids already resolved or unknown. Returns only successfully
    /// answered ids so the caller can extend `host_answered`. `why` labels the log message.
    /// Returns `Approved`.
    ///
    /// Resolutions arrive through the pump, including legacy's local hook replies.
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
    /// Reads the projection again before each answer. Ingestion continues while earlier
    /// answers are sent, so a trip folded during the batch stops subsequent answers.
    ///
    /// In bypass, CLI prompts are answered regardless of their reason, including
    /// `permissions.ask`, unknown kinds, and missing reasons. Prompts naming a call are
    /// returned in `Approved::prompts` with the bypass note for that call's row.
    pub fn approve_pending(&mut self, ids: &[String], why: &str) -> Approved {
        // Its own statement, guard dropped before any answer -- the same lock-order rule the
        // collection below follows.
        if self.tripped() {
            eprintln!("[permission] not answering {why}: the agent reported a policy it was not asked for");
            return Approved::default();
        }
        let external = self.agent().is_external();
        let pending: std::collections::BTreeMap<String, PendingAnswer> = {
            let projection = self.projection();
            ids.iter()
                .filter_map(|id| {
                    let p = projection.pending_permissions.get(id)?;
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

    pub fn shutdown(&mut self) {
        match self {
            AgentBackend::Legacy(session) => session.shutdown(),
            AgentBackend::Conversation { conversation, .. } => conversation.shutdown(),
        }
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
struct AnswerContext<'a> {
    rules: &'a agent::PrefixRules,
    mode: PermissionMode,
    host_answered: &'a mut BTreeSet<String>,
    approvals: &'a mut HumanApprovals,
    answered_for_you: &'a mut AnsweredForYou,
}

/// One `PermissionRequested`, as `decide` reads it.
struct GateRequest<'a> {
    permission_id: &'a str,
    tool_use_id: &'a Option<String>,
    tool_name: &'a str,
    input: &'a serde_json::Value,
    provider_prompt: Option<&'a agent::ProviderPrompt>,
}

/// What `decide` judges a request by besides the request itself.
struct Judge<'a> {
    mode: PermissionMode,
    /// The session is an external agent's, whose requests no policy here judges.
    external: bool,
    /// An Auto tab on a session whose CLI runs its own auto mode (`answer_what_needs_no_human`).
    defers_to_the_cli: bool,
    rules: &'a agent::PrefixRules,
    project_root: &'a Path,
    approvals: &'a HumanApprovals,
}

/// The answer `decide` chose for one request, with everything its row says once it is sent; only
/// `AgentBackend::answer_and_record` sends it.
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

/// How the host answers one gate request without a card, or `None` when it is a card (the reason
/// logged here). The rules are `AgentBackend::answer_what_needs_no_human`'s; this only chooses, and
/// reads whatever must be read before an answer is sent (whether a `Write` creates its file: once
/// the answer is sent the CLI may already have written it).
fn decide(request: GateRequest<'_>, judge: Judge<'_>) -> Option<PlannedAnswer> {
    let Judge {
        mode,
        external,
        defers_to_the_cli,
        rules,
        project_root,
        approvals,
    } = judge;
    let GateRequest {
        tool_use_id,
        tool_name,
        input,
        provider_prompt,
        ..
    } = request;
    let call = named_call(tool_use_id);
    let creates_file = || {
        call.clone()
            .filter(|_| write_over_no_file(tool_name, input, project_root))
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
            Err(why) => {
                eprintln!("[permission] asking the user: {tool_name} (the CLI's own prompt: {why:?})");
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
    if defers_to_the_cli {
        // A saved rule still answers what it answers today, and says so on the row; it is the only
        // allow this host gives an Auto call on such a session. Asked here rather than through
        // `classify_with_rules`, whose classifier half is exactly what is not consulted.
        let planned = match agent::rule_that_allows(tool_name, input, project_root, rules) {
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
    let classification = if mode == PermissionMode::Manual {
        agent::classify_in_manual(tool_name, input, project_root, rules)
    } else {
        agent::classify_with_rules(tool_name, input, project_root, rules)
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
        .then(|| agent::rule_that_allows(tool_name, input, project_root, rules))
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

/// The row note of an external agent's request this host allowed because the tab is in bypass.
pub const EXTERNAL_BYPASS_NOTE: &str = "allowed in bypass";

/// A CLI prompt answered without a card, identified by tool-use id, and the note displayed on
/// its call's row. `ProviderPrompt::label` identifies the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAnsweredForYou {
    pub tool_use_id: String,
    pub note: String,
}

/// A `Write`/`Edit`/`NotebookEdit` call whose gate the acceptEdits fast path answered `allow`.
/// Keeps the tool-use id and exact `(tool_name, input)` the answer saw for the row's auto note.
/// A later CLI prompt for the same call can have an empty tool-use id, so
/// `note_auto_edit_answers` also matches by content to remove the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathAnsweredForYou {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// A call allowed by a saved prefix rule, with the rule text displayed after completion and
/// the exact `(tool_name, input)` that was approved. `note_rule_answers` matches it by
/// content like [`FastPathAnsweredForYou`] when a later prompt has no tool-use id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleAnsweredForYou {
    pub tool_use_id: String,
    pub rule: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// Answers made without a card, keyed by tool-use id for the call's row notes.
/// `answer_what_needs_no_human` records them when it answers. The tab set uses these entries
/// rather than inferring an answer from the absence of a card.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOrigin {
    GateRequest,
    ProviderPrompt,
}

/// One request the host answered itself: see [`AnsweredForYou::gate_answers`].
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// A picker row's display title: the CLI's own title, then the persisted title, then none.
/// This only changes `session` on its way to the frontend. It does not write the conversation
/// record, so `ConversationRecord::title` keeps its first-one-wins behavior and `updated_at`
/// is unchanged. Each greeting reads the transcript again, so resuming refreshes the row.
///
/// A missing or unreadable transcript, no `ai-title` within `AI_TITLE_TAIL_BYTES`, or an
/// unrenderable title leaves `session.title` unchanged. No title is invented.
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

#[cfg(test)]
mod tests {
    use agent::SidecarAvailability::{self, BuildsNamedCheckout, Missing, Runnable};

    /// Every result of `agent::sidecar_availability`. Availability selects the startup line
    /// without changing which backend is chosen.
    const ALL_AVAILABILITIES: [SidecarAvailability; 3] = [Runnable, BuildsNamedCheckout, Missing];

    /// The explicit variable wins over what is installed, whatever is installed -- and now that
    /// legacy is the gated backend, it wins only where this
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

    /// An unrecognized backend value selects the sidecar with a warning in every build.
    /// Compiling in legacy must not make it the fallback for a typo.
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
        // The resume and fork capabilities are asserted at compile time beside the constant.
        // The greeting still offers bypass because the client answers `allow` through the
        // interactive gate; legacy needs no CLI bypass mode. Those capabilities are also
        // asserted beside the constant.
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
        assert_eq!(turn_policy(SessionModeChoice::Manual).reviewer, Reviewer::User);
        assert_eq!(turn_policy(SessionModeChoice::Auto).reviewer, Reviewer::Agent);
        assert_eq!(turn_policy(SessionModeChoice::Bypass).reviewer, Reviewer::User);
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

    /// The CLI's title takes precedence over the persisted title at display time.
    /// Both titles exist here, so the row must show the CLI's title.
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

    /// Folding the prompt before a send keeps the typed words in the projection when the provider
    /// refuses the turn, so live delivery and later snapshots describe the same conversation.
    #[test]
    fn a_rejected_sidecar_turn_still_leaves_the_prompt_recorded() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("rejected-turn-keeps-prompt");
        let conversation =
            AgentConversation::create(std::sync::Arc::new(RejectingProvider), &dir, ProjectTrust::Untrusted)
                .expect("create_session succeeds on RejectingProvider; only send_turn is rigged to fail");
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

        let result = backend.send_turn("wire text with context composed in", "what does this do?");

        result.expect_err("RejectingProvider's send_turn always fails");
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
            match backend
                .take_revised_ui_delivery(
                    project_root,
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
        let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let mut backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };

        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "perm-write".into(),
            tool_use_id: Some("tool-2".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // A write under `.git` still needs a card under the acceptEdits fast path.
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
                    &dir,
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

    /// A hidden chat still answers requests that need no human and counts the card it holds.
    /// The panel's 33 ms pump timer ignores visibility, so the tracker uses `on_screen = false`.
    /// The projection holds both pending requests until the provider's resolution arrives, including
    /// the automatically answered `Read`. The tracker counts only the card delivered to the panel.
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
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // A write under `.git` still needs a card under the acceptEdits fast path.
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
                    &dir,
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
    /// batch. It blocks both gate requests and CLI prompts, which bypass would otherwise
    /// answer regardless of their reason.
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
                    &dir,
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
                    &dir,
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
        let later = pump_until_delivery(&mut backend, &dir, PermissionMode::Bypass);
        assert!(later.iter().any(
            |e| matches!(e, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "later")
        ));
        assert!(provider.resolutions().is_empty(), "nor one in a later batch");

        let answered = backend.approve_pending(&["behind-it".to_string()], "on entering bypass");
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
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert!(delivered
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })));
        assert!(provider.resolutions().is_empty(), "{:?}", provider.resolutions());
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
                    &dir,
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
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // A write under `.git` still needs a card under the acceptEdits fast path.
            provider_prompt: None,
        });

        let mut host_answered = BTreeSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match backend
                .take_revised_ui_delivery(
                    &dir,
                    &rules,
                    PermissionMode::Auto,
                    &mut host_answered,
                    &mut HumanApprovals::default(),
                    &mut AnsweredForYou::default(),
                )
                .without_revisions()
            {
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let quiet = std::time::Duration::from_millis(100);
        let mut delivered = Vec::new();
        let mut last_new: Option<std::time::Instant> = None;
        loop {
            let now = std::time::Instant::now();
            match backend
                .take_revised_ui_delivery(
                    &dir,
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
                    last_new = Some(now);
                }
                _ if last_new.is_some_and(|at| now.duration_since(at) >= quiet) || now > deadline => break,
                _ => std::thread::sleep(std::time::Duration::from_millis(5)),
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
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // A write under `.git` still needs a card under the acceptEdits fast path.
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
            backend
                .take_revised_ui_delivery(
                    &dir,
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
        let answered = backend.approve_pending(&ids, "test");
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

    /// `TabSet::confirm_bypass` calls `approve_pending` between 33ms pumps. The function must
    /// check for an `UngatedCliMode` already folded by background ingestion, before the next
    /// pump marks the tab `Failed`.
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
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }), // A write under `.git` still needs a card under the acceptEdits fast path.
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
        let mut answered = AnsweredForYou::default();
        let mut delivered = Vec::new();
        let mut last_new: Option<std::time::Instant> = None;
        loop {
            let now = std::time::Instant::now();
            match backend
                .take_revised_ui_delivery(dir, rules, mode, host_answered, approvals, &mut answered)
                .without_revisions()
            {
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

    /// The acceptEdits fast path allows only the gate's request for an in-project `Write`.
    /// A later CLI prompt for that same call is still a card in `Auto`, whether a user's
    /// `permissions.ask` rule (`Edit(notes.txt)`) forced it or no rule is behind it: the fast-path
    /// allow is not a human approval, so `HumanApprovals` stays empty. This keeps an ask rule for
    /// an edit from being answered silently in `Auto`. Bypass allows the prompt.
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
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 4, "all card in Auto with no approval: {delivered:?}");
        let ids: Vec<String> = ["perm-ask", "perm-unknown", "perm-silent", "perm-plain"]
            .map(str::to_string)
            .to_vec();
        let answered = backend.approve_pending(&ids, "test");
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
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 4, "{delivered:?}");
        let ids: Vec<String> = ["perm-ask", "perm-silent", "perm-noid", "perm-gate"]
            .map(str::to_string)
            .to_vec();
        let answered = backend.approve_pending(&ids, "on entering bypass");
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

    /// A trip folded by ingestion while the first answer is being sent stops later answers in
    /// the same `approve_pending` batch, matching `answer_what_needs_no_human`.
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
        let delivered = pump_until_delivery(&mut backend, &dir, PermissionMode::Auto);
        assert_eq!(delivered.len(), 2, "{delivered:?}");
        provider.fold_after_next_resolution(vec![AgentDomainEvent::UngatedCliMode {
            reported: "acceptEdits".into(),
            detail: "SessionReady".into(),
        }]);
        let answered = backend.approve_pending(&["perm-1".to_string(), "perm-2".to_string()], "on entering bypass");
        assert_eq!(answered.ids, vec!["perm-1".to_string()]);
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        backend.shutdown();
    }

    // ---- The CLI's own auto mode under the gate ----

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
        backend: &mut AgentBackend,
        dir: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
        approvals: &mut HumanApprovals,
    ) -> (Vec<AgentDomainEvent>, AnsweredForYou) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let quiet = std::time::Duration::from_millis(100);
        let mut answered = AnsweredForYou::default();
        let mut delivered = Vec::new();
        let mut last_new: Option<std::time::Instant> = None;
        loop {
            let now = std::time::Instant::now();
            match backend
                .take_revised_ui_delivery(dir, rules, mode, host_answered, approvals, &mut answered)
                .without_revisions()
            {
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

    #[test]
    fn an_auto_tab_on_an_auto_session_defers_every_gate_request_and_draws_no_card() {
        let dir = a_workspace_holding_one_file();
        let (provider, mut backend) = auto_backend(&dir);
        let mut batch = vec![cli_reports("auto")];
        batch.extend(three_gates());
        provider.queue_all(batch);
        let mut host_answered = BTreeSet::new();
        let (delivered, answered) = deliver_noting(
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

    fn request(id: &str, tool: &str, input: serde_json::Value) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: Some(format!("call-{id}")),
            tool_name: tool.into(),
            input,
            provider_prompt: None,
        }
    }

    /// Every delivery until `want` requests have reached the projection and 100 ms pass quietly.
    fn deliver(
        backend: &mut AgentBackend,
        dir: &Path,
        rules: &agent::PrefixRules,
        mode: PermissionMode,
        host_answered: &mut BTreeSet<String>,
    ) -> Vec<AgentDomainEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut delivered = Vec::new();
        let mut last_new: Option<std::time::Instant> = None;
        loop {
            let now = std::time::Instant::now();
            match backend
                .take_revised_ui_delivery(
                    dir,
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
                    last_new = Some(now);
                }
                _ if last_new.is_some_and(|at| now.duration_since(at) >= std::time::Duration::from_millis(100))
                    || now > deadline =>
                {
                    return delivered
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
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
        let delivered = deliver(&mut backend, &dir, &rules, PermissionMode::Auto, &mut answered);
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
        let delivered = deliver(&mut backend, &dir, &rules, PermissionMode::Bypass, &mut answered);
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
                    &dir,
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
        let delivered = deliver(&mut backend, &dir, &rules, PermissionMode::Auto, &mut host_answered);
        assert!(carded(&delivered, "a1"), "{delivered:?}");
        let approved = backend.approve_pending(&["a1".to_string()], "on entering bypass");
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
        let delivered = deliver(&mut backend, &dir, &rules, PermissionMode::Auto, &mut answered);
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
        let refused = backend.send_turn("hello", "hello").expect_err("no mode, no turn");
        assert!(refused.benign, "{}", refused.message);
        assert!(provider.policies().is_empty());
        backend
            .send_turn_in("hello", "hello", SessionModeChoice::Bypass)
            .expect("the tab's door carries the mode");
        assert_eq!(
            provider.policies(),
            vec![TurnPolicy {
                reviewer: Reviewer::User
            }]
        );
        assert_eq!(provider.starts()[0].policy.reviewer, Reviewer::Agent, "started in Auto");
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
            let refused =
                AgentBackend::start_external(agent, launch, &dir, empty_trust_plan(), SessionModeChoice::Auto, None);
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
            policy: turn_policy(SessionModeChoice::Auto),
            resume: None,
        };
        let conversation = AgentConversation::create_external(provider, start, &dir).unwrap();
        let refused = ExternalConversation::new(AgentId::Acp("gemini".into()), conversation);
        assert!(refused.is_err(), "a codex session is not an ACP tab's");
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
