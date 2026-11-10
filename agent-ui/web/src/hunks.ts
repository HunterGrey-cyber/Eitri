import { codexChangePreview, editPreview, lineDiff, MAX_DIFF_LINES, type DiffLine, type EditPreview } from "./diff";
import type { TimelineItem } from "./timeline";

/** Half-open spans of changed lines; context separates adjacent hunks. */
export function hunksOf(lines: DiffLine[]): { start: number; end: number }[] {
  const hunks: { start: number; end: number }[] = [];
  lines.forEach((line, i) => {
    if (line.kind === "context") return;
    if (i > 0 && lines[i - 1].kind !== "context") hunks[hunks.length - 1].end = i + 1;
    else hunks.push({ start: i, end: i + 1 });
  });
  return hunks;
}

/** The bounded previews on a row, retaining each file's identity for hunk copies. */
export function editPreviewsOf(item: TimelineItem): EditPreview[] | null {
  const tool = item.kind === "tool" ? { name: item.call.name, input: item.call.input }
    : item.kind === "permission" ? { name: item.request.toolName, input: item.request.input } : null;
  if (tool === null) return null;
  if (tool.name.startsWith("acp:")) {
    const diffs = (tool.input as { diffs?: { path: string; oldText?: string | null; newText?: string }[] } | null)?.diffs;
    if (!Array.isArray(diffs)) return null;
    const previews = diffs.flatMap((entry): EditPreview[] => {
      const diff = lineDiff(entry.oldText ?? "", entry.newText ?? "");
      // Oversized files draw counts only; the remaining files still have anchors in DOM order.
      return diff === null ? [] : [{ filePath: entry.path, diff,
        added: diff.filter((line) => line.kind === "added").length,
        removed: diff.filter((line) => line.kind === "removed").length,
        replaceAll: false, wholeFile: false }];
    });
    return previews.length === 0 ? null : previews;
  }
  if (tool.name === "fileChange") {
    const changes = (tool.input as { changes?: { path: string; kind: string; diff: string }[] } | null)?.changes;
    if (!Array.isArray(changes) || changes.length === 0) return null;
    const previews = changes.map((change) => codexChangePreview(change));
    return previews.some((preview) => preview.diff === null) ? null : previews;
  }
  const preview = editPreview(tool.name, tool.input);
  if (preview === null || preview.diff === null) return null;
  // The display may fall back to a whole replacement beyond the LCS limit; it is not a selectable hunk.
  if (preview.diff.filter((l) => l.kind !== "added").length > MAX_DIFF_LINES ||
      preview.diff.filter((l) => l.kind !== "removed").length > MAX_DIFF_LINES) return null;
  return [preview];
}

export function editDiffOf(item: TimelineItem): DiffLine[] | null {
  const previews = editPreviewsOf(item);
  if (previews === null) return null;
  // A file boundary separates changes even when both files have only added lines.
  return previews.flatMap((preview, i) => i === 0 ? preview.diff! : [{ kind: "context" as const, text: "" }, ...preview.diff!]);
}
