import { runSummary } from "./display";
import type { TimelineItem } from "./timeline";
import { formatResultContent } from "./toolRegistry";

const PATH_TOOLS = new Set(["Read", "Edit", "Write", "NotebookEdit"]);

function toolPrimary(name: string, input: unknown): string {
  const fields = input && typeof input === "object" ? (input as Record<string, unknown>) : {};
  if (name === "Bash" && typeof fields.command === "string") return fields.command;
  if (PATH_TOOLS.has(name) && typeof fields.file_path === "string") return fields.file_path;
  return JSON.stringify(input, null, 2);
}

/** N3's `y` (ruling 28): the text a person would paste back. */
export function primaryText(item: TimelineItem): string {
  switch (item.kind) {
    case "prompt":
    case "message":
      return item.text;
    case "tool":
      return toolPrimary(item.call.name, item.call.input);
    case "permission":
      return toolPrimary(item.request.toolName, item.request.input);
    case "run":
      return runSummary(item.calls);
  }
}

/** N3's `Y`: a tool's whole output as it arrived (never the head/tail cut), or `null`. */
export function outputText(item: TimelineItem): string | null {
  if (item.kind !== "tool" || item.call.result === null) return null;
  return formatResultContent(item.call.result.content);
}
