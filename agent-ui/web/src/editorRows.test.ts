import { describe, expect, it } from "vitest";
import { editorResult, editorRow } from "./editorRows";

const done = (content: unknown) => ({ content, isError: false });

describe("editorRow", () => {
  it("each_tool_names_what_happened", () => {
    expect(editorRow("open_file", { path: "a.py", line: 42 }, done("opened"))).toBe("showed a.py:42 in the editor");
    expect(editorRow("open_file", { path: "a.py" }, done("opened"))).toBe("showed a.py in the editor");
    expect(editorRow("read_buffer", { path: "a.py" }, done(JSON.stringify({ path: "a.py", modified: false, text: "x" })))).toBe(
      "read a.py from the editor",
    );
    const entries = Array.from({ length: 12 }, (_, i) => ({ line: i + 1, message: "m" }));
    expect(editorRow("diagnostics", {}, done(JSON.stringify({ entries, modified_files: [], left_out: 0 })))).toBe("12 diagnostics");
    expect(editorRow("diagnostics", {}, done(JSON.stringify({ entries: entries.slice(0, 1), left_out: 0 })))).toBe("1 diagnostic");
    expect(editorRow("diagnostics", {}, done(JSON.stringify({ entries: entries.slice(0, 2), left_out: 3 })))).toBe("5 diagnostics");
    expect(editorRow("editor_state", {}, done("{}"))).toBe("looked at the editor");
    expect(editorRow("something_else", { path: "a.py" }, done("x"))).toBeNull();
  });

  it("a_file_the_person_cannot_see_is_not_called_shown", () => {
    const hidden = JSON.stringify({ shown: true, where: "new_split", line: 42, on_screen: false, note: "n" });
    expect(editorRow("open_file", { path: "a.py", line: 42 }, done(hidden))).toBe(
      "opened a.py:42 in the editor, which is not on screen",
    );
    const elsewhere = JSON.stringify({ shown: true, where: "new_split", line: 42, other_window: true, note: "n" });
    expect(editorRow("open_file", { path: "a.py", line: 42 }, done(elsewhere))).toBe("opened a.py:42 in window 1's editor");
    const seen = JSON.stringify({ shown: true, where: "new_split", line: 42 });
    expect(editorRow("open_file", { path: "a.py", line: 42 }, done(seen))).toBe("showed a.py:42 in the editor");
  });

  it("an_unsaved_read_says_so", () => {
    const answer = JSON.stringify({ path: "a.py", modified: true, text: "x" });
    expect(editorRow("read_buffer", { path: "a.py" }, done(answer))).toBe("read a.py from the editor (unsaved)");
    // The same answer as a list of content blocks, the way an MCP result carries it.
    expect(editorRow("read_buffer", { path: "a.py" }, done([{ type: "text", text: answer }]))).toBe(
      "read a.py from the editor (unsaved)",
    );
  });

  it("an_error_row_says_no_more_than_the_path", () => {
    const failed = { content: "not open in the editor; use Read", isError: true };
    expect(editorRow("open_file", { path: "a.py", line: 42 }, failed)).toBe("open_file a.py");
    expect(editorRow("read_buffer", { path: "a.py" }, failed)).toBe("read_buffer a.py");
    expect(editorRow("diagnostics", {}, failed)).toBe("diagnostics");
    expect(editorRow("editor_state", {}, failed)).toBe("editor_state");
    // No answer yet is the same row.
    expect(editorRow("open_file", { path: "a.py", line: 42 }, null)).toBe("open_file a.py");
    expect(editorRow("read_buffer", { path: "a.py" }, null)).toBe("read_buffer a.py");
  });

  it("usages_counts_what_came_back", () => {
    const item = { path: "a.py", line: 1, column: 1 };
    const input = { path: "a.py", line: 4, symbol: "foo" };
    const answer = (fields: object) => done(JSON.stringify({ symbol: "foo", missing: [], unsupported: [], ...fields }));
    expect(
      editorRow("usages", input, answer({ definitions: [item], references: [item, item], implementations: [], left_out: 9 })),
    ).toBe("12 usages of foo");
    expect(editorRow("usages", input, answer({ definitions: [item], left_out: 0 }))).toBe("1 usage of foo");
    expect(editorRow("usages", input, answer({ references: [], left_out: 0 }))).toBe("0 usages of foo");
    // An error, a call still running and an answer that is not the tool's say the name and the path only.
    expect(editorRow("usages", input, { content: "no language server is attached", isError: true })).toBe("usages a.py");
    expect(editorRow("usages", input, null)).toBe("usages a.py");
    expect(editorRow("usages", input, done("not json {"))).toBe("usages a.py");
    expect(editorRow("usages", input, done("{}"))).toBe("usages a.py");
    expect(editorRow("usages", { path: "a.py" }, answer({ definitions: [item] }))).toBe("usages a.py");
  });

  it("hover_and_symbols_name_what_happened", () => {
    const hover = done(JSON.stringify({ symbol: "foo", text: "**data**", cut: false }));
    expect(editorRow("hover", { path: "a.py", symbol: "foo" }, hover)).toBe("hover on foo");
    const symbols = done(JSON.stringify({ symbols: [{ name: "foo" }], left_out: 4 }));
    expect(editorRow("symbols", { path: "a.py" }, symbols)).toBe("5 symbols in a.py");
    expect(editorRow("symbols", { query: "query" }, symbols)).toBe("5 symbols for query");
    expect(editorRow("symbols", { query: "query" }, done(JSON.stringify({ symbols: [{}], left_out: 0 })))).toBe("1 symbol for query");
    for (const tool of ["hover", "symbols"]) {
      expect(editorRow(tool, { path: "a.py" }, null)).toBe(`${tool} a.py`);
      expect(editorRow(tool, { path: "a.py" }, { content: "failed", isError: true })).toBe(`${tool} a.py`);
      expect(editorRow(tool, { path: "a.py" }, done("bad json"))).toBe(`${tool} a.py`);
    }
  });

  it("a_foreign_server_named_otherwise_is_left_alone", () => {
    // Only the editor's own tools are in the table; any other name is not this table's to word.
    expect(editorRow("search", { path: "a.py" }, done("x"))).toBeNull();
    expect(editorRow("mcp__other__open_file", { path: "a.py" }, done("x"))).toBeNull();
  });

  it("a_malformed_answer_falls_back_to_the_name", () => {
    for (const content of ["not json {", "[1,2]", "null", "42", { odd: true }, [{ type: "image" }], undefined, null]) {
      expect(() => editorRow("read_buffer", { path: "a.py" }, done(content))).not.toThrow();
      expect(editorRow("read_buffer", { path: "a.py" }, done(content))).toBe("read_buffer a.py");
      expect(editorRow("diagnostics", { path: "a.py" }, done(content))).toBe("diagnostics a.py");
    }
    for (const input of [null, undefined, "x", 5, [], { path: 3 }]) {
      expect(() => editorRow("open_file", input, done("x"))).not.toThrow();
      expect(editorRow("open_file", input, done("x"))).toBe("open_file");
    }
    expect(editorRow("diagnostics", {}, done(JSON.stringify({ entries: "many" })))).toBe("diagnostics");
  });
});


