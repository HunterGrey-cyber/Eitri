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
               exists to prevent. `PermissionCard` guards the same case on the same grounds.

     The consequence, stated rather than left to be worked out: on the legacy backend EVERY
     permission carries null, so this set is always empty and the marker below never renders at all.
     That is correct behaviour, not a defect -- but it means the marker is unobservable on the only
     backend this project's machine can currently run. `shell/MANUAL_VERIFICATION.md`'s GUI checklist
     says the same thing, because someone looking for it on legacy would otherwise file a bug. */
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
