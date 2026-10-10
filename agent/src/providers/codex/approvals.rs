//! Codex's requests for a decision: which become a card, which Eitri answers itself, and the exact
//! answer each decision writes.
//!
//! **What a card shows is what Codex will do.** A command card carries the command exactly as Codex runs
//! it, a network card the host and protocol a one-time accept lets through, a file-change card the diff of
//! the `fileChange` item the request names, and a tool card the server, the tool and its arguments from the
//! `mcpToolCall` item it belongs to. A request whose subject Eitri has not seen in full -- no item held for
//! it, an item too large to read, a card too large to show, a tool approval that could belong to calls of
//! different tools -- is declined, never approved blind.
//!
//! **Some requests are never approved, whatever the tab's mode.** Input typed into a running command (the
//! request carries no text to show), a write grant for a whole directory for the rest of the session, and
//! extra permissions are answered here, before any card exists, so bypass cannot approve them either.
//!
//! **No answer widens what later runs without asking.** Approve is a one-time accept, deny a decline,
//! interrupt a cancel; nothing is remembered for the session.

use super::wire::{
    CommandApprovalKind, CommandApprovalParams, Decision, DecisionResponse, ElicitationParams, ElicitationResponse,
    FileChangeApprovalParams, PatchChangeKind, PermissionsApprovalParams, PermissionsGrant, ServerRequest, ThreadItem,
    UserInputParams, UserInputResponse,
};
use crate::jsonrpc_link::{OverCapReply, REQUEST_TOO_LARGE};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// The largest command, set of diffs or tool arguments a card carries. Anything larger cannot be shown
/// for what it is, so it is declined rather than approved half-read.
pub const CARD_MAX_BYTES: usize = 1024 * 1024;

/// How many in-progress items are held for the cards that may name them. Past it an item is not held,
/// and a request naming it is declined as unseen.
pub const HELD_ITEMS_MAX: usize = 512;

/// What `_meta.codex_approval_kind` says on the form Codex builds to ask before a tool call.
const TOOL_APPROVAL_KIND: &str = "mcp_tool_call";
/// The prefix of the question id Codex builds to ask before a tool call, followed by the call's item id.
const TOOL_APPROVAL_QUESTION_PREFIX: &str = "mcp_tool_call_approval_";
/// The option of that question that runs the tool once.
const TOOL_APPROVAL_ACCEPT_LABEL: &str = "Allow";

/// What Eitri says about the reach of an escalated command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wording {
    /// The command runs outside the sandbox.
    OutsideSandbox,
    /// The command wants the network, and may run outside the sandbox to get it.
    Network { host: String, protocol: String },
}

impl Wording {
    pub fn text(&self) -> String {
        match self {
            Wording::OutsideSandbox => "runs outside the sandbox with your full rights".to_string(),
            Wording::Network { host, protocol } => format!(
                "asks for network access to {host} ({protocol}); may run outside the sandbox with your full rights"
            ),
        }
    }
}

/// One path of a file-change card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChangeEntry {
    pub path: String,
    /// `add`, `delete`, `update`, or Codex's own word for a kind this build does not know.
    pub kind: String,
    pub diff: String,
    #[serde(rename = "movePath", skip_serializing_if = "Option::is_none")]
    pub move_path: Option<String>,
}

/// What a card asks the user to approve. It travels as `PermissionRequested.input`, with the tool name of
/// Codex's item type.
#[derive(Debug, Clone, PartialEq)]
pub enum CardKind {
    /// A command, or network access. A network request names a host and no command: Codex asks it for
    /// whatever connects, and the accept lets that host through once, so `command` and `cwd` are `None`
    /// there unless Codex named them.
    Command {
        command: Option<String>,
        cwd: Option<String>,
        escalation: Wording,
        reason: Option<String>,
    },
    FileChange {
        changes: Vec<FileChangeEntry>,
        reason: Option<String>,
    },
    Mcp {
        server: String,
        tool: String,
        arguments: Value,
    },
}

