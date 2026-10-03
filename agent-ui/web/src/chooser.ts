import type { ChooserEnvelope, ChooserRecord, ChooserTab, PermissionModeChoice, TabInfo } from "./types";
import { markerGlyph } from "./tabs";

/** `prefix w`'s rows: `New session` (hidden while a filter narrows the list -- it
 *  never matches typed text and Claude Code's own picker drops it too), one row per open tab
 *  (joined to its `TabInfo` for mode/state/title, `null` only in the defensive race where the
 *  `tabs` envelope has not yet caught up with the `chooser` one), and one row per record not open
 *  in any tab (`open`/`records` already partitioned that way -- `serialize_chooser_for_js`'s own
 *  doc comment). */
export type ChooserRow =
  | { kind: "new" }
  | { kind: "tab"; tab: ChooserTab; info: TabInfo | null }
  | { kind: "record"; record: ChooserRecord };

function recordLead(record: ChooserRecord): string {
  return record.name ?? record.title ?? "untitled";
}

/** Searchable text for the row's first line, matched case-insensitively by `/`.
 *  A record can display both `name` and `title`, so search both fields even when `recordLead`
 *  chooses only one as the leading label. */
function rowSearchText(row: ChooserRow): string {
  if (row.kind === "new") return "New session";
  if (row.kind === "tab") {
    const glyph = markerGlyph(row.tab.marker, row.tab.pending);
    return glyph === "" ? row.tab.label : `${row.tab.label} ${glyph}`;
  }
  const { name, title } = row.record;
  if (name !== null && title !== null) return `${name} ${title}`;
  return recordLead(row.record);
}

/** A record another window's lease holds is shown and never chosen. Every
 *  other row -- `New session`, an open tab -- is always choosable. */
export function choosable(row: ChooserRow): boolean {
  return row.kind !== "record" || !row.record.heldElsewhere;
}

/** Move a picker's list from the list or its filter box: Down and Ctrl+n step down; Up and Ctrl+p
 *  step up. Shared by the chooser and the `/model` and `/effort` pickers. Return null for other
 *  keys so callers retain their own handling of `j`/`k` and Ctrl+j/Ctrl+k. */
export function pickerStep(event: { key: string; ctrlKey: boolean }): 1 | -1 | null {
  if (event.key === "ArrowDown" || (event.ctrlKey && event.key === "n")) return 1;
  if (event.key === "ArrowUp" || (event.ctrlKey && event.key === "p")) return -1;
  return null;
}

/** Lists `New session`, then open tabs, then records open in no tab, preserving Rust's order.
 *  Joins each open row to `TabInfo` by id for its number, name, title, mode and state.
 *  Drops `New session` while filtering so `j`/`k` do not visit an unmatched row. */
export function chooserRows(env: ChooserEnvelope, tabs: TabInfo[], filter: string): ChooserRow[] {
  const openRows: ChooserRow[] = env.open.map((tab) => ({
    kind: "tab",
    tab,
    info: tabs.find((t) => t.id === tab.tab) ?? null,
  }));
  const recordRows: ChooserRow[] = env.records.map((record) => ({ kind: "record", record }));
  const needle = filter.trim().toLowerCase();
  if (needle === "") return [{ kind: "new" }, ...openRows, ...recordRows];
  return [...openRows, ...recordRows].filter((row) => rowSearchText(row).toLowerCase().includes(needle));
}

/** The state word on an open tab's first line. Failed, ended, and not-started states take
 *  precedence over the last marker so a stopped session cannot still read running. Markers
 *  distinguish running, waiting, and done with unread output; otherwise show idle. A waiting card
 *  appears as `⚑N waiting`. Missing tab info reads new, like a not-started tab. */
export function tabStateWord(info: TabInfo | null): string {
  if (info === null || info.state === "not_started") return "new";
  if (info.state === "failed") return "failed";
  if (info.state === "ended") return "ended";
  // Nothing is idle about a tab whose start waits for the user's answer.
  if (info.state === "awaiting_trust") return "trust?";
  if (info.marker === "working") return "running";
  if (info.marker === "needs_input") return "waiting";
  if (info.marker === "unread") return "done, unread";
  return "idle";
}

/** Claude Code's own `/resume` picker wording for a record's `updatedAt`: "just now",
 *  minutes, hours within today, "yesterday", else a bare month/day. A stamp `formatWhen`
 *  (`SessionRow.tsx`) can't parse passes through unchanged, the same defensive fallback that
 *  function has for a hand-edited or corrupted record (`agent::persistence`; nothing produces one,
 *  nothing tests it -- kept for the same reason). */
export function relativeWhen(nowMs: number, stamp: string): string {
  const millis = Number(stamp);
  if (!Number.isFinite(millis) || millis <= 0) return stamp;
  const diff = nowMs - millis;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} min ago`;
  const startOfDay = (ms: number) => {
    const d = new Date(ms);
    return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  };
  const dayDiff = Math.round((startOfDay(nowMs) - startOfDay(millis)) / 86_400_000);
  if (dayDiff <= 0) return `${Math.floor(diff / 3_600_000)} h ago`;
  if (dayDiff === 1) return "yesterday";
  return new Date(millis).toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

/** The mode and destination for starting or resuming a session. Use the active tab's mode when it
 *  is not started, since the session lands there. Otherwise open a new tab with the window's
 *  remembered default. No active tab also means a new tab. */
export function resumeMode(
  active: TabInfo | null,
  defaultMode: PermissionModeChoice,
): { mode: PermissionModeChoice; into: "this tab" | "a new tab" } {
  if (active !== null && active.state === "not_started") return { mode: active.mode, into: "this tab" };
  return { mode: defaultMode, into: "a new tab" };
}
