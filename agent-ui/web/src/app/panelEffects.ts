import { useEffect, useLayoutEffect, useRef } from "react";
import type { Dispatch, MutableRefObject, RefObject, SetStateAction } from "react";
import { buildDisplay, indexOfKey } from "../display";
import { isListScrollAnimating, settleListScroll } from "../jkScroll";
import type { PanelMode } from "../keymap";
import { conversationRows } from "../nav";
import { buildTimeline, type TimelineItem } from "../timeline";
import type { AgentUiState, ThinkingDisplay } from "../types";
import type { HistoryPage } from "../historyPage";
import type { BuiltSelection } from "../visual";
import { clampCursorToView, enterBoxAtNearEnd, revealRow } from "./rowScroll";
import type { ArrivalPark, FrozenSnapshot } from "./rowScroll";

/** Keep expansions of surviving rows, including runs and children currently folded out of view. */
export function pruneExpanded(state: AgentUiState, expanded: Record<string, boolean>, thinking: ThinkingDisplay = "collapsed"): Record<string, boolean> {
  const raw = buildTimeline(state);
  const keys = new Set([
    ...buildDisplay(raw, { expanded: {}, detailed: false, turnRunning: state.activeTurnId !== null, thinking }),
    ...buildDisplay(raw, { expanded, detailed: true, turnRunning: state.activeTurnId !== null, thinking }),
  ].map((row) => row.key));
  return Object.fromEntries(Object.entries(expanded).filter(([key]) => keys.has(key)));
}

/* The cursor is an index into `timeline`, which shrinks on its own -- a permission card resolving
   removes one row without anything here asking for it. Without this, a cursor left pointing past
   the new end copies/expands nothing (`timeline[cursor]` is `undefined`) rather than sliding onto
   the new last row, which is what every other list-with-a-cursor does when its tail disappears. */
export function useClampCursorToTimeline(
  setCursor: Dispatch<SetStateAction<number>>,
  timeline: TimelineItem[],
) {
  useEffect(() => {
    setCursor((c) => Math.min(c, Math.max(timeline.length - 1, 0)));
  }, [timeline.length]);
}

/** A run collapsing or expanding shifts every index after it, the same shrink-by-something-
 *  other-than-a-key the effect above exists for -- but here the row the cursor was ON is usually
 *  still present, just folded into a run or unfolded back out of one, so sliding onto a nearby
 *  index (what the effect above does for a row that is genuinely GONE) would move the cursor off a
 *  row that still exists. `cursorKeyRef` remembers the KEY the cursor sat on across the render that
 *  changed `timeline`; if that key is not where the index now points, `indexOfKey` finds it again
 *  (a `t-<seq>` folded into a run is found at the run's own key) and the cursor follows it there,
 *  landing rather than scrolling (`landingRef.current = "keep"`, the same convention every other
 *  cursor-preserving move in this file uses). A key that has genuinely left the timeline (a
 *  resolved permission) falls through to the effect above instead.
 *
 *  **Why this does not fire on `cursor` alone: the simpler version failed 47 tests.** That version fires this reconciliation on `cursor` alone changing too (its own
 *  dependency array is `[timeline, cursor]` with no guard distinguishing the two), which means an
 *  ORDINARY `j`/`k`/`gg`/`G`/search move -- `timeline` unchanged, only `cursor` moves -- reads as
 *  "the row `want` pointed at relocated", finds that same row still sitting at its OLD index (since
 *  nothing structural changed), and calls `setCursor` right back to where the user just moved away
 *  from. Every keyboard-driven cursor move in the suite failed this way. `prevTimelineRef` below is
 *  the fix: the reconciliation only runs when `timeline`'s own object identity changed since the
 *  last time this effect ran (a real collapse/expand/tab-switch), never merely because `cursor` did. */
/* **Correction (a GUI pass, 2026-09-25): and only when `cursor` did NOT change.**
   `timeline` is a new object on every streamed delta, so a cursor move committed in the same render
   as one -- the scroll clamp, a `j`/`k`, a restore -- met `timelineChanged`, found the key it had just left
   still at its old index, and was put straight back. The move back carried no `"keep"` (the slot
   was spent on the move it undid), so the `[cursor]` effect revealed the stale row, scrolling the
   list up inside a restore's steering window, and `MessageList` stopped following: a tab switched
   back to mid-stream came back parked above the end with a pill. A cursor that moved in this render
   was moved on purpose; only a timeline change under a still cursor is a fold to follow. */
