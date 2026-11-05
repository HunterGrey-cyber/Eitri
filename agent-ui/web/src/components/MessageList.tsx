import { rowCardLine, rowAgentCard, isAgentCall } from "../display";
import { Fragment, memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactElement } from "react";
import type { AgentUiState, ThinkingBlock, ThinkingDisplay, ToolCallRecord } from "../types";
import { renderToolCall } from "../toolRegistry";
import { allCards, buildTimeline, isUsableLink, noticeText, noticeBody, wakeText, questionLines, type TimelineItem } from "../timeline";
import { buildDisplay, runSummary } from "../display";
import { forgetMarkdown, renderMarkdown, streamMarkdown, type StreamedMarkdown } from "../markdown";
import { turnEndingText } from "../turnEnding";
import { endingDuration, NO_TURN_DURATIONS, placeFooters, workedFooters } from "../turnTimes";
import type { TurnDurations } from "../turnTimes";
import { HistoryNotice } from "./HistoryNotice";
import { PermissionCard } from "./PermissionCard";
import { Row } from "./Row";
import { Revealed } from "./Revealed";
import { revealHidden } from "../revealHidden";
import { followSelection } from "../visualPaint";
import type { PermissionDecision } from "../bridge";
import { noteUserScroll, RESUME_FOLLOW_EVENT, USER_SCROLL_EVENT, type UserScrollDirection } from "../follow";
import { pillLabel, pillShown } from "../pill";
import { parsePath } from "../paths";
import type { PathRef } from "../paths";
import { CursorBand, type BandBox } from "../cursorBand";
import { conversationRows } from "../nav";

type Props = {
  /** The conversation's identity: row keys can be shared by different tabs. */
  tab?: number | null;
  state: AgentUiState;
  /** How long each finished turn ran (`turnTimes.ts`): the "Worked for" line under a turn's last row, and
   *  the duration on a turn-ending row. Absent: no turn has a known time. */
  turnDurations?: TurnDurations;
  /** The session is gone. Pending cards stay visible but can no longer submit into it. */
  sessionEnded: boolean;
  /** Which rows are expanded, keyed by the timeline `key`. Owned by `App.tsx` because `Enter` acts
   *  on the cursor, and the cursor is App's -- see `App.tsx`'s `onKeyDown`, `toggle-expand` arm. A
   *  run's own key expanding is what un-collapses it (`display.ts`). */
  expanded: Record<string, boolean>;
  /** The index into THIS component's own `timeline` (below) that `Enter`/`y` act on in `App.tsx`.
   *  Safe to compare by position rather than by identity: both here and there, `timeline` is
   *  `buildDisplay(buildTimeline(state), …)` over the same `state`, `expanded` and `detailed`, a pure
   *  function, so the two agree on what sits at a given index for a given input (`App.tsx` hands
   *  its own in as `timeline`, so in the panel they are the same array). */
  cursor: number;
  /** The display timeline for exactly this `state`, `expanded` and `detailed`, when the caller has
   *  already built it (`App.tsx` builds the same one to drive its cursor). Rebuilding it here doubled
   *  the work of every streamed batch: a merge and sort of the whole conversation. Left out, it is
   *  built here from the same three props. */
  timeline?: TimelineItem[];
  /** `Ctrl+o`: every result shown in full, at wider cuts, with no collapsed runs. Per tab
   *  (`App.tsx`'s `TabViewState.detailed`); defaults to `false`, the state every fresh tab starts
   *  in. */
  detailed?: boolean;
  /** How thinking is shown (`init.lua`'s `agent.thinking`, the `display` envelope). Defaults to
   *  `collapsed`, its default there; `App.tsx` always passes it. */
  thinkingDisplay?: ThinkingDisplay;
  /** Whether the panel has the keyboard (the `pane_focus` envelope). Drives the cursor band's two strengths
   *  in `index.css` (and the sign-cell cursor's solid and hollow states outside the conversation). Defaults to
   *  `true`, the state with no host to say otherwise; `App.tsx` always passes it explicitly. */
  focused?: boolean;
  /** The third button: `permissionId -> "<words> *"`, exactly the rules Rust offered for the
   *  cards currently on screen (`rule_offers`). Absent/no entry means no rule -- the card never
   *  invents its own suggestion. */
  ruleOffers?: Record<string, string>;
  /** The row just yanked, so it can flash (`.row-yanked`). `App.tsx` owns the timer; this only
   *  reads the current key. */
  yankedKey?: string | null;
  onAnswerPermission: (permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) => void;
  /** The cards already answered from this panel, by `permissionId` --
   *  `App.tsx` owns the set, since `a`/`d` answer a card by its id rather than through its buttons.
   *  Absent: none. */
  answeredPermissions?: ReadonlySet<string>;
  /** Each card's reason box as it is typed into, so `App.tsx`'s `d` can send it as the Deny button would. */
  onPermissionReason?: (permissionId: string, reason: string) => void;
  /** A click on a path link or an inline code span that parses as a path opens it -- the same
   *  target `gf` reaches from the keyboard (`App.tsx`'s `open-path` action). Absent, clicks inside
   *  the list do nothing beyond React's own defaults (a permission card's buttons, say). */
  onOpenPath?: (ref: PathRef) => void;
  /** The unread pill, reported upward rather than floated over the last line here (it sits at the
   *  band's right, `↓N` inverted, never over text). `label` is the band's
   *  compact form -- `↓N`, `↓ ⚑` when a card is below the view, or bare `↓` while shown with
   *  nothing counted yet (`pillShown`'s own "far from the bottom, nothing new" case) -- `null` when
   *  the pill should not show at all. `jump` is this call's own `jumpToEnd`, safe to invoke any time
   *  after this fires (it reads `listRef` fresh, not a stale snapshot). `afterSeq` is the current
   *  `seq` threshold itself -- the largest `seq` on screen when following last
   *  stopped, `null` while following -- reported so a caller can save it across a tab switch
   *  (`App.tsx`'s `TabViewState.unseenAfterSeq`) and hand it back as `unseenSeed` below. Called every
   *  time this component recomputes the pill (`updatePill`, below), including with `null` -- the
   *  caller (the band's `unread` fact) is expected to just hold the latest value, the same way
   *  `pill`/`setPill` already do internally. */
  onUnreadChange?: (label: string | null, jump: () => void, afterSeq: number | null) => void;
  /** Seeds the unread threshold back in on a tab switch: a tab switch reuses this
   *  component, so without this the first `updatePill` after a restore falls back to counting from
   *  the CURRENT timeline length, and a row that arrived while the tab was away is never counted --
   *  the defect this prevents. `afterSeq` is the seq threshold to seed (`TabViewState.unseenAfterSeq`
   *  from the tab just switched TO); `tick` distinguishes one seed from the next so a second switch
   *  back to the same `afterSeq` still re-fires the effect. `null` for a tab with no saved threshold
   *  (never parked, or restored at the bottom) -- nothing to seed. */
  unseenSeed?: { afterSeq: number; tick: number } | null;
  /** `"caret"`/`"visual"`/`"vline"` while one is on,
   *  `null`/absent in BROWSE. Drives `data-visual` on the list (`index.css`'s `::selection`/
   *  `.row-current` overrides) -- nothing else here reads it, since the region's own selection is
   *  built directly on the DOM by `App.tsx`, not through props. */
  visual?: "caret" | "visual" | "vline" | null;
};

