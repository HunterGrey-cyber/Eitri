/** The one-line row an editor tool's call gets in a tab: what happened, in words, from the call's input and
 *  its answer. The returned text is plain (it is drawn as text, never as Markdown) and holds whatever the
 *  model wrote in the input, so a caller draws it through `revealHidden`. */

import { splitLines } from "./diff";
import { projectRelative } from "./projectPath";

type Answer = { content: unknown; isError: boolean };

/** How a path is drawn: relative to the project root when it is under it (`projectRelative`), as Claude Code's
 *  own tool lines print one; any other path as it came. */
type Rel = (path: string) => string;

function relTo(projectDir: string): Rel {
  return (path) => projectRelative(path, projectDir);
}

function pathOf(input: unknown): string {
  if (input === null || typeof input !== "object") return "";
  const path = (input as Record<string, unknown>).path;
  return typeof path === "string" ? path : "";
}

function lineOf(input: unknown): number | null {
  if (input === null || typeof input !== "object") return null;
  const line = (input as Record<string, unknown>).line;
  return typeof line === "number" && Number.isInteger(line) && line > 0 ? line : null;
}

function symbolOf(input: unknown): string {
  if (input === null || typeof input !== "object") return "";
  const symbol = (input as Record<string, unknown>).symbol;
  return typeof symbol === "string" ? symbol : "";
}

/** The text of an answer: a string, a list of content blocks, or an MCP result holding such a list. */
function answerText(content: unknown): string | null {
  if (typeof content === "string") return content;
  const blocks = Array.isArray(content)
    ? content
    : content !== null && typeof content === "object" && Array.isArray((content as { content?: unknown }).content)
      ? (content as { content: unknown[] }).content
      : null;
  if (blocks === null) return null;
  const texts: string[] = [];
  for (const block of blocks) {
    if (block === null || typeof block !== "object") return null;
    const text = (block as { text?: unknown }).text;
    if (typeof text !== "string") return null;
    texts.push(text);
  }
  return texts.join("");
}

