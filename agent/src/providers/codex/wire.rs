//! The `codex app-server` wire, hand-written for exactly the methods Eitri uses.
//!
//! **Outbound** types serialize what Eitri sends. The fields that carry policy (the approval policy, the
//! sandbox, the per-turn sandbox policy, `excludeTurns`) are private and filled from `config`'s constants
//! by the constructors, so no caller can put another value on the wire.
//!
//! **Inbound** types read what Codex sends, and are lenient by design: Codex adds item types, statuses
//! and error variants between releases, and a field this build does not know must not stop a turn from
//! ending. An unknown enum value is `Unknown(String)`, an unknown item is `ThreadItem::Unknown`, an
//! unknown method is `Unknown`. A known method whose payload lacks what it must carry is `Malformed`, so
//! the provider can fail that one message closed instead of guessing.
//!
//! The account's details (its email) are never kept: `account/read` keeps only whether an account exists.

use super::config::{sandbox_mode, sandbox_policy, SessionConfig, APPROVAL_POLICY};
use crate::external::{DirTrust, Reviewer, Sandbox};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The method names Eitri sends, and the ones it reads.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const ACCOUNT_READ: &str = "account/read";
    pub const THREAD_START: &str = "thread/start";
    pub const THREAD_RESUME: &str = "thread/resume";
    pub const THREAD_UNSUBSCRIBE: &str = "thread/unsubscribe";
    pub const THREAD_TURNS_LIST: &str = "thread/turns/list";
    pub const THREAD_ITEMS_LIST: &str = "thread/items/list";
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    pub const TURN_STEER: &str = "turn/steer";

    pub const THREAD_STARTED: &str = "thread/started";
    pub const TURN_STARTED: &str = "turn/started";
    pub const TURN_COMPLETED: &str = "turn/completed";
    pub const ITEM_STARTED: &str = "item/started";
    pub const ITEM_COMPLETED: &str = "item/completed";
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    pub const REASONING_SUMMARY_DELTA: &str = "item/reasoning/summaryTextDelta";
    pub const REASONING_TEXT_DELTA: &str = "item/reasoning/textDelta";
    pub const COMMAND_OUTPUT_DELTA: &str = "item/commandExecution/outputDelta";
    pub const AUTO_REVIEW_STARTED: &str = "item/autoApprovalReview/started";
    pub const AUTO_REVIEW_COMPLETED: &str = "item/autoApprovalReview/completed";
    pub const TOKEN_USAGE_UPDATED: &str = "thread/tokenUsage/updated";
    pub const RATE_LIMITS_UPDATED: &str = "account/rateLimits/updated";
    pub const MCP_STARTUP_STATUS: &str = "mcpServer/startupStatus/updated";
    pub const HOOK_STARTED: &str = "hook/started";
    pub const HOOK_COMPLETED: &str = "hook/completed";
    pub const ERROR: &str = "error";
    pub const SERVER_REQUEST_RESOLVED: &str = "serverRequest/resolved";

    pub const COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
    pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";
    pub const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
    pub const MCP_ELICITATION: &str = "mcpServer/elicitation/request";
    pub const TOOL_REQUEST_USER_INPUT: &str = "item/tool/requestUserInput";
}

/// Items per `thread/turns/list` page on resume: turn metadata only, so pages stay small.
pub const TURNS_PAGE: u32 = 10;
/// Items per `thread/items/list` page on resume.
pub const ITEMS_PAGE: u32 = 50;

// ---------------------------------------------------------------- outbound

/// The reviewer word for the tab's mode: Codex's own reviewer in Auto, the user (that is, Eitri's cards
/// or Eitri's bypass) otherwise.
pub fn reviewer_word(reviewer: Reviewer) -> &'static str {
    match reviewer {
        Reviewer::Agent => "auto_review",
        Reviewer::User => "user",
    }
}

/// The `trust_level` of one `projects` entry. The one place the untrusted level is spelled: the same word
/// is also an approval policy, and the wire guard refuses it everywhere else.
pub fn trust_level_word(trust: DirTrust) -> &'static str {
    match trust {
        DirTrust::Trusted => "trusted",
        DirTrust::Untrusted => "untrusted", // codex-wire-guard: DirTrust
    }
}

