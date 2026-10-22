import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent, SetStateAction } from "react";
import { ProjectDirContext } from "./projectPath";
import { initialPageModel, pageModel, withoutHint } from "./page_model";
import type { CommandRecord, PageModel } from "./page_model";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import type { OutboundMessage, PanelKeysMode, PermissionDecision } from "./bridge";
import { noteUserScroll, resumeFollowing } from "./follow";
import {
  cancelListScroll,
  computedLineHeight,
  decideLanding,
  decidePress,
  isListScrollAnimating,
  readBoxState,
  readRowGeometry,
  rowTextElement,
  scrollListTo,
  settleListScroll,
  STEP_LINES,
} from "./jkScroll";
import { cancelHold, isHolding, keepHoldAlive, releaseHold, startHold } from "./jkHold";
import { isPlainAnswerKey, panelEntryKey, pluginKeysHelp, resolveKey } from "./keymap";
import type { KeyLike, PanelBinding, PanelMode, PendingPrefix, PluginKeysHelp } from "./keymap";
import { advanceSequence, boxEntries, FIXED_PENDING_ENTRIES, sequenceTitle, startSequence, WHICH_KEY_DELAY_MS } from "./leader";
import type { BoxEntry, SeqStep } from "./leader";
import { isImeKey } from "./composerKeys";
import { bypassYesCounts, isModeCycleKey, isShiftTab, modeFixedMessage, modeKeyRoute } from "./modeKey";
import { hintTypingFlash, isModifierKey, leaderTypingFlash, tableKeyTypingFlash, TYPE_HINT_FLASH, TYPING_GUARD_MS, TypingGuard } from "./typingGuard";
import {
  compareCarets,
  copySelectionText,
  entrySelectableCaret,
  firstSelectableCaret,
  rebuildSelection,
  repeatMotion,
  revealCaret,
  selectionMatchesBuild,
  swapEnds,
} from "./visual";
import type { BuiltSelection, Caret, SelectionLike, VisualModel } from "./visual";
import { appendQuote, formatQuote } from "./quote";
import { installCtrlBracketAsEscape } from "./ctrlBracket";
import { installHeldSuperTracking } from "./heldSuper";
import { WhichKeyBox } from "./components/WhichKeyBox";
import { TurnChanges } from "./components/TurnChanges";
import { buildTimeline, oldestPendingPermission, oldestWaitingPermission, promptIndex, waitingCardAfter } from "./timeline";
import { buildDisplay, indexOfKey, runKeyOf } from "./display";
import { outputText, primaryText } from "./copyText";
import { codeBlockText, renderedText } from "./markdown";
import { countCodePoints } from "./toolRegistry";
import { findMatch } from "./search";
import { SearchBar } from "./components/SearchBar";
import {
  controlsOf,
  conversationRows,
  countedStop,
  currentStop,
  hintTargets,
  isActivatableControl,
  linkClickNeedsLook,
  linkOpensAtOnce,
  nextControl,
  permissionTarget,
  rowIndexOf,
  rowOf,
  stopOf,
  webLinks,
  HINT_ALPHABET,
} from "./nav";
import type { ShownLink } from "./nav";
import type { AnswerableItem, HintTarget } from "./nav";
import type { TimelineItem } from "./timeline";
import { pathsIn, viewText } from "./paths";
import type { PathRef } from "./paths";
import { PathPick } from "./components/PathPick";
import { LinkPick } from "./components/LinkPick";
import { isExternalTool, agentTitle } from "./externalTools";
import { acceptsEnvelope, activeTabInfo, agentWord, countedTabTarget, forgetClosed, isExternalAgent, saveView, tabPermissionModes, takeView } from "./tabs";
import type { TabViewState } from "./tabs";
import { EmptyTab } from "./components/EmptyTab";
import type { NavKeyRequest } from "./components/EmptyTab";
import { Composer } from "./components/Composer";
import type { RestoredDraft } from "./components/Composer";
import { TabBar } from "./components/TabBar";
import { MessageList } from "./components/MessageList";
import { cardApprovable } from "./components/PermissionCard";
import { Row } from "./components/Row";
import { classify, remedyNamesR } from "./problems";
import { ActivityLine } from "./components/ActivityLine";
import { StatusBand } from "./components/StatusBand";
import { QueueLines } from "./components/QueueLines";
import { heldCount } from "./queueLines";
import { DetailPopover } from "./components/DetailPopover";
import { ContinueInTerminal } from "./components/TerminalHandoff";
import { HintLayer } from "./components/HintLayer";
import type { ShownHint } from "./components/HintLayer";
import { KeymapOverlay } from "./components/KeymapOverlay";
import { ReviewOverlay } from "./components/ReviewOverlay";
import { TrustPrompt } from "./components/TrustPrompt";
import {
  parseExCommand,
  resolveTrustKey,
  scrollTarget,
  TRUST_BAND_PROMPT,
  TRUST_BAND_PROMPT_N_ONLY,
  TRUST_WAIT_FLASH,
  trustFooter,
} from "./trust";
import type { TrustKeyEvent } from "./trust";
import {
  applyReviewKey,
  boxUnderCursor,
  openReview,
  resolveReviewKey,
  scrollBoxFirst,
  typeComment,
} from "./review";
import type { ReviewEffect } from "./review";
import { THINKING_DISPLAYS } from "./types";
import { Chooser } from "./components/Chooser";
import { SlashPicker } from "./components/SlashPicker";
import type { SlashPickerKind } from "./components/SlashPicker";
import { PanelErrorBoundary } from "./components/PanelErrorBoundary";
import { barePickerCommand, completionSource } from "./slashCommands";
import { parseEffortReply, parseModelReply } from "./slashPicker";
import { DRAFT_MIRROR_DELAY_MS, modePill, REFUSED_ECHO_MS, showTabBar } from "./tabs";
import { cardSummary, shortModel, usageSegment } from "./band";
import { latestTurnEnding } from "./turnEnding";
import type { TurnDurations } from "./turnTimes";
import type { ApproveFact, BandFacts } from "./band";
import type { AgentUiState, DetailRow, TabId, TabsEnvelope, ContextSummary, ProviderInfo } from "./types";
import { applyTheme } from "./theme";
import { applyEditorTyping } from "./typingCadence";

/** The line `agent/src/providers/claude_sidecar/spawn.rs::describe_checkout` writes for version skew.
 *  Moved in from the deleted `statusRow.ts`: the band's `warn` fact
 *  is this module's logic now, since `BandFacts` itself stays decoupled from `ProviderInfo`. */
export const SKEW_PREFIX = "Verdandi baseline drift";

/** Title text for `⚠` on version skew or a refusal, or `null` when neither applies. Routine
 *  diagnostics, including the CLI version, appear only in the detail popover. */
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

/** Pixel step for one `j`/`k` scroll inside a tool result's overflow box.
 *  The value is intended to feel like a row step and has not been checked on screen. */
const TOOL_RESULT_SCROLL_STEP_PX = 40;

/** The longest this panel swallows keys after `f` asked `shell` for a HINT, if that HINT never ends
 *  here (see `hintPendingRef`). Long enough for a GTK main loop busy with a frame and the 33ms pump; short
 *  enough that a `shell` which never answers (the HINT was refused, or this page is not hosted by
 *  `shell` at all) costs a second of dead keys, not a stuck panel -- the same stance as `shell`'s own
 *  300ms wait for this panel's answer. */
export const HINT_PENDING_TIMEOUT_MS = 1000;

/** The maximum digit-prefix count before a motion repeats. The cap bounds repeated DOM work
 *  even for an arbitrarily long digit string. Motion loops also stop as soon as a repeat makes
 *  no progress; the count cap provides a second bound. */
export const MAX_MOTION_COUNT = 9999;

/** Fold one typed digit into a count, capped at `MAX_MOTION_COUNT` on every step so even a held
 *  digit key cannot accumulate a value above the cap. Kept pure so the cap is testable without
 *  a DOM; `onKeyDown` calls it for the "count" action. */
export function accumulateMotionCount(count: number | null, digit: number): number {
  return Math.min(MAX_MOTION_COUNT, (count ?? 0) * 10 + digit);
}

/** The row `gg`/`G` lands on: first or last without a count, otherwise row N starting at 1 and
 *  clamped to the available rows. A shared pure function keeps the scroll announcement and jump
 *  in agreement and makes the clamp testable without a DOM. */
export function jumpTarget(to: "first" | "last", count: number | null, rows: number): number {
  const last = Math.max(rows - 1, 0);
  if (count === null) return to === "first" ? 0 : last;
  return Math.max(0, Math.min(count, rows) - 1);
}

/** The band message when `a`/`d`/`D`, or Enter on a card button, is refused because another key
 *  came within `TYPING_GUARD_MS` (`./typingGuard`). It asks the user to pause because even
 *  `Esc` followed quickly by `a` is refused, without any prose being typed. */
const TYPING_FLASH = "a / d answer a card only on their own — pause, then press again; i or Ctrl+j to type";
/** The band message when `a`/`d`/`D`, or Enter/Space on a card button, follows a count such as
 *  `3a`. Card answers take no count, so the key answers nothing. */
const COUNT_ANSWER_FLASH = "a / d / D take no count — press it on its own";
/** The band message when Enter on a card button is refused because a modifier is held. This
 *  includes Shift, which `isPlainAnswerKey` permits for `D`. */
const MODIFIED_ENTER_FLASH = "Enter with a modifier answers no card";
/** The band message when Enter on a card button is refused because focus did not just land
 *  there through `l`/`h`, Tab, or a HINT. This prevents Enter after intervening text from
 *  approving the card natively. */
const ENTER_LANDING_FLASH = "Enter answers a card right after l, h or Tab onto its button";
/** Message shown when Enter lands on a card other than the cursor's `permissionTarget`.
 *  Tab can move between cards' buttons while the row cursor stays on the earlier card. */
const ENTER_ELSEWHERE_FLASH = "Enter answers the card under the cursor — j / k onto it first";
/** The band message when `a`/`d`/`D` have no card under the cursor or gating the tool call
 *  under it, and no card waits elsewhere. This gives feedback instead of silently doing
 *  nothing. */
const NO_CARD_FLASH = "no card here — i, o, A or Ctrl+j to type";
/** The refusal message when a card is waiting elsewhere. It points back to the card, like the
 *  activity line's "j to it", rather than only suggesting typing. */
const CARD_ELSEWHERE_FLASH = "a / d answer the card under the cursor — j / k onto it, then a / d";
/** Message shown when `]p`/`[p` finds no card waiting for an answer in this tab.
 *  Cards may be absent, already answered here, or inert because the session ended. */
const NO_WAITING_CARD_FLASH = "no card waiting here";
/** The v1-ui GUI pass (2026-09-27): what the band says when Enter or Space on the activity line's
 *  Stop did not interrupt because it came in the middle of typing (`TypingGuard.mayActAfterMotion`)
 *  -- "just do it" typed after an arrival reached Stop with its `j` and interrupted the turn. */
const STOP_TYPING_FLASH = "Stop takes a key only on its own — i or Ctrl+j to type";
/** A design decision: what the band says when INPUT's `Ctrl+y` came within `TYPING_GUARD_MS` of
 *  another key (readline's `Ctrl+u`/`Ctrl+w` then `Ctrl+y` yank, typing right after it, a held
 *  key's repeat) and approved nothing. */
const CTRL_Y_TYPING_FLASH = "Ctrl+y approves only on its own — pause, then press it again";
/** Message shown when INPUT's `Ctrl+y` arrives before the target card has been stable
 *  for `TYPING_GUARD_MS`, including a new card, a replacement or the next card after an approval. */
const CTRL_Y_TARGET_FLASH = "Ctrl+y: the waiting card just changed — read it, then press again";
/** Message shown when INPUT's `Ctrl+y` follows a composer kill key.
 *  After `Ctrl+u`, `Ctrl+w` or `Ctrl+k`, it is a yank regardless of the pause. */
const CTRL_Y_YANK_FLASH = "Ctrl+y right after Ctrl+u, Ctrl+w or Ctrl+k is a yank, not an answer — press it again";
/** What the band says when `a` or INPUT's `Ctrl+y` would approve a card whose Approve is disabled: an ACP card
 *  whose agent offered no allow-once option, or one Eitri held back because the card cannot show the whole
 *  call. Nothing is sent; sending would only bring back the agent's refusal under a misleading reason. */
const NOT_APPROVABLE_FLASH = "this card cannot be approved — deny it, or Ctrl+c interrupts the turn";

/** Whether the target is the composer's textarea, where INPUT's `Ctrl+y` can answer.
 *  Search fields and card reason boxes do not qualify. */
function isComposerBox(el: EventTarget | null): el is HTMLTextAreaElement {
  return el instanceof HTMLTextAreaElement && el.closest(".composer") !== null;
}

/** Whether the key is an unmodified `Ctrl+u`, `Ctrl+w` or `Ctrl+k` in the composer.
 *  These kill keys make a following `Ctrl+y` a yank. */
function isKillKey(event: KeyboardEvent<HTMLElement>): boolean {
  return (
    isComposerBox(event.target) &&
    event.ctrlKey &&
    !event.shiftKey &&
    !event.altKey &&
    !event.metaKey &&
    ["u", "w", "k"].includes(event.key.toLowerCase())
  );
}
/** What an open `/model`/`/effort` picker shows. */
type SlashPickerState = { kind: SlashPickerKind; options: string[]; current: string | null };

/** Envelopes that move keyboard focus or cover the conversation without a keydown
 *  cancel a waiting `a`/`d`/`D`. GTK handles `nav_key`'s Ctrl+j/Ctrl+k before the page sees a key.
 *  Shift+Tab is a real keydown, so it is handled outside this Set.
 *  `onModeKey` must call `typingGuard.onKey` directly: its capture handler stops propagation
 *  to prevent backward focus navigation, so the bubble handler cannot report it.
 *  Calling only `cancel()` would drop the pending answer without recording the key's timing. */
const CANCELS_WAITING_ANSWER = new Set(["pane_focus", "arrive", "enter_input", "focus_permission", "hint_collect", "nav_key"]);

/** One `TOOL_RESULT_SCROLL_STEP_PX` step of a tool result's capped box, or `false` when the box is
 *  already at the end of travel that way. Used by `scrollVisibleRowBox` (`Ctrl+e`/`Ctrl+y`); `j`/`k`
 *  step the same box by three of its own lines (`stepWithinRow`). */
function scrollBoxOneStep(box: HTMLElement, direction: 1 | -1): boolean {
  const atStart = box.scrollTop <= 0;
  const atEnd = box.scrollTop + box.clientHeight >= box.scrollHeight - 1;
  if (direction > 0 ? atEnd : atStart) return false;
  box.scrollTop += direction * TOOL_RESULT_SCROLL_STEP_PX;
  return true;
}

/** Ctrl+e/Ctrl+y scroll the cursor row's capped tool result first only while it is partly on
 *  screen and can move in that direction. Otherwise return `false` so the conversation scrolls.
 *  Unlike the `j`/`k` helper, this must not reveal an off-screen cursor row: doing so could
 *  pull the view back to the box repeatedly and prevent scrolling past it. */
function scrollVisibleRowBox(row: HTMLElement | null, direction: 1 | -1): boolean {
  const box = row?.querySelector<HTMLElement>(".tool-result-body") ?? null;
  const list = box?.closest(".message-list") ?? null;
  if (box === null || list === null) return false;
  const b = box.getBoundingClientRect();
  const l = list.getBoundingClientRect();
  if (b.bottom <= l.top || b.top >= l.bottom) return false;
  return scrollBoxOneStep(box, direction);
}

/** One step inside a row's own text, in whole pixels: `STEP_LINES` of the row's TEXT's computed line
 *  height (`rowTextElement`). Derived, not a pixel constant, so it follows the theme's font size and
 *  the display's scale. Used by the `?` overlay's own scrolling; the conversation's rows are stepped
 *  by `./jkScroll`'s `decidePress`, which measures the same line. */
function rowScrollStep(row: HTMLElement): number {
  return STEP_LINES * computedLineHeight(rowTextElement(row));
}

/** The sheet owns its scroll box: line motions share the conversation's text-sized step, while page
 *  motions use the visible sheet height so they still fit after resizing the window. */
function scrollKeymapOverlay(el: HTMLDivElement | null, key: string, ctrl: boolean) {
  if (el === null) return;
  if (ctrl) {
    if (key === "d") el.scrollTop += el.clientHeight / 2;
    else if (key === "u") el.scrollTop -= el.clientHeight / 2;
    else if (key === "f") el.scrollTop += el.clientHeight;
    else if (key === "b") el.scrollTop -= el.clientHeight;
  } else {
    if (key === "j" || key === "ArrowDown") el.scrollTop += rowScrollStep(el);
    else if (key === "k" || key === "ArrowUp") el.scrollTop -= rowScrollStep(el);
    else if (key === "PageDown" || key === " ") el.scrollTop += el.clientHeight;
    else if (key === "PageUp") el.scrollTop -= el.clientHeight;
    else if (key === "gg") el.scrollTop = 0;
    else if (key === "G") el.scrollTop = Math.max(0, el.scrollHeight - el.clientHeight);
  }
}

/** Brings `row` on screen in `list` after the cursor has landed on it, by computing the `scrollTop`
 *  itself (`./jkScroll`'s `decideLanding`) rather than asking the engine's `scrollIntoView`: WebKitGTK
 *  2.52.6's `nearest` top-aligns any row taller than the view, which turned `k` into a row above into a
 *  whole-view jump. `direction` is the key that moved the cursor (`1` for `j`, `-1` for `k`) or `0` for
 *  any other way, which judges from where the row lies. `animate` eases the move out over 150ms.
 *
 *  With no layout to judge -- a panel that is not laid out yet, or jsdom -- it falls back to
 *  `scrollIntoView`, which then does nothing a reader could see anyway. */
function revealRow(list: HTMLElement | null, row: HTMLElement, direction: 1 | -1 | 0, animate = false) {
  const geometry = list === null ? null : readRowGeometry(list, row);
  if (list === null || geometry === null) {
    row.scrollIntoView({ block: "nearest" });
    return;
  }
  const landing = decideLanding({ direction, ...geometry });
  if (landing.how !== "stay") scrollListTo(list, landing.scrollTop, animate);
}

/** A tool result's box is entered at the end nearest the reader -- its start going down, its end going
 *  up -- so it never carries the position it was last read to. Called when `j`/`k` land on the row. */
function enterBoxAtNearEnd(row: HTMLElement, direction: 1 | -1) {
  const box = row.querySelector<HTMLElement>(".tool-result-body");
  if (box === null) return;
  box.scrollTop = direction > 0 ? 0 : Math.max(0, box.scrollHeight - box.clientHeight);
}

/** What `j`/`k` do inside the cursor row before they move off it, up to `times` presses' worth (a count):
 *  bring the row back if it is out of view, step its tool result's box if that is on screen, step the
 *  conversation through the row while its end is past the margin -- `./jkScroll`'s `decidePress`, which
 *  has the rules. Returns how many presses it spent, `0` when the first has nothing to scroll and the
 *  cursor should simply move. A count stops spending itself the moment a press has nothing left to give,
 *  and whatever remains moves rows.
 *
 *  Only a lone press eases; a held key's repeats and a count scroll at once, so a run of them is not a
 *  run of animations restarting. Nothing here measures without a layout (`readRowGeometry`), so a panel
 *  not yet laid out, or jsdom, spends nothing and the cursor moves. */
function stepWithinRow(row: HTMLElement | null, direction: 1 | -1, times: number, animate: boolean): number {
  const list = row?.closest<HTMLElement>(".message-list") ?? null;
  if (row == null || list === null) return 0;
  const easeOut = animate && times === 1;
  let spent = 0;
  while (spent < times) {
    const geometry = readRowGeometry(list, row);
    if (geometry === null) break;
    const box = row.querySelector<HTMLElement>(".tool-result-body");
    const press = decidePress({
      direction,
      view: geometry.view,
      row: geometry.row,
      line: geometry.line,
      box: box === null ? null : readBoxState(list, box),
      boxLine: box === null ? geometry.line : computedLineHeight(box),
    });
    if (press.kind === "move") break;
    if (press.kind === "box") {
      if (box === null) break;
      const before = box.scrollTop;
      box.scrollTop = press.boxScrollTop;
      if (box.scrollTop === before) break;
    } else {
      const before = list.scrollTop;
      scrollListTo(list, press.scrollTop, easeOut);
      if (!easeOut && list.scrollTop === before) break;
    }
    spent++;
  }
  return spent;
}

