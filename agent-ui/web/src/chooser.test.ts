import { describe, expect, it } from "vitest";
import { choosable, chooserRows, rowText } from "./chooser";
import type { ChooserEnvelope } from "./types";

const ENVELOPE: ChooserEnvelope = {
  launch: false,
  open: [
    { tab: 1, label: "1 fix-parser", marker: "needs_input", pending: 2, resumable: true },
    { tab: 2, label: "2 legacy one", marker: null, pending: 0, resumable: false },
  ],
  records: [
    { providerSessionId: "aaaa1111-x", name: "docs", title: "write the docs", createdAt: "1", updatedAt: "2", heldElsewhere: false },
    { providerSessionId: "bbbb2222-x", name: null, title: "fix the picker", createdAt: "1", updatedAt: "2", heldElsewhere: true },
    { providerSessionId: "cccc3333-x", name: null, title: null, createdAt: "1", updatedAt: "2", heldElsewhere: false },
  ],
};

describe("the chooser's rows", () => {
  it("lists open tabs first, then records, in the order Rust sent", () => {
    expect(chooserRows(ENVELOPE, "").map(rowText)).toEqual([
      "1 fix-parser ⚑2",
      "2 legacy one · not resumable",
      "docs",
      "fix the picker · open in another window",
      "claude cccc3333",
    ]);
  });
  it("a record open in another window cannot be chosen; everything else can", () => {
    expect(chooserRows(ENVELOPE, "").map(choosable)).toEqual([true, true, true, false, true]);
  });
  it("/ filters by the text a row shows, case-insensitively", () => {
    expect(chooserRows(ENVELOPE, "PICK").map(rowText)).toEqual(["fix the picker · open in another window"]);
    expect(chooserRows(ENVELOPE, "fix").map(rowText)).toEqual(["1 fix-parser ⚑2", "fix the picker · open in another window"]);
  });
});
