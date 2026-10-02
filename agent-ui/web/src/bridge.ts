import type { AgentDomainEvent, AgentUiSnapshot, ChooserEnvelope, ContextSummary, DetailRow, EditorLinkState, HandoffCommand, Hello, QueueItem, ReviewDiffEnvelope, ReviewEnvelope, ReviewHintEnvelope, ReviewScope, TabId, TabsEnvelope } from "./types";
import type { KeymapHelp, PaneDirection } from "./keymap";

export type OutboundMessage =
  | { type: "ready"; request_id: string }
  | { type: "send_message"; request_id: string; tab: TabId; text: string }
  | { type: "interrupt"; request_id: string; tab: TabId }
  /** `decision` is a closed set, not a boolean: Rust rejects an unrecognized value at parse time
   *  rather than defaulting it, and the tempting default would be the one that runs the tool.
   *  `reason` is only ever sent with a denial -- there is no field downstream that would show the
   *  model an approval's reason. */
  /** `remember: true` on an allow is "always allow" (phase 3 ruling 16) -- Rust honours it only for
   *  a permission id it itself offered a rule for; the panel's own text is never trusted. */
  | { type: "permission_response"; request_id: string; tab: TabId; permission_id: string; decision: PermissionDecision; reason?: string; remember?: boolean }
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
   *  started (spec §6.3, ruling R9): window-level, like `TabVerb` -- there is no tab to name, since
   *  a resume from here would open a fresh one. Cycles `TabSet::default_mode` and re-sends `tabs`. */
  | { type: "cycle_default_mode"; request_id: string }
  | { type: "open_detail"; request_id: string; tab: TabId }
  /** Global `f` HINT: panel has pressed `f` in BROWSE, asking shell to start a global HINT. */
  | { type: "hint_request"; request_id: string }
  /** Answer to hint_collect: the number of visible targets the panel froze for this sessionId. */
  | { type: "hint_targets"; request_id: string; session_id: number; count: number }
  /** Queue this text behind the tab's running turn (phase 3 ruling 3, 5). Rust answers with a
   *  `command_result`; a refusal never touches the box (the panel already sent it). */
  | { type: "queue_message"; request_id: string; tab: TabId; text: string }
  /** `↑` (phase 3 ruling 7): empty the tab's queue and hand its texts back as `queue_taken`. */
  | { type: "take_back_queue"; request_id: string; tab: TabId }
  /** `Ctrl+Enter` (phase 3 ruling 8): queue this text (if non-empty), then interrupt if a turn is
   *  running, or flush at once if idle. */
  | { type: "send_now"; request_id: string; tab: TabId; text: string }
  /** The box's text, mirrored (phase 3 ruling 6). Rust answers nothing. */
  | { type: "draft"; request_id: string; tab: TabId; text: string }
  /** The scratch-editor round trip's draft half (spec §4.2, plan ruling 18): the box's text handed
   *  to the nvim scratch buffer for editing with `Ctrl+g`. */
  | { type: "edit_draft"; request_id: string; tab: TabId; text: string }
  /** Append this typed text to the shared prompt history (phase 3 ruling 11), e.g. `Ctrl+c`'s clear. */
  | { type: "history_push"; request_id: string; text: string }
  /** `gf` on a path (phase 3 ruling 18, 19): open it in the scratch split, at `line` when given. */
  | { type: "open_path"; request_id: string; path: string; line?: number }
  /** `Ctrl+g` on a row (phase 3 ruling 18): show this text read-only in the scratch split. */
  | { type: "view_in_editor"; request_id: string; title: string; text: string }
  /** A tab.* panel-table action (`runPanelAction`, panel round 2 plan Task 8): the same verbs the
   *  prefix's own tmux window keys run. Window-level, like the prefix's own `tab_verb` -- whichever
   *  tab is under the keys decides, so this carries no `tab` of its own (Rust's `InboundMessage::
   *  TabVerb` has none either); sent through `postToRust` directly rather than `post()`, which would
   *  add one. `TabVerbWire`'s wire spelling, `core/src/agent_bridge.rs`. */
  | { type: "tab_verb"; request_id: string; verb: "next" | "prev" | "last" | "new" | "close" | "close_others" | "choose" | "info" }
  /** `y` to a `confirm_close_others` prompt (Owner answers Q2): window-level, like `TabVerb` --
   *  Rust recomputes "every tab but the active one" fresh rather than trusting the set the prompt
   *  was shown with, so this carries no tab list of its own. `InboundMessage::CloseOthers`,
   *  `core/src/agent_bridge.rs`. */
  | { type: "close_others"; request_id: string }
  /** `y`/`Y` to a `confirm_bypass` prompt (v1 spec `2026-09-27-v1-mode-design.md`, D11), guarded by
   *  `modeKey.ts#bypassYesCounts` before this is ever sent. `tab`/`scope`/`nonce` echo the envelope's
   *  own values verbatim -- this side never counts cards or decides which ones a `y` approves; Rust
   *  keeps that list and answers only the delivered cards still pending, still on `tab`, at `nonce`
   *  (D7). `InboundMessage::ConfirmBypass`, `core/src/agent_bridge.rs`. */
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
  /** V1 §3.5's composer mirror: the effective mode this window is in, whenever it changes (and once
   *  after `ready`). Window-level, like `TabVerb` -- Rust keeps one value per window, not per tab,
   *  because the capture controller (`install_module_nav`) that reads it back is itself installed
   *  once per window. `"other"` covers everything that is neither a live BROWSE nor a live INPUT:
   *  the empty tab's own menu/composer count as `"browse"`/`"input"` too (`core::agent_bridge::
   *  PanelKeys`). */
  | { type: "panel_keys"; request_id: string; mode: PanelKeysMode }
  /** V1 §3.5's "stale mirror, both ways": a `nav_key` this page could not apply -- the mirror was a
   *  keystroke stale, an overlay owns the keys, or the session it named has ended -- comes back
   *  here so Rust runs the chord's ordinary `move_focus` instead of silently dropping the key
   *  (`core::agent_bridge::NavKeyDirection`, `serialize_nav_key_for_js`). */
  | { type: "nav_fallthrough"; request_id: string; direction: NavKeyDirection }
  /** `Ctrl+w h/j/k/l` in BROWSE (v1 picks, Task 6, ruling R11; vim's `CTRL-W h/j/k/l`): move the keys from
   *  the panel to the module on that side, by geometry -- exactly what `Ctrl+h/j/k/l` do from it (shell
   *  runs `move_focus(agent, direction)`, the same hook a `nav_fallthrough` uses). Window-level, like
   *  `nav_fallthrough`; a side with no module leaves the keys where they are. Rust answers with a
   *  `command_result`, an error only if no hook is installed. `InboundMessage::PaneNav`,
   *  `core/src/agent_bridge.rs`. */
  | { type: "pane_nav"; request_id: string; direction: PaneDirection }
  /** `gx` on a web link (v1 picks, Task 8, ruling R6): open this address in the system browser. `url` is
   *  the WHATWG-normalized `href` (`nav.ts#webUrl`) -- what the pick showed -- never the spelling the reply
   *  wrote. Window-level, like `open_path`; Rust re-checks it (`agent_panel.rs#web_url`, http(s) and a plain
   *  host only) and answers with a `command_result`, an error when it is not a web link.
   *  `InboundMessage::OpenUrl`, `core/src/agent_bridge.rs`. */
  | { type: "open_url"; request_id: string; url: string }
  /** `c` in BROWSE, and `[`/`]`/`S` inside the review overlay: the files that changed on disk during this
   *  tab's turn (`"latest"`, or a turn number) or, with `scope: "session"`, since the session's first
   *  baseline. Names the tab, as every tab-scoped command does; the reply is a `review` envelope carrying the
   *  same `request_id`, or a `command_result` failure with the reason (no session, no such turn, git
   *  unavailable). `InboundMessage::ReviewRequest`, `core/src/agent_bridge.rs`. */
  | { type: "review_request"; request_id: string; tab: TabId; turn: "latest" | number; scope: ReviewScope }
  /** `Enter` on a file of the review overlay: that file's hunks, under the overlay's current scope. The
   *  reply is a `review_diff` envelope with the same `request_id`, or a `command_result` failure.
   *  `InboundMessage::ReviewDiffRequest`, `core/src/agent_bridge.rs`. */
  | { type: "review_diff_request"; request_id: string; tab: TabId; turn: number; scope: ReviewScope; path: string };

