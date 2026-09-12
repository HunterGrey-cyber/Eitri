import type { AgentDomainEvent, AgentUiState, Hello } from "./types";

export type OutboundMessage =
  | { type: "ready"; request_id: string }
  | { type: "start_session"; request_id: string; mode: "auto" | "bypass" }
  | { type: "send_message"; request_id: string; text: string }
  | { type: "interrupt"; request_id: string }
  | { type: "permission_response"; request_id: string; permission_id: string; allow: boolean; reason?: string };

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
    | { kind: "snapshot"; throughRevision: number; state: AgentUiState }
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
