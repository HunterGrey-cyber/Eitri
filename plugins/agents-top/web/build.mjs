import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { copyFile, mkdir, rm } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));
execFileSync(process.execPath, [fileURLToPath(new URL("node_modules/typescript/bin/tsc", import.meta.url)),
  "--noEmit", "--project", "tsconfig.json"], { cwd: root, stdio: "inherit" });
await rm(new URL("dist/", import.meta.url), { recursive: true, force: true });
await mkdir(new URL("dist/", import.meta.url), { recursive: true });
await build({ absWorkingDir: root, entryPoints: ["src/index.ts"], outfile: "dist/top.js", bundle: true,
  format: "esm", target: "es2020", minify: true, legalComments: "eof" });
await copyFile(new URL("index.html", import.meta.url), new URL("dist/index.html", import.meta.url));
