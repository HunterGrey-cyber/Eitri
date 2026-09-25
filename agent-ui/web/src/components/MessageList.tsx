import { useLayoutEffect, useMemo, useRef } from "react";
import type { AgentUiState, ToolCallRecord } from "../types";
import { renderToolCall } from "../toolRegistry";
import { buildTimeline, isUsableLink } from "../timeline";
import { renderMarkdown } from "../markdown";
import { HistoryNotice } from "./HistoryNotice";
import { PermissionCard } from "./PermissionCard";
import { Row } from "./Row";
import type { PermissionDecision } from "../bridge";
import { USER_SCROLL_EVENT, type UserScrollDirection } from "../follow";

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

export function MessageList({ state, sessionEnded, expanded, cursor, focused = true, onAnswerPermission }: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  /** The one `ResizeObserver`, on the list and on every child of it (see "Correction (the GUI pass)"
   *  below), and which children it is watching. `null` where there is none (jsdom). */
  const resizeObserverRef = useRef<ResizeObserver | null>(null);
  const observedChildrenRef = useRef<Set<Element>>(new Set());

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
      // still decides whether this is a view that is following.
      onScroll();
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
  };

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
      {/* No end-of-list sentinel any more: it existed for the smooth `scrollIntoView` a new row used
          to get, and following is a snap to `scrollHeight` now (the GUI pass, 2026-09-24). */}
    </div>
  );
}
