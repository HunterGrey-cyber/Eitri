import type { TabId } from "./types";

export type CardView = { tab: TabId; shown: number };

function retain<K, V>(entries: Map<K, V>, keep: (key: K, value: V) => boolean): Map<K, V> {
  let kept = entries;
  for (const [key, value] of entries) {
    if (keep(key, value)) continue;
    if (kept === entries) kept = new Map(entries);
    kept.delete(key);
  }
  return kept;
}

/** Keep completion data only while its tab exists. An unchanged table keeps its identity. */
export function pruneTabEntries<T>(entries: Map<TabId, T>, live: ReadonlySet<TabId>): Map<TabId, T> {
  return retain(entries, (tab) => live.has(tab));
}

/** Only the tab whose cards were received has an authoritative pending set. */
export function pruneCardViews(
  entries: Map<string, CardView>,
  live: ReadonlySet<TabId>,
  received?: { tab: TabId; pending: ReadonlySet<string> },
): Map<string, CardView> {
  return retain(entries, (id, view) => live.has(view.tab) &&
    (received === undefined || view.tab !== received.tab || received.pending.has(id)));
}