impl CardKind {
    pub fn tool_name(&self) -> &'static str {
        match self {
            CardKind::Command { .. } => "commandExecution",
            CardKind::FileChange { .. } => "fileChange",
            CardKind::Mcp { .. } => "mcpToolCall",
        }
    }

    pub fn input(&self) -> Value {
        match self {
            CardKind::Command {
                command,
                cwd,
                escalation,
                reason,
            } => {
                let network = match escalation {
                    Wording::OutsideSandbox => Value::Null,
                    Wording::Network { host, protocol } => json!({ "host": host, "protocol": protocol }),
                };
                json!({
                    "command": command,
                    "cwd": cwd,
                    "escalation": escalation.text(),
                    "network": network,
                    "reason": reason,
                })
            }
            CardKind::FileChange { changes, reason } => json!({ "changes": changes, "reason": reason }),
            CardKind::Mcp {
                server,
                tool,
                arguments,
            } => json!({ "server": server, "tool": tool, "arguments": arguments }),
        }
    }

    /// What the card asks about, in a few words, for the row Eitri adds when the request's call has none.
    pub fn subject(&self) -> String {
        match self {
            CardKind::Command {
                escalation: Wording::Network { host, protocol },
                ..
            } => format!("network access to {host} ({protocol})"),
            CardKind::Command { .. } => "the command".to_string(),
            CardKind::FileChange { .. } => "the file change".to_string(),
            CardKind::Mcp { server, tool, .. } => format!("{server}'s tool {tool}"),
        }
    }

    /// The bytes of what the card shows that Codex wrote.
    fn shown_bytes(&self) -> usize {
        match self {
            CardKind::Command {
                command,
                cwd,
                escalation,
                reason,
            } => {
                let network = match escalation {
                    Wording::OutsideSandbox => 0,
                    Wording::Network { host, protocol } => host.len() + protocol.len(),
                };
                command.as_deref().map_or(0, str::len)
                    + cwd.as_deref().map_or(0, str::len)
                    + reason.as_deref().map_or(0, str::len)
                    + network
            }
            CardKind::FileChange { changes, reason } => {
                changes
                    .iter()
                    .map(|c| c.path.len() + c.diff.len() + c.move_path.as_deref().map_or(0, str::len))
                    .sum::<usize>()
                    + reason.as_deref().map_or(0, str::len)
            }
            CardKind::Mcp {
                server,
                tool,
                arguments,
            } => server.len() + tool.len() + arguments.to_string().len(),
        }
    }
}

/// A request Eitri answers itself, in every mode, without drawing a card. Each says why on the call's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoAnswer {
    /// Input for a command already running: the request carries no text to show.
    DeclineStdin,
    /// A file change whose diff Eitri has not seen in full.
    DeclineUnseenFileChange,
    /// A write grant for everything under `root` for the rest of the session.
    DeclineGrantRoot { root: String },
    /// Extra permissions for a turn or a session: granted none.
    EmptyPermissionGrant,
    /// A tool server's form that is not a tool approval.
    DeclineOtherElicitation { server: String },
    /// A question that is not a tool approval.
    UnansweredQuestion,
    /// A command Eitri cannot show: none named, too large, or of a kind this build does not know.
    DeclineUnseenCommand,
    /// A tool call Eitri cannot show: no call held for it, calls of different tools it could be, or too
    /// large.
    DeclineUnseenMcpCall,
}

impl AutoAnswer {
    /// The note on the call's row.
    pub fn note(&self) -> String {
        match self {
            AutoAnswer::DeclineStdin => {
                "Codex asked to type into a running command; Eitri cannot show what it would type".to_string()
            }
            AutoAnswer::DeclineUnseenFileChange => "Codex asked to change files Eitri could not show".to_string(),
            AutoAnswer::DeclineGrantRoot { root } => {
                format!("Codex asked to write anywhere under {root} for the session; Eitri does not grant that")
            }
            AutoAnswer::EmptyPermissionGrant => "Codex asked for extra permissions; Eitri grants none".to_string(),
            AutoAnswer::DeclineOtherElicitation { server } => {
                format!("Codex's tool server {server} asked for input Eitri cannot show; Eitri declined")
            }
            AutoAnswer::UnansweredQuestion => "Codex asked a question Eitri cannot show yet".to_string(),
            AutoAnswer::DeclineUnseenCommand => "Codex asked to run a command Eitri could not show".to_string(),
            AutoAnswer::DeclineUnseenMcpCall => "Codex asked to run a tool Eitri could not show".to_string(),
        }
    }
}

