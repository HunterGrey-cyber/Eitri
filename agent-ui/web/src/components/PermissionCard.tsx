import { useState } from "react";
import type { PermissionRequestRecord } from "../types";
import type { PermissionDecision } from "../bridge";

type Props = {
  request: PermissionRequestRecord;
  /** The session this request belongs to has ended. */
  sessionEnded: boolean;
  onAnswer: (permissionId: string, decision: PermissionDecision, reason?: string) => void;
};

export function PermissionCard({ request, sessionEnded, onAnswer }: Props) {
  const [reason, setReason] = useState("");
  const [answered, setAnswered] = useState(false);

  /* A card belonging to a dead session must not be able to submit into it. The card is not removed
     when the session ends -- an unanswered request is real history, and making it vanish would read
     as a resolution nobody made -- so it is made inert and says why instead. `answered` covers the
     other direction: the SAME decision must not be sendable twice while the real PermissionResolved
     event is still in flight. Neither of these clears the card; only a provider event does that. */
  const inert = answered || sessionEnded;

  function handleAnswer(decision: PermissionDecision) {
    if (inert) return;
    setAnswered(true);
    onAnswer(request.permissionId, decision, decision === "deny" ? reason || undefined : undefined);
  }

  return (
    <div className="permission-card">
      <div className="permission-card-tool">Permission requested: {request.toolName}</div>
      {/* Which call, not just which tool: a turn can have several Bash calls in flight, and this is
          the same id `MessageList` keys that call's own block on, so the two can be read together.
          Rendered only when the backend actually sent one -- a placeholder here would read as a
          lookup that failed rather than as an id this build does not send. The legacy backend sends
          none today by a decision recorded at `agent/src/session.rs`'s `tool_use_id: None`, which
          is also where the evidence for what it could send lives.
          Truthiness rather than `!== null` on purpose: the sidecar's `tool_use_id` crosses proto3,
          where an unset string arrives as "" rather than as an absent field, and "for tool call "
          with nothing after it is worse than saying nothing. */}
      {request.toolUseId && (
        <div className="permission-card-tool-use-id">for tool call {request.toolUseId}</div>
      )}
      <pre className="permission-card-input">{JSON.stringify(request.input, null, 2)}</pre>
      <input
        type="text"
        placeholder="Reason (shown to the agent if you deny)"
        value={reason}
        onChange={(e) => setReason(e.target.value)}
        disabled={inert}
      />
      <div className="permission-card-buttons">
        <button onClick={() => handleAnswer("allow")} disabled={inert}>Approve</button>
        <button onClick={() => handleAnswer("deny")} disabled={inert}>Deny</button>
      </div>
      {sessionEnded && !answered && (
        <div className="permission-card-stale">
          This session ended before the request was answered, so it can no longer be allowed or denied.
        </div>
      )}
    </div>
  );
}
