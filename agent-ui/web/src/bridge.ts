import type { PageLine, AppliedSettings, EffortChoice, ModelOption, AgentDomainEvent, AgentUiSnapshot, ChooserEnvelope, ContextSummary, DetailRow, EditorCardWire, EditorLinkState, HandoffCommand, Hello, QueueItem, ReviewDiffEnvelope, ReviewDraftEnvelope, ReviewEnvelope, ReviewHintEnvelope, ReviewHintLevel, ReviewRecoveryEnvelope, ReviewScope, ReviewSendPreviewEnvelope, TabId, TabsEnvelope, ThinkingDisplay, TurnTimeWire } from "./types";
import type { KeymapHelp, PaneDirection, PluginKeysHelp } from "./keymap";
import { isEditorCardWire } from "./editorCardWire";
import { isHex64, isTrustRemember } from "./trust";
import type { TrustPromptEnvelope } from "./trust";
import type { ProposedChange } from "./proposal";
import type { TabViewState } from "./tabs";
import { detectTransport } from "./transport";
import type { Transport } from "./transport";

export type QuestionAnswerWire = { question_index: number; option_indexes: number[]; other_text?: string | null; notes?: string | null };

export type OutboundMessage =
  | { type: "ready"; request_id: string }
  /** The mounted page has produced its first frame. The host reveals its view and answers nothing. */
  | { type: "painted" }
  | { type: "send_message"; request_id: string; tab: TabId; text: string }
  /** A `!` line entered in the composer: the one message that runs a command. */
  | { type: "run_shell"; request_id: string; tab: TabId; text: string }
  | { type: "question_answer"; request_id: string; tab: TabId; interaction_id: string; answers: QuestionAnswerWire[] }
  | { type: "question_decline"; request_id: string; tab: TabId; interaction_id: string; kind: "cancel" | "chat"; so_far?: QuestionAnswerWire[] }
  | { type: "question_draft"; request_id: string; tab: TabId; interaction_id: string; draft: unknown }
  | { type: "edit_question_other"; request_id: string; tab: TabId; interaction_id: string; question_index: number; text: string }
  | { type: "interrupt"; request_id: string; tab: TabId }
  /** The confirmed `<leader>k`: stop what the tab's agent left running: a Claude tab stops each live task and keeps its
   *  session, another agent's tab ends its session. `running` is
   *  the count the question named; Rust stops nothing when the tab's count is another by then. */
  | { type: "stop_background"; request_id: string; tab: TabId; running: number }
  | { type: "toggle_following"; request_id: string; tab: TabId }
  | { type: "stop_task"; request_id: string; tab: TabId; task_id: string }
  /** `offset` reads the whole output from there, where the session offers it; absent reads the last bytes. */
  | { type: "task_output"; request_id: string; tab: TabId; task_id: string; offset?: number }
  /** Project-relative names only; the CLI expands completed mentions when the draft is sent. */
  | { type: "path_complete"; request_id: string; tab: TabId; query: string; seq: number }
  /** The models the session's CLI offers, for the picker. No turn, so it works while one runs. */
  | { type: "list_models"; request_id: string; tab: TabId }
  /** A change of the running session's model, also during a turn. */
  | { type: "set_model"; request_id: string; tab: TabId; model: string }
  /** A change of the running session's effort, also during a turn. Empty goes back to the settings. */
  | { type: "apply_effort"; request_id: string; tab: TabId; effort: string }
  /** `decision` is a closed set, not a boolean: Rust rejects an unrecognized value at parse time
   *  rather than defaulting it, and the tempting default would be the one that runs the tool.
   *  `reason` is only ever sent with a denial -- there is no field downstream that would show the
   *  model an approval's reason. */
  /** `remember: true` on an allow is "always allow" -- Rust honours it only for
   *  a permission id it itself offered a rule for; the panel's own text is never trusted. */
  | { type: "permission_response"; request_id: string; tab: TabId; permission_id: string; decision: PermissionDecision; reason?: string; remember?: boolean }
  /** The answer to a card Eitri's own tool broker raised (`editor_cards`), by the card's own id. It carries no
   *  reason and no `remember`: a broker card offers neither. `origin` is how the person answered -- `"key"` from
   *  `a`/`d`, `"click"` from the card's button -- and Rust takes no other word. */
  | { type: "editor_card_answer"; request_id: string; tab: TabId; card_id: string; decision: PermissionDecision; origin: "key" | "click" }
  /** How long this WebView took to draw a turn's first assistant text, from its own receipt of the
   *  payload to the animation frame that rendered it. A SPAN, not an instant: `performance.now()`
   *  and Rust's `Instant` have unrelated epochs, so a timestamp crossing this boundary would be a
   *  confident, meaningless number. Diagnostic only -- Rust expects no reply and nothing branches
   *  on it. */
  | { type: "turn_rendered"; request_id: string; tab: TabId; receive_to_frame_ms: number }
  /** "Close this conversation here and give me the command that continues it in a terminal."
   *  Carries nothing else: every input the rule needs is already canonical on the Rust side, and a
   *  session id sent from here would be a second, stale source for the one value that must not be
   *  wrong. The reply is a `handoff` envelope, deferred until the real close has finished. */
  | { type: "handoff_to_terminal"; request_id: string; tab: TabId }
  | { type: "resume"; request_id: string; tab: TabId; provider_session_id: string }
  | { type: "select_tab"; request_id: string; tab: TabId }
  | { type: "rename_tab"; request_id: string; tab: TabId; name: string }
  | { type: "close_tab"; request_id: string; tab: TabId }
  | { type: "reset_tab"; request_id: string; tab: TabId }
  | { type: "cycle_mode"; request_id: string; tab: TabId }
  /** `Shift+Tab` over `New session` or a record in the chooser, once the active tab has already
   *  started: window-level, like `TabVerb` -- there is no tab to name, since
   *  a resume from here would open a fresh one. Cycles `TabSet::default_mode` and re-sends `tabs`. */
  | { type: "cycle_default_mode"; request_id: string }
  | { type: "open_detail"; request_id: string; tab: TabId }
  /** Global `f` HINT: panel has pressed `f` in BROWSE, asking shell to start a global HINT. */
  | { type: "hint_request"; request_id: string }
  /** Answer to hint_collect: the number of visible targets the panel froze for this sessionId. */
  | { type: "hint_targets"; request_id: string; session_id: number; count: number }
  /** Queue this text behind the tab's running turn. Rust answers with a
   *  `command_result`; a refusal never touches the box (the panel already sent it). */
  /** `turn_id`: the turn the page showed running when Enter was pressed; the message goes only into that
   *  one, and is held for its end otherwise. */
  | { type: "queue_message"; request_id: string; tab: TabId; text: string; turn_id: string | null }
  /** `↑`: empty the tab's queue and hand its texts back as `queue_taken`. */
  | { type: "take_back_queue"; request_id: string; tab: TabId }
  /** `Ctrl+Enter`: queue this text (if non-empty), then interrupt if a turn is
   *  running, or flush at once if idle. */
  | { type: "send_now"; request_id: string; tab: TabId; text: string }
  /** The box's text, mirrored so a reload or a tab switch keeps it. Rust answers nothing. */
  | { type: "draft"; request_id: string; tab: TabId; text: string }
  /** The scratch-editor round trip's draft half: the box's text handed
   *  to the nvim scratch buffer for editing with `Ctrl+g`. */
  | { type: "edit_draft"; request_id: string; tab: TabId; text: string }
  /** Append this typed text to the shared prompt history, e.g. `Ctrl+c`'s clear. */
  | { type: "history_push"; request_id: string; text: string }
  /** `gf` on a path: open it in the scratch split, at `line` when given. */
  | { type: "open_path"; request_id: string; path: string; line?: number }
  /** `Ctrl+g` on a row: show this text read-only in the scratch split. */
  | { type: "view_in_editor"; request_id: string; title: string; text: string }
  /** A copy's text for the primary selection too (`primaryCopy.ts`). The shell's page handler takes this
   *  one kind itself and answers nothing, so it carries no request id. */
  | { type: "copy_primary"; text: string }
  /** `Ctrl+g` on a broker card that would apply a proposal: file number `file` of proposal `proposal` of this tab, as
   *  the shell's review store holds it. The texts never come from the page. */
  | { type: "editor_card_view"; request_id: string; tab: TabId; proposal: string; file: number }
  /** `Ctrl+g` on a pending edit card for one file: the file as it is on disk beside the file as the card
   *  would leave it, read-only (`proposal.ts#proposalOf`). `path` is the card's own. */
  | { type: "view_proposal"; request_id: string; path: string; change: ProposedChange }
  /** A tab.* panel-table action (`runPanelAction`): the same verbs the
   *  prefix's own tmux window keys run. Window-level, like the prefix's own `tab_verb` -- whichever
   *  tab is under the keys decides, so this carries no `tab` of its own (Rust's `InboundMessage::
   *  TabVerb` has none either); sent through `postToRust` directly rather than `post()`, which would
   *  add one. `TabVerbWire`'s wire spelling, `core/src/agent_bridge.rs`. `agent` is read by `new`
   *  alone: the word `:tabnew` was typed with (none: the window's default agent). */
  | { type: "tab_verb"; request_id: string; verb: "next" | "prev" | "last" | "new" | "close" | "close_others" | "choose" | "info"; agent?: string }
  /** `y` to a `confirm_close_others` prompt: window-level, like `TabVerb` --
   *  Rust recomputes "every tab but the active one" fresh rather than trusting the set the prompt
   *  was shown with, so this carries no tab list of its own. `InboundMessage::CloseOthers`,
   *  `core/src/agent_bridge.rs`. */
  | { type: "close_others"; request_id: string }
  /** `y`/`Y` to a `confirm_bypass` prompt, guarded by
   *  `modeKey.ts#bypassYesCounts` before this is ever sent. `tab`/`scope`/`nonce` echo the envelope's
   *  own values verbatim -- this side never counts cards or decides which ones a `y` approves; Rust
   *  keeps that list and answers only the delivered cards still pending, still on `tab`, at `nonce`.
   *  `InboundMessage::ConfirmBypass`, `core/src/agent_bridge.rs`. */
  | { type: "confirm_bypass"; request_id: string; tab: TabId | null; scope: "tab" | "default"; nonce: number }
  /** `s` on the launch dashboard: bring back the tabs the last window had open, the first into this
   *  empty tab and the rest into new ones. Names the tab it was pressed in. A saved tab that was in
   *  bypass is asked about first (`confirm_restore`), never given back in bypass unasked.
   *  `InboundMessage::RestoreLast`, `core/src/agent_bridge.rs`. */
  | { type: "restore_last"; request_id: string; tab: TabId }
  /** `y` (`keep_bypass: true`) or `n` to a `confirm_restore` prompt. `nonce` is the envelope's own,
   *  echoed back so only the prompt that was shown can be answered. Window-level, like
   *  `confirm_bypass`. `InboundMessage::RestoreAnswer`, `core/src/agent_bridge.rs`. */
  | { type: "restore_answer"; request_id: string; nonce: number; keep_bypass: boolean }
  /** `y` or `n` to a `trust_prompt`, past the same guards as the bypass prompt's `y`. `tab` and `nonce` name the
   *  prompt that was shown, and `fingerprint`/`findings_digest` are that envelope's `fingerprint` and
   *  `findingsDigest` copied verbatim -- this side never computes either, so Rust can tell an answer to what is
   *  on screen from one to something that has changed since. `InboundMessage::TrustAnswer`,
   *  `core/src/agent_bridge.rs`. */
  | { type: "trust_answer"; request_id: string; tab: TabId; nonce: number; fingerprint: string; findings_digest: string; trust: boolean }
  /** `Escape` on a `trust_prompt`: put the start off. Starts nothing, so it carries no digest.
   *  `InboundMessage::TrustCancel`, `core/src/agent_bridge.rs`. */
  | { type: "trust_cancel"; request_id: string; tab: TabId; nonce: number }
  /** `:trust` or `:untrust` on the command line. Window-level: the trust belongs to the window's project, not to
   *  a tab. `agent` is the agent of the tab on screen (`TabInfo.agent`): each agent's trust is its own, and Rust
   *  refuses a command typed for another agent than the active tab runs. `InboundMessage::TrustCommand`,
   *  `core/src/agent_bridge.rs`. */
  | { type: "trust_command"; request_id: string; action: "trust" | "untrust"; agent: string }
  /** The composer mirror: the effective mode this window is in, whenever it changes (and once
   *  after `ready`). Window-level, like `TabVerb` -- Rust keeps one value per window, not per tab,
   *  because the capture controller (`install_module_nav`) that reads it back is itself installed
   *  once per window. `"other"` covers everything that is neither a live BROWSE nor a live INPUT:
   *  the empty tab's own menu/composer count as `"browse"`/`"input"` too (`core::agent_bridge::
   *  PanelKeys`). */
  | { type: "panel_keys"; request_id: string; mode: PanelKeysMode }
  /** A stale mirror, either way: a `nav_key` this page could not apply -- the mirror was a
   *  keystroke stale, an overlay owns the keys, or the session it named has ended -- comes back
   *  here so Rust runs the chord's ordinary `move_focus` instead of silently dropping the key
   *  (`core::agent_bridge::NavKeyDirection`, `serialize_nav_key_for_js`). */
  | { type: "nav_fallthrough"; request_id: string; direction: NavKeyDirection }
  /** `Ctrl+w h/j/k/l` in BROWSE (vim's `CTRL-W h/j/k/l`): move the keys from
   *  the panel to the module on that side, by geometry -- exactly what `Ctrl+h/j/k/l` do from it (shell
   *  runs `move_focus(agent, direction)`, the same hook a `nav_fallthrough` uses). Window-level, like
   *  `nav_fallthrough`; a side with no module leaves the keys where they are. Rust answers with a
   *  `command_result`, an error only if no hook is installed. `InboundMessage::PaneNav`,
   *  `core/src/agent_bridge.rs`. */
  | { type: "pane_nav"; request_id: string; direction: PaneDirection }
  /** The empty tab's "Open another project" row (`p`) was used. It carries no path: the window opens its own
   *  project chooser. Window-level; `InboundMessage::ChooseProject`, `core/src/agent_bridge.rs`. */
  | { type: "choose_project"; request_id: string }
  | { type: "load_page"; id: string }
  | { type: "page_line"; id: string }
  /** The `?` overlay that `prefix ?` opened was closed. `opening` is the number its `open_keymap` carried, so a
   *  close that arrives after a newer `prefix ?` is told apart; `return_keys` is true only for the overlay's own
   *  keys (`?`, `Esc`, `q`) or a click outside it, never for a close another route forced. Window-level; the
   *  window gives the keys back to the module that held them, and always answers with a `command_result`.
   *  `InboundMessage::KeymapClosed`, `core/src/agent_bridge.rs`. */
  | { type: "keymap_closed"; request_id: string; opening: number; return_keys: boolean }
  /** The answer to `view_export`: this page's view of the tab a move takes to the other window (`null` when it
   *  holds none). `request` is the number `view_export` carried. Window-level; Rust passes the view on unread.
   *  `InboundMessage::ViewExported`, `core/src/agent_bridge.rs`. */
  | { type: "view_exported"; request_id: string; request: number; view: TabViewState | null }
  /** `gx` on a web link: open this address in the system browser. `url` is
   *  the WHATWG-normalized `href` (`nav.ts#webUrl`) -- what the pick showed -- never the spelling the reply
   *  wrote. Window-level, like `open_path`; Rust re-checks it (`panel/src/links.rs#web_url`, http(s) and a plain
   *  host only) and answers with a `command_result`, an error when it is not a web link.
   *  `InboundMessage::OpenUrl`, `core/src/agent_bridge.rs`. */
  | { type: "open_url"; request_id: string; url: string }
  /** `y` (with the grant chosen) or `n`/Escape/Enter to the window's question about adding a phone. Window-level and
   *  base-page only; a `yes` meets the window's answer gate (`InboundMessage::RemoteEnrollAnswer`). */
  | { type: "remote_enroll_answer"; request_id: string; nonce: number; yes: boolean; grant: RemoteGrant }
  /** `y` or `n`/Escape/Enter to the window's question about listening for phones on a named address. */
  | { type: "remote_listen_answer"; request_id: string; nonce: number; yes: boolean }
  /** The device list's key (`remote.devices`) and its `r`: the window answers with `remote_devices`. Base page only;
   *  `InboundMessage::RemoteDevicesOpen`. */
  | { type: "remote_devices_open"; request_id: string }
  /** `a` in the device list: a code, as `eitri remote add-device` makes, by the recipe chosen (`auto` is the window's
   *  default route). A phone presenting it is still added only after the desk's `y`. */
  | { type: "remote_start_enroll"; request_id: string; recipe: RemoteRecipeChoice }
  /** `x` in the device list: the window asks `Revoke <name>? (y/n)` (`remote_revoke_ask`). */
  | { type: "remote_revoke_ask"; request_id: string; device: string }
  /** `y` to the window's revoke question, gated by the window like any answer's `y`. A no is never sent. */
  | { type: "remote_revoke"; request_id: string; device: string; nonce: number }
  /** `c` in BROWSE, and `[`/`]`/`S` inside the review overlay: the files that changed on disk during this
   *  tab's turn (`"latest"`, or a turn number) or, with `scope: "session"`, since the session's first
   *  baseline. Names the tab, as every tab-scoped command does; the reply is a `review` envelope carrying the
   *  same `request_id`, or a `command_result` failure with the reason (no session, no such turn, git
   *  unavailable). `InboundMessage::ReviewRequest`, `core/src/agent_bridge.rs`. */
  | { type: "review_request"; request_id: string; tab: TabId; turn: "latest" | number; scope: ReviewScope }
  /** `Enter` on a file of the review overlay: that file's hunks, under the overlay's current scope. The
   *  reply is a `review_diff` envelope with the same `request_id`, or a `command_result` failure.
   *  `InboundMessage::ReviewDiffRequest`, `core/src/agent_bridge.rs`. */
  | { type: "review_diff_request"; request_id: string; tab: TabId; turn: number; scope: ReviewScope; path: string }
  /** `x` in the review overlay: revert one hunk of the turn shown (`target` names it by id and header, so a
   *  hunk that moved is refused rather than guessed) or the whole file. Answered by a `command_result`.
   *  `InboundMessage::ReviewRevert`, `core/src/agent_bridge.rs`. */
  | { type: "review_revert"; request_id: string; tab: TabId; turn: number; scope: ReviewScope; path: string; target: "file" | { hunk: number; header: string } }
  /** `u`: write back what the last revert made in this tab replaced. Answered by a `command_result`. */
  | { type: "review_undo"; request_id: string; tab: TabId }
  /** `i` then Enter: a comment on lines `from`-`to` of the file's new side. Answered by a `review_draft`. */
  | { type: "review_comment_add"; request_id: string; tab: TabId; turn: number; scope: ReviewScope; path: string; from: number; to: number; text: string }
  /** `x` on a comment. Answered by a `review_draft`. */
  | { type: "review_comment_remove"; request_id: string; tab: TabId; id: number }
  /** `s`: `confirm: null` asks for the preview (a `review_send_preview`); `y` on it sends the draft with the
   *  `digest` the preview carried. A confirmed send is answered by a `command_result`. */
  | { type: "review_send"; request_id: string; tab: TabId; confirm: string | null }
  /** Enter on a recovery row: restore what an interrupted revert replaced, or forget it. Window-level: the
   *  journal belongs to the project, so no tab. Answered by a `command_result`. */
  | { type: "review_recover"; request_id: string; entry: string; answer: "restore" | "dismiss" }
  /** `o` in the review overlay: open the file at the hunk's first line, drawing the turn's hunks in the editor
   *  when the shell can. Answered by a `command_result`. */
  | { type: "open_in_editor"; request_id: string; tab: TabId; turn: number; scope: ReviewScope; path: string; line?: number };

