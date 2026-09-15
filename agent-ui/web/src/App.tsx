import { useEffect, useRef, useState } from "react";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import type { PermissionDecision } from "./bridge";
import { ModeSelector } from "./components/ModeSelector";
import { Composer } from "./components/Composer";
import type { RestoredDraft } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { SessionHeader } from "./components/SessionHeader";
import { ContinueInTerminal, HandoffCommandCard } from "./components/TerminalHandoff";
import type { HandoffCommand, Hello, PermissionModeChoice } from "./types";

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
  /** The command for a conversation that has just been closed here and moved to a terminal. Set by
   *  the `handoff` envelope, which Rust sends only after the real session close finished.
   *
   *  **This is a view of Rust's own `AgentPanelState::last_handoff`, not the only copy.** Rust keeps
   *  the command and re-sends it in the `ready` handshake, so a panel reload (Ctrl+Shift+R, the top
   *  bar's ⟳) or a WebView crash gets it back — on the default legacy backend the id in it is
   *  recoverable from nowhere else at all. Cleared here when a real `snapshot` arrives, which is
   *  also when Rust clears its copy: a session that is actually running, not one merely asked for. */
  const [handoff, setHandoff] = useState<HandoffCommand | null>(null);
  /** The requestId of an in-flight `handoff_to_terminal`, or null.
   *
   *  Non-null means the conversation is CLOSING: Rust has already taken the backend out of its own
   *  state and a worker thread is running the real `shutdown()`. Nothing is torn down here until the
   *  `handoff` envelope arrives, so without this the composer would stay live and a typed Enter
   *  would clear the box into a session that no longer exists. */
  const [handoffRequestId, setHandoffRequestId] = useState<string | null>(null);
  /** A refused command, in words, on screen. Rust's refusals carry real human-readable reasons
   *  (`HandoffRefusal::message`, `BackendError::message`) and every one of them used to reach a
   *  `console.warn` and nothing else. */
  const [commandNotice, setCommandNotice] = useState<string | null>(null);
  /** Text to put back in the composer after a refused send. See `RestoredDraft`. */
  const [restoredDraft, setRestoredDraft] = useState<RestoredDraft | null>(null);
  /** What each in-flight request actually was, so its reply can be handled as that thing. A ref: it
   *  is bookkeeping, never rendered, and a render per outgoing command would be pure cost.
   *
   *  Only the three kinds whose replies need special handling are recorded. An interrupt or a
   *  permission response has no entry and comes back as `undefined`, which is correct rather than a
   *  gap: its refusal takes the plain "show the reason" path. Every recorded request gets exactly
   *  one `command_result` and is deleted there, so this cannot grow. */
  const inFlight = useRef<Map<string, { kind: "start" | "send" | "handoff"; text?: string }>>(new Map());
  /** Monotonic, so two refusals of the same text are two distinct restores. */
  const restoreSeq = useRef(0);
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
        // A snapshot means a session is genuinely RUNNING, which is also when Rust clears its own
        // copy of the command. Clearing it when a start was merely requested would throw it away on
        // a start that then failed -- and on the legacy backend that is the last reference to a
        // conversation nothing else remembers.
        setHandoff(null);
        setCommandNotice(null);
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
        const record = inFlight.current.get(payload.requestId);
        inFlight.current.delete(payload.requestId);
        setStartingRequestId((current) => {
          if (current !== payload.requestId) return current;
          // The deferred reply to our start_session. On failure, fall back to the start screen so
          // the user can retry -- an `error` envelope follows with the real cause.
          if (!payload.ok) setSessionStarted(false);
          return null;
        });
        if (record?.kind === "handoff") {
          // Either the handoff finished (the `handoff` envelope arrived first and already reset
          // everything) or it was refused and the session is untouched. Both end the closing state.
          setHandoffRequestId((current) => (current === payload.requestId ? null : current));
        }
        if (!payload.ok) {
          console.warn("agent-ui: command failed", payload.requestId, payload.error);
          if (record?.kind === "send") {
            // The composer cleared this optimistically. Rust refused it, so it goes back — a
            // message that vanishes with no trace is the outcome this exists to prevent.
            restoreSeq.current += 1;
            setRestoredDraft({ text: record.text ?? "", seq: restoreSeq.current });
            setCommandNotice(`That message was not sent (${payload.error}). It is back in the box.`);
          } else if (record?.kind === "start") {
            // A failed start already has the start screen and an `error` envelope carrying the real
            // cause; a second surface for it would just be noise.
          } else {
            setCommandNotice(payload.error);
          }
        }
      } else if (payload.kind === "handoff") {
        // The session is genuinely closed by the time this arrives (Rust dispatches it only after
        // its own `shutdown()` returned), so the conversation goes with it rather than being left
        // on screen looking live -- the same treatment a fatal error gets, for the same reason.
        setSessionStarted(false);
        setStartingRequestId(null);
        setHandoffRequestId(null);
        setCommandNotice(null);
        setState(initialState());
        setHandoff(payload);
        // Suppresses the resume offer for THIS client's already-delivered `hello`, which was
        // computed once at mount. That only ever matches when this session was itself resume-started
        // -- and that is fine, because it is not the durable half of this rule: Rust applies the
        // same suppression when it builds `hello`, on every mount, which is the case that actually
        // bites (the handed-over session is the most recently updated record, so it would otherwise
        // head the list). See `agent_panel::ready_payloads`. Any OTHER stored session is untouched
        // on both sides -- this drops exactly the one row that was just given away, rather than
        // clearing the offer, which would hide every other session the workspace remembers.
        setHello((current) =>
          current === null
            ? current
            : {
                ...current,
                resumableSessions: current.resumableSessions.filter(
                  (s) => s.providerSessionId !== payload.providerSessionId,
                ),
              },
        );
      } else if (payload.kind === "error") {
        setSessionStarted(false);
        setStartingRequestId(null);
        setHandoffRequestId(null);
        setState(initialState());
        setFatalError(payload.message);
        /* Re-ask for `hello`, because we are about to show the start screen again and the copy we
           captured at mount is a snapshot of the conversation records as they were then.

           The session that just died is exactly the one the user is most likely to want back, and
           it was persisted on adoption (`agent/src/ingestion.rs` -> `conversation::persist_record`)
           -- so it IS on disk and offerable, and only the in-memory list is stale. Without this the
           picker's own note, "Previous conversations here, newest first", is a claim the component
           cannot honour at that moment; the only escape hatch was `Ctrl+Shift+R`, which nothing
           tells the user about.

           Safe to re-post: Rust answers `Ready` from canonical state with a fresh
           `BackendGreeting::for_kind` and a `command_result`, never with another `error`, so there
           is no loop here. */
        requestHello();
      }
    });
    requestHello();
  }, []);

  /** Posts `ready` and tracks it as in-flight. Rust replies with `hello` (and a snapshot, if a
   *  session exists). Called on mount and again whenever the start screen comes back. */
  function requestHello() {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "ready", request_id: requestId });
  }

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
    inFlight.current.set(requestId, { kind: "start" });
    postToRust({ type: "start_session", request_id: requestId, mode, resume });
  }

  function handoffToTerminal() {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    inFlight.current.set(requestId, { kind: "handoff" });
    // Set BEFORE the post, so the composer is disabled from this moment rather than from whenever a
    // reply comes back. Rust takes the session out of its own state inside the handler this message
    // reaches, and every send after that point would be refused.
    setHandoffRequestId(requestId);
    setCommandNotice(null);
    postToRust({ type: "handoff_to_terminal", request_id: requestId });
  }

  function sendMessage(text: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    // The text is kept so a refusal can put it back. Dropped again as soon as the reply arrives.
    inFlight.current.set(requestId, { kind: "send", text });
    setCommandNotice(null);
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

  /* Rendered on both screens. A refused command can return the panel to the start screen (a failed
     handoff close, for one), and a reason that only exists in the conversation view would be gone by
     the time it could be read. */
  const commandNoticeBanner =
    commandNotice === null ? null : (
      <div className="command-notice" role="alert">
        <span>{commandNotice}</span>
        <button onClick={() => setCommandNotice(null)}>Dismiss</button>
      </div>
    );

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
        {commandNoticeBanner}
        {handoff !== null && <HandoffCommandCard handoff={handoff} />}
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
  /* The conversation is on its way out: Rust already owns the backend on a shutdown worker and will
     refuse every command until that finishes. The composer must reflect that rather than accepting
     input it cannot deliver. */
  const handingOff = handoffRequestId !== null;

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
      {commandNoticeBanner}
      <Composer
        // A dead session takes no more turns, and neither does one already being closed for a
        // terminal handoff. Without this, clearing `activeTurnId` on a lost session would have
        // handed the user an enabled composer pointed at nothing.
        disabled={turnInProgress || sessionEnded || handingOff}
        turnInProgress={turnInProgress}
        sessionEnded={sessionEnded}
        closing={handingOff}
        restoredDraft={restoredDraft}
        // Stop is gated on the capability, not on the backend's name: if a provider ever reports
        // that it cannot interrupt, the control disappears rather than sending a command the
        // server would reject.
        canInterrupt={state.capabilities.interrupt}
        onSend={sendMessage}
        onInterrupt={interrupt}
      />
      {/* Below the composer, deliberately: it is a way OUT of this panel, not one of the things the
          panel is for, and it must not compete with the lost-session banner for the space directly
          above the box. Offered for a session that has ENDED too -- continuing a conversation that
          died here is arguably the case where a terminal helps most -- so the only
          conversation-state term is whether a turn is running. `canResume` comes from the provider's
          advertised capability, never from the backend's name. */}
      <ContinueInTerminal
        providerSessionId={state.providerSessionId}
        turnInProgress={turnInProgress}
        canResume={state.capabilities.resume}
        handingOff={handingOff}
        onHandoff={handoffToTerminal}
      />
    </div>
  );
}
