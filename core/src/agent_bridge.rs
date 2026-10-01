//! JSON message types and (de)serialization for the Rust<->JS bridge between `shell` and
//! `agent-ui/web`'s embedded frontend, v2 protocol
//!: every
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModeChoice {
    Auto,
    Bypass,
}

impl From<SessionModeChoice> for PermissionMode {
    fn from(choice: SessionModeChoice) -> Self {
        match choice {
            SessionModeChoice::Auto => PermissionMode::Auto,
            SessionModeChoice::Bypass => PermissionMode::Bypass,
        }
    }
}

impl SessionModeChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionModeChoice::Auto => "auto",
            SessionModeChoice::Bypass => "bypass",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(SessionModeChoice::Auto),
            "bypass" => Some(SessionModeChoice::Bypass),
            _ => None,
        }
    }

    /// The next of `offered` (in order), wrapping; a mode not in `offered` goes to the first one it
    /// can parse. **Not what `Shift+Tab` does since v1 (O2 a):** that key is now a fixed
    /// auto/bypass TOGGLE, frozen as such, and goes through `TabSet::cycle_mode`/
    /// `cycle_default_mode`, which switch on the mode explicitly rather than calling this.
    /// `agent_prefs::startup_mode`'s own "the first offered mode" default reads
    /// `CLIENT_IMPLEMENTED_PERMISSION_MODES.first()` directly and does not call this either -- this
    /// function has no production caller left; it is kept for its own tests, below.
    pub fn cycled(self, offered: &[&str]) -> Self {
        let modes: Vec<Self> = offered.iter().filter_map(|m| Self::parse(m)).collect();
        match modes.iter().position(|m| *m == self) {
            Some(at) => modes[(at + 1) % modes.len()],
            None => modes.first().copied().unwrap_or(self),
        }
    }
}

/// C1's mirror: what the page currently is, for the `Ctrl+j`/`Ctrl+k` decision this
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

/// C1's `Ctrl+j`/`Ctrl+k` direction: `Down` is `Ctrl+j` (BROWSE -> INPUT), `Up` is
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
        /// Allows the request and saves the rule Rust offered for this permission id through
        /// `rule_offers`. The panel supplies no rule text.
        #[serde(default)]
        remember: bool,
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
    /// `Shift+Tab`/`<leader>m` toggles any tab between `auto` and `bypass`. Entering bypass
    /// requires `ModeCycle::Confirm`, answered by `confirm_bypass`; leaving bypass is
    /// immediate.
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
    /// `Enter` while a turn runs: queued with the editor context of this moment.
    QueueMessage {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
    },
    /// `↑` on the box's first line with a queue: returns the whole queue as `queue_taken`.
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
    /// The box's text, mirrored 300 ms after the last change. No `command_result`.
    Draft {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
    },
    /// `Ctrl+g` in INPUT (C5): edit `text` in an nvim scratch buffer; `:wq` returns it.
    EditDraft {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        text: String,
    },
    /// `Ctrl+c` on an idle draft: the text goes to history so `↑` recalls it.
    HistoryPush {
        request_id: String,
        text: String,
    },
    /// `gf` or a click on a path opens it.
    OpenPath {
        request_id: String,
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
    },
    /// `Shift+Tab` on the chooser's `New session` row or a record: the same auto/bypass
    /// TOGGLE as `CycleMode`, applied to `TabSet::default_mode` (the window's remembered default for
    /// new tabs) rather than any one tab's own `mode` -- `cycle_mode{tab}` toggles any tab's mode,
    /// live or empty, not only a `NotStarted` one. Window-level for the same reason `TabVerb` is.
    CycleDefaultMode {
        request_id: String,
    },
    /// Sent after `confirm_close_others`'s y/n is answered `y`: the same close path as `CloseTab`,
    /// run once per tab.
    /// Window-level and carries no tab list of its own -- like `TabVerb`, whichever tabs are still
    /// "every tab but the active one" by the time this arrives decides, recomputed fresh rather
    /// than trusting a set the user could have changed while the prompt was open.
    CloseOthers {
        request_id: String,
    },
    /// C1's mirror, posted whenever the page's effective mode changes and once after
    /// `ready`. See [`PanelKeys`]'s own doc.
    PanelKeys {
        request_id: String,
        mode: PanelKeys,
    },
    /// C1's stale-mirror recovery: a `nav_key` the page could not apply against its own
    /// current state. `shell` runs `move_focus(agent, direction)`, i.e. exactly what the chord would
    /// have done had the panel never intercepted it.
    NavFallthrough {
        request_id: String,
        direction: NavKeyDirection,
    },
    /// Ctrl+w h/j/k/l in BROWSE: move keyboard focus from the panel in that direction, by geometry.
    PaneNav {
        request_id: String,
        direction: PaneNavDirection,
    },
    /// `gx` opens this web link in the system browser. The page sends a WHATWG-normalized
    /// address from `agent-ui/web/src/nav.ts#webUrl`; `shell` rechecks it in
    /// `agent_panel.rs#web_url` before `gtk4::UriLauncher`. No `serde(default)`: a missing
    /// address is malformed.
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
    /// `y`/`Y` answers a `confirm_bypass` prompt under the panel's typed-input checks. Names
    /// the tab, or `null` for the window default, and the displayed nonce so stale prompts
    /// cannot answer newer ones. Routed at window level through `tab_ref` because
    /// `tab`/`scope` identifies its target, as for `TabVerb`.
    ConfirmBypass {
        request_id: String,
        #[serde(default)]
        tab: Option<u64>,
        scope: BypassScopeWire,
        // No `#[serde(default)]`: a missing nonce is a protocol error. An answer must
        // identify the exact prompt shown, rather than whichever prompt is current.
        nonce: u64,
    },
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

