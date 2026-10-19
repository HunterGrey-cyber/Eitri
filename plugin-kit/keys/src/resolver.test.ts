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
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g", length: 1 });
    now = 900;
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "action", action: "scroll.top" });
  });

  it("drops an expired prefix and resets", () => {
    let now = 0;
    const resolver = createResolver(GENERIC_ROWS, () => 1000, () => now);
    resolver.feed(stroke("g"));
    now = 1001;
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "none" });
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g", length: 1 });
  });

  it("]q waits after ] and completes on q", () => {
    const resolver = createResolver([row], () => 1000, () => 0);
    expect(resolver.feed(stroke("]"))).toEqual({ kind: "pending", prefix: "]", length: 1 });
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

  it("waits on a key that is bound and starts a longer key, and fires it after timeoutlen", () => {
    const rows = [...GENERIC_ROWS, { action: "item.next", keys: ["g"], context: "阅读", desc: "下一题" }];
    let now = 0;
    const resolver = createResolver(rows, () => 1000, () => now);
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g", length: 1 });
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "action", action: "scroll.top" });
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g", length: 1 });
    expect(resolver.expire()).toEqual({ kind: "fallback", action: "item.next", consumed: 1 });
    expect(resolver.expire()).toEqual({ kind: "none" });
    resolver.feed(stroke("g"));
    now = 1001;
    expect(resolver.feed(stroke("j"))).toEqual({ kind: "fallback", action: "item.next", consumed: 1 });
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g", length: 1 });
    expect(resolver.feed(stroke("x"))).toEqual({ kind: "fallback", action: "item.next", consumed: 1 });
  });

  it("keeps a shorter completed binding while extending a longer prefix", () => {
    const resolver = createResolver([
      { ...row, action: "short", keys: ["g"] },
      { ...row, action: "long", keys: ["gxy"] },
    ], () => 1000, () => 0);
    resolver.feed(stroke("g"));
    expect(resolver.feed(stroke("x"))).toEqual({ kind: "pending", prefix: "gx", length: 2 });
    expect(resolver.expire()).toEqual({ kind: "fallback", action: "short", consumed: 1 });
    resolver.feed(stroke("g"));
    resolver.feed(stroke("x"));
    expect(resolver.feed(stroke("y"))).toEqual({ kind: "action", action: "long" });
  });

  it("Esc cancels a pending key without its own action and then counts alone", () => {
    const rows = [...GENERIC_ROWS, { action: "item.next", keys: ["g"], context: "阅读", desc: "下一题" }];
    const resolver = createResolver(rows, () => 1000, () => 0);
    resolver.feed(stroke("g"));
    expect(resolver.feed(stroke("<Esc>"))).toEqual({ kind: "action", action: "mode.escape" });
    expect(resolver.expire()).toEqual({ kind: "none" });
  });

  it("completes a sequence that ends in Esc", () => {
    const rows = [...GENERIC_ROWS, { action: "item.custom", keys: ["g<Esc>"], context: "阅读", desc: "自定" }];
    const resolver = createResolver(rows, () => 1000, () => 0);
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g", length: 1 });
    expect(resolver.feed(stroke("<Esc>"))).toEqual({ kind: "action", action: "item.custom" });
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
