/** Where `↑`/`↓` are in the project's history (C5, ruling 13). `index` null: on the draft itself. */
export type HistoryWalk = { index: number | null; stash: string };

export function stepHistory(
  entries: string[],
  walk: HistoryWalk,
  current: string,
  dir: -1 | 1,
): { walk: HistoryWalk; text: string } | null {
  if (entries.length === 0) return null;
  if (walk.index === null) {
    if (dir === 1) return null;
    const index = entries.length - 1;
    return { walk: { index, stash: current }, text: entries[index] };
  }
  const next = walk.index + dir;
  if (next < 0) return null;
  if (next >= entries.length) return { walk: { index: null, stash: "" }, text: walk.stash };
  return { walk: { index: next, stash: walk.stash }, text: entries[next] };
}

/** readline's reverse-i-search over `entries` (oldest first): the newest entry before `before`
 *  (exclusive; `null` = from the newest) that contains `query`. Smartcase, as vim's: a query with
 *  an uppercase letter is case-sensitive. */
export function searchHistory(entries: string[], query: string, before: number | null): number | null {
  if (query === "") return null;
  const sensitive = query !== query.toLowerCase();
  const needle = sensitive ? query : query.toLowerCase();
  for (let i = (before ?? entries.length) - 1; i >= 0; i--) {
    const hay = sensitive ? entries[i] : entries[i].toLowerCase();
    if (hay.includes(needle)) return i;
  }
  return null;
}
