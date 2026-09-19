import { describe, expect, it } from "vitest";
import { editPreview, lineDiff, MAX_DIFF_LINES } from "./diff";

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

  it("survives a malformed or absent input object", () => {
    expect(editPreview("Edit", null)).toBeNull();
    expect(editPreview("Edit", "not an object")).toBeNull();
    expect(editPreview("Edit", {})).toBeNull();
  });

  /** Counts stay available when the diff is refused: "how big is this" is the first question, and
   *  it must not become unanswerable exactly when the change is large. */
  it("still reports counts when the change is too large to render", () => {
    const huge = Array.from({ length: MAX_DIFF_LINES + 5 }, (_, i) => `l${i}`).join("\n");
    const preview = editPreview("Write", { file_path: "/f", content: huge })!;
    expect(preview.diff).toBeNull();
    expect(preview.added).toBe(MAX_DIFF_LINES + 5);
  });
});
