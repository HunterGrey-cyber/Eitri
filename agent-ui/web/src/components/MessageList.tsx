import { marked } from "marked";
import DOMPurify from "dompurify";
import { useEffect, useMemo, useRef } from "react";
import type { AgentUiState } from "../types";
import { renderToolCall } from "../toolRegistry";
import { buildTimeline } from "../timeline";
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

  /* Match pending permissions to tool calls by `tool_use_id`, since several calls of the same
     tool can be in flight. Exclude `null` and "" so a card without a usable id cannot mark an
     unrelated call. A missing source id remains null; proto3 represents an unset string as "",
     including on tool calls. `PermissionCard` guards the same cases. Legacy permissions carry
     null, so this marker does not render on that backend. */
  const awaitingPermission = new Set(
    state.pendingPermissions
      .map((p) => p.toolUseId)
      .filter((id): id is string => id !== null && id !== ""),
  );

  /* One ordered sequence, not three lists one after another.

     Until 2026-09-15 this component mapped `transcript`, then `toolCalls`, then
     `pendingPermissions`, so every tool card rendered below every assistant message however the
     turn really went. `buildTimeline` merges them on `seq`, an ordering key Rust's projection
     assigns and its snapshot ships -- so a reload or a `UiDelivery::Resync`, which rebuild this
     whole state from that snapshot, produce the same order as the live path did.

     Memoised on `state`, which the reducer replaces wholesale on every folded event -- so this
     recomputes exactly as often as the conversation changes, and not on a re-render caused by
     anything else (a `sessionEnded` flip, a parent re-render while the user types). The merge
     allocates three mapped arrays, a Map and a Set and then sorts the whole conversation, which
     during streaming would otherwise run on every 33ms pump batch.

     Not a fix for this component's real cost, and not measured: the `marked.parse` +
     `DOMPurify.sanitize` below still runs over EVERY transcript message on EVERY render, and is
     pre-existing, larger, and nobody has profiled it. */
  const timeline = useMemo(() => buildTimeline(state), [state]);

  return (
    <div className="message-list">
      {timeline.map((item) => {
        switch (item.kind) {
          case "message":
            return (
              <div
                key={item.key}
                className="message assistant-message"
                dangerouslySetInnerHTML={{ __html: DOMPurify.sanitize(marked.parse(item.text) as string) }}
              />
            );
          case "tool":
            return (
              <div
                key={item.key}
                className="message tool-message"
                data-awaiting-permission={awaitingPermission.has(item.call.toolUseId) ? "true" : undefined}
              >
                {renderToolCall(item.call)}
                {awaitingPermission.has(item.call.toolUseId) && (
                  /* "below" is literal for the call that owns the card: `buildTimeline` emits a
                     linked card immediately after it. The one case where it is not is two tool
                     calls sharing one `toolUseId` — the card is consumed by the first, so the
                     second renders this line with the card above it rather than below. That
                     should not happen, `buildTimeline` refuses to amplify it into a duplicate
                     card, and it is not worth a second lookup here; it is noted so the sentence
                     is not read as a guarantee. */
                  <div className="tool-awaiting-permission">Waiting for your decision below.</div>
                )}
              </div>
            );
          case "permission":
            return (
              <PermissionCard
                key={item.key}
                request={item.request}
                sessionEnded={sessionEnded}
                onAnswer={onAnswerPermission}
              />
            );
        }
      })}
      <div ref={bottomRef} />
    </div>
  );
}