export function usePreserveCursorRow(
  prevTimelineRef: MutableRefObject<TimelineItem[] | null>,
  timeline: TimelineItem[],
  prevCursorRef: MutableRefObject<number>,
  cursor: number,
  cursorKeyRef: MutableRefObject<string | null>,
  landingRef: MutableRefObject<1 | -1 | 0 | "keep">,
  setCursor: Dispatch<SetStateAction<number>>,
  evictionBoundary?: number,
  tab?: number | null,
  historyPage?: HistoryPage | null,
) {
  const previousEviction = useRef({ boundary: evictionBoundary, tab, historyPage });
  useLayoutEffect(() => {
    const evicted = evictionBoundary !== undefined && previousEviction.current.tab === tab
      && previousEviction.current.boundary !== evictionBoundary;
    const historyReplaced = previousEviction.current.tab === tab
      && previousEviction.current.historyPage != null && previousEviction.current.historyPage !== historyPage;
    previousEviction.current = { boundary: evictionBoundary, tab, historyPage };
    const timelineChanged = prevTimelineRef.current !== null && prevTimelineRef.current !== timeline;
    const cursorMoved = prevCursorRef.current !== cursor;
    prevTimelineRef.current = timeline;
    prevCursorRef.current = cursor;
    const want = cursorKeyRef.current;
    if (timelineChanged && !cursorMoved && want !== null && timeline[cursor]?.key !== want) {
      const found = indexOfKey(timeline, want);
      if (found === null && (evicted || (want.startsWith("h:") && historyReplaced))) {
        landingRef.current = "keep";
        setCursor(0);
        cursorKeyRef.current = timeline[0]?.key ?? null;
        return;
      }
      if (found !== null && found !== cursor) {
        landingRef.current = "keep";
        setCursor(found);
        return;
      }
    }
    cursorKeyRef.current = timeline[cursor]?.key ?? null;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline, cursor, evictionBoundary, tab, historyPage]);
}

/** Keeps the row the cursor sits on inside the viewport whenever the cursor moves. Before this,
 *  `j`/`k` moved an invisible highlight once it passed the bottom of the message list -- which
 *  reads exactly like the key doing nothing (seen on an installed build: `j` could not move down
 *  inside a long output). `"nearest"` is the least-jarring `ScrollLogicalPosition`: a row
 *  already fully on screen does not move at all, unlike `"start"`/`"center"`, which would shove
 *  the viewport around on every single step even when nothing needed to move.
 *
 *  Reads through `containerRef` -- already queried above for focus -- rather than a second ref
 *  into `MessageList`'s own DOM, since every row is a descendant of it. The row is the cursor's
 *  own (`conversationRows`), never the first `.row-current` in the DOM: a model reply's HTML could
 *  draw one of those above the real row.
 *
 *  Kept from fighting `MessageList`'s own follow-the-newest-message effect (`MessageList.tsx`,
 *  the `bottomRef` effect -- the GUI pass, 2026-09-24: there is no `bottomRef` any more; the follow
 *  is a snap to the end, from that effect and from its `ResizeObserver`, and gated the same way)
 *  by gating THAT effect on the message list's own scroll position
 *  rather than on this cursor -- see its doc comment. This effect never needs to check anything
 *  about that one: it only ever moves the viewport the minimum amount to reveal one row, so if
 *  the other effect already put the tail in view, this is a no-op, and if the user is reading
 *  further up, this is the only one of the two still allowed to move anything.
 *
 *  jsdom implements no layout and has no `scrollIntoView` on `Element` at all (this file's own
 *  `beforeAll` stubs it for the same reason `MessageList.test.tsx`'s does), so a jsdom test can
 *  only assert that this was CALLED on the right element -- never that the row actually ends up
 *  on screen. That is a GUI check nobody has run yet. */
export function useRevealCursorRow(
  landingRef: MutableRefObject<1 | -1 | 0 | "keep">,
  landingAnimateRef: MutableRefObject<boolean>,
  landingHeadRef: MutableRefObject<boolean>,
  frozenSnapshotRef: MutableRefObject<FrozenSnapshot | null>,
  containerRef: RefObject<HTMLDivElement>,
  cursor: number,
) {
  useEffect(() => {
    const landing = landingRef.current;
    const animate = landingAnimateRef.current;
    const head = landingHeadRef.current;
    landingRef.current = 0;
    landingAnimateRef.current = false;
    landingHeadRef.current = false;
    if (landing === "keep") return;
    // While VISUAL is on the list on screen is the frozen one,
    // and `cursor` indexes the LIVE timeline -- a prompt sent at a turn's end moved it to a row the
    // frozen list does not have, and this used to scroll whichever frozen row sat at that index into
    // view under the selection. VISUAL moves the view only for its own caret; the row cursor is
    // placed again, by key, when VISUAL ends (`landCursorOnRowKey`).
    if (frozenSnapshotRef.current !== null) return;
    const root = containerRef.current;
    const row = root === null ? undefined : conversationRows(root)[cursor];
    if (row === undefined) return;
    const list = row.closest<HTMLElement>(".message-list");
    if (list !== null) settleListScroll(list);
    if (landing !== 0) enterBoxAtNearEnd(row, landing);
    revealRow(list, row, landing, animate, head);
  }, [cursor]);
}

