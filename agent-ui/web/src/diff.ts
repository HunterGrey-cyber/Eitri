/** Turning a permission request's raw tool input into something a person can actually review.
 *
 * The data has always been here: a `PreToolUse` payload carries the whole tool-input object, and
 * for `Edit` that object is `{file_path, old_string, new_string, replace_all}`. The card rendered
 * it as `JSON.stringify(..., 2)`, which is the change, escaped, with `\n` written out literally --
 * technically complete and unreadable in the one place where reading it is the entire point.
 *
 * So this needs no wire change, no new event and no protocol work. It is a rendering fix.
 *
 * **This is a DISPLAY diff between two strings the request already contains.** It is not, and must
 * not become, a matcher: deciding whether `old_string` occurs in the file, whether it is unique,
 * and whether the file changed since it was read is the CLI's own job, and it already refuses all
 * three cases by name. A second matching ladder here would re-implement that, worse.
 */

export type DiffLineKind = "context" | "added" | "removed";
export interface DiffLine {
  kind: DiffLineKind;
  text: string;
}

/** Beyond this many lines on either side the diff is summarised instead of computed.
 *
 * The LCS table below is O(n*m) cells, so a `Write` of a large file would be both slow and useless
 * -- nobody reviews ten thousand lines in a card. The summary still says exactly what will happen;
 * it just stops pretending the card is the place to read it.
 */
export const MAX_DIFF_LINES = 400;

/** Splits the way a file does, so a trailing newline does not become a phantom empty last line. */
function lines(text: string): string[] {
  if (text === "") return [];
  const split = text.split("\n");
  if (split.length > 0 && split[split.length - 1] === "") split.pop();
  return split;
}

/** A line-level diff, longest-common-subsequence, smallest thing that is honest.
 *
 * Returns `null` when either side is too large to be worth rendering; the caller says so rather
 * than showing a truncated diff, because a diff that silently omits lines is worse than one that
 * refuses -- the reader would approve what they could see.
 */
export function lineDiff(before: string, after: string): DiffLine[] | null {
  const a = lines(before);
  const b = lines(after);
  if (a.length > MAX_DIFF_LINES || b.length > MAX_DIFF_LINES) return null;

  // lcs[i][j] = length of the longest common subsequence of a[i..] and b[j..]
  const lcs: number[][] = Array.from({ length: a.length + 1 }, () => new Array<number>(b.length + 1).fill(0));
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }

  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      out.push({ kind: "context", text: a[i] });
      i++;
      j++;
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
      out.push({ kind: "removed", text: a[i] });
      i++;
    } else {
      out.push({ kind: "added", text: b[j] });
      j++;
    }
  }
  while (i < a.length) out.push({ kind: "removed", text: a[i++] });
  while (j < b.length) out.push({ kind: "added", text: b[j++] });
  return out;
}

export interface EditPreview {
  /** The file this call would write. Empty when the tool did not name one. */
  filePath: string;
  /** `null` when the change is too large to render; the card says so instead of showing part. */
  diff: DiffLine[] | null;
  /** Counts, always available even when `diff` is null, because "how big" is the first question. */
  added: number;
  removed: number;
  /** True for an `Edit` carrying `replace_all`, which changes how many places this touches. */
  replaceAll: boolean;
  /** True when the whole file is being written, so the card can say so rather than imply a patch. */
  wholeFile: boolean;
  /** The note `EditDiff` shows under a `wholeFile` change instead of `Write`'s own wording ("Writes
   *  the whole file..."), for a tool whose whole-content write is not a whole file -- `NotebookEdit`
   *  writes one cell's `new_source`, and the tool's own schema carries no "before" to diff against.
   *  `undefined` when `wholeFile` is `false`, or for `Write`, which keeps its pinned wording. */
  wholeFileNote?: string;
}

/** A reviewable preview for the tools that change a file, or `null` for every other tool.
 *
 * Returning `null` rather than an empty preview is deliberate: the card falls back to showing the
 * raw input, which for a `Bash` or an `mcp__*` call is the honest rendering. A diff view that
 * quietly appeared for tools it does not understand would be a worse lie than JSON.
 */
export function editPreview(toolName: string, input: unknown): EditPreview | null {
  if (input === null || typeof input !== "object") return null;
  const fields = input as Record<string, unknown>;
  const str = (key: string): string => (typeof fields[key] === "string" ? (fields[key] as string) : "");
  const filePath = str("file_path");

  if (toolName === "Edit") {
    const before = str("old_string");
    const after = str("new_string");
    // An Edit with neither side is not an edit; fall back rather than draw an empty diff.
    if (before === "" && after === "") return null;
    const diff = lineDiff(before, after);
    return {
      filePath,
      diff,
      added: diff ? diff.filter((l) => l.kind === "added").length : lines(after).length,
      removed: diff ? diff.filter((l) => l.kind === "removed").length : lines(before).length,
      replaceAll: fields["replace_all"] === true,
      wholeFile: false,
    };
  }

  if (toolName === "Write") {
    // `Write` carries only what the file WILL contain -- the request says nothing about what is
    // there now, so every line is an addition. Saying `wholeFile` lets the card call it a write
    // rather than draw a patch that implies the rest of the file survives.
    const content = str("content");
    if (content === "" && filePath === "") return null;
    const diff = lineDiff("", content);
    return {
      filePath,
      diff,
      added: lines(content).length,
      removed: 0,
      replaceAll: false,
      wholeFile: true,
    };
  }

  // v1 trial item 7: `NotebookEdit`'s own schema (`notebook_path`, `new_source`) has no "before"
  // field to diff against -- the tool is never told what a cell held, only asked what it should
  // hold now -- so this is a whole-content write of one cell, the same shape `Write` is for a whole
  // file, filed under `notebook_path` rather than `file_path`.
  //
  // Fix round finding 3: `edit_mode: "delete"` carries no `new_source` at all (there is nothing to
  // write), so treating it like every other `NotebookEdit` produced an empty diff, "+0 −0" and
  // "Writes the whole cell" -- true of nothing, and silent about the one thing that actually
  // happened. `edit_mode` defaults to "replace" when the field is absent, matching the tool's own
  // schema; only "delete" gets its own wording here, named by `cell_id` when the call gives one.
  if (toolName === "NotebookEdit") {
    const notebookPath = str("notebook_path");
    const cellId = str("cell_id");
    if (fields["edit_mode"] === "delete") {
      if (notebookPath === "") return null;
      return {
        filePath: notebookPath,
        diff: [],
        added: 0,
        removed: 0,
        replaceAll: false,
        wholeFile: true,
        wholeFileNote: cellId
          ? `Deletes cell ${cellId}. The request carries no record of what it held.`
          : "Deletes a cell. The request carries no record of what it held.",
      };
    }
    const newSource = str("new_source");
    if (newSource === "" && notebookPath === "") return null;
    const diff = lineDiff("", newSource);
    return {
      filePath: notebookPath,
      diff,
      added: lines(newSource).length,
      removed: 0,
      replaceAll: false,
      wholeFile: true,
      wholeFileNote: "Writes the whole cell. The request does not say what is there now.",
    };
  }

  return null;
}
