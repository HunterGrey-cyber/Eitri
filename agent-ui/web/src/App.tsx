import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { applyEvent, applySnapshot, initialState, resumeAttached } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import type { NavKeyDirection, OutboundMessage, PanelKeysMode, PermissionDecision } from "./bridge";
import { noteUserScroll, resumeFollowing } from "./follow";
import { EMPTY_PANEL_TABLE, resolveKey } from "./keymap";
import type { KeyLike, KeymapHelp, PanelBinding, PanelMode, PendingPrefix } from "./keymap";
import { advanceSequence, boxEntries, FIXED_PENDING_ENTRIES, sequenceTitle, startSequence, WHICH_KEY_DELAY_MS } from "./leader";
import type { BoxEntry, SeqStep } from "./leader";
import { isImeKey } from "./composerKeys";
import { isModeCycleKey, isShiftTab, MODE_STARTING_MESSAGE, modeFixedMessage, modeKeyRoute } from "./modeKey";
import { leaderTypingFlash, TypingGuard } from "./typingGuard";
import { WhichKeyBox } from "./components/WhichKeyBox";
import { buildTimeline, oldestPendingPermission, promptIndex } from "./timeline";
import { buildDisplay, indexOfKey } from "./display";
import { outputText, primaryText } from "./copyText";
import { countCodePoints } from "./toolRegistry";
import { findMatch } from "./search";
import { SearchBar } from "./components/SearchBar";
import {
  controlsOf,
  currentStop,
  hintTargets,
  isActivatableControl,
  nextControl,
  nextStop,
  permissionTarget,
  rowIndexOf,
  HINT_ALPHABET,
} from "./nav";
import type { AnswerableItem, HintTarget } from "./nav";
import type { TimelineItem } from "./timeline";
import { pathsIn, viewText } from "./paths";
import type { PathRef } from "./paths";
import { PathPick } from "./components/PathPick";
import { acceptsEnvelope, activeTabInfo, forgetClosed, saveView, takeView, withoutHandoff } from "./tabs";
import type { TabViewState } from "./tabs";
import { EmptyTab } from "./components/EmptyTab";
import type { NavKeyRequest } from "./components/EmptyTab";
import { Composer } from "./components/Composer";
import type { RestoredDraft } from "./components/Composer";
import { TabBar } from "./components/TabBar";
import { MessageList } from "./components/MessageList";
import { Row } from "./components/Row";
import { classify } from "./problems";
import { ActivityLine } from "./components/ActivityLine";
import { StatusBand } from "./components/StatusBand";
import { QueueLines } from "./components/QueueLines";
import { DetailPopover } from "./components/DetailPopover";
import { ContinueInTerminal } from "./components/TerminalHandoff";
import { HintLayer } from "./components/HintLayer";
import type { ShownHint } from "./components/HintLayer";
import { KeymapOverlay } from "./components/KeymapOverlay";
import { Chooser } from "./components/Chooser";
import { DRAFT_MIRROR_DELAY_MS, modePill, showTabBar } from "./tabs";
import { shortModel } from "./band";
import type { BandFacts } from "./band";
import type { AgentUiState, HandoffCommand, Hello, DetailRow, TabId, TabsEnvelope, TurnClock, ChooserEnvelope, ContextSummary, QueueItem, ProviderInfo } from "./types";
import { applyTheme } from "./theme";

/** The line `agent/src/providers/claude_sidecar/spawn.rs::describe_checkout` writes for version skew.
 *  Moved in from the deleted `statusRow.ts` (panel round 2 plan, Task 10): the band's `warn` fact
 *  is this module's logic now, since `BandFacts` itself stays decoupled from `ProviderInfo`. */
export const SKEW_PREFIX = "Verdandi baseline drift";

/** D12 A: `⚠` only for version skew or a refusal (ruling 9). Returns what the glyph's title says, or
 *  null. Routine diagnostics (the CLI version one, present on every session on the owner's host) go
 *  to the detail popover only. */
export function statusWarning(provider: ProviderInfo | null, failure: string | null): string | null {
  if (failure !== null) return failure;
  return provider?.startupDiagnostics.find((line) => line.startsWith(SKEW_PREFIX)) ?? null;
}

/** `ContextSummary` (the wire shape) to `BandFacts["context"]` (band.ts's own shape, `file`
 *  non-nullable once present): the one place both the empty tab's band and the conversation's own
 *  narrow the "no file" case to `null` rather than each repeating the check. */
function contextFact(context: ContextSummary | null): BandFacts["context"] {
  if (context === null || context.file === null) return null;
  return { file: context.file, lines: context.lines };
}

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

/** One `j`/`k` press's worth of scroll inside a tool result's own overflow box, chosen to read
 *  like the row-to-row step it stands in for rather than a full-page jump. Arbitrary and not
 *  verified on a screen -- see the dated record's entry for this change. */
const TOOL_RESULT_SCROLL_STEP_PX = 40;

/** The longest this panel swallows keys after `f` asked `shell` for a HINT, if that HINT never ends
 *  here (see `hintPendingRef`). Long enough for a GTK main loop busy with a frame and the 33ms pump; short
 *  enough that a `shell` which never answers (the HINT was refused, or this page is not hosted by
 *  `shell` at all) costs a second of dead keys, not a stuck panel -- the same stance as `shell`'s own
 *  300ms wait for this panel's answer (spec §3.3). */
export const HINT_PENDING_TIMEOUT_MS = 1000;

/** v1 S1 (spec §2.1): what the band says when a typed `a`/`d`/`D`, or an Enter on a card button,
 *  did not answer because another key came within `TYPING_GUARD_MS` of it (`./typingGuard`). */
const TYPING_FLASH = "a / d answer a card only on their own — i or Ctrl+j to type";
/** v1 S4/F13 (spec §2.2): what the band says when `a`/`d`/`D` have no card under the cursor, nor a
 *  card gating the tool call under it -- instead of doing nothing silently. */
const NO_CARD_FLASH = "no card here — i, o, A or Ctrl+j to type";
/** The v1-ui GUI pass (2026-09-27): what the band says when Enter or Space on the activity line's
 *  Stop did not interrupt because it came in the middle of typing (`TypingGuard.mayActAfterMotion`)
 *  -- "just do it" typed after an arrival reached Stop with its `j` and interrupted the turn. */
const STOP_TYPING_FLASH = "Stop takes a key only on its own — i or Ctrl+j to type";
/** Envelopes that move the keys, or put something over the conversation, with no keydown this panel
 *  sees: each drops a waiting `a`/`d`/`D` (spec §2.1's cancel list -- `pane_focus`, an arrival, an
 *  overlay). `nav_key` is the v1 plan's `Ctrl+j`/`Ctrl+k` (its "Interfaces" section), which GTK
 *  claims before the page sees a key; a Set of strings, so naming it needs no type from that task. */
const CANCELS_WAITING_ANSWER = new Set(["pane_focus", "arrive", "enter_input", "focus_permission", "hint_collect", "nav_key"]);

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

/** `j`/`k` while the `?` keymap overlay is open (spec §3.1): scrolls the overlay itself, by the
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

/** R1 (vim's scrolloff rule): after a scroll the panel did not make by moving the cursor, a cursor
 *  whose row is entirely off screen goes to the nearest visible row. `null`: leave it. Reads
 *  geometry only; never writes `scrollTop` (review focus 5). */
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

