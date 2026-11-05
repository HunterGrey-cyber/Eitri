import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { copyFile, mkdir, rm } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));
const dist = new URL("dist/", import.meta.url);

// A type error fails the build, as it does for the kits.
execFileSync(process.execPath, [fileURLToPath(new URL("node_modules/typescript/bin/tsc", import.meta.url)),
  "--noEmit", "--project", "tsconfig.json"], { cwd: root, stdio: "inherit" });

// The directory is the installed site, so nothing from an earlier build may linger in it.
await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });
// One script and its stylesheet (`strips.css`, from the import in src/index.ts).
await build({
  absWorkingDir: root,
  entryPoints: ["src/index.ts"],
  outfile: "dist/strips.js",
  bundle: true,
  format: "esm",
  target: "es2020",
  minify: true,
  legalComments: "eof",
  logLevel: "warning",
});
await copyFile(new URL("index.html", import.meta.url), new URL("index.html", dist));
