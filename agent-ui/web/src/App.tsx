import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { ProjectDirContext } from "./projectPath";
import { applyCallNotes, applyEvent, applySnapshot, initialState, keepAfterSidecarStop, resumeAttached } from "./reducer";
import { installDispatch, postToRust, nextRequestId } from "./bridge";
import type { NavKeyDirection, OutboundMessage, PanelKeysMode, PermissionDecision } from "./bridge";
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
import { EMPTY_PANEL_TABLE, isPlainAnswerKey, resolveKey } from "./keymap";
import type { KeyLike, KeymapHelp, PanelBinding, PanelMode, PendingPrefix } from "./keymap";
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
  linkOpensAtOnce,
  nextControl,
  permissionTarget,
  rowIndexOf,
  rowOf,
  stopOf,
  webLinks,
  HINT_ALPHABET,
} from "./nav";
import type { AnswerableItem, HintTarget } from "./nav";
import type { TimelineItem } from "./timeline";
import { pathsIn, viewText } from "./paths";
import type { PathRef } from "./paths";
import { PathPick } from "./components/PathPick";
import { LinkPick } from "./components/LinkPick";
import { acceptsEnvelope, activeTabInfo, countedTabTarget, forgetClosed, saveView, takeView, withoutHandoff } from "./tabs";
import type { TabViewState } from "./tabs";
import { EmptyTab } from "./components/EmptyTab";
import type { NavKeyRequest } from "./components/EmptyTab";
import { Composer } from "./components/Composer";
import type { RestoredDraft } from "./components/Composer";
import { TabBar } from "./components/TabBar";
import { MessageList } from "./components/MessageList";
import { Row } from "./components/Row";
import { classify, failureEvidence, remedyNamesR, sidecarStopped } from "./problems";
import { ActivityLine } from "./components/ActivityLine";
import { StatusBand } from "./components/StatusBand";
import { QueueLines } from "./components/QueueLines";
import { DetailPopover } from "./components/DetailPopover";
import { ContinueInTerminal } from "./components/TerminalHandoff";
import { HintLayer } from "./components/HintLayer";
import type { ShownHint } from "./components/HintLayer";
import { KeymapOverlay } from "./components/KeymapOverlay";
import { ReviewOverlay } from "./components/ReviewOverlay";
import { TrustPrompt } from "./components/TrustPrompt";
import { parseTrustCommand, resolveTrustKey, scrollTarget, TRUST_BAND_PROMPT, TRUST_WAIT_FLASH, trustFooter } from "./trust";
import type { TrustKeyEvent, TrustPromptEnvelope } from "./trust";
import {
  applyReviewKey,
  boxUnderCursor,
  failRequest,
  openReview,
  receiveDiff,
  receiveDraft,
  receivePreview,
  receiveRecovery,
  receiveReview,
  resolveReviewKey,
  scrollBoxFirst,
  typeComment,
  withStatus,
} from "./review";
import type { ReviewEffect, ReviewState } from "./review";
import type { ReviewRecoveryEntry } from "./types";
import { Chooser } from "./components/Chooser";
import { SlashPicker } from "./components/SlashPicker";
import type { SlashPickerKind } from "./components/SlashPicker";
import { PanelErrorBoundary } from "./components/PanelErrorBoundary";
import { barePickerCommand } from "./slashCommands";
import { parseEffortReply, parseModelReply } from "./slashPicker";
import { DRAFT_MIRROR_DELAY_MS, modePill, showTabBar } from "./tabs";
import { cardSummary, shortModel, usageSegment } from "./band";
import { latestTurnEnding } from "./turnEnding";
import type { ApproveFact, BandFacts } from "./band";
import type { AgentUiState, HandoffCommand, Hello, DetailRow, TabId, TabsEnvelope, TurnClock, ChooserEnvelope, ContextSummary, EditorLink, QueueItem, ProviderInfo } from "./types";
import { applyTheme } from "./theme";
import { applyEditorTyping } from "./typingCadence";

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

/** `hints` without `tab`'s entry -- the same object when it has none, so a clear that changes nothing
 *  renders nothing. */
