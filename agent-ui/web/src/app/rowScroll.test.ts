// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { clampCursorToView, revealRow, scrollVisibleRowBox, stepWithinRow } from "./rowScroll";
import { PROMPT_HEADER_CLASS } from "../promptHeader";

/* The DOM half of moving through the conversation, with the pinned prompt header shown over the list's top
   (`../promptHeader`): a 400px list at the viewport's top, rows at fixed content positions following `scrollTop`,
   a 20px text line (a 40px margin, a 60px step) and a 24px header. jsdom lays nothing out, so every rect here is
   planted; the pure decisions are pinned in `../jkScroll.test.ts`. */

const HEADER = 24;

afterEach(() => {
  document.body.innerHTML = "";
});

function layout(spans: [number, number][], header: boolean) {
  const scroller = document.createElement("div");
  const list = document.createElement("div");
  list.className = "message-list";
  scroller.append(list);
  let scrollTop = 1000;
  Object.defineProperty(list, "clientHeight", { configurable: true, value: 400 });
  Object.defineProperty(list, "scrollHeight", { configurable: true, value: 6000 });
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = Math.max(0, Math.min(5600, v));
    },
  });
  list.getBoundingClientRect = () => ({ top: 0, bottom: 400 }) as DOMRect;
  const rows = spans.map(([top, bottom]) => {
    const row = document.createElement("div");
    row.dataset.navStop = "row";
    const body = document.createElement("div");
    body.className = "row-body";
    body.style.lineHeight = "20px";
    row.append(body);
    row.getBoundingClientRect = () => ({ top: top - scrollTop, bottom: bottom - scrollTop }) as DOMRect;
    list.append(row);
    return row;
  });
  if (header) {
    const el = document.createElement("div");
    el.className = PROMPT_HEADER_CLASS;
    el.getBoundingClientRect = () => ({ top: 0, bottom: HEADER }) as DOMRect;
    scroller.append(el);
  }
  document.body.append(scroller);
  return { list, rows, at: () => scrollTop };
}

describe("revealRow with the header shown", () => {
  it("lands a row below the header, with its margin", () => {
    const plain = layout([[1010, 1100]], false);
    revealRow(plain.list, plain.rows[0], 0);
    expect(plain.at()).toBe(970);
    document.body.innerHTML = "";
    const covered = layout([[1010, 1100]], true);
    revealRow(covered.list, covered.rows[0], 0);
    expect(covered.at()).toBe(1010 - HEADER - 40);
  });
});

describe("stepWithinRow with the header shown", () => {
  it("k steps a tall row until its head is clear of the header", () => {
    const plain = layout([[1050, 3000]], false);
    expect(stepWithinRow(plain.rows[0], -1, 1, false)).toBe(0);
    document.body.innerHTML = "";
    const covered = layout([[1050, 3000]], true);
    expect(stepWithinRow(covered.rows[0], -1, 1, false)).toBe(1);
    expect(covered.at()).toBe(1050 - 40 - HEADER);
  });
});

describe("clampCursorToView with the header shown", () => {
  it("re-homes a cursor whose row is wholly under the header to the first row below it", () => {
    // Row 0 shows 20px at the list's top, all of it under the 24px header.
    const plain = layout([[980, 1020], [1020, 1200]], false);
    expect(clampCursorToView(plain.list, plain.rows, 0)).toBeNull();
    document.body.innerHTML = "";
    const covered = layout([[980, 1020], [1020, 1200]], true);
    expect(clampCursorToView(covered.list, covered.rows, 0)).toBe(1);
    // A row that still shows below the header is left alone.
    expect(clampCursorToView(covered.list, covered.rows, 1)).toBeNull();
  });
});

describe("scrollVisibleRowBox with the header shown", () => {
  it("leaves a box wholly under the header to the conversation", () => {
    for (const header of [false, true]) {
      document.body.innerHTML = "";
      const { list, rows } = layout([[900, 1300]], header);
      const box = document.createElement("div");
      box.className = "tool-result-body";
      Object.defineProperty(box, "clientHeight", { configurable: true, value: 20 });
      Object.defineProperty(box, "scrollHeight", { configurable: true, value: 200 });
      box.getBoundingClientRect = () => ({ top: 2, bottom: 22 }) as DOMRect;
      rows[0].append(box);
      expect(list.contains(box)).toBe(true);
      expect(scrollVisibleRowBox(rows[0], 1)).toBe(!header);
    }
  });
});
