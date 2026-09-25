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

  /* Measure the list width here because CSS size query containers caused stale `scrollTop` resets
     in WebKitGTK 2.52.6 when the status clock ticked during streaming. Containers on `.row`,
     `.row-body`, the list and assistant rows all caused the same behavior.
     Measure before first paint and update the width when `ResizeObserver` sees the list's box resize.
     Content growth alone does not resize that box. Until the first width write, `index.css`
     keeps content within the row; jsdom uses the same fallback without layout or `ResizeObserver`.
     Observe each row too: changing `--prose-measure` can resize rows without resizing the list.
     While following, snap to the end after layout and before paint so reflow or a shorter viewport
     cannot hide the newest content. Leave a parked reader or a pending provisional stop in place.
     `shell/tests/panel_stream_scroll.rs` covers streaming and resizing in the real engine. */
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);

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
