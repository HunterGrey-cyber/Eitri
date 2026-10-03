import type { PanelMode } from "./keymap";
import type { PermissionModeChoice, TabId, TabInfo, TabMarker, TabsEnvelope } from "./types";

/** Envelopes about ONE tab. Everything else is about the window.
 *
 *  Phase 3: the queue, the draft mirror, the taken-back queue, the rule offers and the scratch
 *  state are each a tab's own -- `history`, `editor_context`,
 *  `editor_link` and `notice` are window-scoped and stay out of this set. The review overlay's two replies
 *  are the active tab's, as are its draft and the send preview (`review_recovery` is the project's journal, so
 *  not); `review_hint` is deliberately not here -- it is kept for the tab it names while
 *  another is on screen. */
const TAB_SCOPED = new Set([
  "events", "snapshot", "handoff", "error", "focus_permission", "tab_detail", "confirm_close", "begin_rename",
  "queue", "draft", "queue_taken", "rule_offers", "scratch", "review", "review_diff",
  "review_draft", "review_send_preview", "trust_prompt",
]);

/** The panel keeps only the active tab's state, so an envelope for another tab is dropped. That
 *  closes the race between a switch and a batch already in flight. `tabs` always
 *  arrives before the new tab's snapshot. */
export function acceptsEnvelope(payload: { kind: string; tab?: number }, activeTab: TabId | null): boolean {
  if (!TAB_SCOPED.has(payload.kind)) return true;
  return activeTab !== null && payload.tab === activeTab;
}

/** Format the permission-mode pill with the panel's mode names from `hello.permissionModes`; its
 *  `auto` differs from the CLI mode. Every tab can attempt a mode change through `modeKeyRoute`.
 *  `cycleOffered` controls the hint in the long form. The bottom band uses `short = true`,
 *  omitting "on" and the hint to save room; backend, model and full mode details appear under
 *  `prefix i`. The default remains the long form. In the short form, bypass spells out "bypass
 *  permissions on" because it answers waiting permission cards automatically. */
export function modePill(mode: PermissionModeChoice, cycleOffered: boolean, short = false): string {
  if (short) return mode === "bypass" ? "⏵⏵ bypass permissions on" : `⏵⏵ ${mode}`;
  // O2 (a): the key toggles auto and bypass, frozen as such -- "toggle", not Claude Code's "cycle".
  return cycleOffered ? `⏵⏵ ${mode} on (shift+tab to toggle)` : `⏵⏵ ${mode} on`;
}

/** One glyph per marker. `working` has none: `TurnActivity`'s motion draws it. */
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

/** vim's `'showtabline'` = 1, and during an inline rename. */
export function showTabBar(tabCount: number, renaming: boolean): boolean {
  return tabCount >= 2 || renaming;
}

export function activeTabInfo(tabs: TabsEnvelope | null): TabInfo | null {
  if (tabs === null) return null;
  return tabs.tabs.find((t) => t.id === tabs.active) ?? null;
}

/** vim's `{N}gt` / `{N}gT` (`:help gt`): `tab.next` with a count is the tab numbered N -- the number
 *  the bar shows and `prefix N` selects -- and `tab.prev` goes N tabs back in the bar's order,
 *  wrapping. `null` when no tab has that number. */
export function countedTabTarget(tabs: TabsEnvelope, action: "tab.next" | "tab.prev", count: number): TabId | null {
  if (action === "tab.next") return tabs.tabs.find((t) => t.number === count)?.id ?? null;
  const at = tabs.tabs.findIndex((t) => t.id === tabs.active);
  if (at === -1) return null;
  const n = tabs.tabs.length;
  return tabs.tabs[(((at - count) % n) + n) % n].id;
}

/** What a tab's reader had on screen: kept in the WebView per tab. Lost on `prefix r`,
 *  as today. Phase 3 moves the unsent composer text, with the queue, to Rust -- it is no
 *  longer part of this store. `detailed` is the detailed-view toggle. */
export type TabViewState = {
  cursor: number;
  mode: PanelMode;
  expanded: Record<string, boolean>;
  scrollTop: number;
  atBottom: boolean;
  detailed: boolean;
  /** The unread pill's `seq` threshold when the tab was left, or `null` if following or never parked.
   *  Restore it as `MessageList`'s `unseenSeed` so rows arriving while the tab is away still count.
   *  Without it, the reused list would start counting from the restored timeline's current length. */
  unseenAfterSeq: number | null;
};

/** How long after the last keystroke the composer mirrors its text to Rust. */
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
