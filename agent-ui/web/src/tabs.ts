import type { PanelMode } from "./keymap";
import type { PermissionModeChoice, TabId, TabInfo, TabMarker, TabsEnvelope } from "./types";

/** Envelopes about ONE tab (session tabs spec §3.1). Everything else is about the window.
 *
 *  Phase 3: the queue, the draft mirror, the taken-back queue, the rule offers and the scratch
 *  state are each a tab's own (ruling 1, 6, 7, 16, plan ruling 18) -- `history`, `editor_context`
 *  and `notice` are window-scoped and stay out of this set. */
const TAB_SCOPED = new Set([
  "events", "snapshot", "handoff", "error", "focus_permission", "tab_detail", "confirm_close", "begin_rename",
  "queue", "draft", "queue_taken", "rule_offers", "scratch",
]);

/** The panel keeps only the active tab's state, so an envelope for another tab is dropped. That
 *  closes the race between a switch and a batch already in flight (spec §3.1). `tabs` always
 *  arrives before the new tab's snapshot. */
export function acceptsEnvelope(payload: { kind: string; tab?: number }, activeTab: TabId | null): boolean {
  if (!TAB_SCOPED.has(payload.kind)) return true;
  return activeTab !== null && payload.tab === activeTab;
}

/** Claude Code's mode pill (docs, permission-modes.md), with neovibe's own word for the mode
 *  (`hello.permissionModes`): neovibe's `auto` is not the CLI's. `cycleOffered` names D6's own
 *  capability gate (`state.capabilities.modeSwitch`) before a start it is always offered
 *  (phase 2's own behaviour); once a session is live it is offered only when Verdandi can actually
 *  change a live session's mode (D6/wave 5's `set_permission_mode`).
 *
 *  `short` (panel round 2 plan, Task 10; spec §5.2): the bottom band has no room for "on" or the
 *  cycle hint, so it always reads `short = true` and gets the bare `⏵⏵ <mode>` -- `cycleOffered` is
 *  ignored entirely in that case, since there is nowhere left to draw the hint anyway. Backend and
 *  model, and the full pill with its hint, moved to `prefix i` (decision 6); nothing left calls this
 *  with `short` omitted, but the parameter defaults to `false` rather than being required, so a
 *  future caller that wants the long form back does not have to relearn what it looked like. */
export function modePill(mode: PermissionModeChoice, cycleOffered: boolean, short = false): string {
  if (short) return `⏵⏵ ${mode}`;
  return cycleOffered ? `⏵⏵ ${mode} on (shift+tab to cycle)` : `⏵⏵ ${mode} on`;
}

/** One glyph per marker (spec §3.2). `working` has none: `TurnActivity`'s motion draws it. */
export function markerGlyph(marker: TabMarker | null, pending: number): string {
  switch (marker) {
    case "needs_input":
      return pending > 1 ? `⚑${pending}` : "⚑";
    case "ended":
      return "✕";
    case "unread":
      return "•";
    default:
      return "";
  }
}

/** vim's `'showtabline'` = 1 (owner: "两个以上才显示"), and during an inline rename (ruling 6). */
export function showTabBar(tabCount: number, renaming: boolean): boolean {
  return tabCount >= 2 || renaming;
}

export function activeTabInfo(tabs: TabsEnvelope | null): TabInfo | null {
  if (tabs === null) return null;
  return tabs.tabs.find((t) => t.id === tabs.active) ?? null;
}

/** What a tab's reader had on screen: kept in the WebView per tab (spec §3.1). Lost on `prefix r`,
 *  as today. Phase 3 moves the unsent composer text, with the queue, to Rust (ruling 6) -- it is no
 *  longer part of this store. `detailed` is the detailed-view toggle (Ctrl+o, R3, ruling 20). */
export type TabViewState = {
  cursor: number;
  mode: PanelMode;
  expanded: Record<string, boolean>;
  scrollTop: number;
  atBottom: boolean;
  detailed: boolean;
  /** The `seq` threshold `MessageList`'s unread pill was counting from when the tab was left (wave 3,
   *  Task 3) -- `null` when the tab was left following (nothing parked) or never had one. Seeded back
   *  into `MessageList` as `unseenSeed` on restore, so a row that arrived while the tab was away is
   *  still counted: without this, a tab switch reuses `MessageList`, and its first `updatePill` after
   *  the restore would start counting fresh from the just-restored timeline's own length. */
  unseenAfterSeq: number | null;
};

/** How long after the last keystroke the composer mirrors its text to Rust (phase 3 ruling 6). */
export const DRAFT_MIRROR_DELAY_MS = 300;

export function saveView(store: Map<TabId, TabViewState>, tab: TabId, view: TabViewState): void {
  store.set(tab, view);
}

export function takeView(store: Map<TabId, TabViewState>, tab: TabId): TabViewState | undefined {
  return store.get(tab);
}

export function forgetClosed(store: Map<TabId, TabViewState>, open: TabId[]): void {
  for (const tab of Array.from(store.keys())) if (!open.includes(tab)) store.delete(tab);
}

/** `handoffRequests` without `tab`'s entry -- only when that entry is `requestId`, if one is named,
 *  so a late answer to an earlier handoff cannot end a later one on the same tab. Returns the same
 *  map when nothing changes, so React skips the render. */
export function withoutHandoff(
  requests: ReadonlyMap<TabId, string>,
  tab: TabId,
  requestId?: string,
): ReadonlyMap<TabId, string> {
  const current = requests.get(tab);
  if (current === undefined || (requestId !== undefined && current !== requestId)) return requests;
  const next = new Map(requests);
  next.delete(tab);
  return next;
}
