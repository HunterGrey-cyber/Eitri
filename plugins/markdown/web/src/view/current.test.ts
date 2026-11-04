import { describe, expect, it, vi } from "vitest";
import { boxAt, CLICK_WINS_MS, clickedRecently, currentBlock, lineAt, noteClick, pickBlock, pickedLine } from "./current";
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
  it("is the block at the reading point, wherever a click or the keys' pick was made", () => {
    scrollTo(0);
    pickBlock(INDEX[2], 6, 1);
    expect(currentBlock(fake(22))).toBe(INDEX[1]);
    expect(currentBlock(fake(100))).toBe(INDEX[2]);
  });
  it("keeps nvim's picked line for the block it was picked for until the cursor moves", () => {
    pickBlock(INDEX[2], 6, 4);
    expect(pickedLine(INDEX[2], 4)).toBe(6);
    expect(pickedLine(INDEX[1], 4)).toBeNull();
    // A scroll that leaves the cursor where it was keeps it; a move of the cursor drops it.
    scrollTo(5);
    expect(pickedLine(INDEX[2], 4)).toBe(6);
    expect(pickedLine(INDEX[2], 5)).toBeNull();
  });
  it("tells a recent click from the keys' pick", () => {
    let now = 1e9;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    try {
      expect(clickedRecently()).toBe(false);
      pickBlock(INDEX[0], 0, 1);
      expect(clickedRecently()).toBe(false);
      noteClick();
      expect(clickedRecently()).toBe(true);
      now += CLICK_WINS_MS;
      expect(clickedRecently()).toBe(false);
    } finally {
      vi.restoreAllMocks();
    }
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
