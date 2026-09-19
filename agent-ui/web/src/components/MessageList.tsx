import { useEffect, useMemo, useRef } from "react";
import type { AgentUiState, ToolCallRecord } from "../types";
import { renderToolCall } from "../toolRegistry";
import { buildTimeline, isUsableLink } from "../timeline";
import { renderMarkdown } from "../markdown";
import { PermissionCard } from "./PermissionCard";
import { Row } from "./Row";
import type { PermissionDecision } from "../bridge";

type Props = {
  state: AgentUiState;
  /** The session is gone. Pending cards stay visible but can no longer submit into it. */
  sessionEnded: boolean;
  /** Which rows are expanded, keyed by the timeline `key`. Owned by `App.tsx` because `Enter` acts
   *  on the cursor, and the cursor is App's -- see `App.tsx`'s `onKeyDown`, `toggle-expand` arm. */
  expanded: Record<string, boolean>;
  /** The index into THIS component's own `timeline` (below) that `Enter`/`y` act on in `App.tsx`.
   *  Safe to compare by position rather than by identity: both here and there, `timeline` is
   *  `buildTimeline(state)` over the same `state`, a pure function, so the two computations always
   *  agree on what sits at a given index for a given `state` even though each holds its own copy. */
  cursor: number;
  onAnswerPermission: (permissionId: string, decision: PermissionDecision, reason?: string) => void;
};

/** Whether a finished tool call succeeded, failed, or is still running -- the state a row's sign
 * glyph carries. A call with no result yet is never "done with nothing to say"; see `ToolResult`
 * in `toolRegistry.tsx`, which draws the same three-way distinction in the body. */
function toolSign(call: ToolCallRecord): string {
  if (call.result === null) return "◐";
  return call.result.isError ? "✗" : "✓";
}

/** How close to the true bottom of `.message-list` still counts as "following the tail," for the
 *  auto-follow guard below. Some slack rather than an exact `0`: a smooth scroll already in flight,
 *  or ordinary sub-pixel rounding, can leave the true bottom a few pixels away even though the user
 *  never scrolled up on purpose. Arbitrary and not tuned against a real screen. */
const BOTTOM_FOLLOW_THRESHOLD_PX = 24;

export function MessageList({ state, sessionEnded, expanded, cursor, onAnswerPermission }: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  // Include `userPrompts` because a prompt can add a row without changing the other collections.
  // Only collection lengths trigger this follow effect; streamed deltas merge into one transcript
  // entry, so a growing reply is not followed here. Guard following so a new row cannot pull a
  // reader back down after `j`/`k` or a wheel scroll moves them up. Use scroll events to set
  // `followingRef`: measuring after a tall new row mounts could falsely report that a reader at the
  // bottom had scrolled away. Decreasing `scrollTop` away from the bottom stops following; a scroll
  // event within the bottom threshold resumes it. A smooth scroll toward the bottom increases
  // `scrollTop`, so it cannot stop itself. Read scroll position rather than cursor index so
  // mouse-only readers, whose cursor stays at zero, can still follow. jsdom has no layout or
  // automatic scroll events, so the auto-follow tests dispatch them explicitly.
  const followingRef = useRef(true);
  const lastScrollTopRef = useRef(0);
  const onScroll = () => {
    const list = listRef.current;
    if (list === null) return;
    const distance = list.scrollHeight - list.scrollTop - list.clientHeight;
    if (distance <= BOTTOM_FOLLOW_THRESHOLD_PX) {
      followingRef.current = true;
    } else if (list.scrollTop < lastScrollTopRef.current) {
      followingRef.current = false;
    }
    lastScrollTopRef.current = list.scrollTop;
  };
  useEffect(() => {
    if (followingRef.current) {
      bottomRef.current?.scrollIntoView({ behavior: "smooth" });
    }
  }, [state.userPrompts.length, state.transcript.length, state.toolCalls.length, state.pendingPermissions.length]);

  /* Match pending permissions to tool calls by `tool_use_id`, since several calls of the same
     tool can be in flight. Use the shared `isUsableLink` predicate from `../timeline` to exclude
     `null` and "", matching `PermissionCard` without duplicating the guard. Legacy permissions
     carry null, so this marker does not render on that backend. */
  const awaitingPermission = new Set(state.pendingPermissions.map((p) => p.toolUseId).filter(isUsableLink));

  /* One ordered sequence, not three lists one after another.

     Until 2026-09-15 this component mapped `transcript`, then `toolCalls`, then
     `pendingPermissions`, so every tool card rendered below every assistant message however the
     turn really went. `buildTimeline` merges them on `seq`, an ordering key Rust's projection
     assigns and its snapshot ships -- so a reload or a `UiDelivery::Resync`, which rebuild this
     whole state from that snapshot, produce the same order as the live path did.

     Memoised on `state`, which the reducer replaces wholesale on every folded event -- so this
     recomputes exactly as often as the conversation changes, and not on a re-render caused by
     anything else (a `sessionEnded` flip, a parent re-render while the user types). The merge
     allocates four mapped arrays, a Map and a Set and then sorts the whole conversation, which
     during streaming would otherwise run on every 33ms pump batch.

     Not a fix for this component's real cost, and not measured: `renderMarkdown` (marked, now with
     highlight.js, then DOMPurify) below still runs over EVERY transcript message on EVERY render,
     and is pre-existing, larger, and nobody has profiled it. */
  const timeline = useMemo(() => buildTimeline(state), [state]);

  return (
    <div className="message-list" ref={listRef} onScroll={onScroll}>
      {timeline.map((item, index) => {
        const current = index === cursor;
        switch (item.kind) {
          case "prompt":
            // Plain text, never markdown: this is what the user typed, and parsing it would render
            // their literal backticks and asterisks as formatting they did not ask for.
            return (
              <Row key={item.key} kind="prompt" sign="›" current={current}>
                {item.text}
              </Row>
            );
          case "message":
            return (
              <Row key={item.key} kind="assistant" sign="" current={current}>
                {/* `renderMarkdown` (`../markdown.ts`) is marked.parse, now with a `code` renderer
                    that runs highlight.js against nvim's own syntax colours, then DOMPurify.sanitize
                    -- one function so no caller can run half of it. */}
                <div dangerouslySetInnerHTML={{ __html: renderMarkdown(item.text) }} />
              </Row>
            );
          case "tool":
            return (
              <Row key={item.key} kind="tool" sign={toolSign(item.call)} current={current}>
                <div data-awaiting-permission={awaitingPermission.has(item.call.toolUseId) ? "true" : undefined}>
                  {renderToolCall(item.call, expanded[item.key] === true)}
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
              </Row>
            );
          case "permission":
            return (
              <Row key={item.key} kind="permission" sign="!" current={current}>
                <PermissionCard request={item.request} sessionEnded={sessionEnded} onAnswer={onAnswerPermission} />
              </Row>
            );
          default: {
            // Exhaustiveness guard. Without it, a fifth `TimelineItem` kind gaining no case here
            // renders as nothing: this `.map()` callback has no pinned return type, a missing case
            // falls through to `undefined`, `undefined` is a valid `ReactNode`, and `tsc -b` stays
            // clean -- exactly how the "prompt" case's own placeholder was allowed to hide for a
            // whole task (see the git history of this switch). Assigning `item` to `never` turns
            // that same mistake into a compile error instead of an invisible blank row.
            const _exhaustive: never = item;
            return _exhaustive;
          }
        }
      })}
      <div ref={bottomRef} />
    </div>
  );
}
