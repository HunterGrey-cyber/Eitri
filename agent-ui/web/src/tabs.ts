import type { PanelMode } from "./keymap";
import type { PermissionModeChoice, TabId, TabInfo, TabMarker, TabsEnvelope } from "./types";

/** Envelopes about ONE tab. Everything else is about the window.
 *
 *  The queue, the draft mirror, the taken-back queue, the rule offers and the scratch
 *  state are each a tab's own, since each belongs to one conversation -- `history`, `editor_context`,
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

/** Claude Code's mode pill (docs, permission-modes.md), with Eitri's own word for the mode
 *  (`hello.permissionModes`): Eitri's `auto` is not the CLI's. `cycleOffered` used to name the
 *  `SetPermissionMode` capability gate; that capability is gone entirely -- every tab can always attempt to cycle now (`modeKey.ts`'s
 *  `modeKeyRoute`) -- but the parameter stays for the long form's own shape, below.
 *
 *  `short`: the bottom band has no room for "on" or the
 *  cycle hint, so it always reads `short = true` and gets the bare `⏵⏵ <mode>` -- `cycleOffered` is
 *  ignored entirely in that case, since there is nowhere left to draw the hint anyway. Backend and
 *  model, and the full pill with its hint, moved to `prefix i`; nothing left calls this
 *  with `short` omitted, but the parameter defaults to `false` rather than being required, so a
 *  future caller that wants the long form back does not have to relearn what it looked like.
 *
 *  Bypass's short pill spells out what it means (`⏵⏵ bypass permissions on`) rather than the
 *  bare mode word every other mode gets -- Eitri's own answer, spent on every waiting card, is not
 *  something a glance at "bypass" alone says. */
export function modePill(mode: PermissionModeChoice, cycleOffered: boolean, short = false): string {
  if (short) return mode === "bypass" ? "⏵⏵ bypass permissions on" : `⏵⏵ ${mode}`;
  // The key cycles manual -> auto -> bypass, Claude Code's order and word.
  return cycleOffered ? `⏵⏵ ${mode} on (shift+tab to cycle)` : `⏵⏵ ${mode} on`;
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

/** vim's `'showtabline'` = 1 (the bar shows only with two or more tabs, since one tab has nothing to
 *  switch to), and during an inline rename, which needs the bar to type into. */
export function showTabBar(tabCount: number, renaming: boolean): boolean {
  return tabCount >= 2 || renaming;
}

/** Whether a tab runs an agent other than Claude (`TabInfo.agent`: `codex`, `acp:<name>`; absent is Claude's). */
export function isExternalAgent(agent: string | undefined): boolean {
  return agent !== undefined && agent !== "claude";
}

/** The word a non-Claude tab shows in its label and in what is said about it: `codex`, or the name an
 *  `acp:<name>` agent was registered under. `null` for a Claude tab. */
export function agentWord(agent: string | undefined): string | null {
  if (!isExternalAgent(agent)) return null;
  return agent!.startsWith("acp:") ? agent!.slice(4) : agent!;
}

/** The modes a tab of this agent offers, in `Shift+Tab`'s order. The handshake's list is the client's; an ACP agent
 *  sets its own mode once, when its session starts, so manual would promise a stricter agent than Eitri can make
 *  it and its tab cycles auto and bypass only (`AgentId::permission_modes` in `core/src/agent_backend.rs`, which
 *  this mirrors: a tab has no list of its own on the wire). */
export function tabPermissionModes(offered: PermissionModeChoice[], agent: string | undefined): PermissionModeChoice[] {
  const isAcp = agent !== undefined && agent.startsWith("acp:");
  return isAcp ? offered.filter((mode) => mode !== "manual") : offered;
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
 *  as today. The unsent composer text lives with the queue in Rust -- it is no
 *  longer part of this store. `detailed` is the detailed-view toggle (Ctrl+o). */
export type TabViewState = {
  cursor: number;
  mode: PanelMode;
  expanded: Record<string, boolean>;
  scrollTop: number;
  atBottom: boolean;
  detailed: boolean;
  /** The `seq` threshold `MessageList`'s unread pill was counting from when the tab was left
   *  -- `null` when the tab was left following (nothing parked) or never had one. Seeded back
   *  into `MessageList` as `unseenSeed` on restore, so a row that arrived while the tab was away is
   *  still counted: without this, a tab switch reuses `MessageList`, and its first `updatePill` after
   *  the restore would start counting fresh from the just-restored timeline's own length. */
  unseenAfterSeq: number | null;
};

/** How long after the last keystroke the composer mirrors its text to Rust (so a reload keeps it). */
export const DRAFT_MIRROR_DELAY_MS = 300;

/** How long after a refused send the page puts back itself Rust's `draft` echo of that refusal is still told apart
 *  from a draft Rust sends on its own (a switch, a reset, a scratch return). */
export const REFUSED_ECHO_MS = 1500;

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
