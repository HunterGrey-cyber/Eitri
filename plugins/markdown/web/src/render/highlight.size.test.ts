// @vitest-environment node
import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";

const here = fileURLToPath(new URL(".", import.meta.url));
const web = fileURLToPath(new URL("../..", import.meta.url));

async function bundled(contents: string): Promise<number> {
  const out = await build({
    stdin: { contents, resolveDir: here, loader: "ts" },
    absWorkingDir: web,
    bundle: true,
    write: false,
    minify: true,
    format: "esm",
    target: "es2020",
    logLevel: "silent",
  });
  return out.outputFiles[0].contents.length;
}

it("keeps the grammar set within five times highlight.js's own core", async () => {
  const core = await bundled('import hljs from "highlight.js/lib/core"; export { hljs };');
  const full = await bundled('export { highlightCode, LANGUAGES } from "./highlight";');
  expect(full - core).toBeGreaterThan(core);
  expect(full - core).toBeLessThanOrEqual(5 * core);
}, 60_000);
