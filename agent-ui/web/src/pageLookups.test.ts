import { describe, expect, it } from "vitest";
import { pruneCardViews, pruneTabEntries } from "./pageLookups";

describe("page lookup lifetimes", () => {
  it("prunes closed tabs from completion requests and results without changing the inputs", () => {
    const live = new Set([2, 3]);
    const requests = new Map([[1, 7], [2, 8]]);
    const paths = new Map([[1, { items: ["gone.rs"], seq: 7 }], [2, { items: ["kept.rs"], seq: 8 }]]);
    expect([...pruneTabEntries(requests, live)]).toEqual([[2, 8]]);
    const kept = pruneTabEntries(paths, live);
    expect([...kept]).toEqual([[2, paths.get(2)]]);
    expect(kept.get(2)).toBe(paths.get(2));
    expect(requests.size).toBe(2);
    expect(paths.size).toBe(2);
    expect(pruneTabEntries(kept, live)).toBe(kept);
    expect(pruneTabEntries(requests, new Set()).size).toBe(0);
  });

  it("drops resolved and closed-tab proposal views, keeping another tab's pending view across a switch", () => {
    const entries = new Map([
      ["closed", { tab: 1, shown: 2 }],
      ["resolved", { tab: 2, shown: 1 }],
      ["waiting", { tab: 2, shown: 3 }],
      ["away", { tab: 3, shown: 4 }],
    ]);
    const live = new Set([2, 3]);
    const kept = pruneCardViews(entries, live, { tab: 2, pending: new Set(["waiting"]) });
    expect([...kept.keys()]).toEqual(["waiting", "away"]);
    expect(kept.get("waiting")).toBe(entries.get("waiting"));
    expect(entries.size).toBe(4);
    // A tabs envelope arrives before the switched-to tab's snapshot.
    expect(pruneCardViews(kept, live)).toBe(kept);
    const back = pruneCardViews(kept, live, { tab: 3, pending: new Set(["away"]) });
    expect(back).toBe(kept);
    expect([...pruneCardViews(back, live, { tab: 3, pending: new Set() }).keys()]).toEqual(["waiting"]);
    expect(pruneCardViews(kept, new Set()).size).toBe(0);
  });
});