/** The composer mirror's three states (spec §3.5). Distinct from `./keymap`'s `PanelMode`, which is
 *  this component's OWN mode ("hint" included, unreachable yet) -- this type is the wire value Rust
 *  reads to decide whether a bare `Ctrl+j`/`Ctrl+k` is claimed at all, and "other" has no `PanelMode`
 *  counterpart (an overlay, a dead session, ... are all folded into it). */
export type PanelKeysMode = "browse" | "input" | "other";

/** Which way a claimed `Ctrl+j`/`Ctrl+k` moved (spec §3.5): `down` is `Ctrl+j`, `up` is `Ctrl+k`. */
export type NavKeyDirection = "down" | "up";

/** Every decision a backend can actually carry. Verdandi's wire is `bool allow` + `string reason`
 *  and the legacy hook relay is the same shape, so there is no allow-for-session anywhere to send
 *  one to -- adding a button for it here would produce a control that silently degrades to a plain
 *  allow. Widening this is a Verdandi protocol change first.
 *
 *  "Always allow" is not a third decision: it is `remember: true` on an `"allow"`, which Rust
 *  honours only for a permission id it itself offered a rule for (phase 3 ruling 16). */
export type PermissionDecision = "allow" | "deny";

declare global {
  interface Window {
    webkit?: { messageHandlers?: { eitriAgent?: { postMessage: (msg: string) => void } } };
    __eitriDispatch?: (json: string) => void;
  }
}