describe("nvim_exec summary", () => {
  it("an_exec_summary_names_the_kind_and_work_done", () => {
    expect(editorRow("nvim_exec", { kind: "lua", code: "one\ntwo\nthree\n" }, done("2"))).toBe(
      "ran 3 lines of Lua in the editor",
    );
    expect(editorRow("nvim_exec", { kind: "lua", code: "return 2" }, done("2"))).toBe(
      "ran 1 line of Lua in the editor",
    );
    expect(editorRow("nvim_exec", { kind: "ex", code: "echo 2" }, done("2"))).toBe("ran Ex commands in the editor");
    for (const result of [null, { content: "refused", isError: true }]) {
      expect(editorRow("nvim_exec", { kind: "lua", code: "one\ntwo" }, result)).toBe("Lua");
      expect(editorRow("nvim_exec", { kind: "ex", code: "echo 2" }, result)).toBe("Ex");
    }
    expect(editorRow("nvim_exec", { kind: "other", code: 4 }, done("2"))).toBe("nvim_exec");
  });

  it("each_phase_c_tool_names_what_happened", () => {
    const files = [{ path: "src/a.rs", hunks: "@@" }, { path: "src/b.rs", hunks: "@@" }, { path: "src/c.rs", hunks: "@@" }];
    const renamed = done(JSON.stringify({ proposal: "p-1", files, left_out_lines: 0 }));
    expect(editorRow("lsp_rename", { path: "src/a.rs", line: 1, symbol: "foo", new_name: "bar" }, renamed)).toBe(
      "proposed renaming `foo` in 3 files",
    );
    const one = done(JSON.stringify({ proposal: "p-2", files: files.slice(0, 1), left_out_lines: 0 }));
    expect(editorRow("lsp_rename", { path: "src/a.rs", line: 1, symbol: "foo", new_name: "bar" }, one)).toBe(
      "proposed renaming `foo` in 1 file",
    );
    expect(editorRow("format", { path: "a.py" }, done(JSON.stringify({ proposal: "p-3", files: files.slice(0, 1) })))).toBe(
      "proposed formatting `a.py`",
    );
    const actions = Array.from({ length: 4 }, (_, i) => ({ id: `a-${i + 1}`, title: "t", applies: true }));
    expect(editorRow("lsp_code_actions", { path: "a.py", line: 3 }, done(JSON.stringify({ actions })))).toBe("4 code actions");
    expect(editorRow("lsp_code_actions", { path: "a.py", line: 3 }, done(JSON.stringify({ actions: actions.slice(0, 1) })))).toBe(
      "1 code action",
    );
    const proposed = done(JSON.stringify({ proposal: "p-4", title: "Add import", files: files.slice(0, 1), left_out_lines: 0 }));
    expect(editorRow("apply_code_action", { id: "a-1" }, proposed)).toBe('proposed "Add import"');
    expect(editorRow("apply_code_action", { id: "a-1" }, done(JSON.stringify({ proposal: "p-4", title: "", files, left_out_lines: 0 })))).toBe(
      "proposed a code action",
    );
    const applied = (paths: string[]) => done(JSON.stringify({ applied: paths, failed: null, not_tried: [] }));
    expect(editorRow("apply_proposal", { id: "p-3" }, applied(["a", "b", "c"]))).toBe("applied `p-3` to 3 files");
    expect(editorRow("apply_proposal", { id: "p-3" }, applied(["a"]))).toBe("applied `p-3` to 1 file");
    expect(editorRow("apply_proposal", { id: "p-3" }, applied([]))).toBe("applied nothing");
    // Running, or answered with a refusal, a row says no more than the tool and what it was given.
    expect(editorRow("lsp_rename", { path: "src/a.rs", symbol: "foo" }, null)).toBe("lsp_rename src/a.rs");
    expect(editorRow("apply_proposal", { id: "p-3" }, null)).toBe("apply_proposal p-3");
    expect(editorRow("apply_code_action", { id: "a-1" }, { content: "no such action", isError: true })).toBe("apply_code_action a-1");
  });

  it("a_failed_apply_says_so", () => {
    const stopped = {
      content: JSON.stringify({ applied: ["a"], failed: { path: "stuck/c.rs", why: "denied" }, not_tried: ["b"] }),
      isError: true,
    };
    expect(editorRow("apply_proposal", { id: "p-3" }, stopped)).toBe("applied `p-3` to 1 of 3 files, `stuck/c.rs` failed");
    const first = { content: JSON.stringify({ applied: [], failed: { path: "stuck/c.rs", why: "denied" }, not_tried: ["b"] }), isError: true };
    expect(editorRow("apply_proposal", { id: "p-3" }, first)).toBe("applied nothing, `stuck/c.rs` failed");
    expect(editorRow("apply_proposal", { id: "p-3" }, { content: "the stored copy of this proposal is gone", isError: true })).toBe(
      "could not apply `p-3`",
    );
  });
});