/** The composer mirror's three states. Distinct from `./keymap`'s `PanelMode`, which is
 *  this component's OWN mode ("hint" included, unreachable yet) -- this type is the wire value Rust
 *  reads to decide whether a bare `Ctrl+j`/`Ctrl+k` is claimed at all, and "other" has no `PanelMode`
 *  counterpart (an overlay, a dead session, ... are all folded into it). */
export type PanelKeysMode = "browse" | "input" | "other";

/** Which way a claimed `Ctrl+j`/`Ctrl+k` moved: `down` is `Ctrl+j`, `up` is `Ctrl+k`. */
export type NavKeyDirection = "down" | "up";

/** Every decision a backend can actually carry. Verdandi's wire is `bool allow` + `string reason`
 *  and the legacy hook relay is the same shape, so there is no allow-for-session anywhere to send
 *  one to -- adding a button for it here would produce a control that silently degrades to a plain
 *  allow. Widening this is a Verdandi protocol change first.
 *
 *  "Always allow" is not a third decision: it is `remember: true` on an `"allow"`, which Rust
 *  honours only for a permission id it itself offered a rule for. */
export type PermissionDecision = "allow" | "deny";

let transport: Transport | undefined;
function activeTransport(): Transport {
  return transport ??= detectTransport(window);
}

