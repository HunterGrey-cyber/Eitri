import type { AgentDomainEvent, AgentUiSnapshot, ChooserEnvelope, ContextSummary, DetailRow, HandoffCommand, Hello, QueueItem, TabId, TabsEnvelope } from "./types";
import type { KeymapHelp, PaneDirection } from "./keymap";

export type OutboundMessage =
  | { type: "ready"; request_id: string }
  | { type: "send_message"; request_id: string; tab: TabId; text: string }
  | { type: "interrupt"; request_id: string; tab: TabId }
  /** `decision` is a closed set, not a boolean: Rust rejects an unrecognized value at parse time
   *  rather than defaulting it, and the tempting default would be the one that runs the tool.
   *  `reason` is only ever sent with a denial -- there is no field downstream that would show the
   *  model an approval's reason. */
  /** `remember: true` on an allow is "always allow" -- Rust honours it only for
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
  | { type: "queue_message"; request_id: string; tab: TabId; text: string }
  /** `↑`: empty the tab's queue and hand its texts back as `queue_taken`. */
  | { type: "take_back_queue"; request_id: string; tab: TabId }
  /** `Ctrl+Enter`: queue this text (if non-empty), then interrupt if a turn is
   *  running, or flush at once if idle. */
  | { type: "send_now"; request_id: string; tab: TabId; text: string }
  /** The box's text, mirrored. Rust answers nothing. */
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
  /** A tab.* panel-table action: the same verbs the
   *  prefix's own tmux window keys run. Window-level, like the prefix's own `tab_verb` -- whichever
   *  tab is under the keys decides, so this carries no `tab` of its own (Rust's `InboundMessage::
   *  TabVerb` has none either); sent through `postToRust` directly rather than `post()`, which would
   *  add one. `TabVerbWire`'s wire spelling, `core/src/agent_bridge.rs`. */
  | { type: "tab_verb"; request_id: string; verb: "next" | "prev" | "last" | "new" | "close" | "close_others" | "choose" | "info" }
  /** `y` to a `confirm_close_others` prompt is window-level, like `TabVerb`.
   *  Rust recomputes every tab except the active one when answering, so this carries no tab list.
   *  Maps to `InboundMessage::CloseOthers` in `core/src/agent_bridge.rs`. */
  | { type: "close_others"; request_id: string }
  /** Answer a `confirm_bypass` prompt with `y`/`Y` after `bypassYesCounts` accepts the key. Echo
   *  `tab`, `scope`, and `nonce` verbatim. Rust retains the delivered card list and answers only
   *  cards still pending on that tab at that nonce; this side never chooses the cards. Maps to
   *  `InboundMessage::ConfirmBypass` in `core/src/agent_bridge.rs`. */
  | { type: "confirm_bypass"; request_id: string; tab: TabId | null; scope: "tab" | "default"; nonce: number }
  /** Mirror the window's effective mode whenever it changes and once after `ready`. Rust keeps one
   *  value per window because `install_module_nav`, which reads it, is installed once per window.
   *  The empty tab's menu and composer also count as browse and input. Use other for states that
   *  are neither live BROWSE nor live INPUT. */
  | { type: "panel_keys"; request_id: string; mode: PanelKeysMode }
  /** Return a `nav_key` this page could not apply because the mode mirror was stale, an overlay
   *  owns the keys, or the session ended. Rust then runs the chord's ordinary `move_focus` instead
   *  of dropping the key. */
  | { type: "nav_fallthrough"; request_id: string; direction: NavKeyDirection }
  /** `Ctrl+w h/j/k/l` in BROWSE: move the keys from
   *  the panel to the module on that side, by geometry -- exactly what `Ctrl+h/j/k/l` do from it (shell
   *  runs `move_focus(agent, direction)`, the same hook a `nav_fallthrough` uses). Window-level, like
   *  `nav_fallthrough`; a side with no module leaves the keys where they are. Rust answers with a
   *  `command_result`, an error only if no hook is installed. `InboundMessage::PaneNav`,
   *  `core/src/agent_bridge.rs`. */
  | { type: "pane_nav"; request_id: string; direction: PaneDirection }
  /** `gx` on a web link: open this address in the system browser. `url` is
   *  the WHATWG-normalized `href` (`nav.ts#webUrl`) -- what the pick showed -- never the spelling the reply
   *  wrote. Window-level, like `open_path`; Rust re-checks it (`agent_panel.rs#web_url`, http(s) and a plain
   *  host only) and answers with a `command_result`, an error when it is not a web link.
   *  `InboundMessage::OpenUrl`, `core/src/agent_bridge.rs`. */
  | { type: "open_url"; request_id: string; url: string };

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

