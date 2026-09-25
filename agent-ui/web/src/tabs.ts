import type { PanelMode } from "./keymap";
import type { PermissionModeChoice, TabId, TabInfo, TabMarker, TabsEnvelope } from "./types";

/** Envelopes about ONE tab (session tabs spec §3.1). Everything else is about the window. */
const TAB_SCOPED = new Set(["events", "snapshot", "handoff", "error", "focus_permission", "tab_detail", "confirm_close", "begin_rename"]);

/** The panel keeps only the active tab's state, so an envelope for another tab is dropped. That
 *  closes the race between a switch and a batch already in flight (spec §3.1). `tabs` always
 *  arrives before the new tab's snapshot. */
export function acceptsEnvelope(payload: { kind: string; tab?: number }, activeTab: TabId | null): boolean {
  if (!TAB_SCOPED.has(payload.kind)) return true;
  return activeTab !== null && payload.tab === activeTab;
}

/** Claude Code's mode pill (docs, permission-modes.md), with neovibe's own word for the mode
 *  (`hello.permissionModes`): neovibe's `auto` is not the CLI's. Before a start it says how to cycle. */
export function modePill(mode: PermissionModeChoice, started: boolean): string {
  return started ? `⏵⏵ ${mode} on` : `⏵⏵ ${mode} on (shift+tab to cycle)`;
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
 *  as today. `draft` is the unsent composer text (ruling 24); phase 3 moves it, with the queue, to Rust. */
export type TabViewState = {
  cursor: number;
  mode: PanelMode;
  expanded: Record<string, boolean>;
  scrollTop: number;
  atBottom: boolean;
  draft: string;
};

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
