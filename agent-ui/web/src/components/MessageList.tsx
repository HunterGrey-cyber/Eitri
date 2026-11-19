import { attachDiagrams, detachDiagrams } from "../diagrams";
/// <reference types="vite/client" />
import { rowCardLine, rowAgentCard, agentCard, isAgentCall } from "../display";
import { Fragment, memo, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactElement, type ReactNode } from "react";
import type { AgentUiState, ThinkingBlock, ThinkingDisplay, ToolCallRecord } from "../types";
import { renderToolCall, TouchRows } from "../toolRegistry";
import { ProjectDirContext } from "../projectPath";
import { allCards, buildTimeline, isUsableLink, noticeText, noticeBody, wakeText, questionLines, planLines, type TimelineItem } from "../timeline";
import { buildDisplay, runSummary } from "../display";
import { forgetMarkdown, renderMarkdown, renderMarkdownRevealed, streamMarkdown, type StreamedMarkdown } from "../markdown";
import { turnEndingText } from "../turnEnding";
import { endingDuration, NO_TURN_DURATIONS, placeFooters, workedFooters } from "../turnTimes";
import type { TurnDurations } from "../turnTimes";
import { HistoryNotice } from "./HistoryNotice";
import { earlierTurnsText, historyNoticeText } from "../history";
import { PermissionCard } from "./PermissionCard";
import type { CardModeView } from "./PermissionCard";
import { Row } from "./Row";
import { Revealed } from "./Revealed";
import { revealedText, revealHidden } from "../revealHidden";
import { followSelection } from "../visualPaint";
import type { PermissionDecision } from "../bridge";
import type { PointerAnswerProof } from "../answerProof";
import { noteUserScroll, RESUME_FOLLOW_EVENT, USER_SCROLL_EVENT, type UserScrollDirection } from "../follow";
import { pillLabel, pillShown } from "../pill";
import { parsePath } from "../paths";
import type { PathRef } from "../paths";
import { CursorBand, type BandBox } from "../cursorBand";
import { conversationRows, stopOf } from "../nav";
import { captureAnchor, restoreAnchor, type ViewAnchor } from "../viewAnchor";
import { firstRowBelow, headerBand, headerCount, headerPrompt, headerText, PROMPT_HEADER_CLASS, type HeaderPrompt } from "../promptHeader";

type Props = {
  lateLines?: import("../syncChip").SendSync["lines"];
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
  historyFromTurn?: number;
  historyToTurn?: number;
  onHistoryOlder?: () => void;
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
  /** A card button's answer, carrying the proof a real pointer click made (`PermissionCard`'s `onAnswer`). */
  onAnswerPermission: (
    permissionId: string,
    decision: PermissionDecision,
    reason: string | undefined,
    remember: boolean | undefined,
    proof: PointerAnswerProof,
  ) => void;
  /** The cards already answered from this panel, by `permissionId` --
   *  `App.tsx` owns the set, since card mode's keys answer a card by its id rather than through its buttons.
   *  Absent: none. */
  answeredPermissions?: ReadonlySet<string>;
  /** The card that holds card mode (`App.tsx`), and how it is drawn; `null` or absent when no card does. The view
   *  object is kept the same while nothing in it changes, so the other rows are not drawn again. */
  cardMode?: { permissionId: string; view: CardModeView } | null;
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
  /** A click on the pinned prompt header (`../promptHeader`): go to the prompt row it names, by its key. Absent, the
   *  header is drawn and a click on it does nothing. */
  onPromptJump?: (key: string) => void;
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
  /** The phone's transcript page: rows open and close by a tap (`onToggle`), a fold says "tap to show", and there
   *  is no cursor band, no visual selection and no keyboard steering of the follow; cards and questions show no
   *  controls (`deskLocation` says where they are answered). Defaults to `false`, the desk. */
  touch?: boolean;
  /** Where a card is answered (`project · window N · tab M`), shown in its place of the controls under `touch`. */
  deskLocation?: string;
  /** This phone's device id under `touch`: what it sent reads "sent from this phone", what it stopped "stopped from
   *  this phone". */
  thisDevice?: string | null;
  /** Entries a snapshot holds only the start of, by `seq`, with about how many bytes the whole entry is; each row
   *  says so and loads it whole on a tap (`onLoadWhole`). Touch only. */
  cut?: ReadonlyMap<number, number>;
  /** The cut entry whose tap waits for a page already on its way; its row says it loads once that page is in. */
  cutWaiting?: number | null;
  onLoadWhole?: (seq: number) => void;
  /** A tap on a row that folds (a call, a run, a thought, a notice or wake with more to say). Touch only. */
  onToggle?: (key: string) => void;
  /** The list reached its top, by a scroll or because it does not fill the view. Touch only. */
  onReachTop?: () => void;
  /** Drawn after the last row, inside the scroll box: a page's own blocks that follow the conversation. */
  after?: ReactNode;
};

