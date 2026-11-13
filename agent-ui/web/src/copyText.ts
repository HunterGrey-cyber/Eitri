import { runSummary } from "./display";
import { editorCardText, noticeText, wakeText, noticeBody, type TimelineItem } from "./timeline";
import { turnEndingText } from "./turnEnding";
import { formatResultContent } from "./toolRegistry";

const PATH_TOOLS = new Set(["Read", "Edit", "Write", "NotebookEdit"]);

export function toolPrimary(name: string, input: unknown): string {
  const fields = input && typeof input === "object" ? (input as Record<string, unknown>) : {};
  if (name === "Bash" && typeof fields.command === "string") return fields.command;
  if (PATH_TOOLS.has(name) && typeof fields.file_path === "string") return fields.file_path;
  return JSON.stringify(input, null, 2);
}

/** What `y` copies: the text a person would paste back. */
export function primaryText(item: TimelineItem): string {
  switch (item.kind) {
    case "prompt":
    case "message":
    case "planmark":
      return item.text;
    case "plan":
      // The plan as Claude wrote it: its Markdown source.
      return item.plan.plan;
    case "tool":
      return toolPrimary(item.call.name, item.call.input);
    case "permission":
      return editorCardText(item.request) ?? toolPrimary(item.request.toolName, item.request.input);
    case "run":
      return runSummary(item.calls);
    case "ending":
      return turnEndingText(item.ending, item.ending.resetsAt ?? null);
    case "wake":
      return [wakeText(item), item.body].filter(Boolean).join("\n");
    case "notice":
      return [noticeText(item.notice), noticeBody(item.notice)].filter(Boolean).join("\n");
    case "thinking":
      // The thought's own text, whether or not its fold is open: the row stands for it.
      return item.block.text;
  }
}

/** `Y`: a tool's whole output as it arrived (never the head/tail cut), or `null`. */
export function outputText(item: TimelineItem): string | null {
  if (item.kind !== "tool" || item.call.result === null) return null;
  return formatResultContent(item.call.result.content);
}
