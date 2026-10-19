// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { closeSheet, openSheet } from "./sheet";
import { GENERIC_ROWS, genericRows } from "./rows";
import type { Row } from "./rows";

const pageRows: Row[] = [
  { action: "item.next", keys: ["]q", "<C-n>"], context: "reading", desc: "next item" },
  { action: "item.pick", keys: ["1"], context: "item", desc: "pick" },
];

function rowTexts() {
  return [...document.querySelectorAll("#eitri-keys-sheet .eitri-keys-sheet-row")].map((row) => row.textContent);
}

afterEach(() => {
  closeSheet();
  document.body.replaceChildren();
  document.head.replaceChildren();
});

it("lists the page's rows first, then one folded line for the generic rows", () => {
  openSheet([...GENERIC_ROWS, ...pageRows]);
  expect(rowTexts()).toEqual([
    "]q / <C-n>  reading  next item",
    "1  item  pick",
    "j k d u gg G H L [[ ]] f ? <Esc>  page",
  ]);
});

it("folds a page row that reuses a generic action, and leaves out a row with no key", () => {
  openSheet([
    { action: "hint.start", keys: ["F"], context: "page", desc: "labels" },
    pageRows[0],
    { action: "item.gone", keys: [], context: "x", desc: "y" },
  ]);
  expect(rowTexts()).toEqual(["]q / <C-n>  reading  next item", "F  page"]);
});

it("labels the folded line with labels.page when the folded rows share no context", () => {
  openSheet([
    { action: "scroll.down", keys: ["j", "<C-e>"], context: "a", desc: "down" },
    { action: "scroll.up", keys: ["k"], context: "b", desc: "up" },
  ], { page: "P" });
  expect(rowTexts()).toEqual(["j / <C-e> k  P"]);
});

it("uses the page's labels for the title, footer, close button and folded line", () => {
  openSheet([...genericRows({ page: "页面" }), ...pageRows],
    { page: "页面", sheetTitle: "按键", sheetFooter: "Esc 关闭", sheetClose: "关闭" });
  const sheet = document.getElementById("eitri-keys-sheet")!;
  expect(sheet.getAttribute("aria-label")).toBe("按键");
  expect(sheet.querySelector("h2")!.textContent).toBe("按键");
  expect(rowTexts().at(-1)).toBe("j k d u gg G H L [[ ]] f ? <Esc>  页面");
  expect(sheet.querySelector(".eitri-keys-sheet-muted")!.textContent).toBe("Esc 关闭");
  expect(sheet.querySelector("button")!.textContent).toBe("关闭");
});

it("defaults to English", () => {
  openSheet(GENERIC_ROWS);
  const sheet = document.getElementById("eitri-keys-sheet")!;
  expect(sheet.querySelector("h2")!.textContent).toBe("Keys");
  expect(sheet.querySelector(".eitri-keys-sheet-muted")!.textContent)
    .toBe("Esc closes · Ctrl+h / Ctrl+l move between modules");
  expect(sheet.querySelector("button")!.textContent).toBe("Close");
});

it("creates only text, and colours nothing inline", () => {
  openSheet([{ action: "x.y", keys: ["x"], context: "<b>c</b>", desc: '<img src=x onerror="alert(1)">' }],
    { sheetTitle: "<i>t</i>" });
  const sheet = document.getElementById("eitri-keys-sheet")!;
  expect(sheet.querySelector("img, b, i")).toBeNull();
  expect(sheet.textContent).toContain('<img src=x onerror="alert(1)">');
  for (const el of [sheet, ...sheet.querySelectorAll<HTMLElement>("*")]) expect(el.getAttribute("style")).toBeNull();
  expect(document.getElementById("eitri-keys-style")).not.toBeNull();
});