/** A scroll the panel did not cause by moving the cursor -- a wheel, a drag, the browser's own
 *  scroll keys -- can leave the cursor's row off screen. Whenever that happens, the cursor moves to
 *  the nearest visible row; the view itself is never touched here (`clampCursorToView` above never
 *  writes `scrollTop`). `sessionStarted` mirrors the conversation layout's own
 *  mount condition, so this attaches only while `.message-list` actually exists. */
export function useClampCursorOnScroll(
  containerRef: RefObject<HTMLDivElement>,
  frozenSnapshotRef: MutableRefObject<FrozenSnapshot | null>,
  cursorRefForScroll: MutableRefObject<number>,
  landingRef: MutableRefObject<1 | -1 | 0 | "keep">,
  setCursor: Dispatch<SetStateAction<number>>,
  arrivalParkRef: MutableRefObject<ArrivalPark | null>,
  sessionStarted: boolean,
) {
  useEffect(() => {
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    if (!list) return;
    const onListScroll = () => {
      // The rows below are the FROZEN list's while VISUAL is on,
      // and the index this writes back would name a live row by a frozen position -- the two differ
      // once a row arrives. VISUAL's own caret scrolls the list; the row cursor is placed again,
      // by key, when VISUAL ends.
      if (frozenSnapshotRef.current !== null) return;
      // While a `j`/`k` eases the view toward its target the cursor's row, still on its way in, can be
      // wholly out of view for a frame; the target has it in view, so nothing is re-homed until it lands.
      if (isListScrollAnimating(list)) return;
      const rows = conversationRows(list);
      const next = clampCursorToView(list, rows, cursorRefForScroll.current);
      if (next !== null && next !== cursorRefForScroll.current) {
        landingRef.current = "keep";
        setCursor(next);
      }
    };
    // A wheel or a touch drag on the list is the reader choosing a new place, keys or not; the
    // next arrival must not undo it by restoring the parked one.
    const dropPark = () => {
      arrivalParkRef.current = null;
    };
    list.addEventListener("scroll", onListScroll, { passive: true });
    list.addEventListener("wheel", dropPark, { passive: true });
    list.addEventListener("touchmove", dropPark, { passive: true });
    return () => {
      list.removeEventListener("scroll", onListScroll);
      list.removeEventListener("wheel", dropPark);
      list.removeEventListener("touchmove", dropPark);
    };
  }, [sessionStarted]);
}

/** At the bottom, the cursor rides the new last row as the conversation grows, and a sent
 *  prompt takes the cursor outright. Keyed on `timeline`, since a plain `state` change (a card
 *  answered, a tool result arriving) must not move the cursor when the timeline itself did not
 *  grow. `landOnPromptRef` is set by the `events` dispatch arm the moment a `user_prompt_submitted`
 *  arrives in the batch; it is read and cleared here rather than in the dispatcher, since the row
 *  it targets does not exist until this render's `timeline` reflects it. */
/* A layout effect since a GUI pass (2026-09-25). As a passive effect it ran after
   the browser's next rendering step, which is where the list's scroll event -- from `MessageList`'s
   own snap to the end -- is dispatched; the scroll clamp therefore saw the cursor still on the row that
   WAS last, off screen once a row taller than the view had arrived, and moved it to the nearest
   visible row, which is not always the last one. The ride then no longer matched (`cursor` was no
   longer `previous - 1`), and a following view kept its cursor a row above the end for the rest
   of the reply. `MessageList`'s snap is a layout effect of a child, so it has already run here and
   the distance read below is the snapped one. */
