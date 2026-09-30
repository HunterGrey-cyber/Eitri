import { describe, expect, it } from "vitest";
import { FIXED_PAIRS } from "./keymap";
import { advanceSequence, boxEntries, FIXED_PENDING_ENTRIES, sequenceTitle, startSequence } from "./leader";
import { binding as b, TABLE } from "./testFixtures";

/* Keep `FIXED_PAIRS` and `FIXED_PENDING_ENTRIES` in sync so every working pair is listed
   in the which-key box and every listed pair completes. */
describe("the box after g / z / [ / ] / Ctrl+w (K01)", () => {
  it("lists exactly the keys FIXED_PAIRS completes each prefix with, each with a label", () => {
    expect(Object.keys(FIXED_PENDING_ENTRIES).sort()).toEqual(Object.keys(FIXED_PAIRS).sort());
    // Ctrl+w, stored as "C-w", is the fifth reserved prefix.
    for (const prefix of ["g", "z", "[", "]", "C-w"] as const) {
      const listed = FIXED_PENDING_ENTRIES[prefix].map((e) => e.key);
      expect(listed.slice().sort(), prefix).toEqual(Object.keys(FIXED_PAIRS[prefix]).sort());
      expect(new Set(listed).size, `${prefix}: each key once`).toBe(listed.length);
      for (const e of FIXED_PENDING_ENTRIES[prefix]) expect(e.label, `${prefix}${e.key}`).not.toBe("");
    }
  });

  /* The which-key box labels the six z pairs for row scrolling and folding, beyond zh/zl. */
  it("names the six z pairs the fold and row-scroll keys add", () => {
    const labels = Object.fromEntries(FIXED_PENDING_ENTRIES.z.map((e) => [e.key, e.label]));
    expect(labels).toMatchObject({
      t: "row to top",
      z: "row to middle",
      b: "row to bottom",
      a: "toggle fold",
      o: "open fold",
      c: "close fold",
    });
  });

  /* The box lists gg, gf and gx in that order because prefix entries follow table order. */
  it("names gx after gg and gf", () => {
    expect(FIXED_PENDING_ENTRIES.g.map((e) => [e.key, e.label])).toEqual([
      ["g", "first row"],
      ["f", "open path"],
      ["x", "open link"],
    ]);
  });

  /* Bracket entries list the prompt pair before the waiting-card pair, matching table order. */
  it("names the ]p and [p pairs after the prompt pairs", () => {
    expect(FIXED_PENDING_ENTRIES["]"].map((e) => [e.key, e.label])).toEqual([
      ["]", "next prompt"],
      ["p", "next waiting card"],
    ]);
    expect(FIXED_PENDING_ENTRIES["["].map((e) => [e.key, e.label])).toEqual([
      ["[", "previous prompt"],
      ["p", "previous waiting card"],
    ]);
  });

  /* The box labels all four Ctrl+w directions; `App.tsx` displays its title as Ctrl+w. */
  it("names the four Ctrl+w pairs by the module they move the keys to", () => {
    expect(FIXED_PENDING_ENTRIES["C-w"].map((e) => [e.key, e.label])).toEqual([
      ["h", "module left"],
      ["j", "module below"],
      ["k", "module above"],
      ["l", "module right"],
    ]);
  });

  /* A table binding cannot start with `Ctrl+w` (`eitri.keymap.set("panel", ...)` parses no `<C-w>`),
     so the box after it is these four rows and nothing the table adds. */
  it("adds nothing from the panel table after Ctrl+w", () => {
    expect(boxEntries(TABLE, ["C-w"], false)).toEqual([]);
  });
});

describe("the sequence engine (which-key's state.lua)", () => {
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
    // App drops pending sequences when the table changes. A racing key resolved against the
    // new table must not run a binding that was removed.
    const fresh = { ...TABLE, bindings: TABLE.bindings.filter((x) => x.action !== "tab.close") };
    expect(advanceSequence(fresh, ["<leader>", "b"], "d").kind).toBe("cancel");
  });
});
