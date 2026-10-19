import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";
import { effectiveRows } from "./bridge";

it("reads the host's shared keys.declare reply with separate key and source arrays", () => {
  const path = resolve(dirname(fileURLToPath(import.meta.url)), "../../../core/src/plugin/fixtures/rep-keys-declare.json");
  const reply = JSON.parse(readFileSync(path, "utf8"));
  expect(effectiveRows(reply.result)).toEqual([
    { action: "scroll.down", keys: ["j"], context: "页面", desc: "向下滚动" },
    { action: "item.next", keys: ["]q", "<C-n>"], context: "题目", desc: "下一题" },
    { action: "hint.open", keys: ["f"], context: "页面", desc: "给能点的地方标上字母" },
  ]);
});
