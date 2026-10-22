import { describe, expect, it } from "vitest";
import { yankText } from "./yank";

const FENCE = "`".repeat(3);
const lines = ["# Title", "", "first line", "second line", "", `${FENCE}sh`, "make", "make install", FENCE, "", "~~~", "x", "~~~~"];

describe("yankText", () => {
  it("copies a block's source lines, end exclusive", () => {
    expect(yankText(lines, { line: 2, end: 4, fence: false })).toBe("first line\nsecond line");
    expect(yankText(lines, { line: 0, end: 1, fence: false })).toBe("# Title");
  });
  it("copies a fenced block's code without its fence lines", () => {
    expect(yankText(lines, { line: 5, end: 9, fence: true })).toBe("make\nmake install");
    expect(yankText(lines, { line: 10, end: 13, fence: true })).toBe("x");
  });
  it("keeps the last line of a fence that runs to the end of the document unclosed", () => {
    expect(yankText([`${FENCE}js`, "a", "b"], { line: 0, end: 3, fence: true })).toBe("a\nb");
  });
  it("does not take a shorter or different marker for the closing fence", () => {
    expect(yankText(["`".repeat(4), "a", FENCE], { line: 0, end: 3, fence: true })).toBe(`a\n${FENCE}`);
    expect(yankText([FENCE, "a", "~~~"], { line: 0, end: 3, fence: true })).toBe("a\n~~~");
    expect(yankText(["  " + FENCE, "a", "  " + FENCE + "  "], { line: 0, end: 3, fence: true })).toBe("a");
  });
  it("copies every line when the block is not a fence, fence-like lines included", () => {
    expect(yankText(lines, { line: 5, end: 9, fence: false })).toBe(`${FENCE}sh\nmake\nmake install\n${FENCE}`);
  });
  it("drops a carriage return that ends a line, and clamps the range to the snapshot", () => {
    expect(yankText(["a\r", "b\r", "c"], { line: 0, end: 2, fence: false })).toBe("a\nb");
    expect(yankText(["a", "b"], { line: 1, end: 9, fence: false })).toBe("b");
    expect(yankText(["a", "b"], { line: 5, end: 9, fence: false })).toBe("");
  });
});