fn serialize_reviewer<S: Serializer>(reviewer: &Reviewer, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(reviewer_word(*reviewer))
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo {
    pub name: &'static str,
    pub title: &'static str,
    pub version: String,
}

/// `initialize`. It asks for no capability: the experimental API stays off, and with it every field Codex
/// marks experimental (extra permissions on a command approval among them).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
}

impl InitializeParams {
    pub fn eitri(version: &str) -> Self {
        InitializeParams {
            client_info: ClientInfo {
                name: "eitri",
                title: "Eitri",
                version: version.to_string(),
            },
        }
    }
}

/// `account/read`, never refreshing a token.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountReadParams {
    refresh_token: bool,
}

impl AccountReadParams {
    pub fn new() -> Self {
        AccountReadParams { refresh_token: false }
    }
}

impl Default for AccountReadParams {
    fn default() -> Self {
        Self::new()
    }
}

/// The per-turn sandbox policy. Its fields are private and only [`super::config::sandbox_policy`] makes
/// one, so a widened policy (a writable root, network, a temporary directory excluded) cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPolicyWire {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    writable_roots: Option<[&'static str; 0]>,
    network_access: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_tmpdir_env_var: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_slash_tmp: Option<bool>,
}

impl SandboxPolicyWire {
    pub(super) fn narrow(kind: &'static str) -> Self {
        SandboxPolicyWire {
            kind,
            writable_roots: Some([]),
            network_access: false,
            exclude_tmpdir_env_var: Some(false),
            exclude_slash_tmp: Some(false),
        }
    }

    /// The form Codex's read-only policy takes: the workspace fields are no part of it.
    pub(super) fn read_only(kind: &'static str) -> Self {
        SandboxPolicyWire {
            kind,
            writable_roots: None,
            network_access: false,
            exclude_tmpdir_env_var: None,
            exclude_slash_tmp: None,
        }
    }

    /// The policy as JSON, the form a start or resume answer echoes it in.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `thread/start`. No `model`: Codex uses the user's own.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    pub cwd: String,
    approval_policy: &'static str,
    #[serde(serialize_with = "serialize_reviewer")]
    pub approvals_reviewer: Reviewer,
    sandbox: &'static str,
    /// The object `config::session_config` built.
    config: SessionConfig,
}

impl ThreadStartParams {
    pub fn new(cwd: String, reviewer: Reviewer, sandbox: Sandbox, config: SessionConfig) -> Self {
        ThreadStartParams {
            cwd,
            approval_policy: APPROVAL_POLICY,
            approvals_reviewer: reviewer,
            sandbox: sandbox_mode(sandbox),
            config,
        }
    }
}

/// `thread/resume`, with the history left out of the answer; it is paged afterwards.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
    pub cwd: String,
    approval_policy: &'static str,
    #[serde(serialize_with = "serialize_reviewer")]
    pub approvals_reviewer: Reviewer,
    sandbox: &'static str,
    config: SessionConfig,
    exclude_turns: bool,
}

impl ThreadResumeParams {
    pub fn new(thread_id: String, cwd: String, reviewer: Reviewer, sandbox: Sandbox, config: SessionConfig) -> Self {
        ThreadResumeParams {
            thread_id,
            cwd,
            approval_policy: APPROVAL_POLICY,
            approvals_reviewer: reviewer,
            sandbox: sandbox_mode(sandbox),
            config,
            exclude_turns: true,
        }
    }
}

/// One element of a turn's input. Eitri sends text only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    Text { text: String },
}

/// `turn/start`. The policy is repeated on every turn: the per-turn overrides are not echoed, so sending
/// the same constants each time is the only way to know what a turn runs under.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    approval_policy: &'static str,
    #[serde(serialize_with = "serialize_reviewer")]
    pub approvals_reviewer: Reviewer,
    sandbox_policy: SandboxPolicyWire,
}

impl TurnStartParams {
    pub fn new(thread_id: String, text: String, reviewer: Reviewer, sandbox: Sandbox) -> Self {
        TurnStartParams {
            thread_id,
            input: vec![UserInput::Text { text }],
            approval_policy: APPROVAL_POLICY,
            approvals_reviewer: reviewer,
            sandbox_policy: sandbox_policy(sandbox),
        }
    }
}

