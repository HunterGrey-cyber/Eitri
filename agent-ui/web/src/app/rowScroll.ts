import { noteUserScroll, resumeFollowing } from "../follow";
import {
  cancelListScroll,
  computedLineHeight,
  decideLanding,
  decidePress,
  readBoxState,
  readRowGeometry,
  rowTextElement,
  scrollListTo,
  STEP_LINES,
} from "../jkScroll";
import { conversationRows } from "../nav";
import { allCards } from "../timeline";
import type { TabViewState } from "../tabs";
import type { TurnDurations } from "../turnTimes";
import type { AgentUiState, TabId } from "../types";
import { TOOL_RESULT_SCROLL_STEP_PX } from "./motion";

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

/** `Ctrl+e`/`Ctrl+y`'s box-first step: the cursor row's capped tool result takes
 *  the unit only while at least part of it is on screen and it can still move that way -- where a
 *  mouse wheel over it would scroll it too. Otherwise `false`, and the unit scrolls the conversation.
 *
 *  It deliberately does not share `j`/`k`'s helper, whose off-screen branch brings the cursor row back
 *  into view and claims the unit. That is right for a cursor MOTION and wrong for a view scroll --
 *  vim's CTRL-E/CTRL-Y never move the view toward the cursor. With the box just below the view, held
 *  `Ctrl+y` would jump back down to the row each time it had scrolled it off and never get past it,
 *  and `Ctrl+e` would jump a whole box height. */
export function scrollVisibleRowBox(row: HTMLElement | null, direction: 1 | -1): boolean {
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
export function rowScrollStep(row: HTMLElement): number {
  return STEP_LINES * computedLineHeight(rowTextElement(row));
}

/** The sheet owns its scroll box: line motions share the conversation's text-sized step, while page
 *  motions use the visible sheet height so they still fit after resizing the window. */
export function scrollKeymapOverlay(el: HTMLDivElement | null, key: string, ctrl: boolean) {
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
 *  `head`: a row taller than the view is entered at its head, from any direction -- a permission card that
 *  `focus_permission` lands on is read from its first line.
 *
 *  With no layout to judge -- a panel that is not laid out yet, or jsdom -- it falls back to
 *  `scrollIntoView`, which then does nothing a reader could see anyway. */
export function revealRow(list: HTMLElement | null, row: HTMLElement, direction: 1 | -1 | 0, animate = false, head = false) {
  const geometry = list === null ? null : readRowGeometry(list, row);
  if (list === null || geometry === null) {
    row.scrollIntoView({ block: "nearest" });
    return;
  }
  const landing = decideLanding({ direction, head, ...geometry });
  if (landing.how !== "stay") scrollListTo(list, landing.scrollTop, animate);
}

/** A tool result's box is entered at the end nearest the reader -- its start going down, its end going
 *  up -- so it never carries the position it was last read to. Called when `j`/`k` land on the row. */
export function enterBoxAtNearEnd(row: HTMLElement, direction: 1 | -1) {
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
export function stepWithinRow(row: HTMLElement | null, direction: 1 | -1, times: number, animate: boolean): number {
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

/** Vim's scrolloff rule: after a scroll the panel did not make by moving the cursor, a cursor
 *  whose row is entirely off screen goes to the nearest visible row. `null`: leave it. Reads
 *  geometry only; never writes `scrollTop`. */
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

/** Everything `MessageList` normally reads live, captured once when VISUAL
 *  starts and handed back unchanged for as long as it is on -- only `cursor`/`focused`/`visual`
 *  itself keep moving. `null` outside VISUAL/V-LINE. */
export type FrozenSnapshot = {
  state: AgentUiState;
  expanded: Record<string, boolean>;
  detailed: boolean;
  ruleOffers: Record<string, string>;
  answeredPermissions: ReadonlySet<string>;
  sessionEnded: boolean;
  turnDurations: TurnDurations;
};

/** The tool name of the OLDEST pending permission -- the lowest `seq`, the one the
 *  model has waited on longest -- for `ActivityLine`'s "needs approval" slot, or `null` when
 *  nothing waits. */
export function oldestPendingTool(state: AgentUiState): string | null {
  let oldest: (typeof state.pendingPermissions)[number] | null = null;
  for (const request of allCards(state)) {
    if (oldest === null || request.seq < oldest.seq) oldest = request;
  }
  return oldest?.toolName ?? null;
}

/** Where a reader was when the keys left the panel (`pane_focus` false), for the `arrive` that
 *  brings them back. One slot beside `viewStore`, never in it: an arrival must neither consume nor
 *  overwrite the view a tab switch saved, and a park dies with a switch (`tabs` arm). `following` is
 *  "at the bottom AND on the last row" -- in a conversation that fits the view `atBottom` alone is
 *  always true, and would send a reader who put the cursor on row 1 back to the last row. `atKey`:
 *  `typingGuard.keyCount()` at the leave, so any panel key since then makes the park stale. */
export type ArrivalPark = { tab: TabId; view: TabViewState; cursorKey: string | null; following: boolean; atKey: number };

/** The scroll half of putting a saved view back (a tab switch's restore and an arrival's): `follow`
 *  re-arms following outright, as a send does; otherwise the write is announced as the user's, so
 *  `MessageList` does not fight it, and the list goes back to where it was. */
export function applyViewScroll(list: HTMLElement, view: TabViewState, follow: boolean, stop = false) {
  // A restored view replaces whatever was easing, even one that began at the very position this writes.
  cancelListScroll(list);
  if (follow) {
    resumeFollowing(list);
    return;
  }
  // `stop`: a view that was not following must come back not following, whatever the
  // list decided while it was away -- a card resolved above the reader shrinks the list, the browser
  // clamps `scrollTop` to the new end, and `MessageList` reads that scroll as "at the bottom" and
  // re-arms following, so the next streamed row would snap the view (and, through the scroll clamp, the
  // cursor) away from the row just restored. "up" stops following outright, as `k`/`gg` do.
  noteUserScroll(list, stop ? "up" : "unknown");
  list.scrollTop = view.scrollTop;
}

