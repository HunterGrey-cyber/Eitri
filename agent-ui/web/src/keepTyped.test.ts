import { describe, expect, it } from "vitest";
import { keepTyped } from "./keepTyped";

describe("keepTyped", () => {
  it("takes the host's text when nothing was typed and the host never had the box", () => {
    expect(keepTyped("q1", [], "")).toBe("q1");
  });
  it("replaces the host's stale copy of the box with the box", () => {
    expect(keepTyped("q1\n\nr", ["r"], "r1")).toBe("q1\n\nr1");
    expect(keepTyped("r", ["r"], "r1")).toBe("r1");
  });
  it("keeps a deletion made since the last mirror", () => {
    expect(keepTyped("run tests", ["run tests"], "run")).toBe("run");
    expect(keepTyped("q1\n\nold", ["old"], "")).toBe("q1");
  });
  it("never drops new text that the host's text happens to contain", () => {
    expect(keepTyped("check parser", [], "parser")).toBe("check parser\n\nparser");
  });
  it("matches a mirror still in flight or the one before it", () => {
    expect(keepTyped("q\n\nd", ["draft", "d"], "draft")).toBe("q\n\ndraft");
  });
  it("keeps a message queued from the box once, not twice", () => {
    expect(keepTyped("q\n\nq", ["q"], "")).toBe("q");
  });
  it("adds the box when the host never had it", () => {
    expect(keepTyped("", [], "r1")).toBe("r1");
    expect(keepTyped("q1", [], "w2")).toBe("q1\n\nw2");
  });
});
