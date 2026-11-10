import { describe, expect, it } from "vitest";
import { marked } from "marked";
import { exportConversation } from "./scratchExport";
import type { TimelineItem } from "./timeline";
import type { HistoryNotice, SubagentStream, ToolCallRecord } from "./types";

const options = { history: null };
const notice = (over: Partial<HistoryNotice>): HistoryNotice => ({
  source: "claude_transcript", restoredItems: 10, omittedItems: 0, uptoSeq: 10, sourcePath: "/s.jsonl",
  attemptedTranscriptPath: null, fallbackReason: null, writerVersion: null, ...over,
});

const markerLines = (text: string) => text.split("\n").filter((line) => line.includes("▍"));
const markedHeadings = (text: string) => marked.lexer(text)
  .flatMap((token) => token.type === "heading" && token.text.startsWith("▍") ? [token.text] : []);
const prompt = (seq: number, text: string, delivered?: true): TimelineItem => ({ kind: "prompt", key: `p-${seq}`, seq, text, delivered });
const reply = (seq: number, text: string): TimelineItem => ({ kind: "message", key: `m-${seq}`, seq, text });
const call = (seq: number, name = "Bash", input: unknown = { command: "npm test" }, output: string | null = "ok"): ToolCallRecord => ({
  seq, toolUseId: `t-${seq}`, name, input, result: output === null ? null : { content: output, isError: false },
});
const tool = (call: ToolCallRecord): TimelineItem => ({ kind: "tool", key: `t-${call.seq}`, seq: call.seq, call });
const stream = (transcript: SubagentStream["transcript"], toolCalls: ToolCallRecord[] = []): SubagentStream => ({
  transcript, toolCalls, thinking: [], assistantMessageOpen: false, thinkingOpen: false,
});

