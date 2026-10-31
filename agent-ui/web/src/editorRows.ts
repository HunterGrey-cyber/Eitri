/** The one-line row an editor tool's call gets in a tab: what happened, in words, from the call's input and
 *  its answer. The returned text is plain (it is drawn as text, never as Markdown) and holds whatever the
 *  model wrote in the input, so a caller draws it through `revealHidden`. */

import { splitLines } from "./diff";

type Answer = { content: unknown; isError: boolean };

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
 *  answer that cannot be read give the name and the path only; exec calls give their kind. */
export function editorRow(tool: string, input: unknown, result: Answer | null): string | null {
  const path = pathOf(input);
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
      const stopped = typeof failed === "string" ? `, \`${failed}\` failed` : "";
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
