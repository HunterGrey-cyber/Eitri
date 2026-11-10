import { outputText, toolPrimary } from "./copyText";
import { editPreviewsOf, hunksOf } from "./hunks";
import { fenced, pathsIn } from "./paths";
import type { TimelineItem } from "./timeline";

export type YankObject = "m" | "o" | "c" | "`" | "f" | "h";
export type YankResult = { text: string } | { refuse: string } | { pick: { label: string; text: string }[] };

export function yankRows(texts: readonly string[]): string { return texts.join("\n\n"); }

export function toolInput(name: string, input: unknown): string {
  const fields = input && typeof input === "object" ? input as Record<string, unknown> : {};
  const field = ({ NotebookEdit: "notebook_path", Grep: "pattern", Glob: "pattern", WebFetch: "url", WebSearch: "query" } as Record<string, string>)[name];
  if (field) return typeof fields[field] === "string" ? fields[field] as string : JSON.stringify(input, null, 2) ?? "null";
  return toolPrimary(name, input) ?? "null";
}

export function yankObject(item: TimelineItem, scope: "i" | "a", object: YankObject, ctx: {
  agentLabel: string; links: string[]; codeBlocks: { code: string; lang: string | null }[]; currentHunk: number | null;
}): YankResult {
  const key = `${scope}${object}`;
  const kind = ({ message: "reply", tool: "tool call", permission: "permission card", thinking: "thought",
    run: "group of tool calls", notice: "notice", wake: "notice", ending: "notice", prompt: "prompt" })[item.kind];
  const tryObject = item.kind === "message" || item.kind === "prompt" ? "im" : item.kind === "tool" || item.kind === "permission" ? "ic" : "yy";
  const hint = item.kind === "run" ? "press Enter to expand, then try yio or yic" : `try ${tryObject}`;
  const wrong = (wanted: string): YankResult => ({ refuse: `${key}: the cursor is on a ${kind}, not a ${wanted} — ${hint}` });
  const choose = (entries: { label: string; text: string }[]): YankResult => entries.length === 1 ? { text: entries[0].text } : { pick: entries };
  const tool = item.kind === "tool" ? { name: item.call.name, input: item.call.input }
    : item.kind === "permission" ? { name: item.request.toolName, input: item.request.input } : null;
  switch (object) {
    case "m":
      if (item.kind !== "prompt" && item.kind !== "message") return wrong("message");
      if (item.text === "") return { refuse: `${key}: the message is empty` };
      return { text: scope === "i" ? item.text : `**${item.kind === "prompt" ? "You" : ctx.agentLabel}**\n\n${item.text}` };
    case "o": {
      if (item.kind !== "tool") return wrong("tool call");
      const output = outputText(item);
      if (output === null || output === "") return { refuse: `${key}: this tool call has no output` };
      return { text: scope === "i" ? output : `${item.call.name}: ${toolInput(item.call.name, item.call.input)}\n${output.replace(/\n$/, "")}\n(${item.call.result?.isError ? "error" : "ok"})` };
    }
    case "c":
      if (tool === null) return wrong("tool call or permission card");
      return { text: `${scope === "a" ? `${tool.name}: ` : ""}${toolInput(tool.name, tool.input)}` };
    case "`":
      if (ctx.codeBlocks.length === 0) return wrong("code block");
      return choose(ctx.codeBlocks.map(({ code, lang }, i) => ({ label: `code block ${i + 1}${lang ? ` (${lang})` : ""}`,
        text: scope === "i" ? code : fenced(code).replace("\n", `${lang ?? ""}\n`) })));
    case "f": {
      const paths = pathsIn(item);
      const entries = paths.map((ref) => ({ label: `${ref.path}${ref.line === null ? "" : `:${ref.line}`}`,
        text: `${ref.path}${scope === "a" && ref.line !== null ? `:${ref.line}` : ""}` }));
      if (entries.length === 0) {
        entries.push(...ctx.links.map((url) => ({ label: url, text: url })));
      }
      return entries.length === 0 ? wrong("path or URL") : choose(entries);
    }
    case "h": {
      const previews = editPreviewsOf(item);
      const entries = (previews ?? []).flatMap((preview) => hunksOf(preview.diff!).map(({ start, end }) => ({
        label: "", text: (scope === "a" ? `--- a/${preview.filePath}\n+++ b/${preview.filePath}\n` : "") +
          preview.diff!.slice(start, end).map((line) => `${line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " "}${line.text}`).join("\n"),
      }))).map((entry, i) => ({ ...entry, label: `hunk ${i + 1}` }));
      if (entries.length === 0) return { refuse: `${key}: no diff on this row` };
      return ctx.currentHunk !== null && entries[ctx.currentHunk] ? { text: entries[ctx.currentHunk].text } : choose(entries);
    }
  }
}
