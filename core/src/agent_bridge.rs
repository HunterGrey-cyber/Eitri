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
    /// A `!` line the person entered in the composer. The only message that can run a command: no other
    /// path reads a leading `!`.
    RunShell {
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
    /// Project-relative names for a composer query. No session or file contents are needed.
    PathComplete {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        query: String,
        seq: u64,
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
    /// Requests the window's project chooser without passing project data to the page.
    ChooseProject {
        request_id: String,
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
    /// `y` or `n` (or Escape, a `no`) to the window's question about adding a phone that presented this window's
    /// enrollment code. Window-level, like `ConfirmBypass`: a `yes` meets the window's answer gate, and only the base
    /// page answers it. `grant` is what the device may do, chosen at the `y` (`watch` or `watch+steer`); none of
    /// `nonce`, `yes` or `grant` has a default, so only the exact question shown is answered, with what was chosen.
    RemoteEnrollAnswer {
        request_id: String,
        nonce: u64,
        yes: bool,
        grant: crate::remote_devices::Grant,
    },
    /// `y` or `n` (or Escape) to the window's question about listening for phones on a named address; gated and
    /// window-level like [`InboundMessage::RemoteEnrollAnswer`].
    RemoteListenAnswer {
        request_id: String,
        nonce: u64,
        yes: bool,
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
            | InboundMessage::ChooseProject { .. }
            | InboundMessage::KeymapClosed { .. }
            | InboundMessage::ViewExported { .. }
            | InboundMessage::ReviewRecover { .. }
            | InboundMessage::TrustCommand { .. }
            | InboundMessage::RemoteEnrollAnswer { .. }
            | InboundMessage::RemoteListenAnswer { .. }
            | InboundMessage::OpenUrl { .. } => return TabRef::WindowLevel,
            // These two name their tab as a plain number: a message without one does not parse.
            InboundMessage::TrustAnswer { tab, .. } | InboundMessage::TrustCancel { tab, .. } => Some(*tab),
            InboundMessage::SendMessage { tab, .. }
            | InboundMessage::RunShell { tab, .. }
            | InboundMessage::Interrupt { tab, .. }
            | InboundMessage::StopBackground { tab, .. }
            | InboundMessage::ToggleFollowing { tab, .. }
            | InboundMessage::StopTask { tab, .. }
            | InboundMessage::TaskOutput { tab, .. }
            | InboundMessage::PathComplete { tab, .. }
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
    path.chars().any(is_control_like)
}

/// Whether `c` is a control character, a bidirectional control or a line or paragraph separator: what no file name and
/// no line of a section may hold.
pub fn is_control_like(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
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
            | InboundMessage::RunShell { request_id, .. }
            | InboundMessage::Interrupt { request_id, .. }
            | InboundMessage::StopBackground { request_id, .. }
            | InboundMessage::ToggleFollowing { request_id, .. }
            | InboundMessage::StopTask { request_id, .. }
            | InboundMessage::TaskOutput { request_id, .. }
            | InboundMessage::PathComplete { request_id, .. }
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
            | InboundMessage::ChooseProject { request_id }
            | InboundMessage::KeymapClosed { request_id, .. }
            | InboundMessage::ViewExported { request_id, .. }
            | InboundMessage::TrustAnswer { request_id, .. }
            | InboundMessage::TrustCancel { request_id, .. }
            | InboundMessage::TrustCommand { request_id, .. }
            | InboundMessage::RemoteEnrollAnswer { request_id, .. }
            | InboundMessage::RemoteListenAnswer { request_id, .. }
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
            // Only the place and the kind: the message is what the person typed, and `e` can quote it.
            let (line, column, kind) = (e.line(), e.column(), e.classify());
            eprintln!(
                "[agent_bridge] failed to parse an inbound message of {} bytes: {kind:?} at {line}:{column}",
                json_str.len()
            );
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

/// Names for the requesting page's current path query. Only the first fifty rows cross the bridge.
pub fn serialize_path_completions_for_js(tab: crate::tabs::TabId, seq: u64, items: &[String]) -> String {
    json!({"kind": "path_completions", "tab": tab.0, "seq": seq,
        "items": items.iter().take(50).collect::<Vec<_>>()})
    .to_string()
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
        "models": list.models, "applied": list.applied, "efforts": list.efforts })
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
    serialize_hello_with_project_for_js(greeting, restore, false)
}

/// The dashboard offers the project chooser only when its window installed a handler.
pub fn serialize_hello_with_project_for_js(
    greeting: &crate::agent_backend::BackendGreeting,
    restore: Option<&crate::tab_restore::RestoreOffer>,
    can_choose_project: bool,
) -> String {
    json!({
        "kind": "hello",
        "canChooseProject": can_choose_project,
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

/// `{"kind":"arrive","intent":"show"|"stop"}`: the same arrival, carrying why the keys came
/// (a reveal to look, or to stop an agent). The intent only informs the page; any action still
/// needs the person's own keys there.
pub fn serialize_arrive_with_intent_for_js(intent: crate::agents::Intent) -> String {
    json!({ "kind": "arrive", "intent": intent }).to_string()
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
    /// The phone behind each message id the provider may still name back with `UserMessageDelivered`
    /// (a steer, or a sent prompt the agent reads only after work of its own), newest last, at most
    /// [`REMOTE_SENDERS_KEPT`]: an id the provider never names back is not kept for the tab's life.
    /// Not taken out at a drop: a message given up on may still be read under the same id.
    #[serde(default, skip_serializing_if = "std::collections::VecDeque::is_empty")]
    pub remote_senders: std::collections::VecDeque<(String, SentFrom)>,
    #[serde(skip)]
    pub remote_notes_dirty: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sent_from: BTreeMap<u64, SentFrom>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stopped_from: BTreeMap<String, SentFrom>,
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

/// The enrolled device responsible for a transcript action.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentFrom {
    pub device_id: String,
    pub name: String,
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

/// How many message ids [`CallNotes::remote_senders`] keeps: more than a provider can still name back
/// (it keeps 64 of each kind of message it may report late).
pub const REMOTE_SENDERS_KEPT: usize = 256;

impl CallNotes {
    /// Remembers which phone sent the message `id`, so its delivery is noted with that phone.
    pub fn note_remote_sender(&mut self, id: String, from: SentFrom) {
        self.remote_senders.retain(|(kept, _)| *kept != id);
        while self.remote_senders.len() >= REMOTE_SENDERS_KEPT {
            self.remote_senders.pop_front();
        }
        self.remote_senders.push_back((id, from));
    }

    /// The phone that sent the message `id`, if one did, taken out.
    pub fn take_remote_sender(&mut self, id: &str) -> Option<SentFrom> {
        let at = self.remote_senders.iter().position(|(kept, _)| kept == id)?;
        self.remote_senders.remove(at).map(|(_, from)| from)
    }

    /// A conversation reset at fold revision `revision`: which phone sent or stopped what in the conversation
    /// before it goes with that conversation, as the view's reset drops it. A prompt after the reset has a
    /// `seq` of at least `revision`.
    pub fn forget_remote_notes_before(&mut self, revision: u64) {
        self.sent_from.retain(|seq, _| *seq >= revision);
        self.stopped_from.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.sent_from.is_empty()
            && self.stopped_from.is_empty()
            && self.allowed_by_rule.is_empty()
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
    add_remote_notes(&mut payload, notes);
    payload.to_string()
}

fn add_remote_notes(value: &mut Value, notes: &CallNotes) {
    if !notes.sent_from.is_empty() {
        value["sentFrom"] = json!(notes.sent_from);
    }
    if !notes.stopped_from.is_empty() {
        value["stoppedFrom"] = json!(notes.stopped_from);
    }
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

    add_remote_notes(&mut state, notes);

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
    /// What can be shown of the instruction files among them. Absent on the wire when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub texts: Vec<TrustTextView>,
}

/// One instruction file at a question about a change: the rows of its diff as the worker folded them,
/// or the line that says why there is none. Row text and `path` are repository text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrustTextView {
    Diff {
        path: String,
        added: usize,
        removed: usize,
        #[serde(rename = "skippedAfter")]
        skipped_after: usize,
        hunks: Vec<TrustHunkView>,
    },
    Plain {
        path: String,
        line: String,
    },
}

/// A run of drawn rows, after `skipped` unchanged lines folded away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustHunkView {
    pub skipped: usize,
    pub lines: Vec<TrustLineView>,
}

/// One row: `kind` is `context`, `added` or `removed`. `noNewline` is left out unless the line ends its
/// side with no line break where the other side has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustLineView {
    pub kind: &'static str,
    pub text: String,
    #[serde(rename = "noNewline", skip_serializing_if = "std::ops::Not::not")]
    pub no_newline: bool,
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
/// `limit` is the number of sessions a project keeps, given once the project holds that many: the list then
/// says that the oldest sessions are dropped, and the key is left out otherwise.
pub fn serialize_chooser_for_js(open: &[ChooserTab], records: &[ChooserRecord], limit: Option<usize>) -> String {
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
    let mut envelope = json!({ "kind": "chooser", "open": open, "records": records });
    if let Some(limit) = limit {
        envelope["limit"] = json!(limit);
    }
    envelope.to_string()
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

/// Whether a `!` line runs in `tab`: its composer behaves as in a running turn until it ends.
pub fn serialize_shell_running_for_js(tab: crate::tabs::TabId, running: bool) -> String {
    json!({ "kind": "shell_running", "tab": tab.0, "running": running }).to_string()
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
mod question_message_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod remote_note_tests {
    use super::*;

    #[test]
    fn notes_without_sent_from_are_byte_identical() {
        let tab = crate::tabs::TabId(1);
        let events = [AgentDomainEvent::UserPromptSubmitted { text: "desk".into() }];
        let expected =
            json!({"kind":"events", "tab":1, "fromRevision":0, "throughRevision":1, "events":events}).to_string();
        assert_eq!(
            serialize_events_with_notes_for_js(tab, 0, 1, &events, &CallNotes::default()),
            expected
        );
    }

    fn phone(n: usize) -> SentFrom {
        SentFrom {
            device_id: format!("d{n}"),
            name: "Pixel 8".into(),
        }
    }

    #[test]
    fn remote_senders_keep_only_the_newest_ids() {
        let mut notes = CallNotes::default();
        for n in 0..=REMOTE_SENDERS_KEPT {
            notes.note_remote_sender(format!("m{n}"), phone(n));
        }
        assert_eq!(notes.remote_senders.len(), REMOTE_SENDERS_KEPT);
        assert_eq!(notes.take_remote_sender("m0"), None, "the oldest id is let go");
        assert_eq!(notes.take_remote_sender("m1"), Some(phone(1)));
        assert_eq!(notes.take_remote_sender("m1"), None, "taken once");
        let newest = format!("m{REMOTE_SENDERS_KEPT}");
        assert_eq!(notes.take_remote_sender(&newest), Some(phone(REMOTE_SENDERS_KEPT)));
    }

    #[test]
    fn a_reset_forgets_the_conversation_before_it() {
        let mut notes = CallNotes::default();
        notes.sent_from.insert(3, phone(1));
        notes.sent_from.insert(9, phone(2));
        notes.stopped_from.insert("t1".into(), phone(1));
        notes.forget_remote_notes_before(9);
        assert_eq!(notes.sent_from.keys().copied().collect::<Vec<_>>(), [9]);
        assert!(notes.stopped_from.is_empty());
    }
}
