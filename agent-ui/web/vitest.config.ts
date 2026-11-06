import { defineConfig } from "vite";
import { configDefaults } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    setupFiles: ["./src/testSetup.ts"],
    // `scripts/` holds `node --test` files (`npm test` runs them after Vitest), not Vitest suites.
    exclude: [...configDefaults.exclude, "scripts/**"],
    // Vitest mocks `.css` imports to an empty module by default (skipping real CSS work is
    // usually the right call for tests). `index.css?raw` needs the real bytes -- it is read as
    // a plain-text fixture, not stylesheet output -- so this file specifically is exempted.
    css: { include: [/index\.css/] },
  },
});
