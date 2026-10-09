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
export function splitLines(text: string): string[] {
  return lines(text);
}

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
let lineDiffs = 0;
/** How many times `lineDiff` has run in this page: the performance tests count it. */
export function lineDiffCount(): number {
  return lineDiffs;
}

export function lineDiff(before: string, after: string): DiffLine[] | null {
  lineDiffs += 1;
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
  // A finished Edit row is drawn again on every render of the conversation (a cursor move, a streamed
  // batch, a scroll), and a 400-by-400-line diff is 160,000 comparisons. A tool call's input never
  // changes once it arrived, so the preview is kept with the input object itself -- and kept only
  // while every top-level field is still the very value it was computed from, so even an input
  // changed in place afterwards can never be shown with a preview of what it used to say.
  const keys = Object.keys(input);
  const values = keys.map((key) => (input as Record<string, unknown>)[key]);
  const held = previews.get(input);
  if (held !== undefined && held.toolName === toolName && sameFields(held, keys, values)) return held.preview;
  const preview = freezePreview(computeEditPreview(toolName, input));
  previews.set(input, { toolName, keys, values, preview });
  return preview;
}

interface HeldPreview {
  toolName: string;
  keys: string[];
  values: unknown[];
  preview: EditPreview | null;
}

const previews = new WeakMap<object, HeldPreview>();

function sameFields(held: HeldPreview, keys: string[], values: unknown[]): boolean {
  if (held.keys.length !== keys.length) return false;
  for (let i = 0; i < keys.length; i++) {
    if (held.keys[i] !== keys[i] || !Object.is(held.values[i], values[i])) return false;
  }
  return true;
}

/** A held preview is shared by every row and card that draws the same input, so none of them may
 *  change it. */
function freezePreview(preview: EditPreview | null): EditPreview | null {
  if (preview === null) return null;
  if (preview.diff !== null) {
    for (const line of preview.diff) Object.freeze(line);
    Object.freeze(preview.diff);
  }
  return Object.freeze(preview);
}

function computeEditPreview(toolName: string, input: object): EditPreview | null {
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

  // `NotebookEdit` has no previous cell contents to diff against. Treat `new_source` as a whole-cell
  // write, like `Write` for a whole file, using `notebook_path` rather than `file_path`.
  // `edit_mode: "delete"` has no `new_source`, so give it deletion wording instead of an empty write diff.
  // Name the cell with `cell_id` when supplied. An absent `edit_mode` defaults to "replace" in the tool schema.
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

/** The most lines a Codex change is drawn with in a conversation row. Codex's own diffs are not limited the way a
 *  card's `Edit` is, and a diff missing lines is one a person would approve anyway, so a larger one is counted and
 *  not shown there. */
const MAX_CODEX_DIFF_LINES = 2 * MAX_DIFF_LINES;

/** The preamble lines of a unified diff that only name the file. */
const FILE_HEADER = /^(--- |\+\+\+ |diff --git |index )/;

/** A Codex `fileChange` entry as a reviewable preview. Codex sends a unified diff for an `update` (its hunk
 *  headers are drawn as context lines) and the whole content for an `add` or a `delete`; a kind this build
 *  does not know is drawn as the unified diff it most likely is. Nothing is matched against the disk.
 *
 *  `whole` draws every line however many there are: a card asking to approve the change must show all of what it
 *  approves, since counts beside an Approve would have a person approve lines they never saw. Its size is bounded
 *  before it reaches a card (Eitri declines a change it did not see in full, or one over 1 MiB, itself). */
export function codexChangePreview(
  change: { path: string; kind: string; diff: string },
  { whole = false }: { whole?: boolean } = {},
): EditPreview {
  const raw = lines(typeof change.diff === "string" ? change.diff : "");
  let diff: DiffLine[];
  if (change.kind === "add") diff = raw.map((text) => ({ kind: "added", text }));
  else if (change.kind === "delete") diff = raw.map((text) => ({ kind: "removed", text }));
  else {
    diff = [];
    // Without any hunk header the text is taken as the hunk's lines.
    let inHunk = !raw.some((text) => text.startsWith("@@"));
    for (const text of raw) {
      if (text.startsWith("@@")) {
        inHunk = true;
        diff.push({ kind: "context", text });
      } else if (!inHunk) {
        // A file header before the first hunk says which file it is, which the card's path already does. Any other
        // line there (a mode change, a rename, a similarity index) is part of what the change does, so it is drawn.
        if (!FILE_HEADER.test(text)) diff.push({ kind: "context", text });
      } else if (text.startsWith("+")) diff.push({ kind: "added", text: text.slice(1) });
      else if (text.startsWith("-")) diff.push({ kind: "removed", text: text.slice(1) });
      else if (text.startsWith("\\")) continue;
      else diff.push({ kind: "context", text: text.startsWith(" ") ? text.slice(1) : text });
    }
  }
  const added = diff.filter((l) => l.kind === "added").length;
  const removed = diff.filter((l) => l.kind === "removed").length;
  return {
    filePath: change.path,
    diff: !whole && diff.length > MAX_CODEX_DIFF_LINES ? null : diff,
    added,
    removed,
    replaceAll: false,
    wholeFile: false,
  };
}
