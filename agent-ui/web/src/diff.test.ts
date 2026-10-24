import { describe, expect, it } from "vitest";
import { codexChangePreview, editPreview, lineDiff, MAX_DIFF_LINES } from "./diff";

describe("lineDiff", () => {
  it("marks only the lines that actually changed", () => {
    const diff = lineDiff("a\nb\nc\n", "a\nB\nc\n")!;
    expect(diff.map((l) => `${l.kind}:${l.text}`)).toEqual([
      "context:a",
      "removed:b",
      "added:B",
      "context:c",
    ]);
  });

  it("keeps unchanged lines as context rather than rewriting the whole block", () => {
    const diff = lineDiff("keep\nkeep\nold\n", "keep\nkeep\nnew\n")!;
    expect(diff.filter((l) => l.kind === "context")).toHaveLength(2);
    expect(diff.filter((l) => l.kind !== "context").map((l) => l.text)).toEqual(["old", "new"]);
  });

  /** A trailing newline is how files end. Counting it as an empty last line would put a phantom
   *  removed/added line at the bottom of every diff. */
  it("does not invent a line from a trailing newline", () => {
    expect(lineDiff("a\n", "a\n")).toEqual([{ kind: "context", text: "a" }]);
  });

  it("handles insertion into the middle without rewriting what surrounds it", () => {
    const diff = lineDiff("a\nc\n", "a\nb\nc\n")!;
    expect(diff.map((l) => l.kind)).toEqual(["context", "added", "context"]);
  });

  /** A diff that silently omits lines is worse than one that refuses: the reader would approve
   *  what they could see. */
  it("refuses rather than truncating when a side is too large", () => {
    const huge = Array.from({ length: MAX_DIFF_LINES + 1 }, (_, i) => `line ${i}`).join("\n");
    expect(lineDiff("", huge)).toBeNull();
  });
});

/** A line break at the end of one side and not the other is a change: an `Edit` from `# check\n` to `# check`
 *  joins the next line onto the comment, and a card that drew both as the same context line, `+0 −0`, would
 *  have a person approve a change it hid. */
describe("a line break only one side ends with", () => {
  it("shows the last line as removed and added, the side without the break marked", () => {
    expect(lineDiff("# safety check\n", "# safety check")).toEqual([
      { kind: "removed", text: "# safety check" },
      { kind: "added", text: "# safety check", noNewline: true },
    ]);
    expect(lineDiff("a\nb", "a\nb\n")).toEqual([
      { kind: "context", text: "a" },
      { kind: "removed", text: "b", noNewline: true },
      { kind: "added", text: "b" },
    ]);
    const preview = editPreview("Edit", { file_path: "/f", old_string: "x\n", new_string: "x" })!;
    expect([preview.added, preview.removed]).toEqual([1, 1]);
  });
  it("marks nothing when both sides end the same way", () => {
    expect(lineDiff("a", "b")).toEqual([
      { kind: "removed", text: "a" },
      { kind: "added", text: "b" },
    ]);
    expect(lineDiff("a\nb", "a\nc")).toEqual([
      { kind: "context", text: "a" },
      { kind: "removed", text: "b" },
      { kind: "added", text: "c" },
    ]);
  });
  it("keeps a Codex diff's own no-newline line as the mark on the line before it", () => {
    const preview = codexChangePreview({ path: "f", kind: "update", diff: "@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+a\n" });
    expect(preview.diff).toEqual([
      { kind: "context", text: "@@ -1 +1 @@" },
      { kind: "removed", text: "a", noNewline: true },
      { kind: "added", text: "a" },
    ]);
  });
});