/** Each transport owns its request-id space for this page load. Requests and mirrors share
 *  its monotonic counter, with a transport-specific prefix keeping their origins distinct. */
export function nextRequestId(): string {
  return activeTransport().nextId("req");
}

/** The id of a mirror (`draft`, `question_draft`): a copy of what the page holds, kept by Rust so a reload or a
 *  switch finds it again. Rust answers a mirror only to refuse it, and nothing on the page waits for that answer,
 *  so the id itself says what it was: a refusal can arrive after the dock or the tab that sent it is gone, and a
 *  table of mirrors in flight would grow with every one Rust took without a word. */
export function nextMirrorId(): string {
  return activeTransport().nextId("mirror");
}
export function isMirrorRequest(requestId: string): boolean {
  return requestId.startsWith("mirror-") || requestId.startsWith("a-mirror-");
}

export function postToRust(message: OutboundMessage): void {
  activeTransport().post(JSON.stringify(message));
}

type InboundHandler = (
  payload:
    | { kind: "hello" } & Hello
    /** `message` is the shell's own one-line word on what it did (`sent`, `queued: ...`), where it has one. */
    | { kind: "command_result"; requestId: string; ok: true; message?: string }
    | { kind: "command_result"; requestId: string; ok: false; error: string }
    /** Plain text: render through revealHidden, never as Markdown or HTML. */
    | { kind: "task_output_result"; requestId: string; tab: TabId; taskId: string; output: string; totalBytes: number; truncated: boolean; offset?: number }
    /** Project-relative suffix matches for the existing gf letter picker. */
    | { kind: "path_choices"; requestId: string; tab: TabId; stopped?: string | null; paths: { path: string; line: number | null }[] }
    /** Repository names are plain text, through revealHidden. */
    | { kind: "path_completions"; tab: TabId; seq: number; items: string[] }
    /** Every word is the CLI's: plain text, through revealHidden. */
    | { kind: "models_result"; requestId: string; tab: TabId; models: ModelOption[]; applied: AppliedSettings; efforts?: EffortChoice[] }
    | { kind: "settings_result"; requestId: string; tab: TabId; applied: AppliedSettings }
    | {
        kind: "events";
        tab: TabId;
        fromRevision: number;
        throughRevision: number;
        events: AgentDomainEvent[];
        /** Post-fold revision per delivered event; gaps include requests answered by the host. */
        revisions?: number[];
        /** Calls in this batch a saved prefix rule answered; absent when none did. */
        ruleNotes?: { toolUseId: string; rule: string }[];
        /** `Write` cards in this batch raised over no file; absent when none were. */
        createsFile?: { permissionId: string; toolUseId: string | null }[];
        /** Calls in this batch whose CLI prompt was answered without a card. */
        promptNotes?: { toolUseId: string; note: string }[];
        /** Tool-use ids of a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast
         *  path answered with no card; absent when none were. */
        autoNotes?: string[];
        /** Tool-use ids of a `Write` answered with no card -- by the
         *  acceptEdits fast path, or in bypass -- over a path where
         *  nothing existed just before the answer -- `createsFile`'s own signal, but for a call that
         *  never raised a card to carry it on. */
        autoCreatesFile?: string[];
        /** The turns that ended in this batch and how long each ran (`TurnTimeWire`); absent when none did. */
        turnTimes?: TurnTimeWire[];
      }
    /** `turnStartedAtMs`: when the running turn started, `Date.now()`'s clock, kept per tab by
     *  Rust (`tab_set`'s `turn_clock`) so a switch or a reload shows its real elapsed time; `null`
     *  (or absent, from an older build) when none runs. `turnTimes`: how long every finished turn of the
     *  tab ran, replacing the panel's whole table (absent from an older build). */
    | { kind: "snapshot"; tab: TabId; throughRevision: number; state: AgentUiSnapshot; turnStartedAtMs?: number | null; turnTimes?: TurnTimeWire[] }
    /** The tab's broker cards, replacing the whole list (Rust's `serialize_editor_cards_for_js`). Sent whenever the
     *  set changes, empty included; the page keeps them only for the tab it shows. */
    | { kind: "editor_cards"; tab: TabId; cards: EditorCardWire[] }
    | ({ kind: "handoff"; tab: TabId } & HandoffCommand)
    | { kind: "theme"; vars: Record<string, string> }
    /** How the panel shows the agent's thinking (`init.lua`'s `agent.thinking`;
     *  `serialize_display_for_js`, `core/src/agent_bridge.rs`). Window-level, sent on every `ready`
     *  before the tabs. */
    | { kind: "display"; thinking: ThinkingDisplay }
    /** Why the project folder is no boundary (`serialize_root_note_for_js`, `core/src/agent_bridge.rs`).
     *  Window-level, sent right after `hello` on every `ready`; the page shows it until its own Dismiss. */
    | { kind: "root_note"; text: string }
    /** The window's turn review switches (`serialize_review_settings_for_js`, `core/src/agent_bridge.rs`).
     *  Window-level, sent on every `ready` before the tabs and again when `init.lua`'s values arrive. */
    | { kind: "review_settings"; enabled: boolean; hint: ReviewHintLevel | boolean }
    /** Whether this panel's pane holds the window's keyboard focus. `shell` decides this from
     *  GTK's focus widget (the window controller's report, `eitri_core::wm::focus`). It is not
     *  decided from this page's own `window` focus/blur: `shell` arbitrates Ctrl+h/Ctrl+l, and the
     *  same answer drives the status bar and the pane outline, so the three agree. */
    | { kind: "pane_focus"; focused: boolean }
    /** Whether the user is typing in the editor, and the gap between the panel's stream pushes
     *  while they do (`typingCadence.ts`; `serialize_editor_typing_for_js`, `core/src/agent_bridge.rs`).
     *  Window-level, and Rust's word is the only one: nothing in the page clears it. */
    | { kind: "editor_typing"; typing: boolean; periodMs: number }
    /** A brand-new tab's own arrival: open the composer with the caret in it. This is `enter_input`'s
     *  only sender now -- every other keyboard arrival that used
     *  to send this now sends `arrive` instead. A click on a row still lands in BROWSE on that row.
     *  See `serialize_enter_input_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "enter_input" }
    /** Every OTHER keyboard arrival (`Ctrl+h/j/k/l` into the chat, `prefix a`/a tray chip with no
     *  card, a launch that starts with the keys already in the chat): BROWSE, on the oldest pending
     *  card if one waits, else on the last row with following resumed. Reverses the 2026-09-19 choice
     *  that `Ctrl+l` put the caret straight into the composer: a reader arrives to read first. See
     *  `serialize_arrive_for_js` in `core/src/agent_bridge.rs`. `intent` says why the keys came (a reveal to look,
     *  or to stop the tab: then the chat asks its own y/n question); it never acts by itself. */
    | { kind: "arrive"; intent?: "show" | "stop" }
    | { kind: "pages"; pages: PageLine[] }
    /** `shell`'s keys for the `?` overlay (`serialize_keymap_for_js`). */
    | ({ kind: "keymap" } & KeymapHelp)
    /** `send-prefix`/`send-keys` with this panel holding the keys: WebKitGTK cannot be handed the
     *  key itself, so the panel acts on the ones it knows (`C-a`: select all) and ignores the rest.
     *  `key` is tmux's spelling. See `serialize_literal_key_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "literal_key"; key: string }
    /** `prefix ?`: open the `?` overlay in BROWSE, with a plugin page's section first when that page held the
     *  keys. See `serialize_open_keymap_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "open_keymap"; opening?: number; plugin?: PluginKeysHelp }
    /** `prefix :` (tmux `command-prompt`): open the `:` command line, which runs
     *  nothing. See `serialize_open_command_line_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "open_command_line" }
    /** The chat was brought back to answer a card (its tray chip `agent ⚑N`, or `Ctrl+a a`): BROWSE,
     *  with the cursor on the oldest pending card. See `serialize_focus_permission_for_js` in
     *  `core/src/agent_bridge.rs`. */
    | { kind: "focus_permission"; tab: TabId }
    /** Global `f` HINT: shell asking panel to report visible targets and freeze the list. */
    | { kind: "hint_collect"; sessionId: number }
    /** shell showing the frozen targets their labels, ready to start typing. */
    | { kind: "hint_show"; sessionId: number; labels: string[] }
    /** shell narrowing the label set as the user types. */
    | { kind: "hint_prefix"; sessionId: number; typed: string }
    /** shell landing on a target by index into the frozen list. */
    | { kind: "hint_land"; sessionId: number; index: number }
    /** shell ending a global HINT: clear the labels and return to normal. */
    | { kind: "hint_end"; sessionId: number }
    | { kind: "error"; tab: TabId; message: string }
    /** Every tab and which is active. */
    | ({ kind: "tabs" } & TabsEnvelope)
    | { kind: "tab_detail"; tab: TabId; rows: DetailRow[] }
    | ({ kind: "chooser" } & ChooserEnvelope)
    /** Newer titles for records the last chooser listed, read after it was sent: laid over an open chooser's
     *  rows in place; with no chooser open, nothing happens (it never opens one). */
    | { kind: "chooser_titles"; titles: { providerSessionId: string; title: string }[] }
    | { kind: "confirm_close"; tab: TabId; lines: string[] }
    /** `<leader>bo` / `tab.close-others`: window-level, unlike `confirm_close`
     *  -- `tabs` is every tab the `y` answer would close, recomputed fresh by Rust rather than
     *  trusted from here. `serialize_confirm_close_others_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "confirm_close_others"; tabs: TabId[]; lines: string[] }
    | { kind: "begin_rename"; tab: TabId; current: string | null }
    /** A move into bypass needs a y/n first:
     *  `scope: "tab"` names the tab switching (`tab` is that id); `scope: "default"` is the window's
     *  own default (`cycle_default_mode` from the chooser or an empty tab), and carries `tab: null`
     *  -- there is no single tab to name. `lines[0]` is Rust's own wording, already carrying the
     *  waiting-card count; this side never counts them itself. `nonce` is echoed back verbatim on
     *  `y`/`Y` so Rust can tell a stale answer from the current prompt. A second envelope while
     *  one is already open REPLACES it (Reprompt) -- this is not additive.
     *  `serialize_confirm_bypass_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "confirm_bypass"; tab: TabId | null; scope: "tab" | "default"; nonce: number; lines: string[] }
    /** A restore found a tab saved in bypass and asks before giving it back: `y` restores as saved,
     *  `n` brings the bypass tabs back in auto. `nonce` is echoed back on either answer, as
     *  `confirm_bypass`'s is. `serialize_confirm_restore_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "confirm_restore"; nonce: number; lines: string[] }
    /** The question before a session loads this project's own configuration for the tab's agent, with what it
     *  would load. Tab-scoped. A second one for the same tab replaces the first. `serialize_trust_prompt_for_js`,
     *  `core/src/agent_bridge.rs`. */
    | TrustPromptEnvelope
    /** This tab's queue, its current refusal reason if the last flush was refused,
     *  and `note`: why its messages are held for the turn's end rather than in the turn. */
    | { kind: "queue"; tab: TabId; items: QueueItem[]; error: string | null; note?: string | null; enterSteers?: boolean | null }
    /** Whether this tab's `!` line is running: its composer behaves as in a running turn. */
    | { kind: "shell_running"; tab: TabId; running: boolean }
    /** Rust's mirror of the composer's own text, sent only on a switch, `ready`,
     *  a reset or a scratch-editor return -- never while the user is typing. `refused` names the send
     *  whose refusal this follows (a first message the trust question put off), which the page has put
     *  back into the box itself. `late` marks the last mirror of the page the tab was just moved out of,
     *  which reached Rust after this page was given the older copy: it replaces the box only while the box
     *  still holds that copy. */
    | { kind: "draft"; tab: TabId; text: string; refused?: string; late?: boolean }
    /** Reply to `take_back_queue`: the queue's texts, for the panel to merge into
     *  the box itself. */
    | { kind: "queue_taken"; tab: TabId; texts: string[] }
    /** The shared prompt history, newest last. */
    | { kind: "history"; entries: string[] }
    /** Per pending card, the "always allow" rule it would install, keyed by
     *  permission id; a card with no entry offers no third button. */
    | { kind: "rule_offers"; tab: TabId; offers: Record<string, string> }
    /** The editor-context line, window-scoped (one editor per window). */
    | ({ kind: "editor_context" } & ContextSummary)
    /** Companion mode only (never sent by the one-window mode): where the panel stands with the
     *  editor beside it. Window-scoped. `serialize_editor_link_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "editor_link"; state: EditorLinkState; text: string }
    /** A question's typed answer as it came back from nvim (Ctrl+G in the question dock). */
    | { kind: "question_other_edited"; tab: TabId; interaction_id: string; question_index: number; text: string }
    /** Whether the scratch-editor round trip currently has this tab's draft open in nvim. */
    | { kind: "scratch"; tab: TabId; editing: boolean }
    /** The footer's transient line, from Rust -- e.g. a `gf` refusal. */
    | { kind: "notice"; text: string }
    /** `install_module_nav` claimed a bare `Ctrl+j`/`Ctrl+k` against the mirror this page
     *  last posted. Window-level, like `enter_input`/`arrive` -- there is no tab to name, since it
     *  is about which of BROWSE/INPUT has the keys, not about a tab's own state. */
    | { kind: "nav_key"; direction: NavKeyDirection }
    /** A move to the other window is about to take `tab`: the page answers with its view of it (`view_exported`).
     *  `serialize_view_export_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "view_export"; request: number; tab: TabId }
    /** The other window's page's view of `tab`, which moved here: kept for the tab as a switch parks one. That
     *  page's input, read by `parseView`. `serialize_view_import_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "view_import"; tab: TabId; view: unknown }
    /** The reply to `review_request`: the turns, the files and the notes (`serialize_review_for_js`,
     *  `core/src/agent_bridge.rs`). Tab-scoped; matched to the overlay by `requestId`. */
    | ({ kind: "review" } & ReviewEnvelope)
    /** The reply to `review_diff_request`: one file's hunks, or `hunks: null` when over the cap. */
    | ({ kind: "review_diff" } & ReviewDiffEnvelope)
    /** A finished turn changed `files` files: the band says so until the overlay is opened on that turn
     *  or a new turn starts. `files: 0` clears it. Names its tab and is kept for it even while another
     *  tab is on screen. */
    | ({ kind: "review_hint" } & ReviewHintEnvelope)
    /** The tab's draft, as the answer to a comment add/remove (`requestId` set) or pushed after a change the
     *  panel did not ask for (`requestId: null`). Tab-scoped. */
    | ({ kind: "review_draft" } & ReviewDraftEnvelope)
    /** The reply to `review_send` with no confirmation. Tab-scoped. */
    | ({ kind: "review_send_preview" } & ReviewSendPreviewEnvelope)
    /** The interrupted reverts the journal holds. Window-level: the journal is the project's. */
    | ({ kind: "review_recovery" } & ReviewRecoveryEnvelope),
) => void;