/// A `tab_verb` payload carries the same actions as `Action::Tab`, including `close_others`.
/// `Select` and `Rename` use `SelectTab` and `RenameTab` because they need a number or text
/// that a verb alone cannot carry.
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
    /// A tab command that names none: a protocol error, never "the active one".
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
            | InboundMessage::OpenUrl { .. } => return TabRef::WindowLevel,
            InboundMessage::SendMessage { tab, .. }
            | InboundMessage::Interrupt { tab, .. }
            | InboundMessage::TurnRendered { tab, .. }
            | InboundMessage::HandoffToTerminal { tab, .. }
            | InboundMessage::PermissionResponse { tab, .. }
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
            | InboundMessage::EditDraft { tab, .. } => *tab,
        };
        match tab {
            Some(id) => TabRef::Named(crate::tabs::TabId(id)),
            None => TabRef::Missing,
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
            | InboundMessage::TurnRendered { request_id, .. }
            | InboundMessage::HandoffToTerminal { request_id, .. }
            | InboundMessage::PermissionResponse { request_id, .. }
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
            | InboundMessage::HistoryPush { request_id, .. }
            | InboundMessage::OpenPath { request_id, .. }
            | InboundMessage::ViewInEditor { request_id, .. }
            | InboundMessage::TabVerb { request_id, .. }
            | InboundMessage::CycleDefaultMode { request_id }
            | InboundMessage::CloseOthers { request_id }
            | InboundMessage::ConfirmBypass { request_id, .. }
            | InboundMessage::RestoreLast { request_id, .. }
            | InboundMessage::RestoreAnswer { request_id, .. }
            | InboundMessage::PanelKeys { request_id, .. }
            | InboundMessage::NavFallthrough { request_id, .. }
            | InboundMessage::PaneNav { request_id, .. }
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

/// `{"kind":"hello","backend":...,"permissionModes":[...],"resumableSessions":[...],...}` is sent
/// once in reply to `ready`, before any snapshot. The backend supplies its runtime capabilities
/// and the sessions this workspace can resume so the frontend can build its start screen.
///
/// `permissionModes` is not narrowed by backend name. Both backends implement an interactive gate
/// and offer the same supported policies.
///
/// `resumableSessions` contains sessions for which the server advertised resume, the client
/// implements it, and this workspace has a persisted provider session id. The frontend renders
/// exactly this array. An empty array means there is nothing to continue.
///
/// Each entry includes the provider, session id, timestamps and display title.
/// `BackendGreeting::for_kind` prefers the CLI transcript's `type:"ai-title"` line, falls back to
/// the persisted first-prompt title, then to `null`. The frontend is not told the title's source,
/// so a missing transcript title silently uses the fallback. It must not invent a label.
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
        // `agent::account`'s configured name, or `null` when none is set.
        "account": greeting.account,
        "restore": restore.map(|offer| json!({ "labels": offer.labels, "bypass": offer.bypass })),
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

/// `{"kind":"pane_focus","focused":bool}`: whether the agent panel's pane holds the window's
/// keyboard focus, as `shell`'s GTK focus tracking decides it (`shell::pane_focus`). The panel dims
/// its mode block when this is `false`. `shell` is the source rather than the page's own
/// `window` focus/blur because `shell` decides Ctrl+h/Ctrl+l, and because the same answer also
/// drives the status bar and the pane outline, so the three cannot disagree.
pub fn serialize_pane_focus_for_js(focused: bool) -> String {
    json!({ "kind": "pane_focus", "focused": focused }).to_string()
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

/// `{"kind":"enter_input"}` opens the composer with a blinking caret when the user enters the
/// panel by keyboard (`Ctrl+l` from the editor). A row click still lands in BROWSE on that row.
/// This is a separate envelope because `pane_focus` cannot distinguish those arrival routes.
pub fn serialize_enter_input_for_js() -> String {
    json!({ "kind": "enter_input" }).to_string()
}

/// `{"kind":"arrive"}`: the keys moved into the panel by keyboard -- replaces
/// `enter_input` at the two arrival sites. Unlike `enter_input`, arriving lands BROWSE
/// rather than opening the composer straight into INPUT. Which row it lands on is the page's own
/// decision, not something this envelope carries: a waiting card, else the last row for a reader
/// who was following it, else the row and scroll they left (a design decision, 2026-09-29).
pub fn serialize_arrive_for_js() -> String {
    json!({ "kind": "arrive" }).to_string()
}

/// `{"kind":"focus_permission","tab":...}`: the chat was brought back to answer a card -- its tray
/// chip `agent ⚑N` activated, or `Ctrl+a a` with a card waiting. The panel goes
/// to BROWSE with its cursor on the oldest pending card. `shell` sends it only when the count it
/// keeps (`crate::attention`) is above zero; a panel that finds no card takes the composer instead,
/// as it does for `enter_input`. `tab` names the session tab it is about; the panel drops it unless
/// that tab is active.
pub fn serialize_focus_permission_for_js(tab: crate::tabs::TabId) -> String {
    json!({ "kind": "focus_permission", "tab": tab.0 }).to_string()
}

/// `{"kind":"keymap", ...}` supplies the `?` overlay from `crate::keymap`.
/// `window` comes from the root table and `prefixKeys` from the effective prefix table.
/// The readable prefix is used in the heading `After <prefix>`.
///
/// Also carries the panel's BROWSE table as `panel` and the effective new-tab chord as
/// `newTabChord` for the chooser's `New session` row. Sent on every `ready` and again when
/// `effective()` changes.
pub fn serialize_keymap_for_js(
    prefix: &str,
    window: &[crate::keymap::HelpRow],
    prefix_keys: &[crate::keymap::HelpRow],
    panel: &crate::keymap::panel::PanelKeymap,
    new_tab_chord: &str,
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
    })
    .to_string()
}

