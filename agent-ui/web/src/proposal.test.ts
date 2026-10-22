import { describe, expect, it } from "vitest";
import { proposalOf } from "./proposal";
import type { TimelineItem } from "./timeline";

const card = (toolName: string, input: unknown, permissionId = "p1"): TimelineItem => ({
  kind: "permission",
  seq: 1,
  key: "p-1",
  request: { seq: 1, permissionId, toolUseId: "t1", toolName, input },
});

/** The stand-in ACP agent's edit: one file, the whole old text and the whole new one. */
const acpEdit = (diffs: unknown[]) => card("acp:edit", { title: "Edit notes.txt", diffs });
const standInDiff = { type: "diff", path: "/work/notes.txt", oldText: "one\ntwo\nthree\n", newText: "one\nTWO (edited)\nthree\nfour\n" };

describe("proposalOf: the pending edits Ctrl+g shows beside their file", () => {
  it("a Claude Edit is its one replacement, replace_all included", () => {
    expect(proposalOf(card("Edit", { file_path: "/p/a.rs", old_string: "a", new_string: "b", replace_all: true }))).toEqual({
      path: "/p/a.rs",
      change: { kind: "edits", edits: [{ oldText: "a", newText: "b", replaceAll: true }], crlfAsLf: true },
    });
    expect(proposalOf(card("Edit", { file_path: "/p/a.rs", old_string: "a", new_string: "" }))).toEqual({
      path: "/p/a.rs",
      change: { kind: "edits", edits: [{ oldText: "a", newText: "", replaceAll: false }], crlfAsLf: true },
    });
  });

  it("a MultiEdit is its replacements in order", () => {
    const input = {
      file_path: "/p/a.rs",
      edits: [
        { old_string: "one", new_string: "1" },
        { old_string: "two", new_string: "2", replace_all: true },
      ],
    };
    expect(proposalOf(card("MultiEdit", input))).toEqual({
      path: "/p/a.rs",
      change: {
        kind: "edits",
        edits: [
          { oldText: "one", newText: "1", replaceAll: false },
          { oldText: "two", newText: "2", replaceAll: true },
        ],
        crlfAsLf: true,
      },
    });
  });

  it("a Write is the file's whole content", () => {
    expect(proposalOf(card("Write", { file_path: "src/new.rs", content: "fn main() {}\n" }))).toEqual({
      path: "src/new.rs",
      change: { kind: "content", content: "fn main() {}\n" },
    });
  });

  it("a Codex fileChange holding one add is the new file's content", () => {
    const input = { changes: [{ path: "/p/new.txt", kind: "add", diff: "hello\n" }] };
    expect(proposalOf(card("fileChange", input))).toEqual({ path: "/p/new.txt", change: { kind: "content", content: "hello\n" } });
  });

  it("the stand-in ACP agent's one-file diff replaces its old text", () => {
    expect(proposalOf(acpEdit([standInDiff]))).toEqual({
      path: "/work/notes.txt",
      change: { kind: "edits", edits: [{ oldText: "one\ntwo\nthree\n", newText: "one\nTWO (edited)\nthree\nfour\n", replaceAll: false }], crlfAsLf: false },
    });
    expect(proposalOf(acpEdit([{ type: "diff", path: "/work/new.txt", oldText: null, newText: "fresh\n" }]))).toEqual({
      path: "/work/new.txt",
      change: { kind: "content", content: "fresh\n" },
    });
  });

  it("a Codex update carries a patch, not a text to match: no proposal", () => {
    expect(proposalOf(card("fileChange", { changes: [{ path: "/p/a.rs", kind: "update", diff: "@@ -1 +1 @@\n-a\n+b\n" }] }))).toBeNull();
  });

  it("a change of two files has no one file to show it beside", () => {
    const two = { changes: [{ path: "/p/a.txt", kind: "add", diff: "a" }, { path: "/p/b.txt", kind: "add", diff: "b" }] };
    expect(proposalOf(card("fileChange", two))).toBeNull();
    expect(proposalOf(acpEdit([standInDiff, { ...standInDiff, path: "/work/other.txt" }]))).toBeNull();
  });

  it("an ACP delete or move is not an edit of a file's text", () => {
    expect(proposalOf(card("acp:delete", { diffs: [standInDiff] }))).toBeNull();
    expect(proposalOf(card("acp:move", { diffs: [standInDiff] }))).toBeNull();
    expect(proposalOf(card("fileChange", { changes: [{ path: "/p/a.txt", kind: "delete", diff: "a" }] }))).toBeNull();
  });

  it("a card this panel already answered shows its row as before", () => {
    const write = card("Write", { file_path: "a.rs", content: "x" }, "p9");
    expect(proposalOf(write, new Set(["p9"]))).toBeNull();
    expect(proposalOf(write, new Set(["p1"]))).not.toBeNull();
  });

  it("a non-edit card, a malformed edit and any other row have no proposal", () => {
    expect(proposalOf(card("Bash", { command: "rm -rf build" }))).toBeNull();
    expect(proposalOf(card("Edit", { file_path: "a.rs", old_string: "", new_string: "x" }))).toBeNull();
    expect(proposalOf(card("Edit", { old_string: "a", new_string: "b" }))).toBeNull();
    expect(proposalOf(card("MultiEdit", { file_path: "a.rs", edits: [] }))).toBeNull();
    expect(proposalOf(card("Write", { file_path: "a.rs" }))).toBeNull();
    expect(proposalOf({ kind: "message", seq: 1, key: "m-1", message: { seq: 1, text: "see a.rs" } } as unknown as TimelineItem)).toBeNull();
    expect(
      proposalOf({
        kind: "tool",
        seq: 1,
        key: "t-1",
        call: { seq: 1, toolUseId: "t", name: "Write", input: { file_path: "a.rs", content: "x" }, result: null },
      } as unknown as TimelineItem),
    ).toBeNull();
  });
});