/** Whether `cards` is a list of broker cards the card view can draw. */
function isWellFormedEditorCards(cards: unknown): boolean {
  return Array.isArray(cards) && cards.every(isEditorCardWire);
}

/** The handler's own payload type, exported so callers (`tabs.ts`'s `acceptsEnvelope`, `App.tsx`)
 *  can name it without re-declaring the union. */
export type InboundPayload = Parameters<InboundHandler>[0];

function isWellFormedTrustPrompt(value: unknown): boolean {
  const o = value as Partial<Record<keyof TrustPromptEnvelope, unknown>>;
  const changed = o.changed as { texts?: unknown } | null | undefined;
  const texts = changed?.texts;
  const count = (value: unknown) => typeof value === "number" && Number.isInteger(value) && value >= 0;
  const hunk = (value: unknown) => {
    if (value === null || typeof value !== "object") return false;
    const h = value as Record<string, unknown>;
    return count(h.skipped) && Array.isArray(h.lines) && h.lines.every((value: unknown) => {
      if (value === null || typeof value !== "object") return false;
      const line = value as Record<string, unknown>;
      return (line.kind === "context" || line.kind === "added" || line.kind === "removed") &&
        typeof line.text === "string" && (!("noNewline" in line) || line.noNewline === true);
    });
  };
  return (
    typeof o.tab === "number" &&
    typeof o.nonce === "number" &&
    typeof o.agent === "string" &&
    o.agent !== "" &&
    isHex64(o.fingerprint) &&
    isHex64(o.findingsDigest) &&
    isTrustRemember(o.remember) &&
    typeof o.root === "string" &&
    typeof o.top === "string" &&
    Array.isArray(o.items) &&
    (texts === undefined || (Array.isArray(texts) && texts.every((entry: unknown) => {
      if (entry === null || typeof entry !== "object") return false;
      const text = entry as Record<string, unknown>;
      return typeof text.path === "string" && (
        (text.kind === "diff" && count(text.added) && count(text.removed) && count(text.skippedAfter) &&
          Array.isArray(text.hunks) && text.hunks.every(hunk)) ||
        (text.kind === "plain" && typeof text.line === "string")
      );
    })))
  );
}