/// How the answer to a card is written: the form depends on which request it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerForm {
    /// A command or file-change approval: `accept`, `decline` or `cancel`.
    Decision,
    /// A tool approval asked as a form.
    ToolForm,
    /// A tool approval asked as a question; the id is the question's.
    ToolQuestion { question_id: String },
}

/// A card's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAnswer {
    Approve,
    Deny,
    /// The user interrupted the turn while the card waited.
    Interrupt,
}

impl AnswerForm {
    /// The result to write for `answer`.
    pub fn reply(&self, answer: CardAnswer) -> Value {
        match (self, answer) {
            (AnswerForm::Decision, CardAnswer::Approve) => to_value(&DecisionResponse {
                decision: Decision::Accept,
            }),
            (AnswerForm::Decision, CardAnswer::Deny) => to_value(&DecisionResponse {
                decision: Decision::Decline,
            }),
            (AnswerForm::Decision, CardAnswer::Interrupt) => to_value(&DecisionResponse {
                decision: Decision::Cancel,
            }),
            (AnswerForm::ToolForm, CardAnswer::Approve) => to_value(&ElicitationResponse::accept_once()),
            // A tool approval has no cancel that ends the turn; the turn is interrupted separately.
            (AnswerForm::ToolForm, CardAnswer::Deny | CardAnswer::Interrupt) => {
                to_value(&ElicitationResponse::decline())
            }
            (AnswerForm::ToolQuestion { question_id }, CardAnswer::Approve) => {
                to_value(&UserInputResponse::one(question_id, TOOL_APPROVAL_ACCEPT_LABEL))
            }
            (AnswerForm::ToolQuestion { .. }, CardAnswer::Deny | CardAnswer::Interrupt) => {
                to_value(&UserInputResponse::unanswered())
            }
        }
    }

    /// Whether this answer ends the turn by itself, so no `turn/interrupt` need follow it.
    pub fn ends_the_turn(&self, answer: CardAnswer) -> bool {
        matches!((self, answer), (AnswerForm::Decision, CardAnswer::Interrupt))
    }
}

fn to_value<T: Serialize>(answer: &T) -> Value {
    // These types hold only strings and maps with string keys, which always serialize.
    serde_json::to_value(answer).unwrap_or(Value::Null)
}

/// What to do with one request.
#[derive(Debug, Clone, PartialEq)]
pub enum Handling {
    /// Draw a card on the call `item_id`, asked from turn `turn_id` of `thread_id` when the request named
    /// its turn.
    Card {
        thread_id: String,
        turn_id: Option<String>,
        item_id: String,
        card: CardKind,
        form: AnswerForm,
    },
    /// Answer at once with `reply` and say why on the row of `item_id`, when there is one.
    Auto {
        thread_id: Option<String>,
        item_id: Option<String>,
        tool_name: &'static str,
        answer: AutoAnswer,
        reply: Value,
    },
    /// Not a request Eitri handles: "method not found".
    NotHandled,
}

impl Handling {
    /// The thread the request names, when it names one.
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Handling::Card { thread_id, .. } => Some(thread_id),
            Handling::Auto { thread_id, .. } => thread_id.as_deref(),
            Handling::NotHandled => None,
        }
    }
}

/// A held item's key: the thread it runs on and its id. Item ids are the model's call ids, so two threads
/// of a family may use the same one.
type HeldKey = (String, String);

fn key(thread: &str, id: &str) -> HeldKey {
    (thread.to_string(), id.to_string())
}

/// The items of the running turns a card may be drawn from, as Codex announced them, by thread. Only calls
/// that may still ask are held, and only until they finish.
#[derive(Debug, Default)]
pub struct HeldItems {
    items: HashMap<HeldKey, ThreadItem>,
    /// Arrival order, so a tool approval that matches two identical calls takes the earlier one.
    order: Vec<HeldKey>,
    /// Tool calls a card was already drawn for. Codex asks once per call, so a later tool approval
    /// belongs to another call even while this one still runs.
    asked: HashSet<HeldKey>,
    /// Threads on which a call may have started without being held: past the cap, in a form that could
    /// not be read, or before the thread was followed. A tool approval there that names no tool may be
    /// for that call, so it is not matched to one that is held.
    missed: HashSet<String>,
    /// As `missed`, for a call whose thread could not be read either.
    missed_anywhere: bool,
}

