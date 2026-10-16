import { Fragment, memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { AgentUiState, ThinkingBlock, ThinkingDisplay, ToolCallRecord } from "../types";
import { renderToolCall } from "../toolRegistry";
import { buildTimeline, isUsableLink, type TimelineItem } from "../timeline";
import { buildDisplay, runSummary } from "../display";
import { forgetMarkdown, renderMarkdown } from "../markdown";
import { turnEndingText } from "../turnEnding";
import { endingDuration, NO_TURN_DURATIONS, placeFooters, workedFooters } from "../turnTimes";
import type { TurnDurations } from "../turnTimes";
import { HistoryNotice } from "./HistoryNotice";
import { PermissionCard } from "./PermissionCard";
import { Row } from "./Row";
import type { PermissionDecision } from "../bridge";
import { noteUserScroll, RESUME_FOLLOW_EVENT, USER_SCROLL_EVENT, type UserScrollDirection } from "../follow";
import { pillLabel, pillShown } from "../pill";
import { parsePath } from "../paths";
import type { PathRef } from "../paths";
import { CursorBand, type BandBox } from "../cursorBand";
import { conversationRows } from "../nav";

type Props = {
  state: AgentUiState;
  /** How long each finished turn ran (`turnTimes.ts`): the "Worked for" line under a turn's last row, and
   *  the duration on a turn-ending row. Absent: no turn has a known time. */
  turnDurations?: TurnDurations;
  /** The session is gone. Pending cards stay visible but can no longer submit into it. */
  sessionEnded: boolean;
  /** Expanded rows, keyed by timeline `key`. `App.tsx` holds this state because Enter acts on
   *  its cursor through `toggle-expand`. Expanding a run's key reveals its calls (`display.ts`). */
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
  /** `Ctrl+o` shows every result in full, at wider cuts, with no collapsed runs. Stored per tab
   *  in `TabViewState.detailed`; fresh tabs default to `false`. */
  detailed?: boolean;
  /** How thinking is shown (`init.lua`'s `agent.thinking`, the `display` envelope). Defaults to
   *  `collapsed`, its default there; `App.tsx` always passes it. */
  thinkingDisplay?: ThinkingDisplay;
  /** Whether the panel has the keyboard (the `pane_focus` envelope). Drives the cursor band's two strengths
   *  in `index.css` (and the sign-cell cursor's solid and hollow states outside the conversation). Defaults to
   *  `true`, the state with no host to say otherwise; `App.tsx` always passes it explicitly. */
  focused?: boolean;
  /** Rules offered by Rust for the visible cards, mapped as `permissionId -> "<words> *"`.
   *  A missing `rule_offers` entry means no rule; the card never invents a suggestion. */
  ruleOffers?: Record<string, string>;
  /** The last yanked row key, used for the `.row-yanked` flash.
   *  `App.tsx` owns the timer; this component only reads the key. */
  yankedKey?: string | null;
  onAnswerPermission: (permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) => void;
  /** v1 hardening: the cards already answered from this panel, by `permissionId` --
   *  `App.tsx` owns the set, since `a`/`d` answer a card by its id rather than through its buttons.
   *  Absent: none. */
  answeredPermissions?: ReadonlySet<string>;
  /** Each card's reason box as it is typed into, so `App.tsx`'s `d` can send it. */
  onPermissionReason?: (permissionId: string, reason: string) => void;
  /** Open clicked path links and inline code that parses as a path, matching the keyboard's
   *  `gf` target through `App.tsx`'s `open-path` action. When absent, clicks retain their normal
   *  control behavior without opening paths. */
  onOpenPath?: (ref: PathRef) => void;
  /** Reports the unread pill to the caller so it can appear in the band without covering text.
   *  `label` is `↓N`, `↓ ⚑` for a card below the view, or bare `↓` when shown with nothing counted yet.
   *  `null` hides the pill. `jump` calls `jumpToEnd` and reads `listRef` fresh when invoked.
   *  `afterSeq` is the largest `seq` on screen when following stopped, or `null` while following.
   *  The caller can save it in `TabViewState.unseenAfterSeq` across a tab switch and return it as `unseenSeed`.
   *  Called whenever `updatePill` recomputes the pill, including with `null`.
   *  The caller holds the latest value, as `pill`/`setPill` do internally. */
  onUnreadChange?: (label: string | null, jump: () => void, afterSeq: number | null) => void;
  /** Seeds the unread threshold back in on a tab switch: a tab switch reuses this
   *  component, so without this the first `updatePill` after a restore falls back to counting from
   *  the CURRENT timeline length, and a row that arrived while the tab was away is never counted --
   *  the defect this task fixes. `afterSeq` is the seq threshold to seed (`TabViewState.unseenAfterSeq`
   *  from the tab just switched TO); `tick` distinguishes one seed from the next so a second switch
   *  back to the same `afterSeq` still re-fires the effect. `null` for a tab with no saved threshold
   *  (never parked, or restored at the bottom) -- nothing to seed. */
  unseenSeed?: { afterSeq: number; tick: number } | null;
  /** Active selection mode: `"caret"`, `"visual"`, or `"vline"`; `null` or absent in BROWSE.
   *  Drives `data-visual` for the list's selection and current-row styles. `App.tsx` builds the
   *  selection directly on the DOM, so this prop only controls styling here. */
  visual?: "caret" | "visual" | "vline" | null;
};

/** Whether a finished tool call succeeded, failed, is still running, or was abandoned -- the state a
 * row's sign glyph carries. A call with no result yet is never "done with nothing to say"; see
 * `ToolResult` in `toolRegistry.tsx`, which draws the matching distinction in the body.
 *
 * `abandoned` (sw-panel-render-6) reuses the `·` glyph `App.tsx` already draws for an ended session
 * row (`<Row kind="ended" sign="·" ...>`), rather than inventing a second "this is over" mark. */
function toolSign(call: ToolCallRecord, abandoned: boolean): string {
  if (call.result === null) return abandoned ? "·" : "◐";
  return call.result.isError ? "✗" : "✓";
}

/** Whether a `null` result can never arrive: the call predates the restored-history boundary (a
 * resume/reload only ever replays what a transcript actually recorded, so a call from before that
 * cut that has no result now never will), the session itself has ended, or the call's own turn is
 * no longer the active one. Without this, `null`
 * meant only "still running" -- so a tool call abandoned by a killed/interrupted session, or one
 * that outlived the transcript it was restored from, rendered a permanent spinner (sw-panel-render-6).
 * A call still genuinely running (an active turn, inside the live span) is untouched: this is never
 * true for it, matching the pre-fix "◐" rendering exactly. */
function isAbandonedCall(call: ToolCallRecord, state: AgentUiState, sessionEnded: boolean): boolean {
  if (call.result !== null) return false;
  if (sessionEnded) return true;
  if (state.history !== null && call.seq < state.history.uptoSeq) return true;
  // Both backends stamp each event with its turn id, so a result can only arrive in the call's own turn.
  // An interrupted, failed or superseded turn cannot finish the call; leaving it running would show a permanent spinner.
  // For a call without a turn id, treat it as abandoned when no turn is active.
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

/** Whether an upward wheel or key aimed at `target` would scroll `list` ITSELF: not when the list is
 *  already at its top, and not when a box between `target` and the list -- an expanded tool result's
 *  own 260px box -- can still scroll up and takes the gesture first. A prediction: the engine decides,
 *  and can latch a whole gesture onto an inner box after this has looked, which is why the stop it
 *  leads to is provisional (`provisionalStopRef` below). */
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
  /** The one `ResizeObserver`, on the list and on every child of it (see "Correction (the GUI pass)"
   *  below), and which children it is watching. `null` where there is none (jsdom). */
  const resizeObserverRef = useRef<ResizeObserver | null>(null);
  const observedChildrenRef = useRef<Set<Element>>(new Set());
  /** The CURRENT render's `onScroll`, for the observer below, which is made once. Calling the first
   *  render's own `onScroll` from there ran that render's `updatePill`, which slices that render's
   *  timeline: every row that arrived since was invisible to it, so a row settling its size after a
   *  card landed below a reader rewrote `↓ ⚑ approval` as `↓ Jump to bottom` (the phase-3 GUI pass,
   *  2026-09-25). Assigned right after `onScroll` is defined, on every render. */
  const onScrollRef = useRef<() => void>(() => {});
  /** The cursor's fill (`../cursorBand`): the element, its animator, and the row it was last put on, by
   *  timeline key. `placeBandRef` is the current render's placement, for the observer below. */
  const bandElRef = useRef<HTMLDivElement>(null);
  const bandRef = useRef<CursorBand | null>(null);
  const bandRowKeyRef = useRef<string | null>(null);
  const placeBandRef = useRef<(glide: boolean) => void>(() => {});

  /* Watch the list and its rows for size changes. Viewport-based margins supply row widths;
     no measured width or size query container is needed. A size container on `.row` caused
     WebKitGTK to reset `scrollTop` when the status line clock ticked during streaming.

     A resize can hide the tail without a state change: narrower replies grow taller, or the
     composer, terminal, or key strip makes the list shorter. Rows must be observed too because
     their size can change without changing the list's box, such as after a margin or font change.
     While following, snap to the end after layout and before paint. Leave parked readers and
     pending provisional stops in place. */
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
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /** Puts every child of the list under the observer above, and lets go of any that left. Run on
   *  every state change, which is the only way a row mounts or goes away. */
  const observeChildren = (list: HTMLElement) => {
    const observer = resizeObserverRef.current;
    if (observer === null) return;
    const observed = observedChildrenRef.current;
    for (const el of observed) {
      if (el.parentElement !== list) {
        observer.unobserve(el);
        observed.delete(el);
      }
    }
    for (const el of Array.from(list.children)) {
      // The band changes size as it glides; it is not content, and is placed from the rows.
      if (el === bandElRef.current) continue;
      if (!observed.has(el)) {
        observer.observe(el);
        observed.add(el);
      }
    }
  };

  // Follow new rows and growing replies only while `followingRef` is true. Include `userPrompts`
  // because submitting a prompt can grow the list before any reply or tool call arrives.
  // Use scroll position rather than the cursor index so following also works without keyboard navigation.
  // An unexplained upward scroll is not enough to stop following: WebKitGTK could reset `scrollTop`
  // on status clock ticks when rows were CSS size query containers, without any list content change.
  // User intent comes from wheels, touch drags, a pointer held on the list, browser scroll keys,
  // focus landing inside the list, or panel scroll commands announced through `noteUserScroll`.
  // An upward scroll while steering stops following. `k`, `Ctrl+u` and `gg` stop it immediately.
  // Browser keys such as PageUp, Home and Shift+Space must steer too when a control inside the list has focus.
  // A mostly vertical upward wheel or an upward browser key provisionally stops following only when
  // `listTakesUpward` says the list can move. This prevents a delta from snapping over a delayed scroll.
  // Other gestures only steer, so horizontal jitter and scrolling an inner box do not leave following off.
  // An actual upward scroll confirms the stop. If the list never moves up, take the stop back when
  // the steering window ends and snap to any content that arrived meanwhile.
  // `scheduleSettle` handles that timeout even if no later state change arrives; later gestures extend
  // it, and a held pointer delays settlement until release and the following steering window.
  // Reaching the true bottom (`AT_BOTTOM_PX`) re-arms following unless a provisional gesture has not moved.
  // A downward user scroll within `BOTTOM_FOLLOW_THRESHOLD_PX` also re-arms it.
  // Snap before paint for both new rows and streamed growth so the newest content is visible immediately.
  // The `ResizeObserver` also snaps while following when the list or its rows change size without a state change.
  // jsdom has no layout and generates no scroll events itself, so following tests dispatch them explicitly.

  /* `buildTimeline` merges conversation items using the `seq` assigned by Rust's projection
     and shipped in snapshots, so reloads and `UiDelivery::Resync` preserve live ordering.
     Memoising the timeline avoids rebuilding and sorting it on unrelated parent renders.
     `MarkdownBody` renders each reply once per text instead of repeating markdown processing
     for every list render. Keep `timeline` above `onScroll` because `updatePill` reads it too. */
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
  /** Where a wheel or key stopped following on a prediction, until the gesture shows whether the list
   *  really went up; `null` when no such stop is pending. See (3b) above. (Its `missedRow` flag went
   *  with the smooth scroll, the GUI pass: a take-back snaps to the end whatever arrived meanwhile.) */
  const provisionalStopRef = useRef<{ from: number } | null>(null);
  const settleTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const steering = () => pointerSteeringRef.current || performance.now() <= steerUntilRef.current;
  /** Brings the newest content into view: a snap to the end, whether a new row arrived, the last one
   *  grew, or the list or a row changed size (see the correction above for why never a smooth scroll). */
  const follow = (list: HTMLElement | null) => {
    if (list !== null && list.scrollHeight - list.scrollTop - list.clientHeight > 0) {
      list.scrollTop = list.scrollHeight;
      // Record where the snap put the view. Found in review: this ref kept the PRE-snap value, so
      // a `k` pressed before the snap's own scroll event arrived ended above the snap but not above
      // the stale value, was not seen as a scroll up, and the next delta snapped the view back.
      lastScrollTopRef.current = list.scrollTop;
    }
  };
  /** The largest `seq` on screen when following stopped, or `null` while following.
   *  A timeline index is not a reliable unread cutoff. This component survives tab switches, and
   *  using the restored timeline's length would miss rows that arrived while the tab was away.
   *  Also, `buildTimeline` places a permission card after its tool call, so a newly arrived card
   *  can sit before an old length cutoff. A `seq` threshold counts arrivals independently of array position. */
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
  /** Settle a pending stop when its gesture ends, even if no further state change arrives. */
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
    // Check the true bottom before user scroll direction, and direction before the near-bottom threshold.
    // An upward user scroll stops following even within the threshold; otherwise the next delta would undo it.
    // The true-bottom check keeps following armed when content shrinks and the browser clamps `scrollTop`.
    // Only a downward user scroll re-arms following within the wider threshold; an unchanged position does not.
    // A scroll event at a provisional gesture's starting position must not clear that stop or re-arm following.
    // WebKitGTK can send an animated wheel's first scroll event before the view moves. Re-arming then
    // would let a snap cancel the wheel. The gesture timer settles the stop when steering ends,
    // or after a held pointer is released.
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
    // (3b): only a mostly vertical wheel up that the list itself can take stops following, and only
    // provisionally. A sideways swipe's jitter, a wheel an inner box takes and a wheel with nothing
    // above only steer: if the list does move, its scroll event says so.
    const onWheel = (event: WheelEvent) => {
      steer();
      if (event.deltaY < 0 && Math.abs(event.deltaY) > Math.abs(event.deltaX) && listTakesUpward(list, event.target)) {
        stopProvisionally(list);
      }
    };
    // (3a): the browser's own scroll keys, from a control inside the list. In a text box they move the
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
      // Settle a stop held open by the pointer after the release steering window ends.
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
    // Listen for both pointer and mouse presses to detect scrollbar dragging.
    // Whether WebKitGTK delivers either event to the list for its own scrollbar is unverified.
    // An undetected drag would be undone by the next streamed delta.
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
  // Run on every `state` change so both new rows and streamed growth remain in view while following.
  // Read `onScroll()` synchronously first because the browser may deliver a user scroll event a frame late.
  // Panel commands also announce their intent through `noteUserScroll` before moving the list.
  // Use a layout effect to snap before paint; a passive effect can paint new text below the viewport first.
  // Snap for both new rows and growth. Repeated smooth animations during streaming can keep restarting.
  // Observe newly mounted rows so size changes without a state change also update the following view.
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

  /** Restore the unread threshold when `App.tsx` restores a parked tab's view.
   *  Run after the `[state]` effect so this `updatePill()` replaces any value computed from the previous tab
   *  or the initial following state. Key on `unseenSeed?.tick` so equal thresholds on later switches still apply.
   *  Use a layout effect: child layout effects run before the parent's scroll restore.
   *  Set following to false and seed the threshold before that restore can produce a scroll event,
   *  so the event retains the saved cutoff instead of computing one from the restored timeline.
   *  `updatePill` clears the threshold only while following. `jumpToEnd` relies on its scroll event
   *  reaching `onScroll` and `updatePill` rather than changing the threshold directly. */
  useLayoutEffect(() => {
    if (unseenSeed == null) return;
    followingRef.current = false;
    unseenAfterSeqRef.current = unseenSeed.afterSeq;
    updatePill();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [unseenSeed?.tick]);

  /* Match pending permissions to tool calls by `tool_use_id`, since several calls of the same
     tool can be in flight. Use the shared `isUsableLink` predicate from `../timeline` to exclude
     `null` and "", matching `PermissionCard` without duplicating the guard. Legacy permissions
     carry null, so this marker does not render on that backend. */
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
    const key = timeline[cursor]?.key ?? null;
    const before = bandRowKeyRef.current;
    bandRowKeyRef.current = key;
    const glide = before !== null && key !== null && before !== key && timeline.some((item) => item.key === before);
    placeBandRef.current(glide);
  });
  useEffect(() => () => bandRef.current?.stop(), []);

  /** The card callbacks a row calls, through one ref that never changes, so a new callback from the
   *  parent (they are made on every render) does not make every row render again. */
  const rowHandlersRef = useRef<RowHandlers>({ onAnswerPermission, onPermissionReason });
  rowHandlersRef.current = { onAnswerPermission, onPermissionReason };

  const footers = useMemo(
    () => workedFooters(state, turnDurations),
    [state.activeTurnId, state.turnEndings, state.transcript, state.thinking, state.toolCalls, state.pendingPermissions, turnDurations],
  );
  // By row key, over the rows actually drawn: a turn's newest row may be a hidden or folded thought.
  const placedFooters = useMemo(() => placeFooters(timeline, footers), [timeline, footers]);

  const awaitingPermission = new Set(state.pendingPermissions.map((p) => p.toolUseId).filter(isUsableLink));

  return (
    <>
      <div
        className="message-list"
        data-focused={String(focused)}
        data-visual={visual ?? undefined}
        ref={listRef}
        onScroll={onScroll}
        // Open `.path-link` spans or inline code in replies when they parse as paths.
        // Exclude fenced code (`pre code`): it is a snippet, and HINT provides `y` to copy it.
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
          return (
            <Fragment key={item.key}>
              <TimelineRow
                item={item}
                current={index === cursor}
                yanked={item.key === yankedKey}
                expanded={expanded[item.key] === true}
                detailed={detailed}
                thinkingDisplay={thinkingDisplay}
                sessionEnded={sessionEnded}
                abandoned={call !== null && isAbandonedCall(call, state, sessionEnded)}
                gated={call !== null && awaitingPermission.has(call.toolUseId)}
                ruleOffer={permissionId === null ? null : (ruleOffers?.[permissionId] ?? null)}
                alreadyAnswered={permissionId !== null && (answeredPermissions?.has(permissionId) ?? false)}
                handlers={rowHandlersRef}
                ranFor={item.kind === "ending" ? endingDuration(turnDurations, item.ending.turnId) : null}
              />
              {footer !== null && (
                // Chrome, not a row: no `data-nav-stop`, so `j`/`k` and `y` never see it, and the inner
                // `aria-hidden` span is what VISUAL's copy leaves out (`VISUAL_CHROME`).
                <div className="turn-time" data-turn-time="">
                  <span aria-hidden="true">{footer}</span>
                </div>
              )}
            </Fragment>
          );
        })}
        {/* No end-of-list sentinel any more: it existed for the smooth `scrollIntoView` a new row used
            to get, and following is a snap to `scrollHeight` now (the GUI pass, 2026-09-24). */}
        {/* The cursor's fill, behind the rows (`../cursorBand`). Hidden from assistive technology and from
            VISUAL's selection (`[aria-hidden="true"]` is chrome there). */}
        <div className="cursor-band" aria-hidden="true" ref={bandElRef} />
      </div>
    </>
  );
}

