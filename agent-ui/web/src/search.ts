import type { TimelineItem } from "./timeline";
import { formatResultContent } from "./toolRegistry";

/** What `/` matches in a row: what the row shows or can show (R4). */
export function rowSearchText(item: TimelineItem): string {
  switch (item.kind) {
    case "prompt":
    case "message":
      return item.text;
    case "tool":
      return [
        item.call.name,
        JSON.stringify(item.call.input),
        item.call.result ? formatResultContent(item.call.result.content) : "",
      ].join("\n");
    case "permission":
      return [item.request.toolName, JSON.stringify(item.request.input)].join("\n");
    case "run":
      // P2: a collapsed run has no invocation of its own on screen to search -- each call's name
      // and input, so `/bash` still finds it even while it is folded.
      return item.calls.flatMap((call) => [call.name, JSON.stringify(call.input)]).join("\n");
  }
}

/** vim's `/` with `wrapscan` and `smartcase`: the first row from `from` (itself when `inclusive`) in
 *  direction `dir` whose text contains `query`, wrapping once round. */
export function findMatch(items: TimelineItem[], query: string, from: number, dir: 1 | -1, inclusive: boolean): number | null {
  if (query === "" || items.length === 0) return null;
  const sensitive = query !== query.toLowerCase();
  const needle = sensitive ? query : query.toLowerCase();
  const n = items.length;
  for (let step = inclusive ? 0 : 1; step <= n; step++) {
    const i = (((from + dir * step) % n) + n) % n;
    const hay = rowSearchText(items[i]);
    if ((sensitive ? hay : hay.toLowerCase()).includes(needle)) return i;
  }
  return null;
}
