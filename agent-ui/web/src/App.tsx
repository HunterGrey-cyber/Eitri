import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { applyEvent, applySnapshot, initialState, resumeAttached } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import type { OutboundMessage, PermissionDecision } from "./bridge";
import { noteUserScroll } from "./follow";
import { resolveKey } from "./keymap";
import type { KeyLike, KeymapHelp, PanelMode, PendingPrefix } from "./keymap";
import { buildTimeline, oldestPendingPermission, promptIndex } from "./timeline";
import { buildDisplay, indexOfKey } from "./display";
import { outputText, primaryText } from "./copyText";
import { countCodePoints } from "./toolRegistry";
import { findMatch } from "./search";
import { SearchBar } from "./components/SearchBar";
import { answerTarget, controlsOf, currentStop, hintTargets, nextControl, nextStop, rowIndexOf, HINT_ALPHABET } from "./nav";
import type { AnswerableItem, HintTarget } from "./nav";
import type { TimelineItem } from "./timeline";
import { pathsIn, viewText } from "./paths";
import type { PathRef } from "./paths";
import { PathPick } from "./components/PathPick";
import { acceptsEnvelope, activeTabInfo, forgetClosed, saveView, takeView, withoutHandoff } from "./tabs";
import type { TabViewState } from "./tabs";
import { EmptyTab } from "./components/EmptyTab";
import { Composer } from "./components/Composer";
import type { RestoredDraft } from "./components/Composer";
import { TabBar } from "./components/TabBar";
import { MessageList } from "./components/MessageList";
import { Row } from "./components/Row";
import { ActivityLine } from "./components/ActivityLine";
import { StatusRow } from "./components/StatusRow";
import { Footer, INPUT_IDLE_HINT, INPUT_RUNNING_HINT } from "./components/Footer";
import { QueueLines } from "./components/QueueLines";
import { ContextLine } from "./components/ContextLine";
import { DetailPopover } from "./components/DetailPopover";
import { ContinueInTerminal } from "./components/TerminalHandoff";
import { HintLayer } from "./components/HintLayer";
import type { ShownHint } from "./components/HintLayer";
import { WhichKey } from "./components/WhichKey";
import { KeymapOverlay } from "./components/KeymapOverlay";
import { Chooser } from "./components/Chooser";
import { stripEntries } from "./whichKey";
import { DRAFT_MIRROR_DELAY_MS, modePill, showTabBar } from "./tabs";
import { statusRowText, statusWarning } from "./statusRow";
import type { AgentUiState, HandoffCommand, Hello, DetailRow, TabId, TabsEnvelope, TurnClock, ChooserEnvelope, ContextSummary, QueueItem } from "./types";
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

/** Whether a key target is inside an activatable control: a button, summary, link or role=button.
 *  Preventing Enter on a focused button cancels its native activation, including permission answers.
 *  Space on buttons and Enter on summaries need the same protection.
 *  `closest` handles both the control itself and nested spans or other child elements. */
function isActivatableControl(el: EventTarget | null): el is HTMLElement {
  if (!(el instanceof HTMLElement)) return false;
  return el.closest("button, summary, a[href], [role=button]") !== null;
}

/** Pixel step for one `j`/`k` scroll inside a tool result's overflow box.
 *  The value is intended to feel like a row step and has not been checked on screen. */
const TOOL_RESULT_SCROLL_STEP_PX = 40;

/** The longest this panel swallows keys after `f` asked `shell` for a HINT, if that HINT never ends
 *  here (see `hintPendingRef`). Long enough for a GTK main loop busy with a frame and the 33ms pump; short
 *  enough that a `shell` which never answers (the HINT was refused, or this page is not hosted by
 *  `shell` at all) costs a second of dead keys, not a stuck panel -- the same stance as `shell`'s own
 *  300ms wait for this panel's answer. */
export const HINT_PENDING_TIMEOUT_MS = 1000;

/** Delay before a lone BROWSE `g` shows the which-key strip when no next key arrives. */
export const WHICH_KEY_G_PREFIX_DELAY_MS = 400;

/** Scroll the cursor row's overflow box before applying a pending `j`/`k` cursor move.
 *  This keeps long tool results reachable from the keyboard instead of skipping unseen content.
 *  DOM measurements belong here, after `resolveKey` chooses a motion, outside the pure key table.
 *  Return `false` when there is no box or it has reached its limit in that direction, so the cursor moves.
 *  Write `scrollTop` directly for a clamped pixel step.
 *  jsdom tests must stub the box dimensions to exercise scrolling because it has no layout. */