/// `{"kind":"literal_key","key":"C-a"}`: `send-prefix`/`send-keys` with the panel holding the keys
///. WebKitGTK cannot be handed a key, so the panel acts on the ones it knows --
/// `C-a`, select all in a text field -- and ignores the rest. `key` is tmux's spelling.
pub fn serialize_literal_key_for_js(key: &str) -> String {
    json!({ "kind": "literal_key", "key": key }).to_string()
}

/// `{"kind":"open_keymap"}`: `prefix ?` -- open the `?` overlay, in BROWSE.
pub fn serialize_open_keymap_for_js() -> String {
    json!({ "kind": "open_keymap" }).to_string()
}

/// `{"kind":"open_command_line"}`: `prefix :` opens the panel's `:` line in BROWSE. It runs
/// no commands: Enter explains that and Esc closes it. The field captures letters typed after
/// the chord so they do not run as panel keys.
pub fn serialize_open_command_line_for_js() -> String {
    json!({ "kind": "open_command_line" }).to_string()
}

/// `{"kind":"nav_key","direction":"down"}` / `"up"`: C1's decision, made in Rust
/// from the `panel_keys` mirror -- `Ctrl+j` claimed in BROWSE, or `Ctrl+k` claimed in INPUT. The page
/// applies it against its own current state and, if that no longer matches, replies
/// `nav_fallthrough` (`InboundMessage::NavFallthrough`).
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