describe("editPreview", () => {
  it("reads an Edit's real fields", () => {
    const preview = editPreview("Edit", {
      file_path: "/p/a.rs",
      old_string: "fn a() {}\n",
      new_string: "fn a() -> u8 { 1 }\n",
      replace_all: false,
    })!;
    expect(preview.filePath).toBe("/p/a.rs");
    expect(preview.added).toBe(1);
    expect(preview.removed).toBe(1);
    expect(preview.wholeFile).toBe(false);
  });

  /** `replace_all` changes how many places in the file this touches, which a reader cannot infer
   *  from the diff itself -- the diff looks identical either way. */
  it("carries replace_all, which the diff alone cannot show", () => {
    const preview = editPreview("Edit", { file_path: "/f", old_string: "x", new_string: "y", replace_all: true })!;
    expect(preview.replaceAll).toBe(true);
  });

  /** A Write request says what the file WILL contain and nothing about what is there now, so
   *  drawing a patch would imply the rest of the file survives. */
  it("calls a Write a whole-file write rather than a patch", () => {
    const preview = editPreview("Write", { file_path: "/f", content: "one\ntwo\n" })!;
    expect(preview.wholeFile).toBe(true);
    expect(preview.added).toBe(2);
    expect(preview.removed).toBe(0);
  });

  /** A diff view appearing for a tool it does not understand would be a worse lie than JSON. */
  it("returns null for every tool that does not change a file", () => {
    expect(editPreview("Bash", { command: "ls" })).toBeNull();
    expect(editPreview("Read", { file_path: "/f" })).toBeNull();
    expect(editPreview("mcp__x__y", { anything: 1 })).toBeNull();
  });

  /** `NotebookEdit`'s own schema (`notebook_path`, `new_source`) carries no
   *  "before" -- the tool never reports what a cell held, unlike `Edit`'s `old_string` -- so this
   *  is a whole-content write for one cell, the same shape as `Write` is for a whole file, and it
   *  must say "cell" rather than "file" so the note is not misleading. */
  it("reads a NotebookEdit as a whole-cell write, filed under notebook_path", () => {
    const preview = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", cell_id: "c1", new_source: "one\ntwo\n" })!;
    expect(preview.filePath).toBe("/p/a.ipynb");
    expect(preview.wholeFile).toBe(true);
    expect(preview.added).toBe(2);
    expect(preview.removed).toBe(0);
    expect(preview.wholeFileNote).toMatch(/cell/i);
  });

  it("survives a NotebookEdit with neither notebook_path nor new_source", () => {
    expect(editPreview("NotebookEdit", {})).toBeNull();
  });

  /** `edit_mode: "delete"` carries no `new_source` at all -- there is nothing
   *  to write -- so diffing it like every other `NotebookEdit` produced an empty diff, "+0 −0" and
   *  the misleading "Writes the whole cell" note, true of nothing and silent about the deletion
   *  that actually happened. */
  it("names a deleted cell instead of drawing an empty whole-cell write for it", () => {
    const preview = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", cell_id: "c1", edit_mode: "delete" })!;
    expect(preview.filePath).toBe("/p/a.ipynb");
    expect(preview.diff).toEqual([]);
    expect(preview.added).toBe(0);
    expect(preview.removed).toBe(0);
    expect(preview.wholeFile).toBe(true);
    expect(preview.wholeFileNote).toBe("Deletes cell c1. The request carries no record of what it held.");
  });

  it("still names a deletion with no cell_id, and never mentions writing", () => {
    const preview = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", edit_mode: "delete" })!;
    expect(preview.wholeFileNote).toBe("Deletes a cell. The request carries no record of what it held.");
    expect(preview.wholeFileNote).not.toMatch(/writes/i);
  });

  it("survives a delete with no notebook_path", () => {
    expect(editPreview("NotebookEdit", { edit_mode: "delete" })).toBeNull();
  });

  it("still reads a plain replace as a whole-cell write, unaffected by the delete branch", () => {
    const preview = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", edit_mode: "replace", new_source: "x\n" })!;
    expect(preview.wholeFileNote).toBe("Replaces a cell. The request does not say what is there now.");
    expect(preview.added).toBe(1);
  });

  it("survives a malformed or absent input object", () => {
    expect(editPreview("Edit", null)).toBeNull();
    expect(editPreview("Edit", "not an object")).toBeNull();
    expect(editPreview("Edit", {})).toBeNull();
  });

  /** Counts stay available when the diff is refused: "how big is this" is the first question, and
   *  it must not become unanswerable exactly when the change is large. */
  it("reports counts and all lines when the change is too large to compute", () => {
    const huge = Array.from({ length: MAX_DIFF_LINES + 5 }, (_, i) => `l${i}`).join("\n");
    const preview = editPreview("Write", { file_path: "/f", content: huge })!;
    expect(preview.diff).toHaveLength(MAX_DIFF_LINES + 5);
    expect(preview.added).toBe(MAX_DIFF_LINES + 5);
  });
});

