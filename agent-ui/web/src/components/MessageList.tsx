import { marked } from "marked";
import DOMPurify from "dompurify";
import { useEffect, useRef } from "react";
import type { AgentUiState } from "../types";
import { renderToolCall } from "../toolRegistry";
import { PermissionCard } from "./PermissionCard";
import type { PermissionDecision } from "../bridge";

type Props = {
  state: AgentUiState;
  /** The session is gone. Pending cards stay visible but can no longer submit into it. */
  sessionEnded: boolean;
  onAnswerPermission: (permissionId: string, decision: PermissionDecision, reason?: string) => void;
};

export function MessageList({ state, sessionEnded, onAnswerPermission }: Props) {
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [state.transcript.length, state.toolCalls.length, state.pendingPermissions.length]);

  return (
    <div className="message-list">
      {state.transcript.map((text, i) => (
        // eslint-disable-next-line react/no-array-index-key -- transcript is append-only, index is stable
        <div key={i} className="message assistant-message" dangerouslySetInnerHTML={{ __html: DOMPurify.sanitize(marked.parse(text) as string) }} />
      ))}
      {state.toolCalls.map((call) => (
        <div key={call.toolUseId} className="message tool-message">
          {renderToolCall(call)}
        </div>
      ))}
      {state.pendingPermissions.map((request) => (
        <PermissionCard
          key={request.permissionId}
          request={request}
          sessionEnded={sessionEnded}
          onAnswer={onAnswerPermission}
        />
      ))}
      <div ref={bottomRef} />
    </div>
  );
}
