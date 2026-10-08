/** The tools a non-Claude agent's tab names its calls by: Codex's item types, and `acp:<kind>` for an ACP
 *  agent. Their cards take no reason with a deny, and their rows may name the subagent that made them. */
const CODEX_TOOLS = new Set(["commandExecution", "fileChange", "mcpToolCall", "webSearch", "collabAgentToolCall"]);

export function isExternalTool(name: string): boolean {
  return CODEX_TOOLS.has(name) || name.startsWith("acp:");
}

/** The task name of the subagent whose thread made a call, when Rust labelled the input with one. */
export function subagentOf(toolName: string, input: unknown): string | null {
  if (!isExternalTool(toolName) || input === null || typeof input !== "object") return null;
  const label = (input as Record<string, unknown>).subagent;
  return typeof label === "string" && label !== "" ? label : null;
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
};

/** The word a collapsed run of calls uses for a tool: Codex's item types read as plain words, every other
 *  name is shown as it is. */
export function shortToolName(name: string): string {
  return Object.prototype.hasOwnProperty.call(SHORT_NAMES, name) ? SHORT_NAMES[name] : name;
}