/** The row kinds a tap opens and closes under `touch`: those `Enter` toggles on the desk. */
const TAP_TOGGLES: ReadonlySet<TimelineItem["kind"]> = new Set(["tool", "run", "thinking", "notice", "wake"]);
/** How close to the top of the list counts as having reached it, for loading older turns. */
const TOP_REACHED_PX = 48;
/** The desk's history notice, whose button copies the desk's file path into the desk's selection. The phone's page
 *  is built with `EITRI_PHONE_PAGE` set (`vite.transcript.config.ts`), which leaves the notice, and the message it
 *  posts to the desk's host, out of that page; under `touch` the notice is drawn as text only in any build. */
const DeskHistoryNotice = import.meta.env.EITRI_PHONE_PAGE === true ? null : HistoryNotice;

/** A cut entry's size, for its row: bytes up to a kilobyte, then KB and MB with one decimal. */
export function cutSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

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

/** Whether a downward wheel or key at `target` is the list's own: no box between it and the list can still scroll
 *  down to take it (a tool output's own box, a wide table's), so at the list's end it asks for the end. */
function listTakesDownward(list: HTMLElement, target: EventTarget | null): boolean {
  for (let el = target instanceof Element ? target : null; el !== null && el !== list; el = el.parentElement) {
    if (el.scrollHeight - el.scrollTop - el.clientHeight > AT_BOTTOM_PX) {
      const { overflowY } = getComputedStyle(el);
      if (overflowY === "auto" || overflowY === "scroll" || overflowY === "overlay") return false;
    }
  }
  return true;
}

