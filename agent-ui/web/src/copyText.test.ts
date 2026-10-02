import { describe, expect, it } from "vitest";
import { outputText, primaryText } from "./copyText";
import type { TimelineItem } from "./timeline";

const tool = (name: string, input: unknown, content: unknown = "out"): TimelineItem => ({
  kind: "tool", seq: 1, key: "t-1", call: { seq: 1, toolUseId: "t", name, input, result: { content, isError: false } },
});

describe("N3: what y and Y copy", () => {
  it("y copies what a person would paste: the markdown, the command, the path", () => {
    expect(primaryText({ kind: "message", seq: 1, key: "m-1", text: "**bold** `code`" })).toBe("**bold** `code`");
    expect(primaryText(tool("Bash", { command: "cargo test --lib" }))).toBe("cargo test --lib");
    for (const name of ["Read", "Edit", "Write", "NotebookEdit"]) expect(primaryText(tool(name, { file_path: "/p/a.rs" })), name).toBe("/p/a.rs");
    expect(primaryText(tool("mcp__x__y", { k: "v" }))).toBe('{\n  "k": "v"\n}');
    const card: TimelineItem = { kind: "permission", seq: 2, key: "p-2", request: { seq: 2, permissionId: "p", toolUseId: null, toolName: "Bash", input: { command: "rm x" } } };
    expect(primaryText(card)).toBe("rm x");
    const run: TimelineItem = { kind: "run", seq: 3, key: "r-3", calls: [
      { seq: 3, toolUseId: "a", name: "Read", input: {}, result: { content: "", isError: false } },
      { seq: 4, toolUseId: "b", name: "Read", input: {}, result: { content: "", isError: false } },
    ] };
    expect(primaryText(run)).toBe("Read ×2");
  });

  it("Y copies a tool's whole output, never the cut one, and nothing else has an output", () => {
    const long = "x".repeat(10000);
    expect(outputText(tool("Bash", { command: "yes" }, long))).toBe(long);
    expect(outputText(tool("Read", {}, [{ type: "text", text: "a" }]))).toContain('"text": "a"');
    expect(outputText({ kind: "prompt", seq: 1, key: "u-1", text: "hi" })).toBeNull();
  });

  it("y on a turn ending copies the sentence the row shows, the provider's words included", () => {
    const ending: TimelineItem = {
      kind: "ending", seq: 4, key: "e-4",
      ending: { seq: 4, turnId: "t1", kind: "limit_reached", reason: "max_turns", apiErrorStatus: null, message: "Reached maximum number of turns (3)" },
    };
    expect(primaryText(ending)).toBe("stopped at the turn limit: Reached maximum number of turns (3)");
    expect(outputText(ending)).toBeNull();
  });
});
