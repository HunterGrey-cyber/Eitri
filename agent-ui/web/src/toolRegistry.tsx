import type { ReactNode } from "react";
import type { ToolCallRecord } from "./types";

export type ToolRenderConfig = {
  label: string;
  render: (input: unknown, result: unknown, isError: boolean) => ReactNode;
};

function commandOf(input: unknown): string {
  if (input && typeof input === "object" && "command" in input) {
    return String((input as { command: unknown }).command);
  }
  return JSON.stringify(input);
}

function pathOf(input: unknown, key: string): string {
  if (input && typeof input === "object" && key in input) {
    return String((input as Record<string, unknown>)[key]);
  }
  return JSON.stringify(input);
}

export const TOOL_REGISTRY: Record<string, ToolRenderConfig> = {
  Bash: {
    label: "Run command",
    render: (input) => <pre className="tool-card tool-card-bash">$ {commandOf(input)}</pre>,
  },
  Read: {
    label: "Read file",
    render: (input) => <div className="tool-card">Read: {pathOf(input, "file_path")}</div>,
  },
  Write: {
    label: "Write file",
    render: (input) => <div className="tool-card">Write: {pathOf(input, "file_path")}</div>,
  },
  Edit: {
    label: "Edit file",
    render: (input) => <div className="tool-card">Edit: {pathOf(input, "file_path")}</div>,
  },
  Grep: {
    label: "Search",
    render: (input) => <div className="tool-card">Grep: {pathOf(input, "pattern")}</div>,
  },
  Glob: {
    label: "Find files",
    render: (input) => <div className="tool-card">Glob: {pathOf(input, "pattern")}</div>,
  },
  WebFetch: {
    label: "Fetch URL",
    render: (input) => <div className="tool-card">Fetch: {pathOf(input, "url")}</div>,
  },
  WebSearch: {
    label: "Web search",
    render: (input) => <div className="tool-card">Search: {pathOf(input, "query")}</div>,
  },
};

export function renderToolCall(call: ToolCallRecord): ReactNode {
  // Skill calls are ordinary tool_use blocks with name === "Skill" -- there is no distinct wire
  // event for this (confirmed real behavior, see the agent-v2 spec) -- so this is purely a
  // rendering-layer special case, not something the reducer or Rust side needs to know about.
  if (call.name === "Skill") {
    const skillName = commandOf(call.input);
    return <div className="tool-card tool-card-skill">Used skill: {skillName}</div>;
  }

  const config = TOOL_REGISTRY[call.name];
  const errorFlag = call.result?.isError ?? false;

  if (config) {
    return (
      <div className="tool-call" data-tool-name={call.name}>
        {config.render(call.input, call.result?.content ?? null, errorFlag)}
        {call.result && errorFlag && <div className="tool-error">error: {String(call.result.content)}</div>}
      </div>
    );
  }

  // Generic fallback for unrecognized tools
  return (
    <div className="tool-call" data-tool-name={call.name}>
      <details className="tool-card tool-card-generic">
        <summary>Unrecognized tool: {call.name}</summary>
        <pre>{JSON.stringify(call.input, null, 2)}</pre>
      </details>
      {call.result && errorFlag && <div className="tool-error">error: {String(call.result.content)}</div>}
    </div>
  );
}