/** The conversation rows inside `list` that are at least partly on screen. */
function visibleRows(list: HTMLElement): HTMLElement[] {
  const l = list.getBoundingClientRect();
  return conversationRows(list).filter((row) => {
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

/** The live inputs to `MessageList`, captured when VISUAL starts and held unchanged while it
 *  remains active. Only `cursor`/`focused`/`visual` keep moving; the snapshot is `null` outside
 *  VISUAL/V-LINE. */
type FrozenSnapshot = {
  state: AgentUiState;
  expanded: Record<string, boolean>;
  detailed: boolean;
  ruleOffers: Record<string, string>;
  answeredPermissions: ReadonlySet<string>;
  sessionEnded: boolean;
  turnDurations: TurnDurations;
};

/** The tool name of the oldest pending permission, selected by lowest `seq`, for
 *  `ActivityLine`'s "needs approval" slot. Returns `null` when nothing waits. */
function oldestPendingTool(state: AgentUiState): string | null {
  let oldest: (typeof state.pendingPermissions)[number] | null = null;
  for (const request of state.pendingPermissions) {
    if (oldest === null || request.seq < oldest.seq) oldest = request;
  }
  return oldest?.toolName ?? null;
}

/** #22: where a reader was when the keys left the panel (`pane_focus` false), for the `arrive` that
 *  brings them back. One slot beside `viewStore`, never in it: an arrival must neither consume nor
 *  overwrite the view a tab switch saved, and a park dies with a switch (`tabs` arm). `following` is
 *  "at the bottom AND on the last row" -- in a conversation that fits the view `atBottom` alone is
 *  always true, and would send a reader who put the cursor on row 1 back to the last row. `atKey`:
 *  `typingGuard.keyCount()` at the leave, so any panel key since then makes the park stale. */
type ArrivalPark = { tab: TabId; view: TabViewState; cursorKey: string | null; following: boolean; atKey: number };

/** The scroll half of putting a saved view back: `follow`
 *  re-arms following outright, as a send does; otherwise the write is announced as the user's, so
 *  `MessageList` does not fight it, and the list goes back to where it was. */
function applyViewScroll(list: HTMLElement, view: TabViewState, follow: boolean, stop = false) {
  // A restored view replaces whatever was easing, even one that began at the very position this writes.
  cancelListScroll(list);
  if (follow) {
    resumeFollowing(list);
    return;
  }
  // `stop` restores a view with following disabled if it was disabled when saved. A card
  // resolving above the reader can shrink the list, clamp `scrollTop` to the bottom, and make
  // `MessageList` resume following while away. Without stopping it again, the next streamed row
  // would move the restored view and cursor. "up" stops following, as `k`/`gg` do.
  noteUserScroll(list, stop ? "up" : "unknown");
  list.scrollTop = view.scrollTop;
}

/** What `<leader>k` says when the agent left nothing running (Rust's `NOTHING_IN_THE_BACKGROUND`). */
const NOTHING_IN_THE_BACKGROUND = "nothing is running in the background";

/** `<leader>k`'s question: what is stopped, by whom it was left running, and what stopping it costs. */
function stopBackgroundQuestion(who: string, running: number): string {
  const what = running === 1 ? "the command" : `the ${running} commands`;
  return `Stop ${what} ${who} left running? This ends the session; the conversation can be resumed. y/n`;
}

export default function App() {
  /** Everything the envelope router decides (`./page_model`). `pageRef` is the latest model, written
   *  synchronously with every change, so envelopes later in the same dispatch batch -- and the handlers that
   *  read the model between renders -- never see the previous render's value. */
  const [page, setPage] = useState(initialPageModel);
  const pageRef = useRef(page);
  const setPageField = useCallback(<K extends keyof PageModel,>(field: K, value: SetStateAction<PageModel[K]>) => {
    const current = pageRef.current;
    const nextValue = typeof value === "function"
      ? (value as (previous: PageModel[K]) => PageModel[K])(current[field]) : value;
    if (Object.is(current[field], nextValue)) return;
    const next = { ...current, [field]: nextValue };
    pageRef.current = next;
    setPage(next);
  }, []);
  const {
    state, hello, tabs, fatalError, handoff, handoffRequests, commandNotice, turnClock, turnDurations, thinkingDisplay, paneFocused,
    queue, queueError, queueNote, queueSteers, history, ruleOffers, editorContext, editorLink, scratchEditing,
    review, reviewHints, reviewRecovery, reviewSettings, chooser,
    inputRequest, arriveRequest, permissionRequest, navKey, keymapHelp, heldTrust, confirm, renaming, queueTaken,
  } = page;
  const {
    setFatalError, setHandoffRequests, setCommandNotice, setReview, setReviewHints, setChooser,
    setInputRequest, setArriveRequest, setPermissionRequest, setHeldTrust, setConfirm, setRenaming,
  } = useMemo(() => ({
    setFatalError: (value: SetStateAction<PageModel["fatalError"]>) => setPageField("fatalError", value),
    setHandoffRequests: (value: SetStateAction<PageModel["handoffRequests"]>) => setPageField("handoffRequests", value),
    setCommandNotice: (value: SetStateAction<PageModel["commandNotice"]>) => setPageField("commandNotice", value),
    setReview: (value: SetStateAction<PageModel["review"]>) => setPageField("review", value),
    setReviewHints: (value: SetStateAction<PageModel["reviewHints"]>) => setPageField("reviewHints", value),
    setChooser: (value: SetStateAction<PageModel["chooser"]>) => setPageField("chooser", value),
    setInputRequest: (value: SetStateAction<PageModel["inputRequest"]>) => setPageField("inputRequest", value),
    setArriveRequest: (value: SetStateAction<PageModel["arriveRequest"]>) => setPageField("arriveRequest", value),
    setPermissionRequest: (value: SetStateAction<PageModel["permissionRequest"]>) => setPageField("permissionRequest", value),
    setHeldTrust: (value: SetStateAction<PageModel["heldTrust"]>) => setPageField("heldTrust", value),
    setConfirm: (value: SetStateAction<PageModel["confirm"]>) => setPageField("confirm", value),
    setRenaming: (value: SetStateAction<PageModel["renaming"]>) => setPageField("renaming", value),
  }), [setPageField]);
  /** Records an outgoing request in the model without a render: nothing draws `inFlight`, and a render of the
   *  whole panel per outgoing command would be pure cost. The next change of the model carries it along, since
   *  every change starts from `pageRef`. */
  function trackRequest(id: string, record: CommandRecord) {
    const current = pageRef.current;
    pageRef.current = { ...current, inFlight: new Map(current.inFlight).set(id, record) };
  }

  /** The active tab for the dispatch handler, which is installed once: set synchronously inside the handler, so
   *  the snapshot that follows a `tabs` envelope in the same batch is accepted (`tabs` from the render would
   *  still hold the previous render's value until React re-renders). */
  const activeTabRef = useRef<TabId | null>(null);
  /** What each tab's reader had on screen (cursor, expansions, scroll, draft), keyed by tab id and never sent to
   *  Rust. Saved just before a switch resets the render state, and handed back through `restoreRef` once the new
   *  tab's own `snapshot` arrives. */
  const viewStore = useRef(new Map<TabId, TabViewState>());
  /** The view to restore once the switch's `snapshot` is applied; read and cleared by the `snapshot` arm and the
   *  scroll-restoring layout effect. */
  const restoreRef = useRef<TabViewState | null>(null);
  /** See `ArrivalPark`. Taken by the `pane_focus` arm when the keys leave, used and cleared by the next
   *  `arrive`, dropped by anything that means the reader chose a place themselves while away (a wheel or touch on
   *  the list, a press in the panel) or by a route that lands elsewhere (a switch, `enter_input`,
   *  `focus_permission`). */
  const arrivalParkRef = useRef<ArrivalPark | null>(null);
  /** The typing guard's key count when an `arrive` or `focus_permission` envelope reached this page, held until
   *  the landing it asked for has run. Both landings happen in effects, so only after React has rendered the
   *  envelope; `shell` moves GTK focus here and sends the envelope at the same moment, so the user's first key can
   *  be handled in between, against the panel as it is drawn. A landing that ran over that key afterwards undid
   *  it -- the first `v` after `Ctrl+l` started CARET and the landing ended it again before it was ever painted,
   *  so only a second `v` seemed to work. A landing whose count no longer matches yields: the key that got there
   *  first already decided. */
  const landingAtKeyRef = useRef<number | null>(null);
  /** Set by the `tabs` arm whenever the active tab changes (the first `tabs` included), and read and cleared by
   *  the `snapshot` arm right after: landing on a tab already holding a card puts the keys on it, the same as
   *  `Ctrl+l` does within one tab. */
  const switchedRef = useRef(false);
  const activeTab = activeTabInfo(tabs);
  /** The conversation for handlers that run between renders; dispatch writes it with the model. */
  const stateRef = useRef(state);
  stateRef.current = state;
  /** True once the active tab actually holds a session, live or already ended: a `starting` tab, or a `failed`
   *  one with no kept conversation, renders the empty tab. Derived, not state -- Rust's `tabs` envelope is the
   *  only source of a tab's `state`. */
  const sessionStarted =
    activeTab !== null &&
    (activeTab.state === "live" ||
      activeTab.state === "ended" ||
      (activeTab.state === "failed" && page.keptStates.has(activeTab.id)));
  /** `sessionStarted` for the bridge listener, which is registered once and would otherwise read
   *  its first render's value forever. Assigned during render, so it is current by the time any
   *  later envelope arrives. */
  const sessionStartedRef = useRef(false);
  /** Whether `<leader>` `mode.cycle` and Shift+Tab flash rather than post, using
   *  `modeKeyRoute`. An ended or failed tab can still cycle out of bypass, but cannot cycle
   *  into it. */
  const modeFixed =
    modeKeyRoute({
      confirmOpen: false,
      chooserOpen: false,
      tabState: activeTab?.state ?? null,
      tabMode: activeTab?.mode ?? null,
    }) === "fixed";
  sessionStartedRef.current = sessionStarted;
  /** Text to put back in the composer after a refused send. See `RestoredDraft`. */
  const [restoredDraft, setRestoredDraft] = useState<RestoredDraft | null>(null);
  /** Monotonic, so two refusals of the same text are two distinct restores. */
  const restoreSeq = useRef(0);
  /** Bumped only when the `snapshot` arm actually applies a saved view's cursor and expansions. The
   *  scroll-restoring layout effect keys on THIS, not on `state`: `state` also changes on the switch's own reset,
   *  in the same render as `restoreRef.current` is first set, and a `[state]` dependency fired there too --
   *  before the new tab's real snapshot ever arrived -- consuming `restoreRef.current` on an empty transcript and
   *  leaving nothing for the real restore. The "keeps each tab's cursor across a switch" test fails with that
   *  dependency. */
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
  /** A held `j`/`k`'s glide (`./jkHold`): the last repeat's time on the keydowns' clock and on
   *  `performance.now()`'s, to date its release for the typing guard, and which way it goes. */
  const holdClockRef = useRef({ typed: 0, perf: 0 });
  const holdDirectionRef = useRef<1 | -1>(1);
  /** `paneFocused` for the `snapshot` dispatch arm, which cannot read render state directly -- the same reason
   *  `sessionStartedRef` exists. A switch landing on a card must check whether the keys are even in this window
   *  right now: landing them silently in a background window would be a surprise nobody asked for. */
  const paneFocusedRef = useRef(false);
  paneFocusedRef.current = paneFocused;
  /** What actually reaches `EmptyTab`'s own `navKeyRequest` prop: only a `navKey` the effect below
   *  found no overlay (`overlayOpen`, `?`, a y/n) already owning the keys for -- EmptyTab has no
   *  visibility into `keymapOpen`/`confirm` at all (they are this component's own state, drawn over
   *  its layout rather than passed down), so forwarding `navKey` itself unfiltered would let it act
   *  on a request that has already been answered with `nav_fallthrough` above it. */
  const [emptyNavKey, setEmptyNavKey] = useState<NavKeyRequest | null>(null);
  /** The empty tab's landing mode on mount or switch: the latest `enter_input` requests INPUT;
   *  `arrive` or a tab switch requests BROWSE. Launch starts in INPUT. */
  const [emptyLanding, setEmptyLanding] = useState<PanelMode>("input");
  /** Bumped by `dropPendingKeys()` so cancel routes also clear the empty tab's pending prefix
   *  and leader sequence through `EmptyTab`'s `dropKeysRequest`. A counter lets repeated
   *  requests act. */
  const [emptyDropKeys, setEmptyDropKeys] = useState(0);
  /** The empty tab's own mode, reported by `EmptyTab`, for the band drawn under it. */
  const [emptyMode, setEmptyMode] = useState<PanelMode>("input");
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
  /** Capture the current view from live refs for both tab switches and leaving the panel.
   *  `atBottom` allows one pixel of slack when comparing the viewport's end with `scrollHeight`. */
  function captureView(list: HTMLElement | null): TabViewState {
    const atBottom = list === null || list.scrollTop + list.clientHeight >= list.scrollHeight - 1;
    return {
      cursor: cursorRef.current,
      mode: modeRef.current,
      expanded: expandedRef.current,
      scrollTop: list?.scrollTop ?? 0,
      atBottom,
      detailed: detailedRef.current,
      // At the bottom, following is what a restore should resume (see the `snapshot` arm's restore
      // block), so no threshold is worth remembering -- and `null` there matches `MessageList`'s own
      // "while following" reset.
      unseenAfterSeq: atBottom ? null : unseenAfterSeqRef.current,
    };
  }
  /** The VISUAL/V-LINE model between keys: both caret ends, the linewise flag, and the goal
   *  column. Also holds the last built native selection for checking that the selection still
   *  matches. Both are `null` outside VISUAL/V-LINE; `mode` indicates whether selection mode is
   *  active. */
  const visualModelRef = useRef<VisualModel | null>(null);
  const visualBuiltRef = useRef<BuiltSelection | null>(null);
  /** The digit count for VISUAL/V-LINE, applied to the next motion and reset by it. Kept
   *  separate from `countRef` because the capture handler stops region keys before BROWSE's
   *  `onKeyDown` can run. Sharing a ref could leave a stale count in the other mode. */
  const visualCountRef = useRef<number | null>(null);
  /** The region's pending `g` for `gg` and the reserved `gv`. Kept separate from BROWSE's
   *  `pendingRef` because the capture handler stops region keys before `onKeyDown` runs.
   *  Carries `PendingPrefix`'s "g" through `resolveKey`'s `ctx.pending`; "z"/"["/"]" prefixes
   *  remain BROWSE-only. */
  const regionPendingGRef = useRef<PendingPrefix | null>(null);
  /** The frozen props passed to `MessageList` while VISUAL is active (`FrozenSnapshot`). Kept
   *  in state because rendering `MessageList` depends on them. */
  const [frozenSnapshot, setFrozenSnapshot] = useState<FrozenSnapshot | null>(null);
  /** `frozenSnapshot` from the last render, indicating whether the displayed DOM is frozen.
   *  Cursor reveal, scroll clamping, and `focus_permission` reveal must not use a live timeline
   *  index against frozen rows: arriving prompts or cards can make those indexes differ.
   *  Assigned during render like `cursorRefForScroll`, so `exitRegion` requesting
   *  `setFrozenSnapshot(null)` does not clear this until the thawed list renders. */
  const frozenSnapshotRef = useRef<FrozenSnapshot | null>(null);
  frozenSnapshotRef.current = frozenSnapshot;
  /** A `focus_permission` reveal waiting for the thawed list (see its layout effect, below the
   *  `permissionRequest` effect). */
  const revealAfterThawRef = useRef<number | null>(null);
  /** The composer's own unsent text, kept only so a tab switch can save it -- never
   *  read to render anything itself; `restoredDraft` is what actually reaches `Composer`. Updated
   *  from `Composer`/`EmptyTab`'s `onDraftChange`, which fires on every keystroke and once more
   *  with `""` after a send. */
  const draftRef = useRef("");
  /** v1 hardening: the cards this tab has answered from this panel, by `permissionId`,
   *  whichever way -- a card's own button or `a`/`d`. This is the card's old `answered` guard lifted
   *  out of `PermissionCard`, because `a`/`d` no longer press the card's button (a DOM query for it
   *  could find a button a model reply drew): they call `answerPermission` with the card's id, and
   *  the double-answer protection has to hold on that path too. `answeredRef` is the same set,
   *  current between renders, for the check `answerPermission` makes and for a wait `a`/`d` are
   *  still in. Tab-scoped like `ruleOffers`: reset on a switch, and an id leaves it once its card
   *  is no longer pending (the effect after `answerableItems`). */
  const [answeredPermissions, setAnsweredPermissions] = useState<ReadonlySet<string>>(() => new Set());
  const answeredRef = useRef<ReadonlySet<string>>(answeredPermissions);
  /** The card INPUT's `Ctrl+y` would approve, as `<tab>:<permission id>`, and whether it has
   *  been the target for `TYPING_GUARD_MS`. Updated by the layout effect after `ctrlYTargetKey`. */
  const ctrlYTargetRef = useRef<{ key: string | null; settled: boolean }>({ key: null, settled: false });
  /** Whether the last non-modifier key was a kill in the composer (`isKillKey`), so the next
   *  `Ctrl+y` yanks text regardless of the pause. */
  const lastKeyWasKillRef = useRef(false);
  /** Whether the composer's textarea has focus, as required by the band's `Ctrl+y` segment and
   *  `approveOldestByKey`. Tracked through `focusin`/`focusout`. */
  const [composerBoxFocused, setComposerBoxFocused] = useState(false);
  useEffect(() => {
    const onIn = (event: FocusEvent) => setComposerBoxFocused(isComposerBox(event.target));
    const onOut = (event: FocusEvent) => setComposerBoxFocused(isComposerBox(event.relatedTarget));
    document.addEventListener("focusin", onIn);
    document.addEventListener("focusout", onOut);
    setComposerBoxFocused(isComposerBox(document.activeElement));
    return () => {
      document.removeEventListener("focusin", onIn);
      document.removeEventListener("focusout", onOut);
    };
  }, []);
  /** The reason typed so far into each pending card's box, reported by the card as it changes, so
   *  `d` sends it exactly as the card's own Deny does. A ref, not state: typing in the
   *  box must not re-render the whole panel. */
  const permissionReasons = useRef(new Map<string, string>());
  // Companion mode: where the panel stands with the editor beside it. `null` is the one-window mode,
  // where Rust never sends one.
  /** The unread pill is reported by `MessageList` through `onUnreadChange` and drawn in the
   *  band. `jump` starts as a no-op so a click before the first `updatePill` is safe. Only the
   *  label is state: reports arrive on every scroll and update, and storing each fresh `jump`
   *  closure in state would render the whole panel on every scroll frame. Keep the latest
   *  closure in a ref. */
  const [unreadLabel, setUnreadLabelState] = useState<string | null>(null);
  /** The label last set, compared before setting: a state setter handed its current value still
   *  renders the panel once more when it is called from a layout effect, as `MessageList`'s is on
   *  every update. */
  const unreadLabelRef = useRef<string | null>(null);
  const setUnreadLabel = (label: string | null) => {
    if (label === unreadLabelRef.current) return;
    unreadLabelRef.current = label;
    setUnreadLabelState(label);
  };
  const unreadJumpRef = useRef<() => void>(() => {});
  /** The unread pill's current `seq` threshold, mirrored from `MessageList`'s `onUnreadChange` third
   *  argument -- read (not `state`) when a switch saves the departing tab's view, so
   *  a row that arrives while a tab is away is still counted once the reader comes back to it. A ref,
   *  not state: nothing here ever renders on it, the same reason `cursorRef`/`modeRef`/etc. above are
   *  refs. */
  const unseenAfterSeqRef = useRef<number | null>(null);
  /** What to seed `MessageList`'s own threshold with on a restore: `tick` changes on
   *  every restore that has something to seed, so a second switch back to the same threshold still
   *  re-fires `MessageList`'s seed effect -- see that prop's own doc comment. `null` between restores
   *  and for a tab that was left following or never had a saved threshold. */
  const [unseenSeed, setUnseenSeed] = useState<{ afterSeq: number; tick: number } | null>(null);
  const unseenSeedTick = useRef(0);
  /** The footer's transient line. `seq` so the same text twice is two flashes. */
  const [flash, setFlash] = useState<{ text: string; seq: number } | null>(null);
  const flashSeq = useRef(0);
  /** The reply link whose address the last click showed instead of opening (`linkClickNeedsLook`). */
  const shownLinkRef = useRef<ShownLink | null>(null);
  function showFlash(text: string) {
    flashSeq.current += 1;
    setFlash({ text, seq: flashSeq.current });
  }
  // Track Super from its own keydown/keyup because WebKitGTK omits it from other keys' events.
  // `isPlainAnswerKey` uses it for `a`/`d`/`D` and bypass `y`. This first effect installs window
  // capture before HINT can stop keys. It also maps `Ctrl+[` to Escape in document capture, ahead
  // of `onModeKey` and the other handlers. HINT's window capture still runs first while pending.
  useEffect(() => {
    const removeHeldSuper = installHeldSuperTracking(document, window);
    const removeCtrlBracket = installCtrlBracketAsEscape(document);
    return () => {
      removeHeldSuper();
      removeCtrlBracket();
    };
  }, []);
  // A newer flash replaces an older one (`showFlash` above); this only ever clears the flash that
  // is STILL the current one when its own two seconds are up, so an older flash's timer firing late
  // cannot erase a newer flash that has since taken its place.
  useEffect(() => {
    if (flash === null) return;
    const seq = flash.seq;
    const timer = setTimeout(() => setFlash((f) => (f?.seq === seq ? null : f)), 2000);
    return () => clearTimeout(timer);
  }, [flash]);
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
  /** A refused send this page has put back into the box itself (`tab`, its `requestId`, and when). The refusal
   *  of a first message put off by the trust question is followed by Rust's `draft` naming that send
   *  (`refused`), its copy of the box with the message in front; that echo must not replace a box that already
   *  holds the message and what was typed after it. Forgotten once the mirror it goes with is delivered. */
  const refusedEchoRef = useRef<{ tab: TabId; requestId: string; at: number } | null>(null);
  const draftTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** The last copy of the box Rust gave this page, and for which tab: a `late` draft replaces the box only
   *  while it still holds exactly this, since anything typed since is newer than the late copy. */
  const givenDraftRef = useRef<{ tab: TabId; text: string } | null>(null);
  function flushDraft() {
    refusedEchoRef.current = null;
    if (draftTimerRef.current !== null) clearTimeout(draftTimerRef.current);
    draftTimerRef.current = null;
    const pending = pendingDraftRef.current;
    pendingDraftRef.current = null;
    if (pending !== null) postToRust({ type: "draft", request_id: nextRequestId(), tab: pending.tab, text: pending.text });
  }
  /** Whether a `draft` for `tab` is the echo of the refusal just recorded: Rust names the refused send in it
   *  (`refused`), which only the trust question's put-off does. Taken once; an echo that never comes expires
   *  (`REFUSED_ECHO_MS`, or when the mirror it follows goes out). */
  function takeRefusedEcho(tab: TabId, refused: string | undefined): boolean {
    const echo = refusedEchoRef.current;
    refusedEchoRef.current = null;
    return (
      echo !== null &&
      refused !== undefined &&
      echo.tab === tab &&
      echo.requestId === refused &&
      performance.now() - echo.at < REFUSED_ECHO_MS
    );
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
  /** Whether the view eases to the next landing (`revealRow`'s `animate`): set only by a lone `j`/`k`,
   *  read and reset by the cursor-follow effect with `landingRef`. */
  const landingAnimateRef = useRef(false);
  /** C1c: whether the last `j` moved, so the first repeat of a held `j` that then stops at the last
   *  stop still flashes once (the v1-ui GUI pass, 2026-09-27) -- see the move branch of `onKeyDown`. */
  const heldMoveRef = useRef(false);
  /** The open `/` prompt and the cursor position when it opened; `lastSearchRef` holds what `n`/`N` repeat. */
  const [search, setSearch] = useState<{ query: string; origin: number } | null>(null);
  const lastSearchRef = useRef("");
  /** The open `:` command line's text, or `null` while closed. It runs no command, so keys such as
   *  `:ls` land in a text box instead of on a card. It closes wherever the `/` prompt closes,
   *  including when an overlay returns focus to the root. Opening either line closes the other. */
  const [exLine, setExLine] = useState<string | null>(null);
  /** Bumped by every `/`, `:` and `panel.search` request and passed to `SearchBar` as `focusRequest`.
   *  Repeating a line's key restores its focus after a click elsewhere, instead of just clearing it.
   *  A newly opened line takes focus on mount. Only one line is open at a time, so both share this
   *  counter. */
  const [lineFocusRequest, setLineFocusRequest] = useState(0);
  function moveCursorTo(index: number) {
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    noteUserScroll(list, index < cursor ? "up" : "down");
    setCursor(index);
  }
  /** The first key of a two-key BROWSE sequence (`g`, `z`, `[`, `]` or `Ctrl+w`), or `null`.
   *  Cleared on every key `onKeyDown` sees, so only the matching second key completes it.
   *  Also cleared on every `pane_focus` envelope to cancel across pane switches handled by GTK.
   *  Passed to `resolveKey` in its context so the key table keeps no memory. */
  const pendingRef = useRef<PendingPrefix | null>(null);
  /** The count starts with `1`-`9` and continues with `0`-`9`. It applies to the next
   *  `j`/`k`/`[[`/`]]`/`G`/`gg`/`gt`/`gT`; a prefix passes it to its second key. Any completed
   *  action resets it. `null` means no digits yet. A ref lets `onKeyDown` read it without
   *  rendering it. */
  const countRef = useRef<number | null>(null);
  /** A leader/table sequence in progress (`./leader`'s `startSequence`/`advanceSequence`), or
   *  `null` between sequences -- the engine's OWN pending state, distinct from `pendingRef` above,
   *  which is `resolveKey`'s five reserved two-key prefixes (`g`/`z`/`[`/`]`/`Ctrl+w`) and predates this
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
   *  key -- armed only while `panelTable.timeout` is true and the pending node both has
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
   *  that -- never runs an action. Added to every place `pendingRef`
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
  /** Cancels a pending prefix, count and leader sequence together on pane arrivals, overlays,
   *  tab switches and Shift+Tab. Also clears the empty tab's local prefix and sequence.
   *  Uses only refs and state setters, so listeners installed once can call it safely. */
  function dropPendingKeys() {
    pendingRef.current = null;
    countRef.current = null;
    clearSequence();
    setEmptyDropKeys((n) => n + 1);
    // A `gx` picker waiting for a letter is pending input too. Clear it on cancellation so the next
    // typed letter cannot open a stale link.
    setLinkPick(null);
  }
  useEffect(() => {
    cancelBoxTimer();
    cancelSeqTimeoutTimer();
  }, []);
  /** One step of the sequence engine: `run` clears the sequence and performs the binding;
   *  `pending` records the new node and, only if the box is not already showing, (re)arms its
   *  `WHICH_KEY_DELAY_MS` timer -- plus, when this node is itself a binding (ambiguous) AND
   *  `panelTable.timeout` is on, a `panelTable.timeoutlen` timer that runs the ambiguous binding
   *  with no further key; `cancel` clears with nothing run. */
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
  /** Runs a table binding reached through the leader engine or a reserved-prefix pair such as `[b`.
   *  `tab.*` sends the window-level `tab_verb` directly through `postToRust`.
   *  `post()` would add a `tab` field the message does not accept; the tab under the keys determines the target. */
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
        // The `/` prompt replaces an open `:` line so only one command line owns the keys.
        setExLine(null);
        setSearch({ query: "", origin: cursor });
        setLineFocusRequest((n) => n + 1);
        break;
      case "panel.keymap":
        keymapOpening.current = null;
        keymapClosedByKey.current = false;
        setKeymapOpen(true);
        break;
      case "panel.handoff": {
        // Only a Claude session can be taken into a terminal `claude`; another agent's tab says so
        // instead of opening a confirmation that could not be carried out.
        const other = agentWord(activeTabInfo(tabsRef.current)?.agent);
        if (other !== null) {
          showFlash(`Continuing a ${other} session in a terminal is not available yet`);
          break;
        }
        // `<leader>t`: opens `ContinueInTerminal`'s confirmation directly, since the
        // band leaves no permanently visible button for a click to reach first any more. Opening
        // unconditionally is deliberate -- if the handoff is blocked (no session id yet, a turn
        // running), that component shows why instead of the confirm dialog, rather than this key
        // doing nothing with no explanation.
        setHandoffOpen(true);
        break;
      }
      case "tab.stop-background": {
        // Only a live tab whose agent counts what it left running has anything to stop; a turn that runs
        // is stopped by an interrupt, which takes what it left with it.
        const tab = activeTab?.id ?? null;
        if (tab === null || activeTab?.state !== "live" || state.backgroundCalls === 0) {
          showFlash(NOTHING_IN_THE_BACKGROUND);
          break;
        }
        if (state.activeTurnId !== null) {
          showFlash("a turn is running: interrupting the turn stops them too");
          break;
        }
        askStopBackground(tab, state.backgroundCalls);
        break;
      }
      case "mode.cycle":
        // Mode cycling is disabled for fixed tabs and flashes instead of posting. `modeKeyRoute`
        // also routes Shift+Tab. All routes except an ended or failed tab that is not already in
        // bypass post the request. Rust asks for confirmation before entering bypass.
        switch (
          modeKeyRoute({
            confirmOpen: false,
            chooserOpen: false,
            tabState: activeTab?.state ?? null,
            tabMode: activeTab?.mode ?? null,
          })
        ) {
          case "cycle":
            post({ type: "cycle_mode" });
            break;
          case "fixed":
            // "r" (not `keymapHelp.newTabChord`): reaching "fixed" means the tab has already ended
            // or failed, and `r` -- not a rebindable panel-table action -- is what restarts it in
            // place on both layouts (`EmptyTab`'s own `r`, BROWSE's `restart` case).
            showFlash(modeFixedMessage("r"));
            break;
        }
        break;
    }
  }
  /** Whether the `?` keymap overlay is open. Toggled by `resolveKey`'s `{kind:"keymap"}`
   *  (opening only -- see `onKeyDown`'s dedicated branch below for why closing never goes through
   *  `resolveKey` at all) and by the handful of places that must force it shut: the session ending,
   *  the start screen coming back, and a HINT starting elsewhere in the window. */
  const [keymapOpen, setKeymapOpen] = useState(false);
  /** The number `prefix ?` gave the opening that is showing (`open_keymap`'s `opening`), or `null` when the
   *  panel's own `?` opened it: only the former reports its close to Rust, which may give the keys back. */
  const keymapOpening = useRef<number | null>(null);
  /** Whether the showing opening is being closed by the overlay's own keys or a click outside it. Every
   *  other close leaves it false, so a forced close (`:`, a review, a new session) gives nothing back. */
  const keymapClosedByKey = useRef(false);
  function closeKeymapByKey() {
    keymapClosedByKey.current = true;
    setKeymapOpen(false);
  }
  useEffect(() => {
    if (keymapOpen) return;
    const opening = keymapOpening.current;
    if (opening !== null) {
      postToRust({ type: "keymap_closed", request_id: nextRequestId(), opening, return_keys: keymapClosedByKey.current });
    }
    keymapOpening.current = null;
    keymapClosedByKey.current = false;
  }, [keymapOpen]);
  const keymapPendingG = useRef(false);
  useEffect(() => { keymapPendingG.current = false; }, [keymapOpen]);

  function onKeymapKey(event: KeyboardEvent) {
    event.preventDefault();
    event.stopPropagation();
    if (isModifierKey(event.key)) return;
    const pendingG = keymapPendingG.current;
    keymapPendingG.current = false;
    if (event.altKey || event.metaKey || event.nativeEvent.isComposing) return;
    if (event.ctrlKey && event.shiftKey) return;
    if (!event.ctrlKey && (event.key === "?" || event.key === "Escape" || event.key === "q")) {
      closeKeymapByKey();
    } else if (!event.ctrlKey && event.key === "g") {
      if (pendingG) scrollKeymapOverlay(keymapOverlayRef.current, "gg", false);
      else keymapPendingG.current = true;
    } else {
      scrollKeymapOverlay(keymapOverlayRef.current, event.key, event.ctrlKey);
    }
  }
  /** The plugin page's section `prefix ?` sent with `open_keymap`, drawn first while the overlay is open. It
   *  belongs to that one opening: once the overlay closes it is dropped, so the panel's own `?` later shows no
   *  page's keys. */
  const [keymapPlugin, setKeymapPlugin] = useState<PluginKeysHelp | null>(null);
  useEffect(() => {
    if (!keymapOpen) setKeymapPlugin(null);
  }, [keymapOpen]);
  /** The review overlay's own root, so `j`/`k` can scroll an open hunk's box before they move the cursor. */
  const reviewOverlayRef = useRef<HTMLDivElement>(null);
  /** The detail popover's rows, or `null` when it is closed. Set by a
   *  `tab_detail` envelope (the reply to `open_detail`, sent by the band and by `prefix i`);
   *  closed the same places `keymapOpen` is forced shut, since the two are mutually exclusive
   *  overlays over the same conversation area. */
  const [detail, setDetail] = useState<DetailRow[] | null>(null);
  /** The row `j`/`k`/`y` act on inside the popover, reset to 0 every time it opens. */
  const [detailCursor, setDetailCursor] = useState(0);
  /** Whether `ContinueInTerminal`'s confirmation is open.
   *  Lifted here from that component's own local state -- which used to gate on a permanently
   *  visible button's click -- because the band leaves no permanently visible button any more: the
   *  only entry points now are `<leader>t` (`"panel.handoff"`, `onKeyDown` below) and the detail
   *  popover's own trailing row. Closed everywhere the other conversation-area overlays are (a
   *  pane-focus change, a tab switch, `enter_input`/`arrive`, `focus_permission`, `hint_collect`,
   *  a rename/close-prompt/chooser opening, an `error`/`handoff` whole-state reset) and by `Esc`. */
  const [handoffOpen, setHandoffOpen] = useState(false);
  /** The paths for a `gf` letter picker, or `null` when none is waiting. Set by `open-path` and
   *  cleared by the letter that selects a path or any other key. */
  const [pathPick, setPathPick] = useState<PathRef[] | null>(null);
  /** `gx` with several links -- or a titled one, or one nobody can see -- waiting for
   *  its letter: the full normalized addresses it lists, in order, or `null` between picks. Like `pathPick`
   *  it owns every key while set (`onKeyDown`, ahead of the key table), but it is also dropped by every route
   *  away (`dropPendingKeys`, the tab switch): a pick left up would open a link on a later letter, over a
   *  conversation that no longer holds it. */
  const [linkPick, setLinkPick] = useState<string[] | null>(null);
  /** `open_path` and `view_in_editor` share `edit_draft`'s `"editor"` request handling.
   *  They have no tab-scoped reply payload to restore; a refusal only needs a footer flash. */
  function openPath(ref: PathRef) {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_path", request_id: requestId, path: ref.path, ...(ref.line === null ? {} : { line: ref.line }) });
  }
  /** `c` in BROWSE: opens the review overlay on this tab's latest turn and asks the shell for it. The
   *  overlay is drawn at once, saying it is loading, and filled in by the `review` envelope that answers
   *  this request id (or told why not by the refusal). Recorded as `"review"`, so a refusal is the
   *  overlay's own header and not the conversation's banner. */
  function openReviewOverlay(tab: TabId) {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "review", tab });
    setReview(openReview(tab, requestId, reviewRecovery));
    postToRust({ type: "review_request", request_id: requestId, tab, turn: "latest", scope: "turn" });
  }
  /** What a key in the review overlay asked for beyond changing its own state. `tab` is the tab the overlay
   *  was opened on, never "the active one": a request names its tab. */
  function runReviewEffect(effect: ReviewEffect, tab: TabId) {
    switch (effect.kind) {
      case "close":
        setReview(null);
        return;
      case "request":
        trackRequest(effect.requestId, { kind: "review", tab });
        postToRust({ type: "review_request", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope });
        return;
      case "request-diff":
        trackRequest(effect.requestId, { kind: "review", tab });
        postToRust({ type: "review_diff_request", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path });
        return;
      case "open": {
        // The editor opens it at the hunk's first line when a patch has been loaded, else at the top. It
        // is the shell that knows whether the turn's hunks are drawn there too, so it is asked, by turn.
        const requestId = nextRequestId();
        trackRequest(requestId, { kind: "editor", tab, review: true });
        postToRust({
          type: "open_in_editor",
          request_id: requestId,
          tab,
          turn: effect.turn,
          scope: effect.scope,
          path: effect.path,
          ...(effect.line === null ? {} : { line: effect.line }),
        });
        return;
      }
      case "revert":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_revert", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path, target: effect.target });
        return;
      case "undo":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_undo", request_id: effect.requestId, tab });
        return;
      case "comment-add":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_comment_add", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path, from: effect.from, to: effect.to, text: effect.text });
        return;
      case "comment-remove":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_comment_remove", request_id: effect.requestId, tab, id: effect.id });
        return;
      case "send":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_send", request_id: effect.requestId, tab, confirm: effect.confirm });
        return;
      case "recover":
        // Window-level: the journal is the project's, so this names no tab.
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_recover", request_id: effect.requestId, entry: effect.entry, answer: effect.answer });
        return;
      case "copy":
        void navigator.clipboard?.writeText(effect.text);
        showFlash(`copied ${effect.text}`);
        return;
    }
  }
  /** `gx` passes the normalized `href` shown to the reader, taken from a `webLinks` `url`, to
   *  Rust. Rust rechecks it with `web_url` and opens the system browser. Track it as `"link"` in
   *  `inFlight` so a refusal flashes in the footer. */
  function openUrl(url: string) {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "link", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_url", request_id: requestId, url });
  }
  /** The popover's own scrollable root -- unused today (it has nothing to scroll into view yet),
   *  kept for parity with `keymapOverlayRef` and because a forwarded ref is part of the component's
   *  contract. */
  const detailRef = useRef<HTMLDivElement>(null);
  /** The picker opened by a bare `/model` or `/effort` reply, or `null` when closed. The client
   *  opens it without a Rust envelope when the events handler parses `turn_completed` after that
   *  send. See `pendingSlashPickerRef` and `../slashPicker`. */
  const [slashPicker, setSlashPicker] = useState<SlashPickerState | null>(null);
  /** A parsed `/model` or `/effort` reply held for one render before opening `slashPicker`.
   *  The effect can read current overlay state; the `events` handler installed once cannot.
   *  If another overlay is open, leave the reply as transcript text. Opening the picker over
   *  another overlay could let `takeKeys` restore focus underneath it and send Enter there. */
  const [slashReply, setSlashReply] = useState<SlashPickerState | null>(null);
  /** Set by `sendMessage` the moment it sends a BARE `/model`/`/effort` (`barePickerCommand`), read
   *  and cleared by the very next `turn_completed` this tab sees, whether or not that reply parses
   *  -- a ref, not state, because it is read and written from inside the `events` handler's own
   *  closure (installed once, in the mount effect) rather than from render. Only the immediately
   *  following turn is ever a candidate: a picker choice itself always carries an argument
   *  (`onChoose` below), so it can never re-arm this, and a follow-up queued behind a running turn
   *  is out of this feature's scope (an edge case `sendMessage`'s own "not running" path does not
   *  reach) -- worst case there, no picker opens and the reply shows as ordinary text, never a
   *  broken one.
   *
   *  Also a single flag rather than a per-tab one: the tab-switch
   *  reset block below clears it the moment the active tab changes, so a switch away before the
   *  reply lands drops the candidate instead of leaving it to be checked against whatever
   *  `turn_completed` the NEW active tab sees next -- the same reasoning `queueMessage`/`send_now`
   *  below never arming this at all is built on. Both `queueMessage` (queued behind a running turn)
   *  and `send_now`'s own post (an immediate send that may itself queue if its interrupt is
   *  refused) can leave more than one turn between the send and its own reply, so "the very next
   *  `turn_completed`" would not reliably mean "this command's own reply" the way it does for
   *  `sendMessage`'s own not-running path -- arming there risks a picker opening against an
   *  unrelated turn's text, worse than today's safe fallback (plain text, no picker). Left
   *  unfixed, deliberately, rather than reaching for a heuristic that would trade a missed picker
   *  for a wrong one. */
  const pendingSlashPickerRef = useRef<SlashPickerKind | null>(null);
  /** True while the chooser, slash picker, tab rename or live conversation's `/` or `:` line owns keys.
   *  Gate the lines on `sessionStarted`: stale line state under the empty layout must not
   *  reserve keys for a `SearchBar` that never renders. `pane_focus` and `arrive` already close
   *  `?`, the detail popover and the handoff confirm. Focus effects use this flag to keep keys
   *  with the visible overlay. */
  const overlayOpen =
    chooser !== null ||
    slashPicker !== null ||
    renaming !== null ||
    (search !== null && sessionStarted) ||
    exLine !== null ||
    review !== null;
  /** Current overlay state for the document key replay listener, installed once. */
  const overlayOpenRef = useRef(overlayOpen);
  overlayOpenRef.current = overlayOpen;
  /** Counters `takeKeys` (below) bumps to ask a specific overlay/composer to re-focus itself,
   *  mirroring the existing `inputRequest`/`arriveRequest` convention: a plain number so a second
   *  request while the first is still pending is never silently coalesced away by React (an
   *  unchanged boolean would be). */
  const [chooserFocusRequest, setChooserFocusRequest] = useState(0);
  /** Requests focus for `SlashPicker`, like `chooserFocusRequest` does for the chooser.
   *  Restoring focus to the picker when GTK returns focus keeps `j`/`k`/`Enter`/`Escape` working
   *  there instead of sending them to the container root. */
  const [slashPickerFocusRequest, setSlashPickerFocusRequest] = useState(0);
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
  /** C1a: which end `composerFocusRequest`'s NEXT bump should place the caret at --
   *  `"kept"` for `i`/`o` (wherever it was left, `Composer`'s own `caretRef`), `"end"` for `A`.
   *  Read once, alongside the bump, by `Composer`'s own effect; not itself a `PanelMode`-changing
   *  request the same way `composerFocusRequest` is, so it carries no `seq` of its own. */
  const [composerCaret, setComposerCaret] = useState<"kept" | "end">("kept");
  const [keysRequest, setKeysRequest] = useState(0);
  /** The panel's which-key table from `keymapHelp`. */
  const panelTable = keymapHelp.panel;
  /** Mirrors of state read by the document-capture Shift+Tab router in `onModeKey`.
   *  That effect is installed once, so refs assigned during render keep it reading current values. */
  const confirmOpenRef = useRef(false);
  confirmOpenRef.current = confirm !== null;
  /* The keys MOVING into a text field after a bypass or restore question appeared (a click on the composer, a
     rename field) end the question: what is typed there is text, and the `y` of "you should" must not give a tab
     bypass just because the question was still up. Only a move counts -- a field that already had the keys when
     the question opened (Shift+Tab pressed in the composer) is where an INPUT user answers, so its `y` still does. */
  const confirmEntersBypass = confirm?.kind === "bypass" || confirm?.kind === "restore" || confirm?.kind === "stop_background";
  useEffect(() => {
    if (!confirmEntersBypass) return;
    function onFocusIn(event: FocusEvent) {
      if (!isEditableElement(event.target)) return;
      setConfirm((c) => (c?.kind === "bypass" || c?.kind === "restore" || c?.kind === "stop_background" ? null : c));
    }
    document.addEventListener("focusin", onFocusIn, true);
    return () => document.removeEventListener("focusin", onFocusIn, true);
  }, [confirmEntersBypass]);
  const chooserOpenRef = useRef(false);
  chooserOpenRef.current = chooser !== null;
  const tabsRef = useRef<TabsEnvelope | null>(null);
  tabsRef.current = tabs;
  /** The previous non-modifier keydown anywhere in the panel, read by `answerConfirm` through
   *  `bypassYesCounts`. This stays one keystroke behind `currentKeyAtRef`: document capture runs
   *  before the same event reaches the bubble handler. Using the current key's timestamp would
   *  make a lone `y`/`Y` fail its own guard. Starting at `-Infinity` lets the first such key
   *  depend only on how long the prompt has been visible. */
  const lastKeyAtRef = useRef(-Infinity);
  /** The running "most recent non-modifier keydown" clock `lastKeyAtRef` lags one keystroke behind;
   *  private to the document-capture effect below. */
  const currentKeyAtRef = useRef(-Infinity);
  /** Defect 2 (2026-09-27 sandbox GUI pass): the key `answerConfirm` last consumed from a y/n
   *  prompt -- answered or cancelled, either way -- so its own auto-repeats can be swallowed rather
   *  than falling through as an ordinary key once `confirm` is null again. Mirrors
   *  `shell::hint::Held`: remember the held
   *  key, not a timer, and stop remembering it the moment its keyup arrives (the effect right
   *  below) -- never on a fresh, different key, so an ordinary later hold of the SAME key (nothing
   *  to do with any prompt) is never mistakenly swallowed just because this key answered a prompt
   *  once, earlier in the session. `null` when nothing consumed by a prompt is still physically held. */
  const promptSwallowKeyRef = useRef<string | null>(null);
  /** The trust question's own scroll box, and whether the key before this one was the first `g` of `gg`. */
  const trustOverlayRef = useRef<HTMLDivElement>(null);
  const trustPendingGRef = useRef(false);
  /* A trust question opens only once no other prompt, field or picker holds the keys. A close, bypass or restore
     prompt answers the next `y`, and the rename field, the chooser, the `:` line and the `gf` picker take typed
     letters; opening over any of them would make the key meant for it an answer to this question, and a `y` here
     loads a repository's hooks. So it waits, and opens with its own on-screen wait once they are gone. A route
     away drops it (`cancelBypassConfirm`, `endKeyPrompts`); Rust asks again afterwards. Another trust question
     on screen is replaced, with its own fingerprint and digest. */
  const keysTakenElsewhere =
    (confirm !== null && confirm.kind !== "trust") ||
    renaming !== null ||
    chooser !== null ||
    exLine !== null ||
    pathPick !== null;
  useLayoutEffect(() => {
    if (heldTrust === null || keysTakenElsewhere) return;
    const payload = heldTrust;
    setHeldTrust(null);
    // An overlay that takes the keys ends a pending sequence, as every confirm kind does.
    dropPendingKeys();
    exitRegion();
    setDetail(null);
    setHandoffOpen(false);
    setKeymapOpen(false);
    // The keys must be somewhere that is not a text field: a first send leaves them in the composer, and
    // a key typed into a field is never an answer.
    (containerRef.current ?? startScreenRef.current)?.focus({ preventScroll: true });
    trustPendingGRef.current = false;
    setConfirm({
      kind: "trust",
      tab: payload.tab,
      nonce: payload.nonce,
      view: payload,
      lines: [payload.state === "too_large" ? TRUST_BAND_PROMPT_N_ONLY : TRUST_BAND_PROMPT],
      openedAt: performance.now(),
      flashAfter: flashSeq.current,
    });
  }, [heldTrust, keysTakenElsewhere]);
  /* The question took the keys to the layout's root (a field must not hold them while it is up). On the start
     screen that root is not what handles keys -- `.empty-tab` inside it is -- so whichever way the question
     ended, the keys are asked for back, through the one path every other overlay's close uses. Reads this
     render's overlays, so a prompt, picker or rename that opened in its place keeps them. */
  const trustOpen = confirm?.kind === "trust";
  const trustWasOpen = useRef(false);
  useEffect(() => {
    if (trustWasOpen.current && !trustOpen) setKeysRequest((n) => n + 1);
    trustWasOpen.current = trustOpen;
  }, [trustOpen]);
  useEffect(() => {
    function clearPromptSwallow(event: globalThis.KeyboardEvent) {
      if (event.key === promptSwallowKeyRef.current) promptSwallowKeyRef.current = null;
    }
    // Capture phase on the window, the same tier `swallowWhilePending` (HINT) uses below, so a
    // keyup that some other handler stops from bubbling still clears this.
    window.addEventListener("keyup", clearPromptSwallow, true);
    return () => window.removeEventListener("keyup", clearPromptSwallow, true);
  }, []);
  /** The overlay's own scrollable root, so `j`/`k` typed while it is open can scroll IT rather than
   *  the conversation underneath (`onKeyDown`'s `keymapOpen` branch). */
  const keymapOverlayRef = useRef<HTMLDivElement>(null);
  /** One ordered view of the conversation for the cursor and expand keys. Keyed on the whole
   *  `state`, like `MessageList`'s memo, plus `expanded` and `detailed`: `buildDisplay` folds
   *  rows using both, so either changing must recompute what the cursor indexes. */
  const timeline = useMemo(
    () =>
      buildDisplay(buildTimeline(state), {
        expanded,
        detailed,
        turnRunning: state.activeTurnId !== null,
        thinking: thinkingDisplay,
      }),
    [state, expanded, detailed, thinkingDisplay],
  );
  /** For the dispatch handler (installed once), as `cursorRef`: #22's park names the cursor's row by key. */
  const timelineRef = useRef(timeline);
  timelineRef.current = timeline;
  /** Compute the displayed timeline from `frozenSnapshot` using the same transformation as the
   *  live timeline and `MessageList`. This lets `rowIndexOf` map a frozen DOM row to its key
   *  without using a live cursor index during VISUAL. `null` outside VISUAL/V-LINE. */
  const frozenTimeline = useMemo(
    () =>
      frozenSnapshot === null
        ? null
        : buildDisplay(buildTimeline(frozenSnapshot.state), {
            expanded: frozenSnapshot.expanded,
            detailed: frozenSnapshot.detailed,
            turnRunning: frozenSnapshot.state.activeTurnId !== null,
            thinking: thinkingDisplay,
          }),
    [frozenSnapshot, thinkingDisplay],
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
  /* An answered card leaves `answeredPermissions` (and its typed reason `permissionReasons`) once it
     is no longer pending: nothing can answer it any more, and an id the provider hands out again
     later names a new card. `state` is always the active tab's (a switch resets it, and both of
     these, in the same dispatch), so this never drops another tab's entry. */
  useEffect(() => {
    const pending = new Set(state.pendingPermissions.map((p) => p.permissionId));
    for (const id of permissionReasons.current.keys()) if (!pending.has(id)) permissionReasons.current.delete(id);
    const kept = [...answeredRef.current].filter((id) => pending.has(id));
    if (kept.length === answeredRef.current.size) return;
    answeredRef.current = new Set(kept);
    setAnsweredPermissions(answeredRef.current);
  }, [state.pendingPermissions]);
  /** The panel's `TypingGuard` receives every keydown seen by `onKeyDown`. It delays `a`/`d`/`D`
   *  by `TYPING_GUARD_MS` and checks Enter on a card button. Keep one instance across renders so
   *  the dispatch handler installed once reads the same guard. */
  const [typingGuard] = useState(() => new TypingGuard());
  /* An overlay drawn over the conversation takes the keys from the card a waiting answer was aimed
     at, whichever route opened it -- a key, an envelope, a click. */
  useEffect(() => {
    if (keymapOpen || detail !== null || handoffOpen || pathPick !== null || linkPick !== null || confirm !== null || overlayOpen) {
      typingGuard.cancel();
      // A held `j`/`k`'s glide stops where it is: the keys are the overlay's now, and its repeats must
      // reach the overlay instead of moving the conversation behind it.
      const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
      if (list !== null) cancelHold(list);
    }
  }, [typingGuard, keymapOpen, detail, handoffOpen, pathPick, linkPick, confirm, overlayOpen]);
  useEffect(() => () => void typingGuard.cancel(), [typingGuard]);
  /* The review overlay ends wherever something else takes the conversation area or the keys, whichever route
     brought it: another overlay or prompt opening (the envelopes that open them also close it directly), a
     click into the composer (INPUT is not BROWSE, and the overlay would swallow what is typed there), a
     switch to another tab (it shows this tab's turns). */
  const reviewCovered =
    keymapOpen ||
    detail !== null ||
    handoffOpen ||
    chooser !== null ||
    slashPicker !== null ||
    renaming !== null ||
    search !== null ||
    exLine !== null ||
    pathPick !== null ||
    linkPick !== null ||
    confirm !== null ||
    mode !== "browse";
  useEffect(() => {
    if (reviewCovered) setReview(null);
  }, [reviewCovered]);
  const activeTabId = tabs?.active ?? null;
  useEffect(() => {
    setReview((current) => (current !== null && current.tab !== activeTabId ? null : current));
  }, [activeTabId]);
  /* A new turn replaces the finished one the band's pointer was about. */
  useEffect(() => {
    if (state.activeTurnId === null) return;
    const tab = activeTabRef.current;
    if (tab !== null) setReviewHints((hints) => withoutHint(hints, tab));
  }, [state.activeTurnId]);
  /* A `gx` pick claims keys before the key table, so any other input or overlay taking focus
     must end it. Otherwise the next letter would open a link instead of reaching that input.
     Check the same overlays as the typing guard. `onKeyDown`'s `isEditableElement` check also
     handles fields such as a card's reason box that take focus without changing our state. */
  useEffect(() => {
    if (linkPick === null) return;
    if (mode !== "browse" || keymapOpen || detail !== null || handoffOpen || pathPick !== null || confirm !== null || overlayOpen) {
      setLinkPick(null);
    }
  }, [linkPick, mode, keymapOpen, detail, handoffOpen, pathPick, confirm, overlayOpen]);
  /* `slashReply` opens a picker only when no other overlay is open. Either way, consume it here.
     CARET/VISUAL/V-LINE also block the picker: focusing it would make the root's `onFocus` end
     the region via `exitRegion()`. Keep such replies as transcript text so an unrelated reply
     cannot interrupt the reader's selection. */
  useEffect(() => {
    if (slashReply === null) return;
    setSlashReply(null);
    const covered =
      overlayOpen ||
      keymapOpen ||
      detail !== null ||
      handoffOpen ||
      pathPick !== null ||
      linkPick !== null ||
      confirm !== null ||
      isRegionMode(modeRef.current);
    if (covered) return;
    // Opening the picker cancels pending prefixes, counts and leader sequences. Its key handler returns
    // before the usual pending-key reset, so stale input could otherwise consume the first key
    // after the picker closes.
    dropPendingKeys();
    setSlashPicker(slashReply);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [slashReply]);
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
  /** Routes keyboard arrivals through one focus handler so they cannot steal keys from an overlay.
   *  Handles `pane_focus`, `arrive`, `enter_input` and permission-card landings. Reads the current
   *  render's overlay and session state, so call it only from an effect after commit.
   *  Focus order is the chooser, slash picker, tab rename, `/` or `:` line, live conversation,
   *  then the empty tab's dashboard. */
  function takeKeys() {
    if (chooser !== null) {
      setChooserFocusRequest((n) => n + 1);
      return;
    }
    // An open picker must keep the keys through a GTK focus change, `arrive`, or a HINT landing, just
    // as the chooser does.
    if (slashPicker !== null) {
      setSlashPickerFocusRequest((n) => n + 1);
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
    // Focus the `:` command line like the search prompt; both use `SearchBar`.
    if (exLine !== null) {
      (containerRef.current ?? startScreenRef.current)?.querySelector<HTMLInputElement>(".search-bar input")?.focus();
      return;
    }
    // An open review comment input (a `SearchBar` too) keeps the keys through a window focus round trip, which
    // the overlay now survives: the conversation root would leave what is typed to the overlay's key table.
    if (review?.prompt?.kind === "comment") {
      containerRef.current?.querySelector<HTMLInputElement>(".review-prompt .search-bar input")?.focus();
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
  /* Shift+Tab switches mode throughout the chat. Capture it on `document` before React handlers
     and WebKit's backward focus navigation, which can hand focus to GTK's window `move-focus`
     when WebKit runs out of focusable elements. */
  useEffect(() => {
    function onModeKey(event: globalThis.KeyboardEvent) {
      // Record every non-modifier keydown before `isModeCycleKey` returns for keys other than
      // Shift+Tab. The guard needs to see recent keys even when this handler does not act on them.
      // Set `lastKeyAtRef` to the previous timestamp because this capture handler runs before
      // `answerConfirm` reads it for the same event.
      if (!isModifierKey(event.key)) {
        lastKeyAtRef.current = currentKeyAtRef.current;
        currentKeyAtRef.current = performance.now();
      }
      if (!isModeCycleKey(event)) return;
      event.preventDefault();
      // Shift+Tab does nothing in CARET/VISUAL/V-LINE: its bypass prompt could consume a `y`
      // intended to copy the selection. Require Esc before opening this overlay.
      if (isRegionMode(modeRef.current)) {
        event.stopPropagation();
        const cancelled = typingGuard.onKey("Tab", event.timeStamp > 0 ? event.timeStamp : performance.now());
        showFlash(cancelled ?? `Esc first: Shift+Tab does not act in ${regionModeName(modeRef.current)}`);
        return;
      }
      const activeInfo = activeTabInfo(tabsRef.current);
      const route = modeKeyRoute({
        confirmOpen: confirmOpenRef.current,
        chooserOpen: chooserOpenRef.current,
        tabState: activeInfo?.state ?? null,
        tabMode: activeInfo?.mode ?? null,
      });
      if (route === "overlay") return;
      // Shift+Tab within `TYPING_GUARD_MS` of a BROWSE `y` counts as typing. This capture handler must
      // check before the bubble handler, claim and record the key, and show any cancellation flash.
      // Only Shift+Tab after a pause cycles the mode.
      const modeKeyAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
      if (modeRef.current === "browse" && typingGuard.afterCopy(modeKeyAt)) {
        event.stopPropagation();
        showFlash(typingGuard.onKey("Tab", modeKeyAt) ?? TYPE_HINT_FLASH);
        return;
      }
      // Shift+Tab follows any pending prefix, count or leader sequence, but `stopPropagation`
      // prevents `onKeyDown` from clearing them. Clear them here for both routes. This uses only
      // refs and state setters, so the listener installed once can call it.
      dropPendingKeys();
      event.stopPropagation();
      // This capture-phase handler stops propagation, so Shift+Tab never reaches the bubble-phase typing
      // guard. Call `typingGuard.onKey` here to cancel deferred answers and count Shift+Tab when
      // guarding the next key. Show its returned cancellation flash. A bare `cancel()` would leave
      // the last-key timestamp unchanged. Use the same `event.timeStamp` fallback as the other
      // guard calls: fake timers can put `event.timeStamp` and `performance.now()` on different
      // clocks.
      const cancelled = typingGuard.onKey("Tab", event.timeStamp > 0 ? event.timeStamp : performance.now());
      if (cancelled !== null) showFlash(cancelled);
      if (route === "cycle") post({ type: "cycle_mode" });
      // "r" (not a rebindable panel-table action, so nothing on the wire names it): reaching
      // "fixed" always means the tab has already ended or failed.
      else if (route === "fixed") showFlash(modeFixedMessage("r"));
    }
    document.addEventListener("keydown", onModeKey, true);
    return () => document.removeEventListener("keydown", onModeKey, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /* Replay keys that land on `<body>` onto the panel root. Removing or disabling a focused
     control can move focus there without a reliable blur event, and disabled buttons receive no
     keys. Replaying preserves the triggering keystroke and focuses a root that can handle later
     keys; targeting the root avoids re-entry. In the empty layout, use `.empty-tab`: its
     `.agent-ui-root` parent handles only keymap and confirmation capture, so replaying there
     would leave the empty tab's keys unreachable. */
  useEffect(() => {
    function onDocumentKeyDown(event: globalThis.KeyboardEvent) {
      if (event.target !== document.body && event.target !== document.documentElement) return;
      const start = startScreenRef.current;
      // On the empty layout, an open overlay keeps the keys. Replaying onto the tab behind it could let a
      // failed tab's `r` reset the session under the chooser.
      if (containerRef.current === null && overlayOpenRef.current) {
        event.preventDefault();
        setKeysRequest((n) => n + 1);
        return;
      }
      const root =
        containerRef.current ?? start?.querySelector<HTMLElement>(".empty-tab[tabindex]") ?? start;
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
  /* Reconcile only when `timeline` changed and `cursor` did not. Streamed deltas create a new
     timeline object, so reconciling a simultaneous clamp, keyboard move or restore would put the
     cursor back on its old row. Moving back can reveal a stale row and stop `MessageList`
     following the end after the intended move consumed the restore's reveal suppression. A
     cursor moved in this render already has an intended destination. */
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
  /** Keeps the cursor's row visible when the cursor moves, so `j`/`k` do not move an unseen highlight.
   *  Reveals only what is needed; a row already fully visible does not move the viewport.
   *  Uses `containerRef` and `conversationRows` to find the actual cursor row. A model reply can
   *  contain its own `.row-current`, so querying the first matching element could pick a false row.
   *  `MessageList` gates following on its scroll position, allowing a reader further up to stay there.
   *  If following already revealed the tail, revealing the cursor row needs no extra scroll.
   *  jsdom has no layout or `scrollIntoView`, so tests can verify the target element and call,
   *  but viewport visibility needs a GUI check. */
  useEffect(() => {
    const landing = landingRef.current;
    const animate = landingAnimateRef.current;
    landingRef.current = 0;
    landingAnimateRef.current = false;
    if (landing === "keep") return;
    // During VISUAL, the displayed list is frozen while `cursor` indexes the live timeline. A new
    // prompt can move that cursor beyond the frozen rows, so revealing it could scroll an unrelated
    // row under the selection. Only VISUAL's caret moves the view; restore the row cursor by key
    // through `landCursorOnRowKey` when VISUAL ends.
    if (frozenSnapshotRef.current !== null) return;
    const root = containerRef.current;
    const row = root === null ? undefined : conversationRows(root)[cursor];
    if (row === undefined) return;
    const list = row.closest<HTMLElement>(".message-list");
    if (list !== null) settleListScroll(list);
    if (landing !== 0) enterBoxAtNearEnd(row, landing);
    revealRow(list, row, landing, animate);
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
      // During VISUAL, these rows belong to the frozen list. Writing their index into the live
      // cursor could select a different row after new rows arrive. Only VISUAL's caret scrolls the
      // list; restore the row cursor by key when VISUAL ends.
      if (frozenSnapshotRef.current !== null) return;
      // While a `j`/`k` eases the view toward its target the cursor's row, still on its way in, can be
      // wholly out of view for a frame; the target has it in view, so nothing is re-homed until it lands.
      if (isListScrollAnimating(list)) return;
      const rows = conversationRows(list);
      const next = clampCursorToView(list, rows, cursorRefForScroll.current);
      if (next !== null && next !== cursorRefForScroll.current) {
        landingRef.current = "keep";
        setCursor(next);
      }
    };
    // #22: a wheel or a touch drag on the list is the reader choosing a new place, keys or not; the
    // next arrival must not undo it by restoring the parked one.
    const dropPark = () => {
      arrivalParkRef.current = null;
    };
    list.addEventListener("scroll", onListScroll, { passive: true });
    list.addEventListener("wheel", dropPark, { passive: true });
    list.addEventListener("touchmove", dropPark, { passive: true });
    return () => {
      list.removeEventListener("scroll", onListScroll);
      list.removeEventListener("wheel", dropPark);
      list.removeEventListener("touchmove", dropPark);
    };
  }, [sessionStarted]);
  /** At the bottom, the cursor follows the new last row as the conversation grows, and a sent
   *  prompt takes the cursor. Key on `timeline`: a state change that does not grow the timeline
   *  must not move the cursor. The events dispatcher sets `landOnPromptRef` on
   *  `user_prompt_submitted`; read and clear it here, once the target row exists in this
   *  render's timeline. */
  const lastLengthRef = useRef(0);
  const landOnPromptRef = useRef(false);
  /** Set by a switch's (or a mount's) first snapshot with no saved view: see the `snapshot` arm. */
  const landOnLastRef = useRef(false);
  /* Use a layout effect to move the cursor before scroll events can clamp it to another visible
     row. A passive effect runs after the browser dispatches the scroll from `MessageList`
     snapping to the end. If the new last row is taller than the view, the old last row may be
     off screen and the clamp can break cursor following. The child's snap layout effect has
     already run here, so the distance read below reflects it. */
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
    // A tab left while following comes back following (the small-defects GUI pass, 2026-09-25):
    // said outright, as a send does, rather than as a scroll to the end the list reads a frame later
    // inside a steering window -- where any other movement above the end counted as the user
    // scrolling up. A tab left parked comes back exactly where it was.
    applyViewScroll(list, view, view.atBottom);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [restoreTick]);
  /** The session is gone (lost or closed). Read before the start-screen branch below, because the
   *  effect under it is a hook and cannot live after a conditional return. */
  const sessionEnded = state.status.kind === "unavailable" || state.status.kind === "closed";
  /** A design decision: the card INPUT's `Ctrl+y` would approve right now -- the active tab's oldest
   *  waiting card (`approveOldestByKey` picks the same one) -- for the band to name, only while the
   *  composer has the keys (INPUT, this pane focused) on a live session. `null` otherwise. */
  const ctrlYCard: ApproveFact | null = (() => {
    // Show the `Ctrl+y` target only while the composer box holds the keys, matching
    // `approveOldestByKey`. A search field must not advertise an approval shortcut that would do
    // nothing.
    if (mode !== "input" || !paneFocused || sessionEnded || !composerBoxFocused) return null;
    const index = oldestWaitingPermission(timeline, answeredPermissions);
    const item = index === null ? undefined : timeline[index];
    // A card Approve cannot answer is not named: `Ctrl+y` would only flash that it cannot.
    return item?.kind === "permission" && cardApprovable(item.request)
      ? { tool: item.request.toolName, summary: cardSummary(item.request.input) }
      : null;
  })();
  /** Identifies INPUT's `Ctrl+y` target by tab and permission id. Each target must stay unchanged
   *  for `TYPING_GUARD_MS` before approval, so a quick second `Ctrl+y` cannot approve a new card.
   *  Arrivals, withdrawals, tab switches and approvals that change the target restart the timer.
   *  Uses a timer like `TypingGuard.defer`, without adding another clock. */
  const ctrlYTargetKey: string | null = (() => {
    if (sessionEnded) return null;
    const index = oldestWaitingPermission(timeline, answeredPermissions);
    const item = index === null ? undefined : timeline[index];
    return item?.kind === "permission" ? `${activeTabRef.current}:${item.request.permissionId}` : null;
  })();
  useLayoutEffect(() => {
    ctrlYTargetRef.current = { key: ctrlYTargetKey, settled: false };
    if (ctrlYTargetKey === null) return;
    const handle = setTimeout(() => {
      if (ctrlYTargetRef.current.key === ctrlYTargetKey) ctrlYTargetRef.current = { key: ctrlYTargetKey, settled: true };
    }, TYPING_GUARD_MS);
    return () => clearTimeout(handle);
  }, [ctrlYTargetKey]);
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
    // End the region when the session dies to clear `frozenSnapshot` and the DOM selection before
    // returning to BROWSE. Calling `exitRegion` is harmless when no region is active.
    if (sessionEnded) {
      exitRegion();
      setMode("browse");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionEnded]);
  // The `?` keymap belongs to a live conversation; a session that just
  // ended is not a reason to keep it up, and the ended/lost banners' own `r` must be reachable
  // without an extra `?`/`Escape`/`q` first.
  useEffect(() => {
    if (sessionEnded) setKeymapOpen(false);
  }, [sessionEnded]);
  /* A new tab arrival or HINT landing on the composer enters INPUT with a blinking caret.
     Like `i` in `resolveKey`, this requires a live session with a composer. Keyed on the request
     alone so a session change cannot open the composer. Arrivals that may find a pending card
     use `arrive` instead. */
  useEffect(() => {
    if (inputRequest === 0) return;
    // An overlay over the conversation must keep the keys so this request cannot open a composer
    // underneath it.
    if (overlayOpen) {
      takeKeys();
      return;
    }
    if (!sessionStarted || sessionEnded) return;
    setMode("input");
    // Requests that open the live composer restore its kept caret, matching i, o and Ctrl+j.
    // A selects the end through a separate focus request, so its caret choice must not carry
    // over into a later request to resume typing at the saved position.
    setComposerCaret("kept");
    setComposerFocusRequest((n) => n + 1);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inputRequest]);
  /* `arrive` lands every keyboard arrival in BROWSE except a brand-new tab's (`enter_input`, above). A
     card waiting in this tab gets the same landing `focus_permission` gives: BROWSE, on the oldest
     one, never a composer the user did not ask to type into. Keyed on the request alone, like
     `inputRequest`.
     Otherwise it restores the reader's place. A reader following the bottom lands on the last row and
     keeps following (the same way a send does, `./follow.ts`); anyone else gets the row they left (by
     key, so a row removed above it while away does not shift the landing) and the scroll they left,
     through the park the `pane_focus` arm took. With no usable park (a launch arrival, a park for
     another tab, one a key or a hand on the list made stale), the live view decides the same way:
     following lands last, anything else moves nothing. */
  useEffect(() => {
    if (arriveRequest === 0) return;
    const park = arrivalParkRef.current;
    arrivalParkRef.current = null;
    // A key handled since the envelope arrived already acted on the panel as drawn (`landingAtKeyRef`):
    // the landing would undo it, so the key wins and nothing below runs.
    const atKey = landingAtKeyRef.current;
    landingAtKeyRef.current = null;
    if (atKey !== null && atKey !== typingGuard.keyCount()) return;
    // End the region before an arrival changes mode or cursor. Otherwise `frozenSnapshot` can keep the
    // old tab's conversation visible over the new one. This is a no-op when no region is active.
    exitRegion();
    // An open overlay keeps the keys and the conversation cursor stays put. Landing BROWSE on the last
    // row would interfere with both.
    if (overlayOpen) {
      takeKeys();
      return;
    }
    if (oldestPendingPermission(timeline) !== null) {
      // The card landing is one more effect away; a key handled before it yields it the same way.
      landingAtKeyRef.current = typingGuard.keyCount();
      setPermissionRequest((n) => n + 1);
      return;
    }
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    const last = Math.max(timeline.length - 1, 0);
    const usable = park !== null && park.tab === activeTabRef.current && park.atKey === typingGuard.keyCount();
    const following = usable
      ? park.following
      : (list === null || list.scrollTop + list.clientHeight >= list.scrollHeight - 1) && cursorRef.current === last;
    setMode("browse");
    if (following) {
      // Only when the cursor really moves -- a `"keep"` set for a `setCursor` that changes nothing
      // fires no `[cursor]` effect, so nothing consumes it, and it swallows the NEXT move's reveal
      // (the same convention the switch restore below follows).
      if (last !== cursorRef.current) landingRef.current = "keep";
      setCursor(last);
      resumeFollowing(list);
      return;
    }
    if (!usable || list === null) return;
    const index =
      (park.cursorKey === null ? null : indexOfKey(timeline, park.cursorKey)) ??
      Math.min(Math.max(park.view.cursor, 0), last);
    if (index !== cursorRef.current) landingRef.current = "keep";
    setCursor(index);
    // Never `park.view.atBottom` here: a park that is not following can still be at the bottom (`k`
    // once from the last row, `gg` in a conversation that fits), and re-arming following would undo
    // the stop that `k`/`gg` made on purpose. Written directly rather than through `restoreRef`, which
    // waits for a switch's snapshot and must not be clobbered by an arrival.
    applyViewScroll(list, park.view, false, true);
    // The row is authoritative. If a width change while hidden leaves it entirely off screen at the
    // restored scroll position, reveal it before the scroll clamp can move the cursor.
    const rows = conversationRows(list);
    const row = rows[index];
    if (row !== undefined && clampCursorToView(list, rows, index) !== null) revealRow(list, row, 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [arriveRequest]);
  /* `focus_permission`: BROWSE, with the cursor on the oldest pending card, so
     `a`/`d` answer it at once. `shell` sends it only when it counts a card; if the card was answered
     in between, there is nothing to land on, and the arrival is an ordinary keyboard one -- the
     composer, as `enter_input` gives it. Keyed on the request alone, like `inputRequest`. */
  useEffect(() => {
    if (permissionRequest === 0) return;
    // As in the `arrive` effect: a key handled since `focus_permission` (or `arrive`, which handed its
    // landing on to this effect) arrived wins over the landing. A tab switch's own card landing sets no
    // count and always lands.
    const atKey = landingAtKeyRef.current;
    landingAtKeyRef.current = null;
    if (atKey !== null && atKey !== typingGuard.keyCount()) return;
    // End the region before landing directly on a permission card, as for `arrive`. A frozen snapshot
    // must not keep showing a different row.
    exitRegion();
    // Keep the keys in an open overlay, as for `arriveRequest`. The `focus_permission` dispatch already
    // closes the chooser, so this preserves focus in a rename field or the `/` prompt.
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
    // Landing moves the cursor, and the `[cursor]` effect reveals its row. Announce this as a
    // user scroll through `./follow.ts`, as for a HINT landing. Otherwise `MessageList` keeps
    // following and the next delta or resize can scroll the older card out of view.
    noteUserScroll(containerRef.current?.querySelector(".message-list"), "unknown");
    // A landing always reveals the card, even over a `"keep"` some earlier restore left behind; and
    // when the cursor is already on it (a switch restored it there), nothing fires the `[cursor]`
    // effect, so the row is revealed here -- after the restore's own scroll, which is a layout effect.
    landingRef.current = 0;
    if (index === cursorRef.current) {
      // `exitRegion` above has only asked for the thawed list. If VISUAL was on,
      // the DOM here is still the frozen one and `index` is a live index -- a card that arrived
      // during VISUAL is not in it -- so the reveal waits for the live list to render.
      if (frozenSnapshotRef.current !== null) revealAfterThawRef.current = index;
      else {
        const root = containerRef.current;
        const row = root === null ? undefined : conversationRows(root)[index];
        if (row) revealRow(row.closest<HTMLElement>(".message-list"), row, 0);
      }
    } else setCursor(index);
    containerRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [permissionRequest]);
  /** `focus_permission`'s own reveal, deferred while the list on screen was still the frozen one
   *: run once the thawed, live list has rendered, and only then indexed by the
   *  live cursor. A layout effect so the reveal lands before that list is painted. */
  useLayoutEffect(() => {
    if (frozenSnapshot !== null) return;
    const index = revealAfterThawRef.current;
    revealAfterThawRef.current = null;
    if (index === null) return;
    const root = containerRef.current;
    const row = root === null ? undefined : conversationRows(root)[index];
    if (row) revealRow(row.closest<HTMLElement>(".message-list"), row, 0);
  }, [frozenSnapshot]);

  /** Clears `frozenSnapshot` and the built selection whenever `mode` leaves a region mode.
   *  Explicit `exitRegion` calls handle normal exits; this catches paths that change `mode`
   *  without using them. `clearVisualLeftovers` does nothing once everything is already `null`. */
  useEffect(() => {
    if (!isRegionMode(mode)) clearVisualLeftovers();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);
  /** Ends the region when `sessionStarted` becomes false, since there is no conversation to select.
   *  Catches routes that remove the conversation without changing `mode`, which the `[mode]`
   *  cleanup cannot see. `error` and `handoff` also call `exitRegion` directly. */
  useEffect(() => {
    if (!sessionStarted) exitRegion();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionStarted]);

  /** Watches `.message-list` for changes to either end of the region's built selection.
   *  `frozenSnapshot` normally keeps the list still, but other DOM writes may bypass that freeze.
   *  A `childList` or `characterData` change touching an endpoint ends the region with a flash,
   *  like a failed selection check when copying. Changes to unrelated rows must not interrupt
   *  the selection merely because a card arrives or a reply streams. */
  useEffect(() => {
    if (!isRegionMode(mode)) return;
    if (typeof MutationObserver === "undefined") return;
    const list = messageListEl();
    if (list === null) return;
    const observer = new MutationObserver((mutations) => {
      const built = visualBuiltRef.current;
      if (built === null) return;
      const touches = (node: Node) =>
        node === built.anchorNode || node === built.focusNode || node.contains(built.anchorNode) || node.contains(built.focusNode);
      for (const mutation of mutations) {
        // `characterData`'s own `target` IS the changed text node -- typically the anchor/focus
        // node itself, since both are text positions. `childList`'s `target` is the still-attached
        // PARENT whose children changed, which stays true (and so keeps `touches` reporting a hit)
        // for content inserted nearby; a REMOVAL is the case that check alone would miss, because a
        // detached anchor/focus's old parent no longer contains it once the removal has happened --
        // `removedNodes` is checked for exactly that.
        if (touches(mutation.target) || Array.from(mutation.removedNodes).some(touches)) {
          // Name the mode that actually ended, as `vend` and the Shift+Tab refusal do, so CARET is not
          // reported as VISUAL.
          showFlash(`the conversation changed under it — ${regionModeName(modeRef.current)} ended; v to start again`);
          exitRegion();
          return;
        }
      }
    });
    observer.observe(list, { childList: true, characterData: true, subtree: true });
    return () => observer.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);

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
  /** A `hint_collect` sessionId waiting for the thawed list, the same deferral
   *  `revealAfterThawRef`/`focus_permission` uses: set only when `hint_collect`
   *  arrived while `frozenSnapshotRef.current` was still non-null, consumed by the layout effect just
   *  below `collectHintTargets`'s own declaration. */
  const pendingHintCollectRef = useRef<number | null>(null);
  /** `hint_collect`'s actual target freeze: read the live DOM, remember it by session, tell `shell` how
   *  many labels to draw. Split out of the dispatch arm so both the immediate path (VISUAL was already
   *  off) and the deferred one (below) call the same code. */
  function collectHintTargets(sessionId: number) {
    const root = containerRef.current ?? startScreenRef.current;
    frozenRef.current = root === null ? [] : hintTargets(root);
    hintSessionRef.current = sessionId;
    // A newer session supersedes whatever an older one still had on screen.
    setHints([]);
    setHintTyped("");
    postToRust({
      type: "hint_targets",
      request_id: nextRequestId(),
      session_id: sessionId,
      count: frozenRef.current.length,
    });
  }
  /** The deferred half of `collectHintTargets`: `hint_collect`'s dispatch arm only *asks* `exitRegion`
   *  to thaw the list -- it
   *  does not repaint synchronously, so collecting from `containerRef.current` right there could still
   *  read the frozen VISUAL DOM and miss a row that arrived during VISUAL (a linked permission card, a
   *  queued prompt): probed at 3 targets with VISUAL framing the collect versus 7 once thawed. Run once
   *  the thawed, live list has rendered -- a layout effect so the freeze lands before HINT's labels are
   *  painted on top of it, same as `revealAfterThawRef`'s own effect. */
  useLayoutEffect(() => {
    if (frozenSnapshot !== null) return;
    const sessionId = pendingHintCollectRef.current;
    pendingHintCollectRef.current = null;
    if (sessionId === null) return;
    collectHintTargets(sessionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [frozenSnapshot]);
  /** A code block HINT landed on: the next `y` copies exactly that block's code rather than the
   *  whole message. Cleared by that copy and by any other key the table resolves. */
  const copyCodeRef = useRef<HTMLElement | null>(null);
  /** Updates `copyCodeRef` and the block's `data-hint-landed` attribute together.
   *  The dispatch handler and `enterRegion` read the ref synchronously. The block lives in
   *  markdown `innerHTML`, so React cannot render the attribute. The cursor band is drawn
   *  beneath the outline and needs no separate list attribute. A streamed delta can replace
   *  this `pre`, removing its attribute and leaving the ref detached (`isConnected`). */
  function markLandedCode(el: HTMLElement | null) {
    const previous = copyCodeRef.current;
    if (previous === el) return;
    previous?.removeAttribute("data-hint-landed");
    copyCodeRef.current = el;
    el?.setAttribute("data-hint-landed", "");
  }
  // Replacing a landed block during streaming removes its outline; the detached ref is left alone.
  // Clear the landing when a non-key event moves the cursor off the block's row, or the outline
  // would remain on a different row. Keys clear it themselves. An arrival that stays on the row
  // keeps the landing, so `y` still copies the block after `Ctrl+h Ctrl+l`.
  useEffect(() => {
    const block = copyCodeRef.current;
    if (block === null || frozenSnapshotRef.current !== null) return;
    if (!block.isConnected) return;
    const root = containerRef.current;
    if (root === null) return;
    if (rowOf(root, block) !== (conversationRows(root)[cursor] ?? null)) markLandedCode(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline, cursor]);
  /** A control HINT landed on, for the `mode` effect above to leave focused: landing from INPUT
   *  blurs the composer, which sets BROWSE, and that effect would otherwise take focus straight back
   *  to the root. Consumed or dropped on the very next commit (the effect just below). */
  const landedControlRef = useRef<HTMLElement | null>(null);
  useEffect(() => {
    landedControlRef.current = null;
  });
  /** The control focused by an `h`/`l` move, plain Tab or HINT landing, with its
   *  `typingGuard.keyCount()`. HINT labels do not reach this page, so use the count at landing.
   *  Enter activates a card button only if this names that button at the preceding key count.
   *  Write it where focus moves, not from key names. Later keys make it stale by count, so it
   *  needs no clearing. Keep it in a ref for the dispatch installed once; `landedControlRef` is
   *  cleared on every commit. */
  const placedRef = useRef<{ el: HTMLElement; atKey: number } | null>(null);
  /** The last plain Tab's `keyCount()` and native event until its focus move, or `null`.
   *  The root's `onFocus` consumes this and records a landing only if the event's default was
   *  not prevented. Check then, after all handlers have run, so a swallowed Tab cannot make an
   *  unrelated later focus change count as a keyboard landing. */
  const tabAtKeyRef = useRef<{ atKey: number; event: globalThis.KeyboardEvent } | null>(null);

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

  /** The keys overlay, the detail popover and the handoff confirmation: what every route that lands the keys
   *  somewhere else, or opens another overlay over the conversation, closes so the two never fight for the keys
   *  (a keys overlay left up over INPUT swallowed every keystroke). */
  function closeTransientOverlays() {
    setKeymapOpen(false);
    setDetail(null);
    setHandoffOpen(false);
  }

  useEffect(() => {
    installDispatch((payload) => {
      // Every session-scoped envelope names its tab, and the page keeps only the active tab's state, so one for
      // any other tab -- a batch still in flight when the user switched away -- is dropped before any arm below
      // can touch the page.
      if (!acceptsEnvelope(payload as { kind: string; tab?: number }, activeTabRef.current)) return;
      const receivedAt = performance.now();
      const previousPage = pageRef.current;
      const nextPage = pageModel(previousPage, { ...payload, receivedAt: Date.now(), monotonicAt: receivedAt });
      // The refs at once, not at the next render: the next envelope of this batch (an `error` for the tab just
      // switched to, a snapshot after `tabs`) must start from this one's result.
      pageRef.current = nextPage;
      stateRef.current = nextPage.state;
      setPage(nextPage);
      // A waiting `a`/`d`/`D` was aimed at a card these envelopes take the keys or the view from. Cancelled here,
      // in the same task as the envelope, so its timer can never come due in between and answer a card that is
      // no longer the one in front of the user.
      if (nextPage.answerEpoch !== previousPage.answerEpoch) typingGuard.cancel();
      if (CANCELS_WAITING_ANSWER.has(payload.kind)) {
        // A held `j`/`k`'s glide stops where it is: the keys, or the view, are someone else's now.
        const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list !== null) cancelHold(list);
      }
      switch (payload.kind) {
        case "theme": {
          applyTheme(payload.vars);
          break;
        }
        case "display": {
          // The model keeps the level it has for a word this build does not know.
          if (!(THINKING_DISPLAYS as readonly string[]).includes(payload.thinking)) {
            console.warn("agent-ui: a display envelope named an unknown thinking level", payload.thinking);
          }
          break;
        }
        case "editor_typing": {
          // Window-level and Rust's alone (a cadence slower than the meter's step): it slows the meter, never
          // pauses it. Not among the envelopes that cancel a waiting answer or touch any other state -- it is not
          // about the panel's own keys.
          applyEditorTyping(document.documentElement, payload.typing, payload.periodMs);
          break;
        }
        case "pane_focus": {
          // The keys leaving can be the first half of the tab leaving this page (a join back into the base page
          // follows at once); a draft still waiting out its debounce would then be posted by a page that no
          // longer shows the tab, so it goes out now.
          if (!payload.focused) flushDraft();
          // A pending prefix is for the very next key; a pane switch in between (GTK takes `Ctrl+h`/`Ctrl+k`
          // before the WebView sees a keydown) must not leave it armed for a key pressed much later, when it
          // would complete a chord nobody meant to start. The leader engine's pending sequence, the box waiting
          // to show it, and a count (which would multiply a `j` pressed after coming back) go the same way.
          dropPendingKeys();
          // The keys overlay has no reason to stay drawn while another pane has the keys; left up, coming back
          // with `Ctrl+l` landed in INPUT with every keystroke swallowed by the overlay. The review overlay is not
          // closed here (see the model's `pane_focus` arm).
          closeTransientOverlays();
          // The `/` prompt and the `:` line are this panel's own, like the keys overlay.
          setSearch(null);
          setExLine(null);
          // A Tab whose focus move never landed inside this page (it left for another widget) must not make
          // whatever this page focuses on the way back a landing.
          tabAtKeyRef.current = null;
          // Losing focus is a route away from whatever this panel was showing, the bypass prompt included;
          // regaining it is not (this pane's own keydowns are the only thing that answers the prompt, and those
          // never fire while it is unfocused anyway).
          if (!payload.focused) cancelBypassConfirm();
          // Losing focus ends VISUAL; nothing else here sets `mode`.
          if (!payload.focused) exitRegion();
          // The keys leave: park where the reader is, after `exitRegion` so a VISUAL mode is never what is
          // parked. Only over a conversation: the empty tab lands its own way.
          if (!payload.focused) {
            const tab = activeTabRef.current;
            const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
            if (tab !== null && list !== null) {
              const view = captureView(list);
              const items = timelineRef.current;
              arrivalParkRef.current = {
                tab,
                view,
                cursorKey: items[view.cursor]?.key ?? null,
                following: view.atBottom && view.cursor === items.length - 1,
                atKey: typingGuard.keyCount(),
              };
            } else arrivalParkRef.current = null;
          }
          // Only regaining focus is a request for the keys back. WebKitGTK's DOM focus across the round trip is
          // not guaranteed to have survived (`takeKeys`), so this is the one place that asks.
          if (payload.focused) setKeysRequest((n) => n + 1);
          break;
        }
        case "enter_input": {
          // This puts the caret in the composer, so no overlay may be left covering it and eating what gets
          // typed. Its only sender is a brand-new tab, so there is never a card waiting to land on instead.
          closeTransientOverlays();
          endKeyPrompts();
          // This lands INPUT, never on a parked row.
          arrivalParkRef.current = null;
          setEmptyLanding("input");
          break;
        }
        case "arrive": {
          // Every keyboard arrival into the chat lands BROWSE; an overlay would sit over whatever it lands on.
          closeTransientOverlays();
          endKeyPrompts();
          // A reserved two-key prefix or a leader sequence armed before this arrival means nothing about it.
          dropPendingKeys();
          setEmptyLanding("browse");
          landingAtKeyRef.current = typingGuard.keyCount();
          break;
        }
        case "focus_permission": {
          // An overlay or landing that takes the keys ends a pending sequence, box and reserved prefix included,
          // so a key the overlay lets bubble can never advance a stale one. The overlays would cover the card the
          // cursor is about to land on.
          dropPendingKeys();
          closeTransientOverlays();
          endKeyPrompts();
          // A card landing wins over a parked row.
          arrivalParkRef.current = null;
          landingAtKeyRef.current = typingGuard.keyCount();
          break;
        }
        case "nav_key": {
          // A key, claimed by GTK before the WebView saw it, so it never reached `answerConfirm`: "any key but a
          // counted y cancels" is applied here instead. It is also a key pressed after any pending
          // `g`/`z`/`[`/`]` prefix or leader sequence, cancelled for the reason `pane_focus` cancels them -- left
          // armed, `g`, Ctrl+j, Ctrl+k, `g` ran a stale `gg`. Whether the chord is claimed or answered with
          // `nav_fallthrough` is decided in an effect against the current render.
          endKeyPrompts();
          dropPendingKeys();
          break;
        }
        case "keymap": {
          // A table that changed while a sequence was pending must never let the old table's binding run against
          // the new one. A deferred single-key binding was cancelled above for the same reason.
          clearSequence();
          break;
        }
        case "literal_key": {
          // `send-prefix`/`send-keys` from shell: WebKitGTK cannot be handed the key itself. `C-a` is what a text
          // field does with it -- select all of the focused one; any other key is ignored. Not
          // `isEditableElement`: this needs "has a text selection", which is exactly these two types. A key GTK
          // claimed, so the y/n cancel is applied here, as for `nav_key`.
          endKeyPrompts();
          if (payload.key !== "C-a") return;
          const el = document.activeElement;
          if (el instanceof HTMLTextAreaElement || (el instanceof HTMLInputElement && el.type === "text")) {
            el.select();
          }
          break;
        }
        case "open_keymap": {
          // `prefix ?` is GTK's chord, so no key of it reaches `answerConfirm` or VISUAL's own handler: a route
          // away from a y/n prompt and from VISUAL here. The start screen draws the overlay too.
          endKeyPrompts();
          exitRegion();
          setMode("browse");
          setKeymapPlugin(pluginKeysHelp(payload.plugin));
          keymapOpening.current = typeof payload.opening === "number" ? payload.opening : null;
          keymapClosedByKey.current = false;
          setKeymapOpen(true);
          break;
        }
        case "open_command_line": {
          // `prefix :`, tmux's `command-prompt`. GTK took the chord, and before it was bound the armed prefix
          // swallowed the `:` and the letters after it ran as panel keys ("kill-window -a" moved the cursor,
          // entered INPUT and sent "ll-window -a"). This opens the same line BROWSE's own `:` opens: it runs
          // nothing, Enter says so, Esc closes it, and it has the keys -- on the start screen too. A route away
          // from every y/n prompt and `gf` picker, from VISUAL and from every overlay over the conversation; the
          // `/` prompt and this line are one command line, so opening one closes the other.
          endKeyPrompts();
          exitRegion();
          closeTransientOverlays();
          setSlashPicker(null);
          setSlashReply(null);
          pendingSlashPickerRef.current = null;
          dropPendingKeys();
          setMode("browse");
          setSearch(null);
          setExLine("");
          setLineFocusRequest((n) => n + 1);
          break;
        }
        case "hint_collect": {
          // A HINT ends VISUAL, and closes what would hide rows under it or take the keys its labels are about to
          // swallow. The `/model`/`/effort` picker too, open or on its way: a reply landing under the labels used
          // to open it there, and landing on the composer then left the keys on the picker.
          exitRegion();
          closeTransientOverlays();
          setSlashPicker(null);
          setSlashReply(null);
          pendingSlashPickerRef.current = null;
          // The labels and the label keys are GTK's, never a keydown here, so the next ordinary key after a
          // landing (the `y` that copies the code block just landed on, the first letter typed into the composer)
          // answered a y/n prompt left open under it. Rust drops its own prompt too.
          endKeyPrompts();
          setSearch(null);
          setExLine(null);
          dropPendingKeys();
          // `exitRegion` has only asked for the thawed list to render, so the DOM here can still be the frozen
          // VISUAL one, which misses any row that arrived during VISUAL (a linked permission card, a queued
          // prompt). Collecting then waits for the layout effect that runs once it is thawed.
          if (frozenSnapshotRef.current !== null) pendingHintCollectRef.current = payload.sessionId;
          else collectHintTargets(payload.sessionId);
          break;
        }
        case "hint_show": {
          if (payload.sessionId !== hintSessionRef.current) return;
          const frozen = frozenRef.current;
          setHints(payload.labels.slice(0, frozen.length).map((label, i) => ({ target: frozen[i], label })));
          setHintTyped("");
          break;
        }
        case "hint_prefix": {
          if (payload.sessionId !== hintSessionRef.current) return;
          setHintTyped(payload.typed);
          break;
        }
        case "hint_land": {
          if (payload.sessionId !== hintSessionRef.current) return;
          landOnHint(frozenRef.current[payload.index]);
          endHint();
          break;
        }
        case "hint_end": {
          if (payload.sessionId !== hintSessionRef.current) return;
          endHint();
          break;
        }
        case "tabs": {
          // A switch to a different tab clears every piece of the page's own per-conversation state (the model
          // clears its part) before the new tab's snapshot arrives. The first `tabs` this page sees takes this
          // branch too, since `activeTabRef.current` starts `null`.
          if (payload.active !== activeTabRef.current) {
            switchedRef.current = true;
            // A switch ends VISUAL, and first: `saveView` below reads `modeRef.current` to park the old tab's
            // mode, and `exitRegion` sets that ref synchronously, so a mode this page cannot re-enter is never
            // what gets saved.
            exitRegion();
            // A sequence pending in the old tab's conversation means nothing about the new one.
            dropPendingKeys();
            // The tab stops being active, so whatever has not yet been mirrored goes now rather than waiting out
            // the rest of its debounce against a tab nobody is reading.
            flushDraft();
            const previous = activeTabRef.current;
            // A `j`/`k` still easing the view is finished first: the old tab's saved view is where that press was
            // going, and the ease must not go on carrying the new tab toward the old one's target.
            const leftList = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
            if (leftList !== null) settleListScroll(leftList);
            // The old tab's view is saved from the live refs, before the resets below overwrite the render state
            // they mirror, and only when there is an old tab.
            if (previous !== null) {
              saveView(viewStore.current, previous, captureView(leftList));
            }
            // An arrival park names a row of the tab just left.
            arrivalParkRef.current = null;
            restoreRef.current = takeView(viewStore.current, payload.active) ?? null;
            // The box goes empty on a switch, never the left tab's words: Rust's own `draft` envelope for the new
            // tab follows in the same batch, so this is never what the reader sees for longer than one dispatch.
            restoreSeq.current += 1;
            draftRef.current = "";
            setRestoredDraft({ text: "", seq: restoreSeq.current });
            setCursor(0);
            setExpanded({});
            setDetailed(false);
            // A switch always lands BROWSE, never whatever mode the tab was left in; nothing later in the switch
            // (the `snapshot` arm's restore included) may set it back to INPUT. The first `tabs` is no switch:
            // launch keeps its INPUT.
            setMode("browse");
            if (previous !== null) setEmptyLanding("browse");
            setKeymapOpen(false);
            setDetail(null);
            setHandoffOpen(false);
            // An open `/model`/`/effort` picker answers through `chooseSlashOption`, which sends to whatever tab
            // is active: left open across a switch, a row chosen from it went to the new, wrong tab. The armed
            // reply is a single flag, not scoped to a tab, and the old tab's reply is now dropped as another
            // tab's, so it is cleared here too rather than matched against this tab's next `turn_completed`.
            setSlashPicker(null);
            setSlashReply(null);
            pendingSlashPickerRef.current = null;
            // A `gf` or `gx` pick names paths or links of the old tab's conversation; left set, its next letter
            // opened one of them whichever tab had the keys.
            setPathPick(null);
            setLinkPick(null);
            // A y/n prompt, the `/` prompt and the `:` line were over the old tab's conversation.
            cancelBypassConfirm();
            setSearch(null);
            setExLine(null);
            answeredRef.current = new Set();
            setAnsweredPermissions(answeredRef.current);
            permissionReasons.current.clear();
            // The old tab's unread pill means nothing over a fresh, empty timeline. The `snapshot` arm's restore
            // seeds a real threshold back only for a tab whose saved view was actually parked.
            setUnreadLabel(null);
            unreadJumpRef.current = () => { };
            unseenAfterSeqRef.current = null;
          }
          activeTabRef.current = payload.active;
          forgetClosed(
            viewStore.current,
            payload.tabs.map((t) => t.id),
          );
          break;
        }
        case "tab_detail": {
          // The reply to `open_detail` (the band, `prefix i`): the popover opens on this tab's rows with the
          // cursor at the top, in BROWSE, and the keys overlay closes -- the two are mutually exclusive over the
          // conversation. A route away from a y/n prompt (the popover's own `y` copies a row) and from VISUAL.
          endKeyPrompts();
          exitRegion();
          setDetail(payload.rows);
          setDetailCursor(0);
          setMode("browse");
          setKeymapOpen(false);
          break;
        }
        case "snapshot": {
          // The view a switch saved for this tab, if it had one: cursor and expansions now (never the mode: the
          // switch lands BROWSE), the scroll position in the layout effect keyed on `restoreTick`, since it needs
          // the row this cursor lands on to exist in the DOM first.
          if (restoreRef.current !== null) {
            const view = restoreRef.current;
            // `landingRef` is set before the cursor moves so the `[cursor]` reveal effect does not fight this
            // restore with its own scroll -- and only when the cursor really moves: a `"keep"` for a move that
            // changes nothing fires no effect, so nothing consumes it, and it swallowed the next move's reveal. A
            // view that was following the end comes back on the last row instead (below), since rows kept
            // arriving while the tab was away.
            if (!view.atBottom) {
              if (view.cursor !== cursorRef.current) landingRef.current = "keep";
              setCursor(view.cursor);
            }
            setExpanded(view.expanded);
            setDetailed(view.detailed);
            setRestoreTick((n) => n + 1);
            // A parked view whose unread threshold was saved gets it seeded back into `MessageList`. A counter
            // of its own, not `restoreTick`: a `null -> value -> null -> value` sequence across three switches
            // must still re-fire the seed effect on the third.
            if (!view.atBottom && view.unseenAfterSeq !== null) {
              unseenSeedTick.current += 1;
              setUnseenSeed({ afterSeq: view.unseenAfterSeq, tick: unseenSeedTick.current });
            }
          }
          // A switch that lands on a tab already holding a card, while the panel has the keys, puts them on it --
          // the landing `Ctrl+l` gives within one tab. Read off the envelope (the timeline still reflects the old
          // tab until this batch renders); the `permissionRequest` effect finds the row once it does. A tab with
          // no saved view (a reload, a first mount, a tab never shown here) opens at the bottom, so the cursor
          // goes to the last row, where the view is; the card landing still wins over it.
          if (switchedRef.current) {
            switchedRef.current = false;
            if (restoreRef.current === null || restoreRef.current.atBottom) landOnLastRef.current = true;
            if (paneFocusedRef.current && payload.state.pendingPermissions.length > 0) setPermissionRequest((n) => n + 1);
          }
          break;
        }
        case "events": {
          // A new turn resets the render trace: each turn reports its own first text, once.
          if (payload.events.some((e) => e.type === "turn_started")) {
            firstTextReceivedAt.current = null;
            renderReportSent.current = false;
          }
          // A sent prompt takes the cursor once its row exists (the `[timeline]` effect reads and clears this).
          if (payload.events.some((e) => e.type === "user_prompt_submitted")) landOnPromptRef.current = true;
          for (const event of payload.events) {
            // The turn right after a bare `/model` or `/effort` send is the one candidate for a picker: armed by
            // `sendMessage`, read and cleared on the very next `turn_completed`, parsed either way, so a later
            // turn is never mistaken for the reply. `result_text` is the one field both backends fill with a
            // local command's reply (on the sidecar backend it otherwise arrives as ordinary assistant text). The
            // picker opens through `slashReply`, whose effect waits until no other overlay is open.
            if (event.type === "turn_completed") {
              const pending = pendingSlashPickerRef.current;
              pendingSlashPickerRef.current = null;
              if (pending === "model") {
                const parsed = parseModelReply(event.result_text);
                if (parsed !== null) setSlashReply({ kind: "model", options: parsed.options, current: parsed.current });
              } else if (pending === "effort") {
                const parsed = parseEffortReply(event.result_text);
                if (parsed !== null) setSlashReply({ kind: "effort", options: parsed.options, current: null });
              }
            }
          }
          // Stamped at receipt, so the span covers the work being measured rather than starting after it.
          if (
            firstTextReceivedAt.current === null &&
            payload.events.some((e) => e.type === "content_delta" && e.kind === "text" && e.text !== "")
          ) {
            firstTextReceivedAt.current = receivedAt;
          }
          break;
        }
        case "command_result": {
          setPendingCommands((prev) => {
            const next = new Set(prev);
            next.delete(payload.requestId);
            return next;
          });
          // The record as it was before this envelope ended it; the model has already put the banner, the
          // overlay's status and the handoff bookkeeping where they go (see `CommandRecord`).
          const record = previousPage.inFlight.get(payload.requestId);
          if (!payload.ok) {
            console.warn("agent-ui: command failed", payload.requestId, payload.error);
            if (record?.kind === "trust" || record?.kind === "review-command" || record?.review === true || record?.kind === "review") break;
            if (record?.kind === "picker-send") {
              // The picker's choice never touched the box, so nothing is put back: a footer flash alone, leaving
              // a draft already in the box exactly as the user set it.
              pendingSlashPickerRef.current = null;
              showFlash(payload.error);
            } else if (record?.kind === "send") {
              // A refused send never started a turn, so a picker it armed must not wait to be matched against
              // some later, unrelated reply -- whichever tab the refusal names, since the flag is not per tab.
              pendingSlashPickerRef.current = null;
              if (record.tab === activeTabRef.current) {
                // The composer cleared this optimistically; Rust refused it, so it goes back. `draftRef` names
                // exactly what the box holds now: if nothing was typed since the clear the refused text comes back
                // alone, else it goes before what was typed, separated by a newline, so neither is lost. Mirrored
                // to Rust here, once, so a later switch brings the recovered text back too (not in the
                // composer's restore effect, which also fires on a switch's clear and on Rust's own echo).
                const typedSince = draftRef.current;
                const recovered = typedSince === "" ? (record.text ?? "") : `${record.text ?? ""}\n${typedSince}`;
                restoreSeq.current += 1;
                setRestoredDraft({ text: recovered, seq: restoreSeq.current });
                mirrorDraft(recovered);
                refusedEchoRef.current = { tab: record.tab, requestId: payload.requestId, at: performance.now() };
              }
            } else if (record?.kind === "editor" || record?.kind === "link" || record?.kind === "tabnew" || record?.kind === "stop-background") {
              // A footer nicety, not the banner reserved for conversation-breaking errors.
              showFlash(payload.error);
            } else {
              // Refused, so not answered: Rust leaves the card waiting, and so must this panel -- otherwise `a`/`d`
              // and the card's own buttons stay dead until a reset.
              if (record?.kind === "permission" && record.permissionId !== undefined && record.tab === activeTabRef.current) {
                const permissionId = record.permissionId;
                if (answeredRef.current.has(permissionId)) {
                  const next = new Set(answeredRef.current);
                  next.delete(permissionId);
                  answeredRef.current = next;
                  setAnsweredPermissions(next);
                }
              }
            }
          }
          break;
        }
        case "handoff":
        case "error": {
          // The model resets the conversation, which neither ends the session as the page sees it nor changes
          // `mode`, so VISUAL -- and the conversation it froze -- would outlive what it was selecting in: the
          // restarted tab drew under the old frozen conversation (its cards included) until an Esc, and the empty
          // tab refused Shift+Tab as "Esc first" with no VISUAL handler left to take that Esc. After a handoff the
          // session really is closed, so there is nothing left to confirm either.
          exitRegion();
          setKeymapOpen(false);
          setHandoffOpen(false);
          break;
        }
        case "begin_rename": {
          // `prefix ,`: an inline field over this tab, prefilled and selected. Like every overlay that takes the
          // keys it ends a pending sequence and VISUAL and closes the other overlays. `prefix ,` is GTK's, so a
          // name starting with `y` typed into the field answered a y/n prompt left open; Rust drops its own too.
          // The `/` prompt and the `:` line close for the chooser's reason: committing or cancelling hands the
          // keys to the conversation root.
          dropPendingKeys();
          exitRegion();
          closeTransientOverlays();
          endKeyPrompts();
          setSearch(null);
          setExLine(null);
          break;
        }
        case "confirm_close":
        case "confirm_close_others":
        case "confirm_bypass":
        case "confirm_restore": {
          // Drawn in the footer in place of the which-key strip, taking every key (`onKeyDown` checks it before
          // every other overlay). An overlay that takes the keys ends a pending sequence, and VISUAL, whose own
          // `y` would otherwise copy instead of answering. The chooser is left as the model leaves it: open under
          // a bypass prompt.
          dropPendingKeys();
          exitRegion();
          closeTransientOverlays();
          break;
        }
        case "chooser": {
          // `prefix w`: ends a pending sequence and VISUAL, and closes the overlays that would fight it for the
          // keys -- the `/model`/`/effort` picker too, open or still on its way, since the bare command it waits
          // for was sent before the user turned to something else.
          dropPendingKeys();
          exitRegion();
          closeTransientOverlays();
          setSlashPicker(null);
          setSlashReply(null);
          pendingSlashPickerRef.current = null;
          // Opening the chooser ends a y/n prompt; the reverse is not true (a bypass prompt opens over it).
          endKeyPrompts();
          // Every way out of the chooser hands the keys to the conversation root, so a `/` prompt or `:` line
          // left open under it stayed drawn without the keys: `:`, then `prefix w` and Esc, then `l` Enter
          // walked onto Approve and pressed it.
          setSearch(null);
          setExLine(null);
          break;
        }
        case "notice": {
          showFlash(payload.text);
          break;
        }
        case "draft": {
          // The page this tab just left sent its last mirror after the tab arrived here, and the box has been
          // edited since: that edit is newer, and its own mirror tells Rust so.
          if (payload.late === true && !(givenDraftRef.current?.tab === payload.tab && draftRef.current === givenDraftRef.current.text)) break;
          // The echo of a refused send this page put back itself: Rust's copy can trail what was typed since, and
          // the box already holds the message and everything typed after it.
          if (takeRefusedEcho(payload.tab, payload.refused)) break;
          // Otherwise Rust's copy wins; it is sent only on a switch, `ready`, a reset or a scratch return.
          restoreSeq.current += 1;
          draftRef.current = payload.text;
          givenDraftRef.current = { tab: payload.tab, text: payload.text };
          pendingDraftRef.current = null;
          setRestoredDraft({ text: payload.text, seq: restoreSeq.current });
          break;
        }
      }
    });
    requestHello();
    // A page mounted again in the same document (a dev reload) starts unslowed; a real reload is a
    // new document, which has no attribute to begin with, and Rust tells it again on `ready`.
    return () => applyEditorTyping(document.documentElement, false, 0);
  }, []);

  /** Mirrors the composer's key mode. `browse`/`input` describe the live tab or empty tab without
   *  an overlay; other states, including an ended session, map to `other`. Recomputed each render
   *  so the posting effect compares current values. Declared after the mount effect because React
   *  runs effects in source order and `ready` must be posted first.
   *  Before the first `tabs` envelope, `activeTab` is `null` and `EmptyTab` is not mounted. Map that
   *  screen to `other` so Rust cannot claim `Ctrl+k` for a composer that does not exist. */
  const panelKeysMode: PanelKeysMode =
    overlayOpen || keymapOpen || detail !== null || confirm !== null || pathPick !== null || linkPick !== null
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
  /** V1 C1, "stale mirror, both ways": every `nav_key` this page cannot apply comes back
   *  as one `nav_fallthrough`, so Rust runs the chord's ordinary `move_focus` instead of dropping the
   *  key. `keymapOpen`/`confirm` are checked here rather than left to `EmptyTab`:
   *  they are this component's own state, drawn over its layout, and `EmptyTab` has no prop carrying
   *  either -- forwarding `navKey` to it unfiltered would let it act on a request this effect has
   *  already answered. `detail`/`pathPick`/`linkPick` are live-tab-only by construction (none ever renders
   *  while `!sessionStarted`), so they are read only inside that branch below. */
  useEffect(() => {
    if (navKey === null) return;
    const { direction } = navKey;
    if (overlayOpen || keymapOpen || confirm !== null) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    // Before the first `tabs` envelope, `EmptyTab` is not mounted. Queuing `nav_key` in `emptyNavKey`
    // would seed `navKeySeenRef` at mount and silently discard the key. Send `nav_fallthrough`
    // instead, as for other screens without a composer.
    if (activeTab === null) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    if (!sessionStarted) {
      setEmptyNavKey(navKey);
      return;
    }
    if (detail !== null || pathPick !== null || linkPick !== null || sessionEnded) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    // `Ctrl+j` enters INPUT and ends any CARET, VISUAL or V-LINE region. Include region modes here so
    // the chord cannot fall through to Rust's `move_focus` instead.
    if (direction === "down" && (mode === "browse" || isRegionMode(mode))) {
      if (isRegionMode(mode)) exitRegion();
      // Keep the caret where it was left, as `i`/`o` do. Set `composerCaret` explicitly so this chord
      // cannot reuse an earlier `A`'s end-of-draft setting.
      setMode("input");
      setComposerCaret("kept");
      setComposerFocusRequest((n) => n + 1);
      return;
    }
    if (direction === "up" && mode === "input") {
      setMode("browse");
      return;
    }
    // Stale: the mirror said browse/input but this render's `mode` no longer agrees.
    postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [navKey]);

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
    markLandedCode(null);
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
    if (target.kind === "control" || target.kind === "link") {
      // An input (a permission card's reason box) takes the keys by being focused; a button is
      // selected by being focused, drawn as the solid cursor block. A web link is
      // the same landing: focused, never clicked, its row current -- and Enter on it is then the browser's
      // own activation (`LinkClicked`), not a key this panel claims.
      // A control inside a conversation row (a card's Approve) also brings the row cursor to that
      // row, the same place `l` would have reached it from: otherwise `h` from it would hand the
      // keys back to some other row, and `a`/`d` would answer a different card.
      const row = root === null ? null : rowOf(root, target.el);
      const rowIndex = root === null || row === null ? null : rowIndexOf(root, row);
      if (rowIndex !== null) setCursor(rowIndex);
      landedControlRef.current = target.el;
      setMode("browse");
      target.el.focus();
      // Save a landing only if focus took; disabled controls cannot take focus.
      // HINT labels do not reach this page, so the current key count identifies the landing
      // for the next Enter, which may press this card button.
      if (document.activeElement === target.el) placedRef.current = { el: target.el, atKey: typingGuard.keyCount() };
      return;
    }
    // A row or a code block: the row cursor goes there and the panel is back in BROWSE. The row's
    // index is read now, from the element, never taken from `target.rowIndex`: rows can be added
    // above it while the labels are up (a permission card is anchored right after its tool call),
    // and the index frozen at `hint_collect` would then name a different row.
    const row = target.kind === "row" ? target.el : root === null ? null : rowOf(root, target.el);
    const rowIndex = root === null || row === null ? null : rowIndexOf(root, row);
    if (rowIndex === null) return;
    setMode("browse");
    if (target.kind === "code") {
      markLandedCode(target.el);
      // Reveal the code block itself. On a row taller than the view with its top outside
      // and bottom inside, `revealRow` with `nearest` aligns the row's bottom with the list's.
      // That can scroll the newly landed block off the top.
      if (rowIndex !== cursorRef.current) landingRef.current = "keep";
      target.el.scrollIntoView({ block: "nearest" });
    }
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
    if (message.type === "edit_draft") trackRequest(requestId, { kind: "editor", tab });
    postToRust({ ...message, request_id: requestId, tab } as unknown as OutboundMessage);
  }

  function handoffToTerminal() {
    const tab = activeTabRef.current;
    if (tab === null) return;
    // Reached by clicks (the detail popover's row, then the confirm's button) that never pass through
    // `answerConfirm`, and the tab it acts on stops being the tab a bypass prompt was drawn for: that
    // prompt goes.
    cancelBypassConfirm();
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    trackRequest(requestId, { kind: "handoff", tab });
    // Set BEFORE the post, so the composer is disabled from this moment rather than from whenever a
    // reply comes back. Rust takes the session out of its own state inside the handler this message
    // reaches, and every send after that point would be refused.
    setHandoffRequests((current) => new Map(current).set(tab, requestId));
    setCommandNotice(null);
    postToRust({ type: "handoff_to_terminal", request_id: requestId, tab });
  }

  /** Sends typed text. On a `NotStarted` tab, starts the backend lazily with the tab's
   *  remembered mode, without a separate `start_session` message. `chooseSlashOption` passes
   *  `fromPicker: true` so `inFlight` records a `"picker-send"`. Both paths post `send_message`,
   *  but a refused picker send must not restore its command text into the composer. */
  function sendMessage(text: string, options?: { fromPicker?: boolean }) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    // The text is kept so a refusal can put it back (ordinary sends only -- `"picker-send"`'s own
    // refusal never reads `record.text` back into the box). Dropped again as soon as the reply
    // arrives.
    trackRequest(requestId, { kind: options?.fromPicker ? "picker-send" : "send", tab, text });
    setCommandNotice(null);
    // Arms `pendingSlashPickerRef` for the `events` handler before the post is answered.
    // The `command_result` handler clears it on refusal because a send that never ran cannot
    // start the awaited turn. A picker choice carries an argument, so `barePickerCommand`
    // returns `null` for it and both send paths can use this assignment.
    // External-agent tabs receive slash commands as typed and do not open this picker.
    pendingSlashPickerRef.current = isExternalAgent(activeTabInfo(tabsRef.current)?.agent) ? null : barePickerCommand(text);
    postToRust({ type: "send_message", request_id: requestId, tab, text });
    // A send follows the reply, wherever the reader had scrolled (`./follow.ts`).
    resumeFollowing(containerRef.current?.querySelector(".message-list"));
  }

  /** The picker's own Enter (`./components/SlashPicker`): sends the choice as an ordinary turn
   *  (never held back -- `barePickerCommand` only ever names a BARE command, and this always
   *  carries an argument) and closes the picker immediately, the same way choosing a chooser row
   *  closes the chooser. */
  function chooseSlashOption(value: string) {
    if (slashPicker === null) return;
    const command = slashPicker.kind === "model" ? "/model" : "/effort";
    setSlashPicker(null);
    sendMessage(`${command} ${value}`, { fromPicker: true });
    // Restore focus after the picker unmounts so the keys do not stay on `<body>`. `takeKeys` runs
    // after the render that closed it.
    setKeysRequest((n) => n + 1);
  }

  /** Closes the picker on Escape/q, sends nothing, and returns the keys,
   *  as `chooseSlashOption` does. */
  function cancelSlashPicker() {
    setSlashPicker(null);
    setKeysRequest((n) => n + 1);
  }

  /** Closes an overlay whose render throws through its own leave path and names it in
   *  the band. The leave path also returns the keys where needed. Each overlay's
   *  `PanelErrorBoundary` calls this through `onError`, keeping the rest of the panel mounted. */
  function overlayFailed(what: string, close: () => void) {
    close();
    showFlash(`the ${what} failed and closed`);
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
    trackRequest(requestId, { kind: "send", tab, text });
    setCommandNotice(null);
    postToRust({ type: "queue_message", request_id: requestId, tab, text, turn_id: stateRef.current.activeTurnId });
  }

  function interrupt() {
    post({ type: "interrupt" });
  }

  /** Resets an ended tab with `r`. `TabSet::reset` combines the queue with Rust's saved draft.
   *  Recovered refusal text uses the normal 300ms draft debounce, so flush any pending mirror
   *  before `reset_tab`. Otherwise Rust could reset from stale text and its draft echo could clear
   *  the pending mirror, losing the recovered text. `flushDraft()` is a no-op with nothing pending. */
  function resetTab() {
    flushDraft();
    post({ type: "reset_tab" });
  }

  /** Every answer to a card goes through here -- its own buttons, Enter in its reason box, and
   *  `a`/`d` -- so one card is answered once, whichever of them gets there first. */
  function answerPermission(permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) {
    const tab = activeTabRef.current;
    if (answeredRef.current.has(permissionId) || tab === null) return;
    answeredRef.current = new Set(answeredRef.current).add(permissionId);
    setAnsweredPermissions(answeredRef.current);
    // Recorded, so a refusal gives the card back (`inFlight`'s doc).
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "permission", tab, permissionId });
    postToRust({
      type: "permission_response",
      request_id: requestId,
      tab,
      permission_id: permissionId,
      decision,
      reason,
      ...(remember ? { remember: true } : {}),
    });
  }

  /** Commit or cancel an inline rename and return the keys to the root. Send `renaming.tab`
   *  because the field can stay open across a focus-free switch with `n`/`p`/digits/`l`. Using
   *  `post` would read the new active tab instead. `answerConfirm` likewise sends a `"close"`
   *  prompt's `confirm.tab` directly; `"close_others"` has no tab and posts at window scope. */
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

  /** Opens the `:` line empty and hands it the keys; one already open is wiped, as vim's `:` gives a fresh line. */
  function openExLine() {
    setSearch(null);
    setExLine("");
    setLineFocusRequest((n) => n + 1);
  }

  /** Hands the keys back to whichever layout is on screen -- the conversation's own root, or the
   *  start screen's while no session has started yet. The same fallback `hint_collect` and the
   *  document-`<body>` replay above already use. */
  function returnKeysToRoot() {
    if (containerRef.current !== null) {
      containerRef.current.focus({ preventScroll: true });
      return;
    }
    // The start screen owns its BROWSE root. Its arrival edge focuses that root and leaves
    // INPUT while keeping the draft, just as returning to the conversation's root does.
    setEmptyLanding("browse");
    setArriveRequest((n) => n + 1);
  }

  /** Feeds Enter and Esc from the `/` and `:` lines to the typing guard before their
   *  callbacks run. `SearchBar` stops those keys, so `onKeyDown` never sees them; otherwise
   *  an immediate `a` after closing a line could answer a card as though it stood alone.
   *  Uses the same `event.timeStamp` clock as `onKeyDown`. */
  function noteLineKey(event: KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) {
    // This key is not a kill, so a following `Ctrl+y` is judged on its own.
    lastKeyWasKillRef.current = false;
    const cancelled = typingGuard.onKey(event.key, event.timeStamp > 0 ? event.timeStamp : performance.now());
    if (cancelled !== null) showFlash(cancelled);
  }

  /** The `prefix w` chooser lists open tabs before saved sessions. Every callback closes the
   *  overlay and returns keys to the panel root. It does not open at launch or hand keys to the
   *  editor on exit. */
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
    setConfirm({ kind: "close", tab, lines: [`close ${label}? (y/n)`] });
    returnKeysToRoot();
  }
  function onChooserLeave() {
    setChooser(null);
    // Closing the chooser returns to the visible layout's BROWSE root, with the start screen
    // using the same arrival edge as other keyboard arrivals.
    if (containerRef.current !== null) {
      containerRef.current.focus({ preventScroll: true });
      return;
    }
    setEmptyLanding("browse");
    setArriveRequest((n) => n + 1);
  }
  /** The chooser's own `New session` row: the same
   *  window-level `tab_verb new` the `tab.new` action already posts (`handleAction` above),
   *  reached from a second place. */
  function onChooserNewSession() {
    postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "new" });
    setChooser(null);
    returnKeysToRoot();
  }
  /** Renames an open tab from its chooser row with `Ctrl+r`. The chooser keeps local rename state
   *  in `Chooser.tsx`; `begin_rename`/`renaming` belong to the tab bar's overlay, which is not
   *  visible while the chooser is open. */
  function onChooserRenameTab(tab: TabId, name: string) {
    postToRust({ type: "rename_tab", request_id: nextRequestId(), tab, name });
  }
  /** `Shift+Tab` over `New session` or a record: the active tab's own mode
   *  when it has not started yet -- a resume would land in it -- else the window's remembered
   *  default, since a resume then opens a new tab. Either way Rust re-sends `tabs` with the new
   *  value, so the chooser's mode line follows without being re-sent itself. */
  function onChooserCycleMode() {
    const info = activeTabInfo(tabs);
    if (info !== null && info.state === "not_started") post({ type: "cycle_mode" });
    else postToRust({ type: "cycle_default_mode", request_id: nextRequestId() });
  }
  /** `Shift+Tab` on an open tab's own row: that tab's own toggle, named by the
   *  row -- `Chooser` offers it only where Rust acts on it (leaving bypass on any tab, entering it on
   *  the active one). The chooser stays open; a bypass prompt this raises is answered inside it. */
  function onChooserCycleTabMode(tab: TabId) {
    postToRust({ type: "cycle_mode", request_id: nextRequestId(), tab });
  }

  /** Cancels bypass and restore prompts when the pane loses focus, the active tab changes,
   *  or keys move to the terminal. Rust also drops its prompt, so the nonce rejects a late `y`.
   *  Leaves close confirmations alone. Routes to another control inside the panel call
   *  `endKeyPrompts` instead, so text entered there cannot answer an earlier close prompt. */
  function cancelBypassConfirm() {
    setConfirm((c) => (c?.kind === "bypass" || c?.kind === "restore" || c?.kind === "trust" || c?.kind === "stop_background" ? null : c));
    setHeldTrust(null);
  }

  /** Opens `<leader>k`'s question for `tab`, naming `running` commands and the tab's agent. Like the other y/n
   *  prompts it takes the keys from whatever was open over the conversation. */
  function askStopBackground(tab: TabId, running: number) {
    dropPendingKeys();
    exitRegion();
    setDetail(null);
    setHandoffOpen(false);
    setKeymapOpen(false);
    const word = agentWord(tabsRef.current?.tabs.find((t) => t.id === tab)?.agent);
    const who = word === null ? "the agent" : word === "codex" ? "Codex" : word;
    setConfirm({ kind: "stop_background", tab, running, lines: [stopBackgroundQuestion(who, running)], openedAt: performance.now() });
  }

  /** What a route that hands the keys to something else ends -- every
   *  pending y/n prompt (`close`, `close_others` and `bypass`, which `cancelBypassConfirm` leaves two of) and a
   *  `gf` letter picker. Both answer the next key they are given, ahead of whatever that key was meant for:
   *  `answerConfirm` runs in the root's capture phase and claims every key while any `confirm` is set, and the
   *  picker's branch of `onKeyDown` runs ahead of the rest of it. So a `y` typed into a rename field, the chooser,
   *  the composer or a card's reason box, which the route has just put the keys in, closed a tab; a letter opened a
   *  file. Called by every such route (`open_command_line`, `begin_rename`, `chooser`, `tab_detail`, `open_keymap`,
   *  `hint_collect`, `enter_input`, `arrive`, `focus_permission`, and the two keys GTK claims, `nav_key` and
   *  `literal_key` -- for which "any key but y cancels" is the prompt's own rule). NOT by `pane_focus` or a switch to
   *  another tab, which take the keys nowhere: a close prompt names its tab by id and stays drawn in the band, so
   *  the `y` that answers it is still a `y` to it (those two keep `cancelBypassConfirm`). Only state setters, so a
   *  listener installed once, with the first render's closure, may call it. */
  function endKeyPrompts() {
    setConfirm(null);
    setPathPick(null);
    setHeldTrust(null);
  }

  /** Handles confirmation keys before other controls in both the conversation and empty layouts.
   *  Called in capture phase at each root and stops propagation of claimed keys, preventing a
   *  chooser from answering twice or an input from sending, renaming or closing before cancellation.
   *  Returns whether it claimed the key.
   *
   *  For a bypass prompt, only a plain, non-repeated `y`/`Y` passing the timing guard confirms.
   *  Other non-modifier keys cancel the switch. A `y`/`Y` too soon after the prompt or another key
   *  cancels with a specific flash. The prompt closes even on refusal, so another stray key cannot
   *  retry without a fresh envelope. `lastKeyAtRef` observes keys throughout the panel.
   *
   *  Feed claimed keys to the typing guard here because they never reach `onKeyDown`.
   *  Otherwise a key that cancels a prompt would be invisible to a later card approval's guard.
   *  Repeats never confirm, since a fresh prompt may arrive while the same key is still held.
   *
   *  After cancellation, swallow that key's repeats until keyup through `promptSwallowKeyRef`,
   *  even with `confirm === null`. Otherwise a held `y` could copy a row and overwrite the refusal
   *  flash. A fresh prompt still reaches the normal branch, which rejects repeats itself. */
  function answerConfirm(event: KeyboardEvent<HTMLElement>): boolean {
    // The keys have moved into a text field (a click on the composer, say) while the restore question
    // was up: what is typed there is text, never an answer -- the `y` of "you should" must not give a
    // tab bypass. The question ends and the key is left for the field.
    if (confirm?.kind === "restore" && isEditableElement(event.target)) {
      setConfirm(null);
      return false;
    }
    if (confirm === null) {
      if (promptSwallowKeyRef.current !== null && event.key === promptSwallowKeyRef.current && event.repeat) {
        event.preventDefault();
        event.stopPropagation();
        return true;
      }
      return false;
    }
    event.stopPropagation();
    // `isModifierKey`, not a four-name list: WebKitGTK names the Super key `"Super"`, not `"Meta"`, so
    // holding it used to cancel the prompt by itself, silently (sandbox pass, 2026-09-28).
    if (!isModifierKey(event.key)) {
      event.preventDefault();
      promptSwallowKeyRef.current = event.key;
      typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, event.timeStamp > 0 ? event.timeStamp : performance.now());
      if (confirm.kind === "bypass") {
        if (event.key === "y" || event.key === "Y") {
          // A `y` with modifiers or during input method composition must not confirm bypass; timing alone
          // cannot distinguish it from a lone `y`. Shift is allowed for `Y`. Share
          // `isPlainAnswerKey` with card answers so Super, Hyper and AltGraph are checked too. Pass
          // `event.nativeEvent`, which carries `getModifierState`, as `resolveKey` does.
          const plain = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike);
          if (
            plain &&
            !event.repeat &&
            bypassYesCounts({ now: performance.now(), openedAt: confirm.openedAt, lastKeyAt: lastKeyAtRef.current })
          ) {
            postToRust({
              type: "confirm_bypass",
              request_id: nextRequestId(),
              tab: confirm.tab,
              scope: confirm.scope,
              nonce: confirm.nonce,
            });
          } else {
            showFlash("y must be pressed on its own to enter bypass — Shift+Tab to ask again");
          }
        }
        setConfirm(null);
        return true;
      }
      if (confirm.kind === "trust") {
        // Escape alone works from inside a text field: it starts nothing. Every other key typed there is the
        // field's text, so it is taken and never answers.
        if (event.key !== "Escape" && isEditableElement(event.target)) {
          showFlash(trustFooter(confirm.view.remember, confirm.view.needsYes === true, confirm.view.state));
          return true;
        }
        const pendingG = trustPendingGRef.current;
        trustPendingGRef.current = false;
        const action = resolveTrustKey(event.nativeEvent as unknown as TrustKeyEvent, {
          now: performance.now(),
          openedAt: confirm.openedAt,
          lastKeyAt: lastKeyAtRef.current,
          pendingG,
          onlyN: confirm.view.state === "too_large",
        });
        if (action.kind === "answer") {
          // Echoes what was drawn: the page never computes either value, so an answer to anything other than
          // the shown findings is refused by Rust.
          const answerId = nextRequestId();
          trackRequest(answerId, { kind: "trust", tab: confirm.tab });
          postToRust({
            type: "trust_answer",
            request_id: answerId,
            tab: confirm.tab,
            nonce: confirm.nonce,
            fingerprint: confirm.view.fingerprint,
            findings_digest: confirm.view.findingsDigest,
            trust: action.trust,
          });
          setConfirm(null);
        } else if (action.kind === "cancel") {
          const cancelId = nextRequestId();
          trackRequest(cancelId, { kind: "trust", tab: confirm.tab });
          postToRust({ type: "trust_cancel", request_id: cancelId, tab: confirm.tab, nonce: confirm.nonce });
          setConfirm(null);
        } else if (action.kind === "scroll") {
          const box = trustOverlayRef.current;
          if (box !== null) box.scrollTop = scrollTarget(action.by, box, rowScrollStep(box));
        } else if (action.kind === "pending_g") {
          trustPendingGRef.current = true;
        } else if (action.kind === "wait") {
          showFlash(TRUST_WAIT_FLASH);
        } else {
          showFlash(trustFooter(confirm.view.remember, confirm.view.needsYes === true, confirm.view.state));
        }
        return true;
      }
      if (confirm.kind === "restore") {
        // `n` is always safe: it brings the bypass tabs back in auto. `y` gives them back in bypass, so
        // it passes every test the bypass prompt's own `y` does: a plain key, not a repeat, and a
        // moment after the prompt appeared with nothing typed around it. Any other key cancels.
        const yes = event.key === "y" || event.key === "Y";
        const no = event.key === "n" || event.key === "N";
        if (yes || no) {
          const plain = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike);
          const counts =
            no ||
            (plain &&
              !event.repeat &&
              bypassYesCounts({ now: performance.now(), openedAt: confirm.openedAt, lastKeyAt: lastKeyAtRef.current }));
          if (counts) {
            postToRust({
              type: "restore_answer",
              request_id: nextRequestId(),
              nonce: confirm.nonce,
              keep_bypass: yes,
            });
          } else {
            showFlash("y must be pressed on its own to restore in bypass — n restores in auto");
          }
        }
        setConfirm(null);
        return true;
      }
      if (confirm.kind === "stop_background") {
        // Any key but a counted `y` puts the question away and stops nothing (`n`, Escape included).
        if (event.key === "y" || event.key === "Y") {
          const plain = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike);
          if (
            !plain ||
            event.repeat ||
            !bypassYesCounts({ now: performance.now(), openedAt: confirm.openedAt, lastKeyAt: lastKeyAtRef.current })
          ) {
            showFlash("y must be pressed on its own to stop them; nothing was stopped");
          } else if (activeTab?.id !== confirm.tab || activeTab?.state !== "live" || state.backgroundCalls === 0) {
            // Not the tab, or not the commands, the question was about any more.
            showFlash(state.backgroundCalls === 0 ? NOTHING_IN_THE_BACKGROUND : "the tab changed; nothing was stopped");
          } else if (state.backgroundCalls !== confirm.running) {
            // The count moved while the question was up: asked again with the count as it is now.
            askStopBackground(confirm.tab, state.backgroundCalls);
            return true;
          } else {
            const stopId = nextRequestId();
            trackRequest(stopId, { kind: "stop-background", tab: confirm.tab });
            postToRust({ type: "stop_background", request_id: stopId, tab: confirm.tab, running: confirm.running });
          }
        }
        setConfirm(null);
        return true;
      }
      // Panel confirmations accept both `y` and `Y`, matching `close_prompt.rs`. Any other
      // non-modifier key cancels.
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

  /** The band's prompt segment. A prompt takes the whole band, so a flash made while the trust question is up
   *  (a `y` that came too soon, a key it does not take) is drawn after the prompt, where it can be read. */
  const confirmBandPrompt =
    confirm === null
      ? null
      : confirm.kind === "trust" && flash !== null && flash.seq > confirm.flashAfter
        ? `${confirm.lines.join(" · ")} · ${flash.text}`
        : confirm.lines.join(" · ");

  /** The trust question, over whichever layout is on screen. A render that throws puts the start off, as
   *  `Escape` would: leaving the tab asking with nothing to answer would strand it. */
  const trustOverlay =
    confirm?.kind === "trust" ? (
      <PanelErrorBoundary
        name="trust prompt"
        onError={() =>
          overlayFailed("trust prompt", () => {
            const cancelId = nextRequestId();
            trackRequest(cancelId, { kind: "trust", tab: confirm.tab });
            postToRust({ type: "trust_cancel", request_id: cancelId, tab: confirm.tab, nonce: confirm.nonce });
            setConfirm(null);
          })
        }
      >
        <TrustPrompt ref={trustOverlayRef} envelope={confirm.view} />
      </PanelErrorBoundary>
    ) : null;

  /** The `:` command line runs only `:tabnew` and trust commands. Enter explains other
   *  commands; Esc and `Ctrl+[` close it silently. It overlays both layouts so a fresh window
   *  can create a tab from the start screen. */
  const commandLineOverlay =
    exLine === null ? null : (
      <PanelErrorBoundary
        name="command line"
        onError={() =>
          overlayFailed("command line", () => {
            setExLine(null);
            returnKeysToRoot();
          })
        }
      >
        <SearchBar
          lead=":"
          label="Command line"
          query={exLine}
          focusRequest={lineFocusRequest}
          onChange={setExLine}
          onAccept={(event) => {
            noteLineKey(event);
            const command = parseExCommand(exLine);
            if (command !== null && command.command === "tabnew") {
              // vim's `:tabnew`: the word names the agent; none is the new-tab key's own tab. Which words
              // name an agent is Rust's to say (`init.lua` may register more), and its refusal, naming
              // every accepted word, is flashed.
              const tabnewId = nextRequestId();
              if (command.agent !== null) {
                trackRequest(tabnewId, { kind: "tabnew", tab: activeTabRef.current ?? 0 });
              }
              postToRust({
                type: "tab_verb",
                request_id: tabnewId,
                verb: "new",
                ...(command.agent !== null ? { agent: command.agent } : {}),
              });
            } else if (command !== null) {
              const commandId = nextRequestId();
              trackRequest(commandId, { kind: "trust", tab: activeTabRef.current ?? 0 });
              // The trust it changes is the agent's of the tab on screen.
              const agent = activeTabInfo(tabsRef.current)?.agent ?? "claude";
              postToRust({ type: "trust_command", request_id: commandId, action: command.command, agent });
            } else {
              showFlash(`:${exLine} — no ex commands here; ? lists this panel's keys`);
            }
            setExLine(null);
            returnKeysToRoot();
          }}
          onCancel={(event) => {
            noteLineKey(event);
            setExLine(null);
            returnKeysToRoot();
          }}
        />
      </PanelErrorBoundary>
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
        // Whether the panel holds the keys, for the stylesheet: the start screen's box is quiet without them.
        data-pane-focused={paneFocused ? "true" : "false"}
        // `.empty-tab` is centred, so a click above or below it lands here; taking focus would put the
        // keys on an element that handles none (the phase-3 GUI pass, 2026-09-25). Its own background
        // only -- a press on anything inside is the browser's.
        onMouseDown={(event) => {
          if (event.target === event.currentTarget) event.preventDefault();
        }}
        // The `?` overlay owns every key while it is open, as it does over a conversation (the
        // live layout's `onKeyDown`); in capture, ahead of `EmptyTab`'s own handler and its
        // composer, which would otherwise take `m`, `j` or a typed letter under it. A y/n prompt
        // comes before even that, in capture for the reason `answerConfirm`'s own doc
        // gives: the chooser's inputs and the composer must not see a key the prompt takes.
        onKeyDownCapture={(event) => {
          if (answerConfirm(event)) return;
          if (!keymapOpen) return;
          onKeymapKey(event);
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
        {/* The stage is the trust question's containing block: it spans everything above the band, so the
            band's own prompt and flashes stay readable while the question is up (an overlay over the whole
            root hid them). Normal flow is the same as without it: one column, `.empty-tab` centred in it. */}
        <div className="empty-stage">
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
              entryKey={panelEntryKey(keymapHelp.window, editorLink !== null)}
              // The first-run hint reads the live prefix instead of assuming the default.
              // This is the same value the `?` overlay shows as `prefixLabel`.
              editorLink={editorLink}
              focusRequest={inputRequest}
              arriveRequest={arriveRequest}
              // `EmptyTab` handles composer focus and root focus itself. Pass requests and overlay state
              // directly because this layout has no `App` conversation `containerRef` to check.
              keysRequest={emptyKeysRequest}
              dropKeysRequest={emptyDropKeys}
              overlayOpen={overlayOpen}
              landing={emptyLanding}
              onModeChange={setEmptyMode}
              // V1 C1: only a `navKey` the effect above found no overlay owning the keys
              // for -- this screen decides the rest itself (a starting/failed tab, its own menu vs.
              // composer), reporting back through `onNavFallthrough` when it cannot apply one either.
              navKeyRequest={emptyNavKey}
              onNavFallthrough={(direction) => postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction })}
              restoredDraft={restoredDraft}
              // `EmptyTab` keeps its own leader sequence and which-key box state, like its local mode. It
              // shares the pure table and `runPanelAction` with the live conversation.
              panelTable={panelTable}
              onPanelAction={runPanelAction}
              typingGuard={typingGuard}
              onExLine={openExLine}
              onFlash={showFlash}
              // Whoever sends from here is typing, and the conversation this starts must open where
              // they are: in INPUT. It mounted in BROWSE, so a second message typed straight away ran
              // as BROWSE keys (the phase-3 GUI pass, 2026-09-25).
              onSend={(text) => {
                setMode("input");
                sendMessage(text);
              }}
              onResume={(id) => post({ type: "resume", provider_session_id: id })}
              onRestoreLast={() => post({ type: "restore_last" })}
              onCycleMode={() => post({ type: "cycle_mode" })}
              onReset={resetTab}
              onHint={requestHint}
              // The dashboard's `w` item: the same window-level
              // `tab_verb choose` `runPanelAction`'s `tab.choose` case posts for `prefix w`.
              onChooseSessions={() => postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "choose" })}
              onDraftChange={mirrorDraft}
              answerConfirm={answerConfirm}
              onQueue={(text) => {
                setMode("input");
                queueMessage(text);
              }}
              history={history}
              queueCount={heldCount(queue)}
              queue={queue}
              queueError={queueError}
              queueNote={queueNote}
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
          {confirm?.kind === "trust" && trustOverlay}
        </div>
        {/* This screen has no `.agent-ui-scroller`. The status band and its `prefix &`
            window-close prompt sit directly under the empty tab; the composer lives inside
            `EmptyTab`. The band replaces the footer and shows editor context in its `context`
            fact. */}
        {commandLineOverlay}
        <StatusBand
          facts={{
            mode: emptyMode,
            pill: modePill(activeTab?.mode ?? "auto", true, true),
            showcmd: null,
            message: flash?.text ?? null,
            prompt: confirmBandPrompt,
            warn: null,
            unread: null,
            cards: 0,
            queued: 0,
            context: contextFact(editorContext),
            link: editorLink,
            position: null,
            model: null,
            usage: null,
            // An empty tab has had no turns.
            ending: null,
          }}
          paneFocused={paneFocused}
          entryKey={panelEntryKey(keymapHelp.window, editorLink !== null)}
        />
        {/* `prefix w`'s chooser can open over an empty tab 1 too -- `.agent-ui-root` is this layout's
            own positioned ancestor (it has no `.agent-ui-scroller` to nest inside). */}
        {chooser !== null && (
          <PanelErrorBoundary name="chooser" onError={() => overlayFailed("chooser", onChooserLeave)}>
            <Chooser
              envelope={chooser}
              tabs={tabs?.tabs ?? []}
              active={tabs?.active ?? null}
              defaultMode={tabs?.defaultMode ?? "auto"}
              projectDir={hello?.projectDir ?? ""}
              newTabChord={keymapHelp.newTabChord}
              focusRequest={chooserFocusRequest}
              dropKeysRequest={emptyDropKeys}
              onSwitch={onChooserSwitch}
              onResume={onChooserResume}
              onNewSession={onChooserNewSession}
              onCloseTab={onChooserCloseTab}
              onRenameTab={onChooserRenameTab}
              onCycleMode={onChooserCycleMode}
              onCycleTabMode={onChooserCycleTabMode}
              onLeave={onChooserLeave}
              answerConfirm={answerConfirm}
            />
          </PanelErrorBoundary>
        )}
        {/* A bare /model or /effort reply opens this picker. Like the chooser above,
            it is positioned in this layout without an `.agent-ui-scroller` to nest inside. */}
        {slashPicker !== null && (
          <PanelErrorBoundary name="picker" onError={() => overlayFailed("picker", cancelSlashPicker)}>
            <SlashPicker
              kind={slashPicker.kind}
              options={slashPicker.options}
              current={slashPicker.current}
              focusRequest={slashPickerFocusRequest}
              onChoose={chooseSlashOption}
              onCancel={cancelSlashPicker}
            />
          </PanelErrorBoundary>
        )}
        {/* `? Keys`, `?` on an empty draft and `prefix ?` open this overlay here too.
            `.agent-ui-root` is this layout's positioned ancestor, as for the chooser. */}
        {keymapOpen && (
          <PanelErrorBoundary name="keys overlay" onError={() => overlayFailed("keys overlay", () => setKeymapOpen(false))}>
            <KeymapOverlay
              ref={keymapOverlayRef}
              onClose={closeKeymapByKey}
              windowKeys={keymapHelp.window}
              prefixKeys={keymapHelp.prefixKeys}
              prefixLabel={keymapHelp.prefix}
              panel={panelTable}
              tmuxSkipped={keymapHelp.tmuxSkipped}
              companion={editorLink !== null}
              plugin={keymapPlugin ?? undefined}
              claudeSlashCommands={!isExternalAgent(activeTab?.agent)}
              agent={isExternalAgent(activeTab?.agent) ? agentTitle(activeTab?.agent) : null}
              modes={hello === null ? undefined : tabPermissionModes(hello.permissionModes, activeTab?.agent)}
            />
          </PanelErrorBoundary>
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
  /** Enter while the turn runs puts the message into it (the agent reads it at its next step) when the
   *  tab's agent says it can; otherwise the message is held for the turn's end. */
  const enterSteers = (queueSteers ?? state.capabilities.steer) === true;
  /* The conversation is on its way out: Rust already owns the backend on a shutdown worker and will
     refuse every command until that finishes. The composer must reflect that rather than accepting
     input it cannot deliver. */
  const handingOff = tabs !== null && handoffRequests.has(tabs.active);

  /** Defers `a`/`d`/`D` on a card and Enter on its button. `stillThere` checks whether
   *  the target can still accept the answer, or is `null` when no card is targeted.
   *  Run `TYPING_GUARD_MS` later only if the key stood alone and nothing cancels the wait.
   *  The card must still accept it and the panel must still be in BROWSE. Only Enter passes
   *  `key` to `TypingGuard.defer` so a quick focus motion followed by Enter can answer. */
  function waitThenAnswer(
    event: KeyboardEvent<HTMLDivElement>,
    typedAt: number,
    stillThere: (() => boolean) | null,
    run: () => void,
    key = "",
  ) {
    if (stillThere === null) {
      const waitingElsewhere = stateRef.current.pendingPermissions.some((p) => !answeredRef.current.has(p.permissionId));
      showFlash(waitingElsewhere ? CARD_ELSEWHERE_FLASH : NO_CARD_FLASH);
      return;
    }
    const waiting = typingGuard.defer(
      typedAt,
      event.repeat,
      () => {
        if (stillThere() && modeRef.current === "browse") run();
      },
      TYPING_FLASH,
      key,
    );
    if (!waiting) showFlash(TYPING_FLASH);
  }

  /** `a`/`d` answer the timeline card at `target` by `permissionId` through `answerPermission`,
   *  just like the card's buttons. A DOM query can include rows drawn inside a model reply,
   *  shifting indexes and answering a different card. `MessageList` draws the cursor on
   *  `timeline[cursor]`, so the timeline item identifies the intended card.
   *  `d` sends the reason already typed in the card's box, like its Deny button. */
  function answerCardByKey(
    event: KeyboardEvent<HTMLDivElement>,
    typedAt: number,
    target: number | null,
    decision: "allow" | "deny",
  ) {
    const item = target === null ? undefined : timeline[target];
    if (decision === "allow" && item?.kind === "permission" && !cardApprovable(item.request)) {
      showFlash(NOT_APPROVABLE_FLASH);
      return;
    }
    const permissionId = item?.kind === "permission" ? item.request.permissionId : null;
    const tab = activeTabRef.current;
    const stillThere =
      permissionId === null
        ? null
        : () => {
            // The same card, in the same tab, still pending and not yet answered; a card whose
            // session ended is inert (its buttons are disabled), so the key is too.
            const now = stateRef.current;
            return (
              activeTabRef.current === tab &&
              !answeredRef.current.has(permissionId) &&
              now.status.kind !== "unavailable" &&
              now.status.kind !== "closed" &&
              now.pendingPermissions.some((p) => p.permissionId === permissionId)
            );
          };
    waitThenAnswer(event, typedAt, stillThere, () => {
      if (permissionId === null) return;
      const reason = decision === "deny" ? permissionReasons.current.get(permissionId) || undefined : undefined;
      answerPermission(permissionId, decision, reason);
    });
  }

  /** INPUT's `Ctrl+y` approves the active tab's oldest waiting card without leaving INPUT.
   *  Only the composer's box can trigger it; the `Ctrl+r` search field contains a query.
   *  With no waiting card, leave the key to the box. Otherwise claim it so the text, caret and
   *  history stay unchanged. Use `TypingGuard.defer` to require `TYPING_GUARD_MS` of quiet before
   *  and after the key, and reject held-key repeats and a yank after `Ctrl+u` or `Ctrl+w`.
   *  At fire time the same card must still wait in the same tab, the session must be live and
   *  the panel must still be in INPUT. Answer by `permissionId` through `answerPermission`,
   *  the same path the card's buttons and `a` use. */
  function approveOldestByKey(event: KeyboardEvent<HTMLDivElement>, typedAt: number, afterKill: boolean) {
    const box = event.target;
    if (!isComposerBox(box)) return;
    const index = sessionEnded ? null : oldestWaitingPermission(timeline, answeredRef.current);
    const item = index === null ? undefined : timeline[index];
    if (item?.kind !== "permission") return;
    event.preventDefault();
    if (!cardApprovable(item.request)) {
      showFlash(NOT_APPROVABLE_FLASH);
      return;
    }
    const permissionId = item.request.permissionId;
    const tab = activeTabRef.current;
    const targetKey = `${tab}:${permissionId}`;
    // Before waiting, refuse recent typing, a preceding composer kill regardless of the pause, or a
    // card that has not stayed named in the band for `TYPING_GUARD_MS`. Each refusal explains
    // why; an approval still advancing the band also fails the last check.
    if (!typingGuard.mayAnswerNow("", typedAt, event.repeat)) {
      showFlash(CTRL_Y_TYPING_FLASH);
      return;
    }
    if (afterKill) {
      showFlash(CTRL_Y_YANK_FLASH);
      return;
    }
    const target = ctrlYTargetRef.current;
    if (target.key !== targetKey || !target.settled) {
      showFlash(CTRL_Y_TARGET_FLASH);
      return;
    }
    const waiting = typingGuard.defer(
      typedAt,
      event.repeat,
      () => {
        const now = stateRef.current;
        if (
          activeTabRef.current !== tab ||
          modeRef.current !== "input" ||
          // The composer must still hold focus when the wait ends. Clicking into search or elsewhere
          // produces no key to cancel the wait.
          document.activeElement !== box ||
          !box.isConnected ||
          // The card must still be the one the band named when the key was pressed.
          ctrlYTargetRef.current.key !== targetKey ||
          answeredRef.current.has(permissionId) ||
          now.status.kind === "unavailable" ||
          now.status.kind === "closed" ||
          !now.pendingPermissions.some((p) => p.permissionId === permissionId)
        ) {
          return;
        }
        answerPermission(permissionId, "allow");
      },
      CTRL_Y_TYPING_FLASH,
    );
    if (!waiting) showFlash(CTRL_Y_TYPING_FLASH);
  }
  // BROWSE visual mode is handled in the root's `onKeyDownCapture` before `onKeyDown`.
  // `resolveKey` resolves VISUAL/V-LINE keys here. Stopping propagation prevents cards,
  // buttons and reason boxes from handling the same key, especially a reason box's Enter.

  function visualRoot(): HTMLElement | null {
    return containerRef.current;
  }

  /** Restricts selection motions and highlight checks to `.message-list`.
   *  `visualRoot()` also contains the activity line, composer and status band.
   *  Using that root would let a motion leave the last row and let `y` copy live controls. */
  function messageListEl(): HTMLElement | null {
    return visualRoot()?.querySelector<HTMLElement>(".message-list") ?? null;
  }

  /** Returns the frozen DOM row containing `caret`, or `null` outside a row.
   *  Shared by `rowKeyOfCaret` and the `scroll-line` line-height calculation.
   *  `rowTextElement` needs a row: reading `.message-list` directly gives a fraction
   *  of the actual text line height. */
  function rowElementOfCaret(caret: Caret): HTMLElement | null {
    const root = visualRoot();
    if (root === null) return null;
    const el = caret.node instanceof Element ? caret.node : caret.node.parentElement;
    if (el === null) return null;
    return rowOf(root, el);
  }

  /** Returns the caret row's key from `frozenTimeline`, or `null` outside a row.
   *  Match rows by key because linked cards arriving during VISUAL can give a row different
   *  positions in the frozen and live timelines. Each row in the frozen DOM belongs to
   *  `frozenTimeline`, and caret movement stays inside `.message-list`. */
  function rowKeyOfCaret(caret: Caret): string | null {
    const root = visualRoot();
    if (root === null || frozenTimeline === null) return null;
    const row = rowElementOfCaret(caret);
    if (row === null) return null;
    const frozenIndex = rowIndexOf(root, row);
    if (frozenIndex === null) return null;
    return frozenTimeline[frozenIndex]?.key ?? null;
  }

  /** Places the live cursor on the row named by `key` without moving the view.
   *  A key absent from the live timeline leaves the cursor alone.
   *  Set `landingRef` to "keep" only when the index changes. An unchanged cursor runs no reveal
   *  effect, so a leftover "keep" would suppress the next unrelated landing's reveal. */
  function landCursorOnRowKey(key: string | null) {
    if (key === null) return;
    const liveIndex = indexOfKey(timeline, key);
    if (liveIndex === null) return;
    if (liveIndex !== cursorRef.current) landingRef.current = "keep";
    setCursor(liveIndex);
  }

  /** Reveals the caret through each scrollable box and the message list using `revealCaret`.
   *  Each box moves only enough to show the caret. `scrollIntoView` on a containing box would
   *  move its ancestors without changing that box's own `scrollTop`.
   *  Announce scrolling for the whole key, including geometry probes before this call.
   *  Compare against `listTopBefore`, captured when the key arrived, so an upward move stops
   *  following even if the caret is already visible here. Other moves use "unknown" direction.
   *  Without layout, use the coarse `scrollIntoView` fallback. */
  function scrollCaretIntoView(caret: Caret, listTopBefore: number | null) {
    const list = messageListEl();
    if (list === null) return;
    const moved = revealCaret(caret, list);
    if (!moved && caretRectMissing(caret)) {
      const startEl = caret.node instanceof Element ? caret.node : caret.node.parentElement;
      startEl?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
    }
    const listMoved = listTopBefore !== null && list.scrollTop !== listTopBefore;
    if (listMoved) noteUserScroll(list, list.scrollTop < listTopBefore! ? "up" : "unknown");
    else if (moved) noteUserScroll(list, "unknown");
  }

  /** Whether `caret` has no usable client rect at all (jsdom, or a node with nothing laid out) --
   *  `scrollCaretIntoView`'s cue to fall back to the element's own `scrollIntoView`. */
  function caretRectMissing(caret: Caret): boolean {
    if (typeof Range === "undefined" || typeof Range.prototype.getBoundingClientRect !== "function") return true;
    const range = document.createRange();
    try {
      range.setStart(caret.node, caret.offset);
    } catch {
      return true;
    }
    range.collapse(true);
    const rect = range.getBoundingClientRect();
    return rect.width === 0 && rect.height === 0 && rect.top === 0 && rect.left === 0;
  }

  /** `v` from BROWSE starts CARET; `V` starts V-LINE. Flash without entering if no row can
   *  supply a caret, including when focus is on a banner or Stop. After a HINT lands on code,
   *  start at that block's first character. Otherwise use the cursor row's first selectable
   *  character on screen. Drop any pending BROWSE count.
   *  The caller captures `landedCodeBlock` before `onKeyDown` clears `copyCodeRef.current`;
   *  reading the ref here would lose the block that HINT just landed on. */
  function enterRegion(line: boolean, landedCodeBlock: HTMLElement | null) {
    const root = visualRoot();
    if (root === null) return;
    if (edgeFocused) {
      showFlash("v selects from a row — j / k onto one");
      return;
    }
    const row = conversationRows(root)[cursor] ?? null;
    if (row === null) {
      showFlash("nothing to select");
      return;
    }
    const codeBlock = landedCodeBlock;
    const container = codeBlock !== null && codeBlock.isConnected && row.contains(codeBlock) ? codeBlock : row;
    const caret = container === row ? entrySelectableCaret(container, messageListEl()) : firstSelectableCaret(container);
    if (caret === null) {
      showFlash("nothing to select");
      return;
    }
    const sel = window.getSelection();
    if (sel === null) return;
    // BROWSE's `v` (`line === false`) starts CARET; `V` (`line === true`)
    // starts V-LINE directly.
    const model: VisualModel = { anchor: caret, cursor: caret, kind: line ? "line" : "caret", goalX: null };
    visualModelRef.current = model;
    visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, model);
    // Discard the BROWSE count when entering the region and start the region's count fresh.
    countRef.current = null;
    visualCountRef.current = null;
    regionPendingGRef.current = null;
    setFrozenSnapshot({ state, expanded, detailed, ruleOffers, answeredPermissions, sessionEnded, turnDurations });
    modeRef.current = line ? "vline" : "caret";
    setMode(modeRef.current);
    root.focus({ preventScroll: true });
  }

  /** Clears region state without changing the mode, so the `[mode]` effect can also clean up
   *  after another route has already left VISUAL/V-LINE and `exitRegion` would return early.
   *  Remove only the selection built here, checked with `selectionMatchesBuild`, leaving a new
   *  mouse selection alone. Clear `frozenSnapshot` too: otherwise `MessageList` keeps showing
   *  stale cards while keyboard answers target the live conversation, possibly in another tab. */
  function clearVisualLeftovers() {
    const sel = window.getSelection();
    const built = visualBuiltRef.current;
    const root = visualRoot();
    if (sel !== null && built !== null && root !== null && selectionMatchesBuild(sel as unknown as SelectionLike, built, root)) {
      sel.removeAllRanges();
    }
    visualModelRef.current = null;
    visualBuiltRef.current = null;
    visualCountRef.current = null;
    regionPendingGRef.current = null;
    countRef.current = null;
    setFrozenSnapshot(null);
  }

  /** Returns the band's label for a region mode: `CARET`, `V-LINE`, or `VISUAL`. */
  function regionModeName(m: PanelMode): string {
    return m === "caret" ? "CARET" : m === "vline" ? "V-LINE" : "VISUAL";
  }

  /** Checks whether `m` is CARET, VISUAL or V-LINE. Exit routes and the `[mode]`
   *  backstop share this check so they all recognize the same region modes. */
  function isRegionMode(m: PanelMode): m is "caret" | "visual" | "vline" {
    return m === "caret" || m === "visual" || m === "vline";
  }

  /** Ends CARET, VISUAL or V-LINE and returns to BROWSE; does nothing outside those modes.
   *  Updates `modeRef.current` synchronously as well as calling `setMode` so an immediate
   *  `saveView` in the `tabs` handler cannot park a region mode that this panel cannot restore. */
  function exitRegion() {
    if (!isRegionMode(modeRef.current)) return;
    clearVisualLeftovers();
    modeRef.current = "browse";
    setMode("browse");
  }

  /** Handles CARET/VISUAL/V-LINE keys in the root's `onKeyDownCapture`, before
   *  `onKeyDown` and descendant handlers. Outside the region, returns without claiming keys
   *  so normal handling can continue. */
  function onVisualKeyDownCapture(event: KeyboardEvent<HTMLDivElement>) {
    const currentMode = modeRef.current;
    if (!isRegionMode(currentMode)) return;
    const root = containerRef.current;
    // Region keys must target the root. If focus escaped without an explicit exit,
    // end the region and swallow this key so a descendant reason box or button cannot act on it.
    if (root === null || event.target !== root) {
      exitRegion();
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    const typedAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
    // Feed every region key to the typing guard so it can cancel a card's deferred
    // `a`/`d`/`D` answer even after the region has started.
    const cancelled = typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typedAt);
    const model = visualModelRef.current;
    if (model === null) {
      exitRegion();
      return;
    }
    // A bare modifier keydown preserves a region-local pending `g`, just as BROWSE's
    // `pendingRef` does. Pressing Shift before `G`/`V`/`$` must not cancel a pending `gg`.
    // Check before reading and clearing `count` and `regionPendingGRef` so both stay intact.
    if (regionPendingGRef.current !== null && isModifierKey(event.key)) {
      if (cancelled !== null) showFlash(cancelled);
      return;
    }
    // Keep region motions, highlight checks and copying inside the message list. The conversation
    // root also contains the activity line, composer and status band, which are outside the
    // frozen region.
    const listEl = messageListEl();
    const count = visualCountRef.current;
    visualCountRef.current = null;
    const pendingG = regionPendingGRef.current;
    regionPendingGRef.current = null;
    const action = resolveKey(currentMode, event.nativeEvent as unknown as KeyLike, {
      sessionEnded,
      count,
      pending: pendingG ?? undefined,
      turnRunning: state.activeTurnId !== null,
    });
    if (action === null) {
      // A null action leaves browser defaults and the current mode alone, including native Ctrl+c
      // copying. Restore the count so a bare modifier before `$` or `V` does not lose it. A
      // consumed pending `g`, such as in `gv`, stays cleared.
      visualCountRef.current = count;
      if (cancelled !== null) showFlash(cancelled);
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const sel = window.getSelection();
    // When a key ends the region, swallow its repeats until keyup through
    // `promptSwallowKeyRef`. Otherwise a held `d` could deny a card, a held `y` could copy a row,
    // or a held `>` could type into the composer after the mode changes.
    // `answerConfirm` checks this ref before this handler on every key, so the repeat cannot
    // reach the BROWSE or INPUT key table.
    function endRegionFromKey() {
      promptSwallowKeyRef.current = event.key;
      exitRegion();
    }
    switch (action.kind) {
      case "count":
        visualCountRef.current = accumulateMotionCount(count, action.digit);
        return;
      case "pending":
        // Arm the first `g` of `gg` or the reserved `gv` pair for the next key.
        regionPendingGRef.current = action.prefix;
        return;
      case "vmove": {
        // Motions accept held-key repeats. `gg`/`G` discard the count and take exactly
        // one step: the panel has no buffer line number for a count such as `3G`.
        if (sel === null || listEl === null) return;
        const times = action.motion === "gg" || action.motion === "G" ? 1 : Math.max(1, count ?? 1);
        const listTopBefore = listEl.scrollTop;
        const next = repeatMotion(sel as unknown as SelectionLike, model, action.motion, times, listEl);
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        scrollCaretIntoView(next.cursor, listTopBefore);
        return;
      }
      case "vswap": {
        // A repeated `o` does nothing. CARET cannot reach this action because
        // `resolveCaretKey` never returns it; keep the repeat guard for consistent handling.
        if (event.repeat || sel === null) return;
        const next = swapEnds(model);
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        scrollCaretIntoView(next.cursor, listEl?.scrollTop ?? null);
        return;
      }
      case "vtoggle": {
        // From CARET, `v`/`V` starts a selection. From VISUAL/V-LINE, the other key
        // switches selection modes; the mode's own key reaches `vback` below.
        // Ignore repeated `v`/`V` in either case.
        if (event.repeat || sel === null) return;
        const next: VisualModel = { ...model, kind: action.line ? "line" : "char" };
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        modeRef.current = action.line ? "vline" : "visual";
        setMode(modeRef.current);
        return;
      }
      case "vback": {
        // VISUAL/V-LINE's own key or Esc returns to CARET at the moving end.
        // Keep the region and frozen snapshot active. Ignore repeated `v`/`V`/Esc.
        if (event.repeat || sel === null) return;
        const caretModel: VisualModel = { anchor: model.cursor, cursor: model.cursor, kind: "caret", goalX: null };
        visualModelRef.current = caretModel;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, caretModel);
        modeRef.current = "caret";
        setMode("caret");
        // Swallow repeats of the key that leaves the mode until keyup. Otherwise a held `Esc` could
        // return to CARET and immediately end the region, dropping the caret and freeze.
        promptSwallowKeyRef.current = event.key;
        return;
      }
      case "vyank": {
        // Ignore repeated `y`. `endRegionFromKey` sets `promptSwallowKeyRef` so later
        // repeats cannot fall through to BROWSE's copy action after the region ends.
        if (event.repeat) return;
        if (sel === null || root === null || listEl === null) {
          endRegionFromKey();
          return;
        }
        const built = visualBuiltRef.current;
        // Copy only the selection still highlighted inside `.message-list`.
        if (built === null || !selectionMatchesBuild(sel as unknown as SelectionLike, built, listEl)) {
          showFlash("selection changed under it — nothing copied; v to start again");
          endRegionFromKey();
          return;
        }
        const text = copySelectionText(sel as unknown as SelectionLike, root, model);
        // After a characterwise yank, place the row cursor on the row containing the first
        // yanked character, the earlier selection end, regardless of where the moving end was left.
        const earlier = compareCarets(model.anchor, model.cursor) <= 0 ? model.anchor : model.cursor;
        const key = rowKeyOfCaret(earlier);
        landCursorOnRowKey(key);
        copied(text, key ?? undefined);
        endRegionFromKey();
        return;
      }
      case "vquote": {
        // `>` quotes the highlighted text into the tab's draft and enters INPUT without
        // sending anything. A held `>` quotes once; `promptSwallowKeyRef` swallows its repeats
        // until keyup so they cannot type into the composer.
        if (event.repeat) return;
        if (sel === null || root === null || listEl === null) {
          endRegionFromKey();
          return;
        }
        const built = visualBuiltRef.current;
        // Quote the same text `y` would copy, after checking the built selection.
        // A failed check quotes nothing, shows the same message and ends the region as `vyank` does.
        if (built === null || !selectionMatchesBuild(sel as unknown as SelectionLike, built, listEl)) {
          showFlash("selection changed under it — nothing copied; v to start again");
          endRegionFromKey();
          return;
        }
        // Refuse quoting when the session has ended or the draft is open in nvim
        // (`scratchEditing`). Keep VISUAL/V-LINE and its selection so the user can still
        // back out with Esc or copy with `y`.
        if (sessionEnded) {
          showFlash("this session has ended — nothing to quote into");
          return;
        }
        if (scratchEditing) {
          showFlash("the draft is open in nvim — finish there first");
          return;
        }
        const text = copySelectionText(sel as unknown as SelectionLike, root, model);
        const quote = formatQuote(text);
        if (quote === null) {
          showFlash("nothing to quote");
          return;
        }
        const earlier = compareCarets(model.anchor, model.cursor) <= 0 ? model.anchor : model.cursor;
        const key = rowKeyOfCaret(earlier);
        landCursorOnRowKey(key);
        const nextDraft = appendQuote(draftRef.current, quote);
        promptSwallowKeyRef.current = event.key;
        exitRegion();
        modeRef.current = "input";
        setMode("input");
        setComposerCaret("end");
        setComposerFocusRequest((n) => n + 1);
        restoreSeq.current += 1;
        setRestoredDraft({ text: nextDraft, seq: restoreSeq.current });
        mirrorDraft(nextDraft);
        return;
      }
      case "vend": {
        // Ignore repeated Esc in CARET. `promptSwallowKeyRef` swallows repeats of a key
        // that ended a mode; this also covers repeats that reach CARET by another route.
        if (event.repeat && action.key === "Escape") return;
        landCursorOnRowKey(rowKeyOfCaret(model.cursor));
        endRegionFromKey();
        // `Esc` exits silently. Other unbound keys show the current mode's hint. Name Space explicitly
        // so the message does not contain an unreadable blank key name. Enter explains how to
        // unfold a row in BROWSE before selecting it, since folded text cannot be reached here.
        if (action.key !== "Escape") {
          const keyName = action.key === " " ? "Space" : action.key;
          if (action.key === "Enter") {
            const modeName = regionModeName(currentMode);
            showFlash(`${modeName} ended: Enter is not a ${modeName} key -- in BROWSE, Enter unfolds a row, then v selects it`);
          } else if (currentMode === "caret") {
            showFlash(`CARET ended: ${keyName} is not a CARET key (v selects)`);
          } else {
            showFlash(`VISUAL ended: ${keyName} is not a VISUAL key (y copies)`);
          }
        }
        return;
      }
      case "vswallow":
        // This modified key was already prevented and stopped above, so leave state alone.
        // An idle Ctrl+c in CARET also explains that nothing is selected instead of invoking
        // native copy.
        if (currentMode === "caret" && event.key === "c" && event.ctrlKey) {
          showFlash("nothing selected — v, then y");
        }
        return;
      case "scroll-line": {
        // Ctrl+e/Ctrl+y scroll the frozen list by the count in text lines without moving
        // the caret or selection or changing mode. Read `computedLineHeight` from the caret row's
        // text through `rowTextElement`; `.message-list` sets neither font-size nor line-height,
        // so reading it directly gives a fraction of the actual line height.
        if (listEl === null) return;
        const row = rowElementOfCaret(model.cursor);
        const scrollTimes = Math.max(1, count ?? 1);
        listEl.scrollTop += action.delta * computedLineHeight(row !== null ? rowTextElement(row) : listEl) * scrollTimes;
        return;
      }
      case "keymap":
        // `?` ends the region and opens the keymap for discovering available keys.
        landCursorOnRowKey(rowKeyOfCaret(model.cursor));
        endRegionFromKey();
        setKeymapOpen(true);
        return;
      case "interrupt":
        // Ctrl+c keeps its BROWSE interrupt action while leaving the region active.
        interrupt();
        return;
      default:
        return;
    }
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
    // overlay and everything else below: tmux's own `confirm-before` prompt takes the
    // keys the same way.
    if (answerConfirm(event)) return;
    // Feed each remaining keydown to the typing guard before overlays, so keys typed into overlays
    // still cancel a waiting `a`/`d`/`D` and show a band notice. The key then continues normally.
    // Normalize WebKitGTK's Shift+Tab through `isShiftTab` to the walk key `Tab`.
    const typedAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
    // A held `j`/`k` glides (`./jkHold`, started with the other motions below). A repeat of its key only
    // keeps it going -- still a key to the guard below, and still the user's own scroll to the list. Any
    // other key ends it first, as letting go would, and then acts on the panel as that leaves it. The
    // release is recorded before this key is: the guard then judges the key against the moment the held
    // one was let go, not against its last repeat.
    const glidingList = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    const keepsGliding =
      glidingList !== null &&
      isHolding(glidingList) &&
      event.repeat &&
      keepHoldAlive(glidingList, event.code || event.key, performance.now());
    if (glidingList !== null && !keepsGliding) releaseHold(glidingList, performance.now());
    // The flash is the cancelled wait's own (`TypingGuard.defer`'s `cancelledFlash`): a cancelled
    // `f` or `L` says what did not happen, never the `a`/`d` text.
    const cancelled = typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typedAt);
    if (cancelled !== null) showFlash(cancelled);
    if (keepsGliding) {
      holdClockRef.current = { typed: typedAt, perf: performance.now() };
      noteUserScroll(glidingList, holdDirectionRef.current > 0 ? "down" : "up");
      lastKeyWasKillRef.current = false;
      tabAtKeyRef.current = null;
      event.preventDefault();
      return;
    }
    // Remember whether the preceding key killed text in the composer, then record whether this key
    // does. Bare modifiers leave that state alone.
    const afterKill = lastKeyWasKillRef.current;
    if (!isModifierKey(event.key)) lastKeyWasKillRef.current = isKillKey(event);
    // A plain Tab can create a focus landing at this key count only if its default remains allowed.
    // Keep the native event so `onFocus` can check after all handlers run. A claimed Tab must
    // not turn a later mouse focus into a keyboard landing.
    tabAtKeyRef.current =
      event.key === "Tab" && !event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey
        ? { atKey: typingGuard.keyCount(), event: event.nativeEvent }
        : null;
    // A card's own answer buttons (Approve, Deny, Always allow): see the Enter/Space rules below.
    const onAnswerButton =
      isActivatableControl(event.target) && (event.target as HTMLElement).closest("[data-nav-action]") !== null;
    // The modal overlays below return before the card-button guards. Prevent Enter and Space here
    // too, so a card button retaining focus under an overlay cannot activate through the
    // browser default.
    if ((chooser !== null || slashPicker !== null) && onAnswerButton && (event.key === "Enter" || event.key === " ")) {
      event.preventDefault();
    }
    // The chooser is modal. Keys it does not handle can bubble here and would otherwise
    // reach `resolveKey`, where `a`/`d` could answer a card covered by the chooser. Return after
    // the typing guard sees the key so it still counts as typing after the chooser closes.
    // Do not prevent the default: filter and rename inputs still need to receive their text.
    if (chooser !== null) return;
    // The picker is modal like the chooser. Its handler stops propagation for claimed
    // keys, so this return handles only unclaimed keys and prevents them from reaching
    // panel actions underneath the picker.
    if (slashPicker !== null) return;
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
    // `gx` accepts only letters without Ctrl, Alt, Meta, Super or AltGr, excluding input-method
    // composition, so Ctrl+a or Alt+s cannot open a link. Repeats and bare modifiers keep the
    // pick waiting; other keys cancel it without acting. In a text field, cancel the pick and
    // let the field receive the key.
    if (linkPick !== null && isEditableElement(event.target)) setLinkPick(null);
    else if (linkPick !== null) {
      event.preventDefault();
      if (event.repeat || isModifierKey(event.key)) return;
      const index = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike) ? HINT_ALPHABET.indexOf(event.key) : -1;
      if (index !== -1 && index < linkPick.length) openUrl(linkPick[index]);
      setLinkPick(null);
      return;
    }
    // Esc closes `ContinueInTerminal`'s confirmation. Let other keys continue so Tab can reach its
    // confirm/cancel buttons and the control handler below can activate them.
    if (handoffOpen && event.key === "Escape") {
      event.preventDefault();
      setHandoffOpen(false);
      return;
    }
    // A bare modifier keydown must preserve a pending prefix, count and which-key box. `5 Ctrl+e`
    // includes a Control keydown before `e`; clearing the count on Control would scroll once
    // instead of five times.
    if ((pendingRef.current !== null || countRef.current !== null) && isModifierKey(event.key)) return;
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
    // The `?` keymap overlay owns every key while it is open, ahead of the editable-
    // element and activatable-control checks below and ahead of `resolveKey` entirely -- not
    // routed through the key table at all, because closing is not one of `resolveKey`'s actions
    // (only OPENING is: its `{kind:"keymap"}` is reached below, and only ever with the overlay
    // already closed, since this branch returns first whenever it is open). `preventDefault`
    // unconditionally: `a`/`d` must not reach a permission card hidden underneath, and neither
    // must anything else this table would otherwise claim.
    if (keymapOpen) {
      onKeymapKey(event);
      return;
    }
    // The review overlay owns every key while it is open, the same way and for the same reason: `a`/`d` must
    // not reach a permission card hidden underneath, and nothing else this table claims may act on a
    // conversation the reader cannot see. Its own keys are `./review`'s, not `resolveKey`'s. A bare modifier
    // is not a key (a held Shift on its way to `G`), and neither it nor an unbound key ends a waiting `g`
    // except by being a different key.
    if (review !== null) {
      // The comment input is a text box inside the overlay: what is typed in it is its own (an `a` or a `d`
      // there is a letter, and reaches no card -- nothing below this branch runs). Its Enter and Escape
      // never get here, the box stops them; every other key does nothing at the root.
      const target = event.target;
      if (target instanceof HTMLInputElement && reviewOverlayRef.current?.contains(target)) return;
      event.preventDefault();
      if (isModifierKey(event.key)) return;
      const action = resolveReviewKey(event.nativeEvent as unknown as KeyLike, review.pendingG, review.prompt);
      if (action === null) {
        if (review.pendingG) setReview({ ...review, pendingG: false });
        return;
      }
      // An open hunk taller than its box scrolls inside it first, as a tall tool result does.
      if (action.kind === "move" && scrollBoxFirst(boxUnderCursor(reviewOverlayRef.current), action.delta, TOOL_RESULT_SCROLL_STEP_PX)) {
        if (review.pendingG) setReview({ ...review, pendingG: false });
        return;
      }
      const next = applyReviewKey(review, action, nextRequestId);
      setReview(next.state);
      if (next.effect !== null) runReviewEffect(next.effect, review.tab);
      return;
    }
    // The detail popover takes every key while open: `j`/`k` move, `y` copies a fact, and Esc/q
    // closes it. Include the trailing `Continue in a terminal...` row by walking `[0, detail.length]`. Enter on that last row opens `ContinueInTerminal`'s confirmation, as
    // clicking it does.
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
    // Focused buttons activate on Enter and Space natively; claiming those everywhere would remove
    // the only keyboard route to Approve. Other keys still reach the table, so `h`/`j`/`k`/`l` can
    // navigate from a focused button. On a card's answer buttons (Approve, Deny, Always allow) Space
    // is claimed and does nothing, since Claude Code's permission prompt confirms with Enter. Enter
    // on those buttons is claimed too: this handler presses the button itself `TYPING_GUARD_MS`
    // after a focus landing on it. `l`/`h`/Tab/HINT onto Approve followed by Enter still works, only
    // deferred as `a`/`d` are. Other buttons keep native activation.
    // A Space inside a pending leader sequence goes to the sequence engine, with its default
    // already prevented so it cannot activate the focused button. `onAnswerButton` is computed
    // before modal overlays return. A pending prefix owns Enter/Space on any focused control;
    // a pending count blocks those keys only on a card's answer buttons.
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ") && (pending !== null || (count !== null && onAnswerButton))) {
      event.preventDefault();
      if (pending === null) showFlash(COUNT_ANSWER_FLASH);
      return;
    }
    if (onAnswerButton && event.key === " ") {
      event.preventDefault();
      if (seqRef.current === null) return;
    }
    // Claim Enter before native button activation. Answer through `waitThenAnswer` after
    // `TYPING_GUARD_MS` only if `mayAnswerNow` permits it, with no modifiers and focus placed
    // on this button by the immediately preceding key. Any intervening key cancels the wait.
    // The button must also belong to `permissionTarget`: Tab can move focus onto another card
    // while the row cursor stays put.
    if (onAnswerButton && event.key === "Enter") {
      event.preventDefault();
      const button = event.target as HTMLButtonElement;
      const placed = placedRef.current;
      if (!typingGuard.mayAnswerNow("Enter", typedAt, event.repeat)) {
        showFlash(TYPING_FLASH);
        return;
      }
      if (event.shiftKey || !isPlainAnswerKey(event.nativeEvent as unknown as KeyLike)) {
        showFlash(MODIFIED_ENTER_FLASH);
        return;
      }
      if (placed === null || placed.el !== button || placed.atKey !== typingGuard.keyCount() - 1) {
        showFlash(ENTER_LANDING_FLASH);
        return;
      }
      const buttonRow = root === null ? null : rowOf(root, button);
      const buttonRowIndex = root === null || buttonRow === null ? null : rowIndexOf(root, buttonRow);
      if (buttonRowIndex === null || buttonRowIndex !== permissionTarget(answerableItems, cursor)) {
        showFlash(ENTER_ELSEWHERE_FLASH);
        return;
      }
      // The same card, in the same tab, still takes it at fire time -- as `answerCardByKey` checks.
      const tab = activeTabRef.current;
      waitThenAnswer(
        event,
        typedAt,
        () => activeTabRef.current === tab && button.isConnected && !button.disabled && document.activeElement === button,
        () => button.click(),
        "Enter",
      );
      return;
    }
    // The activity line's Stop accepts Enter and Space only on a key that stands alone
    // or ends a quick motion (`j` onto it, then Enter). Since `j` from the last row lands here,
    // the guard prevents prose such as "just do it" from stopping the turn on its first Space
    // and denying the waiting card with it.
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
    // Keys within `TYPING_GUARD_MS` after a BROWSE `y` are swallowed with a typing hint; the copy
    // has already happened. Ignore bare modifiers and input-method composition. Check before
    // the focused-control return so Enter or Space in the burst cannot activate a button; a
    // lone Enter after a pause remains available to it.
    if (
      mode === "browse" &&
      !isModifierKey(event.key) &&
      !isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode }) &&
      typingGuard.afterCopy(typedAt)
    ) {
      event.preventDefault();
      // The wait this very key cancelled already said what did not happen, in its own words (and ends
      // with this hint): that message stays.
      if (cancelled === null) showFlash(TYPE_HINT_FLASH);
      return;
    }
    if (isActivatableControl(event.target) && (event.key === "Enter" || (event.key === " " && !onAnswerButton))) return;
    // The leader engine: a pending sequence owns
    // the next key, and only BROWSE ever starts one. An IME key is never a sequence key (Review
    // Focus 2, C4) -- checked here rather than folded into `startSequence`/`advanceSequence`
    // themselves, since neither of those takes an event to read `isComposing` off. A `{kind:"none"}`
    // start falls through to `resolveKey` below, never the other way; `startSequence` itself leaves
    // `g`/`z`/`[`/`]` to `resolveKey`'s own reserved two-key prefixes (`./leader`'s own doc comment).
    if (mode === "browse" && !isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode })) {
      if (seqRef.current !== null) {
        // A bare modifier keydown must leave a pending sequence armed. For example, pressing
        // Control before Ctrl+c must not cancel the sequence and let the following `c` fall through
        // to `resolveKey`'s interrupt action. `pendingRef` and `TypingGuard.onKey` ignore bare
        // modifiers in the same way.
        if (isModifierKey(event.key)) return;
        event.preventDefault();
        // `advanceSequence` compares `event.key` alone, so Ctrl+d could match the plain `d` in
        // `<leader>bd`. Refuse Ctrl/Alt continuations just as sequence starts do. Swallow the chord
        // and cancel the sequence because it matches no table key.
        applySeqStep(
          event.ctrlKey || event.altKey ? { kind: "cancel" } : advanceSequence(panelTable, seqRef.current.typed, event.key),
        );
        return;
      }
      // A prefix's second key cannot start another sequence. `g` then Space cancels `g`
      // through `resolveKey`; it cannot start the leader. `g` then `L` cannot switch tabs.
      if (pending === null && !event.ctrlKey && !event.altKey) {
        const start = startSequence(panelTable, event.key, isActivatableControl(event.target));
        if (start.kind !== "none") {
          event.preventDefault();
          // The v1-ui GUI pass (2026-09-27): the leader starts a sequence only on a key that stands
          // alone or ends a quick motion -- in the middle of typed prose it is swallowed and says
          // so ("set up my" ran `<leader>m`, "the boy" `<leader>bo` and answered it with its `y`).
          if (event.key === panelTable.leader && !typingGuard.mayActAfterMotion(typedAt, event.repeat)) {
            showFlash(leaderTypingFlash(panelTable.leaderLabel));
            return;
          }
          // Defer single-key table bindings such as H/L's tab.prev/tab.next so prose starting
          // with `L` cannot switch tabs and send the rest into another session's composer.
          // `mayActAfterMotion` cannot guard the first key of typed text. As with `a`/`d`, run
          // `TYPING_GUARD_MS` later only if no other key arrived and the panel is still in BROWSE.
          if (start.kind === "run") {
            const { binding } = start;
            const flash = tableKeyTypingFlash(event.key, binding.desc);
            const waiting = typingGuard.defer(
              typedAt,
              event.repeat,
              () => {
                if (modeRef.current === "browse") applySeqStep({ kind: "run", binding });
              },
              flash,
            );
            if (!waiting && !event.repeat) showFlash(flash);
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
    // A design decision: INPUT's Ctrl+y. Handled here, ahead of the unconditional `preventDefault`
    // further down, because with no card waiting the key stays the box's.
    if (action.kind === "approve-oldest") {
      approveOldestByKey(event, typedAt, afterKill);
      return;
    }
    // Accumulate digits into the count read by the next `j`/`k`/`[[`/`]]` through `countRef`.
    // Handle digits first and return so they cannot enter the scroll or row-motion code.
    // `accumulateMotionCount` caps each digit at `MAX_MOTION_COUNT`, so even an unfinished
    // count cannot exceed the limit.
    if (action.kind === "count") {
      event.preventDefault();
      countRef.current = accumulateMotionCount(count, action.digit);
      return;
    }
    // How many times THIS key repeats its move: the count just accumulated (cleared above, so this
    // is the last one) if any, else once, same as vim's own default count of 1.
    const times = Math.max(1, count ?? 1);
    // Announce conversation movement on the list so `MessageList` can distinguish user
    // scrolling from unexplained `scrollTop` drops during a reply. `k`, `Ctrl+u` and `gg` stop
    // following immediately. `j`, `Ctrl+d` and `G` let the scroll decide, resuming at the bottom.
    // `h`/`l` can focus a control that scrolls itself into view in either direction.
    // `Ctrl+y` stops following; `Ctrl+e` resumes it only when the scroll reaches the bottom.
    // Page motions (`Ctrl+f`/`PageDown`/`PageUp`) and arrow-key moves follow the same rule.
    const messageList = root?.querySelector<HTMLElement>(".message-list") ?? null;
    if (
      action.kind === "move" ||
      action.kind === "half-page" ||
      action.kind === "scroll-line" ||
      action.kind === "page"
    ) {
      noteUserScroll(messageList, action.delta > 0 ? "down" : "up");
    } else if (action.kind === "jump") {
      // A counted jump goes to row N, so its direction depends on that row's position
      // relative to the cursor. Bare `gg`/`G` uses its named end regardless of the cursor.
      const up = count === null ? action.to === "first" : jumpTarget(action.to, count, timeline.length) < cursor;
      noteUserScroll(messageList, up ? "up" : "down");
    } else if (action.kind === "control") {
      noteUserScroll(messageList, "unknown");
    }
    // A held `j`/`k` (`./jkHold`): from its first repeat the view glides at a steady speed and the
    // cursor follows the row under its line, instead of a jump per repeat. Only for the row cursor in
    // BROWSE with no count (a count keeps its immediate steps), and only where the glide has somewhere to
    // go and motion is allowed; otherwise the repeat is a press like any other, below.
    if (
      action.kind === "move" &&
      event.repeat &&
      count === null &&
      !edgeFocused &&
      mode === "browse" &&
      frozenSnapshotRef.current === null &&
      messageList !== null &&
      root !== null &&
      currentStop(root, cursor) === (conversationRows(root)[cursor] ?? null)
    ) {
      const heldKey = event.key;
      const direction = action.delta;
      const started = startHold({
        list: messageList,
        rows: () => conversationRows(root),
        cursor,
        direction,
        code: event.code || event.key,
        now: performance.now(),
        onCursor: (index) => {
          // The glide placed the view itself; the landing effect must not move it again. Only for a cursor
          // that really moves: a "keep" no `[cursor]` effect consumes would swallow the next reveal.
          if (index !== cursorRef.current) landingRef.current = "keep";
          setCursor(index);
        },
        onRelease: (at) => {
          // The guard's clock is the keydowns' own; the release is dated on it from the last repeat.
          const clock = holdClockRef.current;
          typingGuard.onRelease(heldKey, clock.typed + Math.max(0, at - clock.perf));
        },
      });
      if (started) {
        holdClockRef.current = { typed: typedAt, perf: performance.now() };
        holdDirectionRef.current = direction;
        markLandedCode(null);
        event.preventDefault();
        return;
      }
    }
    // A press that arrives while the last one is still easing starts from where that one was going,
    // never from a frame in between: every decision below reads the list's geometry.
    if (messageList !== null) settleListScroll(messageList);
    // A code block HINT landed on is "the item" for exactly the next `y`; any other key moves on.
    const landedCode = copyCodeRef.current;
    markLandedCode(null);
    // The row the cursor is on, found by structure (`conversationRows`), never as the first
    // `.row-current` or the `cursor`-th `[data-nav-stop="row"]` in the subtree: a model reply's own
    // HTML can carry either. Read only by the keys that need it.
    const cursorRow = () => (root === null ? null : (conversationRows(root)[cursor] ?? null));
    // `j`/`k` scroll the cursor row's own overflow box (a long tool result) before they move off it
    // -- the vim behaviour -- and failing that scroll the conversation through a
    // current row that is taller than what is left of it on screen (`stepWithinRow`, which holds the
    // order and `./jkScroll` the rules). A count spends itself on those steps first; whatever it has
    // left moves rows below. This is DOM measurement, which `resolveKey`'s pure table must never do,
    // so it happens here, after the key already means "move the cursor". Only while the row cursor has
    // the keys: from a banner's button, `j` is a move, not a scroll of a row that is not current.
    let spentInRow = 0;
    if (action.kind === "move" && !edgeFocused) {
      spentInRow = stepWithinRow(cursorRow(), action.delta, times, !event.repeat);
      if (spentInRow > 0) {
        event.preventDefault();
        // `j`/`k` scroll steps restore root focus just like row moves. Otherwise a focused control
        // could scroll out of view and still respond to Enter.
        if (root !== null && document.activeElement !== root && root.contains(document.activeElement)) {
          root.focus({ preventScroll: true });
        }
        if (spentInRow >= times) return;
      }
    }
    if (action.kind === "move" || action.kind === "control" || action.kind === "answer") {
      event.preventDefault();
      if (root === null) return;
      if (action.kind === "move") {
        // Repeat each step from the row reached by the previous step, so a banner or status
        // line still ends the walk as a single `j`/`k` would. `countedStop` reads the tree once for
        // the entire count, up to `MAX_MOTION_COUNT`, and stops at a boundary when a step makes
        // no progress because `clampStep` clamps instead of returning `null`.
        const landing = countedStop(root, cursor, action.delta, times - spentInRow);
        if (landing === null) return;
        const target = landing.stop;
        // C1c: `j` that cannot move -- `nextStop` clamps rather than returning `null` at
        // a boundary (`clampStep`'s own doc comment), so "the last stop" is the stop the keys are
        // already on (the last row, or Stop while a turn runs), not `target === null` (that case is
        // an EMPTY conversation, handled above and unrelated). Flashes once per press: a fresh `j`
        // there, or -- the v1-ui GUI pass (2026-09-27) found a held `j` reached the bottom in silence,
        // since every step after the first is a repeat -- the first repeat that stops after moving.
        // Never again on the repeats after that. `k` at the FIRST stop stays silent, as vim's own `k`
        // on the first line does (`:h j`) -- only `j` names a route the reader might actually want next.
        if (action.delta === 1) {
          if (target === landing.from) {
            if (!event.repeat || heldMoveRef.current) showFlash(TYPE_HINT_FLASH);
            heldMoveRef.current = false;
          } else {
            heldMoveRef.current = true;
          }
        }
        const finalRow = landing.row;
        if (finalRow !== null) {
          if (finalRow !== cursor) {
            landingRef.current = action.delta;
            // Only a lone press eases the view to the new row: a held key's repeats and a count land at once.
            landingAnimateRef.current = !event.repeat && times === 1;
          }
          setCursor(finalRow);
          root.focus({ preventScroll: true });
        } else {
          controlsOf(target)[0]?.focus();
        }
      } else if (action.kind === "control") {
        const stop = currentStop(root, cursor);
        const target = stop === null ? null : nextControl(stop, action.delta);
        if (target === "stop") root.focus({ preventScroll: true });
        else if (target !== null) {
          // Save a landing only when this key actually moves focus onto the control.
          // A clamped `l` on the last control moves nothing and records no landing.
          const moved = document.activeElement !== target;
          target.focus();
          if (moved && document.activeElement === target) placedRef.current = { el: target, atKey: typingGuard.keyCount() };
        }
      } else {
        // `a`/`d` answer by card id through `answerPermission`, sharing the buttons' guard against a
        // second answer. Only the cursor's card or the card gating its tool call is eligible
        // (`permissionTarget`). A banner button has no current card.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        answerCardByKey(event, typedAt, target, action.decision);
      }
      return;
    }
    event.preventDefault();
    switch (action.kind) {
      case "mode":
        // `i`/`o`/`A` enters INPUT only after an isolated key. `mayStartInput` requires a pause
        // and has no motion exception: "look at" must not walk with `l` and enter INPUT with `o`.
        // Letters inside prose are swallowed with a typing hint instead of opening the composer
        // midword. A lone `i` after a pause works; a fast `ji` needs a pause before `i`.
        if (action.to === "input" && !typingGuard.mayStartInput(typedAt, event.repeat)) {
          if (cancelled === null) showFlash(TYPE_HINT_FLASH);
          break;
        }
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
        // `Esc` shows a reminder here; it never interrupts the turn.
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
      case "fold": {
        // `zo` opens this row's result or collapsed run. `zc` closes its open result, otherwise its
        // containing run. Expanded calls use their own keys, so `runKeyOf` finds the run in the
        // folded timeline; it returns null in detailed view or outside a run. Set `expanded`
        // instead of toggling it so repeated opens or closes change nothing.
        const key = timeline[cursor]?.key;
        if (key === undefined) break;
        const run =
          action.open || expanded[key]
            ? null
            : runKeyOf(
                buildTimeline(state),
                { expanded, detailed, turnRunning: state.activeTurnId !== null, thinking: thinkingDisplay },
                key,
              );
        setExpanded((prev) => ({ ...prev, [run ?? key]: action.open }));
        break;
      }
      case "detailed":
        // `Ctrl+o` toggles per-tab detail, mirrored to Rust by `saveView` and restored on a tab
        // switch.
        setDetailed((d) => !d);
        break;
      case "table-scroll": {
        // T1: `zh`/`zl` scroll the CURRENT row's own table, never the conversation.
        const table = cursorRow()?.querySelector<HTMLElement>(".table-scroll") ?? null;
        if (table !== null) table.scrollLeft += action.delta * TOOL_RESULT_SCROLL_STEP_PX;
        break;
      }
      case "caret":
        // `v` from BROWSE starts CARET. `enterRegion` checks for a row under the cursor and flashes if
        // there is none. Pass the saved `landedCode`, also used by copy, because
        // `copyCodeRef.current` has already been cleared.
        enterRegion(false, landedCode);
        break;
      case "visual":
        // `V` from BROWSE starts V-LINE directly.
        enterRegion(action.line, landedCode);
        break;
      case "copy": {
        // `y` starts a burst in which subsequent keys count as typing.
        typingGuard.noteCopy(typedAt);
        // After HINT landed on a code block: that block's code, not the whole message. Only while it
        // is still in the DOM and still inside the row under the cursor -- anything else and the
        // user is looking at something else now.
        if (landedCode !== null && landedCode.isConnected && cursorRow()?.contains(landedCode)) {
          copied(codeBlockText(landedCode), timeline[cursor]?.key);
          break;
        }
        // No item at the cursor (an empty timeline) writes NOTHING, rather than clobbering
        // whatever the user already had on the clipboard with "" -- found in review.
        const item = timeline[cursor];
        if (item === undefined) break;
        // A reply's row copies what is drawn, not the markdown it came from: the source can hold
        // comments, raw HTML the sanitizer removed, and text that never reaches the screen.
        const body = item.kind === "message" ? (cursorRow()?.querySelector<HTMLElement>(".row-body") ?? null) : null;
        copied(body !== null ? renderedText(body) : primaryText(item), item.key);
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
        // Uses the same target card and wait as `a`/`d`.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        // Another agent's card has no reason box: its deny carries no reason, so `D` says what happens
        // instead of waiting for a box that is not there.
        const targetItem = target === null ? undefined : timeline[target];
        if (targetItem?.kind === "permission" && isExternalTool(targetItem.request.toolName)) {
          showFlash(`${agentTitle(activeTabInfo(tabsRef.current)?.agent)} takes no reason with a deny; d denies`);
          break;
        }
        // The card's row by structure: `conversationRows(root)[target]` is timeline item
        // `target`, which a card row holds no model HTML inside.
        const box =
          target === null
            ? null
            : (conversationRows(root)[target]?.querySelector<HTMLInputElement>(".permission-card input") ?? null);
        waitThenAnswer(event, typedAt, box === null ? null : () => box.isConnected, () => {
          if (box === null) return;
          // Re-read where the card is now: rows may have arrived above it during the wait.
          const row = rowOf(root, box);
          const index = row === null ? null : rowIndexOf(root, row);
          if (index !== null && index !== cursorRef.current) setCursor(index);
          landedControlRef.current = box;
          box.focus();
        });
        break;
      }
      case "restart":
        // `r` on an ended tab resets it to `NotStarted` IN PLACE: the tab keeps its
        // number and its rename, and the `tabs` envelope that follows is what actually drops this
        // render back to the empty tab -- there is no local start-screen reset any more.
        resetTab();
        break;
      case "review": {
        // Turn review. A tab with no session has nothing to review (the start screen never gets here, its
        // keys are the empty tab's own), and a request always names its tab.
        const tab = activeTabRef.current;
        if (tab !== null && sessionStartedRef.current) openReviewOverlay(tab);
        break;
      }
      case "keymap":
        // Only ever reached with the overlay closed -- the `keymapOpen` branch above returns
        // before `resolveKey` runs at all once it is open, `?`/`Escape`/`q` included, so this is
        // opening only, never a toggle-closed here.
        setKeymapOpen(true);
        break;
      case "hint": {
        // Defer `f` because a backward-only `mayActAfterMotion` check cannot distinguish it
        // from the first letter of typed text. Request HINT after `TYPING_GUARD_MS` only if no other
        // key arrived and the panel is still in BROWSE. Once started, `shell` owns the HINT session,
        // collects targets across the window and requests this panel's targets with `hint_collect`.
        // `hintPendingRef` swallows this panel's keys until HINT ends.
        const waiting = typingGuard.defer(
          typedAt,
          event.repeat,
          () => {
            if (modeRef.current === "browse") requestHint(false);
          },
          hintTypingFlash(),
        );
        if (!waiting && !event.repeat) showFlash(hintTypingFlash());
        break;
      }
      case "pending":
        // Claimed (so the key does nothing else) and remembered for exactly one more key.
        pendingRef.current = action.prefix;
        // Restore the count for a pending prefix (`3gg`, `2gt`) so its second key can read it.
        // Like the prefix, the count was read and cleared at the top of this handler.
        countRef.current = count;
        // Show continuations for `g`/`z`/`[`/`]`/`Ctrl+w` after `WHICH_KEY_DELAY_MS` without a
        // completing key. `clearSequence` on each later key, including the second `g` of `gg`,
        // cancels the timer before it fires.
        scheduleBoxTimer();
        break;
      case "panel": {
        // Handles a reserved prefix's second key that also completes a panel-table sequence,
        // such as `[b`, through `resolveKey`'s `ctx.pending` branch. Other table sequences reach
        // the leader engine above. A count before `gt` selects tab N; before `gT` it moves N tabs
        // backward. Other pairs ignore it. `select_tab` takes the tab id, which differs from its displayed number.
        const tabAction = action.binding.keys[0] === "g" ? action.binding.action : null;
        if (count !== null && (tabAction === "tab.next" || tabAction === "tab.prev")) {
          const target = tabs === null ? null : countedTabTarget(tabs, tabAction, count);
          if (target === null) showFlash(`no tab ${count}`);
          else postToRust({ type: "select_tab", request_id: nextRequestId(), tab: target });
          break;
        }
        runPanelAction(action.binding);
        break;
      }
      case "cancel":
        // The key is already prevented and runs no action. Cancelling a prefix stays silent;
        // a count before a card answer shows why the answer was refused.
        if (action.why === "count-on-answer") showFlash(COUNT_ANSWER_FLASH);
        break;
      case "pane":
        // `Ctrl+w h/j/k/l` asks the shell to move focus in that direction through `pane_nav`. A side
        // with no module keeps focus here. The prefix is already consumed, and a preceding
        // count does not affect the move.
        postToRust({ type: "pane_nav", request_id: nextRequestId(), direction: action.direction });
        break;
      case "half-page": {
        // Scroll half the visible height, then use `clampCursorToView` to move an offscreen
        // cursor to its nearest visible row. Keep the view where the scroll put it; revealing
        // that row again with `nearest` could pull the view back by up to a row.
        // When rehoming the cursor, focus the root so an offscreen control cannot keep answering
        // Enter or steering `j`/`k` from its old stop. Leave focus alone when the helper returns
        // `null`, either because the cursor row is partly visible or no row is visible.
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list === null || root === null) break;
        list.scrollTop += action.delta * Math.max(1, Math.floor(list.clientHeight / 2));
        const next = clampCursorToView(list, conversationRows(list), cursor);
        if (next !== null) {
          landingRef.current = "keep";
          setCursor(next);
          root.focus({ preventScroll: true });
        }
        break;
      }
      case "scroll-line": {
        // Scroll `times` units, letting the cursor row's overflow box consume them first using
        // `TOOL_RESULT_SCROLL_STEP_PX`. Each remaining unit scrolls the conversation by one
        // text line. Rehome the cursor with the same rule as `half-page`. The box consumes
        // units only while at least partly visible, through `scrollVisibleRowBox`, without
        // bringing an offscreen row back.
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list === null || root === null) break;
        const row = cursorRow();
        let remaining = times;
        while (remaining > 0 && scrollVisibleRowBox(row, action.delta)) remaining--;
        if (remaining > 0) {
          list.scrollTop += action.delta * computedLineHeight(row !== null ? rowTextElement(row) : list) * remaining;
        }
        // `noteUserScroll` only arms the steering window; rearming following needs a scroll event on
        // the list. Scrolling only a tool-result box produces none because scroll events do not
        // bubble. Dispatch one for downward scrolling so following can resume at the bottom.
        // `onScroll` reads the current position, so an additional real event is harmless.
        // Upward scrolling must keep following off even if the list remains at the bottom.
        if (action.delta > 0) list.dispatchEvent(new Event("scroll"));
        const next = clampCursorToView(list, conversationRows(list), cursor);
        if (next !== null) {
          landingRef.current = "keep";
          setCursor(next);
          root.focus({ preventScroll: true });
        }
        break;
      }
      case "page": {
        // Scroll `times` whole views, retaining two text lines of overlap. Measure those lines from the
        // cursor row's text because the list has no explicit line height. Scroll only the list.
        // If the cursor leaves view, move it to the nearest visible row, preserve the scroll
        // position and restore root focus so an unseen control cannot answer Enter.
        const list = messageList;
        if (list === null || root === null) break;
        const row = cursorRow();
        const line = computedLineHeight(row !== null ? rowTextElement(row) : list);
        list.scrollTop += action.delta * times * Math.max(1, list.clientHeight - 2 * line);
        const next = clampCursorToView(list, conversationRows(list), cursor);
        if (next !== null) {
          landingRef.current = "keep";
          setCursor(next);
          root.focus({ preventScroll: true });
        }
        break;
      }
      case "jump": {
        // Bare `G` scrolls to the list's end, including the bottom of a long last reply;
        // bare `gg` scrolls to its top. With a count, both use `jumpTarget` to select row N,
        // clamped to existing rows, and let the cursor effect reveal the row's near edge.
        // Set the landing only if the cursor changes: an unchanged cursor runs no effect to
        // consume it, so a leftover value would misplace a later unrelated move.
        const target = jumpTarget(action.to, count, timeline.length);
        if (target !== cursor) landingRef.current = count === null ? "keep" : target > cursor ? 1 : -1;
        setCursor(target);
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list !== null && count === null) list.scrollTop = action.to === "first" ? 0 : list.scrollHeight;
        root?.focus({ preventScroll: true });
        break;
      }
      case "scroll-row": {
        // Align the whole row's top, middle or bottom with the view. A count selects the same clamped,
        // one-based row as `{N}G` and moves the cursor there. Call `noteUserScroll` first so
        // upward movement stops following. `"keep"` preserves the alignment, subject to the
        // browser's scroll limits.
        const list = messageList;
        if (list === null || root === null) break;
        const target = count === null ? cursor : jumpTarget("first", count, timeline.length);
        const row = conversationRows(list)[target];
        if (row === undefined) break;
        const l = list.getBoundingClientRect();
        const r = row.getBoundingClientRect();
        const offset =
          action.where === "top"
            ? r.top - l.top
            : action.where === "bottom"
              ? r.bottom - l.bottom
              : (r.top + r.bottom - l.top - l.bottom) / 2;
        noteUserScroll(list, offset < 0 ? "up" : "down");
        list.scrollTop += offset;
        if (target !== cursor) {
          landingRef.current = "keep";
          setCursor(target);
        }
        root.focus({ preventScroll: true });
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
      case "card-jump": {
        // `]p`/`[p` move the cursor `times` waiting cards forward or backward, wrapping without
        // answering. A later `a`/`d` uses the landed card and must pass the typing guard. Skip
        // answered cards and ended sessions so the cursor cannot land on a card that can no
        // longer be answered.
        const target = sessionEnded ? null : waitingCardAfter(timeline, cursor, action.delta, times, answeredRef.current);
        if (target === null) {
          showFlash(NO_WAITING_CARD_FLASH);
          break;
        }
        if (target !== cursor) {
          // The way the cursor really travels, not the key's: a wrap from the last card to the first goes UP
          // the list, and only a scroll up stops the list following. `landingRef` is the key's own -- which
          // edge of a row taller than the view to show (`revealRow`), the top after `]p`, the bottom after `[p`.
          noteUserScroll(messageList, target < cursor ? "up" : "down");
          landingRef.current = action.delta;
          setCursor(target);
        }
        // Even with the cursor staying (the one card waiting is the one it is on): the keys come back to the
        // row from a focused control -- Approve after `l`, Stop -- as every other cursor move does, so the
        // next `a` acts on the card the cursor shows and Enter cannot press a button it has left.
        root?.focus({ preventScroll: true });
        break;
      }
      case "interrupt":
        // `Ctrl+c` interrupts a running turn from anywhere in BROWSE, using the same `interrupt()`
        // as the activity line's Stop button.
        interrupt();
        break;
      case "search":
        // Opening `/` closes `:` because both use the command line. Leaving both open could let
        // `takeKeys` focus the older search prompt.
        setExLine(null);
        setSearch({ query: "", origin: cursor });
        setLineFocusRequest((n) => n + 1);
        break;
      case "ex-line":
        // The command box takes focus through `SearchBar` and runs nothing on opening.
        // If it is already open with focus elsewhere, clear it and request focus again so `:`
        // always presents a fresh command line.
        openExLine();
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
      case "open-link": {
        // the web link(s) of the row under the cursor, read off the DOM as the
        // reader sees them (`webLinks`: http(s) only, normalized, once per address). One whose visible text
        // is its own address, on screen, opens at once; several, a titled one, or one nobody can see wait for
        // a letter with each full address shown -- a link in a model's reply is never opened by a key alone
        // without the reader having seen where it goes. A count is ignored, as `gf`'s is.
        const row = cursorRow();
        const links = row === null ? [] : webLinks(row).slice(0, HINT_ALPHABET.length);
        if (links.length === 0) showFlash("no web link on this row");
        else if (links.length === 1 && root !== null && linkOpensAtOnce(links[0], root)) openUrl(links[0].url);
        else setLinkPick(links.map((link) => link.url));
        break;
      }
      case "view-in-editor": {
        // Open the row's full text in a read-only nvim scratch buffer.
        const item = timeline[cursor];
        if (item === undefined) break;
        const { title, text } = viewText(item);
        const requestId = nextRequestId();
        trackRequest(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
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
  /* Classify a lost or closed session's reason so the error row can show a headline and remedy
     above the raw text. A first-turn sidecar failure may end a live session here instead of
     failing the tab. Unrecognised reasons produce `null` and keep the raw text alone. */
  const sessionEndedProblem =
    state.status.kind === "unavailable" || state.status.kind === "closed"
      ? classify(state.status.reason, hello?.account ?? null)
      : null;
  // What `r` does on this row: Rust continues a session its agent stopped with the conversation whole, and
  // empties the tab to a start screen otherwise (`TabView::resumes_on_r`).
  const restartHint = activeTab?.resumesOnR
    ? "Press r to resume this conversation."
    : "Press r to start a new session here.";
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
        {!remedyNamesR(sessionEndedProblem) && <div className="row-hint">{restartHint}</div>}
      </Row>
    ) : state.status.kind === "closed" ? (
      // Deliberately NOT `row-error`/`✗`: an ordinary close (the host closed it, the provider
      // exited cleanly) is not an error, and styling it like the lost-session row above would
      // train the eye to ignore the one that matters. See `.row-ended` in index.css for the
      // fuller record of why these two were briefly unified and then split back apart.
      <Row kind="ended" sign="·" problem={sessionEndedProblem}>
        This session has ended ({state.status.reason}).
        {!remedyNamesR(sessionEndedProblem) && <div className="row-hint">{restartHint}</div>}
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
            // `Ctrl+w`'s own spelling, not tmux's `"C-w"` (`PendingPrefix`'s): the other four are one character.
            title: pendingRef.current === "C-w" ? "Ctrl+w" : pendingRef.current,
            entries: [...FIXED_PENDING_ENTRIES[pendingRef.current], ...boxEntries(panelTable, [pendingRef.current], modeFixed)],
          }
        : null;

  return (
    <ProjectDirContext.Provider value={hello?.projectDir ?? ""}>
    <div
      className="agent-ui-root agent-ui-conversation"
      ref={containerRef}
      // Lets the stylesheet show key hints only where those keys would act (BROWSE, not INPUT).
      data-panel-mode={mode}
      data-pane-focused={paneFocused ? "true" : "false"}
      // Real, focusable, so BROWSE's keydown handler has a DOM node to bubble from -- see the
      // effect above `onKeyDown` that keeps focus here whenever `mode` is "browse".
      tabIndex={0}
      // A y/n prompt takes keys first in capture phase, wherever they were typed.
      // The VISUAL key table runs next, before `onKeyDown` and descendant handlers.
      // A card's reason box can deny from its own handler, so VISUAL must claim the key here
      // before that handler sees it.
      onKeyDownCapture={(event) => {
        if (answerConfirm(event)) return;
        onVisualKeyDownCapture(event);
      }}
      onKeyDown={onKeyDown}
      // A pointer press anywhere in the panel ends VISUAL before descendant handlers run. Approve
      // still handles its click; a reason box enters BROWSE before a later Enter denies.
      // Capture runs ahead of descendant `onClick` and `onMouseDown`.
      onPointerDownCapture={() => {
        exitRegion();
        arrivalParkRef.current = null; // #22: a press in the panel is a place chosen by hand
      }}
      // Every link in the panel is a reply's (the page writes none of its own), and a click on one is the
      // browser's own navigation, which Rust launches with no look at where it goes. A link whose text is
      // not its own address shows that address first; a second click on it opens it. Capture phase, so
      // the default is gone before anything below sees the click; the click itself still propagates.
      onClickCapture={(event) => {
        const anchor = event.target instanceof Element ? event.target.closest<HTMLAnchorElement>("a[href]") : null;
        const root = containerRef.current;
        if (anchor === null || root === null) return;
        const now = Date.now();
        const url = linkClickNeedsLook(anchor, root, shownLinkRef.current, now);
        if (url === null) {
          shownLinkRef.current = null;
          return;
        }
        event.preventDefault();
        shownLinkRef.current = { el: anchor, row: rowOf(root, anchor), url, at: now };
        showFlash(`${url} — click again to open`);
      }}
      onFocus={(event) => {
        // Record a plain Tab's landing only at that key count and only if its default survived every
        // handler. Consume the first focus event so later effects or envelopes cannot create a
        // keyboard landing.
        const lastTab = tabAtKeyRef.current;
        if (lastTab !== null && lastTab.atKey === typingGuard.keyCount() && !lastTab.event.defaultPrevented) {
          placedRef.current = { el: event.target as HTMLElement, atKey: lastTab.atKey };
        }
        tabAtKeyRef.current = null;
        // The outermost stop (`stopOf`): a link inside a reply is in the reply's row, whatever the
        // reply's own HTML claims to be.
        const stop = containerRef.current === null ? null : stopOf(containerRef.current, event.target as HTMLElement);
        setEdgeFocused(stop !== null && stop.getAttribute("data-nav-stop") !== "row");
        // End VISUAL before handling focus on any panel element other than its root,
        // including a card's reason box, a button, the composer or the tab bar.
        if (containerRef.current !== null && event.target !== containerRef.current) exitRegion();
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
          `.agent-ui-scroller` in index.css). The tab bar goes above `errorBanner`. */}
      <div className="agent-ui-scroller">
        {/* While VISUAL/V-LINE is active, `MessageList` uses props frozen at entry except
            `cursor`, `focused` and `visual`, keeping its DOM stable under the selection.
            `frozenTimeline` computes rows from those same props to translate frozen DOM rows
            into live timeline keys. Express the live cursor by key in the frozen timeline's
            indices. Use -1 when its row is absent there; falling back to the live index would
            mark an unrelated frozen row as current. */}
        <MessageList
          state={frozenSnapshot?.state ?? state}
          turnDurations={frozenSnapshot?.turnDurations ?? turnDurations}
          sessionEnded={frozenSnapshot?.sessionEnded ?? sessionEnded}
          expanded={frozenSnapshot?.expanded ?? expanded}
          cursor={frozenTimeline === null ? cursor : (indexOfKey(frozenTimeline, timeline[cursor]?.key ?? "") ?? -1)}
          timeline={frozenTimeline ?? timeline}
          detailed={frozenSnapshot?.detailed ?? detailed}
          thinkingDisplay={thinkingDisplay}
          // The band is drawn at full strength only while the list has the keys: in INPUT they are in
          // the composer, and a bright band left above it would compete with the box being typed in.
          focused={paneFocused && !edgeFocused && mode !== "input"}
          ruleOffers={frozenSnapshot?.ruleOffers ?? ruleOffers}
          yankedKey={yanked?.key ?? null}
          onAnswerPermission={answerPermission}
          answeredPermissions={frozenSnapshot?.answeredPermissions ?? answeredPermissions}
          onPermissionReason={(permissionId, reason) => {
            if (reason === "") permissionReasons.current.delete(permissionId);
            else permissionReasons.current.set(permissionId, reason);
          }}
          onOpenPath={openPath}
          onUnreadChange={(label, jump, afterSeq) => {
            setUnreadLabel(label);
            unreadJumpRef.current = jump;
            unseenAfterSeqRef.current = afterSeq;
          }}
          unseenSeed={unseenSeed}
          visual={mode === "caret" ? "caret" : mode === "visual" ? "visual" : mode === "vline" ? "vline" : null}
        />
        {keymapOpen && (
          <PanelErrorBoundary name="keys overlay" onError={() => overlayFailed("keys overlay", () => setKeymapOpen(false))}>
            <KeymapOverlay
              ref={keymapOverlayRef}
              onClose={closeKeymapByKey}
              windowKeys={keymapHelp.window}
              prefixKeys={keymapHelp.prefixKeys}
              prefixLabel={keymapHelp.prefix}
              panel={panelTable}
              tmuxSkipped={keymapHelp.tmuxSkipped}
              companion={editorLink !== null}
              plugin={keymapPlugin ?? undefined}
              claudeSlashCommands={!isExternalAgent(activeTab?.agent)}
              agent={isExternalAgent(activeTab?.agent) ? agentTitle(activeTab?.agent) : null}
              modes={hello === null ? undefined : tabPermissionModes(hello.permissionModes, activeTab?.agent)}
            />
          </PanelErrorBoundary>
        )}
        {confirm?.kind === "trust" && trustOverlay}
        {review !== null && (
          <PanelErrorBoundary name="review overlay" onError={() => overlayFailed("review overlay", () => setReview(null))}>
            <ReviewOverlay
              ref={reviewOverlayRef}
              state={review}
              onClose={() => setReview(null)}
              onCommentChange={(text) => setReview((current) => (current === null ? current : typeComment(current, text)))}
              onCommentAccept={(event) => {
                noteLineKey(event);
                const next = applyReviewKey(review, { kind: "accept" }, nextRequestId);
                setReview(next.state);
                if (next.effect !== null) runReviewEffect(next.effect, review.tab);
                returnKeysToRoot();
              }}
              onCommentCancel={(event) => {
                noteLineKey(event);
                setReview(applyReviewKey(review, { kind: "answer", answer: "cancel" }, nextRequestId).state);
                returnKeysToRoot();
              }}
            />
          </PanelErrorBoundary>
        )}
        {detail !== null && (
          <PanelErrorBoundary name="session details" onError={() => overlayFailed("session details", () => setDetail(null))}>
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
          </PanelErrorBoundary>
        )}
        {chooser !== null && (
          <PanelErrorBoundary name="chooser" onError={() => overlayFailed("chooser", onChooserLeave)}>
            <Chooser
              envelope={chooser}
              tabs={tabs?.tabs ?? []}
              active={tabs?.active ?? null}
              defaultMode={tabs?.defaultMode ?? "auto"}
              projectDir={hello?.projectDir ?? ""}
              newTabChord={keymapHelp.newTabChord}
              focusRequest={chooserFocusRequest}
              dropKeysRequest={emptyDropKeys}
              onSwitch={onChooserSwitch}
              onResume={onChooserResume}
              onNewSession={onChooserNewSession}
              onCloseTab={onChooserCloseTab}
              onRenameTab={onChooserRenameTab}
              onCycleMode={onChooserCycleMode}
              onCycleTabMode={onChooserCycleTabMode}
              onLeave={onChooserLeave}
              answerConfirm={answerConfirm}
            />
          </PanelErrorBoundary>
        )}
        {/* A bare /model or /effort reply opens this picker, positioned inside
            `.agent-ui-scroller` like the chooser above. */}
        {slashPicker !== null && (
          <PanelErrorBoundary name="picker" onError={() => overlayFailed("picker", cancelSlashPicker)}>
            <SlashPicker
              kind={slashPicker.kind}
              options={slashPicker.options}
              current={slashPicker.current}
              focusRequest={slashPickerFocusRequest}
              onChoose={chooseSlashOption}
              onCancel={cancelSlashPicker}
            />
          </PanelErrorBoundary>
        )}
        {/* The which-key box: `WHICH_KEY_DELAY_MS` after a leader/table
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
        queued={heldCount(queue)}
        pendingTool={oldestPendingTool(state)}
        mode={mode}
      />
      <TurnChanges state={state} review={reviewSettings} />
      <QueueLines items={queue} error={queueError} note={queueNote} />
      {/* Above the composer, deliberately -- the confirmation
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
        // The held-back slash commands are Claude Code's; another agent's tab sends a leading `/` as typed.
        claudeSlashCommands={!isExternalAgent(activeTab?.agent)}
        // The `/` menu's rows: the agent's own list when it reported one, Claude's built-ins on a Claude tab,
        // none otherwise (`completionSource`). Withheld while a y/n question is up: an open menu
        // would take the `Esc` that answers it, and the question owns the keys.
        slashCommands={confirm !== null ? null : completionSource(activeTab?.agent, state.availableCommands)}
        // `composerFocusRequest` is bumped by the `inputRequest` effect only when no overlay is open,
        // and by the `i`/`o`/`A` mode actions. Passing the raw request would let `Composer`
        // focus its textarea under an overlay.
        focusRequest={composerFocusRequest}
        caretOnFocus={composerCaret}
        // Only while the box can take a message: a landing on a disabled textarea could not focus it.
        hintTarget={!(sessionEnded || handingOff)}
        onModeChange={setMode}
        onSend={sendMessage}
        onDraftChange={mirrorDraft}
        running={turnInProgress}
        enterSteers={enterSteers}
        onQueue={queueMessage}
        onSendNow={(t) => {
          post({ type: "send_now", text: t });
          resumeFollowing(containerRef.current?.querySelector(".message-list"));
        }}
        history={history}
        // `↑` takes back only what is held: a message sent into the turn may already be read.
        queueCount={heldCount(queue)}
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
        // The `Ctrl+r` search's Enter/Tab/Escape reach the typing guard.
        onLineKey={noteLineKey}
      />
      {/* `gf` with several paths and the open `/` prompt each own every key through
          `onKeyDown` branches that run before `resolveKey`. Their placement here does not change that. */}
      {/* Each overlay has its own `PanelErrorBoundary`, so a render failure closes that overlay and
          flashes in the band without unmounting the panel. The pickers leave focus at the root, so
          closing them is enough. The two input lines hold focus and must return it as their Escape
          handlers do. */}
      {pathPick !== null && (
        <PanelErrorBoundary name="path picker" onError={() => overlayFailed("path picker", () => setPathPick(null))}>
          <PathPick paths={pathPick} />
        </PanelErrorBoundary>
      )}
      {linkPick !== null && (
        <PanelErrorBoundary name="link picker" onError={() => overlayFailed("link picker", () => setLinkPick(null))}>
          <LinkPick urls={linkPick} />
        </PanelErrorBoundary>
      )}
      {search !== null && (
        <PanelErrorBoundary
          name="search line"
          onError={() =>
            overlayFailed("search line", () => {
              // Esc's own effect: the cursor goes back to where the search started.
              setCursor(search.origin);
              setSearch(null);
              returnKeysToRoot();
            })
          }
        >
          <SearchBar
            query={search.query}
            focusRequest={lineFocusRequest}
            onChange={(query) => {
              setSearch({ query, origin: search.origin });
              const found = findMatch(timeline, query, search.origin, 1, true);
              setCursor(found ?? search.origin);
            }}
            onAccept={(event) => {
              noteLineKey(event);
              lastSearchRef.current = search.query;
              if (search.query !== "" && findMatch(timeline, search.query, search.origin, 1, true) === null) {
                showFlash(`pattern not found: ${search.query}`);
                setCursor(search.origin);
              }
              setSearch(null);
              containerRef.current?.focus({ preventScroll: true });
            }}
            onCancel={(event) => {
              noteLineKey(event);
              setCursor(search.origin);
              setSearch(null);
              containerRef.current?.focus({ preventScroll: true });
            }}
          />
        </PanelErrorBoundary>
      )}
      {commandLineOverlay}
      {/* The bottom band combines status, unread counts and context in one status row.
          `showcmd` reads the same `box` as the which-key popup but appears immediately, without
          `WHICH_KEY_DELAY_MS`, so `box` is computed unconditionally. The window-close y/n `prompt`
          uses the whole band to the right of the mode section. Backend details are available
          through `prefix i` and have no segment here. */}
      <StatusBand
        facts={{
          mode,
          // `cycleOffered` is ignored in short form, so passing `true` does not affect
          // this `modePill` result.
          pill: modePill(activeTab?.mode ?? "auto", true, true),
          showcmd: box !== null ? `${box.title}…` : null,
          message: flash?.text ?? null,
          prompt: confirmBandPrompt,
          warn: statusWarning(state.provider, activeTab?.failure ?? null),
          unread: unreadLabel,
          cards: state.pendingPermissions.length,
          queued: queue.length,
          context: sessionEnded ? null : contextFact(editorContext),
          // About the window, not the session: an ended session still has an editor beside it.
          link: editorLink,
          position: timeline.length === 0 ? null : `${cursor + 1}/${timeline.length}`,
          model: shortModel(state.model),
          // This tab's last reported usage, right of the model; nothing until one arrives.
          usage: usageSegment(state.usage),
          // How the latest turn ended when it did not complete, until the next turn starts.
          ending: latestTurnEnding(state),
          // A design decision: the card INPUT's Ctrl+y would approve, while the box has the keys.
          approve: ctrlYCard,
          // A finished turn of this tab changed files on disk; `c` shows them.
          review: activeTab !== null && reviewHints[activeTab.id] !== undefined ? { files: reviewHints[activeTab.id].files } : null,
          // An interrupted revert is named whatever `review.hint` says: a half-written file is not a diff notice.
          recovery: reviewRecovery.length > 0 ? { path: reviewRecovery[0].path, more: reviewRecovery.length - 1 } : null,
          // Commands this tab's agent left running after its turns ended; nothing while there are none.
          background: sessionEnded ? null : state.backgroundCalls,
        }}
        paneFocused={paneFocused}
        entryKey={panelEntryKey(keymapHelp.window, editorLink !== null)}
        onOpenDetail={() => post({ type: "open_detail" })}
        onJump={() => unreadJumpRef.current()}
      />
      <HintLayer root={containerRef.current} hints={hints} typed={hintTyped} />
    </div>
    </ProjectDirContext.Provider>
  );
}