declare global {
  interface Window {
    webkit?: { messageHandlers?: { neovibeAgent?: { postMessage: (msg: string) => void } } };
    __neovibeDispatch?: (json: string) => void;
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
  const handler = window.webkit?.messageHandlers?.neovibeAgent;
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
        /** Calls in this batch a saved prefix rule answered; absent when none did. */
        ruleNotes?: { toolUseId: string; rule: string }[];
        /** `Write` cards in this batch raised over no file; absent when none were. */
        createsFile?: { permissionId: string; toolUseId: string | null }[];
        /** Calls in this batch whose CLI prompt was answered without a card. */
        promptNotes?: { toolUseId: string; note: string }[];
        /** Tool-use ids of `Write`, `Edit`, or `NotebookEdit` calls answered by the acceptEdits
         *  fast path without a card. Absent when none were. */
        autoNotes?: string[];
        /** Tool-use ids of `Write` calls answered without a card by acceptEdits or bypass,
         *  for paths that did not exist just before the answer. Carries the `createsFile` signal
         *  for calls that raised no card to carry it. */
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
    /** A brand-new tab's own arrival: open the composer with the caret in it. This is `enter_input`'s
     *  only sender now -- every other keyboard arrival that used
     *  to send this now sends `arrive` instead. A click on a row still lands in BROWSE on that row.
     *  See `serialize_enter_input_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "enter_input" }
    /** Keyboard arrivals enter BROWSE on the oldest pending card, or the last row with following
     *  resumed if none waits. Covers pane navigation, `prefix a`, a tray chip without a card,
     *  and a launch with keyboard focus already in the chat.
     *  See `serialize_arrive_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "arrive" }
    /** `shell`'s keys for the `?` overlay (`serialize_keymap_for_js`). */
    | ({ kind: "keymap" } & KeymapHelp)
    /** `send-prefix`/`send-keys` with this panel holding the keys: WebKitGTK cannot be handed the
     *  key itself, so the panel acts on the ones it knows (`C-a`: select all) and ignores the rest.
     *  `key` is tmux's spelling. See `serialize_literal_key_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "literal_key"; key: string }
    /** `prefix ?`: open the `?` overlay in BROWSE. */
    | { kind: "open_keymap" }
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
    | { kind: "confirm_close"; tab: TabId; lines: string[] }
    /** `<leader>bo` / `tab.close-others` is window-level. `tabs` lists the tabs a `y` would close;
     *  Rust recomputes the set when answering rather than trusting the displayed list.
     *  See `serialize_confirm_close_others_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "confirm_close_others"; tabs: TabId[]; lines: string[] }
    | { kind: "begin_rename"; tab: TabId; current: string | null }
    /** A move into bypass asks for y/n first. With tab scope, `tab` identifies the switching tab;
     *  default scope changes the window default and carries `tab: null`. `lines[0]` already
     *  includes the waiting-card count, which this side never computes. Echo `nonce` on `y`/`Y` so
     *  Rust can reject stale answers. A second envelope replaces an open prompt. Serialized by
     *  `serialize_confirm_bypass_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "confirm_bypass"; tab: TabId | null; scope: "tab" | "default"; nonce: number; lines: string[] }
    /** This tab's queue and the refusal reason if its last flush was refused. */
    | { kind: "queue"; tab: TabId; items: QueueItem[]; error: string | null }
    /** Rust's mirror of the composer's own text, sent only on a switch, `ready`,
     *  a reset or a scratch-editor return -- never while the user is typing. */
    | { kind: "draft"; tab: TabId; text: string }
    /** Reply to `take_back_queue`: the queue's texts, for the panel to merge into
     *  the box itself. */
    | { kind: "queue_taken"; tab: TabId; texts: string[] }
    /** The shared prompt history, newest last. */
    | { kind: "history"; entries: string[] }
    /** Per pending card, the "always allow" rule it would install, keyed by
     *  permission id; a card with no entry offers no third button. */
    | { kind: "rule_offers"; tab: TabId; offers: Record<string, string> }
    /** V1's editor-context line, window-scoped. */
    | ({ kind: "editor_context" } & ContextSummary)
    /** Whether the scratch editor currently has this tab's draft open. */
    | { kind: "scratch"; tab: TabId; editing: boolean }
    /** The footer's transient line, from Rust -- e.g. a `gf` refusal. */
    | { kind: "notice"; text: string }
    /** `install_module_nav` claimed bare `Ctrl+j`/`Ctrl+k` using the mode mirror last posted by
     *  this page. This is window-level because it concerns whether BROWSE or INPUT has the keys, so
     *  it carries no tab id. */
    | { kind: "nav_key"; direction: NavKeyDirection },
) => void;

/** The handler's own payload type, exported so callers (`tabs.ts`'s `acceptsEnvelope`, `App.tsx`)
 *  can name it without re-declaring the union. */
export type InboundPayload = Parameters<InboundHandler>[0];

export function installDispatch(handler: InboundHandler): void {
  window.__neovibeDispatch = (json: string) => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(json);
    } catch (e) {
      console.warn("agent-ui: __neovibeDispatch received invalid JSON from Rust", json, e);
      return;
    }
    if (parsed && typeof parsed === "object" && "kind" in parsed) {
      const obj = parsed as { kind: string };
      // Reject a `confirm_bypass` without a numeric `nonce`, warning and dropping it like an
      // unrecognized envelope. A `y` answer must echo a received nonce; `App.tsx` must never invent
      // one.
      if (obj.kind === "confirm_bypass" && typeof (obj as { nonce?: unknown }).nonce !== "number") {
        console.warn("agent-ui: __neovibeDispatch received a malformed confirm_bypass envelope (no nonce)", parsed);
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
        obj.kind === "enter_input" ||
        obj.kind === "arrive" ||
        obj.kind === "keymap" ||
        obj.kind === "literal_key" ||
        obj.kind === "open_keymap" ||
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
        obj.kind === "begin_rename" ||
        obj.kind === "queue" ||
        obj.kind === "draft" ||
        obj.kind === "queue_taken" ||
        obj.kind === "history" ||
        obj.kind === "rule_offers" ||
        obj.kind === "editor_context" ||
        obj.kind === "scratch" ||
        obj.kind === "notice" ||
        obj.kind === "nav_key"
      ) {
        handler(parsed as Parameters<InboundHandler>[0]);
        return;
      }
    }
    console.warn("agent-ui: __neovibeDispatch received an unrecognized envelope shape", parsed);
  };
}
