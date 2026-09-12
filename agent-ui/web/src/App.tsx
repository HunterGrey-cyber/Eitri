import { useEffect, useState } from "react";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import { ModeSelector } from "./components/ModeSelector";
import { Composer } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { SessionHeader } from "./components/SessionHeader";
import type { Hello, PermissionModeChoice } from "./types";

export default function App() {
  const [state, setState] = useState(initialState());
  const [hello, setHello] = useState<Hello | null>(null);
  const [sessionStarted, setSessionStarted] = useState(false);
  /** The requestId of an in-flight `start_session`. Backend construction is genuinely slow (the
   * sidecar path spawns a process and does a real handshake; a cold Verdandi checkout also builds
   * it), so Rust defers its `command_result` until the worker finishes. This is what lets the start
   * screen say "starting" instead of appearing to have ignored the click. */
  const [startingRequestId, setStartingRequestId] = useState<string | null>(null);
  /** A fatal, session-ending failure, shown in the panel. Replaces window.alert, which cannot be
   * copied, cannot show the sidecar's own multi-line startup diagnostics, and blocks the WebView. */
  const [fatalError, setFatalError] = useState<string | null>(null);
  // Spinner-only, per-requestId in-flight tracking -- never read to answer "is a turn in
  // progress" or "is this permission still pending" (those come only from canonical
  // state.activeTurnId / state.pendingPermissions).
  const [, setPendingCommands] = useState<Set<string>>(new Set());

  useEffect(() => {
    installDispatch((payload) => {
      if (payload.kind === "hello") {
        setHello(payload);
      } else if (payload.kind === "snapshot") {
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
        setStartingRequestId((current) => {
          if (current !== payload.requestId) return current;
          // The deferred reply to our start_session. On failure, fall back to the start screen so
          // the user can retry -- an `error` envelope follows with the real cause.
          if (!payload.ok) setSessionStarted(false);
          return null;
        });
        if (!payload.ok) {
          console.warn("agent-ui: command failed", payload.requestId, payload.error);
        }
      } else if (payload.kind === "error") {
        setSessionStarted(false);
        setStartingRequestId(null);
        setState(initialState());
        setFatalError(payload.message);
      }
    });
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "ready", request_id: requestId });
  }, []);

  /** `resume` carries the Claude provider session id to continue, or nothing for a fresh session.
   * A resume that fails comes back as a normal fatal error and returns here -- it is never turned
   * into a fresh session, by this component or by anything below it. */
  function startSession(mode: PermissionModeChoice, resume?: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    setStartingRequestId(requestId);
    setFatalError(null);
    postToRust({ type: "start_session", request_id: requestId, mode, resume });
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

  const errorBanner =
    fatalError === null ? null : (
      <div className="fatal-error" role="alert">
        <strong>The agent session ended.</strong>
        {/* <pre>, not a <p>: the sidecar's startup diagnostics are multi-line and the exact text
            (a CLI version, a checkout revision) is the whole point. */}
        <pre>{fatalError}</pre>
        <button onClick={() => setFatalError(null)}>Dismiss</button>
      </div>
    );

  if (!sessionStarted) {
    return (
      <div className="agent-ui-root">
        {errorBanner}
        <ModeSelector hello={hello} connecting={startingRequestId !== null} onStart={startSession} />
      </div>
    );
  }

  // Authoritative, server-originated. `activeTurnId` is set by a real TurnStarted event from the
  // provider and cleared by a real TurnCompleted -- never by this component optimistically marking
  // a turn as started when the user pressed Send. The reducer also clears it on a session that ends
  // without one, because no TurnCompleted is ever coming for a session that is gone.
  const turnInProgress = state.activeTurnId !== null;
  const sessionEnded = state.status.kind === "unavailable" || state.status.kind === "closed";

  /* A session that died is announced here, not left to be inferred from a status word in the
     header. `unavailable` specifically means this client stopped being able to observe the session
     -- the transcript above it can be missing its tail, or a piece out of its middle -- so the
     reason text (which says exactly what was lost) is rendered in full and cannot be dismissed.
     A hidden warning about incomplete output is the same thing as no warning. */
  const sessionEndedBanner =
    state.status.kind === "unavailable" ? (
      <div className="session-lost" role="alert">
        <strong>This session was lost. What is shown above may be incomplete.</strong>
        <pre>{state.status.reason}</pre>
      </div>
    ) : state.status.kind === "closed" ? (
      <div className="session-over">This session has ended ({state.status.reason}).</div>
    ) : null;

  return (
    <div className="agent-ui-root agent-ui-conversation">
      <SessionHeader state={state} />
      {errorBanner}
      <MessageList state={state} onAnswerPermission={answerPermission} />
      {sessionEndedBanner}
      <Composer
        // A dead session takes no more turns. Without this, clearing `activeTurnId` on a lost
        // session would have handed the user an enabled composer pointed at nothing.
        disabled={turnInProgress || sessionEnded}
        turnInProgress={turnInProgress}
        sessionEnded={sessionEnded}
        // Stop is gated on the capability, not on the backend's name: if a provider ever reports
        // that it cannot interrupt, the control disappears rather than sending a command the
        // server would reject.
        canInterrupt={state.capabilities.interrupt}
        onSend={sendMessage}
        onInterrupt={interrupt}
      />
    </div>
  );
}
