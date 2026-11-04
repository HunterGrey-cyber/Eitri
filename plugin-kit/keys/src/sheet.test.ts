// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { closeSheet, openSheet, sheetKey } from "./sheet";
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

function contexts() {
  return [...document.querySelectorAll("#eitri-keys-sheet .eitri-keys-sheet-context")].map((h) => h.textContent);
}
function genericLine() {
  return document.querySelector("#eitri-keys-sheet .eitri-keys-sheet-generic")?.textContent ?? null;
}

it("lists the page's rows under their contexts, then one line of the generic rows by group", () => {
  openSheet([...GENERIC_ROWS, ...pageRows]);
  expect(contexts()).toEqual(["reading", "item"]);
  expect(rowTexts()).toEqual([
    "]q / <C-n>  next item",
    "1  pick",
    "scroll j / <C-e> k / <C-y> d / <C-d> u / <C-u> <C-f> <C-b> gg G · history H / <C-o> L / <C-i> · pages [[ ]] · find / n N · hints f · visual v V · help ? · leave <Esc>",
  ]);
  expect(genericLine()).toBe(rowTexts().at(-1));
});

it("folds a page row that reuses a generic action into its group, and leaves out a row with no key", () => {
  openSheet([
    { action: "hint.start", keys: ["F"], context: "page", desc: "labels" },
    pageRows[0],
    { action: "item.gone", keys: [], context: "x", desc: "y" },
  ]);
  expect(contexts()).toEqual(["reading"]);
  expect(rowTexts()).toEqual(["]q / <C-n>  next item", "hints F"]);
});

it("names the groups with the page's labels, keeping the defaults it leaves out", () => {
  openSheet([...GENERIC_ROWS, ...pageRows], { groups: { scroll: "滚动", history: "历史" } });
  expect(genericLine()).toBe(
    "滚动 j / <C-e> k / <C-y> d / <C-d> u / <C-u> <C-f> <C-b> gg G · 历史 H / <C-o> L / <C-i> · pages [[ ]] · find / n N · hints f · visual v V · help ? · leave <Esc>",
  );
});

it("puts a row with no context before the first heading, without a heading of its own", () => {
  openSheet([{ action: "a.b", keys: ["x"], context: "", desc: "plain" }, pageRows[1]]);
  expect(contexts()).toEqual(["item"]);
  expect(rowTexts()).toEqual(["x  plain", "1  pick"]);
});

it("uses the page's labels for the title, footer and close button", () => {
  openSheet([...genericRows({ page: "页面" }), ...pageRows],
    { page: "页面", sheetTitle: "按键", sheetFooter: "Esc 关闭", sheetClose: "关闭" });
  const sheet = document.getElementById("eitri-keys-sheet")!;
  expect(sheet.getAttribute("aria-label")).toBe("按键");
  expect(sheet.querySelector("h2")!.textContent).toBe("按键");
  expect(sheet.querySelector(".eitri-keys-sheet-muted")!.textContent).toBe("Esc 关闭");
  expect(sheet.querySelector("button")!.textContent).toBe("关闭");
});

describe("keys inside the sheet", () => {
  const sized = () => {
    const sheet = document.getElementById("eitri-keys-sheet")!;
    Object.defineProperty(sheet, "clientHeight", { configurable: true, value: 300 });
    Object.defineProperty(sheet, "scrollHeight", { configurable: true, value: 2000 });
    return sheet;
  };
  it.each(["q", "?"])("%s closes it", (k) => {
    openSheet(GENERIC_ROWS);
    expect(sheetKey(k)).toBe(true);
    expect(document.getElementById("eitri-keys-sheet")).toBeNull();
  });
  it("j and k move the sheet by one step, d and u by half its height", () => {
    openSheet(GENERIC_ROWS);
    const sheet = sized();
    sheetKey("j");
    expect(sheet.scrollTop).toBe(60);
    sheetKey("d");
    expect(sheet.scrollTop).toBe(210);
    sheetKey("k");
    expect(sheet.scrollTop).toBe(150);
    sheetKey("u");
    expect(sheet.scrollTop).toBe(0);
  });
  it("gg goes to the top and G to the bottom; a lone g does nothing yet", () => {
    openSheet(GENERIC_ROWS);
    const sheet = sized();
    expect(sheetKey("G")).toBe(true);
    expect(sheet.scrollTop).toBe(2000);
    expect(sheetKey("g")).toBe(true);
    expect(sheet.scrollTop).toBe(2000);
    expect(sheetKey("g")).toBe(true);
    expect(sheet.scrollTop).toBe(0);
  });
  it("any other key is not the sheet's", () => {
    openSheet(GENERIC_ROWS);
    expect(sheetKey("x")).toBe(false);
    expect(document.getElementById("eitri-keys-sheet")).not.toBeNull();
  });
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
