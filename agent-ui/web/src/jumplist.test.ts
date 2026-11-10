import { describe, expect, it } from "vitest";
import { back, emptyJumplist, forward, JUMPLIST_MAX, landingIndex, recordJump } from "./jumplist";
const pos = (key: string, index = 0, offset = 7) => ({ key, index, offset });
describe("jumplist", () => {
  it("records a round trip and saves the newest position before travelling", () => {
    const list = recordJump(recordJump(emptyJumplist(), pos("a")), pos("b"));
    const older = back(list, pos("c"))!;
    expect(older.to).toEqual(pos("b"));
    const oldest = back(older.list, pos("ignored"))!;
    expect(oldest.to).toEqual(pos("a"));
    expect(back(oldest.list, pos("ignored"))).toBeNull();
    const newer = forward(oldest.list)!;
    expect(newer.to).toEqual(pos("b"));
    const newest = forward(newer.list)!;
    expect(newest.to).toEqual(pos("c"));
    expect(forward(newest.list)).toBeNull();
    expect(back(emptyJumplist(), pos("a"))).toBeNull();
  });
  it("drops forward history after a new jump and moves duplicate keys to the end", () => {
    const list = recordJump(recordJump(emptyJumplist(), pos("a")), pos("b"));
    const travelling = back(back(list, pos("c"))!.list, pos("b"))!.list;
    const changed = recordJump(travelling, pos("a", 4));
    expect(changed.entries).toEqual([pos("a", 4)]);
    expect(changed.at).toBe(1);
    expect(forward(changed)).toBeNull();
    expect(recordJump(list, pos("a", 8)).entries).toEqual([pos("b"), pos("a", 8)]);
  });
  it("keeps the newest 100 records, including when starting a trip at capacity", () => {
    let list = emptyJumplist();
    for (let i = 0; i < 101; i++) list = recordJump(list, pos(String(i), i));
    expect(list.entries).toHaveLength(JUMPLIST_MAX);
    expect(list.entries[0].key).toBe("1");
    expect(list.at).toBe(100);
    const trip = back(list, pos("here"))!;
    expect(trip.list.entries).toHaveLength(100);
    expect(trip.to.key).toBe("100");
    expect(forward(trip.list)!.to.key).toBe("here");
  });
  it("finds a surviving key or clamps its former index, including an empty timeline", () => {
    expect(landingIndex(pos("b", 0), ["a", "b"])).toBe(1);
    expect(landingIndex(pos("gone", 9), ["a", "b"])).toBe(1);
    expect(landingIndex(pos("gone", -1), ["a", "b"])).toBe(0);
    expect(landingIndex(pos("gone", 9), [])).toBe(0);
  });
});
