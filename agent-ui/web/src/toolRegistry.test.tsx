// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { renderToStaticMarkup } from "react-dom/server";
import type { ReactNode } from "react";
import {
  countCodePoints,
  DETAILED_HEAD_CHARS,
  DETAILED_TAIL_CHARS,
  formatResultContent,
  lookupTool,
  renderToolCall,
  RESULT_HEAD_CHARS,
  RESULT_TAIL_CHARS,
  truncateResult,
} from "./toolRegistry";
import type { ToolCallRecord } from "./types";
import { ProjectDirContext } from "./projectPath";
import policyRs from "../../../agent/src/permission_policy.rs?raw";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered. Without
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

  it("names the saved rule that allowed a call, and says nothing of the kind otherwise (F18)", () => {
    const { container } = render(<>{renderToolCall(call({ allowedByRule: "Bash(echo *)" }))}</>);
    expect(container.querySelector(".tool-rule-note")?.textContent).toBe("allowed by rule Bash(echo *)");
    cleanup();
    const plain = render(<>{renderToolCall(call({}))}</>);
    expect(plain.container.querySelector(".tool-rule-note")).toBeNull();
  });

  /** v1 trial item 7: a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast path allowed with no
   *  card says so, muted, the same way a rule-answered row does -- and a call without the note says
   *  nothing of the kind, the carded/rule-answered case included. */
  it("names an edit the acceptEdits fast path allowed, and says nothing of the kind otherwise", () => {
    const { container } = render(<>{renderToolCall(call({ name: "Edit", allowedByAuto: true }))}</>);
    expect(container.querySelector(".tool-rule-note")?.textContent).toBe("allowed by auto");
    cleanup();
    const plain = render(<>{renderToolCall(call({ name: "Edit" }))}</>);
    expect(plain.container.querySelector(".tool-rule-note")).toBeNull();
    cleanup();
    const ruled = render(<>{renderToolCall(call({ allowedByRule: "Bash(echo *)" }))}</>);
    expect(ruled.container.textContent).not.toContain("allowed by auto");
  });

  /** The CLI's auto mode: a call its classifier refused says so on its row, muted like the other
   *  notes, with the CLI's reason verbatim -- and names the kind of refusal only when it was not the
   *  classifier's. A call nobody refused says nothing of the kind. */
  it("says the CLI blocked a call, with its reason, and nothing of the kind otherwise", () => {
    const { container } = render(
      <>
        {renderToolCall(
          call({
            input: { command: "git push --force origin main" },
            result: { content: "Permission for this action was denied by the Claude Code auto mode classifier.", isError: true },
            denied: { reasonType: "classifier", reason: "[Git Destructive]" },
          }),
        )}
      </>,
    );
    const note = container.querySelector(".tool-denied-note");
    expect(note?.textContent).toBe("blocked by auto: [Git Destructive]");
    expect(note?.classList.contains("tool-rule-note")).toBe(true);
    cleanup();
    const ruled = render(<>{renderToolCall(call({ denied: { reasonType: "rule", reason: "Bash(git push *)" } }))}</>);
    expect(ruled.container.querySelector(".tool-denied-note")?.textContent).toBe("blocked by auto: Bash(git push *) (rule)");
    cleanup();
    const bare = render(<>{renderToolCall(call({ denied: { reasonType: null, reason: null } }))}</>);
    expect(bare.container.querySelector(".tool-denied-note")?.textContent).toBe("blocked by auto");
    cleanup();
    const plain = render(<>{renderToolCall(call({}))}</>);
    expect(plain.container.querySelector(".tool-denied-note")).toBeNull();
    expect(plain.container.textContent).not.toContain("blocked by auto");
  });

  /** The refusal's wording under the note is the CLI's reason, so it is drawn wrapped and whole; any other
   *  failure keeps the one cut line. */
  it("draws a refused call's failure wrapped and uncut, and another failure as before", () => {
    const long = `Permission for this action was denied by the Claude Code auto mode classifier. Reason: ${"x".repeat(600)}`;
    const refused = render(<>{renderToolCall(call({ result: { content: long, isError: true }, denied: { reasonType: "classifier", reason: "[Git Destructive]" } }), false)}</>);
    const wrapped = refused.container.querySelector(".tool-result-preview")!;
    expect(wrapped.classList.contains("tool-result-preview-wrapped")).toBe(true);
    expect(wrapped.querySelector(".tool-result-preview-line")!.textContent).toBe(long);
    cleanup();
    const failed = render(<>{renderToolCall(call({ result: { content: long, isError: true } }), false)}</>);
    const plain = failed.container.querySelector(".tool-result-preview")!;
    expect(plain.classList.contains("tool-result-preview-wrapped")).toBe(false);
    expect(plain.querySelector(".tool-result-preview-line")!.textContent!.endsWith("…")).toBe(true);
  });

  /** O3 review item 7: a call whose CLI prompt Eitri answered without a card says so, muted, the
   *  way a call a saved rule answered does; a call without one says nothing of the kind. */
  it("says when the CLI's own prompt for a call was answered without a card", () => {
    const { container } = render(<>{renderToolCall(call({ name: "Write", promptNote: "Claude Code asked — allowed in bypass" }))}</>);
    const note = container.querySelector(".tool-prompt-note");
    expect(note?.textContent).toBe("Claude Code asked — allowed in bypass");
    expect(note?.classList.contains("tool-rule-note")).toBe(true);
    cleanup();
    const plain = render(<>{renderToolCall(call({}))}</>);
    expect(plain.container.querySelector(".tool-prompt-note")).toBeNull();
  });

  /** v1 polish F21: a path under the project root is drawn relative, one outside it absolute; the
   *  click/`gf` target stays the path as sent. A folded result draws no line of its own. */
  it("draws project paths relative, keeps others absolute, and folds without a lone marker line", () => {
    const { container } = render(
      <ProjectDirContext.Provider value="/w/proj">
        {renderToolCall(call({ name: "Read", input: { file_path: "/w/proj/src/a.rs" }, result: { content: "x", isError: false } }), false)}
        {renderToolCall(call({ toolUseId: "toolu_2", name: "Read", input: { file_path: "/etc/hosts" } }))}
        {renderToolCall(call({ toolUseId: "toolu_3", name: "Write", input: { file_path: "/w/proj/b.txt", content: "hi" } }))}
      </ProjectDirContext.Provider>,
    );
    const links = [...container.querySelectorAll(".path-link")];
    expect(links.map((l) => l.textContent)).toEqual(["src/a.rs", "/etc/hosts", "b.txt"]);
    expect(links.map((l) => l.getAttribute("data-path"))).toEqual(["/w/proj/src/a.rs", "/etc/hosts", "/w/proj/b.txt"]);
    const folded = container.querySelector('[data-folded="true"]')!;
    expect(folded.textContent).not.toContain("▸");
    expect(folded.querySelector(".tool-result")).toBeNull();
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

  it("sw-panel-render-6: renders a static notice instead of a permanent spinner for a call the panel knows is abandoned", () => {
    // Before the fix, `result === null` meant "still running" unconditionally, so a call abandoned
    // by session end or a resume/reload past its history boundary spun forever. `opts.abandoned` is
    // `MessageList.tsx`'s own `isAbandonedCall` verdict, passed in here rather than recomputed --
    // this level only asserts what `renderToolCall`/`ToolResult` draw once told.
    const { container } = render(<>{renderToolCall(call({ result: null }), true, { abandoned: true })}</>);
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("none");
    expect(result.getAttribute("aria-busy")).toBeNull();
    expect(result.textContent).toContain("no result recorded");
  });

  it("keeps the running spinner for a null result the panel has no reason to call abandoned", () => {
    const { container } = render(<>{renderToolCall(call({ result: null }), true, { abandoned: false })}</>);
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("running");
    expect(result.getAttribute("aria-busy")).toBe("true");
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

/* Task 13: P2 (every real tool is named, ToolSearch is one muted line, a finished run collapses),
   P3 (an edit's diff, folded to a one-line path+counts summary anywhere except the detailed view
   or an expanded row -- v1 trial item 7 replaced its original 6-line fold with this, once Write/
   Edit/NotebookEdit stopped folding into count-only runs and every one of them needed to be this
   compact by default), P4 (a gated row says it waits rather than repeating the call), R3 (the
   detailed view's wider cuts). */
describe("P2, P3, P4, R3 in the registry", () => {
  const html = (node: ReactNode) => renderToStaticMarkup(<div>{node}</div>);

  it("names every tool Claude Code 2.1.27x offers (read from permission_policy.rs)", () => {
    const block = policyRs.match(/TOOLS_OFFERED_BY_CLI_2_1_272: &\[&str\] = &\[([\s\S]*?)\];/)![1];
    const offered = Array.from(block.matchAll(/"([^"]+)"/g)).map((m) => m[1]);
    expect(offered.length).toBeGreaterThan(20);
    for (const name of offered) expect(lookupTool(name), name).toBeDefined();
  });

  /* CLI 2.1.283 renamed `Task` to `Agent` (permission_policy.rs's NEVER_ASK_TOOLS keeps both for
     the same reason). Registering only the old name drew every subagent launch on the new CLI as
     the generic pretty-printed JSON fallback. */
  it("draws the subagent tool as a one-line row under both its names (Task on CLI 2.1.272, Agent on 2.1.283+)", () => {
    for (const name of ["Task", "Agent"]) {
      expect(lookupTool(name), name).toBeDefined();
      const out = html(renderToolCall({ seq: 1, toolUseId: "t", name, input: { description: "look around" }, result: null }));
      expect(out, name).not.toContain("tool-card-generic");
      expect(out, name).toContain(`${name}:`);
      expect(out, name).toContain("look around");
    }
  });

  it("draws ToolSearch as one muted line with no result row", () => {
    const out = html(renderToolCall({ seq: 1, toolUseId: "t", name: "ToolSearch", input: { query: "select:Read" }, result: { content: "schema", isError: false } }));
    expect(out).toContain("tool-card-muted");
    expect(out).toContain("select:Read");
    expect(out).not.toContain("tool-result");
  });

  it("never says Unrecognized, and folds a result away with no marker line (v1 polish F21)", () => {
    const out = html(renderToolCall({ seq: 1, toolUseId: "t", name: "mcp__x__y", input: { name: "z" }, result: { content: "r", isError: false } }, false));
    expect(out).not.toContain("Unrecognized");
    expect(out).toContain("mcp__x__y");
    expect(out).toContain('data-folded="true"');
    expect(out).not.toContain("▸");
    expect(out).not.toContain("Enter to expand");
  });

  it("collapses an edit's diff to a one-line path+counts summary by default; Enter (opts.expanded) shows it whole (P3, v1 trial item 7)", () => {
    const old_string = Array.from({ length: 10 }, (_, i) => `old ${i}`).join("\n");
    const new_string = Array.from({ length: 10 }, (_, i) => `new ${i}`).join("\n");
    const input = { file_path: "/p/a.rs", old_string, new_string };
    const folded = html(renderToolCall({ seq: 1, toolUseId: "t", name: "Edit", input, result: null }, false, { expanded: false }));
    expect(folded).toContain("/p/a.rs");
    expect(folded).toContain("+10 −10");
    expect((folded.match(/class="diff-line/g) ?? []).length).toBe(0);
    // `expanded` is the same per-row flag `Enter` (`toggle-expand`, MessageList.tsx) already
    // toggles for every tool row -- no new key, just a lower default fold.
    const full = html(renderToolCall({ seq: 1, toolUseId: "t", name: "Edit", input, result: null }, true, { expanded: true }));
    expect((full.match(/class="diff-line/g) ?? []).length).toBe(20);
  });

  /** Fix round finding 4: `opts.detailed` (`Ctrl+o`) used to reach only `ToolResult`'s own
   *  head/tail cut, never `editConfig`'s `maxLines` -- a folded row stayed folded in the detailed
   *  view too, so `Ctrl+o` alone never showed the diff `editConfig`'s own doc promised it would. */
  it("Ctrl+o (opts.detailed) widens a folded edit's diff too, not only Enter (fix round finding 4)", () => {
    const old_string = Array.from({ length: 10 }, (_, i) => `old ${i}`).join("\n");
    const new_string = Array.from({ length: 10 }, (_, i) => `new ${i}`).join("\n");
    const input = { file_path: "/p/a.rs", old_string, new_string };
    const detailedButUnexpanded = html(
      renderToolCall({ seq: 1, toolUseId: "t", name: "Edit", input, result: null }, false, { expanded: false, detailed: true }),
    );
    expect((detailedButUnexpanded.match(/class="diff-line/g) ?? []).length).toBe(20);
    // Neither flag: still folded.
    const neither = html(renderToolCall({ seq: 1, toolUseId: "t", name: "Edit", input, result: null }, false, { expanded: false, detailed: false }));
    expect((neither.match(/class="diff-line/g) ?? []).length).toBe(0);
  });

  /** v1 trial item 7: a `Write` over a path with nothing there says so instead of warning about an
   *  overwrite it cannot see -- collapsed or not, since the note sits beside the diff body rather
   *  than inside its line budget. */
  it("says a Write creates a new file even collapsed, and NotebookEdit gets the same real diff view", () => {
    const created = html(
      renderToolCall({ seq: 1, toolUseId: "t", name: "Write", input: { file_path: "/p/new.rs", content: "fn new() {}" }, result: null, createsFile: true }, false, { expanded: false }),
    );
    expect(created).toContain("Creates a new file.");
    expect((created.match(/class="diff-line/g) ?? []).length).toBe(0);

    const notebook = html(
      renderToolCall(
        { seq: 1, toolUseId: "t", name: "NotebookEdit", input: { notebook_path: "/p/a.ipynb", new_source: "one\ntwo\n" }, result: null },
        true,
        { expanded: true },
      ),
    );
    expect(notebook).toContain("a.ipynb");
    expect(notebook).toContain("+2 −0");
    expect(notebook.toLowerCase()).toContain("cell");
    expect((notebook.match(/class="diff-line/g) ?? []).length).toBe(2);
  });

  /** Fix round finding 3, rendered end to end: a deleted cell reads as a deletion, not a
   *  content-free "Writes the whole cell" note (`PermissionCard` and a completed row share this
   *  same `EditDiff` view, so the fix at `editPreview` reaches both). */
  it("names a deleted notebook cell instead of drawing an empty diff for it (fix round finding 3)", () => {
    const out = html(
      renderToolCall(
        { seq: 1, toolUseId: "t", name: "NotebookEdit", input: { notebook_path: "/p/a.ipynb", cell_id: "c1", edit_mode: "delete" }, result: null },
        false,
        { expanded: false },
      ),
    );
    expect(out).toContain("a.ipynb");
    expect(out).toContain("Deletes cell c1");
    expect(out).not.toContain("Writes the whole cell");
    expect((out.match(/class="diff-line/g) ?? []).length).toBe(0);
  });

  it("says a gated call waits for approval instead of repeating it (P4)", () => {
    const out = html(renderToolCall({ seq: 1, toolUseId: "t", name: "Bash", input: { command: "rm -rf build" }, result: null }, true, { gated: true }));
    expect(out).toContain("waiting for approval");
    expect(out).not.toContain("rm -rf build");
  });

  it("raises the cut to 20k / 5k in the detailed view (R3)", () => {
    const long = "h".repeat(30000) + "t".repeat(6000);
    expect(truncateResult(long).shown.length).toBeLessThan(3000);
    const detailed = truncateResult(long, DETAILED_HEAD_CHARS, DETAILED_TAIL_CHARS);
    expect(detailed.shown.startsWith("h".repeat(20000))).toBe(true);
    expect(detailed.shown.endsWith("t".repeat(5000))).toBe(true);
  });
});

/* A folded tool row used to be its invocation alone, so nothing said how much output a finished
   `Bash`/`Read` had produced or whether it failed. It now shows the first lines of the result and how many
   more there are -- Claude Code's own fold: three lines, then a muted `… +N lines (Enter to expand,
   Ctrl+o for all)`, and a failed call in the error colour with its first line. The preview never scrolls on
   its own; `Enter` (the row's own expansion) and `Ctrl+o` (the detailed view) show the whole result. */
describe("a folded result's preview", () => {
  const lines = (n: number, prefix = "line") => Array.from({ length: n }, (_, i) => `${prefix} ${i + 1}`).join("\n");
  const folded = (content: unknown, over: Partial<ToolCallRecord> = {}, isError = false) =>
    render(<>{renderToolCall(call({ result: { content, isError }, ...over }), false)}</>).container;
  const preview = (c: HTMLElement) => c.querySelector<HTMLElement>(".tool-result-preview");
  const shownLines = (c: HTMLElement) =>
    Array.from(c.querySelectorAll(".tool-result-preview-line")).map((el) => el.textContent);
  const more = (c: HTMLElement) => c.querySelector(".tool-result-preview-more")?.textContent ?? null;

  it("shows the first three lines and says how many more there are", () => {
    const c = folded(lines(120));
    expect(shownLines(c)).toEqual(["line 1", "line 2", "line 3"]);
    expect(more(c)).toBe("… +117 lines (Enter to expand, Ctrl+o for all)");
    expect(c.querySelector('[data-folded="true"]')).not.toBeNull();
    expect(c.querySelector(".tool-result-body")).toBeNull();
  });

  it("says nothing more when the whole result fits in three lines", () => {
    for (const n of [1, 2, 3]) {
      cleanup();
      const c = folded(lines(n));
      expect(shownLines(c)).toHaveLength(n);
      expect(more(c)).toBeNull();
    }
  });

  it("counts one line past the three in the singular", () => {
    expect(more(folded(lines(4)))).toBe("… +1 line (Enter to expand, Ctrl+o for all)");
  });

  it("does not count a trailing newline as a line", () => {
    const c = folded("a\nb\nc\n");
    expect(shownLines(c)).toEqual(["a", "b", "c"]);
    expect(more(c)).toBeNull();
    cleanup();
    expect(more(folded("a\nb\nc\nd\n"))).toBe("… +1 line (Enter to expand, Ctrl+o for all)");
  });

  it("reads a Windows line ending as one line break", () => {
    const c = folded("a\r\nb\r\nc\r\nd");
    expect(shownLines(c)).toEqual(["a", "b", "c"]);
    expect(more(c)).toContain("+1 line");
  });

  it("shows nothing extra for a result of no lines at all", () => {
    for (const content of ["", "\n", "  \n \n"]) {
      cleanup();
      const c = folded(content);
      expect(preview(c)).toBeNull();
      expect(c.querySelector(".tool-result")).toBeNull();
      expect(c.querySelector('[data-folded="true"]')).not.toBeNull();
    }
  });

  it("draws a failed call in the error treatment, with its first line only", () => {
    const c = folded("bash: nope: command not found\nsecond\nthird\nfourth", {}, true);
    expect(preview(c)!.classList.contains("tool-result-error")).toBe(true);
    expect(preview(c)!.getAttribute("data-state")).toBe("error");
    expect(shownLines(c)).toEqual(["bash: nope: command not found"]);
    expect(more(c)).toBe("… +3 lines (Enter to expand, Ctrl+o for all)");
  });

  it("a one-line failure is the line and nothing else", () => {
    const c = folded("permission denied", {}, true);
    expect(shownLines(c)).toEqual(["permission denied"]);
    expect(more(c)).toBeNull();
  });

  it("marks a finished preview as done, so it reads differently from a running call", () => {
    expect(preview(folded("ok"))!.getAttribute("data-state")).toBe("done");
  });

  it("is not drawn when the row is expanded, or in the detailed view: the whole result stands instead", () => {
    const expanded = render(<>{renderToolCall(call({ result: { content: lines(10), isError: false } }), true)}</>).container;
    expect(preview(expanded)).toBeNull();
    expect(expanded.querySelector(".tool-result-body")).not.toBeNull();
    cleanup();
    const detailed = render(
      <>{renderToolCall(call({ result: { content: lines(10), isError: false } }), false, { detailed: true })}</>,
    ).container;
    expect(preview(detailed)).toBeNull();
    expect(detailed.querySelector(".tool-result-body")).not.toBeNull();
  });

  it("leaves a running call's own indicator alone", () => {
    const c = render(<>{renderToolCall(call({ result: null }), false)}</>).container;
    expect(preview(c)).toBeNull();
    expect(c.querySelector(".tool-result")!.getAttribute("data-state")).toBe("running");
  });

  it("reads the result of any tool, and a result that is not a string, as the text it would show expanded", () => {
    const c = folded([{ type: "text", text: "hello" }], { name: "SomeFutureTool" });
    expect(shownLines(c).join("\n")).toContain('"type": "text"');
    expect(more(c)).not.toBeNull(); // pretty-printed JSON runs past three lines
  });

  it("keeps a quiet tool's line to itself", () => {
    const c = folded(lines(10), { name: "ToolSearch", input: { query: "x" } });
    expect(preview(c)).toBeNull();
  });

  it("gives a diff-bearing tool no preview when it succeeded -- its row is the diff -- but shows its failure", () => {
    const ok = folded("The file /p/a.rs has been updated successfully.", {
      name: "Edit",
      input: { file_path: "/p/a.rs", old_string: "a", new_string: "b" },
    });
    expect(preview(ok)).toBeNull();
    cleanup();
    const failed = folded(
      "String to replace not found in file.",
      { name: "Edit", input: { file_path: "/p/a.rs", old_string: "a", new_string: "b" } },
      true,
    );
    expect(shownLines(failed)).toEqual(["String to replace not found in file."]);
    expect(preview(failed)!.classList.contains("tool-result-error")).toBe(true);
  });

  it("keeps a runaway line, and a huge result, bounded in what it puts in the page", () => {
    const longLine = "x".repeat(50_000);
    const c = folded(`${longLine}\nsecond\nthird\nfourth`);
    const first = shownLines(c)[0]!;
    expect(first.length).toBeLessThan(1000);
    expect(first.endsWith("…")).toBe(true);
    expect(more(c)).toContain("+1 line");
    cleanup();
    const huge = folded(Array.from({ length: 200_000 }, (_, i) => `l${i}`).join("\n"));
    expect(shownLines(huge)).toEqual(["l0", "l1", "l2"]);
    expect(more(huge)).toBe("… +199,997 lines (Enter to expand, Ctrl+o for all)");
  });
});