/// `turn/steer`: one user message into the running turn. `expectedTurnId` makes Codex refuse it when that
/// turn is no longer the one running, and `clientUserMessageId` comes back on the `userMessage` item Codex
/// emits when the model reads it. No policy rides on it: the turn keeps what its `turn/start` sent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
    pub thread_id: String,
    pub client_user_message_id: String,
    pub input: Vec<UserInput>,
    pub expected_turn_id: String,
}

impl TurnSteerParams {
    pub fn new(thread_id: String, client_user_message_id: String, text: String, expected_turn_id: String) -> Self {
        TurnSteerParams {
            thread_id,
            client_user_message_id,
            input: vec![UserInput::Text { text }],
            expected_turn_id,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUnsubscribeParams {
    pub thread_id: String,
}

/// `thread/turns/list`, newest first, turn metadata only.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurnsListParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    limit: u32,
    sort_direction: &'static str,
    items_view: &'static str,
}

impl ThreadTurnsListParams {
    pub fn new(thread_id: String, cursor: Option<String>) -> Self {
        ThreadTurnsListParams {
            thread_id,
            cursor,
            limit: TURNS_PAGE,
            sort_direction: "desc",
            items_view: "notLoaded",
        }
    }
}

/// `thread/items/list` for one turn.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemsListParams {
    pub thread_id: String,
    pub turn_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    limit: u32,
}

impl ThreadItemsListParams {
    pub fn new(thread_id: String, turn_id: String, cursor: Option<String>) -> Self {
        ThreadItemsListParams {
            thread_id,
            turn_id,
            cursor,
            limit: ITEMS_PAGE,
        }
    }
}

/// The only answers Eitri gives a command or file-change approval. None of them widens what later runs
/// without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    /// Run this one call.
    Accept,
    /// Do not run it; the turn goes on.
    Decline,
    /// Do not run it, and end the turn.
    Cancel,
}

/// The result of a command or file-change approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DecisionResponse {
    pub decision: Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct NoPermissions {}

/// The answer to `item/permissions/requestApproval`: nothing granted, for this turn only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PermissionsGrant {
    permissions: NoPermissions,
    scope: &'static str,
}

impl PermissionsGrant {
    pub fn none_for_turn() -> Self {
        PermissionsGrant {
            permissions: NoPermissions {},
            scope: "turn",
        }
    }
}

/// What a tool server's form asks for. Eitri answers only the tool approval Codex builds as a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ElicitationAction {
    /// Run the tool this once: an accept that carries no wish to be remembered.
    Accept,
    /// Do not run it; the turn goes on.
    Decline,
}

/// The result of `mcpServer/elicitation/request`. `_meta` stays empty: it is where a client would ask
/// Codex to remember the approval, and Eitri never does.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<Value>,
    #[serde(rename = "_meta")]
    meta: Option<Value>,
}

impl ElicitationResponse {
    /// The one-time accept: an empty form and no remembering.
    pub fn accept_once() -> Self {
        ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(Value::Object(Default::default())),
            meta: None,
        }
    }

    pub fn decline() -> Self {
        ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        }
    }
}

/// One answer to one question of `item/tool/requestUserInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserInputAnswer {
    pub answers: Vec<String>,
}

/// The result of `item/tool/requestUserInput`: answers by question id. An empty map answers nothing,
/// which Codex reads as the question going unanswered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserInputResponse {
    pub answers: std::collections::BTreeMap<String, UserInputAnswer>,
}

impl UserInputResponse {
    pub fn unanswered() -> Self {
        UserInputResponse {
            answers: Default::default(),
        }
    }

    pub fn one(question_id: &str, label: &str) -> Self {
        let mut answers = std::collections::BTreeMap::new();
        answers.insert(
            question_id.to_string(),
            UserInputAnswer {
                answers: vec![label.to_string()],
            },
        );
        UserInputResponse { answers }
    }
}

// ---------------------------------------------------------------- inbound: open enums

