import { describe, expect, it } from "vitest";
import { boxAt, currentBlock, setClickedBlock } from "./current";
import type { Box } from "./lineindex";
import type { Preview } from "./preview";

const el = () => document.createElement("p");
const INDEX: Box[] = [
  { line: 0, end: 1, top: 0, height: 20, el: el() },
  { line: 2, end: 3, top: 30, height: 20, el: el() },
  { line: 4, end: 9, top: 60, height: 100, el: el() },
];
const fake = (readingY: number) => ({ index: () => INDEX, readingY: () => readingY }) as unknown as Preview;
const scrollTo = (y: number) => Object.defineProperty(window, "scrollY", { value: y, configurable: true, writable: true });

describe("the current block", () => {
  it("is the block holding the reading position, else the first below it", () => {
    expect(boxAt(INDEX, 10)).toBe(INDEX[0]);
    expect(boxAt(INDEX, 25)).toBe(INDEX[1]);
    expect(boxAt(INDEX, 100)).toBe(INDEX[2]);
    expect(boxAt(INDEX, 500)).toBeNull();
    scrollTo(0);
    expect(currentBlock(fake(10))).toBe(INDEX[0]);
    expect(currentBlock(fake(22))).toBe(INDEX[1]);
  });
  it("is a clicked block until the next scroll", () => {
    scrollTo(0);
    setClickedBlock(INDEX[2]);
    expect(currentBlock(fake(22))).toBe(INDEX[2]);
    scrollTo(5);
    expect(currentBlock(fake(27))).toBe(INDEX[1]);
    scrollTo(0);
    expect(currentBlock(fake(10))).toBe(INDEX[0]);
    expect(currentBlock(fake(22))).toBe(INDEX[1]);
  });
});
