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

export function MessageList({ state, sessionEnded, expanded, cursor, onAnswerPermission }: Props) {
  const bottomRef = useRef<HTMLDivElement>(null);

  // `userPrompts` joined this list when prompts became a timeline item: without it, submitting a
  // message did not scroll to it at all (this effect only re-ran on the OTHER three collections
  // changing, and a fresh prompt with no reply yet touches none of them). The brief that first
  // wrote this effect predates prompts being a row and said to keep it exactly as it was --
  // correct then, wrong once `userPrompts` became one more thing that can grow this list.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [state.userPrompts.length, state.transcript.length, state.toolCalls.length, state.pendingPermissions.length]);

  /* Which tool calls are blocked on a decision the user has not made yet. A turn can have several
     calls of the same tool in flight, so "Bash is waiting" identifies nothing on its own -- this is
     the whole reason `tool_use_id` is carried on a permission request.

     `isUsableLink` (`../timeline`) is the one predicate deciding which ids can identify a call at
     all -- the two that cannot, `null` and `""`, and why each is real, are written out there. It
     used to be re-derived here and in `PermissionCard`, three copies each carrying a paragraph
     saying the other two guarded the identical case; a shared guard cannot drift the way three
     hand-written ones can.

     The consequence, stated rather than left to be worked out: on the legacy backend EVERY
     permission carries null, so this set is always empty and the marker below never renders at all.
     That is correct behaviour, not a defect -- but it is unobservable on the DEFAULT backend, which
     is what a source build still starts on. `shell/MANUAL_VERIFICATION.md`'s GUI checklist says the
     same thing, because someone looking for it on legacy would otherwise file a bug. (It was seen
     on the sidecar backend on 2026-09-15, when that path was driven on a screen for the first
     time.) */
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
    <div className="message-list">
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