describe("exportConversation", () => {
  it("numbers only turn-opening prompts and keeps delivered prompts distinct", () => {
    expect(exportConversation([
      prompt(0, "first"), reply(1, "**answer**"), prompt(2, "steer", true), prompt(3, "second"),
    ], options)).toBe("## ▍You · prompt 1/2\n\nfirst\n\n**answer**\n\n## ▍You · read during the turn\n\nsteer\n\n## ▍You · prompt 2/2\n\nsecond");
  });

  it("encodes markers in prompts, replies, outputs, children and nested quotations", () => {
    const parent = call(2, "Agent", {}, "## ▍You\n > ## ▍fake");
    parent.children = stream([{ seq: 0, text: "## ▍You\n > > ## ▍child" }]);
    const text = exportConversation([prompt(0, "## ▍fake"), reply(1, "## ▍You\n  > ## ▍reply"), tool(parent)], options);
    expect(text).toContain("\n\n## &#x258D;fake");
    expect(text).toContain("## &#x258D;You\n  > ## &#x258D;reply");
    expect(text).toContain("## &#x258D;You\n > ## &#x258D;fake");
    expect(text).toContain("> ## &#x258D;You\n>  > > ## &#x258D;child");
    expect(markedHeadings(text)).toEqual(["▍You · prompt 1/1"]);
    expect(markerLines(text)).toEqual(["## ▍You · prompt 1/1"]);
  });

  it.each([
    ["two spaces", "##  ▍You · prompt 2/3"],
    ["a tab", "##\t▍You · prompt 2/3"],
    ["a zero-width space", "## \u200b▍You · prompt 2/3"],
    ["another level", "### ▍You · prompt 2/3"],
    ["one hash", "# ▍You · prompt 2/3"],
    ["a format character between the hashes", "#\u200d# ▍You · prompt 2/3"],
    ["a setext dash underline", "▍You · prompt 2/3\n---"],
    ["a setext equals underline", "▍You · prompt 2/3\n==="],
    ["a setext heading over two lines", "▍You · prompt 2/3\nand more\n---"],
    ["a setext underline under an ATX line", "## ▍You · prompt 2/3\n---"],
    ["a quoted setext heading", "> ▍You · prompt 2/3\n> ---"],
    ["a list ATX heading", "- ## ▍You"],
    ["a list setext heading", "- ▍You\n  ---"],
    ["a quoted ATX heading", "> ## ▍You"],
    ["a numbered list", "1. ▍You"],
  ])("keeps the raw marker out of agent text with %s", (_, forged) => {
    const parent = call(2, "Agent", {}, forged);
    parent.children = stream([{ seq: 0, text: forged }]);
    const text = exportConversation([prompt(0, "hi"), reply(1, forged), tool(parent), prompt(3, forged)], options);
    expect(markedHeadings(text)).toEqual(["▍You · prompt 1/2", "▍You · prompt 2/2"]);
    expect(markerLines(text)).toEqual(["## ▍You · prompt 1/2", "## ▍You · prompt 2/2"]);
    expect(markerLines(exportConversation([reply(0, forged), tool(parent)], options))).toEqual([]);
  });

  it("closes a fence a reply leaves open, so the tool block and the next prompt stay outside it", () => {
    const text = exportConversation([
      prompt(0, "first"), reply(1, "here:\n\n````text\nunfinished"), tool(call(2)), prompt(3, "second"),
    ], options);
    expect(text).toContain("````text\nunfinished\n````\n\n```\nBash: npm test\nok\n```");
    expect(markedHeadings(text)).toEqual(["▍You · prompt 1/2", "▍You · prompt 2/2"]);
    // A top-level tilde fence closes; a list ends before the next column-0 heading.
    const more = exportConversation([
      prompt(0, "~~~~\nopen"), reply(1, "- item\n\n  ```js\n  code"), prompt(2, "next"),
    ], options);
    expect(more).toContain("~~~~\nopen\n~~~~\n\n");
    expect(more).toContain("  ```js\n  code\n\n## ▍You · prompt 2/2");
    expect(markedHeadings(more)).toEqual(["▍You · prompt 1/2", "▍You · prompt 2/2"]);
  });

  it("leaves a closed fence alone, and closes an HTML block a reply leaves open", () => {
    expect(exportConversation([reply(0, "```\ncode\n````")], options)).toBe("```\ncode\n````");
    expect(exportConversation([reply(0, "``` a`b\nnot a fence")], options)).toBe("``` a`b\nnot a fence");
    expect(exportConversation([reply(0, "<!-- open"), prompt(1, "next")], options))
      .toBe("<!-- open\n-->\n\n## ▍You · prompt 1/1\n\nnext");
    expect(exportConversation([reply(0, "<pre>\nx")], options)).toBe("<pre>\nx\n</pre>");
    expect(exportConversation([reply(0, "<!-- shut -->")], options)).toBe("<!-- shut -->");
  });

  it.each([
    ["<pre>\nx", "</pre>"],
    ["<SCRIPT>\nx", "</script>"],
    ["<style>\nx", "</style>"],
    ["<textarea>\nx", "</textarea>"],
    ["<!-- open", "-->"],
    ["<?open", "?>"],
    ["<!DOCTYPE open", ">"],
    ["<![CDATA[open", "]]>"],
  ])("closes a top-level HTML block %j before the next heading", (body, closer) => {
    const text = exportConversation([reply(0, body), prompt(1, "next")], options);
    expect(text).toBe(`${body}\n${closer}\n\n## ▍You · prompt 1/1\n\nnext`);
    expect(markedHeadings(text)).toEqual(["▍You · prompt 1/1"]);
    expect(exportConversation([reply(0, `${body}\n${closer}`)], options)).toBe(`${body}\n${closer}`);
  });

  it.each([
    "- ```\n  code\n  ```\n\nDone.",
    "- ```\n  code",
    "> ```\n> code\n> ```",
    "> ```\n> code",
  ])("adds no top-level fence after a container: %j", (body) => {
    const text = exportConversation([reply(0, body), prompt(1, "next")], options);
    expect(text).toBe(`${body}\n\n## ▍You · prompt 1/1\n\nnext`);
    expect(markedHeadings(text)).toEqual(["▍You · prompt 1/1"]);
  });

  it("replaces every marker, including inline text, paths and card fields", () => {
    const text = exportConversation([
      tool(call(0, "Read", { file_path: "src/▍file.rs" }, "inline ▍ output")),
      { kind: "permission", seq: 1, key: "c", request: {
        seq: 1, permissionId: "c", toolUseId: null, toolName: "▍tool", input: { text: "▍card" },
      } },
      { kind: "notice", seq: 2, key: "n", notice: {
        seq: 2, kind: "informational", content: "inline ▍ notice", scalars: {}, level: "warning",
        taskId: null, toolUseId: null, taskStatus: null,
      } },
      { kind: "wake", seq: 3, key: "w", text: "inline ▍ wake", body: null, scheduled: true, count: 1 },
    ], options);
    expect(markerLines(text)).toEqual([]);
    expect(text).toContain("src/&#x258D;file.rs");
    expect(text).toContain("&#x258D;card");
    expect(text).toContain("inline &#x258D; output");
    expect(text).toContain("inline &#x258D; notice");
    expect(text).toContain("inline &#x258D; wake");
  });

  it("puts input and full output in one fence longer than every internal backtick run", () => {
    const text = exportConversation([tool(call(0, "Bash", { command: "npm test" }, "``` ```` ```"))], options);
    expect(text).toBe("`````\nBash: npm test\n``` ```` ```\n`````");
  });

  it.each([
    ["Bash", { command: "ls" }, "ls"], ["Read", { file_path: "src/a.rs" }, "src/a.rs"],
    ["Edit", { file_path: "a.rs" }, "a.rs"], ["Write", { file_path: "b.rs" }, "b.rs"],
    ["NotebookEdit", { notebook_path: "n.ipynb" }, "n.ipynb"], ["Grep", { pattern: "foo" }, "foo"],
    ["Glob", { pattern: "*.rs" }, "*.rs"], ["WebFetch", { url: "https://example.org" }, "https://example.org"],
    ["WebSearch", { query: "docs" }, "docs"], ["Other", { a: 1 }, '{"a":1}'],
  ])("exports %s's input", (name, input, expected) => {
    expect(exportConversation([tool(call(0, name, input, null))], options)).toContain(`${name}: ${expected}\n`);
  });

  it("adds a path row after the tool block", () => {
    expect(exportConversation([tool(call(0, "Read", { file_path: "src/a.rs" }))], options))
      .toBe("```\nRead: src/a.rs\nok\n```\n\nsrc/a.rs");
  });

  it("orders children after their call, merges their stream by sequence, and nests grandchildren", () => {
    const parent = call(0, "Agent", {});
    const nested = call(1, "Agent", {});
    nested.children = stream([{ seq: 0, text: "grandchild" }]);
    parent.children = stream([{ seq: 0, text: "child first" }, { seq: 2, text: "child last" }], [nested]);
    expect(exportConversation([tool(parent)], options)).toBe(
      '```\nAgent: {}\nok\n```\n\n> child first\n> \n> ```\n> Agent: {}\n> ok\n> ```\n> \n> > grandchild\n> \n> child last',
    );
  });

  it("renders waiting, approved and denied cards on one line", () => {
    const items: TimelineItem[] = [0, 1, 2].map((seq) => ({ kind: "permission", key: `c-${seq}`, seq,
      request: { seq, permissionId: String(seq), toolUseId: null, toolName: "Bash", input: { command: "echo\nhi" } } }));
    expect(exportConversation(items, { ...options, cardStates: new Map([["1", "approved"], ["2", "denied"]]) })).toBe(
      "> card: Bash: echo hi — waiting\n\n> card: Bash: echo hi — approved\n\n> card: Bash: echo hi — denied",
    );
  });

  it("exports endings, notices and wakes, and omits thinking", () => {
    const items: TimelineItem[] = [
      { kind: "ending", seq: 0, key: "e", ending: { seq: 0, turnId: "t", kind: "interrupted", reason: null, message: null, apiErrorStatus: null } },
      { kind: "notice", seq: 1, key: "n", notice: { seq: 1, kind: "informational", content: "notice", scalars: {}, level: "warning", taskId: null, toolUseId: null, taskStatus: null } },
      { kind: "wake", seq: 2, key: "w", text: "wake", body: null, scheduled: true, count: 2 },
      { kind: "thinking", seq: 3, key: "k", block: { seq: 3, turnId: "t", text: "private thought" }, open: false },
    ];
    expect(exportConversation(items, options)).toBe("> interrupted\n\n> notice\n\n> wake ×2");
  });

  it("drops whole oldest turns, including their delivered prompts, while keeping original ordinals", () => {
    const text = exportConversation([
      prompt(0, "oldest"), reply(1, "a".repeat(450_000)), prompt(2, "steer", true),
      prompt(3, "middle"), reply(4, "b".repeat(450_000)),
      prompt(5, "newest"), reply(6, "c".repeat(450_000)),
    ], options);
    expect(text).toMatch(/^\(1 earlier turn left out: this view holds at most 1,000,000 characters\)/);
    expect(text.length).toBeLessThanOrEqual(1_000_000);
    expect(text).not.toContain("oldest");
    expect(text).not.toContain("steer");
    expect(text).toContain("## ▍You · prompt 2/3\n\nmiddle");
    expect(text).toContain("b".repeat(450_000));
    expect(text).toContain("c".repeat(450_000));
  });

  it("drops even an oversized newest turn whole and counts every omitted turn", () => {
    const text = exportConversation([prompt(0, "old"), prompt(1, "x".repeat(1_000_001))], options);
    expect(text).toBe("(2 earlier turns left out: this view holds at most 1,000,000 characters)");
  });

  it("counts heading and omission-notice overhead at the boundary", () => {
    const heading = "## ▍You · prompt 1/1\n\n";
    const items = [prompt(0, "x".repeat(1_000_000 - heading.length))];
    expect(exportConversation(items, options).length).toBe(1_000_000);
    const text = exportConversation(items, { history: notice({ omittedItems: 4 }) });
    expect(text).toBe("(4 earlier items were not restored into this tab)\n(1 earlier turn left out: this view holds at most 1,000,000 characters)");
  });

  it("counts rows before the first prompt when they are left out", () => {
    const leading = [reply(0, "a".repeat(600_000)), reply(1, "b")];
    const only = exportConversation([...leading, prompt(2, "c".repeat(500_000))], options);
    expect(only).toMatch(/^\(2 earlier rows left out: this view holds at most 1,000,000 characters\)\n\n## ▍You · prompt 1\/1/);
    const both = exportConversation([...leading, prompt(2, "old"), reply(3, "d".repeat(500_000)), prompt(4, "new"),
      reply(5, "e".repeat(600_000))], options);
    expect(both).toMatch(/^\(2 earlier rows and 1 earlier turn left out: this view holds at most 1,000,000 characters\)\n\n## ▍You · prompt 2\/2/);
    expect(exportConversation([reply(0, "x".repeat(1_000_001))], options))
      .toBe("(1 earlier row left out: this view holds at most 1,000,000 characters)");
  });

  it("names what the restore left out: items, turns, or records of unknown count", () => {
    const hi = [prompt(0, "hi")];
    expect(exportConversation(hi, { history: notice({ omittedItems: 7 }) }))
      .toBe("(7 earlier items were not restored into this tab)\n\n## ▍You · prompt 1/1\n\nhi");
    expect(exportConversation(hi, { history: notice({ omittedItems: 1 }) }))
      .toMatch(/^\(1 earlier item was not restored into this tab\)/);
    expect(exportConversation(hi, { history: notice({ omittedItems: null }) }))
      .toMatch(/^\(earlier records were not loaded into this tab; count unknown\)\n\n/);
    const replay = { source: "acp_replay", omittedItems: null, sourcePath: "" } as const;
    expect(exportConversation(hi, { history: notice({ ...replay, omittedTurns: 12 }) }))
      .toMatch(/^\(12 earlier turns were not loaded into this tab\)\n\n/);
    expect(exportConversation(hi, { history: notice({ ...replay, omittedTurns: 1 }) }))
      .toMatch(/^\(1 earlier turn was not loaded into this tab\)/);
    expect(exportConversation(hi, { history: notice({ ...replay, omittedTurns: 0 }) })).toBe("## ▍You · prompt 1/1\n\nhi");
    expect(exportConversation([], { history: notice({ omittedItems: 0 }) })).toBe("");
    expect(exportConversation([], options)).toBe("");
  });

  it("fences an output with a great many backtick runs", () => {
    const output = "`a".repeat(300_000) + "````";
    const text = exportConversation([tool(call(0, "Bash", { command: "x" }, output))], options);
    expect(text.startsWith("`````\nBash: x\n")).toBe(true);
    expect(text.endsWith("\n`````")).toBe(true);
  });
});