/// A string-valued enum that keeps a value this build does not know as `Unknown`, and reads a value of
/// the wrong JSON type as `Unknown` too (its compact JSON), so one odd field never fails a message.
macro_rules! open_enum {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$vmeta:meta])* $variant:ident => $word:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
            Unknown(String),
        }

        impl $name {
            pub fn from_word(word: &str) -> Self {
                match word {
                    $($word => $name::$variant,)+
                    other => $name::Unknown(other.to_string()),
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Ok(match Value::deserialize(d)? {
                    Value::String(s) => Self::from_word(&s),
                    other => $name::Unknown(other.to_string()),
                })
            }
        }
    };
}

open_enum! {
    /// A turn's status.
    pub enum TurnStatus {
        Completed => "completed",
        Interrupted => "interrupted",
        Failed => "failed",
        InProgress => "inProgress",
    }
}

open_enum! {
    /// How much of a turn's `items` was loaded.
    pub enum TurnItemsView {
        NotLoaded => "notLoaded",
        Summary => "summary",
        Full => "full",
    }
}

open_enum! {
    /// The status of a command, file change, tool call or subagent call.
    pub enum ItemStatus {
        InProgress => "inProgress",
        Completed => "completed",
        Failed => "failed",
        Declined => "declined",
    }
}

open_enum! {
    pub enum SubAgentKind {
        Started => "started",
        Interacted => "interacted",
        Interrupted => "interrupted",
        Completed => "completed",
    }
}

open_enum! {
    /// What a command approval is about: a command, or input typed into a command already running.
    pub enum CommandApprovalKind {
        Command => "command",
        WriteStdin => "writeStdin",
    }
}

impl Default for CommandApprovalKind {
    /// Older servers send no `kind`; their requests are commands.
    fn default() -> Self {
        CommandApprovalKind::Command
    }
}

open_enum! {
    /// The state of an auto-review by Codex's own reviewer.
    pub enum ReviewStatus {
        InProgress => "inProgress",
        Approved => "approved",
        Denied => "denied",
        TimedOut => "timedOut",
        Aborted => "aborted",
    }
}

/// Why a turn failed. Codex sends either a bare name (`"usageLimitExceeded"`) or an object with one key
/// whose value may carry an HTTP status (`{"httpConnectionFailed": {"httpStatusCode": 502}}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexErrorInfo {
    Named(String),
    WithStatus {
        name: String,
        http_status_code: Option<u16>,
    },
}

impl CodexErrorInfo {
    pub fn name(&self) -> &str {
        match self {
            CodexErrorInfo::Named(n) | CodexErrorInfo::WithStatus { name: n, .. } => n,
        }
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            CodexErrorInfo::Named(_) => None,
            CodexErrorInfo::WithStatus { http_status_code, .. } => *http_status_code,
        }
    }
}

/// The longest name kept from an error info that is neither of its two forms.
const ODD_ERROR_NAME_MAX: usize = 80;

impl<'de> Deserialize<'de> for CodexErrorInfo {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::String(s) => CodexErrorInfo::Named(s),
            Value::Object(map) if map.len() == 1 => {
                let (name, inner) = map.into_iter().next().unwrap_or_default();
                let http_status_code = inner
                    .get("httpStatusCode")
                    .and_then(Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok());
                CodexErrorInfo::WithStatus { name, http_status_code }
            }
            other => {
                // Neither form: kept as a name, so the turn still ends Failed and says what came.
                let mut text = other.to_string();
                if text.len() > ODD_ERROR_NAME_MAX {
                    let mut cut = ODD_ERROR_NAME_MAX;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                }
                CodexErrorInfo::Named(text)
            }
        })
    }
}

// ---------------------------------------------------------------- inbound: responses

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// `<originator>/<version> (<platform>) ...`: the version is read from it.
    pub user_agent: String,
    #[serde(default)]
    pub codex_home: Option<String>,
}

/// `account/read`. The account object is skipped unread: it carries the email.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountReadResponse {
    #[serde(default)]
    account: Option<IgnoredAny>,
    #[serde(default)]
    pub requires_openai_auth: bool,
}

impl AccountReadResponse {
    /// Codex needs a sign-in it does not have.
    pub fn signed_out(&self) -> bool {
        self.account.is_none() && self.requires_openai_auth
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    #[serde(default)]
    pub cli_version: Option<String>,
    #[serde(default)]
    pub turns: Vec<Turn>,
}

/// The answer to `thread/start`, and to `thread/resume` (the same shape).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    pub thread: Thread,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Instruction files the session loaded (`AGENTS.md` and the like).
    #[serde(default)]
    pub instruction_sources: Vec<String>,
    #[serde(default)]
    approval_policy: Option<Value>,
    #[serde(default)]
    approvals_reviewer: Option<Value>,
    #[serde(default)]
    sandbox: Option<Value>,
}

