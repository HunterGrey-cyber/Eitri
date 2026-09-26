import type { ChooserEnvelope, ChooserRecord, ChooserTab, PermissionModeChoice, TabInfo } from "./types";
import { markerGlyph } from "./tabs";

/** `prefix w`'s rows (spec §6.1): `New session` (hidden while a filter narrows the list -- it
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

/** Line 1's searchable text (also what `/` matches against, case-insensitively). A record's `name`
 *  and `title` are both shown on the row at once (`RecordLead`) when both are set, so both must be
 *  searchable -- not just whichever `recordLead` picks as the leading label (spec §6.1: "filter by
 *  name or title"). */
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

/** A record another window's lease holds is shown and never chosen (spec §3.6, unchanged). Every
 *  other row -- `New session`, an open tab -- is always choosable. */
export function choosable(row: ChooserRow): boolean {
  return row.kind !== "record" || !row.record.heldElsewhere;
}

/** `prefix w` (and D10's launch chooser): `New session` first, then open tabs, then records open
 *  in no tab -- Rust's own order (spec §6.1). `tabs` joins each open row to its `TabInfo` for
 *  number/name/title/mode/state (spec §10.1: "The chooser's open rows join the `tabs` entry of the
 *  same id"). `New session` drops out while filtering (it matches nothing typed, and staying would
 *  make `j`/`k` skip an unmatched row for no reason). */
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

/** The right-hand word on an open tab's first line (spec §6.1): "running | done, unread | idle |
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

/** Claude Code's own `/resume` picker wording for a record's `updatedAt` (spec §6.1): "just now",
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

/** §6.3: the mode a resume (of `New session` or a record) will actually run in, and where it
 *  lands. The active tab's own mode when that tab is `not_started` (the resume goes into it,
 *  `ResumeRoute::StartIn`); otherwise the window's remembered default, since a resume then opens a
 *  new tab (`TabSet::default_mode`). `active === null` (no tab yet) takes the same "a new tab"
 *  branch -- there is nothing to start it in. */
export function resumeMode(
  active: TabInfo | null,
  defaultMode: PermissionModeChoice,
): { mode: PermissionModeChoice; into: "this tab" | "a new tab" } {
  if (active !== null && active.state === "not_started") return { mode: active.mode, into: "this tab" };
  return { mode: defaultMode, into: "a new tab" };
}
