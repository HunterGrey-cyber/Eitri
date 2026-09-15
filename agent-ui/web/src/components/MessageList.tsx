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

  /* Which tool calls are blocked on a decision the user has not made yet. A turn can have several
     calls of the same tool in flight, so "Bash is waiting" identifies nothing on its own -- this is
     the whole reason `tool_use_id` is carried on a permission request.

     Two values are deliberately kept OUT of this set, so a card with no usable id marks nothing
     rather than guessing at the most recent call:
       null -- a request that arrived carrying no tool-use id. Every permission path in both
               backends forwards whatever id its own source message carried and none of them
               substitutes anything when there is nothing to forward, so this is uncommon but
               real (see `agent/src/wire.rs` and `agent/src/session.rs`).
       ""   -- what the sidecar's proto3 `tool_use_id` arrives as when it is unset, since proto3
               has no absent-string. A `ToolCallRecord.toolUseId` crosses the same boundary and can
               be "" for the same reason, so admitting it would cross-link an arbitrary unrelated
               call to an arbitrary unrelated card -- precisely the mis-identification this marker
               exists to prevent. `PermissionCard` guards the same case on the same grounds. */
  const awaitingPermission = new Set(
    state.pendingPermissions
      .map((p) => p.toolUseId)
      .filter((id): id is string => id !== null && id !== ""),
  );

  return (
    <div className="message-list">
      {state.transcript.map((text, i) => (
        // eslint-disable-next-line react/no-array-index-key -- transcript is append-only, index is stable
        <div key={i} className="message assistant-message" dangerouslySetInnerHTML={{ __html: DOMPurify.sanitize(marked.parse(text) as string) }} />
      ))}
      {state.toolCalls.map((call) => (
        <div
          key={call.toolUseId}
          className="message tool-message"
          data-awaiting-permission={awaitingPermission.has(call.toolUseId) ? "true" : undefined}
        >
          {renderToolCall(call)}
          {awaitingPermission.has(call.toolUseId) && (
            <div className="tool-awaiting-permission">Waiting for your decision below.</div>
          )}
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
