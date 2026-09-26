import { describe, expect, it } from "vitest";
import { parsePath, pathsIn, viewText } from "./paths";
import type { TimelineItem } from "./timeline";

describe("N2 paths", () => {
  it("reads a tool's own path field, with no guessing", () => {
    const read: TimelineItem = { kind: "tool", seq: 1, key: "t-1", call: { seq: 1, toolUseId: "t", name: "Read", input: { file_path: "/p/src/a.rs" }, result: null } };
    expect(pathsIn(read)).toEqual([{ path: "/p/src/a.rs", line: null }]);
  });

  it("finds paths in prose: a slash or an extension, a :line, never a URL, each once", () => {
    const message: TimelineItem = {
      kind: "message", seq: 2, key: "m-2",
      text: "See `src/parser.rs:42` and README.md, then src/parser.rs again; docs at https://example.com/a/b.html.",
    };
    expect(pathsIn(message)).toEqual([
      { path: "src/parser.rs", line: 42 },
      { path: "README.md", line: null },
      { path: "src/parser.rs", line: null },
    ]);
  });

  it("parses one path the way a click does", () => {
    expect(parsePath("core/src/tab_set.rs:120")).toEqual({ path: "core/src/tab_set.rs", line: 120 });
    expect(parsePath("not a path")).toBeNull();
    expect(parsePath("https://x.y/z")).toBeNull();
  });

  it("gives Ctrl+g a row's whole text, the full output included", () => {
    const long = "o".repeat(9000);
    const bash: TimelineItem = { kind: "tool", seq: 1, key: "t-1", call: { seq: 1, toolUseId: "t", name: "Bash", input: { command: "make" }, result: { content: long, isError: false } } };
    const view = viewText(bash);
    expect(view.title).toBe("Bash make");
    expect(view.text).toBe(`$ make\n\n\`\`\`\n${long}\n\`\`\``);
  });

  it("fences a tool's output, so the markdown scratch buffer shows it as it was (GUI pass 2026-09-25)", () => {
    // Seen in the sandbox: `crate_1` ... `crate_2` across lines drew as italics in nvim's markdown
    // highlighting, because the output went into the `.md` buffer bare. A fence longer than any
    // backtick run inside keeps the output literal, backticks included.
    const out = "compiling crate_1\ncompiling crate_2\nsee ```a``` and ````b````";
    const bash: TimelineItem = { kind: "tool", seq: 1, key: "t-1", call: { seq: 1, toolUseId: "t", name: "Bash", input: { command: "make" }, result: { content: out, isError: false } } };
    expect(viewText(bash).text).toBe(`$ make\n\n\`\`\`\`\`\n${out}\n\`\`\`\`\``);
  });
});
