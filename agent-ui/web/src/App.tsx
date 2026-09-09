import { useEffect, useState } from "react";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import { ModeSelector } from "./components/ModeSelector";
import { Composer } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { SessionHeader } from "./components/SessionHeader";

export default function App() {
  const [state, setState] = useState(initialState());
  const [sessionStarted, setSessionStarted] = useState(false);
  // Spinner-only, per-requestId in-flight tracking -- never read to answer "is a turn in
  // progress" or "is this permission still pending" (those come only from canonical
  // state.activeTurnId / state.pendingPermissions). Cleared on the matching command_result
  // regardless of ok/error. No spinner UI consumes this yet (out of this task's scope), so the
  // getter is deliberately left unbound here -- this project's tsconfig has `noUnusedLocals`, and
  // binding a name nothing reads would fail the build; the setter alone still exercises the real
  // per-requestId tracking this comment documents.
  const [, setPendingCommands] = useState<Set<string>>(new Set());

  useEffect(() => {
    installDispatch((payload) => {
      if (payload.kind === "snapshot") {
        setState((s) => applySnapshot(s, payload.state));
        setSessionStarted(true);
      } else if (payload.kind === "events") {
        setState((s) => payload.events.reduce((acc, event) => applyEvent(acc, event), s));
      } else if (payload.kind === "command_result") {
        setPendingCommands((prev) => {
          const next = new Set(prev);
          next.delete(payload.requestId);
          return next;
        });
        if (!payload.ok) {
          console.warn("agent-ui: command failed", payload.requestId, payload.error);
        }
      } else if (payload.kind === "error") {
        setSessionStarted(false);
        setState(initialState());
        window.alert(`agent error: ${payload.message}`);
      }
    });
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "ready", request_id: requestId });
  }, []);

  function startSession(mode: "auto" | "bypass") {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "start_session", request_id: requestId, mode });
    setSessionStarted(true);
  }

  function sendMessage(text: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "send_message", request_id: requestId, text });
  }

  function interrupt() {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "interrupt", request_id: requestId });
  }

  function answerPermission(permissionId: string, allow: boolean, reason?: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "permission_response", request_id: requestId, permission_id: permissionId, allow, reason });
  }

  if (!sessionStarted) {
    return (
      <div className="agent-ui-root">
        <ModeSelector onStart={startSession} />
      </div>
    );
  }

  const turnInProgress = state.activeTurnId !== null;
  return (
    <div className="agent-ui-root agent-ui-conversation">
      <SessionHeader state={state} />
      <MessageList state={state} onAnswerPermission={answerPermission} />
      <Composer disabled={turnInProgress} turnInProgress={turnInProgress} onSend={sendMessage} onInterrupt={interrupt} />
    </div>
  );
}