/** What a phone may do, as the desk grants it. */
export type RemoteGrant = "watch" | "watch+steer";

/** The route a code made from the device list sends phones by. */
export type RemoteRecipeChoice = "auto" | "ssh" | "direct";

/** One audit record of a device, as the device list draws it: every field of its own as `[name, text]`, plain text
 *  drawn through `revealHidden`. */
export type RemoteDeviceRecord = { time: string; event: string; writer: string; fields: [string, string][] };

/** One enrolled device, as the device list shows it (`panel/src/remote_enroll.rs`'s `show_devices`): the store's
 *  entry, whether it is connected now, the host its notifications go to, and its newest audit records first. */
export type RemoteDeviceRow = {
  id: string;
  name: string;
  grant: RemoteGrant;
  recipe: "ssh" | "direct" | "unknown";
  enrolled_at: number | null;
  last_seen: number | null;
  last_host: string | null;
  connected: boolean;
  push_host: string | null;
  records: RemoteDeviceRecord[];
};

/** One phone connected to this desk now (`panel/src/remote_socket/devices.rs`): its name as the store holds it, the
 *  grant it has, the `Host` its request named and when it connected (seconds since the Unix epoch). The name and
 *  host are plain text, drawn through `revealHidden`. */
export type RemoteConnectedDevice = { device: string; name: string; grant: RemoteGrant; host: string; since: number };