function withoutHint(hints: Record<number, { turn: number; files: number }>, tab: number): Record<number, { turn: number; files: number }> {
  if (hints[tab] === undefined) return hints;
  const rest = { ...hints };
  delete rest[tab];
  return rest;
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

/** R4 (v1 audit P2-A4): the highest count a digit prefix (`3j`, `1000j`, …) accumulates to before a
 *  motion repeats it. Vim itself never errors on an absurd count -- it silently caps it (a headless
 *  `nvim --headless -u NONE -c "call feedkeys('99999999999999999999j', 'xt')" -c "echo v:count"`
 *  probe against this host's real nvim reports `999999999`, vim's own internal `long` overflow
 *  clamp, whatever the typed digit string). This panel's cap is far smaller and purpose-fit rather
 *  than matching that number: even repeating a cheap DOM lookup `MAX_MOTION_COUNT` times is already
 *  more than any real motion needs, and the cap exists only as a second bound underneath the
 *  boundary early-exit below (the loop stopping the moment a repeat makes no further progress) --
 *  belt and suspenders, not the primary fix. */
export const MAX_MOTION_COUNT = 9999;

/** Folds one more typed digit into a count-in-progress, capped at `MAX_MOTION_COUNT` on every step
 *  (not only at the end) so an arbitrarily long run of digits -- someone holding a number key --
 *  never builds a number past the cap even transiently. A pure function so R4's cap is testable
 *  without a DOM: see `onKeyDown`'s `action.kind === "count"` branch for the one call site. */
export function accumulateMotionCount(count: number | null, digit: number): number {
  return Math.min(MAX_MOTION_COUNT, (count ?? 0) * 10 + digit);
}

/** R2 (v1 picks): the row `gg` / `G` lands on. Bare, the first or the last row. With a count it is
 *  row N, counted from 1 and clamped to the rows there are -- vim's `{N}gg` and `{N}G` (`:help gg`,
 *  `:help G`), where a count past the end lands on the last line. A pure function so `onKeyDown`'s two
 *  readers of it (the scroll announcement and the jump itself) cannot disagree, and so the clamp is
 *  testable without a DOM. */
export function jumpTarget(to: "first" | "last", count: number | null, rows: number): number {
  const last = Math.max(rows - 1, 0);
  if (count === null) return to === "first" ? 0 : last;
  return Math.max(0, Math.min(count, rows) - 1);
}

/** v1 S1 (spec §2.1): what the band says when a typed `a`/`d`/`D`, or an Enter on a card button,
 *  did not answer because another key came within `TYPING_GUARD_MS` of it (`./typingGuard`). v1
 *  hardening (R2-3): it says to pause, since `Esc` then `a` -- the route the activity line names --
 *  is refused the same way when the two come close together, and "type" alone read as if the user
 *  had been typing. */
const TYPING_FLASH = "a / d answer a card only on their own — pause, then press again; i or Ctrl+j to type";
/** K01 (ruling R2): what the band says when `a`/`d`/`D`, or Enter/Space on a card's own button,
 *  came after a count (`3a`) -- a card answer takes no count, so the key answers nothing. */
const COUNT_ANSWER_FLASH = "a / d / D take no count — press it on its own";
/** K02 (ruling R3): what the band says when Enter on a card's own button came with a modifier held
 *  -- Shift included, which `isPlainAnswerKey` lets through for `D` -- and answered nothing. */
const MODIFIED_ENTER_FLASH = "Enter with a modifier answers no card";
/** K02 (ruling R3): what the band says when Enter on a card's own button came with no landing right
 *  before it -- focus put on that button by `l`/`h`, a Tab or a HINT -- and answered nothing. Before
 *  K02, `l`, a pause, `s`, Enter (the `:ls⏎` of the kbux study) approved the card natively. */
const ENTER_LANDING_FLASH = "Enter answers a card right after l, h or Tab onto its button";
/** K02 fix round 3 (review): what the band says when Enter on a card's own button, landed on, is on a
 *  card other than the one `a`/`d` would answer from the cursor (`permissionTarget`) -- a Tab walks on
 *  from one card's buttons into the next card's, and `l`/`h` then walk that card's, while the row
 *  cursor stays where it was. */
const ENTER_ELSEWHERE_FLASH = "Enter answers the card under the cursor — j / k onto it first";
/** v1 S4/F13 (spec §2.2): what the band says when `a`/`d`/`D` have no card under the cursor, nor a
 *  card gating the tool call under it, and no card waits anywhere -- instead of doing nothing
 *  silently. */
const NO_CARD_FLASH = "no card here — i, o, A or Ctrl+j to type";
/** v1 hardening (R2-10): the same refusal while a card IS waiting, just not under the cursor -- it
 *  points back to the card, as the activity line's "j to it" does, instead of only suggesting typing. */
const CARD_ELSEWHERE_FLASH = "a / d answer the card under the cursor — j / k onto it, then a / d";
/** v1 picks, Task 7 (R7): what the band says when `]p` / `[p` found no card waiting for an answer in this
 *  tab -- none at all, every one already answered from this panel, or a session that ended (its cards are
 *  inert) -- instead of doing nothing silently. */
const NO_WAITING_CARD_FLASH = "no card waiting here";
/** The v1-ui GUI pass (2026-09-27): what the band says when Enter or Space on the activity line's
 *  Stop did not interrupt because it came in the middle of typing (`TypingGuard.mayActAfterMotion`)
 *  -- "just do it" typed after an arrival reached Stop with its `j` and interrupted the turn. */
const STOP_TYPING_FLASH = "Stop takes a key only on its own — i or Ctrl+j to type";
/** Owner decision #39: what the band says when INPUT's `Ctrl+y` came within `TYPING_GUARD_MS` of
 *  another key (readline's `Ctrl+u`/`Ctrl+w` then `Ctrl+y` yank, typing right after it, a held
 *  key's repeat) and approved nothing. */
const CTRL_Y_TYPING_FLASH = "Ctrl+y approves only on its own — pause, then press it again";
/** #39 fix round 1 (Opus B-1): what the band says when INPUT's `Ctrl+y` came less than
 *  `TYPING_GUARD_MS` after the card it would approve became the one it names -- a card that just
 *  arrived, one that replaced a withdrawn card, or the next card right after an approval. */
const CTRL_Y_TARGET_FLASH = "Ctrl+y: the waiting card just changed — read it, then press again";
/** #39 fix round 1 (Opus I-1): what the band says when INPUT's `Ctrl+y` came right after a kill in the
 *  box (`Ctrl+u`, `Ctrl+w`, `Ctrl+k`), however long the pause -- readline's yank, never an answer. */
const CTRL_Y_YANK_FLASH = "Ctrl+y right after Ctrl+u, Ctrl+w or Ctrl+k is a yank, not an answer — press it again";

/** #39 fix round 1: the composer's own textarea (never the `Ctrl+r` search field, nor a card's
 *  reason box), the one place INPUT's `Ctrl+y` answers from. */
function isComposerBox(el: EventTarget | null): el is HTMLTextAreaElement {
  return el instanceof HTMLTextAreaElement && el.closest(".composer") !== null;
}

/** #39 fix round 1 (Opus I-1): a readline kill in the composer -- `Ctrl+u`, `Ctrl+w`, `Ctrl+k` with
 *  nothing else held -- after which `Ctrl+y` is a yank. */
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
/** What an open `/model`/`/effort` picker shows (owner trial item 2). */
type SlashPickerState = { kind: SlashPickerKind; options: string[]; current: string | null };

/** Envelopes that move the keys, or put something over the conversation, with no keydown this panel
 *  sees: each drops a waiting `a`/`d`/`D` (spec §2.1's cancel list -- `pane_focus`, an arrival, an
 *  overlay). `nav_key` is the v1 plan's `Ctrl+j`/`Ctrl+k` (its "Interfaces" section), which GTK
 *  claims before the page sees a key; a Set of strings, so naming it needs no type from that task.
 *  **The mode cycle key (Shift+Tab, R3, v1 audit P2-A2) is spec §2.1's cancel list too, but is NOT a
 *  member of this Set**: it is a real keydown, not an inbound envelope with no keydown of its own,
 *  so it cannot be named here -- `onModeKey`'s document-capture handler calls `typingGuard.onKey`
 *  directly instead (fix round 1: not a bare `cancel()`, which would drop a pending answer but never
 *  record the key itself, leaving Shift+Tab invisible to the guard's own "before" half), since its
 *  own `stopPropagation` (needed to keep Shift+Tab from WebKit's backward focus navigation) means the
 *  bubble-phase `onKeyDown` that feeds every OTHER key to `typingGuard.onKey` never runs for it. */
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

/** `Ctrl+e`/`Ctrl+y`'s box-first step (v1 trial item 5): the cursor row's capped tool result takes
 *  the unit only while at least part of it is on screen and it can still move that way -- where a
 *  mouse wheel over it would scroll it too. Otherwise `false`, and the unit scrolls the conversation.
 *
 *  It deliberately does not share `j`/`k`'s helper, whose off-screen branch brings the cursor row back
 *  into view and claims the unit. That is right for a cursor MOTION and wrong for a view scroll --
 *  vim's CTRL-E/CTRL-Y never move the view toward the cursor. With the box just below the view, held
 *  `Ctrl+y` would jump back down to the row each time it had scrolled it off and never get past it,
 *  and `Ctrl+e` would jump a whole box height. */
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

/** `j`/`k` while the `?` keymap overlay is open (spec §3.1): scrolls the overlay itself, by the
 *  same three-line step a tall row uses (`rowScrollStep`) rather than a fresh pixel constant --
 *  the plan asks for exactly this reuse. `el.scrollTop` clamps itself at both ends, the same as
 *  every other raw `scrollTop` write in this file, so there is nothing else here to bound. */
function scrollKeymapOverlay(el: HTMLDivElement | null, direction: 1 | -1) {
  if (el === null) return;
  el.scrollTop += direction * rowScrollStep(el);
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

/** D11 (visual-mode spec): everything `MessageList` normally reads live, captured once when VISUAL
 *  starts and handed back unchanged for as long as it is on -- only `cursor`/`focused`/`visual`
 *  itself keep moving. `null` outside VISUAL/V-LINE. */
type FrozenSnapshot = {
  state: AgentUiState;
  expanded: Record<string, boolean>;
  detailed: boolean;
  ruleOffers: Record<string, string>;
  answeredPermissions: ReadonlySet<string>;
  sessionEnded: boolean;
};

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

/** #22: where a reader was when the keys left the panel (`pane_focus` false), for the `arrive` that
 *  brings them back. One slot beside `viewStore`, never in it: an arrival must neither consume nor
 *  overwrite the view a tab switch saved, and a park dies with a switch (`tabs` arm). `following` is
 *  "at the bottom AND on the last row" -- in a conversation that fits the view `atBottom` alone is
 *  always true, and would send a reader who put the cursor on row 1 back to the last row. `atKey`:
 *  `typingGuard.keyCount()` at the leave, so any panel key since then makes the park stale. */
type ArrivalPark = { tab: TabId; view: TabViewState; cursorKey: string | null; following: boolean; atKey: number };

/** The scroll half of putting a saved view back (Task 9's switch restore and #22's arrival): `follow`
 *  re-arms following outright, as a send does; otherwise the write is announced as the user's, so
 *  `MessageList` does not fight it, and the list goes back to where it was. */
function applyViewScroll(list: HTMLElement, view: TabViewState, follow: boolean, stop = false) {
  // A restored view replaces whatever was easing, even one that began at the very position this writes.
  cancelListScroll(list);
  if (follow) {
    resumeFollowing(list);
    return;
  }
  // `stop` (#22, fix round): a view that was not following must come back not following, whatever the
  // list decided while it was away -- a card resolved above the reader shrinks the list, the browser
  // clamps `scrollTop` to the new end, and `MessageList` reads that scroll as "at the bottom" and
  // re-arms following, so the next streamed row would snap the view (and, through R1's clamp, the
  // cursor) away from the row just restored. "up" stops following outright, as `k`/`gg` do.
  noteUserScroll(list, stop ? "up" : "unknown");
  list.scrollTop = view.scrollTop;
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
  /** #22: see `ArrivalPark`. Taken by the `pane_focus` arm when the keys leave, used and cleared by the
   *  next `arrive`, dropped by anything that means the reader chose a place themselves while away (a
   *  wheel or touch on the list, a press in the panel) or by a route that lands elsewhere (a switch,
   *  `enter_input`, `focus_permission`). */
  const arrivalParkRef = useRef<ArrivalPark | null>(null);
  /** The typing guard's key count when an `arrive` or `focus_permission` envelope reached this page,
   *  held until the landing it asked for has run. Both landings happen in effects, so only after React
   *  has rendered the envelope; `shell` moves GTK focus here and sends the envelope at the same moment,
   *  so the user's first key can be handled in between, against the panel as it is drawn. A landing
   *  that ran over that key afterwards undid it -- the first `v` after `Ctrl+l` started CARET and the
   *  landing ended it again before it was ever painted, so only a second `v` seemed to work. A landing
   *  whose count no longer matches yields: the key that got there first already decided. */
  const landingAtKeyRef = useRef<number | null>(null);
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
  /** v1 polish item 6: the conversation each failed tab showed when its sidecar stopped under it
   *  (`keepAfterSidecarStop`), so it stays on screen as a lost session -- across a switch away and
   *  back too, since Rust sends no snapshot for a failed tab. Dropped once the tab is no longer
   *  failed (`r`) or is closed. Keyed by tab, read during render. */
  const keptStates = useRef(new Map<TabId, AgentUiState>());
  /** `state` for the bridge listener (installed once), so the `error` arm can keep what was shown. */
  const stateRef = useRef(state);
  stateRef.current = state;
  const sessionStarted =
    activeTab !== null &&
    (activeTab.state === "live" ||
      activeTab.state === "ended" ||
      (activeTab.state === "failed" && keptStates.current.has(activeTab.id)));
  /** `sessionStarted` for the bridge listener, which is registered once and would otherwise read
   *  its first render's value forever. Assigned during render, so it is current by the time any
   *  later envelope arrives. */
  const sessionStartedRef = useRef(false);
  /** Whether `<leader>` `mode.cycle` (and, by the same rule, Shift+Tab) would flash rather than post
   *  -- `modeKeyRoute`'s own routing (v1, D6: the wave-5 `SetPermissionMode` capability this used to
   *  read is gone; only an `ended`/`failed` tab already in bypass still cycles, to leave it). */
  const modeFixed =
    modeKeyRoute({
      confirmOpen: false,
      chooserOpen: false,
      tabState: activeTab?.state ?? null,
      tabMode: activeTab?.mode ?? null,
    }) === "fixed";
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
   *  Only the kinds whose replies need special handling are recorded. An interrupt has no entry and
   *  comes back as `undefined`, which is correct rather than a gap: its refusal takes the plain
   *  "show the reason" path. A permission response is recorded (`"permission"`, with its card's id)
   *  since Codex's whole-branch review: Rust refuses one and leaves the card waiting (an "Always
   *  allow" whose rule could not be saved, ruling 16), and the card must then take an answer again
   *  instead of staying answered here for good. Every recorded request gets exactly one
   *  `command_result` and is deleted there, so this cannot grow.
   *
   *  `"editor"` covers `edit_draft`/`open_path`/`view_in_editor` (Task 8/15): all three reach the
   *  scratch editor in Rust, and a refusal of any of them is a footer flash (`showFlash`), not the
   *  banner -- a scratch-editor round trip that a `gf` or `Ctrl+g` failed to start is a footer
   *  nicety, not something that should fill the space a real conversation error gets.
   *
   *  `"link"` (v1 picks, Task 8, R6) is `gx`'s `open_url`: Rust refuses an address that is not a plain
   *  http(s) one, and that refusal is the same footer flash -- unrecorded, it would fall through to
   *  `setCommandNotice`, the banner for errors that break the conversation.
   *
   *  `"picker-send"` (v1 trial seam review finding 2, 2026-09-28): `chooseSlashOption`'s own send,
   *  distinct from `"send"` even though both post `send_message` -- an ordinary send's text really
   *  did leave the composer box (the optimistic clear `Composer.tsx`'s own `submit` does), so
   *  `"send"`'s refusal restores it there; the picker's choice never touched the box at all
   *  (`chooseSlashOption` calls `sendMessage` directly, never through the composer), so restoring
   *  it into whatever the box happens to hold right then -- a draft the user quoted or typed for a
   *  wholly unrelated reason -- would corrupt it. Its refusal is a footer flash alone.
   *
   *  `"review-command"` is what the overlay's keys do beyond reading: revert, undo, comment, send, recover. A
   *  refusal, or a word of success, is the overlay's status line; a comment's answer is a `review_draft` and a
   *  send's first answer a `review_send_preview`, which end the record without a `command_result`.
   *
   *  `"review"` is the review overlay's `review_request`/`review_diff_request`: Rust refuses one with a
   *  reason (no session, no such turn, git unavailable) and no envelope follows, so the overlay says it in
   *  its own header rather than in the conversation's banner.
   *
   *  `"trust"` is a trust question's answer or `Escape`, and `:trust`/`:untrust`. Rust says in a notice what
   *  the user needs to know about one (out of date, asked again, could not be recorded); a refusal without
   *  one is an `n`, an `Escape` or a move away from the question, which the user did themselves. Neither is
   *  drawn again, and never in the banner reserved for what breaks the conversation. */
  const inFlight = useRef<
    Map<
      string,
      {
        kind: "send" | "handoff" | "editor" | "link" | "permission" | "picker-send" | "review" | "review-command" | "trust";
        tab: TabId;
        text?: string;
        permissionId?: string;
        /** An `open_in_editor` from the review overlay: its answer, good or bad, is the overlay's status line. */
        review?: true;
      }
    >
  >(new Map());
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
   *  a card is waiting (P1, unchanged: lands there instead) or the panel lands BROWSE -- on the last row
   *  with following resumed for a reader who was following, else where they were (#22) -- never the
   *  composer, which is the whole point of the reversal. */
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
  /** K01 (fix round 1): bumped by `dropPendingKeys()`, so a cancel route drops the empty tab's own
   *  waiting prefix and leader sequence too (`EmptyTab`'s `dropKeysRequest`), not only this
   *  component's refs. A counter for the reason `arriveRequest` is one. */
  const [emptyDropKeys, setEmptyDropKeys] = useState(0);
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
  /** What the reader has on screen now, from the live refs: what a tab switch parks (Task 9) and what
   *  #22's leave parks, one capture for both. `atBottom` is Task 9's own formula. */
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
      // "while following" reset (wave 3, Task 3).
      unseenAfterSeq: atBottom ? null : unseenAfterSeqRef.current,
    };
  }
  /** Visual-mode spec, D2-D5: the model VISUAL/V-LINE keep between keys (the two caret ends, the
   *  linewise flag, the goal column), and the native selection's own snapshot from the last
   *  rebuild (D8's own "still the one VISUAL built" check). `null` outside VISUAL/V-LINE -- `mode`
   *  itself is what the rest of this file reads to know whether it is on. */
  const visualModelRef = useRef<VisualModel | null>(null);
  const visualBuiltRef = useRef<BuiltSelection | null>(null);
  /** D5: the count VISUAL/V-LINE's own digits accumulate, applied to the next motion and reset by
   *  it -- `countRef`'s own twin, kept separate because BROWSE's `onKeyDown` (which owns `countRef`)
   *  never runs for a VISUAL key (D13: the capture-phase handler stops its propagation), so sharing
   *  one ref between the two would leave whichever mode did not just run holding a stale value. */
  const visualCountRef = useRef<number | null>(null);
  /** D5's region-local pending `g` (added for 3a, §9: `gg`/`G`, and D2's `gv` reservation) --
   *  `visualCountRef`'s own twin, kept separate from BROWSE's `pendingRef` for the identical reason:
   *  the capture-phase region handler never lets a key reach `onKeyDown`, which owns `pendingRef`.
   *  Reuses `PendingPrefix`'s `"g"` value purely as a carrier through `resolveKey`'s `ctx.pending`;
   *  the region never arms `"z"`/`"["`/`"]"`, which stay BROWSE-only. */
  const regionPendingGRef = useRef<PendingPrefix | null>(null);
  /** D11: the props `MessageList` is frozen to while VISUAL is on -- see `FrozenSnapshot`'s own doc
   *  comment. State, not a ref: `MessageList`'s own render depends on it. */
  const [frozenSnapshot, setFrozenSnapshot] = useState<FrozenSnapshot | null>(null);
  /** `frozenSnapshot` as of the last render, i.e. whether the DOM on screen right now is the frozen
   *  list (fix round 3, review finding D11). Read by the handlers and effects that turn the LIVE
   *  cursor into a DOM row -- the `[cursor]` reveal, R1's scroll clamp, `focus_permission`'s own
   *  reveal -- none of which may do so while the list on screen is the frozen one: the live cursor
   *  indexes the live timeline, and the two differ once a row arrives (a queued prompt sent at a
   *  turn's end, a linked card). Assigned during render, like `cursorRefForScroll`, so it describes
   *  the DOM the commit that follows puts on screen -- `exitRegion`'s own `setFrozenSnapshot(null)`
   *  does not clear it until the thawed list has actually rendered. */
  const frozenSnapshotRef = useRef<FrozenSnapshot | null>(null);
  frozenSnapshotRef.current = frozenSnapshot;
  /** A `focus_permission` reveal waiting for the thawed list (see its layout effect, below the
   *  `permissionRequest` effect). */
  const revealAfterThawRef = useRef<number | null>(null);
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
  /** v1 hardening (ruling R2): the cards this tab has answered from this panel, by `permissionId`,
   *  whichever way -- a card's own button or `a`/`d`. This is the card's old `answered` guard lifted
   *  out of `PermissionCard`, because `a`/`d` no longer press the card's button (a DOM query for it
   *  could find a button a model reply drew): they call `answerPermission` with the card's id, and
   *  the double-answer protection has to hold on that path too. `answeredRef` is the same set,
   *  current between renders, for the check `answerPermission` makes and for a wait `a`/`d` are
   *  still in. Tab-scoped like `ruleOffers`: reset on a switch, and an id leaves it once its card
   *  is no longer pending (the effect after `answerableItems`). */
  const [answeredPermissions, setAnsweredPermissions] = useState<ReadonlySet<string>>(() => new Set());
  const answeredRef = useRef<ReadonlySet<string>>(answeredPermissions);
  /** #39 fix round 1 (Opus B-1): the card INPUT's `Ctrl+y` would approve, as `<tab>:<permission id>`,
   *  and whether it has been that card for `TYPING_GUARD_MS` (the layout effect after
   *  `ctrlYTargetKey`). */
  const ctrlYTargetRef = useRef<{ key: string | null; settled: boolean }>({ key: null, settled: false });
  /** #39 fix round 1 (Opus I-1): whether the last non-modifier key the panel saw was a kill in the
   *  composer (`isKillKey`), so a `Ctrl+y` after it is readline's yank whatever the pause. */
  const lastKeyWasKillRef = useRef(false);
  /** #39 fix round 1 (Opus I-3, Codex): whether the composer's own textarea has focus -- what the band's
   *  `Ctrl+y` segment and `approveOldestByKey` both require. Followed through `focusin`/`focusout`. */
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
   *  `d` sends it exactly as the card's own Deny does (ruling R2). A ref, not state: typing in the
   *  box must not re-render the whole panel. */
  const permissionReasons = useRef(new Map<string, string>());
  const [editorContext, setEditorContext] = useState<ContextSummary | null>(null);
  // Companion mode: where the panel stands with the editor beside it. `null` is the one-window mode,
  // where Rust never sends one.
  const [editorLink, setEditorLink] = useState<EditorLink | null>(null);
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
  // Whether Super is held, from its own keydown/keyup: WebKitGTK reports it on no other key's event,
  // and `isPlainAnswerKey` reads this for `a`/`d`/`D` and the bypass `y` (heldSuper.ts). The first
  // effect in this component, so its window-capture listener runs before HINT's, which stops keys.
  // R9: `Ctrl+[` is Esc everywhere in the panel (ctrlBracket.ts). Its document-capture listener is
  // registered here too, so it runs ahead of `onModeKey`'s below and every handler reads a plain
  // Escape; a window-capture listener still runs before it, so HINT's own swallow wins while pending.
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
  /** Whether the view eases to the next landing (`revealRow`'s `animate`): set only by a lone `j`/`k`,
   *  read and reset by the cursor-follow effect with `landingRef`. */
  const landingAnimateRef = useRef(false);
  /** C1c: whether the last `j` moved, so the first repeat of a held `j` that then stops at the last
   *  stop still flashes once (the v1-ui GUI pass, 2026-09-27) -- see the move branch of `onKeyDown`. */
  const heldMoveRef = useRef(false);
  /** R4: the open `/` prompt and where the cursor was when it opened; `lastSearchRef` is what `n`/`N` repeat. */
  const [search, setSearch] = useState<{ query: string; origin: number } | null>(null);
  const lastSearchRef = useRef("");
  /** K02 (ruling R4): the open `:` command line's text, or `null` while it is closed. It runs no
   *  command: it exists so `:ls⏎`-style keys land in a box instead of on a card. Closed wherever the
   *  `/` prompt is, and for the same reasons -- an overlay that hands the keys to the root when it
   *  closes (the chooser, a tab rename) included, since fix round 1 -- and the two are one command
   *  line: opening either closes the other. */
  const [exLine, setExLine] = useState<string | null>(null);
  /** K02 fix round 2 (review): bumped by every `/`, `:` and `panel.search`, and read by both lines'
   *  `SearchBar` as its `focusRequest`, so a line already open when its key comes again -- the keys
   *  had gone elsewhere, a click took them -- takes them back instead of only being wiped (`:`, a
   *  click, `:`, `l`, Enter walked onto Approve and pressed it). A line just opened takes them as it
   *  mounts anyway. The two are never open at once, so one counter serves both. */
  const [lineFocusRequest, setLineFocusRequest] = useState(0);
  function moveCursorTo(index: number) {
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    noteUserScroll(list, index < cursor ? "up" : "down");
    setCursor(index);
  }
  /** The first key of a two-key BROWSE sequence (`resolveKey`'s `{kind:"pending"}`: `g`, `z`, `[`, `]`
   *  or, since v1 picks Task 6, `Ctrl+w`), or `null` between sequences. Cleared on EVERY key
   *  `onKeyDown` sees, so anything but the matching second half cancels it, and on every `pane_focus`
   *  envelope, so a pane switch the WebView never saw as a key cancels it too; handed to `resolveKey`
   *  in its context so the key table itself keeps no memory. (Formerly `pendingGRef`, a plain
   *  boolean, before `[[`/`]]` gave BROWSE a second prefix.) */
  const pendingRef = useRef<PendingPrefix | null>(null);
  /** R4: the count accumulated from `1`-`9` then `0`-`9`, applied to the next `j`/`k`/`[[`/`]]` --
   *  and, since R2 of the v1 picks, to `G`, `gg`, `gt` and `gT` (a prefix hands it on to its second
   *  key) -- and reset by it (or by anything else that runs). `null` when no digit has been pressed
   *  yet. A ref for the same reason `pendingRef` is one: read once by `onKeyDown`, never rendered. */
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
  /** K01: a prefix, a count and a leader sequence are one thing in progress; every cancel route
   *  (pane_focus, arrive, an overlay, a tab switch, Shift+Tab) drops all three together -- the
   *  empty tab's own prefix and sequence included, which live in `EmptyTab` (fix round 1: `g`,
   *  Shift+Tab, `i` swallowed the `i` there). Only refs and state setters, so the listeners installed
   *  once, with the first render's closure, may call it. */
  function dropPendingKeys() {
    pendingRef.current = null;
    countRef.current = null;
    clearSequence();
    setEmptyDropKeys((n) => n + 1);
    // v1 picks, Task 8 (R6): a `gx` pick waiting for its letter is a pending key too -- left up across a
    // cancel route, the next letter typed would open a link and be swallowed.
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
        // K02 fix round 1: one command line, as in vim -- the `/` prompt replaces an open `:` line.
        setExLine(null);
        setSearch({ query: "", origin: cursor });
        setLineFocusRequest((n) => n + 1);
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
        // through `modeKeyRoute`, the same rule Shift+Tab uses (R4). v1 (D6): every route but
        // `ended`/`failed`-not-already-bypass now always posts -- there is no capability left to
        // gate on, and a move into bypass gets its own y/n from Rust rather than being refused or
        // applied silently here.
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
  /** Whether the `?` keymap overlay is open (spec §3). Toggled by `resolveKey`'s `{kind:"keymap"}`
   *  (opening only -- see `onKeyDown`'s dedicated branch below for why closing never goes through
   *  `resolveKey` at all) and by the handful of places that must force it shut: the session ending,
   *  the start screen coming back, and a HINT starting elsewhere in the window. */
  const [keymapOpen, setKeymapOpen] = useState(false);
  /** The turn review overlay (`c` in BROWSE), or `null` when it is closed. Like `?` it is drawn over the
   *  conversation and owns every key while it is open; unlike `?` it is read-only state the shell fills in
   *  (`./review` is the whole of its logic). Closed wherever `?` is forced shut, and by a switch of tab. */
  const [review, setReview] = useState<ReviewState | null>(null);
  /** The review overlay's own root, so `j`/`k` can scroll an open hunk's box before they move the cursor. */
  const reviewOverlayRef = useRef<HTMLDivElement>(null);
  /** Turn review: the finished turn of each tab that changed files, for the band -- kept per tab, because
   *  the shell sends it for the tab it names even while another is on screen. Gone once the overlay is
   *  opened on that turn, when a new turn starts, or when the shell sends `files: 0`. */
  const [reviewHints, setReviewHints] = useState<Record<number, { turn: number; files: number }>>({});
  /** Interrupted reverts the journal still holds (`review_recovery`, the project's, so not a tab's). The band
   *  names the first whatever the hint setting says, and an overlay opened later starts with them. Nothing
   *  is kept here but what the shell last said. */
  const [reviewRecovery, setReviewRecovery] = useState<ReviewRecoveryEntry[]>([]);
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
  /** `gx` (v1 picks, Task 8, R6) with several links -- or a titled one, or one nobody can see -- waiting for
   *  its letter: the full normalized addresses it lists, in order, or `null` between picks. Like `pathPick`
   *  it owns every key while set (`onKeyDown`, ahead of the key table), but it is also dropped by every route
   *  away (`dropPendingKeys`, the tab switch): a pick left up would open a link on a later letter, over a
   *  conversation that no longer holds it. */
  const [linkPick, setLinkPick] = useState<string[] | null>(null);
  /** N2/R3: `open_path` and `view_in_editor` share `"editor"` in-flight bookkeeping with Task 8's
   *  `edit_draft` (see `inFlight`'s own doc comment). Neither carries a `tab`-scoped reply payload of
   *  its own to restore, only a refusal to flash. */
  function openPath(ref: PathRef) {
    const requestId = nextRequestId();
    inFlight.current.set(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_path", request_id: requestId, path: ref.path, ...(ref.line === null ? {} : { line: ref.line }) });
  }
  /** `c` in BROWSE: opens the review overlay on this tab's latest turn and asks the shell for it. The
   *  overlay is drawn at once, saying it is loading, and filled in by the `review` envelope that answers
   *  this request id (or told why not by the refusal). Recorded as `"review"`, so a refusal is the
   *  overlay's own header and not the conversation's banner. */
  function openReviewOverlay(tab: TabId) {
    const requestId = nextRequestId();
    inFlight.current.set(requestId, { kind: "review", tab });
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
        inFlight.current.set(effect.requestId, { kind: "review", tab });
        postToRust({ type: "review_request", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope });
        return;
      case "request-diff":
        inFlight.current.set(effect.requestId, { kind: "review", tab });
        postToRust({ type: "review_diff_request", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path });
        return;
      case "open": {
        // The editor opens it at the hunk's first line when a patch has been loaded, else at the top. It
        // is the shell that knows whether the turn's hunks are drawn there too, so it is asked, by turn.
        const requestId = nextRequestId();
        inFlight.current.set(requestId, { kind: "editor", tab, review: true });
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
        inFlight.current.set(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_revert", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path, target: effect.target });
        return;
      case "undo":
        inFlight.current.set(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_undo", request_id: effect.requestId, tab });
        return;
      case "comment-add":
        inFlight.current.set(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_comment_add", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path, from: effect.from, to: effect.to, text: effect.text });
        return;
      case "comment-remove":
        inFlight.current.set(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_comment_remove", request_id: effect.requestId, tab, id: effect.id });
        return;
      case "send":
        inFlight.current.set(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_send", request_id: effect.requestId, tab, confirm: effect.confirm });
        return;
      case "recover":
        // Window-level: the journal is the project's, so this names no tab.
        inFlight.current.set(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_recover", request_id: effect.requestId, entry: effect.entry, answer: effect.answer });
        return;
      case "copy":
        void navigator.clipboard?.writeText(effect.text);
        showFlash(`copied ${effect.text}`);
        return;
    }
  }
  /** `gx` (R6): hands one address -- always a `webLinks` `url`, the normalized `href` the reader was shown -- to
   *  Rust, which re-checks it (`web_url`) and opens it in the system browser. Recorded as `"link"`, so a
   *  refusal is a footer flash (see `inFlight`). */
  function openUrl(url: string) {
    const requestId = nextRequestId();
    inFlight.current.set(requestId, { kind: "link", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_url", request_id: requestId, url });
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
   *  in `onKeyDown`, through `answerConfirm`.
   *
   *  v1 (spec `2026-09-27-v1-mode-design.md`, D2/D11) adds kind `"bypass"`: a move into bypass, set
   *  by a `confirm_bypass` envelope. `scope`/`tab`/`nonce`/`lines` are that envelope's own values,
   *  echoed back verbatim on `y`/`Y` -- this side never counts cards or invents a nonce. `openedAt`
   *  is stamped with `performance.now()` the moment THIS envelope is recorded (never `Date.now()`,
   *  so it shares a clock with the guard that reads it), which is also what makes a second envelope
   *  arriving while one is already open a clean Reprompt: the whole object is replaced, so its
   *  250ms on-screen wait restarts along with everything else. */
  const [confirm, setConfirm] = useState<
    | { kind: "close"; tab: TabId; lines: string[] }
    | { kind: "close_others"; lines: string[] }
    | { kind: "bypass"; tab: TabId | null; scope: "tab" | "default"; nonce: number; lines: string[]; openedAt: number }
    // A restore found a tab saved in bypass and asks before giving it back: `y` as saved, `n` in auto.
    // The same on-screen wait and lone-key guard as `bypass`, since a `y` here also enters bypass.
    | { kind: "restore"; nonce: number; lines: string[]; openedAt: number }
    // The question before this tab's session loads the project's own Claude configuration. `view` is the
    // envelope as shown, and an answer echoes its `fingerprint` and `findingsDigest` and nothing recomputed
    // here. The same on-screen wait and lone-key guard as `bypass`, since a `y` loads a repository's hooks.
    // `flashAfter` is the flash counter when it opened: only a flash made since then is about this question.
    | { kind: "trust"; tab: TabId; nonce: number; view: TrustPromptEnvelope; lines: string[]; openedAt: number; flashAfter: number }
    | null
  >(null);
  /** `prefix w` (spec §3.6): the open-tabs-then-records overlay, or `null` when closed. Set by a
   *  `chooser` envelope; not tab-scoped (`tabs.ts`'s `TAB_SCOPED` omits it) since it is a
   *  window-wide picker, not a view of the active tab. Wave 4 R2: no longer opened at launch (D10
   *  is gone), only by this key. */
  const [chooser, setChooser] = useState<ChooserEnvelope | null>(null);
  /** Owner trial item 2 (2026-09-28): the picker a bare `/model`/`/effort` reply opens, or `null`
   *  when closed. Opened purely client-side (no Rust envelope, unlike `chooser`) by the `events`
   *  handler below the moment a `turn_completed` following one of those bare sends parses -- see
   *  `pendingSlashPickerRef` and `../slashPicker`. */
  const [slashPicker, setSlashPicker] = useState<SlashPickerState | null>(null);
  /** Whole-branch review finding 4 (v1 trial, 2026-09-28): a parsed `/model`/`/effort` reply on its
   *  way to `slashPicker`, held for one render so the effect below can see every overlay as that
   *  render left it -- the `events` handler is installed once and cannot. A reply that finds another
   *  overlay open (the chooser, a rename, the `/` prompt, `?`, the detail popover, the handoff
   *  confirm, a `gf` pick, a y/n prompt) opens no picker: it stays what it already is in the
   *  transcript, the CLI's own text. The picker used to open on top of whichever it was, and after a
   *  focus round trip `takeKeys` gave the keys to the chooser underneath, so Enter answered the
   *  chooser instead of choosing a model. */
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
   *  Also a single flag rather than a per-tab one (Codex review finding, fix round): the tab-switch
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
  /** Wave 3 Task 1 (launch-chooser bug investigation, `~/.cache/launch-chooser-bug/`): true while an
   *  overlay drawn OVER the conversation or the empty tab owns the keys -- the chooser, a tab
   *  rename, or the `/` search prompt (the live layout's only, hence `sessionStarted`: `search`
   *  can outlive the tab it opened on, and a stale value under the empty layout, where `SearchBar`
   *  never renders, would otherwise strand the keys for good). `?`, the detail popover and the
   *  handoff confirm are NOT here (decision, spec §2.4/Review Focus 1): `pane_focus`/`arrive`
   *  already close those three, so nothing needs to re-check them here. Read by every effect below
   *  that could otherwise steal the keys out from under one of these three overlays. Owner trial
   *  item 2 (2026-09-28) adds `slashPicker` alongside `chooser`: the same reasoning -- it is drawn
   *  over the conversation and must keep the keys the same way. K02 adds the `:` command line
   *  (`exLine`), which is drawn and gated exactly like the `/` prompt. */
  const overlayOpen =
    chooser !== null ||
    slashPicker !== null ||
    renaming !== null ||
    (search !== null && sessionStarted) ||
    (exLine !== null && sessionStarted) ||
    review !== null;
  /** For the document replay (installed once): see its K04 fix-round note. */
  const overlayOpenRef = useRef(overlayOpen);
  overlayOpenRef.current = overlayOpen;
  /** Counters `takeKeys` (below) bumps to ask a specific overlay/composer to re-focus itself,
   *  mirroring the existing `inputRequest`/`arriveRequest` convention: a plain number so a second
   *  request while the first is still pending is never silently coalesced away by React (an
   *  unchanged boolean would be). */
  const [chooserFocusRequest, setChooserFocusRequest] = useState(0);
  /** Fix round (Codex review finding: `takeKeys` had no `slashPicker` branch, so a GTK focus round
   *  trip while the picker was open handed the keys to the container root instead, leaving `j`/
   *  `k`/`Enter`/`Escape` dead). Same convention as `chooserFocusRequest`, read by `SlashPicker`'s
   *  own `focusRequest` prop. */
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
  /* The keys MOVING into a text field after a bypass or restore question appeared (a click on the composer, a
     rename field) end the question: what is typed there is text, and the `y` of "you should" must not give a tab
     bypass just because the question was still up. Only a move counts -- a field that already had the keys when
     the question opened (Shift+Tab pressed in the composer) is where an INPUT user answers, so its `y` still does. */
  const confirmEntersBypass = confirm?.kind === "bypass" || confirm?.kind === "restore";
  useEffect(() => {
    if (!confirmEntersBypass) return;
    function onFocusIn(event: FocusEvent) {
      if (!isEditableElement(event.target)) return;
      setConfirm((c) => (c?.kind === "bypass" || c?.kind === "restore" ? null : c));
    }
    document.addEventListener("focusin", onFocusIn, true);
    return () => document.removeEventListener("focusin", onFocusIn, true);
  }, [confirmEntersBypass]);
  const chooserOpenRef = useRef(false);
  chooserOpenRef.current = chooser !== null;
  const tabsRef = useRef<TabsEnvelope | null>(null);
  tabsRef.current = tabs;
  /** D11: the last non-modifier keydown seen ANYWHERE in the panel BEFORE the one currently being
   *  processed -- read by `answerConfirm`'s `bypassYesCounts` check. Deliberately one keystroke
   *  BEHIND `currentKeyAtRef` (below): the document-capture effect that maintains both runs in the
   *  CAPTURE phase, which always fires before the bubble-phase `onKeyDown` that calls
   *  `answerConfirm` for the very same event -- so if this ref held the CURRENT keystroke's own
   *  time by the time that check ran, a lone `y`/`Y` would always see itself as "another key within
   *  the guard" and bypass could never be entered at all. Starts at `-Infinity` so a `y`/`Y` with no
   *  other key at all since mount is judged purely on how long the prompt itself has been on screen
   *  (`bypassYesCounts`'s `openedAt` half), never cancelled by a keystroke that never happened. */
  const lastKeyAtRef = useRef(-Infinity);
  /** The running "most recent non-modifier keydown" clock `lastKeyAtRef` lags one keystroke behind;
   *  private to the document-capture effect below. */
  const currentKeyAtRef = useRef(-Infinity);
  /** Defect 2 (2026-09-27 sandbox GUI pass): the key `answerConfirm` last consumed from a y/n
   *  prompt -- answered or cancelled, either way -- so its own auto-repeats can be swallowed rather
   *  than falling through as an ordinary key once `confirm` is null again. Mirrors
   *  `shell::hint::Held` (`shell/src/hint.rs`, dated record 2026-09-19 review): remember the held
   *  key, not a timer, and stop remembering it the moment its keyup arrives (the effect right
   *  below) -- never on a fresh, different key, so an ordinary later hold of the SAME key (nothing
   *  to do with any prompt) is never mistakenly swallowed just because this key answered a prompt
   *  once, earlier in the session. `null` when nothing consumed by a prompt is still physically held. */
  const promptSwallowKeyRef = useRef<string | null>(null);
  /** The trust question's own scroll box, and whether the key before this one was the first `g` of `gg`. */
  const trustOverlayRef = useRef<HTMLDivElement>(null);
  const trustPendingGRef = useRef(false);
  /** The trust question Rust sent last, until it can take the keys. */
  const [heldTrust, setHeldTrust] = useState<TrustPromptEnvelope | null>(null);
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
      lines: [TRUST_BAND_PROMPT],
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
  /** One ordered view of the conversation, kept in step with the cursor/expand keys below. See
   *  `MessageList`'s own copy of this memo for why it is keyed on `state` as a whole; `expanded` and
   *  `detailed` join it here for the same reason P2's collapsed runs do -- `buildDisplay` folds or
   *  unfolds rows by both, so a change to either has to recompute what the cursor actually indexes. */
  const timeline = useMemo(
    () => buildDisplay(buildTimeline(state), { expanded, detailed, turnRunning: state.activeTurnId !== null }),
    [state, expanded, detailed],
  );
  /** For the dispatch handler (installed once), as `cursorRef`: #22's park names the cursor's row by key. */
  const timelineRef = useRef(timeline);
  timelineRef.current = timeline;
  /** D11: the SAME pure computation as `timeline` above, over `frozenSnapshot` instead of the live
   *  render -- functionally identical to what `MessageList` recomputes internally once it is handed
   *  the frozen props, so a row found in the frozen DOM can be placed at this array's own index
   *  (`rowIndexOf`) and its `key` read back out of it, without indexing the DOM by the live cursor
   *  while VISUAL is on (D11's own rule). `null` outside VISUAL/V-LINE. */
  const frozenTimeline = useMemo(
    () =>
      frozenSnapshot === null
        ? null
        : buildDisplay(buildTimeline(frozenSnapshot.state), {
            expanded: frozenSnapshot.expanded,
            detailed: frozenSnapshot.detailed,
            turnRunning: frozenSnapshot.state.activeTurnId !== null,
          }),
    [frozenSnapshot],
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
  /** v1 S1 (spec §2.1, `./typingGuard`): every keydown `onKeyDown` sees is fed to it first; `a`/`d`/
   *  `D` wait `TYPING_GUARD_MS` through it, and Enter on a card button asks it. One per panel, kept
   *  across renders (the dispatch below is installed once and reads this same instance). */
  const [typingGuard] = useState(() => new TypingGuard());
  /* An overlay drawn over the conversation takes the keys from the card a waiting answer was aimed
     at (spec §2.1: "an overlay opening"), whichever route opened it -- a key, an envelope, a click. */
  useEffect(() => {
    if (keymapOpen || detail !== null || handoffOpen || pathPick !== null || linkPick !== null || confirm !== null || overlayOpen) {
      typingGuard.cancel();
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
  /* v1 picks, Task 8, fix round 1 (Codex): a `gx` pick owns every key ahead of the key table, so whatever
     else takes the keys must end it, whichever route it came by -- a click into the composer (its
     `onFocus` makes INPUT), the `?` overlay from `prefix ?`, the chooser, a `/` or `:` line, the rename
     box -- or the next letter typed there would open a link and never reach what has the keys. Every
     `pathPick`-style overlay above is read here for the same reason the typing guard reads them. The
     handler's own `isEditableElement` check (below, in `onKeyDown`) covers a text field that takes the keys
     with no state of ours to say so (a card's reason box). */
  useEffect(() => {
    if (linkPick === null) return;
    if (mode !== "browse" || keymapOpen || detail !== null || handoffOpen || pathPick !== null || confirm !== null || overlayOpen) {
      setLinkPick(null);
    }
  }, [linkPick, mode, keymapOpen, detail, handoffOpen, pathPick, confirm, overlayOpen]);
  /* Whole-branch review finding 4: `slashReply` becomes the picker only with no other overlay open
     (see `slashReply`'s own doc); either way it is used up here.
     v1 trial seam review finding 1 (2026-09-28): the region (CARET/VISUAL/V-LINE) is exactly this
     kind of overlay too, and used to be missing here -- `SlashPicker` opened and focused itself
     (`slashPickerFocusRequest`), and the root's own `onFocus` (D12/D13: "focus landing on anything
     but the root ends the region") then called `exitRegion()`, silently, on a reply that has
     nothing to do with what the user was doing. `isRegionMode(modeRef.current)` joins the same
     disjunction the chooser/`?`/`/`-prompt cases already use: the reply stays plain transcript
     text, and neither the picker nor the focus effect ever runs. */
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
    // K01 fix round 2 (review): the picker is a cancel route, as the chooser is. Its keys return
    // from `onKeyDown` ahead of the read-and-reset of `pendingRef`/`countRef`, so a `g`/`z`/`[`/`]`,
    // a count or a leader sequence typed while the reply was on its way outlived it, drawn box and
    // all, and took the first key after it (`i` opened nothing; Space then `m` cycled the mode).
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
  /** Wave 3 Task 1: the one place every keyboard arrival (`pane_focus` regaining focus, `arrive`,
   *  `enter_input`, a landing on a permission card) goes to ask for the keys back, instead of each
   *  reaching for `containerRef`/a composer directly and risking one of them winning over an open
   *  overlay (the launch-chooser investigation's defects 1-2). Reads the CURRENT render's `chooser`/
   *  `renaming`/`overlayOpen`/`sessionStarted`, so it must only ever be called from an effect (which
   *  runs after commit, with this render's closure) -- never during render itself. Order: whichever
   *  overlay is drawn topmost first (chooser, then a tab rename, then the `/` prompt or the `:`
   *  line), else the live conversation, else the empty tab's own dashboard. */
  function takeKeys() {
    if (chooser !== null) {
      setChooserFocusRequest((n) => n + 1);
      return;
    }
    // Fix round (Codex review finding): the same reasoning as the chooser branch just above -- an
    // open picker is drawn over the conversation and must keep the keys through a GTK focus round
    // trip, `arrive`, or a HINT landing, exactly the way the chooser already does.
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
    // K02: the `:` command line, the same way (it is a `SearchBar` too).
    if (exLine !== null && sessionStarted) {
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
      // D11: recorded for EVERY non-modifier keydown, before `isModeCycleKey` returns early for
      // everything but Shift+Tab -- this is the guard's only source of "another key landed
      // recently", and it must see a key whether or not this effect goes on to act on it itself.
      // `lastKeyAtRef` is set to the PRIOR value on purpose (see its own doc comment): this always
      // runs before the bubble-phase `answerConfirm` that would read it for THIS SAME keydown.
      if (!isModifierKey(event.key)) {
        lastKeyAtRef.current = currentKeyAtRef.current;
        currentKeyAtRef.current = performance.now();
      }
      if (!isModeCycleKey(event)) return;
      event.preventDefault();
      // Visual-mode spec D12: Shift+Tab does nothing while CARET/VISUAL/V-LINE is on -- its y/n
      // bypass prompt would take the next `y`, meant as VISUAL's own copy. `Esc` first, same as
      // every other overlay this table would otherwise open over it.
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
      // #26 fix round (Codex review, finding 2): any key within `TYPING_GUARD_MS` of a BROWSE `y` is
      // typing -- and this capture-phase router runs ahead of `onKeyDown`'s own check of that, so
      // `y` then Shift+Tab used to post `cycle_mode` (flipping auto and bypass) right after a copy. The
      // key is claimed and recorded like any other (so a wait it cancels says so), and only a Shift+Tab
      // after a pause cycles the mode.
      const modeKeyAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
      if (modeRef.current === "browse" && typingGuard.afterCopy(modeKeyAt)) {
        event.stopPropagation();
        showFlash(typingGuard.onKey("Tab", modeKeyAt) ?? TYPE_HINT_FLASH);
        return;
      }
      // K01: Shift+Tab is a key after a waiting `g`/`z`/`[`/`]`, a count or a leader sequence, and
      // the `stopPropagation` below keeps it from `onKeyDown`, which would otherwise have dropped
      // them -- so both routes drop them here. Only refs and state setters: safe from a listener
      // installed once, with the first render's closure.
      dropPendingKeys();
      event.stopPropagation();
      // R3 (v1 audit P2-A2): this capture-phase handler runs AHEAD of the bubble-phase `onKeyDown`
      // that would otherwise feed Shift+Tab to `typingGuard.onKey` (the same mechanism
      // `answerConfirm` uses for a `confirm_bypass` prompt, this file's own `answerConfirm`) -- and
      // the `stopPropagation` just above means that bubble handler never runs at all for either route
      // reached from here, "cycle" or "fixed". Without recording it here directly, a card answer
      // `a`/`d` deferred moments earlier stayed armed and fired `TYPING_GUARD_MS` later even though
      // the user had already moved on by pressing Shift+Tab. `EmptyTab` has no permission cards (it
      // only mounts for a `not_started`/`starting` tab), so this class of bug does not reach it; its
      // own doc comment on this same key (`EmptyTab.tsx`'s `onKeyDown`) already says this handler
      // claims Shift+Tab before EmptyTab's bubble handler ever sees it.
      // Fix round 1 (v1 audit review, codex finding 2): `typingGuard.onKey("Tab", ...)`, not a bare
      // `cancel()` -- `cancel()` only drops a pending deferred `a`/`d` and never updates the guard's
      // own "last key" bookkeeping, so a typed key immediately before this route's Shift+Tab kept
      // ending the run, but Shift+Tab itself stayed invisible to it: `a` pressed moments AFTER a
      // Shift+Tab (spec §2.1's "before" half) used to answer at once instead of refusing, exactly the
      // asymmetry `answerConfirm` (below) does not have, since it already calls `onKey` for this same
      // key. `onKey`'s own return is the flash for whatever it cancelled (mirroring the bubble-phase
      // `onKeyDown`'s own `typingGuard.onKey` call), so a deferred `f`/`L` cancelled by a Shift+Tab
      // now says so instead of leaving the band silent about a swallowed `a`. The `event.timeStamp >
      // 0 ? ... : performance.now()` fallback matches every other `onKey`/`defer` call site in this
      // file (`onKeyDown`, `answerConfirm`) -- the two clocks are NOT interchangeable under a test's
      // faked timers (`event.timeStamp` tracks the faked `Date`; `performance.now()` does not), so
      // using the wrong one here would silently compare a real timestamp against a faked one and
      // always read as "long ago".
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
  /* Keys that land on <body> are handed back to the panel. That happens whenever the focused
     control stops existing or stops being focusable: a Dismiss that removes its own banner, an
     Approve that goes disabled once answered. A browser moves focus to <body> in both cases without
     reliably firing `blur` (a removed node fires nothing), and a disabled button receives no key
     events, so the next key would sail past `onKeyDown` and the panel would stop answering its own
     keys. The key is REPLAYED on the root rather than dropped, so the keystroke that noticed the
     problem still does what it says. The replay targets the root, so it never re-enters here.
     K04 (2026-09-29): on the empty layout the root that handles keys is `.empty-tab`, not
     `.agent-ui-root` -- its parent, whose only handler is the `?`/confirm capture, so a key replayed
     there never reached `EmptyTab`'s own `onKeyDown` (a failed tab's `r` was dead), and focusing it
     stranded every later key there too (`returnKeysToRoot`'s own trap, the same answer). */
  useEffect(() => {
    function onDocumentKeyDown(event: globalThis.KeyboardEvent) {
      if (event.target !== document.body && event.target !== document.documentElement) return;
      const start = startScreenRef.current;
      // K04 fix round: under an overlay (the `prefix w` chooser, a rename) the empty layout is modal, as
      // the conversation's is -- the key is not replayed onto the empty tab behind it (a failed tab's
      // `r` would reset it under the chooser); the keys go back to the overlay instead.
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
   *  into `MessageList`'s own DOM, since every row is a descendant of it. The row is the cursor's
   *  own (`conversationRows`), never the first `.row-current` in the DOM: a model reply's HTML could
   *  draw one of those above the real row (v1 hardening, ruling R2).
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
    const animate = landingAnimateRef.current;
    landingRef.current = 0;
    landingAnimateRef.current = false;
    if (landing === "keep") return;
    // D11 (fix round 3, review finding): while VISUAL is on the list on screen is the frozen one,
    // and `cursor` indexes the LIVE timeline -- a prompt sent at a turn's end moved it to a row the
    // frozen list does not have, and this used to scroll whichever frozen row sat at that index into
    // view under the selection. VISUAL moves the view only for its own caret (D7); the row cursor is
    // placed again, by key, when VISUAL ends (`landCursorOnRowKey`).
    if (frozenSnapshotRef.current !== null) return;
    const root = containerRef.current;
    const row = root === null ? undefined : conversationRows(root)[cursor];
    if (row === undefined) return;
    const list = row.closest<HTMLElement>(".message-list");
    if (list !== null) settleListScroll(list);
    if (landing !== 0) enterBoxAtNearEnd(row, landing);
    revealRow(list, row, landing, animate);
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
      // D11 (fix round 3, review finding): the rows below are the FROZEN list's while VISUAL is on,
      // and the index this writes back would name a live row by a frozen position -- the two differ
      // once a row arrives. VISUAL's own caret scrolls the list (D7); the row cursor is placed again,
      // by key, when VISUAL ends.
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
    applyViewScroll(list, view, view.atBottom);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [restoreTick]);
  /** The session is gone (lost or closed). Read before the start-screen branch below, because the
   *  effect under it is a hook and cannot live after a conditional return. */
  const sessionEnded = state.status.kind === "unavailable" || state.status.kind === "closed";
  /** Owner decision #39: the card INPUT's `Ctrl+y` would approve right now -- the active tab's oldest
   *  waiting card (`approveOldestByKey` picks the same one) -- for the band to name, only while the
   *  composer has the keys (INPUT, this pane focused) on a live session. `null` otherwise. */
  const ctrlYCard: ApproveFact | null = (() => {
    // Fix round 1 (Opus I-3, Codex): only while the composer's own box holds the keys -- the same
    // condition `approveOldestByKey` answers under, so the band never names a Ctrl+y that does
    // nothing (the `Ctrl+r` search field).
    if (mode !== "input" || !paneFocused || sessionEnded || !composerBoxFocused) return null;
    const index = oldestWaitingPermission(timeline, answeredPermissions);
    const item = index === null ? undefined : timeline[index];
    return item?.kind === "permission" ? { tool: item.request.toolName, summary: cardSummary(item.request.input) } : null;
  })();
  /** #39 fix round 1 (Opus B-1): which card INPUT's `Ctrl+y` would approve (its tab and id), and
   *  whether it has been that card for `TYPING_GUARD_MS` yet. Without this a `Ctrl+y` approved
   *  whatever was oldest at keydown however recently it had become the target: a second `Ctrl+y`
   *  10 ms after the band moved on to the next card, a card that replaced a withdrawn one, one that
   *  arrived 30 ms before a `Ctrl+y` meant as something else. Every change of target -- an arrival,
   *  a withdrawal, a tab switch, this panel's own approval moving it on -- starts the wait again. A
   *  timer, as `TypingGuard.defer` uses; no new clock. */
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
    // Fix round 1 (reviewer finding, blocking, both review programs): this used to write `mode`
    // directly, bypassing `exitRegion` -- a session dying during VISUAL left `frozenSnapshot` set
    // (and the DOM selection VISUAL built still live), and neither is what a dead session should be
    // showing. `exitRegion` is a no-op when `mode` was never visual/vline, so calling it here
    // unconditionally changes nothing for every other case this effect already handled.
    if (sessionEnded) {
      exitRegion();
      setMode("browse");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
     one -- never a composer the user never asked to type into. Keyed on the request alone, like
     `inputRequest`.
     #22 (owner decision, 2026-09-29: "contrl h之后再contrl l，会自动跳到最底下，能不能类似记住光标位置")
     reverses the landing half for everything else: BROWSE, and the reader's own place. A reader who was
     following the bottom lands on the last row and keeps following (the same way a send does,
     `./follow.ts`); anyone else gets the row they left (by key, so a row removed above it while away
     does not shift the landing) and the scroll they left, through the park the `pane_focus` arm took.
     With no usable park (a launch arrival, a park for another tab, one a key or a hand on the list made
     stale), the live view decides the same way: following lands last, anything else moves nothing. */
  useEffect(() => {
    if (arriveRequest === 0) return;
    const park = arrivalParkRef.current;
    arrivalParkRef.current = null;
    // A key handled since the envelope arrived already acted on the panel as drawn (`landingAtKeyRef`):
    // the landing would undo it, so the key wins and nothing below runs.
    const atKey = landingAtKeyRef.current;
    landingAtKeyRef.current = null;
    if (atKey !== null && atKey !== typingGuard.keyCount()) return;
    // Fix round 1 (reviewer finding, blocking, both review programs): an arrival (a tab switch,
    // `Ctrl+h`/`Ctrl+l`) used to write `mode`/`cursor` directly below without ever calling
    // `exitRegion`, so `frozenSnapshot` -- and the row VISUAL was looking at in the OLD tab -- stayed
    // set while a different tab's live conversation took over underneath it. A no-op when VISUAL was
    // not on.
    exitRegion();
    // Wave 3 Task 1: an overlay open over the conversation keeps the keys and the conversation's
    // cursor does not move (Review Focus 1, test d) -- landing BROWSE on the last row would fight
    // it for both.
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
    // The row is authoritative: if it is entirely off screen at that scroll (the width changed while
    // the panel was hidden), bring it into view rather than leaving R1's clamp to move the cursor.
    const rows = conversationRows(list);
    const row = rows[index];
    if (row !== undefined && clampCursorToView(list, rows, index) !== null) revealRow(list, row, 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [arriveRequest]);
  /* `focus_permission` (modules spec §3.3): BROWSE, with the cursor on the oldest pending card, so
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
    // Fix round 1 (reviewer finding, blocking, both review programs): same reasoning as `arrive`'s
    // own `exitRegion` call just above -- this lands the cursor on a specific card below, directly,
    // and a frozen VISUAL snapshot from a moment ago must not keep showing something else while that
    // happens.
    exitRegion();
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
      // D11 (fix round 3): `exitRegion` above has only asked for the thawed list. If VISUAL was on,
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
   *  (D11, fix round 3): run once the thawed, live list has rendered, and only then indexed by the
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

  /** D11's own backstop, fix round 1 (the blocking finding of both review programs): "the effect that
   *  clears the selection whenever mode leaves visual/vline" -- described in the spec, never actually
   *  written. Every known route out of VISUAL now calls `exitRegion` explicitly (the routes just above
   *  this file's `sessionEnded`/`arrive`/`focus_permission` effects, and the dispatch arms further
   *  down for `pane_focus`/`hint_collect`/the rename/`confirm_*`/`chooser`); this is what still cleans
   *  up `frozenSnapshot` and the built selection if some OTHER path -- today's or a later change's --
   *  ever moves `mode` away without going through one of them. Idempotent and cheap: `clearVisualLeftovers`
   *  no-ops once everything is already `null`, which is true on every render this effect's own routes
   *  did not just handle, including the very first one. */
  useEffect(() => {
    if (!isRegionMode(mode)) clearVisualLeftovers();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);
  /** Fix round 2's backstop for the other axis (reviewer finding, important): VISUAL selects inside
   *  a live conversation, so the moment no conversation is shown any more (`sessionStarted` false:
   *  the tab went back to `not_started`/`starting`, or failed with nothing kept) it cannot still be
   *  on. The `error`/`handoff` arms call `exitRegion` themselves; this catches any other route that
   *  drops the conversation without touching `mode` -- which is exactly the shape that route had,
   *  and why the `[mode]` backstop above never saw it. A no-op outside VISUAL. */
  useEffect(() => {
    if (!sessionStarted) exitRegion();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionStarted]);

  /** D11's defence in depth (fix round 1, minor: spec-required and previously not built at all --
   *  W9 was written to assert "the observer silent", with no observer to be silent). `MessageList`
   *  reading `frozenSnapshot` (D11's PRIMARY mechanism, above) already keeps its own subtree still
   *  while VISUAL is on; this watches `.message-list` anyway, for whatever that primary mechanism
   *  does not cover -- a bug in the freeze itself, or some other code path writing into the DOM
   *  VISUAL is holding a selection in. A `childList`/`characterData` change touching either end of
   *  the selection VISUAL built ends it with D8's own flash, the same as a live check at `y`-time
   *  finding the selection no longer matches. Deliberately narrow (only the two ENDS, not "anything
   *  in the subtree changed"): a delta landing on some other, unrelated row must not interrupt a
   *  read of a different one, and D11 says the mode does not end "on a card arriving or a reply
   *  streaming" by itself. */
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
          // Fix round 2 (review finding, minor): names the mode that ended, as `vend` and the
          // Shift+Tab refusal do -- this said VISUAL while CARET was on.
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
  /** A `hint_collect` sessionId waiting for the thawed list, the same deferral
   *  `revealAfterThawRef`/`focus_permission` uses (D11, fix round 4): set only when `hint_collect`
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
   *  to thaw the list (D11, fix round 4, mirroring `focus_permission`'s own reveal effect above) -- it
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
   *  whole message (spec §2.4). Cleared by that copy and by any other key the table resolves. */
  const copyCodeRef = useRef<HTMLElement | null>(null);
  /** K07: whether `copyCodeRef` holds a block, for `MessageList`'s `data-code-landed` (the row's sign
   *  goes hollow while the block is the item). */
  const [codeLanded, setCodeLanded] = useState(false);
  /** K07: the one way `copyCodeRef` is written. Keeps the ref (the dispatch handler and `enterRegion`
   *  read it synchronously) and draws the landing: `data-hint-landed` on the block itself -- it lives in
   *  markdown `innerHTML`, so React cannot render the attribute -- and `codeLanded` for the row. A block
   *  re-rendered by a streamed delta is a new `pre`: the old one leaves with its attribute, and the ref
   *  already lands nowhere useful then (`isConnected`), as before. */
  function markLandedCode(el: HTMLElement | null) {
    const previous = copyCodeRef.current;
    if (previous === el) return;
    previous?.removeAttribute("data-hint-landed");
    copyCodeRef.current = el;
    el?.setAttribute("data-hint-landed", "");
    setCodeLanded(el !== null);
  }
  // K07: a streamed delta that re-rendered the landed block took its outline with it; the row's sign
  // goes solid again rather than staying hollow for a mark nobody can see. The ref itself is left as it
  // was (what `y` does with a detached block is unchanged). Fix round (review): and the landing ends
  // whenever the cursor is moved off the block's row by anything but a key (which clears it itself) --
  // an arrival landing on a waiting card, say -- or the block would keep its outline and the card its
  // hollow sign. An arrival that stays on the row (a park restored, or following on the last row) keeps
  // it, so `y` still copies the block after `Ctrl+h Ctrl+l`.
  useEffect(() => {
    const block = copyCodeRef.current;
    if (block === null || frozenSnapshotRef.current !== null) return;
    if (!block.isConnected) {
      if (codeLanded) setCodeLanded(false);
      return;
    }
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
  /** K02 (ruling R3): where focus was actually put by a key -- an `h`/`l` that moved it, a plain Tab
   *  -- or by a HINT landing, and the `typingGuard.keyCount()` it happened at (a key's own count; a
   *  HINT's labels never reach this page, so the count at the landing). Enter on a card's own button
   *  presses it only when this names that very button at the key right before Enter. Written where
   *  focus moves (`onKeyDown`'s `control` branch, the root's `onFocus`, `landOnHint`), never inferred
   *  from a key's name, and never cleared: any later key makes it stale by its count alone. A ref,
   *  not `landedControlRef` (which every commit clears): `landOnHint` runs from the dispatch
   *  installed once. */
  const placedRef = useRef<{ el: HTMLElement; atKey: number } | null>(null);
  /** K02: the plain Tab `onKeyDown` saw last -- its `keyCount()` and its native event -- until the
   *  focus move it makes, else `null`. The root's `onFocus` records that move as a landing, and
   *  consumes this either way. Fix round 3 (review): only if the Tab's default is still alive when the
   *  focus arrives, read off the event then rather than at the top of `onKeyDown` -- by then every
   *  handler has run, so a Tab one of them swallowed (the `/` or `:` line's, K01's cancel after a
   *  waiting prefix, a key the leader's sequence does not bind, an overlay's) moved nothing, and the
   *  next focus no key made is not its landing. */
  const tabAtKeyRef = useRef<{ atKey: number; event: globalThis.KeyboardEvent } | null>(null);

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
      } else if (payload.kind === "editor_typing") {
        // Window-level and Rust's alone (a cadence slower than the meter's step): it slows the
        // meter, never pauses it. Deliberately not among the envelopes that cancel a waiting answer
        // or touch any other state -- it is not about the panel's own keys.
        applyEditorTyping(document.documentElement, payload.typing, payload.periodMs);
      } else if (payload.kind === "pane_focus") {
        // A pending prefix is for the very next key; a pane switch in between (GTK takes `Ctrl+h`/
        // `Ctrl+k` before the WebView sees a keydown) must not leave it armed for a key pressed much
        // later, when it would complete a chord nobody meant to start -- review. The leader engine's
        // own pending sequence, and the box waiting to show it (spec §2.4, Review Focus 1), are
        // cancelled the same way and for the same reason: a switch away and back must not leave a
        // table sequence -- or its box timer -- running for a press that has nothing to do with
        // whatever started it. K01: and a count, which used to survive this and multiply a `j`
        // pressed after coming back.
        dropPendingKeys();
        // A keymap for THIS panel has no reason to stay drawn while another pane has the keys, and
        // leaving it up is how the review reproduced a dead keyboard: come back with `Ctrl+l`,
        // land in INPUT, and every keystroke is swallowed by the overlay's own branch (review).
        setKeymapOpen(false);
        setReview(null); // the review overlay is over the same area, and ends where `?` does
        setDetail(null);
        setHandoffOpen(false);
        // R4: the `/` prompt is this panel's own, the same reason the `?` overlay closes here -- and
        // so is K02's `:` line.
        setSearch(null);
        setExLine(null);
        // K02: a Tab whose focus move never landed inside this page (it left for another widget)
        // must not make whatever this page focuses on the way back a landing.
        tabAtKeyRef.current = null;
        setPaneFocused(payload.focused);
        // v1, D11/spec §3.4's cancel list: losing focus is a route away from whatever this panel was
        // showing, bypass prompt included -- REGAINING it is not (this pane's own keydowns are the
        // only thing that answers the prompt, and those never fire while it is unfocused anyway).
        if (!payload.focused) cancelBypassConfirm();
        // Visual-mode spec D12: `pane_focus` false ends VISUAL -- this arm never otherwise sets
        // `mode`, so nothing else here would (spec §7, finding 4).
        if (!payload.focused) exitRegion();
        // #22: the keys leave -- park where the reader is, after `exitRegion` (as the `tabs` arm's
        // capture), so a VISUAL mode is never what is parked. Only over a conversation: the empty tab
        // lands its own way (`EmptyTab`'s `arriveRequest` effect).
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
        setReview(null); // the review overlay is over the same area, and ends where `?` does
        setDetail(null);
        setHandoffOpen(false);
        endKeyPrompts(); // v1, D11/spec §3.4's cancel list
        arrivalParkRef.current = null; // #22: this lands INPUT, never on a parked row
        setEmptyLanding("input");
        setInputRequest((n) => n + 1);
      } else if (payload.kind === "arrive") {
        // Panel round 2 (spec §8, decision 4): every keyboard arrival that used to send `enter_input`
        // -- and so land in INPUT -- now sends this instead, and lands BROWSE. The overlay would sit
        // over whatever this lands on, the same reason `enter_input` closes it above.
        setKeymapOpen(false);
        setReview(null); // the review overlay is over the same area, and ends where `?` does
        setDetail(null);
        setHandoffOpen(false);
        endKeyPrompts(); // v1, D11/spec §3.4's cancel list
        // A reserved two-key prefix or a leader/table sequence armed from before this arrival means
        // nothing about it -- the same reason `pane_focus` cancels both (spec §2.4, Review Focus 1).
        dropPendingKeys();
        setEmptyLanding("browse");
        landingAtKeyRef.current = typingGuard.keyCount();
        setArriveRequest((n) => n + 1);
      } else if (payload.kind === "focus_permission") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        dropPendingKeys();
        // The overlay would cover the card the cursor is about to land on.
        setKeymapOpen(false);
        setReview(null); // the review overlay is over the same area, and ends where `?` does
        setDetail(null);
        setHandoffOpen(false);
        setChooser(null);
        endKeyPrompts(); // v1, D11/spec §3.4's cancel list
        arrivalParkRef.current = null; // #22: a card landing wins over a parked row
        landingAtKeyRef.current = typingGuard.keyCount();
        setPermissionRequest((n) => n + 1);
      } else if (payload.kind === "nav_key") {
        // A key, claimed by GTK before the WebView saw it, so it never reached `answerConfirm`: D1's
        // "any key but a counted y cancels" has to be applied here instead (v1-mode fix round 1).
        endKeyPrompts();
        // GTK takes `Ctrl+j`/`Ctrl+k` before the WebView sees a keydown, so this is a key pressed
        // after any pending `g`/`z`/`[`/`]` prefix or leader sequence -- cancelled here for the same
        // reason `pane_focus` and `arrive` cancel them, whether the effect below claims the chord or
        // answers it with `nav_fallthrough`. Left armed, `g`, Ctrl+j, Ctrl+k, `g` ran a stale `gg`
        // (whole-branch review of v1-ui).
        dropPendingKeys();
        // V1 C1 (spec §3.5): the raw handler only records the request -- see `navKey`'s own doc
        // comment for why the decision has to live in an effect instead.
        navKeySeqRef.current += 1;
        setNavKey({ seq: navKeySeqRef.current, direction: payload.direction });
      } else if (payload.kind === "keymap") {
        // A table that changed while a sequence was pending must never let the OLD table's binding
        // run against the new one (Review Focus 1) -- cancelled before the new table is even
        // stored, so nothing between these two statements could read a mismatched pair.
        clearSequence();
        // v1 audit P2-A6: `clearSequence` only drops a pending MULTI-key sequence (`seqRef`) -- a
        // single-key table binding deferred by `typingGuard.defer` (H/L's default tab.prev/tab.next,
        // or an nvim-read binding on some other plain letter, R2-2) closes over the OLD table's
        // `binding` at keydown time, and stayed armed to run it `TYPING_GUARD_MS` later against
        // whatever the table now says even after being replaced. Cancelled here too, before the new
        // table is stored, for the same reason as the line above.
        typingGuard.cancel();
        setKeymapHelp({
          prefix: payload.prefix,
          window: payload.window,
          prefixKeys: payload.prefixKeys,
          panel: payload.panel,
          newTabChord: payload.newTabChord,
          tmuxSkipped: payload.tmuxSkipped ?? [],
        });
      } else if (payload.kind === "literal_key") {
        // `send-prefix`/`send-keys` from shell: WebKitGTK cannot be handed the key itself. `C-a` is
        // what a text field does with it -- select all of the focused one; any other key is ignored.
        // Deliberately NOT `isEditableElement`: this needs "has a text selection", which is exactly
        // these two types.
        // Like `nav_key` above: a key GTK claimed, so D1's cancel is applied here (v1-mode fix round 1).
        endKeyPrompts();
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
        // `prefix ?` is a route away from a bypass prompt (spec §3.4, v1-mode fix round 1): GTK takes
        // the chord, so no key of it reaches `answerConfirm`.
        endKeyPrompts();
        // Visual-mode spec D12/D10: `?` is one of VISUAL's own routes away too (its OWN `?` key
        // reaches this through `onVisualKeyDownCapture`, not here -- but `prefix ?` is GTK's, and
        // reaches this panel only as this envelope).
        exitRegion();
        setMode("browse");
        setKeymapOpen(true);
        setReview(null); // the review overlay is over the same area, and ends where `?` does
      } else if (payload.kind === "open_command_line") {
        // `prefix :` (tmux `command-prompt`, owner decision #28, K16). GTK took the chord, and before it
        // was bound the armed prefix swallowed the `:` and the letters after it ran as panel keys ("kill-window
        // -a" moved the cursor, entered INPUT and sent "ll-window -a"). This opens the same line BROWSE's own
        // `:` opens (K02, R4): it runs nothing, Enter says so, Esc closes it, and it has the keys.
        // The empty tab has no such line (its `:` is not a key either), so there it says so instead.
        if (!sessionStartedRef.current) {
          showFlash("no command line on this screen — ? lists this panel's keys");
          return;
        }
        // A route away from a bypass prompt, from VISUAL, and from every overlay over the conversation,
        // as `hint_collect` and `tab_detail` are: GTK's chord reaches no key handler here. The `/` prompt
        // and this line are one command line, so opening one closes the other.
        // rc.4 review (Codex): EVERY pending y/n and a `gf` picker, not only a bypass prompt: see `endKeyPrompts`.
        // A key typed into this line is the line's; the prompt it covers is over, as it is for BROWSE's own `:`
        // (whose key ends it).
        endKeyPrompts();
        exitRegion();
        setKeymapOpen(false);
        setReview(null); // the review overlay is over the same area, and ends where `?` does
        setDetail(null);
        setHandoffOpen(false);
        setChooser(null);
        setSlashPicker(null);
        setSlashReply(null);
        pendingSlashPickerRef.current = null;
        dropPendingKeys();
        setMode("browse");
        setSearch(null);
        setExLine("");
        setLineFocusRequest((n) => n + 1);
      } else if (payload.kind === "hint_collect") {
        // A HINT started elsewhere in the window must not label rows hidden under this overlay
        // (spec §3.1). It also frees the keys `hint_collect`'s own reply is about to swallow --
        // this and HINT never actually contend for them, but closing here keeps that true by
        // construction rather than by the two features happening not to overlap in practice.
        // Visual-mode spec D12: a HINT ends VISUAL -- this arm never otherwise sets `mode` (spec
        // §7, finding 4).
        exitRegion();
        setKeymapOpen(false);
        setReview(null); // the review overlay is over the same area, and ends where `?` does
        setDetail(null);
        setHandoffOpen(false);
        setChooser(null);
        // v1 trial fix round 2 (Codex): the `/model`/`/effort` picker too, open or on its way -- the
        // same route away as the chooser (finding 4). A reply landing under the labels used to open
        // it there; landing on the composer then left the keys on the picker.
        setSlashPicker(null);
        setSlashReply(null);
        pendingSlashPickerRef.current = null;
        // The whole-branch review (blocking): `prefix f`'s labels and the label keys are GTK's, never
        // a keydown here, so the next ordinary key after a landing -- the `y` that copies the code
        // block HINT just landed on, the first letter typed into the composer -- answered a bypass
        // prompt left open under it. HINT is a route away (spec §3.4); Rust drops its own prompt too.
        endKeyPrompts();
        // R4: the labels would sit over the search prompt, and HINT and `/` never contend for keys
        // (nor HINT and K02's `:` line).
        setSearch(null);
        setExLine(null);
        // ...and the prefix the strip may still be waiting on, for the reason `pane_focus` does it.
        dropPendingKeys();
        // D11 (fix round 4): `exitRegion` above has only asked for the thawed list to render -- it does
        // not repaint synchronously, so the DOM here can still be the frozen VISUAL one. Collecting
        // targets from it misses any row that arrived during VISUAL (a linked permission card, a queued
        // prompt), the same gap `focus_permission`'s own reveal had (D11, fix round 3): a probe posted 3
        // targets with VISUAL still framing the collect versus 7 once thawed. Defer to `collectHintTargets`'s
        // own layout effect (above, by `pendingHintCollectRef`) when that is the case; collect at once
        // otherwise, unchanged from before this round.
        if (frozenSnapshotRef.current !== null) pendingHintCollectRef.current = payload.sessionId;
        else collectHintTargets(payload.sessionId);
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
          // Visual-mode spec D12: a tab switch ends VISUAL, and does it FIRST -- ahead of
          // `saveView` a few lines down, which reads `modeRef.current` to park the OLD tab's mode;
          // `exitRegion` sets that ref synchronously so "visual"/"vline" is never what gets saved
          // (this panel never restores one, so it would otherwise just be a dead value, but a dead
          // value naming a mode this file cannot re-enter is exactly the kind of thing worth not
          // writing down).
          exitRegion();
          // v1 S1 (spec §2.1): a waiting `a`/`d`/`D` was aimed at the old tab's card.
          typingGuard.cancel();
          // A sequence pending in the OLD tab's conversation means nothing about the new one (spec
          // §2.4's cancel list).
          dropPendingKeys();
          // Ruling 6: the tab stops being active, so whatever has not yet been mirrored goes now
          // rather than waiting out the rest of its 300ms debounce against a tab nobody is reading.
          flushDraft();
          const previous = activeTabRef.current;
          // A `j`/`k` still easing the view is finished first: the old tab's saved view is where that press
          // was going, and the ease must not go on carrying the new tab toward the old one's target.
          const leftList = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
          if (leftList !== null) settleListScroll(leftList);
          // The old tab's view is saved from the live refs -- BEFORE the resets below overwrite
          // the render state they mirror -- and only when there IS an old tab (not on mount).
          if (previous !== null) {
            saveView(viewStore.current, previous, captureView(leftList));
          }
          // #22: an arrival park names a row of the tab just left.
          arrivalParkRef.current = null;
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
          // Fix round (Codex review findings): an open `/model`/`/effort` picker was drawn over the
          // OLD tab's conversation and answers through `chooseSlashOption`, which sends to whatever
          // tab is CURRENTLY active -- left open across a switch, choosing a row from it would send
          // the command to the NEW (wrong) tab. Closed the same way the chooser just above is.
          // `pendingSlashPickerRef` is a single flag, not scoped to a tab: `events`/`turn_completed`
          // are tab-scoped (`acceptsEnvelope`), so a switch away from the tab that armed it drops
          // that tab's own reply on the floor and leaves the ref waiting to be checked against
          // whatever `turn_completed` this (now different) tab sees next. Clearing it here closes
          // that stale cross-tab match at its source, the same moment the picker itself closes.
          setSlashPicker(null);
          setSlashReply(null);
          pendingSlashPickerRef.current = null;
          // v1 audit P2-A5: a `gf` picker waiting over the OLD tab's conversation names paths that
          // mean nothing over the new one -- left set, its very next letter opened one of them
          // regardless of which tab now has the keys (the keydown handler's `pathPick !== null`
          // branch runs ahead of everything else and does not itself check the active tab).
          setPathPick(null);
          // v1 picks, Task 8 (R6): and a `gx` pick, which names links of the OLD tab's reply.
          setLinkPick(null);
          cancelBypassConfirm(); // v1, D11/spec §3.4's cancel list: a DIFFERENT active tab only
          // R4: a `/` prompt was over the OLD tab's conversation and means nothing over the new one;
          // nor does K02's `:` line.
          setSearch(null);
          setExLine(null);
          setTurnClock(null);
          setHandoff(null);
          setFatalError(null);
          setCommandNotice(null);
          setQueue([]);
          setQueueError(null);
          setRuleOffers({});
          answeredRef.current = new Set();
          setAnsweredPermissions(answeredRef.current);
          permissionReasons.current.clear();
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
        // v1 polish item 6: a kept conversation lives only while its tab is still failed, and comes
        // back on a switch to it (after the reset above; no snapshot follows for a failed tab).
        for (const id of [...keptStates.current.keys()]) {
          if (payload.tabs.find((t) => t.id === id)?.state !== "failed") keptStates.current.delete(id);
        }
        if (payload.active !== activeTabRef.current) {
          const kept = keptStates.current.get(payload.active);
          if (kept !== undefined) setState(kept);
        }
        activeTabRef.current = payload.active;
        // The banner belongs to the session that ended. Once the active tab is starting a new one
        // (`r`, a first send after a failure) or running it, that session's reason is not about this
        // tab any more; a start that fails again sends its own `error`. `awaiting_trust` is left out on
        // purpose: nothing has started under it (the user may still put the start off), and a tab that
        // waits draws no ended-session notice of its own.
        const activeState = payload.tabs.find((t) => t.id === payload.active)?.state;
        if (activeState === "starting" || activeState === "live") setFatalError(null);
        setTabs({ active: payload.active, tabs: payload.tabs, defaultMode: payload.defaultMode });
        forgetClosed(
          viewStore.current,
          payload.tabs.map((t) => t.id),
        );
      } else if (payload.kind === "tab_detail") {
        // The reply to `open_detail` (`StatusRow`, `prefix i`): opens the popover on this tab's
        // rows, cursor at the top. Forces BROWSE and closes the `?` overlay the same way
        // `open_keymap` does -- the two overlays are mutually exclusive over the conversation area.
        // A route away from a bypass prompt (v1-mode fix round 1): the popover's own `y` copies a row,
        // and its handoff row is reached by clicks that never pass through `answerConfirm`.
        endKeyPrompts();
        // Visual-mode spec D12: the detail popover is one of the overlays that ends VISUAL.
        exitRegion();
        setDetail(payload.rows);
        setDetailCursor(0);
        setMode("browse");
        setKeymapOpen(false);
        setReview(null); // the review overlay is over the same area, and ends where `?` does
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
        // Likewise the ended-session banner: a snapshot is a session that runs, so a reason left from
        // an earlier session of this tab (a failed start that `r` then redid) is stale.
        setFatalError(null);
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
          // Owner trial item 2 (2026-09-28): the turn immediately following a bare /model or
          // /effort send is the one candidate for a picker -- armed by `sendMessage`, read and
          // cleared here on the very next `turn_completed`, parsed either way, so a follow-up turn
          // is never mistaken for the reply. `result_text` (not accumulated `content_delta` text)
          // is what both backends fill with a local command's reply -- see `../slashPicker`'s own
          // doc comment and `docs/canonical/2026-09-27-slash-commands.md`'s "Overlap with spec
          // §9.2" note for why this is the one field common to both, since a local command's reply
          // reaches this panel as an ordinary content_delta indistinguishable from real assistant
          // text on the sidecar backend.
          if (event.type === "turn_completed") {
            const pending = pendingSlashPickerRef.current;
            pendingSlashPickerRef.current = null;
            // Whole-branch review finding 4: through `slashReply`, whose effect opens the picker
            // only if no other overlay is open by then.
            if (pending === "model") {
              const parsed = parseModelReply(event.result_text);
              if (parsed !== null) setSlashReply({ kind: "model", options: parsed.options, current: parsed.current });
            } else if (pending === "effort") {
              const parsed = parseEffortReply(event.result_text);
              if (parsed !== null) setSlashReply({ kind: "effort", options: parsed.options, current: null });
            }
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
        setState((s) => applyCallNotes(payload.events.reduce((acc, event) => applyEvent(acc, event), s), payload));
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
        if (payload.ok && payload.message !== undefined && (record?.kind === "review-command" || record?.review === true)) {
          // The shell's own word on what it did (`sent`, `queued: ...`, `opened; 2 hunks no longer match`).
          const message = payload.message;
          setReview((current) => (current === null ? current : withStatus(current, message)));
        }
        if (!payload.ok) {
          console.warn("agent-ui: command failed", payload.requestId, payload.error);
          if (record?.kind === "trust") {
            // Already said, if it needed saying (see `inFlight`).
          } else if (record?.kind === "review-command" || record?.review === true) {
            // The overlay's own command was refused (a revert that no longer matches the disk, a running
            // turn, no editor link): said on its status line, where the key was pressed. Not the
            // conversation's banner, and not a flash that would fade before it was read.
            const error = payload.error;
            setReview((current) => (current === null ? current : withStatus(current, error)));
          } else if (record?.kind === "picker-send") {
            // v1 trial seam review finding 2: the picker's own choice was never in the composer box
            // -- `chooseSlashOption` calls `sendMessage` directly, never through `Composer`'s own
            // optimistic clear -- so there is nothing to "put back" and nowhere to put it. Unlike
            // the ordinary `"send"` case just below, this never touches the draft: a footer flash
            // alone, so a quoted or typed draft already sitting in the box (D10's own `>`, or plain
            // typing) is left exactly as the user set it.
            pendingSlashPickerRef.current = null;
            showFlash(payload.error);
          } else if (record?.kind === "send") {
            // Owner trial item 2: a refused send never started a turn, so whatever it armed
            // (`sendMessage`'s own `pendingSlashPickerRef` write) must not sit around waiting to be
            // matched against some later, unrelated turn's reply. Unconditional, regardless of
            // which tab the refusal names: the ref is a single flag, not tab-scoped (the `events`
            // handler that reads it already only ever sees the active tab's events, via
            // `acceptsEnvelope`).
            pendingSlashPickerRef.current = null;
            if (record.tab === activeTabRef.current) {
              // The composer cleared this optimistically. Rust refused it, so it goes back — a
              // message that vanishes with no trace is the outcome this exists to prevent.
              // R1 (P2-A3, v1 audit): `draftRef.current` is mirrored on every keystroke (including
              // the optimistic clear itself), so it names exactly what the box holds right now. If
              // nothing was typed since that clear it is still "", and the refused text is restored
              // alone; if the user typed since, what they typed is kept and the refused text goes
              // BEFORE it, separated by a newline, so neither is lost. Mirrored to Rust here, once,
              // through the same `mirrorDraft` ordinary typing uses (not inside `Composer`'s generic
              // restore effect, which also fires on a plain tab-switch clear and on Rust's own
              // `draft` echo -- mirroring unconditionally there re-echoed both of those back out as
              // a spurious empty `draft` post, caught by an unrelated leader-sequence test that
              // types nothing at all), so a later tab switch brings the recovered text back too.
              const typedSince = draftRef.current;
              const recovered = typedSince === "" ? (record.text ?? "") : `${record.text ?? ""}\n${typedSince}`;
              restoreSeq.current += 1;
              setRestoredDraft({ text: recovered, seq: restoreSeq.current });
              mirrorDraft(recovered);
              setCommandNotice(`That message was not sent (${payload.error}). It is back in the box.`);
            } else {
              // The tab this was sent from is no longer the active one (the user switched away
              // before Rust answered) -- restoring it into a DIFFERENT tab's composer would be
              // exactly the tab-scoping bug this feature exists to rule out (ruling 3), so the
              // refusal is reported by name instead, with the full text so it is not lost.
              setCommandNotice(`A message to tab ${record.tab} was not sent (${payload.error}): ${record.text}`);
            }
          } else if (record?.kind === "review") {
            // The overlay's own request was refused (no session, no such turn, git unavailable): it says
            // so in its header. Never the conversation's banner, which is for what breaks the conversation.
            setReview((current) => (current === null ? current : failRequest(current, payload.requestId, payload.error)));
          } else if (record?.kind === "editor" || record?.kind === "link") {
            // A scratch-editor round trip's own refusal (Task 8/15), or `gx`'s address Rust would not
            // open (v1 picks, Task 8), is a footer nicety, not something that should fill the banner
            // reserved for conversation-breaking errors.
            showFlash(payload.error);
          } else {
            if (record?.kind === "permission" && record.permissionId !== undefined && record.tab === activeTabRef.current) {
              // Refused, so not answered: Rust leaves the card waiting ("the card stays and `a` still
              // works", ruling 16), and so must this panel -- `a`/`d` and the card's own buttons were
              // dead until a reset (Codex's whole-branch review, reproduced against these handlers).
              const permissionId = record.permissionId;
              if (answeredRef.current.has(permissionId)) {
                const next = new Set(answeredRef.current);
                next.delete(permissionId);
                answeredRef.current = next;
                setAnsweredPermissions(next);
              }
            }
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
        // Visual-mode fix round 2 (reviewer finding, important): this whole-state reset neither ends
        // the session (`status` goes back to `starting`, so the `[sessionEnded]` effect never runs)
        // nor changes `mode`, so VISUAL -- and `frozenSnapshot`, the conversation it froze -- outlived
        // the conversation it was selecting in: the empty tab's Shift+Tab was still refused as
        // "Esc first", with no VISUAL handler left to take that Esc.
        exitRegion();
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
        // v1 polish item 6: a sidecar that stopped under a conversation (a resumed one, before any
        // new turn, reaches here as "ended before it started") keeps that conversation on screen as
        // a lost session with the provider's own reason, rather than an empty failed tab guessing
        // the session "no longer exists". Anything else, or nothing said yet: the ordinary reset.
        const kept =
          payload.tab === activeTabRef.current && sidecarStopped(payload.message)
            ? keepAfterSidecarStop(stateRef.current, failureEvidence(payload.message))
            : null;
        if (kept !== null) keptStates.current.set(payload.tab, kept);
        // Visual-mode fix round 2 (reviewer finding, important): the same reason as the `handoff`
        // arm's own call -- a reset to `initialState()` is not an ended session, so nothing else here
        // ended VISUAL, and the restarted tab's next snapshot was drawn UNDER the old, frozen
        // conversation (the dead session's cards included) until an `Esc`.
        exitRegion();
        setState(kept ?? initialState());
        setKeymapOpen(false);
        setHandoffOpen(false);
        // The third of the three whole-state resets, now saying the same thing as the other two.
        setTurnClock(null);
        // A kept conversation says it in its own lost-session row; the banner would say it twice.
        setFatalError(kept === null ? payload.message : null);
        // No `requestHello()` here (ruling 17): Rust re-sends `hello`, recomputed, whenever the set
        // of open provider sessions changes -- this tab failing (or ending, or resetting) is exactly
        // such a change, so the picker's list is already on its way rather than something this side
        // has to go ask for.
      } else if (payload.kind === "begin_rename") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        dropPendingKeys();
        // Visual-mode spec D12/§7 finding 4: a tab rename ends VISUAL -- this arm never otherwise
        // sets `mode`.
        exitRegion();
        // `prefix ,` (spec §3.5): the tab bar stays on screen (ruling 6) with an inline field open
        // over this tab, prefilled and selected. The two other overlays over the conversation area
        // must not fight it for the keys.
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        // The whole-branch review (blocking): `prefix ,` is GTK's, so a name starting with `y` typed
        // into the field answered a bypass prompt left open (spec §3.4). Rust drops its own too.
        endKeyPrompts();
        // K02 fix round 1: the `/` prompt and the `:` line too, for the chooser's reason (its own
        // arm): committing or cancelling the rename hands the keys to the conversation root.
        setSearch(null);
        setExLine(null);
        setRenaming({ tab: payload.tab, initial: payload.current ?? "" });
      } else if (payload.kind === "confirm_close") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        dropPendingKeys();
        // Visual-mode spec D12/§7 finding 4: a close-tab confirm ends VISUAL.
        exitRegion();
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
        dropPendingKeys();
        // Visual-mode spec D12/§7 finding 4: a close-others confirm ends VISUAL.
        exitRegion();
        // `<leader>bo` (Owner answers Q2): the same overlay, window-level -- the other two overlays
        // over the conversation area must not fight it for the keys, same as `confirm_close` above.
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        setConfirm({ kind: "close_others", lines: payload.lines });
      } else if (payload.kind === "confirm_bypass") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review). Mirrors `confirm_close`
        // above -- EXCEPT the chooser: D2/spec §3.4 wants this prompt drawn OVER an open chooser
        // (Shift+Tab there posts `cycle_mode`/`cycle_default_mode` without leaving it), so this is the
        // one confirm kind that must not close it.
        dropPendingKeys();
        // Visual-mode spec D12/§7 finding 4: a bypass y/n prompt ends VISUAL -- its own next `y`
        // must answer the prompt, never be read as VISUAL's `y` (copy).
        exitRegion();
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        // Reprompt (D2/D11): a second envelope while one is already open REPLACES the whole object,
        // restarting `openedAt` (and so the 250ms on-screen half of the guard) along with it --
        // unconditional, the same as every other confirm kind's `setConfirm` above.
        setConfirm({ kind: "bypass", tab: payload.tab, scope: payload.scope, nonce: payload.nonce, lines: payload.lines, openedAt: performance.now() });
      } else if (payload.kind === "confirm_restore") {
        // An overlay that takes the keys ends a pending sequence, as every confirm kind does.
        dropPendingKeys();
        exitRegion();
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        // A second envelope while one is open replaces it, restarting its on-screen wait.
        setConfirm({ kind: "restore", nonce: payload.nonce, lines: payload.lines, openedAt: performance.now() });
      } else if (payload.kind === "trust_prompt") {
        // Not drawn here: the layout effect beside `heldTrust` opens it once nothing else holds the
        // keys. Rust asks again whenever a tab waiting for the answer is on screen without its question,
        // the moment after a close prompt, a rename field or the chooser took the keys included; drawn
        // at once, it took the `y` meant for that prompt or typed into that field.
        setHeldTrust(payload);
      } else if (payload.kind === "chooser") {
        // Spec §2.4's cancel list (the chooser, a prompt, a landing on a card): an overlay that takes
        // the keys ends a pending sequence, box and reserved prefix included, so a key the overlay
        // lets bubble can never advance a stale one (the whole-branch review).
        dropPendingKeys();
        // Visual-mode spec D12/§7 finding 4: the chooser ends VISUAL.
        exitRegion();
        // `prefix w` (spec §3.6): the other two overlays over the conversation area must not fight
        // it for the keys, the same reason `begin_rename` and `confirm_close` close them.
        setDetail(null);
        setHandoffOpen(false);
        setKeymapOpen(false);
        setRenaming(null);
        // Owner trial item 2: the same reasoning -- `prefix w` opening over an already-open picker
        // must not leave both drawn at once fighting for the keys. Whole-branch review finding 4:
        // nor may a picker still on its way open over (or after) the chooser -- the bare
        // `/model`/`/effort` it waits for was sent before the user turned to something else.
        setSlashPicker(null);
        setSlashReply(null);
        pendingSlashPickerRef.current = null;
        endKeyPrompts(); // v1, D11/spec §3.4's cancel list -- opening the chooser, not the
        // reverse: `confirm_bypass`'s own branch deliberately does NOT close an already-open chooser.
        // K02 fix round 1 (Codex, blocking): the `/` prompt and the `:` line close too. Every way out
        // of the chooser hands the keys to the conversation root (`onChooserLeave`,
        // `returnKeysToRoot`), so a line left open under it stayed drawn without the keys: `:`, then
        // `prefix w` and Esc, then "the command" `l⏎` walked onto Approve and pressed it.
        setSearch(null);
        setExLine(null);
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
      } else if (payload.kind === "editor_link") {
        setEditorLink({ state: payload.state, text: payload.text });
      } else if (payload.kind === "scratch") {
        setScratchEditing(payload.editing);
      } else if (payload.kind === "review") {
        // A review that was worked out is answered by this envelope alone, never by a `command_result`, so
        // this is where its in-flight record ends.
        inFlight.current.delete(payload.requestId);
        // The reply to the overlay's newest `review_request`; `receiveReview` drops any other (a request
        // the overlay has since replaced), and an overlay that was closed meanwhile takes nothing.
        setReview((current) => (current === null ? current : receiveReview(current, payload)));
        // Opened on that turn: the band's pointer to it has done its job.
        setReviewHints((hints) => (hints[payload.tab] !== undefined && payload.current >= hints[payload.tab].turn ? withoutHint(hints, payload.tab) : hints));
      } else if (payload.kind === "review_diff") {
        inFlight.current.delete(payload.requestId);
        setReview((current) => (current === null ? current : receiveDiff(current, payload)));
      } else if (payload.kind === "review_draft") {
        // The answer to a comment add or remove (its record ends here), or the draft pushed after a change
        // the panel did not ask for. An overlay that is closed keeps nothing: the next `review` carries it.
        if (payload.requestId !== null) inFlight.current.delete(payload.requestId);
        setReview((current) => (current === null ? current : receiveDraft(current, payload)));
      } else if (payload.kind === "review_send_preview") {
        inFlight.current.delete(payload.requestId);
        setReview((current) => (current === null ? current : receivePreview(current, payload)));
      } else if (payload.kind === "review_recovery") {
        setReviewRecovery(payload.entries);
        setReview((current) => (current === null ? current : receiveRecovery(current, payload.entries)));
      } else if (payload.kind === "review_hint") {
        // Kept for the tab it names, shown only while that tab is on screen. One that arrives while a turn
        // runs in the tab on screen is about the turn before it, which the new turn has already replaced.
        if (payload.files <= 0 || (payload.tab === activeTabRef.current && stateRef.current.activeTurnId !== null)) {
          setReviewHints((hints) => withoutHint(hints, payload.tab));
        } else {
          setReviewHints((hints) => ({ ...hints, [payload.tab]: { turn: payload.turn, files: payload.files } }));
        }
      } else if (payload.kind === "notice") {
        showFlash(payload.text);
      }
    });
    requestHello();
    // A page mounted again in the same document (a dev reload) starts unslowed; a real reload is a
    // new document, which has no attribute to begin with, and Rust tells it again on `ready`.
    return () => applyEditorTyping(document.documentElement, false, 0);
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
  /** V1 C1 (spec §3.5), "stale mirror, both ways": every `nav_key` this page cannot apply comes back
   *  as one `nav_fallthrough`, so Rust runs the chord's ordinary `move_focus` instead of dropping the
   *  key (Review Focus 2). `keymapOpen`/`confirm` are checked here rather than left to `EmptyTab`:
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
    if (detail !== null || pathPick !== null || linkPick !== null || sessionEnded) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    // D14 (visual-mode spec): `Ctrl+j` is one of INPUT's own entry routes, and every INPUT route
    // ends the region (CARET/VISUAL/V-LINE alike). Fix round 1 (reviewer finding, minor): `mode ===
    // "browse"` alone left VISUAL/V-LINE out of this branch, so `Ctrl+j` there fell all the way to
    // the stale-mirror fallback below and ran Rust's ordinary `move_focus` instead -- the same
    // chord did something else entirely depending on which mode happened to be on.
    if (direction === "down" && (mode === "browse" || isRegionMode(mode))) {
      if (isRegionMode(mode)) exitRegion();
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
    markLandedCode(null);
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
    if (target.kind === "control" || target.kind === "link") {
      // An input (a permission card's reason box) takes the keys by being focused; a button is
      // selected by being focused, drawn as the solid cursor block. A web link (v1 picks, Task 8, R6) is
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
      // K02 (ruling R3): a landing -- the Enter right after it may press a card's button. Only if
      // focus took (a disabled control takes none); the labels never reach this page, so the count
      // now is the count the next key reads as "the key before".
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
      // K07: reveal the BLOCK, not its row. `revealRow`'s `nearest` on a row taller than the view whose
      // top is out and bottom in aligns the row's bottom with the list's (CSSOM View's "determine the
      // scroll-into-view position"), which can carry the block just landed on off the top.
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
    // Reached by clicks (the detail popover's row, then the confirm's button) that never pass through
    // `answerConfirm`, and the tab it acts on stops being the tab a bypass prompt was drawn for: that
    // prompt goes (v1-mode fix round 1; Rust drops its own in the `HandoffToTerminal` arm).
    cancelBypassConfirm();
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
   *  tab's remembered mode (ruling 4) -- there is no separate `start_session` message any more.
   *
   *  `fromPicker` (v1 trial seam review finding 2, 2026-09-28): `chooseSlashOption` passes `true`
   *  here rather than restoring the composer's own send path -- see `inFlight`'s own doc comment on
   *  `"picker-send"` for why the two need different refusal handling even though both post the same
   *  `send_message`. */
  function sendMessage(text: string, options?: { fromPicker?: boolean }) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    // The text is kept so a refusal can put it back (ordinary sends only -- `"picker-send"`'s own
    // refusal never reads `record.text` back into the box). Dropped again as soon as the reply
    // arrives.
    inFlight.current.set(requestId, { kind: options?.fromPicker ? "picker-send" : "send", tab, text });
    setCommandNotice(null);
    // Owner trial item 2: arms `pendingSlashPickerRef` for the `events` handler above. Set
    // optimistically, before the post is even answered; the `command_result` handler below clears
    // it again on a refusal, since a send that never ran never starts the turn this is waiting for.
    // A picker choice always carries an argument ("/model haiku"), so `barePickerCommand` already
    // returns `null` for it -- this line is harmless either way, kept as one code path rather than
    // two.
    pendingSlashPickerRef.current = barePickerCommand(text);
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
    // Whole-branch review finding 4: the picker held the keys; unmounted, it leaves them on
    // `<body>`, where no key reaches anything. `takeKeys` runs after the render that closed it.
    setKeysRequest((n) => n + 1);
  }

  /** The picker's own Escape/q: closes it, sends nothing, and hands the keys back (finding 4, as
   *  `chooseSlashOption` does). */
  function cancelSlashPicker() {
    setSlashPicker(null);
    setKeysRequest((n) => n + 1);
  }

  /** K03 (kbux 2026-09-29): an overlay whose render throws is closed through its own leave path (`close`,
   *  which also hands the keys back where that path does) and named in the band -- one throw used to
   *  unmount the whole panel. Called from `PanelErrorBoundary`'s `onError` at each overlay's site below. */
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

  /** `r` on an ended tab (ruling 12). Fix round 1 (v1 audit, codex): `TabSet::reset` (Rust) folds
   *  `tab.queue` and Rust's OWN `tab.draft` together as the tab's fresh draft -- reading whatever
   *  Rust currently has on file, not whatever this box shows. A refusal's recovered text is mirrored
   *  through the same 300ms debounce ordinary typing uses (`mirrorDraft`), so pressing `r` inside
   *  that window used to send `reset_tab` while Rust's copy was still stale: the `draft` echo that
   *  came back both wiped the composer and nulled `pendingDraftRef`, silently dropping the mirror
   *  that debounce would otherwise have sent. `flushDraft()` here delivers whatever is still pending
   *  FIRST, the same way a real tab switch already does (ruling 6's `tabs` handler) -- a no-op when
   *  nothing is pending. */
  function resetTab() {
    flushDraft();
    post({ type: "reset_tab" });
  }

  /** Every answer to a card goes through here -- its own buttons, Enter in its reason box, and
   *  `a`/`d` (ruling R2) -- so one card is answered once, whichever of them gets there first. */
  function answerPermission(permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) {
    const tab = activeTabRef.current;
    if (answeredRef.current.has(permissionId) || tab === null) return;
    answeredRef.current = new Set(answeredRef.current).add(permissionId);
    setAnsweredPermissions(answeredRef.current);
    // Recorded, so a refusal gives the card back (`inFlight`'s doc).
    const requestId = nextRequestId();
    inFlight.current.set(requestId, { kind: "permission", tab, permissionId });
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

  /** K02: the `/` and `:` lines stop their own Enter and Esc (`SearchBar`), so `onKeyDown` never
   *  hands those two keys to the typing guard -- and `:ls`, a pause, Enter, then `a` at once
   *  answered the cursor's card as a key standing alone. Each line's callback hands its key over
   *  here first, on `onKeyDown`'s own clock (`event.timeStamp` is what a test's faked `Date` moves). */
  function noteLineKey(event: KeyboardEvent<HTMLInputElement>) {
    // #39 fix round 1: a key, and not a kill -- so a `Ctrl+y` after it is judged on its own.
    lastKeyWasKillRef.current = false;
    const cancelled = typingGuard.onKey(event.key, event.timeStamp > 0 ? event.timeStamp : performance.now());
    if (cancelled !== null) showFlash(cancelled);
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
  /** `Shift+Tab` on an open tab's own row (v1 D6, fix round 1): that tab's own toggle, named by the
   *  row -- `Chooser` offers it only where Rust acts on it (leaving bypass on any tab, entering it on
   *  the active one). The chooser stays open; a bypass prompt this raises is answered inside it. */
  function onChooserCycleTabMode(tab: TabId) {
    postToRust({ type: "cycle_mode", request_id: nextRequestId(), tab });
  }

  /** D11's "every route away cancels the prompt", spec §3.4: called from the `pane_focus` (losing
   *  focus), `tabs` (a DIFFERENT active tab), `arrive`, `focus_permission` and `enter_input`
   *  branches below -- and, since v1-mode fix round 1, from every other route GTK claims before the
   *  WebView sees a key (`hint_collect`, `begin_rename`, `tab_detail`, `open_keymap`, `nav_key`,
   *  `literal_key`) and from a terminal handoff. Rust drops its own outstanding prompt on the same
   *  routes, so a `y` that slipped through is refused by the nonce. Ends a restore question too (it
   *  is another way into bypass, and Rust drops its own the same way); leaves a `close`/`close_others`
   *  confirm alone. **Since the rc.4 review only three callers are left** -- losing the pane's focus, a switch to
   *  another active tab and the terminal handoff, routes that take the keys nowhere inside the panel: every route
   *  that hands the keys to something else calls `endKeyPrompts` (below) instead, which ends all three kinds of
   *  prompt, because a `y` typed into what such a route opens answered a close prompt too. */
  function cancelBypassConfirm() {
    setConfirm((c) => (c?.kind === "bypass" || c?.kind === "restore" || c?.kind === "trust" ? null : c));
    setHeldTrust(null);
  }

  /** rc.4 review (Codex, then the coordinator): what a route that hands the keys to something else ends -- every
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

  /** The window-close prompt (ruling 7) owns every key ahead of everything else, in BOTH layouts:
   *  the conversation's own `onKeyDown` below, and `EmptyTab`'s root while the chat is on screen
   *  with no session yet (spec §3.4: "prefix & 先显示 chat 并给它键位"). Lifted out of either
   *  handler so both call it first and agree. Returns whether it claimed the key -- only
   *  `confirm === null` does not.
   *
   *  v1 (D1/D11) adds kind `"bypass"`: EVERY non-modifier key still cancels the whole switch except
   *  a counted `y`/`Y` (D1's own "n/any key but a counted y/Y cancels"), and even a `y`/`Y` that
   *  arrives too soon after the prompt appeared or after another key -- typed anywhere in the panel,
   *  `lastKeyAtRef` sees all of it -- cancels too, with its own flash rather than the generic one,
   *  since D11 wants the reader to know EXACTLY why it did not count. Either way the prompt closes:
   *  there is no world where a lone `y` fails the guard and the prompt is left open for a retry with
   *  no new envelope, since that would let a SECOND stray key close the gap the guard just refused.
   *
   *  v1-mode fix round 1 (whole-branch review): three more things, each a way a key did more than
   *  answer the prompt.
   *  - **Called in the CAPTURE phase at each layout's root** (`onKeyDownCapture` below), and it stops
   *    propagation of every key it claims. Called only from bubble handlers, a key from inside the
   *    chooser was answered twice (the chooser's own call, then the conversation root's with the same
   *    stale `confirm`: a second `confirm_bypass` refused as "no longer current" and drawn as an
   *    error banner), and an input's own Enter/Escape (the chooser's filter and rename, the tab bar's
   *    rename, the `/` prompt, the composer's send) ran first and left the prompt open -- spec §3.4:
   *    any key but a counted `y`, "Enter included", cancels the prompt and does nothing else.
   *  - **The typing guard sees the key** (v1 S1): it is fed here, since a claimed key never reaches
   *    `onKeyDown`'s own `typingGuard.onKey`. Without it the key that cancelled the prompt was
   *    invisible to S1, and `x` then `a` 100 ms later approved the card the R06 prompt was drawn
   *    over.
   *  - **A held key's repeat never counts** (S1's own rule for cards): a counted `y` can be answered
   *    with a D7 reprompt within milliseconds, and the same held key's first autorepeat would then
   *    confirm a prompt listing a card the user never saw.
   *
   *  Defect 2 (2026-09-27 sandbox GUI pass): a HELD key whose first press cancelled a prompt (the
   *  paragraph just above -- e.g. a `y` too soon to count) leaves `confirm` null, so every one of
   *  its physical auto-repeats after that used to hit the `confirm === null` return below and fall
   *  straight through as an ordinary key -- BROWSE's own `y` (copy the row under the cursor), which
   *  calls `copied()` and overwrites the "y must be pressed on its own..." flash above with
   *  "copied N chars" within about one repeat interval (~40ms). Fixed by swallowing that SAME key's
   *  repeats, specifically, until its keyup (`promptSwallowKeyRef`, set below, cleared by the effect
   *  that owns it) -- checked here, ahead of the `confirm === null` return, so it still applies once
   *  `confirm` is gone. A confirm that is NOT null (a fresh D7 reprompt arrived in between) skips
   *  this and reaches the branch below unchanged, which already treats a repeat as never counting. */
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
          // Whole-branch review (codex, 2026-09-28): a y with Ctrl/Alt/Meta held, or one an input
          // method is composing, is an editing keystroke, never D11's lone y -- the timing guard
          // alone let `Ctrl+y` or a pinyin syllable's first letter enter bypass. Shift stays (`Y`).
          // v1 audit fixes (2026-09-28): reconciled with keymap.ts's card-answer R2 rule onto one
          // shared predicate, `isPlainAnswerKey` -- this file's own original hand-written check
          // (superseded, not visible above any more) never tested Super/Hyper, the same gap R2's own
          // fix round 1 closed on `a`/`d`/`D`; fix round 2 closed a further gap neither side had,
          // AltGraph. `event.nativeEvent` is what carries a real `getModifierState`, the same cast
          // `resolveKey`'s own call site uses.
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
          showFlash(trustFooter(confirm.view.remember));
          return true;
        }
        const pendingG = trustPendingGRef.current;
        trustPendingGRef.current = false;
        const action = resolveTrustKey(event.nativeEvent as unknown as TrustKeyEvent, {
          now: performance.now(),
          openedAt: confirm.openedAt,
          lastKeyAt: lastKeyAtRef.current,
          pendingG,
        });
        if (action.kind === "answer") {
          // Echoes what was drawn: the page never computes either value, so an answer to anything other than
          // the shown findings is refused by Rust.
          const answerId = nextRequestId();
          inFlight.current.set(answerId, { kind: "trust", tab: confirm.tab });
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
          inFlight.current.set(cancelId, { kind: "trust", tab: confirm.tab });
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
          showFlash(trustFooter(confirm.view.remember));
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
            inFlight.current.set(cancelId, { kind: "trust", tab: confirm.tab });
            postToRust({ type: "trust_cancel", request_id: cancelId, tab: confirm.tab, nonce: confirm.nonce });
            setConfirm(null);
          })
        }
      >
        <TrustPrompt ref={trustOverlayRef} envelope={confirm.view} />
      </PanelErrorBoundary>
    ) : null;

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
        // composer, which would otherwise take `m`, `j` or a typed letter under it. A y/n prompt
        // comes before even that (ruling 7), in capture for the reason `answerConfirm`'s own doc
        // gives: the chooser's inputs and the composer must not see a key the prompt takes.
        onKeyDownCapture={(event) => {
          if (answerConfirm(event)) return;
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
              // V1 P11 (spec §10.1): the dashboard's own first-run hint line reads this live, rather
              // than assuming the stock default -- the same value the `?` overlay already shows as
              // `prefixLabel`.
              prefix={keymapHelp.prefix}
              editorLink={editorLink}
              focusRequest={inputRequest}
              arriveRequest={arriveRequest}
              // Wave 3 Task 1: `EmptyTab` bumps `Composer`'s focus itself and lands its own root on a
              // bare `keysRequest`, so the `prefix w` chooser drawn over an empty tab 1 and a
              // `keysRequest` bump both reach it directly rather than through `App`'s own
              // `containerRef`, which does not exist on this layout.
              keysRequest={emptyKeysRequest}
              dropKeysRequest={emptyDropKeys}
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
              onRestoreLast={() => post({ type: "restore_last" })}
              onCycleMode={() => post({ type: "cycle_mode" })}
              onReset={resetTab}
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
          {confirm?.kind === "trust" && trustOverlay}
        </div>
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
        {/* Owner trial item 2 (2026-09-28): a bare /model or /effort reply opens this, the same
            positioning reasoning as the chooser just above -- no `.agent-ui-scroller` on this
            layout to nest inside either. */}
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
        {/* Spec §7's `? Keys` (and `?` on an empty draft, `prefix ?`): drawn here too since the GUI
            pass (2026-09-26); `.agent-ui-root` is this layout's positioned ancestor, as for the
            chooser. */}
        {keymapOpen && (
          <PanelErrorBoundary name="keys overlay" onError={() => overlayFailed("keys overlay", () => setKeymapOpen(false))}>
            <KeymapOverlay
              ref={keymapOverlayRef}
              onClose={() => setKeymapOpen(false)}
              windowKeys={keymapHelp.window}
              prefixKeys={keymapHelp.prefixKeys}
              prefixLabel={keymapHelp.prefix}
              panel={panelTable}
              tmuxSkipped={keymapHelp.tmuxSkipped}
              companion={editorLink !== null}
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
  /* The conversation is on its way out: Rust already owns the backend on a shutdown worker and will
     refuse every command until that finishes. The composer must reflect that rather than accepting
     input it cannot deliver. */
  const handingOff = tabs !== null && handoffRequests.has(tabs.active);

  /** v1 S1/S4 (spec §2.1-§2.2): `a`/`d`/`D` on a card, and (K02, ruling R3) Enter on a card's own
   *  button. `stillThere` says whether the card the key was aimed at can still take it, or is `null`
   *  when there is no card for the cursor (F13's flash). The answer runs `TYPING_GUARD_MS` later,
   *  only if the key stood alone before and nothing cancels the wait; by then the card must still be
   *  there (`stillThere`) and the panel still in BROWSE. `key` is what the guard judges the key
   *  before against: only Enter passes it, for S1's walk exception (`TypingGuard.defer`). */
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

  /** v1 hardening, ruling R2: `a`/`d` answer the card at timeline index `target` by its own
   *  `permissionId`, through `answerPermission` -- the path the card's buttons take -- never by
   *  clicking a button a DOM query found. The query this replaced indexed every
   *  `[data-nav-stop="row"]` in the subtree with a TIMELINE index, so rows a model reply drew shifted
   *  it, and `a` on the card the cursor showed approved the one above it (review
   *  `2026-09-27-v1-hardening/codex-sec-panel-content-verdicts.md`, finding 1). The timeline item
   *  is the card the cursor is drawn on, by construction (`MessageList` draws `.row-current` on
   *  `timeline[cursor]`). `d` sends the reason already typed into the card's box, as its Deny does. */
  function answerCardByKey(
    event: KeyboardEvent<HTMLDivElement>,
    typedAt: number,
    target: number | null,
    decision: "allow" | "deny",
  ) {
    const item = target === null ? undefined : timeline[target];
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

  /** Owner decision #39 (2026-09-30, "input 直接ctrl y统一吧，不用两次"): INPUT's `Ctrl+y` approves the
   *  ACTIVE tab's OLDEST waiting card (`oldestWaitingPermission`, the card a card landing takes,
   *  R11) without leaving INPUT. Only from the composer's own box -- not the `Ctrl+r` search field,
   *  whose text is a query. With no card waiting it does nothing and leaves the key to the box.
   *  Otherwise the key is claimed (so the box never acts on it: its text, caret and history stay as
   *  they were) and runs S1's rule as `a` does (`TypingGuard.defer`, no new clock): only a Ctrl+y
   *  with no key within `TYPING_GUARD_MS` before it -- so readline's `Ctrl+u`/`Ctrl+w` then `Ctrl+y`
   *  yank never approves -- and none within it after, and never a held key's repeat. At fire time the
   *  same card must still wait in the same tab, the session still live and the panel still in INPUT;
   *  it is answered by its own `permissionId` through `answerPermission`, the path its buttons and
   *  `a` take (S4, R2), never by pressing a button a DOM query found. */
  function approveOldestByKey(event: KeyboardEvent<HTMLDivElement>, typedAt: number, afterKill: boolean) {
    const box = event.target;
    if (!isComposerBox(box)) return;
    const index = sessionEnded ? null : oldestWaitingPermission(timeline, answeredRef.current);
    const item = index === null ? undefined : timeline[index];
    if (item?.kind !== "permission") return;
    event.preventDefault();
    const permissionId = item.request.permissionId;
    const tab = activeTabRef.current;
    const targetKey = `${tab}:${permissionId}`;
    // Fix round 1: three refusals ahead of the wait, each saying why. S1's before-half first (the
    // flash the fast `Ctrl+u` `Ctrl+y` already gave); then a kill right before it, whatever the pause
    // (Opus I-1); then a card that has not been the one the band names for `TYPING_GUARD_MS`
    // (Opus B-1) -- which also refuses while this panel's own approval is still moving the band on.
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
          // Fix round 1 (Codex): the box still has the keys -- a click into the `Ctrl+r` search, or
          // anywhere else, is no key and cancels nothing on its own.
          document.activeElement !== box ||
          !box.isConnected ||
          // Fix round 1 (Opus B-1): still the card the band named when the key was pressed.
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
  // ---- BROWSE visual mode (spec docs/superpowers/specs/2026-09-28-browse-visual-mode-design.md) ----
  //
  // Handled entirely in the root's `onKeyDownCapture` (D13), never in `onKeyDown` below: every
  // VISUAL/V-LINE key is decided by `resolveKey`'s own visual branch and applied here, and the
  // capture handler stops its propagation so no card, button or reason box downstream ever sees it
  // (Review Focus 1 of the spec's §7, the reason box's own Enter/click handlers in particular).

  function visualRoot(): HTMLElement | null {
    return containerRef.current;
  }

  /** D6/D7/D8's own boundary -- "it never leaves `.message-list`", "inside the list" -- which is
   *  narrower than `visualRoot()` (the whole `.agent-ui-conversation` root, which also holds the
   *  activity line, the composer and the status band). Fix round 1 (finding 3 of both review
   *  programs): motions and the D8 "still highlighted" check used to take `visualRoot()` itself,
   *  so a step past the last row could land in, and `y` could copy from, that live chrome. */
  function messageListEl(): HTMLElement | null {
    return visualRoot()?.querySelector<HTMLElement>(".message-list") ?? null;
  }

  /** The row (frozen DOM) `caret` sits in, or `null` if `caret` is not inside any row -- the shared
   *  first half of `rowKeyOfCaret` below and, since v1 trial seam review finding 3, of the
   *  `scroll-line` case's own line-height read (`rowTextElement` needs a row, never `.message-list`
   *  itself: see its own doc comment on why reading the list directly gave a fraction of a real
   *  line). Extracted rather than duplicated when the second caller needed the same "caret -> row"
   *  step `rowKeyOfCaret` already had, one line further. */
  function rowElementOfCaret(caret: Caret): HTMLElement | null {
    const root = visualRoot();
    if (root === null) return null;
    const el = caret.node instanceof Element ? caret.node : caret.node.parentElement;
    if (el === null) return null;
    return rowOf(root, el);
  }

  /** The row `caret` sits in, as a key into `frozenTimeline` -- D11's own rule: rows are matched by
   *  key, never by index, because a row's position in the FROZEN timeline and the LIVE one can
   *  differ once a linked card arrives during VISUAL (spec §7, finding 3). `null` when `caret` is
   *  not inside any row (should not happen: D6 never lets the caret leave `.message-list`, and every
   *  row in the frozen DOM belongs to a row of `frozenTimeline`). */
  function rowKeyOfCaret(caret: Caret): string | null {
    const root = visualRoot();
    if (root === null || frozenTimeline === null) return null;
    const row = rowElementOfCaret(caret);
    if (row === null) return null;
    const frozenIndex = rowIndexOf(root, row);
    if (frozenIndex === null) return null;
    return frozenTimeline[frozenIndex]?.key ?? null;
  }

  /** D8/D9: places the LIVE row cursor on the row named by `key` (found in the frozen DOM by
   *  `rowKeyOfCaret` above), without moving the view (`landingRef` "keep", the same convention every
   *  other cursor-preserving move in this file uses). A `key` no longer in the live timeline (should
   *  not happen for the row VISUAL was just looking at) is simply left alone.
   *
   *  Fix round 1 (reviewer finding, minor): `"keep"` was set even when the live index equals the
   *  cursor already on screen (the common `v…y`/`Esc`-on-the-cursor-row case) -- `landingRef` is
   *  consumed only by the `[cursor]` effect's own reveal, which never fires for a `setCursor` that
   *  changes nothing, so a stale `"keep"` sat there and silently swallowed the NEXT landing's reveal
   *  instead (a `/` search, `n`, a HINT row, `D`). Set only when the index actually changes, the same
   *  guard the `arrive` effect's own `"keep"` write already uses. */
  function landCursorOnRowKey(key: string | null) {
    if (key === null) return;
    const liveIndex = indexOfKey(timeline, key);
    if (liveIndex === null) return;
    if (liveIndex !== cursorRef.current) landingRef.current = "keep";
    setCursor(liveIndex);
  }

  /** D8: keeps the moving end on screen through every scrollable box between it and the list -- a
   *  260px `.tool-result-body`, a `.table-scroll` sideways, then the list itself -- each nudged the
   *  least that reveals the caret's own point (`visual.ts`'s `revealCaret`), never a whole enclosing
   *  element's `scrollIntoView`, and announces the move through `noteUserScroll`.
   *
   *  Fix round 1 (reviewer finding, important, both review programs): the old body called
   *  `scrollIntoView` on the caret's PARENT element, which for text sitting directly inside the
   *  260px box IS that box -- `scrollIntoView` moves an element's ANCESTORS, never its own
   *  `scrollTop`, so a caret past the box's own visible bottom (W6) was never brought back on screen.
   *
   *  Fix round 2 (review finding, important): the announcement covers the WHOLE key, not only this
   *  last nudge. A `j`/`k` whose goal-column snap or geometry probe had to reveal the line it
   *  measured (`visual.ts`) has already scrolled the list by the time this runs, so the caret is on
   *  screen and nothing here moves -- which used to mean nothing was announced, `follow.ts` never
   *  learned the view moved, and a `k` inside a followed reply left the list snapping back to the
   *  bottom when the region ended. `listTopBefore` is the list's `scrollTop` when the key arrived;
   *  any change from it is announced, `"up"` when the list went up (following stops at once, as
   *  BROWSE's own `k` does), `"unknown"` otherwise (the scroll's direction decides). Without real
   *  layout (jsdom) `revealCaret` does nothing and the old coarse fallback runs. */
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

  /** D2: `v`/`V` from BROWSE (revised for 3a, §9: `v` starts CARET, `V` starts V-LINE directly,
   *  O8's kept default). Refuses with a flash and does nothing else when there is nowhere to start
   *  from (no row under the cursor, or the keys are on a banner/Stop rather than a row). Right
   *  after a HINT landed on a code block, the entry caret is that block's own first character
   *  (`landedCodeBlock`, `y`'s own `landedCode` read the same way, D2's own text); otherwise the
   *  cursor row's own first selectable character ON SCREEN (`entrySelectableCaret`, D2's "the first
   *  one under the list's top edge" when the row's own top has scrolled off it). A pending BROWSE
   *  count is dropped (D2: "3v is not in v1").
   *
   *  `landedCodeBlock` fix round 1 (reviewer finding, important, both review programs): `onKeyDown`
   *  clears `copyCodeRef.current` (for its OWN `"copy"` case, the plain `y` a HINT-landed code block
   *  answers) before its switch ever reaches `case "visual"`/`case "caret"` below -- reading the ref
   *  here, as the first version of this function did, always saw `null`, so `v` right after a HINT
   *  never started inside the block. The caller now captures the ref's value once, alongside its own
   *  `landedCode`, and passes it through. */
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
    // D1 (3a): BROWSE's `v` (line === false) lands in CARET; `V` (line === true) still goes straight
    // to V-LINE.
    const model: VisualModel = { anchor: caret, cursor: caret, kind: line ? "line" : "caret", goalX: null };
    visualModelRef.current = model;
    visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, model);
    // D2: a count typed before `v` belongs to nothing now; D5's own count starts fresh.
    countRef.current = null;
    visualCountRef.current = null;
    regionPendingGRef.current = null;
    setFrozenSnapshot({ state, expanded, detailed, ruleOffers, answeredPermissions, sessionEnded });
    modeRef.current = line ? "vline" : "caret";
    setMode(modeRef.current);
    root.focus({ preventScroll: true });
  }

  /** The guts of `exitRegion` below, minus its own `modeRef`/`setMode` write -- split out (fix round
   *  1, the blocking finding of both review programs) so the `[mode]` backstop effect further down
   *  can run the SAME cleanup after `mode` has ALREADY left "visual"/"vline" some other way, when
   *  `exitRegion`'s own guard would already have returned having done nothing. Clears the selection
   *  VISUAL itself built, and only that one (a mouse selection made meanwhile is left alone, D9's own
   *  rule, checked with `selectionMatchesBuild`), and -- the actual bug -- `frozenSnapshot`: left set,
   *  `MessageList` (`state={frozenSnapshot?.state ?? state}` below) keeps showing a stale, answered,
   *  or foreign-tab conversation regardless of what `mode` now is, which is what let a keypress meant
   *  for the card ON SCREEN answer a DIFFERENT, live one instead (Codex's own reproduction: a session
   *  ends during VISUAL, the tab is switched, and the still-frozen card's `a` approves the new tab's). */
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

  /** The band's own word for a region mode (D16): `CARET`, `V-LINE`, else `VISUAL`. */
  function regionModeName(m: PanelMode): string {
    return m === "caret" ? "CARET" : m === "vline" ? "V-LINE" : "VISUAL";
  }

  /** Whether `m` is one of the region's own three modes (D1: CARET/VISUAL/V-LINE together), the one
   *  check every D14 exit route and the `[mode]` backstop share -- extracted once 3a added a third
   *  mode so no call site has to spell the disjunction out by hand. */
  function isRegionMode(m: PanelMode): m is "caret" | "visual" | "vline" {
    return m === "caret" || m === "visual" || m === "vline";
  }

  /** D9/D12/D14: ends the WHOLE region (CARET, VISUAL or V-LINE), back to BROWSE, wherever it is
   *  called from -- a no-op outside all three. `modeRef.current` is set synchronously (not only
   *  through `setMode`) so code that reads it immediately after calling this -- the `tabs` dispatch
   *  arm's own `saveView`, in particular -- never saves "caret"/"visual"/"vline" as a tab's parked
   *  mode (nothing in this panel ever restores one). */
  function exitRegion() {
    if (!isRegionMode(modeRef.current)) return;
    clearVisualLeftovers();
    modeRef.current = "browse";
    setMode("browse");
  }

  /** D13: the whole CARET/VISUAL/V-LINE key table, run from the root's `onKeyDownCapture`, ahead of
   *  `onKeyDown` below and of every descendant's own keydown handler. A no-op outside the region
   *  (returns at once, claiming nothing, so `onKeyDown` and everything under the root see the key
   *  exactly as they do today). */
  function onVisualKeyDownCapture(event: KeyboardEvent<HTMLDivElement>) {
    const currentMode = modeRef.current;
    if (!isRegionMode(currentMode)) return;
    const root = containerRef.current;
    // D12/D13: a region key always targets the root; should focus have escaped it without going
    // through one of the explicit exit routes elsewhere in this file, this is the backstop -- end
    // the region and swallow the key so no descendant (a reason box, a button) ever sees it.
    if (root === null || event.target !== root) {
      exitRegion();
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    const typedAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
    // D13: the typing guard sees every region key too, the same as every other keydown this panel
    // claims -- a card `a`/`d`/`D` deferred moments before the region started stays cancellable by
    // it.
    const cancelled = typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typedAt);
    const model = visualModelRef.current;
    if (model === null) {
      exitRegion();
      return;
    }
    // D5/D2 (3a): a bare modifier's own keydown never drops a region-local pending `g` -- the same
    // convention BROWSE's own `pendingRef` gets, above (`onKeyDown`'s own `isModifierKey` check):
    // physical Shift on the way to `G`/`V`/`$` must not silently cancel a `gg` in progress. Checked
    // before `count`/`regionPendingGRef` are read and cleared below, so both are simply left alone.
    if (regionPendingGRef.current !== null && isModifierKey(event.key)) {
      if (cancelled !== null) showFlash(cancelled);
      return;
    }
    // D6/D8's own boundary, not `root` (`visualRoot()`, the whole conversation root, which also
    // holds live chrome the freeze never touches: the activity line, the composer, the status band).
    // Fix round 1 (reviewer finding, important, both review programs): a motion or the D8 "still
    // highlighted" check taking `root` here could step into, and `y` could copy from, that chrome.
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
      // D10/D12: everything that reaches here with `action === null` is genuinely unclaimed --
      // idle Ctrl+c in VISUAL/V-LINE (native copy), or `gv`'s own reservation (D2: "does nothing"),
      // whose consumed pending `g` stays dropped, already cleared above. Nothing is swallowed:
      // the browser's own default runs undisturbed, and the mode stays exactly as it was -- "keeps
      // a count" (D10) included: fix round 1 (reviewer finding, minor), a count typed before a bare
      // modifier's own keydown (physical Shift on the way to `$`/`V`) was left cleared above with
      // nothing to restore it. (A bare modifier itself never reaches this branch: the early return
      // above only covers one WITH a pending `g`; the ordinary case is `resolveCaretKey`/
      // `resolveVisualKey`'s own `isModifierKey` check, which also returns `null`, restored here.)
      visualCountRef.current = count;
      if (cancelled !== null) showFlash(cancelled);
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const sel = window.getSelection();
    // D10: ending the region with THIS key means its own physical auto-repeats must never fall
    // through to BROWSE's/INPUT's meaning for the same key once `mode` has changed away (a held `d`
    // denying a card, a held `y` copying the row it just landed on, a held `>` typing a literal
    // character into the composer) -- `promptSwallowKeyRef` is the exact mechanism `answerConfirm`
    // already uses for a y/n prompt's own held key (App.tsx's own doc comment on it), reused here
    // for the same class of bug. Set right before ending on every route that leaves the region from
    // a keypress, so the very next (repeat) keydown of this key is swallowed by `answerConfirm`'s
    // own check -- which runs ahead of this handler on every key -- before it ever reaches BROWSE's
    // or INPUT's table.
    function endRegionFromKey() {
      promptSwallowKeyRef.current = event.key;
      exitRegion();
    }
    switch (action.kind) {
      case "count":
        visualCountRef.current = accumulateMotionCount(count, action.digit);
        return;
      case "pending":
        // D5/D2: the first `g` of `gg`, or of the `gv` reservation -- armed for the next key.
        regionPendingGRef.current = action.prefix;
        return;
      case "vmove": {
        // Motions repeat, as a held `l` does in vim (D10) -- no repeat guard here. D6: `gg`/`G`
        // take no count and drop one (vim's `3G` is a buffer line number, which the panel does not
        // have) -- always exactly one step, regardless of what `count` held.
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
        // D5: a repeated `o` does nothing. Not reachable from CARET (`resolveCaretKey` never
        // returns it), but the guard costs nothing to keep uniform.
        if (event.repeat || sel === null) return;
        const next = swapEnds(model);
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        scrollCaretIntoView(next.cursor, listEl?.scrollTop ?? null);
        return;
      }
      case "vtoggle": {
        // D1: from CARET, `v`/`V` always START a selection mode; from VISUAL/V-LINE, the OTHER key
        // switches to the sibling one (the mode's OWN key is `vback`, below, never reaches here).
        // D10: a repeated `v`/`V` does nothing either way.
        if (event.repeat || sel === null) return;
        const next: VisualModel = { ...model, kind: action.line ? "line" : "char" };
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        modeRef.current = action.line ? "vline" : "visual";
        setMode(modeRef.current);
        return;
      }
      case "vback": {
        // D1 (3a): VISUAL/V-LINE's own key, or `Esc`, goes back to CARET on the MOVING end -- the
        // region and its freeze stay on (D13/D14: "Esc from VISUAL is not an exit"). D10: a
        // repeated `v`/`V`/`Esc` does nothing.
        if (event.repeat || sel === null) return;
        const caretModel: VisualModel = { anchor: model.cursor, cursor: model.cursor, kind: "caret", goalX: null };
        visualModelRef.current = caretModel;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, caretModel);
        modeRef.current = "caret";
        setMode("caret");
        // D12, fix round 2 (review finding, minor): the key that leaves a mode swallows its own
        // repeats until keyup. A held `Esc` used to step back to CARET on its first press and then
        // end the whole region on its first auto-repeat (CARET's own `Esc`), dropping the caret and
        // the freeze the user only meant to keep.
        promptSwallowKeyRef.current = event.key;
        return;
      }
      case "vyank": {
        // D10: a repeated `y` does nothing (and must not fall through as BROWSE's own `y`, which
        // `endRegionFromKey`'s `promptSwallowKeyRef` guards against for the repeat that follows).
        if (event.repeat) return;
        if (sel === null || root === null || listEl === null) {
          endRegionFromKey();
          return;
        }
        const built = visualBuiltRef.current;
        // D8: copy only what the user actually saw highlighted, and still inside `.message-list`.
        if (built === null || !selectionMatchesBuild(sel as unknown as SelectionLike, built, listEl)) {
          showFlash("selection changed under it — nothing copied; v to start again");
          endRegionFromKey();
          return;
        }
        const text = copySelectionText(sel as unknown as SelectionLike, root, model);
        // D8: after a charwise yank the row cursor goes to the row holding the FIRST yanked
        // character (`nvim: change.txt:1200`), i.e. the earlier of the two ends -- not wherever the
        // moving end (`cursor`) happened to be left.
        const earlier = compareCarets(model.anchor, model.cursor) <= 0 ? model.anchor : model.cursor;
        const key = rowKeyOfCaret(earlier);
        landCursorOnRowKey(key);
        copied(text, key ?? undefined);
        endRegionFromKey();
        return;
      }
      case "vquote": {
        // D10 (3a): `>` quotes the highlighted text into the tab's draft and ends the region into
        // INPUT -- never sends anything. D10: a held `>` quotes once, its repeats swallowed by
        // `promptSwallowKeyRef` (set below) until keyup, so none is typed into the composer.
        if (event.repeat) return;
        if (sel === null || root === null || listEl === null) {
          endRegionFromKey();
          return;
        }
        const built = visualBuiltRef.current;
        // D10: "Text: exactly what y would copy, through D9's check and shield; a failed check
        // quotes nothing, as y does" -- same message, same whole-region ending as `vyank`'s own.
        if (built === null || !selectionMatchesBuild(sel as unknown as SelectionLike, built, listEl)) {
          showFlash("selection changed under it — nothing copied; v to start again");
          endRegionFromKey();
          return;
        }
        // D10's two refusals: an ended session (the box is disabled, as `i` is refused there) and
        // the draft open in nvim (`scratchEditing`) -- VISUAL/V-LINE STAYS, unlike every other exit
        // above, so the user can back out with `Esc`/`y` instead of losing the selection.
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
        // D12: a repeated `Esc` does nothing (the one that ended a mode is swallowed upstream by
        // `promptSwallowKeyRef`; this covers a repeat that reaches CARET any other way).
        if (event.repeat && action.key === "Escape") return;
        landCursorOnRowKey(rowKeyOfCaret(model.cursor));
        endRegionFromKey();
        // D1: `Esc` says nothing; D12: any other unbound key says what it is not, naming CARET's
        // own hint or VISUAL/V-LINE's. Fix round 1 (reviewer finding, minor): `action.key`
        // interpolated raw read as "VISUAL ended:   is not a VISUAL key" for Space (the leader,
        // likely pressed out of habit) -- a human name for the one key worth naming, the same
        // `humanKey` uses for the leader elsewhere.
        //
        // Finding 4 (v1 trial seam review, 2026-09-28): Enter used to get the same treatment as
        // any other unbound key ("is not a VISUAL key (y copies)"), which is true but useless --
        // it never pointed at the one route that actually reaches a folded item-7 row from here
        // (D7: "Text not drawn is not reachable... Enter before v unfolds a fold or a run"). Enter
        // gets its own short flash naming that route instead, in every mode; every other unbound
        // key keeps its existing wording unchanged.
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
        // D10/D12: a modified key this table does not otherwise claim. Already prevented/stopped
        // above; nothing else changes -- except an idle `Ctrl+c` in CARET specifically (D12, a
        // consequence of D3), which is claimed rather than left to the engine's native copy and
        // says why.
        if (currentMode === "caret" && event.key === "c" && event.ctrlKey) {
          showFlash("nothing selected — v, then y");
        }
        return;
      case "scroll-line": {
        // v1 trial seam review finding 3: Ctrl+e/Ctrl+y scroll the frozen list one line -- counted,
        // the same convention BROWSE's own identical action uses (R4) -- without moving the caret,
        // the selection, or the mode: distinct from every other case in this switch, none of which
        // touch `listEl.scrollTop` while leaving `model`/`sel` alone. `computedLineHeight` is read
        // off the caret's own row text (`rowTextElement`), never `.message-list` itself, which sets
        // neither font-size nor line-height of its own (that helper's doc comment: reading the list
        // directly gave a line a fraction of the real one, the exact bug it exists to avoid).
        if (listEl === null) return;
        const row = rowElementOfCaret(model.cursor);
        const scrollTimes = Math.max(1, count ?? 1);
        listEl.scrollTop += action.delta * computedLineHeight(row !== null ? rowTextElement(row) : listEl) * scrollTimes;
        return;
      }
      case "keymap":
        // D10/D12: `?` ends the region and opens the keymap -- the which-key box, the panel's only
        // discovery route.
        landCursorOnRowKey(rowKeyOfCaret(model.cursor));
        endRegionFromKey();
        setKeymapOpen(true);
        return;
      case "interrupt":
        // D10: the one Ctrl chord the region keeps its BROWSE meaning for -- the region itself
        // stays on.
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
    // overlay and everything else below (ruling 7): tmux's own `confirm-before` prompt takes the
    // keys the same way.
    if (answerConfirm(event)) return;
    // v1 S1 (spec §2.1): every other keydown is "a key" to the typing guard, ahead of the overlays
    // below so a key typed into one still counts. It cancels a waiting `a`/`d`/`D` -- which says
    // so in the band -- and this key still does whatever it does. Shift+Tab as WebKitGTK delivers
    // it (`isShiftTab`) is the walk key `Tab`, like the standard shape.
    const typedAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
    // The flash is the cancelled wait's own (`TypingGuard.defer`'s `cancelledFlash`): a cancelled
    // `f` or `L` says what did not happen, never the `a`/`d` text (the whole-branch review).
    const cancelled = typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typedAt);
    if (cancelled !== null) showFlash(cancelled);
    // #39 fix round 1 (Opus I-1): whether the key before this one was a kill in the composer, and
    // whether this one is (a bare modifier is neither, and leaves it alone).
    const afterKill = lastKeyWasKillRef.current;
    if (!isModifierKey(event.key)) lastKeyWasKillRef.current = isKillKey(event);
    // K02 (ruling R3): a plain Tab's own focus move -- the browser's, right after this keydown -- is
    // a landing, which the root's `onFocus` records against this key's count. Any other key ends it.
    // Fix round 2: only a Tab whose default is still alive moves focus. One that is claimed moves
    // nothing, and the next focus no key made -- a mouse pressed on Approve and dragged off it -- must
    // not become its landing. Fix round 3 (review): a box claims some before this handler runs (the
    // `/` and `:` lines take every Tab), but this handler swallows others further down (K01's cancel
    // after `g`/`z`/`[`/`]`, the leader's sequence, `gf`'s pick, the `?` overlay, the detail
    // popover), so the default is known only once every handler has run: the native event is kept,
    // and `onFocus` reads it when the focus arrives.
    tabAtKeyRef.current =
      event.key === "Tab" && !event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey
        ? { atKey: typingGuard.keyCount(), event: event.nativeEvent }
        : null;
    // A card's own answer buttons (Approve, Deny, Always allow): see the Enter/Space rules below.
    const onAnswerButton =
      isActivatableControl(event.target) && (event.target as HTMLElement).closest("[data-nav-action]") !== null;
    // K02 fix round 1 (review): the two modal overlays just below return ahead of those rules, so a
    // card button still holding focus under one -- a mouse pressed on Approve and dragged off it, with
    // no click -- took Enter or Space as the browser's own activation, answering the card with no
    // guard at all. Claimed here too: under either overlay those keys on a card button do nothing.
    if ((chooser !== null || slashPicker !== null) && onAnswerButton && (event.key === "Enter" || event.key === " ")) {
      event.preventDefault();
    }
    // The chooser is modal (Codex v1-mode finding 5): it is drawn inside this root, and a key it does
    // not handle itself (`a`, `d`, anything but its own j/k/Enter/x/`/`/q/Esc/g/G/Ctrl+r/Shift+Tab)
    // bubbled here and reached `resolveKey`, whose `a`/`d` pressed the Approve/Deny of a card the
    // chooser was covering. It returns only AFTER the typing guard above has seen the key, so a key
    // typed into the chooser still counts as typing once it closes; and with no `preventDefault`,
    // since a key arriving from its filter or rename input is text that input still has to receive.
    if (chooser !== null) return;
    // Owner trial item 2: the picker is modal the same way (its own `j`/`k`/`Enter`/`Escape`/`q`
    // handler `stopPropagation`s every key it claims, so this only ever runs for a key it does
    // not -- exactly the chooser's own reasoning just above).
    if (slashPicker !== null) return;
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
    // v1 picks, Task 8 (R6, Review Focus 1): `gx`'s pick owns the keys the same way, with two differences a
    // copy of the block above would get wrong. Only a PLAIN letter chooses -- no Ctrl, Alt, Meta, Super or
    // AltGr, and no key an input method is composing (`isPlainAnswerKey`, the rule `a`/`d` answer a card by),
    // so Ctrl+a or Alt+s can never open a link. And the `x` of `gx` still held (its auto-repeat), or a bare
    // Shift on its way to a capital, is not a choice: the pick keeps waiting rather than being cancelled the
    // instant it appears. Anything else ends it and does nothing.
    //
    // A key typed into a text field (the composer, a card's reason box) is that field's, not a choice: the pick
    // ends and the key falls through to whatever the field does with it (fix round 1, Codex).
    if (linkPick !== null && isEditableElement(event.target)) setLinkPick(null);
    else if (linkPick !== null) {
      event.preventDefault();
      if (event.repeat || isModifierKey(event.key)) return;
      const index = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike) ? HINT_ALPHABET.indexOf(event.key) : -1;
      if (index !== -1 && index < linkPick.length) openUrl(linkPick[index]);
      setLinkPick(null);
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
    // A bare modifier's own keydown (the Shift of `gT`, v1 polish F16) is not "the next key": it
    // must not drop a pending `g`/`z`/`[`/`]`, a count or its which-key box, as vim waits through it.
    // Fix round 1 (Codex review, v1 trial item 5, finding 1): this comment already said "a count",
    // but the check below it only ever read `pendingRef` -- a real `Ctrl+e` arrives as the bare
    // `Control` keydown FIRST (`ctrlKey: true` already set on that very event) and then `e`, so
    // "5 Ctrl+e" reached the unconditional `countRef.current = null` below on `Control`'s own
    // keydown (`pendingRef` is null with no `g`/`z`/`[`/`]` prefix armed) and `e` read the count back
    // as gone, scrolling once instead of five times.
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
    // claimed and does nothing -- not the button, not the leader. Claude Code's permission prompt
    // confirms with Enter; HTML's Space-activates is the default this overrides, only where a
    // button answers a permission.
    // K02 (ruling R3): Enter on those three is claimed too, and never activates them natively: this
    // handler presses the button itself, `TYPING_GUARD_MS` later and only after a landing on it (the
    // block below). The route the owner's regression needed stays open -- `l`/`h`/Tab/HINT onto
    // Approve, then Enter -- only deferred, as `a`/`d` are.
    // Every other button (Stop, Dismiss, a tab, the chooser's rows) keeps both natively. A Space
    // that arrives inside a pending leader sequence is the sequence's (spec §12, R25), so it falls
    // through to the engine below -- claimed here first either way, so it never reaches the button.
    // (`onAnswerButton` is computed at the top, ahead of the two modal overlays' early returns.)
    // K01: a pending prefix owns the next key, so Enter/Space on a focused control never activates
    // it; a pending count refuses a card's own buttons only.
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ") && (pending !== null || (count !== null && onAnswerButton))) {
      event.preventDefault();
      if (pending === null) showFlash(COUNT_ANSWER_FLASH);
      return;
    }
    if (onAnswerButton && event.key === " ") {
      event.preventDefault();
      if (seqRef.current === null) return;
    }
    // K02 (kbux 2026-09-29: `:ls⏎` approved `rm -rf important`; ruling R3): a card's own button
    // decides only the way `a`/`d` do. Enter is claimed first, so no path below leaves the native
    // activation in place, and the button is pressed through `waitThenAnswer` -- `TYPING_GUARD_MS`
    // later, any key in between cancelling it -- only with all three of: S1's before-half
    // (`mayAnswerNow`, whose walk exception keeps a quick `l⏎`); no modifier at all (Shift included,
    // which `isPlainAnswerKey` lets through for `D`); and a landing, focus put on this very button by
    // the key right before this one (`placedRef`: an `h`/`l` that moved it, a Tab, or a HINT with no
    // key since). So `l`, `s`, Enter and `l`, `g`, `h`, Enter (a cancelled pair moves nothing) answer
    // nothing however slowly they are typed. Fix round 3 (review): and only on the card `a`/`d` would
    // answer from the cursor (`permissionTarget`, S4) -- a Tab walks on from one card's buttons into
    // the next card's, and `l`/`h` then walk that card's (`currentStop` follows focus), while the row
    // cursor stays put, so a landing alone can sit on a card the cursor is not on.
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
    // Owner decision #26 (K15): any key within `TYPING_GUARD_MS` of a BROWSE `y` is typing -- vim's
    // `y i w` copied the row, opened INPUT on the `i` and typed the `w`. The copy is frozen and already
    // happened; what follows it inside the burst is swallowed and says how to type. A bare modifier is
    // not a key (the Shift of a capital), and a key an input method composes is not this panel's.
    // Fix round (Codex review, finding 2): this sits AHEAD of the focused-control early return just
    // below, so `y` then Enter (or Space) on a focused button -- another tab's, Dismiss -- within the
    // burst is claimed too instead of activating it; a lone Enter after a pause is left to the button.
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
    // The leader engine (`./leader`, panel round 2 plan Task 8; spec §2.4): a pending sequence owns
    // the next key, and only BROWSE ever starts one. An IME key is never a sequence key (Review
    // Focus 2, C4) -- checked here rather than folded into `startSequence`/`advanceSequence`
    // themselves, since neither of those takes an event to read `isComposing` off. A `{kind:"none"}`
    // start falls through to `resolveKey` below, never the other way; `startSequence` itself leaves
    // `g`/`z`/`[`/`]` to `resolveKey`'s own reserved two-key prefixes (`./leader`'s own doc comment).
    if (mode === "browse" && !isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode })) {
      if (seqRef.current !== null) {
        // v1 hardening, codex-release-p1 #6 (R25): a bare modifier's own keydown (the Control of a
        // Ctrl+c combo) is not "the next key" -- it must not cancel a pending sequence, the same way
        // `pendingRef`'s own `g`/`z`/`[`/`]` prefix is guarded just below and `TypingGuard.onKey`
        // ignores it. Without this, the bare `Control` cancelled the sequence and the `c` that
        // followed fell through to `resolveKey`'s Ctrl+c interrupt arm instead of being swallowed.
        if (isModifierKey(event.key)) return;
        event.preventDefault();
        // Fix round 1 (reviewer finding, codex-release-p1 #3): `advanceSequence` compares `event.key`
        // alone, so a real Ctrl/Alt chord (Ctrl+d after the bare Control above left the sequence
        // armed) reached it as plain "d" and could complete a binding no vim mapping would ever match
        // -- `<leader>bd` for Ctrl+d, not the bare `d` the table actually names. `startSequence`
        // already refuses to START a sequence on a Ctrl/Alt chord (just below); a chord arriving as a
        // CONTINUATION gets the same refusal, R25's "unbound key after the leader is swallowed" (a
        // chord matches no table key, modifier included, so it is unbound here by definition).
        applySeqStep(
          event.ctrlKey || event.altKey ? { kind: "cancel" } : advanceSequence(panelTable, seqRef.current.typed, event.key),
        );
        return;
      }
      // K01: a prefix's second key never starts a sequence -- `g` then Space is a cancelled `g`
      // (`resolveKey`), not the leader, and `g` then `L` is not `L`'s tab step.
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
          // v1 hardening R2-2: any OTHER single-key table binding that runs on this very first key
          // -- H/L's default tab.prev/tab.next, or an nvim-read binding on some other plain letter --
          // used to run at once ("Only the leader ... runs as before"), so "Looks good, now add
          // tests⏎" switched tabs on its own `L` and sent the rest into another session's composer.
          // `mayActAfterMotion` cannot help here (the review: "L is the first key" -- there is never
          // a motion run behind it), so this defers the same way `a`/`d` do: it runs
          // `TYPING_GUARD_MS` later, only if nothing else was typed meanwhile and the panel is still
          // in BROWSE (typingGuard.ts's own doc comment).
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
    // Owner decision #39: INPUT's Ctrl+y. Handled here, ahead of the unconditional `preventDefault`
    // further down, because with no card waiting the key stays the box's.
    if (action.kind === "approve-oldest") {
      approveOldestByKey(event, typedAt, afterKill);
      return;
    }
    // R4: a digit accumulates into the count the NEXT `j`/`k`/`[[`/`]]` repeats -- read back out of
    // `countRef` (as `count`, above) by that key, once it arrives. Handled first, and returns at
    // once, so a bare digit never falls into the scroll-announcing or row-motion code below it.
    // Capped by `accumulateMotionCount` (v1 audit R4) on every digit, not only at the end, so a long
    // run of digits never accumulates past `MAX_MOTION_COUNT` even transiently.
    if (action.kind === "count") {
      event.preventDefault();
      countRef.current = accumulateMotionCount(count, action.digit);
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
    // re-arms it); `h`/`l` can focus a control that scrolls itself into view, either way. `Ctrl+e`/
    // `Ctrl+y` (v1 trial item 5) follow the identical `Ctrl+d`/`Ctrl+u` rule: `Ctrl+y` stops
    // following outright, `Ctrl+e` only re-arms it if the scroll actually reaches the bottom. A page
    // (`Ctrl+f`/`PageDown`/`PageUp`, v1 picks Task 5) follows the same rule, and so do the arrow keys,
    // which are `move`.
    const messageList = root?.querySelector<HTMLElement>(".message-list") ?? null;
    if (
      action.kind === "move" ||
      action.kind === "half-page" ||
      action.kind === "scroll-line" ||
      action.kind === "page"
    ) {
      noteUserScroll(messageList, action.delta > 0 ? "down" : "up");
    } else if (action.kind === "jump") {
      // R2: a counted jump goes to row N, so its direction is where that row sits against the cursor;
      // a bare `gg`/`G` names its end, wherever the cursor is.
      const up = count === null ? action.to === "first" : jumpTarget(action.to, count, timeline.length) < cursor;
      noteUserScroll(messageList, up ? "up" : "down");
    } else if (action.kind === "control") {
      noteUserScroll(messageList, "unknown");
    }
    // A press that arrives while the last one is still easing starts from where that one was going,
    // never from a frame in between: every decision below reads the list's geometry.
    if (messageList !== null) settleListScroll(messageList);
    // A code block HINT landed on is "the item" for exactly the next `y`; any other key moves on.
    const landedCode = copyCodeRef.current;
    markLandedCode(null);
    // The row the cursor is on, found by structure (`conversationRows`), never as the first
    // `.row-current` or the `cursor`-th `[data-nav-stop="row"]` in the subtree: a model reply's own
    // HTML can carry either (v1 hardening, ruling R2). Read only by the keys that need it.
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
        // `j`/`k` are row motions. If a control inside the row had the keys (after `l` onto a card's
        // Approve), scrolling the row could carry that control off screen while it still answered
        // Enter -- found by the scrolling change's own fix round and left open there. So a scroll
        // step hands the keys back to the row first, the same place `j`/`k` leave them after a move.
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
        // R4's count repeats the step `times` times, each from the row the PREVIOUS step landed on
        // -- not `times` cells in one leap, so a stop with no row (a banner, the status line) still
        // ends the walk exactly where a single `j`/`k` onto it would. v1 audit R4 (P2-A4): the walk
        // is `countedStop`, which reads the tree once whatever `times` is (up to `MAX_MOTION_COUNT`)
        // and stops at a boundary, where a step makes no progress (`clampStep` clamps rather than
        // returning `null` -- the C1c comment below). Its own doc has the measurements.
        const landing = countedStop(root, cursor, action.delta, times - spentInRow);
        if (landing === null) return;
        const target = landing.stop;
        // C1c (spec §3.4): `j` that cannot move -- `nextStop` clamps rather than returning `null` at
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
          // K02 (ruling R3): a landing, recorded only where this key actually moved focus onto the
          // control -- a clamped `l` on the last one moves nothing and lands nothing.
          const moved = document.activeElement !== target;
          target.focus();
          if (moved && document.activeElement === target) placedRef.current = { el: target, atKey: typingGuard.keyCount() };
        }
      } else {
        // `a`/`d` answer the card by its id (ruling R2, `answerCardByKey`), through the same
        // `answerPermission` its buttons use, so one guard against a second answer covers both.
        // Not from a banner's button: the row cursor is hollow there, and not what keys act on,
        // so there is no card for it (F13's flash). v1 S4 (spec §2.2): only the cursor's card or the
        // card gating its tool call (`permissionTarget`) -- ruling 26's "the only card, from any row"
        // is gone.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        answerCardByKey(event, typedAt, target, action.decision);
      }
      return;
    }
    event.preventDefault();
    switch (action.kind) {
      case "mode":
        // Owner decision #26 (K12, K13): `i`/`o`/`A` enter INPUT only on a key that stands alone
        // (`mayStartInput`: a pure pause, and -- unlike the leader's `mayActAfterMotion` -- no motion
        // exception, the review of #26: "look at" walked with `l` and opened INPUT with `o`). In the
        // middle of typed prose the `i` of "explain" or the `o` of "follow-up" is swallowed and says how
        // to type, instead of opening the composer halfway through the word. A lone `i` after a pause is
        // unchanged; a fast `ji` chord now needs a pause.
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
      case "fold": {
        // v1 picks, Task 4. `zo` opens this row's own fold: a tool's result, or a collapsed run's calls
        // (the same `expanded[key]` Enter flips). vim's `zc` closes the INNERMOST open fold: this row's
        // own result if it is open, else the run it was unfolded from -- `buildDisplay` emits an expanded
        // run's calls under their own `t-<seq>` keys, so the run is found by asking the same timeline
        // folded with nothing expanded (`runKeyOf`, null in the detailed view and for a row in no run,
        // where `zc` closes nothing). Both SET `expanded[...]` where Enter flips it, so each is
        // idempotent, as vim's are: `zo` on an open fold and `zc` on a closed one change nothing.
        const key = timeline[cursor]?.key;
        if (key === undefined) break;
        const run =
          action.open || expanded[key]
            ? null
            : runKeyOf(buildTimeline(state), { expanded, detailed, turnRunning: state.activeTurnId !== null }, key);
        setExpanded((prev) => ({ ...prev, [run ?? key]: action.open }));
        break;
      }
      case "detailed":
        // R3: `Ctrl+o`. A per-tab toggle, mirrored to Rust in `saveView`/restored on a switch --
        // see `detailed`'s own doc comment.
        setDetailed((d) => !d);
        break;
      case "table-scroll": {
        // T1: `zh`/`zl` scroll the CURRENT row's own table, never the conversation.
        const table = cursorRow()?.querySelector<HTMLElement>(".table-scroll") ?? null;
        if (table !== null) table.scrollLeft += action.delta * TOOL_RESULT_SCROLL_STEP_PX;
        break;
      }
      case "caret":
        // D2 (3a): `v` from BROWSE starts CARET. `enterRegion` itself decides whether there is
        // anywhere to start (a row under the cursor, not a banner/Stop) and flashes when there is
        // not. `landedCode` is the same ref-read the `"copy"` case just below uses -- fix round 1,
        // see `enterRegion`'s own doc comment: `copyCodeRef.current` was already cleared above by
        // the time this ran.
        enterRegion(false, landedCode);
        break;
      case "visual":
        // D1 (O8's kept default): `V` from BROWSE still starts V-LINE directly.
        enterRegion(action.line, landedCode);
        break;
      case "copy": {
        // Owner decision #26 (K15): a `y` opens a burst in which every other key is typing (above).
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
        // v1 S4 and S1 (spec §2.1-§2.2): the same card `a`/`d` would answer, and the same wait.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        // The card's row by structure (ruling R2): `conversationRows(root)[target]` is timeline item
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
        // `r` on an ended tab resets it to `NotStarted` IN PLACE (ruling 12): the tab keeps its
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
        // v1 hardening R2-1: `f` is always the first key of whatever typed it ("fix the dashboard
        // layout⏎" started a HINT on its own `f`), so `mayActAfterMotion`'s backward-only check
        // cannot guard it -- this defers the same way `a`/`d` do (typingGuard.ts's own doc comment):
        // it asks `shell` for a HINT `TYPING_GUARD_MS` later, only if nothing else was typed
        // meanwhile and the panel is still in BROWSE. `shell` owns the HINT session once it starts:
        // it collects targets across the whole window and asks this panel for its own with
        // `hint_collect`. Until the HINT ends, this panel's keys are swallowed (`hintPendingRef`);
        // nothing else changes here.
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
        // R2: a count survives its prefix (`3gg`, `2gt`), so the second key reads it -- it was read
        // and reset at the top of this function, like the prefix itself.
        countRef.current = count;
        // The box shows what any of the five -- `g`/`z`/`[`/`]`/`Ctrl+w` -- can start once it has waited
        // `WHICH_KEY_DELAY_MS` with nothing completing it (panel round 2 plan, Task 8; spec §2.3
        // widened to §2.4's own delay, replacing the strip's former `g`-only 400ms line).
        // `clearSequence` at the top of this function on every later key -- the second `g` of `gg`
        // included -- is what cancels it before it fires.
        scheduleBoxTimer();
        break;
      case "panel": {
        // The second key of a reserved two-key prefix (`[b`) that also completes a panel-table
        // sequence (`resolveKey`'s own `ctx.pending` branch, Task 7) -- everything else a table
        // sequence can run reaches here through the leader engine above instead.
        // R2: a count before a `g` tab pair is vim's `{N}gt`/`{N}gT` (`:help gt`); every other pair
        // ignores it. `select_tab` names the tab's id, which is not the number the bar shows.
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
        // K01: swallowed (`preventDefault` above), nothing runs. A key that merely ended a prefix
        // says nothing, as vim's `clearopbeep` only beeps; a count before a card answer says why.
        if (action.why === "count-on-answer") showFlash(COUNT_ANSWER_FLASH);
        break;
      case "pane":
        // v1 picks, Task 6 (R11): `Ctrl+w h/j/k/l`, the keys to the module that way. shell runs the move
        // `Ctrl+h/j/k/l` make from this panel (`pane_nav` -> `move_focus`); the panel changes nothing
        // itself, and a side with no module leaves the keys where they are. The prefix was spent at the
        // top of this handler, and a count typed before it means nothing to a move (R2: a table pair
        // ignores it).
        postToRust({ type: "pane_nav", request_id: nextRequestId(), direction: action.direction });
        break;
      case "half-page": {
        // Half the visible height, as in vim. Then, if the cursor's row is not on screen, the
        // cursor comes to the visible row NEAREST to it -- `clampCursorToView` (R1's own scrolloff
        // rule, shared with the scroll-triggered rehome effect above and, since v1 trial item 5,
        // with `scroll-line` just below). The view stays exactly where the scroll put it ("keep"):
        // re-revealing that row with `nearest` would pull the view back by up to a row.
        //
        // A re-home also takes DOM focus back to the root, as `move` and `jump` do. Found in
        // review: without it, a control that had the keys (Approve after `l`, a banner's Dismiss,
        // Stop) KEPT them while the cursor was drawn on another row, so `Enter` activated an
        // Approve scrolled out of sight and `j`/`k` steered from the old stop. `clampCursorToView`
        // returns `null` both when the row is still (partly) visible and when nothing on screen at
        // all -- the two cases the original inline version also left focus untouched for, so moving
        // to this shared helper does not change either.
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
        // v1 trial item 5 (owner: "能不能给browse 加上contrl e/y", copying vim's own `:help
        // CTRL-E`/`:help CTRL-Y`): one text line, not half a view -- and, unlike `half-page` just
        // above, counted (`times`, R4's own count and cap). Box-first, the way `j`/`k` are
        // (`stepWithinRow`): a long tool result's own capped view takes as many of the count's
        // units as it can make progress on -- using ITS OWN step, `TOOL_RESULT_SCROLL_STEP_PX`, not
        // this action's one-line step ("the way j/k do it" is reusing the box's own scroller, not
        // inventing a line-based one for it) -- before the rest fall through to the conversation
        // itself, one line each. Re-homes the cursor through the identical rule `half-page` reuses
        // above.
        //
        // Whole-branch review finding 3: the box takes a unit only while it is at least partly on
        // screen (`scrollVisibleRowBox`), never through `j`/`k`'s bring-the-row-back step.
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list === null || root === null) break;
        const row = cursorRow();
        let remaining = times;
        while (remaining > 0 && scrollVisibleRowBox(row, action.delta)) remaining--;
        if (remaining > 0) {
          list.scrollTop += action.delta * computedLineHeight(row !== null ? rowTextElement(row) : list) * remaining;
        }
        // Fix round 1 (Codex review, v1 trial item 5, finding 3): `noteUserScroll` above only ARMS
        // `MessageList`'s steering window -- the actual re-arm needs a real `scroll` event, which a
        // press absorbed entirely by the row's own box (the `while` loop above) never produces on
        // THIS list (`.tool-result-body` is a different scrollable element, and `scroll` does not
        // bubble). At the true bottom already -- exactly what a box-only `Ctrl+y` then `Ctrl+e`
        // leaves behind -- following stayed off for good even though the box round-tripped back to
        // where it started. A synthetic `scroll` event costs nothing when a real one is coming too
        // (`MessageList`'s `onScroll` only reads the list's CURRENT position) and lets `Ctrl+e`
        // re-arm by the same "did this reach the bottom" rule `Ctrl+d`/`G` already use, box-absorbed
        // or not. `Ctrl+y` (`action.delta < 0`) must not do this: it stops following outright, before
        // its own scroll, by design (the comment above the `noteUserScroll` call), and re-arming it
        // here would undo that the moment a `Ctrl+y` happened to leave the list at the bottom too.
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
        // v1 picks, Task 5 (decision #13; vim `:help CTRL-F`): a whole view, two text lines of the old
        // one kept on screen, `times` of them (R4's count and cap). The two lines are the reader's:
        // the cursor row's own text (`rowTextElement`), never `.message-list` itself, which sets no
        // line height in `index.css` and so reads `normal` -- under the prose line, the very finding
        // `scroll-line` above records. Like `half-page`, the list alone scrolls (a long tool result's
        // own box is not this key's), and a cursor whose row the page carried out of sight goes to the
        // nearest visible row, the view staying where the page put it ("keep"), the keys handed back
        // to the row so a control that had them cannot answer Enter for a row nobody sees.
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
        // `G` goes to the very END of the list, not merely to the last row's top: on a long last
        // reply, that is the line you want. `gg` goes to the very top.
        // R2 (v1 picks): with a count they go to row N instead (`{N}G`, `{N}gg`; `jumpTarget` clamps
        // it). That row is brought on screen the way a `j`/`k` landing brings one -- through the
        // cursor effect, which shows a tall row's near edge -- so the list is not scrolled to either
        // end. The landing is set only for a cursor that really moves: an unchanged cursor runs no
        // effect, and a value left behind would misplace some later, unrelated move.
        const target = jumpTarget(action.to, count, timeline.length);
        if (target !== cursor) landingRef.current = count === null ? "keep" : target > cursor ? 1 : -1;
        setCursor(target);
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list !== null && count === null) list.scrollTop = action.to === "first" ? 0 : list.scrollHeight;
        root?.focus({ preventScroll: true });
        break;
      }
      case "scroll-row": {
        // v1 picks, Task 4 (vim `:help zt`/`zz`/`zb`): the ROW goes to an edge of the view -- its top,
        // its middle or its bottom, whatever its height -- never a text line. With a count it is row N
        // (the row `{N}G` goes to: `jumpTarget`, 1-based, clamped to the rows there are) and the cursor
        // goes there too. The scroll is this key's own, so it is said on the list first, like every
        // other (`noteUserScroll`): a view that moves up stops following at once. `"keep"` keeps the
        // reveal effect from re-aligning the row this has just placed, and `scrollTop` clamps at the
        // list's ends as a browser's does -- the last row's top cannot reach the top of a short list.
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
      case "card-jump": {
        // v1 picks, Task 7 (R7): `]p`/`[p` move the CURSOR to the next/previous card waiting for an answer,
        // wrapping as nvim's `]d` does, `times` cards on -- and do nothing else. No `answerPermission`, no
        // `waitThenAnswer`, no button pressed: a lone `a`/`d` afterwards answers the card this landed on
        // (S4, `permissionTarget`) and an `a` typed hard on the heels of the jump is refused like any other
        // (S1). A card waits when this panel has not answered it and its session lives -- a dead session's
        // cards stay drawn but inert, and `a` on one does nothing -- the test `answerCardByKey`'s
        // `stillThere` makes at fire time, made here so the cursor never lands on a card that cannot answer.
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
        // D1/N1/D5: `Ctrl+c` while a turn runs, from anywhere in BROWSE -- the same `interrupt()`
        // the activity line's own Stop button calls.
        interrupt();
        break;
      case "search":
        // K02 fix round 1: the `/` prompt and the `:` line are one command line, as vim's are, so
        // opening either closes the other. Two boxes drawn, one without the keys, was a state a click
        // back on the conversation could reach, and `takeKeys` then focused the `/` one, newer or not.
        setExLine(null);
        setSearch({ query: "", origin: cursor });
        setLineFocusRequest((n) => n + 1);
        break;
      case "ex-line":
        // K02 (ruling R4): the box takes the keys (`SearchBar` focuses itself), runs nothing. Fix
        // round 2: a line already open -- the keys elsewhere, since a key typed in it is its text --
        // is wiped and handed them again, as vim's `:` gives a fresh command line that has them.
        setSearch(null);
        setExLine("");
        setLineFocusRequest((n) => n + 1);
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
      case "open-link": {
        // R6 (v1 picks, Task 8): the web link(s) of the row under the cursor, read off the DOM as the
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
        {!remedyNamesR(sessionEndedProblem) && <div className="row-hint">Press r to start a new session here.</div>}
      </Row>
    ) : state.status.kind === "closed" ? (
      // Deliberately NOT `row-error`/`✗`: an ordinary close (the host closed it, the provider
      // exited cleanly) is not an error, and styling it like the lost-session row above would
      // train the eye to ignore the one that matters. See `.row-ended` in index.css for the
      // fuller record of why these two were briefly unified and then split back apart.
      <Row kind="ended" sign="·" problem={sessionEndedProblem}>
        This session has ended ({state.status.reason}).
        {!remedyNamesR(sessionEndedProblem) && <div className="row-hint">Press r to start a new session here.</div>}
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
      // Real, focusable, so BROWSE's keydown handler has a DOM node to bubble from -- see the
      // effect above `onKeyDown` that keeps focus here whenever `mode` is "browse".
      tabIndex={0}
      // A y/n prompt takes every key first, from wherever it was typed (the chooser and its inputs,
      // the tab bar's rename, the `/` prompt, the composer): see `answerConfirm`'s own doc. VISUAL's
      // own whole key table (D13) runs right after it, in the same capture-phase handler, ahead of
      // `onKeyDown` and of every descendant's own keydown -- a card's reason box denies from its own
      // handler before `onKeyDown` ever runs (spec §7, finding 1), so VISUAL has to beat it here.
      onKeyDownCapture={(event) => {
        if (answerConfirm(event)) return;
        onVisualKeyDownCapture(event);
      }}
      onKeyDown={onKeyDown}
      // D12/D13: a pointer press anywhere in the panel is the user's explicit act and ends VISUAL
      // before the press's own handling runs (a click on Approve still approves; a click into a
      // card's reason box shows BROWSE at once, and the Enter that follows it then denies as the box
      // says, spec §7 finding 1's own accepted reading). Capture phase, ahead of every descendant's
      // own `onClick`/`onMouseDown`.
      onPointerDownCapture={() => {
        exitRegion();
        arrivalParkRef.current = null; // #22: a press in the panel is a place chosen by hand
      }}
      onFocus={(event) => {
        // K02 (ruling R3): the focus move a plain Tab just made is a landing, at that Tab's count --
        // consumed by the first focus event after it, so a focus nothing typed (an effect, a later
        // envelope) is never one. Fix round 3 (review): and only while that Tab's default is still
        // alive, read now that every handler has run -- a Tab one of them claimed moved nothing.
        const lastTab = tabAtKeyRef.current;
        if (lastTab !== null && lastTab.atKey === typingGuard.keyCount() && !lastTab.event.defaultPrevented) {
          placedRef.current = { el: event.target as HTMLElement, atKey: lastTab.atKey };
        }
        tabAtKeyRef.current = null;
        // The outermost stop (`stopOf`): a link inside a reply is in the reply's row, whatever the
        // reply's own HTML claims to be (v1 hardening, ruling R2).
        const stop = containerRef.current === null ? null : stopOf(containerRef.current, event.target as HTMLElement);
        setEdgeFocused(stop !== null && stop.getAttribute("data-nav-stop") !== "row");
        // D12: VISUAL ends, before anything else happens, when focus lands inside the panel on
        // anything but its own root -- a click into a card's reason box, a button, the composer, the
        // tab bar (D13).
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
          `.agent-ui-scroller` in index.css). The tab bar (Task 11) goes above `errorBanner`. */}
      <div className="agent-ui-scroller">
        {/* D11: while VISUAL/V-LINE is on, `MessageList` reads the props frozen at entry --
            everything but `cursor`/`focused`/`visual` itself -- so the conversation it is built
            from does not change under a selection built directly on its DOM. `frozenTimeline` is
            the SAME pure computation over those frozen props, used only to translate a DOM row back
            into a live timeline key (`rowKeyOfCaret`/`landCursorOnRowKey` above); the `cursor` INDEX
            handed to `MessageList` here is the live cursor re-expressed against the FROZEN
            timeline's own indices, since that is the timeline `MessageList` itself will recompute
            internally from these same frozen props. A live row the frozen list does not have (a
            prompt sent at a turn's end moved the cursor onto it) is -1, no current row at all: fix
            round 3 (review finding D11) -- the old fallback, the live index itself, marked whichever
            unrelated frozen row sat at that position as the cursor's. */}
        <MessageList
          state={frozenSnapshot?.state ?? state}
          sessionEnded={frozenSnapshot?.sessionEnded ?? sessionEnded}
          expanded={frozenSnapshot?.expanded ?? expanded}
          cursor={frozenTimeline === null ? cursor : (indexOfKey(frozenTimeline, timeline[cursor]?.key ?? "") ?? -1)}
          detailed={frozenSnapshot?.detailed ?? detailed}
          focused={paneFocused && !edgeFocused}
          codeLanded={codeLanded}
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
            setUnread({ label, jump });
            unseenAfterSeqRef.current = afterSeq;
          }}
          unseenSeed={unseenSeed}
          visual={mode === "caret" ? "caret" : mode === "visual" ? "visual" : mode === "vline" ? "vline" : null}
        />
        {keymapOpen && (
          <PanelErrorBoundary name="keys overlay" onError={() => overlayFailed("keys overlay", () => setKeymapOpen(false))}>
            <KeymapOverlay
              ref={keymapOverlayRef}
              onClose={() => setKeymapOpen(false)}
              windowKeys={keymapHelp.window}
              prefixKeys={keymapHelp.prefixKeys}
              prefixLabel={keymapHelp.prefix}
              panel={panelTable}
              tmuxSkipped={keymapHelp.tmuxSkipped}
              companion={editorLink !== null}
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
        {/* Owner trial item 2 (2026-09-28): a bare /model or /effort reply opens this, positioned
            inside `.agent-ui-scroller` the same way the chooser just above is. */}
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
        // #39 fix round 1 (Codex): the `Ctrl+r` search's Enter/Tab/Escape reach the typing guard.
        onLineKey={noteLineKey}
      />
      {/* N2/R4: `gf` with several paths, and the open `/` prompt, each still own every key
          (`onKeyDown`'s dedicated branches, above `resolveKey` entirely) -- only WHERE they draw
          moved, once `Footer`'s third slot stopped existing (panel round 2 plan, Task 10). */}
      {/* rc.3 minors (K03's rule, "the overlay closes with a flash", for the four this row of overlays
          left out): each sits in its own `PanelErrorBoundary`, so a throw while it renders closes just
          it and says so in the band rather than unmounting the panel. The two pickers hold no focus
          (the root keeps the keys and reads the letter), so closing them is all it takes; the two
          lines are inputs that DO hold it, so they hand the keys back the way their own Esc does. */}
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
      {/* K02 (ruling R4): the `:` command line, where vim draws it. It runs nothing -- Enter says
          so, Esc (and `Ctrl+[`, R9) closes it silently -- and exists so `:ls⏎`, `:l⏎` and `:d⏎`
          land here, never on a card. */}
      {exLine !== null && (
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
              const command = parseTrustCommand(exLine);
              if (command !== null) {
                const commandId = nextRequestId();
                inFlight.current.set(commandId, { kind: "trust", tab: activeTabRef.current ?? 0 });
                postToRust({ type: "trust_command", request_id: commandId, action: command });
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
          // `true` for `cycleOffered`: ignored entirely in short form (`modePill`'s own doc
          // comment) -- v1 removed the wave-5 capability this used to read.
          pill: modePill(activeTab?.mode ?? "auto", true, true),
          showcmd: box !== null ? `${box.title}…` : null,
          message: flash?.text ?? null,
          prompt: confirmBandPrompt,
          warn: statusWarning(state.provider, activeTab?.failure ?? null),
          unread: unread.label,
          cards: state.pendingPermissions.length,
          queued: queue.length,
          context: sessionEnded ? null : contextFact(editorContext),
          // About the window, not the session: an ended session still has an editor beside it.
          link: editorLink,
          position: timeline.length === 0 ? null : `${cursor + 1}/${timeline.length}`,
          model: shortModel(state.model),
          // R5: this tab's last reported usage, right of the model; nothing until one arrives.
          usage: usageSegment(state.usage),
          // How the latest turn ended when it did not complete, until the next turn starts.
          ending: latestTurnEnding(state),
          // Owner decision #39: the card INPUT's Ctrl+y would approve, while the box has the keys.
          approve: ctrlYCard,
          // A finished turn of this tab changed files on disk; `c` shows them.
          review: activeTab !== null && reviewHints[activeTab.id] !== undefined ? { files: reviewHints[activeTab.id].files } : null,
          // An interrupted revert is named whatever `review.hint` says: a half-written file is not a diff notice.
          recovery: reviewRecovery.length > 0 ? { path: reviewRecovery[0].path, more: reviewRecovery.length - 1 } : null,
        }}
        paneFocused={paneFocused}
        onOpenDetail={() => post({ type: "open_detail" })}
        onJump={unread.jump}
      />
      <HintLayer root={containerRef.current} hints={hints} typed={hintTyped} />
    </div>
    </ProjectDirContext.Provider>
  );
}
