// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { renderToolCall } from "./toolRegistry";
import type { ToolCallRecord } from "./types";

function call(overrides: Partial<ToolCallRecord>): ToolCallRecord {
  return { toolUseId: "toolu_1", name: "Bash", input: { command: "echo hi" }, result: null, ...overrides };
}

describe("renderToolCall", () => {
  it("renders a registered tool (Bash) using its own config", () => {
    render(<>{renderToolCall(call({ name: "Bash", input: { command: "echo hi" } }))}</>);
    expect(screen.getByText(/echo hi/)).toBeTruthy();
  });

  it("special-cases Skill as a one-line 'used skill' summary", () => {
    render(<>{renderToolCall(call({ name: "Skill", input: { command: "brainstorming" } }))}</>);
    expect(screen.getByText(/Used skill: brainstorming/)).toBeTruthy();
  });

  it("falls back to a generic pretty-printed renderer for an unrecognized tool name", () => {
    render(<>{renderToolCall(call({ name: "SomeFutureToolNobodyRegisteredYet", input: { x: 1 } }))}</>);
    expect(screen.getByText(/SomeFutureToolNobodyRegisteredYet/)).toBeTruthy();
    expect(screen.getByText(/"x": 1/)).toBeTruthy();
  });
});
