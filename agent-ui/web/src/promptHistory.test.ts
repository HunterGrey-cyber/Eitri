import { describe, expect, it } from "vitest";
import { searchHistory, stepHistory } from "./promptHistory";

const entries = ["fix the parser", "Run the tests", "run the linter"];

describe("prompt history", () => {
  it("walks from the newest back, keeps the draft it left, and returns it past the newest", () => {
    let step = stepHistory(entries, { index: null, stash: "" }, "half", -1)!;
    expect(step.text).toBe("run the linter");
    step = stepHistory(entries, step.walk, step.text, -1)!;
    expect(step.text).toBe("Run the tests");
    step = stepHistory(entries, step.walk, step.text, 1)!;
    expect(step.text).toBe("run the linter");
    step = stepHistory(entries, step.walk, step.text, 1)!;
    expect(step).toEqual({ walk: { index: null, stash: "" }, text: "half" });
    expect(stepHistory(entries, { index: null, stash: "" }, "x", 1), "nothing below the draft").toBeNull();
    expect(stepHistory(entries, { index: 0, stash: "" }, "fix the parser", -1)).toBeNull();
    expect(stepHistory([], { index: null, stash: "" }, "", -1)).toBeNull();
  });

  it("searches newest first, smartcase, and steps older from a match", () => {
    expect(searchHistory(entries, "run", null)).toBe(2);
    expect(searchHistory(entries, "run", 2), "Run matches a lowercase query").toBe(1);
    expect(searchHistory(entries, "Run", null), "an uppercase letter makes it case-sensitive").toBe(1);
    expect(searchHistory(entries, "nothing", null)).toBeNull();
    expect(searchHistory(entries, "", null)).toBeNull();
  });
});