/** Whether a finished tool call succeeded, failed, is still running, or was abandoned -- the state a
 * row's sign glyph carries. A call with no result yet is never "done with nothing to say"; see
 * `ToolResult` in `toolRegistry.tsx`, which draws the matching distinction in the body.
 *
 * `abandoned` reuses the `·` glyph `App.tsx` already draws for an ended session
 * row (`<Row kind="ended" sign="·" ...>`), rather than inventing a second "this is over" mark. */
function toolSign(call: ToolCallRecord, abandoned: boolean): string {
  if (call.result === null) return abandoned ? "·" : "◐";
  return call.result.isError ? "✗" : "✓";
}

/** A call with no result is abandoned before the restored-history cut, after the session ends,
 *  or outside its active turn. Rendering those cases as running would leave a permanent spinner;
 *  an active call in the live span must still show as running. */
function isAbandonedCall(call: ToolCallRecord, state: Pick<AgentUiState, "history" | "activeTurnId">, sessionEnded: boolean): boolean {
  if (call.result !== null) return false;
  if (sessionEnded) return true;
  if (state.history !== null && call.seq < state.history.uptoSeq) return true;
  // Results belong to the call's turn. Once that turn ends or is replaced, none can arrive.
  // Without a turn id, fall back to whether any turn is active.
  return call.turnId ? call.turnId !== state.activeTurnId : state.activeTurnId === null;
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

/** How long after a wheel notch, a touch drag, a released pointer or one of the panel's own scroll
 *  keys a scroll event still counts as the user's: long enough for WebKit's animated wheel scroll
 *  and a scroll event that lands a frame or two late, short enough that a clamp seconds later is not
 *  mistaken for the user. Arbitrary and not tuned against a real screen. */
const STEER_WINDOW_MS = 300;

/** Which way a key moves the list when `resolveKey` has no action for it and the browser's own
 *  default runs -- PageUp/PageDown, the arrows, Home/End, Space and Shift+Space, pressed with a
 *  control inside the list focused (`hjkl`'s `l`, Tab, a click). `null` for every other key. */
function nativeScrollKeyDirection(event: KeyboardEvent): "up" | "down" | null {
  switch (event.key) {
    case "PageUp":
    case "ArrowUp":
    case "Home":
      return "up";
    case "PageDown":
    case "ArrowDown":
    case "End":
      return "down";
    case " ":
      return event.shiftKey ? "up" : "down";
    default:
      return null;
  }
}

/** A text box: keys typed there move its caret, not the list. */
function isTextEntry(target: EventTarget | null): boolean {
  return target instanceof HTMLElement && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
}

/** Whether an upward wheel or key can scroll the list itself: neither at its top nor when an
 *  inner scroll box can take it. The engine can latch the gesture elsewhere after this prediction,
 *  so `provisionalStopRef` must be taken back if the list never moves up. */
function listTakesUpward(list: HTMLElement, target: EventTarget | null): boolean {
  if (list.scrollTop <= 0) return false;
  for (let el = target instanceof Element ? target : null; el !== null && el !== list; el = el.parentElement) {
    if (el.scrollTop > 0) {
      const { overflowY } = getComputedStyle(el);
      if (overflowY === "auto" || overflowY === "scroll" || overflowY === "overlay") return false;
    }
  }
  return true;
}

/** How far the cursor's band reaches past the column's text on each side, where the margin allows it. */
export const BAND_BLEED_PX = 16;

/** The band's box for a row: the row's own box vertically; horizontally the column the row sits in, widened
 *  by `BAND_BLEED_PX` into the margin each side -- in a narrow panel, where the margin is smaller than that,
 *  the list's whole width, edge to edge (`index.css`, `.cursor-band`). */
export function bandBox(row: HTMLElement, listWidth: number): BandBox {
  const bleed = Math.min(BAND_BLEED_PX, row.offsetLeft);
  const left = row.offsetLeft - bleed;
  return {
    left,
    top: row.offsetTop,
    width: Math.min(listWidth - left, row.offsetWidth + 2 * bleed),
    height: row.offsetHeight,
  };
}

export function MessageList({
  tab = null,
  state,
  turnDurations = NO_TURN_DURATIONS,
  sessionEnded,
  expanded,
  cursor,
  timeline: given,
  detailed = false,
  thinkingDisplay = "collapsed",
  focused = true,
  ruleOffers,
  yankedKey = null,
  onAnswerPermission,
  answeredPermissions,
  onPermissionReason,
  onOpenPath,
  onUnreadChange,
  unseenSeed,
  visual = null,
}: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  /** One observer for the list and its children, and the set it watches; `null` without one (jsdom). */
  const resizeObserverRef = useRef<ResizeObserver | null>(null);
  const observedChildrenRef = useRef<Set<Element>>(new Set());
  /** What the watched children were last found from; a streamed batch leaves all of it as it was. */
  const observedShapeRef = useRef<RowShape | null>(null);
  /** Current `onScroll` for the once-created observer. A stale closure reads an old timeline and
   *  overwrites an approval label when a new card settles its size. Assigned on every render. */
  const onScrollRef = useRef<() => void>(() => {});
  /** Cursor fill: its element, animator and row key. `placeBandRef` keeps placement current for
   *  the once-created observer. */
  const bandElRef = useRef<HTMLDivElement>(null);
  /** VISUAL and V-LINE's selection, drawn by `../visualPaint` while one is on. */
  const paintElRef = useRef<HTMLDivElement>(null);
  const bandRef = useRef<CursorBand | null>(null);
  const bandRowKeyRef = useRef<string | null>(null);
  const bandTabRef = useRef(tab);
  const placeBandRef = useRef<(glide: boolean) => void>(() => {});

  /* Watch both the list and its rows: reflow can leave a following view 482px short after a divider
     drag or 651px after unzooming (measured). A shorter list hides the tail too, even while a card
     waits or a turn is over. Row-only restyles change content height without changing the list box;
     watching just the list misses those. Snap after layout and before paint, leaving parked readers
     and provisional stops alone. Rows use viewport margins, without a measured width or size query
     container: on WebKitGTK 2.52.6 a row size container resets scrollTop on status-clock ticks. */
  useLayoutEffect(() => {
    const list = listRef.current;
    if (list === null) return;
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      // As the follow effect does, read the position first: a scroll whose event has not arrived yet
      // still decides whether this is a view that is following. Through the ref: see `onScrollRef`.
      // A row changed size, or the list did: the cursor's band follows its row at once, before the end is
      // read below, which the band's old extent could otherwise still reach past.
      placeBandRef.current(false);
      onScrollRef.current();
      if (followingRef.current) follow(list);
    });
    observer.observe(list);
    resizeObserverRef.current = observer;
    const observed = observedChildrenRef.current;
    return () => {
      observer.disconnect();
      resizeObserverRef.current = null;
      observed.clear();
      observedShapeRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /** Puts every child of the list under the observer above, and lets go of any that left. Run after
   *  every change of `state` and of the timeline (an expanded run mounts rows that no `state` change
   *  announced), and looks at the DOM only when the rows are not the ones it last found: walking every
   *  row on every streamed batch is a large part of what a batch costs in a long conversation. */
  const observeChildren = (list: HTMLElement) => {
    const observer = resizeObserverRef.current;
    if (observer === null) return;
    const shape: RowShape = { rows: timeline, footers: placedFooters, notice: state.history !== null };
    const held = observedShapeRef.current;
    observedShapeRef.current = shape;
    if (held !== null && sameRowShape(held, shape)) return;
    const observed = observedChildrenRef.current;
    const children = new Set<Element>();
    for (const child of list.children) {
      if (!child.classList.contains("subagent-child")) children.add(child);
      else for (const row of child.children) if (row.classList.contains("row")) children.add(row);
    }
    for (const el of observed) {
      if (!children.has(el)) {
        observer.unobserve(el);
        observed.delete(el);
      }
    }
    for (const el of children) {
      // The band changes size as it glides; it is not content, and is placed from the rows.
      if (el === bandElRef.current || el === paintElRef.current) continue;
      if (!observed.has(el)) {
        observer.observe(el);
        observed.add(el);
      }
    }
  };

  // Following tracks scroll intent, not the cursor: mouse readers expect auto-follow even when
  // the cursor stays at 0. Engine-originated clamps can move scrollTop on a status-clock tick with
  // no content change, so neither direction alone nor "no content changed" identifies user input.
  //
  // A mostly vertical upward wheel/key stops following provisionally only if the list can take it.
  // Native scroll keys from focused controls and focus reveals also steer; keys in text boxes move
  // the caret. The panel announces its own scrolls with `noteUserScroll`; k/Ctrl+u/gg stop outright.
  // A pointer held on the list or an event within `STEER_WINDOW_MS` counts as steering.
  //
  // At the true bottom (`AT_BOTTOM_PX`) following re-arms; a downward user scroll within
  // `BOTTOM_FOLLOW_THRESHOLD_PX` also re-arms it. A pending gesture with no movement cannot re-arm:
  // WebKitGTK can emit a wheel scroll at 0-1ms before motion (measured in 2 of 8 wheels at the tail).
  //
  // Provisional stops prevent a delta between intent and animated motion from snapping over it.
  // If the gesture ends without an upward list move, take the stop back: the engine may have
  // latched onto an inner box. A timer settles even if a card arrives then the stream goes silent;
  // further wheels extend the window, and a held pointer waits until its release window ends.
  //
  // New rows and streamed growth snap before paint: smooth arrival leaves new text below the edge
  // for 1-2 frames (measured), and restarting animation on each 33ms pump batch never finishes.
  // ResizeObserver handles size changes without new state. jsdom has no layout or automatic scroll
  // events; `MessageList.test.tsx` sets dimensions and dispatches them explicitly.

  /* Merge by Rust's seq rather than by item kind, so interleaved calls keep the same order live
     and after snapshots or resync. Memoization avoids allocating mapped arrays, a Map and a Set
     and sorting the whole conversation on unrelated renders during streaming. MarkdownBody
     renders a finished reply once per text, and StreamingMarkdown only the open end of a reply being written.
     Keep this binding above onScroll and updatePill: both closures need the same timeline. */
  const timeline = useMemo(
    () =>
      given ??
      buildDisplay(buildTimeline(state), { expanded, detailed, turnRunning: state.activeTurnId !== null, thinking: thinkingDisplay }),
    [given, state, expanded, detailed, thinkingDisplay],
  );

  const followingRef = useRef(true);
  const lastScrollTopRef = useRef(0);
  const steerUntilRef = useRef(0);
  const pointerSteeringRef = useRef(false);
  /** Where predicted upward scrolling provisionally stopped following; `null` without a pending
   *  stop. Keep it until the gesture confirms movement or settles without moving up. */
  const provisionalStopRef = useRef<{ from: number } | null>(null);
  const settleTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const steering = () => pointerSteeringRef.current || performance.now() <= steerUntilRef.current;
  /** Snap to the newest content, whether a row arrived, grew or resized. Smooth scroll leaves
   *  new text below the edge for frames; see the following model above. */
  const follow = (list: HTMLElement | null) => {
    if (list !== null && list.scrollHeight - list.scrollTop - list.clientHeight > 0) {
      list.scrollTop = list.scrollHeight;
      // Record the post-snap position: a k before the snap's scroll event must compare against
      // that position, or it can go unnoticed and be undone by the next delta.
      lastScrollTopRef.current = list.scrollTop;
    }
  };
  /** Largest `seq` when following stops; `null` while following. An array cutoff misses cards
   *  anchored beside older calls: arrival order and display order differ. A tab switch reuses this
   *  component, so restore the saved seq threshold rather than reset it to the restored length. */
  const unseenAfterSeqRef = useRef<number | null>(null);
  const [pill, setPill] = useState<string | null>(null);
  const updatePill = () => {
    const list = listRef.current;
    if (list === null) return;
    const following = followingRef.current;
    if (following) unseenAfterSeqRef.current = null;
    // The largest `seq` anywhere in the timeline, not the last element's `seq`: an anchored card can
    // sit earlier in the array than an item with a lower `seq` that follows it (the ordering
    // `buildTimeline` deliberately breaks for anchoring), so the tail is not reliably the max. `-1`
    // when the timeline is empty, so every real `seq` (>= 0) still counts as fresh once something
    // does arrive.
    else if (unseenAfterSeqRef.current === null) {
      unseenAfterSeqRef.current = timeline.reduce((max, item) => (item.seq > max ? item.seq : max), -1);
    }
    const distance = list.scrollHeight - list.scrollTop - list.clientHeight;
    const shown = pillShown(pill !== null, following, distance);
    const threshold = unseenAfterSeqRef.current;
    const fresh = threshold === null ? [] : timeline.filter((item) => item.seq > threshold);
    const label = shown ? pillLabel(fresh.length, fresh.some((item) => item.kind === "permission")) : null;
    if (label !== pill) setPill(label);
    // The band's compact form: a card below wins over a count, a
    // count over the bare arrow -- the same priority `pillLabel` already encodes, just short enough
    // for a monospace band instead of the floating pill's full words.
    const cardBelow = fresh.some((item) => item.kind === "permission");
    onUnreadChange?.(shown ? (cardBelow ? "↓ ⚑" : fresh.length > 0 ? `↓${fresh.length}` : "↓") : null, jumpToEnd, threshold);
  };
  const jumpToEnd = () => {
    const list = listRef.current;
    if (list === null) return;
    noteUserScroll(list, "down");
    list.scrollTop = list.scrollHeight;
  };
  /** Run before each follow decision, and when a gesture's window runs out: a provisional stop whose
   *  gesture is over, with the list no higher than where the gesture found it, is taken back.
   *  Returns the stop it took back, `null` if none. */
  const settleProvisionalStop = (list: HTMLElement) => {
    const stop = provisionalStopRef.current;
    if (stop === null || steering()) return null;
    provisionalStopRef.current = null;
    if (list.scrollTop < stop.from - AT_BOTTOM_PX) return null;
    followingRef.current = true;
    return stop;
  };
  /** Settle a pending stop at gesture end even when no next state change arrives. */
  const settleWhenGestureEnds = () => {
    settleTimerRef.current = null;
    const list = listRef.current;
    if (list === null || provisionalStopRef.current === null) return;
    if (steering()) {
      scheduleSettle();
      return;
    }
    if (settleProvisionalStop(list) !== null) follow(list);
  };
  /** Arms (or re-arms) the timer for the end of the current gesture; nothing while no stop is
   *  pending, and nothing while a pointer is held -- its release steers, and calls this again. */
  const scheduleSettle = () => {
    if (settleTimerRef.current !== null) clearTimeout(settleTimerRef.current);
    settleTimerRef.current = null;
    if (provisionalStopRef.current === null || pointerSteeringRef.current) return;
    settleTimerRef.current = setTimeout(settleWhenGestureEnds, Math.max(0, steerUntilRef.current - performance.now()) + 1);
  };
  /** A wheel or key that should move the list up stops following now -- before its scroll arrives,
   *  and before the next delta could snap over it -- but only until the gesture is over. */
  const stopProvisionally = (list: HTMLElement) => {
    if (!followingRef.current) return;
    followingRef.current = false;
    provisionalStopRef.current = { from: list.scrollTop };
    scheduleSettle();
  };
  const onScroll = () => {
    const list = listRef.current;
    if (list === null) return;
    // Check upward movement before the near-bottom threshold: a short k must stop following
    // even inside that threshold, unless it ends at the TRUE bottom (also where content shrinkage
    // clamps the view). The threshold re-arms only on downward scrolling, never this function's
    // synchronous read when nothing moved.
    //
    // A pending gesture with no movement says nothing yet. WebKitGTK's first wheel scroll can
    // arrive at 0-1ms without motion (2 of 8 wheels measured at the tail). Re-arming then lets a
    // delta snap cancel the animation. Keep the gesture timer armed, or a held pointer whose
    // release arms it, so `settleWhenGestureEnds` still resolves the stop.
    const distance = list.scrollHeight - list.scrollTop - list.clientHeight;
    const stop = provisionalStopRef.current;
    const gestureNotYetMoved = stop !== null && Math.abs(list.scrollTop - stop.from) <= AT_BOTTOM_PX;
    if (distance <= AT_BOTTOM_PX) {
      if (!gestureNotYetMoved) {
        followingRef.current = true;
        provisionalStopRef.current = null;
      }
    } else if (steering()) {
      if (list.scrollTop < lastScrollTopRef.current) {
        followingRef.current = false;
        // The list really went up: a provisional stop is now the user's for good.
        provisionalStopRef.current = null;
      } else if (list.scrollTop > lastScrollTopRef.current && distance <= BOTTOM_FOLLOW_THRESHOLD_PX) {
        followingRef.current = true;
        provisionalStopRef.current = null;
      }
    }
    lastScrollTopRef.current = list.scrollTop;
    updatePill();
  };
  onScrollRef.current = onScroll;

  // The intent signals, attached once to the list itself. Passive: none of them cancels anything.
  useLayoutEffect(() => {
    const list = listRef.current;
    if (list === null) return;
    const steer = () => {
      steerUntilRef.current = performance.now() + STEER_WINDOW_MS;
    };
    // Only a mostly vertical wheel up that the list itself can take stops following, and only
    // provisionally. A sideways swipe's jitter, a wheel an inner box takes and a wheel with nothing
    // above only steer: if the list does move, its scroll event says so.
    const onWheel = (event: WheelEvent) => {
      steer();
      if (event.deltaY < 0 && Math.abs(event.deltaY) > Math.abs(event.deltaX) && listTakesUpward(list, event.target)) {
        stopProvisionally(list);
      }
    };
    // The browser's own scroll keys, from a control inside the list. In a text box they move the
    // caret, so they only steer there (a caret reveal can scroll the list, either way).
    const onKeyDown = (event: KeyboardEvent) => {
      const direction = nativeScrollKeyDirection(event);
      if (direction === null) return;
      steer();
      if (direction === "up" && !isTextEntry(event.target) && listTakesUpward(list, event.target)) {
        stopProvisionally(list);
      }
    };
    const onPointerDown = () => {
      pointerSteeringRef.current = true;
    };
    const onPointerUp = () => {
      if (!pointerSteeringRef.current) return;
      pointerSteeringRef.current = false;
      // Scroll events still in flight from a scrollbar drag or a selection autoscroll are the user's.
      steer();
      // Pointer release starts a new window for the drag's pending scroll events, then settles the stop.
      scheduleSettle();
    };
    const onUserScroll = (event: Event) => {
      if ((event as CustomEvent<UserScrollDirection>).detail === "up") {
        // Outright, not provisional: `k`, `Ctrl+u` and `gg` stop following by design.
        followingRef.current = false;
        provisionalStopRef.current = null;
      }
      steer();
    };
    // The user sent a message (`../follow.ts`, `resumeFollowing`): follow again, wherever the view
    // was, and show the end now -- a pending provisional stop or its timer included. A later scroll
    // up stops following exactly as before.
    const onResume = () => {
      followingRef.current = true;
      provisionalStopRef.current = null;
      if (settleTimerRef.current !== null) clearTimeout(settleTimerRef.current);
      settleTimerRef.current = null;
      follow(list);
      onScrollRef.current();
    };
    list.addEventListener("wheel", onWheel, { passive: true });
    list.addEventListener("keydown", onKeyDown, { passive: true });
    // Focus landing on something inside the list (Tab, a click) scrolls it into view, either way.
    list.addEventListener("focusin", steer, { passive: true });
    list.addEventListener("touchmove", steer, { passive: true });
    // Listen for both pointer and mouse events: whether WebKitGTK sends either for its own
    // scrollbar is not yet checked on a screen. An unseen drag is undone by the next delta.
    list.addEventListener("pointerdown", onPointerDown, { passive: true });
    list.addEventListener("mousedown", onPointerDown, { passive: true });
    list.addEventListener(USER_SCROLL_EVENT, onUserScroll);
    list.addEventListener(RESUME_FOLLOW_EVENT, onResume);
    // On the window, not the list: the pointer is often released somewhere else after a drag.
    window.addEventListener("pointerup", onPointerUp, { passive: true });
    window.addEventListener("pointercancel", onPointerUp, { passive: true });
    window.addEventListener("mouseup", onPointerUp, { passive: true });
    return () => {
      list.removeEventListener("wheel", onWheel);
      list.removeEventListener("keydown", onKeyDown);
      list.removeEventListener("focusin", steer);
      list.removeEventListener("touchmove", steer);
      list.removeEventListener("pointerdown", onPointerDown);
      list.removeEventListener("mousedown", onPointerDown);
      list.removeEventListener(USER_SCROLL_EVENT, onUserScroll);
      list.removeEventListener(RESUME_FOLLOW_EVENT, onResume);
      window.removeEventListener("pointerup", onPointerUp);
      window.removeEventListener("pointercancel", onPointerUp);
      window.removeEventListener("mouseup", onPointerUp);
      if (settleTimerRef.current !== null) clearTimeout(settleTimerRef.current);
      settleTimerRef.current = null;
    };
  }, []);
  // Follow every state change, including streamed growth. Read the scroll position first: a user
  // scroll in the steering window may be ahead of its event. k also stops through `noteUserScroll`.
  // Run before paint: a passive effect leaves freshly streamed lines below the fold for a frame.
  // Measured lag with this timing is 30-50ms; without it, one run saw 255/424ms, but a repeat passed
  // at 73-85ms, so that failure was load-sensitive rather than reliably reproduced.
  // Observe new children too, to follow row-only size changes without another state change.
  useLayoutEffect(() => {
    onScroll();
    const list = listRef.current;
    if (list === null) return;
    settleProvisionalStop(list);
    observeChildren(list);
    if (followingRef.current) follow(list);
    updatePill();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);
  /* The rows are watched after every change of the timeline, not only of `state`: `expanded` is the
     caller's, and `Enter` on a collapsed run puts its calls in rows of their own, which no `state` change
     announced. Left unwatched, the next `Enter` -- on one of those calls, to open its result -- grew the list
     below a following view with nothing to snap it to the end. Observing a row reports its size at once, so
     a following view is snapped to the end for the rows this mounted too. */
  useLayoutEffect(() => {
    const list = listRef.current;
    if (list !== null) observeChildren(list);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline]);

  /** Seeds the unread threshold back in when `App.tsx` restores a parked tab's view.
   *  Declared -- and so run -- AFTER the `[state]` effect above: that effect's own `updatePill()` ran
   *  first, against whatever `unseenAfterSeqRef` already held (stale from the tab just left, or reset
   *  to `null` by `followingRef.current` still being `true` from this component's initial mount
   *  default); this effect's own `updatePill()` call runs after it and is what `onUnreadChange`'s
   *  caller actually ends up holding. Keyed on `unseenSeed?.tick`, not on `unseenSeed` itself, so a
   *  second switch back to a tab whose threshold happens to be numerically the same still re-fires --
   *  see the prop's own doc comment. A LAYOUT effect, and not by accident: `App.tsx`'s own scroll
   *  restore (`useLayoutEffect` on `[restoreTick]`) is what actually moves `list.scrollTop` to the
   *  parked position, and child layout effects run before the parent's -- so this must set
   *  `followingRef.current = false` and the threshold BEFORE that restore runs, or the restore's own
   *  (later, asynchronous) `scroll` event would recompute the threshold itself from whatever the
   *  timeline holds by then, rather than from what `App.tsx` remembered. ASSUMES `if (following)
   *  unseenAfterSeqRef.current = null` above is the only other place this ref is reset -- checked:
   *  `jumpToEnd` does not touch it directly (it relies on the `scroll` event it provokes reaching
   *  `onScroll` -> `updatePill`), and the `[state]` effect above only ever calls `updatePill`, never
   *  writes the ref itself. */
  useLayoutEffect(() => {
    if (unseenSeed == null) return;
    followingRef.current = false;
    unseenAfterSeqRef.current = unseenSeed.afterSeq;
    updatePill();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [unseenSeed?.tick]);

  /* Link waiting decisions to calls by tool_use_id: several calls of one tool can be in flight.
     Share isUsableLink with PermissionCard so null and empty proto3 ids cannot drift between
     callers. Legacy permissions have no id, so this set is empty and the marker never renders. */
  /* The cursor's band goes on the cursor's row, found by structure (`conversationRows`), as every other
     cursor-to-row lookup is. It glides when the cursor moved from a row that is still in the list; a row
     that is gone (a tab switch, a cleared conversation) or a first placement puts it there at once. */
  placeBandRef.current = (glide: boolean) => {
    const list = listRef.current;
    const el = bandElRef.current;
    if (list === null || el === null) return;
    const band = (bandRef.current ??= new CursorBand(el));
    const row = conversationRows(list)[cursor];
    band.place(row === undefined ? null : bandBox(row, list.clientWidth), glide);
  };
  useLayoutEffect(() => {
    if (bandTabRef.current !== tab) {
      bandTabRef.current = tab;
      bandRowKeyRef.current = null;
    }
    const key = timeline[cursor]?.key ?? null;
    const before = bandRowKeyRef.current;
    bandRowKeyRef.current = key;
    const glide = before !== null && key !== null && before !== key && timeline.some((item) => item.key === before);
    placeBandRef.current(glide);
  });
  useEffect(() => () => bandRef.current?.stop(), []);
  // The selection's own drawing follows every change of it while VISUAL or V-LINE is on.
  const visualRef = useRef(visual);
  visualRef.current = visual;
  const painting = visual === "visual" || visual === "vline";
  useEffect(() => {
    const list = listRef.current;
    const layer = paintElRef.current;
    if (!painting || !list || !layer) return;
    return followSelection(list, layer, () => visualRef.current ?? null);
  }, [painting]);

  /** The card callbacks a row calls, through one ref that never changes, so a new callback from the
   *  parent (they are made on every render) does not make every row render again. */
  const rowHandlersRef = useRef<RowHandlers>({ onAnswerPermission, onPermissionReason });
  rowHandlersRef.current = { onAnswerPermission, onPermissionReason };

  // By the transcript's length and newest row, not by the list: a streamed batch replaces the list on every delta and
  // changes which rows there are on none of them.
  const newestReply = state.transcript[state.transcript.length - 1]?.seq;
  const footers = useMemo(
    () => workedFooters(state, turnDurations),
    [state.activeTurnId, state.turnEndings, state.transcript.length, newestReply, state.thinking, state.toolCalls, state.pendingPermissions, state.editorCards, turnDurations],
  );
  // By row key, over the rows actually drawn: a turn's newest row may be a hidden or folded thought.
  const placedFooters = useMemo(() => placeFooters(timeline, footers), [timeline, footers]);

  // The reply the agent is still writing: it draws only what changed (`StreamingMarkdown`).
  const streamingKey = state.assistantMessageOpen && newestReply !== undefined ? `m-${newestReply}` : null;
  const awaitingPermission = new Set(allCards(state).map((p) => p.toolUseId).filter(isUsableLink));
  /** Each row's element, kept with the values it was made from: the same element back tells React the row has not
   *  changed, so a batch does not look at a finished row's props. By item, which outlives no row. */
  const heldRows = useRef(new WeakMap<TimelineItem, HeldRow>()).current;

  return (
    <>
      <div
        className="message-list"
        data-focused={String(focused)}
        data-visual={visual ?? undefined}
        ref={listRef}
        onScroll={onScroll}
        // A click on a path -- the tool registry's own `.path-link` spans, or an inline code
        // span in an assistant reply that happens to parse as one. Never a fenced code BLOCK
        // (`pre code`): that is a real snippet, not a path, and HINT's own `y` already owns copying
        // it.
        onClick={(event) => {
          if (onOpenPath === undefined) return;
          const target = (event.target as HTMLElement).closest<HTMLElement>(".path-link, .row-assistant code:not(pre code)");
          if (target === null) return;
          const ref = parsePath(target.dataset.path ?? target.textContent ?? "");
          if (ref !== null) onOpenPath(ref);
        }}
      >
        {/* The top of the list, above the first conversation item and inside the scroll box with it:
            it describes where everything below came from, so it belongs at the head of that document
            rather than pinned over it. It is not a `TimelineItem` and holds no `seq` -- see
            `HistoryNotice` for why it must never become a row. `null` for every fresh session, which
            is most of them. */}
        {state.history !== null && <HistoryNotice notice={state.history} />}
        {timeline.map((item, index) => {
          // Everything a row draws is in these props, each a value or the timeline's own item, so a
          // cursor move renders the two rows it changes and no other (`TimelineRow`).
          const call = item.kind === "tool" ? item.call : null;
          const permissionId = item.kind === "permission" ? item.request.permissionId : null;
          const footer = placedFooters.get(item.key) ?? null;
          const abandoned = call !== null && isAbandonedCall(call, state, sessionEnded);
          const props: TimelineRowProps = {
            item,
            current: index === cursor,
            yanked: item.key === yankedKey,
            expanded: expanded[item.key] === true,
            detailed,
            thinkingDisplay,
            sessionEnded,
            cardLine: call === null ? null : rowCardLine(call, state),
            agent: call !== null && isAgentCall(call) ? rowAgentCard(call, state, abandoned) : null,
            abandoned,
            gated: call !== null && awaitingPermission.has(call.toolUseId),
            ruleOffer: permissionId === null ? null : (ruleOffers?.[permissionId] ?? null),
            alreadyAnswered: permissionId !== null && (answeredPermissions?.has(permissionId) ?? false),
            handlers: rowHandlersRef,
            ranFor: item.kind === "ending" ? endingDuration(turnDurations, item.ending.turnId) : null,
            streaming: item.key === streamingKey && !item.depth,
            selecting: visual !== null,
          };
          const held = heldRows.get(item);
          if (held !== undefined && held.footer === footer && sameRowProps(held.props, props)) return held.element;
          const row = <TimelineRow {...props} />;
          const element = (
            <Fragment key={item.key}>
              {item.depth ? <div className="subagent-child" style={{ marginInlineStart: `${item.depth * 1.5}em` }} data-depth={item.depth}>{row}</div> : row}
              {footer !== null && (
                // Chrome, not a row: no `data-nav-stop`, so `j`/`k` and `y` never see it, and the inner
                // `aria-hidden` span is what VISUAL's copy leaves out (`VISUAL_CHROME`).
                <div className="turn-time" data-turn-time="">
                  <span aria-hidden="true">{footer}</span>
                </div>
              )}
            </Fragment>
          );
          heldRows.set(item, { props, footer, element });
          return element;
        })}
        {/* The cursor's fill, behind the rows (`../cursorBand`). Hidden from assistive technology and from
            VISUAL's selection (`[aria-hidden="true"]` is chrome there). */}
        <div className="cursor-band" aria-hidden="true" ref={bandElRef} />
        {/* VISUAL and V-LINE's selection (`../visualPaint`), over the rows. */}
        <div className="visual-paint" aria-hidden="true" ref={paintElRef} />
      </div>
    </>
  );
}

type RowHandlers = {
  onAnswerPermission: Props["onAnswerPermission"];
  onPermissionReason: Props["onPermissionReason"];
};

/** What decides which elements are the list's direct children: its rows in order, the rows a turn's time stands under
 *  (by row key), and the history notice. */
type RowShape = { rows: TimelineItem[]; footers: ReadonlyMap<string, string>; notice: boolean };

function sameRowShape(a: RowShape, b: RowShape): boolean {
  if (a.notice !== b.notice || a.rows.length !== b.rows.length || a.footers.size !== b.footers.size) return false;
  for (const key of a.footers.keys()) if (!b.footers.has(key)) return false;
  return a.rows === b.rows || a.rows.every((item, i) => item.key === b.rows[i].key && item.depth === b.rows[i].depth);
}

type HeldRow = { props: TimelineRowProps; footer: string | null; element: ReactElement };

/** Every prop by name, so a prop added to `TimelineRowProps` without a line here fails to compile. */
function sameRowProps(a: TimelineRowProps, b: TimelineRowProps): boolean {
  const {
    item, current, yanked, expanded, detailed, thinkingDisplay, sessionEnded, abandoned, cardLine, agent, gated,
    ruleOffer, alreadyAnswered, ranFor, handlers, streaming, selecting, ...none
  } = a;
  const _exhaustive: Record<string, never> = none;
  void _exhaustive;
  return (
    item === b.item && current === b.current && yanked === b.yanked && expanded === b.expanded &&
    detailed === b.detailed && thinkingDisplay === b.thinkingDisplay && sessionEnded === b.sessionEnded &&
    abandoned === b.abandoned && cardLine === b.cardLine && agent === b.agent && gated === b.gated &&
    ruleOffer === b.ruleOffer && alreadyAnswered === b.alreadyAnswered && ranFor === b.ranFor &&
    handlers === b.handlers && streaming === b.streaming && selecting === b.selecting
  );
}

type TimelineRowProps = {
  item: TimelineItem;
  current: boolean;
  yanked: boolean;
  expanded: boolean;
  detailed: boolean;
  thinkingDisplay: ThinkingDisplay;
  sessionEnded: boolean;
  /** A tool call whose result can never arrive (`isAbandonedCall`). */
  abandoned: boolean;
  cardLine: string | null;
  agent: ReturnType<typeof rowAgentCard> | null;
  /** A tool call a card is waiting on. */
  gated: boolean;
  /** A card's offered rule, exactly as Rust offered it. */
  ruleOffer: string | null;
  alreadyAnswered: boolean;
  /** For a turn-ending row: how long the turn ran, worded (`formatDuration`), or `null` when unknown. */
  ranFor: string | null;
  handlers: { current: RowHandlers };
  /** The reply the agent is still writing. */
  streaming: boolean;
  /** VISUAL holds the list still. */
  selecting: boolean;
};

let rowRenders = 0;
/** How many times a conversation row has rendered in this page (the measurement of `TimelineRow`). */
export function timelineRowRenderCount(): number {
  return rowRenders;
}

/** One conversation row. Memoized on its props, which are values or the timeline's own item: a held
 *  `j`/`k` moves the cursor several rows a second, and rendering every row of a long conversation for
 *  each move cost a frame or more. A streamed batch is the same case: `buildTimeline` hands back a
 *  record's item while the record is unchanged, and `buildDisplay` a run's while its members are, so
 *  a batch renders the one row it wrote to. Anything a row draws must come from its item or from a
 *  prop here, or the row keeps showing what it showed before. */
const TimelineRow = memo(function TimelineRow({
  item,
  current,
  yanked,
  expanded,
  detailed,
  thinkingDisplay,
  sessionEnded,
  abandoned,
  cardLine,
  agent,
  gated,
  ruleOffer,
  alreadyAnswered,
  ranFor,
  handlers,
  streaming,
  selecting,
}: TimelineRowProps) {
  rowRenders += 1;
  switch (item.kind) {
    case "prompt": {
      // Plain text, never markdown: this is what the user typed, and parsing it would render
      // their literal backticks and asterisks as formatting they did not ask for.
      // A message the agent read inside its running turn is part of that turn: `row-prompt-delivered`
      // keeps it from opening a new one on screen (`index.css`).
      const promptClass = [item.delivered ? "row-prompt-delivered" : null, yanked ? "row-yanked" : null]
        .filter((name) => name !== null)
        .join(" ");
      return (
        <Row kind="prompt" sign="›" current={current} className={promptClass || undefined} navStop="row">
          {item.text}
        </Row>
      );
    }
    case "message": {
      if (item.question || item.plain) {
        const lines = item.question ? questionLines(item.question) : item.text.split("\n").map((text, i) => ({ text, muted: i > 0 }));
        return <Row kind="question" sign="?" current={current} className={yanked ? "row-yanked" : undefined} navStop="row">
          {lines.map((line, i) => <div key={i} className={line.muted ? "question-line question-status" : "question-line"}>
            <Revealed pieces={revealHidden(line.text)} />
          </div>)}
        </Row>;
      }
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="assistant" sign="" current={current} className={yankedClass} navStop="row">
          {/* `renderMarkdown` (`../markdown.ts`) is marked.parse, now with a `code` renderer
              that runs highlight.js against nvim's own syntax colours, then DOMPurify.sanitize
              -- one function so no caller can run half of it. */}
          <ReplyBody text={item.text} streaming={streaming} selecting={selecting} />
        </Row>
      );
    }
    case "tool": {
      if (agent) {
        const whole = !agent.background && (expanded || detailed);
        return <Row kind="subagent" sign={toolSign(item.call, abandoned)} current={current} className={yanked ? "row-yanked" : undefined} navStop="row">
          <div className="subagent-heading"><Revealed pieces={revealHidden(agent.header)} />{!agent.background && <span className="fold-marker" aria-label={whole ? "expanded" : "collapsed"}>{whole ? "▾" : "▸"}</span>}</div>
          {agent.warnings.map((warning, i) => <div key={i} className="subagent-warning"><Revealed pieces={revealHidden(warning)} /></div>)}
          <div className="subagent-status"><Revealed pieces={revealHidden(agent.line)} /></div>
          {whole && agent.report && <div className="subagent-report"><Revealed pieces={revealHidden(agent.report)} /></div>}
        </Row>;
      }
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="tool" sign={toolSign(item.call, abandoned)} current={current} className={yankedClass} navStop="row">
          <div data-awaiting-permission={gated ? "true" : undefined}>
            {/* A call a card is waiting on no longer repeats its own invocation below the
                card too -- the gated line above says "waiting for approval" already, so the
                two would otherwise say the same thing twice, once as an offer and once as if
                it had already run. */}
            {renderToolCall(item.call, expanded, {
              gated,
              detailed,
              expanded,
              abandoned,
              cardLine,
            })}
          </div>
        </Row>
      );
    }
    case "permission": {
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="permission" sign="!" current={current} className={yankedClass} navStop="row">
          <PermissionCard
            request={item.request}
            sessionEnded={sessionEnded}
            ruleOffer={ruleOffer}
            alreadyAnswered={alreadyAnswered}
            onAnswer={(...args) => handlers.current.onAnswerPermission(...args)}
            onReasonChange={(id, reason) => handlers.current.onPermissionReason?.(id, reason)}
          />
        </Row>
      );
    }
    case "ending": {
      // Where a turn that did not complete stopped, in plain words (`turnEnding.ts`), muted and
      // signed `·` like the other "this is over" rows. A `j`/`k` stop like every conversation
      // row, so `y` copies what it says. Plain text, never markdown: the message is the
      // provider's, and its line breaks are kept by the row's own CSS.
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="ending" sign="·" current={current} className={yankedClass} navStop="row">
          {/* The reset time the ending itself kept: a later limit report never changes an older row. */}
          {turnEndingText(item.ending, item.ending.resetsAt ?? null, ranFor)}
        </Row>
      );
    }
    case "wake": {
      return (
        <Row kind="wake" sign="●" current={current} className={yanked ? "row-yanked" : undefined} navStop="row">
          <div className="wake-head"><span className="wake-text"><Revealed pieces={revealHidden(wakeText(item))} /></span>
            {item.body && !item.inlineBody && <span className="fold-marker" aria-label={expanded ? "expanded" : "collapsed"}>{expanded ? "▾" : "▸"}</span>}</div>
          {item.body && (expanded || detailed || item.inlineBody) && <div className="wake-body"><Revealed pieces={revealHidden(item.body)} /></div>}
        </Row>
      );
    }
    case "notice": {
      // One of the CLI's notices, muted like an ending row and a `j`/`k` stop like every conversation
      // row, so `y` copies all it says (`noticeText`). The hook's words are the CLI's: plain text with
      // hidden characters shown, never markdown, cut to their first lines on screen (`index.css`).
      const yankedClass = yanked ? "row-yanked" : undefined;
      const warning = item.notice.level === "warning" || item.notice.level === "suggestion" || ["hook_block", "model_fallback", "model_refusal", "peer_hold"].includes(item.notice.kind);
      const body = noticeBody(item.notice);
      // What a person's `!` line printed: a program's text, opened where it is rather than repeated below.
      const shell = item.notice.kind === "shell_output";
      return (
        <Row kind="notice" sign={warning ? "⚠" : shell ? "⎿" : "·"} current={current} className={[yankedClass, warning ? "notice-prominent" : "", item.notice.kind === "compaction" ? "notice-compaction" : ""].filter(Boolean).join(" ")} navStop="row">
          <div className={shell && (expanded || detailed) ? "notice-text notice-open" : "notice-text"} data-notice-kind={item.notice.kind}>
            <Revealed pieces={revealHidden(noticeText(item.notice))} />
          </div>
          {body && <span className="fold-marker" aria-label={expanded ? "expanded" : "collapsed"}>{expanded ? "▾" : "▸"}</span>}
          {body && !shell && (expanded || detailed) && <div className="wake-body"><Revealed pieces={revealHidden(body)} /></div>}
        </Row>
      );
    }
    case "thinking": {
      // The agent's thinking: a muted row, its text behind the row's own fold (`Enter`/`zo`/`zc`, on
      // `expanded[key]` like a tool's result) unless the level or the detailed view shows it whole.
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="thinking" sign="∴" current={current} className={yankedClass} navStop="row">
          <ThinkingBody block={item.block} open={item.open} level={thinkingDisplay} expanded={expanded} detailed={detailed} />
        </Row>
      );
    }
    case "run": {
      // A run replaces its calls with one line, `Read ×3 · Bash ×2`; `Enter`
      // (`toggle-expand`, keyed on this row's OWN key) puts them back.
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="tool-run" sign="✓" current={current} className={yankedClass} navStop="row">
          <div className="tool-card tool-card-run">
            {runSummary(item.calls)} <span className="fold-marker" aria-label="collapsed">▸</span>
          </div>
        </Row>
      );
    }
    default: {
      // An exhaustive never check makes a missing TimelineItem case a compile error. Without it,
      // undefined is a valid ReactNode and tsc accepts a silently blank row.
      const _exhaustive: never = item;
      return _exhaustive;
    }
  }
});