let requestCounter = 0;
/** A per-page-load monotonic counter is enough uniqueness here -- this bridge only ever talks to
 * the one Rust host process behind this exact WebView instance, never a shared or multi-client
 * server, so there is no cross-client collision risk a random UUID would guard against. */
export function nextRequestId(): string {
  requestCounter += 1;
  return `req-${requestCounter}`;
}

export function postToRust(message: OutboundMessage): void {
  const handler = window.webkit?.messageHandlers?.eitriAgent;
  if (!handler) {
    console.warn("agent-ui: no WebKitGTK message handler present, message dropped", message);
    return;
  }
  handler.postMessage(JSON.stringify(message));
}

type InboundHandler = (
  payload:
    | { kind: "hello" } & Hello
    | { kind: "command_result"; requestId: string; ok: true }
    | { kind: "command_result"; requestId: string; ok: false; error: string }
    | {
        kind: "events";
        tab: TabId;
        fromRevision: number;
        throughRevision: number;
        events: AgentDomainEvent[];
        /** v1 polish F18: calls in this batch a saved prefix rule answered; absent when none did. */
        ruleNotes?: { toolUseId: string; rule: string }[];
        /** v1 polish F22: `Write` cards in this batch raised over no file; absent when none were. */
        createsFile?: { permissionId: string; toolUseId: string | null }[];
        /** O3 review item 7: calls in this batch whose CLI prompt was answered without a card. */
        promptNotes?: { toolUseId: string; note: string }[];
        /** v1 trial item 7: tool-use ids of a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast
         *  path answered with no card; absent when none were. */
        autoNotes?: string[];
        /** Fix round finding 1: tool-use ids of a `Write` answered with no card -- by the
         *  acceptEdits fast path, or in bypass (whole-branch review finding 6) -- over a path where
         *  nothing existed just before the answer -- `createsFile`'s own signal, but for a call that
         *  never raised a card to carry it on. */
        autoCreatesFile?: string[];
      }
    /** `turnStartedAtMs`: when the running turn started, `Date.now()`'s clock, kept per tab by
     *  Rust (`tab_set`'s `turn_clock`) so a switch or a reload shows its real elapsed time; `null`
     *  (or absent, from an older build) when none runs. */
    | { kind: "snapshot"; tab: TabId; throughRevision: number; state: AgentUiSnapshot; turnStartedAtMs?: number | null }
    | ({ kind: "handoff"; tab: TabId } & HandoffCommand)
    | { kind: "theme"; vars: Record<string, string> }
    /** Whether this panel's pane holds the window's keyboard focus. `shell` decides this from
     *  GTK's focus widget (`shell/src/pane_focus.rs`). It is not decided from this page's own
     *  `window` focus/blur: `shell` arbitrates Ctrl+h/Ctrl+l, and the same answer drives the
     *  status bar and the pane outline, so the three agree. */
    | { kind: "pane_focus"; focused: boolean }
    /** Whether the user is typing in the editor, and the gap between the panel's stream pushes
     *  while they do (`typingCadence.ts`; `serialize_editor_typing_for_js`, `core/src/agent_bridge.rs`).
     *  Window-level, and Rust's word is the only one: nothing in the page clears it. */
    | { kind: "editor_typing"; typing: boolean; periodMs: number }
    /** A brand-new tab's own arrival: open the composer with the caret in it. This is `enter_input`'s
     *  only sender now (panel round 2, spec §8, decision 4) -- every other keyboard arrival that used
     *  to send this now sends `arrive` instead. A click on a row still lands in BROWSE on that row.
     *  See `serialize_enter_input_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "enter_input" }
    /** Every OTHER keyboard arrival (`Ctrl+h/j/k/l` into the chat, `prefix a`/a tray chip with no
     *  card, a launch that starts with the keys already in the chat): BROWSE, on the oldest pending
     *  card if one waits (P1, unchanged), else on the last row with following resumed. Reverses the
     *  2026-09-19 ruling "control l直接闪cursor" (panel round 2 spec §8, decision 4). See
     *  `serialize_arrive_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "arrive" }
    /** `shell`'s keys for the `?` overlay (`serialize_keymap_for_js`). */
    | ({ kind: "keymap" } & KeymapHelp)
    /** `send-prefix`/`send-keys` with this panel holding the keys: WebKitGTK cannot be handed the
     *  key itself, so the panel acts on the ones it knows (`C-a`: select all) and ignores the rest.
     *  `key` is tmux's spelling. See `serialize_literal_key_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "literal_key"; key: string }
    /** `prefix ?`: open the `?` overlay in BROWSE. */
    | { kind: "open_keymap" }
    /** `prefix :` (tmux `command-prompt`, owner decision #28, K16): open the `:` command line, which runs
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
    /** Every tab and which is active (session tabs spec §3.1). */
    | ({ kind: "tabs" } & TabsEnvelope)
    | { kind: "tab_detail"; tab: TabId; rows: DetailRow[] }
    | ({ kind: "chooser" } & ChooserEnvelope)
    | { kind: "confirm_close"; tab: TabId; lines: string[] }
    /** `<leader>bo` / `tab.close-others` (Owner answers Q2): window-level, unlike `confirm_close`
     *  -- `tabs` is every tab the `y` answer would close, recomputed fresh by Rust rather than
     *  trusted from here. `serialize_confirm_close_others_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "confirm_close_others"; tabs: TabId[]; lines: string[] }
    | { kind: "begin_rename"; tab: TabId; current: string | null }
    /** A move into bypass needs a y/n first (v1 spec `2026-09-27-v1-mode-design.md`, D2/D11):
     *  `scope: "tab"` names the tab switching (`tab` is that id); `scope: "default"` is the window's
     *  own default (`cycle_default_mode` from the chooser or an empty tab), and carries `tab: null`
     *  -- there is no single tab to name. `lines[0]` is R06's own wording, already carrying the
     *  waiting-card count; this side never counts them itself. `nonce` is echoed back verbatim on
     *  `y`/`Y` so Rust can tell a stale answer from the current prompt (D7). A second envelope while
     *  one is already open REPLACES it (Reprompt) -- this is not additive.
     *  `serialize_confirm_bypass_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "confirm_bypass"; tab: TabId | null; scope: "tab" | "default"; nonce: number; lines: string[] }
    /** A restore found a tab saved in bypass and asks before giving it back: `y` restores as saved,
     *  `n` brings the bypass tabs back in auto. `nonce` is echoed back on either answer, as
     *  `confirm_bypass`'s is. `serialize_confirm_restore_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "confirm_restore"; nonce: number; lines: string[] }
    /** This tab's queue, and its current refusal reason if the last flush was refused (phase 3
     *  ruling 3). */
    | { kind: "queue"; tab: TabId; items: QueueItem[]; error: string | null }
    /** Rust's mirror of the composer's own text (phase 3 ruling 6), sent only on a switch, `ready`,
     *  a reset or a scratch-editor return -- never while the user is typing. */
    | { kind: "draft"; tab: TabId; text: string }
    /** Reply to `take_back_queue` (phase 3 ruling 7): the queue's texts, for the panel to merge into
     *  the box itself. */
    | { kind: "queue_taken"; tab: TabId; texts: string[] }
    /** The shared prompt history (phase 3 ruling 11, 12), newest last. */
    | { kind: "history"; entries: string[] }
    /** Per pending card, the "always allow" rule it would install (phase 3 ruling 16), keyed by
     *  permission id; a card with no entry offers no third button. */
    | { kind: "rule_offers"; tab: TabId; offers: Record<string, string> }
    /** V1's editor-context line (phase 3 ruling 32), window-scoped. */
    | ({ kind: "editor_context" } & ContextSummary)
    /** Companion mode only (never sent by the one-window mode): where the panel stands with the
     *  editor beside it. Window-scoped. `serialize_editor_link_for_js`, `core/src/agent_bridge.rs`. */
    | { kind: "editor_link"; state: EditorLinkState; text: string }
    /** Whether the scratch-editor round trip currently has this tab's draft open in nvim (plan
     *  ruling 18). */
    | { kind: "scratch"; tab: TabId; editing: boolean }
    /** The footer's transient line (phase 3 ruling 29), from Rust -- e.g. a `gf` refusal. */
    | { kind: "notice"; text: string }
    /** V1 §3.5: `install_module_nav` claimed a bare `Ctrl+j`/`Ctrl+k` against the mirror this page
     *  last posted. Window-level, like `enter_input`/`arrive` -- there is no tab to name, since it
     *  is about which of BROWSE/INPUT has the keys, not about a tab's own state. */
    | { kind: "nav_key"; direction: NavKeyDirection }
    /** The reply to `review_request`: the turns, the files and the notes (`serialize_review_for_js`,
     *  `core/src/agent_bridge.rs`). Tab-scoped; matched to the overlay by `requestId`. */
    | ({ kind: "review" } & ReviewEnvelope)
    /** The reply to `review_diff_request`: one file's hunks, or `hunks: null` when over the cap. */
    | ({ kind: "review_diff" } & ReviewDiffEnvelope)
    /** A finished turn changed `files` files: the band says so until the overlay is opened on that turn
     *  or a new turn starts. `files: 0` clears it. Names its tab and is kept for it even while another
     *  tab is on screen. */
    | ({ kind: "review_hint" } & ReviewHintEnvelope),
) => void;

