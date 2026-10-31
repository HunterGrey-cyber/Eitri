import { describe, expect, it } from "vitest";
import { editorRow } from "./editorRows";

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
