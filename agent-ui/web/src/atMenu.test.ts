import { describe, expect, it } from "vitest";
import { atInsertion, atToken } from "./atMenu";

describe("atToken", () => {
  it("opens only after a boundary, anywhere in a multiline draft", () => {
    expect(atToken("@", 1)).toEqual({ query: "", start: 0, end: 1 });
    expect(atToken("mail me@sm", 10)).toBeNull();
    expect(atToken("hello\n@sub/leaf rest", 9)).toEqual({ query: "su", start: 6, end: 15 });
    expect(atToken("hello\t@sm", 9)).toEqual({ query: "sm", start: 6, end: 9 });
    expect(atToken("hello @sm rest", 10)).toBeNull();
  });
  it("keeps quoted spaces and replaces the full word past the caret", () => {
    expect(atToken('@"with space.txt" rest', 9)).toEqual({ query: "with sp", start: 0, end: 17 });
    expect(atToken('use @"with sp\nnext', 13)).toEqual({ query: "with sp", start: 4, end: 13 });
    expect(atToken('@"my dir/"', 10)).toEqual({ query: "my dir/", start: 0, end: 10 });
  });
  it("leaves shell lines and slash command arguments alone", () => {
    for (const draft of ["!echo @sm", "/cmd @sm", "  /cmd\n@sm"]) expect(atToken(draft, draft.length)).toBeNull();
    expect(atToken("hello /word @sm", 15)?.query).toBe("sm");
  });
});

describe("atInsertion", () => {
  it("quotes whitespace and leaves directories open", () => {
    expect(atInsertion("small.txt")).toEqual({ text: "@small.txt ", keepOpen: false });
    expect(atInsertion("with space.txt")).toEqual({ text: '@"with space.txt" ', keepOpen: false });
    expect(atInsertion("sub/")).toEqual({ text: "@sub/", keepOpen: true });
    expect(atInsertion("my dir/")).toEqual({ text: '@"my dir/"', keepOpen: true });
  });
});