/** How many lines a thought has, not counting the line breaks at its end. */
export function thoughtLineCount(text: string): number {
  const trimmed = text.replace(/\n+$/, "");
  return trimmed === "" ? 0 : trimmed.split("\n").length;
}

/** How many of a thought's last lines `preview` shows while it is written. */
export const THINKING_PREVIEW_LINES = 4;

/** The last `count` lines of a thought, without the line breaks at its end. */
export function lastThoughtLines(text: string, count: number): string {
  return text.replace(/\n+$/, "").split("\n").slice(-count).join("\n");
}

/** One thinking block as its level draws it. The text is a React text child in a `pre-wrap` box, never
 *  markdown or HTML: it is the provider's text, shown as it came. While the block is written the head
 *  says `Thinking…`; after, `Thought` and how many lines it has (none named for a blank one). `preview` shows the newest lines in a
 *  box of fixed height while it is written, so the rows below do not move as lines arrive. */
function ThinkingBody({
  block,
  open,
  level,
  expanded,
  detailed,
}: {
  block: ThinkingBlock;
  open: boolean;
  level: ThinkingDisplay;
  expanded: boolean;
  detailed: boolean;
}) {
  const always = detailed || level === "expanded";
  const whole = always || expanded;
  const lines = thoughtLineCount(block.text);
  // A thought of only spaces and line breaks has nothing to count: "0 lines" or "3 lines" of nothing
  // would both mislead, so its head says just `Thought`.
  const counted = block.text.trim() !== "";
  return (
    <div className="thinking" data-open={open ? "true" : undefined} data-whole={whole ? "true" : undefined}>
      <div className="thinking-head">
        {open ? "Thinking…" : "Thought"}
        {!open && counted && <span className="thinking-count">{` · ${lines} ${lines === 1 ? "line" : "lines"}`}</span>}
        {!always && (
          <>
            {" "}
            <span className="fold-marker" aria-label={whole ? "expanded" : "collapsed"}>
              {whole ? "▾" : "▸"}
            </span>
          </>
        )}
      </div>
      {whole ? (
        <div className="thinking-text">{block.text}</div>
      ) : level === "preview" && open ? (
        <div className="thinking-preview">{lastThoughtLines(block.text, THINKING_PREVIEW_LINES)}</div>
      ) : null}
    </div>
  );
}

