import { describe, expect, it } from "vitest";
import { isExternalTool } from "./externalTools";
import fixture from "./fixtures/codex-cards.json";

/** Every call row and card name Rust's Codex provider put in the fixture it writes. */
function namesInFixture(): Set<string> {
  const names = new Set<string>();
  for (const c of fixture.cases) {
    const state = c.snapshot.state as { toolCalls?: Array<{ name: string }>; pendingPermissions?: Array<{ toolName: string }> };
    for (const call of state.toolCalls ?? []) names.add(call.name);
    for (const card of state.pendingPermissions ?? []) names.add(card.toolName);
  }
  return names;
}

describe("the tool names a Codex tab uses", () => {
  it("every row and card name Rust writes is an external tool here", () => {
    const names = namesInFixture();
    // A fixture that lost its rows would pass vacuously.
    expect(names.size).toBeGreaterThanOrEqual(5);
    const unknown = [...names].filter((name) => !isExternalTool(name));
    expect(unknown).toEqual([]);
  });

  it("the fixture holds each name the web set lists", () => {
    const names = namesInFixture();
    for (const name of ["commandExecution", "fileChange", "mcpToolCall", "webSearch", "collabAgentToolCall"]) {
      expect(names.has(name), name).toBe(true);
      expect(isExternalTool(name)).toBe(true);
    }
  });
});