export function useRideLastRow(
  lastLengthRef: MutableRefObject<number>,
  timeline: TimelineItem[],
  landOnLastRef: MutableRefObject<boolean>,
  cursor: number,
  landOnPromptRef: MutableRefObject<boolean>,
  landingRef: MutableRefObject<1 | -1 | 0 | "keep">,
  setCursor: Dispatch<SetStateAction<number>>,
  containerRef: RefObject<HTMLDivElement>,
) {
  const previousTail = useRef<string>();
  useLayoutEffect(() => {
    const previous = lastLengthRef.current;
    const prepended = timeline.length > previous && timeline[0]?.key.startsWith("h:") && previousTail.current === timeline.at(-1)?.key;
    previousTail.current = timeline.at(-1)?.key;
    lastLengthRef.current = timeline.length;
    if (landOnLastRef.current) {
      landOnLastRef.current = false;
      const last = Math.max(timeline.length - 1, 0);
      // The view is already at the end, so the landing moves nothing -- and `"keep"` only for a
      // cursor that really moves (a `"keep"` no `[cursor]` effect consumes swallows the next reveal).
      if (last !== cursor && !landOnPromptRef.current) {
        landingRef.current = "keep";
        setCursor(last);
        return;
      }
    }
    if (landOnPromptRef.current) {
      landOnPromptRef.current = false;
      for (let i = timeline.length - 1; i >= 0; i--) {
        if (timeline[i].kind === "prompt") {
          setCursor(i);
          return;
        }
      }
    }
    if (timeline.length <= previous || cursor !== previous - 1 || prepended) return;
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    const atBottom = !list || list.scrollHeight - list.scrollTop - list.clientHeight <= 1;
    if (atBottom) {
      landingRef.current = "keep";
      setCursor(timeline.length - 1);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline]);
}

/* A brand-new tab's own arrival (`enter_input`'s only sender now), or a HINT landing on the composer: INPUT with a
   blinking caret. Refused under the same condition `i` is (`resolveKey`): a dead session has no box to type into,
   and no session at all has no composer. Keyed on the request alone, so a session change never opens the composer
   by itself. There is no card to check for here any more -- a tab this fresh has none, and every arrival that COULD
   find one waiting now sends `arrive` instead (the effect just below). */
export function useEnterInput(
  inputRequest: number,
  overlayOpen: boolean,
  takeKeys: () => void,
  sessionStarted: boolean,
  sessionEnded: boolean,
  setMode: Dispatch<SetStateAction<PanelMode>>,
  setComposerCaret: Dispatch<SetStateAction<"kept" | "end">>,
  setComposerFocusRequest: Dispatch<SetStateAction<number>>,
) {
  useEffect(() => {
    if (inputRequest === 0) return;
    // An overlay drawn over the conversation must keep the keys rather than let
    // this open a composer underneath it.
    if (overlayOpen) {
      takeKeys();
      return;
    }
    if (!sessionStarted || sessionEnded) return;
    setMode("input");
    // Requests that open the live composer restore its kept caret, matching i, o and Ctrl+j.
    // A selects the end through a separate focus request, so its caret choice must not carry
    // over into a later request to resume typing at the saved position.
    setComposerCaret("kept");
    setComposerFocusRequest((n) => n + 1);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inputRequest]);
}

/** The freeze's defence in depth (at first not built at all -- a test was written to assert
 *  "the observer silent", with no observer to be silent). `MessageList`
 *  reading `frozenSnapshot` (the freeze's PRIMARY mechanism, above) already keeps its own subtree still
 *  while VISUAL is on; this watches `.message-list` anyway, for whatever that primary mechanism
 *  does not cover -- a bug in the freeze itself, or some other code path writing into the DOM
 *  VISUAL is holding a selection in. A `childList`/`characterData` change touching either end of
 *  the selection VISUAL built ends it with the same flash as a live check at `y`-time
 *  finding the selection no longer matches. Deliberately narrow (only the two ENDS, not "anything
 *  in the subtree changed"): a delta landing on some other, unrelated row must not interrupt a
 *  read of a different one, and the mode does not end "on a card arriving or a reply
 *  streaming" by itself. */
export function useWatchFrozenSelection(
  isRegionMode: (m: PanelMode) => m is "caret" | "visual" | "vline",
  mode: PanelMode,
  messageListEl: () => HTMLElement | null,
  visualBuiltRef: MutableRefObject<BuiltSelection | null>,
  showFlash: (text: string) => void,
  regionModeName: (m: PanelMode) => string,
  modeRef: MutableRefObject<PanelMode>,
  exitRegion: () => void,
) {
  useEffect(() => {
    if (!isRegionMode(mode)) return;
    if (typeof MutationObserver === "undefined") return;
    const list = messageListEl();
    if (list === null) return;
    const observer = new MutationObserver((mutations) => {
      const built = visualBuiltRef.current;
      if (built === null) return;
      const touches = (node: Node) =>
        node === built.anchorNode || node === built.focusNode || node.contains(built.anchorNode) || node.contains(built.focusNode);
      for (const mutation of mutations) {
        // `characterData`'s own `target` IS the changed text node -- typically the anchor/focus
        // node itself, since both are text positions. `childList`'s `target` is the still-attached
        // PARENT whose children changed, which stays true (and so keeps `touches` reporting a hit)
        // for content inserted nearby; a REMOVAL is the case that check alone would miss, because a
        // detached anchor/focus's old parent no longer contains it once the removal has happened --
        // `removedNodes` is checked for exactly that.
        if (touches(mutation.target) || Array.from(mutation.removedNodes).some(touches)) {
          // Names the mode that ended, as `vend` and the
          // Shift+Tab refusal do -- this said VISUAL while CARET was on.
          showFlash(`the conversation changed under it — ${regionModeName(modeRef.current)} ended; v to start again`);
          exitRegion();
          return;
        }
      }
    });
    observer.observe(list, { childList: true, characterData: true, subtree: true });
    return () => observer.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);
}