/** What the window's remote-control questions are drawn from (`panel/src/remote_enroll.rs`), and the answers to
 *  them. Names and addresses are plain text, drawn through `revealHidden`. */
export type RemoteDeskEnvelope =
  /** A one-time code waiting to be scanned: the code itself (typed instead), the pairing URI and its QR modules,
   *  one string of `0`/`1` per row. */
  | { kind: "remote_pairing"; code: string; uri: string; modules: string[]; expiresInMs: number }
  /** Each question says how long the window waits for it (`expiresInMs`); past that it is no. */
  | { kind: "remote_enroll"; nonce: number; name: string; host: string; peer: string; recipe: "ssh" | "direct"; target: string; grants: RemoteGrant[]; grant: RemoteGrant; expiresInMs: number }
  | { kind: "remote_listen"; nonce: number; addr: string; iface: string; link: string; plain: boolean; expiresInMs: number }
  /** `eitri remote grant` asks whether a device's grant may change from `from` to `to`; answered with
   *  `remote_enroll_answer` under its nonce, a `yes` naming `to`. */
  | { kind: "remote_grant"; nonce: number; device: string; name: string; from: RemoteGrant; to: RemoteGrant; expiresInMs: number }
  /** The window stopped waiting for the question `nonce` (it went unanswered past its time). */
  | { kind: "remote_question_closed"; nonce: number }
  /** A device in the store that no desk said yes to (something other than an enrollment wrote it). */
  | { kind: "remote_device_flagged"; device: string; name: string }
  /** Who is connected now, whole: it replaces the last list, and an empty one takes the band's phone mark away. */
  | { kind: "remote_connected"; devices: RemoteConnectedDevice[]; sandbox?: string | null }
  /** Remote control is on in this window: the device list's key is offered. */
  | { kind: "remote_on" }
  /** The device list, whole, read for the base page. */
  | { kind: "remote_devices"; devices: RemoteDeviceRow[] }
  /** The window's y/n before a device is revoked from the list; a `y` is `remote_revoke` under its nonce. */
  | { kind: "remote_revoke_ask"; nonce: number; device: string; name: string; expiresInMs: number }
  | { kind: "command_result"; requestId: string; ok: true }
  | { kind: "command_result"; requestId: string; ok: false; error: string };

