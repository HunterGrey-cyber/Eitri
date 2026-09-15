import { useEffect, useRef, useState } from "react";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import type { PermissionDecision } from "./bridge";
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
  /** When the payload carrying a turn's first assistant text was RECEIVED, for the render trace.
   *  A ref, not state: writing it must not itself cause a render, which would be the thing being
   *  measured. Null except in the window between that payload arriving and its frame being drawn. */
  const firstTextReceivedAt = useRef<number | null>(null);
  /** Guards the render report to one per turn. Without it the effect below re-arms on every one of
   *  a reply's ~400 deltas. */
  const renderReportSent = useRef(false);

  useEffect(() => {
    installDispatch((payload) => {
      if (payload.kind === "hello") {
        setHello(payload);
      } else if (payload.kind === "snapshot") {
        setState((s) => applySnapshot(s, payload.state, payload.throughRevision));
        setSessionStarted(true);
      } else if (payload.kind === "events") {
        // A new turn resets the render trace: each turn reports its own first text, once.
        if (payload.events.some((e) => e.type === "turn_started")) {
          firstTextReceivedAt.current = null;
          renderReportSent.current = false;
        }
        // Stamped before the state update that will cause the render, so the span covers the work
        // being measured rather than starting after it.
        if (
          firstTextReceivedAt.current === null &&
          payload.events.some((e) => e.type === "content_delta" && e.kind === "text" && e.text !== "")
        ) {
          firstTextReceivedAt.current = performance.now();
        }
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

  /* Reports how long this WebView took to draw a turn's first assistant text. The effect runs after
     React has committed the DOM; the animation frame runs just before the browser paints it. That is
     a frame, not a photon -- read it as a floor on what the user perceives, never as a measured
     perceptual latency. Deliberately NOT cancelled on cleanup: with a delta arriving every ~33ms, a
     cleanup that cancelled the pending frame would re-arm faster than the frame could ever fire, and
     the mark would simply never be reported. */
  useEffect(() => {
    const receivedAt = firstTextReceivedAt.current;
    if (receivedAt === null || renderReportSent.current) return;
    renderReportSent.current = true;
    requestAnimationFrame(() => {
      postToRust({
        type: "turn_rendered",
        request_id: nextRequestId(),
        receive_to_frame_ms: performance.now() - receivedAt,
      });
    });
  }, [state.transcript]);

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

  function answerPermission(permissionId: string, decision: PermissionDecision, reason?: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "permission_response", request_id: requestId, permission_id: permissionId, decision, reason });
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
      {/* `sessionEnded` makes every pending card inert. The cards themselves are NOT removed: a
          permission that was still open when the session died is real history, and deleting it
          would read as a resolution nobody made. */}
      <MessageList state={state} sessionEnded={sessionEnded} onAnswerPermission={answerPermission} />
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
