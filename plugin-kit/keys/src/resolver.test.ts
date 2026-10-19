import { describe, expect, it } from "vitest";
import { parseNotation } from "./notation";
import { createResolver } from "./resolver";
import { GENERIC_ROWS, type Row } from "./rows";

const row: Row = { action: "item.next", keys: ["]q"], context: "阅读", desc: "下一题" };
const stroke = (key: string) => parseNotation(key)![0];

describe("resolver", () => {
  it("completes gg within the timeout", () => {
    let now = 0;
    const resolver = createResolver(GENERIC_ROWS, () => 1000, () => now);
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g" });
    now = 900;
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "action", action: "scroll.top" });
  });

  it("drops an expired prefix and resets", () => {
    let now = 0;
    const resolver = createResolver(GENERIC_ROWS, () => 1000, () => now);
    resolver.feed(stroke("g"));
    now = 1001;
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "none" });
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g" });
  });

  it("]q waits after ] and completes on q", () => {
    const resolver = createResolver([row], () => 1000, () => 0);
    expect(resolver.feed(stroke("]"))).toEqual({ kind: "pending", prefix: "]" });
    expect(resolver.feed(stroke("q"))).toEqual({ kind: "action", action: "item.next" });
  });

  it("drops an unknown completion without starting a different action", () => {
    const resolver = createResolver(GENERIC_ROWS, () => 1000, () => 0);
    resolver.feed(stroke("g"));
    expect(resolver.feed(stroke("j"))).toEqual({ kind: "none" });
    expect(resolver.feed(stroke("j"))).toEqual({ kind: "action", action: "scroll.down" });
  });

  it("reset discards a pending prefix", () => {
    const resolver = createResolver([row], () => 1000, () => 0);
    resolver.feed(stroke("]"));
    resolver.reset();
    expect(resolver.feed(stroke("q"))).toEqual({ kind: "none" });
  });

  for (const row of GENERIC_ROWS) {
    it(`resolves every displayed key for ${row.action}`, () => {
      for (const key of row.keys) {
        const resolver = createResolver(GENERIC_ROWS, () => 1000, () => 0);
        const seq = parseNotation(key);
        expect(seq).not.toBeNull();
        let result;
        for (const part of seq!) result = resolver.feed(part);
        expect(result).toEqual({ kind: "action", action: row.action });
      }
    });
  }
});
