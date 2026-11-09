import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { expect, it } from "vitest";

it("the BROWSE gf guides say an early-stopped lookup offers labels and warns about missing matches", () => {
  const english = readFileSync(new URL("../../../publish/files/docs/guide/keys.md", import.meta.url), "utf8");
  const chinese = readFileSync(new URL("../../../publish/files/docs/guide/keys.zh-CN.md", import.meta.url), "utf8");
  const row = english.split("\n").find((line) => line.startsWith("| `gf` |"))!;
  expect(row).toContain("stops early");
  expect(row).toContain("never opens by itself");
  expect(row).toContain("offers the matches it found as labels");
  expect(row).toContain("list may be incomplete");
  const rowZh = chinese.split("\n").find((line) => line.startsWith("| `gf` |"))!;
  expect(rowZh).toContain("提前停止");
  expect(rowZh).toContain("不会自动打开");
  expect(rowZh).toContain("找到的匹配显示为标签");
  expect(rowZh).toContain("列表可能不完整");
});

it("the selecting-text guides explain gf's changed-selection exit and stay paired", () => {
  const english = readFileSync(new URL("../../../publish/files/docs/guide/keys.md", import.meta.url), "utf8");
  const chinese = readFileSync(new URL("../../../publish/files/docs/guide/keys.zh-CN.md", import.meta.url), "utf8");
  const selecting = english.split("### Selecting text")[1].split("## Answering a card")[0];
  expect(selecting).toMatch(/changed selection|selection (?:has )?changed/);
  expect(selecting).toMatch(/none (?:is )?left|no (?:live )?selection/);
  expect(selecting).toMatch(/ends the region with a flash/);
  const selectingZh = chinese.split("- `gf` 在插入符模式")[1].split("\n- ")[0];
  expect(selectingZh).toMatch(/选区.*变化/);
  expect(selectingZh).toContain("提示");
  expect(selectingZh).toContain("结束");
  expect(chinese.split("\n")[1]).toBe(`<!-- translated-from: keys.md sha256=${createHash("sha256").update(english).digest("hex")} -->`);
});
