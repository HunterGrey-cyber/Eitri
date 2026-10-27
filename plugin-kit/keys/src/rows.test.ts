// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { install, mergeRows } from "./install";
import { GENERIC_ROWS, genericRows, isGenericAction } from "./rows";

afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });

it("names the generic rows in English by default", () => {
  expect(GENERIC_ROWS.map(({ action, keys, context, desc }) => [action, keys.join(" "), context, desc])).toEqual([
    ["scroll.down", "j <C-e>", "page", "scroll down"],
    ["scroll.up", "k <C-y>", "page", "scroll up"],
    ["scroll.half_down", "d <C-d>", "page", "half a page down"],
    ["scroll.half_up", "u <C-u>", "page", "half a page up"],
    ["scroll.page_down", "<C-f>", "page", "a page down"],
    ["scroll.page_up", "<C-b>", "page", "a page up"],
    ["scroll.top", "gg", "page", "top of the page"],
    ["scroll.bottom", "G", "page", "bottom of the page"],
    ["history.back", "H <C-o>", "page", "back"],
    ["history.forward", "L <C-i>", "page", "forward"],
    ["link.prev", "[[", "page", "previous page"],
    ["link.next", "]]", "page", "next page"],
    ["find.start", "/", "page", "find in the page"],
    ["find.next", "n", "page", "next match"],
    ["find.prev", "N", "page", "previous match"],
    ["hint.start", "f", "page", "hint mode"],
    ["sheet.open", "?", "page", "this sheet"],
    ["mode.escape", "<Esc>", "page", "leave a mode"],
  ]);
});

it("replaces the generic words with the page's labels, keeping the defaults it leaves out", () => {
  const rows = genericRows({ page: "页面", generic: { "scroll.down": "向下滚动" } });
  expect(rows[0]).toEqual({ action: "scroll.down", keys: ["j", "<C-e>"], context: "页面", desc: "向下滚动" });
  expect(rows[1]).toEqual({ action: "scroll.up", keys: ["k", "<C-y>"], context: "页面", desc: "scroll up" });
  expect(GENERIC_ROWS[0].context).toBe("page");
  expect(mergeRows([], { page: "页面" }, "Ctrl+a")).toEqual(genericRows({ page: "页面" }));
  expect(mergeRows([], { page: "页面" }, "Ctrl+b")).toEqual(
    genericRows({ page: "页面" }).filter((row) => row.action !== "scroll.page_up"));
});

it("tells a generic action from a page action", () => {
  expect(GENERIC_ROWS.every((row) => isGenericAction(row.action))).toBe(true);
  expect(isGenericAction("item.next")).toBe(false);
});

it("declares the labelled rows when installing", async () => {
  const call = vi.fn(async (_verb: string, args: unknown) => args);
  window.eitriPlugin = { call, on: () => () => {} };
  const remove = await install({
    rows: [], run: () => true, labels: { page: "页面", generic: { "hint.start": "提示模式" } },
  });
  const declared = (call.mock.calls[0][1] as { rows: { action: string; context: string; desc: string }[] }).rows;
  expect(declared.find((row) => row.action === "hint.start")).toMatchObject({ context: "页面", desc: "提示模式" });
  expect(declared.find((row) => row.action === "scroll.down")).toMatchObject({ context: "页面", desc: "scroll down" });
  remove();
});
