import { defineConfig, configDefaults } from "vitest/config";
import base from "./vitest.config";

export default defineConfig({
  ...base,
  // Custom env values in Vitest are strings; the phone bundle inlines a boolean constant.
  plugins: [{ name: "phone-page-constant", enforce: "pre", transform(source, id) {
    if (id.includes("/src/")) return source.replaceAll("import.meta.env.EITRI_PHONE_PAGE", "true");
  } }],
  test: { ...base.test, include: ["src/historyEvicted.phone.test.tsx"], exclude: configDefaults.exclude },
});
