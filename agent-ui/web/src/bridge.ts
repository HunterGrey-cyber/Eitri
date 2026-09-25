import type { AgentDomainEvent, AgentUiSnapshot, ChooserEnvelope, DetailRow, HandoffCommand, Hello, TabId, TabsEnvelope } from "./types";
import type { KeymapHelp } from "./keymap";

export type OutboundMessage =
  | { type: "ready"; request_id: string }
  | { type: "send_message"; request_id: string; tab: TabId; text: string }
  | { type: "interrupt"; request_id: string; tab: TabId }
  /** `decision` is a closed set, not a boolean: Rust rejects an unrecognized value at parse time
   *  rather than defaulting it, and the tempting default would be the one that runs the tool.
   *  `reason` is only ever sent with a denial -- there is no field downstream that would show the
   *  model an approval's reason. */
  | { type: "permission_response"; request_id: string; tab: TabId; permission_id: string; decision: PermissionDecision; reason?: string }
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
  | { type: "open_detail"; request_id: string; tab: TabId }
  | { type: "chooser_closed"; request_id: string; launch: boolean }
  /** Global `f` HINT: panel has pressed `f` in BROWSE, asking shell to start a global HINT. */
  | { type: "hint_request"; request_id: string }
  /** Answer to hint_collect: the number of visible targets the panel froze for this sessionId. */
  | { type: "hint_targets"; request_id: string; session_id: number; count: number };

/** Every decision a backend can actually carry. Verdandi's wire is `bool allow` + `string reason`
 *  and the legacy hook relay is the same shape, so there is no allow-for-session anywhere to send
 *  one to -- adding a button for it here would produce a control that silently degrades to a plain
 *  allow. Widening this is a Verdandi protocol change first. */
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
    | { kind: "events"; tab: TabId; fromRevision: number; throughRevision: number; events: AgentDomainEvent[] }
    | { kind: "snapshot"; tab: TabId; throughRevision: number; state: AgentUiSnapshot }
    | ({ kind: "handoff"; tab: TabId } & HandoffCommand)
    | { kind: "theme"; vars: Record<string, string> }
    /** Whether this panel's pane holds the window's keyboard focus. `shell` decides this from
     *  GTK's focus widget (`shell/src/pane_focus.rs`). It is not decided from this page's own
     *  `window` focus/blur: `shell` arbitrates Ctrl+h/Ctrl+l, and the same answer drives the
     *  status bar and the pane outline, so the three agree. */
    | { kind: "pane_focus"; focused: boolean }
    /** The user moved into this panel with the keyboard (`Ctrl+l`), so open the composer with the
     *  caret in it. Only the keyboard route sends this; a click on a row still lands in BROWSE on
     *  that row. See `serialize_enter_input_for_js` in `core/src/agent_bridge.rs`. */
    | { kind: "enter_input" }
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
    /** Every tab and which is active (session tabs spec §3.1). */
    | ({ kind: "tabs" } & TabsEnvelope)
    | { kind: "tab_detail"; tab: TabId; rows: DetailRow[] }
    | ({ kind: "chooser" } & ChooserEnvelope)
    | { kind: "confirm_close"; tab: TabId; lines: string[] }
    | { kind: "begin_rename"; tab: TabId; current: string | null },
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
      if (
        obj.kind === "hello" ||
        obj.kind === "command_result" ||
        obj.kind === "events" ||
        obj.kind === "snapshot" ||
        obj.kind === "handoff" ||
        obj.kind === "theme" ||
        obj.kind === "pane_focus" ||
        obj.kind === "enter_input" ||
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
        obj.kind === "begin_rename"
      ) {
        handler(parsed as Parameters<InboundHandler>[0]);
        return;
      }
    }
    console.warn("agent-ui: __neovibeDispatch received an unrecognized envelope shape", parsed);
  };
}