/// `{"kind":"events","tab":...,"fromRevision":...,"throughRevision":...,"events":[...]}`.
/// Each event uses `AgentDomainEvent`'s tagged `Serialize` shape. `from_revision` is the projection's
/// `last_revision` before folding the batch; `through_revision` is its value afterward.
/// The frontend does not read these revisions. There is no gap detection or resync request;
/// reloading the panel document consumes a fresh snapshot rather than requesting older events.
/// `tab` identifies the session tab. The panel drops the envelope unless that tab is active.
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallNotes {
    /// Tool-use id -> the saved prefix rule that answered its permission request (`Bash(git log *)`,
    /// for example).
    pub allowed_by_rule: BTreeMap<String, String>,
    /// Permission id -> the call's tool-use id, for a `Write` whose file did not exist when its card
    /// was raised: the card says it creates a file rather than warning about
    /// overwriting one it cannot see.
    pub creates_file: BTreeMap<String, Option<String>>,
    /// Maps tool-use ids to row notes for CLI prompts answered locally without a card. The
    /// note explains whether bypass or user approval allowed the call.
    pub prompt_notes: BTreeMap<String, String>,
    /// Tool-use ids of `Write`/`Edit`/`NotebookEdit` calls allowed by the acceptEdits fast
    /// path without a card. The row displays "allowed by auto" through
    /// `tab_set::note_auto_edit_answers`; no separate message is needed. Captured by
    /// `AgentBackend::answer_what_needs_no_human` when answering, rather than inferred from a
    /// missing card.
    pub allowed_by_auto: BTreeSet<String>,
    /// Tool-use ids of `Write` calls allowed without a card over a path that did not exist just
    /// before the answer, either through the acceptEdits fast path in `Auto` or through bypass.
    /// `creates_file` only learns from delivered cards, so it cannot identify these new files.
    /// This set keeps their rows from showing an overwrite warning for a file that did not exist.
    /// Its source is `AgentBackend::answer_what_needs_no_human`'s `AnsweredForYou::creates_file`.
    /// The wire key `autoCreatesFile` covers bypass answers too.
    pub auto_creates_file: BTreeSet<String>,
}

impl CallNotes {
    pub fn is_empty(&self) -> bool {
        self.allowed_by_rule.is_empty()
            && self.creates_file.is_empty()
            && self.prompt_notes.is_empty()
            && self.allowed_by_auto.is_empty()
            && self.auto_creates_file.is_empty()
    }

    /// Whether this tool-use id's `Write` created its file (see `creates_file` and, for the
    /// fast-path case a card never raised, `auto_creates_file`).
    fn creates_file_call(&self, tool_use_id: &str) -> bool {
        self.creates_file.values().any(|id| id.as_deref() == Some(tool_use_id))
            || self.auto_creates_file.contains(tool_use_id)
    }
}

/// [`serialize_events_for_js`], with this batch's row notes:
/// `"ruleNotes":[{"toolUseId":..,"rule":"Bash(git log *)"}]`,
/// `"createsFile":[{"permissionId":..,"toolUseId":..|null}]`,
/// `"autoNotes":["toolu_..", ...]`, and `"autoCreatesFile":["toolu_..", ...]`.
/// `autoCreatesFile` includes bypass `Write` calls too. Each field is omitted when empty.
pub fn serialize_events_with_notes_for_js(
    tab: crate::tabs::TabId,
    from_revision: u64,
    through_revision: u64,
    events: &[AgentDomainEvent],
    notes: &CallNotes,
) -> String {
    let mut payload = json!({
        "kind": "events",
        "tab": tab.0,
        "fromRevision": from_revision,
        "throughRevision": through_revision,
        "events": events
    });
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
    /// Permission ids omitted from `pendingPermissions` by `serialize_snapshot_for_js`.
    /// `Tab::host_answered` holds requests already answered locally while awaiting provider
    /// resolution. `None` is used only by tests constructing a bare `SnapshotView` without a
    /// hidden set.
    pub hidden_pending: Option<&'a BTreeSet<String>>,
}

