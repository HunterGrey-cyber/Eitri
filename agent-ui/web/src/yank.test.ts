import { describe, expect, it } from "vitest";
import { marked } from "marked";
import { toolInput, yankObject, yankRows, type YankObject } from "./yank";
import type { TimelineItem } from "./timeline";

const reply: TimelineItem = { kind: "message", seq: 1, key: "m", text: "**hello**" };
const prompt: TimelineItem = { kind: "prompt", seq: 1, key: "u", text: "hello" };
const tool: TimelineItem = { kind: "tool", seq: 2, key: "t", call: { seq: 2, toolUseId: "t", name: "Bash", input: { command: "echo hello" }, result: { content: "hello\n", isError: false } } };
const permission = { kind: "permission", seq: 3, key: "p", request: { toolName: "Read", input: { file_path: "src/a.rs" } } } as TimelineItem;
const edit: TimelineItem = { kind: "tool", seq: 4, key: "e", call: { seq: 4, toolUseId: "e", name: "Edit", input: { file_path: "src/a.rs", old_string: "a\nx\nb", new_string: "A\nx\nB" }, result: null } };
const ctx = { agentLabel: "Codex", links: [] as string[], codeBlocks: [] as { code: string; lang: string | null }[], currentHunk: null as number | null };
const yank = (row: TimelineItem, scope: "i" | "a", object: YankObject, extra = {}) => yankObject(row, scope, object, { ...ctx, ...extra });

describe("message, output and input objects", () => {
  it.each([
    [reply, "i", "m", "**hello**"], [reply, "a", "m", "**Codex**\n\n**hello**"],
    [prompt, "i", "m", "hello"], [prompt, "a", "m", "**You**\n\nhello"],
    [tool, "i", "o", "hello\n"], [tool, "a", "o", "Bash: echo hello\nhello\n(ok)"],
    [tool, "i", "c", "echo hello"], [tool, "a", "c", "Bash: echo hello"],
    [permission, "i", "c", "src/a.rs"], [permission, "a", "c", "Read: src/a.rs"],
  ] as const)("%j %s%s", (row, scope, object, text) => expect(yank(row, scope, object)).toEqual({ text }));
  it("keeps tool errors and full output", () => {
    const row = { ...tool, call: { ...tool.call, result: { content: "failed", isError: true } } };
    expect(yank(row, "a", "o")).toEqual({ text: "Bash: echo hello\nfailed\n(error)" });
    expect(yank(edit, "i", "o")).toEqual({ refuse: "io: this tool call has no output" });
  });
});

it("reads an ACP call back as its command, else its title, never its raw JSON", () => {
  const input = { diffs: [], locations: null, rawInput: { command: ["ls", "-la", "src"] }, title: "List files" };
  const row: TimelineItem = { kind: "tool", seq: 5, key: "x", call: { seq: 5, toolUseId: "x", name: "acp:execute", input, result: { content: "a.rs\n", isError: false } } };
  expect(yank(row, "a", "o")).toEqual({ text: "acp:execute: ls -la src\na.rs\n(ok)" });
  expect(yank(row, "i", "c")).toEqual({ text: "ls -la src" });
  expect(yank(row, "a", "c")).toEqual({ text: "acp:execute: ls -la src" });
  expect(toolInput("acp:execute", { rawInput: { command: "make test" }, title: "Run" })).toBe("make test");
  expect(toolInput("acp:read", { rawInput: { path: "a.rs" }, title: "Read a.rs" })).toBe("Read a.rs");
  // Neither a command nor a title: the input as it came, as for any other tool.
  expect(toolInput("acp:other", { rawInput: { x: 1 } })).toBe(JSON.stringify({ rawInput: { x: 1 } }, null, 2));
  expect(toolInput("acp:execute", { rawInput: { command: ["ls", 3] }, title: "" })).toContain("rawInput");
});