/** P1 (ruling 26): the tool name of the OLDEST pending permission -- the lowest `seq`, the one the
 *  model has waited on longest -- for `ActivityLine`'s "needs approval" slot, or `null` when
 *  nothing waits. */
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
  /** Every tab and which is active, from Rust's `tabs` envelope (session tabs spec §3.1). `null`
   *  until the first one arrives (before that, this window has not been told which tab exists yet). */
  const [tabs, setTabs] = useState<TabsEnvelope | null>(null);
  /** The active tab for the dispatch handler, which is installed once: set synchronously inside the
   *  handler, so the snapshot that follows a `tabs` envelope in the same batch is accepted
   *  (`acceptsEnvelope` reads this, not `tabs` state, which would still hold the PREVIOUS render's
   *  value until React re-renders). */
  const activeTabRef = useRef<TabId | null>(null);
  /** What each tab's reader had on screen (cursor, mode, expansions, scroll, draft), keyed by tab
   *  id and never sent to Rust in phase 2 (ruling 24). Saved just before a switch resets the render
   *  state below, and handed back through `restoreRef` once the new tab's own `snapshot` arrives. */
  const viewStore = useRef(new Map<TabId, TabViewState>());
  /** The view to restore once the switch's `snapshot` (or, for a tab with none, the switch itself)
   *  is applied. Read and cleared by the `snapshot` arm's layout effect below. */
  const restoreRef = useRef<TabViewState | null>(null);
  /** Set by the `tabs` arm whenever `payload.active` names a different tab than before (mount
   *  included, the same condition that arm's own per-tab reset already runs on), and
   *  read-and-cleared by the `snapshot` arm right after (P1, ruling 26): landing on a tab already
   *  holding a card puts the keys on it, the same as `Ctrl+l` does within one tab. */
  const switchedRef = useRef(false);
  const activeTab = activeTabInfo(tabs);
  /** True once the active tab actually holds a session, live or already ended -- a `starting` or
   *  `failed` tab renders the empty tab too (F3, spec §3.6): it is not "started" until Rust has
   *  really opened a session for it. Derived, not state -- Rust's `tabs` envelope is the only source
   *  of a tab's `state`. */
  const sessionStarted = activeTab !== null && (activeTab.state === "live" || activeTab.state === "ended");
  /** `sessionStarted` for the bridge listener, which is registered once and would otherwise read
   *  its first render's value forever. Assigned during render, so it is current by the time any
   *  later envelope arrives. */
  const sessionStartedRef = useRef(false);
  /** Wave 5: whether THIS session's sidecar can switch its live permission mode
   *  (`Capabilities.modeSwitch`, which `agent_bridge.rs` fills from the live handshake's
   *  `set_permission_mode` capability). `=== true` because the
   *  field is optional and absent on legacy/older sidecars, which must read as "cannot", not
   *  "unknown". */
  const modeSwitch = state.capabilities.modeSwitch === true;
  /** Whether `<leader>` `mode.cycle` (and, by the same rule, Shift+Tab) would flash rather than post
   *  -- `modeKeyRoute`'s own routing, so a `starting`/`failed`/`ended` tab still greys the box entry,
   *  but a `live` tab whose sidecar can switch (`modeSwitch`) does not. */
  const modeFixed =
    modeKeyRoute({ confirmOpen: false, chooserOpen: false, tabState: activeTab?.state ?? null, canSwitch: modeSwitch }) !==
    "cycle";
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
   *  **Per tab, not per window** (the session-tabs whole-branch review): one window-wide id locked
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
   *  `"editor"` covers `edit_draft`/`open_path`/`view_in_editor` (Task 8/15): all three reach the
   *  scratch editor in Rust, and a refusal of any of them is a footer flash (`showFlash`), not the
   *  banner -- a scratch-editor round trip that a `gf` or `Ctrl+g` failed to start is a footer
   *  nicety, not something that should fill the space a real conversation error gets. */
  const inFlight = useRef<Map<string, { kind: "send" | "handoff" | "editor"; tab: TabId; text?: string }>>(
    new Map(),
  );
  /** Monotonic, so two refusals of the same text are two distinct restores. */
  const restoreSeq = useRef(0);
  /** Bumped only when the `snapshot` arm actually applies a saved view's cursor/mode/expanded
   *  (session tabs Task 11). The scroll-restoring layout effect below keys on THIS, not on `state`
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
  /** How long the current turn has been running -- design doc §8.4. Real `useState`, not a ref: it
   *  feeds `ActivityLine`'s render (the elapsed clock; V2, session tabs Task 10, formerly
   *  `StatusLine`), unlike the trace-only refs above. Set with
   *  `exact: true` only from a real `turn_started` inside an `events` envelope; a turn id first
   *  observed inside a `snapshot` (a page reload, or a resync mid-turn) gets `exact: false`, because
   *  this panel cannot know how long the turn had already been running before it first saw it.
   *  Cleared whenever a `turn_completed`/`session_unavailable`/`session_closed`/non-attaching
   *  `resume_outcome` arrives -- the same events that clear `state.activeTurnId` in the reducer,
   *  kept in step here rather than re-derived from `state` because only the raw event batch (not
   *  the folded state) carries the provenance (`events` vs `snapshot`) this needs. */
  const [turnClock, setTurnClock] = useState<TurnClock | null>(null);
  /** Which timeline rows show their full tool result rather than the folded placeholder, keyed by
   *  the timeline `key` -- never the cursor index, because a resolved permission removes a card and
   *  shifts every later index, which would silently move an expansion onto a different row.
   *  Toggled by `Enter` on the row under the cursor, in `onKeyDown` below. */
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  /** R3 (`Ctrl+o`): every result shown in full, wider cuts, no collapsed runs. Per tab, like
   *  `expanded` above -- saved and restored across a switch in `TabViewState.detailed`, reset to
   *  `false` for a tab with no saved view (a fresh tab never starts detailed). */
  const [detailed, setDetailed] = useState(false);
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
  /** `paneFocused` for the `snapshot` dispatch arm (P1, ruling 26), which cannot read render state
   *  directly -- the same reason `sessionStartedRef` exists. A switch landing on a card must check
   *  whether the keys are even in this window right now: landing them silently in a background
   *  window would be a surprise nobody asked for. */
  const paneFocusedRef = useRef(false);
  paneFocusedRef.current = paneFocused;
  /** Bumped by each `enter_input` envelope -- since panel round 2 (spec §8, decision 4), that
   *  envelope's only sender is a brand-new tab, so this is always a plain "open the composer" request:
   *  there is never a card to land on instead (a tab this fresh has none). A counter, not a flag, so
   *  two arrivals in a row both act, and so the dispatch handler, installed once, need not read any
   *  state: the effect below decides, against the current render, whether INPUT is possible. It
   *  is also passed to `Composer` as `focusRequest`, which re-focuses a textarea that is already
   *  mounted, since `setMode("input")` alone does nothing when the mode was already INPUT. */
  const [inputRequest, setInputRequest] = useState(0);
  /** Bumped by each `arrive` envelope -- panel round 2's replacement for every keyboard arrival that
   *  used to send `enter_input` (spec §8, decision 4): `Ctrl+h/j/k/l` into the chat, `prefix a`/a tray
   *  chip with no card, and a launch that starts with the keys already in the chat. A counter for the
   *  same reason `inputRequest` is one. The effect below decides, against the current render, whether
   *  a card is waiting (P1, unchanged: lands there instead) or the panel lands BROWSE on the last row
   *  with following resumed -- never the composer, which is the whole point of the reversal. */
  const [arriveRequest, setArriveRequest] = useState(0);
  /** V1 C1 (spec §3.5): bumped by each `nav_key` envelope, direction carried along -- the
   *  `arriveRequest` pattern (a counter, not a flag), so two arrivals in a row both act and the
   *  dispatch handler below (installed once) need read no state. The effect that follows this
   *  declaration decides against the CURRENT render's `mode`/`overlayOpen`/`sessionEnded`/etc., since
   *  the handler's own closure cannot. */
  const [navKey, setNavKey] = useState<{ seq: number; direction: NavKeyDirection } | null>(null);
  const navKeySeqRef = useRef(0);
  /** What actually reaches `EmptyTab`'s own `navKeyRequest` prop: only a `navKey` the effect below
   *  found no overlay (`overlayOpen`, `?`, a y/n) already owning the keys for -- EmptyTab has no
   *  visibility into `keymapOpen`/`confirm` at all (they are this component's own state, drawn over
   *  its layout rather than passed down), so forwarding `navKey` itself unfiltered would let it act
   *  on a request that has already been answered with `nav_fallthrough` above it. */
  const [emptyNavKey, setEmptyNavKey] = useState<NavKeyRequest | null>(null);
  /** Where an empty tab lands when it mounts or is switched to (`EmptyTab`'s `landing`; GUI pass
   *  2026-09-26, r2-gui): the last of `enter_input` (INPUT, R7), `arrive` or a tab switch (BROWSE,
   *  decision 4). Launch starts INPUT, as it always has. */
  const [emptyLanding, setEmptyLanding] = useState<PanelMode>("input");
  /** The empty tab's own mode, reported by `EmptyTab`, for the band drawn under it. */
  const [emptyMode, setEmptyMode] = useState<PanelMode>("input");
  /** Bumped by each `focus_permission` envelope (modules P2: the tray's `agent ⚑N` chip, or `prefix a`
   *  with a card waiting). A counter for the reason `inputRequest` is one: the handler is installed
   *  once and reads no state; the effect below finds the card against the current render. */
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
   *  handler (installed once, session tabs Task 11), which cannot read render state directly --
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
  /** The composer's own unsent text, kept only so a tab switch can save it (ruling 24) -- never
   *  read to render anything itself; `restoredDraft` is what actually reaches `Composer`. Updated
   *  from `Composer`/`EmptyTab`'s `onDraftChange`, which fires on every keystroke and once more
   *  with `""` after a send. */
  const draftRef = useRef("");
  /** Phase 3 (keymap spec §4.2): the queue behind a running turn, the shared prompt history, the
   *  D7 rule offers, the V1 editor-context line, and whether the scratch editor currently holds
   *  this tab's draft. All tab-scoped except `history` and `editorContext`, which are window-wide
   *  (`tabs.ts`'s `TAB_SCOPED`). */
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [queueError, setQueueError] = useState<string | null>(null);
  const [history, setHistory] = useState<string[]>([]);
  const [ruleOffers, setRuleOffers] = useState<Record<string, string>>({});
  const [editorContext, setEditorContext] = useState<ContextSummary | null>(null);
  /** R2's pill, reported up by `MessageList` (panel round 2 plan, Task 10) rather than floated over
   *  the last line by a `NewPill` this component owned itself -- see `MessageList`'s own
   *  `onUnreadChange` doc comment. `jump` starts as a no-op so the band never has to guard a click
   *  that lands before the first `updatePill` (mount, before any scroll). */
  const [unread, setUnread] = useState<{ label: string | null; jump: () => void }>({ label: null, jump: () => {} });
  /** The unread pill's current `seq` threshold, mirrored from `MessageList`'s `onUnreadChange` third
   *  argument (wave 3, Task 3) -- read (not `state`) when a switch saves the departing tab's view, so
   *  a row that arrives while a tab is away is still counted once the reader comes back to it. A ref,
   *  not state: nothing here ever renders on it, the same reason `cursorRef`/`modeRef`/etc. above are
   *  refs. */
  const unseenAfterSeqRef = useRef<number | null>(null);
  /** What to seed `MessageList`'s own threshold with on a restore (wave 3, Task 3): `tick` changes on
   *  every restore that has something to seed, so a second switch back to the same threshold still
   *  re-fires `MessageList`'s seed effect -- see that prop's own doc comment. `null` between restores
   *  and for a tab that was left following or never had a saved threshold. */
  const [unseenSeed, setUnseenSeed] = useState<{ afterSeq: number; tick: number } | null>(null);
  const unseenSeedTick = useRef(0);
  const [scratchEditing, setScratchEditing] = useState(false);
  /** The footer's transient line (ruling 29). `seq` so the same text twice is two flashes. */
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
  /** `queue_taken`, for the composer to merge (ruling 7). */
  const [queueTaken, setQueueTaken] = useState<{ texts: string[]; seq: number } | null>(null);
  const takenSeq = useRef(0);
  // Deviation from Task 7's brief (task-brief step 3, recorded in that task's report): this task
  // wired the eight envelopes into state with nothing rendering any of them yet. Task 8 read four
  // of them (`queue`, `history`, `scratchEditing`, `queueTaken`); Task 9 reads three more --
  // `queueError` (the queue's own error line), `editorContext` (V1's line) and `flash` (the footer's
  // transient line). `ruleOffers` (D7's third button) is read by `MessageList` -- see its own prop
  // just below.

  /** N3: the row just yanked flashes for 400 ms (codecompanion's yank flash). */
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

  /** Ruling 6: the box's text reaches Rust 300 ms after the last change, and at once when the tab
   *  stops being active. `pendingDraftRef` holds what has not been posted yet and for which tab. */
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
  /** C1c: whether the last `j` moved, so the first repeat of a held `j` that then stops at the last
   *  stop still flashes once (the v1-ui GUI pass, 2026-09-27) -- see the move branch of `onKeyDown`. */
  const heldMoveRef = useRef(false);
  /** R4: the open `/` prompt and where the cursor was when it opened; `lastSearchRef` is what `n`/`N` repeat. */
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
  /** R4: the count accumulated from `1`-`9` then `0`-`9`, applied to the next `j`/`k`/`[[`/`]]` and
   *  reset by it (or by anything else that runs). `null` when no digit has been pressed yet. A ref
   *  for the same reason `pendingRef` is one: read once by `onKeyDown`, never rendered. */
  const countRef = useRef<number | null>(null);
  /** A leader/table sequence in progress (`./leader`'s `startSequence`/`advanceSequence`), or
   *  `null` between sequences -- the engine's OWN pending state, distinct from `pendingRef` above,
   *  which is `resolveKey`'s four reserved two-key prefixes (`g`/`z`/`[`/`]`) and predates this
   *  plan. A ref for `onKeyDown` to read synchronously (the same reason `pendingRef` is one) mirrored
   *  into `seq` (state) so the box can render what it names. */
  const seqRef = useRef<{ typed: string[]; ambiguous: PanelBinding | null } | null>(null);
  const [seq, setSeq] = useState<{ typed: string[]; ambiguous: PanelBinding | null } | null>(null);
  /** Whether the which-key box is actually drawn, `WHICH_KEY_DELAY_MS` after whichever became
   *  pending first -- a leader/table sequence (`seqRef`) or one of `resolveKey`'s own reserved
   *  prefixes (`pendingRef`). A ref alongside the state for the same reason `boxShown`'s own timer
   *  needs to read "is it already showing" synchronously, before the state update that would set it
   *  has committed (`applySeqStep`'s and the `"pending"` case's own "only if not already shown"). */
  const boxShownRef = useRef(false);
  const [boxShown, setBoxShown] = useState(false);
  const boxTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** The `timeoutlen` timer that runs an ambiguous node's own action once it elapses with no further
   *  key (spec §2.4) -- armed only while `panelTable.timeout` is true and the pending node both has
   *  a continuation AND is itself a binding (`SeqStep`'s `ambiguous`). */
  const seqTimeoutTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  function cancelBoxTimer() {
    if (boxTimerRef.current !== null) clearTimeout(boxTimerRef.current);
    boxTimerRef.current = null;
  }
  function cancelSeqTimeoutTimer() {
    if (seqTimeoutTimerRef.current !== null) clearTimeout(seqTimeoutTimerRef.current);
    seqTimeoutTimerRef.current = null;
  }
  function showBox() {
    boxShownRef.current = true;
    setBoxShown(true);
  }
  function hideBox() {
    boxShownRef.current = false;
    setBoxShown(false);
  }
  /** (Re)starts the box's `WHICH_KEY_DELAY_MS` timer -- only while it is not already showing (a
   *  step deeper into an already-drawn box must not blink it off and back on), matching
   *  `applySeqStep`'s own doc comment. Shared by the leader engine's own `"pending"` arm and by
   *  `resolveKey`'s reserved two-key prefixes below, since the two can never both be pending at
   *  once (starting either clears the other, `clearSequence`'s own doc comment). */
  function scheduleBoxTimer() {
    if (boxShownRef.current) return;
    cancelBoxTimer();
    boxTimerRef.current = setTimeout(() => {
      boxTimerRef.current = null;
      showBox();
    }, WHICH_KEY_DELAY_MS);
  }
  /** Cancels a leader/table sequence in progress and hides the box, with no side effect beyond
   *  that -- never runs an action. Added (spec §2.4, Review Focus 1) to every place `pendingRef`
   *  itself is already cleared (a pane switch the WebView never saw as a key, a HINT starting
   *  elsewhere, every key `onKeyDown` sees) and to the `keymap` envelope arm, so a table that
   *  changes mid-sequence can never run a binding from the one it replaced. */
  function clearSequence() {
    seqRef.current = null;
    setSeq(null);
    cancelBoxTimer();
    cancelSeqTimeoutTimer();
    hideBox();
  }
  useEffect(() => {
    cancelBoxTimer();
    cancelSeqTimeoutTimer();
  }, []);
  /** One step of the sequence engine: `run` clears the sequence and performs the binding;
   *  `pending` records the new node and, only if the box is not already showing, (re)arms its
   *  `WHICH_KEY_DELAY_MS` timer -- plus, when this node is itself a binding (ambiguous) AND
   *  `panelTable.timeout` is on, a `panelTable.timeoutlen` timer that runs the ambiguous binding
   *  with no further key (spec §2.4); `cancel` clears with nothing run. */
  function applySeqStep(step: SeqStep) {
    if (step.kind === "run") {
      clearSequence();
      runPanelAction(step.binding);
      return;
    }
    if (step.kind === "cancel") {
      clearSequence();
      return;
    }
    if (step.kind === "none") return;
    seqRef.current = { typed: step.typed, ambiguous: step.ambiguous };
    setSeq(seqRef.current);
    cancelSeqTimeoutTimer();
    scheduleBoxTimer();
    if (step.ambiguous !== null && panelTable.timeout) {
      const ambiguous = step.ambiguous;
      seqTimeoutTimerRef.current = setTimeout(() => {
        seqTimeoutTimerRef.current = null;
        applySeqStep({ kind: "run", binding: ambiguous });
      }, panelTable.timeoutlen);
    }
  }
  /** Runs a table binding reached through the leader engine or a reserved-prefix pair (`[b`) --
   *  panel round 2 plan Task 8. `tab.*` posts the window-level `tab_verb` `postToRust` directly
   *  (never `post()`, which would add a `tab` this message has no field for -- Rust's own
   *  `InboundMessage::TabVerb` names none, since whichever tab is under the keys decides). */
  function runPanelAction(binding: PanelBinding) {
    switch (binding.action) {
      case "tab.new":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "new" });
        break;
      case "tab.next":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "next" });
        break;
      case "tab.prev":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "prev" });
        break;
      case "tab.last":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "last" });
        break;
      case "tab.close":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "close" });
        break;
      case "tab.close-others":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "close_others" });
        break;
      case "tab.choose":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "choose" });
        break;
      case "tab.info":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "info" });
        break;
      case "panel.search":
        setSearch({ query: "", origin: cursor });
        break;
      case "panel.keymap":
        setKeymapOpen(true);
        break;
      case "panel.handoff":
        // `<leader>t` (spec §4, §5.4): opens `ContinueInTerminal`'s confirmation directly, since the
        // band leaves no permanently visible button for a click to reach first any more. Opening
        // unconditionally is deliberate -- if the handoff is blocked (no session id yet, a turn
        // running), that component shows why instead of the confirm dialog, rather than this key
        // doing nothing with no explanation.
        setHandoffOpen(true);
        break;
      case "mode.cycle":
        // Disabled in the box once the tab is fixed (`modeFixed`, `leader.ts`'s own `boxEntries` doc
        // comment): cycling makes no sense any more, so this flashes rather than posting. Routed
        // through `modeKeyRoute`, the same rule Shift+Tab uses (R4), so a `starting` or `failed` tab
        // flashes here too instead of posting a `cycle_mode` Rust's `TabSet::cycle_mode` would only
        // refuse -- and, since wave 5, a `live` tab whose sidecar can switch (`modeSwitch`) posts
        // instead of flashing, the which-key box never having greyed it out in the first place.
        switch (
          modeKeyRoute({
            confirmOpen: false,
            chooserOpen: false,
            tabState: activeTab?.state ?? null,
            canSwitch: modeSwitch,
          })
        ) {
          case "cycle":
            post({ type: "cycle_mode" });
            break;
          case "starting":
            showFlash(MODE_STARTING_MESSAGE);
            break;
          case "fixed":
            showFlash(modeFixedMessage(keymapHelp.newTabChord));
            break;
        }
        break;
    }
  }
  /** Whether the `?` keymap overlay is open (spec §3). Toggled by `resolveKey`'s `{kind:"keymap"}`
   *  (opening only -- see `onKeyDown`'s dedicated branch below for why closing never goes through
   *  `resolveKey` at all) and by the handful of places that must force it shut: the session ending,
   *  the start screen coming back, and a HINT starting elsewhere in the window. */
  const [keymapOpen, setKeymapOpen] = useState(false);
  /** The detail popover's rows (session tabs spec §3.3), or `null` when it is closed. Set by a
   *  `tab_detail` envelope (the reply to `open_detail`, sent by the band and by `prefix i`);
   *  closed the same places `keymapOpen` is forced shut, since the two are mutually exclusive
   *  overlays over the same conversation area. */
  const [detail, setDetail] = useState<DetailRow[] | null>(null);
  /** The row `j`/`k`/`y` act on inside the popover, reset to 0 every time it opens. */
  const [detailCursor, setDetailCursor] = useState(0);
  /** Whether `ContinueInTerminal`'s confirmation is open (panel round 2 plan, Task 10; spec §5.4).
   *  Lifted here from that component's own local state -- which used to gate on a permanently
   *  visible button's click -- because the band leaves no permanently visible button any more: the
   *  only entry points now are `<leader>t` (`"panel.handoff"`, `onKeyDown` below) and the detail
   *  popover's own trailing row. Closed everywhere the other conversation-area overlays are (a
   *  pane-focus change, a tab switch, `enter_input`/`arrive`, `focus_permission`, `hint_collect`,
   *  a rename/close-prompt/chooser opening, an `error`/`handoff` whole-state reset) and by `Esc`. */
  const [handoffOpen, setHandoffOpen] = useState(false);
  /** N2: a `gf` with several paths waiting for its letter (ruling 19), or `null` between them. Set
   *  by the `open-path` action below, cleared by whichever letter (or anything else) answers it. */
  const [pathPick, setPathPick] = useState<PathRef[] | null>(null);
  /** N2/R3: `open_path` and `view_in_editor` share `"editor"` in-flight bookkeeping with Task 8's
   *  `edit_draft` (see `inFlight`'s own doc comment). Neither carries a `tab`-scoped reply payload of
   *  its own to restore, only a refusal to flash. */
  function openPath(ref: PathRef) {
    const requestId = nextRequestId();
    inFlight.current.set(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_path", request_id: requestId, path: ref.path, ...(ref.line === null ? {} : { line: ref.line }) });
  }
  /** The popover's own scrollable root -- unused today (it has nothing to scroll into view yet),
   *  kept for parity with `keymapOverlayRef` and because a forwarded ref is part of the component's
   *  contract. */
  const detailRef = useRef<HTMLDivElement>(null);
  /** The inline rename field's tab and prefilled text, or `null` when no rename is open (session
   *  tabs spec §3.5, ruling 6: the tab bar is shown even with one tab while this is set). Set by a
   *  `begin_rename` envelope (`prefix ,`); cleared on commit or cancel. */
  const [renaming, setRenaming] = useState<{ tab: TabId; initial: string } | null>(null);
  /** The window-close confirmation prompt (spec §3.5, ruling 7, ruling 15), or `null` when closed.
   *  Drawn in the footer in place of the which-key strip, and takes every key while it is open. Set
   *  by a `confirm_close` envelope (`prefix &`, or the chooser's own `x`) -- kind `"close"`, one
   *  named tab -- or a `confirm_close_others` envelope (`<leader>bo` / `tab.close-others`, panel
   *  round 2 plan Task 8) -- kind `"close_others"`, no tab of its own, the same reason
   *  `InboundMessage::CloseOthers` carries none. Answered by `y` (close) or any other key (cancel)
   *  in `onKeyDown`, through `answerConfirm`. */
  const [confirm, setConfirm] = useState<
    { kind: "close"; tab: TabId; lines: string[] } | { kind: "close_others"; lines: string[] } | null
  >(null);
  /** `prefix w` (spec §3.6): the open-tabs-then-records overlay, or `null` when closed. Set by a
   *  `chooser` envelope; not tab-scoped (`tabs.ts`'s `TAB_SCOPED` omits it) since it is a
   *  window-wide picker, not a view of the active tab. Wave 4 R2: no longer opened at launch (D10
   *  is gone), only by this key. */
  const [chooser, setChooser] = useState<ChooserEnvelope | null>(null);
  /** Wave 3 Task 1 (launch-chooser bug investigation, `~/.cache/launch-chooser-bug/`): true while an
   *  overlay drawn OVER the conversation or the empty tab owns the keys -- the chooser, a tab
   *  rename, or the `/` search prompt (the live layout's only, hence `sessionStarted`: `search`
   *  can outlive the tab it opened on, and a stale value under the empty layout, where `SearchBar`
   *  never renders, would otherwise strand the keys for good). `?`, the detail popover and the
   *  handoff confirm are NOT here (decision, spec §2.4/Review Focus 1): `pane_focus`/`arrive`
   *  already close those three, so nothing needs to re-check them here. Read by every effect below
   *  that could otherwise steal the keys out from under one of these three overlays. */
  const overlayOpen = chooser !== null || renaming !== null || (search !== null && sessionStarted);
  /** Counters `takeKeys` (below) bumps to ask a specific overlay/composer to re-focus itself,
   *  mirroring the existing `inputRequest`/`arriveRequest` convention: a plain number so a second
   *  request while the first is still pending is never silently coalesced away by React (an
   *  unchanged boolean would be). */
  const [chooserFocusRequest, setChooserFocusRequest] = useState(0);
  const [tabBarFocusRequest, setTabBarFocusRequest] = useState(0);
  const [emptyKeysRequest, setEmptyKeysRequest] = useState(0);
  /** What `Composer` (the live conversation's) actually reads, instead of the raw `inputRequest`
   *  counter it used to take directly. `Composer`'s own `[focusRequest, mode]` effect focuses the
   *  textarea whenever `mode === "input"`, with no way to gate it from outside -- so a raw
   *  `inputRequest` bump under an open overlay would autofocus a composer drawn underneath it
   *  (defect 2). Bumped from the `inputRequest` effect's own pass branch, below, so ordinary
   *  behaviour (no overlay open) is unchanged; and (C1a) from `resolveKey`'s own `i`/`o`/`A` --
   *  `mode` alone already re-mounts the textarea (BROWSE's stand-in unmounts, INPUT's own mounts),
   *  but this is what makes `Composer`'s effect actually run `focusAtCaret` rather than leaving the
   *  caret to whatever a bare `autoFocus` on a non-empty value happens to land on (defect 1's
   *  sibling, F12: `i` after an `Esc` re-mounted the box at position 0). */
  const [composerFocusRequest, setComposerFocusRequest] = useState(0);
  /** C1a (spec §3.2): which end `composerFocusRequest`'s NEXT bump should place the caret at --
   *  `"kept"` for `i`/`o` (wherever it was left, `Composer`'s own `caretRef`), `"end"` for `A`.
   *  Read once, alongside the bump, by `Composer`'s own effect; not itself a `PanelMode`-changing
   *  request the same way `composerFocusRequest` is, so it carries no `seq` of its own. */
  const [composerCaret, setComposerCaret] = useState<"kept" | "end">("kept");
  const [keysRequest, setKeysRequest] = useState(0);
  /** The overlay's last two sections and its heading, from `shell`'s `keymap` envelope (sent on
   *  every `ready`, right after the theme). `prefix` defaults to `Ctrl+b`, the stock tmux default,
   *  until that envelope arrives. */
  const [keymapHelp, setKeymapHelp] = useState<KeymapHelp>({
    prefix: "Ctrl+b",
    window: [],
    prefixKeys: [],
    // Task 7 note: `panel`/`newTabChord` are new wire fields (Task 5/6); nothing reads them off
    // this state yet -- that is Task 8's own job -- so the pre-envelope default is the same "no
    // bindings" table `KeyContext.table` itself defaults to.
    panel: EMPTY_PANEL_TABLE,
    newTabChord: "",
  });
  /** The panel's own which-key table (panel round 2 plan, Task 8), read off `keymapHelp` -- Task 7's
   *  own note above is this task. */
  const panelTable = keymapHelp.panel;
  /** Wave 4 Task 1: mirrors of state the document-capture Shift+Tab router (below, `onModeKey`)
   *  must read live -- refs so that effect, installed once with an empty dependency array, always
   *  sees this render's values rather than the one from when it was installed. Assigned here in the
   *  render body, the same pattern `sessionStartedRef` above already uses. */
  const confirmOpenRef = useRef(false);
  confirmOpenRef.current = confirm !== null;
  const chooserOpenRef = useRef(false);
  chooserOpenRef.current = chooser !== null;
  const tabsRef = useRef<TabsEnvelope | null>(null);
  tabsRef.current = tabs;
  const newTabChordRef = useRef("");
  newTabChordRef.current = keymapHelp.newTabChord;
  /** Wave 5 Task 5: `modeSwitch` for the same document-capture `onModeKey` effect, which is
   *  installed once and cannot read render state directly -- the same reason the refs above exist. */
  const canSwitchRef = useRef(false);
  canSwitchRef.current = modeSwitch;
  /** The overlay's own scrollable root, so `j`/`k` typed while it is open can scroll IT rather than
   *  the conversation underneath (`onKeyDown`'s `keymapOpen` branch). */
  const keymapOverlayRef = useRef<HTMLDivElement>(null);
  /** One ordered view of the conversation, kept in step with the cursor/expand keys below. See
   *  `MessageList`'s own copy of this memo for why it is keyed on `state` as a whole; `expanded` and
   *  `detailed` join it here for the same reason P2's collapsed runs do -- `buildDisplay` folds or
   *  unfolds rows by both, so a change to either has to recompute what the cursor actually indexes. */
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
  /** v1 S1 (spec §2.1, `./typingGuard`): every keydown `onKeyDown` sees is fed to it first; `a`/`d`/
   *  `D` wait `TYPING_GUARD_MS` through it, and Enter on a card button asks it. One per panel, kept
   *  across renders (the dispatch below is installed once and reads this same instance). */
  const [typingGuard] = useState(() => new TypingGuard());
  /* An overlay drawn over the conversation takes the keys from the card a waiting answer was aimed
     at (spec §2.1: "an overlay opening"), whichever route opened it -- a key, an envelope, a click. */
  useEffect(() => {
    if (keymapOpen || detail !== null || handoffOpen || pathPick !== null || confirm !== null || overlayOpen) {
      typingGuard.cancel();
    }
  }, [typingGuard, keymapOpen, detail, handoffOpen, pathPick, confirm, overlayOpen]);
  useEffect(() => () => void typingGuard.cancel(), [typingGuard]);
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
  /** Wave 3 Task 1: the one place every keyboard arrival (`pane_focus` regaining focus, `arrive`,
   *  `enter_input`, a landing on a permission card) goes to ask for the keys back, instead of each
   *  reaching for `containerRef`/a composer directly and risking one of them winning over an open
   *  overlay (the launch-chooser investigation's defects 1-2). Reads the CURRENT render's `chooser`/
   *  `renaming`/`overlayOpen`/`sessionStarted`, so it must only ever be called from an effect (which
   *  runs after commit, with this render's closure) -- never during render itself. Order: whichever
   *  overlay is drawn topmost first (chooser, then a tab rename, then the `/` prompt), else the live
   *  conversation, else the empty tab's own dashboard. */
  function takeKeys() {
    if (chooser !== null) {
      setChooserFocusRequest((n) => n + 1);
      return;
    }
    if (renaming !== null) {
      setTabBarFocusRequest((n) => n + 1);
      return;
    }
    if (search !== null && sessionStarted) {
      containerRef.current?.querySelector<HTMLInputElement>(".search-bar input")?.focus();
      return;
    }
    if (containerRef.current !== null) {
      // A `pane_focus true` that follows a click on Approve, or a HINT landing on a control, must
      // not steal the keys back from it -- `document.activeElement` already being inside the root
      // (a permission card's own deny-reason box included) means somewhere in here legitimately
      // holds them already (regression guard, test h).
      const root = containerRef.current;
      const active = document.activeElement;
      if (active === null || active === document.body || !root.contains(active)) {
        root.focus({ preventScroll: true });
      }
      return;
    }
    setEmptyKeysRequest((n) => n + 1);
  }
  useEffect(() => {
    if (keysRequest === 0) return;
    takeKeys();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keysRequest]);
  useEffect(() => {
    if (mode !== "browse" || overlayOpen) return;
    const landed = landedControlRef.current;
    if (landed !== null && landed.isConnected) {
      landed.focus();
      return;
    }
    if (isEditableElement(document.activeElement)) return;
    containerRef.current?.focus();
  }, [mode, sessionStarted]);
  /* Wave 4 Task 1: Shift+Tab is Claude Code's mode key everywhere in the chat, and never focus
     navigation (owner, 2026-09-26: "在agent pane都要可以直接切换"). Capture phase on `document`: ahead
     of every React handler (EmptyTab's, Composer's, Chooser's, this component's own `onKeyDown`) and
     of WebKit's own default backward-focus-navigation handling for Shift+Tab, which is what used to
     hand the keys to GTK's window `move-focus` once WebKit ran out of focusable elements. */
  useEffect(() => {
    function onModeKey(event: globalThis.KeyboardEvent) {
      if (!isModeCycleKey(event)) return;
      event.preventDefault();
      const route = modeKeyRoute({
        confirmOpen: confirmOpenRef.current,
        chooserOpen: chooserOpenRef.current,
        tabState: activeTabInfo(tabsRef.current)?.state ?? null,
        canSwitch: canSwitchRef.current,
      });
      if (route === "overlay") return;
      event.stopPropagation();
      if (route === "cycle") post({ type: "cycle_mode" });
      else if (route === "starting") showFlash(MODE_STARTING_MESSAGE);
      else if (route === "fixed") showFlash(modeFixedMessage(newTabChordRef.current));
    }
    document.addEventListener("keydown", onModeKey, true);
    return () => document.removeEventListener("keydown", onModeKey, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
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
  /** P2: a run collapsing or expanding shifts every index after it, the same shrink-by-something-
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
   *  **Deviation from the brief's own snippet, recorded here because it reproduced 47 failing tests
   *  first.** The brief's version fires this reconciliation on `cursor` alone changing too (its own
   *  dependency array is `[timeline, cursor]` with no guard distinguishing the two), which means an
   *  ORDINARY `j`/`k`/`gg`/`G`/search move -- `timeline` unchanged, only `cursor` moves -- reads as
   *  "the row `want` pointed at relocated", finds that same row still sitting at its OLD index (since
   *  nothing structural changed), and calls `setCursor` right back to where the user just moved away
   *  from. Every keyboard-driven cursor move in the suite failed this way. `prevTimelineRef` below is
   *  the fix: the reconciliation only runs when `timeline`'s own object identity changed since the
   *  last time this effect ran (a real collapse/expand/tab-switch), never merely because `cursor` did. */
  const cursorKeyRef = useRef<string | null>(null);
  const prevTimelineRef = useRef<TimelineItem[] | null>(null);
  /* **Correction (the small-defects GUI pass, 2026-09-25): and only when `cursor` did NOT change.**
     `timeline` is a new object on every streamed delta, so a cursor move committed in the same render
     as one -- R1's clamp, a `j`/`k`, a restore -- met `timelineChanged`, found the key it had just left
     still at its old index, and was put straight back. The move back carried no `"keep"` (the slot
     was spent on the move it undid), so the `[cursor]` effect revealed the stale row, scrolling the
     list up inside a restore's steering window, and `MessageList` stopped following: a tab switched
     back to mid-stream came back parked above the end with a pill. A cursor that moved in this render
     was moved on purpose; only a timeline change under a still cursor is a fold to follow. */
  const prevCursorRef = useRef(cursor);
  useLayoutEffect(() => {
    const timelineChanged = prevTimelineRef.current !== null && prevTimelineRef.current !== timeline;
    const cursorMoved = prevCursorRef.current !== cursor;
    prevTimelineRef.current = timeline;
    prevCursorRef.current = cursor;
    const want = cursorKeyRef.current;
    if (timelineChanged && !cursorMoved && want !== null && timeline[cursor]?.key !== want) {
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
  useEffect(() => {
    const landing = landingRef.current;
    landingRef.current = 0;
    if (landing === "keep") return;
    const row = containerRef.current?.querySelector<HTMLElement>(".row-current");
    if (row == null) return;
    revealRow(row.closest<HTMLElement>(".message-list"), row, landing);
  }, [cursor]);
  /** R1: a scroll the panel did not cause by moving the cursor -- a wheel, a drag, the browser's own
   *  scroll keys -- can leave the cursor's row off screen. Whenever that happens, the cursor moves to
   *  the nearest visible row; the view itself is never touched here (`clampCursorToView` above never
   *  writes `scrollTop`, review focus 5). `sessionStarted` mirrors the conversation layout's own
   *  mount condition, so this attaches only while `.message-list` actually exists. */
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
  /** R1: at the bottom, the cursor rides the new last row as the conversation grows, and a sent
   *  prompt takes the cursor outright. Keyed on `timeline`, since a plain `state` change (a card
   *  answered, a tool result arriving) must not move the cursor when the timeline itself did not
   *  grow. `landOnPromptRef` is set by the `events` dispatch arm the moment a `user_prompt_submitted`
   *  arrives in the batch; it is read and cleared here rather than in the dispatcher, since the row
   *  it targets does not exist until this render's `timeline` reflects it. */
  const lastLengthRef = useRef(0);
  const landOnPromptRef = useRef(false);
  /** Set by a switch's (or a mount's) first snapshot with no saved view: see the `snapshot` arm. */
  const landOnLastRef = useRef(false);
  /* A layout effect since the small-defects GUI pass (2026-09-25). As a passive effect it ran after
     the browser's next rendering step, which is where the list's scroll event -- from `MessageList`'s
     own snap to the end -- is dispatched; R1's clamp therefore saw the cursor still on the row that
     WAS last, off screen once a row taller than the view had arrived, and moved it to the nearest
     visible row, which is not always the last one. The ride then no longer matched (`cursor` was no
     longer `previous - 1`), and a following view kept its cursor a row above the end for the rest
     of the reply. `MessageList`'s snap is a layout effect of a child, so it has already run here and
     the distance read below is the snapped one. */
  useLayoutEffect(() => {
    const previous = lastLengthRef.current;
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
    if (timeline.length <= previous || cursor !== previous - 1) return;
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    const atBottom = !list || list.scrollHeight - list.scrollTop - list.clientHeight <= 1;
    if (atBottom) {
      landingRef.current = "keep";
      setCursor(timeline.length - 1);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline]);
  /** The scroll half of a tab switch's view restore (session tabs Task 11): cursor and mode are set
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
    // A tab left while following comes back following (the small-defects GUI pass, 2026-09-25):
    // said outright, as a send does, rather than as a scroll to the end the list reads a frame later
    // inside a steering window -- where any other movement above the end counted as the user
    // scrolling up. A tab left parked comes back exactly where it was.
    if (view.atBottom) {
      resumeFollowing(list);
      return;
    }
    noteUserScroll(list, "unknown");
    list.scrollTop = view.scrollTop;
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
  // The `?` keymap belongs to a live conversation (spec §3, Task 4's docs); a session that just
  // ended is not a reason to keep it up, and the ended/lost banners' own `r` must be reachable
  // without an extra `?`/`Escape`/`q` first.
  useEffect(() => {
    if (sessionEnded) setKeymapOpen(false);
  }, [sessionEnded]);
  /* A brand-new tab's own arrival (panel round 2, spec §8, decision 4: `enter_input`'s only sender
     now), or a HINT landing on the composer: INPUT with a blinking caret. Refused under the same
     condition `i` is (`resolveKey`): a dead session has no box to type into, and no session at all
     has no composer. Keyed on the request alone, so a session change never opens the composer by
     itself. There is no card to check for here any more -- a tab this fresh has none, and every
     arrival that COULD find one waiting now sends `arrive` instead (the effect just below). */
  useEffect(() => {
    if (inputRequest === 0) return;
    // Wave 3 Task 1: an overlay drawn over the conversation must keep the keys rather than let
    // this open a composer underneath it (launch-chooser defect 2).
    if (overlayOpen) {
      takeKeys();
      return;
    }
    if (!sessionStarted || sessionEnded) return;
    setMode("input");
    // Fix round 1 (reviewer finding): every route that reaches the live composer through this
    // counter -- `enter_input`, `landOnHint`'s "composer" HINT target, `focus_permission`'s "no
    // card to land on, treat as an ordinary arrival" fallback, `returnKeysToRoot`'s empty-layout
    // fallback -- must carry the same caret rule `i`/`o` and `Ctrl+j` do (§3.1/§3.2: "kept"), not
    // whatever `composerCaret` was last left at by an earlier `A` press. `A` itself never touches
    // this counter (the "mode" case above bumps `composerFocusRequest` directly and sets its own
    // caret), so nothing here ever legitimately wants "end". Reproduced without this fix: press `A`
    // once, leave INPUT, then land back on the composer via HINT or a stale permission arrival --
    // the caret went to the end of the draft instead of staying where it was left.
    setComposerCaret("kept");
    setComposerFocusRequest((n) => n + 1);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inputRequest]);
  /* `arrive` (panel round 2, spec §8, decision 4): reverses the 2026-09-19 ruling "control l直接闪
     cursor" for every keyboard arrival except a brand-new tab's (`enter_input`, unchanged, above). A
     card waiting in this tab gets the same landing `focus_permission` gives -- BROWSE, on the oldest
     one -- never a composer the user never asked to type into. Otherwise BROWSE on the last row, with
     following resumed the same way a send does (`./follow.ts`): the point of the reversal is that a
     stray keyboard arrival must never fall straight into a live turn's box. Keyed on the request
     alone, like `inputRequest`. */
  useEffect(() => {
    if (arriveRequest === 0) return;
    // Wave 3 Task 1: an overlay open over the conversation keeps the keys and the conversation's
    // cursor does not move (Review Focus 1, test d) -- landing BROWSE on the last row would fight
    // it for both.
    if (overlayOpen) {
      takeKeys();
      return;
    }
    if (oldestPendingPermission(timeline) !== null) {
      setPermissionRequest((n) => n + 1);
      return;
    }
    const last = Math.max(timeline.length - 1, 0);
    // Only when the cursor really moves -- a `"keep"` set for a `setCursor` that changes nothing
    // fires no `[cursor]` effect, so nothing consumes it, and it swallows the NEXT move's reveal
    // (the same convention the switch restore below follows).
    if (last !== cursorRef.current) landingRef.current = "keep";
    setMode("browse");
    setCursor(last);
    resumeFollowing(containerRef.current?.querySelector(".message-list"));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [arriveRequest]);
  /* `focus_permission` (modules spec §3.3): BROWSE, with the cursor on the oldest pending card, so
     `a`/`d` answer it at once. `shell` sends it only when it counts a card; if the card was answered
     in between, there is nothing to land on, and the arrival is an ordinary keyboard one -- the
     composer, as `enter_input` gives it. Keyed on the request alone, like `inputRequest`. */
  useEffect(() => {
    if (permissionRequest === 0) return;
    // Wave 3 Task 1: same as `arriveRequest` above. Note `focus_permission`'s own dispatch arm
    // already closes the chooser (unchanged, decision 7/10), so this only ever keeps the keys in
    // an open rename field or the `/` prompt -- the recorded decision, applied literally.
    if (overlayOpen) {
      takeKeys();
      return;
    }
    const index = oldestPendingPermission(timeline);
    if (index === null) {
      setInputRequest((n) => n + 1);
      return;
    }
    setMode("browse");
    // Landing moves the cursor, and the `[cursor]` effect reveals its row: a scroll the user asked
    // for, announced the way a HINT landing's is (`./follow.ts`). Unannounced, `MessageList` takes it
    // for nobody's and keeps following, and the next delta or resize snaps an older card back out of
    // view (the merge of modules P2 with the streaming-scroll fix, 2026-09-24).
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

  /* The panel's half of the global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md).
     `shell` owns the session and every key typed during it -- the panel never sees those keys, so
     there is no HINT key handling here at all. The panel only answers `shell`'s five envelopes:
     report and freeze its visible targets, draw their labels, narrow them, land on one, clear.
     Refs, because the dispatch handler is installed once and must never read stale render state. */
  /** The HINT session this panel is part of, or null. Every envelope naming another one is ignored
   *  (spec §4, invariant 5): a late `hint_show` from a session `shell` already gave up on must not
   *  draw labels over a panel that has moved on. */
  const hintSessionRef = useRef<number | null>(null);
  /** The targets frozen at `hint_collect`, in the order `shell` addresses them by index. Frozen, not
   *  re-read, so an index means the same element for the whole session even if the DOM changes. */
  const frozenRef = useRef<HintTarget[]>([]);
  const [hints, setHints] = useState<ShownHint[]>([]);
  const [hintTyped, setHintTyped] = useState("");
  /** A code block HINT landed on: the next `y` copies exactly that block's code rather than the
   *  whole message (spec §2.4). Cleared by that copy and by any other key the table resolves. */
  const copyCodeRef = useRef<HTMLElement | null>(null);
  /** A control HINT landed on, for the `mode` effect above to leave focused: landing from INPUT
   *  blurs the composer, which sets BROWSE, and that effect would otherwise take focus straight back
   *  to the root. Consumed or dropped on the very next commit (the effect just below). */
  const landedControlRef = useRef<HTMLElement | null>(null);
  useEffect(() => {
    landedControlRef.current = null;
  });

  /** Set from this panel posting `hint_request` until the HINT it asked for ends here (`endHint`,
   *  which every `hint_end` and `hint_land` reaches), or the timer that gives up waiting fires.
   *  Found in the whole-branch review: `shell` attaches the window key controller that swallows HINT keys only once it has handled the script message, so until then
   *  a label typed right after `f` reached THIS panel's key table -- `a`/`d` pressed a permission
   *  card's Approve/Deny (spec §4 invariants 1 and 6), `i` opened the composer, and a second `f`
   *  asked for a second HINT that cancelled the first. While this is set, the capture-phase listener
   *  below swallows every key before any handler in the panel sees it. Once `shell`'s controller
   *  is attached the panel receives no keys anyway, so this outliving that moment costs nothing. Such a key is dropped, not
   *  replayed: the labels are not drawn yet, so it cannot have been meant for one (spec §2.5). */
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
      // Every session-scoped envelope names its tab (session tabs spec §3.1); the panel keeps only
      // the active tab's state, so one for any other tab -- a batch still in flight when the user
      // switched away -- is dropped here, before any of the arms below can touch `state`.
      if (!acceptsEnvelope(payload as { kind: string; tab?: number }, activeTabRef.current)) return;
      if (CANCELS_WAITING_ANSWER.has(payload.kind)) typingGuard.cancel();
      if (payload.kind === "theme") {
        applyTheme(payload.vars);
      } else if (payload.kind === "pane_focus") {
        // A pending prefix is for the very next key; a pane switch in between (GTK takes `Ctrl+h`/
        // `Ctrl+k` before the WebView sees a keydown) must not leave it armed for a key pressed much
        // later, when it would complete a chord nobody meant to start -- review.
        pendingRef.current = null;
        // The leader engine's own pending sequence, and the box waiting to show it (spec §2.4,
        // Review Focus 1), are cancelled the same way and for the same reason: a switch away and
        // back must not leave a table sequence -- or its box timer -- running for a press that has
        // nothing to do with whatever started it.
        clearSequence();
        // A keymap for THIS panel has no reason to stay drawn while another pane has the keys, and
        // leaving it up is how the review reproduced a dead keyboard: come back with `Ctrl+l`,
        // land in INPUT, and every keystroke is swallowed by the overlay's own branch (review).
        setKeymapOpen(false);
        setDetail(null);
        setHandoffOpen(false);
        // R4: the `/` prompt is this panel's own, the same reason the `?` overlay closes here.
        setSearch(null);
        setPaneFocused(payload.focused);
        // Wave 3 Task 1: only REGAINING focus is a request for the keys back -- losing it is not a
        // request for anything, and WebKitGTK's DOM focus across the round trip is not guaranteed
        // to have survived (`takeKeys`'s own doc comment), so this is the one place that asks.
        if (payload.focused) setKeysRequest((n) => n + 1);
      } else if (payload.kind === "enter_input") {
        // Same reason, the other half of that reproduction: this puts the caret in the composer, so
        // the overlay must not be left covering it and eating what gets typed. Panel round 2 (spec
        // §8, decision 4): this envelope's only sender is a brand-new tab now, so there is never a
        // card waiting for it to land on instead.
        setKeymapOpen(false);
        setDetail(null);
        setHandoffOpen(false);
        setEmptyLanding("input");
        setInputRequest((n) => n + 1);
      } else if (payload.kind === "arrive") {
        // Panel round 2 (spec §8, decision 4): every keyboard arrival that used to send `enter_input`
        // -- and so land in INPUT -- now sends this instead, and lands BROWSE. The overlay would sit
        // over whatever this lands on, the same reason `enter_input` closes it above.
        setKeymapOpen(false);
        setDetail(null);
        setHandoffOpen(false);
        // A reserved two-key prefix or a leader/table sequence armed from before this arrival means
        // nothing about it -- the same reason `pane_focus` cancels both (spec §2.4, Review Focus 1).
        pendingRef.current = null;
        clearSequence();
        setEmptyLanding("browse");
        setArriveRequest((n) => n + 1);
      } else if (payload.kind === "focus_permission") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        pendingRef.current = null;
        clearSequence();
        // The overlay would cover the card the cursor is about to land on.
        setKeymapOpen(false);
        setDetail(null);
        setHandoffOpen(false);
        setChooser(null);
        setPermissionRequest((n) => n + 1);
      } else if (payload.kind === "nav_key") {
        // GTK takes `Ctrl+j`/`Ctrl+k` before the WebView sees a keydown, so this is a key pressed
        // after any pending `g`/`z`/`[`/`]` prefix or leader sequence -- cancelled here for the same
        // reason `pane_focus` and `arrive` cancel them, whether the effect below claims the chord or
        // answers it with `nav_fallthrough`. Left armed, `g`, Ctrl+j, Ctrl+k, `g` ran a stale `gg`
        // (whole-branch review of v1-ui).
        pendingRef.current = null;
        clearSequence();
        // V1 C1 (spec §3.5): the raw handler only records the request -- see `navKey`'s own doc
        // comment for why the decision has to live in an effect instead.
        navKeySeqRef.current += 1;
        setNavKey({ seq: navKeySeqRef.current, direction: payload.direction });
      } else if (payload.kind === "keymap") {
        // A table that changed while a sequence was pending must never let the OLD table's binding
        // run against the new one (Review Focus 1) -- cancelled before the new table is even
        // stored, so nothing between these two statements could read a mismatched pair.
        clearSequence();
        setKeymapHelp({
          prefix: payload.prefix,
          window: payload.window,
          prefixKeys: payload.prefixKeys,
          panel: payload.panel,
          newTabChord: payload.newTabChord,
        });
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
        // Ruling 11 kept the start screen free of the overlay, and ignored this there so nothing
        // armed would pop up over the conversation once a session started. Panel round 2 (spec §7)
        // gave the empty tab `? Keys`, so the start screen draws it now (GUI pass 2026-09-26) and
        // it is opened, and closed, where it is seen.
        setMode("browse");
        setKeymapOpen(true);
      } else if (payload.kind === "hint_collect") {
        // A HINT started elsewhere in the window must not label rows hidden under this overlay
        // (spec §3.1). It also frees the keys `hint_collect`'s own reply is about to swallow --
        // this and HINT never actually contend for them, but closing here keeps that true by
        // construction rather than by the two features happening not to overlap in practice.
        setKeymapOpen(false);
        setDetail(null);
        setHandoffOpen(false);
        setChooser(null);
        // R4: the labels would sit over the search prompt, and HINT and `/` never contend for keys.
        setSearch(null);
        // ...and the prefix the strip may still be waiting on, for the reason `pane_focus` does it.
        pendingRef.current = null;
        clearSequence();
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
        // arrives -- the WebView holds only the active tab's state (spec §3.1), so nothing here may
        // carry over from the tab just left. The first `tabs` this window ever sees (mount) also
        // takes this branch, since `activeTabRef.current` starts `null`.
        if (payload.active !== activeTabRef.current) {
          switchedRef.current = true;
          // v1 S1 (spec §2.1): a waiting `a`/`d`/`D` was aimed at the old tab's card.
          typingGuard.cancel();
          // A sequence pending in the OLD tab's conversation means nothing about the new one (spec
          // §2.4's cancel list).
          pendingRef.current = null;
          clearSequence();
          // Ruling 6: the tab stops being active, so whatever has not yet been mirrored goes now
          // rather than waiting out the rest of its 300ms debounce against a tab nobody is reading.
          flushDraft();
          const previous = activeTabRef.current;
          // The old tab's view is saved from the live refs -- BEFORE the resets below overwrite
          // the render state they mirror -- and only when there IS an old tab (not on mount).
          if (previous !== null) {
            const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
            const atBottom = list === null || list.scrollTop + list.clientHeight >= list.scrollHeight - 1;
            saveView(viewStore.current, previous, {
              cursor: cursorRef.current,
              mode: modeRef.current,
              expanded: expandedRef.current,
              scrollTop: list?.scrollTop ?? 0,
              atBottom,
              detailed: detailedRef.current,
              // At the bottom, following is what a restore should resume (see the `snapshot` arm's
              // restore block below), so no threshold is worth remembering -- and `null` there matches
              // `MessageList`'s own "while following" reset (wave 3, Task 3).
              unseenAfterSeq: atBottom ? null : unseenAfterSeqRef.current,
            });
          }
          restoreRef.current = takeView(viewStore.current, payload.active) ?? null;
          // The box goes empty on a switch, never the left tab's words: Rust's own `draft` envelope
          // for the new tab follows in the same batch (ruling 6), so this is never what the reader
          // actually sees for longer than one dispatch.
          restoreSeq.current += 1;
          draftRef.current = "";
          setRestoredDraft({ text: "", seq: restoreSeq.current });
          setState(initialState());
          setCursor(0);
          setExpanded({});
          setDetailed(false);
          // Panel round 2 (spec §8, Owner answers Q1): a tab switch always lands BROWSE now, never
          // whatever mode the tab was left in -- `ca317ff`'s cursor/scroll restore below is otherwise
          // unchanged. Nothing later in this switch (including the `snapshot` arm's own restore)
          // may set this back to INPUT.
          setMode("browse");
          // The first `tabs` (mount) is no switch: launch keeps its INPUT.
          if (previous !== null) setEmptyLanding("browse");
          setKeymapOpen(false);
          setDetail(null);
          setHandoffOpen(false);
          setChooser(null);
          // R4: a `/` prompt was over the OLD tab's conversation and means nothing over the new one.
          setSearch(null);
          setTurnClock(null);
          setHandoff(null);
          setFatalError(null);
          setCommandNotice(null);
          setQueue([]);
          setQueueError(null);
          setRuleOffers({});
          // The OLD tab's unread pill means nothing over a fresh, empty timeline. A tab with no saved
          // view (never parked, or a reload's/fresh mount's first snapshot) or one left following
          // starts with no threshold to remember -- the `snapshot` arm's restore block below sets a
          // real one back in (via `unseenSeed`) only for a tab whose saved view was actually parked
          // (wave 3, Task 3).
          setUnread({ label: null, jump: () => {} });
          unseenAfterSeqRef.current = null;
          setScratchEditing(false);
          setQueueTaken(null);
        }
        activeTabRef.current = payload.active;
        setTabs({ active: payload.active, tabs: payload.tabs, defaultMode: payload.defaultMode });
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
        // The view a switch saved for this tab (session tabs Task 11), if it had one -- cursor applies
        // now (mode does not: panel round 2, spec §8, Owner answers Q1 -- the switch always lands
        // BROWSE, set by the `tabs` arm above, and nothing here may restore a saved INPUT over it);
        // the scroll position is left to the layout effect below, since it needs the row this cursor
        // lands on to exist in the DOM first. `landingRef` is set BEFORE the restored `setCursor` so
        // the `[cursor]` reveal effect does not fight this restore with its own `"nearest"` scroll for
        // a plain +1/-1 move.
        if (restoreRef.current !== null) {
          const view = restoreRef.current;
          // Only when the cursor really moves: a `"keep"` set for a `setCursor` that changes nothing
          // fires no `[cursor]` effect, so nothing consumes it, and it swallowed the NEXT move's reveal
          // -- P1's landing on a card below the restored view (the phase-3 GUI pass, 2026-09-25).
          // A view that was following the end comes back on the LAST row, as a first snapshot does
          // (`landOnLastRef` below): rows kept arriving while the tab was away, so the cursor saved
          // then names a row that is no longer last (the small-defects GUI pass, 2026-09-25).
          if (!view.atBottom) {
            if (view.cursor !== cursorRef.current) landingRef.current = "keep";
            setCursor(view.cursor);
          }
          setExpanded(view.expanded);
          setDetailed(view.detailed);
          // Tells the scroll-restoring layout effect a real restore landed in THIS render -- see
          // its own doc comment for why it cannot simply key on `state`.
          setRestoreTick((n) => n + 1);
          // Wave 3, Task 3: a parked view whose threshold was actually saved gets it seeded back into
          // `MessageList` -- an `atBottom` view has none (the `tabs` arm's `saveView` call never saves
          // one for it), and `MessageList` itself already comes back following on that path with
          // nothing to seed. `unseenSeedTick`, not `restoreTick`: a `null -> value -> null -> value`
          // sequence across three switches must still re-fire `MessageList`'s seed effect on the third,
          // and reusing `restoreTick` (or gating a `tick` bump on the value actually changing) would
          // silently miss that.
          if (!view.atBottom && view.unseenAfterSeq !== null) {
            unseenSeedTick.current += 1;
            setUnseenSeed({ afterSeq: view.unseenAfterSeq, tick: unseenSeedTick.current });
          }
        }
        // P1 (ruling 26): a switch that lands on a tab already holding a card, while the panel has
        // the keys, puts them on it -- the same landing `Ctrl+l` gives within one tab. Read straight
        // off `payload.state` (not `timeline`, which still reflects the OLD tab's state until this
        // dispatch's `setState` above actually re-renders) and left to the `permissionRequest` effect
        // to find the exact row once it does.
        if (switchedRef.current) {
          switchedRef.current = false;
          // A tab this page has no saved view for -- a reload's (`prefix r`) or a fresh mount's
          // first snapshot, or a tab never shown before -- opens at the bottom (`MessageList`
          // follows from mount), so the cursor goes to the last row, where the view is. It sat on
          // row 1, off screen (the phase-3 GUI pass, 2026-09-25). The `[timeline]` effect does it,
          // once this snapshot's rows exist; P1's card landing below still wins over it.
          if (restoreRef.current === null || restoreRef.current.atBottom) landOnLastRef.current = true;
          if (paneFocusedRef.current && payload.state.pendingPermissions.length > 0) setPermissionRequest((n) => n + 1);
        }
        // A snapshot means a session is genuinely RUNNING, which is also when Rust clears its own
        // copy of the command. Clearing it when a start was merely requested would throw it away on
        // a start that then failed -- and on the legacy backend that is the last reference to a
        // conversation nothing else remembers.
        setHandoff(null);
        setCommandNotice(null);
        // A turn id first seen inside a SNAPSHOT (a page reload, or a resync mid-turn) gets
        // `exact: false` -- this panel cannot know how long it had already been running (design doc
        // §8.4). Untouched if the snapshot names the SAME turn already tracked (a resync must not
        // restart the clock); cleared if the snapshot carries no active turn at all.
        //
        // Since the phase-3 GUI pass (2026-09-25) the envelope can say when the turn started
        // (`turnStartedAtMs`, stamped by Rust's tab set within one 33ms tick of the turn's start):
        // then a switch back or a reload shows the real elapsed time, exactly. Only without it is the
        // reading "at least" (`0s+`) -- which a switch used to give for every running turn.
        const startedAt = payload.turnStartedAtMs;
        setTurnClock((current) => {
          const activeTurnId = payload.state.activeTurnId;
          if (activeTurnId === null) return null;
          if (current !== null && current.turnId === activeTurnId && (current.exact || typeof startedAt !== "number")) return current;
          if (typeof startedAt === "number") return { turnId: activeTurnId, since: startedAt, exact: true };
          return { turnId: activeTurnId, since: Date.now(), exact: false };
        });
      } else if (payload.kind === "events") {
        // A new turn resets the render trace: each turn reports its own first text, once.
        if (payload.events.some((e) => e.type === "turn_started")) {
          firstTextReceivedAt.current = null;
          renderReportSent.current = false;
        }
        // R1: a sent prompt takes the cursor once its row exists -- see the `[timeline]` effect,
        // which reads and clears this the moment the new row is in `timeline`.
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
              // exactly the tab-scoping bug this feature exists to rule out (ruling 3), so the
              // refusal is reported by name instead, with the full text so it is not lost.
              setCommandNotice(`A message to tab ${record.tab} was not sent (${payload.error}): ${record.text}`);
            }
          } else if (record?.kind === "editor") {
            // A scratch-editor round trip's own refusal (Task 8/15) is a footer nicety, not
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
        // "when done" (this component's own doc comment on `handoffOpen`): the session really is
        // closed by the time this arrives, so there is nothing left to confirm.
        setHandoffOpen(false);
        // Same reason `returnToStartScreen` below clears it: a whole-state reset must leave no
        // record of a turn behind. The next snapshot's `activeTurnId === null` branch usually
        // clears it incidentally, which is why this was missed -- but a snapshot arriving with an
        // `activeTurnId` EQUAL to the stale one would keep both the old `since` and the old
        // `exact: true`, rendering an inflated elapsed time with no `+` (design §8.4: "it never
        // shows a number it cannot stand behind"). Whether a turn id can repeat across sessions is
        // a question about ids this repo does not own -- legacy mints uuid v4, the sidecar's come
        // from Verdandi over the wire -- so this clears it rather than answering it.
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
        // Tab-scoped now (ruling 14): Rust sends this only when the failing tab is the active one,
        // and this tab's own `tabs.failure` carries the reason for when it is shown again. This
        // window's other tabs are untouched (spec §3.9).
        setHandoffRequests((current) => withoutHandoff(current, payload.tab));
        setState(initialState());
        setKeymapOpen(false);
        setHandoffOpen(false);
        // The third of the three whole-state resets, now saying the same thing as the other two.
        setTurnClock(null);
        setFatalError(payload.message);
        // No `requestHello()` here (ruling 17): Rust re-sends `hello`, recomputed, whenever the set
        // of open provider sessions changes -- this tab failing (or ending, or resetting) is exactly
        // such a change, so the picker's list is already on its way rather than something this side
        // has to go ask for.
      } else if (payload.kind === "begin_rename") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        pendingRef.current = null;
        clearSequence();
        // `prefix ,` (spec §3.5): the tab bar stays on screen (ruling 6) with an inline field open
        // over this tab, prefilled and selected. The two other overlays over the conversation area
        // must not fight it for the keys.
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        setRenaming({ tab: payload.tab, initial: payload.current ?? "" });
      } else if (payload.kind === "confirm_close") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        pendingRef.current = null;
        clearSequence();
        // `prefix &` (spec §3.4, ruling 7): drawn in the footer in place of the which-key strip,
        // and takes every key -- see `onKeyDown`'s dedicated branch, checked before every other
        // overlay. The other two overlays are closed for the same reason `begin_rename` closes them.
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        setConfirm({ kind: "close", tab: payload.tab, lines: payload.lines });
      } else if (payload.kind === "confirm_close_others") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        pendingRef.current = null;
        clearSequence();
        // `<leader>bo` (Owner answers Q2): the same overlay, window-level -- the other two overlays
        // over the conversation area must not fight it for the keys, same as `confirm_close` above.
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        setConfirm({ kind: "close_others", lines: payload.lines });
      } else if (payload.kind === "chooser") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        pendingRef.current = null;
        clearSequence();
        // `prefix w` (spec §3.6): the other two overlays over the conversation area must not fight
        // it for the keys, the same reason `begin_rename` and `confirm_close` close them.
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        setRenaming(null);
        setChooser({ open: payload.open, records: payload.records });
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

  /** V1 C1 (spec §3.5): the composer mirror. `browse`/`input` mean a live BROWSE/INPUT (this tab's
   *  own `mode`) or the empty tab's menu/composer (`emptyMode`) -- there is no overlay of any kind
   *  drawn over either; everything else, an ended session included, folds into `other`. Recomputed
   *  every render (a plain `const`, not its own `useState`), so the effect below -- which posts only
   *  when it actually changed -- always compares against this render's true value rather than a
   *  copy that could itself go stale. Declared after the `ready`-posting mount effect above, on
   *  purpose: React fires a component's own effects in source order, and `ready` must lead every
   *  other post (`App handshake`'s own test), including this mirror's first one.
   *  Fix round 1 (reviewer finding): before the first `tabs` envelope, `activeTab` is `null` and the
   *  render below shows only "Connecting to the shell…" -- `EmptyTab` is not mounted yet, so there
   *  is no box of any kind (`emptyMode`'s "input" default was never a real composer here). That case
   *  must fold into `other`, the same as every other screen with no box, or Rust would claim `Ctrl+k`
   *  for a composer that does not exist. */
  const panelKeysMode: PanelKeysMode =
    overlayOpen || keymapOpen || detail !== null || confirm !== null || pathPick !== null
      ? "other"
      : sessionStarted
        ? sessionEnded
          ? "other"
          : mode === "input"
            ? "input"
            : "browse"
        : activeTab === null
          ? "other"
          : emptyMode === "input"
            ? "input"
            : "browse";
  useEffect(() => {
    postToRust({ type: "panel_keys", request_id: nextRequestId(), mode: panelKeysMode });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [panelKeysMode]);
  /** V1 C1 (spec §3.5), "stale mirror, both ways": every `nav_key` this page cannot apply comes back
   *  as one `nav_fallthrough`, so Rust runs the chord's ordinary `move_focus` instead of dropping the
   *  key (Review Focus 2). `keymapOpen`/`confirm` are checked here rather than left to `EmptyTab`:
   *  they are this component's own state, drawn over its layout, and `EmptyTab` has no prop carrying
   *  either -- forwarding `navKey` to it unfiltered would let it act on a request this effect has
   *  already answered. `detail`/`pathPick` are live-tab-only by construction (neither ever renders
   *  while `!sessionStarted`), so they are read only inside that branch below. */
  useEffect(() => {
    if (navKey === null) return;
    const { direction } = navKey;
    if (overlayOpen || keymapOpen || confirm !== null) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    // Fix round 1 (reviewer finding): before the first `tabs` envelope there is no `EmptyTab` to
    // hand this to -- it is not mounted yet (the "Connecting to the shell…" div above is), so
    // queuing into `emptyNavKey` would only seed its `navKeySeenRef` at mount and silently drop the
    // key for good. Answer with a fallthrough instead, exactly as every other boxless screen does.
    if (activeTab === null) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    if (!sessionStarted) {
      setEmptyNavKey(navKey);
      return;
    }
    if (detail !== null || pathPick !== null || sessionEnded) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    if (direction === "down" && mode === "browse") {
      // C1a's own route (`i`/`o`): caret "kept", the same rule spec §3.1's row for `Ctrl+j` names
      // ("caret where it was left"). A bare `setMode("input")` would leave `Composer` reading
      // whatever `composerCaret` was last set to by an unrelated `A` press, not this chord's own
      // rule (fix round 1, reviewer finding: reproduced with a scratch test -- a caret left at 3
      // landed at the end of the draft instead, because the last `mode` action before this chord
      // had been `A`, not `i`/`o`).
      setMode("input");
      setComposerCaret("kept");
      setComposerFocusRequest((n) => n + 1);
      return;
    }
    if (direction === "up" && mode === "input") {
      setMode("browse");
      return;
    }
    // Stale: the mirror said browse/input but this render's `mode` no longer agrees (Review Focus 2).
    postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [navKey]);

  /** Clears every trace of a HINT session: labels, prefix, the frozen list and the session itself,
   *  so no later envelope for it does anything (spec §4, invariant 3). */
  function endHint() {
    clearHintPending();
    frozenRef.current = [];
    hintSessionRef.current = null;
    setHints([]);
    setHintTyped("");
  }

  /** Moves the keys to `target` and does nothing else -- HINT never acts (spec §4, invariant 1):
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
      // text box is what landing on one means (spec §2.4); nothing is typed or sent.
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

  /** Adds `request_id` and the active tab to a tab command, and posts nothing when there is no
   *  active tab yet -- there would be nothing to name it with (ruling 2: an inbound command without
   *  a tab is a protocol error, and this side must not manufacture the outbound mirror of that).
   *
   *  `edit_draft` is recorded as an `"editor"` in-flight request (Task 8/15) so its own refusal
   *  flashes in the footer rather than filling `commandNotice`'s banner. */
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
   *  tab's remembered mode (ruling 4) -- there is no separate `start_session` message any more. */
  function sendMessage(text: string) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    // The text is kept so a refusal can put it back. Dropped again as soon as the reply arrives.
    inFlight.current.set(requestId, { kind: "send", tab, text });
    setCommandNotice(null);
    postToRust({ type: "send_message", request_id: requestId, tab, text });
    // A send follows the reply, wherever the reader had scrolled (`./follow.ts`).
    resumeFollowing(containerRef.current?.querySelector(".message-list"));
  }

  /** Queues what is typed behind the running turn. Recorded as a `"send"` for the same reason
   *  `sendMessage` is: the composer cleared the box on Enter, and a refusal here (the session ended,
   *  or a handoff is pending, while this side still showed a turn running) means nothing was queued
   *  and nothing reached history -- Rust remembers a queued text only once it is queued -- so the
   *  box is the only place left to put it.
   *
   *  `send_now` is deliberately NOT recorded this way: its refusal can come after its text was
   *  already queued (an interrupt the backend refused, ruling 8), where putting it back would show it
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

  /** Commits (or cancels) an inline rename (spec §3.5). Both return the keys to the root, the same
   *  as every other overlay closing does. Sends `renaming.tab`, not the active tab: the field can
   *  stay open across a switch (`n`/`p`/digits/`l` move focus-free, ruling D3 B), so `post`'s
   *  `activeTabRef.current` would name the wrong tab -- the same reason `answerConfirm` sends a
   *  `"close"` confirm's own `confirm.tab` directly instead of going through `post` (a
   *  `"close_others"` confirm has no tab at all, and posts window-level like `TabVerb`). */
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

  /** `prefix w` (spec §3.6, ruling ordering: open tabs first, then records to resume). Every
   *  callback closes the overlay and returns the keys to the panel root -- D10's launch chooser is
   *  gone (wave 4 R2), so there is no `onLeave(true)` case that hands the keys to the editor
   *  instead any more. */
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
    // does, from the row's own label -- there is no round trip to Rust for this text (ruling 7).
    const label = chooser?.open.find((t) => t.tab === tab)?.label ?? `tab ${tab}`;
    setChooser(null);
    setConfirm({ kind: "close", tab, lines: [`close ${label}? (y/n)`] });
    returnKeysToRoot();
  }
  function onChooserLeave() {
    setChooser(null);
    // R16 (spec §4.1): over a live conversation, its own root, unchanged (`returnKeysToRoot`'s first
    // branch). Over an empty tab, the dashboard menu in BROWSE, never its composer in INPUT -- `Esc`
    // never lands INPUT anywhere else in this panel either (`resolveKey`'s own Escape rows say so).
    // Reuses the exact pair the `arrive` envelope already sends `EmptyTab` for every other keyboard
    // arrival (panel round 2, decision 4) rather than `returnKeysToRoot`'s `enter_input`-shaped
    // `setInputRequest`, which is what used to land this in INPUT instead.
    if (containerRef.current !== null) {
      containerRef.current.focus({ preventScroll: true });
      return;
    }
    setEmptyLanding("browse");
    setArriveRequest((n) => n + 1);
  }
  /** The chooser's own `New session` row (panel round 2 plan, Task 11; spec §6.1/§6.4): the same
   *  window-level `tab_verb new` the `tab.new` action already posts (`handleAction` above),
   *  reached from a second place. */
  function onChooserNewSession() {
    postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "new" });
    setChooser(null);
    returnKeysToRoot();
  }
  /** `Ctrl+r` on an open tab's own row (spec §6.1/§6.2: "the tab bar's rename field, moved into the
   *  row"): the chooser keeps its own local rename state (`Chooser.tsx`) rather than routing
   *  through `begin_rename`/`renaming` -- that pair is the tab bar's own overlay, which is not on
   *  screen while the chooser is. */
  function onChooserRenameTab(tab: TabId, name: string) {
    postToRust({ type: "rename_tab", request_id: nextRequestId(), tab, name });
  }
  /** `Shift+Tab` over `New session` or a record (spec §6.3, ruling R9): the active tab's own mode
   *  when it has not started yet -- a resume would land in it -- else the window's remembered
   *  default, since a resume then opens a new tab. Either way Rust re-sends `tabs` with the new
   *  value, so the chooser's mode line follows without being re-sent itself. */
  function onChooserCycleMode() {
    const info = activeTabInfo(tabs);
    if (info !== null && info.state === "not_started") post({ type: "cycle_mode" });
    else postToRust({ type: "cycle_default_mode", request_id: nextRequestId() });
  }

  /** The window-close prompt (ruling 7) owns every key ahead of everything else, in BOTH layouts:
   *  the conversation's own `onKeyDown` below, and `EmptyTab`'s root while the chat is on screen
   *  with no session yet (spec §3.4: "prefix & 先显示 chat 并给它键位"). Lifted out of either
   *  handler so both call it first and agree. Returns whether it claimed the key -- only
   *  `confirm === null` does not. */
  function answerConfirm(event: KeyboardEvent<HTMLDivElement>): boolean {
    if (confirm === null) return false;
    if (!["Shift", "Control", "Alt", "Meta"].includes(event.key)) {
      event.preventDefault();
      // V1 P7 (spec §8): every panel y/n accepts `y` and `Y`, like the window's own
      // `close_prompt.rs`. Any other key still cancels (tmux `confirm-before`).
      if (event.key === "y" || event.key === "Y") {
        if (confirm.kind === "close") {
          postToRust({ type: "close_tab", request_id: nextRequestId(), tab: confirm.tab });
        } else {
          postToRust({ type: "close_others", request_id: nextRequestId() });
        }
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
        // The `?` overlay owns every key while it is open, as it does over a conversation (the
        // live layout's `onKeyDown`); in capture, ahead of `EmptyTab`'s own handler and its
        // composer, which would otherwise take `m`, `j` or a typed letter under it.
        onKeyDownCapture={(event) => {
          if (!keymapOpen) return;
          event.preventDefault();
          event.stopPropagation();
          if (event.key === "?" || event.key === "Escape" || event.key === "q") setKeymapOpen(false);
          else if (event.key === "j" || event.key === "k") scrollKeymapOverlay(keymapOverlayRef.current, event.key === "j" ? 1 : -1);
        }}
      >
        {tabs !== null && showTabBar(tabs.tabs.length, renaming !== null) && (
          <TabBar
            tabs={tabs.tabs}
            active={tabs.active}
            renaming={renaming}
            focusRequest={tabBarFocusRequest}
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
            // V1 P11 (spec §10.1): the dashboard's own first-run hint line reads this live, rather
            // than assuming the stock default -- the same value the `?` overlay already shows as
            // `prefixLabel`.
            prefix={keymapHelp.prefix}
            focusRequest={inputRequest}
            arriveRequest={arriveRequest}
            // Wave 3 Task 1: `EmptyTab` bumps `Composer`'s focus itself and lands its own root on a
            // bare `keysRequest`, so the `prefix w` chooser drawn over an empty tab 1 and a
            // `keysRequest` bump both reach it directly rather than through `App`'s own
            // `containerRef`, which does not exist on this layout.
            keysRequest={emptyKeysRequest}
            overlayOpen={overlayOpen}
            landing={emptyLanding}
            onModeChange={setEmptyMode}
            // V1 C1 (spec §3.5): only a `navKey` the effect above found no overlay owning the keys
            // for -- this screen decides the rest itself (a starting/failed tab, its own menu vs.
            // composer), reporting back through `onNavFallthrough` when it cannot apply one either.
            navKeyRequest={emptyNavKey}
            onNavFallthrough={(direction) => postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction })}
            restoredDraft={restoredDraft}
            // Fix round 1 (panel round 2 plan Task 12+13, reviewer finding): the leader engine this
            // screen was missing entirely (spec §7, "Space starts a leader sequence"). `EmptyTab`
            // keeps its own local copy of the sequence/box state (it already keeps its own `mode`
            // separately from the live conversation's), reusing only the pure table and the same
            // `runPanelAction` the live conversation's `onKeyDown` calls.
            panelTable={panelTable}
            onPanelAction={runPanelAction}
            typingGuard={typingGuard}
            onFlash={showFlash}
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
            // The dashboard's `w` item (panel round 2 plan, Task 12; spec §7): the same window-level
            // `tab_verb choose` `runPanelAction`'s `tab.choose` case posts for `prefix w`.
            onChooseSessions={() => postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "choose" })}
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
        {/* No `.agent-ui-scroller` on this screen (ruling 7): the band, and the window-close
            prompt it draws while `prefix &` is open, sit directly under the empty tab instead of
            below a composer that lives inside `EmptyTab` itself. Panel round 2 (spec §5): the band
            replaces this screen's `Footer` the same way it replaces the conversation's; V1's editor
            context (`EmptyTab`'s own `ContextLine`, until Task 10) moves into its `context` fact. */}
        <StatusBand
          facts={{
            mode: emptyMode,
            pill: modePill(activeTab?.mode ?? "auto", true, true),
            showcmd: null,
            message: flash?.text ?? null,
            prompt: confirm !== null ? confirm.lines.join(" · ") : null,
            warn: null,
            unread: null,
            cards: 0,
            queued: 0,
            context: contextFact(editorContext),
            position: null,
            model: null,
          }}
          paneFocused={paneFocused}
        />
        {/* `prefix w`'s chooser can open over an empty tab 1 too -- `.agent-ui-root` is this layout's
            own positioned ancestor (it has no `.agent-ui-scroller` to nest inside). */}
        {chooser !== null && (
          <Chooser
            envelope={chooser}
            tabs={tabs?.tabs ?? []}
            active={tabs?.active ?? null}
            defaultMode={tabs?.defaultMode ?? "auto"}
            backend={hello?.backend ?? "legacy"}
            projectDir={hello?.projectDir ?? ""}
            newTabChord={keymapHelp.newTabChord}
            focusRequest={chooserFocusRequest}
            onSwitch={onChooserSwitch}
            onResume={onChooserResume}
            onNewSession={onChooserNewSession}
            onCloseTab={onChooserCloseTab}
            onRenameTab={onChooserRenameTab}
            onCycleMode={onChooserCycleMode}
            onLeave={onChooserLeave}
          />
        )}
        {/* Spec §7's `? Keys` (and `?` on an empty draft, `prefix ?`): drawn here too since the GUI
            pass (2026-09-26); `.agent-ui-root` is this layout's positioned ancestor, as for the
            chooser. */}
        {keymapOpen && (
          <KeymapOverlay
            ref={keymapOverlayRef}
            onClose={() => setKeymapOpen(false)}
            windowKeys={keymapHelp.window}
            prefixKeys={keymapHelp.prefixKeys}
            prefixLabel={keymapHelp.prefix}
            panel={panelTable}
          />
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

  /** v1 S1/S4 (spec §2.1-§2.2): `a`/`d`/`D` on `target` -- the card's own button or reason box, or
   *  `null` when there is no card for the cursor (F13's flash). The answer runs `TYPING_GUARD_MS`
   *  later, only if the key stood alone before and nothing cancels the wait; by then the card must
   *  still be on screen (a resolved card's row is gone) and the panel still in BROWSE. */
  function waitThenAnswer(event: KeyboardEvent<HTMLDivElement>, typedAt: number, target: HTMLElement | null, run: () => void) {
    if (target === null) {
      showFlash(NO_CARD_FLASH);
      return;
    }
    const waiting = typingGuard.defer(typedAt, event.repeat, () => {
      if (target.isConnected && modeRef.current === "browse") run();
    });
    if (!waiting) showFlash(TYPING_FLASH);
  }
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
    // overlay and everything else below (ruling 7): tmux's own `confirm-before` prompt takes the
    // keys the same way.
    if (answerConfirm(event)) return;
    // v1 S1 (spec §2.1): every other keydown is "a key" to the typing guard, ahead of the overlays
    // below so a key typed into one still counts. It cancels a waiting `a`/`d`/`D` -- which says
    // so in the band -- and this key still does whatever it does. Shift+Tab as WebKitGTK delivers
    // it (`isShiftTab`) is the walk key `Tab`, like the standard shape.
    const typedAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
    if (typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typedAt)) showFlash(TYPING_FLASH);
    // N2: a `gf` with several paths waits here for its letter, ahead of the overlays below (the same
    // reason the close prompt is first) -- `Esc`, or anything else, simply cancels it rather than
    // falling through to whatever that key would otherwise do, since a stray `j`/`k` landing on a
    // path instead of moving the cursor would be a surprise.
    if (pathPick !== null) {
      event.preventDefault();
      const index = HINT_ALPHABET.indexOf(event.key);
      if (index !== -1 && index < pathPick.length) openPath(pathPick[index]);
      setPathPick(null);
      return;
    }
    // Panel round 2 (spec §5.4): `Esc` closes `ContinueInTerminal`'s confirmation, the same way it
    // closes every other small overlay here. Deliberately not an early return for every key, unlike
    // `pathPick` above: the confirm/cancel buttons stay reachable by Tab and by the activatable-
    // control bypass further down, which this must not fight for keys it does not need.
    if (handoffOpen && event.key === "Escape") {
      event.preventDefault();
      setHandoffOpen(false);
      return;
    }
    const root = containerRef.current;
    const pending = pendingRef.current;
    pendingRef.current = null;
    const count = countRef.current;
    countRef.current = null;
    // The box `pendingRef`'s own reserved prefix (`g`/`z`/`[`/`]`) armed is hidden here, ahead of
    // every early return below, the same way `pending` above is read then reset unconditionally --
    // `resolveKey`'s `"pending"` case below re-arms both if THIS key turns out to continue it. Only
    // while no leader/table sequence is running: that one manages its own box entirely through
    // `advanceSequence`'s "cancel" result below (never here), since clearing it here, before the
    // leader engine gets to read `seqRef.current`, would drop every key of a sequence past the
    // first (found running this task's own tests: a plain `clearSequence()` here read `seqRef` as
    // already `null` on the second key of `<leader>bd`).
    if (seqRef.current === null) {
      cancelBoxTimer();
      hideBox();
    }
    // The `?` keymap overlay owns every key while it is open (spec §3.1), ahead of the editable-
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
    // below (spec §3.3: "j/k move, y copies a line, Esc/q close"). Panel round 2 (plan Task 10):
    // its trailing `Continue in a terminal…` row is one more stop past every fact row, so `j`/`k`
    // walk `[0, detail.length]` now rather than `[0, detail.length - 1]`, and `Enter` on that last
    // stop opens `ContinueInTerminal`'s confirmation the same way the row's own click does.
    if (detail !== null) {
      event.preventDefault();
      if (event.key === "Escape" || event.key === "q") setDetail(null);
      else if (event.key === "j") setDetailCursor((c) => Math.min(c + 1, detail.length));
      else if (event.key === "k") setDetailCursor((c) => Math.max(c - 1, 0));
      else if (event.key === "y") void navigator.clipboard?.writeText(detail[detailCursor]?.value ?? "");
      else if (event.key === "Enter" && detailCursor === detail.length) {
        setDetail(null);
        setHandoffOpen(true);
      }
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
    // keyboard route to Approve (found by the owner on an installed build). Every OTHER key still
    // reaches the table, which is what lets `h`/`j`/`k`/`l` carry on from a focused button.
    // v1 S5 (spec §2.3): on a card's own answer buttons (Approve, Deny, Always allow) Space is
    // claimed and does nothing -- not the button, not the leader -- and Enter answers only through
    // S1's check (`mayAnswerNow`: no key within the window before it, bar an unbroken run of
    // `h`/`l`/Tab that walked onto the button from a key standing alone). Claude Code's permission
    // prompt confirms with Enter; HTML's Space-activates is the default this overrides, only where
    // a button answers a permission.
    // Every other button (Stop, Dismiss, a tab, the chooser's rows) keeps both natively. A Space
    // that arrives inside a pending leader sequence is the sequence's (spec §12, R25), so it falls
    // through to the engine below -- claimed here first either way, so it never reaches the button.
    const onAnswerButton =
      isActivatableControl(event.target) && (event.target as HTMLElement).closest("[data-nav-action]") !== null;
    if (onAnswerButton && event.key === " ") {
      event.preventDefault();
      if (seqRef.current === null) return;
    }
    if (onAnswerButton && event.key === "Enter" && !typingGuard.mayAnswerNow("Enter", typedAt, event.repeat)) {
      event.preventDefault();
      showFlash(TYPING_FLASH);
      return;
    }
    // The v1-ui GUI pass (2026-09-27): the activity line's Stop keeps Enter and Space (spec §2.3),
    // but only on a key that stands alone or ends a quick motion (`j` onto it, then Enter). `j` from
    // the last row lands here, so "just do it" typed after an arrival interrupted the turn with its
    // first Space, denying the waiting card with it -- the thing S1 exists to stop.
    const onStop = isActivatableControl(event.target) && (event.target as HTMLElement).matches(".activity-line .stop");
    if (
      onStop &&
      seqRef.current === null &&
      (event.key === "Enter" || event.key === " ") &&
      !typingGuard.mayActAfterMotion(typedAt, event.repeat)
    ) {
      event.preventDefault();
      showFlash(STOP_TYPING_FLASH);
      return;
    }
    if (isActivatableControl(event.target) && (event.key === "Enter" || (event.key === " " && !onAnswerButton))) return;
    // The leader engine (`./leader`, panel round 2 plan Task 8; spec §2.4): a pending sequence owns
    // the next key, and only BROWSE ever starts one. An IME key is never a sequence key (Review
    // Focus 2, C4) -- checked here rather than folded into `startSequence`/`advanceSequence`
    // themselves, since neither of those takes an event to read `isComposing` off. A `{kind:"none"}`
    // start falls through to `resolveKey` below, never the other way; `startSequence` itself leaves
    // `g`/`z`/`[`/`]` to `resolveKey`'s own reserved two-key prefixes (`./leader`'s own doc comment).
    if (mode === "browse" && !isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode })) {
      if (seqRef.current !== null) {
        event.preventDefault();
        applySeqStep(advanceSequence(panelTable, seqRef.current.typed, event.key));
        return;
      }
      if (!event.ctrlKey && !event.altKey) {
        const start = startSequence(panelTable, event.key, isActivatableControl(event.target));
        if (start.kind !== "none") {
          event.preventDefault();
          // The v1-ui GUI pass (2026-09-27): the leader starts a sequence only on a key that stands
          // alone or ends a quick motion -- in the middle of typed prose it is swallowed and says
          // so ("set up my" ran `<leader>m`, "the boy" `<leader>bo` and answered it with its `y`).
          // Only the leader: a single-key table binding (`H`/`L`) runs as before.
          if (event.key === panelTable.leader && !typingGuard.mayActAfterMotion(typedAt, event.repeat)) {
            showFlash(leaderTypingFlash(panelTable.leaderLabel));
            return;
          }
          applySeqStep(start);
          return;
        }
      }
    }
    const action = resolveKey(mode, event.nativeEvent as unknown as KeyLike, {
      sessionEnded,
      pending,
      count,
      turnRunning: turnInProgress,
      table: panelTable,
    });
    if (action === null) return;
    // R4: a digit accumulates into the count the NEXT `j`/`k`/`[[`/`]]` repeats -- read back out of
    // `countRef` (as `count`, above) by that key, once it arrives. Handled first, and returns at
    // once, so a bare digit never falls into the scroll-announcing or row-motion code below it.
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
      // `j`/`k` are row motions. If a control inside the row had the keys (after `l` onto a card's
      // Approve), scrolling the row could carry that control off screen while it still answered
      // Enter -- found by the scrolling change's own fix round and left open there. So a scroll
      // step hands the keys back to the row first, the same place `j`/`k` leave them after a move.
      if (root !== null && document.activeElement !== root && root.contains(document.activeElement)) {
        root.focus({ preventScroll: true });
      }
      return;
    }
    if (action.kind === "move" || action.kind === "control" || action.kind === "answer") {
      event.preventDefault();
      if (root === null) return;
      if (action.kind === "move") {
        // R4's count repeats the step `times` times, each from the row the PREVIOUS step landed on
        // -- not `times` cells in one leap, so a stop with no row (a banner, the status line) still
        // ends the walk exactly where `nextStop` says it must, same as a single `j`/`k` would.
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
        // C1c (spec §3.4): `j` that cannot move -- `nextStop` clamps rather than returning `null` at
        // a boundary (`clampStep`'s own doc comment), so "the last stop" is the stop the keys are
        // already on (the last row, or Stop while a turn runs), not `target === null` (that case is
        // an EMPTY conversation, handled above and unrelated). Flashes once per press: a fresh `j`
        // there, or -- the v1-ui GUI pass (2026-09-27) found a held `j` reached the bottom in silence,
        // since every step after the first is a repeat -- the first repeat that stops after moving.
        // Never again on the repeats after that. `k` at the FIRST stop stays silent, as vim's own `k`
        // on the first line does (`:h j`) -- only `j` names a route the reader might actually want next.
        if (action.delta === 1) {
          if (target === currentStop(root, cursor)) {
            if (!event.repeat || heldMoveRef.current) showFlash("i or Ctrl+j to type");
            heldMoveRef.current = false;
          } else {
            heldMoveRef.current = true;
          }
        }
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
      } else {
        // `a`/`d` press the card's own button, so its guard against a second answer applies here
        // too. Not from a banner's button: the row cursor is hollow there, and not what keys act on,
        // so there is no card for it (F13's flash). v1 S4 (spec §2.2): only the cursor's card or the
        // card gating its tool call (`permissionTarget`) -- ruling 26's "the only card, from any row"
        // is gone.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        const rows = root.querySelectorAll<HTMLElement>('[data-nav-stop="row"]');
        const button =
          target === null
            ? null
            : (rows[target]?.querySelector<HTMLButtonElement>(`[data-nav-action="${action.decision}"]`) ?? null);
        waitThenAnswer(event, typedAt, button, () => button?.click());
      }
      return;
    }
    event.preventDefault();
    switch (action.kind) {
      case "mode":
        setMode(action.to);
        // C1a: entering INPUT (`i`/`o`/`A`) asks `Composer` to place the caret itself, rather than
        // leaving it to whatever a bare `autoFocus` on a freshly re-mounted, non-empty box happens
        // to land on (F12). Leaving BROWSE (`to === "browse"`) carries no caret to place.
        if (action.to === "input") {
          setComposerCaret(action.caret);
          setComposerFocusRequest((n) => n + 1);
        }
        break;
      case "esc-blocked":
        // R34: `Esc` never interrupts (D1) -- this is the reminder, not the action.
        showFlash("Esc does not interrupt — ctrl+c does");
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
        // R3: `Ctrl+o`. A per-tab toggle, mirrored to Rust in `saveView`/restored on a switch --
        // see `detailed`'s own doc comment.
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
        if (root === null) break;
        // v1 S4 and S1 (spec §2.1-§2.2): the same card `a`/`d` would answer, and the same wait.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        const rows = root.querySelectorAll<HTMLElement>('[data-nav-stop="row"]');
        const box = target === null ? null : (rows[target]?.querySelector<HTMLInputElement>(".permission-card input") ?? null);
        waitThenAnswer(event, typedAt, box, () => {
          if (box === null) return;
          // Re-read where the card is now: rows may have arrived above it during the wait.
          const row = box.closest<HTMLElement>('[data-nav-stop="row"]');
          const index = row === null ? null : rowIndexOf(root, row);
          if (index !== null && index !== cursorRef.current) setCursor(index);
          landedControlRef.current = box;
          box.focus();
        });
        break;
      }
      case "restart":
        // `r` on an ended tab resets it to `NotStarted` IN PLACE (ruling 12): the tab keeps its
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
        // The box shows what any of the four -- `g`/`z`/`[`/`]` -- can start once it has waited
        // `WHICH_KEY_DELAY_MS` with nothing completing it (panel round 2 plan, Task 8; spec §2.3
        // widened to §2.4's own delay, replacing the strip's former `g`-only 400ms line).
        // `clearSequence` at the top of this function on every later key -- the second `g` of `gg`
        // included -- is what cancels it before it fires.
        scheduleBoxTimer();
        break;
      case "panel":
        // The second key of a reserved two-key prefix (`[b`) that also completes a panel-table
        // sequence (`resolveKey`'s own `ctx.pending` branch, Task 7) -- everything else a table
        // sequence can run reaches here through the leader engine above instead.
        runPanelAction(action.binding);
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
        // R4's `[[`/`]]`, repeated `times` times, each from the prompt the PREVIOUS step landed on --
        // stopping early (rather than wrapping) the moment there is no earlier/later prompt at all.
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
        // D1/N1/D5: `Ctrl+c` while a turn runs, from anywhere in BROWSE -- the same `interrupt()`
        // the activity line's own Stop button calls.
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
        // N2 (ruling 19): a tool's own path field, or a path found in prose -- never a guess at what
        // the model meant, only what `pathsIn` (`../paths.ts`) can actually read off the row.
        const item = timeline[cursor];
        const paths = item === undefined ? [] : pathsIn(item).slice(0, HINT_ALPHABET.length);
        if (paths.length === 0) showFlash("no path on this row");
        else if (paths.length === 1) openPath(paths[0]);
        else setPathPick(paths);
        break;
      }
      case "view-in-editor": {
        // R3: the row's whole text, uncut, in a read-only nvim scratch buffer.
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
  /* Spec §10.2 (P11) names "an error row" as a classifier site beside the failed tab's own: a
     first-turn failure the sidecar reports (the not-logged-in row, whose real shape was never
     captured, §10.3) ends a live session here rather than failing the tab, so the reason gets its
     headline and remedy above the raw text too. `null` for any unrecognised reason: exactly what
     this showed before (whole-branch review of v1-ui). */
  const sessionEndedProblem =
    state.status.kind === "unavailable" || state.status.kind === "closed"
      ? classify(state.status.reason, hello?.account ?? null)
      : null;
  const sessionEndedBanner =
    state.status.kind === "unavailable" ? (
      <Row kind="error" sign="✗" role="alert" problem={sessionEndedProblem}>
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
      <Row kind="ended" sign="·" problem={sessionEndedProblem}>
        This session has ended ({state.status.reason}).
        <div className="row-hint">Press r to start a new session here.</div>
      </Row>
    ) : null;

  /** What the which-key box draws, or `null` while nothing is pending -- gated by `boxShown` in
   *  the JSX below, so a stale `pendingRef.current` left over after its own timer already fired is
   *  never rendered on its own: every place that abandons a fixed prefix without completing it
   *  (`onKeyDown`'s own top, ahead of the leader engine) resets `boxShown` in the same tick. A
   *  leader/table sequence (`seq`) and a reserved two-key prefix (`pendingRef`) can never both be
   *  set at once -- starting either clears the other -- so this checks `seq` first with no priority
   *  to decide between them. */
  const box: { title: string; entries: BoxEntry[] } | null =
    seq !== null
      ? { title: sequenceTitle(panelTable, seq.typed), entries: boxEntries(panelTable, seq.typed, modeFixed) }
      : pendingRef.current !== null
        ? {
            title: pendingRef.current,
            entries: [...FIXED_PENDING_ENTRIES[pendingRef.current], ...boxEntries(panelTable, [pendingRef.current], modeFixed)],
          }
        : null;

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
          focusRequest={tabBarFocusRequest}
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
          `.agent-ui-scroller` in index.css). The tab bar (Task 11) goes above `errorBanner`. */}
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
          onUnreadChange={(label, jump, afterSeq) => {
            setUnread({ label, jump });
            unseenAfterSeqRef.current = afterSeq;
          }}
          unseenSeed={unseenSeed}
        />
        {keymapOpen && (
          <KeymapOverlay
            ref={keymapOverlayRef}
            onClose={() => setKeymapOpen(false)}
            windowKeys={keymapHelp.window}
            prefixKeys={keymapHelp.prefixKeys}
            prefixLabel={keymapHelp.prefix}
            panel={panelTable}
          />
        )}
        {detail !== null && (
          <DetailPopover
            ref={detailRef}
            rows={detail}
            current={detailCursor}
            onClose={() => setDetail(null)}
            onHandoff={() => {
              setDetail(null);
              setHandoffOpen(true);
            }}
          />
        )}
        {chooser !== null && (
          <Chooser
            envelope={chooser}
            tabs={tabs?.tabs ?? []}
            active={tabs?.active ?? null}
            defaultMode={tabs?.defaultMode ?? "auto"}
            backend={hello?.backend ?? "legacy"}
            projectDir={hello?.projectDir ?? ""}
            newTabChord={keymapHelp.newTabChord}
            focusRequest={chooserFocusRequest}
            onSwitch={onChooserSwitch}
            onResume={onChooserResume}
            onNewSession={onChooserNewSession}
            onCloseTab={onChooserCloseTab}
            onRenameTab={onChooserRenameTab}
            onCycleMode={onChooserCycleMode}
            onLeave={onChooserLeave}
          />
        )}
        {/* The which-key box (panel round 2 plan, Task 8): `WHICH_KEY_DELAY_MS` after a leader/table
            sequence, or one of `resolveKey`'s own reserved `g`/`z`/`[`/`]` prefixes, is still
            pending. Positioned against `.agent-ui-scroller`, the same wrapper the two overlays
            above are (its own doc comment) -- `bottom: 0` is the footer's own top rule. `box` is
            `null` whenever neither kind of pending state names one (nothing pending, or `boxShown`
            is stale from a keystroke this render has not yet reset -- `clearSequence` always runs
            before either kind is set, so this can only ever under-render, never show a mismatch). */}
        {boxShown && box !== null && (
          <WhichKeyBox
            title={box.title}
            entries={box.entries}
            onPick={(key) => {
              // The same event the sequence engine would have read had this key been typed next --
              // dispatched onto the conversation root so `onKeyDown` decides it exactly once, the
              // same route a real keystroke takes (`./leader`'s own `BoxEntry.key` doc comment: it
              // is already the raw key, never the humanized label, except at the leader's own node,
              // which a click can never reach since the leader is only ever `typed[0]`).
              const root = containerRef.current;
              root?.focus({ preventScroll: true });
              root?.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
            }}
          />
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
        mode={mode}
      />
      <QueueLines items={queue} error={queueError} />
      {/* Above the composer, deliberately (panel round 2 plan, Task 10; spec §5.4) -- the confirmation
          used to sit below everything, at the very bottom; the band leaves no room there for a
          multi-line dialog, so it moves up here instead, controlled by `handoffOpen`. It must not
          compete with the lost-session banner for the space directly above the box, which is why it
          sits below that banner and the activity/queue lines rather than at the very top. Offered
          for a session that has ENDED too -- continuing a conversation that died here is arguably the
          case where a terminal helps most -- so the only conversation-state term is whether a turn
          is running. `canResume` comes from the provider's advertised capability, never the
          backend's name. */}
      <ContinueInTerminal
        open={handoffOpen}
        onClose={() => setHandoffOpen(false)}
        providerSessionId={state.providerSessionId}
        turnInProgress={turnInProgress}
        canResume={state.capabilities.resume}
        handingOff={handingOff}
        onHandoff={handoffToTerminal}
      />
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
        // Wave 3 Task 1: the raw `inputRequest` used to reach `Composer` directly here, so its own
        // `[focusRequest, mode]` effect (focusing the textarea whenever `mode === "input"`) had no
        // way to know an overlay was drawn over it. `composerFocusRequest` is bumped from the
        // `inputRequest` effect's pass branch above (which already gates on `overlayOpen`) and from
        // `resolveKey`'s own `i`/`o`/`A` (C1a, the `"mode"` case above).
        focusRequest={composerFocusRequest}
        caretOnFocus={composerCaret}
        // Only while the box can take a message: a landing on a disabled textarea could not focus it.
        hintTarget={!(sessionEnded || handingOff)}
        onModeChange={setMode}
        onSend={sendMessage}
        onDraftChange={mirrorDraft}
        running={turnInProgress}
        onQueue={queueMessage}
        onSendNow={(t) => {
          post({ type: "send_now", text: t });
          resumeFollowing(containerRef.current?.querySelector(".message-list"));
        }}
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
      />
      {/* N2/R4: `gf` with several paths, and the open `/` prompt, each still own every key
          (`onKeyDown`'s dedicated branches, above `resolveKey` entirely) -- only WHERE they draw
          moved, once `Footer`'s third slot stopped existing (panel round 2 plan, Task 10). */}
      {pathPick !== null && <PathPick paths={pathPick} />}
      {search !== null && (
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
      )}
      {/* The bottom band (panel round 2 plan, Task 10; spec §5): replaces `StatusRow`, `Footer`,
          `NewPill` and `ContextLine` with one vim-statusline row. `showcmd` reads the same `box`
          the which-key popup draws from -- vim shows `showcmd` immediately, without that popup's
          own `WHICH_KEY_DELAY_MS` wait (spec §5.2's row for it, `nvim: 'showcmdloc'`), and `box` is
          already computed unconditionally above for exactly that reason. `prompt` is the window-
          close prompt (ruling 7) -- the same y/n `confirm` the old footer drew in its third slot,
          now taking the whole band right of the mode section (spec §5.3.4) rather than one slot
          among several. Backend has no segment here at all (decision 6): `prefix i` only. */}
      <StatusBand
        facts={{
          mode,
          pill: modePill(activeTab?.mode ?? "auto", state.capabilities.modeSwitch === true, true),
          showcmd: box !== null ? `${box.title}…` : null,
          message: flash?.text ?? null,
          prompt: confirm !== null ? confirm.lines.join(" · ") : null,
          warn: statusWarning(state.provider, activeTab?.failure ?? null),
          unread: unread.label,
          cards: state.pendingPermissions.length,
          queued: queue.length,
          context: sessionEnded ? null : contextFact(editorContext),
          position: timeline.length === 0 ? null : `${cursor + 1}/${timeline.length}`,
          model: shortModel(state.model),
        }}
        paneFocused={paneFocused}
        onOpenDetail={() => post({ type: "open_detail" })}
        onJump={unread.jump}
      />
      <HintLayer root={containerRef.current} hints={hints} typed={hintTyped} />
    </div>
  );
}