/** One reply's rendered HTML, made when the reply first shows and again only when its text changes.
 *  Rows are memoized, but a change of a prop every row shares (`detailed`, the thinking level) still
 *  renders them all, and leaving that to `renderMarkdown`'s bounded cache alone re-parses every reply
 *  on such a pass once a conversation holds more replies than the cache holds: each pass in order
 *  evicts the next entry it needed.
 *
 *  A reply that is still streaming changes text on every batch; its earlier text is dropped from the
 *  shared cache as it is replaced, so a long streamed reply does not leave one cached copy of the
 *  whole reply per batch behind it. */
const MarkdownBody = memo(function MarkdownBody({ text }: { text: string }) {
  const html = useMemo(() => renderMarkdown(text), [text]);
  // The text this row last put on screen, and the dropping of the one before it, happen in an effect:
  // effects run only for a render React committed, so a render it threw away cannot drop a text that
  // is still shown. Not dropped when the row unmounts: a tab switch unmounts every row, and coming
  // back would then render every reply again.
  const shownRef = useRef<string | null>(null);
  useEffect(() => {
    const before = shownRef.current;
    shownRef.current = text;
    if (before !== null && before !== text) forgetMarkdown(before);
  }, [text]);
  return <div dangerouslySetInnerHTML={{ __html: html }} />;
});