impl HeldItems {
    /// Keeps an item a request may name; other items are not kept.
    pub fn hold(&mut self, thread: &str, item: &ThreadItem) {
        let keep = matches!(
            item,
            ThreadItem::CommandExecution(_) | ThreadItem::FileChange(_) | ThreadItem::McpToolCall(_)
        );
        if matches!(item, ThreadItem::Malformed { .. }) {
            self.mark_missed(thread);
        }
        let Some(id) = item.id() else { return };
        if !keep {
            return;
        }
        let k = key(thread, id);
        if !self.items.contains_key(&k) {
            if self.items.len() >= HELD_ITEMS_MAX {
                self.mark_missed(thread);
                return;
            }
            self.order.push(k.clone());
        }
        self.items.insert(k, item.clone());
    }

    /// Forgets an item: it finished, or a message about it could not be read, so what is held may no
    /// longer be what Codex will do.
    pub fn forget(&mut self, thread: &str, id: &str) {
        let k = key(thread, id);
        if self.items.remove(&k).is_some() {
            self.order.retain(|held| *held != k);
        }
        self.asked.remove(&k);
    }

    /// Forgets an item on every thread: a message about it could not be read and did not say whose it was.
    pub fn forget_everywhere(&mut self, id: &str) {
        self.items.retain(|(_, held), _| held != id);
        self.order.retain(|(_, held)| held != id);
        self.asked.retain(|(_, held)| held != id);
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.asked.clear();
        self.missed.clear();
        self.missed_anywhere = false;
    }

    /// Notes that a call on `thread` may have started without being held.
    pub fn mark_missed(&mut self, thread: &str) {
        self.missed.insert(thread.to_string());
    }

    /// Notes that a call may have started without being held, on a thread that is not known.
    pub fn mark_missed_anywhere(&mut self) {
        self.missed_anywhere = true;
    }

    pub fn get(&self, thread: &str, id: &str) -> Option<&ThreadItem> {
        self.items.get(&key(thread, id))
    }

    /// Notes that a card was drawn for a held call, so no later tool approval is matched to it.
    pub fn mark_asked(&mut self, thread: &str, id: &str) {
        let k = key(thread, id);
        if self.items.contains_key(&k) {
            self.asked.insert(k);
        }
    }

    /// The held tool call a tool approval belongs to: on `thread` and `server`, with exactly these
    /// arguments, no card drawn for it yet, and named `tool` when the approval says which. `None` when
    /// there is none, or when the candidates are calls of different tools: the card would then name a tool
    /// the accept may not be for.
    fn tool_call(
        &self,
        thread: &str,
        server: &str,
        arguments: &Value,
        tool: Option<&str>,
        carded: &dyn Fn(&str, &str) -> bool,
    ) -> Option<&ThreadItem> {
        if tool.is_none() && (self.missed_anywhere || self.missed.contains(thread)) {
            return None;
        }
        let mut candidates = self
            .order
            .iter()
            .filter(|k| k.0 == thread && !self.asked.contains(*k) && !carded(&k.0, &k.1))
            .filter_map(|k| match self.items.get(k) {
                Some(item @ ThreadItem::McpToolCall(call))
                    if call.server == server
                        && call.arguments == *arguments
                        && tool.is_none_or(|named| call.tool == named) =>
                {
                    Some((item, call.tool.as_str()))
                }
                _ => None,
            });
        let (first, first_tool) = candidates.next()?;
        if candidates.any(|(_, other)| other != first_tool) {
            return None;
        }
        Some(first)
    }
}

/// The tool a tool approval names, when its message is the form Codex writes for a tool server of the
/// user's own: `Allow the <server> MCP server to run tool "<tool>"?`. Any other wording names nothing.
fn named_tool<'a>(message: &'a str, server: &str) -> Option<&'a str> {
    let rest = message.strip_prefix("Allow the ")?.strip_prefix(server)?;
    let tool = rest.strip_prefix(" MCP server to run tool \"")?.strip_suffix("\"?")?;
    (!tool.is_empty() && !tool.contains('"')).then_some(tool)
}

/// How the link answers a request from Codex too large to read: the way a request of its method that
/// could not be read is answered, and with "invalid request" where Eitri has no refusal of its own to send.
/// Nothing of what was asked could be read, so nothing is granted.
pub fn over_cap_reply(method: &str) -> OverCapReply {
    use super::wire::method as m;
    if method == m::PERMISSIONS_APPROVAL {
        OverCapReply::Result(to_value(&PermissionsGrant::none_for_turn()))
    } else if method.ends_with("requestApproval") {
        OverCapReply::Result(decline())
    } else {
        OverCapReply::Error {
            code: REQUEST_TOO_LARGE,
            message: "too large",
        }
    }
}