it("copies code verbatim or fenced with language and a fence longer than any interior run", () => {
  const codeBlocks = [{ code: "let x = `foo`;\n````", lang: "rust" }];
  expect(yank(reply, "i", "`", { codeBlocks })).toEqual({ text: codeBlocks[0].code });
  expect(yank(reply, "a", "`", { codeBlocks })).toEqual({ text: "`````rust\nlet x = `foo`;\n````\n`````" });
  expect(yank(reply, "a", "`", { codeBlocks: [{ code: "x", lang: null }] })).toEqual({ text: "```\nx\n```" });
});

it("offers two code blocks", () => {
  expect(yank(reply, "i", "`", { codeBlocks: [{ code: "a", lang: "rust" }, { code: "b", lang: null }] })).toEqual({ pick: [{ label: "code block 1 (rust)", text: "a" }, { label: "code block 2", text: "b" }] });
});

it.each(["sh`", "$'", "$&", "$`"])("copies a literal code label %s in a valid fence", (lang) => {
  const code = "# heading\n````\n~~~~~\nbody";
  const result = yank(reply, "a", "`", { codeBlocks: [{ code, lang }] });
  const marker = lang.includes("`") ? "~" : "`";
  const fence = marker.repeat(marker === "~" ? 6 : 5);
  expect(result).toEqual({ text: `${fence}${lang}\n${code}\n${fence}` });
  if (!("text" in result)) throw new Error("expected one code block");
  const tokens = marked.lexer(result.text);
  expect(tokens).toHaveLength(1);
  expect(tokens[0]).toMatchObject({ type: "code", lang, text: code });
  expect(yank(reply, "i", "`", { codeBlocks: [{ code, lang }] })).toEqual({ text: code });
});

it.each(["~sh`", "`sh`", "~"])("keeps a label %s that begins with the fence character out of the fence", (lang) => {
  const code = "body\n~~~~";
  const result = yank(reply, "a", "`", { codeBlocks: [{ code, lang }] });
  if (!("text" in result)) throw new Error("expected one code block");
  const tokens = marked.lexer(`${result.text}\n\nafter`);
  expect(tokens.filter((t) => t.type === "code")).toHaveLength(1);
  expect(tokens[0]).toMatchObject({ type: "code", lang, text: code });
  expect(tokens.some((t) => t.type === "paragraph" && t.text === "after")).toBe(true);
});

it("copies paths with optional line numbers, URLs, and offers distinct paths", () => {
  const row = { ...reply, text: "src/a.rs:12" };
  expect(yank(row, "i", "f")).toEqual({ text: "src/a.rs" });
  expect(yank(row, "a", "f")).toEqual({ text: "src/a.rs:12" });
  expect(yank(permission, "i", "f")).toEqual({ text: "src/a.rs" });
  expect(yank({ ...reply, text: "[site](https://EXAMPLE.org)" }, "i", "f", { links: ["https://example.org/"] })).toEqual({ text: "https://example.org/" });
  expect(yank({ ...reply, text: "a.rs b.rs" }, "i", "f")).toEqual({ pick: [{ label: "a.rs", text: "a.rs" }, { label: "b.rs", text: "b.rs" }] });
});

it.each(["[site](javascript:alert(1))", "[site](/relative)"])("never recovers a URL from source: %s", (text) => {
  expect(yank({ ...reply, text }, "i", "f")).toHaveProperty("refuse");
});

it.each(["i", "a"] as const)("%s objects refuse empty messages and tool output", (scope) => {
  expect(yank({ ...reply, text: "" }, scope, "m")).toEqual({ refuse: `${scope}m: the message is empty` });
  expect(yank({ ...tool, call: { ...tool.call, result: { content: "", isError: false } } }, scope, "o"))
    .toEqual({ refuse: `${scope}o: this tool call has no output` });
});

