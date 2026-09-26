import { describe, expect, it } from "vitest";
import { advanceSequence, boxEntries, sequenceTitle, startSequence } from "./leader";
import { binding as b, TABLE } from "./testFixtures";

describe("the sequence engine (spec §2.4, which-key's state.lua)", () => {
  it("runs a one-key binding at once", () => {
    expect(startSequence(TABLE, "H", false)).toEqual({ kind: "run", binding: TABLE.bindings[0] });
  });
  it("leaves g z [ ] and unbound keys to resolveKey", () => {
    for (const k of ["[", "g", "z", "]", "j", "x"]) expect(startSequence(TABLE, k, false).kind).toBe("none");
  });
  it("walks the leader to its action", () => {
    const s1 = startSequence(TABLE, " ", false);
    expect(s1).toEqual({ kind: "pending", typed: ["<leader>"], ambiguous: null });
    const s2 = advanceSequence(TABLE, ["<leader>"], "b");
    expect(s2).toEqual({ kind: "pending", typed: ["<leader>", "b"], ambiguous: null });
    expect(advanceSequence(TABLE, ["<leader>", "b"], "d")).toEqual({ kind: "run", binding: TABLE.bindings[3] });
  });
  it("is not a leader on a focused control", () => {
    expect(startSequence(TABLE, " ", true).kind).toBe("none");
  });
  it("Esc cancels, Backspace goes up, an unbound key is swallowed", () => {
    expect(advanceSequence(TABLE, ["<leader>", "b"], "Escape").kind).toBe("cancel");
    expect(advanceSequence(TABLE, ["<leader>", "b"], "Backspace")).toEqual({ kind: "pending", typed: ["<leader>"], ambiguous: null });
    expect(advanceSequence(TABLE, ["<leader>"], "Backspace").kind).toBe("cancel");
    expect(advanceSequence(TABLE, ["<leader>"], "q").kind).toBe("cancel");
  });
  it("marks a node that is both a binding and a prefix as ambiguous", () => {
    const t = { ...TABLE, bindings: [...TABLE.bindings, b(["<leader>", "b"], "tab.next")] };
    const s = advanceSequence(t, ["<leader>"], "b");
    expect(s.kind === "pending" && s.ambiguous?.action).toBe("tab.next");
  });
  it("lists a node's continuations, keys first then groups, and titles it", () => {
    expect(boxEntries(TABLE, ["<leader>"], true).map((e) => `${e.key}:${e.label}`)).toEqual(["m:mode", "b:+tab", "f:+new"]);
    expect(boxEntries(TABLE, ["<leader>"], true)[0].disabled).toBe(true); // mode.cycle on a live session
    expect(sequenceTitle(TABLE, ["<leader>", "b"])).toBe("Space b");
  });
  it("a new table cancels nothing by itself but a stale typed prefix advances to cancel", () => {
    // Review Focus 1: App drops the pending sequence on a new table; if a key races in, the engine
    // over the NEW table must not run an old binding.
    const fresh = { ...TABLE, bindings: TABLE.bindings.filter((x) => x.action !== "tab.close") };
    expect(advanceSequence(fresh, ["<leader>", "b"], "d").kind).toBe("cancel");
  });
});