/// What the row of a request answered by [`over_cap_reply`] says, with the tool name of the row Eitri adds
/// when the call has none. `None` for a request that is about no call or question, which only the log names.
pub fn over_cap_note(method: &str) -> Option<(&'static str, String)> {
    use super::wire::method as m;
    let (tool_name, note) = match method {
        m::COMMAND_APPROVAL => ("commandExecution", "Codex asked to run a command; Eitri declined it"),
        m::FILE_CHANGE_APPROVAL => ("fileChange", "Codex asked to change files; Eitri declined it"),
        m::PERMISSIONS_APPROVAL => ("permissions", "Codex asked for extra permissions; Eitri grants none"),
        m::MCP_ELICITATION => ("mcpToolCall", "Codex's tool server asked for input; Eitri refused it"),
        m::TOOL_REQUEST_USER_INPUT => ("requestUserInput", "Codex asked a question; Eitri refused it"),
        other if other.ends_with("requestApproval") => ("approval", "Codex asked for an approval; Eitri declined it"),
        _ => return None,
    };
    Some((
        tool_name,
        format!("{note}, since the request was larger than 16 MiB and could not be shown"),
    ))
}

fn decline() -> Value {
    to_value(&DecisionResponse {
        decision: Decision::Decline,
    })
}

/// Decides what to do with one request. `carded(thread, item)` says whether an item already has a card
/// waiting.
pub fn classify(request: ServerRequest, held: &HeldItems, carded: &dyn Fn(&str, &str) -> bool) -> Handling {
    match request {
        ServerRequest::CommandApproval(p) => command(p, held),
        ServerRequest::FileChangeApproval(p) => file_change(p, held),
        ServerRequest::PermissionsApproval(p) => permissions(p),
        ServerRequest::McpElicitation(params) => elicitation(params, held, carded),
        ServerRequest::ToolRequestUserInput(params) => question(params, held),
        ServerRequest::Malformed { method, .. } => malformed(&method),
        ServerRequest::Unknown { .. } => Handling::NotHandled,
    }
}

/// A request of a known method that could not be read is still answered, and never with a grant: an
/// unanswered request would hang Codex.
fn malformed(method: &str) -> Handling {
    use super::wire::method as m;
    let (tool_name, answer, reply) = match method {
        m::COMMAND_APPROVAL => ("commandExecution", AutoAnswer::DeclineUnseenCommand, decline()),
        m::FILE_CHANGE_APPROVAL => ("fileChange", AutoAnswer::DeclineUnseenFileChange, decline()),
        m::PERMISSIONS_APPROVAL => (
            "permissions",
            AutoAnswer::EmptyPermissionGrant,
            to_value(&PermissionsGrant::none_for_turn()),
        ),
        _ => return Handling::NotHandled,
    };
    Handling::Auto {
        thread_id: None,
        item_id: None,
        tool_name,
        answer,
        reply,
    }
}

fn command(p: CommandApprovalParams, held: &HeldItems) -> Handling {
    let auto = |answer: AutoAnswer| Handling::Auto {
        thread_id: Some(p.thread_id.clone()),
        item_id: Some(p.item_id.clone()),
        tool_name: "commandExecution",
        answer,
        reply: decline(),
    };
    match p.kind {
        CommandApprovalKind::Command => {}
        CommandApprovalKind::WriteStdin => return auto(AutoAnswer::DeclineStdin),
        CommandApprovalKind::Unknown(_) => return auto(AutoAnswer::DeclineUnseenCommand),
    }
    let item = match held.get(&p.thread_id, &p.item_id) {
        Some(ThreadItem::CommandExecution(item)) => Some(item),
        _ => None,
    };
    let command = p.command.clone().or_else(|| item.map(|i| i.command.clone()));
    let cwd = p.cwd.clone().or_else(|| item.and_then(|i| i.cwd.clone()));
    let escalation = match &p.network_approval_context {
        // A network request is about a host, not a command: Codex sends it under an id of its own, with
        // no command and no item, and the accept lets that host through once.
        Some(network) => Wording::Network {
            host: network.host.clone(),
            protocol: network.protocol.clone(),
        },
        None if command.is_none() => return auto(AutoAnswer::DeclineUnseenCommand),
        None => Wording::OutsideSandbox,
    };
    let card = CardKind::Command {
        command,
        cwd,
        escalation,
        reason: p.reason.clone(),
    };
    if card.shown_bytes() > CARD_MAX_BYTES {
        return auto(AutoAnswer::DeclineUnseenCommand);
    }
    Handling::Card {
        thread_id: p.thread_id,
        turn_id: Some(p.turn_id),
        item_id: p.item_id,
        card,
        form: AnswerForm::Decision,
    }
}