/** The answer as a JSON object, or `null` when it is anything else. Never throws. */
function answerObject(content: unknown): Record<string, unknown> | null {
  const text = answerText(content);
  if (text === null) return null;
  try {
    const parsed: unknown = JSON.parse(text);
    return parsed !== null && typeof parsed === "object" && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

function idOf(input: unknown): string {
  if (input === null || typeof input !== "object") return "";
  const id = (input as Record<string, unknown>).id;
  return typeof id === "string" ? id : "";
}

function count(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

function quoted(text: string): string {
  return `\`${text}\``;
}

/** A JSON array of strings, or `null` when it is anything else. */
function stringList(value: unknown): string[] | null {
  return Array.isArray(value) && value.every((entry) => typeof entry === "string") ? (value as string[]) : null;
}

function named(tool: string, path: string): string {
  return path === "" ? tool : `${tool} ${path}`;
}

/** The row for `tool` (the bare tool name, without its server) called with `input`, or `null` for a tool
 *  this table does not know. `result` is `null` while the call runs. An error, a call still running and an
 *  answer that cannot be read give the name and the path only; exec calls give their kind. A path under
 *  `projectDir` is drawn relative to it. */
export function editorRow(tool: string, input: unknown, result: Answer | null, projectDir = ""): string | null {
  const rel = relTo(projectDir);
  const path = rel(pathOf(input));
  switch (tool) {
    case "nvim_exec": {
      const fields = input !== null && typeof input === "object" ? (input as Record<string, unknown>) : {};
      const kind = fields.kind === "lua" ? "Lua" : fields.kind === "ex" ? "Ex" : null;
      if (kind === null) return tool;
      if (result === null || result.isError || typeof fields.code !== "string") return kind;
      if (kind === "Ex") return "ran Ex commands in the editor";
      const count = splitLines(fields.code).length;
      return `ran ${count} ${count === 1 ? "line" : "lines"} of Lua in the editor`;
    }
    case "open_file": {
      if (result === null || result.isError) return named(tool, path);
      if (path === "") return tool;
      const line = lineOf(input);
      const place = line === null ? path : `${path}:${line}`;
      const answer = answerObject(result.content);
      if (answer?.on_screen === false) return `opened ${place} in the editor, which is not on screen`;
      if (answer?.other_window === true) return `opened ${place} in window 1's editor`;
      return `showed ${place} in the editor`;
    }
    case "read_buffer": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      if (answer === null || path === "") return named(tool, path);
      return `read ${path} from the editor${answer.modified === true ? " (unsaved)" : ""}`;
    }
    case "diagnostics": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      const entries = answer === null ? null : answer.entries;
      if (!Array.isArray(entries)) return named(tool, path);
      const leftOut = typeof answer?.left_out === "number" && answer.left_out > 0 ? Math.floor(answer.left_out) : 0;
      const count = entries.length + leftOut;
      return `${count} ${count === 1 ? "diagnostic" : "diagnostics"}`;
    }
    case "usages": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      const symbol = symbolOf(input);
      const parts = [answer?.definitions, answer?.implementations, answer?.references].filter(Array.isArray);
      if (parts.length === 0 || symbol === "") return named(tool, path);
      const told = parts.reduce((sum: number, part: unknown[]) => sum + part.length, 0);
      const leftOut = typeof answer?.left_out === "number" && answer.left_out > 0 ? Math.floor(answer.left_out) : 0;
      const count = told + leftOut;
      return `${count} ${count === 1 ? "usage" : "usages"} of ${symbol}`;
    }
    case "hover": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      const symbol = symbolOf(input);
      if (typeof answer?.text !== "string" || symbol === "") return named(tool, path);
      return `hover on ${symbol}`;
    }
    case "symbols": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      if (!Array.isArray(answer?.symbols)) return named(tool, path);
      const query = input !== null && typeof input === "object" ? (input as Record<string, unknown>).query : null;
      if (path === "" && typeof query !== "string") return tool;
      const leftOut = typeof answer.left_out === "number" && answer.left_out > 0 ? Math.floor(answer.left_out) : 0;
      const count = answer.symbols.length + leftOut;
      return `${count} ${count === 1 ? "symbol" : "symbols"} ${path === "" ? `for ${query}` : `in ${path}`}`;
    }
    case "lsp_rename": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      const symbol = symbolOf(input);
      if (!Array.isArray(answer?.files) || symbol === "") return named(tool, path);
      return `proposed renaming \`${symbol}\` in ${count(answer.files.length, "file")}`;
    }
    case "format": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      if (typeof answer?.proposal !== "string" || path === "") return named(tool, path);
      return `proposed formatting \`${path}\``;
    }
    case "lsp_code_actions": {
      if (result === null || result.isError) return named(tool, path);
      const answer = answerObject(result.content);
      if (!Array.isArray(answer?.actions)) return named(tool, path);
      return count(answer.actions.length, "code action");
    }
    case "apply_code_action": {
      const id = idOf(input);
      if (result === null || result.isError) return named(tool, id);
      const answer = answerObject(result.content);
      if (typeof answer?.proposal !== "string") return named(tool, id);
      return typeof answer.title === "string" && answer.title !== "" ? `proposed "${answer.title}"` : "proposed a code action";
    }
    case "apply_proposal": {
      const id = idOf(input);
      if (result === null) return named(tool, id);
      const answer = answerObject(result.content);
      const applied = stringList(answer?.applied);
      if (answer === null || applied === null) return result.isError ? `could not apply ${quoted(id)}` : named(tool, id);
      const failed = answer.failed !== null && typeof answer.failed === "object" ? (answer.failed as { path?: unknown }).path : undefined;
      const notTried = stringList(answer.not_tried) ?? [];
      const stopped = typeof failed === "string" ? `, \`${rel(failed)}\` failed` : "";
      if (applied.length === 0) return `applied nothing${stopped}`;
      if (failed === undefined && notTried.length === 0) return `applied ${quoted(id)} to ${count(applied.length, "file")}`;
      const of = applied.length + notTried.length + (failed === undefined ? 0 : 1);
      return `applied ${quoted(id)} to ${applied.length} of ${count(of, "file")}${stopped}`;
    }
    case "editor_state":
      return result === null || result.isError ? tool : "looked at the editor";
    default:
      return null;
  }
}

function objectOf(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
}