/** The handler's own payload type, exported so callers (`tabs.ts`'s `acceptsEnvelope`, `App.tsx`)
 *  can name it without re-declaring the union. */
export type InboundPayload = Parameters<InboundHandler>[0];

export function installDispatch(handler: InboundHandler): void {
  window.__eitriDispatch = (json: string) => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(json);
    } catch (e) {
      console.warn("agent-ui: __eitriDispatch received invalid JSON from Rust", json, e);
      return;
    }
    if (parsed && typeof parsed === "object" && "kind" in parsed) {
      const obj = parsed as { kind: string };
      // D11/D7: a `confirm_bypass` with no numeric `nonce` can never be answered correctly (there
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
      if (
        obj.kind === "hello" ||
        obj.kind === "command_result" ||
        obj.kind === "events" ||
        obj.kind === "snapshot" ||
        obj.kind === "handoff" ||
        obj.kind === "theme" ||
        obj.kind === "pane_focus" ||
        obj.kind === "editor_typing" ||
        obj.kind === "enter_input" ||
        obj.kind === "arrive" ||
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
        obj.kind === "confirm_close" ||
        obj.kind === "confirm_close_others" ||
        obj.kind === "confirm_bypass" ||
        obj.kind === "confirm_restore" ||
        obj.kind === "begin_rename" ||
        obj.kind === "queue" ||
        obj.kind === "draft" ||
        obj.kind === "queue_taken" ||
        obj.kind === "history" ||
        obj.kind === "rule_offers" ||
        obj.kind === "editor_context" ||
        obj.kind === "editor_link" ||
        obj.kind === "scratch" ||
        obj.kind === "notice" ||
        obj.kind === "nav_key" ||
        obj.kind === "review" ||
        obj.kind === "review_diff" ||
        obj.kind === "review_hint"
      ) {
        handler(parsed as Parameters<InboundHandler>[0]);
        return;
      }
    }
    console.warn("agent-ui: __eitriDispatch received an unrecognized envelope shape", parsed);
  };
}