impl<'a> SnapshotView<'a> {
    /// `hidden` is `Tab::host_answered`. Omit these ids from `pendingPermissions` so requests
    /// already answered locally neither draw cards nor count in the tray. Tool calls and
    /// transcript entries remain visible.
    pub fn of(backend: &'a crate::agent_backend::AgentBackend, hidden: &'a BTreeSet<String>) -> Self {
        Self {
            backend: backend.kind().as_str(),
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
/// unless that tab is active.
pub fn serialize_snapshot_for_js(
    tab: crate::tabs::TabId,
    view: &SnapshotView<'_>,
    turn_started_at_ms: Option<u64>,
) -> String {
    serialize_snapshot_with_notes_for_js(tab, view, turn_started_at_ms, &CallNotes::default())
}

/// [`serialize_snapshot_for_js`], with notes preserved across reloads and tab switches.
/// `allowedByRule` identifies calls allowed by saved rules. `createsFile: true` marks a
/// `Write` card and its call when the file did not exist, including calls answered by the
/// acceptEdits fast path without a card (`CallNotes::creates_file_call`). `allowedByAuto:
/// true` marks `Write`/`Edit`/`NotebookEdit` calls answered by that fast path. Other entries
/// omit these fields, as in the events payload.
pub fn serialize_snapshot_with_notes_for_js(
    tab: crate::tabs::TabId,
    view: &SnapshotView<'_>,
    turn_started_at_ms: Option<u64>,
    notes: &CallNotes,
) -> String {
    let projection = &*view.projection;
    // Written out for the same stated reason as `transcript` just below: both field names happen
    // to be single lowercase words today, so the derive would produce the same JSON, but this
    // function exists precisely because Rust and TS name things differently and a field added
    // later would otherwise reach the frontend in whatever spelling Rust used.
    let user_prompts: Vec<Value> = projection
        .user_prompts
        .iter()
        .map(|prompt| json!({ "seq": prompt.seq, "text": prompt.text }))
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
            // O3: the CLI's own prompt, with its words, so a reload or a tab switch keeps the
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

    let state = json!({
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
        "status": status,
        "activeTurnId": projection.active_turn_id,
        "pendingPermissions": pending_permissions,
        // sw-panel-render-2 (2026-09-27): this used to be missing entirely, and `applySnapshot`
        // forced its own copy of the flag to `false` regardless -- but this projection keeps
        // appending to the open message underneath a snapshot taken mid-reply, so the two sides
        // disagreed about whether the last transcript entry was still open. Sending the real bit
        // is what lets `applySnapshot` take it from the wire instead of guessing "closed".
        "assistantMessageOpen": projection.assistant_message_open,
        // the tab's last reported usage, so a tab switch or a panel reload
        // shows the figure the band draws without waiting for the next turn to report one. An
        // explicit `null` until a provider has reported something (never a zeroed object), then
        // `agent::UsageInfo` as it derives: SNAKE_CASE inside, the same shape the `turn_completed`
        // event carries -- the one place in `state` that is not camelCase, so the panel folds a
        // live report and a snapshot's copy with one type (`types.ts`'s `UsageInfo`).
        "usage": projection.usage,
        "capabilities": {
            "resume": capabilities.resume,
            "fork": capabilities.fork,
            "interrupt": capabilities.interrupt,
            "bypassPermissionMode": capabilities.bypass_permission_mode,
            // No `modeSwitch`: live sessions keep their CLI mode, and the tab's permission
            // mode is local to the client.
        },
        "provider": provider,
    });

    // When the running turn started (wall clock ms, `Date.now()`'s clock), which the tab set keeps
    // per tab so a switch or a reload shows the turn's real elapsed time rather than `0s+`. On the
    // envelope, not in `state`: it is the tab's, not the projection's. `null` when no turn runs.
    json!({
        "kind": "snapshot",
        "tab": tab.0,
        "throughRevision": projection.last_revision,
        "state": state,
        "turnStartedAtMs": turn_started_at_ms,
    })
    .to_string()
}

/// `{"kind":"handoff","tab":...,"command":...,"cwd":...,"providerSessionId":...}` supplies the
/// command that continues the closed conversation in the user's terminal.
/// `agent_panel::collect_pending_handoff` dispatches it only after session shutdown completes,
/// so the session is closed and flushed before the command is shown.
///
/// `providerSessionId` is read from the command's argv so the displayed id matches the resumed id.
/// No process has been spawned and no lease taken, so the frontend shows a concurrency warning.
/// `tab` identifies the session tab. The panel drops the envelope unless that tab is active.
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
/// session tab it is about; the panel drops it unless that tab is active.
pub fn serialize_error_for_js(tab: crate::tabs::TabId, message: &str) -> String {
    json!({ "kind": "error", "tab": tab.0, "message": message }).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabStateWire {
    NotStarted,
    Starting,
    Live,
    Ended,
    Failed,
}

impl TabStateWire {
    pub fn as_str(self) -> &'static str {
        match self {
            TabStateWire::NotStarted => "not_started",
            TabStateWire::Starting => "starting",
            TabStateWire::Live => "live",
            TabStateWire::Ended => "ended",
            TabStateWire::Failed => "failed",
        }
    }
}

/// One tab as the panel draws it (the tab bar, the chooser's open rows, the footer's mode pill).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabView {
    pub id: crate::tabs::TabId,
    pub number: u16,
    pub label: String,
    pub name: Option<String>,
    pub state: TabStateWire,
    pub mode: SessionModeChoice,
    pub marker: Option<crate::tabs::Marker>,
    pub pending: usize,
    /// Whether this tab's session can be resumed later; false on legacy.
    pub resumable: bool,
    /// Why a `failed` tab failed, shown when it is.
    pub failure: Option<String>,
    /// The first prompt's title or the resumed record's display title; `null` before one exists.
    /// Filled from `Tab.title` in `Tab::view`.
    pub title: Option<String>,
}

/// `{"kind":"tabs",...}` carries every tab and the active id. Sent on `ready` and when tabs
/// change, before the active tab's snapshot on a switch so the panel can identify it. Also
/// carries `defaultMode` from `TabSet::default_mode`, used by fresh tabs and resumes into new
/// tabs. The chooser reads its mode line from this field, so cycling the mode needs no new
/// chooser payload.
pub fn serialize_tabs_for_js(active: crate::tabs::TabId, tabs: &[TabView], default_mode: SessionModeChoice) -> String {
    let tabs: Vec<Value> = tabs
        .iter()
        .map(|t| {
            json!({
                "id": t.id.0,
                "number": t.number,
                "label": t.label,
                "name": t.name,
                "state": t.state.as_str(),
                "mode": t.mode.as_str(),
                "marker": t.marker.map(|m| m.wire()),
                "pending": t.pending,
                "resumable": t.resumable,
                "failure": t.failure,
                "title": t.title,
            })
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserRecord {
    pub provider_session_id: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Its lease is held by another window: shown as `open in another window`, not choosable.
    pub held_elsewhere: bool,
}

/// `prefix w`: open tabs first, then the records open in no tab, newest first. Opens the chooser.
pub fn serialize_chooser_for_js(open: &[ChooserTab], records: &[ChooserRecord]) -> String {
    let open: Vec<Value> = open
        .iter()
        .map(|t| {
            json!({
                "tab": t.tab.0, "label": t.label, "marker": t.marker.map(|m| m.wire()),
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
            })
        })
        .collect();
    json!({ "kind": "chooser", "open": open, "records": records }).to_string()
}

/// `prefix &`: the footer's y/n prompt, first line the question (`tabs::close_prompt`).
pub fn serialize_confirm_close_for_js(tab: crate::tabs::TabId, lines: &[String]) -> String {
    json!({ "kind": "confirm_close", "tab": tab.0, "lines": lines }).to_string()
}

/// `<leader>bo`: the footer's y/n prompt to close every tab except the active one.
/// Carries the tab ids and `tabs::close_others` prompt lines in the same shape as `confirm_close`.
pub fn serialize_confirm_close_others_for_js(tabs: &[crate::tabs::TabId], lines: &[String]) -> String {
    let ids: Vec<u64> = tabs.iter().map(|t| t.0).collect();
    json!({ "kind": "confirm_close_others", "tabs": ids, "lines": lines }).to_string()
}

/// `prefix ,`: open the inline rename field on the tab's label, prefilled with `current`.
pub fn serialize_begin_rename_for_js(tab: crate::tabs::TabId, current: Option<&str>) -> String {
    json!({ "kind": "begin_rename", "tab": tab.0, "current": current }).to_string()
}

/// A tab's queue. Sends only the typed text; the wire text also carries editor context, which
/// the panel must not display as the user's own words.
pub fn serialize_queue_for_js(
    tab: crate::tabs::TabId,
    items: &[crate::tab_set::Queued],
    error: Option<&str>,
) -> String {
    let items: Vec<Value> = items
        .iter()
        .map(|q| json!({ "text": q.text, "queuedAt": q.queued_at_ms }))
        .collect();
    json!({ "kind": "queue", "tab": tab.0, "items": items, "error": error }).to_string()
}

/// A tab's unsent draft, sent only when the panel's own copy is not the latest.
pub fn serialize_draft_for_js(tab: crate::tabs::TabId, text: &str) -> String {
    json!({ "kind": "draft", "tab": tab.0, "text": text }).to_string()
}

/// The reply to `take_back_queue`: the whole queue, oldest first.
pub fn serialize_queue_taken_for_js(tab: crate::tabs::TabId, texts: &[String]) -> String {
    json!({ "kind": "queue_taken", "tab": tab.0, "texts": texts }).to_string()
}

/// The project's prompt history, oldest first. Window-level: every tab shares it.
pub fn serialize_history_for_js(entries: &[String]) -> String {
    json!({ "kind": "history", "entries": entries }).to_string()
}

/// The pending permission ids for which a prefix rule is offered, with each rule's display text.
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

/// What V1's line says: the file, relative to the project when inside it, and a selection's lines.
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

/// V1. Window-level: the editor is the window's.
pub fn serialize_editor_context_for_js(summary: Option<&ContextSummary>) -> String {
    json!({
        "kind": "editor_context",
        "file": summary.map(|s| s.file.clone()),
        "lines": summary.and_then(|s| s.lines).map(|(a, b)| json!([a, b])),
    })
    .to_string()
}

/// Whether this tab's draft is out in an nvim scratch buffer (C5).
pub fn serialize_scratch_for_js(tab: crate::tabs::TabId, editing: bool) -> String {
    json!({ "kind": "scratch", "tab": tab.0, "editing": editing }).to_string()
}

/// A one-line message for the footer's transient slot.
pub fn serialize_notice_for_js(text: &str) -> String {
    json!({ "kind": "notice", "text": text }).to_string()
}

/// `{"kind":"confirm_bypass","tab":...,"scope":...,"nonce":...,"lines":[...]}`.
/// Rust retains and rechecks the approval list when confirmation arrives; `plan.approve` is not sent.
/// The panel only displays `lines` and answers with the supplied tab, scope and nonce.
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
    }

    #[test]
    fn the_keymap_envelope_carries_the_panel_table() {
        let (panel, _) = crate::keymap::panel::effective(&Default::default(), None);
        let v: serde_json::Value =
            serde_json::from_str(&serialize_keymap_for_js("Ctrl+b", &[], &[], &panel, "Ctrl+b c")).unwrap();
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

    /// An unrecognized tab verb parses as `None`, like any other unrecognized wire value.
    /// `parse_inbound_message` returns `Option<InboundMessage>`, so the test checks `is_none()`.
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
        let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js()).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "open_keymap" }));
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

