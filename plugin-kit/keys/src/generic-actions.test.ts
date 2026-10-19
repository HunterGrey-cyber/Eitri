import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";
import { GENERIC_ROWS } from "./rows";

it("keeps the generic actions in the host's shared order", () => {
  const list = resolve(dirname(fileURLToPath(import.meta.url)), "../../../core/src/plugin/kit_generic_actions.json");
  const shared = JSON.parse(readFileSync(list, "utf8")) as { actions: string[] };
  expect(GENERIC_ROWS.map((row) => row.action)).toEqual(shared.actions);
});
