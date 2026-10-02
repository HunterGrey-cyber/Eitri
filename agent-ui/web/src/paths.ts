import type { TimelineItem } from "./timeline";
import { formatResultContent } from "./toolRegistry";
import { primaryText } from "./copyText";

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

function fieldPaths(input: unknown): PathRef[] {
  if (!input || typeof input !== "object") return [];
  const fields = input as Record<string, unknown>;
  return ["file_path", "path", "notebook_path"].flatMap((k) =>
    typeof fields[k] === "string" && fields[k] !== "" ? [{ path: fields[k] as string, line: null }] : [],
  );
}

function prosePaths(text: string): PathRef[] {
  const out: PathRef[] = [];
  for (const m of text.matchAll(PATH_RE)) {
    const ref = withLine(text, m.index ?? 0, m[0]);
    if (ref !== null && !out.some((o) => o.path === ref.path && o.line === ref.line)) out.push(ref);
  }
  return out;
}

/** N2 (ruling 19): the paths `gf` can open on this row. */
export function pathsIn(item: TimelineItem): PathRef[] {
  switch (item.kind) {
    case "tool":
      return fieldPaths(item.call.input);
    case "permission":
      return fieldPaths(item.request.input);
    case "prompt":
    case "message":
      return prosePaths(item.text);
    case "run":
    case "ending":
      return [];
  }
}

/** R3's `Ctrl+g` in BROWSE: the row's whole text, never the cut one, and a title for the buffer. */
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
      // drew as emphasis there (the phase-3 GUI pass). The fence outgrows any backtick run inside.
      const body = item.call.result === null ? "" : `\n\n${fenced(formatResultContent(item.call.result.content))}`;
      return { title: `${item.call.name} ${primaryText(item)}`.slice(0, 60), text: head + body };
    }
    case "permission":
      return { title: `${item.request.toolName} request`, text: primaryText(item) };
    case "run":
      return { title: "tool calls", text: primaryText(item) };
    case "ending":
      return { title: "turn ending", text: primaryText(item) };
  }
}
