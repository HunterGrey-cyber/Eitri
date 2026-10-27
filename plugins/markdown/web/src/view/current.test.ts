import { describe, expect, it } from "vitest";
import { boxAt, clickedRecently, currentBlock, lineAt, pickBlock, pickedLine, setClickedBlock } from "./current";
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
    expect(boxAt(INDEX, 500)).toBe(INDEX[2]);
    expect(boxAt([], 5)).toBeNull();
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
  it("is the block holding nvim's cursor when the keys picked it, keeping that line, until the next scroll", () => {
    scrollTo(0);
    pickBlock(INDEX[2], 6);
    expect(currentBlock(fake(22))).toBe(INDEX[2]);
    expect(pickedLine(INDEX[2])).toBe(6);
    expect(pickedLine(INDEX[1])).toBeNull();
    scrollTo(5);
    expect(pickedLine(INDEX[2])).toBeNull();
    expect(currentBlock(fake(27))).toBe(INDEX[1]);
  });
  it("tells a recent click from the keys' pick", () => {
    scrollTo(0);
    setClickedBlock(null);
    expect(clickedRecently()).toBe(false);
    pickBlock(INDEX[0], 0);
    expect(clickedRecently()).toBe(false);
    setClickedBlock(INDEX[1]);
    expect(clickedRecently()).toBe(true);
    expect(pickedLine(INDEX[1])).toBeNull();
  });
});

describe("the line at a position", () => {
  it("is the line under it inside a block, else the first line of the block below", () => {
    expect(lineAt(INDEX, 10)).toBe(0);
    expect(lineAt(INDEX, 25)).toBe(2);
    expect(lineAt(INDEX, 60 + 45)).toBe(6);
    expect(lineAt(INDEX, 159)).toBe(8);
    expect(lineAt(INDEX, 500)).toBe(4);
    expect(lineAt([], 5)).toBeNull();
  });

  it("past the end is the last block, never the end sentinel", () => {
    const end = document.createElement("div");
    end.setAttribute("data-line", "9");
    const index: Box[] = [...INDEX, { line: 9, end: 10, top: 170, height: 1, el: end }];
    expect(boxAt(index, 165)).toBe(INDEX[2]);
    expect(boxAt(index, 170)).toBe(INDEX[2]);
    expect(boxAt(index, 900)).toBe(INDEX[2]);
  });
});
