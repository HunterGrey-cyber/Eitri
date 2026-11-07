import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";
// A plain ES module (no type declarations): `withPolicy` takes the page's HTML and returns it with its policy.
import { withPolicy } from "./scripts/transcript-bundle-check.mjs";

const PAGE = "index.transcript.html";

/** After the single-file plugin has inlined everything: names the page `index.html`, writes its policy (the one
 *  inline script pinned by its hash) as the first element of `<head>`, and the hash beside it as
 *  `script-sha256.txt`, which the app sends as the page's header. A page it cannot pin fails the build. */
function pinnedPolicy(): Plugin {
  return {
    name: "eitri-transcript-policy",
    enforce: "post",
    generateBundle(_options, bundle) {
      const page = bundle[PAGE];
      if (page === undefined || page.type !== "asset" || typeof page.source !== "string") {
        this.error(`${PAGE} is not in the bundle as one HTML file`);
      }
      const { html, hash } = withPolicy(page.source) as { html: string; hash: string };
      page.source = html;
      page.fileName = "index.html";
      delete bundle[PAGE];
      bundle["index.html"] = page;
      this.emitFile({ type: "asset", fileName: "script-sha256.txt", source: `${hash}\n` });
    },
  };
}

// The phone's transcript page: its own entry and its own single file, written to `dist-transcript/index.html`. The
// desk never loads it (`shell/build.rs` does not build it).
export default defineConfig({
  plugins: [react(), viteSingleFile(), pinnedPolicy()],
  // Read by code shared with the desk to leave out what only the desk's page may carry (the desk's history notice
  // and the message its button posts); the desk's build and the tests leave it unset.
  define: { "import.meta.env.EITRI_PHONE_PAGE": "true" },
  build: {
    outDir: "dist-transcript",
    emptyOutDir: true,
    cssCodeSplit: false,
    rollupOptions: { input: fileURLToPath(new URL(PAGE, import.meta.url)) },
  },
});
