// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { countCodePoints, formatResultContent, lookupTool, renderToolCall, RESULT_HEAD_CHARS, RESULT_TAIL_CHARS, truncateResult } from "./toolRegistry";
import type { ToolCallRecord } from "./types";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered. Without
// this, the second render in a file leaves the first one's DOM in place and `getByText` throws on
// finding two matches.
afterEach(cleanup);

function call(overrides: Partial<ToolCallRecord>): ToolCallRecord {
  // `seq` orders a call against the rest of the conversation; nothing in this file renders more
  // than one call at a time, so any value does.
  return { seq: 0, toolUseId: "toolu_1", name: "Bash", input: { command: "echo hi" }, result: null, ...overrides };
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

/* The defect these cover: every registered renderer used to ignore the result entirely, so a
   finished Bash call and one still executing rendered byte-identical markup. "Did that command
   already run?" was unanswerable from the panel. */
describe("a tool call's result", () => {
  it("marks a call with no result yet as still running", () => {
    const { container } = render(<>{renderToolCall(call({ result: null }))}</>);
    const result = container.querySelector(".tool-result")!;
    expect(result).not.toBeNull();
    expect(result.getAttribute("data-state")).toBe("running");
  });

  it("renders a completed call's real result content, not just its invocation", () => {
    const { container } = render(
      <>{renderToolCall(call({ result: { content: "hi\nthere", isError: false } }))}</>,
    );
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("done");
    expect(result.textContent).toContain("hi\nthere");
  });

  it("is genuinely distinguishable from an in-flight call, in the markup itself", () => {
    const pending = render(<>{renderToolCall(call({ result: null }))}</>).container.innerHTML;
    cleanup();
    const done = render(<>{renderToolCall(call({ result: { content: "ok", isError: false } }))}</>)
      .container.innerHTML;
    expect(pending).not.toEqual(done);
  });

  it("marks an error result as an error and still shows what it said", () => {
    const { container } = render(
      <>{renderToolCall(call({ result: { content: "bash: nope: command not found", isError: true } }))}</>,
    );
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("error");
    expect(result.textContent).toContain("command not found");
  });

  it("shows a result for an unregistered tool too, not only for the eight known ones", () => {
    const { container } = render(
      <>{renderToolCall(call({ name: "SomeFutureTool", result: { content: "done", isError: false } }))}</>,
    );
    expect(container.querySelector(".tool-result")!.getAttribute("data-state")).toBe("done");
  });

  /* A tool result that produced no output at all is not the same thing as a tool that has not run
     yet -- the difference is exactly what the running/done distinction above exists for, so an
     empty result must still read as finished. */
  it("says a finished call produced no output rather than rendering an empty box", () => {
    const { container } = render(<>{renderToolCall(call({ result: { content: "", isError: false } }))}</>);
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("done");
    expect(result.textContent).toContain("no output");
  });

  it("truncates a huge result and says how much it is not showing", () => {
    const huge = "x".repeat(50_000);
    const { container } = render(<>{renderToolCall(call({ result: { content: huge, isError: false } }))}</>);
    const body = container.querySelector(".tool-result-body")!.textContent ?? "";
    expect(body.length).toBeLessThan(huge.length);
    // The exact count, not a vague "truncated": knowing 47,500 characters are missing is what tells
    // the reader whether scrolling the real output elsewhere is worth it.
    expect(container.querySelector(".tool-result-truncated")!.textContent).toContain("47,500");
  });
});

describe("formatResultContent", () => {
  it("passes a string result through unchanged -- the common Bash/Read shape", () => {
    expect(formatResultContent("plain output\n")).toBe("plain output\n");
  });

  it("pretty-prints a structured result rather than rendering [object Object]", () => {
    // Claude tool results are frequently an array of content blocks, not a bare string.
    expect(formatResultContent([{ type: "text", text: "hi" }])).toContain('"text": "hi"');
  });

  it("renders a null/absent content as empty rather than the word null", () => {
    expect(formatResultContent(null)).toBe("");
    expect(formatResultContent(undefined)).toBe("");
  });
});

describe("truncateResult", () => {
  it("leaves anything within the budget completely alone", () => {
    const text = "y".repeat(RESULT_HEAD_CHARS + RESULT_TAIL_CHARS);
    expect(truncateResult(text)).toEqual({ shown: text, hiddenChars: 0 });
  });

  /* Head AND tail, not head alone: a failing command says why on its LAST lines, which is precisely
     what a head-only cap would throw away. */
  it("keeps the head and the tail, because a failure's message is at the end", () => {
    const text = `${"H".repeat(RESULT_HEAD_CHARS)}${"M".repeat(10_000)}${"T".repeat(RESULT_TAIL_CHARS)}`;
    const { shown, hiddenChars } = truncateResult(text);
    expect(hiddenChars).toBe(10_000);
    expect(shown.startsWith("H".repeat(RESULT_HEAD_CHARS))).toBe(true);
    expect(shown.endsWith("T".repeat(RESULT_TAIL_CHARS))).toBe(true);
    expect(shown).toContain("10,000");
  });
});

/* The count under the elided middle is the only thing telling a reader how much output they are not
   seeing. `text.length` is a UTF-16 code-unit count, so a result full of emoji or CJK extension-B
   characters was reported as up to twice as much hidden text as there really was -- and both cuts
   could land between the halves of a surrogate pair, printing a U+FFFD the tool never emitted. */
describe("truncateResult counts and cuts in whole characters", () => {
  const ASTRAL = "😀"; // U+1F600: one code point, two UTF-16 code units.

  it("counts a code point once, not once per UTF-16 unit", () => {
    expect(ASTRAL.length).toBe(2); // the hazard itself, stated so the test explains why it exists
    expect(countCodePoints(ASTRAL)).toBe(1);
    expect(countCodePoints(`a${ASTRAL}b`)).toBe(3);
    expect(countCodePoints("plain ascii")).toBe(11);
  });

  it("counts a lone surrogate as the one character it will render as", () => {
    expect(countCodePoints("\ud83d")).toBe(1);
  });

  it("reports the hidden middle in characters, not in code units", () => {
    const hiddenPoints = 1_000;
    const text = `${"H".repeat(RESULT_HEAD_CHARS)}${ASTRAL.repeat(hiddenPoints)}${"T".repeat(RESULT_TAIL_CHARS)}`;
    const { hiddenChars } = truncateResult(text);
    // The old code-unit arithmetic returned 2,000 here -- twice the truth.
    expect(hiddenChars).toBe(hiddenPoints);
  });

  it("never leaves a lone surrogate at either cut", () => {
    // Both budgets land in the MIDDLE of a pair: head ends mid-emoji, tail begins mid-emoji.
    const head = `${"H".repeat(RESULT_HEAD_CHARS - 1)}${ASTRAL}`;
    const tail = `${ASTRAL}${"T".repeat(RESULT_TAIL_CHARS - 1)}`;
    const { shown } = truncateResult(`${head}${"M".repeat(10_000)}${tail}`);
    for (let i = 0; i < shown.length; i++) {
      const unit = shown.charCodeAt(i);
      const isHigh = unit >= 0xd800 && unit <= 0xdbff;
      const isLow = unit >= 0xdc00 && unit <= 0xdfff;
      if (isHigh) {
        const next = shown.charCodeAt(i + 1);
        expect(next >= 0xdc00 && next <= 0xdfff).toBe(true);
        i++;
      } else {
        expect(isLow).toBe(false);
      }
    }
  });

  /* Truncating here costs the reader the elided text AND adds a longer line saying so. Reachable
     because the budget test is in code units while the marker's cost is real text. */
  it("leaves a result alone when the marker would be longer than what it elides", () => {
    const text = "z".repeat(RESULT_HEAD_CHARS + RESULT_TAIL_CHARS + 1);
    expect(truncateResult(text)).toEqual({ shown: text, hiddenChars: 0 });
  });

  /* Pins the boundary rather than a chosen constant: whatever the smallest elision that still earns
     a marker turns out to be, the number printed has to be the real code-point count of what was
     dropped, and the sentence around it has to be grammatical. This also documents the fact the
     implementation comment states -- the smallest marker ever printed hides well more than one
     character, which is why the singular branch has no test of its own. */
  it("prints an exact, grammatical count at the smallest elision that earns a marker", () => {
    const shownAt = (over: number) =>
      truncateResult(`${"H".repeat(RESULT_HEAD_CHARS)}${"M".repeat(over)}${"T".repeat(RESULT_TAIL_CHARS)}`);
    let over = 1;
    while (shownAt(over).hiddenChars === 0 && over < 200) over++;
    const { shown, hiddenChars } = shownAt(over);
    expect(hiddenChars).toBe(over);
    expect(hiddenChars).toBeGreaterThan(1);
    expect(shown).toContain(`${over} characters not shown`);
    // One below it, the marker costs more than it saves and nothing is elided at all.
    expect(shownAt(over - 1).hiddenChars).toBe(0);
  });
});

/* Tool names come off the provider wire, and an MCP server may name a tool anything -- including
   something already on `Object.prototype`. A bare `TOOL_REGISTRY[name]` returns the inherited
   member, which is truthy and has no `renderInvocation`, so `?.` does not short-circuit: it throws
   mid-render, and with no error boundary in `App.tsx` the whole panel unmounts. A blank panel is
   exactly the wedge the reload action exists to undo, so the two halves of this work would have
   been fighting each other. */
describe("a tool named after an Object.prototype member", () => {
  const INHERITED = ["constructor", "toString", "valueOf", "hasOwnProperty", "__proto__"];

  it("is not mistaken for a registered tool", () => {
    for (const name of INHERITED) expect(lookupTool(name)).toBeUndefined();
  });

  it("renders the generic fallback instead of throwing the panel away", () => {
    for (const name of INHERITED) {
      const { container } = render(<>{renderToolCall(call({ name, input: { x: 1 } }))}</>);
      expect(container.querySelector(".tool-card-generic")).not.toBeNull();
      expect(container.textContent).toContain(name);
      cleanup();
    }
  });
});
