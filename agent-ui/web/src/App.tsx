import { useEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { applyEvent, applySnapshot, initialState, resetToStartScreen } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import type { PermissionDecision } from "./bridge";
import { resolveKey } from "./keymap";
import type { KeyLike, PanelMode } from "./keymap";
import { buildTimeline } from "./timeline";
import { controlsOf, currentStop, nextControl, nextStop, permissionTarget, rowIndexOf } from "./nav";
import type { AnswerableItem } from "./nav";
import type { TimelineItem } from "./timeline";
import { ModeSelector } from "./components/ModeSelector";
import { Composer } from "./components/Composer";
import type { RestoredDraft } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { Row } from "./components/Row";
import { Winbar } from "./components/Winbar";
import { StatusLine } from "./components/StatusLine";
import { ContinueInTerminal, HandoffCommandCard } from "./components/TerminalHandoff";
import type { HandoffCommand, Hello, PermissionModeChoice } from "./types";
import { applyTheme } from "./theme";

/** Whether `el` is an ordinary editable control -- an `<input>`, a `<textarea>`, or anything
 *  `contenteditable`. Used to decide what the panel's own keydown handlers must stay out of: this
 *  panel does not own every keystroke inside its own subtree, only the ones outside a text field
 *  someone is actually typing into. Not scoped to any one component on purpose -- a permission
 *  card's deny-reason box is the one that exists today, but the rule is general and must hold for
 *  whatever text field a later card adds too. */
function isEditableElement(el: EventTarget | null): el is HTMLElement {
  if (!(el instanceof HTMLElement)) return false;
  return el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable;
}

/** Whether `el` is, or sits inside, a control a key press ACTIVATES by default -- a `<button>`, a
 *  `<summary>`, a link, or anything wearing `role="button"`.
 *
 *  This is the second half of "the panel does not own every keystroke inside its own subtree", and
 *  it is not a theoretical one. Enter's default action on a focused `<button>` *is* its activation
 *  click; there is no separate click event to let through. So a keydown handler that claims Enter
 *  from any non-editable target and calls `preventDefault()` does not merely also do something
 *  else -- it silently DELETES the button's activation.
 *
 *  **Observed on an installed build before it was fixed** (2026-09-18, the owner: "approve 现在没有
 *  键位能够触及好像"). Tab-to-Approve then Enter is the only keyboard route to a permission
 *  decision today -- the spec's `a`/`d` allow/deny keys belong to a later sub-project and are
 *  deliberately not in this keyboard skeleton -- and this panel's own `onKeyDown` had taken it
 *  away. Before this branch nothing listened for Enter at all, so the route worked; the branch
 *  created the hole and the fix restores exactly what was there, rather than pulling the later
 *  sub-project's keys forward to paper over it.
 *
 *  The same applies to Space on a button and to Enter on `<summary>` (the generic tool card's
 *  disclosure, `toolRegistry.tsx`), which is why this is a selector over activatable controls and
 *  not a special case for Enter. `closest`, not a tag check: a real click target is usually a
 *  `<strong>`/`<span>` INSIDE the button, and focus-then-Enter dispatches the keydown at the
 *  button itself -- both have to bail. */
function isActivatableControl(el: EventTarget | null): el is HTMLElement {
  if (!(el instanceof HTMLElement)) return false;
  return el.closest("button, summary, a[href], [role=button]") !== null;
}

/** One `j`/`k` press's worth of scroll inside a tool result's own overflow box, chosen to read
 *  like the row-to-row step it stands in for rather than a full-page jump. Arbitrary and not
 *  verified on a screen -- see the dated record's entry for this change. */
const TOOL_RESULT_SCROLL_STEP_PX = 40;

/** Whether a pending `j`/`k` cursor move should instead scroll the CURSOR ROW's own overflow box
 *  -- today, only a tool result opened past its 260px fold (`.tool-result-body` in index.css; see
 *  `renderToolCall` in `../toolRegistry.tsx`). Before this, only a mouse wheel could reach that
 *  box: the owner's "j无法在长输出内部下滑" is really two defects (see the dated record), and this
 *  is the second one -- once such a box filled the viewport, `j` moved the cursor straight off the
 *  row while most of its own content stayed unseen and unreachable from the keyboard.
 *
 *  This is a DOM measurement (`scrollTop`/`scrollHeight`/`clientHeight`), which is exactly what
 *  `./keymap`'s pure table must never do -- see its own doc comment -- so it is checked here,
 *  after `resolveKey` has already decided the key means "move the cursor," and BEFORE that
 *  decision is applied.
 *
 *  Returns `false` -- let the cursor move, the ordinary case -- when the current row has no such
 *  box, or the box is already at the end of travel in the pressed direction. That is the vim rule
 *  this exists to reproduce: scroll the inner view to its own limit first, THEN move to the next
 *  row. Mutates `scrollTop` directly rather than `scrollBy`/`scrollIntoView`, neither of which can
 *  express "move by this many pixels, clamped at the box's own natural end" -- which is exactly
 *  what is wanted here.
 *
 *  jsdom implements no layout, so `scrollHeight`/`clientHeight` both read 0 for every element and
 *  this always returns `false` there unless a test overrides them -- `App.test.tsx`'s own tests
 *  for this function do exactly that to reach the `true` branch at all. */
function scrollCursorRowBox(container: HTMLDivElement | null, direction: 1 | -1): boolean {
  const box = container?.querySelector<HTMLElement>(".row-current .tool-result-body") ?? null;
  if (box === null) return false;
  // The box can be off screen: the user mouse-scrolled the list elsewhere while the cursor stayed
  // on this row. Scrolling it then changes nothing visible, and since the cursor does not move the
  // cursor-follow effect never brings it back -- `j` would look dead for many presses, the very
  // symptom this function exists to fix (review finding, 2026-09-19 later). So the first press
  // brings the cursor row back into view and is consumed; the next one scrolls the box as usual.
  // Tests must mock both rects, because jsdom reports every rect as all zeros, which reads as
  // "not visible" here. Not looked at on a screen.
  const list = box.closest(".message-list");
  if (list !== null) {
    const b = box.getBoundingClientRect();
    const l = list.getBoundingClientRect();
    if (b.bottom <= l.top || b.top >= l.bottom) {
      box.closest(".row-current")?.scrollIntoView({ block: "nearest" });
      return true;
    }
  }
  const atStart = box.scrollTop <= 0;
  const atEnd = box.scrollTop + box.clientHeight >= box.scrollHeight - 1;
  if (direction > 0 ? atEnd : atStart) return false;
  box.scrollTop += direction * TOOL_RESULT_SCROLL_STEP_PX;
  return true;
}

/** How many lines of the current row's own text one `j`/`k` press scrolls, when that row is taller
 *  than what is left of it on screen. Three reads as a step through the text rather than a jump,
 *  and is the owner's design (2026-09-19). Not looked at on a screen. */
const ROW_SCROLL_LINES = 3;

/** Sub-pixel slack for "this edge is on screen": fractional scaling leaves rects a fraction of a
 *  pixel off, and a row whose last line is 0.3px past the edge must not eat a keypress. */
const EDGE_SLACK_PX = 1;

/** One `j`/`k` step inside a tall row: `ROW_SCROLL_LINES` of the row's own computed line height.
 *  Derived, not a pixel constant, so it follows the theme's font size and the display's scale.
 *  `line-height: normal` (and jsdom, which computes nothing) has no pixel value, so it falls back to
 *  1.2 x the font size, the usual `normal`; with no font size either, to a 16px font. */
function rowScrollStep(row: HTMLElement): number {
  const style = getComputedStyle(row);
  let line = parseFloat(style.lineHeight);
  if (!Number.isFinite(line) || line <= 0) {
    const font = parseFloat(style.fontSize);
    line = (Number.isFinite(font) && font > 0 ? font : 16) * 1.2;
  }
  return ROW_SCROLL_LINES * line;
}

/** Brings `row` on screen in `list` after the cursor has landed on it. A row that fits is revealed
 *  with `block: "nearest"`, which moves nothing when it is already fully visible. A row TALLER than
 *  the viewport shows the edge you are reading from: its top when you arrived with `j` (+1), its
 *  bottom when you arrived with `k` (-1) -- `"nearest"` on an element bigger than the scrollport
 *  aligns whichever edge happens to be closer, which for a long reply can be the middle of it.
 *  When that edge is already on screen, nothing moves.
 *  `direction` 0 (the cursor moved for some other reason) always means `"nearest"`. */
function revealRow(list: HTMLElement | null, row: HTMLElement, direction: 1 | -1 | 0) {
  if (list !== null && direction !== 0) {
    const r = row.getBoundingClientRect();
    const l = list.getBoundingClientRect();
    const viewport = l.bottom - l.top;
    if (viewport > 0 && r.bottom - r.top > viewport) {
      // Already showing the edge you are reading from: move nothing, and let the next `j`/`k` step
      // through the row (`scrollCursorRow`). Aligning it anyway jumped the view by up to a whole
      // viewport in one press and pushed the tail of the row just read out of sight -- review.
      const edgeShown = direction > 0 ? r.top >= l.top && r.top < l.bottom : r.bottom <= l.bottom && r.bottom > l.top;
      if (!edgeShown) list.scrollTop += direction > 0 ? r.top - l.top : r.bottom - l.bottom;
      return;
    }
  }
  row.scrollIntoView({ block: "nearest" });
}

/** Whether a pending `j`/`k` should instead scroll the conversation within the CURRENT row, because
 *  that row still runs past the visible edge in the direction of travel. The owner, on an installed
 *  build: "现在jk没法在选中输出的时候滚动屏幕，特别是在最后输出很长的时候，没法滚动看下面的" --
 *  `j` jumped from a long reply straight to the next row, skipping everything between, and on the
 *  LAST row there was no next row at all, so the rest of the reply was unreachable from the keyboard.
 *
 *  Tried after `scrollCursorRowBox`, which keeps priority: a tool result's own capped box is the more
 *  specific scroller. The step is `rowScrollStep`, never more than what is left to reveal, so the
 *  press that finishes lands the row's edge exactly on the viewport's edge; the NEXT press moves on.
 *  Holding `j` on a long last reply therefore reads it to its end and then stops.
 *
 *  A row entirely off screen (the list was mouse-scrolled away from it) is brought back first, and
 *  that press is consumed -- the same rule `scrollCursorRowBox` follows, for the same reason.
 *
 *  Returns `false` when there is no layout to judge (a list with no height -- jsdom, or a panel not
 *  laid out yet), or when writing `scrollTop` did not move anything (already at the list's end):
 *  a press that can make no progress must fall through to the ordinary move, never be eaten. */
function scrollCursorRow(container: HTMLDivElement | null, direction: 1 | -1): boolean {
  const row = container?.querySelector<HTMLElement>(".row-current") ?? null;
  const list = row?.closest<HTMLElement>(".message-list") ?? null;
  if (row === null || list === null) return false;
  const r = row.getBoundingClientRect();
  const l = list.getBoundingClientRect();
  if (l.bottom - l.top <= 0) return false;
  const before = list.scrollTop;
  if (r.bottom <= l.top || r.top >= l.bottom) {
    revealRow(list, row, direction);
    return true;
  }
  const overflow = direction > 0 ? r.bottom - l.bottom : l.top - r.top;
  if (overflow <= EDGE_SLACK_PX) return false;
  list.scrollTop += direction * Math.min(rowScrollStep(row), overflow);
  return list.scrollTop !== before;
}

/** The conversation rows inside `list` that are at least partly on screen. */
function visibleRows(list: HTMLElement): HTMLElement[] {
  const l = list.getBoundingClientRect();
  return Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]')).filter((row) => {
    const r = row.getBoundingClientRect();
    return r.bottom > l.top && r.top < l.bottom;
  });
}

