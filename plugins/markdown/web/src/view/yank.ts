/* A fence line: optional indentation, then three or more backticks or tildes. */
const FENCE = /^[ \t]*(`{3,}|~{3,})/;

/** The source a block stands for, from the snapshot the preview rendered: lines `[line, end)`, each without a
 *  carriage return that ended it. For a fenced code block, the code between the fences: the fence lines are markup,
 *  and copying the commands in a code block is what a reader wants. A fence that runs unclosed to the end of the
 *  document has no closing line to drop. */
export function yankText(lines: string[], block: { line: number; end: number; fence: boolean }): string {
  const start = Math.max(0, block.line);
  const stop = Math.min(lines.length, Math.max(start, block.end));
  let taken = lines.slice(start, stop).map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l));
  if (block.fence && taken.length > 0) {
    const open = FENCE.exec(taken[0]);
    if (open) {
      const marker = open[1][0];
      const length = open[1].length;
      taken = taken.slice(1);
      const last = taken.length > 0 ? taken[taken.length - 1].trim() : "";
      if (last.length >= length && [...last].every((c) => c === marker)) taken = taken.slice(0, -1);
    }
  }
  return taken.join("\n");
}
