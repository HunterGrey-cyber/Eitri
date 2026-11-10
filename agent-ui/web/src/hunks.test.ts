import { describe, expect, it } from "vitest";
import { editDiffOf, editPreviewsOf, hunksOf } from "./hunks";
import { MAX_DIFF_LINES, type DiffLine } from "./diff";
import type { TimelineItem } from "./timeline";

const lines = (...kinds: DiffLine["kind"][]): DiffLine[] => kinds.map((kind) => ({ kind, text: kind }));
const tool = (name: string, input: unknown): TimelineItem => ({ kind: "tool", seq: 1, key: "t-1", call: { seq: 1, toolUseId: "t", name, input, result: null } });

describe("hunksOf", () => {
  it.each([
    [lines("context", "removed", "added", "context"), [{ start: 1, end: 3 }]],
    [lines("added", "context", "removed", "added"), [{ start: 0, end: 1 }, { start: 2, end: 4 }]],
    [lines("context", "context"), []],
    [lines("added", "added"), [{ start: 0, end: 2 }]],
    [[], []],
  ])("finds the half-open changed spans in %j", (input, expected) => expect(hunksOf(input as DiffLine[])).toEqual(expected));
});

it("reads Edit and Write previews and refuses a non-edit, MultiEdit, and oversized sides", () => {
  expect(editDiffOf(tool("Edit", { old_string: "old", new_string: "new" }))).toEqual([{ kind: "removed", text: "old" }, { kind: "added", text: "new" }]);
  expect(editDiffOf(tool("Write", { file_path: "a.rs", content: "a\nb\n" }))).toEqual([{ kind: "added", text: "a" }, { kind: "added", text: "b" }]);
  expect(editDiffOf(tool("MultiEdit", { edits: [{ old_string: "a", new_string: "b" }] }))).toBeNull();
  expect(editDiffOf(tool("Read", { file_path: "a.rs" }))).toBeNull();
  expect(editDiffOf({ kind: "message", seq: 1, key: "m", text: "hello" })).toBeNull();
  expect(editDiffOf(tool("Edit", { old_string: "x\n".repeat(MAX_DIFF_LINES + 1), new_string: "y" }))).toBeNull();
  expect(editDiffOf(tool("Write", { file_path: "a", content: "x\n".repeat(MAX_DIFF_LINES + 1) }))).toBeNull();
});

it("reads a permission card and a bounded Codex change", () => {
  const call = tool("Edit", { file_path: "a.rs", old_string: "a", new_string: "b" });
  expect(editDiffOf({ kind: "permission", seq: 1, key: "p", request: { toolName: "Edit", input: call.kind === "tool" ? call.call.input : {} } } as TimelineItem)).toEqual(editDiffOf(call));
  expect(editDiffOf(tool("fileChange", { changes: [{ path: "a", kind: "update", diff: "@@ -1 +1 @@\n-a\n+b\n" }] }))).toEqual([{ kind: "context", text: "@@ -1 +1 @@" }, { kind: "removed", text: "a" }, { kind: "added", text: "b" }]);
  expect(editDiffOf(tool("fileChange", { changes: [{ path: "a", kind: "add", diff: "x\n".repeat(801) }] }))).toBeNull();
  expect(editDiffOf(tool("fileChange", { changes: [] }))).toBeNull();
});

it("keeps added hunks of different Codex files separate", () => {
  const diff = editDiffOf(tool("fileChange", { changes: [{ path: "a", kind: "add", diff: "first\n" }, { path: "b", kind: "add", diff: "second\n" }] }));
  expect(diff).not.toBeNull();
  expect(hunksOf(diff!)).toEqual([{ start: 0, end: 1 }, { start: 2, end: 3 }]);
});

it("reads ACP diffs in rendered order, retaining file boundaries and skipping count-only files", () => {
  const row = tool("acp:edit", { diffs: [
    { path: "large", oldText: "x\n".repeat(MAX_DIFF_LINES + 1), newText: "y" },
    { path: "first", oldText: "a\nx\nb", newText: "A\nx\nB" },
    { path: "second", oldText: null, newText: "new" },
  ] });
  expect(editPreviewsOf(row)?.map((preview) => [preview.filePath, preview.added, preview.removed])).toEqual([["first", 2, 2], ["second", 1, 0]]);
  expect(hunksOf(editDiffOf(row)!)).toEqual([{ start: 0, end: 2 }, { start: 3, end: 5 }, { start: 6, end: 7 }]);
  expect(editPreviewsOf(tool("acp:edit", {}))).toBeNull();
  expect(editPreviewsOf(tool("acp:edit", { diffs: [] }))).toBeNull();
});