pub type ThreadResumeResponse = ThreadStartResponse;

/// The effective policy a start or resume answer reports, as raw JSON: an absent field stays `None` so the
/// comparison with what was sent can fail closed on it, and a value of a shape this build does not know is
/// still compared exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadEcho {
    pub approval_policy: Option<Value>,
    pub approvals_reviewer: Option<Value>,
    pub sandbox: Option<Value>,
}

impl ThreadStartResponse {
    pub fn echo(&self) -> ThreadEcho {
        ThreadEcho {
            approval_policy: self.approval_policy.clone(),
            approvals_reviewer: self.approvals_reviewer.clone(),
            sandbox: self.sandbox.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<CodexErrorInfo>,
    #[serde(default)]
    pub additional_details: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    #[serde(default)]
    pub items: Vec<ThreadItem>,
    #[serde(default)]
    pub items_view: Option<TurnItemsView>,
    pub status: TurnStatus,
    #[serde(default)]
    pub error: Option<TurnError>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub turn: Turn,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurnsListResponse {
    #[serde(default)]
    pub data: Vec<Turn>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemEntry {
    pub turn_id: String,
    pub item: ThreadItem,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemsListResponse {
    #[serde(default)]
    pub data: Vec<ThreadItemEntry>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------- inbound: items

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessageItem {
    pub id: String,
    #[serde(default)]
    pub content: Vec<Value>,
    /// The client's own id of a message sent with `turn/steer`; `None` for a turn's own input.
    #[serde(default)]
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageItem {
    pub id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningItem {
    pub id: String,
    #[serde(default)]
    pub summary: Vec<String>,
    #[serde(default)]
    pub content: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionItem {
    pub id: String,
    /// Exactly as Codex runs it, shell wrapper included.
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    pub status: ItemStatus,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub aggregated_output: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

/// What a file change does to one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchChangeKind {
    Add,
    Delete,
    Update { move_path: Option<String> },
    Unknown(String),
}

impl<'de> Deserialize<'de> for PatchChangeKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let kind = v.get("type").and_then(Value::as_str).map(str::to_string);
        Ok(match kind.as_deref() {
            Some("add") => PatchChangeKind::Add,
            Some("delete") => PatchChangeKind::Delete,
            Some("update") => PatchChangeKind::Update {
                move_path: v
                    .get("move_path")
                    .or_else(|| v.get("movePath"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            Some(other) => PatchChangeKind::Unknown(other.to_string()),
            None => PatchChangeKind::Unknown(v.to_string()),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    #[serde(default)]
    pub diff: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeItem {
    pub id: String,
    #[serde(default)]
    pub changes: Vec<FileUpdateChange>,
    pub status: ItemStatus,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallItem {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub status: ItemStatus,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollabAgentToolCallItem {
    pub id: String,
    pub tool: String,
    pub status: ItemStatus,
    #[serde(default)]
    pub sender_thread_id: Option<String>,
    #[serde(default)]
    pub receiver_thread_ids: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// A subagent's announcement: `kind: started` names a new thread of the tab's family.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubAgentActivityItem {
    pub id: String,
    pub kind: SubAgentKind,
    pub agent_thread_id: String,
    /// The subagent's task name, such as `/root/child`.
    #[serde(default)]
    pub agent_path: String,
}

/// One item of a turn.
#[derive(Debug, Clone)]
pub enum ThreadItem {
    UserMessage(UserMessageItem),
    AgentMessage(AgentMessageItem),
    Reasoning(ReasoningItem),
    CommandExecution(CommandExecutionItem),
    FileChange(FileChangeItem),
    McpToolCall(McpToolCallItem),
    CollabAgentToolCall(CollabAgentToolCallItem),
    SubAgentActivity(SubAgentActivityItem),
    /// An item type this build does not show (web search, plans, images, review markers, future ones).
    Unknown {
        item_type: String,
        id: Option<String>,
        raw: Value,
    },
    /// A known item type missing a field it must carry.
    Malformed {
        item_type: String,
        id: Option<String>,
        error: String,
    },
}

impl ThreadItem {
    pub fn id(&self) -> Option<&str> {
        match self {
            ThreadItem::UserMessage(i) => Some(&i.id),
            ThreadItem::AgentMessage(i) => Some(&i.id),
            ThreadItem::Reasoning(i) => Some(&i.id),
            ThreadItem::CommandExecution(i) => Some(&i.id),
            ThreadItem::FileChange(i) => Some(&i.id),
            ThreadItem::McpToolCall(i) => Some(&i.id),
            ThreadItem::CollabAgentToolCall(i) => Some(&i.id),
            ThreadItem::SubAgentActivity(i) => Some(&i.id),
            ThreadItem::Unknown { id, .. } | ThreadItem::Malformed { id, .. } => id.as_deref(),
        }
    }

    fn from_value(v: Value) -> ThreadItem {
        let item_type = v.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let id = v.get("id").and_then(Value::as_str).map(str::to_string);
        fn typed<T: DeserializeOwned>(
            v: Value,
            f: fn(T) -> ThreadItem,
            item_type: &str,
            id: Option<String>,
        ) -> ThreadItem {
            match serde_json::from_value::<T>(v) {
                Ok(t) => f(t),
                Err(e) => ThreadItem::Malformed {
                    item_type: item_type.to_string(),
                    id,
                    error: e.to_string(),
                },
            }
        }
        match item_type.as_str() {
            "userMessage" => typed(v, ThreadItem::UserMessage, &item_type, id),
            "agentMessage" => typed(v, ThreadItem::AgentMessage, &item_type, id),
            "reasoning" => typed(v, ThreadItem::Reasoning, &item_type, id),
            "commandExecution" => typed(v, ThreadItem::CommandExecution, &item_type, id),
            "fileChange" => typed(v, ThreadItem::FileChange, &item_type, id),
            "mcpToolCall" => typed(v, ThreadItem::McpToolCall, &item_type, id),
            "collabAgentToolCall" => typed(v, ThreadItem::CollabAgentToolCall, &item_type, id),
            "subAgentActivity" => typed(v, ThreadItem::SubAgentActivity, &item_type, id),
            _ => ThreadItem::Unknown { item_type, id, raw: v },
        }
    }
}

impl<'de> Deserialize<'de> for ThreadItem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(ThreadItem::from_value(Value::deserialize(d)?))
    }
}

// ---------------------------------------------------------------- inbound: notifications

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStarted {
    pub thread: Thread,
}

/// `turn/started` and `turn/completed`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNotification {
    pub thread_id: String,
    pub turn: Turn,
}

/// `item/started` and `item/completed`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item: ThreadItem,
}

/// A text delta of one item: an agent message, reasoning, or a command's output.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemDelta {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default)]
    pub delta: String,
    /// The summary part a reasoning summary delta writes to; absent on every other delta.
    #[serde(default)]
    pub summary_index: Option<i64>,
    /// The content part a raw reasoning delta writes to; absent on every other delta.
    #[serde(default)]
    pub content_index: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoReview {
    pub status: ReviewStatus,
    #[serde(default)]
    pub risk_level: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
}

/// `item/autoApprovalReview/started|completed`: Codex's reviewer deciding a request in Auto.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoReviewNotification {
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub review_id: Option<String>,
    /// The reviewed item; none for a network request.
    #[serde(default)]
    pub target_item_id: Option<String>,
    pub review: AutoReview,
    /// What was reviewed (`{"type": "networkAccess", "host": ..., "protocol": ...}` and others), kept as
    /// Codex wrote it: only a network review's host is read from it.
    #[serde(default)]
    pub action: Option<Value>,
}

impl AutoReviewNotification {
    /// The host and protocol a network review is about.
    pub fn network(&self) -> Option<(String, String)> {
        let action = self.action.as_ref()?;
        if action.get("type").and_then(Value::as_str) != Some("networkAccess") {
            return None;
        }
        let host = action.get("host").and_then(Value::as_str)?;
        let protocol = action.get("protocol").and_then(Value::as_str).unwrap_or_default();
        Some((host.to_string(), protocol.to_string()))
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub reasoning_output_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default)]
    pub total: TokenBreakdown,
    #[serde(default)]
    pub last: TokenBreakdown,
    #[serde(default)]
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageUpdated {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: TokenUsage,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    #[serde(default)]
    pub used_percent: Option<f64>,
    #[serde(default)]
    pub window_duration_mins: Option<i64>,
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// The account's rate-limit state. Nothing in it identifies the account.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    #[serde(default)]
    pub limit_id: Option<String>,
    #[serde(default)]
    pub primary: Option<RateLimitWindow>,
    #[serde(default)]
    pub secondary: Option<RateLimitWindow>,
    #[serde(default)]
    pub plan_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsUpdated {
    pub rate_limits: RateLimitSnapshot,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStartupStatus {
    #[serde(default)]
    pub thread_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookRun {
    pub id: String,
    #[serde(default)]
    pub event_name: Option<String>,
    /// The configuration layer the hook came from (`user`, `project`, ...).
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_path: Option<String>,
}

/// `hook/started` and `hook/completed`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookNotification {
    pub thread_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    pub run: HookRun,
}

/// `error`: a turn's error, possibly one Codex retries by itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    pub error: TurnError,
    #[serde(default)]
    pub will_retry: bool,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolved {
    pub thread_id: String,
    pub request_id: Value,
}

/// A notification from the app-server.
#[derive(Debug, Clone)]
pub enum ServerNotification {
    ThreadStarted(ThreadStarted),
    TurnStarted(TurnNotification),
    TurnCompleted(TurnNotification),
    ItemStarted(ItemNotification),
    ItemCompleted(ItemNotification),
    AgentMessageDelta(ItemDelta),
    ReasoningSummaryDelta(ItemDelta),
    ReasoningTextDelta(ItemDelta),
    CommandOutputDelta(ItemDelta),
    AutoReviewStarted(AutoReviewNotification),
    AutoReviewCompleted(AutoReviewNotification),
    TokenUsageUpdated(TokenUsageUpdated),
    RateLimitsUpdated(RateLimitsUpdated),
    McpStartupStatus(McpStartupStatus),
    HookStarted(HookNotification),
    HookCompleted(HookNotification),
    Error(ErrorNotification),
    ServerRequestResolved(ServerRequestResolved),
    /// A method this module does not type; Eitri ignores it.
    Unknown {
        method: String,
        params: Value,
    },
    /// A known method whose payload lacks what it must carry.
    Malformed {
        method: String,
        error: String,
        /// The thread the payload named, when its `threadId` could still be read.
        thread_id: Option<String>,
    },
}

fn typed<T: DeserializeOwned, R>(method: &str, params: Value, f: fn(T) -> R, malformed: fn(String, String) -> R) -> R {
    match serde_json::from_value::<T>(params) {
        Ok(t) => f(t),
        Err(e) => malformed(method.to_string(), e.to_string()),
    }
}

fn malformed_notification(method: String, error: String) -> ServerNotification {
    ServerNotification::Malformed {
        method,
        error,
        thread_id: None,
    }
}

/// Reads one notification. Never fails: an unknown method is `Unknown`, a bad payload `Malformed`.
pub fn parse_notification(name: &str, params: Value) -> ServerNotification {
    use ServerNotification as N;
    let m = malformed_notification;
    // Read before the payload is consumed: a turn's start or end that cannot be read whole still says
    // whose it was, which is what decides what its loss stops.
    let thread_id = if name == method::TURN_STARTED || name == method::TURN_COMPLETED {
        params.get("threadId").and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };
    let parsed = match name {
        method::THREAD_STARTED => typed(name, params, N::ThreadStarted, m),
        method::TURN_STARTED => typed(name, params, N::TurnStarted, m),
        method::TURN_COMPLETED => typed(name, params, N::TurnCompleted, m),
        method::ITEM_STARTED => typed(name, params, N::ItemStarted, m),
        method::ITEM_COMPLETED => typed(name, params, N::ItemCompleted, m),
        method::AGENT_MESSAGE_DELTA => typed(name, params, N::AgentMessageDelta, m),
        method::REASONING_SUMMARY_DELTA => typed(name, params, N::ReasoningSummaryDelta, m),
        method::REASONING_TEXT_DELTA => typed(name, params, N::ReasoningTextDelta, m),
        method::COMMAND_OUTPUT_DELTA => typed(name, params, N::CommandOutputDelta, m),
        method::AUTO_REVIEW_STARTED => typed(name, params, N::AutoReviewStarted, m),
        method::AUTO_REVIEW_COMPLETED => typed(name, params, N::AutoReviewCompleted, m),
        method::TOKEN_USAGE_UPDATED => typed(name, params, N::TokenUsageUpdated, m),
        method::RATE_LIMITS_UPDATED => typed(name, params, N::RateLimitsUpdated, m),
        method::MCP_STARTUP_STATUS => typed(name, params, N::McpStartupStatus, m),
        method::HOOK_STARTED => typed(name, params, N::HookStarted, m),
        method::HOOK_COMPLETED => typed(name, params, N::HookCompleted, m),
        method::ERROR => typed(name, params, N::Error, m),
        method::SERVER_REQUEST_RESOLVED => typed(name, params, N::ServerRequestResolved, m),
        _ => N::Unknown {
            method: name.to_string(),
            params,
        },
    };
    match parsed {
        N::Malformed { method, error, .. } => N::Malformed {
            method,
            error,
            thread_id,
        },
        other => other,
    }
}

// ---------------------------------------------------------------- inbound: server requests

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkApprovalContext {
    pub host: String,
    pub protocol: String,
}

/// `item/commandExecution/requestApproval`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandApprovalParams {
    #[serde(default)]
    pub kind: CommandApprovalKind,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub network_approval_context: Option<NetworkApprovalContext>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `item/fileChange/requestApproval`. The diff is on the `fileChange` item the request names.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    /// A request to write anywhere under this root for the rest of the session.
    #[serde(default)]
    pub grant_root: Option<String>,
}

/// `item/permissions/requestApproval`: extra permissions for a turn or a session. Answered with
/// [`PermissionsGrant::none_for_turn`], so what it asks for is kept only to be shown.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub permissions: Value,
}

/// `mcpServer/elicitation/request`: a tool server's form. Codex also asks before a tool call this way,
/// marking the form in `_meta` and putting the call's arguments there.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationParams {
    pub thread_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    pub server_name: String,
    #[serde(default)]
    pub mode: Option<String>,
    /// The question as Codex words it for a person; read only to tell which tool a tool approval names.
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInputOption {
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub options: Option<Vec<UserInputOption>>,
}

/// `item/tool/requestUserInput`: questions for the user. Codex asks before a tool call this way when
/// its elicitations are turned off.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default)]
    pub questions: Vec<UserInputQuestion>,
}

/// A request from the app-server that needs an answer.
#[derive(Debug, Clone)]
pub enum ServerRequest {
    CommandApproval(CommandApprovalParams),
    FileChangeApproval(FileChangeApprovalParams),
    PermissionsApproval(PermissionsApprovalParams),
    /// An MCP server's elicitation; an MCP tool approval arrives as one. Kept raw.
    McpElicitation(Value),
    /// A question for the user; an MCP tool approval may arrive as one. Kept raw.
    ToolRequestUserInput(Value),
    /// Anything else: answered with "method not found".
    Unknown {
        method: String,
        params: Value,
    },
    Malformed {
        method: String,
        error: String,
    },
}

fn malformed_request(method: String, error: String) -> ServerRequest {
    ServerRequest::Malformed { method, error }
}

/// Reads one server request. Never fails: an unknown method is `Unknown`, a bad payload `Malformed`.
pub fn parse_server_request(name: &str, params: Value) -> ServerRequest {
    use ServerRequest as R;
    let m = malformed_request;
    match name {
        method::COMMAND_APPROVAL => typed(name, params, R::CommandApproval, m),
        method::FILE_CHANGE_APPROVAL => typed(name, params, R::FileChangeApproval, m),
        method::PERMISSIONS_APPROVAL => typed(name, params, R::PermissionsApproval, m),
        method::MCP_ELICITATION => R::McpElicitation(params),
        method::TOOL_REQUEST_USER_INPUT => R::ToolRequestUserInput(params),
        _ => R::Unknown {
            method: name.to_string(),
            params,
        },
    }
}
