import { editorCardText, noticeText, wakeText, noticeBody, type TimelineItem } from "./timeline";
import { turnEndingText } from "./turnEnding";
import { formatResultContent } from "./toolRegistry";

/** What `/` matches in a row: what the row shows or can show. */
export function rowSearchText(item: TimelineItem): string {
  switch (item.kind) {
    case "prompt":
    case "message":
    case "planmark":
      return item.text;
    case "plan":
      // The whole plan, so `/` finds a step while the row is folded.
      return [item.plan.plan, item.text].join("\n");
    case "tool":
      return [
        item.call.name,
        JSON.stringify(item.call.input),
        item.call.result ? formatResultContent(item.call.result.content) : "",
      ].join("\n");
    case "permission":
      return [item.request.toolName, editorCardText(item.request) ?? JSON.stringify(item.request.input)].join("\n");
    case "ending":
      return turnEndingText(item.ending, item.ending.resetsAt ?? null);
    case "wake":
      return [wakeText(item), item.body].filter(Boolean).join("\n");
    case "notice":
      return [noticeText(item.notice), noticeBody(item.notice)].filter(Boolean).join("\n");
    case "run":
      // A collapsed run has no invocation of its own on screen to search -- each call's name
      // and input, so `/bash` still finds it even while it is folded, and the thoughts folded with them.
      return [
        ...item.calls.flatMap((call) => [call.name, JSON.stringify(call.input)]),
        ...(item.thoughts ?? []).map((t) => t.text),
      ].join("\n");
    case "thinking":
      // What the row can show once opened, so `/` finds a thought while it is folded.
      return item.block.text;
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
