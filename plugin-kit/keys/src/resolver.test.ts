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

  it("with prefixWait off every prefix times out as before", () => {
    let now = 0;
    const resolver = createResolver(GENERIC_ROWS, () => 1000, () => now, { prefixWait: () => false });
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

  it("Esc after a pending prefix cancels it: no fallback and no mode.escape", () => {
    const plain = createResolver(GENERIC_ROWS, () => 1000, () => 0);
    plain.feed(stroke("g"));
    expect(plain.feed(stroke("<Esc>"))).toEqual({ kind: "cancelled" });
    expect(plain.expire()).toEqual({ kind: "none" });
    expect(plain.feed(stroke("<Esc>"))).toEqual({ kind: "action", action: "mode.escape" });
    const rows = [...GENERIC_ROWS, { action: "item.next", keys: ["g"], context: "阅读", desc: "下一题" }];
    const resolver = createResolver(rows, () => 1000, () => 0);
    resolver.feed(stroke("g"));
    expect(resolver.hasFallback()).toBe(true);
    expect(resolver.feed(stroke("<Esc>"))).toEqual({ kind: "cancelled" });
    expect(resolver.expire()).toEqual({ kind: "none" });
    expect(resolver.hasFallback()).toBe(false);
  });

  it("a prefix with no shorter binding is still pending long after timeoutlen", () => {
    let now = 0;
    const resolver = createResolver(GENERIC_ROWS, () => 1000, () => now);
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "pending", prefix: "g", length: 1 });
    expect(resolver.hasFallback()).toBe(false);
    now = 10_000;
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "action", action: "scroll.top" });
  });

  it("a prefix whose shorter binding is the fallback still times out", () => {
    const rows = [{ action: "item.next", keys: ["g"], context: "阅读", desc: "下一题" }, ...GENERIC_ROWS];
    let now = 0;
    const resolver = createResolver(rows, () => 1000, () => now, { prefixWait: () => true });
    resolver.feed(stroke("g"));
    expect(resolver.hasFallback()).toBe(true);
    now = 1001;
    expect(resolver.feed(stroke("g"))).toEqual({ kind: "fallback", action: "item.next", consumed: 1 });
  });

  it("continuations lists what may follow, in declared order", () => {
    const rows: Row[] = [
      ...GENERIC_ROWS,
      { action: "fold.toggle", keys: ["za"], context: "阅读", desc: "折叠" },
      { action: "fold.open_all", keys: ["zR"], context: "阅读", desc: "全展开" },
      { action: "fold.close_all", keys: ["zM"], context: "阅读", desc: "全折叠" },
      { action: "code.left", keys: ["zh"], context: "阅读", desc: "代码左" },
      { action: "tab.next", keys: ["gt"], context: "阅读", desc: "下一页" },
      { action: "tab.prev", keys: ["gT"], context: "阅读", desc: "上一页" },
      { action: "toc.toggle", keys: ["gO"], context: "阅读", desc: "目录" },
      { action: "item.next", keys: ["]q", "]qx"], context: "阅读", desc: "下一题" },
      { action: "item.other", keys: ["]qx"], context: "阅读", desc: "重复" },
    ];
    const resolver = createResolver(rows, () => 1000, () => 0);
    expect(resolver.continuations()).toEqual([]);
    resolver.feed(stroke("z"));
    expect(resolver.continuations()).toEqual([
      { rest: "a", action: "fold.toggle" }, { rest: "R", action: "fold.open_all" },
      { rest: "M", action: "fold.close_all" }, { rest: "h", action: "code.left" },
    ]);
    resolver.reset();
    resolver.feed(stroke("g"));
    expect(resolver.continuations().map(({ rest }) => rest)).toEqual(["g", "t", "T", "O"]);
    resolver.reset();
    resolver.feed(stroke("]"));
    expect(resolver.continuations()).toEqual([
      { rest: "]", action: "link.next" }, { rest: "q", action: "item.next" }, { rest: "qx", action: "item.next" },
    ]);
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
