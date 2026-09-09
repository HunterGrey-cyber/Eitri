import { useState } from "react";
import type { PermissionRequestRecord } from "../types";

type Props = {
  request: PermissionRequestRecord;
  onAnswer: (permissionId: string, allow: boolean, reason?: string) => void;
};

export function PermissionCard({ request, onAnswer }: Props) {
  const [reason, setReason] = useState("");
  const [answered, setAnswered] = useState(false);

  function handleAnswer(allow: boolean) {
    setAnswered(true);
    onAnswer(request.permissionId, allow, allow ? undefined : reason || undefined);
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
        disabled={answered}
      />
      <div className="permission-card-buttons">
        <button onClick={() => handleAnswer(true)} disabled={answered}>Approve</button>
        <button onClick={() => handleAnswer(false)} disabled={answered}>Deny</button>
      </div>
    </div>
  );
}