/** A focused control acts on the keys it gets (Space and Enter press a button) instead of scrolling the list. */
function isControl(target: EventTarget | null): boolean {
  return (
    isTextEntry(target) ||
    (target instanceof HTMLElement &&
      (target.closest("button, a[href], select, summary, [role=button], [role=link], [role=checkbox], [role=radio]") !== null))
  );
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
  lateLines,
  tab = null,
  state,
  historyFromTurn,
  historyToTurn,
  onHistoryOlder,
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
  cardMode = null,
  onOpenPath,
  onUnreadChange,
  onPromptJump,
  unseenSeed,
  visual = null,
  touch = false,
  deskLocation = "",
  thisDevice = null,
  cut,
  cutWaiting = null,
  onLoadWhole,
  onToggle,
  onReachTop,
  after,
}: Props) {
  /** Fixed for the list's life: the listeners below are attached once. */
  const touchRef = useRef(touch);
  /** Through a ref: a held row's element keeps the handler it was made with. */
  const loadWholeRef = useRef(onLoadWhole);
  loadWholeRef.current = onLoadWhole;
  const toggleRef = useRef(onToggle);
  toggleRef.current = onToggle;
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
     container: on WebKitGTK 2.52.6 a row size container resets scrollTop on status-clock ticks.
     A reader who is not following gets the place they were reading back when the list's width changes (a zoom, a
     divider drag): see `../viewAnchor` and `reconcileWidth`. That runs first, so the rest of this callback reads the
     restored view. */
  useLayoutEffect(() => {
    const list = listRef.current;
    if (list === null) return;
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      reconcileWidthRef.current(list);
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
  /** Where a reader who is not following is looking (`../viewAnchor`), for a change of the list's width; `null`
   *  while following, on the phone, or with no row on screen. Taken when the view moves, the cursor moves or the
   *  tab changes -- never per streamed batch: rows growing below the reader leave the point where it was. */
  const anchorRef = useRef<ViewAnchor | null>(null);
  /** The list's `clientWidth` at the observer's last callback; `null` before the first. */
  const listWidthRef = useRef<number | null>(null);
  /** The `scrollTop` a width change's restore left. Until the list really moves from there, or the reader asks for
   *  the end outright (`resumeAtEnd`, a resume), its scroll events and reads are that restore's: they neither take a
   *  new anchor (so zooming back returns to the same place, not a line off) nor re-arm following when the restore
   *  was clamped at the end (a wide view of a reader near the end). Input that moves nothing -- a click, a text
   *  selection, a wheel whose movement has not arrived yet -- does not end it. */
  const restoredTopRef = useRef<number | null>(null);
  /** Where a wheel or key up that the list can take found it, while following was already off; `null` once the list
   *  has moved up from there or the gesture is over. Until then, being at the true end is not the reader reaching it:
   *  the gesture's movement is still on its way (the same protection `provisionalStopRef` gives a following view). */
  const upwardFromRef = useRef<number | null>(null);
  const cursorRef = useRef(cursor);
  cursorRef.current = cursor;
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
  const recordAnchor = () => {
    const list = listRef.current;
    if (list === null || touchRef.current || followingRef.current) {
      anchorRef.current = null;
      return;
    }
    const rows = conversationRows(list);
    anchorRef.current = captureAnchor(list, rows, rows[cursorRef.current]);
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
  /** The pinned prompt header (`../promptHeader`), and the width of the list's scrollbar it stops short of, so the
   *  scrollbar's top stays where a pointer can take it. `null` while no prompt is named. */
  const [header, setHeader] = useState<{ prompt: HeaderPrompt; gutter: number } | null>(null);
  /** Re-decides the header from the first row on screen; beside every `updatePill`, and on its own after the rows
   *  change. Nothing is measured while the view follows the newest row, nor on the phone's page. */
  const updateHeader = () => {
    const list = listRef.current;
    const following = followingRef.current;
    const prompt = list === null || touchRef.current || following ? null : headerPrompt(timeline, firstRowOnScreen(list), following);
    const gutter = list === null || prompt === null ? 0 : Math.max(0, list.offsetWidth - list.clientWidth);
    setHeader((held) => {
      if (prompt === null) return null;
      if (held !== null && held.gutter === gutter && held.prompt.index === prompt.index &&
        held.prompt.ordinal === prompt.ordinal && held.prompt.total === prompt.total) return held;
      return { prompt, gutter };
    });
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
  /** The width change is handled wherever the list is read first after it: the observer's callback, but also a
   *  streamed batch's commit or a scroll event (the engine's clamp), either of which can come between the page's new
   *  width and the observer's callback, and would otherwise read the clamp at the end as the reader reaching it. */
  const reconcileWidth = (list: HTMLElement) => {
    const width = list.clientWidth;
    const changed = listWidthRef.current !== null && width !== listWidthRef.current;
    listWidthRef.current = width;
    if (!changed || followingRef.current || anchorRef.current === null) return;
    const top = restoreAnchor(list, anchorRef.current);
    if (top === null) return;
    lastScrollTopRef.current = top;
    restoredTopRef.current = top;
  };
  const reconcileWidthRef = useRef(reconcileWidth);
  reconcileWidthRef.current = reconcileWidth;
  /** A reader at the true end (a restore can leave one there, not following) who asks for the end -- a wheel down the
   *  list itself takes, `End` -- is asking to follow: the engine moves nothing and sends no scroll event there, so this
   *  is the only place it is heard. Not a wheel a nested box takes, nor a key a focused control acts on. */
  const resumeAtEnd = (list: HTMLElement) => {
    if (followingRef.current || provisionalStopRef.current !== null) return;
    if (list.scrollHeight - list.scrollTop - list.clientHeight > AT_BOTTOM_PX) return;
    followingRef.current = true;
    restoredTopRef.current = null;
    anchorRef.current = null;
  };
  const onScroll = () => {
    const list = listRef.current;
    if (list === null) return;
    reconcileWidth(list);
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
    const restored = restoredTopRef.current;
    const holding = restored !== null && Math.abs(list.scrollTop - restored) <= AT_BOTTOM_PX;
    const upward = upwardFromRef.current;
    if (upward !== null && (!steering() || list.scrollTop < upward - AT_BOTTOM_PX)) upwardFromRef.current = null;
    const upwardNotYetMoved = upwardFromRef.current !== null;
    if (!holding) restoredTopRef.current = null;
    const moved = list.scrollTop !== lastScrollTopRef.current;
    if (distance <= AT_BOTTOM_PX) {
      if (!gestureNotYetMoved && !upwardNotYetMoved && !holding) {
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
    if (followingRef.current) anchorRef.current = null;
    else if (moved && !holding) recordAnchor();
    updatePill();
    updateHeader();
  };
  onScrollRef.current = onScroll;

  // The intent signals, attached once to the list itself. Passive: none of them cancels anything.
  useLayoutEffect(() => {
    const list = listRef.current;
    if (list === null) return;
    const steer = () => {
      steerUntilRef.current = performance.now() + STEER_WINDOW_MS;
    };
    /** An upward wheel or key the list can take: provisionally stops a following view, and protects a view that
     *  already was not following from reading the end as reached before the gesture's movement arrives. */
    const goingUp = () => {
      if (followingRef.current) stopProvisionally(list);
      else upwardFromRef.current = list.scrollTop;
    };
    // Only a mostly vertical wheel up that the list itself can take stops following, and only
    // provisionally. A sideways swipe's jitter, a wheel an inner box takes and a wheel with nothing
    // above only steer: if the list does move, its scroll event says so.
    const onWheel = (event: WheelEvent) => {
      steer();
      if (event.deltaY < 0 && Math.abs(event.deltaY) > Math.abs(event.deltaX) && listTakesUpward(list, event.target)) {
        goingUp();
      } else if (event.deltaY > 0 && Math.abs(event.deltaY) > Math.abs(event.deltaX) && listTakesDownward(list, event.target)) {
        resumeAtEnd(list);
      }
    };
    // The browser's own scroll keys, from a control inside the list. In a text box they move the
    // caret, so they only steer there (a caret reveal can scroll the list, either way).
    const onKeyDown = (event: KeyboardEvent) => {
      const direction = nativeScrollKeyDirection(event);
      if (direction === null) return;
      steer();
      if (direction === "up" && !isTextEntry(event.target) && listTakesUpward(list, event.target)) {
        goingUp();
      } else if (event.key === "End" && !isControl(event.target) && listTakesDownward(list, event.target)) {
        resumeAtEnd(list);
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
      restoredTopRef.current = null;
      upwardFromRef.current = null;
      if (settleTimerRef.current !== null) clearTimeout(settleTimerRef.current);
      settleTimerRef.current = null;
      follow(list);
      onScrollRef.current();
    };
    list.addEventListener("wheel", onWheel, { passive: true });
    // A touch page has no keys to steer the follow with.
    if (!touchRef.current) list.addEventListener("keydown", onKeyDown, { passive: true });
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
  /* Under `touch`, older turns arrive above what is being read: the view keeps its place by moving down as far as
     the list grew, when the old first row is still there (a prepend, not a new conversation) and the view is not
     following the tail. Runs before the follow effect below, every render, and records the height it left. */
  const topRowRef = useRef<{ key: string; height: number } | null>(null);
  useLayoutEffect(() => {
    const list = listRef.current;
    if (!touch || list === null) return;
    const first = timeline[0]?.key;
    const held = topRowRef.current;
    if (held !== null && first !== undefined && first !== held.key && !followingRef.current &&
      timeline.some((item) => item.key === held.key)) {
      list.scrollTop += list.scrollHeight - held.height;
      lastScrollTopRef.current = list.scrollTop;
    }
    topRowRef.current = first === undefined ? null : { key: first, height: list.scrollHeight };
  });
  /** Under `touch`, tells the page the top is reached: by a scroll that came within `TOP_REACHED_PX` of it, or
   *  (`fill`) because the rows do not fill the view, so no scroll could ever reach it. */
  const reachTop = (fill: boolean) => {
    const list = listRef.current;
    if (!touch || list === null || onReachTop === undefined) return;
    if (fill ? list.scrollHeight - list.clientHeight <= TOP_REACHED_PX : list.scrollTop <= TOP_REACHED_PX) onReachTop();
  };
  /** Under `touch`, a tap on a row that folds opens or closes it; not a tap that a link, a control or a selection
   *  owns, and not one a handler above already took. */
  const tapRow = (target: Element, taken: boolean) => {
    const list = listRef.current;
    if (list === null || onToggle === undefined || taken) return;
    if (target.closest("a[href], button, input, textarea, select") !== null) return;
    const selection = window.getSelection();
    if (selection !== null && !selection.isCollapsed) return;
    const stop = stopOf(list, target);
    if (stop === null) return;
    const item = timeline[conversationRows(list).indexOf(stop)];
    if (item !== undefined && TAP_TOGGLES.has(item.kind)) onToggle(item.key);
  };

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
    updateHeader();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);
  // After the follow: a list that does not fill the view has reached its top already.
  useLayoutEffect(() => {
    reachTop(true);
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
    // The header names its prompt by index: a run opened or closed above it moves every index.
    updateHeader();
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
    updateHeader();
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
  // The reader's point follows the cursor (its row is the anchor while on screen), and a tab's rows are its own.
  useLayoutEffect(() => {
    restoredTopRef.current = null;
    recordAnchor();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cursor, tab]);
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
  const rowHandlersRef = useRef<RowHandlers>({ onAnswerPermission });
  rowHandlersRef.current = { onAnswerPermission };

  // By the transcript's length and newest row, not by the list: a streamed batch replaces the list on every delta and
  // changes which rows there are on none of them.
  const newestReply = state.transcript[state.transcript.length - 1]?.seq;
  const footers = useMemo(
    () => {
      const lines = new Map(workedFooters(state, turnDurations));
      for (const [id, from] of Object.entries(state.stoppedFrom ?? {})) {
        const ending = state.turnEndings.find((ending) => ending.turnId === id);
        const task = state.backgroundTasks.rows.find((row) => row.task_id === id);
        const call = state.toolCalls.find((call) => call.toolUseId === id);
        const anchor = ending?.seq ?? call?.seq ?? (task ? state.toolCalls.find((call) => call.toolUseId === task.tool_use_id)?.seq : undefined)
          ?? turnDurations.get(id)?.anchorSeq;
        if (anchor !== undefined && anchor !== null) {
          const label = touch && thisDevice !== null && from.deviceId === thisDevice ? "stopped from this phone"
            : `stopped from ${revealHidden(from.name).map((piece) => "text" in piece ? piece.text : piece.escape).join("")}`;
          lines.set(anchor, lines.has(anchor) ? `${lines.get(anchor)} · ${label}` : label);
        }
      }
      return lines;
    },
    [state.activeTurnId, state.turnEndings, state.transcript.length, newestReply, state.thinking, state.toolCalls, state.pendingPermissions, state.editorCards, state.stoppedFrom, state.backgroundTasks, turnDurations, touch, thisDevice],
  );
  // By row key, over the rows actually drawn: a turn's newest row may be a hidden or folded thought.
  const placedFooters = useMemo(() => placeFooters(timeline, footers), [timeline, footers]);

  // The reply the agent is still writing: it draws only what changed (`StreamingMarkdown`).
  const streamingKey = state.assistantMessageOpen && newestReply !== undefined ? `m-${newestReply}` : null;
  const awaitingPermission = new Set(allCards(state).map((p) => p.toolUseId).filter(isUsableLink));
  /** Each row's element, kept with the values it was made from: the same element back tells React the row has not
   *  changed, so a batch does not look at a finished row's props. By item, which outlives no row. */
  const heldRows = useRef(new WeakMap<TimelineItem, HeldRow>()).current;
  const headerItem = header === null ? undefined : timeline[header.prompt.index];

  return (
    <>
      <div
        className="message-list"
        data-focused={String(focused)}
        data-visual={visual ?? undefined}
        ref={listRef}
        onScroll={touch ? () => { onScroll(); reachTop(false); } : onScroll}
        // A click on a path -- the tool registry's own `.path-link` spans, or an inline code
        // span in an assistant reply that happens to parse as one. Never a fenced code BLOCK
        // (`pre code`): that is a real snippet, not a path, and HINT's own `y` already owns copying
        // it.
        onClick={(event) => {
          if (touch) {
            tapRow(event.target as Element, event.defaultPrevented);
            return;
          }
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
        <TouchRows.Provider value={touch}>
        {state.history !== null && (touch || DeskHistoryNotice === null
          // The desk's notice copies the desk's file path to the desk's own selection; the phone only reads it.
          ? <div className="history-notice"><span>{historyNoticeText(state.history)}</span></div>
          : <DeskHistoryNotice notice={state.history} />)}
        {(historyFromTurn ?? state.evicted?.turns ?? 0) > 0 &&
          <div className="history-notice" data-nav-stop={!touch && import.meta.env.EITRI_PHONE_PAGE !== true ? "notice" : undefined}>
            {!touch && import.meta.env.EITRI_PHONE_PAGE !== true && onHistoryOlder
              ? <button className="history-load" onClick={onHistoryOlder}>{earlierTurnsText(historyFromTurn ?? state.evicted!.turns)}</button>
              : <span>{earlierTurnsText(historyFromTurn ?? state.evicted!.turns)}</span>}
          </div>}
        {timeline.flatMap((item, index) => {
          const newer = Math.max(0, (state.evicted?.turns ?? 0) - (historyToTurn ?? state.evicted?.turns ?? 0));
          const gap = newer > 0 && !item.key.startsWith("h:") && (index === 0 || timeline[index - 1].key.startsWith("h:"))
            ? <div key="history-gap" className="history-notice history-gap">{newer} newer {newer === 1 ? "turn" : "turns"} not loaded.</div> : null;
          // Everything a row draws is in these props, each a value or the timeline's own item, so a
          // cursor move renders the two rows it changes and no other (`TimelineRow`).
          const historical = item.key.startsWith("h:");
          const call = item.kind === "tool" ? item.call : null;
          const permissionId = item.kind === "permission" ? item.request.permissionId : null;
          const footer = placedFooters.get(item.key) ?? null;
          const abandoned = call !== null && (historical ? call.result === null : isAbandonedCall(call, state, sessionEnded));
          const source = item.kind === "prompt" ? state.sentFrom?.[String(item.seq)] ?? null : null;
          const sentHere = touch && source !== null && thisDevice !== null && source.deviceId === thisDevice;
          const props: TimelineRowProps = {
            item,
            lateLine: item.kind === "prompt" ? lateLines?.find(line => line.seq === item.seq && line.prompt === (item.promptText ?? item.text))?.text : undefined,
            sentFrom: sentHere ? null : source?.name ?? null,
            sentHere,
            atDesk: touch ? deskLocation : null,
            current: index === cursor,
            yanked: item.key === yankedKey,
            expanded: expanded[item.key] === true,
            detailed,
            thinkingDisplay,
            sessionEnded,
            cardLine: call === null || historical ? null : rowCardLine(call, state),
            agent: call !== null && isAgentCall(call) ? (historical ? agentCard(call, undefined, abandoned) : rowAgentCard(call, state, abandoned)) : null,
            abandoned,
            gated: !historical && call !== null && awaitingPermission.has(call.toolUseId),
            ruleOffer: permissionId === null ? null : (ruleOffers?.[permissionId] ?? null),
            alreadyAnswered: permissionId !== null && (answeredPermissions?.has(permissionId) ?? false),
            cardMode: permissionId !== null && cardMode?.permissionId === permissionId ? cardMode.view : null,
            handlers: rowHandlersRef,
            ranFor: item.kind === "ending" ? endingDuration(turnDurations, item.ending.turnId) : null,
            streaming: item.key === streamingKey && !item.depth,
            selecting: visual !== null,
          };
          const cutBytes = touch ? cut?.get(item.seq) ?? null : null;
          const waiting = cutBytes !== null && cutWaiting === item.seq;
          // An opened run's calls stand in rows of their own, the run's row gone: under `touch` the first of them
          // carries the tap that folds them back into it (the run's key is its first call's).
          const foldsRun = touch && item.kind === "tool" && expanded[`r-${item.seq}`] === true;
          const held = heldRows.get(item);
          if (held !== undefined && held.footer === footer && held.cutBytes === cutBytes && held.waiting === waiting &&
            held.foldsRun === foldsRun && sameRowProps(held.props, props)) {
            return [gap, held.element];
          }
          const row = <TimelineRow {...props} />;
          const element = (
            <Fragment key={item.key}>
              {foldsRun && (
                // Not a row, like the cut entry's button: a tap here folds the run and opens nothing else.
                <button type="button" className="fold-run" onClick={(event) => {
                  event.stopPropagation();
                  toggleRef.current?.(`r-${item.seq}`);
                }}>
                  ▴ these calls · tap to fold them
                </button>
              )}
              {item.depth ? <div className="subagent-child" style={{ marginInlineStart: `${item.depth * 1.5}em` }} data-depth={item.depth}>{row}</div> : row}
              {cutBytes !== null && (
                // Not a row: no `data-nav-stop`, so a tap on it never also opens the row above.
                <button type="button" className="cut-entry" onClick={(event) => {
                  event.stopPropagation();
                  loadWholeRef.current?.(item.seq);
                }}>
                  {waiting
                    ? `Only the start is here · ${cutSize(cutBytes)} in all · loads once the older turns arrive`
                    : `Only the start is here · ${cutSize(cutBytes)} in all · tap to load it whole`}
                </button>
              )}
              {footer !== null && (
                // Chrome, not a row: no `data-nav-stop`, so `j`/`k` and `y` never see it, and the inner
                // `aria-hidden` span is what VISUAL's copy leaves out (`VISUAL_CHROME`).
                <div className="turn-time" data-turn-time="">
                  <span aria-hidden="true">{footer}</span>
                </div>
              )}
            </Fragment>
          );
          heldRows.set(item, { props, footer, cutBytes, waiting, foldsRun, element });
          return [gap, element];
        })}
        {after}
        </TouchRows.Provider>
        {/* The cursor's fill, behind the rows (`../cursorBand`). Hidden from assistive technology and from
            VISUAL's selection (`[aria-hidden="true"]` is chrome there). A touch page has neither. */}
        {!touch && <div className="cursor-band" aria-hidden="true" ref={bandElRef} />}
        {/* VISUAL and V-LINE's selection (`../visualPaint`), over the rows. */}
        {!touch && <div className="visual-paint" aria-hidden="true" ref={paintElRef} />}
      </div>
      {/* The prompt the reader is under, over the top of the list while it is scrolled up (`../promptHeader`).
          The list's sibling, never one of its children: it takes no room in the list, so nothing moves when it
          comes or goes, and no row walk, copy or HINT ever sees it. Pointer only: it takes no focus (the
          mousedown that would move focus or start a selection is cancelled), and the keys reach that prompt
          by `[[`. Chrome, so hidden from assistive technology like the cursor band: the row it names is read
          there. */}
      {headerItem !== undefined && headerItem.kind === "prompt" && header !== null && (
        <div
          className={PROMPT_HEADER_CLASS}
          aria-hidden="true"
          style={header.gutter > 0 ? { right: header.gutter } : undefined}
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => onPromptJump?.(headerItem.key)}
        >
          <span className="prompt-header-text">{headerText(headerItem.text)}</span>
          <span className="prompt-header-count">{headerCount(header.prompt)}</span>
        </div>
      )}
    </>
  );
}

/** The index of the first conversation row on screen in `list` -- the first whose bottom is below the top of what
 *  shows and whose top is above the list's bottom -- or `-1` with none, or no layout to judge (jsdom, a list not
 *  laid out yet). The top of what shows is the list's top plus the header's height, whether or not the header is
 *  drawn at the moment (`headerBand`): were it measured from where the header happens to be, a row ending inside
 *  the band would be the top row while the header is hidden and a different one once it is shown, and the header
 *  would flip with every scroll event. */
function firstRowOnScreen(list: HTMLElement): number {
  const l = list.getBoundingClientRect();
  if (!(l.bottom - l.top > 0)) return -1;
  return firstRowBelow(conversationRows(list), l.top + headerBand(list), l.bottom);
}

type RowHandlers = {
  onAnswerPermission: Props["onAnswerPermission"];
};

/** What decides which elements are the list's direct children: its rows in order, the rows a turn's time stands under
 *  (by row key), and the history notice. */
type RowShape = { rows: TimelineItem[]; footers: ReadonlyMap<string, string>; notice: boolean };

function sameRowShape(a: RowShape, b: RowShape): boolean {
  if (a.notice !== b.notice || a.rows.length !== b.rows.length || a.footers.size !== b.footers.size) return false;
  for (const key of a.footers.keys()) if (!b.footers.has(key)) return false;
  return a.rows === b.rows || a.rows.every((item, i) => item.key === b.rows[i].key && item.depth === b.rows[i].depth);
}

type HeldRow = {
  props: TimelineRowProps; footer: string | null; cutBytes: number | null; waiting: boolean; foldsRun: boolean;
  element: ReactElement;
};

/** Every prop by name, so a prop added to `TimelineRowProps` without a line here fails to compile. */
function sameRowProps(a: TimelineRowProps, b: TimelineRowProps): boolean {
  if (a.sentFrom !== b.sentFrom || a.sentHere !== b.sentHere || a.atDesk !== b.atDesk) return false;
  const {
    lateLine, sentFrom, sentHere, atDesk, item, current, yanked, expanded, detailed, thinkingDisplay, sessionEnded, abandoned, cardLine, agent, gated,
    ruleOffer, alreadyAnswered, cardMode, ranFor, handlers, streaming, selecting, ...none
  } = a;
  const _exhaustive: Record<string, never> = none;
  void _exhaustive;
  return (
    lateLine === b.lateLine && item === b.item && current === b.current && yanked === b.yanked && expanded === b.expanded &&
    detailed === b.detailed && thinkingDisplay === b.thinkingDisplay && sessionEnded === b.sessionEnded &&
    abandoned === b.abandoned && cardLine === b.cardLine && agent === b.agent && gated === b.gated &&
    ruleOffer === b.ruleOffer && alreadyAnswered === b.alreadyAnswered && cardMode === b.cardMode && ranFor === b.ranFor &&
    handlers === b.handlers && streaming === b.streaming && selecting === b.selecting
  );
}

type TimelineRowProps = {
  lateLine?: string;
  sentFrom: string | null;
  /** This phone sent the prompt (the touch page). */
  sentHere: boolean;
  /** Where cards and questions are answered, on a page that shows them without controls; `null` on the desk. */
  atDesk: string | null;
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
  /** A card row holding card mode: how its card is drawn there (`PermissionCard`'s `cardMode`). */
  cardMode: CardModeView | null;
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
  lateLine,
  sentFrom,
  sentHere,
  atDesk,
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
  cardMode,
  ranFor,
  handlers,
  streaming,
  selecting,
}: TimelineRowProps) {
  rowRenders += 1;
  const projectDir = useContext(ProjectDirContext);
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
        <Row kind="prompt" sign="›" current={current} className={promptClass || undefined} navStop="row" rowKey={item.key}>
          {item.promptText ?? item.text}
          {lateLine && <div className="queue-note"><Revealed pieces={revealHidden(lateLine)} /></div>}
          {item.images && <div className="prompt-images">{revealedText(item.images)}</div>}
          {sentHere && <div className="phone-source"><small>sent from this phone</small></div>}
          {sentFrom !== null && <div className="phone-source"><small>from <Revealed pieces={revealHidden(sentFrom)} /></small></div>}
        </Row>
      );
    }
    case "message": {
      if (item.question || item.plain) {
        const lines = item.question ? questionLines(item.question) : item.text.split("\n").map((text, i) => ({ text, muted: i > 0 }));
        return <Row kind="question" sign="?" current={current} className={yanked ? "row-yanked" : undefined} navStop="row" rowKey={item.key}>
          {lines.map((line, i) => <div key={i} className={line.muted ? "question-line question-status" : "question-line"}>
            <Revealed pieces={revealHidden(line.text)} />
          </div>)}
        </Row>;
      }
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="assistant" sign="" current={current} className={yankedClass} navStop="row" rowKey={item.key}>
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
        return <Row kind="subagent" sign={toolSign(item.call, abandoned)} current={current} className={yanked ? "row-yanked" : undefined} navStop="row" rowKey={item.key}>
          <div className="subagent-heading"><Revealed pieces={revealHidden(agent.header)} />{!agent.background && <span className="fold-marker" aria-label={whole ? "expanded" : "collapsed"}>{whole ? "▾" : "▸"}</span>}</div>
          {agent.warnings.map((warning, i) => <div key={i} className="subagent-warning"><Revealed pieces={revealHidden(warning)} /></div>)}
          <div className="subagent-status"><Revealed pieces={revealHidden(agent.line)} /></div>
          {whole && agent.report && <div className="subagent-report"><Revealed pieces={revealHidden(agent.report)} /></div>}
        </Row>;
      }
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="tool" sign={toolSign(item.call, abandoned)} current={current} className={yankedClass} navStop="row" rowKey={item.key}>
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
              projectDir,
            })}
          </div>
        </Row>
      );
    }
    case "permission": {
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="permission" sign="!" current={current} className={yankedClass} navStop="row" rowKey={item.key}>
          <PermissionCard
            request={item.request}
            controls={atDesk === null}
            deskLocation={atDesk ?? ""}
            sessionEnded={sessionEnded}
            ruleOffer={ruleOffer}
            alreadyAnswered={alreadyAnswered}
            cardMode={cardMode}
            onAnswer={(...args) => handlers.current.onAnswerPermission(...args)}
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
        <Row kind="ending" sign="·" current={current} className={yankedClass} navStop="row" rowKey={item.key}>
          {/* The reset time the ending itself kept: a later limit report never changes an older row. */}
          {turnEndingText(item.ending, item.ending.resetsAt ?? null, ranFor)}
        </Row>
      );
    }
    case "plan": {
      // Claude's plan: its first heading and how it ended, the whole plan behind the row's fold (Enter), drawn as
      // the dock draws it -- sanitized Markdown with its hidden characters shown. The rest is text.
      const lines = planLines(item.plan);
      const whole = (expanded || detailed) && item.plan.plan !== "";
      return <Row kind="plan" sign="≡" current={current} className={yanked ? "row-yanked" : undefined} navStop="row" rowKey={item.key}>
        {lines.map((line, i) => <div key={i} className={line.muted ? "question-line question-status" : "question-line"}>
          <Revealed pieces={revealHidden(line.text)} />
          {i === 0 && item.plan.plan !== "" && <span className="fold-marker" aria-label={whole ? "expanded" : "collapsed"}>{whole ? " ▾" : " ▸"}</span>}
        </div>)}
        {whole && <PlanBody text={item.plan.plan} />}
      </Row>;
    }
    case "planmark":
      return <Row kind="notice" sign="·" current={current} className={yanked ? "row-yanked" : undefined} navStop="row" rowKey={item.key}>
        <div className="notice-text">{item.text}</div>
      </Row>;
    case "wake": {
      return (
        <Row kind="wake" sign="●" current={current} className={yanked ? "row-yanked" : undefined} navStop="row" rowKey={item.key}>
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
        <Row kind="notice" sign={warning ? "⚠" : shell ? "⎿" : "·"} current={current} className={[yankedClass, warning ? "notice-prominent" : "", item.notice.kind === "compaction" ? "notice-compaction" : ""].filter(Boolean).join(" ")} navStop="row" rowKey={item.key}>
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
        <Row kind="thinking" sign="∴" current={current} className={yankedClass} navStop="row" rowKey={item.key}>
          <ThinkingBody block={item.block} open={item.open} level={thinkingDisplay} expanded={expanded} detailed={detailed} />
        </Row>
      );
    }
    case "run": {
      // A run replaces its calls with one line, `Read ×3 · Bash ×2`; `Enter`
      // (`toggle-expand`, keyed on this row's OWN key) puts them back.
      const yankedClass = yanked ? "row-yanked" : undefined;
      return (
        <Row kind="tool-run" sign="✓" current={current} className={yankedClass} navStop="row" rowKey={item.key}>
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
  let end = text.length;
  while (end > 0 && text[end - 1] === "\n") end--;
  if (!(count > 0) || !Number.isInteger(count)) return text.slice(0, end).split("\n").slice(-count).join("\n");
  let start = end;
  for (let line = 0; line < count; line++) {
    if (start === 0) return text.slice(0, end);
    start = text.lastIndexOf("\n", start - 1);
    if (start < 0) return text.slice(0, end);
  }
  return text.slice(start + 1, end);
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
  // A thought of only spaces and line breaks has nothing to count: "0 lines" or "3 lines" of nothing
  // would both mislead, so its head says just `Thought`.
  const { lines, counted } = useMemo(() => open ? { lines: 0, counted: false } : {
    lines: thoughtLineCount(block.text), counted: block.text.trim() !== "",
  }, [open, block.text]);
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
  const boxRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const box = boxRef.current;
    if (box) attachDiagrams(box);
    return () => { if (box) detachDiagrams(box); };
  }, [html]);
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
  return <div ref={boxRef} dangerouslySetInnerHTML={{ __html: html }} />;
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
    attachDiagrams(box);
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
      if (box !== null) attachDiagrams(box);
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
  useLayoutEffect(() => {
    const box = boxRef.current;
    return () => { if (box) detachDiagrams(box); };
  }, []);
  useEffect(() => stopResting, []);
  return <div ref={boxRef} />;
});

/** A plan unfolded in its row, drawn as the dock draws it, once per text; past the escape cap, as escaped text. */
const PlanBody = memo(function PlanBody({ text }: { text: string }) {
  const html = renderMarkdownRevealed(text);
  return html === null ? <pre className="plan-row-body plan-text">{revealedText(text)}</pre>
    : <div className="plan-row-body" dangerouslySetInnerHTML={{ __html: html }} />;
});
