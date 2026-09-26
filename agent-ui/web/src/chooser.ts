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

/** The right-hand word on an open tab's first line: "running | done, unread | idle |
 *  ended | failed | new". `state` (failed/ended/not_started) always wins over `marker`, since a
 *  session that has stopped is no longer "running" no matter what its last marker was; `working`
 *  and `unread` are the two markers with their own word, everything else (nothing pending, or a
 *  pending `needs_input` card the chooser already shows as `⚑N`) reads as `idle`. `null` -- the
 *  defensive race `ChooserRow`'s own doc comment names -- reads as `new`, the same word a
 *  `not_started` tab gets, since neither can say anything more specific yet. */
export function tabStateWord(info: TabInfo | null): string {
  if (info === null || info.state === "not_started") return "new";
  if (info.state === "failed") return "failed";
  if (info.state === "ended") return "ended";
  if (info.marker === "working") return "running";
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
