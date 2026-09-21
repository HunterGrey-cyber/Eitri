import { useEffect, useMemo, useRef } from "react";
import type { AgentUiState, ToolCallRecord } from "../types";
import { renderToolCall } from "../toolRegistry";
import { buildTimeline, isUsableLink } from "../timeline";
import { renderMarkdown } from "../markdown";
import { HistoryNotice } from "./HistoryNotice";
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
  /** Whether the panel has the keyboard (the `pane_focus` envelope). Drives the cursor's solid
   *  and hollow states in `index.css`; see the `.row-current .row-sign` rule there. Defaults to
   *  `true`, the state with no host to say otherwise; `App.tsx` always passes it explicitly. */
  focused?: boolean;
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

/** Within this of the true bottom counts as AT it, whatever direction the last scroll went: sub-pixel
 *  rounding, and the browser clamping `scrollTop` when content below shrinks. Smaller than any `k`
 *  step (three lines), so a deliberate scroll up can never read as "at the bottom". */
const AT_BOTTOM_PX = 1;

export function MessageList({ state, sessionEnded, expanded, cursor, focused = true, onAnswerPermission }: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  // `userPrompts` joined this list when prompts became a timeline item: without it, submitting a
  // message did not scroll to it at all (this effect only re-ran on the OTHER three collections
  // changing, and a fresh prompt with no reply yet touches none of them). The brief that first
  // wrote this effect predates prompts being a row and said to keep it exactly as it was --
  // correct then, wrong once `userPrompts` became one more thing that can grow this list.
  //
  // **Guarded on whether the user has scrolled away from the bottom, since 2026-09-19.**
  // Unconditional, this fought `App.tsx`'s cursor-follow effect (`.row-current`'s own
  // `scrollIntoView`) whenever the user had used `j`/`k` to read something further up and a NEW ROW
  // arrived -- a tool call, a permission card, a new assistant message: the viewport snapped back
  // down mid-read. Half of the owner's "j无法在长输出内部下滑" report, see the dated record.
  //
  // Correction (later the same day, from review): an earlier version of this comment said "every
  // new delta" snapped the view. That was wrong. Streamed deltas are merged into ONE transcript
  // entry (`reducer.ts`, `content_delta`), and this effect is keyed only on the four collections'
  // LENGTHS, so it never ran on a delta at all. The flip side, not fixed here and written down so it
  // is not mistaken for fixed: **a long reply growing while it streams is never followed** -- not
  // before this guard, not after it. Only a new row triggers a follow.
  //
  // Correction (same review): the first guard measured distance-to-bottom INSIDE this effect, which
  // runs after the new row is already in the DOM. Any single row taller than
  // `BOTTOM_FOLLOW_THRESHOLD_PX` therefore made a user sitting exactly at the bottom read as
  // "scrolled away", and following stopped for good. The decision is now made from SCROLL EVENTS,
  // which content growth does not produce: `followingRef` turns off only when the list's `scrollTop`
  // goes DOWN (the user -- wheel, `k`, or the cursor-follow effect moving up -- scrolled toward the
  // top), and turns back on whenever a scroll event finds the view within the threshold of the
  // bottom. A smooth scroll toward the bottom only ever increases `scrollTop`, so this effect's own
  // animation cannot switch following off mid-flight.
  //
  // It still reads the list's scroll position rather than the cursor index on purpose: a great
  // many people never touch `j`/`k` at all and still expect ordinary chat-style auto-follow, and
  // gating on the cursor (which defaults to 0 and stays there for exactly those people) would have
  // silently broken that for them. jsdom implements no layout and fires no scroll event on its own,
  // so every test that does not dispatch one sees `followingRef` at its initial `true`;
  // `MessageList.test.tsx`'s "auto-follow" tests dispatch them. **Not looked at on a screen.**
  const followingRef = useRef(true);
  const lastScrollTopRef = useRef(0);
  const onScroll = () => {
    const list = listRef.current;
    if (list === null) return;
    // The order is the fix for a review finding: the threshold used to be checked FIRST, so a `k`
    // step (or a short wheel scroll up) that ended within the threshold of the bottom turned
    // following straight back ON and the next streamed delta snapped the view back down -- the `k`
    // looked eaten. Now a scroll UP always stops following, unless it left the view at the TRUE
    // bottom (within `AT_BOTTOM_PX`): that is not the user moving up but the browser clamping
    // `scrollTop` after content shrank (a resolved permission card's row going away), and a user at
    // the bottom must stay followed through that. The threshold only re-arms following on a scroll
    // that went DOWN -- not on the effect's own synchronous call below, where nothing moved and only
    // the content changed, or a `k` that ended near the bottom would be re-armed by the next row.
    const distance = list.scrollHeight - list.scrollTop - list.clientHeight;
    if (distance <= AT_BOTTOM_PX) {
      followingRef.current = true;
    } else if (list.scrollTop < lastScrollTopRef.current) {
      followingRef.current = false;
    } else if (list.scrollTop > lastScrollTopRef.current && distance <= BOTTOM_FOLLOW_THRESHOLD_PX) {
      followingRef.current = true;
    }
    lastScrollTopRef.current = list.scrollTop;
  };
  //
  // **A reply growing while it streams is followed too, since 2026-09-19** -- the "flip side" above
  // is closed. The owner's panel design: with the view at the bottom, new streamed text stays in
  // view; `k` or a scroll up stops that, as it always stopped the new-row follow. This effect now
  // runs on every `state` (the reducer replaces it on each folded event) and tells the two cases
  // apart by the four lengths: a new row keeps the smooth `scrollIntoView` it always had; growth
  // inside an existing row snaps `scrollTop` to the end instead, because a smooth animation
  // restarted on every 33ms pump batch would never finish.
  //
  // `onScroll()` is called first, synchronously: a browser delivers scroll events at the next
  // frame, so a `k` that scrolled up a moment ago may not have been seen yet, and without this the
  // next delta would snap the view straight back down under the user. **Not looked at on a screen.**
  const rowCountsRef = useRef<string | null>(null);
  useEffect(() => {
    onScroll();
    const counts = `${state.userPrompts.length}/${state.transcript.length}/${state.toolCalls.length}/${state.pendingPermissions.length}`;
    const newRow = counts !== rowCountsRef.current;
    rowCountsRef.current = counts;
    if (!followingRef.current) return;
    if (newRow) {
      bottomRef.current?.scrollIntoView({ behavior: "smooth" });
      return;
    }
    const list = listRef.current;
    if (list !== null && list.scrollHeight - list.scrollTop - list.clientHeight > 0) {
      list.scrollTop = list.scrollHeight;
      // Record where the snap put the view. Found in review: this ref kept the PRE-snap value, so
      // a `k` pressed before the snap's own scroll event arrived ended above the snap but not above
      // the stale value, was not seen as a scroll up, and the next delta snapped the view back.
      lastScrollTopRef.current = list.scrollTop;
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);

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
    <div className="message-list" data-focused={String(focused)} ref={listRef} onScroll={onScroll}>
      {/* The top of the list, above the first conversation item and inside the scroll box with it:
          it describes where everything below came from, so it belongs at the head of that document
          rather than pinned over it. It is not a `TimelineItem` and holds no `seq` -- see
          `HistoryNotice` for why it must never become a row. `null` for every fresh session, which
          is most of them. */}
      {state.history !== null && <HistoryNotice notice={state.history} />}
      {timeline.map((item, index) => {
        const current = index === cursor;
        switch (item.kind) {
          case "prompt":
            // Plain text, never markdown: this is what the user typed, and parsing it would render
            // their literal backticks and asterisks as formatting they did not ask for.
            return (
              <Row key={item.key} kind="prompt" sign="›" current={current} navStop="row">
                {item.text}
              </Row>
            );
          case "message":
            return (
              <Row key={item.key} kind="assistant" sign="" current={current} navStop="row">
                {/* `renderMarkdown` (`../markdown.ts`) is marked.parse, now with a `code` renderer
                    that runs highlight.js against nvim's own syntax colours, then DOMPurify.sanitize
                    -- one function so no caller can run half of it. */}
                <div dangerouslySetInnerHTML={{ __html: renderMarkdown(item.text) }} />
              </Row>
            );
          case "tool":
            return (
              <Row key={item.key} kind="tool" sign={toolSign(item.call)} current={current} navStop="row">
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
              <Row key={item.key} kind="permission" sign="!" current={current} navStop="row">
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