/** A drawing of a reply's open end that took longer than this leaves the page alone for a while afterwards. */
const SLOW_TAIL_MS = 8;
/** How much longer than the drawing took the page is left alone for. */
const SLOW_TAIL_REST = 3;

/** One reply. A reply that was on its way to the screen while this row was mounted keeps the DOM it was drawn into when
 *  it ends; every other reply is one rendering of its text. */
const ReplyBody = memo(function ReplyBody({ text, streaming, selecting }: { text: string; streaming: boolean; selecting: boolean }) {
  const written = useRef(streaming);
  written.current ||= streaming;
  return written.current ? <StreamingMarkdown text={text} streaming={streaming} selecting={selecting} /> : <MarkdownBody text={text} />;
});

/** A reply the agent is writing. Blocks already complete stay in the DOM as they were put there, and each batch
 *  replaces only what comes after them (`streamMarkdown`), so a batch costs what the open block costs, however long the
 *  reply has grown. One block can be most of a reply (a long paragraph, a file in a fence): redrawing it on every batch
 *  would take all the page's time, so a slow drawing is followed by a rest, and the newest text is drawn after it.
 *
 *  When the reply ends it is rendered whole, the one drawing a reload would give, and that is what the row holds. It
 *  is the same HTML as the pieces, so the DOM, and any selection in it, normally stays; the check is the guarantee.
 *
 *  While VISUAL holds a selection nothing here touches the DOM, a drawing waiting out its rest included: `App.tsx` ends
 *  the mode if the DOM under its selection changes. What came in the meantime is drawn when the mode ends. */
