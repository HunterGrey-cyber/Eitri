import { liveTasks } from "./band";
import type { BackgroundTaskRecord, ToolCallRecord } from "./types";

/** The tools a non-Claude agent's tab names its calls by: Codex's item types, and `acp:<kind>` for an ACP
 *  agent. Their cards take no reason with a deny, and their rows may name the subagent that made them. */
const CODEX_TOOLS = new Set(["commandExecution", "fileChange", "mcpToolCall", "webSearch", "collabAgentToolCall"]);

export function isExternalTool(name: string): boolean {
  return CODEX_TOOLS.has(name) || name.startsWith("acp:");
}

/** Rows (never cards) of Codex's requests that Eitri answered itself, which may name a subagent too. */
const CODEX_ANSWERED_REQUESTS = new Set(["permissions", "mcpElicitation", "requestUserInput"]);

/** The provider's subagent label, or Claude's ancestor description when its call tree is available. */
export function subagentOf(toolName: string, input: unknown, origin?: {
  toolUseId: string | null;
  toolCalls: ToolCallRecord[];
  backgroundTasks: BackgroundTaskRecord[];
}): string | null {
  if (!isExternalTool(toolName) && !CODEX_ANSWERED_REQUESTS.has(toolName) && origin) {
    const path = toolCallPath(origin.toolCalls, origin.toolUseId);
    if (!path || path.length < 2) return null;
    for (const parent of path.slice(0, -1).reverse()) {
      if ((parent.name === "Agent" || parent.name === "Task") && parent.input && typeof parent.input === "object") {
        const description = (parent.input as Record<string, unknown>).description;
        if (typeof description === "string" && description.trim()) return description;
      }
    }
    const workflows = liveTasks(origin.backgroundTasks).filter((task) => task.kind === "workflow");
    const name = workflows.length === 1 ? workflows[0].workflow_name : null;
    return `a background agent${name ? ` (workflow ${name} running)` : ""}`;
  }
  if (!(isExternalTool(toolName) || CODEX_ANSWERED_REQUESTS.has(toolName)) || input === null || typeof input !== "object")
    return null;
  const label = (input as Record<string, unknown>).subagent;
  return typeof label === "string" && label !== "" ? label : null;
}

/** The stored ancestry of a call, including an unknown parent's placeholder when one was received. */
export function toolCallPath(calls: ToolCallRecord[], id: string | null): ToolCallRecord[] | null {
  if (!id) return null;
  for (const call of calls) {
    if (call.toolUseId === id) return [call];
    const nested = call.children && toolCallPath(call.children.toolCalls, id);
    if (nested) return [call, ...nested];
  }
  return null;
}

/** What a tab's agent is called in a sentence: `Codex`, or the name an ACP agent was registered under. */
export function agentTitle(agent: string | undefined): string {
  if (agent === undefined || agent === "codex") return "Codex";
  return agent.startsWith("acp:") ? agent.slice(4) : agent;
}

const SHORT_NAMES: Record<string, string> = {
  commandExecution: "command",
  fileChange: "file change",
  mcpToolCall: "tool call",
  webSearch: "web search",
  collabAgentToolCall: "subagent",
  permissions: "extra permissions",
  mcpElicitation: "tool server form",
  requestUserInput: "question",
};

/** The word a collapsed run of calls uses for a tool: Codex's item types read as plain words, every other
 *  name is shown as it is. */
export function shortToolName(name: string): string {
  return Object.prototype.hasOwnProperty.call(SHORT_NAMES, name) ? SHORT_NAMES[name] : name;
}