describe("codexChangePreview", () => {
  const drawn = (diff: string) =>
    codexChangePreview({ path: "f", kind: "update", diff }).diff!.map((l) => `${l.kind}:${l.text}`);

  it("draws a mode change or a rename that comes before the first hunk", () => {
    expect(drawn("old mode 100644\nnew mode 100755\n@@ -1 +1 @@\n-a\n+b\n")).toEqual([
      "context:old mode 100644",
      "context:new mode 100755",
      "context:@@ -1 +1 @@",
      "removed:a",
      "added:b",
    ]);
    expect(drawn("rename from a.rs\nrename to b.rs\n@@ -1 +1 @@\n-a\n+b\n").slice(0, 2)).toEqual([
      "context:rename from a.rs",
      "context:rename to b.rs",
    ]);
  });

  it("still leaves out the file header lines before the first hunk", () => {
    expect(drawn("diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n")).toEqual([
      "context:@@ -1 +1 @@",
      "removed:a",
      "added:b",
    ]);
  });

  it("takes an entry whose diff is not a string as an empty diff", () => {
    for (const diff of [undefined, null, 7, {}]) {
      const preview = codexChangePreview({ path: "a", kind: "update", diff } as never);
      expect(preview.diff).toEqual([]);
      expect(preview.filePath).toBe("a");
    }
  });
});

describe("complete edit previews", () => {
  it("keeps every added line of a Write above the diff cap", () => {
    const content = [...Array.from({ length: 400 }, (_, i) => `line ${i}`), "rm -rf important"].join("\n");
    const preview = editPreview("Write", { file_path: "/p/run.sh", content })!;
    expect(preview.diff).not.toBeNull();
    expect(preview.diff).toHaveLength(401);
    expect(preview.diff!.every((line) => line.kind === "added")).toBe(true);
    expect(preview.diff![400]).toEqual({ kind: "added", text: "rm -rf important", noNewline: true });
    expect([preview.added, preview.removed]).toEqual([401, 0]);
  });

  it("keeps both sides of an Edit above the diff cap", () => {
    const before = Array.from({ length: 401 }, (_, i) => `old ${i}`);
    const after = Array.from({ length: 402 }, (_, i) => `new ${i}`);
    const preview = editPreview("Edit", { file_path: "/p/a", old_string: before.join("\n"), new_string: after.join("\n") + "\n" })!;
    expect(preview.diff?.filter((line) => line.kind === "removed").map((line) => line.text)).toEqual(before);
    expect(preview.diff?.filter((line) => line.kind === "added").map((line) => line.text)).toEqual(after);
    expect(preview.diff![400].noNewline).toBe(true);
    expect(preview.diff![802].noNewline).toBeUndefined();
    expect([preview.added, preview.removed]).toEqual([402, 401]);
  });

  it("keeps every line of a NotebookEdit above the diff cap", () => {
    const preview = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", new_source: "x\n".repeat(401) })!;
    expect(preview.diff).toHaveLength(401);
    expect(preview.diff?.every((line) => line.kind === "added" && line.text === "x" && !line.noNewline)).toBe(true);
  });
});

describe("notebook cell notes", () => {
  it.each([
    [{ edit_mode: "insert", cell_id: "cell-A", cell_type: "markdown" }, "Inserts a new markdown cell after cell cell-A."],
    [{ edit_mode: "insert", cell_type: "code" }, "Inserts a new code cell at the start of the notebook."],
    [{ edit_mode: "insert" }, "Inserts a new cell at the start of the notebook."],
    [{ cell_id: "cell-B", cell_type: "code" }, "Replaces cell cell-B as a code cell. The request does not say what is there now."],
    [{ edit_mode: "replace", cell_id: "cell-B" }, "Replaces cell cell-B. The request does not say what is there now."],
    [{ cell_type: "markdown" }, "Replaces a cell as a markdown cell. The request does not say what is there now."],
    [{ edit_mode: "rotate", cell_id: "cell-C" }, 'Edit mode "rotate" on cell cell-C.'],
    [{ edit_mode: "", cell_id: "cell-C" }, 'Edit mode "" on cell cell-C.'],
    [{ edit_mode: 7, cell_type: "code" }, "Edit mode 7 on a cell as a code cell."],
  ])("names the cell operation %j", (fields, note) => {
    expect(editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", new_source: "x", ...fields })!.wholeFileNote).toBe(note);
  });
});

it("distinguishes a NUL in a line from a missing final line break", () => {
  expect(lineDiff("x", "x\u0000\n")).toEqual([
    { kind: "removed", text: "x", noNewline: true },
    { kind: "added", text: "x\u0000" },
  ]);
});

it.each(["add", "delete"])("marks a Codex %s without a final line break", (kind) => {
  const open = codexChangePreview({ path: "f", kind, diff: "x" });
  const closed = codexChangePreview({ path: "f", kind, diff: "x\n" });
  expect(open.diff).toEqual([{ kind: kind === "add" ? "added" : "removed", text: "x", noNewline: true }]);
  expect(closed.diff).toEqual([{ kind: kind === "add" ? "added" : "removed", text: "x" }]);
  expect(codexChangePreview({ path: "f", kind, diff: "" }).diff).toEqual([]);
});
