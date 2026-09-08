import type { AgentEvent, AgentUiState } from "./types";

export type OutboundMessage =
  | { type: "ready" }
  | { type: "start_session"; mode: "auto" | "bypass" }
  | { type: "send_message"; text: string }
  | { type: "interrupt" }
  | { type: "permission_response"; request_id: string; allow: boolean; reason?: string };

declare global {
  interface Window {
    webkit?: { messageHandlers?: { neovibeAgent?: { postMessage: (msg: string) => void } } };
    __neovibeDispatch?: (json: string) => void;
  }
}

export function postToRust(message: OutboundMessage): void {
  const handler = window.webkit?.messageHandlers?.neovibeAgent;
  if (!handler) {
    // No real WebKitGTK bridge present -- expected when this page is opened directly in a
    // regular browser during frontend development (`npm run dev`). Never throw: the app must
    // still render and be visually inspectable without a real shell process behind it.
    console.warn("agent-ui: no WebKitGTK message handler present, message dropped", message);
    return;
  }
  handler.postMessage(JSON.stringify(message));
}

type InboundHandler = (
  payload:
    | { kind: "event"; event: AgentEvent }
    | { kind: "snapshot"; snapshot: AgentUiState }
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
      if (obj.kind === "event") {
        handler({ kind: "event", event: (parsed as { kind: "event"; event: AgentEvent }).event });
        return;
      }
      if (obj.kind === "snapshot") {
        handler({ kind: "snapshot", snapshot: (parsed as { kind: "snapshot"; snapshot: AgentUiState }).snapshot });
        return;
      }
      if (obj.kind === "error") {
        handler({ kind: "error", message: (parsed as { kind: "error"; message: string }).message });
        return;
      }
    }
    console.warn("agent-ui: __neovibeDispatch received an unrecognized envelope shape", parsed);
  };
}
