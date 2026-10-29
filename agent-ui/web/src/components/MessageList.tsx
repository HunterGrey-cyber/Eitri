import { toolCardLine, agentCard, isAgentCall } from "../display";
import { Fragment, memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { AgentUiState, ThinkingBlock, ThinkingDisplay, ToolCallRecord } from "../types";
import { renderToolCall } from "../toolRegistry";
import { buildTimeline, isUsableLink, noticeText, noticeBody, wakeText, questionLines, type TimelineItem } from "../timeline";
import { buildDisplay, runSummary } from "../display";
import { forgetMarkdown, renderMarkdown } from "../markdown";
import { turnEndingText } from "../turnEnding";
import { endingDuration, NO_TURN_DURATIONS, placeFooters, workedFooters } from "../turnTimes";
import type { TurnDurations } from "../turnTimes";
import { HistoryNotice } from "./HistoryNotice";
import { PermissionCard } from "./PermissionCard";
import { Row } from "./Row";
import { Revealed } from "./Revealed";
import { revealHidden } from "../revealHidden";
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

/** Whether a `null` result can never arrive: the call predates the restored-history boundary (a
 * resume/reload only ever replays what a transcript actually recorded, so a call from before that
 * cut that has no result now never will), the session itself has ended, or the call's own turn is
 * no longer the active one. Without this, `null`
 * meant only "still running" -- so a tool call abandoned by a killed/interrupted session, or one
 * that outlived the transcript it was restored from, rendered a permanent spinner.
 * A call still genuinely running (an active turn, inside the live span) is untouched: this is never
 * true for it, matching the pre-fix "◐" rendering exactly. */
function isAbandonedCall(call: ToolCallRecord, state: AgentUiState, sessionEnded: boolean): boolean {
  if (call.result !== null) return false;
  if (sessionEnded) return true;
  if (state.history !== null && call.seq < state.history.uptoSeq) return true;
  // A result only ever arrives inside the call's own turn (both backends stamp every event of a
  // turn with its id). Once that turn is not the active one -- interrupted, failed, or followed by
  // another -- it never will (an interrupted turn once left `◐ Running…`
  // forever). A call with no turn id on it falls back to "no turn is in flight at all".
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
   *  card landed below a reader rewrote `↓ ⚑ approval` as `↓ Jump to bottom` (seen in a GUI pass,
   *  2026-09-25). Assigned right after `onScroll` is defined, on every render. */
  const onScrollRef = useRef<() => void>(() => {});
  /** The cursor's fill (`../cursorBand`): the element, its animator, and the row it was last put on, by
   *  timeline key. `placeBandRef` is the current render's placement, for the observer below. */
  const bandElRef = useRef<HTMLDivElement>(null);
  const bandRef = useRef<CursorBand | null>(null);
  const bandRowKeyRef = useRef<string | null>(null);
  const placeBandRef = useRef<(glide: boolean) => void>(() => {});

  /* The list and its rows, watched for size (2026-09-24). Until 2026-10-13 this observer also wrote the
     list's content-box width to `--list-inline-size`, from which every wide-content escape in `index.css`
     took the row's width -- the replacement for a CSS size query container on `.row`, which in
     WebKitGTK 2.52.6 made the engine reset this list's `scrollTop` every time the status line's elapsed
     counter ticked while a reply streamed (`shell/tests/panel_stream_scroll.rs` is the regression test,
     against the real engine). The chat page's rows fill the panel between margins written against
     the viewport, so nothing reads a measured width any more and the write is
     gone; no size container came back with it.

     A GUI pass on 2026-09-24 found that when this observer only wrote the width, the follow
     snap below ran only on a state change -- so a resize with nothing streaming behind it
     left a following view short: narrowing the panel reflows the reply taller while `scrollTop` stays
     put (482px short after a divider drag, 651px after an unzoom, measured in the sandbox), and a
     shorter list (the bottom terminal shown, the composer growing, the which-key strip appearing as
     INPUT becomes BROWSE) hides the tail the same way, for as long as a tool call runs, a permission
     card waits, or the turn is over. The same observer now also watches every child of the list (the
     rows), because the rows can change size with the list's box unchanged -- a side margin set on the
     list alone, say, or anything else restyling the conversation alone. (A font push from the theme
     is caught either way: the status line and the composer grow with the same
     `--nv-font-size`, so the list's own box gets shorter too -- measured, the real-engine test with the rows left
     unobserved failed only on its row-only restyle, then `--prose-measure`.) Either way, while following, the
     callback snaps the view to its end: after layout and before paint, the same frame. Snapping is
     all it adds; a parked reader, or a pending provisional stop, is left exactly where it is.
     `shell/tests/panel_stream_scroll.rs` is the real-engine test. */
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
    const children = new Set([
      ...Array.from(list.children).filter((el) => !el.classList.contains("subagent-child")),
      ...list.querySelectorAll(".subagent-child > .row"),
    ]);
    for (const el of observed) {
      if (!children.has(el)) {
        observer.unobserve(el);
        observed.delete(el);
      }
    }
    for (const el of children) {
      // The band changes size as it glides; it is not content, and is placed from the rows.
      if (el === bandElRef.current) continue;
      if (!observed.has(el)) {
        observer.observe(el);
        observed.add(el);
      }
    }
  };

  // Following uses scroll position rather than cursor position so mouse and touch readers can
  // follow replies without moving the keyboard cursor. New prompts, rows, and streamed growth
  // must all be followed, but incoming content must leave a reader who scrolled up in place.
  // Distance measured after new content mounts cannot tell whether the reader was following.
  // Engine clamps can also move `scrollTop`, so upward movement alone is not evidence of intent.
  // A size query container on `.row` caused such jumps when the status line clock changed.
  // Wheel, touch, pointer, focus, and native scroll keys mark a steering window. Panel scrolls
  // announce intent through `noteUserScroll`; `k`, `Ctrl+u`, and `gg` stop following immediately.
  // A mostly vertical upward wheel or native key stops provisionally only if `listTakesUpward`
  // says the list can scroll. Upward movement while steering confirms the stop. If the gesture
  // ends without moving the list up, take the stop back so an inner box consuming the gesture
  // does not leave following disabled. `scheduleSettle` handles this even if no more state arrives;
  // later gestures extend the window, and a held pointer defers settlement until release.
  // The true bottom re-arms following; a downward user scroll near the bottom does too.
  // While following, snap before paint for both new rows and growing text. Smooth scrolling
  // leaves new rows temporarily hidden and would restart on every streamed text update.
  // The `ResizeObserver` also follows size changes that arrive without a state update.
  // jsdom has no layout or automatic scroll events, so tests dispatch scroll events explicitly.

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

     A reply's markdown is rendered by `MarkdownBody` (end of this file), once per text, so a render
     of this list no longer runs marked, highlight.js and DOMPurify over every reply.

     Declared here, above `onScroll` (moved for the pill): the pill's `updatePill` closure, defined
     below alongside `onScroll`, reads `timeline` too, and both need the same binding in scope. */
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
   *  really went up; `null` when no such stop is pending. See the correction on provisional stops above. (Its `missedRow` flag went
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
  /** The largest `seq` on screen when following last stopped; `null` while following.
   *
   *  This used to be a timeline INDEX (`unseenFromRef`, the timeline's length at the
   *  moment following stopped), sliced off the CURRENT timeline with `timeline.slice(from)`. That
   *  broke two ways. First, a tab switch reuses this component (`App.tsx` never remounts it), so the
   *  first `updatePill` after a restore reset `from` to the just-restored (short) timeline's own
   *  length -- every row that had arrived while the tab was away sat BELOW that length and was
   *  silently never counted. Second, even within one tab, `buildTimeline` does not keep the array in
   *  `seq` order: a permission card anchored to an older tool call is spliced in right after that
   *  call (see `timeline.ts`), which can land it BEFORE the old length cutoff even though its own
   *  `seq` -- when it actually arrived -- is far newer than everything after it. A `seq` threshold
   *  does not care where an item landed in the array, only when it happened. */
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
    // The order matters (2026-09-19): the threshold used to be checked
    // FIRST, so a `k` step that ended within the threshold of the bottom turned following straight
    // back ON and the next streamed delta snapped the view back down. A user's scroll UP always
    // stops following unless it left the view at the TRUE bottom, which is also where the browser
    // clamps `scrollTop` after content shrinks (a resolved permission card's row going away) -- a
    // user at the bottom stays followed through that. The threshold only re-arms on a scroll that
    // went DOWN, never on the effect's own synchronous call below, where nothing moved.
    //
    // An event that finds the list where a pending gesture found it says nothing about
    // that gesture, so at the bottom it neither re-arms following nor clears the stop. WebKitGTK
    // animates a wheel's scroll, and its FIRST scroll event, 0-1ms after the wheel, can report no
    // movement yet (seen in 2 of 8 wheels at the tail); re-arming on it let the next delta's snap, or
    // a new row's smooth scroll, cancel the wheel's scroll -- the case the provisional stop exists to
    // prevent. A pending stop always has its gesture's timer armed (or a pointer held, whose release
    // arms it), so the stop is still settled when the gesture ends (`settleWhenGestureEnds`).
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
  // While following, every state update snaps to the end for both new rows and streamed growth.
  // Use a layout effect so new text is visible before paint. Smooth scrolling would leave a new
  // row hidden during its animation and restart repeatedly as streamed text arrives.
  // Read `onScroll()` first to catch user movement whose scroll event has not arrived yet.
  // `noteUserScroll` also stops following before upward panel navigation scrolls.
  // `observeChildren` registers new rows with the `ResizeObserver`, which follows later size
  // changes even when no state update occurs.
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
          const row = (
            <TimelineRow
                item={item}
                current={index === cursor}
                yanked={item.key === yankedKey}
                expanded={expanded[item.key] === true}
                detailed={detailed}
                thinkingDisplay={thinkingDisplay}
                sessionEnded={sessionEnded}
                cardLine={call === null ? null : toolCardLine(call, state)}
                agent={call !== null && isAgentCall(call) ? agentCard(call, state, isAbandonedCall(call, state, sessionEnded)) : null}
                abandoned={call !== null && isAbandonedCall(call, state, sessionEnded)}
                gated={call !== null && awaitingPermission.has(call.toolUseId)}
                ruleOffer={permissionId === null ? null : (ruleOffers?.[permissionId] ?? null)}
                alreadyAnswered={permissionId !== null && (answeredPermissions?.has(permissionId) ?? false)}
                handlers={rowHandlersRef}
                ranFor={item.kind === "ending" ? endingDuration(turnDurations, item.ending.turnId) : null}
              />
          );
          return (
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
  cardLine: string | null;
  agent: ReturnType<typeof agentCard> | null;
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
  cardLine,
  agent,
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
          <MarkdownBody text={item.text} />
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
      return (
        <Row kind="notice" sign={warning ? "⚠" : "·"} current={current} className={[yankedClass, warning ? "notice-prominent" : "", item.notice.kind === "compaction" ? "notice-compaction" : ""].filter(Boolean).join(" ")} navStop="row">
          <div className="notice-text" data-notice-kind={item.notice.kind}>
            <Revealed pieces={revealHidden(noticeText(item.notice))} />
          </div>
          {body && <span className="fold-marker" aria-label={expanded ? "expanded" : "collapsed"}>{expanded ? "▾" : "▸"}</span>}
          {body && (expanded || detailed) && <div className="wake-body"><Revealed pieces={revealHidden(body)} /></div>}
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
      // Exhaustiveness guard. Without it, a fifth `TimelineItem` kind gaining no case here
      // renders as nothing: this component has no pinned return type, a missing case
      // falls through to `undefined`, `undefined` is a valid `ReactNode`, and `tsc -b` stays
      // clean -- exactly how the "prompt" case's own placeholder was allowed to hide for a
      // long time. Assigning `item` to `never` turns
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