fn file_change(p: FileChangeApprovalParams, held: &HeldItems) -> Handling {
    let auto = |answer: AutoAnswer| Handling::Auto {
        thread_id: Some(p.thread_id.clone()),
        item_id: Some(p.item_id.clone()),
        tool_name: "fileChange",
        answer,
        reply: decline(),
    };
    // Checked first: a diff Eitri can show does not make a session-wide grant any smaller.
    if let Some(root) = &p.grant_root {
        return auto(AutoAnswer::DeclineGrantRoot { root: root.clone() });
    }
    let Some(ThreadItem::FileChange(item)) = held.get(&p.thread_id, &p.item_id) else {
        return auto(AutoAnswer::DeclineUnseenFileChange);
    };
    if item.changes.is_empty() {
        return auto(AutoAnswer::DeclineUnseenFileChange);
    }
    let changes = item
        .changes
        .iter()
        .map(|change| {
            let (kind, move_path) = match &change.kind {
                PatchChangeKind::Add => ("add".to_string(), None),
                PatchChangeKind::Delete => ("delete".to_string(), None),
                PatchChangeKind::Update { move_path } => ("update".to_string(), move_path.clone()),
                PatchChangeKind::Unknown(other) => (other.clone(), None),
            };
            FileChangeEntry {
                path: change.path.clone(),
                kind,
                diff: change.diff.clone(),
                move_path,
            }
        })
        .collect();
    let card = CardKind::FileChange {
        changes,
        reason: p.reason.clone(),
    };
    if card.shown_bytes() > CARD_MAX_BYTES {
        return auto(AutoAnswer::DeclineUnseenFileChange);
    }
    Handling::Card {
        thread_id: p.thread_id,
        turn_id: Some(p.turn_id),
        item_id: p.item_id,
        card,
        form: AnswerForm::Decision,
    }
}

fn permissions(p: PermissionsApprovalParams) -> Handling {
    Handling::Auto {
        thread_id: Some(p.thread_id),
        item_id: Some(p.item_id),
        tool_name: "permissions",
        answer: AutoAnswer::EmptyPermissionGrant,
        reply: to_value(&PermissionsGrant::none_for_turn()),
    }
}

/// The card for a tool call, or `None` when it is too large to show.
fn tool_card(item: &ThreadItem) -> Option<CardKind> {
    let ThreadItem::McpToolCall(call) = item else {
        return None;
    };
    let card = CardKind::Mcp {
        server: call.server.clone(),
        tool: call.tool.clone(),
        arguments: call.arguments.clone(),
    };
    (card.shown_bytes() <= CARD_MAX_BYTES).then_some(card)
}