const StreamingMarkdown = memo(function StreamingMarkdown({ text, streaming, selecting }: { text: string; streaming: boolean; selecting: boolean }) {
  const boxRef = useRef<HTMLDivElement>(null);
  const heldRef = useRef<{
    stream: StreamedMarkdown | null;
    settled: string;
    lastSettled: ChildNode | null;
    text: string;
    restUntil: number;
    timer: ReturnType<typeof setTimeout> | null;
  }>({ stream: null, settled: "", lastSettled: null, text, restUntil: 0, timer: null });
  const drawRef = useRef(() => {
    const box = boxRef.current;
    if (box === null) return;
    const held = heldRef.current;
    const started = performance.now();
    const next = streamMarkdown(held.stream, held.text);
    if (next === held.stream) return;
    held.stream = next;
    if (next.restarted) {
      held.settled = "";
      held.lastSettled = null;
    }
    while (box.lastChild !== held.lastSettled) box.lastChild!.remove();
    if (next.added !== "") {
      box.insertAdjacentHTML("beforeend", next.added);
      held.settled += next.added;
      held.lastSettled = box.lastChild;
    }
    if (next.tail !== "") box.insertAdjacentHTML("beforeend", next.tail);
    const now = performance.now();
    const spent = now - started;
    held.restUntil = spent > SLOW_TAIL_MS ? now + spent * SLOW_TAIL_REST : 0;
  });
  const stopResting = () => {
    const held = heldRef.current;
    if (held.timer !== null) clearTimeout(held.timer);
    held.timer = null;
  };
  // Before paint, and before the list's own layout effect reads how tall the conversation is now.
  useLayoutEffect(() => {
    const held = heldRef.current;
    held.text = text;
    if (selecting) {
      stopResting();
      return;
    }
    if (!streaming) {
      stopResting();
      const whole = renderMarkdown(text);
      const box = boxRef.current;
      if (box !== null && (held.stream?.text !== text || held.settled + held.stream.tail !== whole)) box.innerHTML = whole;
      held.stream = null;
      held.settled = "";
      held.lastSettled = null;
      return;
    }
    const rest = held.restUntil - performance.now();
    if (rest <= 0) drawRef.current();
    else
      held.timer ??= setTimeout(() => {
        held.timer = null;
        held.restUntil = 0;
        drawRef.current();
      }, rest);
  }, [text, streaming, selecting]);
  useEffect(() => stopResting, []);
  return <div ref={boxRef} />;
});
