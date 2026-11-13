import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

export default defineConfig({
  plugins: [react(), viteSingleFile()],
  build: {
    target: "es2022",
    // A single conversation panel's transcript is not large enough to need code-splitting,
    // and vite-plugin-singlefile requires everything inlined into one HTML file anyway.
    cssCodeSplit: false,
    // The polyfill only serves `<link rel="modulepreload">` tags, and a single inlined file has none.
    modulePreload: { polyfill: false },
  },
});
