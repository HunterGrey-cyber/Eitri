import type { ChooserEnvelope, ChooserRecord, ChooserTab } from "./types";
import { markerGlyph } from "./tabs";
import { formatWhen } from "./components/SessionRow";

export type ChooserRow = { kind: "tab"; tab: ChooserTab } | { kind: "record"; record: ChooserRecord };

function recordLead(record: ChooserRecord): string {
  return record.name ?? record.title ?? `claude ${record.providerSessionId.slice(0, 8)}`;
}

/** One line per row, as the chooser shows it (and as `/` matches it). */
export function rowText(row: ChooserRow): string {
  if (row.kind === "tab") {
    const glyph = markerGlyph(row.tab.marker, row.tab.pending);
    const parts = [glyph === "" ? row.tab.label : `${row.tab.label} ${glyph}`];
    if (!row.tab.resumable) parts.push("not resumable");
    return parts.join(" · ");
  }
  const parts = [recordLead(row.record), `last opened ${formatWhen(row.record.updatedAt)}`];
  if (row.record.heldElsewhere) parts.push("open in another window");
  return parts.join(" · ");
}

/** A record another window's lease holds is shown and never chosen (spec §3.6). */
export function choosable(row: ChooserRow): boolean {
  return row.kind === "tab" || !row.record.heldElsewhere;
}

export function chooserRows(envelope: ChooserEnvelope, filter: string): ChooserRow[] {
  const rows: ChooserRow[] = [
    ...envelope.open.map((tab): ChooserRow => ({ kind: "tab", tab })),
    ...envelope.records.map((record): ChooserRow => ({ kind: "record", record })),
  ];
  const needle = filter.trim().toLowerCase();
  return needle === "" ? rows : rows.filter((row) => rowText(row).toLowerCase().includes(needle));
}