fn elicitation(params: Value, held: &HeldItems, carded: &dyn Fn(&str, &str) -> bool) -> Handling {
    let declined = |thread_id: Option<String>, item_id: Option<String>, tool_name, answer| Handling::Auto {
        thread_id,
        item_id,
        tool_name,
        answer,
        reply: to_value(&ElicitationResponse::decline()),
    };
    let p: ElicitationParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(_) => {
            return declined(
                None,
                None,
                "mcpElicitation",
                AutoAnswer::DeclineOtherElicitation {
                    server: "(unreadable)".to_string(),
                },
            )
        }
    };
    let meta = p.meta.as_ref();
    let is_tool_approval = p.mode.as_deref() == Some("form")
        && meta.and_then(|m| m.get("codex_approval_kind")).and_then(Value::as_str) == Some(TOOL_APPROVAL_KIND);
    if !is_tool_approval {
        return declined(
            Some(p.thread_id),
            None,
            "mcpElicitation",
            AutoAnswer::DeclineOtherElicitation { server: p.server_name },
        );
    }
    // The form names no call; the call it belongs to is the one of this thread on this server with these
    // arguments, announced just before it, and of the tool the message names when it is Codex's own
    // wording.
    let arguments = meta.and_then(|m| m.get("tool_params")).cloned().unwrap_or(Value::Null);
    let tool = p.message.as_deref().and_then(|m| named_tool(m, &p.server_name));
    let Some(item) = held.tool_call(&p.thread_id, &p.server_name, &arguments, tool, carded) else {
        return declined(Some(p.thread_id), None, "mcpToolCall", AutoAnswer::DeclineUnseenMcpCall);
    };
    let item_id = item.id().unwrap_or_default().to_string();
    match tool_card(item) {
        Some(card) => Handling::Card {
            thread_id: p.thread_id,
            turn_id: p.turn_id,
            item_id,
            card,
            form: AnswerForm::ToolForm,
        },
        None => declined(
            Some(p.thread_id),
            Some(item_id),
            "mcpToolCall",
            AutoAnswer::DeclineUnseenMcpCall,
        ),
    }
}

