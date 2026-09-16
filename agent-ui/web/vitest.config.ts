import { defineConfig } from "vite";

export default defineConfig({
  test: {
    environment: "node",
    // Vitest mocks `.css` imports to an empty module by default (skipping real CSS work is
    // usually the right call for tests). `index.css?raw` needs the real bytes -- it is read as
    // a plain-text fixture, not stylesheet output -- so this file specifically is exempted.
    css: { include: [/index\.css/] },
  },
});