    /// Checks the five page-to-Rust wire strings so the page's `bridge.test.ts` can serialize
    /// the same message shapes.
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

    /// `Ctrl+w h/j/k/l` in BROWSE sends window-level pane navigation with the request id.
    /// All four directions are supported; the message names no session tab.
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

    /// The same one-directional reasoning as `DecisionChoice`'s: a direction Rust does not know is a
    /// parse failure, never a move some later `match` chose for it. `"sideways"` is the plan's own
    /// example; `"Left"` and `"north"` are the near misses (case, and a compass instead of vim's keys),
    /// and a message with no direction or no request id is malformed like any other.
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

    /// `gx` sends a window-level `open_url` message with the request id and chosen address.
    /// The link belongs to the current row, so no session tab is named. The JSON string matches
    /// what `agent-ui/web/src/bridge.test.ts` serializes.
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

    /// a tab's last usage figure rides its snapshot, so a tab switch or a
    /// panel reload shows it at once instead of waiting for the next turn to report one. `null` --
    /// present as a key, never a zeroed object -- until a provider has reported something, and
    /// replaced whole by each later report. Inside, snake_case: the same shape the `turn_completed`
    /// event carries (`agent::UsageInfo`'s own derive), where the rest of `state` is camelCase.
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

    /// O3: a reload or a tab switch rebuilds the panel from this snapshot, so the CLI's own prompt
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