let remoteDesk: ((payload: RemoteDeskEnvelope) => void) | null = null;
const remoteRequests = new Set<string>();

/** Where the remote-control questions and the results of their answers go (a `RemoteDesk`, `remoteDesk.ts`);
 *  `null` stops it. */
export function installRemoteDesk(handler: ((payload: RemoteDeskEnvelope) => void) | null): void {
  remoteDesk = handler;
}

/** Sends an answer to a remote-control question, or a request of the device list; its `command_result` goes to the
 *  remote desk, not the panel. */
export function postRemoteAnswer(
  message: Extract<
    OutboundMessage,
    { type: "remote_enroll_answer" | "remote_listen_answer" | "remote_devices_open" | "remote_start_enroll" | "remote_revoke_ask" | "remote_revoke" }
  >,
): void {
  remoteRequests.add(message.request_id);
  postToRust(message);
}

function isGrant(value: unknown): value is RemoteGrant {
  return value === "watch" || value === "watch+steer";
}

function isWellFormedDeviceRow(value: unknown): boolean {
  const d = value as Record<string, unknown> | null;
  const text = (v: unknown) => typeof v === "string";
  const time = (v: unknown) => v === null || (Number.isSafeInteger(v) && (v as number) >= 0);
  return d !== null && typeof d === "object" && text(d.id) && text(d.name) && isGrant(d.grant) &&
    (d.recipe === "ssh" || d.recipe === "direct" || d.recipe === "unknown") && time(d.enrolled_at) &&
    time(d.last_seen) && (d.last_host === null || text(d.last_host)) && typeof d.connected === "boolean" &&
    (d.push_host === null || text(d.push_host)) && Array.isArray(d.records) && d.records.every((r: unknown) => {
      const record = r as Record<string, unknown> | null;
      return record !== null && typeof record === "object" && text(record.time) && text(record.event) &&
        text(record.writer) && Array.isArray(record.fields) &&
        record.fields.every((f: unknown) => Array.isArray(f) && f.length === 2 && text(f[0]) && text(f[1]));
    });
}

function isWellFormedRemote(value: Record<string, unknown>): boolean {
  const text = (v: unknown) => typeof v === "string";
  const nonce = Number.isSafeInteger(value.nonce) && (value.nonce as number) >= 0;
  const expires = typeof value.expiresInMs === "number" && value.expiresInMs > 0;
  switch (value.kind) {
    case "remote_pairing":
      return text(value.code) && text(value.uri) && Array.isArray(value.modules) &&
        value.modules.every((row) => typeof row === "string" && /^[01]*$/.test(row) && row.length === (value.modules as unknown[]).length) &&
        typeof value.expiresInMs === "number";
    case "remote_enroll":
      return nonce && expires && text(value.name) && text(value.host) && text(value.peer) &&
        (value.recipe === "ssh" || value.recipe === "direct") && text(value.target) && Array.isArray(value.grants) &&
        value.grants.length > 0 && value.grants.every(isGrant) && isGrant(value.grant);
    case "remote_listen":
      return nonce && expires && text(value.addr) && text(value.iface) && text(value.link) && typeof value.plain === "boolean";
    case "remote_grant":
      return nonce && expires && text(value.device) && text(value.name) && isGrant(value.from) && isGrant(value.to);
    case "remote_question_closed":
      return nonce;
    case "remote_device_flagged":
      return text(value.device) && text(value.name);
    case "remote_connected":
      return (value.sandbox == null || typeof value.sandbox === "string") && Array.isArray(value.devices) && value.devices.every((d: unknown) => {
        const device = d as Record<string, unknown> | null;
        return device !== null && typeof device === "object" && text(device.device) && text(device.name) && isGrant(device.grant) &&
          text(device.host) && Number.isSafeInteger(device.since) && (device.since as number) >= 0;
      });
    case "remote_on":
      return true;
    case "remote_devices":
      return Array.isArray(value.devices) && value.devices.every(isWellFormedDeviceRow);
    case "remote_revoke_ask":
      return nonce && expires && text(value.device) && text(value.name);
    default:
      return false;
  }
}