type RowHandlers = {
  onAnswerPermission: Props["onAnswerPermission"];
  onPermissionReason: Props["onPermissionReason"];
};

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
  /** A tool call a card is waiting on. */
  gated: boolean;
  /** A card's offered rule, exactly as Rust offered it. */
  ruleOffer: string | null;
  alreadyAnswered: boolean;
  /** For a turn-ending row: how long the turn ran, worded (`formatDuration`), or `null` when unknown. */
  ranFor: string | null;
  handlers: { current: RowHandlers };
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
  gated,
  ruleOffer,
  alreadyAnswered,
  ranFor,
  handlers,
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
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="assistant" sign="" current={current} className={yankedClass} navStop="row">
          {/* `renderMarkdown` (`../markdown.ts`) is marked.parse, now with a `code` renderer
              that runs highlight.js against nvim's own syntax colours, then DOMPurify.sanitize
              -- one function so no caller can run half of it. */}
          <MarkdownBody text={item.text} />
        </Row>
      );
    }
    case "tool": {
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="tool" sign={toolSign(item.call, abandoned)} current={current} className={yankedClass} navStop="row">
          <div data-awaiting-permission={gated ? "true" : undefined}>
            {/* A gated call shows "waiting for approval" because its permission card already shows
                the invocation. Repeating it here would make a pending call look as if it had run. */}
            {renderToolCall(item.call, expanded, {
              gated,
              detailed,
              expanded,
              abandoned,
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
      // A run replaces its calls with a summary such as `Read ×3 · Bash ×2`.
      // Enter uses `toggle-expand` on the run row key to reveal the calls.
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
      // Exhaustiveness guard. Without it, a fifth `TimelineItem` kind gaining no case here
      // renders as nothing: this component has no pinned return type, a missing case
      // falls through to `undefined`, `undefined` is a valid `ReactNode`, and `tsc -b` stays
      // clean -- exactly how the "prompt" case's own placeholder was allowed to hide for a
      // whole task (see the git history of this switch). Assigning `item` to `never` turns
      // that same mistake into a compile error instead of an invisible blank row.
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
 *  The list renders every row on every update (a cursor move, a streamed batch), and leaving that to
 *  `renderMarkdown`'s bounded cache alone re-parsed every reply on every render once a conversation
 *  held more replies than the cache holds: each pass in order evicted the next entry it needed.
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