describe("editorResult", () => {
  const answer = (value: unknown) => done(JSON.stringify(value));
  const place = { path: "a.py", line: 4, column: 2 };

  it("reads the buffer answer's path, unsaved flag, lines and omission sentence", () => {
    const value = { path: "/p/a.py", modified: true, first_line: 4, last_line: 5, text: "one\ntwo\n",
      left_out: "lines 6 to 8 of 8 left out; read on with start_line 6" };
    expect(editorResult("read_buffer", {}, answer(value))).toBe(
      "/p/a.py:4-5 (unsaved)\none\ntwo\nlines 6 to 8 of 8 left out; read on with start_line 6",
    );
    expect(editorResult("read_buffer", {}, answer({ ...value, modified: false, left_out: undefined })))
      .toBe("/p/a.py:4-5\none\ntwo\n");
  });

  it("uses the actual open line and reports where the file was shown", () => {
    const value = { shown: true, where: "new_split", line: 3 };
    expect(editorResult("open_file", { path: "a.py", line: 42 }, answer(value)))
      .toBe("a.py:3\nshown in a new split in the editor");
    expect(editorResult("open_file", { path: "a.py" }, answer({ ...value, on_screen: false, note: "editor hidden" })))
      .toBe("a.py:3\nopened in a new split in the editor, which is not on screen\neditor hidden");
    expect(editorResult("open_file", { path: "a.py" }, answer({ ...value, other_window: true, note: "may be covered" })))
      .toBe("a.py:3\nopened in a new split in window 1's editor\nmay be covered");
    expect(editorResult("open_file", { path: "a.py" }, answer({ ...value, where: "existing" })))
      .toContain("an existing window");
    expect(editorResult("open_file", { path: "a.py" }, answer({ ...value, where: "agent_window" })))
      .toContain("the agent's window");
  });

  it("lists editor buffers and the current position without inventing windows", () => {
    const value = { root: "/p", buffers: [
      { path: "a.py", modified: true, lines: 8, filetype: "python", shown: true },
      { path: "b.rs", modified: false, lines: 0, filetype: "", shown: false },
    ], other_buffers: 2, current: { path: "a.py", line: 4 }, mode: "visual",
      selection: { path: "a.py", start_line: 4, end_line: 5, text: "one\ntwo" }, buffers_left_out: 3 };
    expect(editorResult("editor_state", {}, answer(value))).toBe(
      "/p\na.py (unsaved) · 8 lines · python · shown\nb.rs · 0 lines · not shown\nand 3 more buffers\n2 other buffers\ncurrent: a.py:4 · visual\nselection: a.py:4-5\none\ntwo",
    );
    expect(editorResult("editor_state", {}, answer({ ...value, buffers: [], current: "a buffer with no file", selection: null })))
      .toContain("current: a buffer with no file · visual");
    expect(editorResult("editor_state", {}, answer({ ...value, selection: "a selection in a file outside the project" })))
      .toContain("selection: a selection in a file outside the project");
  });

  it("lists diagnostics, omitted entries and unsaved files", () => {
    const value = { entries: [{ ...place, end_line: 4, end_column: 3, severity: "warning", message: "bad name" }],
      modified_files: ["a.py"], left_out: 7, baseline_cut: true };
    expect(editorResult("diagnostics", {}, answer(value))).toBe(
      "a.py:4:2 warning bad name\nand 7 more\nunsaved: a.py\nthe turn's diagnostic comparison is incomplete",
    );
    expect(editorResult("diagnostics", {}, answer({ entries: [], modified_files: [], left_out: 0 }))).toBe("0 diagnostics");
  });

  it("lists each usage part and keeps missing and unsupported answers visible", () => {
    const value = { symbol: "foo", definitions: [{ ...place, text: "def foo():" }], implementations: [],
      references: [{ path: "/outside.py", line: 1, column: 1 }], left_out: 2, missing: ["references"], unsupported: ["implementations"] };
    expect(editorResult("usages", {}, answer(value))).toBe(
      "definition: a.py:4:2 def foo():\nreference: /outside.py:1:1\nand 2 more\nmissing: references\nunsupported: implementations",
    );
    expect(editorResult("usages", {}, answer({ ...value, definitions: [], references: [], left_out: 0 })))
      .toContain("0 usages");
  });

  it("shows hover source as literal text and says when it was cut", () => {
    expect(editorResult("hover", {}, answer({ symbol: "foo", text: "**foo** <img src=x>\ncode", cut: false })))
      .toBe("**foo** <img src=x>\ncode");
    expect(editorResult("hover", {}, answer({ symbol: "foo", text: "words", cut: true })))
      .toBe("words\nhover text was cut");
  });

  it("lists symbols with optional identity and outside positions only", () => {
    const value = { symbols: [{ ...place, name: "foo", kind: "Function", container: "Thing" },
      { path: "/outside.py", line: 1, column: 1 }], left_out: 9 };
    expect(editorResult("symbols", {}, answer(value)))
      .toBe("a.py:4:2 Function foo (Thing)\n/outside.py:1:1\nand 9 more");
    expect(editorResult("symbols", {}, answer({ symbols: [], left_out: 0 }))).toBe("0 symbols");
  });

  it.each(["lsp_rename", "format", "apply_code_action"])("shows %s proposal paths and plain hunks", (tool) => {
    const value = { proposal: "p-1", files: [{ path: "a.py", hunks: "@@ -1 +1 @@\n-old\n+new" }], left_out_lines: 2,
      ...(tool === "apply_code_action" ? { title: "Add import" } : {}) };
    expect(editorResult(tool, {}, answer(value))).toBe(
      `${tool === "apply_code_action" ? "Add import\n" : ""}proposal p-1\na.py\n@@ -1 +1 @@\n-old\n+new\nand 2 more hunk lines`,
    );
  });

  it("lists code actions and successful application paths", () => {
    expect(editorResult("lsp_code_actions", {}, answer({ actions: [
      { id: "a-1", title: "Add import", kind: "quickfix", disabled: null, applies: true },
      { id: "a-2", title: "Other", kind: null, disabled: "unavailable", applies: false },
    ], missing: ["code_actions"] }))).toBe(
      "a-1: Add import (quickfix)\na-2: Other · unavailable · cannot apply\nmissing: code_actions",
    );
    expect(editorResult("apply_proposal", {}, answer({ applied: ["a.py", "b.py"], not_tried: [] })))
      .toBe("applied a.py\napplied b.py");
  });

  it("returns no replacement for bad JSON, wrong shapes, errors, pending, exec or foreign tools", () => {
    for (const tool of ["open_file", "read_buffer", "editor_state", "diagnostics", "usages", "hover", "symbols",
      "lsp_rename", "format", "lsp_code_actions", "apply_code_action", "apply_proposal"]) {
      for (const content of ["not json {", "null", "[]", "{}", '{"text":3}', '[{"type":"image"}]', null, undefined]) {
        expect(editorResult(tool, { path: "a.py" }, done(content))).toBeNull();
      }
      expect(editorResult(tool, {}, null)).toBeNull();
      expect(editorResult(tool, {}, { content: "failed", isError: true })).toBeNull();
    }
    const valid = answer({ symbol: "foo", text: "hover", cut: false });
    expect(editorResult("hover", {}, { ...valid, isError: true })).toBeNull();
    for (const tool of ["nvim_exec", "mcp__other__hover", "future_tool"]) expect(editorResult(tool, {}, valid)).toBeNull();
    expect(editorResult("diagnostics", {}, answer({ entries: [null], modified_files: [], left_out: 0 }))).toBeNull();
    expect(editorResult("symbols", {}, answer({ symbols: [{ ...place, name: 3 }], left_out: 0 }))).toBeNull();
  });

  it("reads MCP text envelopes and leaves the original tool result untouched", () => {
    const text = JSON.stringify({ path: "a.py", modified: false, first_line: 1, last_line: 1, text: "one" });
    const content = Object.freeze([Object.freeze({ type: "text", text })]);
    for (const wrapped of [text, content, Object.freeze({ content })]) {
      const result = Object.freeze(done(wrapped));
      const before = JSON.stringify(result);
      expect(editorResult("read_buffer", {}, result)).toBe("a.py:1-1\none");
      expect(JSON.stringify(result)).toBe(before);
    }
  });
});

