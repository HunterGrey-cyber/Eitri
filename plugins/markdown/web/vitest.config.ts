import { defineConfig } from "vitest/config";

export default defineConfig({
  // `@hedgedoc/markdown-it-task-lists` imports `markdown-it/lib/token.js`, a path markdown-it 14 does not have; the
  // alias applies only to modules vite processes, so that package is inlined.
  resolve: { alias: [{ find: /^markdown-it\/lib\/token\.js$/, replacement: "markdown-it/lib/token.mjs" }] },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
    maxWorkers: 1,
    minWorkers: 1,
    server: { deps: { inline: ["@hedgedoc/markdown-it-task-lists"] } },
  },
});