export default function App() {
  const [state, setState] = useState(initialState());
  const [hello, setHello] = useState<Hello | null>(null);
  const [sessionStarted, setSessionStarted] = useState(false);
  /** The requestId of an in-flight `start_session`. Backend construction is genuinely slow (the
   * sidecar path spawns a process and does a real handshake; a cold Verdandi checkout also builds
   * it), so Rust defers its `command_result` until the worker finishes. This is what lets the start
   * screen say "starting" instead of appearing to have ignored the click. */
  const [startingRequestId, setStartingRequestId] = useState<string | null>(null);
  /** The permission mode the CURRENT session was actually started with, remembered here because
   *  `AgentUiState` carries no such field: `Hello.permissionModes` is only the pre-session menu of
   *  choices (gone once a session exists), and `Capabilities.bypassPermissionMode` is a capability
   *  flag, never the session's active mode. Set the moment `startSession` is called, not from any
   *  reply -- its authority comes from the provider, not from this component's own memory of a
   *  click: `ClaudeSidecarProvider::require_permission_mode` REFUSES a mode it cannot honour rather
   *  than silently substituting another, so a session that went on to actually start really is
   *  running the mode requested here. If a provider ever started substituting instead of refusing,
   *  this would have to become a reported fact rather than a remembered request -- it would no
   *  longer be true by construction.
   *
   *  Lives exactly as long as the TRANSCRIPT it describes, not as long as the session is running --
   *  it is deliberately NOT cleared on `session_unavailable`/`session_closed`. A dead session's
   *  transcript stays on screen, and the winbar is still describing something real: that
   *  conversation genuinely ran in this mode, dead or not. Clearing it there would leave the winbar
   *  describing nothing while the conversation it describes is still visible -- worse than showing
   *  a fact about a session that has ended. It IS cleared everywhere the transcript itself goes
   *  away: a failed start's `command_result` (no transcript was ever shown), `handoff`, `error`, and
   *  `returnToStartScreen` all replace `state` with `initialState()`/`resetToStartScreen`, and this
   *  goes with it. The one thing that must never happen -- a DIFFERENT session's mode showing -- is
   *  prevented not by clearing but by overwriting: `startSession` sets this before that session's
   *  own transcript can exist, so a later session can never render with an earlier one's value. */
  const [startedPermissionMode, setStartedPermissionMode] = useState<PermissionModeChoice | null>(null);
  /** A fatal, session-ending failure, shown in the panel. Replaces window.alert, which cannot be
   * copied, cannot show the sidecar's own multi-line startup diagnostics, and blocks the WebView. */
  const [fatalError, setFatalError] = useState<string | null>(null);
  /** The command for a conversation that has just been closed here and moved to a terminal. Set by
   *  the `handoff` envelope, which Rust sends only after the real session close finished.
   *
   *  **This is a view of Rust's own `AgentPanelState::last_handoff`, not the only copy.** Rust keeps
   *  the command and re-sends it in the `ready` handshake, so a panel reload (Ctrl+Shift+R, the top
   *  bar's ⟳) or a WebView crash gets it back — on the default legacy backend the id in it is
   *  recoverable from nowhere else at all. Cleared here when a real `snapshot` arrives, which is
   *  also when Rust clears its copy: a session that is actually running, not one merely asked for. */
  const [handoff, setHandoff] = useState<HandoffCommand | null>(null);
  /** The requestId of an in-flight `handoff_to_terminal`, or null.
   *
   *  Non-null means the conversation is CLOSING: Rust has already taken the backend out of its own
   *  state and a worker thread is running the real `shutdown()`. Nothing is torn down here until the
   *  `handoff` envelope arrives, so without this the composer would stay live and a typed Enter
   *  would clear the box into a session that no longer exists. */
  const [handoffRequestId, setHandoffRequestId] = useState<string | null>(null);
  /** A refused command, in words, on screen. Rust's refusals carry real human-readable reasons
   *  (`HandoffRefusal::message`, `BackendError::message`) and every one of them used to reach a
   *  `console.warn` and nothing else. */
  const [commandNotice, setCommandNotice] = useState<string | null>(null);
  /** Text to put back in the composer after a refused send. See `RestoredDraft`. */
  const [restoredDraft, setRestoredDraft] = useState<RestoredDraft | null>(null);
  /** What each in-flight request actually was, so its reply can be handled as that thing. A ref: it
   *  is bookkeeping, never rendered, and a render per outgoing command would be pure cost.
   *
   *  Only the three kinds whose replies need special handling are recorded. An interrupt or a
   *  permission response has no entry and comes back as `undefined`, which is correct rather than a
   *  gap: its refusal takes the plain "show the reason" path. Every recorded request gets exactly
   *  one `command_result` and is deleted there, so this cannot grow. */
  const inFlight = useRef<Map<string, { kind: "start" | "send" | "handoff"; text?: string }>>(new Map());
  /** Monotonic, so two refusals of the same text are two distinct restores. */
  const restoreSeq = useRef(0);
  // Spinner-only, per-requestId in-flight tracking -- never read to answer "is a turn in
  // progress" or "is this permission still pending" (those come only from canonical
  // state.activeTurnId / state.pendingPermissions).
  const [, setPendingCommands] = useState<Set<string>>(new Set());
  /** When the payload carrying a turn's first assistant text was RECEIVED, for the render trace.
   *  A ref, not state: writing it must not itself cause a render, which would be the thing being
   *  measured. Null except in the window between that payload arriving and its frame being drawn. */
  const firstTextReceivedAt = useRef<number | null>(null);
  /** Guards the render report to one per turn. Without it the effect below re-arms on every one of
   *  a reply's ~400 deltas. */
  const renderReportSent = useRef(false);
  /** Which timeline rows show their full tool result rather than the folded placeholder, keyed by
   *  the timeline `key` -- never the cursor index, because a resolved permission removes a card and
   *  shifts every later index, which would silently move an expansion onto a different row.
   *  Toggled by `Enter` on the row under the cursor, in `onKeyDown` below. */
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  /** BROWSE (read, move the cursor, act on "this item") / INPUT (typing into the composer) / HINT
   *  (declared for the status line's mode block, Task 6 -- nothing reaches it yet). See
   *  `./keymap`'s own doc comment on `PanelMode`. */
  const [mode, setMode] = useState<PanelMode>("browse");
  /** Whether this pane has the window's keyboard focus, as `shell` reports it (`pane_focus`
   *  envelope). It starts `false` because `shell` focuses the editor at startup, and `ready`
   *  re-sends the real value to every freshly loaded document. The mode block is bright only
   *  when this is `true`. Before this existed, a bright BROWSE sat in the panel while the user
   *  typed into the editor, and the panel rework (`9dd39f2`) had removed the composer caret that
   *  used to be the only sign of which pane was focused. */
  const [paneFocused, setPaneFocused] = useState(false);
  /** Bumped by each `enter_input` envelope (`Ctrl+l` from the editor). A counter, not a flag, so
   *  two arrivals in a row both act, and so the dispatch handler, installed once, need not read any
   *  state: the effect below decides, against the current render, whether INPUT is possible. It
   *  is also passed to `Composer` as `focusRequest`, which re-focuses a textarea that is already
   *  mounted, since `setMode("input")` alone does nothing when the mode was already INPUT. */
  const [inputRequest, setInputRequest] = useState(0);
  /** Whether keyboard focus is on a stop that is NOT a conversation row -- a banner's Dismiss, the
   *  status line's Stop, the handoff button (`./nav`). While it is, the row cursor is drawn hollow,
   *  the same way it is when another pane has the keys: it is where `k` brings you back to, and it
   *  is not what the keys act on now. Derived from DOM focus (`onFocus` on the root), never set by
   *  the key handler alone, so a click or a `Tab` onto one of those buttons agrees with `j`/`k`. */
  const [edgeFocused, setEdgeFocused] = useState(false);
  /** The index into `timeline` that `j`/`k` move and `Enter`/`y` act on. */
  const [cursor, setCursor] = useState(0);
  /** How the NEXT cursor change should bring its row on screen (`revealRow`): +1/-1 when `j`/`k`
   *  moved it (a tall row shows its top or its bottom), `"keep"` when the key already put the view
   *  exactly where it belongs (`Ctrl+d`/`Ctrl+u` re-homing, `G`, `gg`), 0 otherwise. A ref, read
   *  and reset by the cursor-follow effect, and only ever set when the cursor really changes -- an
   *  unchanged cursor runs no effect, and a stale value would misplace some later, unrelated move. */
  const landingRef = useRef<1 | -1 | 0 | "keep">(0);
  /** A lone `g` was the last key in BROWSE (`resolveKey`'s `pending-g`). Cleared on EVERY key
   *  `onKeyDown` sees, so anything but a second `g` cancels it, and on every `pane_focus` envelope,
   *  so a pane switch the WebView never saw as a key cancels it too; handed to `resolveKey` in its
   *  context so the key table itself keeps no memory. */
  const pendingGRef = useRef(false);
  /** One ordered view of the conversation, kept in step with the cursor/expand keys below. See
   *  `MessageList`'s own copy of this memo for why it is keyed on `state` as a whole. */
  const timeline = useMemo(() => buildTimeline(state), [state]);
  const answerableItems = useMemo(
    (): AnswerableItem[] =>
      timeline.map((item) =>
        item.kind === "permission"
          ? { kind: "permission", toolUseId: item.request.toolUseId }
          : item.kind === "tool"
            ? { kind: "tool", toolUseId: item.call.toolUseId }
            : { kind: "other" },
      ),
    [timeline],
  );
  /** Kept focused so BROWSE's keydown handler actually receives keys: a keydown bubbles from
   *  whatever DOM node has real focus, which is a browser fact, not a React one. `Escape` leaving
   *  INPUT removes the composer's textarea from the DOM, which drops focus onto whatever the
   *  browser picks next (typically `document.body`) rather than back onto this element -- so
   *  without refocusing here, `j`/`k`/`Enter`/`y`/`r` would stop working after the very first trip
   *  into INPUT and back.
   *
   *  Gated on the same condition as `onKeyDown` below: if something else in this subtree -- a
   *  permission card's deny-reason box, today -- legitimately holds focus, this must NOT yank it
   *  back. Found in review: clicking that box blurs the composer's textarea, `onModeChange`
   *  reports BROWSE, and an ungated version of this effect then stole focus away from the box the
   *  click had just placed it in, so the click landed nowhere. */
  const containerRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (mode !== "browse") return;
    if (isEditableElement(document.activeElement)) return;
    containerRef.current?.focus();
  }, [mode, sessionStarted]);
  /* Keys that land on <body> are handed back to the panel. That happens whenever the focused
     control stops existing or stops being focusable: a Dismiss that removes its own banner, an
     Approve that goes disabled once answered. A browser moves focus to <body> in both cases without
     reliably firing `blur` (a removed node fires nothing), and a disabled button receives no key
     events, so the next key would sail past `onKeyDown` and the panel would stop answering its own
     keys. The key is REPLAYED on the root rather than dropped, so the keystroke that noticed the
     problem still does what it says. The replay targets the root, so it never re-enters here. */
  useEffect(() => {
    function onDocumentKeyDown(event: globalThis.KeyboardEvent) {
      if (event.target !== document.body && event.target !== document.documentElement) return;
      const root = containerRef.current ?? startScreenRef.current;
      if (root === null) return;
      root.focus({ preventScroll: true });
      const replay = new globalThis.KeyboardEvent("keydown", {
        key: event.key,
        code: event.code,
        ctrlKey: event.ctrlKey,
        shiftKey: event.shiftKey,
        altKey: event.altKey,
        metaKey: event.metaKey,
        bubbles: true,
        cancelable: true,
      });
      if (!root.dispatchEvent(replay)) event.preventDefault();
    }
    document.addEventListener("keydown", onDocumentKeyDown);
    return () => document.removeEventListener("keydown", onDocumentKeyDown);
  }, []);
  // The same removal leaves `edgeFocused` claiming a banner that is gone, since no focus event
  // announced its disappearance. Any render after it corrects that, so the row cursor goes solid
  // again as soon as the banner is gone rather than on the next keypress.
  useEffect(() => {
    if (edgeFocused && !containerRef.current?.contains(document.activeElement)) setEdgeFocused(false);
  });

  /* The cursor is an index into `timeline`, which shrinks on its own -- a permission card resolving
     removes one row without anything here asking for it. Without this, a cursor left pointing past
     the new end copies/expands nothing (`timeline[cursor]` is `undefined`) rather than sliding onto
     the new last row, which is what every other list-with-a-cursor does when its tail disappears. */
  useEffect(() => {
    setCursor((c) => Math.min(c, Math.max(timeline.length - 1, 0)));
  }, [timeline.length]);
  /** Keeps the row the cursor sits on inside the viewport whenever the cursor moves. Before this,
   *  `j`/`k` moved an invisible highlight once it passed the bottom of the message list -- which
   *  reads exactly like the key doing nothing (the owner, on an installed build:
   *  "j无法在长输出内部下滑"). `"nearest"` is the least-jarring `ScrollLogicalPosition`: a row
   *  already fully on screen does not move at all, unlike `"start"`/`"center"`, which would shove
   *  the viewport around on every single step even when nothing needed to move.
   *
   *  Reads through `containerRef` -- already queried above for focus -- rather than a second ref
   *  into `MessageList`'s own DOM, since `.row-current` is always a descendant of it.
   *
   *  Kept from fighting `MessageList`'s own follow-the-newest-message effect (`MessageList.tsx`,
   *  the `bottomRef` effect) by gating THAT effect on the message list's own scroll position
   *  rather than on this cursor -- see its doc comment. This effect never needs to check anything
   *  about that one: it only ever moves the viewport the minimum amount to reveal one row, so if
   *  the other effect already put the tail in view, this is a no-op, and if the user is reading
   *  further up, this is the only one of the two still allowed to move anything.
   *
   *  jsdom implements no layout and has no `scrollIntoView` on `Element` at all (this file's own
   *  `beforeAll` stubs it for the same reason `MessageList.test.tsx`'s does), so a jsdom test can
   *  only assert that this was CALLED on the right element -- never that the row actually ends up
   *  on screen. That is a GUI check nobody has run yet. */
  useEffect(() => {
    const landing = landingRef.current;
    landingRef.current = 0;
    if (landing === "keep") return;
    const row = containerRef.current?.querySelector<HTMLElement>(".row-current");
    if (row == null) return;
    revealRow(row.closest<HTMLElement>(".message-list"), row, landing);
  }, [cursor]);
  /** The session is gone (lost or closed). Read before the start-screen branch below, because the
   *  effect under it is a hook and cannot live after a conditional return. */
  const sessionEnded = state.status.kind === "unavailable" || state.status.kind === "closed";
  /* INPUT on a dead session was a one-way trap, and the panel's own banners promised otherwise.
     The composer's textarea is `disabled` once the session ends, so `autoFocus` does nothing and
     focus stays on the root -- keys still ARRIVE, they are just dropped: `resolveKey`'s "input"
     branch resolves nothing but `Escape`. So `r`/`j`/`k`/`y` all went dead while the lost-session
     and ended-session rows kept printing "Press r to return to the start screen." and nothing on
     screen mentioned Escape.

     BROWSE is forced rather than teaching `r` to resolve in INPUT, because INPUT is not merely
     key-poor on a dead session -- it is empty: there is no box to type into, so the mode has
     nothing left to be. The two other routes into it are closed at their own sources: `resolveKey`
     refuses `i` when `ctx.sessionEnded` (`./keymap`), and `Composer` stops offering the focusable
     hint that a Tab could land on. This effect is the third case, a session that dies while the
     user is already in INPUT, which neither of those can reach. */
  useEffect(() => {
    if (sessionEnded) setMode("browse");
  }, [sessionEnded]);
  /* `Ctrl+l` lands in INPUT with a blinking caret (owner, 2026-09-19: "control l 直接闪cursor"),
     as it did before BROWSE became the landing mode. Refused under the same condition `i` is
     (`resolveKey`): a dead session has no box to type into, and no session at all has no
     composer. Keyed on the request alone, so a session change never opens the composer by itself. */
  useEffect(() => {
    if (inputRequest === 0 || !sessionStarted || sessionEnded) return;
    setMode("input");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inputRequest]);
  /** The start screen's own focus target, for the same reason `containerRef` needs one: a keydown
   *  bubbles from whatever has real focus, and nothing here claims it by default. Only ever used to
   *  make `y` (copying a handoff command, see `handleStartScreenKeyDown`) reachable without an
   *  explicit prior click. */
  const startScreenRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!sessionStarted) startScreenRef.current?.focus();
  }, [sessionStarted]);

  useEffect(() => {
    installDispatch((payload) => {
      if (payload.kind === "theme") {
        applyTheme(payload.vars);
      } else if (payload.kind === "pane_focus") {
        // A pending `g` is for the very next key; a pane switch in between (GTK takes `Ctrl+h`/
        // `Ctrl+k` before the WebView sees a keydown) must not leave it armed for a `g` pressed
        // much later, when it would jump to the top -- review.
        pendingGRef.current = false;
        setPaneFocused(payload.focused);
      } else if (payload.kind === "enter_input") {
        setInputRequest((n) => n + 1);
      } else if (payload.kind === "hello") {
        setHello(payload);
      } else if (payload.kind === "snapshot") {
        setState((s) => applySnapshot(s, payload.state, payload.throughRevision));
        setSessionStarted(true);
        // A snapshot means a session is genuinely RUNNING, which is also when Rust clears its own
        // copy of the command. Clearing it when a start was merely requested would throw it away on
        // a start that then failed -- and on the legacy backend that is the last reference to a
        // conversation nothing else remembers.
        setHandoff(null);
        setCommandNotice(null);
      } else if (payload.kind === "events") {
        // A new turn resets the render trace: each turn reports its own first text, once.
        if (payload.events.some((e) => e.type === "turn_started")) {
          firstTextReceivedAt.current = null;
          renderReportSent.current = false;
        }
        // Stamped before the state update that will cause the render, so the span covers the work
        // being measured rather than starting after it.
        if (
          firstTextReceivedAt.current === null &&
          payload.events.some((e) => e.type === "content_delta" && e.kind === "text" && e.text !== "")
        ) {
          firstTextReceivedAt.current = performance.now();
        }
        setState((s) => payload.events.reduce((acc, event) => applyEvent(acc, event), s));
      } else if (payload.kind === "command_result") {
        setPendingCommands((prev) => {
          const next = new Set(prev);
          next.delete(payload.requestId);
          return next;
        });
        const record = inFlight.current.get(payload.requestId);
        inFlight.current.delete(payload.requestId);
        setStartingRequestId((current) => {
          if (current !== payload.requestId) return current;
          // The deferred reply to our start_session. On failure, fall back to the start screen so
          // the user can retry -- an `error` envelope follows with the real cause.
          if (!payload.ok) {
            setSessionStarted(false);
            // The mode `startSession` optimistically remembered belongs to a session that never
            // actually started -- nothing refused it, it simply never came to exist.
            setStartedPermissionMode(null);
          }
          return null;
        });
        if (record?.kind === "handoff") {
          // Either the handoff finished (the `handoff` envelope arrived first and already reset
          // everything) or it was refused and the session is untouched. Both end the closing state.
          setHandoffRequestId((current) => (current === payload.requestId ? null : current));
        }
        if (!payload.ok) {
          console.warn("agent-ui: command failed", payload.requestId, payload.error);
          if (record?.kind === "send") {
            // The composer cleared this optimistically. Rust refused it, so it goes back — a
            // message that vanishes with no trace is the outcome this exists to prevent.
            restoreSeq.current += 1;
            setRestoredDraft({ text: record.text ?? "", seq: restoreSeq.current });
            setCommandNotice(`That message was not sent (${payload.error}). It is back in the box.`);
          } else if (record?.kind === "start") {
            // A failed start already has the start screen and an `error` envelope carrying the real
            // cause; a second surface for it would just be noise.
          } else {
            setCommandNotice(payload.error);
          }
        }
      } else if (payload.kind === "handoff") {
        // The session is genuinely closed by the time this arrives (Rust dispatches it only after
        // its own `shutdown()` returned), so the conversation goes with it rather than being left
        // on screen looking live -- the same treatment a fatal error gets, for the same reason.
        setSessionStarted(false);
        setStartingRequestId(null);
        setHandoffRequestId(null);
        setCommandNotice(null);
        setState(initialState());
        setStartedPermissionMode(null);
        setHandoff(payload);
        // Suppresses the resume offer for THIS client's already-delivered `hello`, which was
        // computed once at mount. That only ever matches when this session was itself resume-started
        // -- and that is fine, because it is not the durable half of this rule: Rust applies the
        // same suppression when it builds `hello`, on every mount, which is the case that actually
        // bites (the handed-over session is the most recently updated record, so it would otherwise
        // head the list). See `agent_panel::ready_payloads`. Any OTHER stored session is untouched
        // on both sides -- this drops exactly the one row that was just given away, rather than
        // clearing the offer, which would hide every other session the workspace remembers.
        setHello((current) =>
          current === null
            ? current
            : {
                ...current,
                resumableSessions: current.resumableSessions.filter(
                  (s) => s.providerSessionId !== payload.providerSessionId,
                ),
              },
        );
      } else if (payload.kind === "error") {
        setSessionStarted(false);
        setStartingRequestId(null);
        setHandoffRequestId(null);
        setState(initialState());
        setStartedPermissionMode(null);
        setFatalError(payload.message);
        /* Re-ask for `hello`, because we are about to show the start screen again and the copy we
           captured at mount is a snapshot of the conversation records as they were then.

           The session that just died is exactly the one the user is most likely to want back, and
           it was persisted on adoption (`agent/src/ingestion.rs` -> `conversation::persist_record`)
           -- so it IS on disk and offerable, and only the in-memory list is stale. Without this the
           picker's own note, "Previous conversations here, newest first", is a claim the component
           cannot honour at that moment; the only escape hatch was `Ctrl+Shift+R`, which nothing
           tells the user about.

           Safe to re-post: Rust answers `Ready` from canonical state with a fresh
           `BackendGreeting::for_kind` and a `command_result`, never with another `error`, so there
           is no loop here. */
        requestHello();
      }
    });
    requestHello();
  }, []);

  /** Posts `ready` and tracks it as in-flight. Rust replies with `hello` (and a snapshot, if a
   *  session exists). Called on mount and again whenever the start screen comes back. */
  function requestHello() {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "ready", request_id: requestId });
  }

  /* Reports how long this WebView took to draw a turn's first assistant text. The effect runs after
     React has committed the DOM; the animation frame runs just before the browser paints it. That is
     a frame, not a photon -- read it as a floor on what the user perceives, never as a measured
     perceptual latency. Deliberately NOT cancelled on cleanup: with a delta arriving every ~33ms, a
     cleanup that cancelled the pending frame would re-arm faster than the frame could ever fire, and
     the mark would simply never be reported. */
  useEffect(() => {
    const receivedAt = firstTextReceivedAt.current;
    if (receivedAt === null || renderReportSent.current) return;
    renderReportSent.current = true;
    requestAnimationFrame(() => {
      postToRust({
        type: "turn_rendered",
        request_id: nextRequestId(),
        receive_to_frame_ms: performance.now() - receivedAt,
      });
    });
  }, [state.transcript]);

  /** `resume` carries the Claude provider session id to continue, or nothing for a fresh session.
   * A resume that fails comes back as a normal fatal error and returns here -- it is never turned
   * into a fresh session, by this component or by anything below it. */
  function startSession(mode: PermissionModeChoice, resume?: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    setStartingRequestId(requestId);
    setFatalError(null);
    // Remembered optimistically, like every other piece of this request -- see
    // `startedPermissionMode`'s own doc comment for why that is sound here specifically: the
    // provider refuses a mode it cannot honour rather than substituting one, so a session that
    // goes on to actually start is running exactly this mode. Cleared again on a failed start,
    // in the `command_result` handler above.
    setStartedPermissionMode(mode);
    inFlight.current.set(requestId, { kind: "start" });
    postToRust({ type: "start_session", request_id: requestId, mode, resume });
  }

  function handoffToTerminal() {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    inFlight.current.set(requestId, { kind: "handoff" });
    // Set BEFORE the post, so the composer is disabled from this moment rather than from whenever a
    // reply comes back. Rust takes the session out of its own state inside the handler this message
    // reaches, and every send after that point would be refused.
    setHandoffRequestId(requestId);
    setCommandNotice(null);
    postToRust({ type: "handoff_to_terminal", request_id: requestId });
  }

  function sendMessage(text: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    // The text is kept so a refusal can put it back. Dropped again as soon as the reply arrives.
    inFlight.current.set(requestId, { kind: "send", text });
    setCommandNotice(null);
    postToRust({ type: "send_message", request_id: requestId, text });
  }

  function interrupt() {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "interrupt", request_id: requestId });
  }

  function answerPermission(permissionId: string, decision: PermissionDecision, reason?: string) {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "permission_response", request_id: requestId, permission_id: permissionId, decision, reason });
  }

  /** What `y` puts on the clipboard for the item under the cursor. §4.3: "复制本条（命令、代码块、
   *  消息 markdown 原文）" -- the message's own markdown SOURCE, not its rendered HTML, which is
   *  what a reader would paste back into an editor.
   *
   *  Takes a real item, not `timeline[cursor]` directly: the caller decides what "nothing at the
   *  cursor" means (do nothing, rather than writing "" over whatever was already on the clipboard --
   *  see `onKeyDown`'s "copy" arm), which is not this function's decision to make. */
  function copyTextForItem(item: TimelineItem): string {
    switch (item.kind) {
      case "prompt":
      case "message":
        return item.text;
      case "tool":
        return JSON.stringify(item.call.input, null, 2);
      case "permission":
        return JSON.stringify(item.request.input, null, 2);
    }
  }

  /** `r` on an ended session. Drops back to the start screen by clearing the session state the same
   *  way a fresh panel load does -- it starts nothing by itself, because which mode and which
   *  resumable session to start is the start screen's question to ask. `backend`/`capabilities`/
   *  `provider` survive (see `resetToStartScreen`) because they came from `hello` and describe the
   *  bridge, not the session that just ended. A fresh `hello` is asked for the same reason the
   *  `error` handler above does it: the session that just ended is exactly the one most likely to
   *  be offered back, and the copy of `hello` captured at mount does not have it yet. */
  function returnToStartScreen() {
    setMode("browse");
    setCursor(0);
    setExpanded({});
    setSessionStarted(false);
    setStartingRequestId(null);
    setHandoffRequestId(null);
    setCommandNotice(null);
    setState(resetToStartScreen);
    setStartedPermissionMode(null);
    requestHello();
  }

  /** The one key the start screen offers: `y`, copying the command `HandoffCommandCard` shows
   *  ("Press y to copy.", `components/TerminalHandoff.tsx`). Deliberately separate from the
   *  conversation's own `onKeyDown` rather than one handler branching on `handoff` -- the two never
   *  run at once (`handoff` is always cleared by the time a real `snapshot` flips `sessionStarted`
   *  true, in the `installDispatch` handler above), and this screen has no cursor, no modes, and no
   *  `TimelineItem`s for a cursor to sit on: the handoff command is the only thing here `y` could
   *  ever mean, because it only exists in a state where the session has already ended and there is
   *  no live conversation row left to compete with it for the key. */
  function handleStartScreenKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (isEditableElement(event.target)) return;
    if (event.nativeEvent.isComposing || event.ctrlKey || event.shiftKey || event.altKey) return;
    const root = startScreenRef.current;
    // The same structural reach as the conversation (`./nav`), over the start screen's stops: each
    // conversation choice and each mode button, top to bottom, plus any banner. There is no row
    // cursor here, so every stop is reached by focusing it and Enter/Space activate it natively.
    if (root !== null && (event.key === "j" || event.key === "k")) {
      event.preventDefault();
      const target = nextStop(root, null, event.key === "j" ? 1 : -1);
      if (target !== null) controlsOf(target)[0]?.focus();
      return;
    }
    if (root !== null && (event.key === "h" || event.key === "l")) {
      event.preventDefault();
      const stop = currentStop(root, null);
      const target = stop === null ? null : nextControl(stop, event.key === "l" ? 1 : -1);
      if (target instanceof HTMLElement) target.focus();
      return;
    }
    if (handoff === null || event.key !== "y") return;
    event.preventDefault();
    void navigator.clipboard?.writeText(handoff.command);
  }

  /* Rendered on both screens. A refused command can return the panel to the start screen (a failed
     handoff close, for one), and a reason that only exists in the conversation view would be gone by
     the time it could be read. */
  const commandNoticeBanner =
    commandNotice === null ? null : (
      <div className="command-notice" role="alert" data-nav-stop="notice">
        <span>{commandNotice}</span>
        <button onClick={() => setCommandNotice(null)}>Dismiss</button>
      </div>
    );

  const errorBanner =
    fatalError === null ? null : (
      <div className="fatal-error" role="alert" data-nav-stop="error">
        <strong>The agent session ended.</strong>
        {/* <pre>, not a <p>: the sidecar's startup diagnostics are multi-line and the exact text
            (a CLI version, a checkout revision) is the whole point. */}
        <pre>{fatalError}</pre>
        <button onClick={() => setFatalError(null)}>Dismiss</button>
      </div>
    );

  if (!sessionStarted) {
    return (
      <div className="agent-ui-root" ref={startScreenRef} tabIndex={0} onKeyDown={handleStartScreenKeyDown}>
        {errorBanner}
        {commandNoticeBanner}
        {handoff !== null && <HandoffCommandCard handoff={handoff} />}
        <ModeSelector hello={hello} connecting={startingRequestId !== null} onStart={startSession} />
      </div>
    );
  }

  // Authoritative, server-originated. `activeTurnId` is set by a real TurnStarted event from the
  // provider and cleared by a real TurnCompleted -- never by this component optimistically marking
  // a turn as started when the user pressed Send. The reducer also clears it on a session that ends
  // without one, because no TurnCompleted is ever coming for a session that is gone.
  const turnInProgress = state.activeTurnId !== null;
  /* The conversation is on its way out: Rust already owns the backend on a shutdown worker and will
     refuse every command until that finishes. The composer must reflect that rather than accepting
     input it cannot deliver. */
  const handingOff = handoffRequestId !== null;

  /** The key table's home: mode + key + context in, an action out, applied here. Only claims what
   *  `resolveKey` claims -- an unrecognised key, or one INPUT leaves to the input method (a
   *  composing Escape), falls straight through with no `preventDefault`. That is what keeps
   *  Ctrl+h/l (pane switch), Ctrl+Shift+R (panel reload) and Ctrl+Shift+O working: GTK takes those
   *  in the capture phase and this handler must not fight it for a chord it does not own.
   *
   *  Also stays out of any OTHER editable control in this subtree -- a permission card's
   *  deny-reason box, today. Found in review: with no guard, `j` typed there never reached the
   *  input (claimed and `preventDefault`ed as a cursor move) and `i` stole focus into the composer
   *  mid-word. The composer's own textarea is the one exception: while INPUT is active it lives
   *  inside `.composer`, and letting its keydowns reach here is exactly the path that lets `Escape`
   *  (bubbling from it) leave INPUT -- `resolveKey`'s "input" branch reacts to nothing else.
   *
   *  And out of any ACTIVATABLE control, for the reason spelled out on `isActivatableControl`: a
   *  focused button's activation IS the default action of the key, so claiming the key removes the
   *  only keyboard route to Approve/Deny/Stop/Dismiss/Cancel. That was a real, reported regression,
   *  not a hypothetical. */
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const root = containerRef.current;
    const pendingG = pendingGRef.current;
    pendingGRef.current = false;
    if (isEditableElement(event.target) && !(event.target as HTMLElement).closest(".composer")) {
      // A text box inside a row (a permission card's reason) takes every key as text, except Esc,
      // which hands the keys back to the row it sits in. Esc mid-composition still belongs to the
      // input method, as it does in the composer.
      if (event.key === "Escape" && !event.nativeEvent.isComposing && root !== null) {
        event.preventDefault();
        root.focus({ preventScroll: true });
      }
      return;
    }
    // A focused button activates on Enter and Space natively; claiming those would swallow the only
    // keyboard route to Approve (found by the owner on an installed build). Every OTHER key still
    // reaches the table, which is what lets `h`/`j`/`k`/`l` carry on from a focused button.
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ")) return;
    const action = resolveKey(mode, event.nativeEvent as unknown as KeyLike, { sessionEnded, pendingG });
    if (action === null) return;
    // `j`/`k` scroll the cursor row's own overflow box (a long tool result) before they move off it
    // -- the vim behaviour the owner expected; `scrollCursorRowBox`'s own doc says why this cannot
    // live in `resolveKey`. Failing that, they scroll the conversation through a current row that
    // is taller than what is on screen (`scrollCursorRow`). Only while the row cursor has the keys:
    // from a banner's button, `j` is a move, not a scroll of a row that is not current.
    if (
      action.kind === "move" &&
      !edgeFocused &&
      (scrollCursorRowBox(root, action.delta) || scrollCursorRow(root, action.delta))
    ) {
      event.preventDefault();
      return;
    }
    if (action.kind === "move" || action.kind === "control" || action.kind === "answer") {
      event.preventDefault();
      if (root === null) return;
      if (action.kind === "move") {
        const target = nextStop(root, cursor, action.delta);
        if (target === null) return;
        const row = rowIndexOf(root, target);
        if (row !== null) {
          if (row !== cursor) landingRef.current = action.delta;
          setCursor(row);
          root.focus({ preventScroll: true });
        } else {
          controlsOf(target)[0]?.focus();
        }
      } else if (action.kind === "control") {
        const stop = currentStop(root, cursor);
        const target = stop === null ? null : nextControl(stop, action.delta);
        if (target === "stop") root.focus({ preventScroll: true });
        else target?.focus();
      } else if (!edgeFocused) {
        // `a`/`d` press the card's own button, so its guard against a second answer applies here
        // too. Not from a banner's button: the row cursor is hollow there, and not what keys act on.
        const target = permissionTarget(answerableItems, cursor);
        const rows = root.querySelectorAll<HTMLElement>('[data-nav-stop="row"]');
        const button =
          target === null ? null : rows[target]?.querySelector<HTMLButtonElement>(`[data-nav-action="${action.decision}"]`);
        button?.click();
      }
      return;
    }
    event.preventDefault();
    switch (action.kind) {
      case "mode":
        setMode(action.to);
        break;
      case "toggle-expand": {
        // Keyed on the timeline KEY, not the cursor index -- see the doc comment on `expanded`
        // above for why an index would silently drift onto the wrong row.
        const key = timeline[cursor]?.key;
        if (key !== undefined) setExpanded((prev) => ({ ...prev, [key]: !prev[key] }));
        break;
      }
      case "copy": {
        // No item at the cursor (an empty timeline) writes NOTHING, rather than clobbering
        // whatever the user already had on the clipboard with "" -- found in review.
        const item = timeline[cursor];
        if (item !== undefined) void navigator.clipboard?.writeText(copyTextForItem(item));
        break;
      }
      case "restart":
        returnToStartScreen();
        break;
      case "pending-g":
        // Claimed (so the `g` does nothing else) and remembered for exactly one key.
        pendingGRef.current = true;
        break;
      case "half-page": {
        // Half the visible height, as in vim. Then, if the cursor's row is not on screen, the
        // cursor comes to the visible row NEAREST to it: the first one on screen when the row lies
        // above the view, the last one when it lies below. (It used to pick by the key's direction
        // alone, so `Ctrl+d` with the cursor's row already off screen BELOW the view pulled the
        // cursor UP by many rows -- found in review.) The view stays exactly where the scroll put it
        // ("keep"): re-revealing that row with `nearest` would pull the view back by up to a row.
        //
        // A re-home also takes DOM focus back to the root, as `move` and `jump` do. Found in
        // review: without it, a control that had the keys (Approve after `l`, a banner's Dismiss,
        // Stop) KEPT them while the cursor was drawn on another row, so `Enter` activated an
        // Approve scrolled out of sight and `j`/`k` steered from the old stop.
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list === null || root === null) break;
        list.scrollTop += action.delta * Math.max(1, Math.floor(list.clientHeight / 2));
        const l = list.getBoundingClientRect();
        if (l.bottom - l.top <= 0) break;
        const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
        const current = rows[cursor];
        const onScreen = visibleRows(list);
        if (current === undefined || onScreen.includes(current) || onScreen.length === 0) break;
        const below = current.getBoundingClientRect().top >= l.bottom;
        const next = rows.indexOf(below ? onScreen[onScreen.length - 1] : onScreen[0]);
        if (next !== -1 && next !== cursor) {
          landingRef.current = "keep";
          setCursor(next);
        }
        root.focus({ preventScroll: true });
        break;
      }
      case "jump": {
        // `G` goes to the very END of the list, not merely to the last row's top: on a long last
        // reply, that is the line you want. `gg` goes to the very top.
        const last = Math.max(timeline.length - 1, 0);
        const target = action.to === "first" ? 0 : last;
        if (target !== cursor) landingRef.current = "keep";
        setCursor(target);
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list !== null) list.scrollTop = action.to === "first" ? 0 : list.scrollHeight;
        root?.focus({ preventScroll: true });
        break;
      }
    }
  };

  /* A session that died is announced here, not left to be inferred from a status word in the
     header. `unavailable` specifically means this client stopped being able to observe the session
     -- the transcript above it can be missing its tail, or a piece out of its middle -- so the
     reason text (which says exactly what was lost) is rendered in full and cannot be dismissed.
     A hidden warning about incomplete output is the same thing as no warning. */
  const sessionEndedBanner =
    state.status.kind === "unavailable" ? (
      <Row kind="error" sign="✗" role="alert">
        <strong>This session was lost. What is shown above may be incomplete.</strong>
        <pre>{state.status.reason}</pre>
        {/* `r` is offered only on this banner and the one below, for returning to the start
            screen -- see `onKeyDown`'s "restart" arm above and `resolveKey`'s `case "r"` in
            `./keymap`, which is what actually enforces "only where the spec offers it". The mode
            this hint is read in is always BROWSE: `resolveKey` refuses `i` once the session has
            ended and the effect near the top of this component forces BROWSE if it died while
            INPUT was active, so this sentence cannot be on screen in a mode that drops `r`. */}
        <div className="row-hint">Press r to return to the start screen.</div>
      </Row>
    ) : state.status.kind === "closed" ? (
      // Deliberately NOT `row-error`/`✗`: an ordinary close (the host closed it, the provider
      // exited cleanly) is not an error, and styling it like the lost-session row above would
      // train the eye to ignore the one that matters. See `.row-ended` in index.css for the
      // fuller record of why these two were briefly unified and then split back apart.
      <Row kind="ended" sign="·">
        This session has ended ({state.status.reason}).
        <div className="row-hint">Press r to return to the start screen.</div>
      </Row>
    ) : null;

  return (
    <div
      className="agent-ui-root agent-ui-conversation"
      ref={containerRef}
      // Real, focusable, so BROWSE's keydown handler has a DOM node to bubble from -- see the
      // effect above `onKeyDown` that keeps focus here whenever `mode` is "browse".
      tabIndex={0}
      onKeyDown={onKeyDown}
      onFocus={(event) => {
        const stop = (event.target as HTMLElement).closest("[data-nav-stop]");
        setEdgeFocused(stop !== null && stop.getAttribute("data-nav-stop") !== "row");
      }}
    >
      <Winbar state={state} permissionMode={startedPermissionMode} />
      {errorBanner}
      {/* `sessionEnded` makes every pending card inert. The cards themselves are NOT removed: a
          permission that was still open when the session died is real history, and deleting it
          would read as a resolution nobody made. */}
      <MessageList state={state} sessionEnded={sessionEnded} expanded={expanded} cursor={cursor} focused={paneFocused && !edgeFocused} onAnswerPermission={answerPermission} />
      {sessionEndedBanner}
      {commandNoticeBanner}
      <StatusLine
        mode={mode}
        paneFocused={paneFocused}
        state={state}
        position={{ index: cursor, total: timeline.length }}
        // Stop is gated on the capability, never on the backend's name. Spec §3.4's Send/Stop pair
        // leaving the composer is done in the same change as this StatusLine addition (see
        // `Composer.tsx`): this is now the ONLY Stop control, for mouse users, since Enter alone
        // sends from the keyboard and there is no other way to interrupt a turn without a keyboard.
        canInterrupt={state.capabilities.interrupt}
        onInterrupt={interrupt}
      />
      <Composer
        // A dead session takes no more turns, and neither does one already being closed for a
        // terminal handoff. Without this, clearing `activeTurnId` on a lost session would have
        // handed the user an enabled composer pointed at nothing.
        disabled={turnInProgress || sessionEnded || handingOff}
        sessionEnded={sessionEnded}
        closing={handingOff}
        restoredDraft={restoredDraft}
        mode={mode}
        focusRequest={inputRequest}
        onModeChange={setMode}
        onSend={sendMessage}
      />
      {/* Below the composer, deliberately: it is a way OUT of this panel, not one of the things the
          panel is for, and it must not compete with the lost-session banner for the space directly
          above the box. Offered for a session that has ENDED too -- continuing a conversation that
          died here is arguably the case where a terminal helps most -- so the only
          conversation-state term is whether a turn is running. `canResume` comes from the provider's
          advertised capability, never from the backend's name. */}
      <ContinueInTerminal
        providerSessionId={state.providerSessionId}
        turnInProgress={turnInProgress}
        canResume={state.capabilities.resume}
        handingOff={handingOff}
        onHandoff={handoffToTerminal}
      />
    </div>
  );
}