fn question(params: Value, held: &HeldItems) -> Handling {
    let unanswered = to_value(&UserInputResponse::unanswered());
    let p: UserInputParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(_) => {
            return Handling::Auto {
                thread_id: None,
                item_id: None,
                tool_name: "requestUserInput",
                answer: AutoAnswer::UnansweredQuestion,
                reply: unanswered,
            }
        }
    };
    let approval_id = format!("{TOOL_APPROVAL_QUESTION_PREFIX}{}", p.item_id);
    let is_tool_approval = p.questions.len() == 1
        && p.questions[0].id == approval_id
        && p.questions[0]
            .options
            .as_deref()
            .is_some_and(|options| options.iter().any(|o| o.label == TOOL_APPROVAL_ACCEPT_LABEL));
    if !is_tool_approval {
        return Handling::Auto {
            thread_id: Some(p.thread_id),
            item_id: Some(p.item_id),
            tool_name: "requestUserInput",
            answer: AutoAnswer::UnansweredQuestion,
            reply: unanswered,
        };
    }
    match held.get(&p.thread_id, &p.item_id).and_then(tool_card) {
        Some(card) => Handling::Card {
            thread_id: p.thread_id,
            turn_id: Some(p.turn_id),
            item_id: p.item_id,
            card,
            form: AnswerForm::ToolQuestion {
                question_id: approval_id,
            },
        },
        None => Handling::Auto {
            thread_id: Some(p.thread_id),
            item_id: Some(p.item_id),
            tool_name: "mcpToolCall",
            answer: AutoAnswer::DeclineUnseenMcpCall,
            reply: unanswered,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_one_time_and_never_remembered() {
        assert_eq!(
            AnswerForm::Decision.reply(CardAnswer::Approve),
            json!({"decision": "accept"})
        );
        assert_eq!(
            AnswerForm::Decision.reply(CardAnswer::Deny),
            json!({"decision": "decline"})
        );
        assert_eq!(
            AnswerForm::Decision.reply(CardAnswer::Interrupt),
            json!({"decision": "cancel"})
        );
        assert_eq!(
            AnswerForm::ToolForm.reply(CardAnswer::Approve),
            json!({"action": "accept", "content": {}, "_meta": null})
        );
        assert_eq!(
            AnswerForm::ToolForm.reply(CardAnswer::Interrupt),
            json!({"action": "decline", "content": null, "_meta": null})
        );
        let q = AnswerForm::ToolQuestion {
            question_id: "mcp_tool_call_approval_c1".into(),
        };
        assert_eq!(
            q.reply(CardAnswer::Approve),
            json!({"answers": {"mcp_tool_call_approval_c1": {"answers": ["Allow"]}}})
        );
        assert_eq!(q.reply(CardAnswer::Deny), json!({"answers": {}}));
        assert!(AnswerForm::Decision.ends_the_turn(CardAnswer::Interrupt));
        assert!(!AnswerForm::ToolForm.ends_the_turn(CardAnswer::Interrupt));
    }

    #[test]
    fn an_oversized_command_is_declined_not_carded() {
        let params = json!({"threadId": "t", "turnId": "u", "itemId": "i", "command": "x".repeat(CARD_MAX_BYTES + 1)});
        let request = super::super::wire::parse_server_request(super::super::wire::method::COMMAND_APPROVAL, params);
        match classify(request, &HeldItems::default(), &|_, _| false) {
            Handling::Auto { answer, .. } => assert_eq!(answer, AutoAnswer::DeclineUnseenCommand),
            other => panic!("expected a decline, got {other:?}"),
        }
    }

    fn item(v: Value) -> ThreadItem {
        serde_json::from_value(v).expect("an item")
    }

    #[test]
    fn held_items_belong_to_their_thread() {
        let mut held = HeldItems::default();
        let args = json!({"name": "prod"});
        held.hold(
            "parent",
            &item(json!({"type": "commandExecution", "id": "call-1", "command": "ls", "status": "inProgress"})),
        );
        held.hold(
            "child",
            &item(
                json!({"type": "commandExecution", "id": "call-1", "command": "rm -rf build", "status": "inProgress"}),
            ),
        );
        held.hold(
            "child",
            &item(
                json!({"type": "mcpToolCall", "id": "call-2", "server": "probe", "tool": "drop",
                         "arguments": args, "status": "inProgress"}),
            ),
        );
        // The same call id on two threads: each request gets its own thread's command.
        let params = json!({"threadId": "child", "turnId": "u", "itemId": "call-1"});
        let request = super::super::wire::parse_server_request(super::super::wire::method::COMMAND_APPROVAL, params);
        match classify(request, &held, &|_, _| false) {
            Handling::Card {
                card: CardKind::Command { command, .. },
                ..
            } => assert_eq!(command.as_deref(), Some("rm -rf build")),
            other => panic!("expected the child's command, got {other:?}"),
        }
        // A tool approval from the parent does not take the child's call.
        let elicitation = |thread: &str| {
            json!({"threadId": thread, "turnId": "u", "serverName": "probe", "mode": "form",
                   "_meta": {"codex_approval_kind": "mcp_tool_call", "tool_params": args}})
        };
        let request = super::super::wire::parse_server_request(
            super::super::wire::method::MCP_ELICITATION,
            elicitation("parent"),
        );
        match classify(request, &held, &|_, _| false) {
            Handling::Auto { answer, .. } => assert_eq!(answer, AutoAnswer::DeclineUnseenMcpCall),
            other => panic!("expected a decline, got {other:?}"),
        }
        let request =
            super::super::wire::parse_server_request(super::super::wire::method::MCP_ELICITATION, elicitation("child"));
        assert!(matches!(
            classify(request, &held, &|_, _| false),
            Handling::Card { ref item_id, .. } if item_id == "call-2"
        ));
        // Once forgotten on the child, the parent's call of the same id is still held.
        held.forget("child", "call-1");
        assert!(held.get("parent", "call-1").is_some());
        assert!(held.get("child", "call-1").is_none());
        held.forget_everywhere("call-1");
        assert!(held.get("parent", "call-1").is_none());
    }

    #[test]
    fn the_tool_a_message_names_is_read_only_from_codex_wording() {
        assert_eq!(
            named_tool("Allow the probe MCP server to run tool \"probe_write\"?", "probe"),
            Some("probe_write")
        );
        assert_eq!(
            named_tool("Allow the probe MCP server to run tool \"probe_write\"?", "other"),
            None
        );
        assert_eq!(named_tool("Allow?", "probe"), None);
        assert_eq!(named_tool("Allow this app to run tool \"x\"?", "probe"), None);
        assert_eq!(
            named_tool("Allow the probe MCP server to run tool \"\"?", "probe"),
            None
        );
    }

    #[test]
    fn a_question_without_the_accept_option_is_not_a_tool_approval() {
        let params = json!({"threadId": "t", "turnId": "u", "itemId": "c1", "questions": [
            {"id": "mcp_tool_call_approval_c1", "options": [{"label": "Cancel"}]}]});
        let request =
            super::super::wire::parse_server_request(super::super::wire::method::TOOL_REQUEST_USER_INPUT, params);
        match classify(request, &HeldItems::default(), &|_, _| false) {
            Handling::Auto { answer, reply, .. } => {
                assert_eq!(answer, AutoAnswer::UnansweredQuestion);
                assert_eq!(reply, json!({"answers": {}}));
            }
            other => panic!("expected no answer, got {other:?}"),
        }
    }
}