it.each([
  [{ kind: "thinking", seq: 1, key: "k", block: { seq: 1, text: "hmm" }, open: false }, "thought", "try yy"],
  [{ kind: "run", seq: 1, key: "r", calls: [tool.call] }, "group of tool calls", "press Enter to expand, then try yio or yic"],
  [{ kind: "notice", seq: 1, key: "n", notice: {} }, "notice", "try yy"],
  [{ kind: "wake", seq: 1, key: "w", text: "wake", body: null, scheduled: false, count: 1 }, "notice", "try yy"],
  [{ kind: "ending", seq: 1, key: "e", ending: {} }, "notice", "try yy"],
] as const)("names other row kinds in refusals: %j", (row, name, hint) => {
  expect(yank(row as TimelineItem, "i", "o")).toEqual({ refuse: `io: the cursor is on a ${name}, not a tool call — ${hint}` });
});

it("offers hunks, uses the current one, and adds file headers without a range", () => {
  expect(yank(edit, "i", "h")).toEqual({ pick: [{ label: "hunk 1", text: "-a\n+A" }, { label: "hunk 2", text: "-b\n+B" }] });
  expect(yank(edit, "i", "h", { currentHunk: 1 })).toEqual({ text: "-b\n+B" });
  expect(yank(edit, "a", "h", { currentHunk: 0 })).toEqual({ text: "--- a/src/a.rs\n+++ b/src/a.rs\n-a\n+A" });
  const one = { ...edit, call: { ...edit.call, input: { file_path: "a", old_string: "a", new_string: "b" } } };
  expect(yank(one, "i", "h")).toEqual({ text: "-a\n+b" });
});

it.each([
  [tool, "m", "im: the cursor is on a tool call, not a message — try ic"],
  [reply, "o", "io: the cursor is on a reply, not a tool call — try im"],
  [reply, "c", "ic: the cursor is on a reply, not a tool call or permission card — try im"],
  [reply, "`", "i`: the cursor is on a reply, not a code block — try im"],
  [reply, "f", "if: the cursor is on a reply, not a path or URL — try im"],
  [reply, "h", "ih: no diff on this row"],
] as const)("refuses %s %s with a useful alternative", (row, object, refuse) => {
  expect(yank(row, "i", object)).toEqual({ refuse });
  expect(yank(row, "a", object)).toEqual({ refuse: `a${refuse.slice(1)}` });
});

it.each([
  ["Bash", { command: "ls" }, "ls"], ["Read", { file_path: "a" }, "a"],
  ["Edit", { file_path: "a" }, "a"], ["Write", { file_path: "a" }, "a"],
  ["NotebookEdit", { notebook_path: "n.ipynb" }, "n.ipynb"],
  ["Grep", { pattern: "hello" }, "hello"], ["Glob", { pattern: "*.rs" }, "*.rs"],
  ["WebFetch", { url: "https://example.org" }, "https://example.org"],
  ["WebSearch", { query: "vim" }, "vim"], ["Other", { x: 1 }, '{\n  "x": 1\n}'],
  ["NotebookEdit", { file_path: "wrong-field" }, '{\n  "file_path": "wrong-field"\n}'],
  ["Read", { other: true }, '{\n  "other": true\n}'],
] as const)("toolInput %s", (name, input, text) => expect(toolInput(name, input)).toBe(text));

it("joins rows with one blank line", () => {
  expect(yankRows(["first", "second", "third"])).toBe("first\n\nsecond\n\nthird");
  expect(yankRows([])).toBe("");
});

it("copies hunks from several Codex files with their own paths", () => {
  const row: TimelineItem = { ...edit, call: { ...edit.call, name: "fileChange", input: { changes: [
    { path: "one.rs", kind: "add", diff: "one\n" }, { path: "two.rs", kind: "update", diff: "@@ -1 +1 @@\n-old\n+new\n" },
  ] } } };
  expect(yank(row, "a", "h")).toEqual({ pick: [
    { label: "hunk 1", text: "--- a/one.rs\n+++ b/one.rs\n+one" },
    { label: "hunk 2", text: "--- a/two.rs\n+++ b/two.rs\n-old\n+new" },
  ] });
  expect(yank(row, "i", "h", { currentHunk: 1 })).toEqual({ text: "-old\n+new" });
});
