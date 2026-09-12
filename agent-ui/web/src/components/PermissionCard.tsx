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