function scrollCursorRowBox(container: HTMLDivElement | null, direction: 1 | -1): boolean {
  const box = container?.querySelector<HTMLElement>(".row-current .tool-result-body") ?? null;
  if (box === null) return false;
  // The user can scroll the list away while the cursor stays on this row. Scrolling an
  // off-screen box changes nothing visible, and an unchanged cursor runs no reveal effect.
  // Consume the first press to bring the row back into view; later presses scroll its box.
  // Tests must mock both rects because jsdom's zero rects read as off screen.
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
 *  though it has not been looked at on a screen. */
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

/** `j`/`k` while the `?` keymap overlay is open: scrolls the overlay itself, by the
 *  same three-line step a tall row uses (`rowScrollStep`) rather than a fresh pixel constant --
 *  the plan asks for exactly this reuse. `el.scrollTop` clamps itself at both ends, the same as
 *  every other raw `scrollTop` write in this file, so there is nothing else here to bound. */
function scrollKeymapOverlay(el: HTMLDivElement | null, direction: 1 | -1) {
  if (el === null) return;
  el.scrollTop += direction * rowScrollStep(el);
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

/** Scroll within the current row while it extends past the visible edge in the motion's direction.
 *  Moving straight to the next row skips long replies and leaves a long last reply's tail unreachable.
 *  `scrollCursorRowBox` keeps priority for a tool result's capped box.
 *  Each step is limited to the remaining hidden text; after the edge is revealed, the next press moves on.
 *  An entirely off-screen row is revealed first, consuming that press.
 *  Return `false` without usable layout or when `scrollTop` cannot move, so a stalled scroll allows a motion. */
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

/** After a scroll that was not caused by cursor motion, move an entirely off-screen cursor to
 *  the nearest visible row. Return `null` to leave it unchanged. Reads geometry without writing
 *  `scrollTop`. */
export function clampCursorToView(list: HTMLElement, rows: HTMLElement[], cursor: number): number | null {
  const current = rows[cursor];
  if (current === undefined) return null;
  const l = list.getBoundingClientRect();
  const r = current.getBoundingClientRect();
  if (r.bottom > l.top && r.top < l.bottom) return null;
  const onScreen = visibleRows(list);
  if (onScreen.length === 0) return null;
  const target = r.bottom <= l.top ? onScreen[0] : onScreen[onScreen.length - 1];
  const index = rows.indexOf(target);
  return index === -1 ? null : index;
}

/** The tool name of the oldest pending permission, selected by lowest `seq`, for
 *  `ActivityLine`'s "needs approval" slot. Returns `null` when nothing waits. */
function oldestPendingTool(state: AgentUiState): string | null {
  let oldest: (typeof state.pendingPermissions)[number] | null = null;
  for (const request of state.pendingPermissions) {
    if (oldest === null || request.seq < oldest.seq) oldest = request;
  }
  return oldest?.toolName ?? null;
}

export default function App() {
  const [state, setState] = useState(initialState());
  const [hello, setHello] = useState<Hello | null>(null);
  /** Every tab and which is active, from Rust's `tabs` envelope. `null`
   *  until the first one arrives (before that, this window has not been told which tab exists yet). */
  const [tabs, setTabs] = useState<TabsEnvelope | null>(null);
  /** The active tab for the dispatch handler, which is installed once: set synchronously inside the
   *  handler, so the snapshot that follows a `tabs` envelope in the same batch is accepted
   *  (`acceptsEnvelope` reads this, not `tabs` state, which would still hold the PREVIOUS render's
   *  value until React re-renders). */
  const activeTabRef = useRef<TabId | null>(null);
  /** What each tab's reader had on screen (cursor, mode, expansions, scroll, draft), keyed by tab
   *  id and never sent to Rust in phase 2. Saved just before a switch resets the render
   *  state below, and handed back through `restoreRef` once the new tab's own `snapshot` arrives. */
  const viewStore = useRef(new Map<TabId, TabViewState>());
  /** The view to restore once the switch's `snapshot` (or, for a tab with none, the switch itself)
   *  is applied. Read and cleared by the `snapshot` arm's layout effect below. */
  const restoreRef = useRef<TabViewState | null>(null);
  /** Set by the `tabs` arm whenever `payload.active` names a different tab than before (mount
   *  included, the same condition that arm's own per-tab reset already runs on), and
   *  read-and-cleared by the `snapshot` arm right after: landing on a tab already
   *  holding a card puts the keys on it, the same as `Ctrl+l` does within one tab. */
  const switchedRef = useRef(false);
  const activeTab = activeTabInfo(tabs);
  /** True once the active tab actually holds a session, live or already ended -- a `starting` or
   *  `failed` tab renders the empty tab too: it is not "started" until Rust has
   *  really opened a session for it. Derived, not state -- Rust's `tabs` envelope is the only source
   *  of a tab's `state`. */
  const sessionStarted = activeTab !== null && (activeTab.state === "live" || activeTab.state === "ended");
  /** `sessionStarted` for the bridge listener, which is registered once and would otherwise read
   *  its first render's value forever. Assigned during render, so it is current by the time any
   *  later envelope arrives. */
  const sessionStartedRef = useRef(false);
  sessionStartedRef.current = sessionStarted;
  /** A fatal, session-ending failure, shown in the panel. Replaces window.alert, which cannot be
   * copied, cannot show the sidecar's own multi-line startup diagnostics, and blocks the WebView. */
  const [fatalError, setFatalError] = useState<string | null>(null);
  /** The command for a conversation that has just been closed here and moved to a terminal. Set by
   *  the `handoff` envelope, which Rust sends only after the real session close finished.
   *
   *  **This is a view of Rust's own `AgentPanelState::last_handoff`, not the only copy.** Rust keeps
   *  the command and re-sends it in the `ready` handshake, so a panel reload (prefix r, the top
   *  bar's ⟳) or a WebView crash gets it back — on the default legacy backend the id in it is
   *  recoverable from nowhere else at all. Cleared here when a real `snapshot` arrives, which is
   *  also when Rust clears its copy: a session that is actually running, not one merely asked for. */
  const [handoff, setHandoff] = useState<HandoffCommand | null>(null);
  /** The requestId of each tab's in-flight `handoff_to_terminal`, keyed by the tab it closes.
   *
   *  An entry means that tab's conversation is CLOSING: Rust has already taken the backend out of
   *  its own state and a worker thread is running the real `shutdown()`. Nothing is torn down here
   *  until the `handoff` envelope arrives, so without this the composer would stay live and a typed
   *  Enter would clear the box into a session that no longer exists.
   *
   *  **Per tab, not per window**: one window-wide id locked
   *  the composer of whichever tab was on screen, so handing off tab 1 and switching to a live tab 2
   *  left tab 2 refusing input until tab 1's close answered. An entry is removed by its own
   *  `command_result` (Rust owes one for every handoff, including one whose tab was closed), or by
   *  a `handoff`/`error` envelope naming its tab. */
  const [handoffRequests, setHandoffRequests] = useState<ReadonlyMap<TabId, string>>(new Map());
  /** A refused command, in words, on screen. Rust's refusals carry real human-readable reasons
   *  (`HandoffRefusal::message`, `BackendError::message`) and every one of them used to reach a
   *  `console.warn` and nothing else. */
  const [commandNotice, setCommandNotice] = useState<string | null>(null);
  /** Text to put back in the composer after a refused send. See `RestoredDraft`. */
  const [restoredDraft, setRestoredDraft] = useState<RestoredDraft | null>(null);
  /** What each in-flight request actually was, so its reply can be handled as that thing. A ref: it
   *  is bookkeeping, never rendered, and a render per outgoing command would be pure cost.
   *
   *  Only the kinds whose replies need special handling are recorded. An interrupt or a permission
   *  response has no entry and comes back as `undefined`, which is correct rather than a gap: its
   *  refusal takes the plain "show the reason" path. Every recorded request gets exactly one
   *  `command_result` and is deleted there, so this cannot grow.
   *
   *  `"editor"` covers `edit_draft`/`open_path`/`view_in_editor`: all three reach the
   *  scratch editor in Rust, and a refusal of any of them is a footer flash (`showFlash`), not the
   *  banner -- a scratch-editor round trip that a `gf` or `Ctrl+g` failed to start is a footer
   *  nicety, not something that should fill the space a real conversation error gets. */
  const inFlight = useRef<Map<string, { kind: "send" | "handoff" | "editor"; tab: TabId; text?: string }>>(
    new Map(),
  );
  /** Monotonic, so two refusals of the same text are two distinct restores. */
  const restoreSeq = useRef(0);
  /** Bumped only when the `snapshot` arm actually applies a saved view's cursor/mode/expanded
   *. The scroll-restoring layout effect below keys on THIS, not on `state`
   *  directly: `state` also changes on the switch's OWN reset (`setState(initialState())`, in the
   *  `tabs` arm, same render as `restoreRef.current` is first set), and a `[state]` dependency fired
   *  there too -- before the new tab's real snapshot ever arrived -- consuming `restoreRef.current`
   *  on an empty transcript and leaving nothing for the real restore. Reproduced by the "keeps each
   *  tab's cursor across a switch" test failing with this dependency; fixed by decoupling the two. */
  const [restoreTick, setRestoreTick] = useState(0);
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
  /** Elapsed time for the current turn, stored in state because it renders in `ActivityLine`.
   *  `exact: true` requires `turn_started` in an `events` envelope. A turn first seen in a snapshot
   *  gets `exact: false` because its earlier duration is unknown.
   *  Clear it on the events that clear `state.activeTurnId`. Read raw envelopes because folded state
   *  does not retain whether the turn was first seen in events or a snapshot. */
  const [turnClock, setTurnClock] = useState<TurnClock | null>(null);
  /** Which timeline rows show their full tool result rather than the folded placeholder, keyed by
   *  the timeline `key` -- never the cursor index, because a resolved permission removes a card and
   *  shifts every later index, which would silently move an expansion onto a different row.
   *  Toggled by `Enter` on the row under the cursor, in `onKeyDown` below. */
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  /** `Ctrl+o` shows full results, wider cuts, and no collapsed runs. Saved per tab in
   *  `TabViewState.detailed`, like `expanded`, and restored on a switch. A tab with no saved
   *  view starts with `false`. */
  const [detailed, setDetailed] = useState(false);
  /** BROWSE (read, move the cursor, act on "this item") / INPUT (typing into the composer) / HINT
   *. See
   *  `./keymap`'s own doc comment on `PanelMode`. */
  const [mode, setMode] = useState<PanelMode>("browse");
  /** Whether this pane has keyboard focus, reported by `shell` in `pane_focus`.
   *  Starts false because startup focuses the editor; `ready` resends the value to a new document.
   *  The mode block is bright only while the pane is focused, so it identifies which pane receives keys. */
  const [paneFocused, setPaneFocused] = useState(false);
  /** `paneFocused` for the `snapshot` dispatch arm, which cannot read render state
   *  directly -- the same reason `sessionStartedRef` exists. A switch landing on a card must check
   *  whether the keys are even in this window right now: landing them silently in a background
   *  window would be a surprise nobody asked for. */
  const paneFocusedRef = useRef(false);
  paneFocusedRef.current = paneFocused;
  /** Bumped by each `enter_input` envelope (`Ctrl+l` from the editor). A counter makes repeated
   *  requests act and lets the handler stay installed once without reading state. The effect
   *  checks the current render for INPUT availability. Passed to `Composer` as `focusRequest`
   *  to refocus an already mounted textarea even when the mode is already INPUT. `arrivalRef`
   *  distinguishes an arrival that may land on a waiting card from a direct request for the
   *  composer. */
  const [inputRequest, setInputRequest] = useState(0);
  /** Set `true` only by the `enter_input` dispatch arm, and read-and-cleared by the `inputRequest`
   *  effect on the very next run: `Ctrl+l` is `shell`'s one accelerator for "give
   *  this pane the keys AND put them in the composer", which is the arrival this rule means to catch.
   *  A HINT landing on the composer bumps `inputRequest` too (through the same effect), but is not an
   *  arrival in this sense -- the user aimed at the composer specifically, so a card waiting must not
   *  hijack that keystroke into BROWSE instead. */
  const arrivalRef = useRef(false);
  /** Bumped by each `focus_permission` envelope. A counter lets the handler stay installed once
   *  without reading state; the effect finds the card against the current render. */
  const [permissionRequest, setPermissionRequest] = useState(0);
  /** Whether keyboard focus is on a stop that is NOT a conversation row -- a banner's Dismiss, the
   *  status line's Stop, the handoff button (`./nav`). While it is, the row cursor is drawn hollow,
   *  the same way it is when another pane has the keys: it is where `k` brings you back to, and it
   *  is not what the keys act on now. Derived from DOM focus (`onFocus` on the root), never set by
   *  the key handler alone, so a click or a `Tab` onto one of those buttons agrees with `j`/`k`. */
  const [edgeFocused, setEdgeFocused] = useState(false);
  /** The index into `timeline` that `j`/`k` move and `Enter`/`y` act on. */
  const [cursor, setCursor] = useState(0);
  /** `cursor`/`mode`/`expanded`/the composer's draft, mirrored into refs for the `tabs` dispatch
   *  handler, which cannot read render state directly --
   *  the same reason `sessionStartedRef` exists above. Assigned during render, so a switch always
   *  saves what the CURRENT render actually shows, not a stale one from before the last update. */
  const cursorRef = useRef(cursor);
  cursorRef.current = cursor;
  const modeRef = useRef(mode);
  modeRef.current = mode;
  const expandedRef = useRef(expanded);
  expandedRef.current = expanded;
  const detailedRef = useRef(detailed);
  detailedRef.current = detailed;
  /** The composer's own unsent text, kept only so a tab switch can save it -- never
   *  read to render anything itself; `restoredDraft` is what actually reaches `Composer`. Updated
   *  from `Composer`/`EmptyTab`'s `onDraftChange`, which fires on every keystroke and once more
   *  with `""` after a send. */
  const draftRef = useRef("");
  /** The queue behind a running turn, prompt history, rule offers, editor context, and whether
   *  the scratch editor holds this tab's draft. All are tab-scoped except `history` and
   *  `editorContext`, which are window-wide (`tabs.ts`'s `TAB_SCOPED`). */
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [queueError, setQueueError] = useState<string | null>(null);
  const [history, setHistory] = useState<string[]>([]);
  const [ruleOffers, setRuleOffers] = useState<Record<string, string>>({});
  const [editorContext, setEditorContext] = useState<ContextSummary | null>(null);
  const [scratchEditing, setScratchEditing] = useState(false);
  /** The footer's transient line. `seq` so the same text twice is two flashes. */
  const [flash, setFlash] = useState<{ text: string; seq: number } | null>(null);
  const flashSeq = useRef(0);
  function showFlash(text: string) {
    flashSeq.current += 1;
    setFlash({ text, seq: flashSeq.current });
  }
  // A newer flash replaces an older one (`showFlash` above); this only ever clears the flash that
  // is STILL the current one when its own two seconds are up, so an older flash's timer firing late
  // cannot erase a newer flash that has since taken its place.
  useEffect(() => {
    if (flash === null) return;
    const seq = flash.seq;
    const timer = setTimeout(() => setFlash((f) => (f?.seq === seq ? null : f)), 2000);
    return () => clearTimeout(timer);
  }, [flash]);
  /** `queue_taken`, for the composer to merge. */
  const [queueTaken, setQueueTaken] = useState<{ texts: string[]; seq: number } | null>(null);
  const takenSeq = useRef(0);
  // `queueError` supplies the queue error line, `editorContext` supplies the editor context line, and
  // `flash` supplies the footer's transient message. `MessageList` receives `ruleOffers` for the
  // permission card's third button.

  /** The row just yanked flashes for 400 ms. */
  const [yanked, setYanked] = useState<{ key: string; seq: number } | null>(null);
  const yankSeq = useRef(0);
  useEffect(() => {
    if (yanked === null) return;
    const seq = yanked.seq;
    const timer = setTimeout(() => setYanked((y) => (y?.seq === seq ? null : y)), 400);
    return () => clearTimeout(timer);
  }, [yanked]);
  function copied(text: string, key: string | undefined) {
    void navigator.clipboard?.writeText(text);
    showFlash(`copied ${countCodePoints(text)} chars`);
    if (key !== undefined) {
      yankSeq.current += 1;
      setYanked({ key, seq: yankSeq.current });
    }
  }

  /** The composer's text reaches Rust 300ms after its last change and immediately when the tab
   *  stops being active. `pendingDraftRef` holds the unsent text and its tab. */
  const pendingDraftRef = useRef<{ tab: TabId; text: string } | null>(null);
  const draftTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  function flushDraft() {
    if (draftTimerRef.current !== null) clearTimeout(draftTimerRef.current);
    draftTimerRef.current = null;
    const pending = pendingDraftRef.current;
    pendingDraftRef.current = null;
    if (pending !== null) postToRust({ type: "draft", request_id: nextRequestId(), tab: pending.tab, text: pending.text });
  }
  function mirrorDraft(text: string) {
    draftRef.current = text;
    const tab = activeTabRef.current;
    if (tab === null) return;
    pendingDraftRef.current = { tab, text };
    if (draftTimerRef.current !== null) clearTimeout(draftTimerRef.current);
    draftTimerRef.current = setTimeout(flushDraft, DRAFT_MIRROR_DELAY_MS);
  }
  useEffect(
    () => () => {
      if (draftTimerRef.current !== null) clearTimeout(draftTimerRef.current);
    },
    [],
  );
  /** How the NEXT cursor change should bring its row on screen (`revealRow`): +1/-1 when `j`/`k`
   *  moved it (a tall row shows its top or its bottom), `"keep"` when the key already put the view
   *  exactly where it belongs (`Ctrl+d`/`Ctrl+u` re-homing, `G`, `gg`), 0 otherwise. A ref, read
   *  and reset by the cursor-follow effect, and only ever set when the cursor really changes -- an
   *  unchanged cursor runs no effect, and a stale value would misplace some later, unrelated move. */
  const landingRef = useRef<1 | -1 | 0 | "keep">(0);
  /** The open `/` prompt and the cursor position when it opened; `lastSearchRef` holds what `n`/`N` repeat. */
  const [search, setSearch] = useState<{ query: string; origin: number } | null>(null);
  const lastSearchRef = useRef("");
  function moveCursorTo(index: number) {
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    noteUserScroll(list, index < cursor ? "up" : "down");
    setCursor(index);
  }
  /** The first key of a two-key BROWSE sequence (`resolveKey`'s `{kind:"pending"}`), or `null`
   *  between sequences. Cleared on EVERY key `onKeyDown` sees, so anything but the matching second
   *  half cancels it, and on every `pane_focus` envelope, so a pane switch the WebView never saw as
   *  a key cancels it too; handed to `resolveKey` in its context so the key table itself keeps no
   *  memory. (Formerly `pendingGRef`, a plain boolean, before `[[`/`]]` gave BROWSE a second prefix.) */
  const pendingRef = useRef<PendingPrefix | null>(null);
  /** The count accumulated from `1`-`9` followed by `0`-`9`, applied to the next
   *  `j`/`k`/`[[`/`]]` and reset by it or any other action. `null` before any digit. Kept in a
   *  ref because `onKeyDown` reads it without rendering it. */
  const countRef = useRef<number | null>(null);
  /** Whether the which-key strip is currently showing the `g` prefix's continuation,
   *  i.e. whether `WHICH_KEY_G_PREFIX_DELAY_MS` has elapsed since the last lone `g` with nothing
   *  cancelling it since. A real state, not a ref like `pendingRef`: this one drives what the
   *  strip renders, so it must cause a re-render when it flips. */
  const [gShown, setGShown] = useState(false);
  /** The pending 400ms timer that would set `gShown` true, or `null` when none is running. Cleared
   *  in every place `pendingRef` itself is cleared (see that ref's own doc comment) -- a `g` that
   *  gets cancelled before the delay elapses must never let a stale timer flip the strip on late. */
  const gShownTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  function cancelGPrefixTimer() {
    if (gShownTimerRef.current !== null) clearTimeout(gShownTimerRef.current);
    gShownTimerRef.current = null;
  }
  function hideGPrefix() {
    cancelGPrefixTimer();
    setGShown(false);
  }
  useEffect(() => cancelGPrefixTimer, []);
  /** Whether the `?` keymap overlay is open. Toggled by `resolveKey`'s `{kind:"keymap"}`
   *  (opening only -- see `onKeyDown`'s dedicated branch below for why closing never goes through
   *  `resolveKey` at all) and by the handful of places that must force it shut: the session ending,
   *  the start screen coming back, and a HINT starting elsewhere in the window. */
  const [keymapOpen, setKeymapOpen] = useState(false);
  /** The detail popover's rows, or `null` when it is closed. Set by a
   *  `tab_detail` envelope (the reply to `open_detail`, sent by `StatusRow` and by `prefix i`);
   *  closed the same places `keymapOpen` is forced shut, since the two are mutually exclusive
   *  overlays over the same conversation area. */
  const [detail, setDetail] = useState<DetailRow[] | null>(null);
  /** The row `j`/`k`/`y` act on inside the popover, reset to 0 every time it opens. */
  const [detailCursor, setDetailCursor] = useState(0);
  /** The paths for a `gf` letter picker, or `null` when none is waiting. Set by `open-path` and
   *  cleared by the letter that selects a path or any other key. */
  const [pathPick, setPathPick] = useState<PathRef[] | null>(null);
  /** `open_path` and `view_in_editor` share `edit_draft`'s `"editor"` request handling.
   *  They have no tab-scoped reply payload to restore; a refusal only needs a footer flash. */
  function openPath(ref: PathRef) {
    const requestId = nextRequestId();
    inFlight.current.set(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_path", request_id: requestId, path: ref.path, ...(ref.line === null ? {} : { line: ref.line }) });
  }
  /** The popover's own scrollable root -- unused today (it has nothing to scroll into view yet),
   *  kept for parity with `keymapOverlayRef` and because a forwarded ref is part of the component's
   *  contract. */
  const detailRef = useRef<HTMLDivElement>(null);
  /** The inline rename field's tab and prefilled text, or `null` when closed.
   *  While open, the tab bar stays visible even with one tab. `begin_rename` opens it;
   *  commit or cancel clears it. */
  const [renaming, setRenaming] = useState<{ tab: TabId; initial: string } | null>(null);
  /** The window-close confirmation prompt, or `null` when closed.
   *  Drawn in the footer in place of the which-key strip, and takes every key while it is open. Set
   *  by a `confirm_close` envelope (`prefix &`); answered by `y` (close) or any other key (cancel)
   *  in `onKeyDown`. */
  const [confirm, setConfirm] = useState<{ tab: TabId; lines: string[] } | null>(null);
  /** The overlay opened by `prefix w` or the launch chooser, listing open tabs then saved
   *  conversations; `null` when closed. Set by a `chooser` envelope and omitted from
   *  `tabs.ts`'s `TAB_SCOPED` because it is a window-wide picker. */
  const [chooser, setChooser] = useState<ChooserEnvelope | null>(null);
  /** The overlay's last two sections and its heading, from `shell`'s `keymap` envelope (sent on
   *  every `ready`, right after the theme). `prefix` defaults to `Ctrl+b`, the stock tmux default,
   *  until that envelope arrives. */
  const [keymapHelp, setKeymapHelp] = useState<KeymapHelp>({ prefix: "Ctrl+b", window: [], prefixKeys: [] });
  /** The overlay's own scrollable root, so `j`/`k` typed while it is open can scroll IT rather than
   *  the conversation underneath (`onKeyDown`'s `keymapOpen` branch). */
  const keymapOverlayRef = useRef<HTMLDivElement>(null);
  /** One ordered view of the conversation for the cursor and expand keys. Keyed on the whole
   *  `state`, like `MessageList`'s memo, plus `expanded` and `detailed`: `buildDisplay` folds
   *  rows using both, so either changing must recompute what the cursor indexes. */
  const timeline = useMemo(
    () => buildDisplay(buildTimeline(state), { expanded, detailed, turnRunning: state.activeTurnId !== null }),
    [state, expanded, detailed],
  );
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
    const landed = landedControlRef.current;
    if (landed !== null && landed.isConnected) {
      landed.focus();
      return;
    }
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
  /** Keep the cursor on the same row when a run collapses or expands and shifts later indices.
   *  `cursorKeyRef` remembers the row key; `indexOfKey` finds it again, including a call folded into a run.
   *  Use `landingRef.current = "keep"` to preserve scroll position. A removed key uses the index clamp above.
   *  Reconcile only when `timeline` changes identity. Running on cursor changes alone would find the
   *  old row at its unchanged index and undo ordinary keyboard navigation. */
  const cursorKeyRef = useRef<string | null>(null);
  const prevTimelineRef = useRef<TimelineItem[] | null>(null);
  useLayoutEffect(() => {
    const timelineChanged = prevTimelineRef.current !== null && prevTimelineRef.current !== timeline;
    prevTimelineRef.current = timeline;
    const want = cursorKeyRef.current;
    if (timelineChanged && want !== null && timeline[cursor]?.key !== want) {
      const found = indexOfKey(timeline, want);
      if (found !== null && found !== cursor) {
        landingRef.current = "keep";
        setCursor(found);
        return;
      }
    }
    cursorKeyRef.current = timeline[cursor]?.key ?? null;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline, cursor]);
  /** Keeps the cursor row visible when the cursor moves, using `.row-current` through `containerRef`.
   *  Following new messages depends on scroll position, so reading earlier text does not force the view
   *  to the bottom. jsdom can check which element is revealed but cannot prove the resulting layout. */
  useEffect(() => {
    const landing = landingRef.current;
    landingRef.current = 0;
    if (landing === "keep") return;
    const row = containerRef.current?.querySelector<HTMLElement>(".row-current");
    if (row == null) return;
    revealRow(row.closest<HTMLElement>(".message-list"), row, landing);
  }, [cursor]);
  /** A wheel, drag or browser scroll key can leave the cursor's row off screen. Move the cursor
   *  to the nearest visible row without moving the view; `clampCursorToView` never writes
   *  `scrollTop`. `sessionStarted` matches the conversation layout's mount condition, so this
   *  attaches only while `.message-list` exists. */
  const cursorRefForScroll = useRef(cursor);
  cursorRefForScroll.current = cursor;
  useEffect(() => {
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    if (!list) return;
    const onListScroll = () => {
      const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
      const next = clampCursorToView(list, rows, cursorRefForScroll.current);
      if (next !== null && next !== cursorRefForScroll.current) {
        landingRef.current = "keep";
        setCursor(next);
      }
    };
    list.addEventListener("scroll", onListScroll, { passive: true });
    return () => list.removeEventListener("scroll", onListScroll);
  }, [sessionStarted]);
  /** At the bottom, the cursor follows the new last row as the conversation grows, and a sent
   *  prompt takes the cursor. Key on `timeline`: a state change that does not grow the timeline
   *  must not move the cursor. The events dispatcher sets `landOnPromptRef` on
   *  `user_prompt_submitted`; read and clear it here, once the target row exists in this
   *  render's timeline. */
  const lastLengthRef = useRef(0);
  const landOnPromptRef = useRef(false);
  useEffect(() => {
    const previous = lastLengthRef.current;
    lastLengthRef.current = timeline.length;
    if (landOnPromptRef.current) {
      landOnPromptRef.current = false;
      for (let i = timeline.length - 1; i >= 0; i--) {
        if (timeline[i].kind === "prompt") {
          setCursor(i);
          return;
        }
      }
    }
    if (timeline.length <= previous || cursor !== previous - 1) return;
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    const atBottom = !list || list.scrollHeight - list.scrollTop - list.clientHeight <= 1;
    if (atBottom) {
      landingRef.current = "keep";
      setCursor(timeline.length - 1);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline]);
  /** The scroll half of a tab switch's view restore: cursor and mode are set
   *  directly in the `snapshot` handler above, but the row the cursor lands on must exist in the DOM
   *  first, so the scroll position waits for the render that `setState` there causes. A layout
   *  effect, not the ordinary one, so it runs before the browser paints the restored state at the
   *  wrong scroll position for even one frame. `noteUserScroll` marks it the same way a HINT landing
   *  or `focus_permission` does, so `MessageList`'s own follow-the-newest-message effect does not
   *  fight it or snap back on the very next delta.
   *
   *  Keyed on `restoreTick`, not on `state` directly (see that ref's own doc comment): `state` also
   *  changes on the switch's OWN reset, one render before the real snapshot this is meant to act on,
   *  and firing there would consume `restoreRef.current` against an empty transcript. */
  useLayoutEffect(() => {
    const view = restoreRef.current;
    if (view === null) return;
    restoreRef.current = null;
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    if (!list) return;
    noteUserScroll(list, "unknown");
    list.scrollTop = view.atBottom ? list.scrollHeight : view.scrollTop;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [restoreTick]);
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
  // The `?` keymap belongs to a live conversation; a session that just
  // ended is not a reason to keep it up, and the ended/lost banners' own `r` must be reachable
  // without an extra `?`/`Escape`/`q` first.
  useEffect(() => {
    if (sessionEnded) setKeymapOpen(false);
  }, [sessionEnded]);
  /* `Ctrl+l` lands in INPUT with a blinking caret,
     as it did before BROWSE became the landing mode. Refused under the same condition `i` is
     (`resolveKey`): a dead session has no box to type into, and no session at all has no
     composer. Keyed on the request alone, so a session change never opens the composer by itself. */
  useEffect(() => {
    if (inputRequest === 0) return;
    const arrival = arrivalRef.current;
    arrivalRef.current = false;
    // An arrival with a pending card in this tab lands on the card in BROWSE, like
    // `focus_permission`. Only `enter_input` (`Ctrl+l`) is treated as an arrival here. A HINT
    // landing on the composer uses the same counter but must still enter the composer.
    if (arrival && oldestPendingPermission(timeline) !== null) {
      setPermissionRequest((n) => n + 1);
      return;
    }
    if (!sessionStarted || sessionEnded) return;
    setMode("input");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inputRequest]);
  /* `focus_permission`: BROWSE, with the cursor on the oldest pending card, so
     `a`/`d` answer it at once. `shell` sends it only when it counts a card; if the card was answered
     in between, there is nothing to land on, and the arrival is an ordinary keyboard one -- the
     composer, as `enter_input` gives it. Keyed on the request alone, like `inputRequest`. */
  useEffect(() => {
    if (permissionRequest === 0) return;
    const index = oldestPendingPermission(timeline);
    if (index === null) {
      setInputRequest((n) => n + 1);
      return;
    }
    setMode("browse");
    // Landing moves the cursor, and the `[cursor]` effect reveals its row. Announce this as a
    // user scroll through `./follow.ts`, as for a HINT landing. Otherwise `MessageList` keeps
    // following and the next delta or resize can scroll the older card out of view.
    noteUserScroll(containerRef.current?.querySelector(".message-list"), "unknown");
    // A landing always reveals the card, even over a `"keep"` some earlier restore left behind; and
    // when the cursor is already on it (a switch restored it there), nothing fires the `[cursor]`
    // effect, so the row is revealed here -- after the restore's own scroll, which is a layout effect.
    landingRef.current = 0;
    if (index === cursorRef.current) {
      const row = containerRef.current?.querySelector<HTMLElement>(".row-current");
      if (row) revealRow(row.closest<HTMLElement>(".message-list"), row, 0);
    } else setCursor(index);
    containerRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [permissionRequest]);
  /** The start screen's own DOM anchor -- `HintLayer`'s containing block and the `containerRef ??
   *  startScreenRef` fallback a couple of handlers below use. It used to also be focused whenever
   *  `!sessionStarted` (so `y`, handled by the now-deleted `handleStartScreenKeyDown`, was reachable
   *  without a prior click). `EmptyTab` (F3) owns that job for itself now, autofocusing its own live
   *  composer -- and this effect, left in place, stole that focus right back the moment the tab
   *  changed state: the composer's `autoFocus` runs at DOM insertion, but this ran on `sessionStarted`
   *  flipping in a `useEffect`, which fires afterwards and moved focus to the root, blurring the
   *  textarea into `Composer`'s own BROWSE fallback (found while testing the terminal-handoff flow: a
   *  fresh empty tab rendered its composer hint instead of a textarea). */
  const startScreenRef = useRef<HTMLDivElement>(null);

  /* The panel's half of the global `f` HINT.
     `shell` owns the session and every key typed during it -- the panel never sees those keys, so
     there is no HINT key handling here at all. The panel only answers `shell`'s five envelopes:
     report and freeze its visible targets, draw their labels, narrow them, land on one, clear.
     Refs, because the dispatch handler is installed once and must never read stale render state. */
  /** The HINT session this panel is part of, or null. Every envelope naming another one is ignored
   *: a late `hint_show` from a session `shell` already gave up on must not
   *  draw labels over a panel that has moved on. */
  const hintSessionRef = useRef<number | null>(null);
  /** The targets frozen at `hint_collect`, in the order `shell` addresses them by index. Frozen, not
   *  re-read, so an index means the same element for the whole session even if the DOM changes. */
  const frozenRef = useRef<HintTarget[]>([]);
  const [hints, setHints] = useState<ShownHint[]>([]);
  const [hintTyped, setHintTyped] = useState("");
  /** A code block HINT landed on: the next `y` copies exactly that block's code rather than the
   *  whole message. Cleared by that copy and by any other key the table resolves. */
  const copyCodeRef = useRef<HTMLElement | null>(null);
  /** A control HINT landed on, for the `mode` effect above to leave focused: landing from INPUT
   *  blurs the composer, which sets BROWSE, and that effect would otherwise take focus straight back
   *  to the root. Consumed or dropped on the very next commit (the effect just below). */
  const landedControlRef = useRef<HTMLElement | null>(null);
  useEffect(() => {
    landedControlRef.current = null;
  });

  /** Set while a requested HINT is pending, until `endHint` or the timeout clears it.
   *  `shell` installs its key controller only after handling the script message. Until then, letters
   *  typed after `f` could reach panel commands, answer cards or request another HINT.
   *  The capture listener swallows these keys before panel handlers see them.
   *  Drop them without replay: labels are not drawn yet, so they cannot identify a visible target. */
  const hintPendingRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  function clearHintPending() {
    if (hintPendingRef.current !== null) clearTimeout(hintPendingRef.current);
    hintPendingRef.current = null;
  }
  /** `f`: ask `shell` for a HINT, once. A held `f`'s auto-repeat asks nothing (review: two requests
   *  reaching `shell` toggled the HINT straight back off). */
  function requestHint(repeat: boolean) {
    if (repeat || hintPendingRef.current !== null) return;
    postToRust({ type: "hint_request", request_id: nextRequestId() });
    hintPendingRef.current = setTimeout(() => {
      hintPendingRef.current = null;
    }, HINT_PENDING_TIMEOUT_MS);
  }
  useEffect(() => {
    function swallowWhilePending(event: globalThis.KeyboardEvent) {
      if (hintPendingRef.current === null) return;
      event.preventDefault();
      event.stopImmediatePropagation();
    }
    // Capture phase on the window: ahead of React's own listener, the document-level replay above,
    // and the composer's and every control's own keydown -- including a focused button's native
    // activation, which is the default action this prevents.
    window.addEventListener("keydown", swallowWhilePending, true);
    return () => {
      window.removeEventListener("keydown", swallowWhilePending, true);
      clearHintPending();
    };
  }, []);

  useEffect(() => {
    installDispatch((payload) => {
      // Every session-scoped envelope names its tab; the panel keeps only
      // the active tab's state, so one for any other tab -- a batch still in flight when the user
      // switched away -- is dropped here, before any of the arms below can touch `state`.
      if (!acceptsEnvelope(payload as { kind: string; tab?: number }, activeTabRef.current)) return;
      if (payload.kind === "theme") {
        applyTheme(payload.vars);
      } else if (payload.kind === "pane_focus") {
        // A pending prefix is for the very next key; a pane switch in between (GTK takes `Ctrl+h`/
        // `Ctrl+k` before the WebView sees a keydown) must not leave it armed for a key pressed much
        // later, when it would complete a chord nobody meant to start -- review.
        pendingRef.current = null;
        // The which-key strip's own memory of that `g` is cancelled the same way and
        // for the same reason: a switch away and back must not leave its 400ms timer running for a
        // press that has nothing to do with the `g` that started it.
        hideGPrefix();
        // A keymap for THIS panel has no reason to stay drawn while another pane has the keys, and
        // leaving it up is how the review reproduced a dead keyboard: come back with `Ctrl+l`,
        // land in INPUT, and every keystroke is swallowed by the overlay's own branch (review).
        setKeymapOpen(false);
        setDetail(null);
        // The `/` prompt belongs to this panel, so it closes here like the `?` overlay.
        setSearch(null);
        setPaneFocused(payload.focused);
      } else if (payload.kind === "enter_input") {
        // Same reason, the other half of that reproduction: this puts the caret in the composer, so
        // the overlay must not be left covering it and eating what gets typed.
        setKeymapOpen(false);
        setDetail(null);
        // Mark the `Ctrl+l` arrival separately from a HINT landing on the composer, which
        // increments the same counter. The next `inputRequest` effect reads and clears this flag.
        arrivalRef.current = true;
        setInputRequest((n) => n + 1);
      } else if (payload.kind === "focus_permission") {
        // The overlay would cover the card the cursor is about to land on.
        setKeymapOpen(false);
        setDetail(null);
        setChooser(null);
        setPermissionRequest((n) => n + 1);
      } else if (payload.kind === "keymap") {
        setKeymapHelp({ prefix: payload.prefix, window: payload.window, prefixKeys: payload.prefixKeys });
      } else if (payload.kind === "literal_key") {
        // `send-prefix`/`send-keys` from shell: WebKitGTK cannot be handed the key itself. `C-a` is
        // what a text field does with it -- select all of the focused one; any other key is ignored.
        // Deliberately NOT `isEditableElement`: this needs "has a text selection", which is exactly
        // these two types.
        if (payload.key !== "C-a") return;
        const el = document.activeElement;
        if (el instanceof HTMLTextAreaElement || (el instanceof HTMLInputElement && el.type === "text")) {
          el.select();
        }
      } else if (payload.kind === "open_keymap") {
        // The start screen draws no overlay. Arming it here anyway would pop it up over
        // the conversation once a session starts, swallowing every key until `?`/Esc/q.
        if (!sessionStartedRef.current) return;
        setMode("browse");
        setKeymapOpen(true);
      } else if (payload.kind === "hint_collect") {
        // A HINT started elsewhere in the window must not label rows hidden under this overlay
        //. It also frees the keys `hint_collect`'s own reply is about to swallow --
        // this and HINT never actually contend for them, but closing here keeps that true by
        // construction rather than by the two features happening not to overlap in practice.
        setKeymapOpen(false);
        setDetail(null);
        setChooser(null);
        // Close the search prompt so HINT labels do not cover it or compete with `/` for keys.
        setSearch(null);
        // ...and the prefix the strip may still be waiting on, for the reason `pane_focus` does it.
        pendingRef.current = null;
        hideGPrefix();
        const root = containerRef.current ?? startScreenRef.current;
        frozenRef.current = root === null ? [] : hintTargets(root);
        hintSessionRef.current = payload.sessionId;
        // A newer session supersedes whatever an older one still had on screen.
        setHints([]);
        setHintTyped("");
        postToRust({
          type: "hint_targets",
          request_id: nextRequestId(),
          session_id: payload.sessionId,
          count: frozenRef.current.length,
        });
      } else if (payload.kind === "hint_show") {
        if (payload.sessionId !== hintSessionRef.current) return;
        const frozen = frozenRef.current;
        setHints(payload.labels.slice(0, frozen.length).map((label, i) => ({ target: frozen[i], label })));
        setHintTyped("");
      } else if (payload.kind === "hint_prefix") {
        if (payload.sessionId !== hintSessionRef.current) return;
        setHintTyped(payload.typed);
      } else if (payload.kind === "hint_land") {
        if (payload.sessionId !== hintSessionRef.current) return;
        landOnHint(frozenRef.current[payload.index]);
        endHint();
      } else if (payload.kind === "hint_end") {
        if (payload.sessionId !== hintSessionRef.current) return;
        endHint();
      } else if (payload.kind === "hello") {
        setHello(payload);
      } else if (payload.kind === "tabs") {
        // A switch to a DIFFERENT tab clears every piece of per-conversation state the old
        // start-screen resets used to clear, before the new tab's own `snapshot` (if it has one)
        // arrives -- the WebView holds only the active tab's state, so nothing here may
        // carry over from the tab just left. The first `tabs` this window ever sees (mount) also
        // takes this branch, since `activeTabRef.current` starts `null`.
        if (payload.active !== activeTabRef.current) {
          switchedRef.current = true;
          // Flush the draft before the tab becomes inactive rather than waiting for the rest of its 300ms
          // debounce.
          flushDraft();
          const previous = activeTabRef.current;
          // The old tab's view is saved from the live refs -- BEFORE the resets below overwrite
          // the render state they mirror -- and only when there IS an old tab (not on mount).
          if (previous !== null) {
            const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
            saveView(viewStore.current, previous, {
              cursor: cursorRef.current,
              mode: modeRef.current,
              expanded: expandedRef.current,
              scrollTop: list?.scrollTop ?? 0,
              atBottom: list === null || list.scrollTop + list.clientHeight >= list.scrollHeight - 1,
              detailed: detailedRef.current,
            });
          }
          restoreRef.current = takeView(viewStore.current, payload.active) ?? null;
          // The box goes empty on a switch, never the left tab's words: Rust's own `draft` envelope
          // for the new tab follows in the same batch, so this is never what the reader
          // actually sees for longer than one dispatch.
          restoreSeq.current += 1;
          draftRef.current = "";
          setRestoredDraft({ text: "", seq: restoreSeq.current });
          setState(initialState());
          setCursor(0);
          setExpanded({});
          setDetailed(false);
          setMode("browse");
          setKeymapOpen(false);
          setDetail(null);
          setChooser(null);
          // The `/` prompt belongs to the old tab's conversation and must close on a tab switch.
          setSearch(null);
          setTurnClock(null);
          setHandoff(null);
          setFatalError(null);
          setCommandNotice(null);
          setQueue([]);
          setQueueError(null);
          setRuleOffers({});
          setScratchEditing(false);
          setQueueTaken(null);
        }
        activeTabRef.current = payload.active;
        setTabs({ active: payload.active, tabs: payload.tabs });
        forgetClosed(
          viewStore.current,
          payload.tabs.map((t) => t.id),
        );
      } else if (payload.kind === "tab_detail") {
        // The reply to `open_detail` (`StatusRow`, `prefix i`): opens the popover on this tab's
        // rows, cursor at the top. Forces BROWSE and closes the `?` overlay the same way
        // `open_keymap` does -- the two overlays are mutually exclusive over the conversation area.
        setDetail(payload.rows);
        setDetailCursor(0);
        setMode("browse");
        setKeymapOpen(false);
      } else if (payload.kind === "snapshot") {
        setState((s) => applySnapshot(s, payload.state, payload.throughRevision));
        // The view a switch saved for this tab, if it had one -- cursor and
        // mode apply now; the scroll position is left to the layout effect below, since it needs
        // the row this cursor lands on to exist in the DOM first. `landingRef` is set BEFORE the
        // restored `setCursor` so the `[cursor]` reveal effect does not fight this restore with its
        // own `"nearest"` scroll for a plain +1/-1 move.
        if (restoreRef.current !== null) {
          const view = restoreRef.current;
          // Set `"keep"` only when the cursor moves. An unchanged `setCursor` fires no `[cursor]`
          // effect to consume it, so it would suppress the next move's reveal, including a landing
          // on a card below the restored view.
          if (view.cursor !== cursorRef.current) landingRef.current = "keep";
          setCursor(view.cursor);
          setMode(view.mode);
          setExpanded(view.expanded);
          setDetailed(view.detailed);
          // Tells the scroll-restoring layout effect a real restore landed in THIS render -- see
          // its own doc comment for why it cannot simply key on `state`.
          setRestoreTick((n) => n + 1);
        }
        // When switching to a tab with a pending card while the panel has the keys, land on the
        // card as `Ctrl+l` does within one tab. Read `payload.state` because `timeline` still
        // belongs to the old tab until `setState` renders. The `permissionRequest` effect finds the
        // exact row after that render.
        if (switchedRef.current) {
          switchedRef.current = false;
          if (paneFocusedRef.current && payload.state.pendingPermissions.length > 0) setPermissionRequest((n) => n + 1);
        }
        // A snapshot means a session is genuinely RUNNING, which is also when Rust clears its own
        // copy of the command. Clearing it when a start was merely requested would throw it away on
        // a start that then failed -- and on the legacy backend that is the last reference to a
        // conversation nothing else remembers.
        setHandoff(null);
        setCommandNotice(null);
        // A turn first seen in a snapshot gets `exact: false`, since the panel cannot know how
        // long it was already running. Preserve the clock if the snapshot names the tracked
        // turn so resync does not restart it. Clear the clock if there is no active turn.
        setTurnClock((current) => {
          const activeTurnId = payload.state.activeTurnId;
          if (activeTurnId === null) return null;
          if (current !== null && current.turnId === activeTurnId) return current;
          return { turnId: activeTurnId, since: Date.now(), exact: false };
        });
      } else if (payload.kind === "events") {
        // A new turn resets the render trace: each turn reports its own first text, once.
        if (payload.events.some((e) => e.type === "turn_started")) {
          firstTextReceivedAt.current = null;
          renderReportSent.current = false;
        }
        // A sent prompt takes the cursor once its row exists. The `[timeline]` effect reads and
        // clears this flag when the new row appears.
        if (payload.events.some((e) => e.type === "user_prompt_submitted")) landOnPromptRef.current = true;
        // The clock's only EXACT provenance: a real `turn_started` inside this batch. Everything
        // that ends a turn clears it, mirroring exactly what already clears `state.activeTurnId` in
        // the reducer (`turn_completed`, `session_unavailable`, `session_closed`, a non-attaching
        // `resume_outcome`) -- read here off the raw events rather than off `state` afterwards,
        // because `state` does not carry which envelope a turn id arrived in.
        for (const event of payload.events) {
          if (event.type === "turn_started") {
            const turnId = event.turn_id;
            setTurnClock((current) => (current?.turnId === turnId ? current : { turnId, since: Date.now(), exact: true }));
          } else if (event.type === "turn_completed" || event.type === "session_unavailable" || event.type === "session_closed") {
            setTurnClock(null);
          } else if (event.type === "resume_outcome" && !resumeAttached(event)) {
            setTurnClock(null);
          }
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
        if (record?.kind === "handoff") {
          // Either the handoff finished (the `handoff` envelope arrived first and already reset
          // everything) or it was refused and the session is untouched. Both end the closing state
          // -- of the tab it was sent from, which need not be the one on screen now.
          setHandoffRequests((current) => withoutHandoff(current, record.tab, payload.requestId));
        }
        if (!payload.ok) {
          console.warn("agent-ui: command failed", payload.requestId, payload.error);
          if (record?.kind === "send") {
            if (record.tab === activeTabRef.current) {
              // The composer cleared this optimistically. Rust refused it, so it goes back — a
              // message that vanishes with no trace is the outcome this exists to prevent.
              restoreSeq.current += 1;
              setRestoredDraft({ text: record.text ?? "", seq: restoreSeq.current });
              setCommandNotice(`That message was not sent (${payload.error}). It is back in the box.`);
            } else {
              // The tab this was sent from is no longer the active one (the user switched away
              // before Rust answered) -- restoring it into a DIFFERENT tab's composer would be
              // exactly the tab-scoping bug this feature exists to rule out, so the
              // refusal is reported by name instead, with the full text so it is not lost.
              setCommandNotice(`A message to tab ${record.tab} was not sent (${payload.error}): ${record.text}`);
            }
          } else if (record?.kind === "editor") {
            // A scratch-editor round trip's own refusal is a footer nicety, not
            // something that should fill the banner reserved for conversation-breaking errors.
            showFlash(payload.error);
          } else {
            setCommandNotice(payload.error);
          }
        }
      } else if (payload.kind === "handoff") {
        // The session is genuinely closed by the time this arrives (Rust dispatches it only after
        // its own `shutdown()` returned), so the conversation goes with it rather than being left
        // on screen looking live -- the same treatment a fatal error gets, for the same reason.
        // `sessionStarted` follows from `tabs` alone; the `tabs` envelope naming this tab
        // `not_started` again is what actually drops this render back to the empty tab.
        setHandoffRequests((current) => withoutHandoff(current, payload.tab));
        setCommandNotice(null);
        setState(initialState());
        setKeymapOpen(false);
        // Clear the turn clock on a whole-state reset, as `returnToStartScreen` does. A later
        // snapshot with the same `activeTurnId` would otherwise retain the old `since` and `exact: true`, showing an inflated elapsed time without `+`. Resetting avoids relying on turn ids
        // being unique across sessions.
        setTurnClock(null);
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
        // Tab-scoped now: Rust sends this only when the failing tab is the active one,
        // and this tab's own `tabs.failure` carries the reason for when it is shown again. This
        // window's other tabs are untouched.
        setHandoffRequests((current) => withoutHandoff(current, payload.tab));
        setState(initialState());
        setKeymapOpen(false);
        // The third of the three whole-state resets, now saying the same thing as the other two.
        setTurnClock(null);
        setFatalError(payload.message);
        // No `requestHello()` here: Rust re-sends `hello`, recomputed, whenever the set
        // of open provider sessions changes -- this tab failing (or ending, or resetting) is exactly
        // such a change, so the picker's list is already on its way rather than something this side
        // has to go ask for.
      } else if (payload.kind === "begin_rename") {
        // `prefix ,`: the tab bar stays on screen with an inline field open
        // over this tab, prefilled and selected. The two other overlays over the conversation area
        // must not fight it for the keys.
        setDetail(null);
        setKeymapOpen(false);
        setRenaming({ tab: payload.tab, initial: payload.current ?? "" });
      } else if (payload.kind === "confirm_close") {
        // `prefix &`: drawn in the footer in place of the which-key strip,
        // and takes every key -- see `onKeyDown`'s dedicated branch, checked before every other
        // overlay. The other two overlays are closed for the same reason `begin_rename` closes them.
        setDetail(null);
        setKeymapOpen(false);
        setConfirm({ tab: payload.tab, lines: payload.lines });
      } else if (payload.kind === "chooser") {
        // The chooser opened by `prefix w` or at launch takes the keys. Close the other
        // conversation overlays so they cannot compete with it, as `begin_rename` and
        // `confirm_close` do.
        setDetail(null);
        setKeymapOpen(false);
        setRenaming(null);
        setChooser({ launch: payload.launch, open: payload.open, records: payload.records });
      } else if (payload.kind === "queue") {
        setQueue(payload.items);
        setQueueError(payload.error);
      } else if (payload.kind === "draft") {
        // Rust's copy wins only when it is sent: a switch, `ready`, a reset, a scratch return.
        restoreSeq.current += 1;
        draftRef.current = payload.text;
        pendingDraftRef.current = null;
        setRestoredDraft({ text: payload.text, seq: restoreSeq.current });
      } else if (payload.kind === "queue_taken") {
        takenSeq.current += 1;
        setQueueTaken({ texts: payload.texts, seq: takenSeq.current });
      } else if (payload.kind === "history") {
        setHistory(payload.entries);
      } else if (payload.kind === "rule_offers") {
        setRuleOffers(payload.offers);
      } else if (payload.kind === "editor_context") {
        setEditorContext(payload.file === null ? null : { file: payload.file, lines: payload.lines });
      } else if (payload.kind === "scratch") {
        setScratchEditing(payload.editing);
      } else if (payload.kind === "notice") {
        showFlash(payload.text);
      }
    });
    requestHello();
  }, []);

  /** Clears every trace of a HINT session: labels, prefix, the frozen list and the session itself,
   *  so no later envelope for it does anything. */
  function endHint() {
    clearHintPending();
    frozenRef.current = [];
    hintSessionRef.current = null;
    setHints([]);
    setHintTyped("");
  }

  /** Moves the keys to `target` and does nothing else -- HINT never acts:
   *  a button is focused, never clicked, and `Enter` is what presses it afterwards. A target whose
   *  element has left the DOM since it was frozen lands nowhere rather than somewhere else. */
  function landOnHint(target: HintTarget | undefined) {
    copyCodeRef.current = null;
    if (target === undefined || !target.el.isConnected) return;
    const root = containerRef.current;
    // Landing can scroll the conversation (a focused control, the cursor's row revealed), and that
    // scroll is the user's -- see the note on `noteUserScroll` in `onKeyDown`.
    noteUserScroll(root?.querySelector(".message-list"), "unknown");
    if (target.kind === "composer") {
      // The same route `Ctrl+l` takes (`inputRequest`): INPUT, with the caret in the box. Entering a
      // text box is what landing on one means; nothing is typed or sent.
      setInputRequest((n) => n + 1);
      return;
    }
    if (target.kind === "control") {
      // An input (a permission card's reason box) takes the keys by being focused; a button is
      // selected by being focused, drawn as the solid cursor block.
      // A control inside a conversation row (a card's Approve) also brings the row cursor to that
      // row, the same place `l` would have reached it from: otherwise `h` from it would hand the
      // keys back to some other row, and `a`/`d` would answer a different card.
      const row = target.el.closest<HTMLElement>('[data-nav-stop="row"]');
      const rowIndex = root === null || row === null ? null : rowIndexOf(root, row);
      if (rowIndex !== null) setCursor(rowIndex);
      landedControlRef.current = target.el;
      setMode("browse");
      target.el.focus();
      return;
    }
    // A row or a code block: the row cursor goes there and the panel is back in BROWSE. The row's
    // index is read now, from the element, never taken from `target.rowIndex`: rows can be added
    // above it while the labels are up (a permission card is anchored right after its tool call),
    // and the index frozen at `hint_collect` would then name a different row.
    const row = target.kind === "row" ? target.el : target.el.closest<HTMLElement>('[data-nav-stop="row"]');
    const rowIndex = root === null || row === null ? null : rowIndexOf(root, row);
    if (rowIndex === null) return;
    if (target.kind === "code") copyCodeRef.current = target.el;
    setMode("browse");
    setCursor(rowIndex);
    root?.focus({ preventScroll: true });
  }

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
      post({ type: "turn_rendered", receive_to_frame_ms: performance.now() - receivedAt });
    });
  }, [state.transcript]);

  /** Adds `request_id` and the active tab to a tab command. Posts nothing without an active tab,
   *  since a tab command without a tab is a protocol error.
   *  Tracks `edit_draft` as an `"editor"` request so refusals flash in the footer instead of
   *  filling the `commandNotice` banner. */
  function post<M extends { type: string }>(message: M) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    if (message.type === "edit_draft") inFlight.current.set(requestId, { kind: "editor", tab });
    postToRust({ ...message, request_id: requestId, tab } as unknown as OutboundMessage);
  }

  function handoffToTerminal() {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    inFlight.current.set(requestId, { kind: "handoff", tab });
    // Set BEFORE the post, so the composer is disabled from this moment rather than from whenever a
    // reply comes back. Rust takes the session out of its own state inside the handler this message
    // reaches, and every send after that point would be refused.
    setHandoffRequests((current) => new Map(current).set(tab, requestId));
    setCommandNotice(null);
    postToRust({ type: "handoff_to_terminal", request_id: requestId, tab });
  }

  /** Sends what is typed. On a `NotStarted` tab this is what starts the backend, lazily, with that
   *  tab's remembered mode -- there is no separate `start_session` message any more. */
  function sendMessage(text: string) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    // The text is kept so a refusal can put it back. Dropped again as soon as the reply arrives.
    inFlight.current.set(requestId, { kind: "send", tab, text });
    setCommandNotice(null);
    postToRust({ type: "send_message", request_id: requestId, tab, text });
  }

  /** Queues what is typed behind the running turn. Recorded as a `"send"` for the same reason
   *  `sendMessage` is: the composer cleared the box on Enter, and a refusal here (the session ended,
   *  or a handoff is pending, while this side still showed a turn running) means nothing was queued
   *  and nothing reached history -- Rust remembers a queued text only once it is queued -- so the
   *  box is the only place left to put it.
   *
   *  `send_now` is deliberately NOT recorded this way: its refusal can come after its text was
   *  already queued, where putting it back would show it
   *  twice. Rust saves a `send_now`'s text to history before either outcome, so `↑` recovers it. */
  function queueMessage(text: string) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    inFlight.current.set(requestId, { kind: "send", tab, text });
    setCommandNotice(null);
    postToRust({ type: "queue_message", request_id: requestId, tab, text });
  }

  function interrupt() {
    post({ type: "interrupt" });
  }

  function answerPermission(permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) {
    post({
      type: "permission_response",
      permission_id: permissionId,
      decision,
      reason,
      ...(remember ? { remember: true } : {}),
    });
  }

  /** Commit or cancel an inline rename and return the keys to the root. Send `renaming.tab`
   *  because the field can stay open across a focus-free switch with `n`/`p`/digits/`l`. Using
   *  `post` would read the new active tab instead. `answerConfirm` likewise sends `confirm.tab`
   *  directly. */
  function commitRename(name: string) {
    if (renaming !== null) {
      postToRust({ type: "rename_tab", request_id: nextRequestId(), tab: renaming.tab, name });
    }
    setRenaming(null);
    containerRef.current?.focus();
  }
  function cancelRename() {
    setRenaming(null);
    containerRef.current?.focus();
  }

  /** Hands the keys back to whichever layout is on screen -- the conversation's own root, or the
   *  start screen's while no session has started yet. The same fallback `hint_collect` and the
   *  document-`<body>` replay above already use. */
  function returnKeysToRoot() {
    if (containerRef.current !== null) {
      containerRef.current.focus({ preventScroll: true });
      return;
    }
    // The empty layout: `.agent-ui-root` handles no key, so focusing it strands the keys (GUI pass,
    // 2026-09-25). The empty tab's live control is its composer, in INPUT -- the same request
    // `enter_input` makes, which `EmptyTab`'s `Composer` answers by focusing its textarea.
    startScreenRef.current?.focus({ preventScroll: true });
    setInputRequest((n) => n + 1);
  }

  /** The `prefix w` and launch chooser list open tabs before saved sessions. Callbacks close the
   *  overlay and return keys to the panel root, except `onLeave(true)` for Esc/q at launch.
   *  There `shell` hands keys to the editor, so `returnKeysToRoot` would fight the handoff. */
  function onChooserSwitch(tab: TabId) {
    postToRust({ type: "select_tab", request_id: nextRequestId(), tab });
    setChooser(null);
    returnKeysToRoot();
  }
  function onChooserResume(providerSessionId: string) {
    post({ type: "resume", provider_session_id: providerSessionId });
    setChooser(null);
    returnKeysToRoot();
  }
  function onChooserCloseTab(tab: TabId) {
    // Rust's own close prompt is `prefix &`; this asks the same question tmux's `choose-tree` `x`
    // does, from the row's own label -- there is no round trip to Rust for this text.
    const label = chooser?.open.find((t) => t.tab === tab)?.label ?? `tab ${tab}`;
    setChooser(null);
    setConfirm({ tab, lines: [`close ${label}? (y/n)`] });
    returnKeysToRoot();
  }
  function onChooserLeave(launch: boolean) {
    postToRust({ type: "chooser_closed", request_id: nextRequestId(), launch });
    setChooser(null);
    if (!launch) returnKeysToRoot();
  }

  /** The window-close prompt owns every key ahead of everything else, in BOTH layouts:
   *  the conversation's own `onKeyDown` below, and `EmptyTab`'s root while the chat is on screen
   *  with no session yet. Lifted out of either
   *  handler so both call it first and agree. Returns whether it claimed the key -- only
   *  `confirm === null` does not. */
  function answerConfirm(event: KeyboardEvent<HTMLDivElement>): boolean {
    if (confirm === null) return false;
    if (!["Shift", "Control", "Alt", "Meta"].includes(event.key)) {
      event.preventDefault();
      if (event.key === "y") {
        postToRust({ type: "close_tab", request_id: nextRequestId(), tab: confirm.tab });
      }
      setConfirm(null); // tmux confirm-before: any other key cancels
    }
    return true;
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
    // `errorBanner` deliberately stays out of this branch: a failed tab shows its own reason inside
    // `EmptyTab` (`activeTab.failure`), and a fatal error that arrived before any `tabs` envelope
    // (nothing to blame it on yet) still reaches the reader through `failure` below, as a fallback.
    return (
      <div
        className="agent-ui-root"
        ref={startScreenRef}
        tabIndex={-1}
        // `.empty-tab` is centred, so a click above or below it lands here; taking focus would put the
        // keys on an element that handles none (the phase-3 GUI pass, 2026-09-25). Its own background
        // only -- a press on anything inside is the browser's.
        onMouseDown={(event) => {
          if (event.target === event.currentTarget) event.preventDefault();
        }}
      >
        {tabs !== null && showTabBar(tabs.tabs.length, renaming !== null) && (
          <TabBar
            tabs={tabs.tabs}
            active={tabs.active}
            renaming={renaming}
            onSelect={(tab) => postToRust({ type: "select_tab", request_id: nextRequestId(), tab })}
            onRenameCommit={commitRename}
            onRenameCancel={cancelRename}
          />
        )}
        {commandNoticeBanner}
        {activeTab === null ? (
          <div className="empty-tab">
            <p className="connecting">Connecting to the shell…</p>
          </div>
        ) : (
          <EmptyTab
            hello={hello}
            tab={activeTab}
            handoff={handoff}
            failure={activeTab.failure ?? fatalError}
            paneFocused={paneFocused}
            focusRequest={inputRequest}
            restoredDraft={restoredDraft}
            // Whoever sends from here is typing, and the conversation this starts must open where
            // they are: in INPUT. It mounted in BROWSE, so a second message typed straight away ran
            // as BROWSE keys (the phase-3 GUI pass, 2026-09-25).
            onSend={(text) => {
              setMode("input");
              sendMessage(text);
            }}
            onResume={(id) => post({ type: "resume", provider_session_id: id })}
            onCycleMode={() => post({ type: "cycle_mode" })}
            onReset={() => post({ type: "reset_tab" })}
            onHint={requestHint}
            onDraftChange={mirrorDraft}
            answerConfirm={answerConfirm}
            onQueue={(text) => {
              setMode("input");
              queueMessage(text);
            }}
            history={history}
            queueCount={queue.length}
            queue={queue}
            queueError={queueError}
            editorContext={editorContext}
            onTakeBackQueue={() => post({ type: "take_back_queue" })}
            queueTaken={queueTaken}
            onHistoryPush={(t) => postToRust({ type: "history_push", request_id: nextRequestId(), text: t })}
            onEditInNvim={(t) => post({ type: "edit_draft", text: t })}
            editingInNvim={scratchEditing}
            onOpenKeymap={() => {
              setMode("browse");
              setKeymapOpen(true);
            }}
          />
        )}
        {/* No `.agent-ui-scroller` on this screen: the footer, and the window-close
            prompt it draws while `prefix &` is open, sit directly under the empty tab instead of
            below a composer that lives inside `EmptyTab` itself. */}
        <Footer
          mode="input"
          paneFocused={paneFocused}
          pill={modePill(activeTab?.mode ?? "auto", true)}
          flash={flash?.text ?? null}
          hint={INPUT_IDLE_HINT}
        >
          {confirm !== null ? (
            <span className="confirm-close" role="alertdialog">
              {confirm.lines.join(" · ")}
            </span>
          ) : undefined}
        </Footer>
        {/* The launch chooser opens over empty tab 1 before the chat receives keys. This layout
            has no `.agent-ui-scroller`, so `.agent-ui-root` is its positioned ancestor. */}
        {chooser !== null && (
          <Chooser envelope={chooser} active={tabs?.active ?? null} onSwitch={onChooserSwitch} onResume={onChooserResume} onCloseTab={onChooserCloseTab} onLeave={onChooserLeave} />
        )}
        <HintLayer root={startScreenRef.current} hints={hints} typed={hintTyped} />
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
  const handingOff = tabs !== null && handoffRequests.has(tabs.active);

  /** The key table's home: mode + key + context in, an action out, applied here. Only claims what
   *  `resolveKey` claims -- an unrecognised key, or one INPUT leaves to the input method (a
   *  composing Escape), falls straight through with no `preventDefault`. That is what keeps
   *  Ctrl+h/l (pane switch), prefix r (panel reload) and Ctrl+Shift+O working: GTK takes those
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
    // The window-close confirmation prompt owns every key, first of all, ahead of the `?` keymap
    // overlay and everything else below: tmux's own `confirm-before` prompt takes the
    // keys the same way.
    if (answerConfirm(event)) return;
    // A `gf` picker with several paths takes the next key before other overlays. A valid letter
    // opens its path; Esc or any other key cancels without falling through, so the key cannot also
    // move the cursor.
    if (pathPick !== null) {
      event.preventDefault();
      const index = HINT_ALPHABET.indexOf(event.key);
      if (index !== -1 && index < pathPick.length) openPath(pathPick[index]);
      setPathPick(null);
      return;
    }
    const root = containerRef.current;
    const pending = pendingRef.current;
    pendingRef.current = null;
    const count = countRef.current;
    countRef.current = null;
    // Every key seen here cancels whatever the PREVIOUS key armed, the which-key strip's `g`
    // continuation included -- `pending` above already captured whether THIS key still gets to
    // read it. Unconditional, ahead of every early return below, so a key that turns out to be
    // claimed by a text box or a focused button still cancels a stale prefix line.
    hideGPrefix();
    // The `?` keymap overlay owns every key while it is open, ahead of the editable-
    // element and activatable-control checks below and ahead of `resolveKey` entirely -- not
    // routed through the key table at all, because closing is not one of `resolveKey`'s actions
    // (only OPENING is: its `{kind:"keymap"}` is reached below, and only ever with the overlay
    // already closed, since this branch returns first whenever it is open). `preventDefault`
    // unconditionally: `a`/`d` must not reach a permission card hidden underneath, and neither
    // must anything else this table would otherwise claim.
    if (keymapOpen) {
      event.preventDefault();
      if (event.key === "?" || event.key === "Escape" || event.key === "q") {
        setKeymapOpen(false);
      } else if (event.key === "j" || event.key === "k") {
        scrollKeymapOverlay(keymapOverlayRef.current, event.key === "j" ? 1 : -1);
      }
      return;
    }
    // The detail popover, the same way: it owns every key while it is open, ahead of everything
    // below.
    if (detail !== null) {
      event.preventDefault();
      if (event.key === "Escape" || event.key === "q") setDetail(null);
      else if (event.key === "j") setDetailCursor((c) => Math.min(c + 1, detail.length - 1));
      else if (event.key === "k") setDetailCursor((c) => Math.max(c - 1, 0));
      else if (event.key === "y") void navigator.clipboard?.writeText(detail[detailCursor]?.value ?? "");
      return;
    }
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
    // keyboard route to Approve. Every OTHER key still
    // reaches the table, which is what lets `h`/`j`/`k`/`l` carry on from a focused button.
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ")) return;
    const action = resolveKey(mode, event.nativeEvent as unknown as KeyLike, { sessionEnded, pending, count, turnRunning: turnInProgress });
    if (action === null) return;
    // Accumulate digits into the count for the next `j`/`k`/`[[`/`]]`. That key reads it from
    // `countRef` as `count`. Return immediately so a bare digit cannot reach the scroll or
    // row-motion code.
    if (action.kind === "count") {
      event.preventDefault();
      countRef.current = (count ?? 0) * 10 + action.digit;
      return;
    }
    // How many times THIS key repeats its move: the count just accumulated (cleared above, so this
    // is the last one) if any, else once, same as vim's own default count of 1.
    const times = Math.max(1, count ?? 1);
    // Every key below that can move the conversation says so first, on the list itself, so
    // `MessageList` knows the scroll that follows is the user's (`./follow.ts`, 2026-09-24): it no
    // longer takes an unexplained drop in `scrollTop` for the reader leaving the bottom, because
    // WebKitGTK once produced such drops by itself mid-reply. `k`, `Ctrl+u` and `gg` end following at
    // once, as they always did; `j`, `Ctrl+d` and `G` let the scroll decide (reaching the bottom
    // re-arms it); `h`/`l` can focus a control that scrolls itself into view, either way.
    const messageList = root?.querySelector<HTMLElement>(".message-list") ?? null;
    if (action.kind === "move" || action.kind === "half-page") {
      noteUserScroll(messageList, action.delta > 0 ? "down" : "up");
    } else if (action.kind === "jump") {
      noteUserScroll(messageList, action.to === "first" ? "up" : "down");
    } else if (action.kind === "control") {
      noteUserScroll(messageList, "unknown");
    }
    // A code block HINT landed on is "the item" for exactly the next `y`; any other key moves on.
    const landedCode = copyCodeRef.current;
    copyCodeRef.current = null;
    // `j`/`k` scroll the cursor row's own overflow box (a long tool result) before they move off it
    // -- the vim behaviour; `scrollCursorRowBox`'s own doc says why this cannot
    // live in `resolveKey`. Failing that, they scroll the conversation through a current row that
    // is taller than what is on screen (`scrollCursorRow`). Only while the row cursor has the keys:
    // from a banner's button, `j` is a move, not a scroll of a row that is not current.
    if (
      action.kind === "move" &&
      !edgeFocused &&
      (scrollCursorRowBox(root, action.delta) || scrollCursorRow(root, action.delta))
    ) {
      event.preventDefault();
      // A scroll step returns DOM focus to the conversation root, as a row move does. Otherwise a card
      // control could scroll out of view while still responding to Enter.
      if (root !== null && document.activeElement !== root && root.contains(document.activeElement)) {
        root.focus({ preventScroll: true });
      }
      return;
    }
    if (action.kind === "move" || action.kind === "control" || action.kind === "answer") {
      event.preventDefault();
      if (root === null) return;
      if (action.kind === "move") {
        // Repeat the step `times` times, starting each step from the previous landing. Do not leap
        // by `times` rows: a banner or status-line stop has no row and must end the walk where a
        // single `j`/`k` would.
        let target: HTMLElement | null = null;
        let row = cursor;
        for (let n = 0; n < times; n++) {
          const next = nextStop(root, row, action.delta);
          if (next === null) break;
          target = next;
          const nextRow = rowIndexOf(root, next);
          if (nextRow === null) break;
          row = nextRow;
        }
        if (target === null) return;
        const finalRow = rowIndexOf(root, target);
        if (finalRow !== null) {
          if (finalRow !== cursor) landingRef.current = action.delta;
          setCursor(finalRow);
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
        // `answerTarget`, not `permissionTarget`: with exactly one card anywhere in
        // the conversation, it answers from any row, not only the cursor's own.
        const target = answerTarget(answerableItems, cursor);
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
        // above for why an index would silently drift onto the wrong row. For a "run" key this
        // un-collapses it right back into its calls (`display.ts`'s `buildDisplay`).
        const key = timeline[cursor]?.key;
        if (key !== undefined) setExpanded((prev) => ({ ...prev, [key]: !prev[key] }));
        break;
      }
      case "detailed":
        // `Ctrl+o` toggles per-tab detail, mirrored to Rust by `saveView` and restored on a tab
        // switch.
        setDetailed((d) => !d);
        break;
      case "table-scroll": {
        // T1: `zh`/`zl` scroll the CURRENT row's own table, never the conversation.
        const table = root?.querySelector<HTMLElement>(".row-current .table-scroll") ?? null;
        if (table !== null) table.scrollLeft += action.delta * TOOL_RESULT_SCROLL_STEP_PX;
        break;
      }
      case "copy": {
        // After HINT landed on a code block: that block's code, not the whole message. Only while it
        // is still in the DOM and still inside the row under the cursor -- anything else and the
        // user is looking at something else now.
        const cursorRow = root?.querySelectorAll<HTMLElement>('[data-nav-stop="row"]')[cursor];
        if (landedCode !== null && landedCode.isConnected && cursorRow?.contains(landedCode)) {
          copied(landedCode.querySelector("code")?.textContent ?? landedCode.textContent ?? "", timeline[cursor]?.key);
          break;
        }
        // No item at the cursor (an empty timeline) writes NOTHING, rather than clobbering
        // whatever the user already had on the clipboard with "" -- found in review.
        const item = timeline[cursor];
        if (item !== undefined) copied(primaryText(item), item.key);
        break;
      }
      case "copy-output": {
        const item = timeline[cursor];
        const text = item === undefined ? null : outputText(item);
        if (text === null) showFlash("this row has no output");
        else copied(text, item.key);
        break;
      }
      case "deny-reason": {
        if (root === null || edgeFocused) break;
        const target = answerTarget(answerableItems, cursor);
        const rows = root.querySelectorAll<HTMLElement>('[data-nav-stop="row"]');
        const box = target === null ? null : rows[target]?.querySelector<HTMLInputElement>(".permission-card input");
        if (box) {
          if (target !== cursor) setCursor(target!);
          landedControlRef.current = box;
          box.focus();
        }
        break;
      }
      case "restart":
        // `r` on an ended tab resets it to `NotStarted` IN PLACE: the tab keeps its
        // number and its rename, and the `tabs` envelope that follows is what actually drops this
        // render back to the empty tab -- there is no local start-screen reset any more.
        post({ type: "reset_tab" });
        break;
      case "keymap":
        // Only ever reached with the overlay closed -- the `keymapOpen` branch above returns
        // before `resolveKey` runs at all once it is open, `?`/`Escape`/`q` included, so this is
        // opening only, never a toggle-closed here.
        setKeymapOpen(true);
        break;
      case "hint":
        // `shell` owns the HINT session: it collects targets across the whole window and asks this
        // panel for its own with `hint_collect`. Until the HINT ends, this panel's keys are swallowed
        // (`hintPendingRef`); nothing else changes here.
        requestHint(event.repeat);
        break;
      case "pending":
        // Claimed (so the key does nothing else) and remembered for exactly one more key.
        pendingRef.current = action.prefix;
        // The strip shows what `g` can start once it has waited long enough that this is not just
        // the first half of `gg` -- `[[`/`]]` have no strip continuation of their own,
        // so only `g` arms this timer. The timer is the only thing that ever sets `gShown` true, and
        // `hideGPrefix` at the top of this function on every later key -- the second `g` of `gg`
        // included -- is the only thing that cancels it before it fires.
        if (action.prefix === "g") {
          gShownTimerRef.current = setTimeout(() => {
            gShownTimerRef.current = null;
            setGShown(true);
          }, WHICH_KEY_G_PREFIX_DELAY_MS);
        }
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
      case "prompt-jump": {
        // Repeat `[[`/`]]` `times` times from each previous prompt landing. Stop at the first
        // missing earlier or later prompt instead of wrapping.
        let target: number | null = cursor;
        for (let n = 0; n < times && target !== null; n++) target = promptIndex(timeline, target, action.delta) ?? null;
        if (target !== null && target !== cursor) {
          noteUserScroll(messageList, action.delta > 0 ? "down" : "up");
          landingRef.current = action.delta;
          setCursor(target);
        }
        root?.focus({ preventScroll: true });
        break;
      }
      case "interrupt":
        // `Ctrl+c` interrupts a running turn from anywhere in BROWSE, using the same `interrupt()`
        // as the activity line's Stop button.
        interrupt();
        break;
      case "search":
        setSearch({ query: "", origin: cursor });
        break;
      case "search-next": {
        const found = findMatch(timeline, lastSearchRef.current, cursor, action.delta, false);
        if (found === null) {
          if (lastSearchRef.current !== "") showFlash(`pattern not found: ${lastSearchRef.current}`);
        } else moveCursorTo(found);
        // Same reason `move`/`jump`/`prompt-jump` and the Ctrl+d/Ctrl+u re-home all do this: a
        // cursor move must take the keys back from a focused control (e.g. a permission card's
        // Approve after `l`), or a stale `document.activeElement` keeps answering Enter/Space for a
        // row the cursor no longer shows (found in review).
        root?.focus({ preventScroll: true });
        break;
      }
      case "open-path": {
        // Use a tool's path field or a path found in prose, limited to what `pathsIn` in
        // `../paths.ts` can read from the row. Do not infer what the model meant.
        const item = timeline[cursor];
        const paths = item === undefined ? [] : pathsIn(item).slice(0, HINT_ALPHABET.length);
        if (paths.length === 0) showFlash("no path on this row");
        else if (paths.length === 1) openPath(paths[0]);
        else setPathPick(paths);
        break;
      }
      case "view-in-editor": {
        // Open the row's full text in a read-only nvim scratch buffer.
        const item = timeline[cursor];
        if (item === undefined) break;
        const { title, text } = viewText(item);
        const requestId = nextRequestId();
        inFlight.current.set(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
        postToRust({ type: "view_in_editor", request_id: requestId, title, text });
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
        {/* `r` is offered only on this banner and the one below, for resetting this tab -- see
            `onKeyDown`'s "restart" arm above and `resolveKey`'s `case "r"` in `./keymap`, which is
            what actually enforces "only where the spec offers it". The mode this hint is read in is
            always BROWSE: `resolveKey` refuses `i` once the session has ended and the effect near
            the top of this component forces BROWSE if it died while INPUT was active, so this
            sentence cannot be on screen in a mode that drops `r`. */}
        <div className="row-hint">Press r to start a new session here.</div>
      </Row>
    ) : state.status.kind === "closed" ? (
      // Deliberately NOT `row-error`/`✗`: an ordinary close (the host closed it, the provider
      // exited cleanly) is not an error, and styling it like the lost-session row above would
      // train the eye to ignore the one that matters. See `.row-ended` in index.css for the
      // fuller record of why these two were briefly unified and then split back apart.
      <Row kind="ended" sign="·">
        This session has ended ({state.status.reason}).
        <div className="row-hint">Press r to start a new session here.</div>
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
      {tabs !== null && showTabBar(tabs.tabs.length, renaming !== null) && (
        <TabBar
          tabs={tabs.tabs}
          active={tabs.active}
          renaming={renaming}
          onSelect={(tab) => postToRust({ type: "select_tab", request_id: nextRequestId(), tab })}
          onRenameCommit={commitRename}
          onRenameCancel={cancelRename}
        />
      )}
      {errorBanner}
      {/* `sessionEnded` makes every pending card inert. The cards themselves are NOT removed: a
          permission that was still open when the session died is real history, and deleting it
          would read as a resolution nobody made. */}
      {/* The list, the `?` overlay and the detail popover share one positioned region, so either
          overlay covers the conversation and nothing else -- the activity line, the status row and
          the footer are outside its box rather than lifted back above it (review; see
          `.agent-ui-scroller` in index.css). The tab bar goes above `errorBanner`. */}
      <div className="agent-ui-scroller">
        <MessageList
          state={state}
          sessionEnded={sessionEnded}
          expanded={expanded}
          cursor={cursor}
          detailed={detailed}
          focused={paneFocused && !edgeFocused}
          ruleOffers={ruleOffers}
          yankedKey={yanked?.key ?? null}
          onAnswerPermission={answerPermission}
          onOpenPath={openPath}
        />
        {keymapOpen && (
          <KeymapOverlay
            ref={keymapOverlayRef}
            onClose={() => setKeymapOpen(false)}
            windowKeys={keymapHelp.window}
            prefixKeys={keymapHelp.prefixKeys}
            prefixLabel={keymapHelp.prefix}
          />
        )}
        {detail !== null && (
          <DetailPopover ref={detailRef} rows={detail} current={detailCursor} onClose={() => setDetail(null)} />
        )}
        {chooser !== null && (
          <Chooser envelope={chooser} active={tabs?.active ?? null} onSwitch={onChooserSwitch} onResume={onChooserResume} onCloseTab={onChooserCloseTab} onLeave={onChooserLeave} />
        )}
      </div>
      {sessionEndedBanner}
      {commandNoticeBanner}
      <ActivityLine
        state={state}
        turnClock={turnClock}
        canInterrupt={state.capabilities.interrupt}
        onInterrupt={interrupt}
        queued={queue.length}
        pendingTool={oldestPendingTool(state)}
      />
      <QueueLines items={queue} error={queueError} />
      {!sessionEnded && <ContextLine context={editorContext} />}
      <Composer
        // C1: a running turn no longer disables the composer -- it queues a follow-up instead. Only
        // a dead session, or one already being closed for a terminal handoff, takes no more turns:
        // clearing `activeTurnId` on a lost session must not hand the user an enabled composer
        // pointed at nothing.
        disabled={sessionEnded || handingOff}
        sessionEnded={sessionEnded}
        closing={handingOff}
        restoredDraft={restoredDraft}
        mode={mode}
        focusRequest={inputRequest}
        // Only while the box can take a message: a landing on a disabled textarea could not focus it.
        hintTarget={!(sessionEnded || handingOff)}
        onModeChange={setMode}
        onSend={sendMessage}
        onDraftChange={mirrorDraft}
        running={turnInProgress}
        onQueue={queueMessage}
        onSendNow={(t) => post({ type: "send_now", text: t })}
        history={history}
        queueCount={queue.length}
        onTakeBackQueue={() => post({ type: "take_back_queue" })}
        queueTaken={queueTaken}
        onHistoryPush={(t) => postToRust({ type: "history_push", request_id: nextRequestId(), text: t })}
        onInterrupt={interrupt}
        onEditInNvim={(t) => post({ type: "edit_draft", text: t })}
        editingInNvim={scratchEditing}
        onOpenKeymap={() => {
          setMode("browse");
          setKeymapOpen(true);
        }}
        onShiftTab={() => {
          if (!state.capabilities.modeSwitch) showFlash("mode is fixed for this session");
        }}
      />
      <StatusRow
        text={statusRowText(state, { index: cursor, total: timeline.length })}
        warning={statusWarning(state.provider, activeTab?.failure ?? null)}
        onOpenDetail={() => post({ type: "open_detail" })}
      />
      <Footer
        mode={mode}
        paneFocused={paneFocused}
        pill={modePill(activeTab?.mode ?? "auto", state.capabilities.modeSwitch === true)}
        flash={flash?.text ?? null}
        hint={mode === "input" ? (turnInProgress ? INPUT_RUNNING_HINT : INPUT_IDLE_HINT) : undefined}
      >
        {/* The window-close prompt takes the footer's third slot while it is open, the
            same way tmux draws `confirm-before` in its status line -- in place of everything else,
            flash included. */}
        {confirm !== null ? (
          <span className="confirm-close" role="alertdialog">
            {confirm.lines.join(" · ")}
          </span>
        ) : // `gf` with several paths waiting for its letter, ahead of the flash and the
        // which-key strip for the same reason the close prompt is: a card with pending input owns
        // the space until it is answered.
        pathPick !== null ? (
          <PathPick paths={pathPick} />
        ) : // The open `/` prompt, ahead of the flash and the which-key strip; vim's own
        // command line takes the same spot the status line otherwise uses.
        search !== null ? (
          <SearchBar
            query={search.query}
            onChange={(query) => {
              setSearch({ query, origin: search.origin });
              const found = findMatch(timeline, query, search.origin, 1, true);
              setCursor(found ?? search.origin);
            }}
            onAccept={() => {
              lastSearchRef.current = search.query;
              if (search.query !== "" && findMatch(timeline, search.query, search.origin, 1, true) === null) {
                showFlash(`pattern not found: ${search.query}`);
                setCursor(search.origin);
              }
              setSearch(null);
              containerRef.current?.focus({ preventScroll: true });
            }}
            onCancel={() => {
              setCursor(search.origin);
              setSearch(null);
              containerRef.current?.focus({ preventScroll: true });
            }}
          />
        ) : // Only in BROWSE (INPUT is typing, HINT's labels take over, the start screen
        // has its own text), only once no HINT labels are on screen -- `hints.length === 0` is the
        // same predicate `HintLayer` itself uses to decide whether it is drawing anything at all,
        // so this and the labels can never both claim the same space -- and only while no flash is
        // showing. `undefined`
        // in every other case, deliberately, not `false`: `Footer`'s own fallback tests `children`
        // with `??`, which only falls through on `null`/`undefined`, never on a falsy boolean.
        mode === "browse" && hints.length === 0 && flash === null ? (
          <WhichKey
            // `edgeFocused` too: `onKeyDown`'s answer arm is gated on it, so with the keys on a
            // banner or the status row `a`/`d` resolve to nothing and the strip must not offer
            // them (review). Its own argument, not `sessionEnded`'s: passed as that, a live
            // session's status row offered `r new session` (GUI pass, 2026-09-25).
            entries={stripEntries(timeline, answerableItems, cursor, sessionEnded, edgeFocused)}
            focused={paneFocused}
            prefix={gShown ? "g" : null}
          />
        ) : undefined}
      </Footer>
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
      <HintLayer root={containerRef.current} hints={hints} typed={hintTyped} />
    </div>
  );
}
