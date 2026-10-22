import type { TimelineItem } from "./timeline";

/** One replacement a pending edit proposes, made on the file's text as the earlier ones left it. */
export type ProposedEdit = { oldText: string; newText: string; replaceAll: boolean };

/** What a pending edit card would do to its one file: replacements, or the file's whole new content.
 *  Rust's `ProposedChange` (`core/src/agent_bridge.rs`) is the other half of this shape. */
export type ProposedChange =
  /** `crlfAsLf`: the agent's tool reads a CR-LF file with its line ends made LF, replaces there and writes the
   *  file's line end back over every line (Claude's `Edit` and `MultiEdit`), so the shell must apply the
   *  replacements that way even where they also match the raw bytes. */
  | { kind: "edits"; edits: ProposedEdit[]; crlfAsLf: boolean }
  | { kind: "content"; content: string };

const NONE: ReadonlySet<string> = new Set();

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

/** A Claude `Edit` or `MultiEdit` entry. An empty old text names no place in the file (the tool reads it as
 *  "create the file"), so it is not one this view can draw against the file on disk. */
function claudeEdit(value: unknown): ProposedEdit | null {
  const fields = record(value);
  if (fields === null) return null;
  const { old_string: oldText, new_string: newText } = fields;
  if (typeof oldText !== "string" || oldText === "" || typeof newText !== "string") return null;
  return { oldText, newText, replaceAll: fields.replace_all === true };
}

/** The change `Ctrl+g` can show beside the file it changes: a pending card (not one this panel already
 *  answered) for one file -- a Claude `Edit`, `MultiEdit` or `Write`, a Codex `fileChange` holding one `add`,
 *  or an ACP edit holding one diff. Anything else -- a Codex `update` (its diff is a patch, not a text to
 *  match), several files, a delete or a move, any other row -- is `null`, and `Ctrl+g` shows the row's text. */
export function proposalOf(
  item: TimelineItem,
  answered: ReadonlySet<string> = NONE,
): { path: string; change: ProposedChange } | null {
  if (item.kind !== "permission" || answered.has(item.request.permissionId)) return null;
  const { toolName } = item.request;
  const input = record(item.request.input);
  if (input === null) return null;
  const one = (path: unknown, change: ProposedChange | null) =>
    typeof path === "string" && path !== "" && change !== null ? { path, change } : null;
  switch (toolName) {
    case "Edit": {
      const edit = claudeEdit(input);
      return one(input.file_path, edit === null ? null : { kind: "edits", edits: [edit], crlfAsLf: true });
    }
    case "MultiEdit": {
      const list = Array.isArray(input.edits) ? input.edits.map(claudeEdit) : [];
      if (list.length === 0 || list.some((edit) => edit === null)) return null;
      return one(input.file_path, { kind: "edits", edits: list as ProposedEdit[], crlfAsLf: true });
    }
    case "Write":
      return one(input.file_path, typeof input.content === "string" ? { kind: "content", content: input.content } : null);
    case "fileChange": {
      const changes = Array.isArray(input.changes) ? input.changes : [];
      const change = changes.length === 1 ? record(changes[0]) : null;
      if (change === null || change.kind !== "add" || typeof change.diff !== "string") return null;
      return one(change.path, { kind: "content", content: change.diff });
    }
    case "acp:edit": {
      const diffs = Array.isArray(input.diffs) ? input.diffs : [];
      const diff = diffs.length === 1 ? record(diffs[0]) : null;
      if (diff === null || typeof diff.newText !== "string") return null;
      // No old text (a new file) or an empty one: the new text is the whole file.
      const change: ProposedChange =
        typeof diff.oldText === "string" && diff.oldText !== ""
          ? { kind: "edits", edits: [{ oldText: diff.oldText, newText: diff.newText, replaceAll: false }], crlfAsLf: false }
          : { kind: "content", content: diff.newText };
      return one(diff.path, change);
    }
    default:
      return null;
  }
}
