import type { TimelineItem } from "./timeline";
import { formatResultContent } from "./toolRegistry";
import { primaryText } from "./copyText";
import { fileChangesOf } from "./diff";

export type PathRef = { path: string; line: number | null };

/** A token with a `/` between name characters, or a name with an extension; an optional `:line`.
 *  Checked against a URL separately: the scheme's `://` rules a token out. */
export const PATH_RE = /(?:[\w.\-]+\/)+[\w.\-]+|[\w\-]+\.[A-Za-z][A-Za-z0-9]{0,7}\b/g;

function withLine(text: string, start: number, match: string): PathRef | null {
  const before = text.slice(Math.max(0, start - 3), start);
  if (before.endsWith("://") || /^\w+:\/\//.test(text.slice(start))) return null;
  if (text.slice(start - 1, start) === "/") return null;
  const rest = text.slice(start + match.length);
  const line = /^:(\d+)/.exec(rest);
  return { path: match, line: line ? Number(line[1]) : null };
}

export function parsePath(text: string): PathRef | null {
  const trimmed = text.trim();
  if (/^\w+:\/\//.test(trimmed)) return null;
  const m = /^((?:[\w.\-]+\/)+[\w.\-]+|[\w\-]+\.[A-Za-z][A-Za-z0-9]{0,7})(?::(\d+))?$/.exec(trimmed);
  return m ? { path: m[1], line: m[2] ? Number(m[2]) : null } : null;
}

/** U+0000-U+001F and U+007F. A tool's path field is whatever the model or a repository put there,
 *  and the editor is where it goes: no real file name needs one, and a newline in a name is how a
 *  path would turn into a second command. */
const CONTROL_RE = /[\u0000-\u001f\u007f]/;

function fieldPaths(name: string, input: unknown): PathRef[] {
  if (!input || typeof input !== "object") return [];
  const fields = input as Record<string, unknown>;
  const candidates = [
    ...["file_path", "path", "notebook_path"].map((key) => fields[key]),
    ...fileChangesOf(name, input).map((change) => change.filePath),
  ];
  const seen = new Set<string>();
  return candidates.flatMap((value) => {
    if (typeof value !== "string" || value === "" || CONTROL_RE.test(value) || seen.has(value)) return [];
    seen.add(value);
    return [{ path: value, line: null }];
  });
}

/** The longest run of path characters searched for a path. Matching inside a run costs up to the square of its
 *  length, so this bounds the work per character of a reply. */
const MAX_PATH_RUN = 256;

function prosePaths(text: string): PathRef[] {
  const out: PathRef[] = [];
  const seen = new Set<string>();
  for (const run of text.matchAll(/[\w.\-/]+/g)) {
    // Bounding each regex input keeps long blobs from retrying a path match at every character; a run this
    // long in prose is a blob (base64, a hash dump), not a path a reader would open.
    if (run[0].length > MAX_PATH_RUN || !/[./]/.test(run[0])) continue;
    for (const m of run[0].matchAll(PATH_RE)) {
      const ref = withLine(text, run.index! + m.index!, m[0]);
      if (ref === null) continue;
      const key = `${ref.path}:${ref.line}`;
      if (!seen.has(key)) {
        seen.add(key);
        out.push(ref);
      }
    }
  }
  return out;
}

/** The paths `gf` can open on this row, including the file headers it draws. */
export function pathsIn(item: TimelineItem): PathRef[] {
  switch (item.kind) {
    case "tool":
      return fieldPaths(item.call.name, item.call.input);
    case "permission":
      return fieldPaths(item.request.toolName, item.request.input);
    case "prompt":
    case "message":
      return prosePaths(item.text);
    case "run":
    case "ending":
    // A thought names files it was considering, not ones it is about; `gf` there would open
    // something the row does not even show while folded.
    case "thinking":
      return [];
  }
}

/** `Ctrl+g` in BROWSE: the row's whole text, never the cut one, and a title for the buffer. */
function fenced(text: string): string {
  const longest = Math.max(0, ...(text.match(/`+/g) ?? []).map((run) => run.length));
  const fence = "`".repeat(Math.max(3, longest + 1));
  return `${fence}\n${text}\n${fence}`;
}

export function viewText(item: TimelineItem): { title: string; text: string } {
  switch (item.kind) {
    case "prompt":
      return { title: "prompt", text: item.text };
    case "message":
      return { title: "reply", text: item.text };
    case "tool": {
      const head = item.call.name === "Bash" ? `$ ${primaryText(item)}` : primaryText(item);
      // Fenced: the scratch buffer is markdown (right for a reply), and a bare output's `_`s and `*`s
      // drew as emphasis there. The fence outgrows any backtick run inside.
      const body = item.call.result === null ? "" : `\n\n${fenced(formatResultContent(item.call.result.content))}`;
      return { title: `${item.call.name} ${primaryText(item)}`.slice(0, 60), text: head + body };
    }
    case "permission":
      return { title: `${item.request.toolName} request`, text: primaryText(item) };
    case "run":
      return { title: "tool calls", text: primaryText(item) };
    case "ending":
      return { title: "turn ending", text: primaryText(item) };
    case "thinking":
      // Fenced: the scratch buffer is markdown, and a thought is plain text.
      return { title: "thinking", text: fenced(item.block.text) };
  }
}
