import { marked } from "marked";
import DOMPurify from "dompurify";
import { useEffect, useRef } from "react";
import type { AgentUiState } from "../types";
import { renderToolCall } from "../toolRegistry";
import { PermissionCard } from "./PermissionCard";

type Props = {
  state: AgentUiState;
  onAnswerPermission: (requestId: string, allow: boolean, reason?: string) => void;
};

export function MessageList({ state, onAnswerPermission }: Props) {
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
        <div key={call.id} className="message tool-message">
          {renderToolCall(call)}
        </div>
      ))}
      {state.pendingPermissions.map((request) => (
        <PermissionCard key={request.requestId} request={request} onAnswer={onAnswerPermission} />
      ))}
      <div ref={bottomRef} />
    </div>
  );
}
