import { useLayoutEffect, useMemo, useRef, useState } from "react";
import type { AgentUiState, ToolCallRecord } from "../types";
import { renderToolCall } from "../toolRegistry";
import { buildTimeline, isUsableLink } from "../timeline";
import { buildDisplay, runSummary } from "../display";
import { renderMarkdown } from "../markdown";
import { turnEndingText } from "../turnEnding";
import { HistoryNotice } from "./HistoryNotice";
import { PermissionCard } from "./PermissionCard";
import { Row } from "./Row";
import type { PermissionDecision } from "../bridge";
import { noteUserScroll, RESUME_FOLLOW_EVENT, USER_SCROLL_EVENT, type UserScrollDirection } from "../follow";
import { pillLabel, pillShown } from "../pill";
import { parsePath } from "../paths";
import type { PathRef } from "../paths";

type Props = {
  state: AgentUiState;
  /** The session is gone. Pending cards stay visible but can no longer submit into it. */
  sessionEnded: boolean;
  /** Which rows are expanded, keyed by the timeline `key`. Owned by `App.tsx` because `Enter` acts
   *  on the cursor, and the cursor is App's -- see `App.tsx`'s `onKeyDown`, `toggle-expand` arm. A
   *  run's own key expanding is what un-collapses it (P2, `display.ts`). */
  expanded: Record<string, boolean>;
  /** The index into THIS component's own `timeline` (below) that `Enter`/`y` act on in `App.tsx`.
   *  Safe to compare by position rather than by identity: both here and there, `timeline` is
   *  `buildDisplay(buildTimeline(state), …)` over the same `state`, `expanded` and `detailed`, a pure
   *  function, so the two computations always agree on what sits at a given index for a given input
   *  even though each holds its own copy. */
  cursor: number;
  /** R3 (`Ctrl+o`): every result shown in full, at wider cuts, with no collapsed runs. Per tab
   *  (`App.tsx`'s `TabViewState.detailed`); defaults to `false`, the state every fresh tab starts
   *  in. */
  detailed?: boolean;
  /** Whether the panel has the keyboard (the `pane_focus` envelope). Drives the cursor's solid
   *  and hollow states in `index.css`; see the `.row-current .row-sign` rule there. Defaults to
   *  `true`, the state with no host to say otherwise; `App.tsx` always passes it explicitly. */
  focused?: boolean;
  /** K07: a HINT landing on a code block is "the item" until the next key; drawn as the block's own
   *  outline (`data-hint-landed`, set on the `pre` by `App.tsx`, since the block lives in markdown HTML)
   *  and, through this, a hollow sign on the current row -- one solid mark at a time. */
  codeLanded?: boolean;
  /** D7's third button: `permissionId -> "<words> *"`, exactly the rules Rust offered for the
   *  cards currently on screen (`rule_offers`). Absent/no entry means no rule -- the card never
   *  invents its own suggestion. */
  ruleOffers?: Record<string, string>;
  /** N3: the row just yanked, so it can flash (`.row-yanked`). `App.tsx` owns the timer; this only
   *  reads the current key. */
  yankedKey?: string | null;
  onAnswerPermission: (permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) => void;
  /** v1 hardening (ruling R2): the cards already answered from this panel, by `permissionId` --
   *  `App.tsx` owns the set, since `a`/`d` answer a card by its id rather than through its buttons.
   *  Absent: none. */
  answeredPermissions?: ReadonlySet<string>;
  /** Each card's reason box as it is typed into, so `App.tsx`'s `d` can send it (ruling R2). */
  onPermissionReason?: (permissionId: string, reason: string) => void;
  /** N2: a click on a path link or an inline code span that parses as a path opens it -- the same
   *  target `gf` reaches from the keyboard (`App.tsx`'s `open-path` action). Absent, clicks inside
   *  the list do nothing beyond React's own defaults (a permission card's buttons, say). */
  onOpenPath?: (ref: PathRef) => void;
  /** R2's pill, reported upward rather than floated over the last line here (panel round 2 plan,
   *  Task 10, spec §5.1: "the band's right, `↓N` inverted; never over text"). `label` is the band's
   *  compact form -- `↓N`, `↓ ⚑` when a card is below the view, or bare `↓` while shown with
   *  nothing counted yet (`pillShown`'s own "far from the bottom, nothing new" case) -- `null` when
   *  the pill should not show at all. `jump` is this call's own `jumpToEnd`, safe to invoke any time
   *  after this fires (it reads `listRef` fresh, not a stale snapshot). `afterSeq` is the current
   *  `seq` threshold itself (wave 3, Task 3) -- the largest `seq` on screen when following last
   *  stopped, `null` while following -- reported so a caller can save it across a tab switch
   *  (`App.tsx`'s `TabViewState.unseenAfterSeq`) and hand it back as `unseenSeed` below. Called every
   *  time this component recomputes the pill (`updatePill`, below), including with `null` -- the
   *  caller (the band's `unread` fact) is expected to just hold the latest value, the same way
   *  `pill`/`setPill` already do internally. */
  onUnreadChange?: (label: string | null, jump: () => void, afterSeq: number | null) => void;
  /** Seeds the unread threshold back in on a tab switch (wave 3, Task 3): a tab switch reuses this
   *  component, so without this the first `updatePill` after a restore falls back to counting from
   *  the CURRENT timeline length, and a row that arrived while the tab was away is never counted --
   *  the defect this task fixes. `afterSeq` is the seq threshold to seed (`TabViewState.unseenAfterSeq`
   *  from the tab just switched TO); `tick` distinguishes one seed from the next so a second switch
   *  back to the same `afterSeq` still re-fires the effect. `null` for a tab with no saved threshold
   *  (never parked, or restored at the bottom) -- nothing to seed. */
  unseenSeed?: { afterSeq: number; tick: number } | null;
  /** Visual-mode spec D1/D14 (revised for 3a, §9): `"caret"`/`"visual"`/`"vline"` while one is on,
   *  `null`/absent in BROWSE. Drives `data-visual` on the list (`index.css`'s `::selection`/
   *  `.row-current` overrides) -- nothing else here reads it, since the region's own selection is
   *  built directly on the DOM by `App.tsx`, not through props. */
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
  // A result only ever arrives inside the call's own turn (both backends stamp every event of a
  // turn with its id). Once that turn is not the active one -- interrupted, failed, or followed by
  // another -- it never will (Codex, whole-branch review: an interrupted turn left `◐ Running…`
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

/** Writes the list's content-box inline size, in px, to `--list-inline-size` on the list itself --
 *  what `index.css`'s `.row` derives `--row-inline-size` from, and every wide-content escape and the
 *  prompt's inset read. Skips the write when nothing changed, so an unchanged size never restyles
 *  the conversation. */
function writeListInlineSize(list: HTMLElement, contentWidth: number): void {
  const value = `${Math.max(0, contentWidth)}px`;
  if (list.style.getPropertyValue("--list-inline-size") !== value) {
    list.style.setProperty("--list-inline-size", value);
  }
}

export function MessageList({
  state,
  sessionEnded,
  expanded,
  cursor,
  detailed = false,
  focused = true,
  codeLanded = false,
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

  /* The list's width, measured here rather than queried by CSS (2026-09-24).

     Until today `.row` was a size query container (`container-type: inline-size`) so that code,
     diffs and tool output could reach the row's right edge through `100cqw`. In WebKitGTK 2.52.6
     that made the engine reset this list's `scrollTop` to a stale value every time the status line's
     elapsed counter ticked while a reply streamed -- measured, and the whole of the owner's "看不到
     最下面输出" (`.superpowers/panel-scroll/root-cause.md`, not in git, summarised in the dated
     record's 2026-09-24 (later) entry; `shell/tests/panel_stream_scroll.rs` is the regression test,
     against the real engine). A container on `.row-body`, on this list, or on the assistant rows
     only was each measured to break the same way, so the width now comes from here: measured once
     before the first paint, and again by a `ResizeObserver` only when the list's own box changes
     size -- never per streamed delta, since content growth does not resize a scroll container. `index.css` falls back to "no escape" until the first write, and so does
     jsdom, which has neither layout nor `ResizeObserver`.

     Correction (the GUI pass, 2026-09-24, its finding F-a): this observer only wrote the width, and
     the follow snap below ran only on a state change -- so a resize with nothing streaming behind it
     left a following view short: narrowing the panel reflows the reply taller while `scrollTop` stays
     put (482px short after a divider drag, 651px after an unzoom, measured in the sandbox), and a
     shorter list (the bottom terminal shown, the composer growing, the which-key strip appearing as
     INPUT becomes BROWSE) hides the tail the same way, for as long as a tool call runs, a permission
     card waits, or the turn is over. The same observer now also watches every child of the list (the
     rows), because the rows can change size with the list's box unchanged -- `--prose-measure`, say,
     or anything else restyling the conversation alone. (A font push from the theme, the
     investigation's F5, is caught either way: the status line and the composer grow with the same
     `--nv-font-size`, so the list's own box gets shorter too -- measured, S9 with the rows left
     unobserved failed only on the prose measure.) Either way, while following, the
     callback snaps the view to its end: after layout and before paint, the same frame. Snapping is
     all it adds; a parked reader, or a pending provisional stop, is left exactly where it is.
     `shell/tests/panel_stream_scroll.rs`'s S9 is the real-engine test. */
  useLayoutEffect(() => {
    const list = listRef.current;
    if (list === null) return;
    const style = getComputedStyle(list);
    writeListInlineSize(list, list.clientWidth - parseFloat(style.paddingLeft || "0") - parseFloat(style.paddingRight || "0"));
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      // `contentRect` is the content box, which excludes the padding and a vertical scrollbar --
      // the same box the rows are laid out across. Only the list's own entry carries its width.
      for (const entry of entries) {
        if (entry.target === list) writeListInlineSize(list, entry.contentRect.width);
      }
      // As the follow effect does, read the position first: a scroll whose event has not arrived yet
      // still decides whether this is a view that is following. Through the ref: see `onScrollRef`.
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
      if (!observed.has(el)) {
        observer.observe(el);
        observed.add(el);
      }
    }
  };

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
  //
  // **Correction (2026-09-24): following is ended by what the user DID, never by which way an
  // unexplained scroll moved.** The paragraph above rests on "content growth does not produce
  // [scroll events]", and WebKitGTK broke it: while `.row` was a size query container, the engine
  // itself dropped `scrollTop` on every tick of the status line's clock mid-reply, and this code read
  // each drop as the reader scrolling up and stopped following for the rest of the reply -- the
  // owner's "看不到最下面输出" (dated record, 2026-09-24 (later)). The container is gone; this is the
  // hardening, so that the next engine-originated clamp cannot do the same:
  //
  // - Leaving the bottom takes an intent signal: a wheel moving up (any distance -- the old rule's
  //   accidental "a 2px wheel stops following" is now the deliberate rule), or a scroll that goes up
  //   while the user is STEERING -- a pointer held on the list (its scrollbar, or a selection being
  //   dragged), within `STEER_WINDOW_MS` of a wheel or a touch drag, or after the panel's own
  //   `j`/`k`/`Ctrl+d`/`Ctrl+u`/`G`/`gg`/HINT scrolls, which `App.tsx` announces with
  //   `noteUserScroll` (`../follow.ts`) before it scrolls. `k`, `Ctrl+u` and `gg` say "up", which ends
  //   following at once.
  // - Any scroll event that finds the view at the true bottom (`AT_BOTTOM_PX`) re-arms following,
  //   whoever caused it; a user scroll that ends within `BOTTOM_FOLLOW_THRESHOLD_PX` of it going DOWN
  //   re-arms too, as before.
  // - Anything else -- a clamp, a page write, the snap below, a smooth scroll in flight -- moves
  //   nothing here. While following, the effect below re-snaps on the very next state change even if
  //   the view moved by itself, so an engine jump costs at most the time until the next delta.
  //
  // **Correction (fix round 1, the same day), from the fix's adversarial review.** The first version
  // of this list missed two things, and each was a regression against the base:
  //
  // - (3a) **A scroll up by a route the panel does not own was undone.** With a control inside the
  //   list focused (`l`, Tab, a click -- the `hjkl` model does that on purpose), PageUp, the arrows,
  //   Home and Shift+Space are the BROWSER's keys (`resolveKey` has no action for them), and their
  //   scroll carried none of the signals above, so the next delta put the reader back at the bottom
  //   -- a trap, measured in Chromium with a real PageUp; the base had left that reader alone. The
  //   dated record's claim that "nothing here produces one" was wrong for exactly this case. Such a
  //   key bubbling out of the list now steers, and an upward one stops following the way a wheel does
  //   (below); focus landing inside the list (a Tab reveal) steers too.
  // - (3b) **A wheel that never moved the list ended following.** Any `deltaY < 0` did it: a sideways
  //   touchpad swipe over a wide code block with a sub-pixel vertical jitter, a wheel an expanded tool
  //   result's own box took, a wheel with nothing above to scroll to. The base kept following in all
  //   three. A wheel (or key) now stops following only when it is mostly vertical, goes up, and the
  //   list itself can take it (`listTakesUpward`); anything else only steers, and the list's own scroll
  //   event -- if one comes -- decides.
  //
  //   And a stop made that way is PROVISIONAL: it is taken back once the gesture is over (the steering
  //   window has run out) if the list never went up from where the gesture found it -- the engine
  //   latched the gesture onto an inner box, or something swallowed it. Without the stop, a delta
  //   landing between the wheel and its (animated) scroll would snap the view and cancel the scroll;
  //   without taking it back, a misprediction would leave the reader at the bottom with following off.
  //   A scroll up seen while steering confirms the stop, as before; `k`/`Ctrl+u`/`gg` still stop
  //   following outright (the owner's design: `k` stops following).
  //
  //   The review warned against the other obvious fix, "a scroll up with no content change behind it is
  //   the user's": the measured engine drop came on a status-line change with no list content change,
  //   so that rule would read the original bug as the user again. Nothing here depends on content.
  //
  // **Correction (fix round 2, from the fix's re-review, M1).** The take-back above ran only inside
  // the follow effect below, which runs on a state change. A misprediction followed, inside the
  // gesture's window, by a new row and then silence -- a permission card, after which the turn waits
  // -- left that row below the view until something else arrived, which may be nothing. So round 1's
  // "a misprediction costs at most one steering window, and the next delta then snaps" was only true
  // when a next delta came. A pending stop now also has a timer for the end of its gesture
  // (`scheduleSettle`), which takes it back the same way and catches up with whatever the stop held
  // back: a smooth scroll to a new row, as the effect would have done, or a snap to the end of text
  // that grew. It re-arms itself while the gesture goes on (a later wheel extends the window), and a
  // pointer held on the list keeps the stop until it is released, whose own steering window then runs.
  //
  // **Correction (the GUI pass, 2026-09-24): following is a snap, always, and not only on a state
  // change.** Two findings of the sandbox GUI pass. (F-c) A new row used to get a smooth
  // `scrollIntoView`, which leaves it below the edge for the frame or two the animation takes: as a
  // reply's first row mounted, the user's own prompt was still the last row with text on screen,
  // which the checklist's "no frame" forbids (1-2 frames per new row, measured; S1(c) in
  // `shell/tests/panel_stream_scroll.rs` now holds every frame to the end). The snap happens before
  // paint, so a new row is in view in the first frame that shows it; the price is that a new row no
  // longer glides in. (F-a) The snap ran only when the state changed, so a resize with nothing
  // streaming left the view short -- see the `ResizeObserver` above, which now snaps too.

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
     and is pre-existing, larger, and nobody has profiled it.

     Declared here, above `onScroll` (moved for R2): the pill's `updatePill` closure, defined
     below alongside `onScroll`, reads `timeline` too, and both need the same binding in scope. */
  const timeline = useMemo(
    () => buildDisplay(buildTimeline(state), { expanded, detailed, turnRunning: state.activeTurnId !== null }),
    [state, expanded, detailed],
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
  /** The largest `seq` on screen when following last stopped; `null` while following (R2).
   *
   *  Wave 3, Task 3: this used to be a timeline INDEX (`unseenFromRef`, the timeline's length at the
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
    // The band's compact form (panel round 2 plan, Task 10): a card below wins over a count, a
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
  /** (M1) Settles a pending stop when its gesture ends, not only at the next state change. */
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
    // The order is the fix for a review finding (2026-09-19): the threshold used to be checked
    // FIRST, so a `k` step that ended within the threshold of the bottom turned following straight
    // back ON and the next streamed delta snapped the view back down. A user's scroll UP always
    // stops following unless it left the view at the TRUE bottom, which is also where the browser
    // clamps `scrollTop` after content shrinks (a resolved permission card's row going away) -- a
    // user at the bottom stays followed through that. The threshold only re-arms on a scroll that
    // went DOWN, never on the effect's own synchronous call below, where nothing moved.
    //
    // Fix round 2: an event that finds the list where a pending gesture found it says nothing about
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
      // A stop held open by the pointer is settled once this new window is over (M1).
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
    // Both the pointer and the mouse flavour, deliberately: whether WebKitGTK delivers either one to
    // the list for a press on its OWN scrollbar has not been checked on a screen
    // (`shell/MANUAL_VERIFICATION.md`, "The panel follows a streaming reply", item 8), and a
    // scrollbar drag that is not seen here is undone by the next delta.
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
  // (2026-09-24: `k` now also announces itself through `noteUserScroll`, which ends following before
  // it scrolls; this read still catches a user scroll inside the steering window whose event is late.)
  //
  // **A layout effect, not a passive one, since 2026-09-24** (the investigation's F6). A passive effect
  // runs after the browser has painted, so every streamed delta drew one frame with its new lines
  // below the fold before the snap caught up. Once removing the query container (see the width
  // measurement above) stopped the engine from parking the view, that lag was all
  // `shell/tests/panel_stream_scroll.rs`'s S1(b) could still see, and it failed there: the newest
  // text more than 50px out of view for 255ms and 424ms at a stretch in two of its four
  // configurations (the replay streams faster than frames paint, so consecutive frames each caught
  // a fresh delta un-snapped). Snapping before paint took the longest stretch to 35-48ms, one
  // frame at a new row's smooth scroll. The effect's body is unchanged; only when it runs moved.
  // Correction (fix round 1, from the review): that failure was seen once and NOT reproduced -- the
  // review's re-run of the same configuration without this change passed 16/16 at 73-85ms, and 255ms
  // against a 250ms limit says S1(b) was load-sensitive there. It stays because it cuts the lag: the
  // longest stretch measured with it is 30-50ms across the fix's own runs and the review's (the "35-48"
  // above disagreed with the dated record's "32-43"; both are inside that range).
  //
  // Correction (the GUI pass, 2026-09-24): this effect no longer tells a new row from growth by the
  // four lengths -- both snap now (see the correction above `followingRef`), so there is nothing left
  // to tell apart. It also puts each newly mounted row under the `ResizeObserver` (`observeChildren`),
  // which is what follows a row that changes size with no state change behind it.
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

  /** Wave 3, Task 3: seeds the unread threshold back in when `App.tsx` restores a parked tab's view.
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

  return (
    <>
      <div
        className="message-list"
        data-focused={String(focused)}
        data-code-landed={codeLanded ? "" : undefined}
        data-visual={visual ?? undefined}
        ref={listRef}
        onScroll={onScroll}
        // N2: a click on a path -- the tool registry's own `.path-link` spans, or an inline code
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
          const current = index === cursor;
          switch (item.kind) {
            case "prompt": {
              // Plain text, never markdown: this is what the user typed, and parsing it would render
              // their literal backticks and asterisks as formatting they did not ask for.
              const yanked = item.key === yankedKey ? "row-yanked" : undefined;
              return (
                <Row key={item.key} kind="prompt" sign="›" current={current} className={yanked} navStop="row">
                  {item.text}
                </Row>
              );
            }
            case "message": {
              const yanked = item.key === yankedKey ? "row-yanked" : undefined;
              return (
                <Row key={item.key} kind="assistant" sign="" current={current} className={yanked} navStop="row">
                  {/* `renderMarkdown` (`../markdown.ts`) is marked.parse, now with a `code` renderer
                      that runs highlight.js against nvim's own syntax colours, then DOMPurify.sanitize
                      -- one function so no caller can run half of it. */}
                  <div dangerouslySetInnerHTML={{ __html: renderMarkdown(item.text) }} />
                </Row>
              );
            }
            case "tool": {
              const yanked = item.key === yankedKey ? "row-yanked" : undefined;
              const abandoned = isAbandonedCall(item.call, state, sessionEnded);
              return (
                <Row key={item.key} kind="tool" sign={toolSign(item.call, abandoned)} current={current} className={yanked} navStop="row">
                  <div data-awaiting-permission={awaitingPermission.has(item.call.toolUseId) ? "true" : undefined}>
                    {/* P4: a call a card is waiting on no longer repeats its own invocation below the
                        card too -- the gated line above says "waiting for approval" already, so the
                        two would otherwise say the same thing twice, once as an offer and once as if
                        it had already run. */}
                    {renderToolCall(item.call, expanded[item.key] === true, {
                      gated: awaitingPermission.has(item.call.toolUseId),
                      detailed,
                      expanded: expanded[item.key] === true,
                      abandoned,
                    })}
                  </div>
                </Row>
              );
            }
            case "permission": {
              const yanked = item.key === yankedKey ? "row-yanked" : undefined;
              return (
                <Row key={item.key} kind="permission" sign="!" current={current} className={yanked} navStop="row">
                  <PermissionCard
                    request={item.request}
                    sessionEnded={sessionEnded}
                    ruleOffer={ruleOffers?.[item.request.permissionId] ?? null}
                    alreadyAnswered={answeredPermissions?.has(item.request.permissionId) ?? false}
                    onAnswer={onAnswerPermission}
                    onReasonChange={onPermissionReason}
                  />
                </Row>
              );
            }
            case "ending": {
              // Where a turn that did not complete stopped, in plain words (`turnEnding.ts`), muted and
              // signed `·` like the other "this is over" rows. A `j`/`k` stop like every conversation
              // row, so `y` copies what it says. Plain text, never markdown: the message is the
              // provider's, and its line breaks are kept by the row's own CSS.
              const yanked = item.key === yankedKey ? "row-yanked" : undefined;
              return (
                <Row key={item.key} kind="ending" sign="·" current={current} className={yanked} navStop="row">
                  {turnEndingText(item.ending)}
                </Row>
              );
            }
            case "run": {
              // P2: a run replaces its calls with one line, `Read ×3 · Bash ×2`; `Enter`
              // (`toggle-expand`, keyed on this row's OWN key) puts them back.
              const yanked = item.key === yankedKey ? "row-yanked" : undefined;
              return (
                <Row key={item.key} kind="tool-run" sign="✓" current={current} className={yanked} navStop="row">
                  <div className="tool-card tool-card-run">
                    {runSummary(item.calls)} <span className="fold-marker" aria-label="collapsed">▸</span>
                  </div>
                </Row>
              );
            }
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
        {/* No end-of-list sentinel any more: it existed for the smooth `scrollIntoView` a new row used
            to get, and following is a snap to `scrollHeight` now (the GUI pass, 2026-09-24). */}
      </div>
    </>
  );
}
