import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { rename } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));
execFileSync(process.execPath, [fileURLToPath(new URL("node_modules/typescript/bin/tsc", import.meta.url)),
  "--emitDeclarationOnly", "--project", "tsconfig.json"], { cwd: root, stdio: "inherit" });
await rename(new URL("dist/index.d.ts", import.meta.url), new URL("dist/keys.d.ts", import.meta.url));
await build({
  absWorkingDir: root,
  entryPoints: ["src/index.ts"],
  outfile: "dist/keys.js",
  bundle: true,
  format: "iife",
  target: "es2020",
  // Vimium's notice (a `/*!` comment) has to travel with every copy of the bundle, including a vendored one.
  legalComments: "eof",
});
