import type { AgentDomainEvent, AgentUiSnapshot, Hello } from "./types";

export type OutboundMessage =
  | { type: "ready"; request_id: string }
  | { type: "start_session"; request_id: string; mode: "auto" | "bypass"; resume?: string }
  | { type: "send_message"; request_id: string; text: string }
  | { type: "interrupt"; request_id: string }
  /** `decision` is a closed set, not a boolean: Rust rejects an unrecognized value at parse time
   *  rather than defaulting it, and the tempting default would be the one that runs the tool.
   *  `reason` is only ever sent with a denial -- there is no field downstream that would show the
   *  model an approval's reason. */
  | { type: "permission_response"; request_id: string; permission_id: string; decision: PermissionDecision; reason?: string }
  /** How long this WebView took to draw a turn's first assistant text, from its own receipt of the
   *  payload to the animation frame that rendered it. A SPAN, not an instant: `performance.now()`
   *  and Rust's `Instant` have unrelated epochs, so a timestamp crossing this boundary would be a
   *  confident, meaningless number. Diagnostic only -- Rust expects no reply and nothing branches
   *  on it. */
  | { type: "turn_rendered"; request_id: string; receive_to_frame_ms: number };

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
    | { kind: "events"; fromRevision: number; throughRevision: number; events: AgentDomainEvent[] }
    | { kind: "snapshot"; throughRevision: number; state: AgentUiSnapshot }
    | { kind: "error"; message: string },
) => void;

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
      if (obj.kind === "hello" || obj.kind === "command_result" || obj.kind === "events" || obj.kind === "snapshot" || obj.kind === "error") {
        handler(parsed as Parameters<InboundHandler>[0]);
        return;
      }
    }
    console.warn("agent-ui: __neovibeDispatch received an unrecognized envelope shape", parsed);
  };
}