    /// Checks every key in the restored-history notice. The panel's four wordings read
    /// different keys; a missing key renders as `undefined`, which the TS type does not
    /// allow.
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
        // Every restored item sits below `uptoSeq`, letting the panel distinguish historical
        // tool calls that cannot complete from live calls still running.
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
    /// `pendingPermissions`. Tool calls and transcript entries are unaffected; this test
    /// constructs neither, so their absence does not test that behavior.
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

    /// The wire contract at the top of the v1-mode plan, both directions.
    #[test]
    fn the_confirm_bypass_envelope_matches_the_wire_contract() {
        let tab_plan = crate::tab_set::BypassPlan {
            scope: crate::tab_set::BypassScope::Tab(TabId(3)),
            nonce: 17,
            approve: vec!["p1".to_string(), "p2".to_string()],
            // Use the strings `TabSet` sends so this assertion cannot drift from the actual prompt text.
            lines: vec![crate::tabs::bypass_prompt(crate::tabs::PromptScope::LiveTab, 2)],
            prompt: crate::tabs::PromptScope::LiveTab,
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

        // A missing nonce fails to parse. An answer must identify the exact prompt shown,
        // rather than whichever prompt is current.
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

    /// The panel's half of what the user asked: a snapshot
    /// must carry `user_prompts` too, or a reload/resync loses every prompt the live event stream
    /// already showed. There is no shared `view_of`/`SnapshotView` builder in this module -- every
    /// neighbouring test constructs one inline with the fields it needs and defaults for the rest,
    /// so this follows that pattern rather than inventing a helper.
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
    fn the_legacy_hello_keeps_its_real_two_mode_choice() {
        let greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Legacy,
            std::path::PathBuf::from("/tmp/project"),
        );
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["backend"], "legacy");
        assert_eq!(parsed["permissionModes"], json!(["auto", "bypass"]));
        assert_eq!(
            parsed["resumableSessions"],
            json!([]),
            "the legacy backend can never offer a resume"
        );
        assert!(parsed["expectedVerdandiRevision"].is_null());
    }