function natural(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function positive(value: unknown): value is number {
  return natural(value) && value > 0;
}

function optionalString(value: unknown): boolean {
  return value === undefined || typeof value === "string";
}

function items(value: unknown, render: (item: Record<string, unknown>) => string | null): string[] | null {
  if (!Array.isArray(value)) return null;
  const lines: string[] = [];
  for (const entry of value) {
    const item = objectOf(entry);
    const line = item === null ? null : render(item);
    if (line === null) return null;
    lines.push(line);
  }
  return lines;
}

function position(item: Record<string, unknown>, rel: Rel): string | null {
  return typeof item.path === "string" && positive(item.line) && positive(item.column)
    ? `${rel(item.path)}:${item.line}:${item.column}` : null;
}

function withMore(lines: string[], leftOut: number, empty: string, noun = ""): string[] {
  return [...(lines.length === 0 && leftOut === 0 ? [empty] : lines), ...(leftOut > 0 ? [`and ${leftOut} more${noun}`] : [])];
}

function partNotes(answer: Record<string, unknown>, lines: string[]): boolean {
  for (const field of ["missing", "unsupported"]) {
    if (answer[field] === undefined) continue;
    const parts = stringList(answer[field]);
    if (parts === null) return false;
    if (parts.length > 0) lines.push(`${field}: ${parts.join(", ")}`);
  }
  return true;
}

/** The editor's state. Its first line is the root itself, which stays absolute: it says which project. */
function stateBody(answer: Record<string, unknown>, rel: Rel): string | null {
  if (typeof answer.root !== "string" || typeof answer.mode !== "string" || !natural(answer.other_buffers) ||
    (answer.buffers_left_out !== undefined && !natural(answer.buffers_left_out))) return null;
  const buffers = items(answer.buffers, (buffer) => {
    if (typeof buffer.path !== "string" || typeof buffer.modified !== "boolean" || !natural(buffer.lines) ||
      typeof buffer.filetype !== "string" || typeof buffer.shown !== "boolean") return null;
    return `${rel(buffer.path)}${buffer.modified ? " (unsaved)" : ""} · ${count(buffer.lines, "line")}` +
      `${buffer.filetype === "" ? "" : ` · ${buffer.filetype}`} · ${buffer.shown ? "shown" : "not shown"}`;
  });
  if (buffers === null) return null;
  const current = objectOf(answer.current);
  const currentText = typeof answer.current === "string" ? answer.current
    : current !== null && typeof current.path === "string" && positive(current.line) ? `${rel(current.path)}:${current.line}` : null;
  if (currentText === null) return null;
  const lines = [answer.root, ...withMore(buffers, (answer.buffers_left_out as number | undefined) ?? 0, "0 buffers", " buffers")];
  if (answer.other_buffers > 0) lines.push(`${answer.other_buffers} other buffers`);
  lines.push(`current: ${currentText} · ${answer.mode}`);
  if (typeof answer.selection === "string") lines.push(`selection: ${answer.selection}`);
  else if (answer.selection !== null) {
    const selection = objectOf(answer.selection);
    if (selection === null || typeof selection.path !== "string" || !positive(selection.start_line) ||
      !positive(selection.end_line) || typeof selection.text !== "string") return null;
    lines.push(`selection: ${rel(selection.path)}:${selection.start_line}-${selection.end_line}`, selection.text);
  }
  return lines.join("\n");
}

/** A readable expanded answer, or no replacement when it is not a known successful answer.
 *  This is display text only: the result stays intact for detailed view and the host owns delivery to the model.
 *  Every returned character remains untrusted and must be drawn through `Shown`, never Markdown. A path under
 *  `projectDir` is drawn relative to it, as in the row above the body. */
export function editorResult(tool: string, input: unknown, result: Answer | null, projectDir = ""): string | null {
  const rel = relTo(projectDir);
  if (result === null || result.isError) return null;
  const answer = answerObject(result.content);
  if (answer === null) return null;
  switch (tool) {
    case "open_file": {
      const path = rel(pathOf(input));
      const places: Record<string, string> = {
        existing: "an existing window", agent_window: "the agent's window", new_split: "a new split",
      };
      if (path === "" || answer.shown !== true || !positive(answer.line) || typeof answer.where !== "string" ||
        !Object.prototype.hasOwnProperty.call(places, answer.where) || !optionalString(answer.note) ||
        (answer.on_screen !== undefined && typeof answer.on_screen !== "boolean") ||
        (answer.other_window !== undefined && typeof answer.other_window !== "boolean")) return null;
      const away = answer.on_screen === false || answer.other_window === true;
      const editor = answer.other_window === true ? "window 1's editor" : "the editor";
      return `${path}:${answer.line}\n${away ? "opened" : "shown"} in ${places[answer.where]} in ${editor}` +
        `${answer.on_screen === false ? ", which is not on screen" : ""}${answer.note ? `\n${answer.note}` : ""}`;
    }
    case "read_buffer": {
      if (typeof answer.path !== "string" || typeof answer.modified !== "boolean" || typeof answer.text !== "string" ||
        !positive(answer.first_line) || !natural(answer.last_line) || !optionalString(answer.left_out)) return null;
      const header = `${rel(answer.path)}:${answer.first_line}-${answer.last_line}${answer.modified ? " (unsaved)" : ""}`;
      return `${header}\n${answer.text}${answer.left_out ? `${answer.text.endsWith("\n") ? "" : "\n"}${answer.left_out}` : ""}`;
    }
    case "editor_state":
      return stateBody(answer, rel);
    case "diagnostics": {
      const modified = stringList(answer.modified_files);
      if (modified === null || !natural(answer.left_out) ||
        (answer.baseline_cut !== undefined && typeof answer.baseline_cut !== "boolean")) return null;
      const entries = items(answer.entries, (entry) => {
        const place = position(entry, rel);
        return place !== null && typeof entry.severity === "string" && typeof entry.message === "string"
          ? `${place} ${entry.severity} ${entry.message}` : null;
      });
      if (entries === null) return null;
      const lines = withMore(entries, answer.left_out, "0 diagnostics");
      if (modified.length > 0) lines.push(`unsaved: ${modified.map(rel).join(", ")}`);
      if (answer.baseline_cut === true) lines.push("the turn's diagnostic comparison is incomplete");
      return lines.join("\n");
    }
    case "usages": {
      if (typeof answer.symbol !== "string" || !natural(answer.left_out) ||
        stringList(answer.missing) === null || stringList(answer.unsupported) === null) return null;
      const lines: string[] = [];
      for (const [part, label] of [["definitions", "definition"], ["implementations", "implementation"], ["references", "reference"]]) {
        if (answer[part] === undefined) continue;
        const entries = items(answer[part], (entry) => {
          const place = position(entry, rel);
          return place !== null && optionalString(entry.text) ? `${label}: ${place}${entry.text ? ` ${entry.text}` : ""}` : null;
        });
        if (entries === null) return null;
        lines.push(...entries);
      }
      const body = withMore(lines, answer.left_out, "0 usages");
      return partNotes(answer, body) ? body.join("\n") : null;
    }
    case "hover":
      return typeof answer.symbol === "string" && typeof answer.text === "string" && typeof answer.cut === "boolean"
        ? `${answer.text}${answer.cut ? "\nhover text was cut" : ""}` : null;
    case "symbols": {
      if (!natural(answer.left_out)) return null;
      const symbols = items(answer.symbols, (symbol) => {
        const place = position(symbol, rel);
        if (place === null || ![symbol.name, symbol.kind, symbol.container].every(optionalString)) return null;
        return `${place}${symbol.kind ? ` ${symbol.kind}` : ""}${symbol.name ? ` ${symbol.name}` : ""}` +
          `${symbol.container ? ` (${symbol.container})` : ""}`;
      });
      return symbols === null ? null : withMore(symbols, answer.left_out, "0 symbols").join("\n");
    }
    case "lsp_rename":
    case "format":
    case "apply_code_action": {
      if (typeof answer.proposal !== "string" || !natural(answer.left_out_lines) || !optionalString(answer.title) ||
        (answer.command_not_run !== undefined && typeof answer.command_not_run !== "boolean")) return null;
      const files = items(answer.files, (file) => typeof file.path === "string" && typeof file.hunks === "string"
        ? `${rel(file.path)}\n${file.hunks}` : null);
      if (files === null) return null;
      const lines = [...(answer.title ? [answer.title] : []), `proposal ${answer.proposal}`,
        ...withMore(files, answer.left_out_lines, "0 files", " hunk lines")];
      if (answer.command_not_run === true) lines.push("the code action's command was not run");
      return lines.join("\n");
    }
    case "lsp_code_actions": {
      const actions = items(answer.actions, (action) => {
        if (typeof action.id !== "string" || typeof action.title !== "string" || typeof action.applies !== "boolean" ||
          (action.kind !== null && typeof action.kind !== "string") ||
          (action.disabled !== null && typeof action.disabled !== "string")) return null;
        return `${action.id}: ${action.title}${action.kind ? ` (${action.kind})` : ""}` +
          `${action.disabled ? ` · ${action.disabled}` : ""}${action.applies ? "" : " · cannot apply"}`;
      });
      if (actions === null) return null;
      const lines = actions.length === 0 ? ["0 code actions"] : actions;
      return partNotes(answer, lines) ? lines.join("\n") : null;
    }
    case "apply_proposal": {
      const applied = stringList(answer.applied);
      const notTried = stringList(answer.not_tried);
      if (applied === null || notTried === null || answer.failed !== undefined) return null;
      return [...(applied.length === 0 ? ["applied nothing"] : applied.map((path) => `applied ${rel(path)}`)),
        ...notTried.map((path) => `not tried: ${rel(path)}`)].join("\n");
    }
    default:
      return null;
  }
}
