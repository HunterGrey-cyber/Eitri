import { describe, expect, it } from "vitest";
import { findMatch, rowSearchText } from "./search";
import type { TimelineItem } from "./timeline";

const rows: TimelineItem[] = [
  { kind: "prompt", seq: 1, key: "u-1", text: "Fix the parser" },
  { kind: "message", seq: 2, key: "m-2", text: "Looking at parser.rs" },
  {
    kind: "tool",
    seq: 3,
    key: "t-3",
    call: { seq: 3, toolUseId: "t", name: "Bash", input: { command: "cargo test" }, result: { content: "test result: FAILED", isError: true } },
  },
  { kind: "permission", seq: 4, key: "p-4", request: { seq: 4, permissionId: "p", toolUseId: null, toolName: "Write", input: { file_path: "src/parser.rs" } } },
];

describe("R4 search", () => {
  it("reads what each row shows: text, a tool's input and result, a card's tool and input", () => {
    expect(rowSearchText(rows[0])).toBe("Fix the parser");
    expect(rowSearchText(rows[2])).toContain("cargo test");
    expect(rowSearchText(rows[2])).toContain("FAILED");
    expect(rowSearchText(rows[3])).toContain("src/parser.rs");
  });

  /* P2 (Task 13): a collapsed run's own row shows a summary, not any one call's input -- so `/`
     still finds it by what a folded call would show, each name and input. */
  it("reads a collapsed run by its calls, not just its summary", () => {
    const run: TimelineItem = {
      kind: "run",
      seq: 5,
      key: "r-5",
      calls: [
        { seq: 5, toolUseId: "a", name: "Read", input: { file_path: "src/parser.rs" }, result: { content: "ok", isError: false } },
        { seq: 6, toolUseId: "b", name: "Bash", input: { command: "cargo test" }, result: { content: "ok", isError: false } },
      ],
    };
    expect(rowSearchText(run)).toContain("parser.rs");
    expect(rowSearchText(run)).toContain("cargo test");
  });

  it("finds forward from the cursor, wraps, and is smartcase", () => {
    expect(findMatch(rows, "parser", 0, 1, true)).toBe(0);
    expect(findMatch(rows, "parser", 0, 1, false)).toBe(1);
    expect(findMatch(rows, "parser", 3, 1, false), "wraps past the end").toBe(0);
    expect(findMatch(rows, "parser", 0, -1, false), "backward wraps too").toBe(3);
    expect(findMatch(rows, "Fix", 1, 1, false)).toBe(0);
    expect(findMatch(rows, "fix", 1, 1, false), "lowercase: case-insensitive").toBe(0);
    expect(findMatch(rows, "FIX", 1, 1, false), "uppercase: case-sensitive").toBeNull();
    expect(findMatch(rows, "", 0, 1, true)).toBeNull();
  });
});
