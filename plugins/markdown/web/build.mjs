import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { copyFile, mkdir, readdir, readFile, rm } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));
const dist = new URL("dist/", import.meta.url);

// A type error fails the build, as it does for the keyboard kit.
execFileSync(process.execPath, [fileURLToPath(new URL("node_modules/typescript/bin/tsc", import.meta.url)),
  "--noEmit", "--project", "tsconfig.json"], { cwd: root, stdio: "inherit" });

// The directory is the installed site, so nothing from an earlier build may linger in it.
await rm(dist, { recursive: true, force: true });
await mkdir(new URL("katex/fonts/", dist), { recursive: true });

// KaTeX's stylesheet names every face as woff2, woff and ttf. WebKitGTK reads woff2, so only those ship, beside the
// page under katex/fonts/, and the stylesheet names only them.
const katexFonts = {
  name: "katex-fonts",
  setup(b) {
    b.onLoad({ filter: /katex[\\/]dist[\\/]katex\.css$/ }, async (args) => ({
      loader: "css",
      contents: (await readFile(args.path, "utf8"))
        .replace(/,\s*url\(fonts\/[^)]+\.(?:woff|ttf)\)\s*format\("(?:woff|truetype)"\)/g, ""),
    }));
    b.onResolve({ filter: /^fonts\/KaTeX_[A-Za-z0-9_-]+\.woff2$/ }, (args) => ({ path: `katex/${args.path}`, external: true }));
  },
};

const common = {
  absWorkingDir: root,
  bundle: true,
  format: "esm",
  target: "es2020",
  minify: true,
  // VS Code's notice and every package's own `/*!` notice travel with the bundle.
  legalComments: "eof",
  alias: { "markdown-it/lib/token.js": "markdown-it/lib/token.mjs" },
  logLevel: "warning",
};
// The page. Mermaid is a second file, imported the first time a document holds a diagram.
await build({ ...common, entryPoints: ["src/main.ts"], outfile: "dist/preview.js", external: ["./mermaid.js"],
  plugins: [katexFonts] });
// Mermaid with everything it imports in one file, rather than a hundred chunks to ship and list.
await build({ ...common, entryPoints: ["src/mermaid-entry.ts"], outfile: "dist/mermaid.js", splitting: false });

await copyFile(new URL("index.html", import.meta.url), new URL("index.html", dist));
const fonts = new URL("node_modules/katex/dist/fonts/", import.meta.url);
for (const name of (await readdir(fonts)).filter((n) => n.endsWith(".woff2"))) {
  await copyFile(new URL(name, fonts), new URL(`katex/fonts/${name}`, dist));
}
