import { describe, expect, it } from "vitest";
import * as mod from "./index";

describe("package", () => {
  it("loads", () => {
    expect(mod).toBeDefined();
  });
});