describe("paths under the project", () => {
  const answer = (value: unknown) => done(JSON.stringify(value));
  const at = (path: string) => ({ path, line: 3, column: 1 });

  it("are drawn relative to the project root in the row and the body; other paths as they came", () => {
    expect(editorRow("read_buffer", { path: "/p/src/a.rs" }, answer({ modified: false }), "/p")).toBe("read src/a.rs from the editor");
    expect(editorRow("read_buffer", { path: "/q/a.rs" }, answer({ modified: false }), "/p")).toBe("read /q/a.rs from the editor");
    expect(editorRow("format", { path: "/p/a.rs" }, answer({ proposal: "f1" }), "/p")).toBe("proposed formatting `a.rs`");
    const read = { path: "/p/src/a.rs", modified: false, first_line: 1, last_line: 1, text: "x" };
    expect(editorResult("read_buffer", {}, answer(read), "/p")).toBe("src/a.rs:1-1\nx");
    expect(editorResult("read_buffer", {}, answer(read), "/pro")).toBe("/p/src/a.rs:1-1\nx");
    expect(editorResult("read_buffer", {}, answer(read))).toBe("/p/src/a.rs:1-1\nx");
    const diagnostics = { entries: [{ ...at("/p/a.rs"), severity: "error", message: "m" }], left_out: 0,
      modified_files: ["/p/b.rs", "/q/c.rs"] };
    expect(editorResult("diagnostics", {}, answer(diagnostics), "/p")).toBe("a.rs:3:1 error m\nunsaved: b.rs, /q/c.rs");
    expect(editorResult("apply_proposal", {}, answer({ applied: ["/p/a.rs"], not_tried: ["/p/b.rs"] }), "/p"))
      .toBe("applied a.rs\nnot tried: b.rs");
  });

  it("leave the editor state's root line absolute: it says which project", () => {
    const state = { root: "/p", mode: "n", other_buffers: 0, current: { path: "/p/a.rs", line: 2 }, selection: null,
      buffers: [{ path: "/p/a.rs", modified: false, lines: 9, filetype: "rust", shown: true }] };
    expect(editorResult("editor_state", {}, answer(state), "/p")?.split("\n")).toEqual(
      ["/p", "a.rs · 9 lines · rust · shown", "current: a.rs:2 · n"],
    );
  });
});
