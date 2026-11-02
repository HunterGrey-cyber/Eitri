/** The project mention under the caret. Quotes keep spaces inside a single mention. */
export function atToken(draft: string, caret: number): { query: string; start: number; end: number } | null {
  if (draft.startsWith("!") || /^\s*\//.test(draft) || caret < 0 || caret > draft.length) return null;
  for (let start = 0; start < draft.length; start += 1) {
    if (draft[start] !== "@" || (start > 0 && !/\s/u.test(draft[start - 1]))) continue;
    const quoted = draft[start + 1] === '"';
    const from = start + (quoted ? 2 : 1);
    let end = from;
    while (end < draft.length && (quoted ? draft[end] !== '"' && draft[end] !== "\n" : !/\s/u.test(draft[end]))) end += 1;
    const queryEnd = end;
    if (quoted && draft[end] === '"') end += 1;
    if (caret > start && caret <= end) {
      return { query: draft.slice(from, Math.max(from, Math.min(caret, queryEnd))), start, end };
    }
    start = end - 1;
  }
  return null;
}

/** A directory keeps completing; a file leaves the caret after its separating space. */
export function atInsertion(path: string, quoted = false): { text: string; keepOpen: boolean } {
  const keepOpen = path.endsWith("/");
  // A file named like one of the editor's sources stays a file when the word was quoted: `@"buffers"`.
  const name = /\s/u.test(path) || (quoted && EDITOR_MENTIONS.some((m) => m.name === path)) ? `"${path}"` : path;
  return { text: `@${name}${keepOpen ? "" : " "}`, keepOpen };
}

/** The editor's own sources, which are not files: the host reads them from nvim when the message is sent. */
export const EDITOR_MENTIONS = [
  { name: "buffers", hint: "open files, as on screen" },
  { name: "quickfix", hint: "the quickfix list" },
  { name: "diagnostics", hint: "language server problems" },
] as const;

/** The editor sources a query is the start of, ahead of the files. A path (`sub/`) or a quoted name is never one. */
export function editorMentionRows(query: string): string[] {
  if (query.includes("/") || query.includes('"')) return [];
  const lower = query.toLowerCase();
  return EDITOR_MENTIONS.filter((m) => m.name.startsWith(lower)).map((m) => m.name);
}

/** What the menu says beside a row, if the row is one of the editor's sources. */
export function editorMentionHint(row: string): string | undefined {
  return EDITOR_MENTIONS.find((m) => m.name === row)?.hint;
}

/** Where the editor's sources stand in the menu. A query one of them starts leads with it, as any match does. A bare `@`
 *  keeps the first file first, as Claude Code's does (Tab takes it), so the sources come after the files: a person who
 *  presses Up from the top wraps to them. */
export function withEditorMentions(query: string, files: readonly string[]): string[] {
  const own = editorMentionRows(query);
  const rest = files.filter((file) => !own.includes(file));
  return query === "" ? [...rest, ...own] : [...own, ...rest];
}
