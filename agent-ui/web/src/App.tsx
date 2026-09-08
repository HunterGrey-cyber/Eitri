import { useEffect, useState } from "react";
import { applyEvent, applySnapshot, initialState, markPermissionAnswered, markTurnInterrupted, markTurnStarted } from "./reducer";
import { installDispatch, postToRust } from "./bridge";
import { ModeSelector } from "./components/ModeSelector";
import { Composer } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { SessionHeader } from "./components/SessionHeader";

export default function App() {
  const [state, setState] = useState(initialState());
  const [sessionStarted, setSessionStarted] = useState(false);

  useEffect(() => {
    installDispatch((payload) => {
      if (payload.kind === "snapshot") {
        setState((s) => applySnapshot(s, payload.snapshot));
        // A snapshot only ever arrives after a real session exists (rehydration after reload) --
        // treat its arrival as proof a session is already running, so a WebView reload doesn't
        // re-show the mode selector for a conversation that's already past that point.
        setSessionStarted(true);
      } else if (payload.kind === "error") {
        // Reset first, alert second: window.alert() blocks the page's JS until its modal is
        // dismissed, and this sandbox's synthetic-input support for dismissing a real WebKitGTK
        // script dialog is unverified -- resetting first means the recovery (back to the mode
        // selector) happens unconditionally, never gated on a dialog actually getting dismissed.
        // Reset the whole AgentUiState, not just sessionStarted: this envelope now also covers a
        // fatal send_turn/interrupt/respond_permission failure mid-conversation (not just a
        // failed start), and Rust drops the dead AgentSession in the fatal cases too -- a
        // subsequent successful "start_session" must not carry over the previous session's stale
        // state.
        setSessionStarted(false);
        setState(initialState());
        window.alert(`agent error: ${payload.message}`);
      } else {
        setState((s) => applyEvent(s, payload.event));
      }
    });
    postToRust({ type: "ready" });
  }, []);

  function startSession(mode: "auto" | "bypass") {
    postToRust({ type: "start_session", mode });
    setSessionStarted(true);
  }

  function sendMessage(text: string) {
    postToRust({ type: "send_message", text });
    // Optimistic: no AgentEvent ever announces "a turn just started" (that's local Rust-side
    // bookkeeping, set synchronously inside AgentSession::send_turn before any wire event comes
    // back) -- without this, turnInProgress would never become true and the Stop button/composer
    // disabling would be permanently dead. A real turn_finished event (or a rehydration
    // snapshot) is what eventually clears it back to false.
    setState((s) => markTurnStarted(s));
  }

  function interrupt() {
    postToRust({ type: "interrupt" });
    // Optimistic: AgentSession::interrupt() now clears pending_permissions on the Rust side too
    // (any request from the turn being stopped can never be genuinely answered) -- without this,
    // any permission card visible at the moment of interrupt would linger forever in the frontend.
    setState((s) => markTurnInterrupted(s));
  }

  function answerPermission(requestId: string, allow: boolean, reason?: string) {
    postToRust({ type: "permission_response", request_id: requestId, allow, reason });
    // Optimistic: no AgentEvent ever announces "this permission request was resolved" -- without
    // this, the card stays rendered forever (a real bug found during Task 8's real sandbox
    // verification, confirmed 3/3 times). A duplicate answer on an already-cleared card is a no-op
    // here and a logged, harmless error on the Rust side.
    setState((s) => markPermissionAnswered(s, requestId));
  }

  if (!sessionStarted) {
    return (
      <div className="agent-ui-root">
        <ModeSelector onStart={startSession} />
      </div>
    );
  }

  return (
    <div className="agent-ui-root agent-ui-conversation">
      <SessionHeader state={state} />
      <MessageList state={state} onAnswerPermission={answerPermission} />
      <Composer disabled={state.turnInProgress} turnInProgress={state.turnInProgress} onSend={sendMessage} onInterrupt={interrupt} />
    </div>
  );
}
