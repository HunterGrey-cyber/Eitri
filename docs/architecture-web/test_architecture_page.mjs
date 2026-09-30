import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const page = await readFile(new URL("./index.html", import.meta.url), "utf8");

assert.match(page, /<title>Eitri · architecture<\/title>/);
assert.match(page, /id="theme-toggle"/);
assert.match(page, /<svg[^>]*aria-label="Eitri runtime architecture"/);

for (const label of [
  "GTK4 shell",
  "neovide-editor",
  "agent-ui/web",
  "eitri-core",
  "legacy",
  "sidecar",
  "terminal-pane",
  "semantic-pane",
]) {
  assert.ok(page.includes(label), `missing architecture component: ${label}`);
}

for (const flow of ["编辑路径", "智能体路径", "Verdandi 运行时路径"]) {
  assert.ok(page.includes(flow), `missing data-flow label: ${flow}`);
}

console.log("architecture page content contract passed");
