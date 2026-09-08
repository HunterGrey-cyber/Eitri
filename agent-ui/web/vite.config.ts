import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

export default defineConfig({
  plugins: [react(), viteSingleFile()],
  build: {
    // A single conversation panel's transcript is not large enough to need code-splitting,
    // and vite-plugin-singlefile requires everything inlined into one HTML file anyway.
    cssCodeSplit: false,
  },
});