    #[test]
    fn a_tab_command_parses_and_says_which_tab_it_names() {
        let named = parse_inbound_message(r#"{"type":"send_message","request_id":"r1","tab":3,"text":"hi"}"#).unwrap();
        assert!(matches!(named.tab_ref(), TabRef::Named(TabId(3))));
        // Parse the request so its requestId can be answered, then refuse it for naming no tab.
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
            "start_session is gone"
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
    fn the_tabs_envelope_lists_every_tab_with_its_marker_and_the_active_one() {
        let tabs = [
            TabView {
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
                title: Some("fix the parser".into()),
            },
            TabView {
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
                title: None,
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
                "title": "fix the parser"
            })
        );
        assert_eq!(value["tabs"][1]["state"], "failed");
        assert_eq!(value["tabs"][1]["marker"], "ended");
        assert_eq!(value["tabs"][1]["name"], serde_json::Value::Null);
        assert_eq!(value["tabs"][1]["title"], serde_json::Value::Null);
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
        use crate::tab_set::Queued;
        let tab = TabId(4);
        let queue = [Queued {
            text: "and the tests".into(),
            wire: "ctx\n\nand the tests".into(),
            queued_at_ms: 17,
        }];
        let value = |s: String| -> Value { serde_json::from_str(&s).unwrap() };
        assert_eq!(
            value(serialize_queue_for_js(tab, &queue, Some("refused"))),
            json!({ "kind": "queue", "tab": 4, "items": [{ "text": "and the tests", "queuedAt": 17 }], "error": "refused" }),
            "the wire text (with its context) never goes to the panel"
        );
        assert_eq!(
            value(serialize_draft_for_js(tab, "half")),
            json!({ "kind": "draft", "tab": 4, "text": "half" })
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

    /// the snapshot no longer carries `modeSwitch` -- no session's CLI mode is ever
    /// switched, so there is no capability for the panel to gate on. Absent, not `false`: a panel
    /// still reading it must see nothing, never a "no" it could mistake for a live answer.
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
    fn hello_carries_what_the_launch_may_bring_back_or_null() {
        let greeting = greeting_with(Vec::new());
        let value: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert!(value["restore"].is_null(), "present, and null when nothing is on offer");
        let offer = crate::tab_restore::RestoreOffer {
            labels: vec!["api".to_string(), "docs".to_string()],
            bypass: 1,
        };
        let value: Value = serde_json::from_str(&serialize_hello_with_restore_for_js(&greeting, Some(&offer))).unwrap();
        assert_eq!(value["restore"], json!({ "labels": ["api", "docs"], "bypass": 1 }));
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
}