export function installDispatch(handler: InboundHandler): void {
  activeTransport().listen((json: string) => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(json);
    } catch (e) {
      console.warn("agent-ui: __eitriDispatch received invalid JSON from Rust", json, e);
      return;
    }
    if (parsed && typeof parsed === "object" && "kind" in parsed) {
      const obj = parsed as { kind: string };
      // The remote-control questions are drawn by their own component; one that cannot be answered as shown is
      // dropped like any malformed question.
      if (
        obj.kind === "remote_pairing" ||
        obj.kind === "remote_enroll" ||
        obj.kind === "remote_listen" ||
        obj.kind === "remote_grant" ||
        obj.kind === "remote_question_closed" ||
        obj.kind === "remote_device_flagged" ||
        obj.kind === "remote_connected" ||
        obj.kind === "remote_on" ||
        obj.kind === "remote_devices" ||
        obj.kind === "remote_revoke_ask"
      ) {
        if (!isWellFormedRemote(parsed as Record<string, unknown>)) {
          console.warn("agent-ui: __eitriDispatch received a malformed remote-control envelope", parsed);
          return;
        }
        remoteDesk?.(parsed as RemoteDeskEnvelope);
        return;
      }
      if (obj.kind === "command_result") {
        const requestId = (parsed as { requestId?: unknown }).requestId;
        if (typeof requestId === "string" && remoteRequests.delete(requestId)) {
          remoteDesk?.(parsed as RemoteDeskEnvelope);
          return;
        }
      }
      // A `confirm_bypass` with no numeric `nonce` can never be answered correctly (there
      // would be nothing real to echo back on `y`), so it is rejected the same way an unrecognized
      // envelope is -- warned about and dropped -- rather than reaching `App.tsx` as a confirm this
      // side would have to guess a nonce for.
      if (obj.kind === "confirm_bypass" && typeof (obj as { nonce?: unknown }).nonce !== "number") {
        console.warn("agent-ui: __eitriDispatch received a malformed confirm_bypass envelope (no nonce)", parsed);
        return;
      }
      // The same rule for the restore question: an answer can only echo a real nonce.
      if (obj.kind === "confirm_restore" && typeof (obj as { nonce?: unknown }).nonce !== "number") {
        console.warn("agent-ui: __eitriDispatch received a malformed confirm_restore envelope (no nonce)", parsed);
        return;
      }
      // The trust question can only be answered by echoing what it showed, so one that cannot be echoed is
      // dropped here rather than drawn: a numeric nonce, both 64-hex digests, a known `remember` and the list
      // it draws.
      if (obj.kind === "trust_prompt" && !isWellFormedTrustPrompt(parsed)) {
        console.warn("agent-ui: __eitriDispatch received a malformed trust_prompt envelope", parsed);
        return;
      }
      // A card that cannot be drawn whole cannot be answered knowingly, so the list is dropped rather than drawn.
      if (obj.kind === "editor_cards" && !isWellFormedEditorCards((obj as { cards?: unknown }).cards)) {
        console.warn("agent-ui: __eitriDispatch received a malformed editor_cards envelope", parsed);
        return;
      }
      if (obj.kind === "path_choices") {
        const value = parsed as Record<string, unknown>;
        if (typeof value.requestId !== "string" || !Number.isSafeInteger(value.tab) || !Array.isArray(value.paths) ||
            (value.stopped !== undefined && value.stopped !== null && typeof value.stopped !== "string") ||
            !value.paths.every((item) => item !== null && typeof item === "object" && typeof item.path === "string" &&
              (item.line === null || (Number.isSafeInteger(item.line) && item.line >= 0)))) {
          console.warn("agent-ui: malformed path_choices envelope", parsed);
          return;
        }
      }
      if (obj.kind === "path_completions") {
        const value = parsed as Record<string, unknown>;
        if (
          !Number.isSafeInteger(value.tab) || !Number.isSafeInteger(value.seq) ||
          (value.seq as number) < 0 || !Array.isArray(value.items) ||
          !value.items.every((item) => typeof item === "string")
        ) {
          console.warn("agent-ui: malformed path_completions envelope", parsed);
          return;
        }
      }
      if (
        obj.kind === "path_choices" ||
        obj.kind === "path_completions" ||
        obj.kind === "hello" ||
        obj.kind === "command_result" ||
        obj.kind === "task_output_result" ||
        obj.kind === "models_result" ||
        obj.kind === "settings_result" ||
        obj.kind === "question_other_edited" ||
        obj.kind === "events" ||
        obj.kind === "snapshot" ||
        obj.kind === "editor_cards" ||
        obj.kind === "handoff" ||
        obj.kind === "theme" ||
        obj.kind === "display" ||
        obj.kind === "root_note" ||
        obj.kind === "review_settings" ||
        obj.kind === "pane_focus" ||
        obj.kind === "editor_typing" ||
        obj.kind === "enter_input" ||
        obj.kind === "arrive" ||
        obj.kind === "pages" ||
        obj.kind === "keymap" ||
        obj.kind === "literal_key" ||
        obj.kind === "open_keymap" ||
        obj.kind === "open_command_line" ||
        obj.kind === "focus_permission" ||
        obj.kind === "hint_collect" ||
        obj.kind === "hint_show" ||
        obj.kind === "hint_prefix" ||
        obj.kind === "hint_land" ||
        obj.kind === "hint_end" ||
        obj.kind === "error" ||
        obj.kind === "tabs" ||
        obj.kind === "tab_detail" ||
        obj.kind === "chooser" ||
        obj.kind === "chooser_titles" ||
        obj.kind === "confirm_close" ||
        obj.kind === "confirm_close_others" ||
        obj.kind === "confirm_bypass" ||
        obj.kind === "confirm_restore" ||
        obj.kind === "trust_prompt" ||
        obj.kind === "begin_rename" ||
        obj.kind === "queue" ||
        obj.kind === "shell_running" ||
        obj.kind === "draft" ||
        obj.kind === "queue_taken" ||
        obj.kind === "history" ||
        obj.kind === "rule_offers" ||
        obj.kind === "editor_context" ||
        obj.kind === "editor_link" ||
        obj.kind === "scratch" ||
        obj.kind === "notice" ||
        obj.kind === "nav_key" ||
        obj.kind === "view_export" ||
        obj.kind === "view_import" ||
        obj.kind === "review" ||
        obj.kind === "review_diff" ||
        obj.kind === "review_hint" ||
        obj.kind === "review_draft" ||
        obj.kind === "review_send_preview" ||
        obj.kind === "review_recovery"
      ) {
        handler(parsed as Parameters<InboundHandler>[0]);
        return;
      }
    }
    console.warn("agent-ui: __eitriDispatch received an unrecognized envelope shape", parsed);
  });
}
